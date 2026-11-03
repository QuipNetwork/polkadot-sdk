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

//! Contains the state of the chain synchronization process
//!
//! At any given point in time, a running node tries as much as possible to be at the head of the
//! chain. This module handles the logic of which blocks to request from remotes, and processing
//! responses. It yields blocks to check and potentially move to the database.
//!
//! # Usage
//!
//! The `ChainSync` struct maintains the state of the block requests. Whenever something happens on
//! the network, or whenever a block has been successfully verified, call the appropriate method in
//! order to update it.

use crate::{
	block_relay_protocol::{BlockDownloader, BlockResponseError},
	blocks::{BlockCollection, Metrics as BlockCollectionMetrics},
	justification_requests::ExtraRequests,
	schema::v1::{StateRequest, StateResponse},
	service::network::NetworkServiceHandle,
	strategy::{
		disconnected_peers::DisconnectedPeers,
		state_sync::{ImportResult, StateSync, StateSyncProvider},
		warp::{WarpSyncPhase, WarpSyncProgress},
		StrategyKey, SyncingAction, SyncingStrategy,
	},
	types::{BadPeer, SyncState, SyncStatus},
	LOG_TARGET,
};

use codec::Encode;
use futures::{channel::oneshot, FutureExt};
use log::{debug, error, info, trace, warn};
use prometheus_endpoint::{register, Counter, Gauge, PrometheusError, Registry, U64};
use prost::Message;
use sc_client_api::{blockchain::BlockGap, BlockBackend, ProofProvider};
use sc_consensus::{BlockImportError, BlockImportStatus, IncomingBlock};
use sc_network::{IfDisconnected, ProtocolName};
use sc_network_common::sync::message::{
	BlockAnnounce, BlockAttributes, BlockData, BlockRequest, BlockResponse, Direction, FromBlock,
};
use sc_network_types::PeerId;
use sp_arithmetic::traits::Saturating;
use sp_blockchain::{Error as ClientError, HeaderBackend, HeaderMetadata};
use sp_consensus::{BlockOrigin, BlockStatus};
use sp_runtime::{
	traits::{
		Block as BlockT, CheckedSub, Header as HeaderT, NumberFor, One, SaturatedConversion, Zero,
	},
	EncodedJustification, Justifications,
};

use std::{
	any::Any,
	collections::{HashMap, HashSet},
	fmt,
	ops::{AddAssign, Range},
	sync::Arc,
};

#[cfg(test)]
mod test;

/// Maximum blocks to store in the import queue.
const MAX_IMPORTING_BLOCKS: usize = 2048;

/// Maximum blocks to download ahead of any gap.
const MAX_DOWNLOAD_AHEAD: u32 = 2048;

/// Maximum blocks to look backwards. The gap is the difference between the highest block and the
/// common block of a node.
const MAX_BLOCKS_TO_LOOK_BACKWARDS: u32 = MAX_DOWNLOAD_AHEAD / 2;

/// Pick the state to sync as the latest finalized number minus this.
const STATE_SYNC_FINALITY_THRESHOLD: u32 = 8;

/// We use a heuristic that with a high likelihood, by the time
/// `MAJOR_SYNC_BLOCKS` have been imported we'll be on the same
/// chain as (or at least closer to) the peer so we want to delay
/// the ancestor search to not waste time doing that when we are
/// so far behind.
const MAJOR_SYNC_BLOCKS: u8 = 5;

mod rep {
	use sc_network::ReputationChange as Rep;
	/// Reputation change when a peer sent us a message that led to a
	/// database read error.
	pub const BLOCKCHAIN_READ_ERROR: Rep = Rep::new(-(1 << 16), "DB Error");

	/// Reputation change when a peer sent us a status message with a different
	/// genesis than us.
	pub const GENESIS_MISMATCH: Rep = Rep::new(i32::MIN, "Genesis mismatch");

	/// Reputation change for peers which send us a block with an incomplete header.
	pub const INCOMPLETE_HEADER: Rep = Rep::new(-(1 << 20), "Incomplete header");

	/// Reputation change for peers which send us a block which we fail to verify.
	pub const VERIFICATION_FAIL: Rep = Rep::new(-(1 << 29), "Block verification failed");

	/// Reputation change for peers which send us a known bad block.
	pub const BAD_BLOCK: Rep = Rep::new(-(1 << 29), "Bad block");

	/// Peer did not provide us with advertised block data.
	pub const NO_BLOCK: Rep = Rep::new(-(1 << 29), "No requested block data");

	/// Reputation change for peers which send us non-requested block data.
	pub const NOT_REQUESTED: Rep = Rep::new(-(1 << 29), "Not requested block data");

	/// Peer could not serve any body of a gap sync request that required them.
	/// Mild, since the peer may still be backfilling its own block history.
	pub const NO_GAP_BODIES: Rep = Rep::new(-(1 << 26), "No gap sync bodies");

	/// Reputation change for peers which send us a block with bad justifications.
	pub const BAD_JUSTIFICATION: Rep = Rep::new(-(1 << 16), "Bad justification");

	/// Reputation change when a peer sent us invalid ancestry result.
	pub const UNKNOWN_ANCESTOR: Rep = Rep::new(-(1 << 16), "DB Error");

	/// Peer response data does not have requested bits.
	pub const BAD_RESPONSE: Rep = Rep::new(-(1 << 12), "Incomplete response");

	/// We received a message that failed to decode.
	pub const BAD_MESSAGE: Rep = Rep::new(-(1 << 12), "Bad message");
}

struct Metrics {
	queued_blocks: Gauge<U64>,
	fork_targets: Gauge<U64>,
	block_collection: BlockCollectionMetrics,
	gap_body_empty_responses: Counter<U64>,
	gap_header_only_downgrades: Counter<U64>,
	gap_oldest_required_body: Gauge<U64>,
}

impl Metrics {
	fn register(r: &Registry) -> Result<Self, PrometheusError> {
		Ok(Self {
			queued_blocks: {
				let g =
					Gauge::new("substrate_sync_queued_blocks", "Number of blocks in import queue")?;
				register(g, r)?
			},
			fork_targets: {
				let g = Gauge::new("substrate_sync_fork_targets", "Number of fork sync targets")?;
				register(g, r)?
			},
			block_collection: BlockCollectionMetrics::register(r)?,
			gap_body_empty_responses: {
				let c = Counter::new(
					"substrate_sync_gap_body_empty_responses_total",
					"Number of empty responses to gap sync requests that required bodies; \
					 each drops the responding peer",
				)?;
				register(c, r)?
			},
			gap_header_only_downgrades: {
				let c = Counter::new(
					"substrate_sync_gap_header_only_downgrades_total",
					"Number of gap sync requests issued header-only because the moving body \
					 cutoff passed the range",
				)?;
				register(c, r)?
			},
			gap_oldest_required_body: {
				let g = Gauge::new(
					"substrate_sync_gap_oldest_required_body",
					"Oldest block number for which gap sync still requires a body; \
					 0 when gap sync is inactive or bodies are not required",
				)?;
				register(g, r)?
			},
		})
	}
}

#[derive(Debug, Clone)]
enum AllowedRequests {
	Some(HashSet<PeerId>),
	All,
}

impl AllowedRequests {
	fn add(&mut self, id: &PeerId) {
		if let Self::Some(ref mut set) = self {
			set.insert(*id);
		}
	}

	fn take(&mut self) -> Self {
		std::mem::take(self)
	}

	fn set_all(&mut self) {
		*self = Self::All;
	}

	fn contains(&self, id: &PeerId) -> bool {
		match self {
			Self::Some(set) => set.contains(id),
			Self::All => true,
		}
	}

	fn is_empty(&self) -> bool {
		match self {
			Self::Some(set) => set.is_empty(),
			Self::All => false,
		}
	}

	fn clear(&mut self) {
		std::mem::take(self);
	}
}

impl Default for AllowedRequests {
	fn default() -> Self {
		Self::Some(HashSet::default())
	}
}

/// Statistics for gap sync operations.
#[derive(Debug, Default, Clone)]
struct GapSyncStats {
	/// Size of headers downloaded during gap sync
	header_bytes: usize,
	/// Size of bodies downloaded during gap sync
	body_bytes: usize,
	/// Size of justifications downloaded during gap sync
	justification_bytes: usize,
}

impl GapSyncStats {
	fn new() -> Self {
		Self::default()
	}

	fn total_bytes(&self) -> usize {
		self.header_bytes + self.body_bytes + self.justification_bytes
	}

	fn bytes_to_mib(bytes: usize) -> f64 {
		bytes as f64 / (1024.0 * 1024.0)
	}
}

impl fmt::Display for GapSyncStats {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let total = self.total_bytes();
		write!(
			f,
			"hdr: {} B ({:.2} MiB), body: {} B ({:.2} MiB), just: {} B ({:.2} MiB) | total: {} B ({:.2} MiB)",
			self.header_bytes,
			Self::bytes_to_mib(self.header_bytes),
			self.body_bytes,
			Self::bytes_to_mib(self.body_bytes),
			self.justification_bytes,
			Self::bytes_to_mib(self.justification_bytes),
			total,
			Self::bytes_to_mib(total),
		)
	}
}

impl AddAssign for GapSyncStats {
	fn add_assign(&mut self, other: Self) {
		self.header_bytes += other.header_bytes;
		self.body_bytes += other.body_bytes;
		self.justification_bytes += other.justification_bytes;
	}
}

struct GapSync<B: BlockT> {
	blocks: BlockCollection<B>,
	best_queued_number: NumberFor<B>,
	target: NumberFor<B>,
	stats: GapSyncStats,
}

/// Sync operation mode.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ChainSyncMode {
	/// Full block download and verification.
	Full,
	/// Download blocks and the latest state.
	LightState {
		/// Skip state proof download and verification.
		skip_proofs: bool,
		/// Download indexed transactions for recent blocks.
		storage_chain_mode: bool,
	},
}

impl ChainSyncMode {
	/// Returns the base block attributes required for this sync mode.
	pub fn required_block_attributes(&self) -> BlockAttributes {
		match self {
			ChainSyncMode::Full | ChainSyncMode::LightState { storage_chain_mode: false, .. } => {
				BlockAttributes::HEADER | BlockAttributes::JUSTIFICATION | BlockAttributes::BODY
			},
			ChainSyncMode::LightState { storage_chain_mode: true, .. } => {
				BlockAttributes::HEADER |
					BlockAttributes::JUSTIFICATION |
					BlockAttributes::INDEXED_BODY
			},
		}
	}
}

/// Which block bodies gap sync downloads while backfilling the block history below a
/// warp-synced block.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum GapSyncBodyPolicy {
	/// Backfill gap headers and justifications without bodies.
	HeadersOnly,
	/// Require bodies for the complete gap.
	All,
	/// Require bodies for blocks within the given window below the finalized block.
	/// The window is expected to be pre-shrunk by the node with a safety margin, so
	/// that peers whose finality runs ahead of ours still retain every requested body.
	///
	/// While the node is major syncing, local finality can lag the network beyond the
	/// safety margin, so gap ranges above the cutoff are deferred until it has caught up.
	BodiesWithinWindow(u32),
}

/// Resolves the [`GapSyncBodyPolicy`] lazily, when a `ChainSync` instance is created.
///
/// On a warp-syncing node this happens right after state sync completes, so the
/// provider can query runtime state at the warp target. Errors fail `ChainSync`
/// creation instead of silently degrading the policy.
pub type GapSyncBodyPolicyProvider =
	Arc<dyn Fn() -> Result<GapSyncBodyPolicy, ClientError> + Send + Sync>;

/// All the data we have about a Peer that we are trying to sync with
#[derive(Debug, Clone)]
pub(crate) struct PeerSync<B: BlockT> {
	/// Peer id of this peer.
	pub peer_id: PeerId,
	/// The common number is the block number that is a common point of
	/// ancestry for both our chains (as far as we know).
	pub common_number: NumberFor<B>,
	/// The hash of the best block that we've seen for this peer.
	pub best_hash: B::Hash,
	/// The number of the best block that we've seen for this peer.
	pub best_number: NumberFor<B>,
	/// The state of syncing this peer is in for us, generally categories
	/// into `Available` or "busy" with something as defined by `PeerSyncState`.
	pub state: PeerSyncState<B>,
}

impl<B: BlockT> PeerSync<B> {
	/// Update the `common_number` iff `new_common > common_number`.
	fn update_common_number(&mut self, new_common: NumberFor<B>) {
		if self.common_number < new_common {
			trace!(
				target: LOG_TARGET,
				"Updating peer {} common number from={} => to={}.",
				self.peer_id,
				self.common_number,
				new_common,
			);
			self.common_number = new_common;
		}
	}
}

struct ForkTarget<B: BlockT> {
	number: NumberFor<B>,
	parent_hash: Option<B::Hash>,
	peers: HashSet<PeerId>,
}

/// The state of syncing between a Peer and ourselves.
///
/// Generally two categories, "busy" or `Available`. If busy, the enum
/// defines what we are busy with.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum PeerSyncState<B: BlockT> {
	/// Available for sync requests.
	Available,
	/// Searching for ancestors the Peer has in common with us.
	AncestorSearch {
		/// The best queued number when starting the ancestor search.
		start: NumberFor<B>,
		/// The current block that is being downloaded.
		current: NumberFor<B>,
		/// The state of the search.
		state: AncestorSearchState<B>,
	},
	/// Actively downloading new blocks, starting from the given Number.
	DownloadingNew(NumberFor<B>),
	/// Downloading a stale block with given Hash. Stale means that it is a
	/// block with a number that is lower than our best number. It might be
	/// from a fork and not necessarily already imported.
	DownloadingStale(B::Hash),
	/// Downloading justification for given block hash.
	DownloadingJustification(B::Hash),
	/// Downloading state.
	DownloadingState,
	/// Actively downloading block history after warp sync.
	DownloadingGap(NumberFor<B>),
}

impl<B: BlockT> PeerSyncState<B> {
	pub fn is_available(&self) -> bool {
		matches!(self, Self::Available)
	}
}

/// The main data structure which contains all the state for a chains
/// active syncing strategy.
pub struct ChainSync<B: BlockT, Client> {
	/// Chain client.
	client: Arc<Client>,
	/// The active peers that we are using to sync and their PeerSync status
	peers: HashMap<PeerId, PeerSync<B>>,
	disconnected_peers: DisconnectedPeers,
	/// A `BlockCollection` of blocks that are being downloaded from peers
	blocks: BlockCollection<B>,
	/// The best block number in our queue of blocks to import
	best_queued_number: NumberFor<B>,
	/// The best block hash in our queue of blocks to import
	best_queued_hash: B::Hash,
	/// Current mode (full/light)
	mode: ChainSyncMode,
	/// Any extra justification requests.
	extra_justifications: ExtraRequests<B>,
	/// A set of hashes of blocks that are being downloaded or have been
	/// downloaded and are queued for import.
	queue_blocks: HashSet<B::Hash>,
	/// A pending attempt to start the state sync.
	///
	/// The initiation of state sync may be deferred in cases where other conditions
	/// are not yet met when the finalized block notification is received, such as
	/// when `queue_blocks` is not empty or there are no peers. This field holds the
	/// necessary information to attempt the state sync at a later point when
	/// conditions are satisfied.
	pending_state_sync_attempt: Option<(B::Hash, NumberFor<B>, bool)>,
	/// Fork sync targets.
	fork_targets: HashMap<B::Hash, ForkTarget<B>>,
	/// A set of peers for which there might be potential block requests
	allowed_requests: AllowedRequests,
	/// Maximum number of peers to ask the same blocks in parallel.
	max_parallel_downloads: u32,
	/// Maximum blocks per request.
	max_blocks_per_request: u32,
	/// Protocol name used to send out state requests
	state_request_protocol_name: ProtocolName,
	/// Total number of downloaded blocks.
	downloaded_blocks: usize,
	/// State sync in progress, if any.
	state_sync: Option<StateSync<B, Client>>,
	/// Enable importing existing blocks. This is used after the state download to
	/// catch up to the latest state while re-importing blocks.
	import_existing: bool,
	/// Block downloader
	block_downloader: Arc<dyn BlockDownloader<B>>,
	/// Which block bodies gap sync downloads.
	gap_sync_body_policy: GapSyncBodyPolicy,
	/// Gap download process.
	gap_sync: Option<GapSync<B>>,
	/// Pending actions.
	actions: Vec<SyncingAction<B>>,
	/// Prometheus metrics.
	metrics: Option<Metrics>,
}

impl<B, Client> SyncingStrategy<B> for ChainSync<B, Client>
where
	B: BlockT,
	Client: HeaderBackend<B>
		+ BlockBackend<B>
		+ HeaderMetadata<B, Error = sp_blockchain::Error>
		+ ProofProvider<B>
		+ Send
		+ Sync
		+ 'static,
{
	fn add_peer(&mut self, peer_id: PeerId, best_hash: B::Hash, best_number: NumberFor<B>) {
		match self.add_peer_inner(peer_id, best_hash, best_number) {
			Ok(Some(request)) => {
				let action = self.create_block_request_action(peer_id, request);
				self.actions.push(action);
			},
			Ok(None) => {},
			Err(bad_peer) => self.actions.push(SyncingAction::DropPeer(bad_peer)),
		}
	}

	fn remove_peer(&mut self, peer_id: &PeerId) {
		self.blocks.clear_peer_download(peer_id);
		if let Some(gap_sync) = &mut self.gap_sync {
			gap_sync.blocks.clear_peer_download(peer_id)
		}

		if let Some(state) = self.peers.remove(peer_id) {
			if !state.state.is_available() {
				if let Some(bad_peer) =
					self.disconnected_peers.on_disconnect_during_request(*peer_id)
				{
					self.actions.push(SyncingAction::DropPeer(bad_peer));
				}
			}
		}

		self.extra_justifications.cancel_request(peer_id);
		self.allowed_requests.set_all();
		self.fork_targets.retain(|_, target| {
			target.peers.remove(peer_id);
			!target.peers.is_empty()
		});
		if let Some(metrics) = &self.metrics {
			metrics.fork_targets.set(self.fork_targets.len().try_into().unwrap_or(u64::MAX));
		}

		let blocks = self.ready_blocks();

		if !blocks.is_empty() {
			self.validate_and_queue_blocks(blocks, false);
		}
	}

	fn on_validated_block_announce(
		&mut self,
		is_best: bool,
		peer_id: PeerId,
		announce: &BlockAnnounce<B::Header>,
	) -> Option<(B::Hash, NumberFor<B>)> {
		let number = *announce.header.number();
		let hash = announce.header.hash();
		let parent_status =
			self.block_status(announce.header.parent_hash()).unwrap_or(BlockStatus::Unknown);
		let known_parent = parent_status != BlockStatus::Unknown;
		let ancient_parent = parent_status == BlockStatus::InChainPruned;

		let known = self.is_known(&hash);
		let is_major_syncing = self.is_major_syncing();
		let peer = if let Some(peer) = self.peers.get_mut(&peer_id) {
			peer
		} else {
			error!(target: LOG_TARGET, "💔 Called `on_validated_block_announce` with a bad peer ID {peer_id}");
			return Some((hash, number));
		};

		if let PeerSyncState::AncestorSearch { .. } = peer.state {
			trace!(target: LOG_TARGET, "Peer {} is in the ancestor search state.", peer_id);
			return None;
		}

		// The node is continuing a known fork if either the block itself is known, the
		// parent is known or the block references the previously announced `best_hash`.
		let continues_known_fork =
			known || known_parent || announce.header.parent_hash() == &peer.best_hash;

		let peer_info = is_best.then(|| {
			// update their best block
			peer.best_number = number;
			peer.best_hash = hash;

			(hash, number)
		});

		// If the announced block is the best they have and is not ahead of us, our common number
		// is either one further ahead or it's the one they just announced, if we know about it.
		if is_best {
			let best_queued_number = self.best_queued_number;

			if known && best_queued_number >= number {
				peer.update_common_number(number);
			} else if announce.header.parent_hash() == &self.best_queued_hash ||
				known_parent && best_queued_number >= number
			{
				peer.update_common_number(number.saturating_sub(One::one()));
			}

			// If this announced block isn't following any known fork, we have to start an
			// ancestor search to find out our real common block. However, we skip this during
			// major sync to avoid pulling peers out of the download pool.
			if !continues_known_fork && !is_major_syncing {
				let current = number.min(best_queued_number);
				peer.common_number = peer.common_number.min(self.client.info().finalized_number);
				let old_state = std::mem::replace(
					&mut peer.state,
					PeerSyncState::AncestorSearch {
						current,
						start: best_queued_number,
						state: AncestorSearchState::ExponentialBackoff(One::one()),
					},
				);
				self.cancel_peer_request(peer_id, old_state);

				let request = ancestry_request::<B>(current);
				let action = self.create_block_request_action(peer_id, request);
				self.actions.push(action);

				return peer_info;
			}
		}
		self.allowed_requests.add(&peer_id);

		// known block case
		if known || self.is_already_downloading(&hash) {
			trace!(target: LOG_TARGET, "Known block announce from {}: {}", peer_id, hash);
			if let Some(target) = self.fork_targets.get_mut(&hash) {
				target.peers.insert(peer_id);
			}
			return peer_info;
		}

		if ancient_parent {
			trace!(
				target: LOG_TARGET,
				"Ignored ancient block announced from {}: {} {:?}",
				peer_id,
				hash,
				announce.header,
			);
			return peer_info;
		}

		if self.status().state == SyncState::Idle {
			trace!(
				target: LOG_TARGET,
				"Added sync target for block announced from {}: {} {:?}",
				peer_id,
				hash,
				announce.summary(),
			);
			self.fork_targets
				.entry(hash)
				.or_insert_with(|| {
					if let Some(metrics) = &self.metrics {
						metrics.fork_targets.inc();
					}

					ForkTarget {
						number,
						parent_hash: Some(*announce.header.parent_hash()),
						peers: Default::default(),
					}
				})
				.peers
				.insert(peer_id);
		}

		peer_info
	}

	// The implementation is similar to `on_validated_block_announce` with unknown parent hash.
	fn set_sync_fork_request(
		&mut self,
		mut peers: Vec<PeerId>,
		hash: &B::Hash,
		number: NumberFor<B>,
	) {
		if peers.is_empty() {
			peers = self
				.peers
				.iter()
				// Only request blocks from peers who are ahead or on a par.
				.filter(|(_, peer)| peer.best_number >= number)
				.map(|(id, _)| *id)
				.collect();

			debug!(
				target: LOG_TARGET,
				"Explicit sync request for block {hash:?} with no peers specified. \
				Syncing from these peers {peers:?} instead.",
			);
		} else {
			debug!(
				target: LOG_TARGET,
				"Explicit sync request for block {hash:?} with {peers:?}",
			);
		}

		if self.is_known(hash) {
			debug!(target: LOG_TARGET, "Refusing to sync known hash {hash:?}");
			return;
		}

		trace!(target: LOG_TARGET, "Downloading requested old fork {hash:?}");
		for peer_id in &peers {
			if let Some(peer) = self.peers.get_mut(peer_id) {
				if let PeerSyncState::AncestorSearch { .. } = peer.state {
					continue;
				}

				if number > peer.best_number {
					peer.best_number = number;
					peer.best_hash = *hash;
				}
				self.allowed_requests.add(peer_id);
			}
		}

		self.fork_targets
			.entry(*hash)
			.or_insert_with(|| {
				if let Some(metrics) = &self.metrics {
					metrics.fork_targets.inc();
				}

				ForkTarget { number, peers: Default::default(), parent_hash: None }
			})
			.peers
			.extend(peers);
	}

	fn request_justification(&mut self, hash: &B::Hash, number: NumberFor<B>) {
		let client = &self.client;
		self.extra_justifications
			.schedule((*hash, number), |base, block| is_descendent_of(&**client, base, block))
	}

	fn clear_justification_requests(&mut self) {
		self.extra_justifications.reset();
	}

	fn on_justification_import(&mut self, hash: B::Hash, number: NumberFor<B>, success: bool) {
		let finalization_result = if success { Ok((hash, number)) } else { Err(()) };
		self.extra_justifications
			.try_finalize_root((hash, number), finalization_result, true);
		self.allowed_requests.set_all();
	}

	fn on_generic_response(
		&mut self,
		peer_id: &PeerId,
		key: StrategyKey,
		protocol_name: ProtocolName,
		response: Box<dyn Any + Send>,
	) {
		if Self::STRATEGY_KEY != key {
			warn!(
				target: LOG_TARGET,
				"Unexpected generic response strategy key {key:?}, protocol {protocol_name}",
			);
			debug_assert!(false);
			return;
		}

		if protocol_name == self.state_request_protocol_name {
			let Ok(response) = response.downcast::<Vec<u8>>() else {
				warn!(target: LOG_TARGET, "Failed to downcast state response");
				debug_assert!(false);
				return;
			};

			if let Err(bad_peer) = self.on_state_data(&peer_id, &response) {
				self.actions.push(SyncingAction::DropPeer(bad_peer));
			}
		} else if &protocol_name == self.block_downloader.protocol_name() {
			let Ok(response) = response
				.downcast::<(BlockRequest<B>, Result<Vec<BlockData<B>>, BlockResponseError>)>()
			else {
				warn!(target: LOG_TARGET, "Failed to downcast block response");
				debug_assert!(false);
				return;
			};

			let (request, response) = *response;
			let blocks = match response {
				Ok(blocks) => blocks,
				Err(BlockResponseError::DecodeFailed(e)) => {
					debug!(
						target: LOG_TARGET,
						"Failed to decode block response from peer {:?}: {:?}.",
						peer_id,
						e
					);
					self.actions.push(SyncingAction::DropPeer(BadPeer(*peer_id, rep::BAD_MESSAGE)));
					return;
				},
				Err(BlockResponseError::ExtractionFailed(e)) => {
					debug!(
						target: LOG_TARGET,
						"Failed to extract blocks from peer response {:?}: {:?}.",
						peer_id,
						e
					);
					self.actions.push(SyncingAction::DropPeer(BadPeer(*peer_id, rep::BAD_MESSAGE)));
					return;
				},
			};

			if let Err(bad_peer) = self.on_block_response(peer_id, key, request, blocks) {
				self.actions.push(SyncingAction::DropPeer(bad_peer));
			}
		} else {
			warn!(
				target: LOG_TARGET,
				"Unexpected generic response protocol {protocol_name}, strategy key \
				{key:?}",
			);
			debug_assert!(false);
		}
	}

	fn on_blocks_processed(
		&mut self,
		imported: usize,
		count: usize,
		results: Vec<(Result<BlockImportStatus<NumberFor<B>>, BlockImportError>, B::Hash)>,
	) {
		trace!(target: LOG_TARGET, "Imported {imported} of {count}");

		let mut has_error = false;
		for (_, hash) in &results {
			if self.queue_blocks.remove(hash) {
				if let Some(metrics) = &self.metrics {
					metrics.queued_blocks.dec();
				}
			}
			self.blocks.clear_queued(hash);
			if let Some(gap_sync) = &mut self.gap_sync {
				gap_sync.blocks.clear_queued(hash);
			}
		}
		for (result, hash) in results {
			if has_error {
				break;
			}

			has_error |= result.is_err();

			match result {
				Ok(BlockImportStatus::ImportedKnown(number, peer_id)) => {
					if let Some(peer) = peer_id {
						self.update_peer_common_number(&peer, number);
					}
					self.complete_gap_if_target(number);
				},
				Ok(BlockImportStatus::ImportedUnknown(number, aux, peer_id)) => {
					if aux.clear_justification_requests {
						trace!(
							target: LOG_TARGET,
							"Block imported clears all pending justification requests {number}: {hash:?}",
						);
						self.clear_justification_requests();
					}

					if aux.needs_justification {
						trace!(
							target: LOG_TARGET,
							"Block imported but requires justification {number}: {hash:?}",
						);
						self.request_justification(&hash, number);
					}

					if aux.bad_justification {
						if let Some(ref peer) = peer_id {
							warn!("💔 Sent block with bad justification to import");
							self.actions.push(SyncingAction::DropPeer(BadPeer(
								*peer,
								rep::BAD_JUSTIFICATION,
							)));
						}
					}

					if let Some(peer) = peer_id {
						self.update_peer_common_number(&peer, number);
					}
					let state_sync_complete =
						self.state_sync.as_ref().map_or(false, |s| s.target_hash() == hash);
					if state_sync_complete {
						info!(
							target: LOG_TARGET,
							"State sync is complete ({} MiB), restarting block sync.",
							self.state_sync.as_ref().map_or(0, |s| s.progress().size / (1024 * 1024)),
						);
						self.state_sync = None;
						self.mode = ChainSyncMode::Full;
						self.restart();
					}

					self.complete_gap_if_target(number);
				},
				Err(BlockImportError::IncompleteHeader(peer_id)) => {
					if let Some(peer) = peer_id {
						warn!(
							target: LOG_TARGET,
							"💔 Peer sent block with incomplete header to import",
						);
						self.actions
							.push(SyncingAction::DropPeer(BadPeer(peer, rep::INCOMPLETE_HEADER)));
						self.restart();
					}
				},
				Err(BlockImportError::VerificationFailed(peer_id, e)) => {
					let extra_message = peer_id
						.map_or_else(|| "".into(), |peer| format!(" received from ({peer})"));

					warn!(
						target: LOG_TARGET,
						"💔 Verification failed for block {hash:?}{extra_message}: {e:?}",
					);

					if let Some(peer) = peer_id {
						self.actions
							.push(SyncingAction::DropPeer(BadPeer(peer, rep::VERIFICATION_FAIL)));
					}

					self.restart();
				},
				Err(BlockImportError::BadBlock(peer_id)) => {
					if let Some(peer) = peer_id {
						warn!(
							target: LOG_TARGET,
							"💔 Block {hash:?} received from peer {peer} has been blacklisted",
						);
						self.actions.push(SyncingAction::DropPeer(BadPeer(peer, rep::BAD_BLOCK)));
					}
				},
				Err(BlockImportError::MissingState) => {
					// This may happen if the chain we were requesting upon has been discarded
					// in the meantime because other chain has been finalized.
					// Don't mark it as bad as it still may be synced if explicitly requested.
					trace!(target: LOG_TARGET, "Obsolete block {hash:?}");
				},
				e @ Err(BlockImportError::UnknownParent) | e @ Err(BlockImportError::Other(_)) => {
					warn!(target: LOG_TARGET, "💔 Error importing block {hash:?}: {}", e.unwrap_err());
					self.state_sync = None;
					self.restart();
				},
				Err(BlockImportError::Cancelled) => {},
			};
		}

		self.allowed_requests.set_all();
	}

	fn on_block_finalized(&mut self, hash: &B::Hash, number: NumberFor<B>) {
		let client = &self.client;
		let r = self.extra_justifications.on_block_finalized(hash, number, |base, block| {
			is_descendent_of(&**client, base, block)
		});

		if let ChainSyncMode::LightState { skip_proofs, .. } = &self.mode {
			if self.state_sync.is_none() {
				if !self.peers.is_empty() && self.queue_blocks.is_empty() {
					self.attempt_state_sync(*hash, number, *skip_proofs);
				} else {
					self.pending_state_sync_attempt.replace((*hash, number, *skip_proofs));
				}
			}
		}

		if let Err(err) = r {
			warn!(
				target: LOG_TARGET,
				"💔 Error cleaning up pending extra justification data requests: {err}",
			);
		}
	}

	fn update_chain_info(&mut self, best_hash: &B::Hash, best_number: NumberFor<B>) {
		self.on_block_queued(best_hash, best_number);
	}

	fn is_major_syncing(&self) -> bool {
		self.status().state.is_major_syncing()
	}

	fn num_peers(&self) -> usize {
		self.peers.len()
	}

	fn status(&self) -> SyncStatus<B> {
		let median_seen = self.median_seen();
		let best_seen_block =
			median_seen.and_then(|median| (median > self.best_queued_number).then_some(median));
		let sync_state = if let Some(target) = median_seen {
			// A chain is classified as downloading if the provided best block is
			// more than `MAJOR_SYNC_BLOCKS` behind the best block or as importing
			// if the same can be said about queued blocks.
			let best_block = self.client.info().best_number;
			if target > best_block && target - best_block > MAJOR_SYNC_BLOCKS.into() {
				// If target is not queued, we're downloading, otherwise importing.
				if target > self.best_queued_number {
					SyncState::Downloading { target }
				} else {
					SyncState::Importing { target }
				}
			} else {
				SyncState::Idle
			}
		} else {
			SyncState::Idle
		};

		let warp_sync_progress = self.gap_sync.as_ref().map(|gap_sync| WarpSyncProgress {
			phase: WarpSyncPhase::DownloadingBlocks(gap_sync.best_queued_number),
			total_bytes: 0,
			status: None,
		});

		SyncStatus {
			state: sync_state,
			best_seen_block,
			num_peers: self.peers.len() as u32,
			queued_blocks: self.queue_blocks.len() as u32,
			state_sync: self.state_sync.as_ref().map(|s| s.progress()),
			warp_sync: warp_sync_progress,
		}
	}

	fn num_downloaded_blocks(&self) -> usize {
		self.downloaded_blocks
	}

	fn num_sync_requests(&self) -> usize {
		self.fork_targets
			.values()
			.filter(|f| f.number <= self.best_queued_number)
			.count()
	}

	fn actions(
		&mut self,
		network_service: &NetworkServiceHandle,
	) -> Result<Vec<SyncingAction<B>>, ClientError> {
		if !self.peers.is_empty() && self.queue_blocks.is_empty() {
			if let Some((hash, number, skip_proofs)) = self.pending_state_sync_attempt.take() {
				self.attempt_state_sync(hash, number, skip_proofs);
			}
		}

		let block_requests = self
			.block_requests()
			.into_iter()
			.map(|(peer_id, request)| self.create_block_request_action(peer_id, request))
			.collect::<Vec<_>>();
		self.actions.extend(block_requests);

		let justification_requests = self
			.justification_requests()
			.into_iter()
			.map(|(peer_id, request)| self.create_block_request_action(peer_id, request))
			.collect::<Vec<_>>();
		self.actions.extend(justification_requests);

		let state_request = self.state_request().into_iter().map(|(peer_id, request)| {
			trace!(
				target: LOG_TARGET,
				"Created `StateRequest` to {peer_id}.",
			);

			let (tx, rx) = oneshot::channel();

			network_service.start_request(
				peer_id,
				self.state_request_protocol_name.clone(),
				request.encode_to_vec(),
				tx,
				IfDisconnected::ImmediateError,
			);

			SyncingAction::StartRequest {
				peer_id,
				key: Self::STRATEGY_KEY,
				request: async move {
					Ok(rx.await?.and_then(|(response, protocol_name)| {
						Ok((Box::new(response) as Box<dyn Any + Send>, protocol_name))
					}))
				}
				.boxed(),
			}
		});
		self.actions.extend(state_request);

		Ok(std::mem::take(&mut self.actions))
	}
}

impl<B, Client> ChainSync<B, Client>
where
	B: BlockT,
	Client: HeaderBackend<B>
		+ BlockBackend<B>
		+ HeaderMetadata<B, Error = sp_blockchain::Error>
		+ ProofProvider<B>
		+ Send
		+ Sync
		+ 'static,
{
	/// Strategy key used by chain sync.
	pub const STRATEGY_KEY: StrategyKey = StrategyKey::new("ChainSync");

	/// Create a new instance.
	pub fn new(
		mode: ChainSyncMode,
		client: Arc<Client>,
		max_parallel_downloads: u32,
		max_blocks_per_request: u32,
		state_request_protocol_name: ProtocolName,
		block_downloader: Arc<dyn BlockDownloader<B>>,
		gap_sync_body_policy: GapSyncBodyPolicy,
		metrics_registry: Option<&Registry>,
		initial_peers: impl Iterator<Item = (PeerId, B::Hash, NumberFor<B>)>,
	) -> Result<Self, ClientError> {
		info!(target: LOG_TARGET, "Gap sync body policy: {gap_sync_body_policy:?}");
		let metrics = metrics_registry.and_then(|r| match Metrics::register(r) {
			Ok(metrics) => Some(metrics),
			Err(err) => {
				log::error!(target: LOG_TARGET, "Failed to register `ChainSync` metrics {err:?}");
				None
			},
		});
		let mut sync = Self {
			client,
			peers: HashMap::new(),
			disconnected_peers: DisconnectedPeers::new(),
			blocks: BlockCollection::with_metrics(
				metrics.as_ref().map(|m| m.block_collection.clone()),
			),
			best_queued_hash: Default::default(),
			best_queued_number: Zero::zero(),
			extra_justifications: ExtraRequests::new("justification", metrics_registry),
			mode,
			queue_blocks: Default::default(),
			pending_state_sync_attempt: None,
			fork_targets: Default::default(),
			allowed_requests: Default::default(),
			max_parallel_downloads,
			max_blocks_per_request,
			state_request_protocol_name,
			downloaded_blocks: 0,
			state_sync: None,
			import_existing: false,
			block_downloader,
			gap_sync_body_policy,
			gap_sync: None,
			actions: Vec::new(),
			metrics,
		};

		sync.reset_sync_start_point()?;
		initial_peers.for_each(|(peer_id, best_hash, best_number)| {
			sync.add_peer(peer_id, best_hash, best_number);
		});

		Ok(sync)
	}

	/// Complete the gap sync if the target number is reached and there is a gap.
	fn complete_gap_if_target(&mut self, number: NumberFor<B>) {
		let Some(gap_sync) = &self.gap_sync else { return };

		if gap_sync.target != number {
			return;
		}

		info!(
			target: LOG_TARGET,
			"Block history download is complete. Downloaded {}.",
			gap_sync.stats,
		);
		self.gap_sync = None;
		if let Some(metrics) = &self.metrics {
			metrics.gap_oldest_required_body.set(0);
		}
	}

	#[must_use]
	fn add_peer_inner(
		&mut self,
		peer_id: PeerId,
		best_hash: B::Hash,
		best_number: NumberFor<B>,
	) -> Result<Option<BlockRequest<B>>, BadPeer> {
		// There is nothing sync can get from the node that has no blockchain data.
		match self.block_status(&best_hash) {
			Err(e) => {
				debug!(target: LOG_TARGET, "Error reading blockchain: {e}");
				Err(BadPeer(peer_id, rep::BLOCKCHAIN_READ_ERROR))
			},
			Ok(BlockStatus::KnownBad) => {
				info!(
					"💔 New peer {peer_id} with known bad best block {best_hash} ({best_number})."
				);
				Err(BadPeer(peer_id, rep::BAD_BLOCK))
			},
			Ok(BlockStatus::Unknown) => {
				if best_number.is_zero() {
					info!(
						"💔 New peer {} with unknown genesis hash {} ({}).",
						peer_id, best_hash, best_number,
					);
					return Err(BadPeer(peer_id, rep::GENESIS_MISMATCH));
				}

				// If there are more than `MAJOR_SYNC_BLOCKS` in the import queue then we have
				// enough to do in the import queue that it's not worth kicking off
				// an ancestor search, which is what we do in the next match case below.
				if self.queue_blocks.len() > MAJOR_SYNC_BLOCKS as usize {
					debug!(
						target: LOG_TARGET,
						"New peer {} with unknown best hash {} ({}), assuming common block.",
						peer_id,
						self.best_queued_hash,
						self.best_queued_number
					);
					self.peers.insert(
						peer_id,
						PeerSync {
							peer_id,
							common_number: self.best_queued_number,
							best_hash,
							best_number,
							state: PeerSyncState::Available,
						},
					);
					return Ok(None);
				}

				// If we are at genesis, just start downloading.
				let (state, req) = if self.best_queued_number.is_zero() {
					debug!(
						target: LOG_TARGET,
						"New peer {peer_id} with best hash {best_hash} ({best_number}).",
					);

					(PeerSyncState::Available, None)
				} else {
					let common_best = std::cmp::min(self.best_queued_number, best_number);

					debug!(
						target: LOG_TARGET,
						"New peer {} with unknown best hash {} ({}), searching for common ancestor.",
						peer_id,
						best_hash,
						best_number
					);

					(
						PeerSyncState::AncestorSearch {
							current: common_best,
							start: self.best_queued_number,
							state: AncestorSearchState::ExponentialBackoff(One::one()),
						},
						Some(ancestry_request::<B>(common_best)),
					)
				};

				self.allowed_requests.add(&peer_id);
				self.peers.insert(
					peer_id,
					PeerSync {
						peer_id,
						common_number: Zero::zero(),
						best_hash,
						best_number,
						state,
					},
				);

				Ok(req)
			},
			Ok(BlockStatus::Queued) |
			Ok(BlockStatus::InChainWithState) |
			Ok(BlockStatus::InChainPruned) => {
				debug!(
					target: LOG_TARGET,
					"New peer {peer_id} with known best hash {best_hash} ({best_number}).",
				);
				self.peers.insert(
					peer_id,
					PeerSync {
						peer_id,
						common_number: std::cmp::min(self.best_queued_number, best_number),
						best_hash,
						best_number,
						state: PeerSyncState::Available,
					},
				);
				self.allowed_requests.add(&peer_id);
				Ok(None)
			},
		}
	}

	/// Release bookkeeping for a peer's old request and queue its cancellation.
	///
	/// The caller handles the peer's state transition and must call this before scheduling
	/// replacement work. The engine only drops the old response future.
	fn cancel_peer_request(&mut self, peer_id: PeerId, old_state: PeerSyncState<B>) {
		match old_state {
			PeerSyncState::Available => return,
			PeerSyncState::DownloadingNew(_) => self.blocks.clear_peer_download(&peer_id),
			PeerSyncState::DownloadingGap(_) => {
				if let Some(gap_sync) = &mut self.gap_sync {
					gap_sync.blocks.clear_peer_download(&peer_id);
				}
			},
			PeerSyncState::DownloadingJustification(_) => {
				self.extra_justifications.cancel_request(&peer_id);
			},
			// State requests are regenerated from the last imported cursor; fork targets
			// remain pending. Neither has a separate in-flight range reservation.
			PeerSyncState::DownloadingState |
			PeerSyncState::DownloadingStale(_) |
			PeerSyncState::AncestorSearch { .. } => {},
		}
		self.actions
			.push(SyncingAction::CancelRequest { peer_id, key: Self::STRATEGY_KEY });
		// Let any available peer pick up the released work, e.g. while this peer does
		// ancestry search.
		self.allowed_requests.set_all();
	}

	fn create_block_request_action(
		&mut self,
		peer_id: PeerId,
		request: BlockRequest<B>,
	) -> SyncingAction<B> {
		let downloader = self.block_downloader.clone();

		SyncingAction::StartRequest {
			peer_id,
			key: Self::STRATEGY_KEY,
			request: async move {
				Ok(downloader.download_blocks(peer_id, request.clone()).await?.and_then(
					|(response, protocol_name)| {
						let decoded_response =
							downloader.block_response_into_blocks(&request, response);
						let result = Box::new((request, decoded_response)) as Box<dyn Any + Send>;
						Ok((result, protocol_name))
					},
				))
			}
			.boxed(),
		}
	}

	/// Submit a block response for processing.
	#[must_use]
	fn on_block_data(
		&mut self,
		peer_id: &PeerId,
		request: Option<BlockRequest<B>>,
		response: BlockResponse<B>,
	) -> Result<(), BadPeer> {
		self.downloaded_blocks += response.blocks.len();
		let mut gap = false;
		let new_blocks: Vec<IncomingBlock<B>> = if let Some(peer) = self.peers.get_mut(peer_id) {
			let mut blocks = response.blocks;
			if request.as_ref().map_or(false, |r| r.direction == Direction::Descending) {
				trace!(target: LOG_TARGET, "Reversing incoming block list");
				blocks.reverse()
			}
			self.allowed_requests.add(peer_id);
			if let Some(request) = request {
				match &mut peer.state {
					PeerSyncState::DownloadingNew(_) => {
						self.blocks.clear_peer_download(peer_id);
						peer.state = PeerSyncState::Available;
						if let Some(start_block) =
							validate_blocks::<B>(&blocks, peer_id, Some(request))?
						{
							self.blocks.insert(start_block, blocks, *peer_id);
						}
						self.ready_blocks()
					},
					PeerSyncState::DownloadingGap(_) => {
						peer.state = PeerSyncState::Available;
						if blocks.is_empty() && request.fields.contains(BlockAttributes::BODY) {
							// An empty response means the peer holds no body of the entire
							// range (bodies are pruned oldest-first). Disconnect it to free
							// the slot for a peer that does; the mild penalty lets it retry
							// later.
							debug!(
								target: LOG_TARGET,
								"Peer {peer_id} sent an empty response for gap block request \
								 {request:?} that required bodies; disconnecting it",
							);
							if let Some(metrics) = &self.metrics {
								metrics.gap_body_empty_responses.inc();
							}
							if let Some(gap_sync) = &mut self.gap_sync {
								gap_sync.blocks.clear_peer_download(peer_id);
							}
							return Err(BadPeer(*peer_id, rep::NO_GAP_BODIES));
						}
						if let Some(gap_sync) = &mut self.gap_sync {
							gap_sync.blocks.clear_peer_download(peer_id);
							if let Some(start_block) =
								validate_blocks::<B>(&blocks, peer_id, Some(request))?
							{
								gap_sync.blocks.insert(start_block, blocks, *peer_id);
							}
							gap = true;
							let mut batch_gap_sync_stats = GapSyncStats::new();
							let blocks: Vec<_> = gap_sync
								.blocks
								.ready_blocks(gap_sync.best_queued_number + One::one())
								.into_iter()
								.map(|block_data| {
									let justifications =
										block_data.block.justifications.or_else(|| {
											legacy_justification_mapping(
												block_data.block.justification,
											)
										});
									let gap_sync_stats = GapSyncStats {
										header_bytes: block_data
											.block
											.header
											.as_ref()
											.map(|h| h.encoded_size())
											.unwrap_or(0),
										body_bytes: block_data
											.block
											.body
											.as_ref()
											.map(|b| b.encoded_size())
											.unwrap_or(0),
										justification_bytes: justifications
											.as_ref()
											.map(|j| j.encoded_size())
											.unwrap_or(0),
									};
									batch_gap_sync_stats += gap_sync_stats;

									IncomingBlock {
										hash: block_data.block.hash,
										header: block_data.block.header,
										body: block_data.block.body,
										indexed_body: block_data.block.indexed_body,
										justifications,
										origin: block_data.origin,
										allow_missing_state: true,
										// Warp-synced blocks are header-only. Allow re-import to
										// store bodies if gap sync requested them.
										import_existing: true,
										skip_execution: true,
										state: None,
									}
								})
								.collect();

							debug!(
								target: LOG_TARGET,
								"Drained {} gap blocks from {}",
								blocks.len(),
								gap_sync.best_queued_number,
							);

							gap_sync.stats += batch_gap_sync_stats;

							if blocks.len() > 0 {
								trace!(
									target: LOG_TARGET,
									"Gap sync cumulative stats: {}",
									gap_sync.stats
								);
							}
							blocks
						} else {
							debug!(target: LOG_TARGET, "Unexpected gap block response from {peer_id}");
							return Err(BadPeer(*peer_id, rep::NO_BLOCK));
						}
					},
					PeerSyncState::DownloadingStale(_) => {
						peer.state = PeerSyncState::Available;
						if blocks.is_empty() {
							debug!(target: LOG_TARGET, "Empty block response from {peer_id}");
							return Err(BadPeer(*peer_id, rep::NO_BLOCK));
						}
						validate_blocks::<B>(&blocks, peer_id, Some(request))?;
						blocks
							.into_iter()
							.map(|b| {
								let justifications = b
									.justifications
									.or_else(|| legacy_justification_mapping(b.justification));
								IncomingBlock {
									hash: b.hash,
									header: b.header,
									body: b.body,
									indexed_body: None,
									justifications,
									origin: Some(*peer_id),
									allow_missing_state: true,
									import_existing: self.import_existing,
									skip_execution: self.skip_execution(),
									state: None,
								}
							})
							.collect()
					},
					PeerSyncState::AncestorSearch { current, start, state } => {
						let matching_hash = match (blocks.get(0), self.client.hash(*current)) {
							(Some(block), Ok(maybe_our_block_hash)) => {
								trace!(
									target: LOG_TARGET,
									"Got ancestry block #{} ({}) from peer {}",
									current,
									block.hash,
									peer_id,
								);
								maybe_our_block_hash.filter(|x| x == &block.hash)
							},
							(None, _) => {
								debug!(
									target: LOG_TARGET,
									"Invalid response when searching for ancestor from {peer_id}",
								);
								return Err(BadPeer(*peer_id, rep::UNKNOWN_ANCESTOR));
							},
							(_, Err(e)) => {
								info!(
									target: LOG_TARGET,
									"❌ Error answering legitimate blockchain query: {e}",
								);
								return Err(BadPeer(*peer_id, rep::BLOCKCHAIN_READ_ERROR));
							},
						};
						if matching_hash.is_some() {
							if *start < self.best_queued_number &&
								self.best_queued_number <= peer.best_number
							{
								// We've made progress on this chain since the search was started.
								// Opportunistically set common number to updated number
								// instead of the one that started the search.
								trace!(
									target: LOG_TARGET,
									"Ancestry search: opportunistically updating peer {} common number from={} => to={}.",
									*peer_id,
									peer.common_number,
									self.best_queued_number,
								);
								peer.common_number = self.best_queued_number;
							} else if peer.common_number < *current {
								trace!(
									target: LOG_TARGET,
									"Ancestry search: updating peer {} common number from={} => to={}.",
									*peer_id,
									peer.common_number,
									*current,
								);
								peer.common_number = *current;
							}
						}
						if matching_hash.is_none() && current.is_zero() {
							trace!(
								target: LOG_TARGET,
								"Ancestry search: genesis mismatch for peer {peer_id}",
							);
							return Err(BadPeer(*peer_id, rep::GENESIS_MISMATCH));
						}
						if let Some((next_state, next_num)) =
							handle_ancestor_search_state(state, *current, matching_hash.is_some())
						{
							peer.state = PeerSyncState::AncestorSearch {
								current: next_num,
								start: *start,
								state: next_state,
							};
							let request = ancestry_request::<B>(next_num);
							let action = self.create_block_request_action(*peer_id, request);
							self.actions.push(action);
							return Ok(());
						} else {
							// Ancestry search is complete. Check if peer is on a stale fork unknown
							// to us and add it to sync targets if necessary.
							trace!(
								target: LOG_TARGET,
								"Ancestry search complete. Ours={} ({}), Theirs={} ({}), Common={:?} ({})",
								self.best_queued_hash,
								self.best_queued_number,
								peer.best_hash,
								peer.best_number,
								matching_hash,
								peer.common_number,
							);
							if peer.common_number < peer.best_number &&
								peer.best_number < self.best_queued_number
							{
								trace!(
									target: LOG_TARGET,
									"Added fork target {} for {}",
									peer.best_hash,
									peer_id,
								);
								self.fork_targets
									.entry(peer.best_hash)
									.or_insert_with(|| {
										if let Some(metrics) = &self.metrics {
											metrics.fork_targets.inc();
										}

										ForkTarget {
											number: peer.best_number,
											parent_hash: None,
											peers: Default::default(),
										}
									})
									.peers
									.insert(*peer_id);
							}
							peer.state = PeerSyncState::Available;
							return Ok(());
						}
					},
					PeerSyncState::Available |
					PeerSyncState::DownloadingJustification(..) |
					PeerSyncState::DownloadingState => Vec::new(),
				}
			} else {
				// When request.is_none() this is a block announcement. Just accept blocks.
				validate_blocks::<B>(&blocks, peer_id, None)?;
				blocks
					.into_iter()
					.map(|b| {
						let justifications = b
							.justifications
							.or_else(|| legacy_justification_mapping(b.justification));
						IncomingBlock {
							hash: b.hash,
							header: b.header,
							body: b.body,
							indexed_body: None,
							justifications,
							origin: Some(*peer_id),
							allow_missing_state: true,
							import_existing: false,
							skip_execution: true,
							state: None,
						}
					})
					.collect()
			}
		} else {
			// We don't know of this peer, so we also did not request anything from it.
			return Err(BadPeer(*peer_id, rep::NOT_REQUESTED));
		};

		self.validate_and_queue_blocks(new_blocks, gap);

		Ok(())
	}

	fn on_block_response(
		&mut self,
		peer_id: &PeerId,
		key: StrategyKey,
		request: BlockRequest<B>,
		blocks: Vec<BlockData<B>>,
	) -> Result<(), BadPeer> {
		if key != Self::STRATEGY_KEY {
			error!(
				target: LOG_TARGET,
				"`on_block_response()` called with unexpected key {key:?} for chain sync",
			);
			debug_assert!(false);
		}
		let block_response = BlockResponse::<B> { id: request.id, blocks };

		let blocks_range = || match (
			block_response
				.blocks
				.first()
				.and_then(|b| b.header.as_ref().map(|h| h.number())),
			block_response.blocks.last().and_then(|b| b.header.as_ref().map(|h| h.number())),
		) {
			(Some(first), Some(last)) if first != last => format!(" ({}..{})", first, last),
			(Some(first), Some(_)) => format!(" ({})", first),
			_ => Default::default(),
		};

		trace!(
			target: LOG_TARGET,
			"BlockResponse {} from {} with {} blocks {}",
			block_response.id,
			peer_id,
			block_response.blocks.len(),
			blocks_range(),
		);

		if request.fields == BlockAttributes::JUSTIFICATION {
			self.on_block_justification(*peer_id, block_response)
		} else {
			self.on_block_data(peer_id, Some(request), block_response)
		}
	}

	/// Submit a justification response for processing.
	#[must_use]
	fn on_block_justification(
		&mut self,
		peer_id: PeerId,
		response: BlockResponse<B>,
	) -> Result<(), BadPeer> {
		let peer = if let Some(peer) = self.peers.get_mut(&peer_id) {
			peer
		} else {
			error!(
				target: LOG_TARGET,
				"💔 Called on_block_justification with a peer ID of an unknown peer",
			);
			return Ok(());
		};

		self.allowed_requests.add(&peer_id);
		if let PeerSyncState::DownloadingJustification(hash) = peer.state {
			peer.state = PeerSyncState::Available;

			// We only request one justification at a time
			let justification = if let Some(block) = response.blocks.into_iter().next() {
				if hash != block.hash {
					warn!(
						target: LOG_TARGET,
						"💔 Invalid block justification provided by {}: requested: {:?} got: {:?}",
						peer_id,
						hash,
						block.hash,
					);
					return Err(BadPeer(peer_id, rep::BAD_JUSTIFICATION));
				}

				block
					.justifications
					.or_else(|| legacy_justification_mapping(block.justification))
			} else {
				// we might have asked the peer for a justification on a block that we assumed it
				// had but didn't (regardless of whether it had a justification for it or not).
				trace!(
					target: LOG_TARGET,
					"Peer {peer_id:?} provided empty response for justification request {hash:?}",
				);

				None
			};

			if let Some((peer_id, hash, number, justifications)) =
				self.extra_justifications.on_response(peer_id, justification)
			{
				self.actions.push(SyncingAction::ImportJustifications {
					peer_id,
					hash,
					number,
					justifications,
				});
				return Ok(());
			}
		}

		Ok(())
	}

	/// Returns the median seen block number.
	fn median_seen(&self) -> Option<NumberFor<B>> {
		let mut best_seens = self.peers.values().map(|p| p.best_number).collect::<Vec<_>>();

		if best_seens.is_empty() {
			None
		} else {
			let middle = best_seens.len() / 2;

			// Not the "perfect median" when we have an even number of peers.
			Some(*best_seens.select_nth_unstable(middle).1)
		}
	}

	fn skip_execution(&self) -> bool {
		match self.mode {
			ChainSyncMode::Full => false,
			ChainSyncMode::LightState { .. } => true,
		}
	}

	fn validate_and_queue_blocks(&mut self, mut new_blocks: Vec<IncomingBlock<B>>, gap: bool) {
		let orig_len = new_blocks.len();
		new_blocks.retain(|b| !self.queue_blocks.contains(&b.hash));
		if new_blocks.len() != orig_len {
			debug!(
				target: LOG_TARGET,
				"Ignoring {} blocks that are already queued",
				orig_len - new_blocks.len(),
			);
		}

		let origin = if gap {
			// Gap sync: filling historical blocks after warp sync
			BlockOrigin::GapSync
		} else if !self.status().state.is_major_syncing() {
			// Normal operation: receiving new blocks
			BlockOrigin::NetworkBroadcast
		} else {
			// Initial sync: catching up with the chain
			BlockOrigin::NetworkInitialSync
		};

		if let Some((h, n)) = new_blocks
			.last()
			.and_then(|b| b.header.as_ref().map(|h| (&b.hash, *h.number())))
		{
			trace!(
				target: LOG_TARGET,
				"Accepted {} blocks ({:?}) with origin {:?}",
				new_blocks.len(),
				h,
				origin,
			);
			self.on_block_queued(h, n)
		}
		self.queue_blocks.extend(new_blocks.iter().map(|b| b.hash));
		if let Some(metrics) = &self.metrics {
			metrics
				.queued_blocks
				.set(self.queue_blocks.len().try_into().unwrap_or(u64::MAX));
		}

		self.actions.push(SyncingAction::ImportBlocks { origin, blocks: new_blocks })
	}

	fn update_peer_common_number(&mut self, peer_id: &PeerId, new_common: NumberFor<B>) {
		if let Some(peer) = self.peers.get_mut(peer_id) {
			peer.update_common_number(new_common);
		}
	}

	/// Called when a block has been queued for import.
	///
	/// Updates our internal state for best queued block and then goes
	/// through all peers to update our view of their state as well.
	fn on_block_queued(&mut self, hash: &B::Hash, number: NumberFor<B>) {
		if self.fork_targets.remove(hash).is_some() {
			if let Some(metrics) = &self.metrics {
				metrics.fork_targets.dec();
			}
			trace!(target: LOG_TARGET, "Completed fork sync {hash:?}");
		}
		if let Some(gap_sync) = &mut self.gap_sync {
			if number > gap_sync.best_queued_number && number <= gap_sync.target {
				gap_sync.best_queued_number = number;
			}
		}
		if number > self.best_queued_number {
			self.best_queued_number = number;
			self.best_queued_hash = *hash;
			// Update common blocks
			for (n, peer) in self.peers.iter_mut() {
				if let PeerSyncState::AncestorSearch { .. } = peer.state {
					// Wait for ancestry search to complete first.
					continue;
				}
				let new_common_number =
					if peer.best_number >= number { number } else { peer.best_number };
				trace!(
					target: LOG_TARGET,
					"Updating peer {} info, ours={}, common={}->{}, their best={}",
					n,
					number,
					peer.common_number,
					new_common_number,
					peer.best_number,
				);
				peer.common_number = new_common_number;
			}
		}
		self.allowed_requests.set_all();
	}

	/// Restart the sync process. This will reset all pending block requests and return an iterator
	/// of new block requests to make to peers. Peers that were downloading finality data (i.e.
	/// their state was `DownloadingJustification`) are unaffected and will stay in the same state.
	fn restart(&mut self) {
		self.blocks.clear();
		if let Err(e) = self.reset_sync_start_point() {
			warn!(target: LOG_TARGET, "💔  Unable to restart sync: {e}");
		}
		self.allowed_requests.set_all();
		debug!(
			target: LOG_TARGET,
			"Restarted with {} ({})",
			self.best_queued_number,
			self.best_queued_hash,
		);
		let old_peers = std::mem::take(&mut self.peers);

		old_peers.into_iter().for_each(|(peer_id, mut peer_sync)| {
			match peer_sync.state {
				PeerSyncState::DownloadingJustification(_) => {
					// Peers that were downloading justifications
					// should be kept in that state.
					// We make sure our common number is at least something we have.
					trace!(
						target: LOG_TARGET,
						"Keeping peer {} after restart, updating common number from={} => to={} (our best).",
						peer_id,
						peer_sync.common_number,
						self.best_queued_number,
					);
					peer_sync.common_number = self.best_queued_number;
					self.peers.insert(peer_id, peer_sync);
				},
				_ => {
					self.cancel_peer_request(peer_id, peer_sync.state);
					self.add_peer(peer_id, peer_sync.best_hash, peer_sync.best_number);
				},
			}
		});
	}

	/// Find a block to start sync from. If we sync with state, that's the latest block we have
	/// state for.
	fn reset_sync_start_point(&mut self) -> Result<(), ClientError> {
		let info = self.client.info();
		debug!(target: LOG_TARGET, "Restarting sync with client info {info:?}");

		if matches!(self.mode, ChainSyncMode::LightState { .. }) && info.finalized_state.is_some() {
			warn!(
				target: LOG_TARGET,
				"Can't use fast sync mode with a partially synced database. Reverting to full sync mode."
			);
			self.mode = ChainSyncMode::Full;
		}

		self.import_existing = false;
		self.best_queued_hash = info.best_hash;
		self.best_queued_number = info.best_number;

		if self.mode == ChainSyncMode::Full &&
			self.client.block_status(info.best_hash)? != BlockStatus::InChainWithState
		{
			self.import_existing = true;
			// Latest state is missing, start with the last finalized state or genesis instead.
			if let Some((hash, number)) = info.finalized_state {
				debug!(target: LOG_TARGET, "Starting from finalized state #{number}");
				self.best_queued_hash = hash;
				self.best_queued_number = number;
			} else {
				debug!(target: LOG_TARGET, "Restarting from genesis");
				self.best_queued_hash = Default::default();
				self.best_queued_number = Zero::zero();
			}
		}

		// The client is the source of truth for the gap. Rebuild the gap sync state from it, or
		// drop ours if the database has already closed the gap.
		let old_gap = self.gap_sync.take().map(|g| (g.best_queued_number, g.target));
		if let Some(BlockGap { start, end, .. }) = info.block_gap {
			debug!(target: LOG_TARGET, "Starting gap sync #{start} - #{end} (old gap best and target: {old_gap:?})");
			self.gap_sync = Some(GapSync {
				best_queued_number: start - One::one(),
				target: end,
				blocks: BlockCollection::with_metrics(
					self.metrics.as_ref().map(|m| m.block_collection.clone()),
				),
				stats: GapSyncStats::new(),
			});
		} else if let Some((best, target)) = old_gap {
			debug!(
				target: LOG_TARGET,
				"Block gap is closed in the database, dropping gap sync state (best: #{best}, target: #{target})",
			);
			if let Some(metrics) = &self.metrics {
				metrics.gap_oldest_required_body.set(0);
			}
		}
		trace!(
			target: LOG_TARGET,
			"Restarted sync at #{} ({:?})",
			self.best_queued_number,
			self.best_queued_hash,
		);
		Ok(())
	}

	/// What is the status of the block corresponding to the given hash?
	fn block_status(&self, hash: &B::Hash) -> Result<BlockStatus, ClientError> {
		if self.queue_blocks.contains(hash) {
			return Ok(BlockStatus::Queued);
		}
		self.client.block_status(*hash)
	}

	/// Is the block corresponding to the given hash known?
	fn is_known(&self, hash: &B::Hash) -> bool {
		self.block_status(hash).ok().map_or(false, |s| s != BlockStatus::Unknown)
	}

	/// Is any peer downloading the given hash?
	fn is_already_downloading(&self, hash: &B::Hash) -> bool {
		self.peers
			.iter()
			.any(|(_, p)| p.state == PeerSyncState::DownloadingStale(*hash))
	}

	/// Get the set of downloaded blocks that are ready to be queued for import.
	fn ready_blocks(&mut self) -> Vec<IncomingBlock<B>> {
		self.blocks
			.ready_blocks(self.best_queued_number + One::one())
			.into_iter()
			.map(|block_data| {
				let justifications = block_data
					.block
					.justifications
					.or_else(|| legacy_justification_mapping(block_data.block.justification));
				IncomingBlock {
					hash: block_data.block.hash,
					header: block_data.block.header,
					body: block_data.block.body,
					indexed_body: block_data.block.indexed_body,
					justifications,
					origin: block_data.origin,
					allow_missing_state: true,
					import_existing: self.import_existing,
					skip_execution: self.skip_execution(),
					state: None,
				}
			})
			.collect()
	}

	/// Get justification requests scheduled by sync to be sent out.
	fn justification_requests(&mut self) -> Vec<(PeerId, BlockRequest<B>)> {
		let peers = &mut self.peers;
		let mut matcher = self.extra_justifications.matcher();
		std::iter::from_fn(move || {
			if let Some((peer, request)) = matcher.next(peers) {
				peers
					.get_mut(&peer)
					.expect(
						"`Matcher::next` guarantees the `PeerId` comes from the given peers; qed",
					)
					.state = PeerSyncState::DownloadingJustification(request.0);
				let req = BlockRequest::<B> {
					id: 0,
					fields: BlockAttributes::JUSTIFICATION,
					from: FromBlock::Hash(request.0),
					direction: Direction::Ascending,
					max: Some(1),
				};
				Some((peer, req))
			} else {
				None
			}
		})
		.collect()
	}

	/// Block attributes for gap requests, plus the body cutoff: bodies are required for
	/// gap blocks above the cutoff and stripped from ranges entirely at or below it.
	/// Recomputed every scheduling pass so the cutoff follows finality.
	fn gap_request_attributes(
		&self,
		finalized_number: NumberFor<B>,
	) -> (BlockAttributes, Option<NumberFor<B>>) {
		let attrs = self.mode.required_block_attributes();
		match self.gap_sync_body_policy {
			GapSyncBodyPolicy::HeadersOnly => (attrs & !BlockAttributes::BODY, None),
			GapSyncBodyPolicy::All => (attrs, None),
			GapSyncBodyPolicy::BodiesWithinWindow(window) => {
				// The gap is created by importing a finalized block right above it,
				// so `gap.target + 1` is a lower bound for the finalized number even
				// while the client's finality info still lags right after warp sync.
				let anchor = self.gap_sync.as_ref().map_or(finalized_number, |gap| {
					std::cmp::max(finalized_number, gap.target + One::one())
				});
				(attrs, Some(anchor.saturating_sub(window.into())))
			},
		}
	}

	/// Get block requests scheduled by sync to be sent out.
	fn block_requests(&mut self) -> Vec<(PeerId, BlockRequest<B>)> {
		if self.allowed_requests.is_empty() || self.state_sync.is_some() {
			return Vec::new();
		}

		if self.queue_blocks.len() > MAX_IMPORTING_BLOCKS {
			trace!(target: LOG_TARGET, "Too many blocks in the queue.");
			return Vec::new();
		}
		let is_major_syncing = self.status().state.is_major_syncing();
		let mode = self.mode;
		let finalized_number = self.client.info().finalized_number;
		let (gap_attrs, gap_body_cutoff) = self.gap_request_attributes(finalized_number);
		if let (Some(metrics), Some(cutoff), Some(gap)) =
			(self.metrics.as_ref(), gap_body_cutoff, self.gap_sync.as_ref())
		{
			// The oldest gap block still requiring a body: the lowest not-yet-queued
			// block, clamped from below by the cutoff.
			let oldest_required = std::cmp::max(gap.best_queued_number, cutoff) + One::one();
			metrics.gap_oldest_required_body.set(if oldest_required <= gap.target {
				oldest_required.saturated_into::<u64>()
			} else {
				0
			});
		}
		let blocks = &mut self.blocks;
		let fork_targets = &mut self.fork_targets;
		let last_finalized = std::cmp::min(self.best_queued_number, finalized_number);
		let best_queued = self.best_queued_number;
		let client = &self.client;
		let queue_blocks = &self.queue_blocks;
		let allowed_requests = self.allowed_requests.clone();
		let max_parallel = if is_major_syncing { 1 } else { self.max_parallel_downloads };
		let max_blocks_per_request = self.max_blocks_per_request;
		let gap_sync = &mut self.gap_sync;
		let disconnected_peers = &mut self.disconnected_peers;
		let metrics = self.metrics.as_ref();
		let requests = self
			.peers
			.iter_mut()
			.filter_map(move |(&id, peer)| {
				if !peer.state.is_available() ||
					!allowed_requests.contains(&id) ||
					!disconnected_peers.is_peer_available(&id)
				{
					return None;
				}

				// If our best queued is more than `MAX_BLOCKS_TO_LOOK_BACKWARDS` blocks away from
				// the common number, the peer best number is higher than our best queued and the
				// common number is smaller than the last finalized block number, we should do an
				// ancestor search to find a better common block. If the queue is full we wait till
				// all blocks are imported though.
				if best_queued.saturating_sub(peer.common_number) >
					MAX_BLOCKS_TO_LOOK_BACKWARDS.into() &&
					best_queued < peer.best_number &&
					peer.common_number < last_finalized &&
					queue_blocks.len() <= MAJOR_SYNC_BLOCKS as usize
				{
					trace!(
						target: LOG_TARGET,
						"Peer {:?} common block {} too far behind of our best {}. Starting ancestry search.",
						id,
						peer.common_number,
						best_queued,
					);
					let current = std::cmp::min(peer.best_number, best_queued);
					peer.state = PeerSyncState::AncestorSearch {
						current,
						start: best_queued,
						state: AncestorSearchState::ExponentialBackoff(One::one()),
					};
					Some((id, ancestry_request::<B>(current)))
				} else if let Some((range, req)) = peer_block_request(
					&id,
					peer,
					blocks,
					mode.required_block_attributes(),
					max_parallel,
					max_blocks_per_request,
					last_finalized,
					best_queued,
				) {
					peer.state = PeerSyncState::DownloadingNew(range.start);
					trace!(
						target: LOG_TARGET,
						"New block request for {}, (best:{}, common:{}) {:?}",
						id,
						peer.best_number,
						peer.common_number,
						req,
					);
					Some((id, req))
				} else if let Some((hash, req)) = fork_sync_request(
					&id,
					fork_targets,
					best_queued,
					last_finalized,
					mode.required_block_attributes(),
					|hash| {
						if queue_blocks.contains(hash) {
							BlockStatus::Queued
						} else {
							client.block_status(*hash).unwrap_or(BlockStatus::Unknown)
						}
					},
					max_blocks_per_request,
					metrics,
				) {
					trace!(target: LOG_TARGET, "Downloading fork {hash:?} from {id}");
					peer.state = PeerSyncState::DownloadingStale(hash);
					Some((id, req))
				} else if let Some((range, req)) = gap_sync.as_mut().and_then(|sync| {
					peer_gap_block_request(
						&id,
						peer,
						&mut sync.blocks,
						gap_attrs,
						gap_body_cutoff,
						is_major_syncing,
						sync.target,
						sync.best_queued_number,
						max_blocks_per_request,
						metrics,
					)
				}) {
					peer.state = PeerSyncState::DownloadingGap(range.start);
					trace!(
						target: LOG_TARGET,
						"New gap block request for {}, (best:{}, common:{}) {:?}",
						id,
						peer.best_number,
						peer.common_number,
						req,
					);
					Some((id, req))
				} else {
					None
				}
			})
			.collect::<Vec<_>>();

		// Clear the allowed_requests state when sending new block requests
		// to prevent multiple inflight block requests from being issued.
		if !requests.is_empty() {
			self.allowed_requests.take();
		}

		requests
	}

	/// Get a state request scheduled by sync to be sent out (if any).
	fn state_request(&mut self) -> Option<(PeerId, StateRequest)> {
		if self.allowed_requests.is_empty() {
			return None;
		}
		if self.state_sync.is_some() &&
			self.peers.iter().any(|(_, peer)| peer.state == PeerSyncState::DownloadingState)
		{
			// Only one pending state request is allowed.
			return None;
		}
		if let Some(sync) = &self.state_sync {
			if sync.is_complete() {
				return None;
			}

			for (id, peer) in self.peers.iter_mut() {
				if peer.state.is_available() &&
					peer.common_number >= sync.target_number() &&
					self.disconnected_peers.is_peer_available(&id)
				{
					peer.state = PeerSyncState::DownloadingState;
					let request = sync.next_request();
					trace!(target: LOG_TARGET, "New StateRequest for {}: {:?}", id, request);
					self.allowed_requests.clear();
					return Some((*id, request));
				}
			}
		}
		None
	}

	#[must_use]
	fn on_state_data(&mut self, peer_id: &PeerId, response: &[u8]) -> Result<(), BadPeer> {
		let response = match StateResponse::decode(response) {
			Ok(response) => response,
			Err(error) => {
				debug!(
					target: LOG_TARGET,
					"Failed to decode state response from peer {peer_id:?}: {error:?}.",
				);

				return Err(BadPeer(*peer_id, rep::BAD_RESPONSE));
			},
		};

		if let Some(peer) = self.peers.get_mut(peer_id) {
			if let PeerSyncState::DownloadingState = peer.state {
				peer.state = PeerSyncState::Available;
				self.allowed_requests.set_all();
			}
		}
		let import_result = if let Some(sync) = &mut self.state_sync {
			debug!(
				target: LOG_TARGET,
				"Importing state data from {} with {} keys, {} proof nodes.",
				peer_id,
				response.entries.len(),
				response.proof.len(),
			);
			sync.import(response)
		} else {
			debug!(target: LOG_TARGET, "Ignored obsolete state response from {peer_id}");
			return Err(BadPeer(*peer_id, rep::NOT_REQUESTED));
		};

		match import_result {
			ImportResult::Import(hash, header, state, body, justifications) => {
				let origin = BlockOrigin::NetworkInitialSync;
				let block = IncomingBlock {
					hash,
					header: Some(header),
					body,
					indexed_body: None,
					justifications,
					origin: None,
					allow_missing_state: true,
					import_existing: true,
					skip_execution: self.skip_execution(),
					state: Some(state),
				};
				debug!(target: LOG_TARGET, "State download is complete. Import is queued");
				self.actions.push(SyncingAction::ImportBlocks { origin, blocks: vec![block] });
				Ok(())
			},
			ImportResult::Continue => Ok(()),
			ImportResult::BadResponse => {
				debug!(target: LOG_TARGET, "Bad state data received from {peer_id}");
				Err(BadPeer(*peer_id, rep::BAD_BLOCK))
			},
		}
	}

	fn attempt_state_sync(
		&mut self,
		finalized_hash: B::Hash,
		finalized_number: NumberFor<B>,
		skip_proofs: bool,
	) {
		let mut heads: Vec<_> = self.peers.values().map(|peer| peer.best_number).collect();
		heads.sort();
		let median = heads[heads.len() / 2];
		if finalized_number + STATE_SYNC_FINALITY_THRESHOLD.saturated_into() >= median {
			if let Ok(Some(header)) = self.client.header(finalized_hash) {
				log::debug!(
					target: LOG_TARGET,
					"Starting state sync for #{finalized_number} ({finalized_hash})",
				);
				self.state_sync =
					Some(StateSync::new(self.client.clone(), header, None, None, skip_proofs));
				self.allowed_requests.set_all();
			} else {
				log::error!(
					target: LOG_TARGET,
					"Failed to start state sync: header for finalized block \
					  #{finalized_number} ({finalized_hash}) is not available",
				);
				debug_assert!(false);
			}
		}
	}

	/// A version of `actions()` that doesn't schedule extra requests. For testing only.
	#[cfg(test)]
	#[must_use]
	fn take_actions(&mut self) -> impl Iterator<Item = SyncingAction<B>> {
		std::mem::take(&mut self.actions).into_iter()
	}
}

// This is purely during a backwards compatible transitionary period and should be removed
// once we can assume all nodes can send and receive multiple Justifications
// The ID tag is hardcoded here to avoid depending on the GRANDPA crate.
// See: https://github.com/paritytech/substrate/issues/8172
fn legacy_justification_mapping(
	justification: Option<EncodedJustification>,
) -> Option<Justifications> {
	justification.map(|just| (*b"FRNK", just).into())
}

/// Request the ancestry for a block. Sends a request for header and justification for the given
/// block number. Used during ancestry search.
fn ancestry_request<B: BlockT>(block: NumberFor<B>) -> BlockRequest<B> {
	BlockRequest::<B> {
		id: 0,
		fields: BlockAttributes::HEADER | BlockAttributes::JUSTIFICATION,
		from: FromBlock::Number(block),
		direction: Direction::Ascending,
		max: Some(1),
	}
}

/// The ancestor search state expresses which algorithm, and its stateful parameters, we are using
/// to try to find an ancestor block
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum AncestorSearchState<B: BlockT> {
	/// Use exponential backoff to find an ancestor, then switch to binary search.
	/// We keep track of the exponent.
	ExponentialBackoff(NumberFor<B>),
	/// Using binary search to find the best ancestor.
	/// We keep track of left and right bounds.
	BinarySearch(NumberFor<B>, NumberFor<B>),
}

/// This function handles the ancestor search strategy used. The goal is to find a common point
/// that both our chains agree on that is as close to the tip as possible.
/// The way this works is we first have an exponential backoff strategy, where we try to step
/// forward until we find a block hash mismatch. The size of the step doubles each step we take.
///
/// When we've found a block hash mismatch we then fall back to a binary search between the two
/// last known points to find the common block closest to the tip.
fn handle_ancestor_search_state<B: BlockT>(
	state: &AncestorSearchState<B>,
	curr_block_num: NumberFor<B>,
	block_hash_match: bool,
) -> Option<(AncestorSearchState<B>, NumberFor<B>)> {
	let two = <NumberFor<B>>::one() + <NumberFor<B>>::one();
	match state {
		AncestorSearchState::ExponentialBackoff(next_distance_to_tip) => {
			let next_distance_to_tip = *next_distance_to_tip;
			if block_hash_match && next_distance_to_tip == One::one() {
				// We found the ancestor in the first step so there is no need to execute binary
				// search.
				return None;
			}
			if block_hash_match {
				let left = curr_block_num;
				let right = left + next_distance_to_tip / two;
				let middle = left + (right - left) / two;
				Some((AncestorSearchState::BinarySearch(left, right), middle))
			} else {
				let next_block_num =
					curr_block_num.checked_sub(&next_distance_to_tip).unwrap_or_else(Zero::zero);
				let next_distance_to_tip = next_distance_to_tip * two;
				Some((
					AncestorSearchState::ExponentialBackoff(next_distance_to_tip),
					next_block_num,
				))
			}
		},
		AncestorSearchState::BinarySearch(mut left, mut right) => {
			if left >= curr_block_num {
				return None;
			}
			if block_hash_match {
				left = curr_block_num;
			} else {
				right = curr_block_num;
			}
			assert!(right >= left);
			let middle = left + (right - left) / two;
			if middle == curr_block_num {
				None
			} else {
				Some((AncestorSearchState::BinarySearch(left, right), middle))
			}
		},
	}
}

/// Get a new block request for the peer if any.
fn peer_block_request<B: BlockT>(
	id: &PeerId,
	peer: &PeerSync<B>,
	blocks: &mut BlockCollection<B>,
	attrs: BlockAttributes,
	max_parallel_downloads: u32,
	max_blocks_per_request: u32,
	finalized: NumberFor<B>,
	best_num: NumberFor<B>,
) -> Option<(Range<NumberFor<B>>, BlockRequest<B>)> {
	if best_num >= peer.best_number {
		// Will be downloaded as alternative fork instead.
		return None;
	} else if peer.common_number < finalized {
		trace!(
			target: LOG_TARGET,
			"Requesting pre-finalized chain from {:?}, common={}, finalized={}, peer best={}, our best={}",
			id, peer.common_number, finalized, peer.best_number, best_num,
		);
	}
	let range = blocks.needed_blocks(
		*id,
		max_blocks_per_request,
		peer.best_number,
		peer.common_number,
		max_parallel_downloads,
		MAX_DOWNLOAD_AHEAD,
	)?;

	// The end is not part of the range.
	let last = range.end.saturating_sub(One::one());

	let from = if peer.best_number == last {
		FromBlock::Hash(peer.best_hash)
	} else {
		FromBlock::Number(last)
	};

	let request = BlockRequest::<B> {
		id: 0,
		fields: attrs,
		from,
		direction: Direction::Descending,
		max: Some((range.end - range.start).saturated_into::<u32>()),
	};

	Some((range, request))
}

/// Get a new gap block request for the peer if any.
///
/// Bodies are stripped from the request if the whole range is at or below
/// `body_cutoff`. A range straddling the cutoff requests bodies for all its blocks
/// rather than being split.
///
/// While the node is major syncing, the cutoff is a stale lower bound of the true one
/// (peers may have pruned bodies right above it), so scheduling is clamped to the
/// header-only region at or below it until the node has caught up.
fn peer_gap_block_request<B: BlockT>(
	id: &PeerId,
	peer: &PeerSync<B>,
	blocks: &mut BlockCollection<B>,
	attrs: BlockAttributes,
	body_cutoff: Option<NumberFor<B>>,
	is_major_syncing: bool,
	target: NumberFor<B>,
	common_number: NumberFor<B>,
	max_blocks_per_request: u32,
	metrics: Option<&Metrics>,
) -> Option<(Range<NumberFor<B>>, BlockRequest<B>)> {
	let mut scheduling_bound = std::cmp::min(peer.best_number, target);
	if let Some(cutoff) = body_cutoff.filter(|_| is_major_syncing) {
		scheduling_bound = std::cmp::min(scheduling_bound, cutoff);
	}
	let range = blocks.needed_blocks(
		*id,
		max_blocks_per_request,
		scheduling_bound,
		common_number,
		1,
		MAX_DOWNLOAD_AHEAD,
	)?;

	// The end is not part of the range.
	let last = range.end.saturating_sub(One::one());
	let from = FromBlock::Number(last);

	let attrs = match body_cutoff {
		Some(cutoff) if last <= cutoff => {
			if let Some(metrics) = metrics {
				metrics.gap_header_only_downgrades.inc();
			}
			attrs & !BlockAttributes::BODY
		},
		_ => attrs,
	};

	let request = BlockRequest::<B> {
		id: 0,
		fields: attrs,
		from,
		direction: Direction::Descending,
		max: Some((range.end - range.start).saturated_into::<u32>()),
	};
	Some((range, request))
}

/// Get pending fork sync targets for a peer.
fn fork_sync_request<B: BlockT>(
	id: &PeerId,
	fork_targets: &mut HashMap<B::Hash, ForkTarget<B>>,
	best_num: NumberFor<B>,
	finalized: NumberFor<B>,
	attributes: BlockAttributes,
	check_block: impl Fn(&B::Hash) -> BlockStatus,
	max_blocks_per_request: u32,
	metrics: Option<&Metrics>,
) -> Option<(B::Hash, BlockRequest<B>)> {
	fork_targets.retain(|hash, r| {
		if r.number <= finalized {
			trace!(
				target: LOG_TARGET,
				"Removed expired fork sync request {:?} (#{})",
				hash,
				r.number,
			);
			return false;
		}
		if check_block(hash) != BlockStatus::Unknown {
			trace!(
				target: LOG_TARGET,
				"Removed obsolete fork sync request {:?} (#{})",
				hash,
				r.number,
			);
			return false;
		}
		true
	});
	if let Some(metrics) = metrics {
		metrics.fork_targets.set(fork_targets.len().try_into().unwrap_or(u64::MAX));
	}
	for (hash, r) in fork_targets {
		if !r.peers.contains(&id) {
			continue;
		}
		// Download the fork only if it is behind or not too far ahead our tip of the chain
		// Otherwise it should be downloaded in full sync mode.
		if r.number <= best_num ||
			(r.number - best_num).saturated_into::<u32>() < max_blocks_per_request as u32
		{
			let parent_status = r.parent_hash.as_ref().map_or(BlockStatus::Unknown, check_block);
			let count = if parent_status == BlockStatus::Unknown {
				(r.number - finalized).saturated_into::<u32>() // up to the last finalized block
			} else {
				// request only single block
				1
			};
			trace!(
				target: LOG_TARGET,
				"Downloading requested fork {hash:?} from {id}, {count} blocks",
			);
			return Some((
				*hash,
				BlockRequest::<B> {
					id: 0,
					fields: attributes,
					from: FromBlock::Hash(*hash),
					direction: Direction::Descending,
					max: Some(count),
				},
			));
		} else {
			trace!(target: LOG_TARGET, "Fork too far in the future: {:?} (#{})", hash, r.number);
		}
	}
	None
}

/// Returns `true` if the given `block` is a descendent of `base`.
fn is_descendent_of<Block, T>(
	client: &T,
	base: &Block::Hash,
	block: &Block::Hash,
) -> sp_blockchain::Result<bool>
where
	Block: BlockT,
	T: HeaderMetadata<Block, Error = sp_blockchain::Error> + ?Sized,
{
	if base == block {
		return Ok(false);
	}

	let ancestor = sp_blockchain::lowest_common_ancestor(client, *block, *base)?;

	Ok(ancestor.hash == *base)
}

/// Validate that the given `blocks` are correct.
/// Returns the number of the first block in the sequence.
///
/// It is expected that `blocks` are in ascending order.
pub fn validate_blocks<Block: BlockT>(
	blocks: &Vec<BlockData<Block>>,
	peer_id: &PeerId,
	request: Option<BlockRequest<Block>>,
) -> Result<Option<NumberFor<Block>>, BadPeer> {
	if let Some(request) = request {
		if Some(blocks.len() as _) > request.max {
			debug!(
				target: LOG_TARGET,
				"Received more blocks than requested from {}. Expected in maximum {:?}, got {}.",
				peer_id,
				request.max,
				blocks.len(),
			);

			return Err(BadPeer(*peer_id, rep::NOT_REQUESTED));
		}

		let block_header =
			if request.direction == Direction::Descending { blocks.last() } else { blocks.first() }
				.and_then(|b| b.header.as_ref());

		let expected_block = block_header.as_ref().map_or(false, |h| match request.from {
			FromBlock::Hash(hash) => h.hash() == hash,
			FromBlock::Number(n) => h.number() == &n,
		});

		if !expected_block {
			debug!(
				target: LOG_TARGET,
				"Received block that was not requested. Requested {:?}, got {:?}.",
				request.from,
				block_header,
			);

			return Err(BadPeer(*peer_id, rep::NOT_REQUESTED));
		}

		if request.fields.contains(BlockAttributes::HEADER) &&
			blocks.iter().any(|b| b.header.is_none())
		{
			trace!(
				target: LOG_TARGET,
				"Missing requested header for a block in response from {peer_id}.",
			);

			return Err(BadPeer(*peer_id, rep::BAD_RESPONSE));
		}

		if request.fields.contains(BlockAttributes::BODY) && blocks.iter().any(|b| b.body.is_none())
		{
			trace!(
				target: LOG_TARGET,
				"Missing requested body for a block in response from {peer_id}.",
			);

			return Err(BadPeer(*peer_id, rep::BAD_RESPONSE));
		}
	}

	for b in blocks {
		if let Some(header) = &b.header {
			let hash = header.hash();
			if hash != b.hash {
				debug!(
					target: LOG_TARGET,
					"Bad header received from {}. Expected hash {:?}, got {:?}",
					peer_id,
					b.hash,
					hash,
				);
				return Err(BadPeer(*peer_id, rep::BAD_BLOCK));
			}
		}
	}

	Ok(blocks.first().and_then(|b| b.header.as_ref()).map(|h| *h.number()))
}
