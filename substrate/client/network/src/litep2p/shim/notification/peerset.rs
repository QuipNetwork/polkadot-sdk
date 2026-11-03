// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! [`Peerset`] implementation for `litep2p`.
//!
//! [`Peerset`] is a separate but related component running alongside the notification protocol,
//! responsible for maintaining connectivity to remote peers. [`Peerset`] has an imperfect view of
//! the network as the notification protocol is behind an asynchronous channel. Based on this
//! imperfect view, it tries to connect to remote peers and disconnect peers that should be
//! disconnected from.
//!
//! [`Peerset`] knows of two types of peers:
//!  - normal peers
//!  - reserved peers
//!
//! Reserved peers are those which the [`Peerset`] should be connected at all times and it will make
//! an effort to do so by constantly checking that there are no disconnected reserved peers (except
//! banned) and if there are, it will open substreams to them.
//!
//! [`Peerset`] may also contain "slots", both inbound and outbound, which mark how many incoming
//! and outgoing connections it should maintain at all times. Peers for the inbound slots are filled
//! by remote peers opening inbound substreams towards the local node and peers for the outbound
//! slots are filled by querying the `Peerstore` which contains all peers known to `sc-network`.
//! Peers for outbound slots are selected in a decreasing order of reputation.

use crate::{
	peer_store::{PeerStoreProvider, ProtocolHandle},
	service::{
		metrics::NotificationMetrics,
		traits::{self, ValidationResult},
	},
	ProtocolName, ReputationChange as Reputation,
};

use futures::{channel::oneshot, future::BoxFuture, stream::FuturesUnordered, Stream, StreamExt};
use futures_timer::Delay;
use litep2p::protocol::notification::NotificationError;

use sc_network_types::PeerId;
use sc_utils::mpsc::{tracing_unbounded, TracingUnboundedReceiver, TracingUnboundedSender};

use std::{
	collections::{HashMap, HashSet},
	future::Future,
	pin::Pin,
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	},
	task::{Context, Poll},
	time::Duration,
};

/// Logging target for the file.
const LOG_TARGET: &str = "sub-libp2p::peerset";

/// Default backoff for connection re-attempts.
const DEFAULT_BACKOFF: Duration = Duration::from_secs(5);

/// Open failure backoff.
const OPEN_FAILURE_BACKOFF: Duration = Duration::from_secs(5);

/// Slot allocation frequency.
///
/// How often should [`Peerset`] attempt to establish outbound connections.
const SLOT_ALLOCATION_FREQUENCY: Duration = Duration::from_secs(1);

/// Reputation adjustment when a peer gets disconnected.
///
/// Lessens the likelyhood of the peer getting selected for an outbound connection soon.
const DISCONNECT_ADJUSTMENT: Reputation = Reputation::new(-256, "Peer disconnected");

/// Reputation adjustment when a substream fails to open.
///
/// Lessens the likelyhood of the peer getting selected for an outbound connection soon.
const OPEN_FAILURE_ADJUSTMENT: Reputation = Reputation::new(-1024, "Open failure");

/// Is the peer reserved?
///
/// Regular peers count towards slot allocation.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Reserved {
	Yes,
	No,
}

impl From<bool> for Reserved {
	fn from(value: bool) -> Reserved {
		match value {
			true => Reserved::Yes,
			false => Reserved::No,
		}
	}
}

impl From<Reserved> for bool {
	fn from(value: Reserved) -> bool {
		std::matches!(value, Reserved::Yes)
	}
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Direction {
	/// Inbound substream.
	Inbound(Reserved),

	/// Outbound substream.
	Outbound(Reserved),
}

impl Direction {
	fn set_reserved(&mut self, new_reserved: Reserved) {
		match self {
			Direction::Inbound(ref mut reserved) | Direction::Outbound(ref mut reserved) => {
				*reserved = new_reserved
			},
		}
	}
}

impl From<Direction> for traits::Direction {
	fn from(direction: Direction) -> traits::Direction {
		match direction {
			Direction::Inbound(_) => traits::Direction::Inbound,
			Direction::Outbound(_) => traits::Direction::Outbound,
		}
	}
}

/// Open result for a fully-opened connection.
#[derive(PartialEq, Eq, Debug)]
pub enum OpenResult {
	/// Accept the connection.
	Accept {
		/// Direction which [`Peerset`] considers to be correct.
		direction: traits::Direction,
	},

	/// Reject the connection because it was canceled while it was opening.
	Reject,
}

/// Commands emitted by other subsystems of the blockchain to [`Peerset`].
#[derive(Debug)]
pub enum PeersetCommand {
	/// Set current reserved peer set.
	///
	/// This command removes all reserved peers that are not in `peers`.
	SetReservedPeers {
		/// New reserved peer set.
		peers: HashSet<PeerId>,
	},

	/// Add one or more reserved peers.
	///
	/// This command doesn't remove any reserved peers but only add new peers.
	AddReservedPeers {
		/// Reserved peers to add.
		peers: HashSet<PeerId>,
	},

	/// Remove reserved peers.
	RemoveReservedPeers {
		/// Reserved peers to remove.
		peers: HashSet<PeerId>,
	},

	/// Set reserved-only mode to true/false.
	SetReservedOnly {
		/// Should the protocol only accept/establish connections to reserved peers.
		reserved_only: bool,
	},

	/// Disconnect peer.
	DisconnectPeer {
		/// Peer ID.
		peer: PeerId,
	},

	/// Get reserved peers.
	GetReservedPeers {
		/// `oneshot::Sender` for sending the current set of reserved peers.
		tx: oneshot::Sender<Vec<PeerId>>,
	},
}

/// Commands emitted by [`Peerset`] to the notification protocol.
#[derive(Debug)]
pub struct PeersetNotificationCommand {
	/// Open substreams to one or more peers.
	pub open_peers: Vec<PeerId>,

	/// Close substream to one or more peers.
	pub close_peers: Vec<PeerId>,
}

impl PeersetNotificationCommand {
	/// Open substream to peers.
	pub fn open_substream(peers: Vec<PeerId>) -> Self {
		Self { open_peers: peers, close_peers: vec![] }
	}

	/// Close substream to peers.
	pub fn close_substream(peers: Vec<PeerId>) -> Self {
		Self { open_peers: vec![], close_peers: peers }
	}
}

/// Peer state.
///
/// Peer can be in 6 different state:
///  - disconnected
///  - connected
///  - connection is opening
///  - connection is closing
///  - connection is backed-off
///  - connection is canceled
///
/// Opening and closing are separate states as litep2p guarantees to report when the substream is
/// either fully open or fully closed and the slot allocation for opening a substream is tied to a
/// state transition which moves the peer to [`PeerState::Opening`]. This is because it allows
/// reserving a slot for peer to prevent infinite outbound substreams. If the substream is opened
/// successfully, peer is moved to state [`PeerState::Connected`] but there is no modification to
/// the slot count as an outbound slot was already allocated for the peer. If the substream fails to
/// open, the event is reported by litep2p and [`Peerset::report_substream_open_failure()`] is
/// called which will decrease the outbound slot count. Similarly for inbound streams, the slot is
/// allocated in [`Peerset::report_inbound_substream()`] which will prevent `Peerset` from accepting
/// infinite inbound substreams. If the inbound substream fails to open and since [`Peerset`] was
/// notified of it, litep2p will report the open failure and the inbound slot count is once again
/// decreased in [`Peerset::report_substream_open_failure()`]. If the substream is opened
/// successfully, the slot count is not modified.
///
/// Since closing a substream is not instantaneous, there is a separate [`PeerState::Closing`]
/// state which indicates that the substream is being closed but hasn't been closed by litep2p yet.
/// This state is used to prevent invalid state transitions where, for example, [`Peerset`] would
/// close a substream and then try to reopen it immediately.
///
/// Irrespective of which side closed the substream (local/remote), the substream is chilled for a
/// small amount of time ([`DEFAULT_BACKOFF`]) and during this time no inbound or outbound
/// substreams are accepted/established. Any request to open an outbound substream while the peer
/// is backed-off is ignored. If the peer is a reserved peer, an outbound substream is not opened
/// for them immediately but after the back-off has expired, `Peerset` will attempt to open a
/// substream to the peer if it's still counted as a reserved peer.
///
/// Disconnections and open failures will contribute negatively to the peer score to prevent it from
/// being selected for another outbound substream request soon after the failure/disconnection. The
/// reputation decays towards zero over time and eventually the peer will be as likely to be
/// selected for an outbound substream as any other freshly added peer.
///
/// [`Peerset`] must also be able to handle the case where an outbound substream was opened to peer
/// and while it was opening, an inbound substream was received from that same peer. Since `litep2p`
/// is the source of truth of the actual state of the connection, [`Peerset`] must compensate for
/// this and if it happens that inbound substream is opened for a peer that was marked outbound, it
/// will attempt to allocate an inbound slot for the peer. If it fails to do so, the inbound
/// substream is rejected and the peer is marked as canceled.
///
/// Since substream is not opened immediately, a peer can be disconnected even if the substream was
/// not yet open. This can happen, for example, when a peer has connected over the syncing protocol
/// and it was added to, e.g., GRANDPA's reserved peers, an outbound substream was opened
/// ([`PeerState::Opening`]) and then the peer disconnected. This state transition is handled by the
/// [`Peerset`] with `PeerState::Canceled` which indicates that should the substream open
/// successfully, it should be closed immediately and if the connection is opened successfully while
/// the peer was marked as canceled, the substream will be closed without notifying the protocol
/// about the substream.
#[derive(Debug, PartialEq, Eq)]
pub enum PeerState {
	/// No active connection to peer.
	Disconnected,

	/// Substream to peer was recently closed and the peer is currently backed off.
	///
	/// Backoff only applies to outbound substreams. Inbound substream will not experience any sort
	/// of "banning" even if the peer is backed off and an inbound substream for the peer is
	/// received.
	Backoff,

	/// Connection to peer is pending.
	Opening {
		/// Direction of the connection.
		direction: Direction,
	},

	// Connected to peer.
	Connected {
		/// Is the peer inbound or outbound.
		direction: Direction,
	},

	/// Substream was opened and while it was opening (no response had been heard from litep2p),
	/// the substream was canceled by either calling `disconnect_peer()` or by removing peer
	/// from the reserved set.
	///
	/// After the opened substream is acknowledged by litep2p (open success/failure), the peer is
	/// moved to [`PeerState::Backoff`] from which it will then be moved to
	/// [`PeerState::Disconnected`].
	Canceled {
		/// Is the peer inbound or outbound.
		direction: Direction,
	},

	/// Connection to peer is closing.
	///
	/// State implies that the substream was asked to be closed by the local node and litep2p is
	/// closing the substream. No command modifying the connection state is accepted until the
	/// state has been set to [`PeerState::Disconnected`].
	Closing {
		/// Is the peer inbound or outbound.
		direction: Direction,
	},
}

/// `Peerset` implementation.
///
/// `Peerset` allows other subsystems of the blockchain to modify the connection state
/// of the notification protocol by adding and removing reserved peers.
///
/// `Peerset` is also responsible for maintaining the desired amount of peers the protocol is
/// connected to by establishing outbound connections and accepting/rejecting inbound connections.
#[derive(Debug)]
pub struct Peerset {
	/// Protocol name.
	protocol: ProtocolName,

	/// RX channel for receiving commands.
	cmd_rx: TracingUnboundedReceiver<PeersetCommand>,

	/// Maximum number of outbound peers.
	max_out: usize,

	/// Current number of outbound peers.
	num_out: usize,

	/// Maximum number of inbound peers.
	max_in: usize,

	/// Current number of inbound peers.
	num_in: usize,

	/// Only connect to/accept connections from reserved peers.
	reserved_only: bool,

	/// Current reserved peer set.
	reserved_peers: HashSet<PeerId>,

	/// Handle to `Peerstore`.
	peerstore_handle: Arc<dyn PeerStoreProvider>,

	/// Peers.
	peers: HashMap<PeerId, PeerState>,

	/// Counter connected peers.
	connected_peers: Arc<AtomicUsize>,

	/// Pending backoffs for peers who recently disconnected.
	pending_backoffs: FuturesUnordered<BoxFuture<'static, (PeerId, Reputation)>>,

	/// Next time when [`Peerset`] should perform slot allocation.
	next_slot_allocation: Delay,

	/// Notification metrics.
	metrics: NotificationMetrics,
}

macro_rules! decrement_or_warn {
    ($slot:expr, $protocol:expr, $peer:expr, $direction:expr) => {{
		match $slot.checked_sub(1) {
			Some(value) => {
				$slot = value;
			}
			None => {
				log::warn!(
					target: LOG_TARGET,
					"{}: state mismatch, {:?} is not counted as part of {:?} slots",
					$protocol, $peer, $direction
				);
				debug_assert!(false);
			}
		}
    }};
}

/// Handle to [`Peerset`], given to `Peerstore`.
#[derive(Debug)]
struct PeersetHandle {
	/// TX channel for sending commands to [`Peerset`].
	tx: TracingUnboundedSender<PeersetCommand>,
}

impl ProtocolHandle for PeersetHandle {
	/// Disconnect peer, as a result of a ban.
	fn disconnect_peer(&self, peer: PeerId) {
		let _ = self.tx.unbounded_send(PeersetCommand::DisconnectPeer { peer });
	}
}

impl Peerset {
	/// Create new [`Peerset`].
	pub fn new(
		protocol: ProtocolName,
		max_out: usize,
		max_in: usize,
		reserved_only: bool,
		reserved_peers: HashSet<PeerId>,
		connected_peers: Arc<AtomicUsize>,
		peerstore_handle: Arc<dyn PeerStoreProvider>,
		metrics: NotificationMetrics,
	) -> (Self, TracingUnboundedSender<PeersetCommand>) {
		let (cmd_tx, cmd_rx) = tracing_unbounded("mpsc-peerset-protocol", 100_000);
		let peers = reserved_peers
			.iter()
			.map(|peer| (*peer, PeerState::Disconnected))
			.collect::<HashMap<_, _>>();

		// register protocol's command channel to `Peerstore` so it can issue disconnect commands
		// if some connected peer gets banned.
		peerstore_handle.register_protocol(Arc::new(PeersetHandle { tx: cmd_tx.clone() }));

		log::debug!(
			target: LOG_TARGET,
			"{}: creating new peerset with max_outbound {} and max_inbound {} and reserved_only {}",
			protocol,
			max_out,
			max_in,
			reserved_only,
		);

		(
			Self {
				protocol,
				max_out,
				num_out: 0usize,
				max_in,
				num_in: 0usize,
				reserved_peers,
				cmd_rx,
				peerstore_handle,
				reserved_only,
				peers,
				connected_peers,
				pending_backoffs: FuturesUnordered::new(),
				next_slot_allocation: Delay::new(SLOT_ALLOCATION_FREQUENCY),
				metrics,
			},
			cmd_tx,
		)
	}

	/// Report to [`Peerset`] that a substream was opened.
	///
	/// Slot for the stream was "preallocated" when it was initiated (outbound) or accepted
	/// (inbound) by the local node which is why this function doesn't allocate a slot for the peer.
	///
	/// Returns `true` if the substream should be kept open and `false` if the substream had been
	/// canceled while it was opening and litep2p should close the substream.
	pub fn report_substream_opened(
		&mut self,
		peer: PeerId,
		direction: traits::Direction,
	) -> OpenResult {
		log::trace!(
			target: LOG_TARGET,
			"{}: substream opened to {peer:?}, direction {direction:?}, reserved peer {}",
			self.protocol,
			self.reserved_peers.contains(&peer),
		);

		let Some(state) = self.peers.get_mut(&peer) else {
			log::warn!(target: LOG_TARGET, "{}: substream opened for unknown peer {peer:?}", self.protocol);
			debug_assert!(false);
			return OpenResult::Reject;
		};

		match state {
			PeerState::Opening { direction: substream_direction } => {
				let real_direction: traits::Direction = (*substream_direction).into();

				*state = PeerState::Connected { direction: *substream_direction };
				self.connected_peers.fetch_add(1usize, Ordering::Relaxed);

				return OpenResult::Accept { direction: real_direction };
			},
			// litep2p doesn't support the ability to cancel an opening substream so if the
			// substream was closed while it was opening, it was marked as canceled and if the
			// substream opens succesfully, it will be closed
			PeerState::Canceled { direction: substream_direction } => {
				log::trace!(
					target: LOG_TARGET,
					"{}: substream to {peer:?} is canceled, issue disconnection request",
					self.protocol,
				);

				self.connected_peers.fetch_add(1usize, Ordering::Relaxed);
				*state = PeerState::Closing { direction: *substream_direction };

				return OpenResult::Reject;
			},
			// The peer was already rejected by the `report_inbound_substream` call and this
			// should never happen. However, this code path is exercised by our fuzzer.
			PeerState::Disconnected => {
				log::debug!(
					target: LOG_TARGET,
					"{}: substream opened for a peer that was previously rejected {peer:?}",
					self.protocol,
				);
				return OpenResult::Reject;
			},
			state => {
				log::error!(
					target: LOG_TARGET,
					"{}: substream opened for a peer in invalid state {peer:?}: {state:?}",
					self.protocol,
				);

				debug_assert!(false);
				return OpenResult::Reject;
			},
		}
	}

	/// Report to [`Peerset`] that a substream was closed.
	///
	/// If the peer was not a reserved peer, the inbound/outbound slot count is adjusted to account
	/// for the disconnected peer. After the connection is closed, the peer is chilled for a
	/// duration of [`DEFAULT_BACKOFF`] which prevens [`Peerset`] from establishing/accepting new
	/// connections for that time period.
	pub fn report_substream_closed(&mut self, peer: PeerId) {
		log::trace!(target: LOG_TARGET, "{}: substream closed to {peer:?}", self.protocol);

		let Some(state) = self.peers.get_mut(&peer) else {
			log::warn!(target: LOG_TARGET, "{}: substream closed for unknown peer {peer:?}", self.protocol);
			debug_assert!(false);
			return;
		};

		match &state {
			// close was initiated either by remote ([`PeerState::Connected`]) or local node
			// ([`PeerState::Closing`]) and it was a non-reserved peer
			PeerState::Connected { direction: Direction::Inbound(Reserved::No) } |
			PeerState::Closing { direction: Direction::Inbound(Reserved::No) } => {
				log::trace!(
					target: LOG_TARGET,
					"{}: inbound substream closed to non-reserved peer {peer:?}: {state:?}",
					self.protocol,
				);

				decrement_or_warn!(
					self.num_in,
					peer,
					self.protocol,
					Direction::Inbound(Reserved::No)
				);
			},
			// close was initiated either by remote ([`PeerState::Connected`]) or local node
			// ([`PeerState::Closing`]) and it was a non-reserved peer
			PeerState::Connected { direction: Direction::Outbound(Reserved::No) } |
			PeerState::Closing { direction: Direction::Outbound(Reserved::No) } => {
				log::trace!(
					target: LOG_TARGET,
					"{}: outbound substream closed to non-reserved peer {peer:?} {state:?}",
					self.protocol,
				);

				decrement_or_warn!(
					self.num_out,
					peer,
					self.protocol,
					Direction::Outbound(Reserved::No)
				);
			},
			// reserved peers don't require adjustments to slot counts
			PeerState::Closing { .. } | PeerState::Connected { .. } => {
				log::debug!(target: LOG_TARGET, "{}: reserved peer {peer:?} disconnected", self.protocol);
			},
			// The peer was already rejected by the `report_inbound_substream` call and this
			// should never happen. However, this code path is exercised by our fuzzer.
			PeerState::Disconnected => {
				log::debug!(
					target: LOG_TARGET,
					"{}: substream closed for a peer that was previously rejected {peer:?}",
					self.protocol,
				);
			},
			state => {
				log::warn!(target: LOG_TARGET, "{}: invalid state for disconnected peer {peer:?}: {state:?}", self.protocol);
				debug_assert!(false);
			},
		}

		// Rejected peers do not count towards slot allocation.
		if !matches!(state, PeerState::Disconnected) {
			self.connected_peers.fetch_sub(1usize, Ordering::Relaxed);
		}

		*state = PeerState::Backoff;
		self.pending_backoffs.push(Box::pin(async move {
			Delay::new(DEFAULT_BACKOFF).await;
			(peer, DISCONNECT_ADJUSTMENT)
		}));
	}

	/// Report to [`Peerset`] that an inbound substream was opened and that it should validate it.
	pub fn report_inbound_substream(&mut self, peer: PeerId) -> ValidationResult {
		log::trace!(target: LOG_TARGET, "{}: inbound substream from {peer:?}", self.protocol);

		if self.peerstore_handle.is_banned(&peer) {
			log::debug!(
				target: LOG_TARGET,
				"{}: rejecting banned peer {peer:?}",
				self.protocol,
			);

			return ValidationResult::Reject;
		}

		let state = self.peers.entry(peer).or_insert(PeerState::Disconnected);
		let is_reserved_peer = self.reserved_peers.contains(&peer);

		// Check if this is a non-reserved peer and if the protocol is in reserved-only mode.
		let should_reject = self.reserved_only && !is_reserved_peer;

		match state {
			// disconnected peers that are reserved-only peers are rejected
			PeerState::Disconnected if should_reject => {
				log::trace!(
					target: LOG_TARGET,
					"{}: rejecting non-reserved peer {peer:?} in reserved-only mode (prev state: {state:?})",
					self.protocol,
				);

				return ValidationResult::Reject;
			},
			// disconnected peers proceed directly to inbound slot allocation
			PeerState::Disconnected => {},
			// peer is backed off but if it can be accepted (either a reserved peer or inbound slot
			// available), accept the peer and then just ignore the back-off timer when it expires
			PeerState::Backoff => {
				if !is_reserved_peer && self.num_in == self.max_in {
					log::trace!(
						target: LOG_TARGET,
						"{}: ({peer:?}) is backed-off and cannot accept, reject inbound substream",
						self.protocol,
					);

					return ValidationResult::Reject;
				}

				// The peer remains in the `PeerState::Backoff` state until the current timer
				// expires. Then, the peer will be in the disconnected state, subject to further
				// rejection if the peer is not reserved by then.
				if should_reject {
					return ValidationResult::Reject;
				}
			},

			// `Peerset` had initiated an outbound substream but litep2p had received an inbound
			// substream before the command to open the substream was received, meaning local and
			// remote desired to open a connection at the same time. Since outbound substreams
			// cannot be canceled with litep2p and the command has already been registered, accept
			// the inbound peer since the local node had wished a connection to be opened either way
			// but keep the direction of the substream as it was (outbound).
			//
			// litep2p doesn't care what `Peerset` considers the substream direction to be and since
			// it's used for bookkeeping for substream counts, keeping the substream direction
			// unmodified simplies the implementation a lot. The direction would otherwise be
			// irrelevant for protocols but because `SyncingEngine` has a hack to reject excess
			// inbound substreams, that system has to be kept working for the time being. Once that
			// issue is fixed, this approach can be re-evaluated if need be.
			PeerState::Opening { direction: Direction::Outbound(reserved) } => {
				if should_reject {
					log::trace!(
						target: LOG_TARGET,
						"{}: rejecting inbound substream from {peer:?} ({reserved:?}) in reserved-only mode that was marked outbound",
						self.protocol,
					);

					*state = PeerState::Canceled { direction: Direction::Outbound(*reserved) };
					return ValidationResult::Reject;
				}

				log::trace!(
					target: LOG_TARGET,
					"{}: inbound substream received for {peer:?} ({reserved:?}) that was marked outbound",
					self.protocol,
				);

				return ValidationResult::Accept;
			},
			PeerState::Canceled { direction } => {
				log::trace!(
					target: LOG_TARGET,
					"{}: {peer:?} is canceled, rejecting substream should_reject={should_reject}",
					self.protocol,
				);

				*state = PeerState::Canceled { direction: *direction };
				return ValidationResult::Reject;
			},
			state => {
				log::warn!(
					target: LOG_TARGET,
					"{}: invalid state ({state:?}) for inbound substream, peer {peer:?}",
					self.protocol
				);
				debug_assert!(false);
				return ValidationResult::Reject;
			},
		}

		if is_reserved_peer {
			log::trace!(
				target: LOG_TARGET,
				"{}: {peer:?} accepting peer as reserved peer",
				self.protocol,
			);

			*state = PeerState::Opening { direction: Direction::Inbound(is_reserved_peer.into()) };
			return ValidationResult::Accept;
		}

		if self.num_in < self.max_in {
			log::trace!(
				target: LOG_TARGET,
				"{}: {peer:?} accepting peer as regular peer",
				self.protocol,
			);

			self.num_in += 1;

			*state = PeerState::Opening { direction: Direction::Inbound(is_reserved_peer.into()) };
			return ValidationResult::Accept;
		}

		log::trace!(
			target: LOG_TARGET,
			"{}: reject {peer:?}, not a reserved peer and no free inbound slots",
			self.protocol,
		);

		*state = PeerState::Disconnected;
		return ValidationResult::Reject;
	}

	/// Report to [`Peerset`] that there was an error opening a substream.
	pub fn report_substream_open_failure(&mut self, peer: PeerId, error: NotificationError) {
		log::trace!(
			target: LOG_TARGET,
			"{}: failed to open substream to {peer:?}: {error:?}",
			self.protocol,
		);

		match self.peers.get(&peer) {
			Some(PeerState::Opening { direction: Direction::Outbound(Reserved::No) }) => {
				decrement_or_warn!(
					self.num_out,
					self.protocol,
					peer,
					Direction::Outbound(Reserved::No)
				);
			},
			Some(PeerState::Opening { direction: Direction::Inbound(Reserved::No) }) => {
				decrement_or_warn!(
					self.num_in,
					self.protocol,
					peer,
					Direction::Inbound(Reserved::No)
				);
			},
			Some(PeerState::Canceled { direction }) => match direction {
				Direction::Inbound(Reserved::No) => {
					decrement_or_warn!(
						self.num_in,
						self.protocol,
						peer,
						Direction::Inbound(Reserved::No)
					);
				},
				Direction::Outbound(Reserved::No) => {
					decrement_or_warn!(
						self.num_out,
						self.protocol,
						peer,
						Direction::Outbound(Reserved::No)
					);
				},
				_ => {},
			},
			// reserved peers do not require change in the slot counts
			Some(PeerState::Opening { direction: Direction::Inbound(Reserved::Yes) }) |
			Some(PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) }) => {
				log::debug!(
					target: LOG_TARGET,
					"{}: substream open failure for reserved peer {peer:?}",
					self.protocol,
				);
			},
			state => {
				log::debug!(
					target: LOG_TARGET,
					"{}: substream open failure for a unknown state: {state:?}",
					self.protocol,
				);

				return;
			},
		}

		self.peers.insert(peer, PeerState::Backoff);
		self.pending_backoffs.push(Box::pin(async move {
			Delay::new(OPEN_FAILURE_BACKOFF).await;
			(peer, OPEN_FAILURE_ADJUSTMENT)
		}));
	}

	/// [`Peerset`] had accepted a peer but it was then rejected by the protocol.
	pub fn report_substream_rejected(&mut self, peer: PeerId) {
		log::trace!(target: LOG_TARGET, "{}: {peer:?} rejected by the protocol", self.protocol);

		match self.peers.remove(&peer) {
			Some(PeerState::Opening { direction }) => match direction {
				Direction::Inbound(Reserved::Yes) | Direction::Outbound(Reserved::Yes) => {
					log::warn!(
						target: LOG_TARGET,
						"{}: reserved peer {peer:?} rejected by the protocol",
						self.protocol,
					);
					self.peers.insert(peer, PeerState::Disconnected);
				},
				Direction::Inbound(Reserved::No) => {
					decrement_or_warn!(
						self.num_in,
						peer,
						self.protocol,
						Direction::Inbound(Reserved::No)
					);
					self.peers.insert(peer, PeerState::Disconnected);
				},
				Direction::Outbound(Reserved::No) => {
					decrement_or_warn!(
						self.num_out,
						peer,
						self.protocol,
						Direction::Outbound(Reserved::No)
					);
					self.peers.insert(peer, PeerState::Disconnected);
				},
			},
			Some(state @ PeerState::Canceled { .. }) => {
				log::debug!(
					target: LOG_TARGET,
					"{}: substream to {peer:?} rejected by protocol but already canceled",
					self.protocol,
				);

				self.peers.insert(peer, state);
			},
			Some(state) => {
				log::debug!(
					target: LOG_TARGET,
					"{}: {peer:?} rejected by the protocol but not opening anymore: {state:?}",
					self.protocol,
				);

				self.peers.insert(peer, state);
			},
			None => {},
		}
	}

	/// Calculate how many of the connected peers were counted as normal inbound/outbound peers
	/// which is needed to adjust slot counts when new reserved peers are added.
	///
	/// If the peer is not already in the [`Peerset`], it is added as a disconnected peer.
	fn calculate_slot_adjustment<'a>(
		&'a mut self,
		peers: impl Iterator<Item = &'a PeerId>,
	) -> (usize, usize) {
		peers.fold((0, 0), |(mut inbound, mut outbound), peer| {
			match self.peers.get_mut(peer) {
				Some(PeerState::Disconnected | PeerState::Backoff) => {},
				Some(
					PeerState::Opening { ref mut direction } |
					PeerState::Connected { ref mut direction } |
					PeerState::Canceled { ref mut direction } |
					PeerState::Closing { ref mut direction },
				) => {
					*direction = match direction {
						Direction::Inbound(Reserved::No) => {
							inbound += 1;
							Direction::Inbound(Reserved::Yes)
						},
						Direction::Outbound(Reserved::No) => {
							outbound += 1;
							Direction::Outbound(Reserved::Yes)
						},
						ref direction => **direction,
					};
				},
				None => {
					self.peers.insert(*peer, PeerState::Disconnected);
				},
			}

			(inbound, outbound)
		})
	}

	/// Checks if the peer should be disconnected based on the current state of the [`Peerset`]
	/// and the provided direction.
	///
	/// Note: The role of the peer is not checked.
	fn should_disconnect(&self, direction: Direction) -> bool {
		match direction {
			Direction::Inbound(_) => self.num_in >= self.max_in,
			Direction::Outbound(_) => self.num_out >= self.max_out,
		}
	}

	/// Increment the slot count for given peer.
	fn increment_slot(&mut self, direction: Direction) {
		match direction {
			Direction::Inbound(Reserved::No) => self.num_in += 1,
			Direction::Outbound(Reserved::No) => self.num_out += 1,
			_ => {},
		}
	}

	/// Report connected peer counts to metrics.
	fn update_slot_metrics(&self) {
		let (mut in_reserved, mut in_non_reserved) = (0usize, 0usize);
		let (mut out_reserved, mut out_non_reserved) = (0usize, 0usize);
		let (mut num_disconnected, mut num_backoff) = (0usize, 0usize);

		for state in self.peers.values() {
			match state {
				PeerState::Connected { direction: Direction::Inbound(Reserved::Yes) } => {
					in_reserved += 1
				},
				PeerState::Connected { direction: Direction::Inbound(Reserved::No) } => {
					in_non_reserved += 1
				},
				PeerState::Connected { direction: Direction::Outbound(Reserved::Yes) } => {
					out_reserved += 1
				},
				PeerState::Connected { direction: Direction::Outbound(Reserved::No) } => {
					out_non_reserved += 1
				},
				PeerState::Disconnected => num_disconnected += 1,
				PeerState::Backoff => num_backoff += 1,

				_ => {},
			}
		}

		self.metrics.set_peerset_num_connected(
			&self.protocol,
			in_reserved,
			in_non_reserved,
			out_reserved,
			out_non_reserved,
			num_disconnected,
			num_backoff,
		);
	}

	/// Connect to all reserved peers.
	///
	/// Under the following conditions:
	/// 1. The peer must be present to the current set of peers.
	/// 2. The peer must be disconnected.
	/// 3. The peer must not be banned.
	///
	/// All reserved peers returned are transitioned to the `PeerState::Opening` state.
	fn connect_reserved_peers(&mut self) -> Vec<PeerId> {
		self.reserved_peers
			.iter()
			.filter_map(|peer| {
				let peer_state = self.peers.get(peer);
				if peer_state != Some(&PeerState::Disconnected) {
					return None;
				}

				if self.peerstore_handle.is_banned(peer) {
					return None;
				}

				// Transition peer to the opening state.
				self.peers.insert(
					*peer,
					PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) },
				);
				Some(*peer)
			})
			.collect::<Vec<_>>()
	}

	/// Get the number of inbound peers.
	#[cfg(test)]
	pub fn num_in(&self) -> usize {
		self.num_in
	}

	/// Get the number of outbound peers.
	#[cfg(test)]
	pub fn num_out(&self) -> usize {
		self.num_out
	}

	/// Get reference to known peers.
	#[cfg(test)]
	pub fn peers(&self) -> &HashMap<PeerId, PeerState> {
		&self.peers
	}

	/// Get reference to known peers.
	#[cfg(test)]
	pub fn peers_mut(&mut self) -> &mut HashMap<PeerId, PeerState> {
		&mut self.peers
	}

	/// Get reference to reserved peers.
	#[cfg(test)]
	pub fn reserved_peers(&self) -> &HashSet<PeerId> {
		&self.reserved_peers
	}
}

impl Stream for Peerset {
	type Item = PeersetNotificationCommand;

	fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		while let Poll::Ready(Some((peer, reputation))) = self.pending_backoffs.poll_next_unpin(cx)
		{
			log::trace!(target: LOG_TARGET, "{}: backoff expired for {peer:?}", self.protocol);

			if std::matches!(self.peers.get(&peer), None | Some(PeerState::Backoff)) {
				self.peers.insert(peer, PeerState::Disconnected);
			}

			self.peerstore_handle.report_peer(peer, reputation);
		}

		if let Poll::Ready(Some(action)) = Pin::new(&mut self.cmd_rx).poll_next(cx) {
			log::trace!(target: LOG_TARGET, "{}: received command {action:?}", self.protocol);

			match action {
				PeersetCommand::DisconnectPeer { peer } if !self.reserved_peers.contains(&peer) => {
					match self.peers.remove(&peer) {
						Some(PeerState::Connected { direction }) => {
							log::trace!(
								target: LOG_TARGET,
								"{}: close connection to {peer:?}, direction {direction:?}",
								self.protocol,
							);

							self.peers.insert(peer, PeerState::Closing { direction });
							return Poll::Ready(Some(PeersetNotificationCommand::close_substream(
								vec![peer],
							)));
						},
						Some(PeerState::Backoff) => {
							log::trace!(
								target: LOG_TARGET,
								"{}: cannot disconnect {peer:?}, already backed-off",
								self.protocol,
							);

							self.peers.insert(peer, PeerState::Backoff);
						},
						// substream might have been opening but not yet fully open when the
						// protocol or `Peerstore` request the connection to be closed
						//
						// if the substream opens successfully, close it immediately and mark the
						// peer as `Disconnected`
						Some(PeerState::Opening { direction }) => {
							log::trace!(
								target: LOG_TARGET,
								"{}: canceling substream to disconnect peer {peer:?}",
								self.protocol,
							);

							self.peers.insert(peer, PeerState::Canceled { direction });
						},
						// protocol had issued two disconnection requests in rapid succession and
						// the substream hadn't closed before the second disconnection request was
						// received, this is harmless and can be ignored.
						Some(state @ PeerState::Closing { .. }) => {
							log::trace!(
								target: LOG_TARGET,
								"{}: cannot disconnect {peer:?}, already closing ({state:?})",
								self.protocol,
							);

							self.peers.insert(peer, state);
						},
						// if peer is banned, e.g. due to genesis mismatch, `Peerstore` will issue a
						// global disconnection request to all protocols, irrespective of the
						// connectivity state. Peer isn't necessarily connected to all protocols at
						// all times so this is a harmless state to be in if a disconnection request
						// is received.
						Some(state @ PeerState::Disconnected) => {
							self.peers.insert(peer, state);
						},
						// peer had an opening substream earlier which was canceled and then,
						// e.g., the peer was banned which caused it to be disconnected again
						Some(state @ PeerState::Canceled { .. }) => {
							log::debug!(
								target: LOG_TARGET,
								"{}: cannot disconnect {peer:?}, already canceled ({state:?})",
								self.protocol,
							);

							self.peers.insert(peer, state);
						},
						// peer doesn't exist
						//
						// this can happen, for example, when peer connects over
						// `/block-announces/1` and it has wrong genesis hash which initiates a ban
						// for that peer. Since the ban is reported to all protocols but the peer
						// mightn't have been registered to GRANDPA or transactions yet, the peer
						// doesn't exist in their `Peerset`s and the error can just be ignored.
						None => {
							log::debug!(target: LOG_TARGET, "{}: {peer:?} doesn't exist", self.protocol);
						},
					}
				},
				PeersetCommand::DisconnectPeer { peer } => {
					log::debug!(
						target: LOG_TARGET,
						"{}: ignoring disconnection request for reserved peer {peer}",
						self.protocol,
					);
				},
				// set new reserved peers for the protocol
				//
				// Current reserved peers not in the new set are moved to the regular set of peers
				// or disconnected (if there are no slots available). The new reserved peers are
				// scheduled for outbound substreams
				PeersetCommand::SetReservedPeers { peers } => {
					log::debug!(target: LOG_TARGET, "{}: set reserved peers {peers:?}", self.protocol);

					// reserved peers don't consume any slots so if there are any regular connected
					// peers, inbound/outbound slot count must be adjusted to not account for these
					// peers anymore
					//
					// calculate how many of the previously connected peers were counted as regular
					// peers and substract these counts from `num_out`/`num_in`
					//
					// If a reserved peer is not already tracked, it is added as disconnected by
					// `calculate_slot_adjustment`. This ensures at the next slot allocation (1sec)
					// that we'll try to establish a connection with the reserved peer.
					let (in_peers, out_peers) = self.calculate_slot_adjustment(peers.iter());
					self.num_out -= out_peers;
					self.num_in -= in_peers;

					// collect all *reserved* peers who are not in the new reserved set
					let reserved_peers_maybe_remove =
						self.reserved_peers.difference(&peers).cloned().collect::<Vec<_>>();

					self.reserved_peers = peers;

					let peers_to_remove = reserved_peers_maybe_remove
						.into_iter()
						.filter(|peer| {
							match self.peers.remove(&peer) {
								Some(PeerState::Connected { mut direction }) => {
									// The direction contains a `Reserved::Yes` flag, because this
									// is a reserve peer that we want to close.
									// The `Reserved::Yes` ensures we don't adjust the slot count
									// when the substream is closed.

									let disconnect =
										self.reserved_only || self.should_disconnect(direction);

									if disconnect {
										log::trace!(
											target: LOG_TARGET,
											"{}: close connection to previously reserved {peer:?}, direction {direction:?}",
											self.protocol,
										);

										self.peers.insert(*peer, PeerState::Closing { direction });
										true
									} else {
										log::trace!(
											target: LOG_TARGET,
											"{}: {peer:?} is no longer reserved, move to regular peers, direction {direction:?}",
											self.protocol,
										);

										// The peer is kept connected as non-reserved. This will
										// further count towards the slot count.
										direction.set_reserved(Reserved::No);
										self.increment_slot(direction);

										self.peers
											.insert(*peer, PeerState::Connected { direction });
										false
									}
								},
								// substream might have been opening but not yet fully open when
								// the protocol request the reserved set to be changed
								Some(PeerState::Opening { direction }) => {
									log::trace!(
										target: LOG_TARGET,
										"{}: cancel substream to {peer:?}, direction {direction:?}",
										self.protocol,
									);

									self.peers.insert(*peer, PeerState::Canceled { direction });
									false
								},
								Some(state) => {
									self.peers.insert(*peer, state);
									false
								},
								None => {
									log::debug!(target: LOG_TARGET, "{}: {peer:?} doesn't exist", self.protocol);
									debug_assert!(false);
									false
								},
							}
						})
						.collect();

					// Open substreams to the new reserved peers that are disconnected.
					// This ensures we are not relying on the slot allocation timer to connect to
					// the new reserved peers. Therefore, we start connecting to them immediately.
					let connect_to = self.connect_reserved_peers();
					let command = PeersetNotificationCommand {
						open_peers: connect_to,
						close_peers: peers_to_remove,
					};

					log::trace!(
						target: LOG_TARGET,
						"{}: SetReservedPeers result {command:?}",
						self.protocol,
					);

					return Poll::Ready(Some(command));
				},
				PeersetCommand::AddReservedPeers { peers } => {
					log::debug!(target: LOG_TARGET, "{}: add reserved peers {peers:?}", self.protocol);

					// reserved peers don't consume any slots so if there are any regular connected
					// peers, inbound/outbound slot count must be adjusted to not account for these
					// peers anymore
					//
					// calculate how many of the previously connected peers were counted as regular
					// peers and substract these counts from `num_out`/`num_in`
					let (in_peers, out_peers) = self.calculate_slot_adjustment(peers.iter());
					self.num_out -= out_peers;
					self.num_in -= in_peers;

					let peers = peers
						.iter()
						.filter_map(|peer| {
							if !self.reserved_peers.insert(*peer) {
								log::warn!(
									target: LOG_TARGET,
									"{}: {peer:?} is already a reserved peer",
									self.protocol,
								);
								return None;
							}

							std::matches!(
								self.peers.get_mut(peer),
								None | Some(PeerState::Disconnected)
							)
							.then(|| {
								self.peers.insert(
									*peer,
									PeerState::Opening {
										direction: Direction::Outbound(Reserved::Yes),
									},
								);
								*peer
							})
						})
						.collect();

					log::debug!(target: LOG_TARGET, "{}: start connecting to {peers:?}", self.protocol);

					return Poll::Ready(Some(PeersetNotificationCommand::open_substream(peers)));
				},
				PeersetCommand::RemoveReservedPeers { peers } => {
					log::debug!(target: LOG_TARGET, "{}: remove reserved peers {peers:?}", self.protocol);

					let peers_to_remove = peers
						.iter()
						.filter_map(|peer| {
							if !self.reserved_peers.remove(peer) {
								log::debug!(
									target: LOG_TARGET,
									"{}: {peer} is not a reserved peer",
									self.protocol,
								);
								return None
							}

							match self.peers.remove(peer)? {
								// peer might have already disconnected by the time request to
								// disconnect them was received and the peer was backed off but
								// it had no expired by the time the request to disconnect the
								// peer was received
								PeerState::Backoff => {
									log::trace!(
										target: LOG_TARGET,
										"{}: cannot disconnect removed reserved peer {peer:?}, already backed-off",
										self.protocol,
									);

									self.peers.insert(*peer, PeerState::Backoff);
									None
								},

								// if there is a rapid change in substream state, the peer may
								// be canceled when the substream is asked to be closed.
								//
								// this can happen if substream is first opened and the very
								// soon after canceled. The substream may not have had time to
								// open yet and second open is ignored. If the substream is now
								// closed again before it has had time to open, it will be in
								// canceled state since `Peerset` is still waiting to hear
								// either success/failure on the original substream it tried to
								// cancel.
								PeerState::Canceled { direction } => {
									log::trace!(
										target: LOG_TARGET,
										"{}: cannot disconnect removed reserved peer {peer:?}, already canceled",
										self.protocol,
									);

									self.peers.insert(*peer, PeerState::Canceled { direction });
									None
								},

								// substream to the peer might have failed to open which caused
								// the peer to be backed off
								//
								// the back-off might've expired by the time the peer was
								// disconnected at which point the peer is already disconnected
								// when the protocol asked the peer to be disconnected
								PeerState::Disconnected => {
									log::trace!(
										target: LOG_TARGET,
										"{}: cannot disconnect removed reserved peer {peer:?}, already disconnected",
										self.protocol,
									);

									self.peers.insert(*peer, PeerState::Disconnected);
									None
								},

								// if a node disconnects, it's put into `PeerState::Closing`
								// which indicates that `Peerset` wants the substream closed and
								// has asked litep2p to close it but it hasn't yet received a
								// confirmation. If the peer is added as a reserved peer while
								// the substream is closing, the peer will remain in the closing
								// state as `Peerset` can't do anything with the peer until it
								// has heard from litep2p. It's possible that the peer is then
								// removed from the reserved set before substream close event
								// has been reported to `Peerset` (which the code below is
								// handling) and it will once again be ignored until the close
								// event is heard from litep2p.
								PeerState::Closing { direction } => {
									log::trace!(
										target: LOG_TARGET,
										"{}: cannot disconnect removed reserved peer {peer:?}, already closing",
										self.protocol,
									);

									self.peers.insert(*peer, PeerState::Closing { direction });
									None
								},
								// peer is currently connected as a reserved peer
								//
								// check if the peer can be accepted as a regular peer based on its
								// substream direction and available slots
								//
								// if there are enough slots, the peer is just converted to
								// a regular peer and the used slot count is increased and if the
								// peer cannot be accepted, litep2p is asked to close the substream.
								PeerState::Connected { mut direction } => {
									let disconnect = self.should_disconnect(direction);

									if disconnect {
										log::trace!(
											target: LOG_TARGET,
											"{}: close connection to removed reserved {peer:?}, direction {direction:?}",
											self.protocol,
										);

										self.peers.insert(*peer, PeerState::Closing { direction });
										Some(*peer)
									} else {
										log::trace!(
											target: LOG_TARGET,
											"{}: {peer:?} converted to regular peer {peer:?} direction {direction:?}",
											self.protocol,
										);

										// The peer is kept connected as non-reserved. This will
										// further count towards the slot count.
										direction.set_reserved(Reserved::No);
										self.increment_slot(direction);

										self.peers
											.insert(*peer, PeerState::Connected { direction });

										None
									}
								},

								PeerState::Opening { mut direction } => {
									let disconnect = self.should_disconnect(direction);

									if disconnect {
										log::trace!(
											target: LOG_TARGET,
											"{}: cancel substream to disconnect removed reserved peer {peer:?}, direction {direction:?}",
											self.protocol,
										);

										self.peers.insert(
											*peer,
											PeerState::Canceled {
												direction
											},
										);
									} else {
										log::trace!(
											target: LOG_TARGET,
											"{}: {peer:?} converted to regular peer {peer:?} direction {direction:?}",
											self.protocol,
										);

										// The peer is kept connected as non-reserved. This will
										// further count towards the slot count.
										direction.set_reserved(Reserved::No);
										self.increment_slot(direction);

										self.peers
											.insert(*peer, PeerState::Opening { direction });
									}

									None
								},
							}
						})
						.collect();

					log::debug!(
						target: LOG_TARGET,
						"{}: close substreams to {peers_to_remove:?}",
						self.protocol,
					);

					return Poll::Ready(Some(PeersetNotificationCommand::close_substream(
						peers_to_remove,
					)));
				},
				PeersetCommand::SetReservedOnly { reserved_only } => {
					log::debug!(target: LOG_TARGET, "{}: set reserved only mode to {reserved_only}", self.protocol);

					// update mode and if it's set to true, disconnect all non-reserved peers
					self.reserved_only = reserved_only;

					if reserved_only {
						let peers_to_remove = self
							.peers
							.iter()
							.filter_map(|(peer, state)| {
								(!self.reserved_peers.contains(peer) &&
									std::matches!(state, PeerState::Connected { .. }))
								.then_some(*peer)
							})
							.collect::<Vec<_>>();

						// set peers to correct states

						// peers who are connected are move to [`PeerState::Closing`]
						// and peers who are already opening are moved to [`PeerState::Canceled`]
						// and if the substream for them opens, it will be closed right after.
						self.peers.iter_mut().for_each(|(_, state)| match state {
							PeerState::Connected { direction } => {
								*state = PeerState::Closing { direction: *direction };
							},
							// peer for whom a substream was opening are canceled and if the
							// substream opens successfully, it will be closed immediately
							PeerState::Opening { direction } => {
								*state = PeerState::Canceled { direction: *direction };
							},
							_ => {},
						});

						return Poll::Ready(Some(PeersetNotificationCommand::close_substream(
							peers_to_remove,
						)));
					}
				},
				PeersetCommand::GetReservedPeers { tx } => {
					let _ = tx.send(self.reserved_peers.iter().cloned().collect());
				},
			}
		}

		// periodically check if `Peerset` is currently not connected to some reserved peers
		// it should be connected to
		//
		// also check if there are free outbound slots and if so, fetch peers with highest
		// reputations from `Peerstore` and start opening substreams to these peers
		if let Poll::Ready(()) = Pin::new(&mut self.next_slot_allocation).poll(cx) {
			let mut connect_to = self.connect_reserved_peers();

			// if the number of outbound peers is lower than the desired amount of outbound peers,
			// query `PeerStore` and try to get a new outbound candidated.
			if self.num_out < self.max_out && !self.reserved_only {
				// From the candidates offered by the peerstore we need to ignore:
				// - all peers that are not in the `PeerState::Disconnected` state (ie they are
				//   connected / closing)
				// - reserved peers since we initiated a connection to them in the previous step
				let ignore: HashSet<PeerId> = self
					.peers
					.iter()
					.filter_map(|(peer, state)| {
						(!std::matches!(state, PeerState::Disconnected)).then_some(*peer)
					})
					.chain(self.reserved_peers.iter().cloned())
					.collect();

				let peers: Vec<_> =
					self.peerstore_handle.outgoing_candidates(self.max_out - self.num_out, ignore);

				if peers.len() > 0 {
					peers.iter().for_each(|peer| {
						self.peers.insert(
							*peer,
							PeerState::Opening { direction: Direction::Outbound(Reserved::No) },
						);
					});

					self.num_out += peers.len();
					connect_to.extend(peers);
				}
			}

			// Start timer for the next allocation and if there were peers which the `Peerset`
			// wasn't connected but should be, send command to litep2p to start opening substreams.
			self.next_slot_allocation = Delay::new(SLOT_ALLOCATION_FREQUENCY);

			// Update metrics on every tick of slot allocation. This ensures metrics are
			// eventually consistent at 1s intervals.
			self.update_slot_metrics();

			if !connect_to.is_empty() {
				log::trace!(
					target: LOG_TARGET,
					"{}: start connecting to peers {connect_to:?}",
					self.protocol,
				);

				return Poll::Ready(Some(PeersetNotificationCommand::open_substream(connect_to)));
			}
		}

		Poll::Pending
	}
}
