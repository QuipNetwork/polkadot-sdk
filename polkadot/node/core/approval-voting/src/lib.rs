// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! The Approval Voting Subsystem.
//!
//! This subsystem is responsible for determining candidates to do approval checks
//! on, performing those approval checks, and tracking the assignments and approvals
//! of others. It uses this information to determine when candidates and blocks have
//! been sufficiently approved to finalize.

use futures_timer::Delay;
use polkadot_node_primitives::{
	approval::{
		v1::{BlockApprovalMeta, DelayTranche},
		v2::{
			AssignmentCertKindV2, BitfieldError, CandidateBitfield, CoreBitfield,
			IndirectAssignmentCertV2, IndirectSignedApprovalVoteV2,
		},
	},
	ValidationResult, DISPUTE_WINDOW,
};
use polkadot_node_subsystem::{
	errors::RecoveryError,
	messages::{
		ApprovalCheckError, ApprovalCheckResult, ApprovalDistributionMessage,
		ApprovalVotingMessage, AssignmentCheckError, AssignmentCheckResult,
		AvailabilityRecoveryMessage, BlockDescription, CandidateValidationMessage, ChainApiMessage,
		ChainSelectionMessage, CheckedIndirectAssignment, CheckedIndirectSignedApprovalVote,
		DisputeCoordinatorMessage, HighestApprovedAncestorBlock, PvfExecKind, RuntimeApiMessage,
		RuntimeApiRequest,
	},
	overseer, FromOrchestra, OverseerSignal, SpawnedSubsystem, SubsystemError, SubsystemResult,
	SubsystemSender,
};
use polkadot_node_subsystem_util::{
	self,
	database::Database,
	metrics::{self, prometheus},
	runtime::{Config as RuntimeInfoConfig, ExtendedSessionInfo, RuntimeInfo},
	TimeoutExt,
};
use polkadot_primitives::{
	ApprovalVoteMultipleCandidates, BlockNumber, CandidateHash, CandidateIndex,
	CandidateReceiptV2 as CandidateReceipt, CoalescedApprovalCandidateHashes, CoreIndex,
	GroupIndex, Hash, SessionIndex, SessionInfo, ValidatorId, ValidatorIndex, ValidatorPair,
	ValidatorSignature,
};
use sc_keystore::LocalKeystore;
use sp_application_crypto::Pair;
use sp_consensus::SyncOracle;
use sp_consensus_slots::Slot;
use std::time::Instant;

// The max number of blocks we keep track of assignments gathering times. Normally,
// this would never be reached because we prune the data on finalization, but we need
// to also ensure the data is not growing unecessarily large.
const MAX_BLOCKS_WITH_ASSIGNMENT_TIMESTAMPS: u32 = 100;

use futures::{
	channel::oneshot,
	future::{BoxFuture, RemoteHandle},
	prelude::*,
	stream::FuturesUnordered,
	StreamExt,
};

use std::{
	cmp::min,
	collections::{
		btree_map::Entry as BTMEntry, hash_map::Entry as HMEntry, BTreeMap, HashMap, HashSet,
	},
	sync::Arc,
	time::Duration,
};

use schnellru::{ByLength, LruMap};

use approval_checking::RequiredTranches;
use bitvec::{order::Lsb0, vec::BitVec};
pub use criteria::{AssignmentCriteria, Config as AssignmentConfig, RealAssignmentCriteria};
use persisted_entries::{ApprovalEntry, BlockEntry, CandidateEntry};
use polkadot_node_primitives::approval::time::{
	slot_number_to_tick, Clock, ClockExt, DelayedApprovalTimer, SystemClock, Tick,
};

mod approval_checking;
pub mod approval_db;
mod backend;
pub mod criteria;
mod import;
mod ops;
mod persisted_entries;

use crate::{
	approval_checking::{Check, TranchesToApproveResult},
	approval_db::common::{Config as DatabaseConfig, DbBackend},
	backend::{Backend, OverlayedBackend},
	criteria::InvalidAssignmentReason,
	persisted_entries::OurApproval,
};

#[cfg(test)]
mod tests;

const APPROVAL_CHECKING_TIMEOUT: Duration = Duration::from_secs(120);
/// How long are we willing to wait for approval signatures?
///
/// Value rather arbitrarily: Should not be hit in practice, it exists to more easily diagnose dead
/// lock issues for example.
const WAIT_FOR_SIGS_TIMEOUT: Duration = Duration::from_millis(500);
const APPROVAL_CACHE_SIZE: u32 = 1024;

/// The maximum number of times we retry to approve a block if is still needed.
const MAX_APPROVAL_RETRIES: u32 = 16;

const APPROVAL_DELAY: Tick = 2;
pub(crate) const LOG_TARGET: &str = "parachain::approval-voting";

// The max number of ticks we delay sending the approval after we are ready to issue the approval
const MAX_APPROVAL_COALESCE_WAIT_TICKS: Tick = 12;

// If the node restarted and the tranche has passed without the assignment
// being trigger, we won't trigger the assignment at restart because we don't have
// an wakeup schedule for it.
// The solution, is to always schedule a wake up after the restart and let the
// process_wakeup to decide if the assignment needs to be triggered.
// We need to have a delay after restart to give time to the node to catch up with
// messages and not trigger its assignment unnecessarily, because it hasn't seen
// the assignments from the other validators.
const RESTART_WAKEUP_DELAY: Tick = 12;

/// Configuration for the approval voting subsystem
#[derive(Debug, Clone)]
pub struct Config {
	/// The column family in the DB where approval-voting data is stored.
	pub col_approval_data: u32,
	/// The slot duration of the consensus algorithm, in milliseconds. Should be evenly
	/// divisible by 500.
	pub slot_duration_millis: u64,
}

// The mode of the approval voting subsystem. It should start in a `Syncing` mode when it first
// starts, and then once it's reached the head of the chain it should move into the `Active` mode.
//
// In `Active` mode, the node is an active participant in the approvals protocol. When syncing,
// the node follows the new incoming blocks and finalized number, but does not yet participate.
//
// When transitioning from `Syncing` to `Active`, the node notifies the `ApprovalDistribution`
// subsystem of all unfinalized blocks and the candidates included within them, as well as all
// votes that the local node itself has cast on candidates within those blocks.
enum Mode {
	Active,
	Syncing(Box<dyn SyncOracle + Send>),
}

/// The approval voting subsystem.
pub struct ApprovalVotingSubsystem {
	/// `LocalKeystore` is needed for assignment keys, but not necessarily approval keys.
	///
	/// We do a lot of VRF signing and need the keys to have low latency.
	keystore: Arc<LocalKeystore>,
	db_config: DatabaseConfig,
	slot_duration_millis: u64,
	db: Arc<dyn Database>,
	mode: Mode,
	metrics: Metrics,
	clock: Arc<dyn Clock + Send + Sync>,
	spawner: Arc<dyn overseer::gen::Spawner + 'static>,
	/// The maximum time we retry to approve a block if it is still needed and PoV fetch failed.
	max_approval_retries: u32,
	/// The backoff before we retry the approval.
	retry_backoff: Duration,
}

#[derive(Clone)]
struct MetricsInner {
	imported_candidates_total: prometheus::Counter<prometheus::U64>,
	assignments_produced: prometheus::Histogram,
	approvals_produced_total: prometheus::CounterVec<prometheus::U64>,
	no_shows_total: prometheus::Counter<prometheus::U64>,
	// The difference from `no_shows_total` is that this counts all observed no-shows at any
	// moment in time. While `no_shows_total` catches that the no-shows at the moment the candidate
	// is approved, approvals might arrive late and `no_shows_total` wouldn't catch that number.
	observed_no_shows: prometheus::Counter<prometheus::U64>,
	approved_by_one_third: prometheus::Counter<prometheus::U64>,
	wakeups_triggered_total: prometheus::Counter<prometheus::U64>,
	coalesced_approvals_buckets: prometheus::Histogram,
	coalesced_approvals_delay: prometheus::Histogram,
	candidate_approval_time_ticks: prometheus::Histogram,
	block_approval_time_ticks: prometheus::Histogram,
	time_db_transaction: prometheus::Histogram,
	time_recover_and_approve: prometheus::Histogram,
	candidate_signatures_requests_total: prometheus::Counter<prometheus::U64>,
	unapproved_candidates_in_unfinalized_chain: prometheus::Gauge<prometheus::U64>,
	// The time it takes in each stage to gather enough assignments.
	// We defined a `stage` as being the entire process of gathering enough assignments to
	// be able to approve a candidate:
	// E.g:
	// - Stage 0: We wait for the needed_approvals assignments to be gathered.
	// - Stage 1: We wait for enough tranches to cover all no-shows in stage 0.
	// - Stage 2: We wait for enough tranches to cover all no-shows  of stage 1.
	assignments_gathering_time_by_stage: prometheus::HistogramVec,
}

/// Approval Voting metrics.
#[derive(Default, Clone)]
pub struct Metrics(Option<MetricsInner>);

impl Metrics {
	fn on_candidate_imported(&self) {
		if let Some(metrics) = &self.0 {
			metrics.imported_candidates_total.inc();
		}
	}

	fn on_assignment_produced(&self, tranche: DelayTranche) {
		if let Some(metrics) = &self.0 {
			metrics.assignments_produced.observe(tranche as f64);
		}
	}

	fn on_approval_coalesce(&self, num_coalesced: u32) {
		if let Some(metrics) = &self.0 {
			// Count how many candidates we covered with this coalesced approvals,
			// so that the heat-map really gives a good understanding of the scales.
			for _ in 0..num_coalesced {
				metrics.coalesced_approvals_buckets.observe(num_coalesced as f64)
			}
		}
	}

	fn on_delayed_approval(&self, delayed_ticks: u64) {
		if let Some(metrics) = &self.0 {
			metrics.coalesced_approvals_delay.observe(delayed_ticks as f64)
		}
	}

	fn on_approval_stale(&self) {
		if let Some(metrics) = &self.0 {
			metrics.approvals_produced_total.with_label_values(&["stale"]).inc()
		}
	}

	fn on_approval_invalid(&self) {
		if let Some(metrics) = &self.0 {
			metrics.approvals_produced_total.with_label_values(&["invalid"]).inc()
		}
	}

	fn on_approval_unavailable(&self) {
		if let Some(metrics) = &self.0 {
			metrics.approvals_produced_total.with_label_values(&["unavailable"]).inc()
		}
	}

	fn on_approval_error(&self) {
		if let Some(metrics) = &self.0 {
			metrics.approvals_produced_total.with_label_values(&["internal error"]).inc()
		}
	}

	fn on_approval_produced(&self) {
		if let Some(metrics) = &self.0 {
			metrics.approvals_produced_total.with_label_values(&["success"]).inc()
		}
	}

	fn on_no_shows(&self, n: usize) {
		if let Some(metrics) = &self.0 {
			metrics.no_shows_total.inc_by(n as u64);
		}
	}

	fn on_observed_no_shows(&self, n: usize) {
		if let Some(metrics) = &self.0 {
			metrics.observed_no_shows.inc_by(n as u64);
		}
	}

	fn on_approved_by_one_third(&self) {
		if let Some(metrics) = &self.0 {
			metrics.approved_by_one_third.inc();
		}
	}

	fn on_wakeup(&self) {
		if let Some(metrics) = &self.0 {
			metrics.wakeups_triggered_total.inc();
		}
	}

	fn on_candidate_approved(&self, ticks: Tick) {
		if let Some(metrics) = &self.0 {
			metrics.candidate_approval_time_ticks.observe(ticks as f64);
		}
	}

	fn on_block_approved(&self, ticks: Tick) {
		if let Some(metrics) = &self.0 {
			metrics.block_approval_time_ticks.observe(ticks as f64);
		}
	}

	fn on_candidate_signatures_request(&self) {
		if let Some(metrics) = &self.0 {
			metrics.candidate_signatures_requests_total.inc();
		}
	}

	fn time_db_transaction(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.time_db_transaction.start_timer())
	}

	fn time_recover_and_approve(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.time_recover_and_approve.start_timer())
	}

	fn on_unapproved_candidates_in_unfinalized_chain(&self, count: usize) {
		if let Some(metrics) = &self.0 {
			metrics.unapproved_candidates_in_unfinalized_chain.set(count as u64);
		}
	}

	pub fn observe_assignment_gathering_time(&self, stage: usize, elapsed_as_millis: usize) {
		if let Some(metrics) = &self.0 {
			let stage_string = stage.to_string();
			// We don't want to have too many metrics entries with this label to not put unncessary
			// pressure on the metrics infrastructure, so we cap the stage at 10, which is
			// equivalent to having already a finalization lag to 10 * no_show_slots, so it should
			// be more than enough.
			metrics
				.assignments_gathering_time_by_stage
				.with_label_values(&[if stage < 10 { stage_string.as_str() } else { "inf" }])
				.observe(elapsed_as_millis as f64);
		}
	}
}

impl metrics::Metrics for Metrics {
	fn try_register(
		registry: &prometheus::Registry,
	) -> std::result::Result<Self, prometheus::PrometheusError> {
		let metrics = MetricsInner {
			imported_candidates_total: prometheus::register(
				prometheus::Counter::new(
					"polkadot_parachain_imported_candidates_total",
					"Number of candidates imported by the approval voting subsystem",
				)?,
				registry,
			)?,
			assignments_produced: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_assignments_produced",
						"Assignments and tranches produced by the approval voting subsystem",
					).buckets(vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 10.0, 15.0, 25.0, 40.0, 70.0]),
				)?,
				registry,
			)?,
			approvals_produced_total: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"polkadot_parachain_approvals_produced_total",
						"Number of approvals produced by the approval voting subsystem",
					),
					&["status"]
				)?,
				registry,
			)?,
			no_shows_total: prometheus::register(
				prometheus::Counter::new(
					"polkadot_parachain_approvals_no_shows_total",
					"Number of assignments which became no-shows in the approval voting subsystem",
				)?,
				registry,
			)?,
			observed_no_shows: prometheus::register(
				prometheus::Counter::new(
					"polkadot_parachain_approvals_observed_no_shows_total",
					"Number of observed no shows at any moment in time",
				)?,
				registry,
			)?,
			wakeups_triggered_total: prometheus::register(
				prometheus::Counter::new(
					"polkadot_parachain_approvals_wakeups_total",
					"Number of times we woke up to process a candidate in the approval voting subsystem",
				)?,
				registry,
			)?,
			candidate_approval_time_ticks: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_approvals_candidate_approval_time_ticks",
						"Number of ticks (500ms) to approve candidates.",
					).buckets(vec![6.0, 12.0, 18.0, 24.0, 30.0, 36.0, 72.0, 100.0, 144.0]),
				)?,
				registry,
			)?,
			coalesced_approvals_buckets: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_approvals_coalesced_approvals_buckets",
						"Number of coalesced approvals.",
					).buckets(vec![1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, 8.5, 9.5]),
				)?,
				registry,
			)?,
			coalesced_approvals_delay: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_approvals_coalescing_delay",
						"Number of ticks we delay the sending of a candidate approval",
					).buckets(vec![1.1, 2.1, 3.1, 4.1, 6.1, 8.1, 12.1, 20.1, 32.1]),
				)?,
				registry,
			)?,
			approved_by_one_third: prometheus::register(
				prometheus::Counter::new(
					"polkadot_parachain_approved_by_one_third",
					"Number of candidates where more than one third had to vote ",
				)?,
				registry,
			)?,
			block_approval_time_ticks: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_approvals_blockapproval_time_ticks",
						"Number of ticks (500ms) to approve blocks.",
					).buckets(vec![6.0, 12.0, 18.0, 24.0, 30.0, 36.0, 72.0, 100.0, 144.0]),
				)?,
				registry,
			)?,
			time_db_transaction: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_time_approval_db_transaction",
						"Time spent writing an approval db transaction.",
					)
				)?,
				registry,
			)?,
			time_recover_and_approve: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_time_recover_and_approve",
						"Time spent recovering and approving data in approval voting",
					)
				)?,
				registry,
			)?,
			candidate_signatures_requests_total: prometheus::register(
				prometheus::Counter::new(
					"polkadot_parachain_approval_candidate_signatures_requests_total",
					"Number of times signatures got requested by other subsystems",
				)?,
				registry,
			)?,
			unapproved_candidates_in_unfinalized_chain: prometheus::register(
				prometheus::Gauge::new(
					"polkadot_parachain_approval_unapproved_candidates_in_unfinalized_chain",
					"Number of unapproved candidates in unfinalized chain",
				)?,
				registry,
			)?,
			assignments_gathering_time_by_stage: prometheus::register(
				prometheus::HistogramVec::new(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_assignments_gather_time_by_stage_ms",
						"The time in ms it takes for each stage to gather enough assignments needed for approval",
					)
					.buckets(vec![0.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0, 32000.0]),
					&["stage"],
				)?,
				registry,
			)?,
		};

		Ok(Metrics(Some(metrics)))
	}
}

impl ApprovalVotingSubsystem {
	/// Create a new approval voting subsystem with the given keystore, config, and database.
	pub fn with_config(
		config: Config,
		db: Arc<dyn Database>,
		keystore: Arc<LocalKeystore>,
		sync_oracle: Box<dyn SyncOracle + Send>,
		metrics: Metrics,
		spawner: Arc<dyn overseer::gen::Spawner + 'static>,
	) -> Self {
		ApprovalVotingSubsystem::with_config_and_clock(
			config,
			db,
			keystore,
			sync_oracle,
			metrics,
			Arc::new(SystemClock {}),
			spawner,
			MAX_APPROVAL_RETRIES,
			APPROVAL_CHECKING_TIMEOUT / 2,
		)
	}

	/// Create a new approval voting subsystem with the given keystore, config, and database.
	pub fn with_config_and_clock(
		config: Config,
		db: Arc<dyn Database>,
		keystore: Arc<LocalKeystore>,
		sync_oracle: Box<dyn SyncOracle + Send>,
		metrics: Metrics,
		clock: Arc<dyn Clock + Send + Sync>,
		spawner: Arc<dyn overseer::gen::Spawner + 'static>,
		max_approval_retries: u32,
		retry_backoff: Duration,
	) -> Self {
		ApprovalVotingSubsystem {
			keystore,
			slot_duration_millis: config.slot_duration_millis,
			db,
			db_config: DatabaseConfig { col_approval_data: config.col_approval_data },
			mode: Mode::Syncing(sync_oracle),
			metrics,
			clock,
			spawner,
			max_approval_retries,
			retry_backoff,
		}
	}

	/// Revert to the block corresponding to the specified `hash`.
	/// The operation is not allowed for blocks older than the last finalized one.
	pub fn revert_to(&self, hash: Hash) -> Result<(), SubsystemError> {
		let config =
			approval_db::common::Config { col_approval_data: self.db_config.col_approval_data };
		let mut backend = approval_db::common::DbBackend::new(self.db.clone(), config);
		let mut overlay = OverlayedBackend::new(&backend);

		ops::revert_to(&mut overlay, hash)?;

		let ops = overlay.into_write_ops();
		backend.write(ops)
	}
}

// Checks and logs approval vote db state. It is perfectly normal to start with an
// empty approval vote DB if we changed DB type or the node will sync from scratch.
fn db_sanity_check(db: Arc<dyn Database>, config: DatabaseConfig) -> SubsystemResult<()> {
	let backend = DbBackend::new(db, config);
	let all_blocks = backend.load_all_blocks()?;

	if all_blocks.is_empty() {
		gum::info!(target: LOG_TARGET, "Starting with an empty approval vote DB.",);
	} else {
		gum::debug!(
			target: LOG_TARGET,
			"Starting with {} blocks in approval vote DB.",
			all_blocks.len()
		);
	}

	Ok(())
}

#[overseer::subsystem(ApprovalVoting, error = SubsystemError, prefix = self::overseer)]
impl<Context: Send> ApprovalVotingSubsystem {
	fn start(self, mut ctx: Context) -> SpawnedSubsystem {
		let backend = DbBackend::new(self.db.clone(), self.db_config);
		let to_other_subsystems = ctx.sender().clone();
		let to_approval_distr = ctx.sender().clone();

		let future = run::<DbBackend, _, _, _>(
			ctx,
			to_other_subsystems,
			to_approval_distr,
			self,
			Box::new(RealAssignmentCriteria),
			backend,
		)
		.map_err(|e| SubsystemError::with_origin("approval-voting", e))
		.boxed();

		SpawnedSubsystem { name: "approval-voting-subsystem", future }
	}
}

#[derive(Debug, Clone)]
struct ApprovalVoteRequest {
	validator_index: ValidatorIndex,
	block_hash: Hash,
}

#[derive(Default)]
struct Wakeups {
	// Tick -> [(Relay Block, Candidate Hash)]
	wakeups: BTreeMap<Tick, Vec<(Hash, CandidateHash)>>,
	reverse_wakeups: HashMap<(Hash, CandidateHash), Tick>,
	block_numbers: BTreeMap<BlockNumber, HashSet<Hash>>,
}

impl Wakeups {
	// Returns the first tick there exist wakeups for, if any.
	fn first(&self) -> Option<Tick> {
		self.wakeups.keys().next().map(|t| *t)
	}

	fn note_block(&mut self, block_hash: Hash, block_number: BlockNumber) {
		self.block_numbers.entry(block_number).or_default().insert(block_hash);
	}

	// Schedules a wakeup at the given tick. no-op if there is already an earlier or equal wake-up
	// for these values. replaces any later wakeup.
	fn schedule(
		&mut self,
		block_hash: Hash,
		block_number: BlockNumber,
		candidate_hash: CandidateHash,
		tick: Tick,
	) {
		if let Some(prev) = self.reverse_wakeups.get(&(block_hash, candidate_hash)) {
			if prev <= &tick {
				return;
			}

			// we are replacing previous wakeup with an earlier one.
			if let BTMEntry::Occupied(mut entry) = self.wakeups.entry(*prev) {
				if let Some(pos) =
					entry.get().iter().position(|x| x == &(block_hash, candidate_hash))
				{
					entry.get_mut().remove(pos);
				}

				if entry.get().is_empty() {
					let _ = entry.remove_entry();
				}
			}
		} else {
			self.note_block(block_hash, block_number);
		}

		self.reverse_wakeups.insert((block_hash, candidate_hash), tick);
		self.wakeups.entry(tick).or_default().push((block_hash, candidate_hash));
	}

	fn prune_finalized_wakeups(&mut self, finalized_number: BlockNumber) {
		let after = self.block_numbers.split_off(&(finalized_number + 1));
		let pruned_blocks: HashSet<_> = std::mem::replace(&mut self.block_numbers, after)
			.into_iter()
			.flat_map(|(_number, hashes)| hashes)
			.collect();

		let mut pruned_wakeups = BTreeMap::new();
		self.reverse_wakeups.retain(|(h, c_h), tick| {
			let live = !pruned_blocks.contains(h);
			if !live {
				pruned_wakeups.entry(*tick).or_insert_with(HashSet::new).insert((*h, *c_h));
			}
			live
		});

		for (tick, pruned) in pruned_wakeups {
			if let BTMEntry::Occupied(mut entry) = self.wakeups.entry(tick) {
				entry.get_mut().retain(|wakeup| !pruned.contains(wakeup));
				if entry.get().is_empty() {
					let _ = entry.remove();
				}
			}
		}
	}

	// Get the wakeup for a particular block/candidate combo, if any.
	fn wakeup_for(&self, block_hash: Hash, candidate_hash: CandidateHash) -> Option<Tick> {
		self.reverse_wakeups.get(&(block_hash, candidate_hash)).map(|t| *t)
	}

	// Returns the next wakeup. this future never returns if there are no wakeups.
	async fn next(&mut self, clock: &(dyn Clock + Sync)) -> (Tick, Hash, CandidateHash) {
		match self.first() {
			None => future::pending().await,
			Some(tick) => {
				clock.wait(tick).await;
				match self.wakeups.entry(tick) {
					BTMEntry::Vacant(_) => {
						panic!("entry is known to exist since `first` was `Some`; qed")
					},
					BTMEntry::Occupied(mut entry) => {
						let (hash, candidate_hash) = entry.get_mut().pop()
							.expect("empty entries are removed here and in `schedule`; no other mutation of this map; qed");

						if entry.get().is_empty() {
							let _ = entry.remove();
						}

						self.reverse_wakeups.remove(&(hash, candidate_hash));

						(tick, hash, candidate_hash)
					},
				}
			},
		}
	}
}

struct ApprovalStatus {
	required_tranches: RequiredTranches,
	tranche_now: DelayTranche,
	block_tick: Tick,
	last_no_shows: usize,
	no_show_validators: Vec<ValidatorIndex>,
}

#[derive(Copy, Clone)]
enum ApprovalOutcome {
	Approved,
	Failed,
	TimedOut,
}

#[derive(Clone)]
struct RetryApprovalInfo {
	candidate: CandidateReceipt,
	backing_group: GroupIndex,
	core_index: Option<CoreIndex>,
	session_index: SessionIndex,
	attempts_remaining: u32,
	backoff: Duration,
}

struct ApprovalState {
	validator_index: ValidatorIndex,
	candidate_hash: CandidateHash,
	approval_outcome: ApprovalOutcome,
	retry_info: Option<RetryApprovalInfo>,
}

impl ApprovalState {
	fn approved(validator_index: ValidatorIndex, candidate_hash: CandidateHash) -> Self {
		Self {
			validator_index,
			candidate_hash,
			approval_outcome: ApprovalOutcome::Approved,
			retry_info: None,
		}
	}
	fn failed(validator_index: ValidatorIndex, candidate_hash: CandidateHash) -> Self {
		Self {
			validator_index,
			candidate_hash,
			approval_outcome: ApprovalOutcome::Failed,
			retry_info: None,
		}
	}

	fn failed_with_retry(
		validator_index: ValidatorIndex,
		candidate_hash: CandidateHash,
		retry_info: Option<RetryApprovalInfo>,
	) -> Self {
		Self {
			validator_index,
			candidate_hash,
			approval_outcome: ApprovalOutcome::Failed,
			retry_info,
		}
	}
}

struct CurrentlyCheckingSet {
	candidate_hash_map: HashMap<CandidateHash, HashSet<Hash>>,
	currently_checking: FuturesUnordered<BoxFuture<'static, ApprovalState>>,
}

impl Default for CurrentlyCheckingSet {
	fn default() -> Self {
		Self { candidate_hash_map: HashMap::new(), currently_checking: FuturesUnordered::new() }
	}
}

impl CurrentlyCheckingSet {
	// This function will lazily launch approval voting work whenever the
	// candidate is not already undergoing validation.
	pub async fn insert_relay_block_hash(
		&mut self,
		candidate_hash: CandidateHash,
		validator_index: ValidatorIndex,
		relay_block: Hash,
		launch_work: impl Future<Output = SubsystemResult<RemoteHandle<ApprovalState>>>,
	) -> SubsystemResult<()> {
		match self.candidate_hash_map.entry(candidate_hash) {
			HMEntry::Occupied(mut entry) => {
				// validation already undergoing. just add the relay hash if unknown.
				entry.get_mut().insert(relay_block);
			},
			HMEntry::Vacant(entry) => {
				// validation not ongoing. launch work and time out the remote handle.
				entry.insert(HashSet::new()).insert(relay_block);
				let work = launch_work.await?;
				self.currently_checking.push(Box::pin(async move {
					match work.timeout(APPROVAL_CHECKING_TIMEOUT).await {
						None => ApprovalState {
							candidate_hash,
							validator_index,
							approval_outcome: ApprovalOutcome::TimedOut,
							retry_info: None,
						},
						Some(approval_state) => approval_state,
					}
				}));
			},
		}

		Ok(())
	}

	pub async fn next(
		&mut self,
		approvals_cache: &mut LruMap<CandidateHash, ApprovalOutcome>,
	) -> (HashSet<Hash>, ApprovalState) {
		if !self.currently_checking.is_empty() {
			if let Some(approval_state) = self.currently_checking.next().await {
				let out = self
					.candidate_hash_map
					.remove(&approval_state.candidate_hash)
					.unwrap_or_default();
				approvals_cache
					.insert(approval_state.candidate_hash, approval_state.approval_outcome);
				return (out, approval_state);
			}
		}

		future::pending().await
	}
}

async fn get_extended_session_info_by_index<'a, Sender>(
	runtime_info: &'a mut RuntimeInfo,
	sender: &mut Sender,
	block_hash: Hash,
	session_index: SessionIndex,
) -> Option<&'a ExtendedSessionInfo>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	match runtime_info.get_session_info_by_index(sender, block_hash, session_index).await {
		Ok(extended_info) => Some(&extended_info),
		Err(_) => {
			gum::debug!(
				target: LOG_TARGET,
				session = session_index,
				?block_hash,
				"Can't obtain SessionInfo or ExecutorParams"
			);
			None
		},
	}
}

async fn get_session_info_by_index<'a, Sender>(
	runtime_info: &'a mut RuntimeInfo,
	sender: &mut Sender,
	block_hash: Hash,
	session_index: SessionIndex,
) -> Option<&'a SessionInfo>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	get_extended_session_info_by_index(runtime_info, sender, block_hash, session_index)
		.await
		.map(|extended_info| &extended_info.session_info)
}

struct State {
	keystore: Arc<LocalKeystore>,
	slot_duration_millis: u64,
	clock: Arc<dyn Clock + Send + Sync>,
	assignment_criteria: Box<dyn AssignmentCriteria + Send + Sync>,
	// Per block, candidate records about how long we take until we gather enough
	// assignments, this is relevant because it gives us a good idea about how many
	// tranches we trigger and why.
	per_block_assignments_gathering_times:
		LruMap<BlockNumber, HashMap<(Hash, CandidateHash), AssignmentGatheringRecord>>,
	no_show_stats: NoShowStats,
}

// Regularly dump the no-show stats at this block number frequency.
const NO_SHOW_DUMP_FREQUENCY: BlockNumber = 50;
// The maximum number of validators we record no-shows for, per candidate.
pub(crate) const MAX_RECORDED_NO_SHOW_VALIDATORS_PER_CANDIDATE: usize = 20;

// No show stats per validator and per parachain.
// This is valuable information when we have to debug live network issue, because
// it gives information if things are going wrong only for some validators or just
// for some parachains.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct NoShowStats {
	per_validator_no_show: HashMap<SessionIndex, HashMap<ValidatorIndex, usize>>,
	per_parachain_no_show: HashMap<u32, usize>,
	last_dumped_block_number: BlockNumber,
}

impl NoShowStats {
	// Print the no-show stats if NO_SHOW_DUMP_FREQUENCY blocks have passed since the last
	// print.
	fn maybe_print(&mut self, current_block_number: BlockNumber) {
		if self.last_dumped_block_number > current_block_number ||
			current_block_number - self.last_dumped_block_number < NO_SHOW_DUMP_FREQUENCY
		{
			return;
		}
		if self.per_parachain_no_show.is_empty() && self.per_validator_no_show.is_empty() {
			return;
		}

		gum::debug!(
			target: LOG_TARGET,
			"Validators with no_show {:?} and parachains with no_shows {:?} since {:}",
			self.per_validator_no_show,
			self.per_parachain_no_show,
			self.last_dumped_block_number
		);

		self.last_dumped_block_number = current_block_number;

		self.per_validator_no_show.clear();
		self.per_parachain_no_show.clear();
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AssignmentGatheringRecord {
	// The stage we are in.
	// Candidate assignment gathering goes in stages, first we wait for needed_approvals(stage 0)
	// Then if we have no-shows, we move into stage 1 and wait for enough tranches to cover all
	// no-shows.
	stage: usize,
	// The time we started the stage.
	stage_start: Option<Instant>,
}

impl Default for AssignmentGatheringRecord {
	fn default() -> Self {
		AssignmentGatheringRecord { stage: 0, stage_start: Some(Instant::now()) }
	}
}

#[overseer::contextbounds(ApprovalVoting, prefix = self::overseer)]
impl State {
	// Compute the required tranches for approval for this block and candidate combo.
	// Fails if there is no approval entry for the block under the candidate or no candidate entry
	// under the block, or if the session is out of bounds.
	async fn approval_status<Sender, 'a, 'b>(
		&'a self,
		sender: &mut Sender,
		session_info_provider: &'a mut RuntimeInfo,
		block_entry: &'a BlockEntry,
		candidate_entry: &'b CandidateEntry,
	) -> Option<(&'b ApprovalEntry, ApprovalStatus)>
	where
		Sender: SubsystemSender<RuntimeApiMessage>,
	{
		let session_info = match get_session_info_by_index(
			session_info_provider,
			sender,
			block_entry.parent_hash(),
			block_entry.session(),
		)
		.await
		{
			Some(s) => s,
			None => return None,
		};
		let block_hash = block_entry.block_hash();

		let tranche_now = self.clock.tranche_now(self.slot_duration_millis, block_entry.slot());
		let block_tick = slot_number_to_tick(self.slot_duration_millis, block_entry.slot());
		let no_show_duration = slot_number_to_tick(
			self.slot_duration_millis,
			Slot::from(u64::from(session_info.no_show_slots)),
		);

		if let Some(approval_entry) = candidate_entry.approval_entry(&block_hash) {
			let TranchesToApproveResult {
				required_tranches,
				total_observed_no_shows,
				no_show_validators,
			} = approval_checking::tranches_to_approve(
				approval_entry,
				candidate_entry.approvals(),
				tranche_now,
				block_tick,
				no_show_duration,
				session_info.needed_approvals as _,
			);

			let status = ApprovalStatus {
				required_tranches,
				block_tick,
				tranche_now,
				last_no_shows: total_observed_no_shows,
				no_show_validators,
			};

			Some((approval_entry, status))
		} else {
			None
		}
	}

	fn mark_begining_of_gathering_assignments(
		&mut self,
		block_number: BlockNumber,
		block_hash: Hash,
		candidate: CandidateHash,
	) {
		if let Some(record) = self
			.per_block_assignments_gathering_times
			.get_or_insert(block_number, HashMap::new)
			.and_then(|records| Some(records.entry((block_hash, candidate)).or_default()))
		{
			if record.stage_start.is_none() {
				record.stage += 1;
				gum::debug!(
					target: LOG_TARGET,
					stage = ?record.stage,
					?block_hash,
					?candidate,
					"Started a new assignment gathering stage",
				);
				record.stage_start = Some(Instant::now());
			}
		}
	}

	fn mark_gathered_enough_assignments(
		&mut self,
		block_number: BlockNumber,
		block_hash: Hash,
		candidate: CandidateHash,
	) -> AssignmentGatheringRecord {
		let record = self
			.per_block_assignments_gathering_times
			.get(&block_number)
			.and_then(|entry| entry.get_mut(&(block_hash, candidate)));
		let stage = record.as_ref().map(|record| record.stage).unwrap_or_default();
		AssignmentGatheringRecord {
			stage,
			stage_start: record.and_then(|record| record.stage_start.take()),
		}
	}

	fn cleanup_assignments_gathering_timestamp(&mut self, remove_lower_than: BlockNumber) {
		while let Some((block_number, _)) = self.per_block_assignments_gathering_times.peek_oldest()
		{
			if *block_number < remove_lower_than {
				self.per_block_assignments_gathering_times.pop_oldest();
			} else {
				break;
			}
		}
	}

	fn observe_assignment_gathering_status(
		&mut self,
		metrics: &Metrics,
		required_tranches: &RequiredTranches,
		block_hash: Hash,
		block_number: BlockNumber,
		candidate_hash: CandidateHash,
	) {
		match required_tranches {
			RequiredTranches::All | RequiredTranches::Pending { .. } => {
				self.mark_begining_of_gathering_assignments(
					block_number,
					block_hash,
					candidate_hash,
				);
			},
			RequiredTranches::Exact { .. } => {
				let time_to_gather =
					self.mark_gathered_enough_assignments(block_number, block_hash, candidate_hash);
				if let Some(gathering_started) = time_to_gather.stage_start {
					if gathering_started.elapsed().as_millis() > 6000 {
						gum::trace!(
							target: LOG_TARGET,
							?block_hash,
							?candidate_hash,
							"Long assignment gathering time",
						);
					}
					metrics.observe_assignment_gathering_time(
						time_to_gather.stage,
						gathering_started.elapsed().as_millis() as usize,
					)
				}
			},
		}
	}

	fn record_no_shows(
		&mut self,
		session_index: SessionIndex,
		para_id: u32,
		no_show_validators: &Vec<ValidatorIndex>,
	) {
		if !no_show_validators.is_empty() {
			*self.no_show_stats.per_parachain_no_show.entry(para_id.into()).or_default() += 1;
		}
		for validator_index in no_show_validators {
			*self
				.no_show_stats
				.per_validator_no_show
				.entry(session_index)
				.or_default()
				.entry(*validator_index)
				.or_default() += 1;
		}
	}
}

#[derive(Debug, Clone)]
enum Action {
	ScheduleWakeup {
		block_hash: Hash,
		block_number: BlockNumber,
		candidate_hash: CandidateHash,
		tick: Tick,
	},
	LaunchApproval {
		claimed_candidate_indices: CandidateBitfield,
		candidate_hash: CandidateHash,
		indirect_cert: IndirectAssignmentCertV2,
		assignment_tranche: DelayTranche,
		relay_block_hash: Hash,
		session: SessionIndex,
		candidate: CandidateReceipt,
		backing_group: GroupIndex,
		distribute_assignment: bool,
		core_index: Option<CoreIndex>,
	},
	NoteApprovedInChainSelection(Hash),
	IssueApproval(CandidateHash, ApprovalVoteRequest),
	BecomeActive,
	Conclude,
}

/// Trait for providing approval voting subsystem with work.
#[async_trait::async_trait]
pub trait ApprovalVotingWorkProvider {
	async fn recv(&mut self) -> SubsystemResult<FromOrchestra<ApprovalVotingMessage>>;
}

#[async_trait::async_trait]
#[overseer::contextbounds(ApprovalVoting, prefix = self::overseer)]
impl<Context> ApprovalVotingWorkProvider for Context {
	async fn recv(&mut self) -> SubsystemResult<FromOrchestra<ApprovalVotingMessage>> {
		self.recv().await
	}
}

#[overseer::contextbounds(ApprovalVoting, prefix = self::overseer)]
async fn run<
	B,
	WorkProvider: ApprovalVotingWorkProvider,
	Sender: SubsystemSender<ChainApiMessage>
		+ SubsystemSender<RuntimeApiMessage>
		+ SubsystemSender<ChainSelectionMessage>
		+ SubsystemSender<AvailabilityRecoveryMessage>
		+ SubsystemSender<DisputeCoordinatorMessage>
		+ SubsystemSender<CandidateValidationMessage>
		+ Clone,
	ADSender: SubsystemSender<ApprovalDistributionMessage>,
>(
	mut work_provider: WorkProvider,
	mut to_other_subsystems: Sender,
	mut to_approval_distr: ADSender,
	mut subsystem: ApprovalVotingSubsystem,
	assignment_criteria: Box<dyn AssignmentCriteria + Send + Sync>,
	mut backend: B,
) -> SubsystemResult<()>
where
	B: Backend,
{
	if let Err(err) = db_sanity_check(subsystem.db.clone(), subsystem.db_config) {
		gum::warn!(target: LOG_TARGET, ?err, "Could not run approval vote DB sanity check");
	}

	let mut state = State {
		keystore: subsystem.keystore,
		slot_duration_millis: subsystem.slot_duration_millis,
		clock: subsystem.clock,
		assignment_criteria,
		per_block_assignments_gathering_times: LruMap::new(ByLength::new(
			MAX_BLOCKS_WITH_ASSIGNMENT_TIMESTAMPS,
		)),
		no_show_stats: NoShowStats::default(),
	};

	let mut last_finalized_height: Option<BlockNumber> = {
		let (tx, rx) = oneshot::channel();
		to_other_subsystems
			.send_message(ChainApiMessage::FinalizedBlockNumber(tx))
			.await;
		match rx.await? {
			Ok(number) => Some(number),
			Err(err) => {
				gum::warn!(target: LOG_TARGET, ?err, "Failed fetching finalized number");
				None
			},
		}
	};

	// `None` on start-up. Gets initialized/updated on leaf update
	let mut session_info_provider = RuntimeInfo::new_with_config(RuntimeInfoConfig {
		keystore: None,
		session_cache_lru_size: DISPUTE_WINDOW.get(),
	});

	let mut wakeups = Wakeups::default();
	let mut currently_checking_set = CurrentlyCheckingSet::default();
	let mut delayed_approvals_timers = DelayedApprovalTimer::default();
	let mut approvals_cache = LruMap::new(ByLength::new(APPROVAL_CACHE_SIZE));

	loop {
		let mut overlayed_db = OverlayedBackend::new(&backend);
		let actions = futures::select! {
			(_tick, woken_block, woken_candidate) = wakeups.next(&*state.clock).fuse() => {
				subsystem.metrics.on_wakeup();
				process_wakeup(
					&mut to_other_subsystems,
					&mut state,
					&mut overlayed_db,
					&mut session_info_provider,
					woken_block,
					woken_candidate,
					&subsystem.metrics,
					&wakeups,
				).await?
			}
			next_msg = work_provider.recv().fuse() => {
				let mut actions = handle_from_overseer(
					&mut to_other_subsystems,
					&mut to_approval_distr,
					&subsystem.spawner,
					&mut state,
					&mut overlayed_db,
					&mut session_info_provider,
					&subsystem.metrics,
					next_msg?,
					&mut last_finalized_height,
					&mut wakeups,
				).await?;

				if let Mode::Syncing(ref mut oracle) = subsystem.mode {
					if !oracle.is_major_syncing() {
						// note that we're active before processing other actions.
						actions.insert(0, Action::BecomeActive)
					}
				}

				actions
			}
			approval_state = currently_checking_set.next(&mut approvals_cache).fuse() => {
				let mut actions = Vec::new();
				let (
					relay_block_hashes,
					ApprovalState {
						validator_index,
						candidate_hash,
						approval_outcome,
						retry_info,
					}
				) = approval_state;

				if matches!(approval_outcome, ApprovalOutcome::Approved) {
					let mut approvals: Vec<Action> = relay_block_hashes
						.iter()
						.map(|block_hash|
							Action::IssueApproval(
								candidate_hash,
								ApprovalVoteRequest {
									validator_index,
									block_hash: *block_hash,
								},
							)
						)
						.collect();
					actions.append(&mut approvals);
				}

				if let Some(retry_info) = retry_info {
					for block_hash in relay_block_hashes {
						if overlayed_db.load_block_entry(&block_hash).map(|block_info| block_info.is_some()).unwrap_or(false) {
							let sender = to_other_subsystems.clone();
							let spawn_handle = subsystem.spawner.clone();
							let metrics = subsystem.metrics.clone();
							let retry_info = retry_info.clone();
							let candidate = retry_info.candidate.clone();

							currently_checking_set
								.insert_relay_block_hash(
									candidate_hash,
									validator_index,
									block_hash,
									async move {
										launch_approval(
											sender,
											spawn_handle,
											metrics,
											retry_info.session_index,
											candidate,
											validator_index,
											block_hash,
											retry_info.backing_group,
											retry_info.core_index,
											retry_info,
										)
										.await
									},
								)
								.await?;
						}
					}
				}

				actions
			},
			(block_hash, validator_index) = delayed_approvals_timers.select_next_some() => {
				gum::debug!(
					target: LOG_TARGET,
					?block_hash,
					?validator_index,
					"Sign approval for multiple candidates",
				);

				match maybe_create_signature(
					&mut overlayed_db,
					&mut session_info_provider,
					&state,
					&mut to_other_subsystems,
					&mut to_approval_distr,
					block_hash,
					validator_index,
					&subsystem.metrics,
				).await {
					Ok(Some(next_wakeup)) => {
						delayed_approvals_timers.maybe_arm_timer(next_wakeup, state.clock.as_ref(), block_hash, validator_index);
					},
					Ok(None) => {}
					Err(err) => {
						gum::error!(
							target: LOG_TARGET,
							?err,
							"Failed to create signature",
						);
					}
				}
				vec![]
			}
		};

		if handle_actions(
			&mut to_other_subsystems,
			&mut to_approval_distr,
			&subsystem.spawner,
			&mut state,
			&mut overlayed_db,
			&mut session_info_provider,
			&subsystem.metrics,
			&mut wakeups,
			&mut currently_checking_set,
			&mut delayed_approvals_timers,
			&mut approvals_cache,
			&mut subsystem.mode,
			actions,
			subsystem.max_approval_retries,
			subsystem.retry_backoff,
		)
		.await?
		{
			break;
		}

		if !overlayed_db.is_empty() {
			let _timer = subsystem.metrics.time_db_transaction();
			let ops = overlayed_db.into_write_ops();
			backend.write(ops)?;
		}
	}

	Ok(())
}

// Starts a worker thread that runs the approval voting subsystem.
pub async fn start_approval_worker<
	WorkProvider: ApprovalVotingWorkProvider + Send + 'static,
	Sender: SubsystemSender<ChainApiMessage>
		+ SubsystemSender<RuntimeApiMessage>
		+ SubsystemSender<ChainSelectionMessage>
		+ SubsystemSender<AvailabilityRecoveryMessage>
		+ SubsystemSender<DisputeCoordinatorMessage>
		+ SubsystemSender<CandidateValidationMessage>
		+ Clone,
	ADSender: SubsystemSender<ApprovalDistributionMessage>,
>(
	work_provider: WorkProvider,
	to_other_subsystems: Sender,
	to_approval_distr: ADSender,
	config: Config,
	db: Arc<dyn Database>,
	keystore: Arc<LocalKeystore>,
	sync_oracle: Box<dyn SyncOracle + Send>,
	metrics: Metrics,
	spawner: Arc<dyn overseer::gen::Spawner + 'static>,
	task_name: &'static str,
	group_name: &'static str,
	clock: Arc<dyn Clock + Send + Sync>,
) -> SubsystemResult<()> {
	let approval_voting = ApprovalVotingSubsystem::with_config_and_clock(
		config,
		db.clone(),
		keystore,
		sync_oracle,
		metrics,
		clock,
		spawner,
		MAX_APPROVAL_RETRIES,
		APPROVAL_CHECKING_TIMEOUT / 2,
	);
	let backend = DbBackend::new(db.clone(), approval_voting.db_config);
	let spawner = approval_voting.spawner.clone();
	spawner.spawn_blocking(
		task_name,
		Some(group_name),
		Box::pin(async move {
			if let Err(err) = run(
				work_provider,
				to_other_subsystems,
				to_approval_distr,
				approval_voting,
				Box::new(RealAssignmentCriteria),
				backend,
			)
			.await
			{
				gum::error!(target: LOG_TARGET, ?err, "Approval voting worker stopped processing messages");
			};
		}),
	);
	Ok(())
}

// Handle actions is a function that accepts a set of instructions
// and subsequently updates the underlying approvals_db in accordance
// with the linear set of instructions passed in. Therefore, actions
// must be processed in series to ensure that earlier actions are not
// negated/corrupted by later actions being executed out-of-order.
//
// However, certain Actions can cause additional actions to need to be
// processed by this function. In order to preserve linearity, we would
// need to handle these newly generated actions before we finalize
// completing additional actions in the submitted sequence of actions.
//
// Since recursive async functions are not stable yet, we are
// forced to modify the actions iterator on the fly whenever a new set
// of actions are generated by handling a single action.
//
// This particular problem statement is specified in issue 3311:
// 	https://github.com/paritytech/polkadot/issues/3311
//
// returns `true` if any of the actions was a `Conclude` command.
#[overseer::contextbounds(ApprovalVoting, prefix = self::overseer)]
async fn handle_actions<
	Sender: SubsystemSender<ChainApiMessage>
		+ SubsystemSender<RuntimeApiMessage>
		+ SubsystemSender<ChainSelectionMessage>
		+ SubsystemSender<AvailabilityRecoveryMessage>
		+ SubsystemSender<DisputeCoordinatorMessage>
		+ SubsystemSender<CandidateValidationMessage>
		+ Clone,
	ADSender: SubsystemSender<ApprovalDistributionMessage>,
>(
	sender: &mut Sender,
	approval_voting_sender: &mut ADSender,
	spawn_handle: &Arc<dyn overseer::gen::Spawner + 'static>,
	state: &mut State,
	overlayed_db: &mut OverlayedBackend<'_, impl Backend>,
	session_info_provider: &mut RuntimeInfo,
	metrics: &Metrics,
	wakeups: &mut Wakeups,
	currently_checking_set: &mut CurrentlyCheckingSet,
	delayed_approvals_timers: &mut DelayedApprovalTimer,
	approvals_cache: &mut LruMap<CandidateHash, ApprovalOutcome>,
	mode: &mut Mode,
	actions: Vec<Action>,
	max_approval_retries: u32,
	retry_backoff: Duration,
) -> SubsystemResult<bool> {
	let mut conclude = false;
	let mut actions_iter = actions.into_iter();
	while let Some(action) = actions_iter.next() {
		match action {
			Action::ScheduleWakeup { block_hash, block_number, candidate_hash, tick } => {
				wakeups.schedule(block_hash, block_number, candidate_hash, tick);
			},
			Action::IssueApproval(candidate_hash, approval_request) => {
				// Note that the IssueApproval action will create additional
				// actions that will need to all be processed before we can
				// handle the next action in the set passed to the ambient
				// function.
				//
				// In order to achieve this, we append the existing iterator
				// to the end of the iterator made up of these newly generated
				// actions.
				//
				// Note that chaining these iterators is O(n) as we must consume
				// the prior iterator.
				let next_actions: Vec<Action> = issue_approval(
					sender,
					approval_voting_sender,
					state,
					overlayed_db,
					session_info_provider,
					metrics,
					candidate_hash,
					delayed_approvals_timers,
					approval_request,
					&wakeups,
				)
				.await?
				.into_iter()
				.map(|v| v.clone())
				.chain(actions_iter)
				.collect();

				actions_iter = next_actions.into_iter();
			},
			Action::LaunchApproval {
				claimed_candidate_indices,
				candidate_hash,
				indirect_cert,
				assignment_tranche,
				relay_block_hash,
				session,
				candidate,
				backing_group,
				distribute_assignment,
				core_index,
			} => {
				// Don't launch approval work if the node is syncing.
				if let Mode::Syncing(_) = *mode {
					continue;
				}

				metrics.on_assignment_produced(assignment_tranche);
				let block_hash = indirect_cert.block_hash;
				let validator_index = indirect_cert.validator;

				if distribute_assignment {
					approval_voting_sender.send_unbounded_message(
						ApprovalDistributionMessage::DistributeAssignment(
							indirect_cert,
							claimed_candidate_indices,
						),
					);
				}

				match approvals_cache.get(&candidate_hash) {
					Some(ApprovalOutcome::Approved) => {
						let new_actions: Vec<Action> = std::iter::once(Action::IssueApproval(
							candidate_hash,
							ApprovalVoteRequest { validator_index, block_hash },
						))
						.map(|v| v.clone())
						.chain(actions_iter)
						.collect();
						actions_iter = new_actions.into_iter();
					},
					None => {
						let sender = sender.clone();
						let spawn_handle = spawn_handle.clone();

						let retry = RetryApprovalInfo {
							candidate: candidate.clone(),
							backing_group,
							core_index,
							session_index: session,
							attempts_remaining: max_approval_retries,
							backoff: retry_backoff,
						};

						currently_checking_set
							.insert_relay_block_hash(
								candidate_hash,
								validator_index,
								relay_block_hash,
								async move {
									launch_approval(
										sender,
										spawn_handle,
										metrics.clone(),
										session,
										candidate,
										validator_index,
										block_hash,
										backing_group,
										core_index,
										retry,
									)
									.await
								},
							)
							.await?;
					},
					Some(_) => {},
				}
			},
			Action::NoteApprovedInChainSelection(block_hash) => {
				sender.send_message(ChainSelectionMessage::Approved(block_hash)).await;
			},
			Action::BecomeActive => {
				*mode = Mode::Active;

				let (messages, next_actions) = distribution_messages_for_activation(
					overlayed_db,
					state,
					delayed_approvals_timers,
				)
				.await?;
				for message in messages.into_iter() {
					approval_voting_sender.send_unbounded_message(message);
				}
				let next_actions: Vec<Action> =
					next_actions.into_iter().map(|v| v.clone()).chain(actions_iter).collect();

				actions_iter = next_actions.into_iter();
			},
			Action::Conclude => {
				conclude = true;
			},
		}
	}

	Ok(conclude)
}

fn cores_to_candidate_indices(
	core_indices: &CoreBitfield,
	block_entry: &BlockEntry,
) -> Result<CandidateBitfield, BitfieldError> {
	let mut candidate_indices = Vec::new();

	// Map from core index to candidate index.
	for claimed_core_index in core_indices.iter_ones() {
		if let Some(candidate_index) = block_entry
			.candidates()
			.iter()
			.position(|(core_index, _)| core_index.0 == claimed_core_index as u32)
		{
			candidate_indices.push(candidate_index as _);
		}
	}

	CandidateBitfield::try_from(candidate_indices)
}

// Returns the claimed core bitfield from the assignment cert.
fn get_assignment_core_indices(assignment: &AssignmentCertKindV2) -> CoreBitfield {
	match &assignment {
		AssignmentCertKindV2::RelayVRFModuloCompact { core_bitfield } => core_bitfield.clone(),
		AssignmentCertKindV2::RelayVRFDelay { core_index } => {
			CoreBitfield::try_from(vec![*core_index]).expect("Not an empty vec; qed")
		},
	}
}

async fn distribution_messages_for_activation(
	db: &OverlayedBackend<'_, impl Backend>,
	state: &State,
	delayed_approvals_timers: &mut DelayedApprovalTimer,
) -> SubsystemResult<(Vec<ApprovalDistributionMessage>, Vec<Action>)> {
	let all_blocks: Vec<Hash> = db.load_all_blocks()?;

	let mut approval_meta = Vec::with_capacity(all_blocks.len());
	let mut messages = Vec::new();
	let mut approvals = Vec::new();
	let mut actions = Vec::new();

	messages.push(ApprovalDistributionMessage::NewBlocks(Vec::new())); // dummy value.

	for block_hash in all_blocks {
		let block_entry = match db.load_block_entry(&block_hash)? {
			Some(b) => b,
			None => {
				gum::warn!(target: LOG_TARGET, ?block_hash, "Missing block entry");

				continue;
			},
		};

		approval_meta.push(BlockApprovalMeta {
			hash: block_hash,
			number: block_entry.block_number(),
			parent_hash: block_entry.parent_hash(),
			candidates: block_entry
				.candidates()
				.iter()
				.map(|(core_index, c_hash)| {
					let candidate = db.load_candidate_entry(c_hash).ok().flatten();
					let group_index = candidate
						.and_then(|entry| {
							entry.approval_entry(&block_hash).map(|entry| entry.backing_group())
						})
						.unwrap_or_else(|| {
							gum::warn!(
								target: LOG_TARGET,
								?block_hash,
								?c_hash,
								"Missing candidate entry or approval entry",
							);
							GroupIndex::default()
						});
					(*c_hash, *core_index, group_index)
				})
				.collect(),
			slot: block_entry.slot(),
			session: block_entry.session(),
			vrf_story: block_entry.relay_vrf_story(),
		});
		let mut signatures_queued = HashSet::new();
		for (core_index, candidate_hash) in block_entry.candidates() {
			let candidate_entry = match db.load_candidate_entry(&candidate_hash)? {
				Some(c) => c,
				None => {
					gum::warn!(
						target: LOG_TARGET,
						?block_hash,
						?candidate_hash,
						"Missing candidate entry",
					);

					continue;
				},
			};

			match candidate_entry.approval_entry(&block_hash) {
				Some(approval_entry) => {
					match approval_entry.local_statements() {
						(None, None) => {
							if approval_entry
								.our_assignment()
								.map(|assignment| !assignment.triggered())
								.unwrap_or(false)
							{
								actions.push(Action::ScheduleWakeup {
									block_hash,
									block_number: block_entry.block_number(),
									candidate_hash: *candidate_hash,
									tick: state.clock.tick_now() + RESTART_WAKEUP_DELAY,
								})
							}
						},
						(None, Some(_)) => {}, // second is impossible case.
						(Some(assignment), None) => {
							let claimed_core_indices =
								get_assignment_core_indices(&assignment.cert().kind);

							if block_entry.has_candidates_pending_signature() {
								delayed_approvals_timers.maybe_arm_timer(
									state.clock.tick_now(),
									state.clock.as_ref(),
									block_entry.block_hash(),
									assignment.validator_index(),
								)
							}

							match cores_to_candidate_indices(&claimed_core_indices, &block_entry) {
								Ok(bitfield) => {
									gum::debug!(
										target: LOG_TARGET,
										candidate_hash = ?candidate_entry.candidate_receipt().hash(),
										?block_hash,
										"Discovered, triggered assignment, not approved yet",
									);

									let indirect_cert = IndirectAssignmentCertV2 {
										block_hash,
										validator: assignment.validator_index(),
										cert: assignment.cert().clone(),
									};
									messages.push(
										ApprovalDistributionMessage::DistributeAssignment(
											indirect_cert.clone(),
											bitfield.clone(),
										),
									);

									if !block_entry.candidate_is_pending_signature(*candidate_hash)
									{
										actions.push(Action::LaunchApproval {
											claimed_candidate_indices: bitfield,
											candidate_hash: candidate_entry
												.candidate_receipt()
												.hash(),
											indirect_cert,
											assignment_tranche: assignment.tranche(),
											relay_block_hash: block_hash,
											session: block_entry.session(),
											candidate: candidate_entry.candidate_receipt().clone(),
											backing_group: approval_entry.backing_group(),
											distribute_assignment: false,
											core_index: Some(*core_index),
										});
									}
								},
								Err(err) => {
									// Should never happen. If we fail here it means the
									// assignment is null (no cores claimed).
									gum::warn!(
										target: LOG_TARGET,
										?block_hash,
										?candidate_hash,
										?err,
										"Failed to create assignment bitfield",
									);
								},
							}
						},
						(Some(assignment), Some(approval_sig)) => {
							let claimed_core_indices =
								get_assignment_core_indices(&assignment.cert().kind);
							match cores_to_candidate_indices(&claimed_core_indices, &block_entry) {
								Ok(bitfield) => messages.push(
									ApprovalDistributionMessage::DistributeAssignment(
										IndirectAssignmentCertV2 {
											block_hash,
											validator: assignment.validator_index(),
											cert: assignment.cert().clone(),
										},
										bitfield,
									),
								),
								Err(err) => {
									gum::warn!(
										target: LOG_TARGET,
										?block_hash,
										?candidate_hash,
										?err,
										"Failed to create assignment bitfield",
									);
									// If we didn't send assignment, we don't send approval.
									continue;
								},
							}
							if signatures_queued
								.insert(approval_sig.signed_candidates_indices.clone())
							{
								approvals.push(ApprovalDistributionMessage::DistributeApproval(
									IndirectSignedApprovalVoteV2 {
										block_hash,
										candidate_indices: approval_sig.signed_candidates_indices,
										validator: assignment.validator_index(),
										signature: approval_sig.signature,
									},
								))
							};
						},
					}
				},
				None => {
					gum::warn!(
						target: LOG_TARGET,
						?block_hash,
						?candidate_hash,
						"Missing approval entry",
					);
				},
			}
		}
	}

	messages[0] = ApprovalDistributionMessage::NewBlocks(approval_meta);
	// Approvals are appended at the end, to make sure all assignments are sent
	// before the approvals, otherwise if they arrive ahead in approval-distribution
	// they will be ignored.
	messages.extend(approvals.into_iter());
	Ok((messages, actions))
}

// Handle an incoming signal from the overseer. Returns true if execution should conclude.
async fn handle_from_overseer<
	Sender: SubsystemSender<ChainApiMessage>
		+ SubsystemSender<RuntimeApiMessage>
		+ SubsystemSender<ChainSelectionMessage>
		+ Clone,
	ADSender: SubsystemSender<ApprovalDistributionMessage>,
>(
	sender: &mut Sender,
	approval_voting_sender: &mut ADSender,
	spawn_handle: &Arc<dyn overseer::gen::Spawner + 'static>,
	state: &mut State,
	db: &mut OverlayedBackend<'_, impl Backend>,
	session_info_provider: &mut RuntimeInfo,
	metrics: &Metrics,
	x: FromOrchestra<ApprovalVotingMessage>,
	last_finalized_height: &mut Option<BlockNumber>,
	wakeups: &mut Wakeups,
) -> SubsystemResult<Vec<Action>> {
	let actions = match x {
		FromOrchestra::Signal(OverseerSignal::ActiveLeaves(update)) => {
			let mut actions = Vec::new();
			if let Some(activated) = update.activated {
				let head = activated.hash;
				match import::handle_new_head(
					sender,
					approval_voting_sender,
					state,
					db,
					session_info_provider,
					head,
					last_finalized_height,
				)
				.await
				{
					Err(e) => return Err(SubsystemError::with_origin("db", e)),
					Ok(block_imported_candidates) => {
						// Schedule wakeups for all imported candidates.
						for block_batch in block_imported_candidates {
							gum::debug!(
								target: LOG_TARGET,
								block_number = ?block_batch.block_number,
								block_hash = ?block_batch.block_hash,
								num_candidates = block_batch.imported_candidates.len(),
								"Imported new block.",
							);

							state.no_show_stats.maybe_print(block_batch.block_number);

							for (c_hash, c_entry) in block_batch.imported_candidates {
								metrics.on_candidate_imported();

								let our_tranche = c_entry
									.approval_entry(&block_batch.block_hash)
									.and_then(|a| a.our_assignment().map(|a| a.tranche()));

								if let Some(our_tranche) = our_tranche {
									let tick = our_tranche as Tick + block_batch.block_tick;
									gum::trace!(
										target: LOG_TARGET,
										tranche = our_tranche,
										candidate_hash = ?c_hash,
										block_hash = ?block_batch.block_hash,
										block_tick = block_batch.block_tick,
										"Scheduling first wakeup.",
									);

									// Our first wakeup will just be the tranche of our assignment,
									// if any. This will likely be superseded by incoming
									// assignments and approvals which trigger rescheduling.
									actions.push(Action::ScheduleWakeup {
										block_hash: block_batch.block_hash,
										block_number: block_batch.block_number,
										candidate_hash: c_hash,
										tick,
									});
								}
							}
						}
					},
				}
			}

			actions
		},
		FromOrchestra::Signal(OverseerSignal::BlockFinalized(block_hash, block_number)) => {
			gum::debug!(target: LOG_TARGET, ?block_hash, ?block_number, "Block finalized");
			*last_finalized_height = Some(block_number);

			crate::ops::canonicalize(db, block_number, block_hash)
				.map_err(|e| SubsystemError::with_origin("db", e))?;

			// `prune_finalized_wakeups` prunes all finalized block hashes. We prune spans
			// accordingly.
			wakeups.prune_finalized_wakeups(block_number);
			state.cleanup_assignments_gathering_timestamp(block_number);

			// // `prune_finalized_wakeups` prunes all finalized block hashes. We prune spans
			// accordingly. let hash_set =
			// wakeups.block_numbers.values().flatten().collect::<HashSet<_>>(); state.spans.
			// retain(|hash, _| hash_set.contains(hash));

			Vec::new()
		},
		FromOrchestra::Signal(OverseerSignal::Conclude) => {
			vec![Action::Conclude]
		},
		FromOrchestra::Communication { msg } => match msg {
			ApprovalVotingMessage::ImportAssignment(checked_assignment, tx) => {
				let (check_outcome, actions) =
					import_assignment(sender, state, db, session_info_provider, checked_assignment)
						.await?;
				// approval-distribution makes sure this assignment is valid and expected,
				// so this import should never fail, if it does it might mean one of two things,
				// there is a bug in the code or the two subsystems got out of sync.
				if let AssignmentCheckResult::Bad(ref err) = check_outcome {
					gum::debug!(target: LOG_TARGET, ?err, "Unexpected fail when importing an assignment");
				}
				let _ = tx.map(|tx| tx.send(check_outcome));
				actions
			},
			ApprovalVotingMessage::ImportApproval(a, tx) => {
				let result =
					import_approval(sender, state, db, session_info_provider, metrics, a, &wakeups)
						.await?;
				// approval-distribution makes sure this vote is valid and expected,
				// so this import should never fail, if it does it might mean one of two things,
				// there is a bug in the code or the two subsystems got out of sync.
				if let ApprovalCheckResult::Bad(ref err) = result.1 {
					gum::debug!(target: LOG_TARGET, ?err, "Unexpected fail when importing an approval");
				}
				let _ = tx.map(|tx| tx.send(result.1));

				result.0
			},
			ApprovalVotingMessage::ApprovedAncestor(target, lower_bound, res) => {
				match handle_approved_ancestor(sender, db, target, lower_bound, wakeups, &metrics)
					.await
				{
					Ok(v) => {
						let _ = res.send(v);
					},
					Err(e) => {
						let _ = res.send(None);
						return Err(e);
					},
				}

				Vec::new()
			},
			ApprovalVotingMessage::GetApprovalSignaturesForCandidate(candidate_hash, tx) => {
				metrics.on_candidate_signatures_request();
				get_approval_signatures_for_candidate(
					approval_voting_sender.clone(),
					spawn_handle,
					db,
					candidate_hash,
					tx,
				)
				.await?;
				Vec::new()
			},
		},
	};

	Ok(actions)
}

/// Retrieve approval signatures.
///
/// This involves an unbounded message send to approval-distribution, the caller has to ensure that
/// calls to this function are infrequent and bounded.
#[overseer::contextbounds(ApprovalVoting, prefix = self::overseer)]
async fn get_approval_signatures_for_candidate<
	Sender: SubsystemSender<ApprovalDistributionMessage>,
>(
	mut sender: Sender,
	spawn_handle: &Arc<dyn overseer::gen::Spawner + 'static>,
	db: &OverlayedBackend<'_, impl Backend>,
	candidate_hash: CandidateHash,
	tx: oneshot::Sender<
		HashMap<ValidatorIndex, (CoalescedApprovalCandidateHashes, ValidatorSignature)>,
	>,
) -> SubsystemResult<()> {
	let send_votes = |votes| {
		if let Err(_) = tx.send(votes) {
			gum::debug!(
				target: LOG_TARGET,
				"Sending approval signatures back failed, as receiver got closed."
			);
		}
	};
	let entry = match db.load_candidate_entry(&candidate_hash)? {
		None => {
			send_votes(HashMap::new());
			gum::debug!(
				target: LOG_TARGET,
				?candidate_hash,
				"Sent back empty votes because the candidate was not found in db."
			);
			return Ok(());
		},
		Some(e) => e,
	};

	let relay_hashes = entry.block_assignments.keys();

	let mut candidate_indices = HashSet::new();
	let mut candidate_indices_to_candidate_hashes: HashMap<
		Hash,
		HashMap<CandidateIndex, CandidateHash>,
	> = HashMap::new();

	// Retrieve `CoreIndices`/`CandidateIndices` as required by approval-distribution:
	for hash in relay_hashes {
		let entry = match db.load_block_entry(hash)? {
			None => {
				gum::debug!(
					target: LOG_TARGET,
					?candidate_hash,
					?hash,
					"Block entry for assignment missing."
				);
				continue;
			},
			Some(e) => e,
		};
		for (candidate_index, (_core_index, c_hash)) in entry.candidates().iter().enumerate() {
			if c_hash == &candidate_hash {
				candidate_indices.insert((*hash, candidate_index as u32));
			}
			candidate_indices_to_candidate_hashes
				.entry(*hash)
				.or_default()
				.insert(candidate_index as _, *c_hash);
		}
	}

	let get_approvals = async move {
		let (tx_distribution, rx_distribution) = oneshot::channel();
		sender.send_unbounded_message(ApprovalDistributionMessage::GetApprovalSignatures(
			candidate_indices,
			tx_distribution,
		));

		// Because of the unbounded sending and the nature of the call (just fetching data from
		// state), this should not block long:
		match rx_distribution.timeout(WAIT_FOR_SIGS_TIMEOUT).await {
			None => {
				gum::warn!(
					target: LOG_TARGET,
					"Waiting for approval signatures timed out - dead lock?"
				);
			},
			Some(Err(_)) => gum::debug!(
				target: LOG_TARGET,
				"Request for approval signatures got cancelled by `approval-distribution`."
			),
			Some(Ok(votes)) => {
				let votes = votes
					.into_iter()
					.filter_map(|(validator_index, (hash, signed_candidates_indices, signature))| {
						let candidates_hashes = candidate_indices_to_candidate_hashes.get(&hash);

						if candidates_hashes.is_none() {
							gum::warn!(
								target: LOG_TARGET,
								?hash,
								"Possible bug! Could not find map of candidate_hashes for block hash received from approval-distribution"
							);
						}

						let num_signed_candidates = signed_candidates_indices.len();

						let signed_candidates_hashes: Vec<CandidateHash> =
							signed_candidates_indices
								.into_iter()
								.filter_map(|candidate_index| {
									candidates_hashes.and_then(|candidate_hashes| {
										if let Some(candidate_hash) =
											candidate_hashes.get(&candidate_index)
										{
											Some(*candidate_hash)
										} else {
											gum::warn!(
												target: LOG_TARGET,
												?candidate_index,
												"Possible bug! Could not find candidate hash for candidate_index coming from approval-distribution"
											);
											None
										}
									})
								})
								.collect();
						if num_signed_candidates == signed_candidates_hashes.len() {
							match signed_candidates_hashes.try_into() {
								Ok(signed_candidates_hashes) =>
									Some((validator_index, (signed_candidates_hashes, signature))),
								Err(_) => {
									gum::warn!(
										target: LOG_TARGET,
										"Skipping approval signature coalescing more than MAX_COALESCE_APPROVALS candidates"
									);
									None
								},
							}
						} else {
							gum::warn!(
								target: LOG_TARGET,
								"Possible bug! Could not find all hashes for candidates coming from approval-distribution"
							);
							None
						}
					})
					.collect();
				send_votes(votes)
			},
		}
	};

	// No need to block subsystem on this (also required to break cycle).
	// We should not be sending this message frequently - caller must make sure this is bounded.
	gum::trace!(
		target: LOG_TARGET,
		?candidate_hash,
		"Spawning task for fetching signatures from approval-distribution"
	);
	spawn_handle.spawn(
		"get-approval-signatures",
		Some("approval-voting-subsystem"),
		Box::pin(get_approvals),
	);
	Ok(())
}

#[overseer::contextbounds(ApprovalVoting, prefix = self::overseer)]
async fn handle_approved_ancestor<Sender: SubsystemSender<ChainApiMessage>>(
	sender: &mut Sender,
	db: &OverlayedBackend<'_, impl Backend>,
	target: Hash,
	lower_bound: BlockNumber,
	wakeups: &Wakeups,
	metrics: &Metrics,
) -> SubsystemResult<Option<HighestApprovedAncestorBlock>> {
	const MAX_TRACING_WINDOW: usize = 200;
	const ABNORMAL_DEPTH_THRESHOLD: usize = 5;
	const LOGGING_DEPTH_THRESHOLD: usize = 10;

	let mut all_approved_max = None;

	let target_number = {
		let (tx, rx) = oneshot::channel();

		sender.send_message(ChainApiMessage::BlockNumber(target, tx)).await;

		match rx.await {
			Ok(Ok(Some(n))) => n,
			Ok(Ok(None)) => return Ok(None),
			Ok(Err(_)) | Err(_) => return Ok(None),
		}
	};

	if target_number <= lower_bound {
		return Ok(None);
	}

	// request ancestors up to but not including the lower bound,
	// as a vote on the lower bound is implied if we cannot find
	// anything else.
	let ancestry = if target_number > lower_bound + 1 {
		let (tx, rx) = oneshot::channel();

		sender
			.send_message(ChainApiMessage::Ancestors {
				hash: target,
				k: (target_number - (lower_bound + 1)) as usize,
				response_channel: tx,
			})
			.await;

		match rx.await {
			Ok(Ok(a)) => a,
			Err(_) | Ok(Err(_)) => return Ok(None),
		}
	} else {
		Vec::new()
	};
	let ancestry_len = ancestry.len();

	let mut block_descriptions = Vec::new();

	let mut bits: BitVec<u8, Lsb0> = Default::default();
	for (i, block_hash) in std::iter::once(target).chain(ancestry).enumerate() {
		// Block entries should be present as the assumption is that
		// nothing here is finalized. If we encounter any missing block
		// entries we can fail.
		let entry = match db.load_block_entry(&block_hash)? {
			None => {
				let block_number = target_number.saturating_sub(i as u32);
				gum::info!(
					target: LOG_TARGET,
					unknown_number = ?block_number,
					unknown_hash = ?block_hash,
					"Chain between ({}, {}) and {} not fully known. Forcing vote on {}",
					target,
					target_number,
					lower_bound,
					lower_bound,
				);
				return Ok(None);
			},
			Some(b) => b,
		};

		// even if traversing millions of blocks this is fairly cheap and always dwarfed by the
		// disk lookups.
		bits.push(entry.is_fully_approved());
		if entry.is_fully_approved() {
			if all_approved_max.is_none() {
				// First iteration of the loop is target, i = 0. After that,
				// ancestry is moving backwards.
				all_approved_max = Some((block_hash, target_number - i as BlockNumber));
			}
			block_descriptions.push(BlockDescription {
				block_hash,
				session: entry.session(),
				candidates: entry
					.candidates()
					.iter()
					.map(|(_idx, candidate_hash)| *candidate_hash)
					.collect(),
			});
		} else if bits.len() <= ABNORMAL_DEPTH_THRESHOLD {
			all_approved_max = None;
			block_descriptions.clear();
		} else {
			all_approved_max = None;
			block_descriptions.clear();

			let unapproved: Vec<_> = entry.unapproved_candidates().collect();
			gum::debug!(
				target: LOG_TARGET,
				"Block {} is {} blocks deep and has {}/{} candidates unapproved",
				block_hash,
				bits.len() - 1,
				unapproved.len(),
				entry.candidates().len(),
			);
			if ancestry_len >= LOGGING_DEPTH_THRESHOLD && i > ancestry_len - LOGGING_DEPTH_THRESHOLD
			{
				gum::trace!(
					target: LOG_TARGET,
					?block_hash,
					"Unapproved candidates at depth {}: {:?}",
					bits.len(),
					unapproved
				)
			}
			metrics.on_unapproved_candidates_in_unfinalized_chain(unapproved.len());
			for candidate_hash in unapproved {
				match db.load_candidate_entry(&candidate_hash)? {
					None => {
						gum::warn!(
							target: LOG_TARGET,
							?candidate_hash,
							"Missing expected candidate in DB",
						);

						continue;
					},
					Some(c_entry) => match c_entry.approval_entry(&block_hash) {
						None => {
							gum::warn!(
								target: LOG_TARGET,
								?candidate_hash,
								?block_hash,
								"Missing expected approval entry under candidate.",
							);
						},
						Some(a_entry) => {
							let status = || {
								let n_assignments = a_entry.n_assignments();

								// Take the approvals, filtered by the assignments
								// for this block.
								let n_approvals = c_entry
									.approvals()
									.iter()
									.by_vals()
									.enumerate()
									.filter(|(i, approved)| {
										*approved && a_entry.is_assigned(ValidatorIndex(*i as _))
									})
									.count();

								format!(
									"{}/{}/{}",
									n_assignments,
									n_approvals,
									a_entry.n_validators(),
								)
							};

							match a_entry.our_assignment() {
								None => gum::debug!(
									target: LOG_TARGET,
									?candidate_hash,
									?block_hash,
									status = %status(),
									"no assignment."
								),
								Some(a) => {
									let tranche = a.tranche();
									let triggered = a.triggered();

									let next_wakeup =
										wakeups.wakeup_for(block_hash, candidate_hash);

									let approved =
										triggered && { a_entry.local_statements().1.is_some() };

									gum::debug!(
										target: LOG_TARGET,
										?candidate_hash,
										?block_hash,
										tranche,
										?next_wakeup,
										status = %status(),
										triggered,
										approved,
										"assigned."
									);
								},
							}
						},
					},
				}
			}
		}
	}

	gum::debug!(
		target: LOG_TARGET,
		"approved blocks {}-[{}]-{}",
		target_number,
		{
			// formatting to divide bits by groups of 10.
			// when comparing logs on multiple machines where the exact vote
			// targets may differ, this grouping is useful.
			let mut s = String::with_capacity(bits.len());
			for (i, bit) in bits.iter().enumerate().take(MAX_TRACING_WINDOW) {
				s.push(if *bit { '1' } else { '0' });
				if (target_number - i as u32).is_multiple_of(10) && i != bits.len() - 1 {
					s.push(' ');
				}
			}

			s
		},
		if bits.len() > MAX_TRACING_WINDOW {
			format!(
				"{}... (truncated due to large window)",
				target_number - MAX_TRACING_WINDOW as u32 + 1,
			)
		} else {
			format!("{}", lower_bound + 1)
		},
	);

	// `reverse()` to obtain the ascending order from lowest to highest
	// block within the candidates, which is the expected order
	block_descriptions.reverse();

	let all_approved_max =
		all_approved_max.map(|(hash, block_number)| HighestApprovedAncestorBlock {
			hash,
			number: block_number,
			descriptions: block_descriptions,
		});

	Ok(all_approved_max)
}

// `Option::cmp` treats `None` as less than `Some`.
fn min_prefer_some<T: std::cmp::Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
	match (a, b) {
		(None, None) => None,
		(None, Some(x)) | (Some(x), None) => Some(x),
		(Some(x), Some(y)) => Some(std::cmp::min(x, y)),
	}
}

fn schedule_wakeup_action(
	approval_entry: &ApprovalEntry,
	block_hash: Hash,
	block_number: BlockNumber,
	candidate_hash: CandidateHash,
	block_tick: Tick,
	tick_now: Tick,
	required_tranches: RequiredTranches,
) -> Option<Action> {
	let maybe_action = match required_tranches {
		_ if approval_entry.is_approved() => None,
		RequiredTranches::All => None,
		RequiredTranches::Exact { next_no_show, last_assignment_tick, .. } => {
			// Take the earlier of the next no show or the last assignment tick + required delay,
			// only considering the latter if it is after the current moment.
			min_prefer_some(
				last_assignment_tick.map(|l| l + APPROVAL_DELAY).filter(|t| t > &tick_now),
				next_no_show,
			)
			.map(|tick| Action::ScheduleWakeup {
				block_hash,
				block_number,
				candidate_hash,
				tick,
			})
		},
		RequiredTranches::Pending { considered, next_no_show, clock_drift, .. } => {
			// select the minimum of `next_no_show`, or the tick of the next non-empty tranche
			// after `considered`, including any tranche that might contain our own untriggered
			// assignment.
			let next_non_empty_tranche = {
				let next_announced = approval_entry
					.tranches()
					.iter()
					.skip_while(|t| t.tranche() <= considered)
					.map(|t| t.tranche())
					.next();

				let our_untriggered = approval_entry.our_assignment().and_then(|t| {
					if !t.triggered() && t.tranche() > considered {
						Some(t.tranche())
					} else {
						None
					}
				});

				// Apply the clock drift to these tranches.
				min_prefer_some(next_announced, our_untriggered)
					.map(|t| t as Tick + block_tick + clock_drift)
			};

			min_prefer_some(next_non_empty_tranche, next_no_show).map(|tick| {
				Action::ScheduleWakeup { block_hash, block_number, candidate_hash, tick }
			})
		},
	};

	match maybe_action {
		Some(Action::ScheduleWakeup { ref tick, .. }) => gum::trace!(
			target: LOG_TARGET,
			tick,
			?candidate_hash,
			?block_hash,
			block_tick,
			"Scheduling next wakeup.",
		),
		None => gum::trace!(
			target: LOG_TARGET,
			?candidate_hash,
			?block_hash,
			block_tick,
			"No wakeup needed.",
		),
		Some(_) => {}, // unreachable
	}

	maybe_action
}

async fn import_assignment<Sender>(
	sender: &mut Sender,
	state: &State,
	db: &mut OverlayedBackend<'_, impl Backend>,
	session_info_provider: &mut RuntimeInfo,
	checked_assignment: CheckedIndirectAssignment,
) -> SubsystemResult<(AssignmentCheckResult, Vec<Action>)>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	let tick_now = state.clock.tick_now();
	let assignment = checked_assignment.assignment();
	let candidate_indices = checked_assignment.candidate_indices();
	let tranche = checked_assignment.tranche();

	let block_entry = match db.load_block_entry(&assignment.block_hash)? {
		Some(b) => b,
		None => {
			return Ok((
				AssignmentCheckResult::Bad(AssignmentCheckError::UnknownBlock(
					assignment.block_hash,
				)),
				Vec::new(),
			))
		},
	};

	let session_info = match get_session_info_by_index(
		session_info_provider,
		sender,
		block_entry.parent_hash(),
		block_entry.session(),
	)
	.await
	{
		Some(s) => s,
		None => {
			return Ok((
				AssignmentCheckResult::Bad(AssignmentCheckError::UnknownSessionIndex(
					block_entry.session(),
				)),
				Vec::new(),
			))
		},
	};

	let n_cores = session_info.n_cores as usize;

	// Early check the candidate bitfield and core bitfields lengths < `n_cores`.
	// Core bitfield length is checked later in `check_assignment_cert`.
	if candidate_indices.len() > n_cores {
		gum::debug!(
			target: LOG_TARGET,
			validator = assignment.validator.0,
			n_cores,
			candidate_bitfield_len = ?candidate_indices.len(),
			"Oversized bitfield",
		);

		return Ok((
			AssignmentCheckResult::Bad(AssignmentCheckError::InvalidBitfield(
				candidate_indices.len(),
			)),
			Vec::new(),
		));
	}

	let mut claimed_core_indices = Vec::new();
	let mut assigned_candidate_hashes = Vec::new();

	for candidate_index in candidate_indices.iter_ones() {
		let (claimed_core_index, assigned_candidate_hash) =
			match block_entry.candidate(candidate_index) {
				Some((c, h)) => (*c, *h),
				None => {
					return Ok((
						AssignmentCheckResult::Bad(AssignmentCheckError::InvalidCandidateIndex(
							candidate_index as _,
						)),
						Vec::new(),
					))
				}, // no candidate at core.
			};

		let mut candidate_entry = match db.load_candidate_entry(&assigned_candidate_hash)? {
			Some(c) => c,
			None => {
				return Ok((
					AssignmentCheckResult::Bad(AssignmentCheckError::InvalidCandidate(
						candidate_index as _,
						assigned_candidate_hash,
					)),
					Vec::new(),
				))
			}, // no candidate at core.
		};

		if candidate_entry.approval_entry_mut(&assignment.block_hash).is_none() {
			return Ok((
				AssignmentCheckResult::Bad(AssignmentCheckError::Internal(
					assignment.block_hash,
					assigned_candidate_hash,
				)),
				Vec::new(),
			));
		};

		claimed_core_indices.push(claimed_core_index);
		assigned_candidate_hashes.push(assigned_candidate_hash);
	}

	// Error on null assignments.
	if claimed_core_indices.is_empty() {
		return Ok((
			AssignmentCheckResult::Bad(AssignmentCheckError::InvalidCert(
				assignment.validator,
				format!("{:?}", InvalidAssignmentReason::NullAssignment),
			)),
			Vec::new(),
		));
	}

	let mut actions = Vec::new();
	let res = {
		let mut is_duplicate = true;
		// Import the assignments for all cores in the cert.
		for (assigned_candidate_hash, candidate_index) in
			assigned_candidate_hashes.iter().zip(candidate_indices.iter_ones())
		{
			let mut candidate_entry = match db.load_candidate_entry(&assigned_candidate_hash)? {
				Some(c) => c,
				None => {
					return Ok((
						AssignmentCheckResult::Bad(AssignmentCheckError::InvalidCandidate(
							candidate_index as _,
							*assigned_candidate_hash,
						)),
						Vec::new(),
					))
				},
			};

			let approval_entry = match candidate_entry.approval_entry_mut(&assignment.block_hash) {
				Some(a) => a,
				None => {
					return Ok((
						AssignmentCheckResult::Bad(AssignmentCheckError::Internal(
							assignment.block_hash,
							*assigned_candidate_hash,
						)),
						Vec::new(),
					))
				},
			};

			let is_duplicate_for_candidate = approval_entry.is_assigned(assignment.validator);
			is_duplicate &= is_duplicate_for_candidate;
			approval_entry.import_assignment(
				tranche,
				assignment.validator,
				tick_now,
				is_duplicate_for_candidate,
			);

			// We've imported a new assignment, so we need to schedule a wake-up for when that might
			// no-show.
			if let Some((approval_entry, status)) = state
				.approval_status(sender, session_info_provider, &block_entry, &candidate_entry)
				.await
			{
				actions.extend(schedule_wakeup_action(
					approval_entry,
					block_entry.block_hash(),
					block_entry.block_number(),
					*assigned_candidate_hash,
					status.block_tick,
					tick_now,
					status.required_tranches,
				));
			}

			// We also write the candidate entry as it now contains the new candidate.
			db.write_candidate_entry(candidate_entry.into());
		}

		// Since we don't account for tranche in distribution message fingerprinting, some
		// validators can be assigned to the same core (VRF modulo vs VRF delay). These can be
		// safely ignored. However, if an assignment is for multiple cores (these are only
		// tranche0), we cannot ignore it, because it would mean ignoring other non duplicate
		// assignments.
		if is_duplicate {
			AssignmentCheckResult::AcceptedDuplicate
		} else if candidate_indices.count_ones() > 1 {
			gum::trace!(
				target: LOG_TARGET,
				validator = assignment.validator.0,
				candidate_hashes = ?assigned_candidate_hashes,
				assigned_cores = ?claimed_core_indices,
				?tranche,
				"Imported assignments for multiple cores.",
			);

			AssignmentCheckResult::Accepted
		} else {
			gum::trace!(
				target: LOG_TARGET,
				validator = assignment.validator.0,
				candidate_hashes = ?assigned_candidate_hashes,
				assigned_cores = ?claimed_core_indices,
				"Imported assignment for a single core.",
			);

			AssignmentCheckResult::Accepted
		}
	};

	Ok((res, actions))
}

async fn import_approval<Sender>(
	sender: &mut Sender,
	state: &mut State,
	db: &mut OverlayedBackend<'_, impl Backend>,
	session_info_provider: &mut RuntimeInfo,
	metrics: &Metrics,
	approval: CheckedIndirectSignedApprovalVote,
	wakeups: &Wakeups,
) -> SubsystemResult<(Vec<Action>, ApprovalCheckResult)>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	macro_rules! respond_early {
		($e: expr) => {{
			return Ok((Vec::new(), $e));
		}};
	}

	let block_entry = match db.load_block_entry(&approval.block_hash)? {
		Some(b) => b,
		None => {
			respond_early!(ApprovalCheckResult::Bad(ApprovalCheckError::UnknownBlock(
				approval.block_hash
			),))
		},
	};

	let approved_candidates_info: Result<Vec<(CandidateIndex, CandidateHash)>, ApprovalCheckError> =
		approval
			.candidate_indices
			.iter_ones()
			.map(|candidate_index| {
				block_entry
					.candidate(candidate_index)
					.ok_or(ApprovalCheckError::InvalidCandidateIndex(candidate_index as _))
					.map(|candidate| (candidate_index as _, candidate.1))
			})
			.collect();

	let approved_candidates_info = match approved_candidates_info {
		Ok(approved_candidates_info) => approved_candidates_info,
		Err(err) => {
			respond_early!(ApprovalCheckResult::Bad(err))
		},
	};

	gum::trace!(
		target: LOG_TARGET,
		"Received approval for num_candidates {:}",
		approval.candidate_indices.count_ones()
	);

	let mut actions = Vec::new();
	for (approval_candidate_index, approved_candidate_hash) in approved_candidates_info {
		let block_entry = match db.load_block_entry(&approval.block_hash)? {
			Some(b) => b,
			None => {
				respond_early!(ApprovalCheckResult::Bad(ApprovalCheckError::UnknownBlock(
					approval.block_hash
				),))
			},
		};

		let candidate_entry = match db.load_candidate_entry(&approved_candidate_hash)? {
			Some(c) => c,
			None => {
				respond_early!(ApprovalCheckResult::Bad(ApprovalCheckError::InvalidCandidate(
					approval_candidate_index,
					approved_candidate_hash
				),))
			},
		};

		// Don't accept approvals until assignment.
		match candidate_entry.approval_entry(&approval.block_hash) {
			None => {
				respond_early!(ApprovalCheckResult::Bad(ApprovalCheckError::Internal(
					approval.block_hash,
					approved_candidate_hash
				),))
			},
			Some(e) if !e.is_assigned(approval.validator) => {
				respond_early!(ApprovalCheckResult::Bad(ApprovalCheckError::NoAssignment(
					approval.validator
				),))
			},
			_ => {},
		}

		gum::trace!(
			target: LOG_TARGET,
			validator_index = approval.validator.0,
			candidate_hash = ?approved_candidate_hash,
			para_id = ?candidate_entry.candidate_receipt().descriptor.para_id(),
			"Importing approval vote",
		);

		let new_actions = advance_approval_state(
			sender,
			state,
			db,
			session_info_provider,
			&metrics,
			block_entry,
			approved_candidate_hash,
			candidate_entry,
			ApprovalStateTransition::RemoteApproval(approval.validator),
			wakeups,
		)
		.await;
		actions.extend(new_actions);
	}

	// importing the approval can be heavy as it may trigger acceptance for a series of blocks.
	Ok((actions, ApprovalCheckResult::Accepted))
}

#[derive(Debug)]
enum ApprovalStateTransition {
	RemoteApproval(ValidatorIndex),
	LocalApproval(ValidatorIndex),
	WakeupProcessed,
}

impl ApprovalStateTransition {
	fn validator_index(&self) -> Option<ValidatorIndex> {
		match *self {
			ApprovalStateTransition::RemoteApproval(v) |
			ApprovalStateTransition::LocalApproval(v) => Some(v),
			ApprovalStateTransition::WakeupProcessed => None,
		}
	}

	fn is_local_approval(&self) -> bool {
		match *self {
			ApprovalStateTransition::RemoteApproval(_) => false,
			ApprovalStateTransition::LocalApproval(_) => true,
			ApprovalStateTransition::WakeupProcessed => false,
		}
	}

	fn is_remote_approval(&self) -> bool {
		matches!(*self, ApprovalStateTransition::RemoteApproval(_))
	}
}

// Advance the approval state, either by importing an approval vote which is already checked to be
// valid and corresponding to an assigned validator on the candidate and block, or by noting that
// there are no further wakeups or tranches needed. This updates the block entry and candidate entry
// as necessary and schedules any further wakeups.
async fn advance_approval_state<Sender>(
	sender: &mut Sender,
	state: &mut State,
	db: &mut OverlayedBackend<'_, impl Backend>,
	session_info_provider: &mut RuntimeInfo,
	metrics: &Metrics,
	mut block_entry: BlockEntry,
	candidate_hash: CandidateHash,
	mut candidate_entry: CandidateEntry,
	transition: ApprovalStateTransition,
	wakeups: &Wakeups,
) -> Vec<Action>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	let validator_index = transition.validator_index();

	let already_approved_by = validator_index.as_ref().map(|v| candidate_entry.mark_approval(*v));
	let candidate_approved_in_block = block_entry.is_candidate_approved(&candidate_hash);

	// Check for early exits.
	//
	// If the candidate was approved
	// but not the block, it means that we still need more approvals for the candidate under the
	// block.
	//
	// If the block was approved, but the validator hadn't approved it yet, we should still hold
	// onto the approval vote on-disk in case we restart and rebroadcast votes. Otherwise, our
	// assignment might manifest as a no-show.
	if !transition.is_local_approval() {
		// We don't store remote votes and there's nothing to store for processed wakeups,
		// so we can early exit as long at the candidate is already concluded under the
		// block i.e. we don't need more approvals.
		if candidate_approved_in_block {
			return Vec::new();
		}
	}

	let mut actions = Vec::new();
	let block_hash = block_entry.block_hash();
	let block_number = block_entry.block_number();
	let session_index = block_entry.session();
	let para_id = candidate_entry.candidate_receipt().descriptor().para_id();
	let tick_now = state.clock.tick_now();

	let (is_approved, status) = if let Some((approval_entry, status)) = state
		.approval_status(sender, session_info_provider, &block_entry, &candidate_entry)
		.await
	{
		let check = approval_checking::check_approval(
			&candidate_entry,
			approval_entry,
			status.required_tranches.clone(),
		);
		state.observe_assignment_gathering_status(
			&metrics,
			&status.required_tranches,
			block_hash,
			block_entry.block_number(),
			candidate_hash,
		);

		// Check whether this is approved, while allowing a maximum
		// assignment tick of `now - APPROVAL_DELAY` - that is, that
		// all counted assignments are at least `APPROVAL_DELAY` ticks old.
		let is_approved = check.is_approved(tick_now.saturating_sub(APPROVAL_DELAY));
		if status.last_no_shows != 0 {
			metrics.on_observed_no_shows(status.last_no_shows);
			gum::trace!(
				target: LOG_TARGET,
				?candidate_hash,
				?block_hash,
				last_no_shows = ?status.last_no_shows,
				"Observed no_shows",
			);
		}
		if is_approved {
			gum::trace!(
				target: LOG_TARGET,
				?candidate_hash,
				?block_hash,
				"Candidate approved under block.",
			);

			let no_shows = check.known_no_shows();

			let was_block_approved = block_entry.is_fully_approved();
			block_entry.mark_approved_by_hash(&candidate_hash);
			let is_block_approved = block_entry.is_fully_approved();

			if no_shows != 0 {
				metrics.on_no_shows(no_shows);
			}
			if check == Check::ApprovedOneThird {
				// No-shows are not counted when more than one third of validators approve a
				// candidate, so count candidates where more than one third of validators had to
				// approve it, this is indicative of something breaking.
				metrics.on_approved_by_one_third()
			}

			metrics.on_candidate_approved(status.tranche_now as _);

			if is_block_approved && !was_block_approved {
				metrics.on_block_approved(status.tranche_now as _);
				actions.push(Action::NoteApprovedInChainSelection(block_hash));
			}

			db.write_block_entry(block_entry.into());
		} else if transition.is_local_approval() {
			// Local approvals always update the block_entry, so we need to flush it to
			// the database.
			db.write_block_entry(block_entry.into());
		}

		(is_approved, status)
	} else {
		gum::warn!(
			target: LOG_TARGET,
			?candidate_hash,
			?block_hash,
			?validator_index,
			"No approval entry for approval under block",
		);

		return Vec::new();
	};

	{
		let approval_entry = candidate_entry
			.approval_entry_mut(&block_hash)
			.expect("Approval entry just fetched; qed");

		let was_approved = approval_entry.is_approved();
		let newly_approved = is_approved && !was_approved;

		if is_approved {
			approval_entry.mark_approved();
		}
		if newly_approved {
			state.record_no_shows(session_index, para_id.into(), &status.no_show_validators);
		}
		actions.extend(schedule_wakeup_action(
			&approval_entry,
			block_hash,
			block_number,
			candidate_hash,
			status.block_tick,
			tick_now,
			status.required_tranches,
		));

		if is_approved && transition.is_remote_approval() {
			// Make sure we wake other blocks in case they have
			// a no-show that might be covered by this approval.
			for (fork_block_hash, fork_approval_entry) in candidate_entry
				.block_assignments
				.iter()
				.filter(|(hash, _)| **hash != block_hash)
			{
				let assigned_on_fork_block = validator_index
					.as_ref()
					.map(|validator_index| fork_approval_entry.is_assigned(*validator_index))
					.unwrap_or_default();
				if wakeups.wakeup_for(*fork_block_hash, candidate_hash).is_none() &&
					!fork_approval_entry.is_approved() &&
					assigned_on_fork_block
				{
					let fork_block_entry = db.load_block_entry(fork_block_hash);
					if let Ok(Some(fork_block_entry)) = fork_block_entry {
						actions.push(Action::ScheduleWakeup {
							block_hash: *fork_block_hash,
							block_number: fork_block_entry.block_number(),
							candidate_hash,
							// Schedule the wakeup next tick, since the assignment must be a
							// no-show, because there is no-wakeup scheduled.
							tick: tick_now + 1,
						})
					} else {
						gum::debug!(
							target: LOG_TARGET,
							?fork_block_entry,
							?fork_block_hash,
							"Failed to load block entry"
						)
					}
				}
			}
		}
		// We have no need to write the candidate entry if all of the following
		// is true:
		//
		// 1. This is not a local approval, as we don't store anything new in the approval entry.
		// 2. The candidate is not newly approved, as we haven't altered the approval entry's
		//    approved flag with `mark_approved` above.
		// 3. The approver, if any, had already approved the candidate, as we haven't altered the
		// bitfield.
		if transition.is_local_approval() || newly_approved || !already_approved_by.unwrap_or(true)
		{
			// In all other cases, we need to write the candidate entry.
			db.write_candidate_entry(candidate_entry);
		}
	}

	actions
}

fn should_trigger_assignment(
	approval_entry: &ApprovalEntry,
	candidate_entry: &CandidateEntry,
	required_tranches: RequiredTranches,
	tranche_now: DelayTranche,
) -> bool {
	match approval_entry.our_assignment() {
		None => false,
		Some(ref assignment) if assignment.triggered() => false,
		Some(ref assignment) if assignment.tranche() == 0 => true,
		Some(ref assignment) => {
			match required_tranches {
				RequiredTranches::All => !approval_checking::check_approval(
					&candidate_entry,
					&approval_entry,
					RequiredTranches::All,
				)
				// when all are required, we are just waiting for the first 1/3+
				.is_approved(Tick::max_value()),
				RequiredTranches::Pending { maximum_broadcast, clock_drift, .. } => {
					let drifted_tranche_now =
						tranche_now.saturating_sub(clock_drift as DelayTranche);
					assignment.tranche() <= maximum_broadcast &&
						assignment.tranche() <= drifted_tranche_now
				},
				RequiredTranches::Exact { .. } => {
					// indicates that no new assignments are needed at the moment.
					false
				},
			}
		},
	}
}

async fn process_wakeup<Sender: SubsystemSender<RuntimeApiMessage>>(
	sender: &mut Sender,
	state: &mut State,
	db: &mut OverlayedBackend<'_, impl Backend>,
	session_info_provider: &mut RuntimeInfo,
	relay_block: Hash,
	candidate_hash: CandidateHash,
	metrics: &Metrics,
	wakeups: &Wakeups,
) -> SubsystemResult<Vec<Action>> {
	let block_entry = db.load_block_entry(&relay_block)?;
	let candidate_entry = db.load_candidate_entry(&candidate_hash)?;

	// If either is not present, we have nothing to wakeup. Might have lost a race with finality
	let (mut block_entry, mut candidate_entry) = match (block_entry, candidate_entry) {
		(Some(b), Some(c)) => (b, c),
		_ => return Ok(Vec::new()),
	};

	let (no_show_slots, needed_approvals) = match get_session_info_by_index(
		session_info_provider,
		sender,
		block_entry.block_hash(),
		block_entry.session(),
	)
	.await
	{
		Some(i) => (i.no_show_slots, i.needed_approvals),
		None => return Ok(Vec::new()),
	};

	let block_tick = slot_number_to_tick(state.slot_duration_millis, block_entry.slot());
	let no_show_duration =
		slot_number_to_tick(state.slot_duration_millis, Slot::from(u64::from(no_show_slots)));
	let tranche_now = state.clock.tranche_now(state.slot_duration_millis, block_entry.slot());

	gum::trace!(
		target: LOG_TARGET,
		tranche = tranche_now,
		?candidate_hash,
		block_hash = ?relay_block,
		"Processing wakeup",
	);

	let (should_trigger, backing_group) = {
		let approval_entry = match candidate_entry.approval_entry(&relay_block) {
			Some(e) => e,
			None => return Ok(Vec::new()),
		};

		let tranches_to_approve = approval_checking::tranches_to_approve(
			&approval_entry,
			candidate_entry.approvals(),
			tranche_now,
			block_tick,
			no_show_duration,
			needed_approvals as _,
		);

		let should_trigger = should_trigger_assignment(
			&approval_entry,
			&candidate_entry,
			tranches_to_approve.required_tranches,
			tranche_now,
		);

		(should_trigger, approval_entry.backing_group())
	};

	gum::trace!(target: LOG_TARGET, "Wakeup processed. Should trigger: {}", should_trigger);

	let mut actions = Vec::new();
	let candidate_receipt = candidate_entry.candidate_receipt().clone();

	let maybe_cert = if should_trigger {
		let maybe_cert = {
			let approval_entry = candidate_entry
				.approval_entry_mut(&relay_block)
				.expect("should_trigger only true if this fetched earlier; qed");

			approval_entry.trigger_our_assignment(state.clock.tick_now())
		};

		db.write_candidate_entry(candidate_entry.clone());

		maybe_cert
	} else {
		None
	};

	if let Some((cert, val_index, tranche)) = maybe_cert {
		let indirect_cert =
			IndirectAssignmentCertV2 { block_hash: relay_block, validator: val_index, cert };

		gum::trace!(
			target: LOG_TARGET,
			?candidate_hash,
			para_id = ?candidate_receipt.descriptor.para_id(),
			block_hash = ?relay_block,
			"Launching approval work.",
		);

		let candidate_core_index = block_entry
			.candidates()
			.iter()
			.find_map(|(core_index, h)| (h == &candidate_hash).then_some(*core_index));

		let claimed_core_indices = get_assignment_core_indices(&indirect_cert.cert.kind);
		match cores_to_candidate_indices(&claimed_core_indices, &block_entry) {
			Ok(claimed_candidate_indices) => {
				// Ensure we distribute multiple core assignments just once.
				let distribute_assignment = if claimed_candidate_indices.count_ones() > 1 {
					!block_entry.mark_assignment_distributed(claimed_candidate_indices.clone())
				} else {
					true
				};
				db.write_block_entry(block_entry.clone());
				actions.push(Action::LaunchApproval {
					claimed_candidate_indices,
					candidate_hash,
					indirect_cert,
					assignment_tranche: tranche,
					relay_block_hash: relay_block,
					session: block_entry.session(),
					candidate: candidate_receipt,
					backing_group,
					distribute_assignment,
					core_index: candidate_core_index,
				});
			},
			Err(err) => {
				// Never happens, it should only happen if no cores are claimed, which is a
				// bug.
				gum::warn!(
					target: LOG_TARGET,
					block_hash = ?relay_block,
					?err,
					"Failed to create assignment bitfield"
				);
			},
		};
	}
	// Although we checked approval earlier in this function,
	// this wakeup might have advanced the state to approved via
	// a no-show that was immediately covered and therefore
	// we need to check for that and advance the state on-disk.
	//
	// Note that this function also schedules a wakeup as necessary.
	actions.extend(
		advance_approval_state(
			sender,
			state,
			db,
			session_info_provider,
			metrics,
			block_entry,
			candidate_hash,
			candidate_entry,
			ApprovalStateTransition::WakeupProcessed,
			wakeups,
		)
		.await,
	);

	Ok(actions)
}

// Launch approval work, returning an `AbortHandle` which corresponds to the background task
// spawned. When the background work is no longer needed, the `AbortHandle` should be dropped
// to cancel the background work and any requests it has spawned.
#[overseer::contextbounds(ApprovalVoting, prefix = self::overseer)]
async fn launch_approval<
	Sender: SubsystemSender<RuntimeApiMessage>
		+ SubsystemSender<AvailabilityRecoveryMessage>
		+ SubsystemSender<DisputeCoordinatorMessage>
		+ SubsystemSender<CandidateValidationMessage>,
>(
	mut sender: Sender,
	spawn_handle: Arc<dyn overseer::gen::Spawner + 'static>,
	metrics: Metrics,
	session_index: SessionIndex,
	candidate: CandidateReceipt,
	validator_index: ValidatorIndex,
	block_hash: Hash,
	backing_group: GroupIndex,
	core_index: Option<CoreIndex>,
	retry: RetryApprovalInfo,
) -> SubsystemResult<RemoteHandle<ApprovalState>> {
	let (a_tx, a_rx) = oneshot::channel();
	let (code_tx, code_rx) = oneshot::channel();

	// The background future returned by this function may
	// be dropped before completing. This guard is used to ensure that the approval
	// work is correctly counted as stale even if so.
	struct StaleGuard(Option<Metrics>);

	impl StaleGuard {
		fn take(mut self) -> Metrics {
			self.0.take().expect(
				"
				consumed after take; so this cannot be called twice; \
				nothing in this function reaches into the struct to avoid this API; \
				qed
			",
			)
		}
	}

	impl Drop for StaleGuard {
		fn drop(&mut self) {
			if let Some(metrics) = self.0.as_ref() {
				metrics.on_approval_stale();
			}
		}
	}

	let candidate_hash = candidate.hash();
	let para_id = candidate.descriptor.para_id();
	let mut next_retry = None;
	gum::trace!(target: LOG_TARGET, ?candidate_hash, ?para_id, "Recovering data.");

	let timer = metrics.time_recover_and_approve();
	sender
		.send_message(AvailabilityRecoveryMessage::RecoverAvailableData(
			candidate.clone(),
			session_index,
			Some(backing_group),
			core_index,
			a_tx,
		))
		.await;

	sender
		.send_message(RuntimeApiMessage::Request(
			block_hash,
			RuntimeApiRequest::ValidationCodeByHash(
				candidate.descriptor.validation_code_hash(),
				code_tx,
			),
		))
		.await;

	let candidate = candidate.clone();
	let metrics_guard = StaleGuard(Some(metrics));
	let background = async move {
		// Force the move of the timer into the background task.
		let _timer = timer;
		let available_data = match a_rx.await {
			Err(_) => return ApprovalState::failed(validator_index, candidate_hash),
			Ok(Ok(a)) => a,
			Ok(Err(e)) => {
				match &e {
					&RecoveryError::Unavailable => {
						gum::warn!(
							target: LOG_TARGET,
							?para_id,
							?candidate_hash,
							attempts_remaining = retry.attempts_remaining,
							"Data unavailable for candidate {:?}",
							(candidate_hash, candidate.descriptor.para_id()),
						);
						// Availability could fail if we did not discover much of the network, so
						// let's back off and order the subsystem to retry at a later point if the
						// approval is still needed, because no-show wasn't covered yet.
						if retry.attempts_remaining > 0 {
							Delay::new(retry.backoff).await;
							next_retry = Some(RetryApprovalInfo {
								candidate,
								backing_group,
								core_index,
								session_index,
								attempts_remaining: retry.attempts_remaining - 1,
								backoff: retry.backoff,
							});
						} else {
							next_retry = None;
						}
						metrics_guard.take().on_approval_unavailable();
					},
					&RecoveryError::ChannelClosed => {
						gum::warn!(
							target: LOG_TARGET,
							?para_id,
							?candidate_hash,
							"Channel closed while recovering data for candidate {:?}",
							(candidate_hash, candidate.descriptor.para_id()),
						);
						// do nothing. we'll just be a no-show and that'll cause others to rise up.
						metrics_guard.take().on_approval_unavailable();
					},
					&RecoveryError::Invalid => {
						gum::warn!(
							target: LOG_TARGET,
							?para_id,
							?candidate_hash,
							"Data recovery invalid for candidate {:?}",
							(candidate_hash, candidate.descriptor.para_id()),
						);
						issue_local_invalid_statement(
							&mut sender,
							session_index,
							candidate_hash,
							candidate.clone(),
						);
						metrics_guard.take().on_approval_invalid();
					},
				}
				return ApprovalState::failed_with_retry(
					validator_index,
					candidate_hash,
					next_retry,
				);
			},
		};

		let validation_code = match code_rx.await {
			Err(_) => return ApprovalState::failed(validator_index, candidate_hash),
			Ok(Err(_)) => return ApprovalState::failed(validator_index, candidate_hash),
			Ok(Ok(Some(code))) => code,
			Ok(Ok(None)) => {
				gum::warn!(
					target: LOG_TARGET,
					"Validation code unavailable for block {:?} in the state of block {:?} (a recent descendant)",
					candidate.descriptor.relay_parent(),
					block_hash,
				);

				// No dispute necessary, as this indicates that the chain is not behaving
				// according to expectations.
				metrics_guard.take().on_approval_unavailable();
				return ApprovalState::failed(validator_index, candidate_hash);
			},
		};

		let (val_tx, val_rx) = oneshot::channel();
		sender
			.send_message(CandidateValidationMessage::ValidateFromExhaustive {
				validation_data: available_data.validation_data,
				validation_code,
				candidate_receipt: candidate.clone(),
				pov: available_data.pov,
				scheduling_session_index: session_index,
				exec_kind: PvfExecKind::Approval,
				response_sender: val_tx,
			})
			.await;

		match val_rx.await {
			Err(_) => return ApprovalState::failed(validator_index, candidate_hash),
			Ok(Ok(ValidationResult::Valid(_, _))) => {
				// Validation checked out. Issue an approval command. If the underlying service is
				// unreachable, then there isn't anything we can do.

				gum::trace!(target: LOG_TARGET, ?candidate_hash, ?para_id, "Candidate Valid");

				let _ = metrics_guard.take();
				return ApprovalState::approved(validator_index, candidate_hash);
			},
			Ok(Ok(ValidationResult::Invalid(reason))) => {
				gum::warn!(
					target: LOG_TARGET,
					?reason,
					?candidate_hash,
					?para_id,
					"Detected invalid candidate as an approval checker.",
				);

				issue_local_invalid_statement(
					&mut sender,
					session_index,
					candidate_hash,
					candidate.clone(),
				);
				metrics_guard.take().on_approval_invalid();
				return ApprovalState::failed(validator_index, candidate_hash);
			},
			Ok(Err(e)) => {
				gum::error!(
					target: LOG_TARGET,
					err = ?e,
					?candidate_hash,
					?para_id,
					"Failed to validate candidate due to internal error",
				);
				metrics_guard.take().on_approval_error();
				return ApprovalState::failed(validator_index, candidate_hash);
			},
		}
	};
	let (background, remote_handle) = background.remote_handle();
	spawn_handle.spawn("approval-checks", Some("approval-voting-subsystem"), Box::pin(background));
	Ok(remote_handle)
}

// Issue and import a local approval vote. Should only be invoked after approval checks
// have been done.
#[overseer::contextbounds(ApprovalVoting, prefix = self::overseer)]
async fn issue_approval<
	Sender: SubsystemSender<RuntimeApiMessage>,
	ADSender: SubsystemSender<ApprovalDistributionMessage>,
>(
	sender: &mut Sender,
	approval_voting_sender: &mut ADSender,
	state: &mut State,
	db: &mut OverlayedBackend<'_, impl Backend>,
	session_info_provider: &mut RuntimeInfo,
	metrics: &Metrics,
	candidate_hash: CandidateHash,
	delayed_approvals_timers: &mut DelayedApprovalTimer,
	ApprovalVoteRequest { validator_index, block_hash }: ApprovalVoteRequest,
	wakeups: &Wakeups,
) -> SubsystemResult<Vec<Action>> {
	let mut block_entry = match db.load_block_entry(&block_hash)? {
		Some(b) => b,
		None => {
			// not a cause for alarm - just lost a race with pruning, most likely.
			metrics.on_approval_stale();
			return Ok(Vec::new());
		},
	};

	let candidate_index = match block_entry.candidates().iter().position(|e| e.1 == candidate_hash)
	{
		None => {
			gum::warn!(
				target: LOG_TARGET,
				"Candidate hash {} is not present in the block entry's candidates for relay block {}",
				candidate_hash,
				block_entry.parent_hash(),
			);

			metrics.on_approval_error();
			return Ok(Vec::new());
		},
		Some(idx) => idx,
	};

	let candidate_hash = match block_entry.candidate(candidate_index as usize) {
		Some((_, h)) => *h,
		None => {
			gum::warn!(
				target: LOG_TARGET,
				"Received malformed request to approve out-of-bounds candidate index {} included at block {:?}",
				candidate_index,
				block_hash,
			);

			metrics.on_approval_error();
			return Ok(Vec::new());
		},
	};

	let candidate_entry = match db.load_candidate_entry(&candidate_hash)? {
		Some(c) => c,
		None => {
			gum::warn!(
				target: LOG_TARGET,
				"Missing entry for candidate index {} included at block {:?}",
				candidate_index,
				block_hash,
			);

			metrics.on_approval_error();
			return Ok(Vec::new());
		},
	};

	let session_info = match get_session_info_by_index(
		session_info_provider,
		sender,
		block_entry.parent_hash(),
		block_entry.session(),
	)
	.await
	{
		Some(s) => s,
		None => return Ok(Vec::new()),
	};

	if block_entry
		.defer_candidate_signature(
			candidate_index as _,
			candidate_hash,
			compute_delayed_approval_sending_tick(
				state,
				&block_entry,
				&candidate_entry,
				session_info,
				&metrics,
			),
		)
		.is_some()
	{
		gum::error!(
			target: LOG_TARGET,
			?candidate_hash,
			?block_hash,
			validator_index = validator_index.0,
			"Possible bug, we shouldn't have to defer a candidate more than once",
		);
	}

	gum::debug!(
		target: LOG_TARGET,
		?candidate_hash,
		?block_hash,
		validator_index = validator_index.0,
		"Ready to issue approval vote",
	);

	let actions = advance_approval_state(
		sender,
		state,
		db,
		session_info_provider,
		metrics,
		block_entry,
		candidate_hash,
		candidate_entry,
		ApprovalStateTransition::LocalApproval(validator_index as _),
		wakeups,
	)
	.await;

	if let Some(next_wakeup) = maybe_create_signature(
		db,
		session_info_provider,
		state,
		sender,
		approval_voting_sender,
		block_hash,
		validator_index,
		metrics,
	)
	.await?
	{
		delayed_approvals_timers.maybe_arm_timer(
			next_wakeup,
			state.clock.as_ref(),
			block_hash,
			validator_index,
		);
	}
	Ok(actions)
}

// Create signature for the approved candidates pending signatures
#[overseer::contextbounds(ApprovalVoting, prefix = self::overseer)]
async fn maybe_create_signature<
	Sender: SubsystemSender<RuntimeApiMessage>,
	ADSender: SubsystemSender<ApprovalDistributionMessage>,
>(
	db: &mut OverlayedBackend<'_, impl Backend>,
	session_info_provider: &mut RuntimeInfo,
	state: &State,
	sender: &mut Sender,
	approval_voting_sender: &mut ADSender,
	block_hash: Hash,
	validator_index: ValidatorIndex,
	metrics: &Metrics,
) -> SubsystemResult<Option<Tick>> {
	let mut block_entry = match db.load_block_entry(&block_hash)? {
		Some(b) => b,
		None => {
			// not a cause for alarm - just lost a race with pruning, most likely.
			metrics.on_approval_stale();
			gum::debug!(
				target: LOG_TARGET,
				"Could not find block that needs signature {:}", block_hash
			);
			return Ok(None);
		},
	};

	let approval_params = session_info_provider
		.get_session_info_by_index(sender, block_hash, block_entry.session())
		.await
		.map(|info| info.approval_voting_params)
		.unwrap_or_default();

	gum::trace!(
		target: LOG_TARGET,
		"Candidates pending signatures {:}", block_entry.num_candidates_pending_signature()
	);
	let tick_now = state.clock.tick_now();

	let (candidates_to_sign, sign_no_later_then) = block_entry
		.get_candidates_that_need_signature(tick_now, approval_params.max_approval_coalesce_count);

	let (candidates_hashes, candidates_indices) = match candidates_to_sign {
		Some(candidates_to_sign) => candidates_to_sign,
		None => return Ok(sign_no_later_then),
	};

	let session_info = match get_session_info_by_index(
		session_info_provider,
		sender,
		block_entry.parent_hash(),
		block_entry.session(),
	)
	.await
	{
		Some(s) => s,
		None => {
			metrics.on_approval_error();
			gum::error!(
				target: LOG_TARGET,
				"Could not retrieve the session"
			);
			return Ok(None);
		},
	};

	let validator_pubkey = match session_info.validators.get(validator_index) {
		Some(p) => p,
		None => {
			gum::error!(
				target: LOG_TARGET,
				"Validator index {} out of bounds in session {}",
				validator_index.0,
				block_entry.session(),
			);

			metrics.on_approval_error();
			return Ok(None);
		},
	};

	let signature = match sign_approval(
		&state.keystore,
		&validator_pubkey,
		&candidates_hashes,
		block_entry.session(),
	) {
		Some(sig) => sig,
		None => {
			gum::error!(
				target: LOG_TARGET,
				validator_index = ?validator_index,
				session = ?block_entry.session(),
				"Could not issue approval signature. Assignment key present but not validator key?",
			);

			metrics.on_approval_error();
			return Ok(None);
		},
	};
	metrics.on_approval_coalesce(candidates_hashes.len() as u32);

	let candidate_entries = candidates_hashes
		.iter()
		.map(|candidate_hash| db.load_candidate_entry(candidate_hash))
		.collect::<SubsystemResult<Vec<Option<CandidateEntry>>>>()?;

	for mut candidate_entry in candidate_entries {
		let approval_entry = candidate_entry.as_mut().and_then(|candidate_entry| {
			candidate_entry.approval_entry_mut(&block_entry.block_hash())
		});

		match approval_entry {
			Some(approval_entry) => approval_entry.import_approval_sig(OurApproval {
				signature: signature.clone(),
				signed_candidates_indices: candidates_indices.clone(),
			}),
			None => {
				gum::error!(
					target: LOG_TARGET,
					candidate_entry = ?candidate_entry,
					"Candidate scheduled for signing approval entry should not be None"
				);
			},
		};
		candidate_entry.map(|candidate_entry| db.write_candidate_entry(candidate_entry));
	}

	metrics.on_approval_produced();

	approval_voting_sender.send_unbounded_message(ApprovalDistributionMessage::DistributeApproval(
		IndirectSignedApprovalVoteV2 {
			block_hash: block_entry.block_hash(),
			candidate_indices: candidates_indices,
			validator: validator_index,
			signature,
		},
	));

	gum::trace!(
		target: LOG_TARGET,
		?block_hash,
		signed_candidates = ?block_entry.num_candidates_pending_signature(),
		"Issue approval votes",
	);
	block_entry.issued_approval();
	db.write_block_entry(block_entry.into());
	Ok(None)
}

// Sign an approval vote. Fails if the key isn't present in the store.
fn sign_approval(
	keystore: &LocalKeystore,
	public: &ValidatorId,
	candidate_hashes: &[CandidateHash],
	session_index: SessionIndex,
) -> Option<ValidatorSignature> {
	let key = keystore.key_pair::<ValidatorPair>(public).ok().flatten()?;

	let payload = ApprovalVoteMultipleCandidates(candidate_hashes).signing_payload(session_index);

	Some(key.sign(&payload[..]))
}

/// Send `IssueLocalStatement` to dispute-coordinator.
fn issue_local_invalid_statement<Sender>(
	sender: &mut Sender,
	session_index: SessionIndex,
	candidate_hash: CandidateHash,
	candidate: CandidateReceipt,
) where
	Sender: SubsystemSender<DisputeCoordinatorMessage>,
{
	// We need to send an unbounded message here to break a cycle:
	// DisputeCoordinatorMessage::IssueLocalStatement ->
	// ApprovalVotingMessage::GetApprovalSignaturesForCandidate.
	//
	// Use of unbounded _should_ be fine here as raising a dispute should be an
	// exceptional event. Even in case of bugs: There can be no more than
	// number of slots per block requests every block. Also for sending this
	// message a full recovery and validation procedure took place, which takes
	// longer than issuing a local statement + import.
	sender.send_unbounded_message(DisputeCoordinatorMessage::IssueLocalStatement(
		session_index,
		candidate_hash,
		candidate.clone(),
		false,
	));
}

// Computes what is the latest tick we can send an approval
fn compute_delayed_approval_sending_tick(
	state: &State,
	block_entry: &BlockEntry,
	candidate_entry: &CandidateEntry,
	session_info: &SessionInfo,
	metrics: &Metrics,
) -> Tick {
	let current_block_tick = slot_number_to_tick(state.slot_duration_millis, block_entry.slot());
	let assignment_tranche = candidate_entry
		.approval_entry(&block_entry.block_hash())
		.and_then(|approval_entry| approval_entry.our_assignment())
		.map(|our_assignment| our_assignment.tranche())
		.unwrap_or_default();

	let assignment_triggered_tick = current_block_tick + assignment_tranche as Tick;

	let no_show_duration_ticks = slot_number_to_tick(
		state.slot_duration_millis,
		Slot::from(u64::from(session_info.no_show_slots)),
	);
	let tick_now = state.clock.tick_now();

	let sign_no_later_than = min(
		tick_now + MAX_APPROVAL_COALESCE_WAIT_TICKS as Tick,
		// We don't want to accidentally cause no-shows, so if we are past
		// the second half of the no show time, force the sending of the
		// approval immediately.
		assignment_triggered_tick + no_show_duration_ticks / 2,
	);

	metrics.on_delayed_approval(sign_no_later_than.checked_sub(tick_now).unwrap_or_default());
	sign_no_later_than
}
