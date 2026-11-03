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

//! Statement handling to plug on top of the network service.
//!
//! This crate implements gossip-based propagation of statements between nodes, layered on the
//! substrate notifications protocol. Two protocol versions are negotiated per peer: `statement/2`
//! (preferred) with `statement/1` as a fallback.
//!
//! ## Propagation
//!
//! - During major chain synchronization, statement gossip is paused so peers prioritize downloading
//!   blocks; it resumes automatically once the node is fully synced (peers are reconnected to
//!   recover statements missed while syncing).
//! - A propagation loop runs every second (`config::PROPAGATE_TIMEOUT`): it takes all statements
//!   added since the previous round and queues their hashes to a per-peer outbox. Each peer has at
//!   most one propagation chunk in flight at a time. When its send slot is free, statements are
//!   fetched from the store, filtered and encoded up to the maximum notification size
//!   (`config::MAX_STATEMENT_NOTIFICATION_SIZE`, ~1 MiB).
//! - Incoming statements are pushed onto a bounded validation queue
//!   (`config::MAX_PENDING_STATEMENTS`); if the queue is full, incoming statements are dropped.
//! - Peer reputation is adjusted based on statement quality (good, duplicate, invalid, flooding).
//!
//! ## Tracking received statements
//!
//! There is no per-peer record of delivered statements. A statement is propagated once, on the
//! tick after its import: the propagation pass drains the store's recent set, so no later pass
//! can pick it up again. The only duplicates worth preventing are statements sent back to the
//! peers they came from.
//!
//! While a statement waits for validation, the peers it came from are recorded in
//! `pending_statements_peers`. On import they move to `recently_received_statements`, and a
//! peer that resends the statement before its tick is added there too. Propagation and
//! initial-sync chunks both skip the recorded peers, and the propagation pass clears
//! `recently_received_statements` when done.
//!
//! ## Initial sync
//!
//! A peer joining (or changing its topic affinity) receives the store's existing statements
//! through a cursor over the store's admission journal: scheduling captures the journal's
//! watermark, and bursts walk the admissions below it chunk by chunk, advancing the cursor as
//! each send is confirmed — a failed chunk is resent from the same position. The watermark also
//! splits delivery ownership between the two paths: admissions below the peer's highest
//! watermark are the sync cursor's job, and propagation skips them, so within one sync a
//! statement reaches the peer through at most one path. Duplicates stay possible at the edges —
//! an affinity-change re-sync restarts the cursor from zero and redelivers earlier admissions,
//! and a chunk whose send timed out may have arrived regardless, so its resend repeats it.
//!
//! ## Send scheduling
//!
//! Every peer has one send slot, shared by propagation and initial sync, so at most one chunk per
//! connection is in flight. Each chunk carries a fresh id, so the result of a send left over from
//! a previous connection cannot free the current one's slot. A completed chunk frees the slot at
//! once and the next propagation chunk follows, a failed send included: a failed propagation
//! chunk is not retried, but the rest of the backlog keeps draining, while a failed initial-sync
//! chunk is resent from the sync's cursor.
//!
//! Propagation queues hashes in a per-peer outbox and fetches, filters and encodes them only when
//! the slot is free, so a slow peer holds one encoded chunk rather than its whole backlog. An
//! outbox past `config::MAX_PROPAGATION_OUTBOX_LEN` drops its oldest hashes, since the freshest
//! statements are the ones still worth delivering. Dropped hashes are counted in
//! `undelivered_statements`.
//!
//! In-flight bytes of both kinds are held against the shared
//! `config::MAX_SEND_IN_FLIGHT_BYTES` budget. A peer that finds the budget full, or whose store
//! fetch fails, is parked once and refilled in parking order as completed sends free bytes and on
//! propagation ticks. While initial syncs are pending, propagation parks
//! `config::INITIAL_SYNC_RESERVED_BYTES` early: refills reclaim freed bytes synchronously,
//! while the timer-driven sync bursts would otherwise always find the budget full.
//!
//! ## Topic affinity and light nodes
//!
//! The `statement/2` protocol lets a peer advertise which topics it cares about as a bloom filter
//! ("topic affinity"). Once a peer has an active affinity filter, only matching statements are
//! forwarded to it; when its affinity changes, newly relevant statements are re-sent. Affinity
//! advertisements are rate-limited. See the `affinity` module.
//!
//! Light-client peers on `statement/2` must advertise an affinity before receiving any statements:
//! a light V2 peer pulls only the topics it cares about instead of the full feed, and is synced
//! those statements in an initial burst on connect. Full nodes receive all statements unless they
//! opt into an affinity.
//!
//! ## Usage
//!
//! - Use [`StatementHandlerPrototype::new`] to create a prototype.
//! - Pass the `NonDefaultSetConfig` returned from [`StatementHandlerPrototype::new`] to the network
//!   configuration as an extra peers set.
//! - Use [`StatementHandlerPrototype::build`] then [`StatementHandler::run`] to obtain a `Future`
//!   that processes statements.

mod affinity;

use crate::config::*;

use affinity::AffinityFilter;
use codec::{Compact, Decode, Encode, MaxEncodedLen};
use futures::{
	channel::oneshot,
	future::{pending, FusedFuture},
	prelude::*,
	stream::FuturesUnordered,
};
use governor::{
	clock::{Clock, DefaultClock},
	middleware::NoOpMiddleware,
	state::{InMemoryState, NotKeyed},
	Quota, RateLimiter,
};
use prometheus_endpoint::{
	exponential_buckets, register, Counter, CounterVec, Gauge, GaugeVec, Histogram, HistogramOpts,
	HistogramVec, Opts, PrometheusError, Registry, U64,
};
use rand::seq::IteratorRandom;
use sc_network::{
	config::{NonReservedPeerMode, SetConfig},
	error, multiaddr,
	peer_store::PeerStoreProvider,
	service::{
		traits::{NotificationEvent, NotificationService, ValidationResult},
		NotificationMetrics,
	},
	types::ProtocolName,
	utils::interval,
	NetworkBackend, NetworkEventStream, NetworkPeers,
};
use sc_network_sync::{SyncEvent, SyncEventStream};
use sc_network_types::PeerId;
use sp_runtime::{
	traits::{Block as BlockT, ConstU32},
	BoundedVec,
};
use sp_statement_store::{
	AdmittedBatch, FilterDecision, Hash, Statement, StatementSource, StatementStore, SubmitResult,
};
use std::{
	collections::{hash_map::Entry, HashMap, HashSet, VecDeque},
	fmt, iter,
	num::NonZeroU32,
	pin::Pin,
	sync::Arc,
	time::Instant,
};
use tokio::time::timeout;
pub mod config;

/// A set of statements.
pub type Statements = Vec<Statement>;

type StatementBatch = BoundedVec<Statement, ConstU32<{ MAX_STATEMENTS_PER_NOTIFICATION as u32 }>>;

/// The protocol version that was negotiated with a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerProtocolVersion {
	/// V1: messages are encoded as `Vec<Statement>` (the legacy format).
	V1,
	/// V2: messages are encoded as `StatementMessage` enum (supports topic affinity).
	V2,
}

impl PeerProtocolVersion {
	/// Returns the encoding envelope overhead for this protocol version.
	fn envelope_overhead(&self) -> usize {
		match self {
			PeerProtocolVersion::V1 => V1_ENVELOPE_OVERHEAD,
			PeerProtocolVersion::V2 => V2_ENVELOPE_OVERHEAD,
		}
	}
}

#[derive(Debug, Encode, Decode)]
enum StatementMessage {
	#[codec(index = 0)]
	Statements(StatementBatch),
	/// Bloom filter bytes representing the topics this peer is interested in.
	#[codec(index = 1)]
	ExplicitTopicAffinity(AffinityFilter),
}

/// Codec variant index for `StatementMessage::Statements`, kept in sync with `#[codec(index)]`.
const STATEMENTS_VARIANT_INDEX: u8 = 0;

impl StatementMessage {
	/// Encode a slice of statement references as a `StatementMessage::Statements`
	/// without cloning the statements.
	fn encode_statement_refs(statements: &[&Statement]) -> Vec<u8> {
		let mut out = Vec::new();
		STATEMENTS_VARIANT_INDEX.encode_to(&mut out);
		statements.encode_to(&mut out);
		out
	}
}

/// Future resolving to statement import result.
pub type StatementImportFuture = oneshot::Receiver<SubmitResult>;

mod rep {
	use sc_network::ReputationChange as Rep;
	/// Reputation change when a peer sends us any statement.
	///
	/// This forces node to verify it, thus the negative value here. Once statement is verified,
	/// reputation change should be refunded with `ANY_STATEMENT_REFUND`
	pub const ANY_STATEMENT: Rep = Rep::new(-(1 << 4), "Any statement");
	/// Reputation change when a peer sends us any statement that is not invalid.
	pub const ANY_STATEMENT_REFUND: Rep = Rep::new(1 << 4, "Any statement (refund)");
	/// Reputation change when a peer sends us an statement that we didn't know about.
	pub const GOOD_STATEMENT: Rep = Rep::new(1 << 8, "Good statement");
	/// Reputation change when a peer sends us an invalid statement.
	pub const INVALID_STATEMENT: Rep = Rep::new(-(1 << 12), "Invalid statement");
	/// Reputation change when a peer sends us a duplicate statement.
	pub const DUPLICATE_STATEMENT: Rep = Rep::new(-(1 << 7), "Duplicate statement");
	/// Reputation change when a peer floods us with statements.
	pub const STATEMENT_FLOODING: Rep = Rep::new_fatal("Statement flooding");
	/// Reputation change when a peer sends us a message we can't decode.
	pub const BAD_MESSAGE: Rep = Rep::new(-(1 << 12), "Bad statement message");
}

const LOG_TARGET: &str = "statement-gossip";
/// V2 statement protocol suffix, work in progress protocol with topic affinity and other
/// improvements, may have breaking changes before stabilization.
const STATEMENT_PROTOCOL_V2: &str = "statement/2";
/// V1 statement protocol suffix, current stable protocol, no breaking changes will be made to it.
const STATEMENT_PROTOCOL_V1: &str = "statement/1";
/// Maximum time we wait for sending a notification to a peer.
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Interval for sending statement batches during initial sync to new peers.
const INITIAL_SYNC_BURST_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// Maximum admission-journal entries one initial-sync fetch visits, so that a peer whose
/// affinity matches nothing cannot make one burst walk the whole journal in the event loop.
const INITIAL_SYNC_SCAN_LIMIT: usize = 4096;
/// Interval for processing pending topic affinity changes from peers.
const PENDING_AFFINITIES_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
/// Delay before re-adding a peer to the reserved set after a forced disconnect for sync recovery.
const SYNC_RECOVERY_READD_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

struct Metrics {
	propagated_statements: Counter<U64>,
	known_statements_received: Counter<U64>,
	skipped_oversized_statements: Counter<U64>,
	propagated_statements_chunks: HistogramVec,
	pending_statements: Gauge<U64>,
	ignored_statements: Counter<U64>,
	peers_connected: GaugeVec<U64>,
	statements_received: Counter<U64>,
	bytes_sent_total: Counter<U64>,
	bytes_received_total: Counter<U64>,
	sent_latency_seconds: Histogram,
	initial_sync_statements_sent: Counter<U64>,
	initial_sync_bursts_total: Counter<U64>,
	initial_sync_in_flight_bytes: Gauge<U64>,
	propagation_in_flight_bytes: Gauge<U64>,
	initial_sync_peers_active: Gauge<U64>,
	initial_sync_duration_seconds: HistogramVec,
	statement_flooding_detected: Counter<U64>,
	send_failures: CounterVec<U64>,
	undelivered_statements: CounterVec<U64>,
}

mod send_failure {
	/// The network layer rejected the send.
	pub const NETWORK: &str = "network";
	/// The send did not complete within `SEND_TIMEOUT`.
	pub const TIMEOUT: &str = "timeout";
	/// The chunk was never handed to the network because the peer had no message sink.
	pub const NO_SINK: &str = "no_sink";
	/// The peer's propagation outbox overflowed and the oldest queued hashes were dropped.
	pub const OUTBOX_FULL: &str = "outbox_full";
}

mod sync_outcome {
	/// A burst found the backlog drained, so every statement reached the peer.
	pub const COMPLETED: &str = "completed";
	/// The sync ended before its backlog was drained.
	pub const ABANDONED: &str = "abandoned";
}

impl Metrics {
	fn register(r: &Registry) -> Result<Self, PrometheusError> {
		let peers_connected = register(
			GaugeVec::new(
				Opts::new(
					"substrate_sync_statement_peers_connected",
					"Number of peers connected using the statement protocol by kind",
				),
				&["kind"],
			)?,
			r,
		)?;
		peers_connected.with_label_values(&["full"]).set(0);
		peers_connected.with_label_values(&["light"]).set(0);

		Ok(Self {
			propagated_statements: register(
				Counter::new(
					"substrate_sync_propagated_statements",
					"Total statements propagated to peers, counted once per recipient (a statement sent to N peers increments by N)",
				)?,
				r,
			)?,
			known_statements_received: register(
				Counter::new(
					"substrate_sync_known_statement_received",
					"Number of statements received via gossiping that were already in the statement store",
				)?,
				r,
			)?,
			skipped_oversized_statements: register(
				Counter::new(
					"substrate_sync_skipped_oversized_statements",
					"Number of oversized statements that were skipped to be gossiped",
				)?,
				r,
			)?,
			propagated_statements_chunks: register(
				HistogramVec::new(
					HistogramOpts::new(
						"substrate_sync_propagated_statements_chunks",
						"Distribution of chunk sizes when sending statements, by send path. Initial \
						 sync fills every chunk to the size limit, propagation does not",
					)
					.buckets(exponential_buckets(1.0, 2.0, 14)?),
					&["kind"],
				)?,
				r,
			)?,
			pending_statements: register(
				Gauge::new(
					"substrate_sync_pending_statement_validations",
					"Number of pending statement validations, sampled once per propagation tick",
				)?,
				r,
			)?,
			ignored_statements: register(
				Counter::new(
					"substrate_sync_ignored_statements",
					"Number of statements ignored due to exceeding MAX_PENDING_STATEMENTS limit",
				)?,
				r,
			)?,
			peers_connected,
			statements_received: register(
				Counter::new(
					"substrate_sync_statements_received",
					"Total number of statements received from peers",
				)?,
				r,
			)?,
			bytes_sent_total: register(
				Counter::new(
					"substrate_sync_statement_bytes_sent_total",
					"Total bytes sent for statement protocol messages",
				)?,
				r,
			)?,
			bytes_received_total: register(
				Counter::new(
					"substrate_sync_statement_bytes_received_total",
					"Total bytes received for statement protocol messages (includes bytes from notifications that are later discarded — e.g. while major-syncing)",
				)?,
				r,
			)?,
			sent_latency_seconds: register(
				Histogram::with_opts(
					HistogramOpts::new(
						"substrate_sync_statement_sent_latency_seconds",
						"Time to send statement messages to peers",
					)
					// Buckets from 1μs to ~1s covering microsecond to millisecond range.
					.buckets(vec![0.000_001, 0.000_01, 0.000_1, 0.001, 0.01, 0.1, 1.0]),
				)?,
				r,
			)?,
			initial_sync_statements_sent: register(
				Counter::new(
					"substrate_sync_initial_sync_statements_sent",
					"Total statements sent during initial sync bursts to newly connected peers",
				)?,
				r,
			)?,
			initial_sync_bursts_total: register(
				Counter::new(
					"substrate_sync_initial_sync_bursts_total",
					"Total initial-sync burst rounds attempted (includes rounds that return early with no hashes left)",
				)?,
				r,
			)?,
			initial_sync_in_flight_bytes: register(
				Gauge::new(
					"substrate_sync_initial_sync_in_flight_bytes",
					"Encoded bytes of initial-sync chunks currently queued for sending",
				)?,
				r,
			)?,
			propagation_in_flight_bytes: register(
				Gauge::new(
					"substrate_sync_propagation_in_flight_bytes",
					"Encoded bytes of propagation chunks currently queued for sending",
				)?,
				r,
			)?,
			initial_sync_peers_active: register(
				Gauge::new(
					"substrate_sync_initial_sync_peers_active",
					"Number of peers currently being synced via initial sync",
				)?,
				r,
			)?,
			initial_sync_duration_seconds: register(
				HistogramVec::new(
					HistogramOpts::new(
						"substrate_sync_initial_sync_duration_seconds",
						"Per-peer duration of initial sync, by outcome: completed (backlog drained) or abandoned (ended with statements still queued)",
					)
					.buckets(vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0]),
					&["outcome"],
				)?,
				r,
			)?,
			statement_flooding_detected: register(
				Counter::new(
					"substrate_sync_statement_flooding_detected",
					"Number of peers disconnected for exceeding statement rate limits",
				)?,
				r,
			)?,
			send_failures: register(
				CounterVec::new(
					Opts::new(
						"substrate_sync_statement_send_failures_total",
						"Total statement sends that never reached the peer, by reason",
					),
					&["reason"],
				)?,
				r,
			)?,
			undelivered_statements: register(
				CounterVec::new(
					Opts::new(
						"substrate_sync_statement_undelivered_total",
						"Total statements whose delivery was abandoned, so the peer never received them, by reason",
					),
					&["reason"],
				)?,
				r,
			)?,
		})
	}
}

/// Prototype for a [`StatementHandler`].
pub struct StatementHandlerPrototype {
	protocol_name: ProtocolName,
	notification_service: Box<dyn NotificationService>,
}

impl StatementHandlerPrototype {
	/// Create a new instance.
	pub fn new<
		Hash: AsRef<[u8]>,
		Block: BlockT,
		Net: NetworkBackend<Block, <Block as BlockT>::Hash>,
	>(
		genesis_hash: Hash,
		fork_id: Option<&str>,
		metrics: NotificationMetrics,
		peer_store_handle: Arc<dyn PeerStoreProvider>,
	) -> (Self, Net::NotificationProtocolConfig) {
		let genesis_hash = genesis_hash.as_ref();
		let hex = array_bytes::bytes2hex("", genesis_hash);
		let (protocol_name, fallback_name) = if let Some(fork_id) = fork_id {
			(
				format!("/{hex}/{fork_id}/{STATEMENT_PROTOCOL_V2}"),
				format!("/{hex}/{fork_id}/{STATEMENT_PROTOCOL_V1}"),
			)
		} else {
			(format!("/{hex}/{STATEMENT_PROTOCOL_V2}"), format!("/{hex}/{STATEMENT_PROTOCOL_V1}"))
		};
		let (config, notification_service) = Net::notification_config(
			protocol_name.clone().into(),
			vec![fallback_name.into()],
			MAX_STATEMENT_NOTIFICATION_SIZE,
			None,
			SetConfig {
				in_peers: 0,
				out_peers: 0,
				reserved_nodes: Vec::new(),
				non_reserved_mode: NonReservedPeerMode::Deny,
			},
			metrics,
			peer_store_handle,
		);

		(Self { protocol_name: protocol_name.into(), notification_service }, config)
	}

	/// Turns the prototype into the actual handler.
	///
	/// Important: the statements handler is initially disabled and doesn't gossip statements.
	/// Gossiping is enabled when major syncing is done.
	pub fn build<
		N: NetworkPeers + NetworkEventStream,
		S: SyncEventStream + sp_consensus::SyncOracle,
	>(
		self,
		network: N,
		sync: S,
		statement_store: Arc<dyn StatementStore>,
		metrics_registry: Option<&Registry>,
		executor: impl Fn(Pin<Box<dyn Future<Output = ()> + Send>>) + Send,
		mut num_submission_workers: usize,
		statements_per_second: u32,
	) -> error::Result<StatementHandler<N, S>> {
		let sync_event_stream = sync.event_stream("statement-handler-sync");
		// Still bounded via the `MAX_PENDING_STATEMENTS` check in `on_statements`.
		let (queue_sender, queue_receiver) = async_channel::unbounded();

		if num_submission_workers == 0 {
			log::warn!(
				target: LOG_TARGET,
				"num_submission_workers is 0, defaulting to 1"
			);
			num_submission_workers = 1;
		}

		let statements_per_second = match NonZeroU32::new(statements_per_second) {
			Some(rate) => rate,
			None => {
				log::warn!(
					target: LOG_TARGET,
					"statements_per_second is 0, defaulting to {}",
					DEFAULT_STATEMENTS_PER_SECOND
				);
				NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND)
					.expect("DEFAULT_STATEMENTS_PER_SECOND is nonzero")
			},
		};

		let metrics =
			if let Some(r) = metrics_registry { Some(Metrics::register(r)?) } else { None };

		for _ in 0..num_submission_workers {
			let store = statement_store.clone();
			let mut queue_receiver = queue_receiver.clone();
			executor(
				async move {
					loop {
						let task: Option<(Statement, oneshot::Sender<SubmitResult>)> =
							queue_receiver.next().await;
						match task {
							None => return,
							Some((statement, completion)) => {
								let result = store.submit(statement, StatementSource::Network);
								if completion.send(result).is_err() {
									log::debug!(
										target: LOG_TARGET,
										"Error sending validation completion"
									);
								}
							},
						}
					}
				}
				.boxed(),
			);
		}

		let handler = StatementHandler {
			protocol_name: self.protocol_name,
			notification_service: self.notification_service,
			propagate_timeout: (Box::pin(interval(PROPAGATE_TIMEOUT))
				as Pin<Box<dyn Stream<Item = ()> + Send>>)
				.fuse(),
			pending_statements: FuturesUnordered::new(),
			pending_statements_peers: HashMap::new(),
			recently_received_statements: HashMap::new(),
			network,
			sync,
			sync_event_stream: sync_event_stream.fuse(),
			peers: HashMap::new(),
			statement_store,
			queue_sender,
			statements_per_second,
			metrics,
			initial_sync_timeout: Box::pin(tokio::time::sleep(INITIAL_SYNC_BURST_INTERVAL).fuse()),
			pending_affinities_timeout: Box::pin(
				tokio::time::sleep(PENDING_AFFINITIES_INTERVAL).fuse(),
			),
			pending_initial_syncs: HashMap::new(),
			initial_sync_peer_queue: VecDeque::new(),
			next_initial_sync_id: 0,
			initial_sync_in_flight_bytes: 0,
			propagation_outboxes: HashMap::new(),
			in_flight_chunks: HashMap::new(),
			next_chunk_id: 0,
			propagation_in_flight_bytes: 0,
			parked_propagations: VecDeque::new(),
			pending_sends: FuturesUnordered::new(),
			deferred_peers: HashSet::new(),
			dropped_statements_during_sync: false,
			sync_recovery_peer: None,
			sync_recovery_readd_timeout: Box::pin(pending().fuse()),
		};

		Ok(handler)
	}
}

/// Handler for statements. Call [`StatementHandler::run`] to start the processing.
pub struct StatementHandler<
	N: NetworkPeers + NetworkEventStream,
	S: SyncEventStream + sp_consensus::SyncOracle,
> {
	protocol_name: ProtocolName,
	/// Interval at which we call `propagate_statements`.
	propagate_timeout: stream::Fuse<Pin<Box<dyn Stream<Item = ()> + Send>>>,
	/// Pending statements verification tasks.
	pending_statements:
		FuturesUnordered<Pin<Box<dyn Future<Output = (Hash, Option<SubmitResult>)> + Send>>>,
	/// As multiple peers can send us the same statement, we group
	/// these peers using the statement hash while the statement is
	/// imported. This prevents that we import the same statement
	/// multiple times concurrently.
	pending_statements_peers: HashMap<Hash, HashSet<PeerId>>,
	/// Statements received from peers and imported since the last propagation
	/// pass, each with the peers that sent it. Propagation skips those peers,
	/// so a statement never returns to a peer it came from. Cleared after each
	/// pass.
	recently_received_statements: HashMap<Hash, HashSet<PeerId>>,
	/// Network service to use to send messages and manage peers.
	network: N,
	/// Syncing service.
	sync: S,
	/// Receiver for syncing-related events.
	sync_event_stream: stream::Fuse<Pin<Box<dyn Stream<Item = SyncEvent> + Send>>>,
	/// Notification service.
	notification_service: Box<dyn NotificationService>,
	// All connected peers
	peers: HashMap<PeerId, Peer>,
	statement_store: Arc<dyn StatementStore>,
	queue_sender: async_channel::Sender<(Statement, oneshot::Sender<SubmitResult>)>,
	/// Maximum statements per second per peer.
	statements_per_second: NonZeroU32,
	/// Prometheus metrics.
	metrics: Option<Metrics>,
	/// Timeout for sending next statement batch during initial sync.
	initial_sync_timeout: Pin<Box<dyn FusedFuture<Output = ()> + Send>>,
	/// Timeout for processing pending topic affinity changes.
	pending_affinities_timeout: Pin<Box<dyn FusedFuture<Output = ()> + Send>>,
	/// Pending initial syncs per peer.
	pending_initial_syncs: HashMap<PeerId, PendingInitialSync>,
	/// Queue for round-robin processing of initial syncs.
	initial_sync_peer_queue: VecDeque<PeerId>,
	/// Next value to hand out as [`PendingInitialSync::sync_id`].
	next_initial_sync_id: u64,
	/// Encoded bytes of initial-sync chunks in `pending_sends`, held against the shared
	/// [`MAX_SEND_IN_FLIGHT_BYTES`] budget.
	initial_sync_in_flight_bytes: u64,
	/// Statement hashes queued for propagation to each peer, drained from the front as chunks
	/// are fetched, whether the fetch yields a send or not.
	propagation_outboxes: HashMap<PeerId, VecDeque<Hash>>,
	/// Id of the chunk in flight, per peer — the peer's single send slot, shared by
	/// propagation and initial sync.
	in_flight_chunks: HashMap<PeerId, u64>,
	/// Next value to hand out as [`PendingSendResult::chunk_id`].
	next_chunk_id: u64,
	/// Encoded bytes of propagation chunks in `pending_sends`, held against the shared
	/// [`MAX_SEND_IN_FLIGHT_BYTES`] budget.
	propagation_in_flight_bytes: u64,
	/// Peers whose propagation chunk was deferred because the shared byte budget was
	/// exhausted or the store fetch failed, refilled in parking order as bytes free up
	/// and on propagation ticks. A peer parks at most once.
	parked_propagations: VecDeque<PeerId>,
	/// Pending propagation sends, polled by the main event loop.
	pending_sends: PendingSends,
	/// Tracks peers that connected while major sync was active and adds them to the reserved set
	/// once sync ends
	deferred_peers: HashSet<PeerId>,
	/// Set to `true` when an incoming statement is dropped because `is_major_syncing()` is true
	dropped_statements_during_sync: bool,
	/// Peer scheduled for forced disconnect+reconnect to recover statements missed during sync
	sync_recovery_peer: Option<PeerId>,
	/// Fires when the `sync_recovery_peer` re-add delay has elapsed
	sync_recovery_readd_timeout: Pin<Box<dyn FusedFuture<Output = ()> + Send>>,
}

/// A token bucket, measured against whatever clock it was built with.
trait TokenBucket: fmt::Debug + Send + Sync {
	/// Whether admitting `n` more cells would exceed the quota.
	fn would_exceed(&self, n: NonZeroU32) -> bool;
}

impl<C> TokenBucket for RateLimiter<NotKeyed, InMemoryState, C, NoOpMiddleware<C::Instant>>
where
	C: Clock + fmt::Debug + Send + Sync,
	C::Instant: fmt::Debug + Send + Sync,
{
	fn would_exceed(&self, n: NonZeroU32) -> bool {
		!matches!(self.check_n(n), Ok(Ok(())))
	}
}

/// Per-peer rate limiter using a token bucket algorithm.
///
/// The token bucket allows short bursts up to the per-second limit while enforcing
/// the average rate over time.
#[derive(Debug)]
struct PeerRateLimiter {
	bucket: Box<dyn TokenBucket>,
}

impl PeerRateLimiter {
	fn new(statements_per_second: NonZeroU32, burst: NonZeroU32) -> Self {
		Self::with_clock(statements_per_second, burst, &DefaultClock::default())
	}

	/// The same quota, measured against `clock`.
	fn with_clock<C>(statements_per_second: NonZeroU32, burst: NonZeroU32, clock: &C) -> Self
	where
		C: Clock + fmt::Debug + Send + Sync + 'static,
		C::Instant: fmt::Debug + Send + Sync,
	{
		let quota = Quota::per_second(statements_per_second).allow_burst(burst);
		Self { bucket: Box::new(RateLimiter::direct_with_clock(quota, clock)) }
	}

	/// Check if receiving `count` statements would exceed the rate limit.
	fn is_flooding(&self, count: usize) -> bool {
		if count > u32::MAX as usize {
			return true;
		}

		let Some(n) = NonZeroU32::new(count as u32) else {
			return false;
		};
		self.bucket.would_exceed(n)
	}
}

/// Peer information
#[cfg_attr(not(any(test, feature = "test-helpers")), doc(hidden))]
#[derive(Debug)]
pub struct Peer {
	/// Rate limiter for statement flooding protection.
	rate_limiter: PeerRateLimiter,
	/// Protocol version negotiated with this peer.
	protocol_version: PeerProtocolVersion,
	/// Topic affinity filter received from a v2 peer.
	/// When set, only statements matching this filter should be propagated to the peer.
	topic_affinity: Option<AffinityFilter>,
	/// Whether this peer is a light client.
	/// Light clients on V2 must set topic affinity before receiving statements.
	is_light: bool,
	/// A pending topic affinity filter waiting to be scheduled for initial sync.
	/// Set when a new `ExplicitTopicAffinity` arrives; consumed by the main loop
	/// once any in-progress initial sync for this peer completes.
	pending_topic_affinity: Option<AffinityFilter>,
	/// One past the newest admission covered by the peer's initial syncs.
	sync_watermark: u64,
}

/// Tracks pending initial sync state for a peer as a cursor over the store's admission
/// journal (statements fetched on-demand).
struct PendingInitialSync {
	/// Admission sequence number the next burst resumes fetching from.
	cursor: u64,
	/// One past the newest admission covered by this sync.
	watermark: u64,
	started_at: Instant,
	/// Identifies this scheduling, so that a chunk still in flight from a previous one can be told
	/// apart once its result arrives.
	sync_id: u64,
}

enum SendOutcome {
	/// The notification was accepted by the network layer.
	Sent,
	/// The network layer rejected the send.
	NetworkError(error::Error),
	/// The send did not complete within `SEND_TIMEOUT`.
	TimedOut,
}

enum SendKind {
	Propagation,
	InitialSync {
		sync_id: u64,
		/// Cursor position the pending sync advances to once this chunk's send is confirmed.
		next_cursor: u64,
	},
}

impl SendKind {
	fn label(&self) -> &'static str {
		match self {
			Self::Propagation => "propagation",
			Self::InitialSync { .. } => "initial_sync",
		}
	}
}

/// Result of an asynchronous send.
struct PendingSendResult {
	peer: PeerId,
	statement_count: usize,
	bytes_sent: u64,
	result: SendOutcome,
	kind: SendKind,
	/// Id of the sent chunk, propagation or initial-sync. The result is stale if the id
	/// doesn't match the peer's send slot in `in_flight_chunks`.
	chunk_id: u64,
}

/// Type alias for the pending sends future collection, this is a list of in-flight sends to peers.
type PendingSends =
	FuturesUnordered<Pin<Box<dyn Future<Output = PendingSendResult> + Send + 'static>>>;

/// Encoding overhead for V1: just the `Compact<u32>` vec length prefix (max 5 bytes).
const V1_ENVELOPE_OVERHEAD: usize = 5;

/// Encoding overhead for V2: 1 byte enum discriminant + `Compact<u32>` vec length prefix.
const V2_ENVELOPE_OVERHEAD: usize = 1 + V1_ENVELOPE_OVERHEAD;

/// Returns the maximum payload size for statement notifications given the
/// protocol envelope overhead.
fn max_statement_payload_size(envelope_overhead: usize) -> usize {
	debug_assert_eq!(
		V1_ENVELOPE_OVERHEAD,
		Compact::<u32>::max_encoded_len(),
		"V1_ENVELOPE_OVERHEAD must equal Compact::<u32>::max_encoded_len()"
	);
	MAX_STATEMENT_NOTIFICATION_SIZE as usize - envelope_overhead
}

fn unix_timestamp_secs() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default()
		.as_secs()
}

/// Fetch the next chunk of statements admitted between `cursor` and `watermark`, filtering
/// in the `admitted_statements` callback so non-matching statements are never cloned into
/// the batch.
///
/// Returns the batch and the accumulated encoded size. A size above `max_size` signals a
/// lone oversized statement: it is taken and the cursor sits past it, so the caller must
/// drop the chunk and move on.
fn fetch_admitted_chunk(
	store: &dyn StatementStore,
	recently_received_statements: &HashMap<Hash, HashSet<PeerId>>,
	pending_statements_peers: &HashMap<Hash, HashSet<PeerId>>,
	who: &PeerId,
	peer_data: &Peer,
	cursor: u64,
	watermark: u64,
	max_size: usize,
) -> sp_statement_store::Result<(AdmittedBatch, usize)> {
	let now = unix_timestamp_secs();
	let mut accumulated_size = 0;
	let batch = store.admitted_statements(
		cursor,
		watermark,
		INITIAL_SYNC_SCAN_LIMIT,
		&mut |hash, encoded, stmt| {
			if stmt.is_expired(now) {
				return FilterDecision::Skip;
			}
			if peer_data.topic_affinity.as_ref().is_some_and(|a| !a.matches_statement(stmt)) {
				return FilterDecision::Skip;
			}
			// The peer supplied this statement, do not send it back.
			if has_received_from(recently_received_statements, pending_statements_peers, hash, who)
			{
				return FilterDecision::Skip;
			}
			if accumulated_size > 0 && accumulated_size + encoded.len() > max_size {
				return FilterDecision::Abort;
			}
			accumulated_size += encoded.len();
			FilterDecision::Take
		},
	)?;
	Ok((batch, accumulated_size))
}

/// Fetch the next chunk of statements for a peer from `hashes`, filtering in the
/// `statements_by_hashes` callback so non-matching statements are never materialized.
fn fetch_statement_chunk(
	store: &dyn StatementStore,
	recently_received_statements: &HashMap<Hash, HashSet<PeerId>>,
	pending_statements_peers: &HashMap<Hash, HashSet<PeerId>>,
	who: &PeerId,
	peer_data: &Peer,
	hashes: &[Hash],
	max_size: usize,
) -> sp_statement_store::Result<(Vec<(Hash, Statement)>, usize, usize)> {
	let now = unix_timestamp_secs();
	let mut accumulated_size = 0;
	let (statements, processed) =
		store.statements_by_hashes(hashes, &mut |hash, encoded, stmt| {
			if stmt.is_expired(now) {
				return FilterDecision::Skip;
			}
			if peer_data.topic_affinity.as_ref().is_some_and(|a| !a.matches_statement(stmt)) {
				return FilterDecision::Skip;
			}
			// The peer supplied this statement, do not send it back.
			if has_received_from(recently_received_statements, pending_statements_peers, hash, who)
			{
				return FilterDecision::Skip;
			}
			if accumulated_size > 0 && accumulated_size + encoded.len() > max_size {
				return FilterDecision::Abort;
			}
			accumulated_size += encoded.len();
			FilterDecision::Take
		})?;
	Ok((statements, processed, accumulated_size))
}

async fn send_with_timeout<F>(send: F) -> SendOutcome
where
	F: Future<Output = Result<(), error::Error>>,
{
	match timeout(SEND_TIMEOUT, send).await {
		Ok(Ok(())) => SendOutcome::Sent,
		Ok(Err(error)) => SendOutcome::NetworkError(error),
		Err(_elapsed) => SendOutcome::TimedOut,
	}
}

/// Whether the peer sent us the statement, directly or while it was queued for
/// validation. `pending_statements_peers` covers the race where a statement is
/// drained for propagation while the peers that sent it still sit there.
fn has_received_from(
	recently_received_statements: &HashMap<Hash, HashSet<PeerId>>,
	pending_statements_peers: &HashMap<Hash, HashSet<PeerId>>,
	hash: &Hash,
	who: &PeerId,
) -> bool {
	recently_received_statements.get(hash).is_some_and(|peers| peers.contains(who)) ||
		pending_statements_peers.get(hash).is_some_and(|peers| peers.contains(who))
}

impl Peer {
	/// Create a new peer for testing/benchmarking purposes.
	#[cfg(any(test, feature = "test-helpers"))]
	pub fn new_for_testing(statements_per_second: NonZeroU32, burst: NonZeroU32) -> Self {
		Self {
			rate_limiter: PeerRateLimiter::new(statements_per_second, burst),
			protocol_version: PeerProtocolVersion::V1,
			topic_affinity: None,
			is_light: false,
			pending_topic_affinity: None,
			sync_watermark: 0,
		}
	}

	/// Whether this peer is ready to receive statements.
	///
	/// Light V2 peers must set their topic affinity before receiving any statements.
	fn can_receive(&self) -> bool {
		!(self.is_light &&
			self.protocol_version == PeerProtocolVersion::V2 &&
			self.topic_affinity.is_none())
	}

	fn kind(&self) -> &'static str {
		if self.is_light {
			"light"
		} else {
			"full"
		}
	}
}

impl<N, S> StatementHandler<N, S>
where
	N: NetworkPeers + NetworkEventStream,
	S: SyncEventStream + sp_consensus::SyncOracle,
{
	/// Create a new `StatementHandler` for testing/benchmarking purposes.
	#[cfg(any(test, feature = "test-helpers"))]
	pub fn new_for_testing(
		protocol_name: ProtocolName,
		notification_service: Box<dyn NotificationService>,
		propagate_timeout: stream::Fuse<Pin<Box<dyn Stream<Item = ()> + Send>>>,
		network: N,
		sync: S,
		sync_event_stream: stream::Fuse<Pin<Box<dyn Stream<Item = SyncEvent> + Send>>>,
		peers: HashMap<PeerId, Peer>,
		statement_store: Arc<dyn StatementStore>,
		queue_sender: async_channel::Sender<(Statement, oneshot::Sender<SubmitResult>)>,
		statements_per_second: NonZeroU32,
	) -> Self {
		Self {
			protocol_name,
			notification_service,
			propagate_timeout,
			pending_statements: FuturesUnordered::new(),
			pending_statements_peers: HashMap::new(),
			recently_received_statements: HashMap::new(),
			network,
			sync,
			sync_event_stream,
			peers,
			statement_store,
			queue_sender,
			statements_per_second,
			metrics: None,
			initial_sync_timeout: Box::pin(pending().fuse()),
			pending_affinities_timeout: Box::pin(pending().fuse()),
			pending_initial_syncs: HashMap::new(),
			initial_sync_peer_queue: VecDeque::new(),
			next_initial_sync_id: 0,
			initial_sync_in_flight_bytes: 0,
			propagation_outboxes: HashMap::new(),
			in_flight_chunks: HashMap::new(),
			next_chunk_id: 0,
			propagation_in_flight_bytes: 0,
			parked_propagations: VecDeque::new(),
			pending_sends: FuturesUnordered::new(),
			deferred_peers: HashSet::new(),
			dropped_statements_during_sync: false,
			sync_recovery_peer: None,
			sync_recovery_readd_timeout: Box::pin(pending().fuse()),
		}
	}

	/// Get mutable access to pending statements for testing/benchmarking.
	#[cfg(any(test, feature = "test-helpers"))]
	pub fn pending_statements_mut(
		&mut self,
	) -> &mut FuturesUnordered<Pin<Box<dyn Future<Output = (Hash, Option<SubmitResult>)> + Send>>>
	{
		&mut self.pending_statements
	}

	/// Turns the [`StatementHandler`] into a future that should run forever and not be
	/// interrupted.
	pub async fn run(mut self) {
		loop {
			futures::select_biased! {
				send_result = self.pending_sends.select_next_some() => {
					self.handle_send_result(send_result);
				},
				_ = self.propagate_timeout.next() => {
					self.propagate_statements().await;
					self.shrink_pending_statements_peers();
					self.metrics.as_ref().map(|metrics| {
						metrics.pending_statements.set(self.pending_statements.len() as u64);
					});
				},
				(hash, result) = self.pending_statements.select_next_some() => {
					self.on_statement_submit_result(hash, result);
				},
				sync_event = self.sync_event_stream.next() => {
					if let Some(sync_event) = sync_event {
						self.handle_sync_event(sync_event);
					} else {
						// Syncing has seemingly closed. Closing as well.
						return;
					}
				}
				event = self.notification_service.next_event().fuse() => {
					if let Some(event) = event {
						self.handle_notification_event(event).await
					} else {
						// `Notifications` has seemingly closed. Closing as well.
						return
					}
				}
				_ = &mut self.initial_sync_timeout => {
					self.process_initial_sync_burst();
					self.initial_sync_timeout =
						Box::pin(tokio::time::sleep(INITIAL_SYNC_BURST_INTERVAL).fuse());
				},
				_ = &mut self.pending_affinities_timeout => {
					self.process_pending_affinities();
					self.pending_affinities_timeout =
						Box::pin(tokio::time::sleep(PENDING_AFFINITIES_INTERVAL).fuse());
				},
				_ = &mut self.sync_recovery_readd_timeout => {
					self.try_readd_sync_recovery_peer();
					self.sync_recovery_readd_timeout = Box::pin(pending().fuse());
				},
			}

			if !self.sync.is_major_syncing() {
				self.drain_deferred_peers();
				self.start_sync_recovery();
			}
		}
	}

	/// Release excess map capacity once the map is less than a quarter full.
	fn shrink_pending_statements_peers(&mut self) {
		const MIN_RETAINED_CAPACITY: usize = 1024;
		let map = &mut self.pending_statements_peers;
		if map.capacity() > MIN_RETAINED_CAPACITY && map.capacity() / 4 > map.len() {
			map.shrink_to(MIN_RETAINED_CAPACITY);
		}
	}

	/// Record a send attempt that never reached the peer.
	fn record_send_failure(&self, reason: &str) {
		self.metrics.as_ref().map(|metrics| {
			metrics.send_failures.with_label_values(&[reason]).inc();
		});
	}

	/// Record a failed send whose statements' delivery to the peer is abandoned.
	fn record_abandoned_send(&self, reason: &str, statement_count: usize) {
		self.record_send_failure(reason);
		self.metrics.as_ref().map(|metrics| {
			metrics
				.undelivered_statements
				.with_label_values(&[reason])
				.inc_by(statement_count as u64);
		});
	}

	/// Add all peers that were deferred during major sync to the reserved set
	fn drain_deferred_peers(&mut self) {
		if self.deferred_peers.is_empty() {
			return;
		}

		log::debug!(
			target: LOG_TARGET,
			"Major sync complete, adding {} deferred statement peers",
			self.deferred_peers.len(),
		);

		let addrs: HashSet<multiaddr::Multiaddr> = self
			.deferred_peers
			.drain()
			.map(|p| {
				iter::once(multiaddr::Protocol::P2p(p.into())).collect::<multiaddr::Multiaddr>()
			})
			.collect();

		if let Err(err) = self.network.add_peers_to_reserved_set(self.protocol_name.clone(), addrs)
		{
			log::warn!(target: LOG_TARGET, "Failed to add deferred peers: {err}");
		}
	}

	/// Pick one connected peer, remove it from the reserved set (forcing a disconnect), and
	/// schedule it for re-adding after `SYNC_RECOVERY_READD_DELAY`. When the peer reconnects it
	/// performs a fresh initial sync, delivering any statements that were dropped while the
	/// `is_major_syncing` guard was active
	fn start_sync_recovery(&mut self) {
		if !self.dropped_statements_during_sync {
			return;
		}
		self.dropped_statements_during_sync = false;

		if self.sync_recovery_peer.is_some() {
			return;
		}

		let Some(&peer_id) = self.peers.keys().choose(&mut rand::thread_rng()) else {
			return;
		};

		log::trace!(
			target: LOG_TARGET,
			"Major sync complete, force-reconnecting {peer_id} for statement recovery",
		);

		if let Err(err) = self.network.remove_peers_from_reserved_set(
			self.protocol_name.clone(),
			iter::once(peer_id).collect(),
		) {
			log::warn!(target: LOG_TARGET, "Failed to remove peer {peer_id} for sync recovery: {err}");
			return;
		}

		self.sync_recovery_peer = Some(peer_id);
		self.sync_recovery_readd_timeout =
			Box::pin(tokio::time::sleep(SYNC_RECOVERY_READD_DELAY).fuse());
	}

	/// Re-adds the sync-recovery peer to the reserved set after the backoff window has elapsed
	fn try_readd_sync_recovery_peer(&mut self) {
		let Some(peer_id) = self.sync_recovery_peer.take() else { return };
		log::trace!(
			target: LOG_TARGET,
			"Re-adding {peer_id} to reserved set after sync recovery window",
		);
		let addr =
			iter::once(multiaddr::Protocol::P2p(peer_id.into())).collect::<multiaddr::Multiaddr>();
		if let Err(err) = self
			.network
			.add_peers_to_reserved_set(self.protocol_name.clone(), iter::once(addr).collect())
		{
			log::warn!(target: LOG_TARGET, "Failed to re-add sync recovery peer {peer_id}: {err}");
		}
	}

	/// React to peer connect/disconnect events from the sync subsystem:
	///
	/// - On connect while major-syncing: defer the peer (kept in `deferred_peers`) instead of
	///   adding it to the statement protocol's reserved set, prioritizing block download; deferred
	///   peers are flushed once syncing finishes.
	/// - On connect otherwise: add the peer to the reserved set.
	/// - On disconnect: remove the peer from the reserved set, or from the deferred set if it never
	///   joined.
	fn handle_sync_event(&mut self, event: SyncEvent) {
		match event {
			SyncEvent::PeerConnected { peer_id: remote, roles: _ } => {
				if self.sync.is_major_syncing() {
					log::trace!(
						target: LOG_TARGET,
						"Major sync in progress, deferring connection to {remote}",
					);
					self.deferred_peers.insert(remote);
					return;
				}
				let addr = iter::once(multiaddr::Protocol::P2p(remote.into()))
					.collect::<multiaddr::Multiaddr>();
				let result = self.network.add_peers_to_reserved_set(
					self.protocol_name.clone(),
					iter::once(addr).collect(),
				);
				if let Err(err) = result {
					log::error!(target: LOG_TARGET, "Add reserved peer failed: {}", err);
				}
			},
			SyncEvent::PeerDisconnected(remote) => {
				if self.deferred_peers.remove(&remote) {
					return;
				}
				let result = self.network.remove_peers_from_reserved_set(
					self.protocol_name.clone(),
					iter::once(remote).collect(),
				);
				if let Err(err) = result {
					log::error!(target: LOG_TARGET, "Failed to remove reserved peer: {err}");
				}
			},
		}
	}

	/// Dispatch a notification-protocol event for the statement protocol:
	///
	/// - Validates inbound substreams by peer role.
	/// - Tracks stream open/close to maintain per-peer state.
	/// - Decodes incoming notifications: V1 peers send raw statement batches; V2 peers send a
	///   `StatementMessage` that is either a batch of statements or an `ExplicitTopicAffinity`
	///   advertisement.
	/// - Rate-limits affinity advertisements (reporting `rep::BAD_MESSAGE` on abuse); otherwise
	///   stores the filter as pending until applied by the main loop.
	async fn handle_notification_event(&mut self, event: NotificationEvent) {
		match event {
			NotificationEvent::ValidateInboundSubstream { peer, handshake, result_tx, .. } => {
				// Only accept peers whose role can be determined
				let result = self
					.network
					.peer_role(peer, handshake)
					.map_or(ValidationResult::Reject, |_| ValidationResult::Accept);
				let _ = result_tx.send(result);
			},
			NotificationEvent::NotificationStreamOpened {
				peer,
				negotiated_fallback,
				handshake,
				..
			} => {
				// If negotiated_fallback is Some, the peer connected on a fallback protocol
				// (v1). If None, the peer connected on the main protocol (v2).
				let protocol_version = if negotiated_fallback.is_some() {
					PeerProtocolVersion::V1
				} else {
					PeerProtocolVersion::V2
				};
				let Some(peer_role) = self.network.peer_role(peer, handshake) else {
					log::debug!(
						target: LOG_TARGET,
						"Peer {peer} connected but role could not be determined, ignoring"
					);
					return;
				};
				let is_light = peer_role.is_light();
				log::debug!(
					target: LOG_TARGET,
					"Peer {peer} connected with statement protocol {protocol_version:?}, role={peer_role:?}"
				);
				let _was_in = self.peers.insert(
					peer,
					Peer {
						rate_limiter: PeerRateLimiter::new(
							self.statements_per_second,
							NonZeroU32::new(
								self.statements_per_second.get() *
									config::STATEMENTS_BURST_COEFFICIENT,
							)
							.expect("burst capacity is nonzero"),
						),
						protocol_version,
						topic_affinity: None,
						is_light,
						pending_topic_affinity: None,
						sync_watermark: 0,
					},
				);
				debug_assert!(_was_in.is_none());

				self.metrics.as_ref().map(|metrics| {
					if let Some(peer) = self.peers.get(&peer) {
						metrics.peers_connected.with_label_values(&[peer.kind()]).inc();
					}
				});

				// Light V2 peers must set topic affinity before receiving statements.
				// All other peers get initial sync immediately.
				if self.peers.get(&peer).map_or(false, |p| p.can_receive()) {
					self.schedule_initial_sync_for_peer(peer);
				}
			},
			NotificationEvent::NotificationStreamClosed { peer } => {
				let removed_peer = self.peers.remove(&peer);
				debug_assert!(removed_peer.is_some());

				if let Some(removed_peer) = removed_peer {
					self.metrics.as_ref().map(|metrics| {
						metrics.peers_connected.with_label_values(&[removed_peer.kind()]).dec();
					});
				}

				if let Some(pending) = self.pending_initial_syncs.remove(&peer) {
					self.record_initial_sync_completion(
						sync_outcome::ABANDONED,
						pending.started_at,
					);
				}
				self.initial_sync_peer_queue.retain(|p| *p != peer);
				self.propagation_outboxes.remove(&peer);
				self.in_flight_chunks.remove(&peer);
			},
			NotificationEvent::NotificationReceived { peer, notification } => {
				let bytes_received = notification.len() as u64;
				self.metrics.as_ref().map(|metrics| {
					metrics.bytes_received_total.inc_by(bytes_received);
				});

				// Accept statements only when node is not major syncing
				if self.sync.is_major_syncing() {
					log::trace!(
						target: LOG_TARGET,
						"{peer}: Ignoring statements while major syncing or offline"
					);
					self.dropped_statements_during_sync = true;
					return;
				}

				let Some(peer_data) = self.peers.get(&peer) else {
					log::error!(target: LOG_TARGET, "Received notification from unknown peer {peer}");
					return;
				};

				match peer_data.protocol_version {
					PeerProtocolVersion::V1 => {
						// V1 peers send raw Vec<Statement>.
						if let Ok(statements) =
							<StatementBatch as Decode>::decode(&mut notification.as_ref())
						{
							self.on_statements(peer, statements.into_inner());
						} else {
							log::debug!(
								target: LOG_TARGET,
								"Failed to decode v1 statement list from {peer}"
							);
							self.network.report_peer(peer, rep::BAD_MESSAGE);
						}
					},
					PeerProtocolVersion::V2 => {
						// V2 peers send StatementMessage enum.
						if let Ok(message) = StatementMessage::decode(&mut notification.as_ref()) {
							match message {
								StatementMessage::Statements(statements) => {
									self.on_statements(peer, statements.into_inner())
								},
								StatementMessage::ExplicitTopicAffinity(filter) => {
									if let Some(peer_data) = self.peers.get_mut(&peer) {
										if peer_data.rate_limiter.is_flooding(1) {
											log::debug!(
												target: LOG_TARGET,
												"Rate-limiting ExplicitTopicAffinity from {peer}"
											);
											self.network.report_peer(peer, rep::BAD_MESSAGE);
										} else {
											log::debug!(
												target: LOG_TARGET,
												"Received topic affinity filter from {peer}"
											);
											// Defer both the affinity update and sync scheduling
											// to the main loop tick.
											peer_data.pending_topic_affinity = Some(filter);
										}
									}
								},
							}
						} else {
							log::debug!(
								target: LOG_TARGET,
								"Failed to decode v2 statement message from {peer}"
							);
							self.network.report_peer(peer, rep::BAD_MESSAGE);
						}
					},
				}
			},
		}
	}

	/// Handle a batch of statements received from a peer.
	///
	/// For the batch:
	/// - Enforces the per-peer rate limit — on abuse, disconnects the peer and reports
	///   `rep::STATEMENT_FLOODING`.
	/// - Skips statements already in the store, reporting `rep::DUPLICATE_STATEMENT` if the same
	///   peer sent it twice.
	/// - Enqueues unknown statements onto the bounded validation queue.
	/// - Drops the remaining statements in the batch if the queue is full
	///   (`MAX_PENDING_STATEMENTS`).
	#[cfg_attr(not(any(test, feature = "test-helpers")), doc(hidden))]
	pub fn on_statements(&mut self, who: PeerId, statements: Statements) {
		log::trace!(target: LOG_TARGET, "Received {} statements from {}", statements.len(), who);

		self.metrics.as_ref().map(|metrics| {
			metrics.statements_received.inc_by(statements.len() as u64);
		});

		if let Some(ref mut peer) = self.peers.get_mut(&who) {
			if peer.rate_limiter.is_flooding(statements.len()) {
				log::warn!(
					target: LOG_TARGET,
					"Peer {} exceeded statement rate limit ({} statements/sec). Disconnecting.",
					who,
					self.statements_per_second
				);

				self.network.report_peer(who, rep::STATEMENT_FLOODING);

				// Initiate peer state cleanup in the `NotificationStreamClosed` handler
				self.network.disconnect_peer(who, self.protocol_name.clone());

				if let Some(ref metrics) = self.metrics {
					metrics.statement_flooding_detected.inc();
				}

				return;
			}

			let mut statements_left = statements.len() as u64;
			for s in statements {
				if self.pending_statements.len() > MAX_PENDING_STATEMENTS {
					log::debug!(
						target: LOG_TARGET,
						"Ignoring {} statements that exceed `MAX_PENDING_STATEMENTS`({}) limit",
						statements_left,
						MAX_PENDING_STATEMENTS,
					);
					self.metrics.as_ref().map(|metrics| {
						metrics.ignored_statements.inc_by(statements_left);
					});
					break;
				}

				let hash = s.hash();

				if self.statement_store.has_statement(&hash) {
					self.metrics.as_ref().map(|metrics| {
						metrics.known_statements_received.inc();
					});

					// If the statement still awaits its propagation pass, record the
					// peer so the pass does not send it back. Only join an existing
					// entry, or replays would grow the map without bound. Peers that
					// sent a not yet imported statement are tracked in
					// `pending_statements_peers` and move here on import.
					if let Some(peers) = self.recently_received_statements.get_mut(&hash) {
						peers.insert(who);
					}

					// The store can already hold the statement while its validation
					// completion still awaits processing by the event loop. Join the
					// pending entry so the peer moves to `recently_received_statements`
					// on import, exactly as if the message had arrived before the
					// store insert.
					if let Some(peers) = self.pending_statements_peers.get_mut(&hash) {
						if peers.insert(who) {
							self.network.report_peer(who, rep::ANY_STATEMENT);
						} else {
							log::trace!(
								target: LOG_TARGET,
								"Already received the statement from the same peer {who}.",
							);
							self.network.report_peer(who, rep::DUPLICATE_STATEMENT);
						}
					}
					continue;
				}

				self.network.report_peer(who, rep::ANY_STATEMENT);

				match self.pending_statements_peers.entry(hash) {
					Entry::Vacant(entry) => {
						let (completion_sender, completion_receiver) = oneshot::channel();
						match self.queue_sender.try_send((s, completion_sender)) {
							Ok(()) => {
								self.pending_statements.push(
									async move {
										let res = completion_receiver.await;
										(hash, res.ok())
									}
									.boxed(),
								);
								entry.insert(HashSet::from_iter([who]));
							},
							Err(async_channel::TrySendError::Full(_)) => {
								log::debug!(
									target: LOG_TARGET,
									"Dropped statement because validation channel is full",
								);
							},
							Err(async_channel::TrySendError::Closed(_)) => {
								log::trace!(
									target: LOG_TARGET,
									"Dropped statement because validation channel is closed",
								);
							},
						}
					},
					Entry::Occupied(mut entry) => {
						if !entry.get_mut().insert(who) {
							// Already received this from the same peer.
							self.network.report_peer(who, rep::DUPLICATE_STATEMENT);
						}
					},
				}

				statements_left -= 1;
			}
		}
	}

	/// Adjust the sending peer's reputation based on the outcome of importing a statement it sent.
	///
	/// Every newly received statement is first charged `rep::ANY_STATEMENT` (a small **decrease**)
	/// in [`on_statements`](Self::on_statements); this method applies the follow-up adjustment
	/// once the statement has been validated:
	///
	/// - `New` → **increase** by `rep::GOOD_STATEMENT` — a valid, previously unknown statement; the
	///   net change is positive (the reward outweighs the initial charge).
	/// - `Known` → **increase** by `rep::ANY_STATEMENT_REFUND`, which exactly cancels the initial
	///   `rep::ANY_STATEMENT` charge (net zero) — valid but already in the store.
	/// - `Invalid` → **decrease** by `rep::INVALID_STATEMENT`, a large penalty — the statement
	///   failed validation.
	/// - `KnownExpired`, `Rejected`, `InternalError` → no follow-up change, so the peer keeps the
	///   initial `rep::ANY_STATEMENT` charge.
	fn on_handle_statement_import(&mut self, who: PeerId, import: &SubmitResult) {
		match import {
			SubmitResult::New => self.network.report_peer(who, rep::GOOD_STATEMENT),
			SubmitResult::Known => self.network.report_peer(who, rep::ANY_STATEMENT_REFUND),
			SubmitResult::KnownExpired => {},
			SubmitResult::Rejected(_) => {},
			SubmitResult::Invalid(_) => self.network.report_peer(who, rep::INVALID_STATEMENT),
			SubmitResult::InternalError(_) => {},
		}
	}

	/// Handle a completed validation task. Adjusts the reputation of every peer
	/// that sent us the statement and, if the statement awaits propagation,
	/// records those peers so it is not sent back to them.
	fn on_statement_submit_result(&mut self, hash: Hash, result: Option<SubmitResult>) {
		if let Some(peers) = self.pending_statements_peers.remove(&hash) {
			if let Some(result) = result {
				for peer in &peers {
					self.on_handle_statement_import(*peer, &result);
				}
				// `New` and `Known` mean the statement awaits the next propagation
				// pass. Remember who sent it so the pass does not send it back.
				if matches!(result, SubmitResult::New | SubmitResult::Known) {
					self.recently_received_statements.entry(hash).or_default().extend(peers);
				}
			}
		} else {
			log::warn!(target: LOG_TARGET, "Inconsistent state, no peers for pending statement!");
		}
	}

	/// Queue the given `statements` for propagation to the given `peer`.
	///
	/// Internally filters out statements the peer sent to us.
	/// For v2 peers with a topic affinity filter, also filters by topic match.
	/// Surviving hashes are appended to the peer's outbox.
	fn queue_statements_for_peer(&mut self, who: &PeerId, statements: &[(u64, Hash, Statement)]) {
		let Self {
			peers,
			propagation_outboxes,
			recently_received_statements,
			pending_statements_peers,
			..
		} = self;
		let Some(peer) = peers.get(who) else {
			return;
		};

		if !peer.can_receive() {
			return;
		}

		let to_send = statements.iter().filter_map(|(seq, hash, stmt)| {
			if *seq < peer.sync_watermark {
				return None;
			}
			// The peer supplied this statement, do not send it back.
			if has_received_from(recently_received_statements, pending_statements_peers, hash, who)
			{
				return None;
			}
			// For v2 peers with topic affinity, filter by topic match.
			if peer.topic_affinity.as_ref().is_some_and(|a| !a.matches_statement(stmt)) {
				return None;
			}
			Some(*hash)
		});

		let outbox = propagation_outboxes.entry(*who).or_default();
		let mut queued = 0;
		let mut overflow = 0;
		for hash in to_send {
			// The freshest statements are the ones still worth delivering, so an
			// overflowing outbox drops from the front.
			if outbox.len() == MAX_PROPAGATION_OUTBOX_LEN {
				outbox.pop_front();
				overflow += 1;
			}
			outbox.push_back(hash);
			queued += 1;
		}

		log::trace!(target: LOG_TARGET, "We have {queued} statements that the peer doesn't know about");

		if overflow > 0 {
			self.record_abandoned_send(send_failure::OUTBOX_FULL, overflow);
		}
		self.try_send_next_chunk(*who);
	}

	/// Send the next propagation chunk to `who` if its send slot is free.
	///
	/// Statements are fetched, filtered and encoded only when a chunk actually goes out, so a
	/// slow peer holds one encoded chunk, not its whole backlog. Hashes whose statements left
	/// the store since they were queued are dropped.
	fn try_send_next_chunk(&mut self, who: PeerId) {
		if self.in_flight_chunks.contains_key(&who) {
			return;
		}

		loop {
			let Some(outbox) = self.propagation_outboxes.get(&who) else {
				return;
			};
			if outbox.is_empty() {
				self.propagation_outboxes.remove(&who);
				return;
			}
			let Some(peer_data) = self.peers.get(&who) else {
				self.propagation_outboxes.remove(&who);
				return;
			};
			// Admission is checked before fetching, so a saturated budget leaves the
			// outbox untouched.
			if self.send_in_flight_bytes() >= self.propagation_send_budget() {
				// A peer parks once per saturation, not once per tick, so a budget that
				// stays full does not grow the deque without bound.
				if !self.parked_propagations.contains(&who) {
					self.parked_propagations.push_back(who);
				}
				return;
			}
			let peer_version = peer_data.protocol_version;
			let max_size = max_statement_payload_size(peer_version.envelope_overhead());
			let Some(outbox) = self.propagation_outboxes.get_mut(&who) else {
				return;
			};
			let (statements, processed, accumulated_size) = match fetch_statement_chunk(
				&*self.statement_store,
				&self.recently_received_statements,
				&self.pending_statements_peers,
				&who,
				peer_data,
				outbox.make_contiguous(),
				max_size,
			) {
				Ok(result) => result,
				Err(e) => {
					// A store read error says nothing about the queued hashes, so the outbox
					// is kept and the peer parked: the fetch is retried when a completed send
					// or a propagation tick next refills parked peers.
					log::warn!(
						target: LOG_TARGET,
						"Failed to fetch statements for propagation to {who}, retaining {} queued hashes: {e:?}",
						outbox.len(),
					);
					if !self.parked_propagations.contains(&who) {
						self.parked_propagations.push_back(who);
					}
					return;
				},
			};

			debug_assert!(
				processed > 0,
				"a fetch from a non-empty outbox consumes at least one hash"
			);
			if processed == 0 {
				return;
			}

			// Consume the fetched hashes before the oversized check, otherwise the oversized
			// statement would be fetched again on the next iteration.
			outbox.drain(..processed);

			if accumulated_size > max_size {
				log::warn!(target: LOG_TARGET, "Statement too large, skipping");
				self.metrics.as_ref().map(|metrics| {
					metrics.skipped_oversized_statements.inc();
				});
				continue;
			}

			if statements.is_empty() {
				// Everything fetched was filtered out or pruned, but the remaining hashes
				// may still yield a chunk.
				continue;
			}

			let statement_count = statements.len();
			let send_stmts: Vec<_> = statements.iter().map(|(_, stmt)| stmt).collect();
			let encoded = match peer_version {
				PeerProtocolVersion::V1 => send_stmts.encode(),
				PeerProtocolVersion::V2 => StatementMessage::encode_statement_refs(&send_stmts),
			};
			let bytes_sent = encoded.len() as u64;
			let Some(message_sink) = self.notification_service.message_sink(&who) else {
				let abandoned = statement_count +
					self.propagation_outboxes.get(&who).map_or(0, |outbox| outbox.len());
				log::debug!(
					target: LOG_TARGET,
					"Failed to get message sink for peer {who}, abandoning {abandoned} statements ({bytes_sent} bytes in the current chunk)",
				);
				self.record_abandoned_send(send_failure::NO_SINK, abandoned);
				self.propagation_outboxes.remove(&who);
				return;
			};
			let chunk_id = self.occupy_send_slot(who);
			let in_flight = self.propagation_in_flight_bytes.saturating_add(bytes_sent);
			self.set_propagation_in_flight_bytes(in_flight);
			let sent_latency =
				self.metrics.as_ref().map(|metrics| metrics.sent_latency_seconds.clone());
			self.pending_sends.push(Box::pin(async move {
				let sent_latency_timer = sent_latency.map(|metric| metric.start_timer());
				let result = send_with_timeout(message_sink.send_async_notification(encoded)).await;
				drop(sent_latency_timer);
				PendingSendResult {
					peer: who,
					statement_count,
					bytes_sent,
					result,
					kind: SendKind::Propagation,
					chunk_id,
				}
			}));
			return;
		}
	}

	fn handle_send_result(&mut self, send_result: PendingSendResult) {
		let peer = send_result.peer;
		let slot_freed = self.process_send_result(send_result);
		self.fill_parked_propagations();
		if slot_freed {
			self.try_send_next_chunk(peer);
		}
	}

	/// Returns whether the result freed the peer's send slot.
	fn process_send_result(&mut self, send_result: PendingSendResult) -> bool {
		let PendingSendResult { peer, statement_count, bytes_sent, result, kind, chunk_id } =
			send_result;

		let kind_label = kind.label();
		match kind {
			SendKind::Propagation => {
				debug_assert!(
					self.propagation_in_flight_bytes >= bytes_sent,
					"propagation in-flight byte counter underflow"
				);
				let in_flight = self.propagation_in_flight_bytes.saturating_sub(bytes_sent);
				self.set_propagation_in_flight_bytes(in_flight);
			},
			SendKind::InitialSync { .. } => {
				debug_assert!(
					self.initial_sync_in_flight_bytes >= bytes_sent,
					"initial-sync in-flight byte counter underflow"
				);
				let in_flight = self.initial_sync_in_flight_bytes.saturating_sub(bytes_sent);
				self.set_initial_sync_in_flight_bytes(in_flight);
			},
		}

		let failure = match result {
			SendOutcome::Sent => {
				log::trace!(target: LOG_TARGET, "Sent {} statements to {}", statement_count, peer);
				self.metrics.as_ref().map(|metrics| {
					metrics.propagated_statements.inc_by(statement_count as u64);
					metrics.bytes_sent_total.inc_by(bytes_sent);
					metrics
						.propagated_statements_chunks
						.with_label_values(&[kind_label])
						.observe(statement_count as f64);
				});
				None
			},
			SendOutcome::NetworkError(error) => {
				log::debug!(
					target: LOG_TARGET,
					"Failed to send {statement_count} statements ({bytes_sent} bytes) to {peer}: {error}",
				);
				Some(send_failure::NETWORK)
			},
			SendOutcome::TimedOut => {
				log::warn!(
					target: LOG_TARGET,
					"Send of {statement_count} statements ({bytes_sent} bytes) to {peer} timed out after {SEND_TIMEOUT:?}",
				);
				Some(send_failure::TIMEOUT)
			},
		};

		if let Some(reason) = failure {
			match kind {
				SendKind::Propagation => self.record_abandoned_send(reason, statement_count),
				SendKind::InitialSync { .. } => self.record_send_failure(reason),
			}
		}

		// A send future is not cancelled on disconnect, so its result can outlive the
		// connection. Only the result of the chunk still occupying the slot frees it.
		let slot_freed = self.in_flight_chunks.get(&peer) == Some(&chunk_id);
		if slot_freed {
			self.in_flight_chunks.remove(&peer);
		}

		let SendKind::InitialSync { sync_id, next_cursor } = kind else { return slot_freed };

		// A peer that reconnects inside the send timeout loses its sync on disconnect and gets a
		// fresh one under the same `PeerId`; a stale result would advance or abort the wrong sync.
		if self.pending_initial_syncs.get(&peer).map(|pending| pending.sync_id) != Some(sync_id) {
			return slot_freed;
		}

		if failure.is_some() {
			// The cursor still points at the unconfirmed chunk, so a later burst resends it.
			self.initial_sync_peer_queue.push_back(peer);
			return slot_freed;
		}

		if let Some(pending) = self.pending_initial_syncs.get_mut(&peer) {
			pending.cursor = next_cursor;
		}
		self.metrics.as_ref().map(|metrics| {
			metrics.initial_sync_statements_sent.inc_by(statement_count as u64);
		});
		// Reached only for the live sync, which is out of the queue for as long as its chunk is
		// in flight; a superseded sync's chunk can still be in flight under the same `PeerId`, so
		// the bound is one chunk per sync, not per peer.
		self.initial_sync_peer_queue.push_back(peer);
		slot_freed
	}

	#[cfg(test)]
	async fn flush_pending_sends(&mut self) {
		while let Some(result) = self.pending_sends.next().await {
			self.handle_send_result(result);
		}
	}

	fn do_propagate_statements(&mut self, statements: &[(u64, Hash, Statement)]) {
		log::debug!(target: LOG_TARGET, "Propagating {} statements for {} peers", statements.len(), self.peers.len());
		let peers: Vec<_> = self.peers.keys().copied().collect();
		for who in peers {
			log::trace!(target: LOG_TARGET, "Start propagating statements for {}", who);
			self.queue_statements_for_peer(&who, statements);
		}
		log::trace!(target: LOG_TARGET, "Statements queued for propagation to all peers");
	}

	/// Call when we must propagate ready statements to peers.
	async fn propagate_statements(&mut self) {
		// Send out statements only when node is not major syncing
		if self.sync.is_major_syncing() {
			return;
		}

		// A peer parked by a failed store fetch has no completed send to unpark it, so
		// parked peers are also refilled on the tick.
		self.fill_parked_propagations();

		let Ok(statements) = self.statement_store.take_recent_statements() else { return };
		if !statements.is_empty() {
			self.do_propagate_statements(&statements);
		}
		// Every entry here belongs to an already drained statement, so it is done
		// propagating. Statements imported after the drain get their entries only
		// after this clear, when the event loop processes their submit results.
		self.recently_received_statements.clear();
	}

	/// Schedule an initial sync for a peer, sending all known statements.
	///
	/// This is called both when a new peer connects and when a peer's topic
	/// affinity changes (so that newly-matching statements get sent).
	/// If the peer already has a pending initial sync, it is replaced.
	fn schedule_initial_sync_for_peer(&mut self, peer: PeerId) {
		// A peer absent from the map has no entry to mirror the sync's watermark into.
		if !self.peers.contains_key(&peer) {
			return;
		}
		// The watermark is read before the existing sync is touched, so a store error
		// leaves an in-progress sync running instead of destroying it with no successor.
		let watermark = match self.statement_store.admission_watermark() {
			Ok(watermark) => watermark,
			Err(e) => {
				log::warn!(
					target: LOG_TARGET,
					"Failed to read the admission watermark, skipping initial sync for {peer}: {e:?}",
				);
				return;
			},
		};
		let sync_id = self.next_initial_sync_id;
		self.next_initial_sync_id = self.next_initial_sync_id.saturating_add(1);
		if let Some(pending) = self.pending_initial_syncs.remove(&peer) {
			self.record_initial_sync_completion(sync_outcome::ABANDONED, pending.started_at);
			self.initial_sync_peer_queue.retain(|p| *p != peer);
		}
		if watermark > 0 {
			if let Some(peer_data) = self.peers.get_mut(&peer) {
				peer_data.sync_watermark = peer_data.sync_watermark.max(watermark);
			}
			// Hashes queued for propagation before this scheduling sit below the new
			// watermark, so the cursor already covers them; dropping the outbox keeps
			// them from arriving twice.
			self.propagation_outboxes.remove(&peer);
			self.pending_initial_syncs.insert(
				peer,
				PendingInitialSync { cursor: 0, watermark, started_at: Instant::now(), sync_id },
			);
			self.initial_sync_peer_queue.push_back(peer);
			self.metrics.as_ref().map(|metrics| {
				metrics.initial_sync_peers_active.inc();
			});
		}
	}

	/// Process pending topic affinity changes for peers that have no active initial sync.
	///
	/// When a peer sends `ExplicitTopicAffinity`, we defer the expensive
	/// `schedule_initial_sync_for_peer` call. This method applies the pending affinity
	/// and schedules the sync once the peer's current sync (if any) has completed.
	fn process_pending_affinities(&mut self) {
		let ready_peers: Vec<PeerId> = self
			.peers
			.iter()
			.filter(|(peer_id, peer_data)| {
				peer_data.pending_topic_affinity.is_some() &&
					!self.pending_initial_syncs.contains_key(peer_id)
			})
			.map(|(peer_id, _)| *peer_id)
			.collect();

		for peer_id in ready_peers {
			if let Some(peer_data) = self.peers.get_mut(&peer_id) {
				peer_data.topic_affinity = peer_data.pending_topic_affinity.take();
			}
			self.schedule_initial_sync_for_peer(peer_id);
		}
	}

	/// Set the in-flight initial-sync byte counter.
	fn set_initial_sync_in_flight_bytes(&mut self, bytes: u64) {
		self.initial_sync_in_flight_bytes = bytes;
		self.metrics
			.as_ref()
			.map(|metrics| metrics.initial_sync_in_flight_bytes.set(bytes));
	}

	/// Set the in-flight propagation byte counter.
	fn set_propagation_in_flight_bytes(&mut self, bytes: u64) {
		self.propagation_in_flight_bytes = bytes;
		self.metrics
			.as_ref()
			.map(|metrics| metrics.propagation_in_flight_bytes.set(bytes));
	}

	/// Total encoded bytes in flight across initial-sync and propagation chunks,
	/// held against the shared [`MAX_SEND_IN_FLIGHT_BYTES`] budget.
	fn send_in_flight_bytes(&self) -> u64 {
		self.initial_sync_in_flight_bytes
			.saturating_add(self.propagation_in_flight_bytes)
	}

	/// Byte budget available to propagation sends.
	///
	/// While initial syncs are pending, [`INITIAL_SYNC_RESERVED_BYTES`] are withheld:
	/// propagation reclaims freed budget synchronously on every completed send, while sync
	/// bursts only check on a timer, so without the reserve enough parked propagations
	/// starve initial sync indefinitely.
	fn propagation_send_budget(&self) -> u64 {
		if self.pending_initial_syncs.is_empty() {
			MAX_SEND_IN_FLIGHT_BYTES
		} else {
			MAX_SEND_IN_FLIGHT_BYTES - INITIAL_SYNC_RESERVED_BYTES
		}
	}

	/// Refill parked peers' send slots while the in-flight byte budget allows.
	///
	/// Peers are served in parking order. When the budget saturates the loop stops and the
	/// remaining peers keep their position for the next completed send. Each peer gets one
	/// attempt per pass: a peer whose store fetch fails parks itself again, and an unbounded
	/// loop would spin on it.
	fn fill_parked_propagations(&mut self) {
		for _ in 0..self.parked_propagations.len() {
			if self.send_in_flight_bytes() >= self.propagation_send_budget() {
				return;
			}
			let Some(peer) = self.parked_propagations.pop_front() else { return };
			self.try_send_next_chunk(peer);
		}
	}

	/// Occupy the peer's send slot with a fresh chunk id and return the id.
	fn occupy_send_slot(&mut self, peer: PeerId) -> u64 {
		let chunk_id = self.next_chunk_id;
		self.next_chunk_id = self.next_chunk_id.saturating_add(1);
		self.in_flight_chunks.insert(peer, chunk_id);
		chunk_id
	}

	/// Record initial sync completion metrics for a peer being removed.
	fn record_initial_sync_completion(&self, outcome: &str, started_at: Instant) {
		self.metrics.as_ref().map(|metrics| {
			metrics.initial_sync_peers_active.dec();
			metrics
				.initial_sync_duration_seconds
				.with_label_values(&[outcome])
				.observe(started_at.elapsed().as_secs_f64());
		});
	}

	/// Process one batch of initial sync for the next peer in the queue (round-robin).
	fn process_initial_sync_burst(&mut self) {
		if self.sync.is_major_syncing() {
			return;
		}

		if self.send_in_flight_bytes() >= MAX_SEND_IN_FLIGHT_BYTES {
			log::debug!(
				target: LOG_TARGET,
				"Skipping initial sync burst, {} bytes still in flight",
				self.send_in_flight_bytes(),
			);
			return;
		}

		// A peer whose send slot is busy keeps its turn for a later burst, so one slow peer
		// does not stall every other pending sync.
		let Some(pos) = self
			.initial_sync_peer_queue
			.iter()
			.position(|peer| !self.in_flight_chunks.contains_key(peer))
		else {
			return;
		};
		self.initial_sync_peer_queue.rotate_left(pos);
		let Some(peer_id) = self.initial_sync_peer_queue.pop_front() else {
			return;
		};

		let Entry::Occupied(mut entry) = self.pending_initial_syncs.entry(peer_id) else {
			return;
		};
		let sync_id = entry.get().sync_id;

		self.metrics.as_ref().map(|metrics| {
			metrics.initial_sync_bursts_total.inc();
		});

		if entry.get().cursor >= entry.get().watermark {
			let started_at = entry.get().started_at;
			entry.remove();
			self.record_initial_sync_completion(sync_outcome::COMPLETED, started_at);
			return;
		}

		// Fetch statements up to max_statement_payload_size, filtering directly in the
		// callback (see `fetch_admitted_chunk`).
		let Some(peer_data) = self.peers.get(&peer_id) else {
			log::error!(target: LOG_TARGET, "Peer {peer_id} has pending initial sync but is not in peers map");
			let pending = entry.remove();
			self.record_initial_sync_completion(sync_outcome::ABANDONED, pending.started_at);
			return;
		};
		let peer_version = peer_data.protocol_version;
		let envelope_overhead = peer_version.envelope_overhead();
		let max_size = max_statement_payload_size(envelope_overhead);
		let (batch, accumulated_size) = match fetch_admitted_chunk(
			&*self.statement_store,
			&self.recently_received_statements,
			&self.pending_statements_peers,
			&peer_id,
			peer_data,
			entry.get().cursor,
			entry.get().watermark,
			max_size,
		) {
			Ok(r) => r,
			Err(e) => {
				// A store read error says nothing about the journal, and the cursor is
				// retained, so the sync resumes from the same position on a later burst.
				log::warn!(
					target: LOG_TARGET,
					"Failed to fetch statements for initial sync of {peer_id}, will retry: {e:?}",
				);
				self.initial_sync_peer_queue.push_back(peer_id);
				return;
			},
		};

		// A failed send must resend the same admissions, so the cursor advances only when a
		// send is confirmed; chunks that queue no send advance it here.
		if accumulated_size > max_size {
			log::warn!(target: LOG_TARGET, "Statement too large, skipping");
			self.metrics.as_ref().map(|metrics| {
				metrics.skipped_oversized_statements.inc();
			});
			entry.get_mut().cursor = batch.cursor;
			self.initial_sync_peer_queue.push_back(peer_id);
			return;
		}

		if batch.statements.is_empty() {
			// Nothing was queued, so no result will arrive for this peer. Put it back and let the
			// next burst either send the remainder or observe that the sync is done.
			entry.get_mut().cursor = batch.cursor;
			self.initial_sync_peer_queue.push_back(peer_id);
			return;
		}

		let next_cursor = batch.cursor;

		let statement_count = batch.statements.len();
		let send_stmts: Vec<_> = batch.statements.iter().map(|(_, stmt)| stmt).collect();
		let encoded = match peer_version {
			PeerProtocolVersion::V1 => send_stmts.encode(),
			PeerProtocolVersion::V2 => StatementMessage::encode_statement_refs(&send_stmts),
		};
		let bytes_to_send = encoded.len() as u64;
		let Some(message_sink) = self.notification_service.message_sink(&peer_id) else {
			// A missing sink usually means the peer is disconnecting, which removes the sync;
			// until then the retained cursor lets a later burst retry.
			log::debug!(
				target: LOG_TARGET,
				"Failed to get message sink for peer {peer_id}, its initial sync will retry",
			);
			self.record_send_failure(send_failure::NO_SINK);
			self.initial_sync_peer_queue.push_back(peer_id);
			return;
		};
		let sent_latency =
			self.metrics.as_ref().map(|metrics| metrics.sent_latency_seconds.clone());
		let in_flight = self.initial_sync_in_flight_bytes.saturating_add(bytes_to_send);
		self.set_initial_sync_in_flight_bytes(in_flight);
		let chunk_id = self.occupy_send_slot(peer_id);
		self.pending_sends.push(Box::pin(async move {
			let sent_latency_timer = sent_latency.map(|metric| metric.start_timer());
			let result = send_with_timeout(message_sink.send_async_notification(encoded)).await;
			drop(sent_latency_timer);
			PendingSendResult {
				peer: peer_id,
				statement_count,
				bytes_sent: bytes_to_send,
				result,
				kind: SendKind::InitialSync { sync_id, next_cursor },
				chunk_id,
			}
		}));
	}
}

#[cfg(test)]
mod tests {

	use super::*;
	use governor::clock::FakeRelativeClock;
	use std::{
		sync::{
			atomic::{AtomicBool, AtomicUsize, Ordering},
			Mutex,
		},
		time::Duration,
	};

	/// Default seed used for bloom filters in tests.
	const BLOOM_SEED: u128 = 0x5EED_5EED_5EED_5EED;

	fn new_live_statement() -> Statement {
		let mut statement = sp_statement_store::Statement::new();
		statement.set_expiry_from_parts(u32::MAX, 0);
		statement
	}

	#[derive(Clone)]
	struct TestNetwork {
		reported_peers: Arc<Mutex<Vec<(PeerId, sc_network::ReputationChange)>>>,
		disconnected_peers: Arc<Mutex<Vec<PeerId>>>,
		/// Role to return from `peer_role`. Default: `Full`.
		default_role: sc_network::ObservedRole,
		added_reserved: Arc<Mutex<Vec<HashSet<sc_network::Multiaddr>>>>,
		removed_reserved: Arc<Mutex<Vec<Vec<PeerId>>>>,
	}

	impl TestNetwork {
		fn new() -> Self {
			Self {
				reported_peers: Arc::new(Mutex::new(Vec::new())),
				disconnected_peers: Arc::new(Mutex::new(Vec::new())),
				default_role: sc_network::ObservedRole::Full,
				added_reserved: Arc::new(Mutex::new(Vec::new())),
				removed_reserved: Arc::new(Mutex::new(Vec::new())),
			}
		}

		fn new_light() -> Self {
			Self {
				reported_peers: Arc::new(Mutex::new(Vec::new())),
				disconnected_peers: Arc::new(Mutex::new(Vec::new())),
				default_role: sc_network::ObservedRole::Light,
				added_reserved: Arc::new(Mutex::new(Vec::new())),
				removed_reserved: Arc::new(Mutex::new(Vec::new())),
			}
		}

		fn get_reports(&self) -> Vec<(PeerId, sc_network::ReputationChange)> {
			self.reported_peers.lock().unwrap().clone()
		}

		fn get_disconnected_peers(&self) -> Vec<PeerId> {
			self.disconnected_peers.lock().unwrap().clone()
		}

		fn get_added_reserved(&self) -> Vec<HashSet<sc_network::Multiaddr>> {
			self.added_reserved.lock().unwrap().clone()
		}

		fn get_removed_reserved(&self) -> Vec<Vec<PeerId>> {
			self.removed_reserved.lock().unwrap().clone()
		}
	}

	#[async_trait::async_trait]
	impl NetworkPeers for TestNetwork {
		fn set_authorized_peers(&self, _: std::collections::HashSet<PeerId>) {
			unimplemented!()
		}

		fn set_authorized_only(&self, _: bool) {
			unimplemented!()
		}

		fn add_known_address(&self, _: PeerId, _: sc_network::Multiaddr) {
			unimplemented!()
		}

		fn report_peer(&self, peer_id: PeerId, cost_benefit: sc_network::ReputationChange) {
			self.reported_peers.lock().unwrap().push((peer_id, cost_benefit));
		}

		fn peer_reputation(&self, _: &PeerId) -> i32 {
			unimplemented!()
		}

		fn disconnect_peer(&self, peer: PeerId, _: sc_network::ProtocolName) {
			self.disconnected_peers.lock().unwrap().push(peer);
		}

		fn accept_unreserved_peers(&self) {
			unimplemented!()
		}

		fn deny_unreserved_peers(&self) {
			unimplemented!()
		}

		fn add_reserved_peer(
			&self,
			_: sc_network::config::MultiaddrWithPeerId,
		) -> Result<(), String> {
			unimplemented!()
		}

		fn remove_reserved_peer(&self, _: PeerId) {
			unimplemented!()
		}

		fn set_reserved_peers(
			&self,
			_: sc_network::ProtocolName,
			_: std::collections::HashSet<sc_network::Multiaddr>,
		) -> Result<(), String> {
			unimplemented!()
		}

		fn add_peers_to_reserved_set(
			&self,
			_: sc_network::ProtocolName,
			addrs: std::collections::HashSet<sc_network::Multiaddr>,
		) -> Result<(), String> {
			self.added_reserved.lock().unwrap().push(addrs);
			Ok(())
		}

		fn remove_peers_from_reserved_set(
			&self,
			_: sc_network::ProtocolName,
			peers: Vec<PeerId>,
		) -> Result<(), String> {
			self.removed_reserved.lock().unwrap().push(peers);
			Ok(())
		}

		fn sync_num_connected(&self) -> usize {
			unimplemented!()
		}

		fn peer_role(&self, _: PeerId, _: Vec<u8>) -> Option<sc_network::ObservedRole> {
			Some(self.default_role)
		}

		async fn reserved_peers(&self) -> Result<Vec<PeerId>, ()> {
			unimplemented!();
		}
	}

	#[derive(Clone)]
	struct TestSync {
		major_syncing: Arc<AtomicBool>,
	}

	impl TestSync {
		fn new() -> Self {
			Self { major_syncing: Arc::new(AtomicBool::new(false)) }
		}

		fn with_syncing(initial: bool) -> (Self, Arc<AtomicBool>) {
			let flag = Arc::new(AtomicBool::new(initial));
			(Self { major_syncing: flag.clone() }, flag)
		}
	}

	impl SyncEventStream for TestSync {
		fn event_stream(
			&self,
			_name: &'static str,
		) -> Pin<Box<dyn Stream<Item = sc_network_sync::types::SyncEvent> + Send>> {
			Box::pin(futures::stream::pending())
		}
	}

	impl sp_consensus::SyncOracle for TestSync {
		fn is_major_syncing(&self) -> bool {
			self.major_syncing.load(Ordering::Relaxed)
		}

		fn is_offline(&self) -> bool {
			unimplemented!()
		}
	}

	impl NetworkEventStream for TestNetwork {
		fn event_stream(
			&self,
			_name: &'static str,
		) -> Pin<Box<dyn Stream<Item = sc_network::Event> + Send>> {
			unimplemented!()
		}
	}

	#[derive(Debug, Clone)]
	struct TestNotificationService {
		sent_notifications: Arc<Mutex<Vec<(PeerId, Vec<u8>)>>>,
		block_sends: Arc<AtomicBool>,
		fail_sends: Arc<AtomicBool>,
		sinks_available: Arc<AtomicUsize>,
	}

	impl TestNotificationService {
		fn new() -> Self {
			Self {
				sent_notifications: Arc::new(Mutex::new(Vec::new())),
				block_sends: Arc::new(AtomicBool::new(false)),
				fail_sends: Arc::new(AtomicBool::new(false)),
				sinks_available: Arc::new(AtomicUsize::new(usize::MAX)),
			}
		}

		fn get_sent_notifications(&self) -> Vec<(PeerId, Vec<u8>)> {
			self.sent_notifications.lock().unwrap().clone()
		}

		fn clear_sent_notifications(&self) {
			self.sent_notifications.lock().unwrap().clear();
		}

		fn block_sends(&self) {
			self.block_sends.store(true, Ordering::Relaxed);
		}

		fn fail_sends(&self) {
			self.fail_sends.store(true, Ordering::Relaxed);
		}

		fn allow_sends(&self) {
			self.fail_sends.store(false, Ordering::Relaxed);
		}

		fn serve_sinks(&self, count: usize) {
			self.sinks_available.store(count, Ordering::Relaxed);
		}
	}

	struct TestMessageSink {
		peer: PeerId,
		sent_notifications: Arc<Mutex<Vec<(PeerId, Vec<u8>)>>>,
		block_sends: Arc<AtomicBool>,
		fail_sends: Arc<AtomicBool>,
	}

	#[async_trait::async_trait]
	impl sc_network::service::traits::MessageSink for TestMessageSink {
		fn send_sync_notification(&self, notification: Vec<u8>) {
			self.sent_notifications.lock().unwrap().push((self.peer, notification));
		}

		async fn send_async_notification(
			&self,
			notification: Vec<u8>,
		) -> Result<(), sc_network::error::Error> {
			if self.block_sends.load(Ordering::Relaxed) {
				futures::future::pending::<()>().await;
			}
			if self.fail_sends.load(Ordering::Relaxed) {
				return Err(sc_network::error::Error::ConnectionClosed);
			}
			self.sent_notifications.lock().unwrap().push((self.peer, notification));
			Ok(())
		}
	}

	#[async_trait::async_trait]
	impl NotificationService for TestNotificationService {
		async fn open_substream(&mut self, _peer: PeerId) -> Result<(), ()> {
			unimplemented!()
		}

		async fn close_substream(&mut self, _peer: PeerId) -> Result<(), ()> {
			unimplemented!()
		}

		fn send_sync_notification(&mut self, peer: &PeerId, notification: Vec<u8>) {
			self.sent_notifications.lock().unwrap().push((*peer, notification));
		}

		async fn send_async_notification(
			&mut self,
			peer: &PeerId,
			notification: Vec<u8>,
		) -> Result<(), sc_network::error::Error> {
			if self.fail_sends.load(Ordering::Relaxed) {
				return Err(sc_network::error::Error::ConnectionClosed);
			}
			self.sent_notifications.lock().unwrap().push((*peer, notification));
			Ok(())
		}

		async fn set_handshake(&mut self, _handshake: Vec<u8>) -> Result<(), ()> {
			unimplemented!()
		}

		fn try_set_handshake(&mut self, _handshake: Vec<u8>) -> Result<(), ()> {
			unimplemented!()
		}

		async fn next_event(&mut self) -> Option<sc_network::service::traits::NotificationEvent> {
			None
		}

		fn clone(&mut self) -> Result<Box<dyn NotificationService>, ()> {
			unimplemented!()
		}

		fn protocol(&self) -> &sc_network::types::ProtocolName {
			unimplemented!()
		}

		fn message_sink(
			&self,
			peer: &PeerId,
		) -> Option<Box<dyn sc_network::service::traits::MessageSink>> {
			self.sinks_available
				.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
				.ok()?;
			Some(Box::new(TestMessageSink {
				peer: *peer,
				sent_notifications: self.sent_notifications.clone(),
				block_sends: self.block_sends.clone(),
				fail_sends: self.fail_sends.clone(),
			}))
		}
	}

	#[derive(Clone)]
	struct TestStatementStore {
		statements: Arc<Mutex<HashMap<sp_statement_store::Hash, sp_statement_store::Statement>>>,
		recent_statements:
			Arc<Mutex<HashMap<sp_statement_store::Hash, sp_statement_store::Statement>>>,
		/// Admission journal: the vector index is the statement's admission sequence number.
		admissions: Arc<Mutex<Vec<sp_statement_store::Hash>>>,
		fail_fetches: Arc<AtomicBool>,
	}

	impl TestStatementStore {
		fn new() -> Self {
			Self {
				statements: Default::default(),
				recent_statements: Default::default(),
				admissions: Default::default(),
				fail_fetches: Arc::new(AtomicBool::new(false)),
			}
		}

		/// Insert a statement into the store, recording it in the admission journal.
		fn insert(&self, statement: sp_statement_store::Statement) {
			let hash = statement.hash();
			self.statements.lock().unwrap().insert(hash, statement);
			self.admit(hash);
		}

		/// Record the hash in the admission journal unless it is already admitted, and
		/// return its admission sequence number.
		fn admit(&self, hash: sp_statement_store::Hash) -> u64 {
			let mut admissions = self.admissions.lock().unwrap();
			if let Some(seq) = admissions.iter().position(|admitted| *admitted == hash) {
				return seq as u64;
			}
			admissions.push(hash);
			(admissions.len() - 1) as u64
		}
	}

	impl StatementStore for TestStatementStore {
		fn statements(
			&self,
		) -> sp_statement_store::Result<
			Vec<(sp_statement_store::Hash, sp_statement_store::Statement)>,
		> {
			Ok(self.statements.lock().unwrap().iter().map(|(h, s)| (*h, s.clone())).collect())
		}

		fn take_recent_statements(
			&self,
		) -> sp_statement_store::Result<
			Vec<(u64, sp_statement_store::Hash, sp_statement_store::Statement)>,
		> {
			// A recent statement is a statement the store holds, so make the drained
			// statements visible to `statements_by_hashes` like the real store does.
			let drained: Vec<_> = self.recent_statements.lock().unwrap().drain().collect();
			let mut statements = self.statements.lock().unwrap();
			for (hash, statement) in &drained {
				statements.insert(*hash, statement.clone());
			}
			drop(statements);
			let mut result: Vec<_> = drained
				.into_iter()
				.map(|(hash, statement)| (self.admit(hash), hash, statement))
				.collect();
			result.sort_unstable_by_key(|(seq, ..)| *seq);
			Ok(result)
		}

		fn statement(
			&self,
			_hash: &sp_statement_store::Hash,
		) -> sp_statement_store::Result<Option<sp_statement_store::Statement>> {
			unimplemented!()
		}

		fn has_statement(&self, hash: &sp_statement_store::Hash) -> bool {
			self.statements.lock().unwrap().contains_key(hash)
		}

		fn statements_by_hashes(
			&self,
			hashes: &[sp_statement_store::Hash],
			filter: &mut dyn FnMut(
				&sp_statement_store::Hash,
				&[u8],
				&sp_statement_store::Statement,
			) -> FilterDecision,
		) -> sp_statement_store::Result<(
			Vec<(sp_statement_store::Hash, sp_statement_store::Statement)>,
			usize,
		)> {
			if self.fail_fetches.load(Ordering::Relaxed) {
				return Err(sp_statement_store::Error::Db("fetch failed".into()));
			}
			let statements = self.statements.lock().unwrap();
			let mut result = Vec::new();
			let mut processed = 0;
			for hash in hashes {
				let Some(stmt) = statements.get(hash) else {
					processed += 1;
					continue;
				};
				let encoded = stmt.encode();
				match filter(hash, &encoded, stmt) {
					FilterDecision::Skip => {
						processed += 1;
					},
					FilterDecision::Take => {
						processed += 1;
						result.push((*hash, stmt.clone()));
					},
					FilterDecision::Abort => break,
				}
			}
			Ok((result, processed))
		}

		fn admission_watermark(&self) -> sp_statement_store::Result<u64> {
			Ok(self.admissions.lock().unwrap().len() as u64)
		}

		fn admitted_statements(
			&self,
			mut cursor: u64,
			watermark: u64,
			scan_limit: usize,
			filter: &mut dyn FnMut(
				&sp_statement_store::Hash,
				&[u8],
				&sp_statement_store::Statement,
			) -> FilterDecision,
		) -> sp_statement_store::Result<sp_statement_store::AdmittedBatch> {
			if self.fail_fetches.load(Ordering::Relaxed) {
				return Err(sp_statement_store::Error::Db("fetch failed".into()));
			}
			let admissions = self.admissions.lock().unwrap();
			let statements = self.statements.lock().unwrap();
			let mut result = Vec::new();
			let mut aborted = false;
			let mut scanned = 0usize;
			while cursor < watermark {
				if scanned == scan_limit {
					aborted = true;
					break;
				}
				scanned += 1;
				let Some(hash) = admissions.get(cursor as usize) else { break };
				// A journal entry whose statement left the store is a dead sequence number.
				let Some(statement) = statements.get(hash) else {
					cursor += 1;
					continue;
				};
				let encoded = statement.encode();
				match filter(hash, &encoded, statement) {
					FilterDecision::Skip => cursor += 1,
					FilterDecision::Take => {
						result.push((*hash, statement.clone()));
						cursor += 1;
					},
					FilterDecision::Abort => {
						aborted = true;
						break;
					},
				}
			}
			if !aborted && cursor < watermark {
				cursor = watermark;
			}
			Ok(sp_statement_store::AdmittedBatch {
				statements: result,
				cursor,
				done: cursor >= watermark,
			})
		}

		fn broadcasts(
			&self,
			_match_all_topics: &[sp_statement_store::Topic],
		) -> sp_statement_store::Result<Vec<Vec<u8>>> {
			unimplemented!()
		}

		fn posted(
			&self,
			_match_all_topics: &[sp_statement_store::Topic],
			_dest: [u8; 32],
		) -> sp_statement_store::Result<Vec<Vec<u8>>> {
			unimplemented!()
		}

		fn posted_clear(
			&self,
			_match_all_topics: &[sp_statement_store::Topic],
			_dest: [u8; 32],
		) -> sp_statement_store::Result<Vec<Vec<u8>>> {
			unimplemented!()
		}

		fn broadcasts_stmt(
			&self,
			_match_all_topics: &[sp_statement_store::Topic],
		) -> sp_statement_store::Result<Vec<Vec<u8>>> {
			unimplemented!()
		}

		fn posted_stmt(
			&self,
			_match_all_topics: &[sp_statement_store::Topic],
			_dest: [u8; 32],
		) -> sp_statement_store::Result<Vec<Vec<u8>>> {
			unimplemented!()
		}

		fn posted_clear_stmt(
			&self,
			_match_all_topics: &[sp_statement_store::Topic],
			_dest: [u8; 32],
		) -> sp_statement_store::Result<Vec<Vec<u8>>> {
			unimplemented!()
		}

		fn submit(
			&self,
			_statement: sp_statement_store::Statement,
			_source: sp_statement_store::StatementSource,
		) -> sp_statement_store::SubmitResult {
			unimplemented!()
		}

		fn remove(&self, _hash: &sp_statement_store::Hash) -> sp_statement_store::Result<()> {
			unimplemented!()
		}

		fn remove_by(&self, _who: [u8; 32]) -> sp_statement_store::Result<()> {
			unimplemented!()
		}
	}

	fn build_handler(
		num_peers: usize,
	) -> (
		StatementHandler<TestNetwork, TestSync>,
		TestStatementStore,
		TestNetwork,
		TestNotificationService,
		async_channel::Receiver<(Statement, oneshot::Sender<SubmitResult>)>,
		Vec<PeerId>,
	) {
		let statement_store = TestStatementStore::new();
		let (queue_sender, queue_receiver) = async_channel::bounded(100);
		let network = TestNetwork::new();
		let notification_service = TestNotificationService::new();
		let mut peers = HashMap::new();
		let mut peer_ids = Vec::with_capacity(num_peers);

		for _ in 0..num_peers {
			let peer_id = PeerId::random();
			peer_ids.push(peer_id);
			peers.insert(
				peer_id,
				Peer {
					rate_limiter: PeerRateLimiter::new(
						NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND)
							.expect("DEFAULT_STATEMENTS_PER_SECOND is nonzero"),
						NonZeroU32::new(
							DEFAULT_STATEMENTS_PER_SECOND * config::STATEMENTS_BURST_COEFFICIENT,
						)
						.expect("burst capacity is nonzero"),
					),
					protocol_version: PeerProtocolVersion::V1,
					topic_affinity: None,
					is_light: false,
					pending_topic_affinity: None,
					sync_watermark: 0,
				},
			);
		}

		let handler = StatementHandler {
			protocol_name: format!("/{STATEMENT_PROTOCOL_V1}").into(),
			notification_service: Box::new(notification_service.clone()),
			propagate_timeout: (Box::pin(futures::stream::pending())
				as Pin<Box<dyn Stream<Item = ()> + Send>>)
				.fuse(),
			pending_statements: FuturesUnordered::new(),
			pending_statements_peers: HashMap::new(),
			recently_received_statements: HashMap::new(),
			network: network.clone(),
			sync: TestSync::new(),
			sync_event_stream: (Box::pin(futures::stream::pending())
				as Pin<Box<dyn Stream<Item = sc_network_sync::types::SyncEvent> + Send>>)
				.fuse(),
			peers,
			statement_store: Arc::new(statement_store.clone()),
			queue_sender,
			statements_per_second: NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND)
				.expect("DEFAULT_STATEMENTS_PER_SECOND is nonzero"),
			metrics: None,
			initial_sync_timeout: Box::pin(futures::future::pending()),
			pending_affinities_timeout: Box::pin(futures::future::pending()),
			pending_initial_syncs: HashMap::new(),
			initial_sync_peer_queue: VecDeque::new(),
			next_initial_sync_id: 0,
			initial_sync_in_flight_bytes: 0,
			propagation_outboxes: HashMap::new(),
			in_flight_chunks: HashMap::new(),
			next_chunk_id: 0,
			propagation_in_flight_bytes: 0,
			parked_propagations: VecDeque::new(),
			pending_sends: FuturesUnordered::new(),
			deferred_peers: HashSet::new(),
			dropped_statements_during_sync: false,
			sync_recovery_peer: None,
			sync_recovery_readd_timeout: Box::pin(futures::future::pending()),
		};
		(handler, statement_store, network, notification_service, queue_receiver, peer_ids)
	}

	fn get_peer_hashes(sent: &[(PeerId, Vec<u8>)], peer: PeerId) -> Vec<sp_statement_store::Hash> {
		sent.iter()
			.filter(|(p, _)| *p == peer)
			.flat_map(|(_, notification)| {
				<Statements as Decode>::decode(&mut notification.as_slice()).unwrap()
			})
			.map(|s| s.hash())
			.collect()
	}

	/// Import one queued statement into the store and feed `result` back into the
	/// handler as the main loop would.
	async fn import_queued_statement(
		handler: &mut StatementHandler<TestNetwork, TestSync>,
		statement_store: &TestStatementStore,
		queue_receiver: &async_channel::Receiver<(Statement, oneshot::Sender<SubmitResult>)>,
		result: SubmitResult,
	) {
		let (statement, completion) = queue_receiver.try_recv().unwrap();
		let hash = statement.hash();
		statement_store.insert(statement.clone());
		statement_store.recent_statements.lock().unwrap().insert(hash, statement);
		completion.send(result).unwrap();
		let (hash, result) = handler.pending_statements.next().await.unwrap();
		handler.on_statement_submit_result(hash, result);
	}

	#[tokio::test]
	async fn statement_is_not_sent_back_to_the_peers_it_came_from() {
		let (
			mut handler,
			statement_store,
			_network,
			notification_service,
			queue_receiver,
			peer_ids,
		) = build_handler(3);
		let (sender_a, sender_b, receiver) = (peer_ids[0], peer_ids[1], peer_ids[2]);

		let mut statement = new_live_statement();
		statement.set_plain_data(b"statement from two peers".to_vec());
		let hash = statement.hash();

		// Both peers supply the statement while it is queued for validation.
		handler.on_statements(sender_a, vec![statement.clone()]);
		handler.on_statements(sender_b, vec![statement.clone()]);
		import_queued_statement(&mut handler, &statement_store, &queue_receiver, SubmitResult::New)
			.await;

		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let sent = notification_service.get_sent_notifications();
		assert!(get_peer_hashes(&sent, sender_a).is_empty(), "statement returned to sender_a");
		assert!(get_peer_hashes(&sent, sender_b).is_empty(), "statement returned to sender_b");
		assert_eq!(get_peer_hashes(&sent, receiver), vec![hash]);
		assert!(
			handler.recently_received_statements.is_empty(),
			"recently received statements must be cleared after the propagation pass"
		);

		// Replaying a propagated statement finds no entry to join, so a peer cannot
		// grow the map by resending.
		handler.on_statements(sender_a, vec![statement]);
		assert!(handler.recently_received_statements.is_empty());
	}

	#[tokio::test]
	async fn statement_received_mid_import_is_not_sent_back_to_the_sender() {
		let (
			mut handler,
			statement_store,
			_network,
			notification_service,
			queue_receiver,
			peer_ids,
		) = build_handler(3);
		let (sender_a, sender_b, receiver) = (peer_ids[0], peer_ids[1], peer_ids[2]);

		let mut statement = new_live_statement();
		statement.set_plain_data(b"statement received mid-import".to_vec());
		let hash = statement.hash();

		handler.on_statements(sender_a, vec![statement.clone()]);

		// The worker has inserted the statement into the store, but the event loop
		// has not processed the validation completion yet.
		let (queued, completion) = queue_receiver.try_recv().unwrap();
		statement_store.insert(queued.clone());
		statement_store.recent_statements.lock().unwrap().insert(hash, queued);

		// The second peer sends the same statement inside that window.
		handler.on_statements(sender_b, vec![statement]);
		assert!(handler
			.pending_statements_peers
			.get(&hash)
			.is_some_and(|peers| peers.contains(&sender_b)));

		completion.send(SubmitResult::New).unwrap();
		let (hash, result) = handler.pending_statements.next().await.unwrap();
		handler.on_statement_submit_result(hash, result);

		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let sent = notification_service.get_sent_notifications();
		assert!(get_peer_hashes(&sent, sender_a).is_empty(), "statement returned to sender_a");
		assert!(get_peer_hashes(&sent, sender_b).is_empty(), "statement returned to sender_b");
		assert_eq!(get_peer_hashes(&sent, receiver), vec![hash]);
	}

	#[tokio::test]
	async fn statement_forwarded_before_the_tick_is_not_sent_back_to_the_forwarder() {
		let (
			mut handler,
			statement_store,
			_network,
			notification_service,
			queue_receiver,
			peer_ids,
		) = build_handler(3);
		let (sender, forwarder, receiver) = (peer_ids[0], peer_ids[1], peer_ids[2]);

		let mut statement = new_live_statement();
		statement.set_plain_data(b"late forwarder".to_vec());
		let hash = statement.hash();

		handler.on_statements(sender, vec![statement.clone()]);
		// Another path imported the statement while it was queued, so validation
		// completes with `Known`. The peer that sent it must be recorded all the same.
		import_queued_statement(
			&mut handler,
			&statement_store,
			&queue_receiver,
			SubmitResult::Known,
		)
		.await;
		// The statement is imported but not yet propagated when another peer forwards it.
		handler.on_statements(forwarder, vec![statement]);

		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let sent = notification_service.get_sent_notifications();
		assert!(
			get_peer_hashes(&sent, sender).is_empty(),
			"statement returned to the peer that sent it"
		);
		assert!(get_peer_hashes(&sent, forwarder).is_empty(), "statement returned to forwarder");
		assert_eq!(get_peer_hashes(&sent, receiver), vec![hash]);
	}

	#[tokio::test]
	async fn recently_received_statements_survive_a_major_sync_early_return() {
		let (
			mut handler,
			statement_store,
			_network,
			notification_service,
			queue_receiver,
			peer_ids,
		) = build_handler(2);
		let (sender, receiver) = (peer_ids[0], peer_ids[1]);

		let mut statement = new_live_statement();
		statement.set_plain_data(b"during major sync".to_vec());
		let hash = statement.hash();

		handler.on_statements(sender, vec![statement]);
		import_queued_statement(&mut handler, &statement_store, &queue_receiver, SubmitResult::New)
			.await;

		// The early return skips the drain, so the statement stays recent and its
		// entry must stay with it.
		handler.sync.major_syncing.store(true, Ordering::Relaxed);
		handler.propagate_statements().await;
		assert!(
			handler.recently_received_statements.contains_key(&hash),
			"entries must survive the major-sync early return"
		);

		handler.sync.major_syncing.store(false, Ordering::Relaxed);
		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let sent = notification_service.get_sent_notifications();
		assert!(
			get_peer_hashes(&sent, sender).is_empty(),
			"statement returned to the peer that sent it"
		);
		assert_eq!(get_peer_hashes(&sent, receiver), vec![hash]);
		assert!(handler.recently_received_statements.is_empty());
	}

	#[tokio::test]
	async fn propagation_does_not_wait_for_pending_send() {
		let (mut handler, statement_store, _, notification_service, _, _) = build_handler(1);
		let mut statement = new_live_statement();
		statement.set_plain_data(b"statement".to_vec());
		statement_store
			.recent_statements
			.lock()
			.unwrap()
			.insert(statement.hash(), statement);

		notification_service.block_sends();
		let result =
			tokio::time::timeout(Duration::from_secs(1), handler.propagate_statements()).await;

		assert!(result.is_ok(), "Propagation waited for a pending send");
		assert_eq!(handler.pending_sends.len(), 1);
	}

	#[tokio::test]
	async fn slow_peer_keeps_one_propagation_chunk_in_flight() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		// 100 KB each, so the tick spans several 1 MiB chunks.
		for i in 0..25u8 {
			let mut statement = new_live_statement();
			let mut data = vec![0u8; 100 * 1024];
			data[0] = i;
			statement.set_plain_data(data);
			statement_store
				.recent_statements
				.lock()
				.unwrap()
				.insert(statement.hash(), statement);
		}

		// The peer never reads its substream.
		notification_service.block_sends();
		handler.propagate_statements().await;

		assert_eq!(handler.pending_sends.len(), 1, "only one chunk may be in flight");
		let backlog = handler.propagation_outboxes.get(&peer_id).unwrap().len();
		assert!(backlog > 0, "the remaining hashes stay in the outbox");

		// Another tick accumulates into the same outbox while the slot is busy.
		let mut statement = new_live_statement();
		statement.set_plain_data(b"second tick".to_vec());
		statement_store
			.recent_statements
			.lock()
			.unwrap()
			.insert(statement.hash(), statement);
		handler.propagate_statements().await;

		assert_eq!(handler.pending_sends.len(), 1);
		assert_eq!(handler.propagation_outboxes.get(&peer_id).unwrap().len(), backlog + 1);
	}

	#[tokio::test]
	async fn statement_pruned_between_tick_and_send_is_skipped() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		let mut kept = new_live_statement();
		kept.set_plain_data(b"kept".to_vec());
		let kept_hash = kept.hash();
		statement_store.insert(kept);

		let mut pruned = new_live_statement();
		pruned.set_plain_data(b"pruned".to_vec());
		let pruned_hash = pruned.hash();

		// The pruned statement's hash is queued but the statement left the store.
		handler
			.propagation_outboxes
			.insert(peer_id, VecDeque::from(vec![pruned_hash, kept_hash]));
		handler.try_send_next_chunk(peer_id);
		handler.flush_pending_sends().await;

		let sent = get_peer_hashes(&notification_service.get_sent_notifications(), peer_id);
		assert_eq!(sent, vec![kept_hash]);
		assert!(
			!handler.propagation_outboxes.contains_key(&peer_id),
			"the drained outbox must be removed"
		);
	}

	#[tokio::test]
	async fn failed_store_fetch_parks_the_peer_and_the_tick_retries() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		let mut statement = new_live_statement();
		statement.set_plain_data(b"statement".to_vec());
		let hash = statement.hash();
		statement_store.statements.lock().unwrap().insert(hash, statement);
		handler.propagation_outboxes.insert(peer_id, VecDeque::from(vec![hash]));

		statement_store.fail_fetches.store(true, Ordering::Relaxed);
		handler.try_send_next_chunk(peer_id);

		assert!(handler.pending_sends.is_empty(), "a failed fetch must not queue a send");
		assert_eq!(
			handler.propagation_outboxes.get(&peer_id).unwrap().len(),
			1,
			"the outbox must be retained"
		);
		assert_eq!(handler.parked_propagations, VecDeque::from([peer_id]));

		// No new statements arrive for the peer: the tick alone must retry the parked fetch.
		statement_store.fail_fetches.store(false, Ordering::Relaxed);
		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let sent = get_peer_hashes(&notification_service.get_sent_notifications(), peer_id);
		assert_eq!(sent, vec![hash]);
		assert!(!handler.propagation_outboxes.contains_key(&peer_id));
		assert!(handler.parked_propagations.is_empty());
	}

	#[tokio::test]
	async fn oversized_statement_in_the_outbox_is_consumed() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		handler.metrics = Some(Metrics::register(&Registry::new()).unwrap());
		let peer_id = peer_ids[0];

		let mut oversized = new_live_statement();
		oversized.set_plain_data(vec![1u8; MAX_STATEMENT_NOTIFICATION_SIZE as usize]);
		let oversized_hash = oversized.hash();
		let mut small = new_live_statement();
		small.set_plain_data(b"small".to_vec());
		let small_hash = small.hash();
		statement_store.insert(oversized);
		statement_store.insert(small);

		// The oversized statement heads the outbox. It must be consumed, not
		// re-fetched forever, and the statement behind it must still go out.
		handler
			.propagation_outboxes
			.insert(peer_id, VecDeque::from(vec![oversized_hash, small_hash]));
		handler.try_send_next_chunk(peer_id);
		handler.flush_pending_sends().await;

		let sent = get_peer_hashes(&notification_service.get_sent_notifications(), peer_id);
		assert_eq!(sent, vec![small_hash]);
		assert_eq!(handler.metrics.as_ref().unwrap().skipped_oversized_statements.get(), 1);
	}

	#[tokio::test]
	async fn disconnect_clears_the_outbox_and_send_slot() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		// Several chunks worth of statements with a blocked substream, so the slot
		// is taken and a backlog stays queued.
		for i in 0..25u8 {
			let mut statement = new_live_statement();
			let mut data = vec![0u8; 100 * 1024];
			data[0] = i;
			statement.set_plain_data(data);
			statement_store
				.recent_statements
				.lock()
				.unwrap()
				.insert(statement.hash(), statement);
		}
		notification_service.block_sends();
		handler.propagate_statements().await;
		assert!(handler.propagation_outboxes.contains_key(&peer_id));
		assert!(handler.in_flight_chunks.contains_key(&peer_id));

		handler
			.handle_notification_event(NotificationEvent::NotificationStreamClosed {
				peer: peer_id,
			})
			.await;

		assert!(!handler.propagation_outboxes.contains_key(&peer_id));
		assert!(!handler.in_flight_chunks.contains_key(&peer_id));
	}

	#[tokio::test]
	async fn failed_propagation_send_frees_the_slot_and_the_backlog_keeps_draining() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		// 700 KiB each, so a 1 MiB chunk carries exactly one statement.
		let mut first = new_live_statement();
		first.set_plain_data(vec![1u8; 700 * 1024]);
		let first_hash = first.hash();
		let mut second = new_live_statement();
		second.set_plain_data(vec![2u8; 700 * 1024]);
		let second_hash = second.hash();
		statement_store.insert(first);
		statement_store.insert(second);
		handler
			.propagation_outboxes
			.insert(peer_id, VecDeque::from(vec![first_hash, second_hash]));

		// Only the first chunk's send fails.
		notification_service.fail_sends();
		handler.try_send_next_chunk(peer_id);
		assert!(handler.in_flight_chunks.contains_key(&peer_id));
		let result = handler.pending_sends.next().await.unwrap();
		notification_service.allow_sends();
		handler.handle_send_result(result);

		// The failure freed the slot and the backlog kept draining: the second
		// statement went out, the failed one was not retried.
		handler.flush_pending_sends().await;
		let sent = get_peer_hashes(&notification_service.get_sent_notifications(), peer_id);
		assert_eq!(sent, vec![second_hash]);
		assert!(!handler.propagation_outboxes.contains_key(&peer_id));
		assert!(!handler.in_flight_chunks.contains_key(&peer_id));
	}

	#[tokio::test]
	async fn overflowing_outbox_drops_the_oldest_hashes() {
		let (mut handler, statement_store, _network, _notification_service, _, peer_ids) =
			build_handler(1);
		handler.metrics = Some(Metrics::register(&Registry::new()).unwrap());
		let peer_id = peer_ids[0];

		let mut old = new_live_statement();
		old.set_plain_data(b"oldest".to_vec());
		let old_hash = old.hash();

		// Three fresh statements arrive by tick while the outbox is full and the
		// peer's slot is busy.
		let fresh_hashes: HashSet<_> = (0..3u8)
			.map(|i| {
				let mut fresh = new_live_statement();
				fresh.set_plain_data(vec![i; 8]);
				let hash = fresh.hash();
				statement_store.recent_statements.lock().unwrap().insert(hash, fresh);
				hash
			})
			.collect();
		handler.in_flight_chunks.insert(peer_id, 0);
		handler
			.propagation_outboxes
			.insert(peer_id, VecDeque::from(vec![old_hash; MAX_PROPAGATION_OUTBOX_LEN]));

		handler.propagate_statements().await;

		let outbox = handler.propagation_outboxes.get(&peer_id).unwrap();
		assert_eq!(outbox.len(), MAX_PROPAGATION_OUTBOX_LEN);
		let tail: HashSet<_> =
			outbox.iter().skip(MAX_PROPAGATION_OUTBOX_LEN - 3).copied().collect();
		assert_eq!(tail, fresh_hashes, "the freshest hashes must survive the overflow");

		let metrics = handler.metrics.as_ref().unwrap();
		assert_eq!(
			metrics
				.undelivered_statements
				.with_label_values(&[send_failure::OUTBOX_FULL])
				.get(),
			3,
			"each dropped hash counts as undelivered"
		);
		assert_eq!(
			metrics.send_failures.with_label_values(&[send_failure::OUTBOX_FULL]).get(),
			1,
			"one overflow event"
		);
	}

	#[tokio::test]
	async fn statement_received_while_queued_in_the_outbox_is_not_echoed() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		let mut statement = new_live_statement();
		statement.set_plain_data(b"received after append".to_vec());
		let hash = statement.hash();
		statement_store.insert(statement);

		// The hash was appended while the peer's slot was busy, and the peer sent
		// us the statement before the slot freed: the encode-time senders check
		// must catch what the append-time check could not have seen.
		handler.propagation_outboxes.insert(peer_id, VecDeque::from(vec![hash]));
		handler.recently_received_statements.insert(hash, HashSet::from_iter([peer_id]));

		handler.try_send_next_chunk(peer_id);
		handler.flush_pending_sends().await;

		assert!(
			get_peer_hashes(&notification_service.get_sent_notifications(), peer_id).is_empty(),
			"statement returned to the peer that sent it"
		);
	}

	/// Simulate the network closing the substream for every disconnected
	/// peer, so the handler runs its per-peer cleanup.
	async fn dispatch_disconnects(
		handler: &mut StatementHandler<TestNetwork, TestSync>,
		network: &TestNetwork,
	) {
		for peer in network.get_disconnected_peers() {
			handler
				.handle_notification_event(NotificationEvent::NotificationStreamClosed { peer })
				.await;
		}
	}

	#[tokio::test]
	async fn test_skips_processing_statements_that_already_in_store() {
		let (mut handler, statement_store, _network, _notification_service, queue_receiver, _) =
			build_handler(1);

		let mut statement1 = new_live_statement();
		statement1.set_plain_data(b"statement1".to_vec());

		statement_store.insert(statement1.clone());

		let mut statement2 = new_live_statement();
		statement2.set_plain_data(b"statement2".to_vec());
		let hash2 = statement2.hash();

		let peer_id = *handler.peers.keys().next().unwrap();

		handler.on_statements(peer_id, vec![statement1, statement2]);

		let to_submit = queue_receiver.try_recv();
		assert_eq!(to_submit.unwrap().0.hash(), hash2, "Expected only statement2 to be queued");

		let no_more = queue_receiver.try_recv();
		assert!(no_more.is_err(), "Expected only one statement to be queued");
	}

	#[tokio::test]
	async fn test_reports_for_duplicate_statements() {
		let (mut handler, statement_store, network, _notification_service, queue_receiver, _) =
			build_handler(1);

		let peer_id = *handler.peers.keys().next().unwrap();

		let mut statement1 = new_live_statement();
		statement1.set_plain_data(b"statement1".to_vec());

		handler.on_statements(peer_id, vec![statement1.clone()]);
		{
			// Manually process statements submission
			let (s, _) = queue_receiver.try_recv().unwrap();
			statement_store.insert(s);
			handler.network.report_peer(peer_id, rep::ANY_STATEMENT_REFUND);
		}

		handler.on_statements(peer_id, vec![statement1]);

		let reports = network.get_reports();
		assert_eq!(
			reports,
			vec![
				(peer_id, rep::ANY_STATEMENT),        // Report for first statement
				(peer_id, rep::ANY_STATEMENT_REFUND), // Refund for first statement
				(peer_id, rep::DUPLICATE_STATEMENT)   // Report for duplicate statement
			],
			"Expected ANY_STATEMENT, ANY_STATEMENT_REFUND, DUPLICATE_STATEMENT reputation change, but got: {:?}",
			reports
		);
	}

	#[tokio::test]
	async fn test_splits_large_batches_into_smaller_chunks() {
		let (mut handler, statement_store, _network, notification_service, _queue_receiver, _) =
			build_handler(1);

		let num_statements = 30;
		let statement_size = 100 * 1024; // 100KB per statement
		for i in 0..num_statements {
			let mut statement = new_live_statement();
			let mut data = vec![0u8; statement_size];
			data[0] = i as u8;
			statement.set_plain_data(data);
			let hash = statement.hash();
			statement_store.recent_statements.lock().unwrap().insert(hash, statement);
		}

		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let sent = notification_service.get_sent_notifications();
		let mut total_statements_sent = 0;
		assert!(
			sent.len() == 3,
			"Expected batch to be split into 3 chunks, but got {} chunks",
			sent.len()
		);
		for (_peer, notification) in sent.iter() {
			assert!(
				notification.len() <= MAX_STATEMENT_NOTIFICATION_SIZE as usize,
				"Notification size {} exceeds limit {}",
				notification.len(),
				MAX_STATEMENT_NOTIFICATION_SIZE
			);
			if let Ok(stmts) = <Statements as Decode>::decode(&mut notification.as_slice()) {
				total_statements_sent += stmts.len();
			}
		}

		assert_eq!(
			total_statements_sent, num_statements,
			"Expected all {} statements to be sent, but only {} were sent",
			num_statements, total_statements_sent
		);
	}

	#[tokio::test]
	async fn test_skips_only_oversized_statements() {
		let (mut handler, statement_store, _network, notification_service, _queue_receiver, _) =
			build_handler(1);

		let mut statement1 = new_live_statement();
		statement1.set_plain_data(vec![1u8; 100]);
		let hash1 = statement1.hash();
		statement_store
			.recent_statements
			.lock()
			.unwrap()
			.insert(hash1, statement1.clone());

		let mut oversized1 = new_live_statement();
		oversized1.set_plain_data(vec![2u8; MAX_STATEMENT_NOTIFICATION_SIZE as usize * 100]);
		let hash_oversized1 = oversized1.hash();
		statement_store
			.recent_statements
			.lock()
			.unwrap()
			.insert(hash_oversized1, oversized1);

		let mut statement2 = new_live_statement();
		statement2.set_plain_data(vec![3u8; 100]);
		let hash2 = statement2.hash();
		statement_store
			.recent_statements
			.lock()
			.unwrap()
			.insert(hash2, statement2.clone());

		let mut oversized2 = new_live_statement();
		oversized2.set_plain_data(vec![4u8; MAX_STATEMENT_NOTIFICATION_SIZE as usize]);
		let hash_oversized2 = oversized2.hash();
		statement_store
			.recent_statements
			.lock()
			.unwrap()
			.insert(hash_oversized2, oversized2);

		let mut statement3 = new_live_statement();
		statement3.set_plain_data(vec![5u8; 100]);
		let hash3 = statement3.hash();
		statement_store
			.recent_statements
			.lock()
			.unwrap()
			.insert(hash3, statement3.clone());

		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let sent = notification_service.get_sent_notifications();

		let mut sent_hashes = sent
			.iter()
			.flat_map(|(_peer, notification)| {
				<Statements as Decode>::decode(&mut notification.as_slice()).unwrap()
			})
			.map(|s| s.hash())
			.collect::<Vec<_>>();
		sent_hashes.sort();
		let mut expected_hashes = vec![hash1, hash2, hash3];
		expected_hashes.sort();
		assert_eq!(sent_hashes, expected_hashes, "Only small statements should be sent");
	}

	fn build_handler_no_peers() -> (
		StatementHandler<TestNetwork, TestSync>,
		TestStatementStore,
		TestNetwork,
		TestNotificationService,
	) {
		let statement_store = TestStatementStore::new();
		let (queue_sender, _queue_receiver) = async_channel::bounded(2);
		let network = TestNetwork::new();
		let notification_service = TestNotificationService::new();

		let handler = StatementHandler {
			protocol_name: format!("/{STATEMENT_PROTOCOL_V1}").into(),
			notification_service: Box::new(notification_service.clone()),
			propagate_timeout: (Box::pin(futures::stream::pending())
				as Pin<Box<dyn Stream<Item = ()> + Send>>)
				.fuse(),
			pending_statements: FuturesUnordered::new(),
			pending_statements_peers: HashMap::new(),
			recently_received_statements: HashMap::new(),
			network: network.clone(),
			sync: TestSync::new(),
			sync_event_stream: (Box::pin(futures::stream::pending())
				as Pin<Box<dyn Stream<Item = sc_network_sync::types::SyncEvent> + Send>>)
				.fuse(),
			peers: HashMap::new(),
			statement_store: Arc::new(statement_store.clone()),
			queue_sender,
			statements_per_second: NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND)
				.expect("DEFAULT_STATEMENTS_PER_SECOND is nonzero"),
			metrics: None,
			initial_sync_timeout: Box::pin(futures::future::pending()),
			pending_affinities_timeout: Box::pin(futures::future::pending()),
			pending_initial_syncs: HashMap::new(),
			initial_sync_peer_queue: VecDeque::new(),
			next_initial_sync_id: 0,
			initial_sync_in_flight_bytes: 0,
			propagation_outboxes: HashMap::new(),
			in_flight_chunks: HashMap::new(),
			next_chunk_id: 0,
			propagation_in_flight_bytes: 0,
			parked_propagations: VecDeque::new(),
			pending_sends: FuturesUnordered::new(),
			deferred_peers: HashSet::new(),
			dropped_statements_during_sync: false,
			sync_recovery_peer: None,
			sync_recovery_readd_timeout: Box::pin(futures::future::pending()),
		};
		(handler, statement_store, network, notification_service)
	}

	/// Like `build_handler_no_peers` but the network mock returns `Light` for peer roles.
	fn build_handler_no_peers_light() -> (
		StatementHandler<TestNetwork, TestSync>,
		TestStatementStore,
		TestNetwork,
		TestNotificationService,
	) {
		let statement_store = TestStatementStore::new();
		let (queue_sender, _queue_receiver) = async_channel::bounded(2);
		let network = TestNetwork::new_light();
		let notification_service = TestNotificationService::new();

		let handler = StatementHandler {
			protocol_name: format!("/{STATEMENT_PROTOCOL_V1}").into(),
			notification_service: Box::new(notification_service.clone()),
			propagate_timeout: (Box::pin(futures::stream::pending())
				as Pin<Box<dyn Stream<Item = ()> + Send>>)
				.fuse(),
			pending_statements: FuturesUnordered::new(),
			pending_statements_peers: HashMap::new(),
			recently_received_statements: HashMap::new(),
			network: network.clone(),
			sync: TestSync::new(),
			sync_event_stream: (Box::pin(futures::stream::pending())
				as Pin<Box<dyn Stream<Item = sc_network_sync::types::SyncEvent> + Send>>)
				.fuse(),
			peers: HashMap::new(),
			statement_store: Arc::new(statement_store.clone()),
			queue_sender,
			statements_per_second: NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND)
				.expect("DEFAULT_STATEMENTS_PER_SECOND is nonzero"),
			metrics: None,
			initial_sync_timeout: Box::pin(futures::future::pending()),
			pending_affinities_timeout: Box::pin(futures::future::pending()),
			pending_initial_syncs: HashMap::new(),
			initial_sync_peer_queue: VecDeque::new(),
			next_initial_sync_id: 0,
			initial_sync_in_flight_bytes: 0,
			propagation_outboxes: HashMap::new(),
			in_flight_chunks: HashMap::new(),
			next_chunk_id: 0,
			propagation_in_flight_bytes: 0,
			parked_propagations: VecDeque::new(),
			pending_sends: FuturesUnordered::new(),
			deferred_peers: HashSet::new(),
			dropped_statements_during_sync: false,
			sync_recovery_peer: None,
			sync_recovery_readd_timeout: Box::pin(futures::future::pending()),
		};
		(handler, statement_store, network, notification_service)
	}

	#[tokio::test]
	async fn test_initial_sync_burst_single_peer() {
		let (mut handler, statement_store, _network, notification_service, _, _) = build_handler(0);

		// Create 20MB of statements (200 statements x 100KB each)
		// Using 100KB ensures ~10 statements per 1MB batch, requiring ~20 bursts
		let num_statements = 200;
		let statement_size = 100 * 1024; // 100KB per statement
		let mut expected_hashes = Vec::new();
		for i in 0..num_statements {
			let mut statement = new_live_statement();
			let mut data = vec![0u8; statement_size];
			// Use multiple bytes for uniqueness since we have >255 statements
			data[0] = (i % 256) as u8;
			data[1] = (i / 256) as u8;
			statement.set_plain_data(data);
			let hash = statement.hash();
			expected_hashes.push(hash);
			statement_store.insert(statement);
		}

		// Setup peer and simulate connection
		let peer_id = PeerId::random();

		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: Some(format!("/{STATEMENT_PROTOCOL_V1}").into()),
			})
			.await;

		// Verify peer was added and initial sync was queued
		assert!(handler.peers.contains_key(&peer_id));
		assert!(handler.pending_initial_syncs.contains_key(&peer_id));
		assert_eq!(handler.initial_sync_peer_queue.len(), 1);

		// Process bursts until all statements are sent
		let mut burst_count = 0;
		while handler.pending_initial_syncs.contains_key(&peer_id) {
			handler.process_initial_sync_burst();
			handler.flush_pending_sends().await;
			burst_count += 1;
			// Safety limit
			assert!(burst_count <= 300, "Too many bursts, possible infinite loop");
		}

		// Verify multiple bursts were needed
		// With 200 statements x 100KB each and ~1MB per batch, we expect many bursts
		assert!(
			burst_count >= 10,
			"Expected multiple bursts for 200 statements of 100KB each, got {}",
			burst_count
		);

		// Verify all statements were sent
		let sent = notification_service.get_sent_notifications();
		let mut sent_hashes: Vec<_> = sent
			.iter()
			.flat_map(|(peer, notification)| {
				assert_eq!(*peer, peer_id);
				<Statements as Decode>::decode(&mut notification.as_slice()).unwrap()
			})
			.map(|s| s.hash())
			.collect();
		sent_hashes.sort();
		expected_hashes.sort();

		assert_eq!(
			sent_hashes.len(),
			expected_hashes.len(),
			"Expected {} statements to be sent, got {}",
			expected_hashes.len(),
			sent_hashes.len()
		);
		assert_eq!(sent_hashes, expected_hashes, "All statements should be sent");

		// Verify cleanup
		assert!(!handler.pending_initial_syncs.contains_key(&peer_id));
		assert!(handler.initial_sync_peer_queue.is_empty());
	}

	#[tokio::test]
	async fn initial_sync_network_error_leaves_the_sync_to_retry() {
		let (mut handler, statement_store, _, notification_service, _, peer_ids) = build_handler(1);
		let peer_id = peer_ids[0];
		handler.metrics = Some(Metrics::register(&Registry::new()).unwrap());

		// Two statements small enough to share one chunk, so a statement count cannot be
		// mistaken for a chunk count.
		let mut hashes: Vec<_> = [b"initial-sync-one".to_vec(), b"initial-sync-two".to_vec()]
			.into_iter()
			.map(|payload| {
				let mut statement = new_live_statement();
				statement.set_plain_data(payload);
				let hash = statement.hash();
				statement_store.insert(statement);
				hash
			})
			.collect();

		handler.schedule_initial_sync_for_peer(peer_id);
		assert!(handler.pending_initial_syncs.contains_key(&peer_id));

		// The network layer rejects the send with a real error, not a timeout.
		notification_service.fail_sends();
		handler.process_initial_sync_burst();
		handler.flush_pending_sends().await;

		assert!(notification_service.get_sent_notifications().is_empty());
		let pending = handler.pending_initial_syncs.get(&peer_id).unwrap();
		assert_eq!(pending.cursor, 0, "an unconfirmed chunk must not advance the cursor");
		assert_eq!(
			handler.initial_sync_peer_queue.iter().filter(|p| **p == peer_id).count(),
			1,
			"the failed send must requeue the peer"
		);

		let metrics = handler.metrics.as_ref().unwrap();
		assert_eq!(metrics.send_failures.with_label_values(&[send_failure::NETWORK]).get(), 1,);
		assert_eq!(
			metrics.undelivered_statements.with_label_values(&[send_failure::NETWORK]).get(),
			0,
			"a retried sync chunk is not abandoned, so its statements are not undelivered"
		);
		assert_eq!(
			metrics.send_failures.with_label_values(&[send_failure::TIMEOUT]).get(),
			0,
			"a network error must not be attributed to a timeout"
		);

		// The network recovers: the retried chunk delivers the same statements.
		notification_service.allow_sends();
		handler.process_initial_sync_burst();
		handler.flush_pending_sends().await;

		let mut sent = get_peer_hashes(&notification_service.get_sent_notifications(), peer_id);
		sent.sort();
		hashes.sort();
		assert_eq!(sent, hashes);

		handler.process_initial_sync_burst();
		assert!(!handler.pending_initial_syncs.contains_key(&peer_id));
	}

	#[tokio::test]
	async fn rescheduling_a_sync_drops_the_propagation_outbox() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		let mut statement = new_live_statement();
		statement.set_plain_data(b"queued before re-sync".to_vec());
		let hash = statement.hash();
		statement_store.insert(statement);
		handler.propagation_outboxes.insert(peer_id, VecDeque::from(vec![hash]));

		handler.schedule_initial_sync_for_peer(peer_id);

		assert!(
			!handler.propagation_outboxes.contains_key(&peer_id),
			"hashes queued before the sync are covered by its cursor"
		);

		// The sync cursor remains the only path, so the statement arrives exactly once.
		handler.process_initial_sync_burst();
		handler.flush_pending_sends().await;
		let sent = get_peer_hashes(&notification_service.get_sent_notifications(), peer_id);
		assert_eq!(sent, vec![hash]);
	}

	#[tokio::test]
	async fn missing_sink_leaves_the_initial_sync_to_retry() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		let mut statement = new_live_statement();
		statement.set_plain_data(b"initial-sync statement".to_vec());
		let hash = statement.hash();
		statement_store.insert(statement);

		handler.schedule_initial_sync_for_peer(peer_id);

		// No sink is available for the first burst.
		notification_service.serve_sinks(0);
		handler.process_initial_sync_burst();

		assert!(handler.pending_sends.is_empty());
		assert_eq!(
			handler.pending_initial_syncs.get(&peer_id).unwrap().cursor,
			0,
			"a chunk that found no sink must not advance the cursor"
		);
		assert_eq!(
			handler.initial_sync_peer_queue.iter().filter(|p| **p == peer_id).count(),
			1,
			"the peer must be requeued for a retry"
		);

		// The sink comes back: the retried chunk delivers.
		notification_service.serve_sinks(usize::MAX);
		handler.process_initial_sync_burst();
		handler.flush_pending_sends().await;

		let sent = get_peer_hashes(&notification_service.get_sent_notifications(), peer_id);
		assert_eq!(sent, vec![hash]);
	}

	#[tokio::test]
	async fn failed_store_fetch_retains_the_sync_and_a_later_burst_retries() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		let mut statement = new_live_statement();
		statement.set_plain_data(b"initial-sync statement".to_vec());
		let hash = statement.hash();
		statement_store.insert(statement);

		handler.schedule_initial_sync_for_peer(peer_id);
		let watermark = handler.pending_initial_syncs.get(&peer_id).unwrap().watermark;

		statement_store.fail_fetches.store(true, Ordering::Relaxed);
		handler.process_initial_sync_burst();

		assert!(handler.pending_sends.is_empty(), "a failed fetch must not queue a send");
		let pending = handler.pending_initial_syncs.get(&peer_id).unwrap();
		assert_eq!(pending.cursor, 0, "the cursor must stay at the failed position");
		assert_eq!(pending.watermark, watermark);
		assert_eq!(
			handler.initial_sync_peer_queue.iter().filter(|p| **p == peer_id).count(),
			1,
			"the peer must be requeued for a retry"
		);

		// The store recovers: the next burst resumes from the retained cursor.
		statement_store.fail_fetches.store(false, Ordering::Relaxed);
		handler.process_initial_sync_burst();
		handler.flush_pending_sends().await;

		let sent = get_peer_hashes(&notification_service.get_sent_notifications(), peer_id);
		assert_eq!(sent, vec![hash]);

		// The completed send requeued the peer; the next burst observes the finished sync.
		handler.process_initial_sync_burst();
		assert!(!handler.pending_initial_syncs.contains_key(&peer_id));
	}

	#[tokio::test]
	async fn missing_sink_counts_every_abandoned_statement() {
		let (mut handler, statement_store, _, notification_service, _, peer_ids) = build_handler(1);
		let peer_id = peer_ids[0];
		handler.metrics = Some(Metrics::register(&Registry::new()).unwrap());

		// 100 KB each, so the batch spans three 1 MiB chunks. Two chunks must remain after the
		// sink disappears, otherwise "the whole remainder" and "the current chunk" are the same
		// number and the test proves nothing.
		let total = 25;
		for i in 0..total {
			let mut statement = new_live_statement();
			let mut data = vec![0u8; 100 * 1024];
			data[0] = i as u8;
			statement.set_plain_data(data);
			statement_store
				.recent_statements
				.lock()
				.unwrap()
				.insert(statement.hash(), statement);
		}

		// Serve the first chunk, then behave as if the peer went away.
		notification_service.serve_sinks(1);
		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let delivered =
			get_peer_hashes(&notification_service.get_sent_notifications(), peer_id).len();
		assert!(delivered > 0);
		assert!(
			total - delivered > delivered,
			"the abandoned remainder must exceed one full chunk, delivered {delivered} of {total}"
		);

		let metrics = handler.metrics.as_ref().unwrap();
		assert_eq!(metrics.send_failures.with_label_values(&[send_failure::NO_SINK]).get(), 1,);
		assert_eq!(
			metrics.undelivered_statements.with_label_values(&[send_failure::NO_SINK]).get(),
			(total - delivered) as u64,
			"every statement after the delivered chunk counts as undelivered"
		);
	}

	#[tokio::test]
	async fn burst_with_nothing_to_send_returns_the_peer_to_the_queue() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		let mut statement = new_live_statement();
		statement.set_plain_data(b"filtered by affinity".to_vec());
		statement.set_topic(0, [0xAA; 32].into());
		statement_store.insert(statement);

		// A topic affinity matching nothing in the store, so the burst finds no
		// statement to send.
		let mut filter = AffinityFilter::new(BLOOM_SEED, 0.01, 100);
		filter.insert(&[0xBB; 32]);
		{
			let peer = handler.peers.get_mut(&peer_id).unwrap();
			peer.protocol_version = PeerProtocolVersion::V2;
			peer.topic_affinity = Some(filter);
		}
		handler.schedule_initial_sync_for_peer(peer_id);

		// No chunk means no send result to advance the sync, so the burst has to requeue the peer
		// itself or the sync stalls with its entry alive and the peer out of the queue.
		handler.process_initial_sync_burst();
		assert!(handler.pending_sends.is_empty());
		assert!(notification_service.get_sent_notifications().is_empty());
		assert_eq!(handler.initial_sync_peer_queue.len(), 1);

		handler.process_initial_sync_burst();
		assert!(!handler.pending_initial_syncs.contains_key(&peer_id));
		assert!(handler.initial_sync_peer_queue.is_empty());
	}

	#[tokio::test]
	async fn initial_sync_keeps_one_chunk_per_peer_in_flight() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		// 100 KB each, so the store spans several 1 MiB chunks and the peer has more to receive
		// after its first one.
		for i in 0..25u8 {
			let mut statement = new_live_statement();
			let mut data = vec![0u8; 100 * 1024];
			data[0] = i;
			statement.set_plain_data(data);
			statement_store.insert(statement);
		}

		// The peer never reads its substream.
		notification_service.block_sends();
		handler.schedule_initial_sync_for_peer(peer_id);

		for _ in 0..10 {
			handler.process_initial_sync_burst();
		}

		assert_eq!(handler.pending_sends.len(), 1);
		assert!(handler.initial_sync_peer_queue.is_empty());
	}

	#[tokio::test]
	async fn superseded_initial_sync_result_is_discarded() {
		let (mut handler, statement_store, _network, _notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		let mut statement = new_live_statement();
		statement.set_plain_data(b"superseded".to_vec());
		statement_store.insert(statement);

		handler.schedule_initial_sync_for_peer(peer_id);
		handler.process_initial_sync_burst();
		assert_eq!(handler.pending_sends.len(), 1);
		assert!(handler.initial_sync_in_flight_bytes > 0);

		// An affinity change re-schedules the sync while the chunk is still in flight.
		handler.schedule_initial_sync_for_peer(peer_id);
		handler.flush_pending_sends().await;

		assert_eq!(
			handler.initial_sync_peer_queue.iter().filter(|peer| **peer == peer_id).count(),
			1
		);
		assert_eq!(handler.initial_sync_in_flight_bytes, 0);
	}

	#[tokio::test]
	async fn failed_superseded_initial_sync_result_does_not_abort_the_new_sync() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		let mut statement = new_live_statement();
		statement.set_plain_data(b"superseded failure".to_vec());
		statement_store.insert(statement);

		// Queue a chunk, then arrange for that very send to come back as a failure.
		handler.schedule_initial_sync_for_peer(peer_id);
		handler.process_initial_sync_burst();
		notification_service.fail_sends();

		// A reconnect replaces the sync while that chunk is still in flight.
		handler.schedule_initial_sync_for_peer(peer_id);
		let sync_id = handler.pending_initial_syncs.get(&peer_id).unwrap().sync_id;
		handler.flush_pending_sends().await;

		assert_eq!(
			handler.pending_initial_syncs.get(&peer_id).map(|pending| pending.sync_id),
			Some(sync_id)
		);
		assert_eq!(
			handler.initial_sync_peer_queue.iter().filter(|peer| **peer == peer_id).count(),
			1
		);
	}

	#[tokio::test]
	async fn initial_sync_respects_the_payload_size_boundary() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		handler.metrics = Some(Metrics::register(&Registry::new()).unwrap());
		let peer_id = peer_ids[0];
		let overhead = handler.peers.get(&peer_id).unwrap().protocol_version.envelope_overhead();
		let max_size = max_statement_payload_size(overhead);

		let mut data_len = max_size - 32;
		let exact = loop {
			let mut candidate = new_live_statement();
			candidate.set_plain_data(vec![7u8; data_len]);
			let size = candidate.encoded_size();
			assert!(size <= max_size, "no data length encodes to exactly {max_size}");
			if size == max_size {
				break candidate;
			}
			data_len += 1;
		};
		let exact_hash = exact.hash();
		let mut oversized = new_live_statement();
		oversized.set_plain_data(vec![2u8; MAX_STATEMENT_NOTIFICATION_SIZE as usize]);
		statement_store.insert(exact);
		statement_store.insert(oversized);

		handler.schedule_initial_sync_for_peer(peer_id);
		for _ in 0..10 {
			if !handler.pending_initial_syncs.contains_key(&peer_id) {
				break;
			}
			handler.process_initial_sync_burst();
			handler.flush_pending_sends().await;
		}

		assert!(!handler.pending_initial_syncs.contains_key(&peer_id));
		assert_eq!(handler.metrics.as_ref().unwrap().skipped_oversized_statements.get(), 1);
		let sent = get_peer_hashes(&notification_service.get_sent_notifications(), peer_id);
		assert_eq!(sent, vec![exact_hash], "only the exactly-fitting statement is delivered");
	}

	#[tokio::test]
	async fn initial_sync_in_flight_budget_is_bounded() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(20);

		// ~1.1 MB of statements, so each peer's first chunk sits just under the 1 MiB cap and a
		// handful of peers is enough to exhaust the budget.
		for i in 0..11u8 {
			let mut statement = new_live_statement();
			let mut data = vec![0u8; 100 * 1024];
			data[0] = i;
			statement.set_plain_data(data);
			statement_store.insert(statement);
		}

		// No peer reads, so nothing ever leaves the budget.
		notification_service.block_sends();
		for peer_id in &peer_ids {
			handler.schedule_initial_sync_for_peer(*peer_id);
		}

		let mut bursts = 0;
		while handler.initial_sync_in_flight_bytes < MAX_SEND_IN_FLIGHT_BYTES {
			handler.process_initial_sync_burst();
			bursts += 1;
			assert!(bursts <= 100, "the budget was never reached after {bursts} bursts");
		}

		let in_flight = handler.initial_sync_in_flight_bytes;
		assert!(in_flight >= MAX_SEND_IN_FLIGHT_BYTES);
		assert!(
			in_flight < MAX_SEND_IN_FLIGHT_BYTES + MAX_STATEMENT_NOTIFICATION_SIZE,
			"the budget may only be overshot by the single chunk that crossed it, got {in_flight}"
		);

		// A throttled burst must leave the round-robin untouched rather than burn a peer's turn.
		let queued = handler.initial_sync_peer_queue.clone();
		handler.process_initial_sync_burst();
		assert_eq!(handler.initial_sync_peer_queue, queued);
		assert_eq!(handler.initial_sync_in_flight_bytes, in_flight);
	}

	#[tokio::test]
	async fn saturated_send_budget_defers_propagation() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		let mut statement = new_live_statement();
		statement.set_plain_data(b"deferred by budget".to_vec());
		let hash = statement.hash();
		statement_store.recent_statements.lock().unwrap().insert(hash, statement);

		// The whole budget is taken by initial-sync bytes.
		handler.initial_sync_in_flight_bytes = MAX_SEND_IN_FLIGHT_BYTES;
		handler.propagate_statements().await;

		assert!(handler.pending_sends.is_empty(), "no chunk may be queued over the budget");
		assert_eq!(
			handler.propagation_outboxes.get(&peer_id).unwrap(),
			&VecDeque::from(vec![hash])
		);
		assert_eq!(handler.parked_propagations, VecDeque::from([peer_id]));

		// A completed initial-sync send frees the budget and refills the parked peer.
		handler.handle_send_result(PendingSendResult {
			peer: peer_id,
			statement_count: 1,
			bytes_sent: MAX_SEND_IN_FLIGHT_BYTES,
			result: SendOutcome::Sent,
			kind: SendKind::InitialSync { sync_id: 0, next_cursor: 0 },
			chunk_id: 0,
		});
		handler.flush_pending_sends().await;

		let sent = get_peer_hashes(&notification_service.get_sent_notifications(), peer_id);
		assert_eq!(sent, vec![hash]);
		assert!(handler.parked_propagations.is_empty());
	}

	#[tokio::test]
	async fn parked_peers_are_refilled_in_parking_order() {
		let (mut handler, statement_store, _network, notification_service, _, _) = build_handler(2);

		// One chunk is ~900 KB, so freeing a few bytes admits exactly one of the
		// two parked peers into the 16 MiB budget.
		let mut statement = new_live_statement();
		statement.set_plain_data(vec![7u8; 900 * 1024]);
		let hash = statement.hash();
		statement_store.recent_statements.lock().unwrap().insert(hash, statement);

		handler.initial_sync_in_flight_bytes = MAX_SEND_IN_FLIGHT_BYTES;
		handler.propagate_statements().await;
		assert_eq!(handler.parked_propagations.len(), 2);
		let first = handler.parked_propagations[0];
		let second = handler.parked_propagations[1];

		// Freeing a sliver of budget admits only the first parked peer.
		handler.handle_send_result(PendingSendResult {
			peer: first,
			statement_count: 1,
			bytes_sent: 100,
			result: SendOutcome::Sent,
			kind: SendKind::InitialSync { sync_id: 0, next_cursor: 0 },
			chunk_id: 0,
		});
		assert_eq!(handler.pending_sends.len(), 1);
		assert_eq!(handler.parked_propagations, VecDeque::from([second]));

		// The first chunk's completion frees enough for the second peer.
		handler.flush_pending_sends().await;
		let sent = notification_service.get_sent_notifications();
		assert_eq!(
			sent.iter().map(|(peer, _)| *peer).collect::<Vec<_>>(),
			vec![first, second],
			"peers must be served in parking order"
		);
		assert!(handler.parked_propagations.is_empty());
		assert_eq!(handler.propagation_in_flight_bytes, 0);
	}

	#[tokio::test]
	async fn pending_initial_sync_reserves_send_budget_from_propagation() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(2);
		let propagation_peer = peer_ids[0];
		let sync_peer = peer_ids[1];

		let mut statement = new_live_statement();
		statement.set_plain_data(b"backlog".to_vec());
		let hash = statement.hash();
		statement_store.insert(statement);
		handler.schedule_initial_sync_for_peer(sync_peer);
		assert!(handler.pending_initial_syncs.contains_key(&sync_peer));

		// Propagation holds the whole budget and one peer waits parked.
		handler.propagation_in_flight_bytes = MAX_SEND_IN_FLIGHT_BYTES;
		handler
			.propagation_outboxes
			.insert(propagation_peer, VecDeque::from(vec![hash]));
		handler.parked_propagations.push_back(propagation_peer);

		// A completed send frees exactly the reserve: the parked peer must stay parked,
		// keeping the headroom for the sync burst.
		handler.handle_send_result(PendingSendResult {
			peer: propagation_peer,
			statement_count: 1,
			bytes_sent: INITIAL_SYNC_RESERVED_BYTES,
			result: SendOutcome::Sent,
			kind: SendKind::Propagation,
			chunk_id: 0,
		});
		assert!(handler.pending_sends.is_empty(), "the reserve must not refill propagation");
		assert_eq!(handler.parked_propagations, VecDeque::from([propagation_peer]));

		// The sync burst finds the reserved headroom and proceeds.
		handler.process_initial_sync_burst();
		assert_eq!(handler.pending_sends.len(), 1, "the sync burst must use the reserve");
		handler.flush_pending_sends().await;
		let synced = get_peer_hashes(&notification_service.get_sent_notifications(), sync_peer);
		assert_eq!(synced, vec![hash]);
		assert_eq!(
			handler.parked_propagations,
			VecDeque::from([propagation_peer]),
			"the reserve holds while the sync is still pending"
		);

		// The next burst observes the drained sync and completes it, releasing the
		// reserve back to propagation.
		handler.process_initial_sync_burst();
		assert!(handler.pending_initial_syncs.is_empty());
		handler.fill_parked_propagations();
		assert!(handler.parked_propagations.is_empty());
		handler.flush_pending_sends().await;
		let propagated =
			get_peer_hashes(&notification_service.get_sent_notifications(), propagation_peer);
		assert_eq!(propagated, vec![hash]);
	}

	#[tokio::test]
	async fn burst_skips_a_peer_with_a_busy_send_slot() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(2);

		let mut statement = new_live_statement();
		statement.set_plain_data(b"burst behind a busy slot".to_vec());
		statement_store.insert(statement);

		handler.schedule_initial_sync_for_peer(peer_ids[0]);
		handler.schedule_initial_sync_for_peer(peer_ids[1]);
		// The first queued peer's send slot is taken by a propagation chunk.
		handler.in_flight_chunks.insert(peer_ids[0], 42);

		handler.process_initial_sync_burst();
		handler.flush_pending_sends().await;

		// The same burst serves the next queued peer instead of returning.
		let sent = notification_service.get_sent_notifications();
		assert_eq!(sent.len(), 1);
		assert_eq!(sent[0].0, peer_ids[1]);
		// The busy peer keeps its sync and its turn.
		assert!(handler.pending_initial_syncs.contains_key(&peer_ids[0]));
		assert!(handler.initial_sync_peer_queue.contains(&peer_ids[0]));
	}

	#[tokio::test]
	async fn peer_with_both_kinds_pending_sends_propagation_first() {
		let (mut handler, statement_store, _network, notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		// An initial-sync chunk takes the slot, then a fresh statement arrives by
		// tick while the slot is busy.
		let mut synced = new_live_statement();
		synced.set_plain_data(b"snapshot statement".to_vec());
		let synced_hash = synced.hash();
		statement_store.insert(synced);
		handler.schedule_initial_sync_for_peer(peer_id);
		handler.process_initial_sync_burst();
		assert!(handler.in_flight_chunks.contains_key(&peer_id));

		let mut fresh = new_live_statement();
		fresh.set_plain_data(b"fresh gossip".to_vec());
		let fresh_hash = fresh.hash();
		statement_store.recent_statements.lock().unwrap().insert(fresh_hash, fresh);
		handler.propagate_statements().await;
		assert_eq!(handler.pending_sends.len(), 1, "the propagation chunk must wait in the outbox");

		// The initial-sync completion frees the slot and propagation claims it
		// before the next burst tick gets a chance.
		let result = handler.pending_sends.next().await.unwrap();
		handler.handle_send_result(result);
		assert_eq!(handler.pending_sends.len(), 1);
		assert!(handler.in_flight_chunks.contains_key(&peer_id));
		handler.process_initial_sync_burst();
		assert_eq!(handler.pending_sends.len(), 1, "a burst must not bypass the busy slot");

		handler.flush_pending_sends().await;
		let sent = get_peer_hashes(&notification_service.get_sent_notifications(), peer_id);
		assert_eq!(sent, vec![synced_hash, fresh_hash]);
	}

	#[tokio::test]
	async fn initial_sync_and_propagation_share_the_budget() {
		let (mut handler, statement_store, _network, _notification_service, _, peer_ids) =
			build_handler(1);
		let peer_id = peer_ids[0];

		let mut statement = new_live_statement();
		statement.set_plain_data(b"shared budget".to_vec());
		statement_store.insert(statement);
		handler.schedule_initial_sync_for_peer(peer_id);

		// Propagation bytes alone exhaust the shared budget, so the burst must wait.
		handler.propagation_in_flight_bytes = MAX_SEND_IN_FLIGHT_BYTES;
		let queued = handler.initial_sync_peer_queue.clone();
		handler.process_initial_sync_burst();

		assert!(handler.pending_sends.is_empty());
		assert_eq!(
			handler.initial_sync_peer_queue, queued,
			"a throttled burst must not burn the peer's turn"
		);
	}

	#[tokio::test]
	async fn test_initial_sync_burst_multiple_peers_round_robin() {
		let (mut handler, statement_store, _network, notification_service, _, _) = build_handler(0);

		// Create 20MB of statements (200 statements x 100KB each)
		let num_statements = 200;
		let statement_size = 100 * 1024; // 100KB per statement
		let mut expected_hashes = Vec::new();
		for i in 0..num_statements {
			let mut statement = new_live_statement();
			let mut data = vec![0u8; statement_size];
			data[0] = (i % 256) as u8;
			data[1] = (i / 256) as u8;
			statement.set_plain_data(data);
			let hash = statement.hash();
			expected_hashes.push(hash);
			statement_store.insert(statement);
		}

		// Setup 3 peers and simulate connections
		let peer1 = PeerId::random();
		let peer2 = PeerId::random();
		let peer3 = PeerId::random();

		// Connect peers
		for peer in [peer1, peer2, peer3] {
			handler
				.handle_notification_event(NotificationEvent::NotificationStreamOpened {
					peer,
					direction: sc_network::service::traits::Direction::Inbound,
					handshake: vec![],
					negotiated_fallback: Some(format!("/{STATEMENT_PROTOCOL_V1}").into()),
				})
				.await;
		}

		// Verify all peers were added and initial syncs were queued
		assert_eq!(handler.peers.len(), 3);
		assert_eq!(handler.pending_initial_syncs.len(), 3);
		assert_eq!(handler.initial_sync_peer_queue.len(), 3);

		// Track which peer was processed on each burst for round-robin verification
		let mut peer_burst_order = Vec::new();
		let mut burst_count = 0;

		while !handler.pending_initial_syncs.is_empty() {
			// Record which peer will be processed next
			if let Some(&next_peer) = handler.initial_sync_peer_queue.front() {
				peer_burst_order.push(next_peer);
			}
			handler.process_initial_sync_burst();
			handler.flush_pending_sends().await;
			burst_count += 1;
			// Safety limit
			assert!(burst_count <= 500, "Too many bursts, possible infinite loop");
		}

		// Verify multiple bursts were needed
		// With 3 peers and many bursts per peer, we expect many bursts total
		assert!(
			burst_count >= 30,
			"Expected many bursts for 3 peers with 200 statements each, got {}",
			burst_count
		);

		// Verify round-robin pattern in first 9 bursts (3 peers x 3 rounds)
		assert!(peer_burst_order.len() >= 9, "Expected at least 9 bursts");
		// First round
		assert_eq!(peer_burst_order[0], peer1, "First burst should be peer1");
		assert_eq!(peer_burst_order[1], peer2, "Second burst should be peer2");
		assert_eq!(peer_burst_order[2], peer3, "Third burst should be peer3");
		// Second round
		assert_eq!(peer_burst_order[3], peer1, "Fourth burst should be peer1");
		assert_eq!(peer_burst_order[4], peer2, "Fifth burst should be peer2");
		assert_eq!(peer_burst_order[5], peer3, "Sixth burst should be peer3");

		// Verify all peers received all statements
		let sent = notification_service.get_sent_notifications();
		let mut peer1_hashes = get_peer_hashes(&sent, peer1);
		let mut peer2_hashes = get_peer_hashes(&sent, peer2);
		let mut peer3_hashes = get_peer_hashes(&sent, peer3);

		peer1_hashes.sort();
		peer2_hashes.sort();
		peer3_hashes.sort();
		expected_hashes.sort();

		assert_eq!(peer1_hashes, expected_hashes, "Peer1 should receive all statements");
		assert_eq!(peer2_hashes, expected_hashes, "Peer2 should receive all statements");
		assert_eq!(peer3_hashes, expected_hashes, "Peer3 should receive all statements");

		// Verify cleanup
		assert!(handler.pending_initial_syncs.is_empty());
		assert!(handler.initial_sync_peer_queue.is_empty());
	}

	#[tokio::test]
	async fn test_send_statements_in_chunks_exact_max_size() {
		let (mut handler, statement_store, _network, notification_service, _queue_receiver, _) =
			build_handler(1);

		// Calculate the data sizes so that 100 statements together exactly fill max_size.
		// This tests that all 100 statements fit in a single notification.
		//
		// The limit check in `fetch_statement_chunk` is:
		//   max_size = MAX_STATEMENT_NOTIFICATION_SIZE - Compact::<u32>::max_encoded_len()
		//
		// Statement encoding (encodes as Vec<Field>):
		// - Compact<u32> for number of fields (1 byte for value 2: expiry + data)
		// - Field::Expiry discriminant (1 byte, value 2)
		// - u64 expiry value (8 bytes)
		// - Field::Data discriminant (1 byte, value 8)
		// - Compact<u32> for the data length (2 bytes for small data)
		// So per-statement overhead = 1 + 1 + 8 + 1 + 2 = 13 bytes
		let max_size = MAX_STATEMENT_NOTIFICATION_SIZE as usize - Compact::<u32>::max_encoded_len();
		let num_statements: usize = 100;
		let per_statement_overhead = 1 + 1 + 8 + 1 + 2; // Vec<Field> length + expiry field + data discriminant + Compact data length
		let total_overhead = per_statement_overhead * num_statements;
		let total_data_size = max_size - total_overhead;
		let per_statement_data_size = total_data_size / num_statements;
		let remainder = total_data_size % num_statements;

		let mut expected_hashes = Vec::with_capacity(num_statements);
		let mut total_encoded_size = 0;

		for i in 0..num_statements {
			let mut statement = new_live_statement();
			// Distribute remainder across first `remainder` statements to exactly fill max_size
			let extra = if i < remainder { 1 } else { 0 };
			let mut data = vec![42u8; per_statement_data_size + extra];
			// Make each statement unique by modifying the first few bytes
			data[0] = i as u8;
			data[1] = (i >> 8) as u8;
			statement.set_plain_data(data);

			total_encoded_size += statement.encoded_size();

			let hash = statement.hash();
			expected_hashes.push(hash);
			statement_store.recent_statements.lock().unwrap().insert(hash, statement);
		}

		// Verify our calculation: total encoded size should be <= max_size
		assert!(
			total_encoded_size == max_size,
			"Total encoded size {} should be <= max_size {}",
			total_encoded_size,
			max_size
		);

		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let sent = notification_service.get_sent_notifications();

		// All statements should fit in a single chunk
		assert_eq!(
			sent.len(),
			1,
			"Expected 1 notification for all {} statements, but got {}",
			num_statements,
			sent.len()
		);

		let (_peer, notification) = &sent[0];
		assert!(
			notification.len() <= MAX_STATEMENT_NOTIFICATION_SIZE as usize,
			"Notification size {} exceeds limit {}",
			notification.len(),
			MAX_STATEMENT_NOTIFICATION_SIZE
		);

		let decoded = <Statements as Decode>::decode(&mut notification.as_slice()).unwrap();
		assert_eq!(
			decoded.len(),
			num_statements,
			"Expected {} statements in the notification",
			num_statements
		);

		// Verify all statements were sent (order may differ due to HashMap iteration)
		let mut received_hashes: Vec<_> = decoded.iter().map(|s| s.hash()).collect();
		expected_hashes.sort();
		received_hashes.sort();
		assert_eq!(expected_hashes, received_hashes, "All statement hashes should match");
	}

	#[tokio::test]
	async fn test_initial_sync_burst_size_limit_consistency() {
		// A filter measuring against the raw `MAX_STATEMENT_NOTIFICATION_SIZE` would admit
		// statements the encoder cannot fit, so both must use `max_statement_payload_size`.
		let (mut handler, statement_store, _network, notification_service, _, _) = build_handler(0);

		// This peer connects as V1 (see negotiated_fallback below).
		let payload_limit = max_statement_payload_size(V1_ENVELOPE_OVERHEAD);

		// Create first statement that's just over half the payload limit
		let first_stmt_data_size = payload_limit / 2 + 10;
		let mut stmt1 = new_live_statement();
		stmt1.set_plain_data(vec![1u8; first_stmt_data_size]);
		let stmt1_encoded_size = stmt1.encoded_size();

		// Create second statement that, combined with the first, exceeds the payload limit.
		// This means the filter will only accept the first statement.
		let remaining = payload_limit.saturating_sub(stmt1_encoded_size);
		let target_stmt2_encoded = remaining + 3; // 3 bytes over limit when combined
		let stmt2_data_size = target_stmt2_encoded.saturating_sub(4); // ~4 bytes encoding overhead
		let mut stmt2 = new_live_statement();
		stmt2.set_plain_data(vec![2u8; stmt2_data_size]);
		let stmt2_encoded_size = stmt2.encoded_size();

		let total_encoded = stmt1_encoded_size + stmt2_encoded_size;

		// Verify our setup: total exceeds payload limit
		assert!(
			total_encoded > payload_limit,
			"Total {} should exceed payload_limit {} so filter rejects second statement",
			total_encoded,
			payload_limit
		);

		let hash1 = stmt1.hash();
		let hash2 = stmt2.hash();
		statement_store.insert(stmt1);
		statement_store.insert(stmt2);

		// Setup peer and simulate connection
		let peer_id = PeerId::random();

		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: Some(format!("/{STATEMENT_PROTOCOL_V1}").into()),
			})
			.await;

		// Verify initial sync was queued covering both statements
		assert!(handler.pending_initial_syncs.contains_key(&peer_id));
		let pending = handler.pending_initial_syncs.get(&peer_id).unwrap();
		assert_eq!(pending.watermark - pending.cursor, 2);

		// Process first burst - should send only one statement (the other doesn't fit)
		handler.process_initial_sync_burst();
		handler.flush_pending_sends().await;

		// The filter and the chunk bound agree, so only the statement that fits is
		// fetched and sent.
		let sent = notification_service.get_sent_notifications();
		assert_eq!(sent.len(), 1, "First burst should send one notification");

		let decoded = <Statements as Decode>::decode(&mut sent[0].1.as_slice()).unwrap();
		assert_eq!(decoded.len(), 1, "First notification should contain one statement");

		// Verify one of the two statements was sent (order is non-deterministic due to HashMap)
		let sent_hash = decoded[0].hash();
		assert!(
			sent_hash == hash1 || sent_hash == hash2,
			"Sent statement should be one of the two created"
		);

		// Second statement should still be pending
		assert!(handler.pending_initial_syncs.contains_key(&peer_id));
		let pending = handler.pending_initial_syncs.get(&peer_id).unwrap();
		assert_eq!(pending.watermark - pending.cursor, 1);

		// Process second burst - should send the remaining statement
		handler.process_initial_sync_burst();
		handler.flush_pending_sends().await;

		let sent = notification_service.get_sent_notifications();
		assert_eq!(sent.len(), 2, "Second burst should send another notification");

		// Both statements should now be sent
		let mut sent_hashes: Vec<_> = sent
			.iter()
			.flat_map(|(_, notification)| {
				<Statements as Decode>::decode(&mut notification.as_slice()).unwrap()
			})
			.map(|s| s.hash())
			.collect();
		sent_hashes.sort();
		let mut expected_hashes = vec![hash1, hash2];
		expected_hashes.sort();
		assert_eq!(sent_hashes, expected_hashes, "Both statements should be sent");

		// The sync is closed by the burst that finds nothing left to send, not by the one that sent
		// the last chunk.
		handler.process_initial_sync_burst();
		assert!(!handler.pending_initial_syncs.contains_key(&peer_id));
	}

	#[tokio::test]
	async fn test_peer_disconnected_on_flooding() {
		let (mut handler, _statement_store, network, _notification_service, _queue_receiver, _) =
			build_handler(1);

		let peer_id = *handler.peers.keys().next().unwrap();

		let mut flood_statements = Vec::new();
		for i in 0..600_000 {
			let mut statement = new_live_statement();
			statement.set_plain_data(vec![i as u8, (i >> 8) as u8, (i >> 16) as u8]);
			flood_statements.push(statement);
		}

		handler.on_statements(peer_id, flood_statements);

		let reports = network.get_reports();
		assert!(
			reports
				.iter()
				.any(|(id, rep)| *id == peer_id && *rep == rep::STATEMENT_FLOODING),
			"Expected STATEMENT_FLOODING reputation change, but got: {:?}",
			reports
		);

		let disconnected = network.get_disconnected_peers();
		assert!(
			disconnected.contains(&peer_id),
			"Expected peer {} to be disconnected, but it wasn't. Disconnected peers: {:?}",
			peer_id,
			disconnected
		);

		dispatch_disconnects(&mut handler, &network).await;

		// Verify peer state was cleaned up
		assert!(!handler.peers.contains_key(&peer_id), "Peer should be removed from peers map");
		assert!(
			!handler.pending_initial_syncs.contains_key(&peer_id),
			"Peer should be removed from pending_initial_syncs"
		);
		assert!(
			!handler.initial_sync_peer_queue.contains(&peer_id),
			"Peer should be removed from initial_sync_peer_queue"
		);
	}

	#[tokio::test]
	async fn test_legitimate_traffic_not_flagged() {
		let (
			mut handler,
			_statement_store,
			network,
			_notification_service,
			_queue_receiver,
			_,
			clock,
		) = build_handler_with_fake_clock(1);

		let peer_id = *handler.peers.keys().next().unwrap();
		let mut counter = 0u32;

		// 100 steps of 100ms is 10 simulated seconds: twice the 5s the full burst takes to drain,
		// so any drift in the accounting would have tripped by the end.
		for _ in 0..100 {
			handler.on_statements(peer_id, statement_batch(5_000, &mut counter));
			clock.advance(Duration::from_millis(100));
		}

		let reports = network.get_reports();
		assert!(
			!reports
				.iter()
				.any(|(id, rep)| *id == peer_id && *rep == rep::STATEMENT_FLOODING),
			"Legitimate traffic should not trigger flooding detection. Reports: {:?}",
			reports
		);

		let disconnected = network.get_disconnected_peers();
		assert!(
			!disconnected.contains(&peer_id),
			"Legitimate traffic should not cause disconnection. Disconnected peers: {:?}",
			disconnected
		);

		assert!(handler.peers.contains_key(&peer_id), "Peer should still be connected");
	}

	#[tokio::test]
	async fn test_just_over_rate_limit_triggers_flooding() {
		let (mut handler, _statement_store, network, _notification_service, _queue_receiver, _) =
			build_handler(1);

		let peer_id = *handler.peers.keys().next().unwrap();

		let mut statements = Vec::new();
		for i in 0..260_000 {
			let mut statement = new_live_statement();
			statement.set_plain_data(vec![
				i as u8,
				(i >> 8) as u8,
				(i >> 16) as u8,
				(i >> 24) as u8,
			]);
			statements.push(statement);
		}

		handler.on_statements(peer_id, statements);

		let reports = network.get_reports();
		let expected_burst = DEFAULT_STATEMENTS_PER_SECOND * config::STATEMENTS_BURST_COEFFICIENT;
		assert!(
			reports
				.iter()
				.any(|(id, rep)| *id == peer_id && *rep == rep::STATEMENT_FLOODING),
			"Sending 260,000 statements should trigger flooding (burst limit: {}). Reports: {:?}",
			expected_burst,
			reports
		);

		let disconnected = network.get_disconnected_peers();
		assert!(
			disconnected.contains(&peer_id),
			"Peer should be disconnected after exceeding rate limit. Disconnected: {:?}",
			disconnected
		);

		dispatch_disconnects(&mut handler, &network).await;

		assert!(!handler.peers.contains_key(&peer_id), "Peer should be removed from peers map");
	}

	#[tokio::test]
	async fn test_burst_of_250k_statements_allowed() {
		let (mut handler, _statement_store, network, _notification_service, _queue_receiver, _) =
			build_handler(1);

		let peer_id = *handler.peers.keys().next().unwrap();

		let mut statements = Vec::new();
		for i in 0..250_000 {
			let mut statement = new_live_statement();
			statement.set_plain_data(vec![
				i as u8,
				(i >> 8) as u8,
				(i >> 16) as u8,
				(i >> 24) as u8,
			]);
			statements.push(statement);
		}

		handler.on_statements(peer_id, statements);

		let reports = network.get_reports();
		assert!(
			!reports
				.iter()
				.any(|(id, rep)| *id == peer_id && *rep == rep::STATEMENT_FLOODING),
			"250k burst should be allowed (burst = rate × 5). Reports: {:?}",
			reports
		);

		assert!(
			handler.peers.contains_key(&peer_id),
			"Peer should still be connected after 250k burst"
		);
	}

	/// Like [`build_handler`], but every peer's rate limiter runs on the returned clock instead of
	/// on wall-clock time, so rate-limit behaviour spanning several batches is exact. The quota is
	/// the production one.
	fn build_handler_with_fake_clock(
		num_peers: usize,
	) -> (
		StatementHandler<TestNetwork, TestSync>,
		TestStatementStore,
		TestNetwork,
		TestNotificationService,
		async_channel::Receiver<(Statement, oneshot::Sender<SubmitResult>)>,
		Vec<PeerId>,
		FakeRelativeClock,
	) {
		let (mut handler, statement_store, network, notification_service, queue_receiver, peer_ids) =
			build_handler(num_peers);

		let clock = FakeRelativeClock::default();
		for peer in handler.peers.values_mut() {
			peer.rate_limiter =
				PeerRateLimiter::with_clock(statements_per_second(), burst(), &clock);
		}

		(handler, statement_store, network, notification_service, queue_receiver, peer_ids, clock)
	}

	/// The production quota the handler is built with: 50k statements/sec, 250k burst.
	fn statements_per_second() -> NonZeroU32 {
		NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND)
			.expect("DEFAULT_STATEMENTS_PER_SECOND is nonzero")
	}

	fn burst() -> NonZeroU32 {
		NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND * STATEMENTS_BURST_COEFFICIENT)
			.expect("burst capacity is nonzero")
	}

	fn statement_batch(count: u32, counter: &mut u32) -> Statements {
		(0..count)
			.map(|i| {
				let mut statement = Statement::new();
				statement.set_plain_data(vec![
					*counter as u8,
					(*counter >> 8) as u8,
					(*counter >> 16) as u8,
					i as u8,
				]);
				*counter = counter.wrapping_add(1);
				statement
			})
			.collect()
	}

	/// A rate sustained above the quota must drain the bucket across successive batches and
	/// eventually get the peer reported and disconnected.
	#[tokio::test]
	async fn test_sustained_rate_above_limit_triggers_flooding() {
		let (
			mut handler,
			_statement_store,
			network,
			_notification_service,
			_queue_receiver,
			_,
			clock,
		) = build_handler_with_fake_clock(1);

		let peer_id = *handler.peers.keys().next().unwrap();
		let mut counter = 0u32;

		let flooding_reported = |network: &TestNetwork| {
			network
				.get_reports()
				.iter()
				.any(|(id, rep)| *id == peer_id && *rep == rep::STATEMENT_FLOODING)
		};

		for batch in 0..9 {
			handler.on_statements(peer_id, statement_batch(30_000, &mut counter));
			assert!(
				!flooding_reported(&network),
				"batch {batch} is still within the burst and must not be flagged",
			);
			clock.advance(Duration::from_millis(100));
		}

		handler.on_statements(peer_id, statement_batch(30_000, &mut counter));
		assert!(
			flooding_reported(&network),
			"the 10th batch overdraws the burst and must be flagged as flooding",
		);

		let disconnected = network.get_disconnected_peers();
		assert!(
			disconnected.contains(&peer_id),
			"Peer should be disconnected after sustained high rate. Disconnected: {:?}",
			disconnected
		);

		dispatch_disconnects(&mut handler, &network).await;

		assert!(!handler.peers.contains_key(&peer_id), "Peer should be removed from peers map");
	}

	#[tokio::test]
	async fn test_v2_peer_detected_when_no_fallback() {
		let (mut handler, _statement_store, _network, _notification_service) =
			build_handler_no_peers();

		let peer_id = PeerId::random();

		// No negotiated_fallback means the peer connected on the main protocol (v2).
		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: None,
			})
			.await;

		assert_eq!(
			handler.peers.get(&peer_id).unwrap().protocol_version,
			PeerProtocolVersion::V2,
			"Peer should be detected as v2 when no fallback is negotiated"
		);
	}

	#[tokio::test]
	async fn test_v1_peer_detected_when_fallback_negotiated() {
		let (mut handler, _statement_store, _network, _notification_service) =
			build_handler_no_peers();

		let peer_id = PeerId::random();

		// negotiated_fallback is Some means the peer fell back to v1.
		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: Some(format!("/{STATEMENT_PROTOCOL_V1}").into()),
			})
			.await;

		assert_eq!(
			handler.peers.get(&peer_id).unwrap().protocol_version,
			PeerProtocolVersion::V1,
			"Peer should be detected as v1 when fallback is negotiated"
		);
	}

	#[tokio::test]
	async fn test_v1_peer_decodes_raw_statements() {
		let (mut handler, _statement_store, _network, _notification_service) =
			build_handler_no_peers();

		let peer_id = PeerId::random();
		let (queue_sender, queue_receiver) = async_channel::bounded(10);
		handler.queue_sender = queue_sender;

		// Connect peer as v1 (with fallback).
		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: Some(format!("/{STATEMENT_PROTOCOL_V1}").into()),
			})
			.await;

		// V1 peer sends raw Vec<Statement>.
		let mut statement = new_live_statement();
		statement.set_plain_data(b"v1 statement".to_vec());
		let hash = statement.hash();
		let raw_encoded = vec![statement].encode();

		handler
			.handle_notification_event(NotificationEvent::NotificationReceived {
				peer: peer_id,
				notification: raw_encoded.into(),
			})
			.await;

		let (received, _) = queue_receiver.try_recv().unwrap();
		assert_eq!(received.hash(), hash, "V1 peer's raw statement should be decoded correctly");
	}

	#[tokio::test]
	async fn test_v2_peer_decodes_statement_message() {
		let (mut handler, _statement_store, _network, _notification_service) =
			build_handler_no_peers();

		let peer_id = PeerId::random();
		let (queue_sender, queue_receiver) = async_channel::bounded(10);
		handler.queue_sender = queue_sender;

		// Connect peer as v2 (no fallback).
		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: None,
			})
			.await;

		// V2 peer sends StatementMessage::Statements.
		let mut statement = new_live_statement();
		statement.set_plain_data(b"v2 statement".to_vec());
		let hash = statement.hash();
		let msg = StatementMessage::Statements(vec![statement].try_into().unwrap());
		let encoded = msg.encode();

		handler
			.handle_notification_event(NotificationEvent::NotificationReceived {
				peer: peer_id,
				notification: encoded.into(),
			})
			.await;

		let (received, _) = queue_receiver.try_recv().unwrap();
		assert_eq!(received.hash(), hash, "V2 peer's StatementMessage should be decoded correctly");
	}

	#[tokio::test]
	async fn test_v2_peer_topic_affinity_stored() {
		let (mut handler, _statement_store, _network, _notification_service) =
			build_handler_no_peers();

		let peer_id = PeerId::random();

		// Connect peer as v2.
		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: None,
			})
			.await;

		assert!(
			handler.peers.get(&peer_id).unwrap().topic_affinity.is_none(),
			"Topic affinity should be None initially"
		);

		// Send ExplicitTopicAffinity message.
		let topic: [u8; 32] = [0xAA; 32];
		let mut filter = AffinityFilter::new(BLOOM_SEED, 0.01, 100);
		filter.insert(&topic);
		let msg = StatementMessage::ExplicitTopicAffinity(filter);
		let encoded = msg.encode();

		handler
			.handle_notification_event(NotificationEvent::NotificationReceived {
				peer: peer_id,
				notification: encoded.into(),
			})
			.await;

		// Affinity is deferred; process it.
		handler.process_pending_affinities();

		let peer_data = handler.peers.get(&peer_id).unwrap();
		assert!(
			peer_data.topic_affinity.is_some(),
			"Topic affinity should be set after receiving ExplicitTopicAffinity"
		);
		// The filter should match the topic we inserted.
		assert!(
			peer_data.topic_affinity.as_ref().unwrap().contains(&topic),
			"Stored affinity filter should match the topic"
		);
	}

	#[tokio::test]
	async fn test_topic_affinity_filters_propagation() {
		let (mut handler, statement_store, _network, notification_service) =
			build_handler_no_peers();

		let peer_id = PeerId::random();

		// Connect peer as v2.
		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: None,
			})
			.await;

		// Set up topic affinity: peer is interested in topic 0xAA only.
		let topic_aa: [u8; 32] = [0xAA; 32];
		let topic_bb: [u8; 32] = [0xBB; 32];
		let mut filter = AffinityFilter::new(BLOOM_SEED, 0.01, 100);
		filter.insert(&topic_aa);
		let msg = StatementMessage::ExplicitTopicAffinity(filter);
		let encoded = msg.encode();
		handler
			.handle_notification_event(NotificationEvent::NotificationReceived {
				peer: peer_id,
				notification: encoded.into(),
			})
			.await;

		// Affinity is deferred; process it.
		handler.process_pending_affinities();

		// Create statements: one matching, one not matching, one with no topics.
		let mut stmt_matching = new_live_statement();
		stmt_matching.set_plain_data(b"matching".to_vec());
		stmt_matching.set_topic(0, topic_aa.into());
		let hash_matching = stmt_matching.hash();

		let mut stmt_not_matching = new_live_statement();
		stmt_not_matching.set_plain_data(b"not matching".to_vec());
		stmt_not_matching.set_topic(0, topic_bb.into());
		let hash_not_matching = stmt_not_matching.hash();

		let mut stmt_no_topic = new_live_statement();
		stmt_no_topic.set_plain_data(b"no topic".to_vec());
		let hash_no_topic = stmt_no_topic.hash();

		statement_store
			.recent_statements
			.lock()
			.unwrap()
			.insert(hash_matching, stmt_matching);
		statement_store
			.recent_statements
			.lock()
			.unwrap()
			.insert(hash_not_matching, stmt_not_matching);
		statement_store
			.recent_statements
			.lock()
			.unwrap()
			.insert(hash_no_topic, stmt_no_topic);

		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let sent = notification_service.get_sent_notifications();
		let mut sent_hashes: Vec<_> = sent
			.iter()
			.flat_map(|(_, notification)| {
				// V2 peer gets StatementMessage encoding.
				match StatementMessage::decode(&mut notification.as_slice()).unwrap() {
					StatementMessage::Statements(stmts) => stmts,
					_ => panic!("Expected StatementMessage::Statements"),
				}
			})
			.map(|s| s.hash())
			.collect();
		sent_hashes.sort();

		// Matching and no-topic statements should be sent; non-matching should be filtered.
		assert!(
			sent_hashes.contains(&hash_matching),
			"Statement matching topic affinity should be propagated"
		);
		assert!(
			sent_hashes.contains(&hash_no_topic),
			"Statement with no topics should be propagated (broadcast)"
		);
		assert!(
			!sent_hashes.contains(&hash_not_matching),
			"Statement NOT matching topic affinity should be filtered out"
		);
	}

	#[tokio::test]
	async fn test_v1_peer_no_topic_filtering() {
		let (mut handler, statement_store, _network, notification_service) =
			build_handler_no_peers();

		let peer_id = PeerId::random();

		// Connect peer as v1 (with fallback).
		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: Some(format!("/{STATEMENT_PROTOCOL_V1}").into()),
			})
			.await;

		// V1 peers have no topic affinity - all statements should be propagated.
		let topic_aa: [u8; 32] = [0xAA; 32];
		let mut stmt_with_topic = new_live_statement();
		stmt_with_topic.set_plain_data(b"with topic".to_vec());
		stmt_with_topic.set_topic(0, topic_aa.into());
		let hash_with_topic = stmt_with_topic.hash();

		let mut stmt_no_topic = new_live_statement();
		stmt_no_topic.set_plain_data(b"no topic".to_vec());
		let hash_no_topic = stmt_no_topic.hash();

		statement_store
			.recent_statements
			.lock()
			.unwrap()
			.insert(hash_with_topic, stmt_with_topic);
		statement_store
			.recent_statements
			.lock()
			.unwrap()
			.insert(hash_no_topic, stmt_no_topic);

		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let sent = notification_service.get_sent_notifications();
		let sent_hashes: Vec<_> = sent
			.iter()
			.flat_map(|(_, notification)| {
				<Statements as Decode>::decode(&mut notification.as_slice()).unwrap()
			})
			.map(|s| s.hash())
			.collect();

		assert_eq!(
			sent_hashes.len(),
			2,
			"V1 peer should receive all statements regardless of topics"
		);
		assert!(sent_hashes.contains(&hash_with_topic));
		assert!(sent_hashes.contains(&hash_no_topic));
	}

	#[tokio::test]
	async fn test_affinity_change_triggers_resync() {
		let (mut handler, statement_store, _network, notification_service) =
			build_handler_no_peers_light();

		let peer_id = PeerId::random();

		// Add statements with different topics to the store.
		let topic_aa: [u8; 32] = [0xAA; 32];
		let topic_bb: [u8; 32] = [0xBB; 32];

		let mut stmt_aa = new_live_statement();
		stmt_aa.set_plain_data(b"stmt_aa".to_vec());
		stmt_aa.set_topic(0, topic_aa.into());
		let hash_aa = stmt_aa.hash();

		let mut stmt_bb = new_live_statement();
		stmt_bb.set_plain_data(b"stmt_bb".to_vec());
		stmt_bb.set_topic(0, topic_bb.into());
		let hash_bb = stmt_bb.hash();

		let mut stmt_no_topic = new_live_statement();
		stmt_no_topic.set_plain_data(b"no topic".to_vec());
		let hash_no_topic = stmt_no_topic.hash();

		statement_store.insert(stmt_aa);
		statement_store.insert(stmt_bb);
		statement_store.insert(stmt_no_topic);

		// Connect peer as v2.
		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: None,
			})
			.await;

		// Light V2 peers should NOT get initial sync on connect (must set affinity first).
		assert!(
			!handler.pending_initial_syncs.contains_key(&peer_id),
			"Light V2 peer should NOT have initial sync scheduled on connect"
		);

		// Set topic affinity to topic_aa — this triggers the first initial sync.
		let mut filter = AffinityFilter::new(BLOOM_SEED, 0.01, 100);
		filter.insert(&topic_aa);
		let msg = StatementMessage::ExplicitTopicAffinity(filter);
		let encoded = msg.encode();
		handler
			.handle_notification_event(NotificationEvent::NotificationReceived {
				peer: peer_id,
				notification: encoded.into(),
			})
			.await;

		// Affinity is deferred; process it.
		handler.process_pending_affinities();

		assert!(
			handler.pending_initial_syncs.contains_key(&peer_id),
			"Initial sync should be scheduled after setting affinity"
		);

		// Drain initial sync — only stmt_aa and stmt_no_topic should be sent.
		while handler.pending_initial_syncs.contains_key(&peer_id) {
			handler.process_initial_sync_burst();
			handler.flush_pending_sends().await;
		}

		let sent = notification_service.get_sent_notifications();
		let sent_hashes: HashSet<_> = sent
			.iter()
			.flat_map(|(_, notification)| {
				match StatementMessage::decode(&mut notification.as_slice()).unwrap() {
					StatementMessage::Statements(stmts) => stmts,
					_ => panic!("Expected StatementMessage::Statements"),
				}
			})
			.map(|s| s.hash())
			.collect();
		assert!(sent_hashes.contains(&hash_aa), "stmt_aa should be sent (matches affinity)");
		assert!(
			sent_hashes.contains(&hash_no_topic),
			"stmt_no_topic should be sent (broadcast, no topic)"
		);
		assert!(!sent_hashes.contains(&hash_bb), "stmt_bb should NOT be sent (filtered)");

		// Now change affinity to topic_bb — triggers re-sync.
		let mut filter = AffinityFilter::new(BLOOM_SEED, 0.01, 100);
		filter.insert(&topic_bb);
		let msg = StatementMessage::ExplicitTopicAffinity(filter);
		let encoded = msg.encode();
		handler
			.handle_notification_event(NotificationEvent::NotificationReceived {
				peer: peer_id,
				notification: encoded.into(),
			})
			.await;

		// Affinity is deferred; process it.
		handler.process_pending_affinities();

		assert!(
			handler.pending_initial_syncs.contains_key(&peer_id),
			"Initial sync should be re-scheduled after affinity change"
		);

		notification_service.clear_sent_notifications();
		while handler.pending_initial_syncs.contains_key(&peer_id) {
			handler.process_initial_sync_burst();
			handler.flush_pending_sends().await;
		}

		let sent_after_bb = notification_service.get_sent_notifications();
		let sent_hashes_bb: HashSet<_> = sent_after_bb
			.iter()
			.flat_map(|(_, notification)| {
				match StatementMessage::decode(&mut notification.as_slice()).unwrap() {
					StatementMessage::Statements(stmts) => stmts,
					_ => panic!("Expected StatementMessage::Statements"),
				}
			})
			.map(|s| s.hash())
			.collect();
		// stmt_bb was previously filtered and should now be sent.
		assert!(
			sent_hashes_bb.contains(&hash_bb),
			"stmt_bb should now be sent after affinity changed to topic_bb"
		);
		// Known statements are redelivered on affinity change.
		assert!(
			sent_hashes_bb.contains(&hash_no_topic),
			"stmt_no_topic should be re-sent (initial sync resends everything matching)"
		);
	}

	#[tokio::test]
	async fn test_affinity_change_sends_previously_filtered_statements() {
		// This tests the scenario where:
		// 1. Peer connects and immediately sets affinity (before initial sync).
		// 2. Statements not matching the initial affinity are not delivered.
		// 3. When affinity changes to include those topics, they ARE sent.
		let (mut handler, statement_store, _network, notification_service) =
			build_handler_no_peers_light();

		let peer_id = PeerId::random();

		let topic_aa: [u8; 32] = [0xAA; 32];
		let topic_bb: [u8; 32] = [0xBB; 32];

		let mut stmt_aa = new_live_statement();
		stmt_aa.set_plain_data(b"stmt_aa".to_vec());
		stmt_aa.set_topic(0, topic_aa.into());
		let hash_aa = stmt_aa.hash();

		let mut stmt_bb = new_live_statement();
		stmt_bb.set_plain_data(b"stmt_bb".to_vec());
		stmt_bb.set_topic(0, topic_bb.into());
		let hash_bb = stmt_bb.hash();

		statement_store.insert(stmt_aa.clone());
		statement_store.insert(stmt_bb.clone());

		// Also put them in recent_statements so propagate_statements can find them.
		statement_store.recent_statements.lock().unwrap().insert(hash_aa, stmt_aa);
		statement_store.recent_statements.lock().unwrap().insert(hash_bb, stmt_bb);

		// Connect peer as v2.
		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: None,
			})
			.await;

		// Immediately set affinity to topic_aa BEFORE any initial sync runs.
		let mut filter = AffinityFilter::new(BLOOM_SEED, 0.01, 100);
		filter.insert(&topic_aa);
		let msg = StatementMessage::ExplicitTopicAffinity(filter);
		let encoded = msg.encode();
		handler
			.handle_notification_event(NotificationEvent::NotificationReceived {
				peer: peer_id,
				notification: encoded.into(),
			})
			.await;

		// Affinity is deferred; process it.
		handler.process_pending_affinities();

		// Drain initial sync — should only send stmt_aa (matches affinity).
		while handler.pending_initial_syncs.contains_key(&peer_id) {
			handler.process_initial_sync_burst();
			handler.flush_pending_sends().await;
		}

		let sent = notification_service.get_sent_notifications();
		let sent_hashes: HashSet<_> = sent
			.iter()
			.flat_map(|(_, notification)| {
				match StatementMessage::decode(&mut notification.as_slice()).unwrap() {
					StatementMessage::Statements(stmts) => stmts,
					_ => panic!("Expected StatementMessage::Statements"),
				}
			})
			.map(|s| s.hash())
			.collect();
		assert!(sent_hashes.contains(&hash_aa), "stmt_aa should be sent (matches affinity)");
		assert!(
			!sent_hashes.contains(&hash_bb),
			"stmt_bb should NOT be sent (filtered by affinity)"
		);

		// Propagation must apply the same affinity filter. The original statements sit below
		// the sync watermark and belong to the cursor, so fresh admissions carry the check.
		let mut stmt_aa2 = new_live_statement();
		stmt_aa2.set_plain_data(b"stmt_aa2".to_vec());
		stmt_aa2.set_topic(0, topic_aa.into());
		let hash_aa2 = stmt_aa2.hash();
		let mut stmt_bb2 = new_live_statement();
		stmt_bb2.set_plain_data(b"stmt_bb2".to_vec());
		stmt_bb2.set_topic(0, topic_bb.into());
		let hash_bb2 = stmt_bb2.hash();
		statement_store.recent_statements.lock().unwrap().insert(hash_aa2, stmt_aa2);
		statement_store.recent_statements.lock().unwrap().insert(hash_bb2, stmt_bb2);

		notification_service.clear_sent_notifications();
		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let sent = notification_service.get_sent_notifications();
		let sent_hashes: HashSet<_> = sent
			.iter()
			.flat_map(|(_, notification)| {
				match StatementMessage::decode(&mut notification.as_slice()).unwrap() {
					StatementMessage::Statements(stmts) => stmts,
					_ => panic!("Expected StatementMessage::Statements"),
				}
			})
			.map(|s| s.hash())
			.collect();
		assert!(
			sent_hashes.contains(&hash_aa2),
			"stmt_aa2 should be propagated (matches affinity)"
		);
		assert!(
			!sent_hashes.contains(&hash_bb2),
			"stmt_bb2 should NOT be propagated (filtered by affinity)"
		);
		assert!(
			!sent_hashes.contains(&hash_aa),
			"a statement below the sync watermark is delivered by the cursor, not propagation"
		);

		// Now change affinity to include topic_bb.
		let mut filter = AffinityFilter::new(BLOOM_SEED, 0.01, 100);
		filter.insert(&topic_aa);
		filter.insert(&topic_bb);
		let msg = StatementMessage::ExplicitTopicAffinity(filter);
		let encoded = msg.encode();

		notification_service.clear_sent_notifications();
		handler
			.handle_notification_event(NotificationEvent::NotificationReceived {
				peer: peer_id,
				notification: encoded.into(),
			})
			.await;

		// Affinity is deferred; process it.
		handler.process_pending_affinities();

		// Drain re-sync — stmt_bb should now be sent.
		while handler.pending_initial_syncs.contains_key(&peer_id) {
			handler.process_initial_sync_burst();
			handler.flush_pending_sends().await;
		}

		let sent = notification_service.get_sent_notifications();
		let sent_hashes: HashSet<_> = sent
			.iter()
			.flat_map(|(_, notification)| {
				match StatementMessage::decode(&mut notification.as_slice()).unwrap() {
					StatementMessage::Statements(stmts) => stmts,
					_ => panic!("Expected StatementMessage::Statements"),
				}
			})
			.map(|s| s.hash())
			.collect();
		assert!(
			sent_hashes.contains(&hash_bb),
			"stmt_bb should now be sent after affinity expanded to include topic_bb"
		);
		// stmt_aa is also redelivered on affinity change.
		assert!(
			sent_hashes.contains(&hash_aa),
			"stmt_aa should be re-sent (initial sync resends everything matching)"
		);
	}

	#[test]
	fn test_encode_statement_refs_matches_derive_encoding() {
		let mut stmt1 = new_live_statement();
		stmt1.set_plain_data(b"first".to_vec());
		let mut stmt2 = new_live_statement();
		stmt2.set_plain_data(b"second".to_vec());

		let refs: Vec<&Statement> = vec![&stmt1, &stmt2];
		let hand_rolled = StatementMessage::encode_statement_refs(&refs);
		let derive_encoded =
			StatementMessage::Statements(vec![stmt1, stmt2].try_into().unwrap()).encode();

		assert_eq!(
			hand_rolled, derive_encoded,
			"encode_statement_refs must produce identical bytes to derive Encode"
		);
	}

	#[test]
	fn test_encode_statement_refs_empty() {
		let refs: Vec<&Statement> = vec![];
		let hand_rolled = StatementMessage::encode_statement_refs(&refs);
		let derive_encoded = StatementMessage::Statements(vec![].try_into().unwrap()).encode();

		assert_eq!(hand_rolled, derive_encoded);
	}

	#[test]
	fn test_can_receive_all_combinations() {
		let make_peer = |is_light: bool, version: PeerProtocolVersion, has_affinity: bool| {
			let topic_affinity = has_affinity.then(|| AffinityFilter::new(BLOOM_SEED, 0.01, 10));
			Peer {
				rate_limiter: PeerRateLimiter::new(
					NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND).expect("nonzero"),
					NonZeroU32::new(
						DEFAULT_STATEMENTS_PER_SECOND * config::STATEMENTS_BURST_COEFFICIENT,
					)
					.expect("nonzero"),
				),
				protocol_version: version,
				topic_affinity,
				is_light,
				pending_topic_affinity: None,
				sync_watermark: 0,
			}
		};

		// Full node, V1, no affinity → can receive
		assert!(make_peer(false, PeerProtocolVersion::V1, false).can_receive());
		// Full node, V2, no affinity → can receive
		assert!(make_peer(false, PeerProtocolVersion::V2, false).can_receive());
		// Light, V1, no affinity → can receive (V1 doesn't gate)
		assert!(make_peer(true, PeerProtocolVersion::V1, false).can_receive());
		// Light, V2, no affinity → CANNOT receive (must set affinity first)
		assert!(!make_peer(true, PeerProtocolVersion::V2, false).can_receive());
		// Light, V2, with affinity → can receive
		assert!(make_peer(true, PeerProtocolVersion::V2, true).can_receive());
		// Full node, V2, with affinity → can receive
		assert!(make_peer(false, PeerProtocolVersion::V2, true).can_receive());
	}

	#[tokio::test]
	async fn test_send_chunk_v1_vs_v2_encoding() {
		let (mut handler, statement_store, _network, notification_service) =
			build_handler_no_peers();

		let v1_peer = PeerId::random();
		let v2_peer = PeerId::random();

		// Connect V1 peer.
		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: v1_peer,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: Some(format!("/{STATEMENT_PROTOCOL_V1}").into()),
			})
			.await;

		// Connect V2 peer.
		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: v2_peer,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: None,
			})
			.await;

		let mut stmt = new_live_statement();
		stmt.set_plain_data(b"encoding test".to_vec());
		statement_store.insert(stmt);

		// Send to V1 peer.
		notification_service.clear_sent_notifications();
		handler.schedule_initial_sync_for_peer(v1_peer);
		while handler.pending_initial_syncs.contains_key(&v1_peer) {
			handler.process_initial_sync_burst();
			handler.flush_pending_sends().await;
		}
		let v1_sent = notification_service.get_sent_notifications();
		assert_eq!(v1_sent.len(), 1);
		let v1_bytes = &v1_sent[0].1;
		// V1 encoding is raw Vec<Statement>.
		let decoded_v1 = <Statements as Decode>::decode(&mut v1_bytes.as_slice())
			.expect("V1 peer should receive raw Vec<Statement> encoding");
		assert_eq!(decoded_v1.len(), 1);

		// Send to V2 peer.
		notification_service.clear_sent_notifications();
		handler.schedule_initial_sync_for_peer(v2_peer);
		while handler.pending_initial_syncs.contains_key(&v2_peer) {
			handler.process_initial_sync_burst();
			handler.flush_pending_sends().await;
		}
		let v2_sent = notification_service.get_sent_notifications();
		assert_eq!(v2_sent.len(), 1);
		let v2_bytes = &v2_sent[0].1;
		// V2 encoding is StatementMessage::Statements.
		let decoded_v2 = StatementMessage::decode(&mut v2_bytes.as_slice())
			.expect("V2 peer should receive StatementMessage encoding");
		match decoded_v2 {
			StatementMessage::Statements(stmts) => assert_eq!(stmts.len(), 1),
			_ => panic!("Expected StatementMessage::Statements for V2 peer"),
		}

		// Verify the two encodings are different (V2 has an extra enum discriminant byte).
		assert_ne!(v1_bytes, v2_bytes, "V1 and V2 encodings should differ");
	}

	#[tokio::test]
	async fn test_schedule_initial_sync_replaces_existing() {
		let (mut handler, statement_store, _network, _notification_service) =
			build_handler_no_peers();

		let peer_id = PeerId::random();

		// Add some statements to the store.
		let mut stmt1 = new_live_statement();
		stmt1.set_plain_data(b"stmt1".to_vec());
		statement_store.insert(stmt1);

		// Connect peer as V1.
		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: Some(format!("/{STATEMENT_PROTOCOL_V1}").into()),
			})
			.await;

		// Should have initial sync scheduled.
		assert!(handler.pending_initial_syncs.contains_key(&peer_id));
		assert_eq!(
			handler.initial_sync_peer_queue.iter().filter(|p| **p == peer_id).count(),
			1,
			"Peer should appear exactly once in the queue"
		);

		// Add another statement and re-schedule.
		let mut stmt2 = new_live_statement();
		stmt2.set_plain_data(b"stmt2".to_vec());
		statement_store.insert(stmt2);

		handler.schedule_initial_sync_for_peer(peer_id);

		// Peer should still appear exactly once in the queue (no duplicates).
		assert_eq!(
			handler.initial_sync_peer_queue.iter().filter(|p| **p == peer_id).count(),
			1,
			"Peer should NOT be duplicated in the queue after re-schedule"
		);
		// The new sync should cover both admissions.
		let pending = handler.pending_initial_syncs.get(&peer_id).unwrap();
		assert_eq!(pending.cursor, 0);
		assert_eq!(pending.watermark, 2);
	}

	#[tokio::test]
	async fn test_initial_sync_queued_during_major_sync_processed_after() {
		let statement_store = TestStatementStore::new();
		let (queue_sender, _queue_receiver) = async_channel::bounded(2);
		let network = TestNetwork::new();
		let notification_service = TestNotificationService::new();
		let sync = TestSync::new();
		// Set major syncing to true.
		sync.major_syncing.store(true, Ordering::Relaxed);

		let mut handler = StatementHandler {
			protocol_name: format!("/{STATEMENT_PROTOCOL_V1}").into(),
			notification_service: Box::new(notification_service.clone()),
			propagate_timeout: (Box::pin(futures::stream::pending())
				as Pin<Box<dyn Stream<Item = ()> + Send>>)
				.fuse(),
			pending_statements: FuturesUnordered::new(),
			pending_statements_peers: HashMap::new(),
			recently_received_statements: HashMap::new(),
			network: network.clone(),
			sync: sync.clone(),
			sync_event_stream: (Box::pin(futures::stream::pending())
				as Pin<Box<dyn Stream<Item = sc_network_sync::types::SyncEvent> + Send>>)
				.fuse(),
			peers: HashMap::new(),
			statement_store: Arc::new(statement_store.clone()),
			queue_sender,
			statements_per_second: NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND)
				.expect("DEFAULT_STATEMENTS_PER_SECOND is nonzero"),
			metrics: None,
			initial_sync_timeout: Box::pin(futures::future::pending()),
			pending_affinities_timeout: Box::pin(futures::future::pending()),
			pending_initial_syncs: HashMap::new(),
			initial_sync_peer_queue: VecDeque::new(),
			next_initial_sync_id: 0,
			initial_sync_in_flight_bytes: 0,
			propagation_outboxes: HashMap::new(),
			in_flight_chunks: HashMap::new(),
			next_chunk_id: 0,
			propagation_in_flight_bytes: 0,
			parked_propagations: VecDeque::new(),
			pending_sends: FuturesUnordered::new(),
			deferred_peers: HashSet::new(),
			dropped_statements_during_sync: false,
			sync_recovery_peer: None,
			sync_recovery_readd_timeout: Box::pin(futures::future::pending()),
		};

		// Add a statement so there's something to sync.
		let mut stmt = new_live_statement();
		stmt.set_plain_data(b"during major sync".to_vec());
		statement_store.insert(stmt);

		// Add a peer manually.
		let peer_id = PeerId::random();
		handler.peers.insert(
			peer_id,
			Peer::new_for_testing(
				NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND).unwrap(),
				NonZeroU32::new(
					DEFAULT_STATEMENTS_PER_SECOND * config::STATEMENTS_BURST_COEFFICIENT,
				)
				.unwrap(),
			),
		);

		// Scheduling during major sync should queue the peer.
		handler.schedule_initial_sync_for_peer(peer_id);

		assert!(
			handler.pending_initial_syncs.contains_key(&peer_id),
			"Initial sync should be queued even during major sync"
		);
		assert_eq!(handler.initial_sync_peer_queue.len(), 1);

		// But burst processing should be a no-op while major syncing.
		handler.process_initial_sync_burst();
		handler.flush_pending_sends().await;
		assert!(
			handler.pending_initial_syncs.contains_key(&peer_id),
			"Pending sync should remain untouched during major sync"
		);

		// Once major sync completes, burst processing should proceed.
		sync.major_syncing.store(false, Ordering::Relaxed);
		while handler.pending_initial_syncs.contains_key(&peer_id) {
			handler.process_initial_sync_burst();
			handler.flush_pending_sends().await;
		}
		assert!(
			handler.initial_sync_peer_queue.is_empty(),
			"Peer should have been processed after major sync ended"
		);
	}

	#[tokio::test]
	async fn test_schedule_initial_sync_resends_all_matching() {
		let (mut handler, statement_store, _network, _notification_service) =
			build_handler_no_peers();

		let peer_id = PeerId::random();

		// Add statements to the store.
		let mut stmt1 = new_live_statement();
		stmt1.set_plain_data(b"delivered before".to_vec());
		let mut stmt2 = new_live_statement();
		stmt2.set_plain_data(b"never delivered".to_vec());

		statement_store.insert(stmt1);
		statement_store.insert(stmt2);

		handler.peers.insert(
			peer_id,
			Peer {
				rate_limiter: PeerRateLimiter::new(
					NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND).unwrap(),
					NonZeroU32::new(
						DEFAULT_STATEMENTS_PER_SECOND * config::STATEMENTS_BURST_COEFFICIENT,
					)
					.unwrap(),
				),
				protocol_version: PeerProtocolVersion::V1,
				topic_affinity: None,
				is_light: false,
				pending_topic_affinity: None,
				sync_watermark: 0,
			},
		);

		handler.schedule_initial_sync_for_peer(peer_id);

		let pending = handler.pending_initial_syncs.get(&peer_id).unwrap();
		// The whole admission journal is covered for redelivery.
		assert_eq!(pending.cursor, 0, "The sync must start from the oldest admission");
		assert_eq!(pending.watermark, 2, "Both admissions must sit below the watermark");
	}

	#[tokio::test]
	async fn statement_below_the_watermark_reaches_a_syncing_peer_exactly_once() {
		let (mut handler, statement_store, _network, notification_service) =
			build_handler_no_peers();
		let peer_id = PeerId::random();

		let mut statement = new_live_statement();
		statement.set_plain_data(b"pre-watermark".to_vec());
		let hash = statement.hash();
		statement_store.insert(statement.clone());
		// The statement is also due for a propagation tick, racing the sync.
		statement_store.recent_statements.lock().unwrap().insert(hash, statement);

		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: Some(format!("/{STATEMENT_PROTOCOL_V1}").into()),
			})
			.await;
		assert!(handler.pending_initial_syncs.contains_key(&peer_id));

		// A tick between scheduling and the first burst must not queue the statement:
		// below the peer's sync watermark it is the cursor's job.
		handler.propagate_statements().await;
		assert!(!handler.propagation_outboxes.contains_key(&peer_id));

		while handler.pending_initial_syncs.contains_key(&peer_id) {
			handler.process_initial_sync_burst();
			handler.flush_pending_sends().await;
		}

		let sent = get_peer_hashes(&notification_service.get_sent_notifications(), peer_id);
		assert_eq!(sent, vec![hash], "the sync cursor is the only delivery path");
	}

	#[tokio::test]
	async fn sync_watermark_keeps_filtering_propagation_after_the_sync_completes() {
		let (mut handler, statement_store, _network, notification_service) =
			build_handler_no_peers();
		let peer_id = PeerId::random();

		let mut pre = new_live_statement();
		pre.set_plain_data(b"pre-watermark".to_vec());
		let pre_hash = pre.hash();
		statement_store.insert(pre.clone());

		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: Some(format!("/{STATEMENT_PROTOCOL_V1}").into()),
			})
			.await;
		while handler.pending_initial_syncs.contains_key(&peer_id) {
			handler.process_initial_sync_burst();
			handler.flush_pending_sends().await;
		}
		notification_service.clear_sent_notifications();

		// A late tick drains the already synced statement together with a fresh one.
		let mut fresh = new_live_statement();
		fresh.set_plain_data(b"post-watermark".to_vec());
		let fresh_hash = fresh.hash();
		statement_store.recent_statements.lock().unwrap().insert(pre_hash, pre);
		statement_store.recent_statements.lock().unwrap().insert(fresh_hash, fresh);

		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let sent = get_peer_hashes(&notification_service.get_sent_notifications(), peer_id);
		assert_eq!(
			sent,
			vec![fresh_hash],
			"only admissions at or above the watermark are propagated"
		);
	}

	#[tokio::test]
	async fn test_malformed_v2_message_does_not_panic() {
		let (mut handler, _statement_store, _network, _notification_service) =
			build_handler_no_peers();

		let peer_id = PeerId::random();

		// Connect peer as V2.
		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: None,
			})
			.await;

		// Send garbage data — should not panic, just log debug.
		handler
			.handle_notification_event(NotificationEvent::NotificationReceived {
				peer: peer_id,
				notification: vec![0xFF, 0xFE, 0xFD].into(),
			})
			.await;

		// Send V1-encoded data to V2 peer — also should not panic.
		let mut stmt = new_live_statement();
		stmt.set_plain_data(b"v1 encoded".to_vec());
		let v1_encoded = vec![stmt].encode();
		handler
			.handle_notification_event(NotificationEvent::NotificationReceived {
				peer: peer_id,
				notification: v1_encoded.into(),
			})
			.await;

		// If we got here without panic, the test passes.
		assert!(handler.peers.contains_key(&peer_id), "Peer should still be connected");
	}

	// A batch of one more empty statement (a single `0x00` field-count byte each) than a
	// notification can hold. An unbounded `Vec<Statement>` would decode it in full.
	fn oversized_statement_batch() -> Vec<u8> {
		let count = MAX_STATEMENTS_PER_NOTIFICATION + 1;
		let mut batch = Compact(count as u32).encode();
		batch.extend(iter::repeat_n(0u8, count));
		batch
	}

	#[tokio::test]
	async fn test_v1_oversized_batch_reported_as_bad_message() {
		let (mut handler, _statement_store, network, _notification_service) =
			build_handler_no_peers();

		let peer_id = PeerId::random();

		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: Some(format!("/{STATEMENT_PROTOCOL_V1}").into()),
			})
			.await;

		handler
			.handle_notification_event(NotificationEvent::NotificationReceived {
				peer: peer_id,
				notification: oversized_statement_batch().into(),
			})
			.await;

		let reports = network.get_reports();
		assert!(
			reports.iter().any(|(id, rep)| *id == peer_id && *rep == rep::BAD_MESSAGE),
			"Expected BAD_MESSAGE reputation change, but got: {reports:?}"
		);
		assert!(
			!network.get_disconnected_peers().contains(&peer_id),
			"Expected oversized-batch peer to stay connected"
		);
	}

	#[tokio::test]
	async fn test_v2_oversized_batch_reported_as_bad_message() {
		let (mut handler, _statement_store, network, _notification_service) =
			build_handler_no_peers();

		let peer_id = PeerId::random();

		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: None,
			})
			.await;

		// `StatementMessage::Statements` variant byte followed by the oversized batch.
		let mut notification = vec![STATEMENTS_VARIANT_INDEX];
		notification.extend(oversized_statement_batch());

		handler
			.handle_notification_event(NotificationEvent::NotificationReceived {
				peer: peer_id,
				notification: notification.into(),
			})
			.await;

		let reports = network.get_reports();
		assert!(
			reports.iter().any(|(id, rep)| *id == peer_id && *rep == rep::BAD_MESSAGE),
			"Expected BAD_MESSAGE reputation change, but got: {reports:?}"
		);
		assert!(
			!network.get_disconnected_peers().contains(&peer_id),
			"Expected oversized-batch peer to stay connected"
		);
	}

	#[test]
	fn test_max_statement_payload_size_v2_overhead() {
		let v1_max = max_statement_payload_size(V1_ENVELOPE_OVERHEAD);
		let v2_max = max_statement_payload_size(V2_ENVELOPE_OVERHEAD);

		// V2 has strictly less payload space than V1.
		assert!(
			v2_max < v1_max,
			"V2 payload capacity ({v2_max}) should be less than V1 ({v1_max})"
		);
		assert_eq!(v1_max - v2_max, 1, "V2 overhead is exactly 1 byte more than V1");
	}

	#[tokio::test]
	async fn test_full_node_v2_gets_initial_sync_immediately() {
		let (mut handler, statement_store, _network, _notification_service) =
			build_handler_no_peers();

		// Add a statement so there's something to sync.
		let mut stmt = new_live_statement();
		stmt.set_plain_data(b"full node v2".to_vec());
		statement_store.insert(stmt);

		let peer_id = PeerId::random();

		// Connect as full-node V2 (no fallback, network returns Full role).
		handler
			.handle_notification_event(NotificationEvent::NotificationStreamOpened {
				peer: peer_id,
				direction: sc_network::service::traits::Direction::Inbound,
				handshake: vec![],
				negotiated_fallback: None,
			})
			.await;

		// Full-node V2 peer should get initial sync immediately (not gated).
		assert!(
			handler.pending_initial_syncs.contains_key(&peer_id),
			"Full-node V2 peer should have initial sync scheduled immediately"
		);
		assert_eq!(handler.peers.get(&peer_id).unwrap().protocol_version, PeerProtocolVersion::V2);
		assert!(!handler.peers.get(&peer_id).unwrap().is_light);
	}

	#[tokio::test]
	async fn test_propagation_reaches_all_connected_peers() {
		let (
			mut handler,
			statement_store,
			_network,
			notification_service,
			_queue_receiver,
			peer_ids,
		) = build_handler(5);

		// Insert 3 statements into recent_statements for propagation
		let mut expected_hashes = Vec::new();
		for i in 0..3u8 {
			let mut statement = new_live_statement();
			statement.set_plain_data(vec![i; 100]);
			let hash = statement.hash();
			expected_hashes.push(hash);
			statement_store.recent_statements.lock().unwrap().insert(hash, statement);
		}
		expected_hashes.sort();

		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let sent = notification_service.get_sent_notifications();

		// Verify each peer received all 3 statements
		for peer_id in &peer_ids {
			let mut received_hashes = get_peer_hashes(&sent, *peer_id);
			received_hashes.sort();

			assert_eq!(
				received_hashes, expected_hashes,
				"Peer {peer_id} should have received all 3 statements"
			);
		}

		// Recent statements should be drained
		assert!(statement_store.recent_statements.lock().unwrap().is_empty());
	}

	#[tokio::test]
	async fn test_received_statement_filtering_per_peer() {
		let (
			mut handler,
			statement_store,
			_network,
			notification_service,
			_queue_receiver,
			peer_ids,
		) = build_handler(3);

		let peer_a = peer_ids[0];
		let peer_b = peer_ids[1];
		let peer_c = peer_ids[2];

		// Create 5 statements
		let mut hashes = Vec::new();
		for i in 0..5u8 {
			let mut statement = new_live_statement();
			statement.set_plain_data(vec![i; 100]);
			let hash = statement.hash();
			hashes.push(hash);
			statement_store.recent_statements.lock().unwrap().insert(hash, statement);
		}

		// peer_a sent s1 and s2, peer_b sent s3, peer_c sent none.
		handler
			.recently_received_statements
			.insert(hashes[0], HashSet::from_iter([peer_a]));
		handler
			.recently_received_statements
			.insert(hashes[1], HashSet::from_iter([peer_a]));
		handler
			.recently_received_statements
			.insert(hashes[2], HashSet::from_iter([peer_b]));

		handler.propagate_statements().await;
		handler.flush_pending_sends().await;

		let sent = notification_service.get_sent_notifications();

		let peer_a_hashes = get_peer_hashes(&sent, peer_a);
		let peer_b_hashes = get_peer_hashes(&sent, peer_b);
		let peer_c_hashes = get_peer_hashes(&sent, peer_c);

		// peer_a sent s1,s2 → should only get s3,s4,s5
		assert_eq!(peer_a_hashes.len(), 3, "peer_a should get 3 statements");
		assert!(!peer_a_hashes.contains(&hashes[0]), "peer_a sent s1");
		assert!(!peer_a_hashes.contains(&hashes[1]), "peer_a sent s2");
		assert!(peer_a_hashes.contains(&hashes[2]));
		assert!(peer_a_hashes.contains(&hashes[3]));
		assert!(peer_a_hashes.contains(&hashes[4]));

		// peer_b sent s3 → should get s1,s2,s4,s5
		assert_eq!(peer_b_hashes.len(), 4, "peer_b should get 4 statements");
		assert!(!peer_b_hashes.contains(&hashes[2]), "peer_b sent s3");
		assert!(peer_b_hashes.contains(&hashes[0]));
		assert!(peer_b_hashes.contains(&hashes[1]));
		assert!(peer_b_hashes.contains(&hashes[3]));
		assert!(peer_b_hashes.contains(&hashes[4]));

		// peer_c sent nothing → should get all 5
		let mut sorted_peer_c: Vec<_> = peer_c_hashes.into_iter().collect();
		sorted_peer_c.sort();
		let mut all_hashes = hashes.clone();
		all_hashes.sort();
		assert_eq!(sorted_peer_c, all_hashes, "peer_c should get all 5 statements");
	}

	/// Verifies that peers connecting during major sync are buffered in `deferred_peers` with no
	/// network calls, and that a disconnect before sync ends removes the peer from the buffer
	#[test]
	fn major_sync_defers_peers_and_handles_disconnect() {
		let (sync, _flag) = TestSync::with_syncing(true);
		let network = TestNetwork::new();
		let notification_service = TestNotificationService::new();
		let statement_store = TestStatementStore::new();
		let (queue_sender, _queue_receiver) = async_channel::bounded(100);

		let mut handler = StatementHandler {
			protocol_name: "/statement/1".into(),
			notification_service: Box::new(notification_service),
			propagate_timeout: (Box::pin(futures::stream::pending())
				as Pin<Box<dyn Stream<Item = ()> + Send>>)
				.fuse(),
			pending_statements: FuturesUnordered::new(),
			pending_statements_peers: HashMap::new(),
			recently_received_statements: HashMap::new(),
			network: network.clone(),
			sync,
			sync_event_stream: (Box::pin(futures::stream::pending())
				as Pin<Box<dyn Stream<Item = sc_network_sync::types::SyncEvent> + Send>>)
				.fuse(),
			peers: HashMap::new(),
			statement_store: Arc::new(statement_store),
			queue_sender,
			statements_per_second: NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND)
				.expect("DEFAULT_STATEMENTS_PER_SECOND is nonzero"),
			metrics: None,
			initial_sync_timeout: Box::pin(futures::future::pending()),
			pending_affinities_timeout: Box::pin(futures::future::pending()),
			pending_initial_syncs: HashMap::new(),
			initial_sync_peer_queue: VecDeque::new(),
			next_initial_sync_id: 0,
			initial_sync_in_flight_bytes: 0,
			propagation_outboxes: HashMap::new(),
			in_flight_chunks: HashMap::new(),
			next_chunk_id: 0,
			propagation_in_flight_bytes: 0,
			parked_propagations: VecDeque::new(),
			pending_sends: FuturesUnordered::new(),
			deferred_peers: HashSet::new(),
			dropped_statements_during_sync: false,
			sync_recovery_peer: None,
			sync_recovery_readd_timeout: Box::pin(pending().fuse()),
		};

		let peer1 = PeerId::random();
		let peer2 = PeerId::random();
		let peer3 = PeerId::random();

		handler.handle_sync_event(SyncEvent::PeerConnected {
			peer_id: peer1,
			roles: sc_network::Roles::FULL,
		});
		handler.handle_sync_event(SyncEvent::PeerConnected {
			peer_id: peer2,
			roles: sc_network::Roles::FULL,
		});
		handler.handle_sync_event(SyncEvent::PeerConnected {
			peer_id: peer3,
			roles: sc_network::Roles::FULL,
		});

		// No network calls while major sync is active
		assert!(network.get_added_reserved().is_empty());
		assert!(network.get_removed_reserved().is_empty());
		assert_eq!(handler.deferred_peers.len(), 3);

		// Disconnect before sync ends must remove from buffer only
		handler.handle_sync_event(SyncEvent::PeerDisconnected(peer1));
		assert_eq!(handler.deferred_peers.len(), 2);
		assert!(!handler.deferred_peers.contains(&peer1), "disconnected peer must leave buffer");
		assert!(handler.deferred_peers.contains(&peer2));
		assert!(handler.deferred_peers.contains(&peer3));
		assert!(network.get_removed_reserved().is_empty(), "no remove call for buffered peer");
	}

	#[test]
	fn deferred_peers_flushed_on_sync_end_without_remove() {
		let (sync, flag) = TestSync::with_syncing(true);
		let network = TestNetwork::new();
		let notification_service = TestNotificationService::new();
		let statement_store = TestStatementStore::new();
		let (queue_sender, _queue_receiver) = async_channel::bounded(100);

		let peer1 = PeerId::random();
		let peer2 = PeerId::random();
		let mut deferred = HashSet::new();
		deferred.insert(peer1);
		deferred.insert(peer2);

		let mut handler = StatementHandler {
			protocol_name: "/statement/1".into(),
			notification_service: Box::new(notification_service),
			propagate_timeout: (Box::pin(futures::stream::pending())
				as Pin<Box<dyn Stream<Item = ()> + Send>>)
				.fuse(),
			pending_statements: FuturesUnordered::new(),
			pending_statements_peers: HashMap::new(),
			recently_received_statements: HashMap::new(),
			network: network.clone(),
			sync,
			sync_event_stream: (Box::pin(futures::stream::pending())
				as Pin<Box<dyn Stream<Item = sc_network_sync::types::SyncEvent> + Send>>)
				.fuse(),
			peers: HashMap::new(),
			statement_store: Arc::new(statement_store),
			queue_sender,
			statements_per_second: NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND)
				.expect("DEFAULT_STATEMENTS_PER_SECOND is nonzero"),
			metrics: None,
			initial_sync_timeout: Box::pin(futures::future::pending()),
			pending_affinities_timeout: Box::pin(futures::future::pending()),
			pending_initial_syncs: HashMap::new(),
			initial_sync_peer_queue: VecDeque::new(),
			next_initial_sync_id: 0,
			initial_sync_in_flight_bytes: 0,
			propagation_outboxes: HashMap::new(),
			in_flight_chunks: HashMap::new(),
			next_chunk_id: 0,
			propagation_in_flight_bytes: 0,
			parked_propagations: VecDeque::new(),
			pending_sends: FuturesUnordered::new(),
			deferred_peers: deferred,
			dropped_statements_during_sync: false,
			sync_recovery_peer: None,
			sync_recovery_readd_timeout: Box::pin(pending().fuse()),
		};

		flag.store(false, std::sync::atomic::Ordering::Relaxed);
		handler.drain_deferred_peers();

		assert!(handler.deferred_peers.is_empty());

		let added = network.get_added_reserved();
		assert_eq!(added.len(), 1);
		let added_addrs = &added[0];
		let expected_addr1: sc_network::Multiaddr =
			iter::once(multiaddr::Protocol::P2p(peer1.into())).collect();
		let expected_addr2: sc_network::Multiaddr =
			iter::once(multiaddr::Protocol::P2p(peer2.into())).collect();
		assert!(added_addrs.contains(&expected_addr1), "peer1 must be in added set");
		assert!(added_addrs.contains(&expected_addr2), "peer2 must be in added set");

		assert!(network.get_removed_reserved().is_empty());
	}

	#[tokio::test]
	async fn sync_recovery_schedules_remove_for_one_connected_peer() {
		let network = TestNetwork::new();
		let notification_service = TestNotificationService::new();
		let (sync, _flag) = TestSync::with_syncing(false);
		let (queue_sender, _) = async_channel::bounded(2);
		let statement_store = TestStatementStore::new();

		let connected_peer = PeerId::random();

		let mut peers = HashMap::new();
		peers.insert(
			connected_peer,
			Peer {
				rate_limiter: PeerRateLimiter::new(
					NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND)
						.expect("DEFAULT_STATEMENTS_PER_SECOND is nonzero"),
					NonZeroU32::new(
						DEFAULT_STATEMENTS_PER_SECOND * config::STATEMENTS_BURST_COEFFICIENT,
					)
					.expect("burst capacity is nonzero"),
				),
				protocol_version: PeerProtocolVersion::V1,
				topic_affinity: None,
				is_light: false,
				pending_topic_affinity: None,
				sync_watermark: 0,
			},
		);

		let mut handler = StatementHandler {
			protocol_name: format!("/{STATEMENT_PROTOCOL_V1}").into(),
			notification_service: Box::new(notification_service),
			propagate_timeout: (Box::pin(futures::stream::pending())
				as Pin<Box<dyn Stream<Item = ()> + Send>>)
				.fuse(),
			pending_statements: FuturesUnordered::new(),
			pending_statements_peers: HashMap::new(),
			recently_received_statements: HashMap::new(),
			network: network.clone(),
			sync,
			sync_event_stream: (Box::pin(futures::stream::pending())
				as Pin<Box<dyn Stream<Item = sc_network_sync::types::SyncEvent> + Send>>)
				.fuse(),
			peers,
			statement_store: Arc::new(statement_store),
			queue_sender,
			statements_per_second: NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND)
				.expect("DEFAULT_STATEMENTS_PER_SECOND is nonzero"),
			metrics: None,
			initial_sync_timeout: Box::pin(futures::future::pending()),
			pending_affinities_timeout: Box::pin(futures::future::pending()),
			pending_initial_syncs: HashMap::new(),
			initial_sync_peer_queue: VecDeque::new(),
			next_initial_sync_id: 0,
			initial_sync_in_flight_bytes: 0,
			propagation_outboxes: HashMap::new(),
			in_flight_chunks: HashMap::new(),
			next_chunk_id: 0,
			propagation_in_flight_bytes: 0,
			parked_propagations: VecDeque::new(),
			pending_sends: FuturesUnordered::new(),
			deferred_peers: HashSet::new(),
			dropped_statements_during_sync: true,
			sync_recovery_peer: None,
			sync_recovery_readd_timeout: Box::pin(futures::future::pending()),
		};

		handler.start_sync_recovery();

		// One remove call must have been issued for the connected peer
		{
			let removed = network.removed_reserved.lock().unwrap();
			assert_eq!(
				removed.len(),
				1,
				"Expected exactly one remove_peers_from_reserved_set call"
			);
			assert!(removed[0].contains(&connected_peer));
		}

		// The recovery peer must be stored and the timeout future must be armed
		assert_eq!(handler.sync_recovery_peer, Some(connected_peer));

		// Calling try_readd_sync_recovery_peer directly (as the select arm would after the future
		// resolves) must re-add the peer and clear the field
		handler.try_readd_sync_recovery_peer();
		assert!(handler.sync_recovery_peer.is_none());
		{
			let added = network.added_reserved.lock().unwrap();
			assert_eq!(added.len(), 1);
			let expected_addr: multiaddr::Multiaddr =
				iter::once(multiaddr::Protocol::P2p(connected_peer.into())).collect();
			assert!(added[0].contains(&expected_addr));
		}

		// Re-entry guard: restore state to simulate a second sync-end while recovery is still
		// in flight (sync_recovery_peer is Some). The second call must not issue another remove.
		{
			let peer2 = PeerId::random();
			handler.sync_recovery_peer = Some(peer2);
			handler.start_sync_recovery();
			assert_eq!(
				handler.sync_recovery_peer,
				Some(peer2),
				"Re-entry guard: recovery peer must not change on second call"
			);
			assert_eq!(
				network.removed_reserved.lock().unwrap().len(),
				1,
				"Re-entry guard: no extra remove call while recovery is in flight"
			);
		}
	}

	#[tokio::test]
	async fn sync_recovery_gated_by_dropped_statements_flag() {
		let make_peer = || Peer {
			rate_limiter: PeerRateLimiter::new(
				NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND)
					.expect("DEFAULT_STATEMENTS_PER_SECOND is nonzero"),
				NonZeroU32::new(
					DEFAULT_STATEMENTS_PER_SECOND * config::STATEMENTS_BURST_COEFFICIENT,
				)
				.expect("burst capacity is nonzero"),
			),
			protocol_version: PeerProtocolVersion::V1,
			topic_affinity: None,
			is_light: false,
			pending_topic_affinity: None,
			sync_watermark: 0,
		};

		let make_handler =
			|network: TestNetwork, dropped: bool| -> StatementHandler<TestNetwork, TestSync> {
				let (sync, _) = TestSync::with_syncing(false);
				let (queue_sender, _) = async_channel::bounded(2);
				let mut peers = HashMap::new();
				peers.insert(PeerId::random(), make_peer());
				StatementHandler {
					protocol_name: format!("/{STATEMENT_PROTOCOL_V1}").into(),
					notification_service: Box::new(TestNotificationService::new()),
					propagate_timeout: (Box::pin(futures::stream::pending())
						as Pin<Box<dyn Stream<Item = ()> + Send>>)
						.fuse(),
					pending_statements: FuturesUnordered::new(),
					pending_statements_peers: HashMap::new(),
					recently_received_statements: HashMap::new(),
					network,
					sync,
					sync_event_stream: (Box::pin(futures::stream::pending())
						as Pin<Box<dyn Stream<Item = sc_network_sync::types::SyncEvent> + Send>>)
						.fuse(),
					peers,
					statement_store: Arc::new(TestStatementStore::new()),
					queue_sender,
					statements_per_second: NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND)
						.expect("DEFAULT_STATEMENTS_PER_SECOND is nonzero"),
					metrics: None,
					initial_sync_timeout: Box::pin(futures::future::pending()),
					pending_affinities_timeout: Box::pin(futures::future::pending()),
					pending_initial_syncs: HashMap::new(),
					initial_sync_peer_queue: VecDeque::new(),
					next_initial_sync_id: 0,
					initial_sync_in_flight_bytes: 0,
					propagation_outboxes: HashMap::new(),
					in_flight_chunks: HashMap::new(),
					next_chunk_id: 0,
					propagation_in_flight_bytes: 0,
					parked_propagations: VecDeque::new(),
					pending_sends: FuturesUnordered::new(),
					deferred_peers: HashSet::new(),
					dropped_statements_during_sync: dropped,
					sync_recovery_peer: None,
					sync_recovery_readd_timeout: Box::pin(pending().fuse()),
				}
			};

		// flag=false → no recovery
		let net = TestNetwork::new();
		let mut handler = make_handler(net.clone(), false);
		handler.start_sync_recovery();
		assert!(handler.sync_recovery_peer.is_none());
		assert!(net.get_removed_reserved().is_empty());

		// flag=true → recovery fires
		let net2 = TestNetwork::new();
		let mut handler2 = make_handler(net2.clone(), true);
		handler2.start_sync_recovery();
		assert!(handler2.sync_recovery_peer.is_some());
		assert_eq!(net2.get_removed_reserved().len(), 1);
	}

	#[test]
	fn send_paths_skip_expired_statements() {
		let mut live = new_live_statement();
		live.set_expiry_from_parts(u32::MAX, 0);
		live.set_plain_data(vec![1u8; 16]);
		let live_hash = live.hash();

		let mut stale = Statement::new();
		stale.set_plain_data(vec![2u8; 16]);
		let stale_hash = stale.hash();

		let store = TestStatementStore::new();
		store.insert(stale.clone());
		store.insert(live.clone());

		let peer = Peer {
			rate_limiter: PeerRateLimiter::new(
				NonZeroU32::new(DEFAULT_STATEMENTS_PER_SECOND)
					.expect("DEFAULT_STATEMENTS_PER_SECOND is nonzero"),
				NonZeroU32::new(
					DEFAULT_STATEMENTS_PER_SECOND * config::STATEMENTS_BURST_COEFFICIENT,
				)
				.expect("burst capacity is nonzero"),
			),
			protocol_version: PeerProtocolVersion::V1,
			topic_affinity: None,
			is_light: false,
			pending_topic_affinity: None,
			sync_watermark: 0,
		};
		let who = PeerId::random();
		let received = HashMap::new();
		let pending = HashMap::new();
		let max_size = max_statement_payload_size(V1_ENVELOPE_OVERHEAD);

		let (statements, _processed, _size) = fetch_statement_chunk(
			&store,
			&received,
			&pending,
			&who,
			&peer,
			&[stale_hash, live_hash],
			max_size,
		)
		.expect("the test store never fails a fetch");
		assert_eq!(statements.iter().map(|(hash, _)| *hash).collect::<Vec<_>>(), vec![live_hash],);

		let watermark = store.admission_watermark().expect("watermark is readable");
		let (batch, _size) =
			fetch_admitted_chunk(&store, &received, &pending, &who, &peer, 0, watermark, max_size)
				.expect("the test store never fails a walk");
		assert_eq!(
			batch.statements.iter().map(|(hash, _)| *hash).collect::<Vec<_>>(),
			vec![live_hash],
		);
	}
}
