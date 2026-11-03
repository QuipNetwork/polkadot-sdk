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

//! The Candidate Validation subsystem.
//!
//! This handles incoming requests from other subsystems to validate candidates
//! according to a validation function. This delegates validation to an underlying
//! pool of processes used for execution of the Wasm.

#![deny(unused_crate_dependencies, unused_results)]
#![warn(missing_docs)]

use polkadot_node_core_pvf::{
	InternalValidationError, InvalidCandidate as WasmInvalidCandidate, PossiblyInvalidError,
	PrepareError, PrepareJobKind, PvfPrepData, ValidationError, ValidationHost,
};
use polkadot_node_core_pvf_common::execute::ValidationContext;
use polkadot_node_primitives::{InvalidCandidate, PoV, ValidationResult, DISPUTE_WINDOW};
use polkadot_node_subsystem::{
	errors::RuntimeApiError,
	messages::{
		CandidateValidationMessage, ChainApiMessage, PreCheckOutcome, PvfExecKind,
		RuntimeApiMessage, RuntimeApiRequest, ValidationFailed,
	},
	overseer, FromOrchestra, OverseerSignal, SpawnedSubsystem, SubsystemError, SubsystemResult,
	SubsystemSender,
};
use polkadot_node_subsystem_util::{
	self as util, request_node_features, request_session_executor_params,
	runtime::{fetch_scheduling_lookahead, ClaimQueueSnapshot},
};
use polkadot_overseer::{ActivatedLeaf, ActiveLeavesUpdate};
use polkadot_parachain_primitives::primitives::ValidationResult as WasmValidationResult;
use polkadot_primitives::{
	executor_params::{
		DEFAULT_APPROVAL_EXECUTION_TIMEOUT, DEFAULT_BACKING_EXECUTION_TIMEOUT,
		DEFAULT_LENIENT_PREPARATION_TIMEOUT, DEFAULT_PRECHECK_PREPARATION_TIMEOUT,
	},
	node_features::FeatureIndex,
	transpose_claim_queue, AuthorityDiscoveryId, CandidateCommitments,
	CandidateDescriptorV2 as CandidateDescriptor, CandidateEvent,
	CandidateReceiptV2 as CandidateReceipt,
	CommittedCandidateReceiptV2 as CommittedCandidateReceipt, ExecutorParams, Hash,
	PersistedValidationData, PvfExecKind as RuntimePvfExecKind, PvfPrepKind, SessionIndex,
	ValidationCode, ValidationCodeHash, ValidatorId,
};
use sp_application_crypto::{AppCrypto, ByteArray};
use sp_keystore::KeystorePtr;

use codec::Encode;

use futures::{channel::oneshot, prelude::*, stream::FuturesUnordered};

use schnellru::{ByLength, LruMap};

use std::{
	collections::HashSet,
	path::PathBuf,
	pin::Pin,
	sync::Arc,
	time::{Duration, Instant},
};

use async_trait::async_trait;

mod metrics;
use self::metrics::Metrics;

#[cfg(test)]
mod tests;

const LOG_TARGET: &'static str = "parachain::candidate-validation";

/// The amount of time to wait before retrying after a retry-able approval validation error. We use
/// a higher value for the approval case since we have more time, and if we wait longer it is more
/// likely that transient conditions will resolve.
#[cfg(not(test))]
const PVF_APPROVAL_EXECUTION_RETRY_DELAY: Duration = Duration::from_secs(3);
#[cfg(test)]
const PVF_APPROVAL_EXECUTION_RETRY_DELAY: Duration = Duration::from_millis(200);

// The task queue size is chosen to be somewhat bigger than the PVF host incoming queue size
// to allow exhaustive validation messages to fall through in case the tasks are clogged
const TASK_LIMIT: usize = 30;

/// Configuration for the candidate validation subsystem
#[derive(Clone, Default)]
pub struct Config {
	/// The path where candidate validation can store compiled artifacts for PVFs.
	pub artifacts_cache_path: PathBuf,
	/// The version of the node. `None` can be passed to skip the version check (only for tests).
	pub node_version: Option<String>,
	/// Whether the node is attempting to run as a secure validator.
	pub secure_validator_mode: bool,
	/// Path to the preparation worker binary
	pub prep_worker_path: PathBuf,
	/// Path to the execution worker binary
	pub exec_worker_path: PathBuf,
	/// The maximum number of pvf execution workers.
	pub pvf_execute_workers_max_num: usize,
	/// The maximum number of pvf workers that can be spawned in the pvf prepare pool for tasks
	/// with the priority below critical.
	pub pvf_prepare_workers_soft_max_num: usize,
	/// The absolute number of pvf workers that can be spawned in the pvf prepare pool.
	pub pvf_prepare_workers_hard_max_num: usize,
}

/// The candidate validation subsystem.
pub struct CandidateValidationSubsystem {
	keystore: KeystorePtr,
	#[allow(missing_docs)]
	pub metrics: Metrics,
	#[allow(missing_docs)]
	pub pvf_metrics: polkadot_node_core_pvf::Metrics,
	config: Option<Config>,
}

impl CandidateValidationSubsystem {
	/// Create a new `CandidateValidationSubsystem`.
	pub fn with_config(
		config: Option<Config>,
		keystore: KeystorePtr,
		metrics: Metrics,
		pvf_metrics: polkadot_node_core_pvf::Metrics,
	) -> Self {
		CandidateValidationSubsystem { keystore, config, metrics, pvf_metrics }
	}
}

#[overseer::subsystem(CandidateValidation, error=SubsystemError, prefix=self::overseer)]
impl<Context> CandidateValidationSubsystem {
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		if let Some(config) = self.config {
			let future = run(ctx, self.keystore, self.metrics, self.pvf_metrics, config)
				.map_err(|e| SubsystemError::with_origin("candidate-validation", e))
				.boxed();
			SpawnedSubsystem { name: "candidate-validation-subsystem", future }
		} else {
			polkadot_overseer::DummySubsystem.start(ctx)
		}
	}
}

// Returns the claim queue at relay parent and logs a warning if it is not available.
async fn claim_queue<Sender>(relay_parent: Hash, sender: &mut Sender) -> Option<ClaimQueueSnapshot>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	match util::runtime::fetch_claim_queue(sender, relay_parent).await {
		Ok(cq) => Some(cq),
		Err(err) => {
			gum::warn!(
				target: LOG_TARGET,
				?relay_parent,
				?err,
				"Claim queue not available"
			);
			None
		},
	}
}

/// Session-scoped parameters needed for candidate validation.
///
/// Each field may come from a different session for V3+ descriptors:
/// - `executor_params`: relay-parent (execution) session.
/// - `validation_code_bomb_limit`: scheduling session
///
/// For V1 descriptors both sessions are identical.
#[derive(Clone)]
struct SessionParams {
	/// Fetched for the relay-parent (execution) session.
	executor_params: ExecutorParams,
	/// Fetched for the scheduling session.
	validation_code_bomb_limit: u32,
}

/// Per-session cache for parameters needed during candidate validation.
struct SessionCache {
	/// Cached executor parameters, keyed by session index.
	executor_params: LruMap<SessionIndex, ExecutorParams>,
	/// Cached validation code bomb limits, keyed by session index.
	bomb_limit: LruMap<SessionIndex, u32>,
}

impl SessionCache {
	fn new() -> Self {
		Self {
			executor_params: LruMap::new(ByLength::new(DISPUTE_WINDOW.get())),
			bomb_limit: LruMap::new(ByLength::new(DISPUTE_WINDOW.get())),
		}
	}

	/// Fetch session-scoped parameters for a candidate.
	///
	/// For V2+ descriptors the sessions come from the descriptor itself. For V1
	/// descriptors `scheduling_session_index` is used as fallback for both
	/// execution and scheduling session (they are identical in V1). Results are
	/// cached per session.
	async fn fetch_params<Sender>(
		&mut self,
		recent_leaf: Hash,
		scheduling_session_index: SessionIndex,
		candidate_descriptor: &CandidateDescriptor,
		v3_ever_seen: bool,
		sender: &mut Sender,
	) -> Result<SessionParams, String>
	where
		Sender: SubsystemSender<RuntimeApiMessage>,
	{
		// Executor params: relay-parent (execution) session.
		// V2+ has this in the descriptor, V1 falls back to scheduling_session_index.
		let execution_session = candidate_descriptor
			.session_index_for_candidate_validation(v3_ever_seen)
			.unwrap_or(scheduling_session_index);

		let executor_params = match self.executor_params.get(&execution_session) {
			Some(cached) => cached.clone(),
			None => {
				let params =
					request_session_executor_params(recent_leaf, execution_session, sender)
						.await
						.await
						.map_err(|e| format!("Cannot fetch executor params: channel error: {e:?}"))?
						.map_err(|e| format!("Cannot fetch executor params: runtime error: {e:?}"))?
						.ok_or_else(|| "Executor params not found for session".to_string())?;
				let _ = self.executor_params.insert(execution_session, params.clone());
				params
			},
		};

		// Bomb limit uses the scheduling session. Both scheduling and execution
		// session would be sensible, what matters is that validators agree.
		let scheduling_session = candidate_descriptor
			.scheduling_session_for_candidate_validation(v3_ever_seen)
			.unwrap_or(scheduling_session_index);

		let validation_code_bomb_limit = match self.bomb_limit.get(&scheduling_session) {
			Some(cached) => *cached,
			None => {
				let limit = util::runtime::fetch_validation_code_bomb_limit(
					recent_leaf,
					scheduling_session,
					sender,
				)
				.await
				.map_err(|_| {
					"Cannot fetch validation code bomb limit from the runtime".to_string()
				})?;
				let _ = self.bomb_limit.insert(scheduling_session, limit);
				limit
			},
		};

		Ok(SessionParams { executor_params, validation_code_bomb_limit })
	}
}

/// Output of [`pre_validate_candidate`]: data needed by PVF execution and
/// post-validation.
struct PreValidationOutput {
	/// Validation code bomb limit for PVF preparation.
	validation_code_bomb_limit: u32,
	/// Claim queue for backing-only UMP signal post-validation. `None` for
	/// approval/dispute.
	claim_queue: Option<ClaimQueueSnapshot>,
}

/// Errors from [`pre_validate_candidate`].
enum PreValidationError {
	/// The candidate is definitively invalid.
	Invalid(InvalidCandidate),
	/// A runtime API call failed — cannot determine validity.
	RuntimeError(String),
}

/// Pre-validate a candidate before PVF execution.
///
/// Performs all checks that don't require running the PVF:
/// - Persisted validation data matches the descriptor
/// - Basic checks: PoV hash, code hash, PoV size
/// - Backing-only: scheduling session matches runtime, relay parent valid in claimed session, claim
///   queue fetch
///
/// Backing-only checks are skipped for approval/dispute because the runtime
/// validates them at backing time and the chain state they depend on may not
/// be available in disputes.
async fn pre_validate_candidate<Sender>(
	sender: &mut Sender,
	candidate_receipt: &CandidateReceipt,
	validation_data: &PersistedValidationData,
	pov: &PoV,
	validation_code_hash: &ValidationCodeHash,
	validation_code_bomb_limit: u32,
	exec_kind: PvfExecKind,
	v3_ever_seen: bool,
) -> Result<PreValidationOutput, PreValidationError>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	// Ensure that the validation data matches the announced one.
	if validation_data.hash() != candidate_receipt.descriptor.persisted_validation_data_hash() {
		return Err(PreValidationError::Invalid(InvalidCandidate::PersistedValidationDataMismatch));
	}

	if let Err(e) = perform_basic_checks(
		&candidate_receipt.descriptor,
		validation_data.max_pov_size,
		pov,
		validation_code_hash,
	) {
		return Err(PreValidationError::Invalid(e));
	}

	let claim_queue = match exec_kind {
		PvfExecKind::Backing(_) | PvfExecKind::BackingSystemParas(_) => {
			let scheduling_parent = candidate_receipt
				.descriptor
				.scheduling_parent_for_candidate_validation(v3_ever_seen);

			// Verify scheduling session.
			let expected_scheduling_session =
				get_session_index(sender, scheduling_parent).await.ok_or_else(|| {
					PreValidationError::RuntimeError(
						"Scheduling session index not found".to_string(),
					)
				})?;

			if let Some(scheduling_session) = candidate_receipt
				.descriptor
				.scheduling_session_for_candidate_validation(v3_ever_seen)
			{
				if scheduling_session != expected_scheduling_session {
					return Err(PreValidationError::Invalid(
						InvalidCandidate::InvalidSchedulingSession,
					));
				}
			}

			// Verify relay parent is valid in the claimed session.
			// Uses the node-side utility which handles both the self-query case
			// (scheduling_parent == relay_parent, V2) and ancestor queries (V3).
			if let Some(session_index) = candidate_receipt
				.descriptor
				.session_index_for_candidate_validation(v3_ever_seen)
			{
				let relay_parent = candidate_receipt.descriptor.relay_parent();
				match util::check_relay_parent_session(
					sender,
					scheduling_parent,
					session_index,
					relay_parent,
				)
				.await
				{
					util::CheckRelayParentSessionResult::Valid => {},
					// Safe to skip: on old runtimes cross-session relay parents don't
					// exist, and the scheduling session check above already covers the
					// relay parent session (scheduling_parent == relay_parent).
					util::CheckRelayParentSessionResult::NotSupported => {},
					util::CheckRelayParentSessionResult::NotFound => {
						return Err(PreValidationError::Invalid(
							InvalidCandidate::InvalidRelayParentSession,
						))
					},
					util::CheckRelayParentSessionResult::RuntimeError(err) => {
						return Err(PreValidationError::RuntimeError(err))
					},
				}
			}

			let cq = claim_queue(scheduling_parent, sender).await.ok_or_else(|| {
				PreValidationError::RuntimeError("Claim queue not available".to_string())
			})?;

			Some(cq)
		},
		_ => None,
	};

	Ok(PreValidationOutput { validation_code_bomb_limit, claim_queue })
}

fn handle_validation_message<S, V>(
	mut sender: S,
	validation_host: V,
	metrics: Metrics,
	v3_ever_seen: bool,
	msg: CandidateValidationMessage,
	session_params: Option<SessionParams>,
) -> Pin<Box<dyn Future<Output = ()> + Send>>
where
	S: SubsystemSender<RuntimeApiMessage>,
	V: ValidationBackend + Clone + Send + 'static,
{
	match msg {
		CandidateValidationMessage::ValidateFromExhaustive {
			validation_data,
			validation_code,
			candidate_receipt,
			pov,
			exec_kind,
			response_sender,
			..
		} => async move {
			let _timer = metrics.time_validate_from_exhaustive();

			// Session params were resolved by the run loop (cached, fetched at a
			// recent leaf). If resolution failed (e.g. no active leaf yet), the
			// task cannot proceed.
			let session_params = match session_params {
				Some(params) => params,
				None => {
					let _ = response_sender.send(Err(ValidationFailed(
						"Session params unavailable (no active leaf?)".to_string(),
					)));
					return;
				},
			};

			// Phase 1: Pre-validation — basic checks + backing-specific checks.
			let pre = match pre_validate_candidate(
				&mut sender,
				&candidate_receipt,
				&validation_data,
				&pov,
				&validation_code.hash(),
				session_params.validation_code_bomb_limit,
				exec_kind,
				v3_ever_seen,
			)
			.await
			{
				Ok(pre) => pre,
				Err(PreValidationError::Invalid(e)) => {
					let _ = response_sender.send(Ok(ValidationResult::Invalid(e)));
					return;
				},
				Err(PreValidationError::RuntimeError(err)) => {
					let _ = response_sender.send(Err(ValidationFailed(err)));
					return;
				},
			};

			// Phase 2: PVF execution + output validation.
			let res = validate_candidate(
				validation_host,
				validation_data,
				validation_code,
				candidate_receipt,
				pov,
				session_params.executor_params,
				exec_kind,
				&metrics,
				v3_ever_seen,
				pre,
			)
			.await;

			metrics.on_validation_event(&res);
			let _ = response_sender.send(res);
		}
		.boxed(),
		CandidateValidationMessage::PreCheck {
			relay_parent,
			validation_code_hash,
			response_sender,
			..
		} => async move {
			let Some(session_index) = get_session_index(&mut sender, relay_parent).await else {
				let error = "cannot fetch session index from the runtime";
				gum::warn!(
					target: LOG_TARGET,
					?relay_parent,
					error,
				);

				let _ = response_sender.send(PreCheckOutcome::Failed);
				return;
			};

			// This will return a default value for the limit if runtime API is not available.
			// however we still error out if there is a weird runtime API error.
			let Ok(validation_code_bomb_limit) = util::runtime::fetch_validation_code_bomb_limit(
				relay_parent,
				session_index,
				&mut sender,
			)
			.await
			else {
				let error = "cannot fetch validation code bomb limit from the runtime";
				gum::warn!(
					target: LOG_TARGET,
					?relay_parent,
					error,
				);

				let _ = response_sender.send(PreCheckOutcome::Failed);
				return;
			};

			let precheck_result = precheck_pvf(
				&mut sender,
				validation_host,
				relay_parent,
				validation_code_hash,
				validation_code_bomb_limit,
			)
			.await;

			let _ = response_sender.send(precheck_result);
		}
		.boxed(),
	}
}

#[overseer::contextbounds(CandidateValidation, prefix = self::overseer)]
async fn run<Context>(
	mut ctx: Context,
	keystore: KeystorePtr,
	metrics: Metrics,
	pvf_metrics: polkadot_node_core_pvf::Metrics,
	Config {
		artifacts_cache_path,
		node_version,
		secure_validator_mode,
		prep_worker_path,
		exec_worker_path,
		pvf_execute_workers_max_num,
		pvf_prepare_workers_soft_max_num,
		pvf_prepare_workers_hard_max_num,
	}: Config,
) -> SubsystemResult<()> {
	let (mut validation_host, task) = polkadot_node_core_pvf::start(
		polkadot_node_core_pvf::Config::new(
			artifacts_cache_path,
			node_version,
			secure_validator_mode,
			prep_worker_path,
			exec_worker_path,
			pvf_execute_workers_max_num,
			pvf_prepare_workers_soft_max_num,
			pvf_prepare_workers_hard_max_num,
		),
		pvf_metrics,
	)
	.await?;
	ctx.spawn_blocking("pvf-validation-host", task.boxed())?;

	let mut tasks = FuturesUnordered::new();
	let mut state = State::default();

	loop {
		loop {
			futures::select! {
				comm = ctx.recv().fuse() => {
					match comm {
						Ok(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(update))) => {
							handle_active_leaves_update(
								ctx.sender(),
								keystore.clone(),
								&mut validation_host,
								update,
								&mut state,
							).await
						},
						Ok(FromOrchestra::Signal(OverseerSignal::BlockFinalized(..))) => {},
						Ok(FromOrchestra::Signal(OverseerSignal::Conclude)) => return Ok(()),
						Ok(FromOrchestra::Communication { msg }) => {
								let session_params = match &msg {
								CandidateValidationMessage::ValidateFromExhaustive {
									scheduling_session_index,
									candidate_receipt,
									..
								} => {
									if let Some(recent_leaf) = state.last_active_leaf {
										match state.session_cache.fetch_params(
											recent_leaf,
											*scheduling_session_index,
											&candidate_receipt.descriptor,
											state.v3_ever_seen,
											ctx.sender(),
										)
										.await
										{
											Ok(params) => Some(params),
											Err(err) => {
												gum::warn!(
													target: LOG_TARGET,
													?err,
													"Failed to fetch session params",
												);
												None
											},
										}
									} else {
										None
									}
								},
								_ => None,
							};
							let task = handle_validation_message(
								ctx.sender().clone(),
								validation_host.clone(),
								metrics.clone(),
								state.v3_ever_seen,
								msg,
								session_params,
							);
							tasks.push(task);
							if tasks.len() >= TASK_LIMIT {
								break
							}
						},
						Err(e) => return Err(SubsystemError::from(e)),
					}
				},
				_ = tasks.select_next_some() => ()
			}
		}

		gum::debug!(target: LOG_TARGET, "Validation task limit hit");

		loop {
			futures::select! {
				signal = ctx.recv_signal().fuse() => {
					match signal {
						Ok(OverseerSignal::ActiveLeaves(_)) => {},
						Ok(OverseerSignal::BlockFinalized(..)) => {},
						Ok(OverseerSignal::Conclude) => return Ok(()),
						Err(e) => return Err(SubsystemError::from(e)),
					}
				},
				_ = tasks.select_next_some() => {
					if tasks.len() < TASK_LIMIT {
						break
					}
				}
			}
		}
	}
}

/// Top-level subsystem state, owning session tracking, V3 transition detection,
/// and PVF preparation bookkeeping.
struct State {
	/// Current session index, tracked across active leaf updates.
	session_index: Option<SessionIndex>,
	/// Most recent active leaf.
	last_active_leaf: Option<Hash>,
	/// Monotonic flag: set to `true` once any activated leaf has the V3 candidate
	/// descriptor node feature enabled. Once set, never unset.
	/// Used to determine whether approval/dispute validation should trust
	/// `version()` (V3-capable) or fall back to `version_old_rules()`.
	/// See `CandidateDescriptorV2::version_for_candidate_validation` for the safety argument.
	v3_ever_seen: bool,
	/// Per-session cache for session-scoped validation parameters.
	session_cache: SessionCache,
	/// PVF preparation state (proactive pre-compilation for next session).
	pvf_prep: PvfPrepState,
}

impl Default for State {
	fn default() -> Self {
		Self {
			session_index: None,
			last_active_leaf: None,
			v3_ever_seen: false,
			session_cache: SessionCache::new(),
			pvf_prep: PvfPrepState::default(),
		}
	}
}

/// State for proactive PVF preparation.
///
/// Tracks whether we're a next-session authority and which code hashes we've already
/// sent to the PVF host.
struct PvfPrepState {
	is_next_session_authority: bool,
	// PVF host won't prepare the same code hash twice, so here we just avoid extra communication
	already_prepared_code_hashes: HashSet<ValidationCodeHash>,
	// How many PVFs per block we take to prepare themselves for the next session validation
	per_block_limit: usize,
}

impl Default for PvfPrepState {
	fn default() -> Self {
		Self {
			is_next_session_authority: false,
			already_prepared_code_hashes: HashSet::new(),
			per_block_limit: 1,
		}
	}
}

/// Check if the V3 candidate descriptor node feature is enabled at the given
/// session. Returns `true` if the feature is set.
async fn check_v3_feature<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
	session_index: SessionIndex,
) -> bool
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	if let Ok(Ok(features)) = request_node_features(relay_parent, session_index, sender).await.await
	{
		if FeatureIndex::CandidateReceiptV3.is_set(&features) {
			gum::info!(
				target: LOG_TARGET,
				?session_index,
				"CandidateReceiptV3 node feature detected, \
				 switching to V3-aware approval/dispute validation",
			);
			return true;
		}
	}
	false
}

async fn handle_active_leaves_update<Sender>(
	sender: &mut Sender,
	keystore: KeystorePtr,
	validation_host: &mut impl ValidationBackend,
	update: ActiveLeavesUpdate,
	state: &mut State,
) where
	Sender: SubsystemSender<ChainApiMessage> + SubsystemSender<RuntimeApiMessage>,
{
	update_active_leaves_validation_backend(sender, validation_host, update.clone()).await;

	let Some(activated) = update.activated else { return };
	state.last_active_leaf = Some(activated.hash);
	let maybe_session_index = get_session_index(sender, activated.hash).await;

	// Detect session change
	let new_session = match (state.session_index, maybe_session_index) {
		(Some(old), Some(new)) => (new > old).then_some(new),
		(None, Some(new)) => Some(new),
		_ => None,
	};

	state.session_index = new_session.or(state.session_index);

	// V3 feature detection on session change
	if !state.v3_ever_seen {
		if let Some(session_index) = new_session {
			state.v3_ever_seen = check_v3_feature(sender, activated.hash, session_index).await;
		}
	}

	// Proactive PVF preparation
	maybe_prepare_validation(
		sender,
		keystore.clone(),
		validation_host,
		activated,
		&mut state.pvf_prep,
		new_session,
	)
	.await;
}

async fn maybe_prepare_validation<Sender>(
	sender: &mut Sender,
	keystore: KeystorePtr,
	validation_backend: &mut impl ValidationBackend,
	leaf: ActivatedLeaf,
	pvf_prep: &mut PvfPrepState,
	new_session: Option<SessionIndex>,
) where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	if let Some(new_session_index) = new_session {
		pvf_prep.already_prepared_code_hashes.clear();
		pvf_prep.is_next_session_authority =
			check_next_session_authority(sender, keystore, leaf.hash, new_session_index).await;
	}

	// On every active leaf check candidates and prepare PVFs our node doesn't have yet.
	if pvf_prep.is_next_session_authority {
		let code_hashes = prepare_pvfs_for_backed_candidates(
			sender,
			validation_backend,
			leaf.hash,
			&pvf_prep.already_prepared_code_hashes,
			pvf_prep.per_block_limit,
		)
		.await;
		pvf_prep.already_prepared_code_hashes.extend(code_hashes.unwrap_or_default());
	}
}

async fn get_session_index<Sender>(sender: &mut Sender, relay_parent: Hash) -> Option<SessionIndex>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	let Ok(Ok(session_index)) =
		util::request_session_index_for_child(relay_parent, sender).await.await
	else {
		gum::warn!(
			target: LOG_TARGET,
			?relay_parent,
			"cannot fetch session index from runtime API",
		);
		return None;
	};

	Some(session_index)
}

// Returns true if the node is an authority in the next session.
async fn check_next_session_authority<Sender>(
	sender: &mut Sender,
	keystore: KeystorePtr,
	relay_parent: Hash,
	session_index: SessionIndex,
) -> bool
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	// In spite of function name here we request past, present and future authorities.
	// It's ok to stil prepare PVFs in other cases, but better to request only future ones.
	let Ok(Ok(authorities)) = util::request_authorities(relay_parent, sender).await.await else {
		gum::warn!(
			target: LOG_TARGET,
			?relay_parent,
			"cannot fetch authorities from runtime API",
		);
		return false;
	};

	// We need to exclude at least current session authority from the previous request
	let Ok(Ok(Some(session_info))) =
		util::request_session_info(relay_parent, session_index, sender).await.await
	else {
		gum::warn!(
			target: LOG_TARGET,
			?relay_parent,
			"cannot fetch session info from runtime API",
		);
		return false;
	};

	let is_past_present_or_future_authority = authorities
		.iter()
		.any(|v| keystore.has_keys(&[(v.to_raw_vec(), AuthorityDiscoveryId::ID)]));

	// We could've checked discovery_keys but on Kusama validators.len() < discovery_keys.len().
	let is_present_validator = session_info
		.validators
		.iter()
		.any(|v| keystore.has_keys(&[(v.to_raw_vec(), ValidatorId::ID)]));

	// There is still a chance to be a previous session authority, but this extra work does not
	// affect the finalization.
	is_past_present_or_future_authority && !is_present_validator
}

// Sends PVF with unknown code hashes to the validation host returning the list of code hashes sent.
async fn prepare_pvfs_for_backed_candidates<Sender>(
	sender: &mut Sender,
	validation_backend: &mut impl ValidationBackend,
	relay_parent: Hash,
	already_prepared: &HashSet<ValidationCodeHash>,
	per_block_limit: usize,
) -> Option<Vec<ValidationCodeHash>>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	let Ok(Ok(events)) = util::request_candidate_events(relay_parent, sender).await.await else {
		gum::warn!(
			target: LOG_TARGET,
			?relay_parent,
			"cannot fetch candidate events from runtime API",
		);
		return None;
	};
	let code_hashes = events
		.into_iter()
		.filter_map(|e| match e {
			CandidateEvent::CandidateBacked(receipt, ..) => {
				let h = receipt.descriptor.validation_code_hash();
				if already_prepared.contains(&h) {
					None
				} else {
					Some(h)
				}
			},
			_ => None,
		})
		.take(per_block_limit)
		.collect::<Vec<_>>();

	let Ok(executor_params) = util::executor_params_at_relay_parent(relay_parent, sender).await
	else {
		gum::warn!(
			target: LOG_TARGET,
			?relay_parent,
			"cannot fetch executor params for the session",
		);
		return None;
	};
	let timeout = pvf_prep_timeout(&executor_params, PvfPrepKind::Prepare);

	let mut active_pvfs = vec![];
	let mut processed_code_hashes = vec![];
	for code_hash in code_hashes {
		let Ok(Ok(Some(validation_code))) =
			util::request_validation_code_by_hash(relay_parent, code_hash, sender)
				.await
				.await
		else {
			gum::warn!(
				target: LOG_TARGET,
				?relay_parent,
				?code_hash,
				"cannot fetch validation code hash from runtime API",
			);
			continue;
		};

		let Some(session_index) = get_session_index(sender, relay_parent).await else { continue };

		let validation_code_bomb_limit = match util::runtime::fetch_validation_code_bomb_limit(
			relay_parent,
			session_index,
			sender,
		)
		.await
		{
			Ok(limit) => limit,
			Err(err) => {
				gum::warn!(
					target: LOG_TARGET,
					?relay_parent,
					?err,
					"cannot fetch validation code bomb limit from runtime API",
				);
				continue;
			},
		};

		let pvf = PvfPrepData::from_code(
			validation_code.0,
			executor_params.clone(),
			timeout,
			PrepareJobKind::Prechecking,
			validation_code_bomb_limit,
		);

		active_pvfs.push(pvf);
		processed_code_hashes.push(code_hash);
	}

	if active_pvfs.is_empty() {
		return None;
	}

	if let Err(err) = validation_backend.heads_up(active_pvfs).await {
		gum::warn!(
			target: LOG_TARGET,
			?relay_parent,
			?err,
			"cannot prepare PVF for the next session",
		);
		return None;
	};

	gum::debug!(
		target: LOG_TARGET,
		?relay_parent,
		?processed_code_hashes,
		"Prepared PVF for the next session",
	);

	Some(processed_code_hashes)
}

async fn update_active_leaves_validation_backend<Sender>(
	sender: &mut Sender,
	validation_backend: &mut impl ValidationBackend,
	update: ActiveLeavesUpdate,
) where
	Sender: SubsystemSender<ChainApiMessage> + SubsystemSender<RuntimeApiMessage>,
{
	let ancestors = if let Some(ref activated) = update.activated {
		get_block_ancestors(sender, activated.hash).await
	} else {
		vec![]
	};
	if let Err(err) = validation_backend.update_active_leaves(update, ancestors).await {
		gum::warn!(
			target: LOG_TARGET,
			?err,
			"cannot update active leaves in validation backend",
		);
	};
}

/// Get list of still valid scheduling parents for the given leaf.
///
/// TODO: This function does not take into account session boundaries, which leads to wasted effort:
/// https://github.com/paritytech/polkadot-sdk/issues/11301
async fn get_block_ancestors<Sender>(sender: &mut Sender, leaf: Hash) -> Vec<Hash>
where
	Sender: SubsystemSender<ChainApiMessage> + SubsystemSender<RuntimeApiMessage>,
{
	let Some(session_index) = get_session_index(sender, leaf).await else {
		gum::warn!(target: LOG_TARGET, ?leaf, "Failed to request session index for leaf.");
		return vec![];
	};
	let scheduling_lookahead = match fetch_scheduling_lookahead(leaf, session_index, sender).await {
		Ok(scheduling_lookahead) => scheduling_lookahead,
		res => {
			gum::warn!(target: LOG_TARGET, ?res, "Failed to request scheduling lookahead");
			return vec![];
		},
	};

	let (tx, rx) = oneshot::channel();
	sender
		.send_message(ChainApiMessage::Ancestors {
			hash: leaf,
			// Subtract 1 from the claim queue length, as it includes current `scheduling_parent`.
			k: scheduling_lookahead.saturating_sub(1) as usize,
			response_channel: tx,
		})
		.await;
	match rx.await {
		Ok(Ok(x)) => x,
		res => {
			gum::warn!(target: LOG_TARGET, ?res, "cannot request ancestors");
			vec![]
		},
	}
}

struct RuntimeRequestFailed;

async fn runtime_api_request<T, Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
	request: RuntimeApiRequest,
	receiver: oneshot::Receiver<Result<T, RuntimeApiError>>,
) -> Result<T, RuntimeRequestFailed>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	sender
		.send_message(RuntimeApiMessage::Request(relay_parent, request).into())
		.await;

	receiver
		.await
		.map_err(|_| {
			gum::debug!(target: LOG_TARGET, ?relay_parent, "Runtime API request dropped");

			RuntimeRequestFailed
		})
		.and_then(|res| {
			res.map_err(|e| {
				gum::debug!(
					target: LOG_TARGET,
					?relay_parent,
					err = ?e,
					"Runtime API request internal error"
				);

				RuntimeRequestFailed
			})
		})
}

async fn request_validation_code_by_hash<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
	validation_code_hash: ValidationCodeHash,
) -> Result<Option<ValidationCode>, RuntimeRequestFailed>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	let (tx, rx) = oneshot::channel();
	runtime_api_request(
		sender,
		relay_parent,
		RuntimeApiRequest::ValidationCodeByHash(validation_code_hash, tx),
		rx,
	)
	.await
}

async fn precheck_pvf<Sender>(
	sender: &mut Sender,
	mut validation_backend: impl ValidationBackend,
	relay_parent: Hash,
	validation_code_hash: ValidationCodeHash,
	validation_code_bomb_limit: u32,
) -> PreCheckOutcome
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	let validation_code =
		match request_validation_code_by_hash(sender, relay_parent, validation_code_hash).await {
			Ok(Some(code)) => code,
			_ => {
				// The reasoning why this is "failed" and not invalid is because we assume that
				// during pre-checking voting the relay-chain will pin the code. In case the code
				// actually is not there, we issue failed since this looks more like a bug.
				gum::warn!(
					target: LOG_TARGET,
					?relay_parent,
					?validation_code_hash,
					"precheck: requested validation code is not found on-chain!",
				);
				return PreCheckOutcome::Failed;
			},
		};

	let executor_params = if let Ok(executor_params) =
		util::executor_params_at_relay_parent(relay_parent, sender).await
	{
		gum::debug!(
			target: LOG_TARGET,
			?relay_parent,
			?validation_code_hash,
			"precheck: acquired executor params for the session: {:?}",
			executor_params,
		);
		executor_params
	} else {
		gum::warn!(
			target: LOG_TARGET,
			?relay_parent,
			?validation_code_hash,
			"precheck: failed to acquire executor params for the session, thus voting against.",
		);
		return PreCheckOutcome::Invalid;
	};

	let timeout = pvf_prep_timeout(&executor_params, PvfPrepKind::Precheck);

	let pvf = PvfPrepData::from_code(
		validation_code.0,
		executor_params,
		timeout,
		PrepareJobKind::Prechecking,
		validation_code_bomb_limit,
	);

	match validation_backend.precheck_pvf(pvf).await {
		Ok(_) => PreCheckOutcome::Valid,
		Err(prepare_err) => {
			if prepare_err.is_deterministic() {
				PreCheckOutcome::Invalid
			} else {
				PreCheckOutcome::Failed
			}
		},
	}
}

/// Execute a PVF and validate the candidate's output.
///
/// Assumes all pre-validation ([`pre_validate_candidate`]) has already passed.
/// Handles:
/// 1. PVF execution (backing: single attempt; approval/dispute: with retry)
/// 2. Post-validation: para_head hash, commitments hash
/// 3. Backing-only post-validation: UMP signal validation against claim queue
async fn validate_candidate(
	mut validation_backend: impl ValidationBackend + Send,
	persisted_validation_data: PersistedValidationData,
	validation_code: ValidationCode,
	candidate_receipt: CandidateReceipt,
	pov: Arc<PoV>,
	executor_params: ExecutorParams,
	exec_kind: PvfExecKind,
	metrics: &Metrics,
	v3_seen: bool,
	pre: PreValidationOutput,
) -> Result<ValidationResult, ValidationFailed> {
	let _timer = metrics.time_validate_candidate_exhaustive();
	let para_id = candidate_receipt.descriptor.para_id();
	let candidate_hash = candidate_receipt.hash();

	gum::debug!(
		target: LOG_TARGET,
		?candidate_hash,
		?para_id,
		"About to validate a candidate.",
	);

	let persisted_validation_data = Arc::new(persisted_validation_data);

	// Create the validation context shared by both backing and approval/dispute paths
	let validation_context = ValidationContext {
		candidate_receipt: candidate_receipt.clone(),
		pvd: persisted_validation_data.clone(),
		pov: pov.clone(),
		executor_params: executor_params.clone(),
		exec_timeout: pvf_exec_timeout(&executor_params, exec_kind.into()),
		v3_seen,
	};

	let result = match exec_kind {
		// Retry is disabled to reduce the chance of nondeterministic blocks getting backed and
		// honest backers getting slashed.
		PvfExecKind::Backing(_) | PvfExecKind::BackingSystemParas(_) => {
			let prep_timeout = pvf_prep_timeout(&executor_params, PvfPrepKind::Prepare);
			let pvf = PvfPrepData::from_code(
				validation_code.0,
				executor_params,
				prep_timeout,
				PrepareJobKind::Compilation,
				pre.validation_code_bomb_limit,
			);

			validation_backend.validate_candidate(pvf, validation_context, exec_kind).await
		},
		PvfExecKind::Approval | PvfExecKind::Dispute => {
			validation_backend
				.validate_candidate_with_retry(
					validation_code.0,
					validation_context,
					PVF_APPROVAL_EXECUTION_RETRY_DELAY,
					exec_kind,
					pre.validation_code_bomb_limit,
				)
				.await
		},
	};

	if let Err(ref error) = result {
		gum::info!(target: LOG_TARGET, ?para_id, ?candidate_hash, ?error, "Failed to validate candidate");
	}

	match result {
		Err(ValidationError::Internal(e)) => {
			gum::warn!(
				target: LOG_TARGET,
				?para_id,
				?candidate_hash,
				?e,
				"An internal error occurred during validation, will abstain from voting",
			);
			Err(ValidationFailed(e.to_string()))
		},
		Err(ValidationError::Invalid(WasmInvalidCandidate::HardTimeout)) => {
			Ok(ValidationResult::Invalid(InvalidCandidate::Timeout))
		},
		Err(ValidationError::Invalid(WasmInvalidCandidate::WorkerReportedInvalid(e))) => {
			Ok(ValidationResult::Invalid(InvalidCandidate::ExecutionError(e)))
		},
		Err(ValidationError::Invalid(WasmInvalidCandidate::PoVDecompressionFailure)) => {
			Ok(ValidationResult::Invalid(InvalidCandidate::PoVDecompressionFailure))
		},
		Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath)) => {
			Ok(ValidationResult::Invalid(InvalidCandidate::ExecutionError(
				"ambiguous worker death".to_string(),
			)))
		},
		Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::JobError(err))) => {
			Ok(ValidationResult::Invalid(InvalidCandidate::ExecutionError(err)))
		},
		Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::RuntimeConstruction(err))) => {
			Ok(ValidationResult::Invalid(InvalidCandidate::ExecutionError(err)))
		},
		Err(ValidationError::PossiblyInvalid(err @ PossiblyInvalidError::CorruptedArtifact)) => {
			Ok(ValidationResult::Invalid(InvalidCandidate::ExecutionError(err.to_string())))
		},

		Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousJobDeath(err))) => {
			Ok(ValidationResult::Invalid(InvalidCandidate::ExecutionError(format!(
				"ambiguous job death: {err}"
			))))
		},
		Err(ValidationError::Preparation(e)) => {
			gum::warn!(
				target: LOG_TARGET,
				?para_id,
				?e,
				"Deterministic error occurred during preparation (should have been ruled out by pre-checking phase)",
			);
			Err(ValidationFailed(e.to_string()))
		},
		Err(e @ ValidationError::ExecutionDeadline) => {
			gum::warn!(
				target: LOG_TARGET,
				?para_id,
				?e,
				"Job assigned too late, execution queue probably overloaded",
			);
			Err(ValidationFailed(e.to_string()))
		},
		Ok(res) => {
			if res.head_data.hash() != candidate_receipt.descriptor.para_head() {
				gum::info!(target: LOG_TARGET, ?para_id, "Invalid candidate (para_head)");
				Ok(ValidationResult::Invalid(InvalidCandidate::ParaHeadHashMismatch))
			} else {
				let committed_candidate_receipt = CommittedCandidateReceipt {
					descriptor: candidate_receipt.descriptor.clone(),
					commitments: CandidateCommitments {
						head_data: res.head_data,
						upward_messages: res.upward_messages,
						horizontal_messages: res.horizontal_messages,
						new_validation_code: res.new_validation_code,
						processed_downward_messages: res.processed_downward_messages,
						hrmp_watermark: res.hrmp_watermark,
					},
				};

				if candidate_receipt.commitments_hash !=
					committed_candidate_receipt.commitments.hash()
				{
					gum::info!(
						target: LOG_TARGET,
						?para_id,
						?candidate_hash,
						"Invalid candidate (commitments hash)"
					);

					gum::trace!(
						target: LOG_TARGET,
						?para_id,
						?candidate_hash,
						produced_commitments = ?committed_candidate_receipt.commitments,
						"Invalid candidate commitments"
					);

					// If validation produced a new set of commitments, we treat the candidate as
					// invalid.
					Ok(ValidationResult::Invalid(InvalidCandidate::CommitmentsHashMismatch))
				} else {
					// Backing-only: validate UMP signals against the claim queue.
					if let Some(claim_queue) = &pre.claim_queue {
						if let Err(err) = committed_candidate_receipt
							.parse_ump_signals(&transpose_claim_queue(claim_queue.0.clone()))
						{
							gum::warn!(
								target: LOG_TARGET,
								candidate_hash = ?candidate_receipt.hash(),
								"Invalid UMP signals: {}",
								err
							);
							return Ok(ValidationResult::Invalid(
								InvalidCandidate::InvalidUMPSignals(err),
							));
						}
					}

					Ok(ValidationResult::Valid(
						committed_candidate_receipt.commitments,
						(*persisted_validation_data).clone(),
					))
				}
			}
		},
	}
}

#[async_trait]
trait ValidationBackend {
	/// Tries executing a PVF a single time (no retries).
	async fn validate_candidate(
		&mut self,
		pvf: PvfPrepData,
		validation_context: ValidationContext,
		exec_kind: PvfExecKind,
	) -> Result<WasmValidationResult, ValidationError>;

	/// Tries executing a PVF. Will retry once if an error is encountered that may have
	/// been transient.
	///
	/// NOTE: Should retry only on errors that are a result of execution itself, and not of
	/// preparation.
	async fn validate_candidate_with_retry(
		&mut self,
		code: Vec<u8>,
		validation_context: ValidationContext,
		retry_delay: Duration,
		exec_kind: PvfExecKind,
		validation_code_bomb_limit: u32,
	) -> Result<WasmValidationResult, ValidationError> {
		let exec_timeout = validation_context.exec_timeout;
		let executor_params = validation_context.executor_params.clone();
		let prep_timeout = pvf_prep_timeout(&executor_params, PvfPrepKind::Prepare);
		// Construct the PVF a single time, since it is an expensive operation. Cloning it is cheap.
		let pvf = PvfPrepData::from_code(
			code,
			executor_params,
			prep_timeout,
			PrepareJobKind::Compilation,
			validation_code_bomb_limit,
		);
		// We keep track of the total time that has passed and stop retrying if we are taking too
		// long.
		let total_time_start = Instant::now();

		let mut validation_result = self
			.validate_candidate(pvf.clone(), validation_context.clone(), exec_kind)
			.await;
		if validation_result.is_ok() {
			return validation_result;
		}

		macro_rules! break_if_no_retries_left {
			($counter:ident) => {
				if $counter > 0 {
					$counter -= 1;
				} else {
					break;
				}
			};
		}

		// Allow limited retries for each kind of error.
		let mut num_death_retries_left = 1;
		let mut num_job_error_retries_left = 1;
		let mut num_internal_retries_left = 1;
		let mut num_execution_error_retries_left = 1;
		loop {
			// Stop retrying if we exceeded the timeout.
			if total_time_start.elapsed() + retry_delay > exec_timeout {
				break;
			}
			let mut retry_immediately = false;
			match validation_result {
				Err(ValidationError::PossiblyInvalid(
					PossiblyInvalidError::AmbiguousWorkerDeath |
					PossiblyInvalidError::AmbiguousJobDeath(_),
				)) => break_if_no_retries_left!(num_death_retries_left),

				Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::JobError(_))) => {
					break_if_no_retries_left!(num_job_error_retries_left)
				},

				Err(ValidationError::Internal(_)) => {
					break_if_no_retries_left!(num_internal_retries_left)
				},

				Err(ValidationError::PossiblyInvalid(
					PossiblyInvalidError::RuntimeConstruction(_) |
					PossiblyInvalidError::CorruptedArtifact,
				)) => {
					break_if_no_retries_left!(num_execution_error_retries_left);
					self.precheck_pvf(pvf.clone()).await?;
					// In this case the error is deterministic
					// And a retry forces the ValidationBackend
					// to re-prepare the artifact so
					// there is no need to wait before the retry
					retry_immediately = true;
				},

				Ok(_) |
				Err(
					ValidationError::Invalid(_) |
					ValidationError::Preparation(_) |
					ValidationError::ExecutionDeadline,
				) => break,
			}

			// If we got a possibly transient error, retry once after a brief delay, on the
			// assumption that the conditions that caused this error may have resolved on their own.
			{
				// In case of many transient errors it is necessary to wait a little bit
				// for the error to be probably resolved
				if !retry_immediately {
					futures_timer::Delay::new(retry_delay).await;
				}

				let new_timeout = exec_timeout.saturating_sub(total_time_start.elapsed());

				gum::warn!(
					target: LOG_TARGET,
					?pvf,
					?new_timeout,
					"Re-trying failed candidate validation due to possible transient error: {:?}",
					validation_result
				);

				// Update the validation context with the new timeout
				let mut retry_context = validation_context.clone();
				retry_context.exec_timeout = new_timeout;

				validation_result =
					self.validate_candidate(pvf.clone(), retry_context, exec_kind).await;
			}
		}

		validation_result
	}

	async fn precheck_pvf(&mut self, pvf: PvfPrepData) -> Result<(), PrepareError>;

	async fn heads_up(&mut self, active_pvfs: Vec<PvfPrepData>) -> Result<(), String>;

	/// Inform the backend about active leaf changes
	///
	/// Ancestors provided should match the still valid scheduling parents (implicit view) as of the
	/// activated leaf. This is used for pruning queued jobs which became obsolete.
	async fn update_active_leaves(
		&mut self,
		update: ActiveLeavesUpdate,
		ancestors: Vec<Hash>,
	) -> Result<(), String>;
}

#[async_trait]
impl ValidationBackend for ValidationHost {
	/// Tries executing a PVF a single time (no retries).
	async fn validate_candidate(
		&mut self,
		pvf: PvfPrepData,
		validation_context: ValidationContext,
		exec_kind: PvfExecKind,
	) -> Result<WasmValidationResult, ValidationError> {
		let (tx, rx) = oneshot::channel();
		if let Err(err) =
			self.execute_pvf(pvf, validation_context, exec_kind.into(), exec_kind, tx).await
		{
			return Err(InternalValidationError::HostCommunication(format!(
				"cannot send pvf to the validation host, it might have shut down: {:?}",
				err
			))
			.into());
		}

		rx.await.map_err(|_| {
			ValidationError::from(InternalValidationError::HostCommunication(
				"validation was cancelled".into(),
			))
		})?
	}

	async fn precheck_pvf(&mut self, pvf: PvfPrepData) -> Result<(), PrepareError> {
		let (tx, rx) = oneshot::channel();
		if let Err(err) = self.precheck_pvf(pvf, tx).await {
			// Return an IO error if there was an error communicating with the host.
			return Err(PrepareError::IoErr(err));
		}

		let precheck_result = rx.await.map_err(|err| PrepareError::IoErr(err.to_string()))?;

		precheck_result
	}

	async fn heads_up(&mut self, active_pvfs: Vec<PvfPrepData>) -> Result<(), String> {
		self.heads_up(active_pvfs).await
	}

	async fn update_active_leaves(
		&mut self,
		update: ActiveLeavesUpdate,
		ancestors: Vec<Hash>,
	) -> Result<(), String> {
		self.update_active_leaves(update, ancestors).await
	}
}

/// Does basic checks of a candidate. Provide the encoded PoV-block. Returns `Ok` if basic checks
/// are passed, `Err` otherwise.
fn perform_basic_checks(
	candidate: &CandidateDescriptor,
	max_pov_size: u32,
	pov: &PoV,
	validation_code_hash: &ValidationCodeHash,
) -> Result<(), InvalidCandidate> {
	let pov_hash = pov.hash();

	let encoded_pov_size = pov.encoded_size();
	if encoded_pov_size > max_pov_size as usize {
		return Err(InvalidCandidate::ParamsTooLarge(encoded_pov_size as u64));
	}

	if pov_hash != candidate.pov_hash() {
		return Err(InvalidCandidate::PoVHashMismatch);
	}

	if *validation_code_hash != candidate.validation_code_hash() {
		return Err(InvalidCandidate::CodeHashMismatch);
	}

	Ok(())
}

/// To determine the amount of timeout time for the pvf execution.
///
/// Precheck
/// 	The time period after which the preparation worker is considered
/// unresponsive and will be killed.
///
/// Prepare
/// The time period after which the preparation worker is considered
/// unresponsive and will be killed.
fn pvf_prep_timeout(executor_params: &ExecutorParams, kind: PvfPrepKind) -> Duration {
	if let Some(timeout) = executor_params.pvf_prep_timeout(kind) {
		return timeout;
	}
	match kind {
		PvfPrepKind::Precheck => DEFAULT_PRECHECK_PREPARATION_TIMEOUT,
		PvfPrepKind::Prepare => DEFAULT_LENIENT_PREPARATION_TIMEOUT,
	}
}

/// To determine the amount of timeout time for the pvf execution.
///
/// Backing subsystem
/// The amount of time to spend on execution during backing.
///
/// Approval subsystem
/// The amount of time to spend on execution during approval or disputes.
/// This should be much longer than the backing execution timeout to ensure that in the
/// absence of extremely large disparities between hardware, blocks that pass backing are
/// considered executable by approval checkers or dispute participants.
fn pvf_exec_timeout(executor_params: &ExecutorParams, kind: RuntimePvfExecKind) -> Duration {
	if let Some(timeout) = executor_params.pvf_exec_timeout(kind) {
		return timeout;
	}
	match kind {
		RuntimePvfExecKind::Backing => DEFAULT_BACKING_EXECUTION_TIMEOUT,
		RuntimePvfExecKind::Approval => DEFAULT_APPROVAL_EXECUTION_TIMEOUT,
	}
}
