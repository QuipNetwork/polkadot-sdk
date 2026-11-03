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

//! `SyncingEngine` is the actor responsible for syncing Substrate chain
//! to tip and keep the blockchain up to date with network updates.

use crate::{
	block_announce_validator::{
		BlockAnnounceValidationResult, BlockAnnounceValidator as BlockAnnounceValidatorStream,
	},
	pending_responses::{PendingResponses, ResponseEvent},
	service::{
		self,
		syncing_service::{SyncingService, ToServiceCommand},
	},
	strategy::{SyncingAction, SyncingStrategy},
	types::{BadPeer, ExtendedPeerInfo, SyncEvent},
	LOG_TARGET,
};

use codec::{Decode, DecodeAll, Encode};
use futures::{channel::oneshot, StreamExt};
use log::{debug, error, trace};
use prometheus_endpoint::{
	register, Counter, Gauge, MetricSource, Opts, PrometheusError, Registry, SourcedGauge, U64,
};
use schnellru::{ByLength, LruMap};
use tokio::time::{Interval, MissedTickBehavior};

use sc_client_api::{BlockBackend, HeaderBackend, ProofProvider};
use sc_consensus::{import_queue::ImportQueueService, IncomingBlock};
use sc_network::{
	config::{FullNetworkConfiguration, NotificationHandshake, ProtocolId, SetConfig},
	peer_store::PeerStoreProvider,
	request_responses::{OutboundFailure, RequestFailure},
	service::{
		traits::{Direction, NotificationConfig, NotificationEvent, ValidationResult},
		NotificationMetrics,
	},
	types::ProtocolName,
	utils::LruHashSet,
	NetworkBackend, NotificationService, ReputationChange,
};
use sc_network_common::{
	role::Roles,
	sync::message::{BlockAnnounce, BlockAnnouncesHandshake, BlockState},
};
use sc_network_types::PeerId;
use sc_utils::mpsc::{tracing_unbounded, TracingUnboundedReceiver, TracingUnboundedSender};
use sp_blockchain::{Error as ClientError, HeaderMetadata};
use sp_consensus::{block_validation::BlockAnnounceValidator, BlockOrigin};
use sp_runtime::{
	traits::{Block as BlockT, Header, NumberFor, Zero},
	Justifications,
};

use std::{
	collections::{HashMap, HashSet},
	iter,
	num::NonZeroUsize,
	sync::{
		atomic::{AtomicBool, AtomicUsize, Ordering},
		Arc,
	},
};

/// Interval at which we perform time based maintenance
const TICK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1100);

/// Maximum number of known block hashes to keep for a peer.
const MAX_KNOWN_BLOCKS: usize = 1024; // ~32kb per peer + LruHashSet overhead

/// Maximum allowed size for a block announce.
const MAX_BLOCK_ANNOUNCE_SIZE: u64 = 1024 * 1024;

/// Generate the block announces protocol name from the genesis hash and fork id.
pub fn block_announces_protocol_name<Hash: AsRef<[u8]>>(
	genesis_hash: Hash,
	fork_id: Option<&str>,
) -> String {
	let genesis_hash = genesis_hash.as_ref();
	if let Some(fork_id) = fork_id {
		format!("/{}/{}/block-announces/1", array_bytes::bytes2hex("", genesis_hash), fork_id)
	} else {
		format!("/{}/block-announces/1", array_bytes::bytes2hex("", genesis_hash))
	}
}

/// Generate the legacy block announces protocol name from chain specific protocol identifier.
pub fn block_announces_legacy_protocol_name(protocol_id: &ProtocolId) -> String {
	format!("/{}/block-announces/1", protocol_id.as_ref())
}

mod rep {
	use sc_network::ReputationChange as Rep;
	/// Peer has different genesis.
	pub const GENESIS_MISMATCH: Rep = Rep::new_fatal("Genesis mismatch");
	/// Peer send us a block announcement that failed at validation.
	pub const BAD_BLOCK_ANNOUNCEMENT: Rep = Rep::new(-(1 << 12), "Bad block announcement");
	/// Peer is on unsupported protocol version.
	pub const BAD_PROTOCOL: Rep = Rep::new_fatal("Unsupported protocol");
	/// Reputation change when a peer refuses a request.
	pub const REFUSED: Rep = Rep::new(-(1 << 10), "Request refused");
	/// Reputation change when a peer doesn't respond in time to our messages.
	pub const TIMEOUT: Rep = Rep::new(-(1 << 10), "Request timeout");
	/// Reputation change when a peer connection failed with IO error.
	pub const IO: Rep = Rep::new(-(1 << 10), "IO error during request");
}

struct Metrics {
	peers: Gauge<U64>,
	import_queue_blocks_submitted: Counter<U64>,
	import_queue_justifications_submitted: Counter<U64>,
}

impl Metrics {
	fn register(r: &Registry, major_syncing: Arc<AtomicBool>) -> Result<Self, PrometheusError> {
		MajorSyncingGauge::register(r, major_syncing)?;
		Ok(Self {
			peers: {
				let g = Gauge::new("substrate_sync_peers", "Number of peers we sync with")?;
				register(g, r)?
			},
			import_queue_blocks_submitted: {
				let c = Counter::new(
					"substrate_sync_import_queue_blocks_submitted",
					"Number of blocks submitted to the import queue.",
				)?;
				register(c, r)?
			},
			import_queue_justifications_submitted: {
				let c = Counter::new(
					"substrate_sync_import_queue_justifications_submitted",
					"Number of justifications submitted to the import queue.",
				)?;
				register(c, r)?
			},
		})
	}
}

/// The "major syncing" metric.
#[derive(Clone)]
pub struct MajorSyncingGauge(Arc<AtomicBool>);

impl MajorSyncingGauge {
	/// Registers the [`MajorSyncGauge`] metric whose value is
	/// obtained from the given `AtomicBool`.
	fn register(registry: &Registry, value: Arc<AtomicBool>) -> Result<(), PrometheusError> {
		prometheus_endpoint::register(
			SourcedGauge::new(
				&Opts::new(
					"substrate_sub_libp2p_is_major_syncing",
					"Whether the node is performing a major sync or not.",
				),
				MajorSyncingGauge(value),
			)?,
			registry,
		)?;

		Ok(())
	}
}

impl MetricSource for MajorSyncingGauge {
	type N = u64;

	fn collect(&self, mut set: impl FnMut(&[&str], Self::N)) {
		set(&[], self.0.load(Ordering::Relaxed) as u64);
	}
}

/// Peer information
#[derive(Debug)]
pub struct Peer<B: BlockT> {
	pub info: ExtendedPeerInfo<B>,
	/// Holds a set of blocks known to this peer.
	pub known_blocks: LruHashSet<B::Hash>,
	/// Is the peer inbound.
	inbound: bool,
}

pub struct SyncingEngine<B: BlockT, Client> {
	/// Syncing strategy.
	strategy: Box<dyn SyncingStrategy<B>>,

	/// Blockchain client.
	client: Arc<Client>,

	/// Number of peers we're connected to.
	num_connected: Arc<AtomicUsize>,

	/// Are we actively catching up with the chain?
	is_major_syncing: Arc<AtomicBool>,

	/// Network service.
	network_service: service::network::NetworkServiceHandle,

	/// Channel for receiving service commands
	service_rx: TracingUnboundedReceiver<ToServiceCommand<B>>,

	/// Assigned roles.
	roles: Roles,

	/// Genesis hash.
	genesis_hash: B::Hash,

	/// Set of channels for other protocols that have subscribed to syncing events.
	event_streams: Vec<TracingUnboundedSender<SyncEvent>>,

	/// Interval at which we call `tick`.
	tick_timeout: Interval,

	/// All connected peers. Contains both full and light node peers.
	peers: HashMap<PeerId, Peer<B>>,

	/// List of nodes for which we perform additional logging because they are important for the
	/// user.
	important_peers: HashSet<PeerId>,

	/// Actual list of connected no-slot nodes.
	default_peers_set_no_slot_connected_peers: HashSet<PeerId>,

	/// List of nodes that should never occupy peer slots.
	default_peers_set_no_slot_peers: HashSet<PeerId>,

	/// Value that was passed as part of the configuration. Used to cap the number of full
	/// nodes.
	default_peers_set_num_full: usize,

	/// Number of slots to allocate to light nodes.
	default_peers_set_num_light: usize,

	/// Maximum number of inbound peers.
	max_in_peers: usize,

	/// Number of inbound peers accepted so far.
	num_in_peers: usize,

	/// Dynamic updatable no-slot peer set (see [`SyncingService::set_no_slot_peers`]).
	/// Treated identically to `default_peers_set_no_slot_peers` for inbound slot accounting.
	dynamic_no_slot_peers: HashSet<PeerId>,

	/// Async processor of block announce validations.
	block_announce_validator: BlockAnnounceValidatorStream<B>,

	/// A cache for the data that was associated to a block announcement.
	block_announce_data_cache: LruMap<B::Hash, Vec<u8>>,

	/// The `PeerId`'s of all boot nodes.
	boot_node_ids: HashSet<PeerId>,

	/// Protocol name used for block announcements
	block_announce_protocol_name: ProtocolName,

	/// Prometheus metrics.
	metrics: Option<Metrics>,

	/// Handle that is used to communicate with `sc_network::Notifications`.
	notification_service: Box<dyn NotificationService>,

	/// Handle to `PeerStore`.
	peer_store_handle: Arc<dyn PeerStoreProvider>,

	/// Pending responses
	pending_responses: PendingResponses,

	/// Handle to import queue.
	import_queue: Box<dyn ImportQueueService<B>>,
}

impl<B: BlockT, Client> SyncingEngine<B, Client>
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
	pub fn new<N>(
		roles: Roles,
		client: Arc<Client>,
		metrics_registry: Option<&Registry>,
		network_metrics: NotificationMetrics,
		net_config: &FullNetworkConfiguration<B, <B as BlockT>::Hash, N>,
		protocol_id: ProtocolId,
		fork_id: Option<&str>,
		block_announce_validator: Box<dyn BlockAnnounceValidator<B> + Send>,
		syncing_strategy: Box<dyn SyncingStrategy<B>>,
		network_service: service::network::NetworkServiceHandle,
		import_queue: Box<dyn ImportQueueService<B>>,
		peer_store_handle: Arc<dyn PeerStoreProvider>,
	) -> Result<(Self, SyncingService<B>, N::NotificationProtocolConfig), ClientError>
	where
		N: NetworkBackend<B, <B as BlockT>::Hash>,
	{
		let cache_capacity = (net_config.network_config.default_peers_set.in_peers +
			net_config.network_config.default_peers_set.out_peers)
			.max(1);
		let important_peers = {
			let mut imp_p = HashSet::new();
			for reserved in &net_config.network_config.default_peers_set.reserved_nodes {
				imp_p.insert(reserved.peer_id);
			}
			for config in net_config.notification_protocols() {
				let peer_ids = config.set_config().reserved_nodes.iter().map(|info| info.peer_id);
				imp_p.extend(peer_ids);
			}

			imp_p.shrink_to_fit();
			imp_p
		};
		let boot_node_ids = {
			let mut list = HashSet::new();
			for node in &net_config.network_config.boot_nodes {
				list.insert(node.peer_id);
			}
			list.shrink_to_fit();
			list
		};
		let default_peers_set_no_slot_peers = {
			let mut no_slot_p: HashSet<PeerId> = net_config
				.network_config
				.default_peers_set
				.reserved_nodes
				.iter()
				.map(|reserved| reserved.peer_id)
				.collect();
			no_slot_p.shrink_to_fit();
			no_slot_p
		};
		let default_peers_set_num_full =
			net_config.network_config.default_peers_set_num_full as usize;
		let default_peers_set_num_light = {
			let total = net_config.network_config.default_peers_set.out_peers +
				net_config.network_config.default_peers_set.in_peers;
			total.saturating_sub(net_config.network_config.default_peers_set_num_full) as usize
		};

		let info = client.info();

		let (block_announce_config, notification_service) =
			Self::get_block_announce_proto_config::<N>(
				protocol_id,
				fork_id,
				roles,
				info.best_number,
				info.best_hash,
				info.genesis_hash,
				&net_config.network_config.default_peers_set,
				network_metrics,
				Arc::clone(&peer_store_handle),
			);

		let block_announce_protocol_name = block_announce_config.protocol_name().clone();
		let (tx, service_rx) = tracing_unbounded("mpsc_chain_sync", 100_000);
		let num_connected = Arc::new(AtomicUsize::new(0));
		let is_major_syncing = Arc::new(AtomicBool::new(false));

		// `default_peers_set.in_peers` contains an unspecified amount of light peers so the number
		// of full inbound peers must be calculated from the total full peer count
		let max_full_peers = net_config.network_config.default_peers_set_num_full;
		let max_out_peers = net_config.network_config.default_peers_set.out_peers;
		let max_in_peers = (max_full_peers - max_out_peers) as usize;

		let tick_timeout = {
			let mut interval = tokio::time::interval(TICK_TIMEOUT);
			interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
			interval
		};

		Ok((
			Self {
				roles,
				client,
				strategy: syncing_strategy,
				network_service,
				peers: HashMap::new(),
				block_announce_data_cache: LruMap::new(ByLength::new(cache_capacity)),
				block_announce_protocol_name,
				block_announce_validator: BlockAnnounceValidatorStream::new(
					block_announce_validator,
				),
				num_connected: num_connected.clone(),
				is_major_syncing: is_major_syncing.clone(),
				service_rx,
				genesis_hash: info.genesis_hash,
				important_peers,
				default_peers_set_no_slot_connected_peers: HashSet::new(),
				boot_node_ids,
				default_peers_set_no_slot_peers,
				default_peers_set_num_full,
				default_peers_set_num_light,
				num_in_peers: 0usize,
				max_in_peers,
				dynamic_no_slot_peers: HashSet::new(),
				event_streams: Vec::new(),
				notification_service,
				tick_timeout,
				peer_store_handle,
				metrics: if let Some(r) = metrics_registry {
					match Metrics::register(r, is_major_syncing.clone()) {
						Ok(metrics) => Some(metrics),
						Err(err) => {
							log::error!(target: LOG_TARGET, "Failed to register metrics {err:?}");
							None
						},
					}
				} else {
					None
				},
				pending_responses: PendingResponses::new(),
				import_queue,
			},
			SyncingService::new(tx, num_connected, is_major_syncing),
			block_announce_config,
		))
	}

	fn update_peer_info(
		&mut self,
		peer_id: &PeerId,
		best_hash: B::Hash,
		best_number: NumberFor<B>,
	) {
		if let Some(ref mut peer) = self.peers.get_mut(peer_id) {
			peer.info.best_hash = best_hash;
			peer.info.best_number = best_number;
		}
	}

	/// Process the result of the block announce validation.
	fn process_block_announce_validation_result(
		&mut self,
		validation_result: BlockAnnounceValidationResult<B::Header>,
	) {
		match validation_result {
			BlockAnnounceValidationResult::Skip { peer_id: _ } => {},
			BlockAnnounceValidationResult::Process { is_new_best, peer_id, announce } => {
				if let Some((best_hash, best_number)) =
					self.strategy.on_validated_block_announce(is_new_best, peer_id, &announce)
				{
					self.update_peer_info(&peer_id, best_hash, best_number);
				}

				if let Some(data) = announce.data {
					if !data.is_empty() {
						self.block_announce_data_cache.insert(announce.header.hash(), data);
					}
				}
			},
			BlockAnnounceValidationResult::Failure { peer_id, disconnect } => {
				if disconnect {
					log::debug!(
						target: LOG_TARGET,
						"Disconnecting peer {peer_id} due to block announce validation failure",
					);
					self.network_service
						.disconnect_peer(peer_id, self.block_announce_protocol_name.clone());
				}

				self.network_service.report_peer(peer_id, rep::BAD_BLOCK_ANNOUNCEMENT);
			},
		}
	}

	/// Push a block announce validation.
	pub fn push_block_announce_validation(
		&mut self,
		peer_id: PeerId,
		announce: BlockAnnounce<B::Header>,
	) {
		let hash = announce.header.hash();

		let peer = match self.peers.get_mut(&peer_id) {
			Some(p) => p,
			None => {
				log::error!(
					target: LOG_TARGET,
					"Received block announce from disconnected peer {peer_id}",
				);
				debug_assert!(false);
				return;
			},
		};
		peer.known_blocks.insert(hash);

		if peer.info.roles.is_full() {
			let is_best = match announce.state.unwrap_or(BlockState::Best) {
				BlockState::Best => true,
				BlockState::Normal => false,
			};

			self.block_announce_validator
				.push_block_announce_validation(peer_id, hash, announce, is_best);
		}
	}

	/// Make sure an important block is propagated to peers.
	///
	/// In chain-based consensus, we often need to make sure non-best forks are
	/// at least temporarily synced.
	pub fn announce_block(&mut self, hash: B::Hash, data: Option<Vec<u8>>) {
		let header = match self.client.header(hash) {
			Ok(Some(header)) => header,
			Ok(None) => {
				log::warn!(target: LOG_TARGET, "Trying to announce unknown block: {hash}");
				return;
			},
			Err(e) => {
				log::warn!(target: LOG_TARGET, "Error reading block header {hash}: {e}");
				return;
			},
		};

		// don't announce genesis block since it will be ignored
		if header.number().is_zero() {
			return;
		}

		let is_best = self.client.info().best_hash == hash;
		log::debug!(target: LOG_TARGET, "Reannouncing block {hash:?} is_best: {is_best}");

		let data = data
			.or_else(|| self.block_announce_data_cache.get(&hash).cloned())
			.unwrap_or_default();

		for (peer_id, ref mut peer) in self.peers.iter_mut() {
			let inserted = peer.known_blocks.insert(hash);
			if inserted {
				log::trace!(target: LOG_TARGET, "Announcing block {hash:?} to {peer_id}");
				let message = BlockAnnounce {
					header: header.clone(),
					state: if is_best { Some(BlockState::Best) } else { Some(BlockState::Normal) },
					data: Some(data.clone()),
				};

				let _ = self.notification_service.send_sync_notification(peer_id, message.encode());
			}
		}
	}

	pub async fn run(mut self) {
		loop {
			tokio::select! {
				_ = self.tick_timeout.tick() => {
					// TODO: This tick should not be necessary, but
					//  `self.process_strategy_actions()` is not called in some cases otherwise and
					//  some tests fail because of this
				},
				command = self.service_rx.select_next_some() =>
					self.process_service_command(command),
				notification_event = self.notification_service.next_event() => match notification_event {
					Some(event) => self.process_notification_event(event),
					None => {
						error!(
							target: LOG_TARGET,
							"Terminating `SyncingEngine` because `NotificationService` has terminated.",
						);

						return;
					}
				},
				response_event = self.pending_responses.select_next_some() =>
					self.process_response_event(response_event),
				validation_result = self.block_announce_validator.select_next_some() =>
					self.process_block_announce_validation_result(validation_result),
			}

			// Update atomic variables
			self.is_major_syncing.store(self.strategy.is_major_syncing(), Ordering::Relaxed);

			// Process actions requested by a syncing strategy.
			if let Err(e) = self.process_strategy_actions() {
				error!(
					target: LOG_TARGET,
					"Terminating `SyncingEngine` due to fatal error: {e:?}.",
				);
				return;
			}
		}
	}

	fn process_strategy_actions(&mut self) -> Result<(), ClientError> {
		for action in self.strategy.actions(&self.network_service)? {
			match action {
				SyncingAction::StartRequest { peer_id, key, request } => {
					if !self.peers.contains_key(&peer_id) {
						trace!(
							target: LOG_TARGET,
							"Cannot start request with strategy key {key:?} to unknown peer \
							{peer_id}",
						);
						debug_assert!(false);
						continue;
					}

					self.pending_responses.insert(peer_id, key, request);
				},
				SyncingAction::CancelRequest { peer_id, key } => {
					let removed = self.pending_responses.remove(peer_id, key);

					trace!(
						target: LOG_TARGET,
						"Processed `SyncingAction::CancelRequest`, response removed: {removed}.",
					);
				},
				SyncingAction::DropPeer(BadPeer(peer_id, rep)) => {
					self.pending_responses.remove_all(&peer_id);
					self.network_service
						.disconnect_peer(peer_id, self.block_announce_protocol_name.clone());
					self.network_service.report_peer(peer_id, rep);

					trace!(target: LOG_TARGET, "{peer_id:?} dropped: {rep:?}.");
				},
				SyncingAction::ImportBlocks { origin, blocks } => {
					let count = blocks.len();
					self.import_blocks(origin, blocks);

					trace!(
						target: LOG_TARGET,
						"Processed `ChainSyncAction::ImportBlocks` with {count} blocks.",
					);
				},
				SyncingAction::ImportJustifications { peer_id, hash, number, justifications } => {
					self.import_justifications(peer_id, hash, number, justifications);

					trace!(
						target: LOG_TARGET,
						"Processed `ChainSyncAction::ImportJustifications` from peer {} for block {} ({}).",
						peer_id,
						hash,
						number,
					)
				},
				// Nothing to do, this is handled internally by `PolkadotSyncingStrategy`.
				SyncingAction::Finished => {},
			}
		}

		Ok(())
	}

	/// Reconcile per-peer slot tracking against `new_dynamic_no_slot`. See
	/// [`apply_no_slot_set_inner`] for details.
	fn apply_no_slot_set(&mut self, new_dynamic_no_slot: HashSet<PeerId>) {
		let connected_peers = &self.peers;
		apply_no_slot_set_inner(
			|peer_id| {
				connected_peers
					.get(peer_id)
					.map(|peer| peer.inbound && peer.info.roles.is_full())
			},
			&self.default_peers_set_no_slot_peers,
			&self.dynamic_no_slot_peers,
			&new_dynamic_no_slot,
			&mut self.default_peers_set_no_slot_connected_peers,
			&mut self.num_in_peers,
			self.max_in_peers,
			&self.network_service,
			&self.block_announce_protocol_name,
		);
		self.dynamic_no_slot_peers = new_dynamic_no_slot;
	}

	fn process_service_command(&mut self, command: ToServiceCommand<B>) {
		match command {
			ToServiceCommand::SetSyncForkRequest(peers, hash, number) => {
				self.strategy.set_sync_fork_request(peers, &hash, number);
			},
			ToServiceCommand::EventStream(tx) => {
				// Let a new subscriber know about already connected peers.
				for (peer_id, peer) in self.peers.iter() {
					let _ = tx.unbounded_send(SyncEvent::PeerConnected {
						peer_id: *peer_id,
						roles: peer.info.roles,
					});
				}
				self.event_streams.push(tx);
			},
			ToServiceCommand::RequestJustification(hash, number) => {
				self.strategy.request_justification(&hash, number)
			},
			ToServiceCommand::ClearJustificationRequests => {
				self.strategy.clear_justification_requests()
			},
			ToServiceCommand::BlocksProcessed(imported, count, results) => {
				self.strategy.on_blocks_processed(imported, count, results);
			},
			ToServiceCommand::JustificationImported(peer_id, hash, number, import_result) => {
				let success =
					matches!(import_result, sc_consensus::JustificationImportResult::Success);
				self.strategy.on_justification_import(hash, number, success);

				match import_result {
					sc_consensus::JustificationImportResult::OutdatedJustification => {
						log::info!(
							target: LOG_TARGET,
							"💔 Outdated justification provided by {peer_id} for #{hash}",
						);
					},
					sc_consensus::JustificationImportResult::Failure => {
						log::info!(
							target: LOG_TARGET,
							"💔 Invalid justification provided by {peer_id} for #{hash}",
						);
						self.network_service
							.disconnect_peer(peer_id, self.block_announce_protocol_name.clone());
						self.network_service.report_peer(
							peer_id,
							ReputationChange::new_fatal("Invalid justification"),
						);
					},
					sc_consensus::JustificationImportResult::Success => {
						log::debug!(
							target: LOG_TARGET,
							"Justification for block #{hash} ({number}) imported from {peer_id} successfully",
						);
					},
				}
			},
			ToServiceCommand::AnnounceBlock(hash, data) => self.announce_block(hash, data),
			ToServiceCommand::NewBestBlockImported(hash, number) => {
				log::debug!(target: LOG_TARGET, "New best block imported {:?}/#{}", hash, number);

				self.strategy.update_chain_info(&hash, number);
				let _ = self.notification_service.try_set_handshake(
					BlockAnnouncesHandshake::<B>::build(
						self.roles,
						number,
						hash,
						self.genesis_hash,
					)
					.encode(),
				);
			},
			ToServiceCommand::Status(tx) => {
				let _ = tx.send(self.strategy.status());
			},
			ToServiceCommand::NumActivePeers(tx) => {
				let _ = tx.send(self.num_active_peers());
			},
			ToServiceCommand::NumDownloadedBlocks(tx) => {
				let _ = tx.send(self.strategy.num_downloaded_blocks());
			},
			ToServiceCommand::NumSyncRequests(tx) => {
				let _ = tx.send(self.strategy.num_sync_requests());
			},
			ToServiceCommand::PeersInfo(tx) => {
				let peers_info =
					self.peers.iter().map(|(peer_id, peer)| (*peer_id, peer.info)).collect();
				let _ = tx.send(peers_info);
			},
			ToServiceCommand::SetNoSlotPeers(peers) => self.apply_no_slot_set(peers),
			ToServiceCommand::OnBlockFinalized(hash, header) => {
				self.strategy.on_block_finalized(&hash, *header.number())
			},
		}
	}

	fn process_notification_event(&mut self, event: NotificationEvent) {
		match event {
			NotificationEvent::ValidateInboundSubstream { peer, handshake, result_tx } => {
				let validation_result = self
					.validate_connection(&peer, handshake, Direction::Inbound)
					.map_or(ValidationResult::Reject, |_| ValidationResult::Accept);

				let _ = result_tx.send(validation_result);
			},
			NotificationEvent::NotificationStreamOpened { peer, handshake, direction, .. } => {
				log::debug!(
					target: LOG_TARGET,
					"Substream opened for {peer}, handshake {handshake:?}"
				);

				match self.validate_connection(&peer, handshake, direction) {
					Ok(handshake) => {
						if self.on_sync_peer_connected(peer, &handshake, direction).is_err() {
							log::debug!(target: LOG_TARGET, "Failed to register peer {peer}");
							self.network_service
								.disconnect_peer(peer, self.block_announce_protocol_name.clone());
						}
					},
					Err(wrong_genesis) => {
						log::debug!(target: LOG_TARGET, "`SyncingEngine` rejected {peer}");

						if wrong_genesis {
							self.peer_store_handle.report_peer(peer, rep::GENESIS_MISMATCH);
						}

						self.network_service
							.disconnect_peer(peer, self.block_announce_protocol_name.clone());
					},
				}
			},
			NotificationEvent::NotificationStreamClosed { peer } => {
				self.on_sync_peer_disconnected(peer);
			},
			NotificationEvent::NotificationReceived { peer, notification } => {
				if !self.peers.contains_key(&peer) {
					log::error!(
						target: LOG_TARGET,
						"received notification from {peer} who had been earlier refused by `SyncingEngine`",
					);
					return;
				}

				let Ok(announce) = BlockAnnounce::decode(&mut notification.as_ref()) else {
					log::warn!(target: LOG_TARGET, "failed to decode block announce");
					return;
				};

				self.push_block_announce_validation(peer, announce);
			},
		}
	}

	fn is_no_slot_peer(&self, peer_id: &PeerId) -> bool {
		self.default_peers_set_no_slot_peers.contains(peer_id) ||
			self.dynamic_no_slot_peers.contains(peer_id)
	}

	/// Called by peer when it is disconnecting.
	///
	/// Returns a result if the handshake of this peer was indeed accepted.
	fn on_sync_peer_disconnected(&mut self, peer_id: PeerId) {
		let Some(info) = self.peers.remove(&peer_id) else {
			log::debug!(target: LOG_TARGET, "{peer_id} does not exist in `SyncingEngine`");
			return;
		};
		if let Some(metrics) = &self.metrics {
			metrics.peers.dec();
		}
		self.num_connected.fetch_sub(1, Ordering::AcqRel);

		if self.important_peers.contains(&peer_id) {
			log::warn!(target: LOG_TARGET, "Reserved peer {peer_id} disconnected");
		} else {
			log::debug!(target: LOG_TARGET, "{peer_id} disconnected");
		}

		if !self.default_peers_set_no_slot_connected_peers.remove(&peer_id) &&
			info.inbound &&
			info.info.roles.is_full()
		{
			match self.num_in_peers.checked_sub(1) {
				Some(value) => {
					self.num_in_peers = value;
				},
				None => {
					log::error!(
						target: LOG_TARGET,
						"trying to disconnect an inbound node which is not counted as inbound"
					);
					debug_assert!(false);
				},
			}
		}

		self.strategy.remove_peer(&peer_id);
		self.pending_responses.remove_all(&peer_id);
		self.event_streams
			.retain(|stream| stream.unbounded_send(SyncEvent::PeerDisconnected(peer_id)).is_ok());
	}

	/// Validate received handshake.
	fn validate_handshake(
		&mut self,
		peer_id: &PeerId,
		handshake: Vec<u8>,
	) -> Result<BlockAnnouncesHandshake<B>, bool> {
		log::trace!(target: LOG_TARGET, "Validate handshake for {peer_id}");

		let handshake = <BlockAnnouncesHandshake<B> as DecodeAll>::decode_all(&mut &handshake[..])
			.map_err(|error| {
				log::debug!(target: LOG_TARGET, "Failed to decode handshake for {peer_id}: {error:?}");
				false
			})?;

		if handshake.genesis_hash != self.genesis_hash {
			if self.important_peers.contains(&peer_id) {
				log::error!(
					target: LOG_TARGET,
					"Reserved peer id `{peer_id}` is on a different chain (our genesis: {} theirs: {})",
					self.genesis_hash,
					handshake.genesis_hash,
				);
			} else if self.boot_node_ids.contains(&peer_id) {
				log::error!(
					target: LOG_TARGET,
					"Bootnode with peer id `{peer_id}` is on a different chain (our genesis: {} theirs: {})",
					self.genesis_hash,
					handshake.genesis_hash,
				);
			} else {
				log::debug!(
					target: LOG_TARGET,
					"Peer is on different chain (our genesis: {} theirs: {})",
					self.genesis_hash,
					handshake.genesis_hash
				);
			}

			return Err(true);
		}

		Ok(handshake)
	}

	/// Validate connection.
	// NOTE Returning `Err(bool)` is a really ugly hack to work around the issue
	// that `ProtocolController` thinks the peer is connected when in fact it can
	// still be under validation. If the peer has different genesis than the
	// local node the validation fails but the peer cannot be reported in
	// `validate_connection()` as that is also called by
	// `ValidateInboundSubstream` which means that the peer is still being
	// validated and banning the peer when handling that event would
	// result in peer getting dropped twice.
	//
	// The proper way to fix this is to integrate `ProtocolController` more
	// tightly with `NotificationService` or add an additional API call for
	// banning pre-accepted peers (which is not desirable)
	fn validate_connection(
		&mut self,
		peer_id: &PeerId,
		handshake: Vec<u8>,
		direction: Direction,
	) -> Result<BlockAnnouncesHandshake<B>, bool> {
		log::trace!(target: LOG_TARGET, "New peer {peer_id} {handshake:?}");

		let handshake = self.validate_handshake(peer_id, handshake)?;

		if self.peers.contains_key(&peer_id) {
			log::error!(
				target: LOG_TARGET,
				"Called `validate_connection()` with already connected peer {peer_id}",
			);
			debug_assert!(false);
			return Err(false);
		}

		let no_slot_peer = self.is_no_slot_peer(&peer_id);
		let this_peer_reserved_slot: usize = if no_slot_peer { 1 } else { 0 };

		if handshake.roles.is_full() &&
			self.strategy.num_peers() >=
				self.default_peers_set_num_full +
					self.default_peers_set_no_slot_connected_peers.len() +
					this_peer_reserved_slot
		{
			log::debug!(
				target: LOG_TARGET,
				"Too many full nodes, rejecting {peer_id} (no_slot_peer={no_slot_peer}, num_peers={}, full_cap={}, no_slot_connected={}, this_reserved={})",
				self.strategy.num_peers(),
				self.default_peers_set_num_full,
				self.default_peers_set_no_slot_connected_peers.len(),
				this_peer_reserved_slot,
			);
			return Err(false);
		}

		// make sure to accept no more than `--in-peers` many full nodes
		if !no_slot_peer &&
			handshake.roles.is_full() &&
			direction.is_inbound() &&
			self.num_in_peers >= self.max_in_peers
		{
			if self.num_in_peers > self.max_in_peers {
				log::warn!(
					target: LOG_TARGET,
					"num_in_peers ({}) exceeds max_in_peers ({}), this is a slot accounting bug ",
					self.num_in_peers,
					self.max_in_peers,
				);
				debug_assert!(false);
			}
			log::debug!(
				target: LOG_TARGET,
				"All inbound slots have been consumed, rejecting {peer_id} (no_slot_peer={no_slot_peer}, num_in_peers={}, max_in_peers={})",
				self.num_in_peers,
				self.max_in_peers,
			);
			return Err(false);
		}

		// make sure that all slots are not occupied by light peers
		//
		// `ChainSync` only accepts full peers whereas `SyncingEngine` accepts both full and light
		// peers. Verify that there is a slot in `SyncingEngine` for the inbound light peer
		if handshake.roles.is_light() &&
			(self.peers.len() - self.strategy.num_peers()) >= self.default_peers_set_num_light
		{
			log::debug!(target: LOG_TARGET, "Too many light nodes, rejecting {peer_id}");
			return Err(false);
		}

		Ok(handshake)
	}

	/// Called on the first connection between two peers on the default set, after their exchange
	/// of handshake.
	///
	/// Returns `Ok` if the handshake is accepted and the peer added to the list of peers we sync
	/// from.
	fn on_sync_peer_connected(
		&mut self,
		peer_id: PeerId,
		status: &BlockAnnouncesHandshake<B>,
		direction: Direction,
	) -> Result<(), ()> {
		log::trace!(target: LOG_TARGET, "New peer {peer_id} {status:?}");

		let peer = Peer {
			info: ExtendedPeerInfo {
				roles: status.roles,
				best_hash: status.best_hash,
				best_number: status.best_number,
			},
			known_blocks: LruHashSet::new(
				NonZeroUsize::new(MAX_KNOWN_BLOCKS).expect("Constant is nonzero"),
			),
			inbound: direction.is_inbound(),
		};

		// Only forward full peers to syncing strategy.
		if status.roles.is_full() {
			self.strategy.add_peer(peer_id, peer.info.best_hash, peer.info.best_number);
		}

		log::debug!(target: LOG_TARGET, "Connected {peer_id}");

		if self.peers.insert(peer_id, peer).is_none() {
			if let Some(metrics) = &self.metrics {
				metrics.peers.inc();
			}
			self.num_connected.fetch_add(1, Ordering::AcqRel);
		}
		self.peer_store_handle.set_peer_role(&peer_id, status.roles.into());

		if self.is_no_slot_peer(&peer_id) {
			self.default_peers_set_no_slot_connected_peers.insert(peer_id);
		} else if direction.is_inbound() && status.roles.is_full() {
			self.num_in_peers += 1;
		}

		self.event_streams.retain(|stream| {
			stream
				.unbounded_send(SyncEvent::PeerConnected { peer_id, roles: status.roles })
				.is_ok()
		});

		Ok(())
	}

	fn process_response_event(&mut self, response_event: ResponseEvent) {
		let ResponseEvent { peer_id, key, response: response_result } = response_event;

		match response_result {
			Ok(Ok((response, protocol_name))) => {
				self.strategy.on_generic_response(&peer_id, key, protocol_name, response);
			},
			Ok(Err(e)) => {
				debug!(target: LOG_TARGET, "Request to peer {peer_id:?} failed: {e:?}.");

				match e {
					RequestFailure::Network(OutboundFailure::Timeout) => {
						self.network_service.report_peer(peer_id, rep::TIMEOUT);
						self.network_service
							.disconnect_peer(peer_id, self.block_announce_protocol_name.clone());
					},
					RequestFailure::Network(OutboundFailure::UnsupportedProtocols) => {
						self.network_service.report_peer(peer_id, rep::BAD_PROTOCOL);
						self.network_service
							.disconnect_peer(peer_id, self.block_announce_protocol_name.clone());
					},
					RequestFailure::Network(OutboundFailure::DialFailure) => {
						self.network_service
							.disconnect_peer(peer_id, self.block_announce_protocol_name.clone());
					},
					RequestFailure::Refused => {
						self.network_service.report_peer(peer_id, rep::REFUSED);
						self.network_service
							.disconnect_peer(peer_id, self.block_announce_protocol_name.clone());
					},
					RequestFailure::Network(OutboundFailure::ConnectionClosed) |
					RequestFailure::NotConnected => {
						self.network_service
							.disconnect_peer(peer_id, self.block_announce_protocol_name.clone());
					},
					RequestFailure::UnknownProtocol => {
						debug_assert!(false, "Block request protocol should always be known.");
					},
					RequestFailure::InvalidRequest => {
						debug_assert!(false, "Block request payload should always be valid.");
					},
					RequestFailure::Obsolete => {
						debug_assert!(
							false,
							"Can not receive `RequestFailure::Obsolete` after dropping the \
							response receiver.",
						);
					},
					RequestFailure::Network(OutboundFailure::Io(_)) => {
						self.network_service.report_peer(peer_id, rep::IO);
						self.network_service
							.disconnect_peer(peer_id, self.block_announce_protocol_name.clone());
					},
				}
			},
			Err(oneshot::Canceled) => {
				trace!(
					target: LOG_TARGET,
					"Request to peer {peer_id:?} failed due to oneshot being canceled.",
				);
				self.network_service
					.disconnect_peer(peer_id, self.block_announce_protocol_name.clone());
			},
		}
	}

	/// Returns the number of peers we're connected to and that are being queried.
	fn num_active_peers(&self) -> usize {
		self.pending_responses.len()
	}

	/// Get config for the block announcement protocol
	fn get_block_announce_proto_config<N: NetworkBackend<B, <B as BlockT>::Hash>>(
		protocol_id: ProtocolId,
		fork_id: Option<&str>,
		roles: Roles,
		best_number: NumberFor<B>,
		best_hash: B::Hash,
		genesis_hash: B::Hash,
		set_config: &SetConfig,
		metrics: NotificationMetrics,
		peer_store_handle: Arc<dyn PeerStoreProvider>,
	) -> (N::NotificationProtocolConfig, Box<dyn NotificationService>) {
		let block_announces_protocol = block_announces_protocol_name(genesis_hash, fork_id);

		N::notification_config(
			block_announces_protocol.into(),
			iter::once(block_announces_legacy_protocol_name(&protocol_id).into()).collect(),
			MAX_BLOCK_ANNOUNCE_SIZE,
			Some(NotificationHandshake::new(BlockAnnouncesHandshake::<B>::build(
				roles,
				best_number,
				best_hash,
				genesis_hash,
			))),
			set_config.clone(),
			metrics,
			peer_store_handle,
		)
	}

	/// Import blocks.
	fn import_blocks(&mut self, origin: BlockOrigin, blocks: Vec<IncomingBlock<B>>) {
		if let Some(metrics) = &self.metrics {
			metrics.import_queue_blocks_submitted.inc();
		}

		self.import_queue.import_blocks(origin, blocks);
	}

	/// Import justifications.
	fn import_justifications(
		&mut self,
		peer_id: PeerId,
		hash: B::Hash,
		number: NumberFor<B>,
		justifications: Justifications,
	) {
		if let Some(metrics) = &self.metrics {
			metrics.import_queue_justifications_submitted.inc();
		}

		self.import_queue.import_justifications(peer_id, hash, number, justifications);
	}
}

/// Update per-peer slot tracking for changes in the dynamic no-slot set.
/// Promotes newly added peers, demotes removed ones, ignoring static no-slot peers.
///
/// `peer_inbound_full(peer_id)` returns `true` if `peer_id` is inbound and full.
///  Returns `None` if the peer is not connected.
///
/// If removing a peer from no-slot would make `num_in_peers` exceed `max_in_peers`,
/// disconnect the peer instead and keep it in `connected_no_slot` until the async disconnect
/// handler will update `num_in_peers`..
/// Caller needs to update `dynamic_no_slot_peers` after calling this function.
fn apply_no_slot_set_inner(
	peer_inbound_full: impl Fn(&PeerId) -> Option<bool>,
	static_no_slot: &HashSet<PeerId>,
	old_dynamic_no_slot: &HashSet<PeerId>,
	new_dynamic_no_slot: &HashSet<PeerId>,
	connected_no_slot: &mut HashSet<PeerId>,
	num_in_peers: &mut usize,
	max_in_peers: usize,
	network_service: &service::network::NetworkServiceHandle,
	protocol: &ProtocolName,
) {
	// Skip static-set and disconnected peers and return the slot-affecting flag for the rest.
	let slot_impact = |peer_id: &PeerId| -> Option<bool> {
		if static_no_slot.contains(peer_id) {
			return None;
		}
		peer_inbound_full(peer_id)
	};

	let mut promoted = 0;
	let mut demoted = 0;
	let mut disconnected = 0;

	for peer_id in new_dynamic_no_slot.difference(old_dynamic_no_slot) {
		let Some(affects_slots) = slot_impact(peer_id) else { continue };
		// Defensive check, should never happen as we filter above.
		if !connected_no_slot.insert(*peer_id) {
			log::error!(
				target: LOG_TARGET,
				"{peer_id} promoted to no-slot but was already in connected_no_slot",
			);
			debug_assert!(false);
			continue;
		}
		if affects_slots {
			if let Some(n) = num_in_peers.checked_sub(1) {
				*num_in_peers = n;
			} else {
				log::error!(
					target: LOG_TARGET,
					"num_in_peers underflow promoting {peer_id} to no-slot",
				);
				debug_assert!(false);
			}
			promoted += 1;
		}
	}

	for peer_id in old_dynamic_no_slot.difference(new_dynamic_no_slot) {
		let Some(affects_slots) = slot_impact(peer_id) else { continue };
		if !connected_no_slot.contains(peer_id) {
			continue;
		}
		if affects_slots && *num_in_peers >= max_in_peers {
			log::debug!(
				target: LOG_TARGET,
				"Demoting {peer_id} would exceed max_in_peers ({max_in_peers}); disconnecting",
			);
			network_service.disconnect_peer(*peer_id, protocol.clone());
			disconnected += 1;
			continue;
		}
		connected_no_slot.remove(peer_id);
		if affects_slots {
			*num_in_peers += 1;
			demoted += 1;
		}
	}

	log::debug!(
		target: LOG_TARGET,
		"Dynamic no-slot peer set updated: {} peers: +{} in, -{} out, {} disconnected",
		new_dynamic_no_slot.len(),
		promoted,
		demoted,
		disconnected,
	);
}

#[cfg(test)]
mod tests {
	use super::*;

	fn fresh_peers<const N: usize>() -> [PeerId; N] {
		std::array::from_fn(|_| PeerId::random())
	}

	fn set_of<const N: usize>(peers: [PeerId; N]) -> HashSet<PeerId> {
		peers.into_iter().collect()
	}

	/// Run [`apply_no_slot_set`] with the given initial state. Uses `usize::MAX` for
	/// `max_in_peers` so demotion never trips the disconnect path. Returns the final
	/// `connected_no_slot` set and `num_in_peers`.
	#[track_caller]
	fn run_apply(
		connected: Vec<(PeerId, bool)>,
		static_no_slot: HashSet<PeerId>,
		old_dynamic: HashSet<PeerId>,
		new_dynamic: HashSet<PeerId>,
		initial_connected_no_slot: HashSet<PeerId>,
		initial_num_in_peers: usize,
	) -> (HashSet<PeerId>, usize) {
		let (connected_no_slot, num_in, disconnects) = run_apply_with_cap(
			connected,
			static_no_slot,
			old_dynamic,
			new_dynamic,
			initial_connected_no_slot,
			initial_num_in_peers,
			usize::MAX,
		);
		assert!(disconnects.is_empty(), "unexpected disconnects: {disconnects:?}");
		(connected_no_slot, num_in)
	}

	/// Variant of [`run_apply`] that exposes `max_in_peers` and the list of peers the
	/// function asked the network to disconnect (drained from the `NetworkServiceHandle`'s
	/// command channel).
	#[track_caller]
	fn run_apply_with_cap(
		connected: Vec<(PeerId, bool)>,
		static_no_slot: HashSet<PeerId>,
		old_dynamic: HashSet<PeerId>,
		new_dynamic: HashSet<PeerId>,
		initial_connected_no_slot: HashSet<PeerId>,
		initial_num_in_peers: usize,
		max_in_peers: usize,
	) -> (HashSet<PeerId>, usize, Vec<PeerId>) {
		use crate::service::network::{NetworkServiceHandle, ToServiceCommand as NetCmd};

		let peer_inbound_full: HashMap<PeerId, bool> = connected.into_iter().collect();
		let (tx, mut rx) = tracing_unbounded::<NetCmd>("test_apply_no_slot_set_disconnects", 100);
		let network_service = NetworkServiceHandle::new(tx);
		let protocol: ProtocolName = "/test/block-announces/1".into();
		let mut connected_no_slot = initial_connected_no_slot;
		let mut num_in_peers = initial_num_in_peers;
		apply_no_slot_set_inner(
			|peer_id| peer_inbound_full.get(peer_id).copied(),
			&static_no_slot,
			&old_dynamic,
			&new_dynamic,
			&mut connected_no_slot,
			&mut num_in_peers,
			max_in_peers,
			&network_service,
			&protocol,
		);
		drop(network_service);

		let mut disconnects = Vec::new();
		while let Ok(cmd) = rx.try_recv() {
			if let NetCmd::DisconnectPeer(peer, _) = cmd {
				disconnects.push(peer);
			}
		}
		(connected_no_slot, num_in_peers, disconnects)
	}

	#[test]
	fn apply_promotes_multiple_inbound_full_peers() {
		// `already` is in both old and new dynamic — it must stay in `connected_no_slot`
		// without releasing another slot.
		let [a, b, c, already] = fresh_peers();
		let (connected_no_slot, num_in) = run_apply(
			vec![(a, true), (b, true), (c, true), (already, true)],
			HashSet::new(),
			set_of([already]),
			set_of([a, b, c, already]),
			set_of([already]),
			10,
		);
		assert_eq!(connected_no_slot, set_of([a, b, c, already]));
		assert_eq!(num_in, 7);
	}

	#[test]
	fn apply_demotes_multiple_inbound_full_peers() {
		let [a, b, c, stays] = fresh_peers();
		let (connected_no_slot, num_in) = run_apply(
			vec![(a, true), (b, true), (c, true), (stays, true)],
			HashSet::new(),
			set_of([a, b, c, stays]),
			set_of([stays]),
			set_of([a, b, c, stays]),
			2,
		);
		assert_eq!(connected_no_slot, set_of([stays]));
		assert_eq!(num_in, 5);
	}

	#[test]
	fn apply_ignores_non_slot_consuming_peers() {
		// Outbound peers and inbound light peers both yield `affects_slots = false`. Either
		// kind transitioning must update `connected_no_slot` but not move `num_in_peers`.
		let [outbound, light] = fresh_peers();
		let (connected_no_slot, num_in) = run_apply(
			vec![(outbound, false), (light, false)],
			HashSet::new(),
			HashSet::new(),
			set_of([outbound, light]),
			HashSet::new(),
			5,
		);
		assert_eq!(connected_no_slot, set_of([outbound, light]));
		assert_eq!(num_in, 5);
	}

	#[test]
	fn apply_static_peers_stay_no_slot_when_removed_from_dynamic() {
		// `control` (dynamic-only) IS demoted, proving the wiring is live — the static peers
		// must NOT be demoted because the static set takes precedence over the dynamic one.
		let [s1, s2, control] = fresh_peers();
		let (connected_no_slot, num_in) = run_apply(
			vec![(s1, true), (s2, true), (control, true)],
			set_of([s1, s2]),
			set_of([s1, s2, control]),
			HashSet::new(),
			set_of([s1, s2, control]),
			2,
		);
		assert_eq!(connected_no_slot, set_of([s1, s2]));
		assert_eq!(num_in, 3);
	}

	#[test]
	fn apply_static_peers_added_to_dynamic_are_unchanged() {
		let [s1, s2] = fresh_peers();
		let (connected_no_slot, num_in) = run_apply(
			vec![(s1, true), (s2, true)],
			set_of([s1, s2]),
			HashSet::new(),
			set_of([s1, s2]),
			set_of([s1, s2]),
			4,
		);
		assert_eq!(connected_no_slot, set_of([s1, s2]));
		assert_eq!(num_in, 4);
	}

	#[test]
	fn apply_unconnected_peers_in_new_set_are_ignored() {
		// Unconnected peers go into `dynamic_no_slot_peers` (caller-installed) and take effect
		// on connect; they must not appear in `connected_no_slot` here.
		let [connected_a, connected_b] = fresh_peers();
		let [unconnected_a, unconnected_b] = fresh_peers();
		let (connected_no_slot, num_in) = run_apply(
			vec![(connected_a, true), (connected_b, true)],
			HashSet::new(),
			HashSet::new(),
			set_of([unconnected_a, unconnected_b]),
			HashSet::new(),
			3,
		);
		assert!(connected_no_slot.is_empty());
		assert_eq!(num_in, 3);
	}

	#[test]
	fn apply_idempotent_same_set() {
		let [in_full, out_full, light] = fresh_peers();
		let target = set_of([in_full, out_full, light]);
		let (connected_no_slot, num_in) = run_apply(
			vec![(in_full, true), (out_full, false), (light, false)],
			HashSet::new(),
			target.clone(),
			target.clone(),
			target.clone(),
			2,
		);
		assert_eq!(connected_no_slot, target);
		assert_eq!(num_in, 2);
	}

	#[test]
	fn apply_empty_set_clears_dynamic_only_peers() {
		let [in1, in2, out, light] = fresh_peers();
		let [static_peer] = fresh_peers();
		let old = set_of([in1, in2, out, light, static_peer]);
		let (connected_no_slot, num_in) = run_apply(
			vec![(in1, true), (in2, true), (out, false), (light, false), (static_peer, true)],
			set_of([static_peer]),
			old.clone(),
			HashSet::new(),
			old,
			0,
		);
		assert_eq!(connected_no_slot, set_of([static_peer]));
		assert_eq!(num_in, 2);
	}

	#[test]
	fn apply_mixed_promote_and_demote() {
		let [p1, p2] = fresh_peers();
		let [d1, d2] = fresh_peers();
		let (connected_no_slot, num_in) = run_apply(
			vec![(p1, true), (p2, true), (d1, true), (d2, true)],
			HashSet::new(),
			set_of([d1, d2]),
			set_of([p1, p2]),
			set_of([d1, d2]),
			5,
		);
		assert_eq!(connected_no_slot, set_of([p1, p2]));
		assert_eq!(num_in, 5);
	}

	#[test]
	fn apply_demote_at_capacity_disconnects_peer() {
		// Scenario: PeerX was promoted (freeing a slot), then a regular PeerY filled that slot,
		// bringing `num_in_peers` back to capacity. Now PeerX is demoted out of the dynamic set
		// — incrementing `num_in_peers` would push it strictly above `max_in_peers`. The peer
		// must be disconnected instead, and left in `connected_no_slot` so the async disconnect
		// handler is the sole updater of `num_in_peers`.
		let [px] = fresh_peers();
		let (connected_no_slot, num_in, disconnects) = run_apply_with_cap(
			vec![(px, true)],
			HashSet::new(),
			set_of([px]),
			HashSet::new(),
			set_of([px]),
			8,
			8,
		);
		assert_eq!(connected_no_slot, set_of([px]));
		assert_eq!(num_in, 8);
		assert_eq!(disconnects, vec![px]);
	}

	#[test]
	fn apply_demote_below_capacity_increments_normally() {
		// Same shape as the over-capacity test but with `num_in_peers < max_in_peers`: the
		// peer is regularly demoted and `num_in_peers` is incremented.
		let [px] = fresh_peers();
		let (connected_no_slot, num_in, disconnects) = run_apply_with_cap(
			vec![(px, true)],
			HashSet::new(),
			set_of([px]),
			HashSet::new(),
			set_of([px]),
			7,
			8,
		);
		assert!(connected_no_slot.is_empty());
		assert_eq!(num_in, 8);
		assert!(disconnects.is_empty());
	}
}
