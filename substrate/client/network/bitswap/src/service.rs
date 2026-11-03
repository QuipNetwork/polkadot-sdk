// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Substrate.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate. If not, see <https://www.gnu.org/licenses/>.

//! Bitswap service for indexed transactions.
//!
//! Inbound requests are resolved against local storage and returned to the requesting peer.
//! Outbound requests are scheduled across connected peers and delivered through the user-facing
//! response stream.
//!
//! Both paths share one actor, which coordinates network events, request limits, retries, and
//! backpressure.

use super::{
	is_cid_supported, BitswapCommand, BitswapHandle, Cid, FetchItem, LOG_TARGET, MAX_WANTED_BLOCKS,
};
use crate::{
	handle::BitswapError,
	metrics::{
		errors as metric_errors, outbound_events, outcomes as metric_outcomes, BitswapMetrics,
	},
};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use litep2p::protocol::libp2p::bitswap::{
	BitswapEvent, BitswapHandle as Litep2pBitswapHandle, BlockPresenceType, ResponseType, WantType,
};
use prometheus_endpoint::Registry;
use rand::{seq::IteratorRandom, Rng};
use sc_client_api::BlockBackend;
use sc_network_common::role::Roles;
use sc_network_sync::{SyncEvent, SyncEventStream};
use slotmap::{new_key_type, SlotMap};
use smallvec::SmallVec;
use sp_core::H256;
use sp_runtime::traits::Block as BlockT;
use std::{
	collections::{hash_map::Entry, HashMap, HashSet, VecDeque},
	future::Future,
	pin::Pin,
	sync::Arc,
	time::Duration,
};
use tokio::{
	sync::{mpsc, OwnedSemaphorePermit, Semaphore},
	time::Instant,
};

/// Transport boundary used by the Bitswap actor.
/// Implementations must verify block bytes before emitting `VerifiedBlock`.
#[async_trait]
trait BitswapTransport: Send {
	async fn next_event(&mut self) -> Option<TransportEvent>;
	async fn send_request(&self, peer: litep2p::PeerId, cids: Vec<(Cid, WantType)>);
	async fn send_response(&self, peer: litep2p::PeerId, responses: Vec<ResponseType>);
}

enum TransportEvent {
	Request { peer: litep2p::PeerId, cids: Vec<(Cid, WantType)> },
	Response { peer: litep2p::PeerId, responses: Vec<TransportResponse> },
}

enum TransportResponse {
	/// A block whose CID is guaranteed by the transport to match `bytes`. litep2p derives
	/// the CID from the received bytes (the wire format only carries a CID prefix), so
	/// the pairing needs no further verification; any other transport must uphold this.
	VerifiedBlock {
		cid: Cid,
		bytes: Bytes,
	},
	Presence {
		cid: Cid,
		presence: BlockPresenceType,
	},
}

/// Sends are bounded by [`TRANSPORT_SEND_TIMEOUT`]: the channels in both directions are
/// bounded and litep2p blocks when the event channel towards us is full, so unbounded sends
/// on our side could deadlock both tasks if both channels ever fill up simultaneously.
/// Dropping is safe: wants are retried and remote peers re-request lost responses.
#[async_trait]
impl BitswapTransport for Litep2pBitswapHandle {
	async fn next_event(&mut self) -> Option<TransportEvent> {
		StreamExt::next(self).await.map(|event| match event {
			BitswapEvent::Request { peer, cids } => TransportEvent::Request { peer, cids },
			BitswapEvent::Response { peer, responses } => TransportEvent::Response {
				peer,
				responses: responses
					.into_iter()
					.map(|response| match response {
						ResponseType::Block { cid, block } => {
							TransportResponse::VerifiedBlock { cid, bytes: block.into() }
						},
						ResponseType::Presence { cid, presence } => {
							TransportResponse::Presence { cid, presence }
						},
					})
					.collect(),
			},
		})
	}

	async fn send_request(&self, peer: litep2p::PeerId, cids: Vec<(Cid, WantType)>) {
		let send = Litep2pBitswapHandle::send_request(self, peer, cids);
		if tokio::time::timeout(TRANSPORT_SEND_TIMEOUT, send).await.is_err() {
			log::warn!(
				target: LOG_TARGET,
				"litep2p command channel congested; dropped bitswap request to {peer:?}",
			);
		}
	}

	async fn send_response(&self, peer: litep2p::PeerId, responses: Vec<ResponseType>) {
		let send = Litep2pBitswapHandle::send_response(self, peer, responses);
		if tokio::time::timeout(TRANSPORT_SEND_TIMEOUT, send).await.is_err() {
			log::warn!(
				target: LOG_TARGET,
				"litep2p command channel congested; dropped bitswap response to {peer:?}",
			);
		}
	}
}

const MAX_LIVE_CIDS: usize = 1024;
const MAX_CONCURRENT_INBOUND_LOOKUPS: usize = 8;
const MAX_QUEUED_INBOUND_ENTRIES_PER_PEER: usize = MAX_LIVE_CIDS;
const CMD_CHANNEL_CAPACITY: usize = 256;
/// Upper bound on one send into the litep2p command channel; see the
/// [`BitswapTransport`] impl for [`Litep2pBitswapHandle`].
const TRANSPORT_SEND_TIMEOUT: Duration = Duration::from_secs(10);
const PER_PEER_TIMEOUT: Duration = Duration::from_secs(5);
const ROUND_RETRY_DELAY: Duration = Duration::from_secs(5);
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

new_key_type! { struct UserRequestId; }

#[derive(Clone, Copy, Default)]
enum CidRequestPhase {
	#[default]
	Ready,
	Queued {
		retry_at: Option<Instant>,
	},
	InFlight {
		peer: litep2p::PeerId,
		deadline: Instant,
	},
	RetryAt(Instant),
}

#[derive(Default)]
struct CidRequestState {
	tried_peers: HashSet<litep2p::PeerId>,
	user_requests: SmallVec<[UserRequestId; 2]>,
	phase: CidRequestPhase,
}

impl CidRequestState {
	fn has_user_requests(&self) -> bool {
		!self.user_requests.is_empty()
	}

	fn is_idle(&self) -> bool {
		self.user_requests.is_empty() && !matches!(self.phase, CidRequestPhase::InFlight { .. })
	}

	fn is_queued(&self) -> bool {
		matches!(self.phase, CidRequestPhase::Queued { .. })
	}

	/// Moves a ready CID into the pending queue while preserving its retry deadline.
	/// Returns `false` when the CID is already queued or in flight.
	fn queue(&mut self) -> bool {
		let retry_at = match self.phase {
			CidRequestPhase::Ready => None,
			CidRequestPhase::RetryAt(at) => Some(at),
			CidRequestPhase::Queued { .. } | CidRequestPhase::InFlight { .. } => return false,
		};
		self.phase = CidRequestPhase::Queued { retry_at };
		true
	}

	/// Removes a CID from the pending queue and restores its prior scheduling phase.
	/// Returns `false` when the state was not queued.
	fn dequeue(&mut self) -> bool {
		let CidRequestPhase::Queued { retry_at } = self.phase else { return false };
		self.phase = retry_at.map_or(CidRequestPhase::Ready, CidRequestPhase::RetryAt);
		true
	}

	/// Finishes the active request only when `peer` currently owns it.
	/// The return value indicates whether an in-flight slot was released.
	fn finish_peer(&mut self, peer: litep2p::PeerId) -> bool {
		let CidRequestPhase::InFlight { peer: active_peer, .. } = self.phase else { return false };
		if active_peer != peer {
			return false;
		}
		self.phase = CidRequestPhase::Ready;
		true
	}

	/// Parks an exhausted CID until `at`, including while it waits in the queue.
	/// Returns `false` when a retry is already scheduled or work remains in flight.
	fn schedule_retry(&mut self, at: Instant) -> bool {
		match &mut self.phase {
			CidRequestPhase::Ready => self.phase = CidRequestPhase::RetryAt(at),
			CidRequestPhase::Queued { retry_at: slot @ None } => *slot = Some(at),
			CidRequestPhase::Queued { retry_at: Some(_) } |
			CidRequestPhase::InFlight { .. } |
			CidRequestPhase::RetryAt(_) => return false,
		}
		true
	}

	/// Starts a new peer-selection round once the retry deadline has elapsed.
	/// Peer history is cleared so every connected peer becomes eligible again.
	fn restart_round(&mut self, now: Instant) -> bool {
		self.phase = match self.phase {
			CidRequestPhase::RetryAt(at) if at <= now => CidRequestPhase::Ready,
			CidRequestPhase::Queued { retry_at: Some(at) } if at <= now => {
				CidRequestPhase::Queued { retry_at: None }
			},
			_ => return false,
		};
		self.tried_peers.clear();
		true
	}
}

struct RequestScheduler {
	cid_states: HashMap<Cid, CidRequestState>,
	/// FIFO of CIDs waiting for a free dispatch-window slot. May contain stale entries
	/// (resolved, abandoned or already-dispatched CIDs); those are skipped on pop, guarded
	/// by [`CidRequestState::is_queued`].
	pending: VecDeque<Cid>,
	/// Number of CIDs with at least one in-flight peer request.
	live_cids: usize,
	/// Number of queued [`CidRequestState`] entries, maintained incrementally: the
	/// metrics path reads it after every actor event, so recomputing it by scanning
	/// `cid_states` would make large requests quadratic.
	queued_cids: usize,
	/// Dispatch-window size.
	max_live_cids: usize,
}

impl RequestScheduler {
	fn new(max_live_cids: usize) -> Self {
		Self {
			cid_states: HashMap::new(),
			pending: VecDeque::new(),
			live_cids: 0,
			queued_cids: 0,
			max_live_cids,
		}
	}

	fn contains(&self, cid: &Cid) -> bool {
		self.cid_states.contains_key(cid)
	}

	fn add_user_request(&mut self, cid: Cid, user_request: UserRequestId) {
		self.cid_states.entry(cid).or_default().user_requests.push(user_request);
	}

	/// Detaches a user request from `cid` and removes newly idle state.
	/// In-flight state remains until its response, timeout, or disconnect arrives.
	fn remove_user_request(&mut self, cid: Cid, user_request: UserRequestId) {
		if let Some(cid_state) = self.cid_states.get_mut(&cid) {
			cid_state.user_requests.retain(|r| *r != user_request);
		}
		self.remove_if_idle(cid);
	}

	fn all_cids(&self) -> Vec<Cid> {
		self.cid_states.keys().copied().collect()
	}

	/// Removes a delivered CID and returns every user request that should receive it.
	/// Scheduler counters are released according to the CID's previous phase.
	fn take_user_requests_for_delivered_cid(
		&mut self,
		cid: Cid,
	) -> Option<SmallVec<[UserRequestId; 2]>> {
		self.cid_states.remove(&cid).map(|cid_state| {
			if matches!(cid_state.phase, CidRequestPhase::InFlight { .. }) {
				self.live_cids -= 1;
			}
			if cid_state.is_queued() {
				self.queued_cids -= 1;
			}
			cid_state.user_requests
		})
	}

	fn has_window_capacity(&self) -> bool {
		self.live_cids < self.max_live_cids
	}

	/// Pops the next still-valid queued CID and updates its queue accounting.
	/// Stale FIFO entries are skipped until a queued state is found.
	fn pop_pending(&mut self) -> Option<Cid> {
		while let Some(cid) = self.pending.pop_front() {
			if let Some(cid_state) = self.cid_states.get_mut(&cid) {
				if cid_state.dequeue() {
					self.queued_cids -= 1;
					return Some(cid);
				}
			}
		}
		None
	}

	/// Selects an eligible peer for `cid`, queueing it when the dispatch window is full.
	/// A selected peer owns one live slot until completion, timeout, or disconnect.
	fn next_peer_to_request<R: Rng + ?Sized>(
		&mut self,
		cid: Cid,
		connected_peers: &HashSet<litep2p::PeerId>,
		now: Instant,
		rng: &mut R,
	) -> Option<litep2p::PeerId> {
		let has_window_capacity = self.has_window_capacity();
		let cid_state = self.cid_states.get_mut(&cid)?;
		if !cid_state.has_user_requests() ||
			matches!(cid_state.phase, CidRequestPhase::InFlight { .. })
		{
			return None;
		}

		if !has_window_capacity {
			// Preserve the CID for promotion when a dispatch slot opens.
			if cid_state.queue() {
				self.pending.push_back(cid);
				self.queued_cids += 1;
			}
			return None;
		}

		let is_untried = |peer: &litep2p::PeerId| !cid_state.tried_peers.contains(peer);
		let Some(peer) = connected_peers
			.iter()
			.filter(|peer| is_untried(peer))
			.choose(&mut *rng)
			.copied()
		else {
			if !connected_peers.is_empty() {
				// Retry later once every connected peer has been tried.
				if cid_state.schedule_retry(now + ROUND_RETRY_DELAY) {
					log::trace!(
						target: LOG_TARGET,
						"all peers tried for {cid}, scheduling new round",
					);
				}
			}
			return None;
		};

		if cid_state.is_queued() {
			self.queued_cids -= 1;
		}
		self.live_cids += 1;
		cid_state.phase = CidRequestPhase::InFlight { peer, deadline: now + PER_PEER_TIMEOUT };

		Some(peer)
	}

	/// Records a terminal response from `peer` and releases its active slot.
	/// Late responses cannot finish a newer request owned by a different peer.
	fn mark_peer_done_for_cid(&mut self, peer: litep2p::PeerId, cid: Cid) {
		if let Some(cid_state) = self.cid_states.get_mut(&cid) {
			if cid_state.finish_peer(peer) {
				self.live_cids -= 1;
			}
			cid_state.tried_peers.insert(peer);
		}
		self.remove_if_idle(cid);
	}

	/// Releases every active request owned by a disconnected peer.
	/// Returned CIDs are still wanted and ready for immediate failover.
	fn remove_in_flight_peer(&mut self, peer: litep2p::PeerId) -> Vec<Cid> {
		let mut affected_cids = Vec::new();
		for (cid, cid_state) in self.cid_states.iter_mut() {
			if cid_state.finish_peer(peer) {
				self.live_cids -= 1;
				affected_cids.push(*cid);
			}
		}

		self.remove_idle_and_filter_existing(affected_cids)
	}

	/// Expire in-flight requests whose per-peer deadline passed. Returns the affected CIDs
	/// still wanted (for re-dispatch) and the total number of timed-out peer requests.
	fn expire_peer_timeouts(&mut self, now: Instant) -> (Vec<Cid>, usize) {
		let mut timed_out_cids = Vec::new();
		for (cid, cid_state) in self.cid_states.iter_mut() {
			if let CidRequestPhase::InFlight { peer, deadline } = cid_state.phase {
				if deadline <= now {
					cid_state.phase = CidRequestPhase::Ready;
					cid_state.tried_peers.insert(peer);
					timed_out_cids.push(*cid);
				}
			}
		}
		let timed_out_count = timed_out_cids.len();
		self.live_cids -= timed_out_count;

		(self.remove_idle_and_filter_existing(timed_out_cids), timed_out_count)
	}

	/// Restarts every exhausted CID whose retry delay has elapsed.
	/// Returned CIDs should be reconsidered for immediate dispatch.
	fn restart_exhausted_rounds(&mut self, now: Instant) -> Vec<Cid> {
		let mut restarted_cids = Vec::new();
		for (cid, cid_state) in self.cid_states.iter_mut() {
			if cid_state.has_user_requests() && cid_state.restart_round(now) {
				restarted_cids.push(*cid);
			}
		}
		restarted_cids
	}

	fn clear(&mut self) {
		self.cid_states.clear();
		self.pending.clear();
		self.live_cids = 0;
		self.queued_cids = 0;
	}

	/// Removes idle states from `cids` and returns those still tracked.
	/// The filtered result is safe to feed back into peer selection.
	fn remove_idle_and_filter_existing(&mut self, cids: Vec<Cid>) -> Vec<Cid> {
		for cid in &cids {
			self.remove_if_idle(*cid);
		}
		cids.into_iter().filter(|cid| self.cid_states.contains_key(cid)).collect()
	}

	fn remove_if_idle(&mut self, cid: Cid) {
		let Entry::Occupied(entry) = self.cid_states.entry(cid) else { return };
		if !entry.get().is_idle() {
			return;
		}
		let cid_state = entry.remove();
		if cid_state.is_queued() {
			self.queued_cids -= 1;
		}
	}
}

struct UserRequest {
	cids_remaining: HashSet<Cid>,
	sink: mpsc::Sender<FetchItem>,
}

/// A served batch, carrying the worker permit: the slot is released only once the actor
/// has forwarded the responses, so downstream backpressure reaches the dispatch path and
/// at most [`MAX_CONCURRENT_INBOUND_LOOKUPS`] responses are retained at any time.
type InboundLookupResult = (litep2p::PeerId, Vec<ResponseType>, OwnedSemaphorePermit);

struct InboundLookupPool<B: BlockT> {
	client: Arc<dyn BlockBackend<B> + Send + Sync>,
	result_tx: mpsc::Sender<InboundLookupResult>,
	semaphore: Arc<Semaphore>,
	metrics: BitswapMetrics,
}

impl<B: BlockT> InboundLookupPool<B> {
	fn new(
		client: Arc<dyn BlockBackend<B> + Send + Sync>,
		max_lookups: usize,
		metrics: BitswapMetrics,
	) -> (Self, mpsc::Receiver<InboundLookupResult>) {
		let (result_tx, result_rx) = mpsc::channel(max_lookups);
		(
			Self { client, result_tx, semaphore: Arc::new(Semaphore::new(max_lookups)), metrics },
			result_rx,
		)
	}

	/// Reserves a worker slot, or returns `None` if all workers are busy.
	/// Dropping an unused permit immediately returns the slot.
	fn try_acquire_worker(&self) -> Option<OwnedSemaphorePermit> {
		self.semaphore.clone().try_acquire_owned().ok()
	}

	/// Serves a wantlist on a blocking worker occupying `permit`'s slot.
	/// The permit travels with the result so backpressure retains the slot.
	fn submit(
		&self,
		permit: OwnedSemaphorePermit,
		peer: litep2p::PeerId,
		cids: Vec<(Cid, WantType)>,
	) {
		let client = self.client.clone();
		let result_tx = self.result_tx.clone();
		let metrics = self.metrics.clone();
		tokio::task::spawn_blocking(move || {
			let responses = serve_inbound(&*client, cids, &metrics);
			let _ = result_tx.blocking_send((peer, responses, permit));
		});
	}
}

/// Fair per-peer queues. Overflow is dropped rather than reported as `DONT_HAVE`.
struct InboundQueue {
	per_peer: HashMap<litep2p::PeerId, VecDeque<(Cid, WantType)>>,
	rotation: VecDeque<litep2p::PeerId>,
	max_entries_per_peer: usize,
}

impl InboundQueue {
	fn new(max_entries_per_peer: usize) -> Self {
		Self { per_peer: HashMap::new(), rotation: VecDeque::new(), max_entries_per_peer }
	}

	/// Queue `entries` for `peer`. Returns the number of entries dropped because the
	/// peer's queue is full.
	fn enqueue(&mut self, peer: litep2p::PeerId, mut entries: Vec<(Cid, WantType)>) -> usize {
		if entries.is_empty() {
			return 0;
		}
		let queue = self.per_peer.entry(peer).or_default();
		let free_slots = self.max_entries_per_peer.saturating_sub(queue.len());
		let dropped = entries.len().saturating_sub(free_slots);
		entries.truncate(free_slots);
		// Enter the rotation only on the empty-to-nonempty transition.
		if queue.is_empty() && !entries.is_empty() {
			self.rotation.push_back(peer);
		}
		queue.extend(entries);
		if queue.is_empty() {
			self.per_peer.remove(&peer);
		}
		dropped
	}

	/// Drops all queued entries of a peer, returning how many were removed.
	fn remove_peer(&mut self, peer: &litep2p::PeerId) -> usize {
		let removed = self.per_peer.remove(peer).map_or(0, |queue| queue.len());
		if removed > 0 {
			self.rotation.retain(|rotated| rotated != peer);
		}
		removed
	}

	/// Take the next batch of up to [`MAX_WANTED_BLOCKS`] entries, rotating the serviced
	/// peer to the back of the queue.
	fn next_batch(&mut self) -> Option<(litep2p::PeerId, Vec<(Cid, WantType)>)> {
		while let Some(peer) = self.rotation.pop_front() {
			let Some(queue) = self.per_peer.get_mut(&peer) else {
				log::error!(target: LOG_TARGET, "stale peer in inbound queue rotation: {peer}");
				continue;
			};
			if queue.is_empty() {
				log::error!(target: LOG_TARGET, "empty peer queue in inbound rotation: {peer}");
				self.per_peer.remove(&peer);
				continue;
			}

			let batch: Vec<_> = queue.drain(..queue.len().min(MAX_WANTED_BLOCKS)).collect();
			if queue.is_empty() {
				self.per_peer.remove(&peer);
			} else {
				self.rotation.push_back(peer);
			}
			return Some((peer, batch));
		}
		None
	}
}

pub(crate) struct BitswapService<B: BlockT> {
	handle: Box<dyn BitswapTransport>,

	cmd_rx: mpsc::Receiver<BitswapCommand>,
	cmd_channel_closed: bool,
	sync_event_stream: Pin<Box<dyn Stream<Item = SyncEvent> + Send>>,
	/// Runs blocking local transaction lookups with bounded concurrency.
	inbound_lookup_pool: InboundLookupPool<B>,
	inbound_lookup_rx: mpsc::Receiver<InboundLookupResult>,
	/// Buffers inbound wantlist entries fairly per peer until a lookup worker is available.
	inbound_queue: InboundQueue,

	connected_peers: HashSet<litep2p::PeerId>,
	/// Coordinates each requested CID across user requests, peers, retries, and dispatch slots.
	scheduler: RequestScheduler,
	/// Active user-facing requests that receive blocks resolved by the scheduler.
	user_requests: SlotMap<UserRequestId, UserRequest>,
	metrics: BitswapMetrics,
}

/// Build the Bitswap service, returning the service future and the user-facing handle.
pub fn start<B: BlockT, S>(
	client: Arc<dyn BlockBackend<B> + Send + Sync>,
	sync: &S,
	litep2p_handle: Litep2pBitswapHandle,
	metrics_registry: Option<&Registry>,
) -> (Pin<Box<dyn Future<Output = ()> + Send>>, BitswapHandle)
where
	S: SyncEventStream + ?Sized,
{
	let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAPACITY);
	let metrics = BitswapMetrics::new(metrics_registry).unwrap_or_else(|err| {
		log::debug!(target: LOG_TARGET, "failed to register bitswap metrics: {err}");
		BitswapMetrics::default()
	});
	let (inbound_lookup_pool, inbound_lookup_rx) =
		InboundLookupPool::new(client, MAX_CONCURRENT_INBOUND_LOOKUPS, metrics.clone());

	let user_handle = BitswapHandle::new(cmd_tx);
	let sync_event_stream = sync.event_stream("bitswap");

	let service = BitswapService {
		handle: Box::new(litep2p_handle),
		cmd_rx,
		cmd_channel_closed: false,
		sync_event_stream,
		inbound_lookup_pool,
		inbound_lookup_rx,
		inbound_queue: InboundQueue::new(MAX_QUEUED_INBOUND_ENTRIES_PER_PEER),
		connected_peers: HashSet::new(),
		scheduler: RequestScheduler::new(MAX_LIVE_CIDS),
		user_requests: SlotMap::with_key(),
		metrics,
	};

	let future = Box::pin(async move { service.run().await });

	(future, user_handle)
}

impl<B: BlockT> BitswapService<B> {
	/// Runs the service actor until a required input stream closes.
	/// Each event is handled serially before metrics are refreshed.
	async fn run(mut self) {
		log::debug!(target: LOG_TARGET, "BitswapService starting");
		let mut sweep_ticker = tokio::time::interval(SWEEP_INTERVAL);
		sweep_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
		sweep_ticker.tick().await;
		self.update_metrics();

		loop {
			tokio::select! {
				event = self.handle.next_event() => match event {
					Some(TransportEvent::Request { peer, cids }) =>
						self.on_inbound_request(peer, cids),
					Some(TransportEvent::Response { peer, responses }) =>
						self.on_inbound_response(peer, responses).await,
					None => {
						log::debug!(target: LOG_TARGET, "litep2p bitswap stream ended; shutting down");
						self.shutdown_user_requests();
						return;
					},
				},

				cmd = self.cmd_rx.recv(), if !self.cmd_channel_closed => match cmd {
					Some(BitswapCommand::RequestStream { cids, sink }) =>
						self.on_request_stream(cids, sink).await,
					None => {
						log::debug!(
							target: LOG_TARGET,
							"all bitswap handles dropped; serving inbound requests only",
						);
						self.cmd_channel_closed = true;
					},
				},

				sync_event = self.sync_event_stream.next() => match sync_event {
					Some(SyncEvent::PeerConnected { peer_id, roles }) =>
						self.on_peer_connected(peer_id.into(), roles).await,
					Some(SyncEvent::PeerDisconnected(peer)) => self.on_peer_disconnected(peer.into()).await,
					None => {
						log::debug!(target: LOG_TARGET, "sync event stream ended; shutting down");
						self.shutdown_user_requests();
						return;
					},
				},

				Some((peer, responses, permit)) = self.inbound_lookup_rx.recv() => {
					self.handle.send_response(peer, responses).await;
					drop(permit);
					self.dispatch_inbound_lookups();
				},

				_ = sweep_ticker.tick() => {
					self.on_sweep().await;
				},
			}
			self.update_metrics();
		}
	}

	/// Admits a user wantlist and attaches one user request to all requested CIDs.
	async fn on_request_stream(&mut self, cids: HashSet<Cid>, sink: mpsc::Sender<FetchItem>) {
		let user_request_id =
			self.user_requests.insert(UserRequest { cids_remaining: cids.clone(), sink });

		for cid in &cids {
			self.scheduler.add_user_request(*cid, user_request_id);
		}

		self.top_up_in_flight(cids).await;
	}

	/// Schedules the supplied CIDs, then promotes queued CIDs until the dispatch window is full.
	/// Eligible wants are grouped by peer and sent in protocol-sized batches.
	async fn top_up_in_flight(&mut self, cids: impl IntoIterator<Item = Cid>) {
		let now = Instant::now();
		let wants_by_peer = {
			let mut rng = rand::thread_rng();
			let mut wants_by_peer: HashMap<litep2p::PeerId, Vec<(Cid, WantType)>> = HashMap::new();
			for cid in cids {
				if let Some(peer) =
					self.scheduler.next_peer_to_request(cid, &self.connected_peers, now, &mut rng)
				{
					log::trace!(target: LOG_TARGET, "WANT-BLOCK {cid} -> {peer:?}");
					wants_by_peer.entry(peer).or_default().push((cid, WantType::Block));
				}
			}

			// Fill remaining dispatch slots from the pending FIFO.
			while self.scheduler.has_window_capacity() {
				let Some(cid) = self.scheduler.pop_pending() else { break };
				if let Some(peer) =
					self.scheduler.next_peer_to_request(cid, &self.connected_peers, now, &mut rng)
				{
					log::trace!(target: LOG_TARGET, "WANT-BLOCK {cid} -> {peer:?} (promoted)");
					wants_by_peer.entry(peer).or_default().push((cid, WantType::Block));
				}
			}
			wants_by_peer
		};

		for (peer, wants) in wants_by_peer {
			self.metrics.record_outbound(outbound_events::REQUESTED, wants.len());
			for chunk in wants.chunks(MAX_WANTED_BLOCKS) {
				self.handle.send_request(peer, chunk.to_vec()).await;
			}
		}
	}

	/// Applies verified blocks and presence updates received from a peer.
	/// Unresolved presence responses are immediately reconsidered for dispatch.
	async fn on_inbound_response(
		&mut self,
		peer: litep2p::PeerId,
		responses: Vec<TransportResponse>,
	) {
		let mut cids_to_top_up: HashSet<Cid> = HashSet::new();

		for response in responses {
			match response {
				TransportResponse::VerifiedBlock { cid, bytes } => {
					// A late verified block may satisfy a want reassigned to another peer.
					self.scheduler.mark_peer_done_for_cid(peer, cid);

					if self.scheduler.contains(&cid) {
						self.deliver_block(cid, bytes);
					} else {
						log::debug!(
							target: LOG_TARGET,
							"{peer:?} sent unsolicited or unwanted block for {cid}",
						);
					}
				},
				TransportResponse::Presence { cid, presence } => match presence {
					BlockPresenceType::DontHave => {
						log::trace!(target: LOG_TARGET, "{peer:?} DONT_HAVE {cid}");
						self.scheduler.mark_peer_done_for_cid(peer, cid);
						cids_to_top_up.insert(cid);
					},
					BlockPresenceType::Have => {
						log::debug!(
							target: LOG_TARGET,
							"{peer:?} sent unsolicited HAVE for {cid}; only WANT-BLOCK is issued",
						);
					},
				},
			}
		}

		self.top_up_in_flight(cids_to_top_up).await;
	}

	/// Delivers a resolved block to every user request sharing its CID.
	/// The CID is removed once, releasing any replacement in-flight request.
	fn deliver_block(&mut self, cid: Cid, bytes: Bytes) {
		let Some(user_request_ids) = self.scheduler.take_user_requests_for_delivered_cid(cid)
		else {
			return;
		};
		self.metrics.record_outbound(outbound_events::DELIVERED, 1);

		for user_request_id in user_request_ids {
			let Some(user_request) = self.user_requests.get_mut(user_request_id) else { continue };
			if !user_request.cids_remaining.remove(&cid) {
				continue;
			}
			if user_request.sink.try_send(Ok((cid, bytes.clone()))).is_err() {
				log::trace!(target: LOG_TARGET, "user request sink full/closed for {cid}");
				self.drop_user_request(user_request_id);
				continue;
			}
			if user_request.cids_remaining.is_empty() {
				self.drop_user_request(user_request_id);
			}
		}
	}

	/// Removes a user request and detaches it from every unresolved CID.
	/// CID state is retained only while another user request or peer request needs it.
	fn drop_user_request(&mut self, id: UserRequestId) {
		let Some(user_request) = self.user_requests.remove(id) else { return };

		for cid in &user_request.cids_remaining {
			self.scheduler.remove_user_request(*cid, id);
		}
	}

	/// Adds a connected non-light peer and reconsiders every outstanding CID.
	/// Light peers are ignored because they do not store indexed transactions.
	async fn on_peer_connected(&mut self, peer: litep2p::PeerId, roles: Roles) {
		if roles.is_light() {
			return;
		}
		self.connected_peers.insert(peer);
		let cids = self.scheduler.all_cids();
		self.top_up_in_flight(cids).await;
	}

	/// Removes a peer and immediately fails over requests it owned.
	/// Late responses remain safe because peer ownership is checked on completion.
	/// Queued inbound wantlist entries of the peer are dropped so departed peers
	/// neither occupy queue capacity nor waste lookup work.
	async fn on_peer_disconnected(&mut self, peer: litep2p::PeerId) {
		self.connected_peers.remove(&peer);
		let dropped = self.inbound_queue.remove_peer(&peer);
		if dropped > 0 {
			log::trace!(
				target: LOG_TARGET,
				"dropped {dropped} queued inbound entries of disconnected {peer:?}",
			);
		}
		let cids_to_top_up = self.scheduler.remove_in_flight_peer(peer);
		self.top_up_in_flight(cids_to_top_up).await;
	}

	/// Periodic housekeeping: drop user requests whose receiver was dropped, expire per-peer
	/// request timeouts, and start new rounds for CIDs whose retry delay has passed.
	async fn on_sweep(&mut self) {
		let abandoned_user_requests: Vec<UserRequestId> = self
			.user_requests
			.iter()
			.filter_map(|(id, user_request)| user_request.sink.is_closed().then_some(id))
			.collect();
		self.metrics
			.record_outbound(outbound_events::ABANDONED, abandoned_user_requests.len());
		for id in abandoned_user_requests {
			log::trace!(target: LOG_TARGET, "dropping abandoned user request {id:?}");
			self.drop_user_request(id);
		}

		let now = Instant::now();
		let (mut cids_to_top_up, timed_out_count) = self.scheduler.expire_peer_timeouts(now);
		self.metrics.record_outbound(outbound_events::TIMED_OUT, timed_out_count);
		let restarted_cids = self.scheduler.restart_exhausted_rounds(now);
		self.metrics
			.record_outbound(outbound_events::ROUND_RESTARTED, restarted_cids.len());
		cids_to_top_up.extend(restarted_cids);
		self.top_up_in_flight(cids_to_top_up).await;

		// Recover capacity after a lookup task exits without a result.
		self.dispatch_inbound_lookups();
	}

	/// Queues an inbound peer wantlist and records entries dropped by backpressure.
	/// Available lookup workers are dispatched immediately after admission.
	fn on_inbound_request(&mut self, peer: litep2p::PeerId, cids: Vec<(Cid, WantType)>) {
		let dropped = self.inbound_queue.enqueue(peer, cids);
		if dropped > 0 {
			self.metrics.record_inbound_queue_overflow(dropped);
			log::debug!(
				target: LOG_TARGET,
				"inbound queue for {peer:?} full; dropped {dropped} wantlist entries",
			);
		}
		self.dispatch_inbound_lookups();
	}

	/// Starts queued inbound lookups while worker permits remain available.
	/// Work stays queued when either the pool is full or no batch is ready.
	fn dispatch_inbound_lookups(&mut self) {
		while let Some(permit) = self.inbound_lookup_pool.try_acquire_worker() {
			let Some((peer, batch)) = self.inbound_queue.next_batch() else { break };
			self.inbound_lookup_pool.submit(permit, peer, batch);
		}
	}

	fn update_metrics(&self) {
		self.metrics.set_state(
			self.scheduler.live_cids,
			self.scheduler.queued_cids,
			self.user_requests.len(),
		);
	}

	/// Notifies every user request that the service closed and clears scheduler state.
	/// Send failures are ignored because the corresponding receiver already vanished.
	fn shutdown_user_requests(&mut self) {
		for (_, user_request) in self.user_requests.drain() {
			let _ = user_request.sink.try_send(Err(BitswapError::ServiceClosed));
		}
		self.scheduler.clear();
		self.update_metrics();
	}
}

/// Resolves an inbound wantlist against locally indexed transactions.
/// Unsupported CIDs and missing transactions become `DONT_HAVE` responses.
fn serve_inbound<B: BlockT>(
	client: &(dyn BlockBackend<B> + Send + Sync),
	cids: Vec<(Cid, WantType)>,
	metrics: &BitswapMetrics,
) -> Vec<ResponseType> {
	let started = std::time::Instant::now();
	let mut responses = Vec::with_capacity(cids.len());
	for (cid, want_type) in cids {
		let response = {
			if !is_cid_supported(&cid) {
				metrics.record_entry(metric_outcomes::UNSUPPORTED_CID);
				responses
					.push(ResponseType::Presence { cid, presence: BlockPresenceType::DontHave });
				continue;
			}
			// Supported CIDs always carry a 32-byte digest.
			let hash = H256::from_slice(&cid.hash().digest()[0..32]);
			match want_type {
				// A HAVE query only needs an existence check: `has_indexed_transaction`
				// answers it via `Database::contains` without loading the transaction body.
				WantType::Have => match client.has_indexed_transaction(hash) {
					Ok(true) => ResponseType::Presence { cid, presence: BlockPresenceType::Have },
					Ok(false) => {
						ResponseType::Presence { cid, presence: BlockPresenceType::DontHave }
					},
					Err(e) => {
						metrics.record_error(metric_errors::CLIENT);
						log::error!(
							target: LOG_TARGET,
							"has_indexed_transaction({hash}) failed: {e}",
						);
						ResponseType::Presence { cid, presence: BlockPresenceType::DontHave }
					},
				},
				WantType::Block => match client.indexed_transaction(hash) {
					Ok(Some(transaction)) => ResponseType::Block { cid, block: transaction },
					Ok(None) => {
						ResponseType::Presence { cid, presence: BlockPresenceType::DontHave }
					},
					Err(e) => {
						metrics.record_error(metric_errors::CLIENT);
						log::error!(target: LOG_TARGET, "indexed_transaction({hash}) failed: {e}");
						ResponseType::Presence { cid, presence: BlockPresenceType::DontHave }
					},
				},
			}
		};
		metrics.record_response(&response);
		responses.push(response);
	}
	metrics.record_response_bytes(&responses);
	metrics.record_duration(started.elapsed());
	responses
}

#[cfg(test)]
mod tests;
