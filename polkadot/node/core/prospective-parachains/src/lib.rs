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

//! Implementation of the Prospective Parachains subsystem - this tracks and handles
//! prospective parachain fragments and informs other backing-stage subsystems
//! of work to be done.
//!
//! This is the main coordinator of work within the node for the collation and
//! backing phases of parachain consensus.
//!
//! This is primarily an implementation of "Fragment Chains", as described in
//! [`polkadot_node_subsystem_util::inclusion_emulator`].
//!
//! This subsystem also handles concerns such as the relay-chain being forkful and session changes.

#![deny(unused_crate_dependencies)]

// Acknowledge dev-deps that are only consumed by integration tests under `tests/`,
// keeping the `unused_crate_dependencies` lint quiet. The lint checks each Cargo target
// against the whole dependency set, so the lib target flags a dep that only the `tests/`
// target uses. See https://github.com/rust-lang/rust/issues/95513.
#[cfg(test)]
use polkadot_subsystem_test_sim as _;

use std::collections::{BTreeSet, HashMap, HashSet};

use fragment_chain::CandidateStorage;
use futures::{channel::oneshot, prelude::*};

use polkadot_node_subsystem::{
	messages::{
		Ancestors, BackableCandidateRef, ChainApiMessage, HypotheticalCandidate,
		HypotheticalMembership, HypotheticalMembershipRequest, IntroduceSecondedCandidateRequest,
		KnownOutputHeads, ParentHeadData, ProspectiveParachainsMessage,
		ProspectiveValidationDataRequest, RuntimeApiMessage,
	},
	overseer, ActiveLeavesUpdate, FromOrchestra, OverseerSignal, SpawnedSubsystem, SubsystemError,
};
use polkadot_node_subsystem_util::{
	fetch_relay_parent_info,
	inclusion_emulator::{Constraints, RelayChainBlockInfo as RelayParentInfo},
	request_backing_constraints, request_candidates_pending_availability,
	request_session_index_for_child,
	runtime::{fetch_claim_queue, fetch_scheduling_lookahead},
};
use polkadot_primitives::{
	transpose_claim_queue, vstaging::RelayParentInfo as RuntimeRelayParentInfo, BlockNumber,
	CandidateHash, CommittedCandidateReceiptV2 as CommittedCandidateReceipt, Hash, Id as ParaId,
	PersistedValidationData, SessionIndex,
};
use schnellru::{ByLength, LruMap};

use crate::{
	error::{FatalError, FatalResult, JfyiError, JfyiErrorResult, Result},
	fragment_chain::{
		CandidateEntry, Error as FragmentChainError, FragmentChain, SchedulingScope,
		Scope as FragmentChainScope,
	},
};

mod error;
mod fragment_chain;
#[cfg(test)]
mod tests;

mod metrics;
use self::metrics::Metrics;

const LOG_TARGET: &str = "parachain::prospective-parachains";

/// Capacity of the relay-parent-info LRU cache. Bound: at most
/// `max_relay_parent_session_age * session_length` entries per session tracked.
/// However, we don't actually expect candidates with relay parents this old.
/// 2400 relay parents correspond to a session duration of 4 hours and should take around 190Kb if
/// full.
const RELAY_PARENT_INFO_CACHE_CAPACITY: u32 = 2400;

/// LRU cache mapping `(leaf_session, relay_parent)` to runtime-reported relay parent info.
type RelayParentInfoCache = LruMap<(SessionIndex, Hash), RuntimeRelayParentInfo<Hash, BlockNumber>>;

struct PerSchedulingParent {
	// The fragment chains for current and upcoming scheduled paras.
	fragment_chains: HashMap<ParaId, FragmentChain>,
	// The relay chain scope containing the scheduling parent and its allowed ancestors.
	// This is shared across all paras for this scheduling parent.
	scheduling_scope: SchedulingScope,
	// The session index at this scheduling parent. Used as a fallback when a V1 descriptor
	// doesn't carry a session index field, since V1 has `relay_parent == scheduling_parent`.
	session_index: SessionIndex,
}

struct View {
	/// Per scheduling parent fragment chains.
	///
	/// The keys include every currently active leaf plus the scheduling ancestors of each leaf
	/// (up to `scheduling_lookahead - 1` blocks deep). Entries may be retained for
	/// blocks that were active leaves at some point and are now in the ancestry of an active leaf.
	/// This is what preserves fragment-chain state across relay-chain
	/// reorgs. A candidate introduced under a former leaf remains available after the reorg as
	/// long as that former leaf is still referenced by a current leaf's ancestor set.
	///
	/// Entries are pruned in `handle_active_leaves_update` when no active leaf's ancestor set
	/// references them anymore.
	per_scheduling_parent: HashMap<Hash, PerSchedulingParent>,
	/// The hashes of the currently active leaves. Always a subset of the keys in
	/// `per_scheduling_parent`.
	active_leaves: HashSet<Hash>,
	/// LRU cache of relay-parent-info answers keyed by `(leaf_session, relay_parent)`.
	///
	/// Semantically this caches "under a leaf in this session, the runtime returned this info
	/// for this relay parent hash". Session-keying means entries from older sessions are
	/// naturally invalidated (miss + repopulate) when we move forward, matching the runtime's
	/// `max_relay_parent_session_age` pruning behavior. Only positive results are cached;
	/// `None`/`Err` results force a fresh query on the next call.
	relay_parent_info_cache: RelayParentInfoCache,
}

impl View {
	// Initialize with empty values.
	fn new() -> Self {
		View {
			per_scheduling_parent: HashMap::new(),
			active_leaves: HashSet::new(),
			relay_parent_info_cache: LruMap::new(ByLength::new(RELAY_PARENT_INFO_CACHE_CAPACITY)),
		}
	}
}

/// The prospective parachains subsystem.
#[derive(Default)]
pub struct ProspectiveParachainsSubsystem {
	metrics: Metrics,
}

impl ProspectiveParachainsSubsystem {
	/// Create a new instance of the `ProspectiveParachainsSubsystem`.
	pub fn new(metrics: Metrics) -> Self {
		Self { metrics }
	}
}

#[overseer::subsystem(ProspectiveParachains, error = SubsystemError, prefix = self::overseer)]
impl<Context> ProspectiveParachainsSubsystem
where
	Context: Send + Sync,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		SpawnedSubsystem {
			future: run(ctx, self.metrics)
				.map_err(|e| SubsystemError::with_origin("prospective-parachains", e))
				.boxed(),
			name: "prospective-parachains-subsystem",
		}
	}
}

#[overseer::contextbounds(ProspectiveParachains, prefix = self::overseer)]
async fn run<Context>(mut ctx: Context, metrics: Metrics) -> FatalResult<()> {
	let mut view = View::new();
	loop {
		crate::error::log_error(
			run_iteration(&mut ctx, &mut view, &metrics).await,
			"Encountered issue during run iteration",
		)?;
	}
}

#[overseer::contextbounds(ProspectiveParachains, prefix = self::overseer)]
async fn run_iteration<Context>(
	ctx: &mut Context,
	view: &mut View,
	metrics: &Metrics,
) -> Result<()> {
	loop {
		match ctx.recv().await.map_err(FatalError::SubsystemReceive)? {
			FromOrchestra::Signal(OverseerSignal::Conclude) => return Ok(()),
			FromOrchestra::Signal(OverseerSignal::ActiveLeaves(update)) => {
				handle_active_leaves_update(ctx, view, update, metrics).await?;
			},
			FromOrchestra::Signal(OverseerSignal::BlockFinalized(..)) => {},
			FromOrchestra::Communication { msg } => match msg {
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(request, tx) => {
					handle_introduce_seconded_candidate(ctx, view, request, tx, metrics).await
				},
				ProspectiveParachainsMessage::CandidateBacked(para, candidate_hash) => {
					handle_candidate_backed(view, para, candidate_hash, metrics).await
				},
				ProspectiveParachainsMessage::GetBackableCandidates {
					leaf,
					para_id,
					count,
					ancestors,
					sender,
				} => answer_get_backable_candidates(&view, leaf, para_id, count, ancestors, sender),
				ProspectiveParachainsMessage::GetHypotheticalMembership(request, tx) => {
					answer_hypothetical_membership_request(ctx, view, request, tx, metrics).await
				},
				ProspectiveParachainsMessage::GetProspectiveValidationData(request, tx) => {
					answer_prospective_validation_data_request(ctx, view, request, tx).await
				},
				ProspectiveParachainsMessage::GetKnownOutputHeads(para_ids, tx) => {
					answer_get_known_output_heads(view, para_ids, tx)
				},
			},
		}
	}
}

#[overseer::contextbounds(ProspectiveParachains, prefix = self::overseer)]
async fn handle_active_leaves_update<Context>(
	ctx: &mut Context,
	view: &mut View,
	update: ActiveLeavesUpdate,
	metrics: &Metrics,
) -> JfyiErrorResult<()> {
	// For any new active leaf:
	// - determine the scheduled paras
	// - pre-populate the candidate storage with pending availability candidates and candidates from
	//   the parent leaf
	// - populate the fragment chain
	// - add it to the active leaves
	//
	// Then mark the newly-deactivated leaves as deactivated.
	// Finally, remove any scheduling parents that are no longer part of an active leaf's ancestry.

	let _timer = metrics.time_handle_active_leaves_update();

	gum::trace!(
		target: LOG_TARGET,
		activated = ?update.activated,
		deactivated = ?update.deactivated,
		"Handle ActiveLeavesUpdate"
	);

	// There can only be one newly activated leaf, `update.activated` is an `Option`.
	for activated in update.activated.into_iter() {
		if update.deactivated.contains(&activated.hash) {
			continue;
		}

		let hash = activated.hash;
		let leaf_number = activated.number;

		let transposed_claim_queue =
			transpose_claim_queue(fetch_claim_queue(ctx.sender(), hash).await?.0);

		let session_index = request_session_index_for_child(hash, ctx.sender())
			.await
			.await
			.map_err(JfyiError::RuntimeApiRequestCanceled)??;

		let ancestry_len = fetch_scheduling_lookahead(hash, session_index, ctx.sender())
			.await?
			.saturating_sub(1);

		let ancestors = fetch_scheduling_parent_ancestors(
			ctx,
			hash,
			leaf_number,
			ancestry_len as usize,
			session_index,
		)
		.await?;

		let prev_fragment_chains = ancestors
			.first()
			.and_then(|(prev_leaf, _)| view.per_scheduling_parent.get(prev_leaf))
			.map(|d| &d.fragment_chains);

		// Create the relay chain scope once for this scheduling parent.
		// All paras share the same relay chain ancestry.
		// The ancestry is already limited by session boundaries and scheduling lookahead.
		let scheduling_scope = match SchedulingScope::new((hash, leaf_number), ancestors.clone()) {
			Ok(scope) => scope,
			Err(unexpected_ancestors) => {
				gum::warn!(
					target: LOG_TARGET,
					?ancestors,
					leaf = ?hash,
					"Relay chain ancestors have wrong order: {:?}",
					unexpected_ancestors
				);
				continue;
			},
		};

		let mut fragment_chains = HashMap::new();
		for (para, claims_by_depth) in transposed_claim_queue.iter() {
			// Find constraints and pending availability candidates.
			let Some((constraints, pending_availability)) =
				fetch_backing_constraints_and_candidates(ctx, hash, *para).await?
			else {
				// This indicates a runtime conflict of some kind.
				gum::debug!(
					target: LOG_TARGET,
					para_id = ?para,
					relay_parent = ?hash,
					"Failed to get inclusion backing state."
				);

				continue;
			};

			let pending_availability = preprocess_candidates_pending_availability(
				ctx,
				&mut view.relay_parent_info_cache,
				hash,
				session_index,
				&constraints,
				pending_availability,
			)
			.await?;
			let mut compact_pending = Vec::with_capacity(pending_availability.len());

			let mut pending_availability_storage = CandidateStorage::default();

			for c in pending_availability {
				let candidate_hash = c.compact.candidate_hash;
				let res = pending_availability_storage.add_pending_availability_candidate(
					candidate_hash,
					c.candidate,
					c.persisted_validation_data,
				);

				match res {
					Ok(_) | Err(FragmentChainError::CandidateAlreadyKnown) => {},
					Err(err) => {
						gum::warn!(
							target: LOG_TARGET,
							?candidate_hash,
							para_id = ?para,
							?err,
							"Scraped invalid candidate pending availability",
						);

						break;
					},
				}

				compact_pending.push(c.compact);
			}

			let max_backable_chain_len =
				claims_by_depth.values().flatten().collect::<BTreeSet<_>>().len();

			let min_relay_parent_number = constraints.min_relay_parent_number;

			let scope =
				FragmentChainScope::new(constraints, compact_pending, max_backable_chain_len);

			gum::trace!(
				target: LOG_TARGET,
				relay_parent = ?hash,
				min_relay_parent = min_relay_parent_number,
				max_backable_chain_len,
				para_id = ?para,
				ancestors = ?ancestors,
				"Creating fragment chain"
			);

			let number_of_pending_candidates = pending_availability_storage.len();

			// Init the fragment chain with the pending availability candidates.
			let mut chain =
				FragmentChain::init(&scheduling_scope, scope, pending_availability_storage);

			if chain.len() < number_of_pending_candidates {
				gum::warn!(
					target: LOG_TARGET,
					relay_parent = ?hash,
					para_id = ?para,
					"Not all pending availability candidates could be introduced. Actual vs expected count: {}, {}",
					chain.len(),
					number_of_pending_candidates
				)
			}

			// If we know the previous fragment chain, use that for further populating the fragment
			// chain.
			if let Some(prev_fragment_chain) =
				prev_fragment_chains.and_then(|chains| chains.get(para))
			{
				chain.populate_from_previous(&scheduling_scope, prev_fragment_chain);
			}

			gum::trace!(
				target: LOG_TARGET,
				relay_parent = ?hash,
				para_id = ?para,
				"Populated fragment chain with {} candidates: {:?}",
				chain.len(),
				chain.candidate_hashes()
			);

			gum::trace!(
				target: LOG_TARGET,
				relay_parent = ?hash,
				para_id = ?para,
				"Potential candidate storage for para: {:?}",
				chain.unconnected().map(|candidate| candidate.hash()).collect::<Vec<_>>()
			);

			fragment_chains.insert(*para, chain);
		}

		view.per_scheduling_parent
			.insert(hash, PerSchedulingParent { fragment_chains, scheduling_scope, session_index });

		view.active_leaves.insert(hash);
	}
	for deactivated in update.deactivated {
		view.active_leaves.remove(&deactivated);
	}

	// Prune scheduling parents that are no longer referenced by any active leaf.
	// Collect scheduling parents to keep: each active leaf plus all ancestors in its relay chain
	// scope.
	{
		let scheduling_parents_to_keep: HashSet<Hash> = view
			.active_leaves
			.iter()
			.filter_map(|leaf| {
				view.per_scheduling_parent.get(leaf).map(|data| {
					// Include the leaf itself and all its allowed ancestors:
					data.scheduling_scope.scheduling_parent_hashes()
				})
			})
			.flatten()
			.collect();

		view.per_scheduling_parent.retain(|h, _| scheduling_parents_to_keep.contains(h));
	}

	if metrics.0.is_some() {
		let mut active_connected = 0;
		let mut active_unconnected = 0;
		let mut candidates_in_implicit_view = 0;

		for (hash, PerSchedulingParent { fragment_chains, .. }) in view.per_scheduling_parent.iter()
		{
			if view.active_leaves.contains(hash) {
				for chain in fragment_chains.values() {
					active_connected += chain.len();
					active_unconnected += chain.unconnected_len();
				}
			} else {
				for chain in fragment_chains.values() {
					candidates_in_implicit_view += chain.len();
					candidates_in_implicit_view += chain.unconnected_len();
				}
			}
		}

		metrics.record_candidate_count(active_connected as u64, active_unconnected as u64);
		metrics.record_candidate_count_in_implicit_view(candidates_in_implicit_view as u64);
	}

	let num_active_leaves = view.active_leaves.len() as u64;
	let num_inactive_leaves =
		(view.per_scheduling_parent.len() as u64).saturating_sub(num_active_leaves);
	metrics.record_leaves_count(num_active_leaves, num_inactive_leaves);

	Ok(())
}

struct ImportablePendingAvailability {
	candidate: CommittedCandidateReceipt,
	persisted_validation_data: PersistedValidationData,
	compact: fragment_chain::PendingAvailability,
}

#[overseer::contextbounds(ProspectiveParachains, prefix = self::overseer)]
/// Preprocesses candidates pending availability into a format suitable for fragment chain storage.
///
/// This function transforms candidates that are pending availability (already on-chain but not
/// yet included) into the `ImportablePendingAvailability` format needed by fragment chains.
///
/// # Arguments
/// * `ctx` - Subsystem context for fetching block information
/// * `leaf` - Active leaf hash, used as the `query_at` for `fetch_relay_parent_info`
/// * `leaf_session_index` - Session index at the leaf; used as a fallback when a V1 descriptor
///   doesn't carry a session index
/// * `constraints` - Base constraints from the latest included candidate
/// * `pending_availability` - List of candidates pending availability, expected to form a chain
///
/// # Returns
/// A vector of importable pending availability candidates, potentially truncated if any
/// candidate's relay parent information cannot be fetched.
///
/// # Behavior
/// - Fetches relay parent info for each candidate via the runtime (or chain API fallback).
/// - Stops early if any relay parent info is unavailable (logs and returns partial list).
/// - Constructs `PersistedValidationData` for each candidate using constraints and the fetched
///   relay parent info.
async fn preprocess_candidates_pending_availability<Context>(
	ctx: &mut Context,
	rp_info_cache: &mut RelayParentInfoCache,
	leaf: Hash,
	leaf_session_index: SessionIndex,
	constraints: &Constraints,
	pending_availability: Vec<CommittedCandidateReceipt>,
) -> JfyiErrorResult<Vec<ImportablePendingAvailability>> {
	let mut required_parent = constraints.required_parent.clone();

	let mut importable = Vec::new();
	let expected_count = pending_availability.len();

	for (i, pending) in pending_availability.into_iter().enumerate() {
		let candidate_hash = pending.hash();

		let fetch_session = pending.descriptor.session_index().unwrap_or(leaf_session_index);
		let relay_parent = pending.descriptor.relay_parent();

		let Some(relay_parent_info) = fetch_relay_parent_info_cached(
			ctx.sender(),
			rp_info_cache,
			leaf_session_index,
			leaf,
			fetch_session,
			relay_parent,
		)
		.await?
		else {
			let para_id = pending.descriptor.para_id();
			gum::debug!(
				target: LOG_TARGET,
				?candidate_hash,
				?para_id,
				index = ?i,
				?expected_count,
				"Had to stop processing pending candidates early due to missing info.",
			);

			break;
		};

		let next_required_parent = pending.commitments.head_data.clone();
		importable.push(ImportablePendingAvailability {
			candidate: CommittedCandidateReceipt {
				descriptor: pending.descriptor,
				commitments: pending.commitments,
			},
			persisted_validation_data: PersistedValidationData {
				parent_head: required_parent,
				max_pov_size: constraints.max_pov_size as _,
				relay_parent_number: relay_parent_info.number,
				relay_parent_storage_root: relay_parent_info.state_root,
			},
			compact: fragment_chain::PendingAvailability {
				candidate_hash,
				relay_parent: RelayParentInfo {
					hash: relay_parent,
					number: relay_parent_info.number,
					storage_root: relay_parent_info.state_root,
				},
			},
		});

		required_parent = next_required_parent;
	}

	Ok(importable)
}

/// Verifies that the candidate's relay parent is within the leaf's
/// scope.
async fn verify_relay_parent_within_scope<Sender>(
	sender: &mut Sender,
	rp_info_cache: &mut RelayParentInfoCache,
	query_at: Hash,
	leaf_session_index: SessionIndex,
	candidate: &CommittedCandidateReceipt,
	relay_parent_number: BlockNumber,
	relay_parent_storage_root: Hash,
) -> JfyiErrorResult<()>
where
	Sender: polkadot_node_subsystem::SubsystemSender<RuntimeApiMessage>
		+ polkadot_node_subsystem::SubsystemSender<ChainApiMessage>,
{
	// For V1 descriptors `session_index()` is None; relay_parent == scheduling_parent for V1, so
	// the leaf's session applies. For V2/V3 the descriptor carries the session of its relay parent.
	let fetch_session = candidate.descriptor.session_index().unwrap_or(leaf_session_index);
	let relay_parent = candidate.descriptor.relay_parent();

	match fetch_relay_parent_info_cached(
		sender,
		rp_info_cache,
		leaf_session_index,
		query_at,
		fetch_session,
		relay_parent,
	)
	.await?
	{
		Some(info)
			if info.number == relay_parent_number &&
				info.state_root == relay_parent_storage_root =>
		{
			Ok(())
		},
		_ => Err(JfyiError::RelayParentOutOfScope),
	}
}

#[overseer::contextbounds(ProspectiveParachains, prefix = self::overseer)]
async fn handle_introduce_seconded_candidate<Context>(
	ctx: &mut Context,
	view: &mut View,
	request: IntroduceSecondedCandidateRequest,
	tx: oneshot::Sender<bool>,
	metrics: &Metrics,
) {
	let _timer = metrics.time_introduce_seconded_candidate();

	let IntroduceSecondedCandidateRequest {
		candidate_para: para,
		candidate_receipt: candidate,
		persisted_validation_data: pvd,
	} = request;

	let candidate_hash = candidate.hash();
	let relay_parent = candidate.descriptor.relay_parent();

	let candidate_entry =
		match CandidateEntry::new_seconded(candidate_hash, candidate.clone(), pvd.clone()) {
			Ok(candidate) => candidate,
			Err(err) => {
				gum::warn!(
					target: LOG_TARGET,
					para_id = ?para,
					"Cannot add seconded candidate: {}",
					err
				);

				let _ = tx.send(false);
				return;
			},
		};

	let mut added = Vec::with_capacity(view.per_scheduling_parent.len());
	let mut para_scheduled = false;
	// We don't iterate only through the active leaves. We also update any ancestor scheduling
	// parents that are still retained, so that their upcoming children may see these candidates.
	for (scheduling_parent, sp_data) in view.per_scheduling_parent.iter_mut() {
		let Some(chain) = sp_data.fragment_chains.get_mut(&para) else { continue };
		let is_active_leaf = view.active_leaves.contains(scheduling_parent);

		para_scheduled = true;

		if let Err(err) = verify_relay_parent_within_scope(
			ctx.sender(),
			&mut view.relay_parent_info_cache,
			*scheduling_parent,
			sp_data.session_index,
			&candidate,
			pvd.relay_parent_number,
			pvd.relay_parent_storage_root,
		)
		.await
		{
			gum::trace!(
				target: LOG_TARGET,
				?para,
				?candidate_hash,
				?scheduling_parent,
				?relay_parent,
				"Cannot introduce seconded candidate: {}",
				err
			);
			continue;
		}

		match chain.try_adding_seconded_candidate(&sp_data.scheduling_scope, &candidate_entry) {
			Ok(()) => {
				added.push(*scheduling_parent);
			},
			Err(FragmentChainError::CandidateAlreadyKnown) => {
				gum::trace!(
					target: LOG_TARGET,
					?para,
					?scheduling_parent,
					?is_active_leaf,
					"Attempting to introduce an already known candidate: {:?}",
					candidate_hash
				);
				added.push(*scheduling_parent);
			},
			Err(err) => {
				gum::trace!(
					target: LOG_TARGET,
					?para,
					?scheduling_parent,
					?candidate_hash,
					?is_active_leaf,
					"Cannot introduce seconded candidate: {}",
					err
				)
			},
		}
	}

	if !para_scheduled {
		gum::warn!(
			target: LOG_TARGET,
			para_id = ?para,
			?candidate_hash,
			"Received seconded candidate for inactive para",
		);
	}

	if added.is_empty() {
		gum::debug!(
			target: LOG_TARGET,
			para_id = ?para,
			candidate = ?candidate_hash,
			"Newly-seconded candidate cannot be kept under any scheduling parent",
		);
	} else {
		gum::debug!(
			target: LOG_TARGET,
			?para,
			"Added/Kept seconded candidate {:?} on scheduling parents: {:?}",
			candidate_hash,
			added
		);
	}

	let _ = tx.send(!added.is_empty());
}

async fn handle_candidate_backed(
	view: &mut View,
	para: ParaId,
	candidate_hash: CandidateHash,
	metrics: &Metrics,
) {
	let _timer = metrics.time_candidate_backed();

	let mut found_candidate = false;
	let mut found_para = false;

	// We don't iterate only through the active leaves. We also update any ancestor scheduling
	// parents that are still retained, so that their upcoming children may see these candidates.
	for (scheduling_parent, sp_data) in view.per_scheduling_parent.iter_mut() {
		let Some(chain) = sp_data.fragment_chains.get_mut(&para) else { continue };
		let is_active_leaf = view.active_leaves.contains(scheduling_parent);

		found_para = true;
		if chain.is_candidate_backed(&candidate_hash) {
			gum::debug!(
				target: LOG_TARGET,
				?para,
				?candidate_hash,
				?is_active_leaf,
				"Received redundant instruction to mark as backed an already backed candidate",
			);
			found_candidate = true;
		} else if chain.contains_unconnected_candidate(&candidate_hash) {
			found_candidate = true;
			// Mark the candidate as backed. This can recreate the fragment chain.
			chain.candidate_backed(&sp_data.scheduling_scope, &candidate_hash);

			gum::trace!(
				target: LOG_TARGET,
				?scheduling_parent,
				?para,
				?is_active_leaf,
				?candidate_hash,
				"Candidate backed. Candidate chain for para: {:?}",
				chain.candidate_hashes()
			);

			gum::trace!(
				target: LOG_TARGET,
				?scheduling_parent,
				?para,
				?is_active_leaf,
				"Potential candidate storage for para: {:?}",
				chain.unconnected().map(|candidate| candidate.hash()).collect::<Vec<_>>()
			);
		}
	}

	if !found_para {
		gum::warn!(
			target: LOG_TARGET,
			?para,
			?candidate_hash,
			"Received instruction to back a candidate for unscheduled para",
		);

		return;
	}

	if !found_candidate {
		// This can be harmless. It can happen if we received a better backed candidate before and
		// dropped this other candidate already.
		gum::debug!(
			target: LOG_TARGET,
			?para,
			?candidate_hash,
			"Received instruction to back unknown candidate",
		);
	}
}

fn answer_get_backable_candidates(
	view: &View,
	leaf: Hash,
	para: ParaId,
	count: u32,
	ancestors: Ancestors,
	tx: oneshot::Sender<Vec<BackableCandidateRef>>,
) {
	if !view.active_leaves.contains(&leaf) {
		gum::debug!(
			target: LOG_TARGET,
			?leaf,
			para_id = ?para,
			"Requested backable candidate for inactive leaf."
		);

		let _ = tx.send(vec![]);
		return;
	}
	let Some(data) = view.per_scheduling_parent.get(&leaf) else {
		gum::debug!(
			target: LOG_TARGET,
			?leaf,
			para_id = ?para,
			"Requested backable candidate for inexistent leaf."
		);

		let _ = tx.send(vec![]);
		return;
	};

	let Some(chain) = data.fragment_chains.get(&para) else {
		gum::debug!(
			target: LOG_TARGET,
			?leaf,
			para_id = ?para,
			"Requested backable candidate for inactive para."
		);

		let _ = tx.send(vec![]);
		return;
	};

	gum::trace!(
		target: LOG_TARGET,
		?leaf,
		para_id = ?para,
		"Candidate chain for para: {:?}",
		chain.candidate_hashes()
	);

	gum::trace!(
		target: LOG_TARGET,
		?leaf,
		para_id = ?para,
		"Potential candidate storage for para: {:?}",
		chain.unconnected().map(|candidate| candidate.hash()).collect::<Vec<_>>()
	);

	let backable_candidates = chain.find_backable_chain(ancestors.clone(), count);

	if backable_candidates.is_empty() {
		gum::trace!(
			target: LOG_TARGET,
			?ancestors,
			para_id = ?para,
			%leaf,
			"Could not find any backable candidate",
		);
	} else {
		gum::trace!(
			target: LOG_TARGET,
			?leaf,
			?backable_candidates,
			?ancestors,
			"Found backable candidates",
		);
	}

	let _ = tx.send(backable_candidates);
}

#[overseer::contextbounds(ProspectiveParachains, prefix = self::overseer)]
async fn answer_hypothetical_membership_request<Context>(
	ctx: &mut Context,
	view: &mut View,
	request: HypotheticalMembershipRequest,
	tx: oneshot::Sender<Vec<(HypotheticalCandidate, HypotheticalMembership)>>,
	metrics: &Metrics,
) {
	let _timer = metrics.time_hypothetical_membership_request();

	let mut response = Vec::with_capacity(request.candidates.len());
	for candidate in request.candidates {
		response.push((candidate, vec![]));
	}

	let required_active_leaf = request.fragment_chain_relay_parent;
	for active_leaf in view
		.active_leaves
		.iter()
		.filter(|h| required_active_leaf.as_ref().map_or(true, |x| h == &x))
	{
		let Some(leaf_view) = view.per_scheduling_parent.get(active_leaf) else { continue };
		for (candidate, membership) in &mut response {
			let para_id = &candidate.candidate_para();
			let Some(fragment_chain) = leaf_view.fragment_chains.get(para_id) else { continue };

			let res = match candidate {
				HypotheticalCandidate::Complete {
					candidate_hash,
					ref receipt,
					ref persisted_validation_data,
				} => {
					// For Complete candidates, verify the relay parent against this leaf before
					// running the membership check. Incomplete candidates carry no PVD —
					// nothing to verify.
					if let Err(err) = verify_relay_parent_within_scope(
						ctx.sender(),
						&mut view.relay_parent_info_cache,
						*active_leaf,
						leaf_view.session_index,
						receipt.as_ref(),
						persisted_validation_data.relay_parent_number,
						persisted_validation_data.relay_parent_storage_root,
					)
					.await
					{
						gum::trace!(
							target: LOG_TARGET,
							para_id = ?para_id,
							candidate = ?candidate.candidate_hash(),
							relay_parent = ?receipt.descriptor.relay_parent(),
							"Candidate is not a hypothetical member on {:?}: {}",
							active_leaf,
							err,
						);
						continue;
					}

					// For complete candidates, build a CandidateEntry and run the full
					// potential check including constraint validation.
					let entry = fragment_chain::CandidateEntry::new_seconded(
						*candidate_hash,
						(**receipt).clone(),
						persisted_validation_data.clone(),
					);
					match entry {
						Ok(entry) => fragment_chain
							.can_add_candidate_as_potential(&leaf_view.scheduling_scope, &entry),
						Err(_) => continue,
					}
				},
				HypotheticalCandidate::Incomplete { .. } => {
					fragment_chain.can_add_candidate_as_potential_hypothetical(
						&leaf_view.scheduling_scope,
						candidate.scheduling_parent(),
						// This could be Some(..), but we need to fetch the relay parent info and
						// we don't have the session index in the advertisement..
						None,
						candidate.candidate_hash(),
						candidate.parent_head_data_hash(),
						candidate.output_head_data_hash(),
					)
				},
			};
			match res {
				Err(FragmentChainError::CandidateAlreadyKnown) | Ok(()) => {
					membership.push(*active_leaf);
				},
				Err(err) => {
					gum::trace!(
						target: LOG_TARGET,
						para_id = ?para_id,
						leaf = ?active_leaf,
						candidate = ?candidate.candidate_hash(),
						"Candidate is not a hypothetical member on: {}",
						err
					)
				},
			};
		}
	}

	for (candidate, membership) in &response {
		if membership.is_empty() {
			gum::debug!(
				target: LOG_TARGET,
				para_id = ?candidate.candidate_para(),
				active_leaves = ?view.active_leaves,
				?required_active_leaf,
				candidate = ?candidate.candidate_hash(),
				"Candidate is not a hypothetical member on any of the active leaves",
			)
		}
	}

	let _ = tx.send(response);
}

#[overseer::contextbounds(ProspectiveParachains, prefix = self::overseer)]
async fn answer_prospective_validation_data_request<Context>(
	ctx: &mut Context,
	view: &mut View,
	request: ProspectiveValidationDataRequest,
	tx: oneshot::Sender<Option<PersistedValidationData>>,
) {
	// Try getting the needed data from any fragment chain.

	let (mut head_data, parent_head_data_hash) = match request.parent_head_data {
		ParentHeadData::OnlyHash(parent_head_data_hash) => (None, parent_head_data_hash),
		ParentHeadData::WithData { head_data, hash } => (Some(head_data), hash),
	};

	// Search fragment chains across active leaves to find the head_data, relay_parent_info, and
	// max_pov_size needed to construct the PersistedValidationData for this candidate:
	let mut relay_parent_info = None;
	let mut max_pov_size = None;
	for (leaf, leaf_session_index, fragment_chain) in
		view.active_leaves.iter().filter_map(|active_leaf| {
			view.per_scheduling_parent.get(active_leaf).and_then(|data| {
				data.fragment_chains
					.get(&request.para_id)
					.map(|chain| (*active_leaf, data.session_index, chain))
			})
		}) {
		if head_data.is_some() && relay_parent_info.is_some() && max_pov_size.is_some() {
			break;
		}

		if relay_parent_info.is_none() {
			relay_parent_info = if let Some(info) = fetch_relay_parent_info_cached(
				ctx.sender(),
				&mut view.relay_parent_info_cache,
				leaf_session_index,
				leaf,
				request.session_index,
				request.candidate_relay_parent,
			)
			.await
			.ok()
			.flatten()
			{
				if max_pov_size.is_none() {
					// TODO(https://github.com/paritytech/polkadot-sdk/issues/11256): serve
					// `max_pov_size` from the candidate's relay-parent session rather than the
					// scheduling session. We are leaning hard on two assumptions here:
					// 1. Collators need to use the max_pov_size of the scheduling session, not of
					//    the relay parent session.
					// 2. The max_pov_size is only configurable per session and is expected to
					//    change extremely rarely. It is acceptable if the collators will have to
					//    rebuild the block if there was a change in the max_pov_size.
					max_pov_size = Some(fragment_chain.scope().base_constraints().max_pov_size);
				}

				Some(info)
			} else {
				None
			}
		}

		if head_data.is_none() {
			head_data = fragment_chain.get_head_data_by_hash(&parent_head_data_hash);
		}
	}

	let _ = tx.send(match (head_data, relay_parent_info, max_pov_size) {
		(Some(h), Some(i), Some(m)) => Some(PersistedValidationData {
			parent_head: h,
			relay_parent_number: i.number,
			relay_parent_storage_root: i.state_root,
			max_pov_size: m as _,
		}),
		_ => None,
	});
}

fn answer_get_known_output_heads(
	view: &View,
	para_ids: Vec<ParaId>,
	tx: oneshot::Sender<KnownOutputHeads>,
) {
	let mut known = KnownOutputHeads::new();
	for (sp, per_sp) in &view.per_scheduling_parent {
		for para_id in &para_ids {
			if let Some(per_para) = per_sp.fragment_chains.get(&para_id) {
				let per_para_known: &mut HashMap<ParaId, HashSet<Hash>> =
					known.entry(*sp).or_default();
				let entry = per_para_known.entry(*para_id).or_default();
				entry.extend(per_para.known_output_heads());
			}
		}
	}
	let _ = tx.send(known);
}

#[overseer::contextbounds(ProspectiveParachains, prefix = self::overseer)]
async fn fetch_backing_constraints_and_candidates<Context>(
	ctx: &mut Context,
	relay_parent: Hash,
	para_id: ParaId,
) -> JfyiErrorResult<Option<(Constraints, Vec<CommittedCandidateReceipt>)>> {
	let maybe_constraints = request_backing_constraints(relay_parent, para_id, ctx.sender())
		.await
		.await
		.map_err(JfyiError::RuntimeApiRequestCanceled)??;

	let Some(constraints) = maybe_constraints else { return Ok(None) };

	let pending_availability =
		request_candidates_pending_availability(relay_parent, para_id, ctx.sender())
			.await
			.await
			.map_err(JfyiError::RuntimeApiRequestCanceled)??;

	Ok(Some((From::from(constraints), pending_availability)))
}

/// Fetches block information for ancestors of a given relay chain block.
///
/// Returns up to `ancestors` ancestor blocks in descending order (from most recent to oldest),
/// stopping early if an ancestor is from a different session than `required_session`, if block
/// info cannot be fetched, or if genesis is reached.
///
/// # Returns
///
/// A vector of `BlockInfo` containing block hashes, numbers, and storage roots for all
/// ancestors within `required_session`, in descending order by block number.
#[overseer::contextbounds(ProspectiveParachains, prefix = self::overseer)]
async fn fetch_scheduling_parent_ancestors<Context>(
	ctx: &mut Context,
	relay_hash: Hash,
	mut number: BlockNumber,
	ancestors: usize,
	required_session: u32,
) -> JfyiErrorResult<Vec<(Hash, BlockNumber)>> {
	if ancestors == 0 {
		return Ok(Vec::new());
	}

	let (tx, rx) = oneshot::channel();
	ctx.send_message(ChainApiMessage::Ancestors {
		hash: relay_hash,
		k: ancestors,
		response_channel: tx,
	})
	.await;

	let hashes = rx.map_err(JfyiError::ChainApiRequestCanceled).await??;

	let mut ancestors = Vec::with_capacity(hashes.len());

	for hash in hashes {
		// The relay chain cannot accept blocks backed from previous sessions, with
		// potentially previous validators. This is a technical limitation we need to
		// respect here.

		let session = request_session_index_for_child(hash, ctx.sender())
			.await
			.await
			.map_err(JfyiError::RuntimeApiRequestCanceled)??;

		if session != required_session {
			break;
		}

		number = number.saturating_sub(1);
		ancestors.push((hash, number));
	}

	Ok(ancestors)
}

/// Caching wrapper over `fetch_relay_parent_info`.
///
/// On hit: returns the cached info without any runtime/chain calls.
/// On miss: calls `fetch_relay_parent_info`; on `Ok(Some(info))` inserts into the cache before
/// returning. `None`/`Err` are passed through without caching (a future query with different
/// inputs may succeed).
async fn fetch_relay_parent_info_cached<Sender>(
	sender: &mut Sender,
	cache: &mut RelayParentInfoCache,
	leaf_session: SessionIndex,
	query_at: Hash,
	fetch_session: SessionIndex,
	relay_parent: Hash,
) -> JfyiErrorResult<Option<RuntimeRelayParentInfo<Hash, BlockNumber>>>
where
	Sender: polkadot_node_subsystem::SubsystemSender<RuntimeApiMessage>
		+ polkadot_node_subsystem::SubsystemSender<ChainApiMessage>,
{
	if let Some(info) = cache.get(&(leaf_session, relay_parent)) {
		return Ok(Some(info.clone()));
	}
	match fetch_relay_parent_info(sender, query_at, fetch_session, relay_parent).await? {
		Some(info) => {
			cache.insert((leaf_session, relay_parent), info.clone());
			Ok(Some(info))
		},
		None => Ok(None),
	}
}
