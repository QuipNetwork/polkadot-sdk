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

//! Segment-based collation management for the experimental validator side.
//!
//! Every advertisement is stored as a segment: V1 an empty-entries segment, V2/V3 a
//! single by-hash entry, V4 an age-ordered list of by-output-head fingerprints. A
//! stored segment is one fetch entitlement. The planner fills claim-queue positions
//! back-to-front, ranking the live segments for each position's para by collator
//! reputation, then earliest arrival. The picked segment is resolved to a fetch target
//! at fetch time: entries are walked oldest-first and the first entry that is not
//! blocked is launched. An entry is blocked when its output head is already fetched
//! (para-wide, across in-view scheduling parents), in flight, or known to prospective
//! parachains (whose answer covers candidate output heads plus the para's latest included
//! head, so an entry that would cycle the chain is blocked too). Blockers are
//! classified: prospective-parachains knowledge is hard (durable); fetched and
//! in-flight heads are soft (pending attempts that may still fail). A head that is
//! both is hard.
//!
//! A launch consumes the picked segment. A segment with no launchable entry is
//! consumed only when every entry is hard-blocked; with at least one soft blocker it
//! is held un-consumed instead — stored as the retry channel, picked again once the
//! pending attempt concludes. A failed fetch or an invalidated collation frees the
//! head and the held segment launches; a seconded one turns the blocker hard and the
//! segment is consumed on a later pick. Either way the position falls through to the
//! next-ranked segment. Consumption sets a flag; memory is reclaimed by a sweep at
//! the top of `note_fetched`, and consumed segments are invisible to every reader via
//! `live_segments`.
//!
//! Prospective-parachains knowledge is queried live before each planner pass
//! (`GetKnownOutputHeads`) and passed in; staleness is bounded by PP's own
//! statement-ingestion latency. PP answers per scheduling parent it still holds:
//! every active leaf plus retained ancestors within scheduling lookahead. Knowledge
//! is per leaf: each (leaf, core) view is filled with PP's answer for that leaf plus
//! our fetched and in-flight heads whose scheduling parent is on that leaf's path.
//! A head known only under a sibling relay fork does not block. A segment at a
//! scheduling parent shared by two live leaves that disagree is resolved by whichever
//! view is filled first; the collator re-advertises next slot.

use crate::{
	extract_leaf_scheduling_info, is_scheduling_parent_valid,
	validator_side::{
		descriptor_version_sanity_check_with_params, error::SecondingError,
		request_persisted_validation_data, request_prospective_validation_data, BlockedCollationId,
	},
	validator_side_experimental::{
		common::{
			Advertisement, CanSecond, CollationFetchError, CollationFetchResponse,
			ProspectiveCandidate, Score, SecondingRejectionInfo, FAILED_FETCH_SLASH,
			INSTANT_FETCH_REP_THRESHOLD, MAX_FETCH_DELAY,
		},
		error::{Error, FatalResult, Result},
	},
	LeafClaimQueues, LeafSchedulingInfo, LOG_TARGET,
};
use fatality::Split;
use futures::{channel::oneshot, stream::FusedStream};
use polkadot_node_clock::Clock;
use polkadot_node_network_protocol::{
	peer_set::CollationVersion,
	request_response::{outgoing::RequestError, v2 as request_v2, Requests},
	OurView, PeerId,
};
use polkadot_node_primitives::PoV;
use polkadot_node_subsystem::{
	messages::{CanSecondRequest, CandidateBackingMessage, KnownOutputHeads},
	ActivatedLeaf, CollatorProtocolSenderTrait,
};
use polkadot_node_subsystem_util::{
	backing_implicit_view::View as ImplicitView, metrics::prometheus::prometheus::HistogramTimer,
	request_session_index_for_child, request_validator_groups, request_validators,
	runtime::recv_runtime,
};
use polkadot_primitives::{
	CandidateDescriptorVersion, CandidateHash, CandidateReceiptV2 as CandidateReceipt, CoreIndex,
	GroupIndex, GroupRotationInfo, Hash, HeadData, Id as ParaId, PersistedValidationData,
	SessionIndex,
};
use requests::PendingRequests;
use schnellru::{ByLength, LruMap};
use sp_keystore::KeystorePtr;
use sp_runtime::Either;
use std::{
	collections::{BTreeMap, BTreeSet, HashMap, HashSet},
	sync::Arc,
	time::{Duration, Instant},
};

mod requests;

/// Reason for rejecting an advertisement.
#[derive(Debug, thiserror::Error)]
pub enum AdvertisementError {
	#[error("Duplicate advertisement")]
	Duplicate,
	#[error("Advertised scheduling parent is out of our view")]
	OutOfOurView,
	#[error("Peer reached the candidate limit (or para is not schedulable from this SP)")]
	PeerLimitReached,
	#[error("Seconding not allowed by backing subsystem")]
	BlockedByBacking,
	#[error("V1 advertisements are only allowed on active leaves")]
	V1AdvertisementForImplicitParent,
	#[error("For V3 candidate descriptors, scheduling_parent does not match any expected scheduling parent.")]
	SchedulingParentNotValid,
	#[error("Segment mixes by-hash and by-output-head entries")]
	MixedClaimShapes,
}

pub struct CollationManager {
	// The backing implicit view, which is used to track the active leaves and their implicit
	// ancestors.
	implicit_view: ImplicitView,

	// The per-core claim queues (plus scheduling lookahead) for each active leaf.
	leaf_claim_queues: HashMap<Hash, LeafClaimQueues>,

	// Collations which we haven't been able to second due to their parent not being known by
	// prospective-parachains. Mapped from the para_id and parent_head_hash to the fetched
	// collation data. Only needed for async backing. For elastic scaling, the fetched collation
	// must contain the full parent head data.
	blocked_from_seconding: HashMap<BlockedCollationId, Vec<FetchedCollation>>,

	// Information kept per scheduling parent.
	per_scheduling_parent: HashMap<Hash, PerSchedulingParent>,

	// Session info cache.
	per_session: LruMap<SessionIndex, PerSessionInfo>,

	// Collection of active collation fetch requests.
	fetching: PendingRequests,

	// Key store.
	keystore: KeystorePtr,
	leaf_scheduling_info: HashMap<Hash, LeafSchedulingInfo>,
	// Clock for time reads (V3 scheduling-parent slot validation, advertisement timestamps).
	clock: Arc<dyn Clock>,
	// Rate-limiting state for the (potentially frequent) collation-fetch error warnings, so a
	// flaky network or a buggy `Canceled` loop can't flood the logs.
	network_error_freq: gum::Freq,
	canceled_freq: gum::Freq,
	// Set by mutations that can enable a fetch launch; consumed by the main
	// loop to decide whether to run a planner pass.
	replan: bool,
}

impl CollationManager {
	pub async fn new<Sender: CollatorProtocolSenderTrait>(
		sender: &mut Sender,
		keystore: KeystorePtr,
		active_leaf: ActivatedLeaf,
		clock: Arc<dyn Clock>,
	) -> FatalResult<Self> {
		let mut instance = Self {
			implicit_view: ImplicitView::new(),
			leaf_claim_queues: HashMap::new(),
			per_scheduling_parent: HashMap::new(),
			blocked_from_seconding: HashMap::new(),
			per_session: LruMap::new(ByLength::new(2)),
			fetching: PendingRequests::default(),
			keystore,
			leaf_scheduling_info: HashMap::default(),
			clock,
			network_error_freq: gum::Freq::new(),
			canceled_freq: gum::Freq::new(),
			replan: false,
		};

		instance.update_view(sender, OurView::new([active_leaf.hash], 0)).await?;

		Ok(instance)
	}

	pub fn take_replan(&mut self) -> bool {
		std::mem::take(&mut self.replan)
	}

	pub fn mark_replan(&mut self) {
		self.replan = true;
	}

	pub async fn update_view<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		new_view: OurView,
	) -> FatalResult<()> {
		let removed = self
			.implicit_view
			.leaves()
			.filter(|h| !new_view.contains(h))
			.cloned()
			.collect::<Vec<_>>();
		let added = new_view
			.iter()
			.filter(|h| !self.implicit_view.contains_leaf(h))
			.cloned()
			.collect::<Vec<_>>();

		gum::trace!(
			target: LOG_TARGET,
			?added,
			?removed,
			"CollationManager: Processing view update"
		);

		for leaf in added.iter() {
			match extract_leaf_scheduling_info(sender, *leaf).await {
				Some(info) => {
					self.leaf_scheduling_info.insert(*leaf, info);
				},
				None => {
					gum::warn!(
						target: LOG_TARGET,
						?leaf,
						"Could not extract BABE slot from leaf header; \
						 V3 scheduling parent validation will reject advertisements \
						 referencing this leaf",
					);
				},
			}

			if let Err(err) = self
				.implicit_view
				.activate_leaf(sender, *leaf)
				.await
				.map_err(Error::FailedToActivateLeafInImplicitView)
			{
				err.split()?.log();
				continue;
			}
		}

		for leaf in removed {
			self.implicit_view.deactivate_leaf(leaf);
			self.leaf_scheduling_info.remove(&leaf);
			self.leaf_claim_queues.remove(&leaf);
		}

		// Rebuild `per_scheduling_parent`, dropping entries no longer reachable from any
		// current leaf and cancelling their in-flight fetches.
		self.per_scheduling_parent = std::mem::take(&mut self.per_scheduling_parent)
			.into_iter()
			.filter_map(|(sp, per_sp)| {
				if !self.implicit_view.paths_via_relay_parent(&sp).is_empty() {
					return Some((sp, per_sp));
				}

				gum::trace!(
					target: LOG_TARGET,
					scheduling_parent = ?sp,
					"Scheduling parent no longer reachable from any leaf; dropping it and cancelling its in-flight fetches",
				);

				let to_cancel: Vec<_> = self.fetching.iter().filter(|adv| adv.scheduling_parent == sp).copied().collect();
				for advertisement in to_cancel {
					self.fetching.cancel(&advertisement);
				}
				None
			})
			.collect();

		// Remove blocked seconding requests whose scheduling parent is no longer tracked.
		self.blocked_from_seconding.retain(|_, collations| {
			collations.retain(|c| self.per_scheduling_parent.contains_key(&c.scheduling_parent()));
			!collations.is_empty()
		});

		for leaf in added.iter() {
			let Some(allowed_ancestry) =
				self.implicit_view.known_allowed_relay_parents_under(leaf).map(|v| v.to_vec())
			else {
				continue;
			};
			let session_index =
				match recv_runtime(request_session_index_for_child(*leaf, sender).await)
					.await
					.map_err(Error::Runtime)
				{
					Ok(session_index) => session_index,
					Err(err) => {
						err.split()?.log();
						continue;
					},
				};

			// Register every newly-known scheduling parent (the leaf and any of its allowed
			// ancestors not yet in our view) with the core our group is assigned to *at that
			// block*. This is what determines which core's view of the leaf's CQ applies to
			// advertisements rooted at that scheduling parent.
			for ancestor in allowed_ancestry.iter() {
				if self.per_scheduling_parent.contains_key(ancestor) {
					continue;
				}

				let core = match self.get_our_core(sender, ancestor, session_index).await {
					Ok(core) => core,
					Err(err) => {
						err.split()?.log();
						continue;
					},
				};
				gum::trace!(
					target: LOG_TARGET,
					scheduling_parent = ?ancestor,
					?core,
					session_index,
					"Registered scheduling parent on our assigned core",
				);
				self.per_scheduling_parent
					.insert(*ancestor, PerSchedulingParent::new(session_index, core, &*self.clock));
			}

			// Fetch and store the leaf's per-core claim queues and scheduling lookahead. Capacity
			// at every scheduling parent on a path to this leaf is computed from these via offset
			// arithmetic — the leaf is authoritative because it's closest to what the runtime will
			// see when candidates get backed.
			match LeafClaimQueues::fetch(*leaf, session_index, sender)
				.await
				.map_err(Error::Runtime)
			{
				Ok(leaf_claim_queues) => {
					self.leaf_claim_queues.insert(*leaf, leaf_claim_queues);
				},
				Err(err) => {
					err.split()?.log();
					continue;
				},
			}
		}

		self.mark_replan();
		Ok(())
	}

	pub fn response_stream(&mut self) -> &mut impl FusedStream<Item = CollationFetchResponse> {
		self.fetching.response_stream()
	}

	/// All paras our group will back at *some* scheduling parent in our view. Used to decide
	/// which collators we should be willing to talk to. We take the union across all
	/// scheduling parents of `our_window(sp)` — the slice of the leaf's CQ visible from that
	/// SP for our core.
	pub fn assignments(&self) -> BTreeSet<ParaId> {
		self.per_scheduling_parent
			.iter()
			.flat_map(|(sp, per_sp)| self.our_window(sp, per_sp.core_index))
			.collect()
	}

	/// Number of CQ positions assigned to `para_id` in the SP's visible window of our core.
	///
	/// Returns `0` if the SP isn't in view.
	///
	/// Note: this is *not* a capacity check. Capacity (which slots are still unfulfilled) is
	/// enforced separately in `try_make_new_fetch_requests` via
	/// `unfulfilled_claim_queue_entries`. Accepting an advertisement that won't be fetchable
	/// right away is fine — it stays parked in `peer_advertisements` until a slot opens up.
	fn slots_available(&self, scheduling_parent: &Hash, para_id: ParaId) -> usize {
		let Some(per_sp) = self.per_scheduling_parent.get(scheduling_parent) else {
			return 0;
		};
		self.our_window(scheduling_parent, per_sp.core_index)
			.iter()
			.filter(|p| **p == para_id)
			.count()
	}

	/// Accept an advertisement of any protocol version, stored uniformly as a segment: V1 is
	/// an empty-entries segment, V2/V3 a length-1 by-hash segment, V4 a length-N segment of
	/// fingerprints that carry no candidate hash. A stored segment is ONE fetch entitlement:
	/// launching resolves which entry to use and consumes the whole segment.
	///
	/// Duplicate rules are claim-shape-driven, preserving the old per-version behavior
	/// exactly: a hash is a complete claim identity, so an already-fetched or in-flight
	/// by-hash claim is a duplicate; for by-output-head segments an overlap with
	/// fetched/in-flight state is NOT a duplicate — resolution handles it at fetch time.
	/// Byte-identical segments (same descriptor version, same entries) from the same peer
	/// are duplicates while stored; consumption at launch means a re-advertisement after the
	/// fetch launched is a fresh entitlement.
	pub async fn try_accept_segment<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		peer_id: PeerId,
		para_id: ParaId,
		scheduling_parent: Hash,
		descriptor_version: Option<CandidateDescriptorVersion>,
		entries: Vec<ProspectiveCandidate>,
	) -> std::result::Result<(), AdvertisementError> {
		// Segments are homogeneous by construction: one message, one claim shape.
		let homogeneous = entries.iter().all(|e| matches!(e, ProspectiveCandidate::ByHash { .. })) ||
			entries.iter().all(|e| matches!(e, ProspectiveCandidate::ByOutputHead { .. }));
		if !homogeneous {
			gum::error!(
				target: LOG_TARGET,
				?peer_id,
				?para_id,
				?scheduling_parent,
				?descriptor_version,
				?entries,
				"Segment mixes by-hash and by-output-head entries; rejecting. \
				 Entries should be homogeneous by construction.",
			);
			return Err(AdvertisementError::MixedClaimShapes);
		}

		let segment = StoredSegment {
			descriptor_version,
			entries,
			received_at: self.clock.now(),
			para_id,
			consumed: false,
		};

		// V1 advertisements (empty entries) are only allowed on active leaves.
		if segment.entries.is_empty() && !self.implicit_view.contains_leaf(&scheduling_parent) {
			return Err(AdvertisementError::V1AdvertisementForImplicitParent);
		}

		// V3 candidate descriptors require scheduling_parent to be the block from the last
		// finished relay chain slot.
		if segment.descriptor_version == Some(CandidateDescriptorVersion::V3) &&
			!is_scheduling_parent_valid(
				&*self.clock,
				&scheduling_parent,
				&self.leaf_scheduling_info,
			) {
			return Err(AdvertisementError::SchedulingParentNotValid);
		}

		let available_slots = self.slots_available(&scheduling_parent, para_id);

		let Some(per_sp) = self.per_scheduling_parent.get_mut(&scheduling_parent) else {
			return Err(AdvertisementError::OutOfOurView);
		};

		let maybe_advertisement = match segment.entries.as_slice() {
			// A hash is a complete claim identity: an already-fetched or in-flight candidate
			// makes the advertisement a duplicate.
			[ProspectiveCandidate::ByHash { candidate_hash, .. }] => {
				let advertisement = segment
					.as_advertisement(peer_id, scheduling_parent)
					.expect("single-entry segment always has an advertisement; qed");
				if per_sp.fetched_collations.contains_key(candidate_hash) {
					return Err(AdvertisementError::Duplicate);
				}
				if self.fetching.contains(&advertisement) {
					return Err(AdvertisementError::Duplicate);
				}
				Some(advertisement)
			},
			// V1: at most one identical offer may be in flight.
			[] => {
				let advertisement = segment
					.as_advertisement(peer_id, scheduling_parent)
					.expect("V1 empty segment; qed");
				if self.fetching.contains(&advertisement) {
					return Err(AdvertisementError::Duplicate);
				}
				Some(advertisement)
			},
			// By-output-head segments get no fetched/in-flight rejection: overlap is not a
			// duplicate — the fetch-time walk advances past in-flight entries and deletes
			// all-known segments.
			_ => None,
		};

		per_sp.can_keep_segment(&segment, available_slots, peer_id)?;
		if let Some(advertisement) = &maybe_advertisement {
			if !backing_allows_seconding(sender, advertisement).await {
				return Err(AdvertisementError::BlockedByBacking);
			}
		}

		per_sp.add_segment(segment, peer_id);
		self.mark_replan();
		Ok(())
	}

	/// CQ positions at `core` schedulable by an advertisement made at `scheduling_parent`.
	///
	/// We use the *leaf's* CQ rather than the SP's: the SP's original CQ predicted slots
	/// SP+1…SP+L, but `d` of those have already been filled by the blocks from SP up to
	/// and including the leaf. The leaf's CQ is what remains unconsumed.
	///
	/// Example: leaf-CQ = [A, B, C, D] (L=4), SP at depth d=2:
	///
	///   blocks:   SP ──── b₁ ──── leaf ──── s₁ ──── s₂ ──── s₃ ──── s₄
	///                  ╰─ 2 of SP's ─╯       ▲       ▲       ▲       ▲
	///                  ╰─ CQ filled  ─╯      A       B       C       D
	///                                        ╰── usable ──╯╰── trimmed ──╯
	///
	/// `s₃, s₄` would land after SP exits view (its lifetime is bounded by L), so we
	/// keep [A, B] = leaf-CQ[0 .. L-d).
	///
	/// Across forks the same SP sits under multiple leaves with different `d` and
	/// prefix-nested windows, so longest = union.
	fn our_window(&self, scheduling_parent: &Hash, core: CoreIndex) -> Vec<ParaId> {
		self.implicit_view
			.paths_via_relay_parent(scheduling_parent)
			.into_iter()
			.filter_map(|path| {
				let leaf = path.last()?;
				let depth = path
					.iter()
					.rev()
					.position(|h| h == scheduling_parent)
					.expect("paths_via_relay_parent only returns paths containing the SP; qed");
				Some(self.leaf_claim_queues.get(leaf)?.window(core, depth))
			})
			.max_by_key(Vec::len)
			.unwrap_or_default()
	}

	pub fn try_make_new_fetch_requests<
		RepQueryFn: Fn(&PeerId, &ParaId) -> Option<Score>,
		TimerFn: FnMut() -> Option<HistogramTimer>,
	>(
		&mut self,
		connected_rep_query_fn: RepQueryFn,
		max_scores: HashMap<ParaId, Score>,
		pp_known: &KnownOutputHeads,
		mut create_timer_fn: TimerFn,
	) -> (Vec<Requests>, Option<Duration>) {
		let now = self.clock.now();
		let mut requests = vec![];
		let mut maybe_min_delay = None;

		// Soft-blocker cache for this pass: output heads of fetched and in-flight
		// candidates, built lazily once per (leaf, para) from that leaf's path and kept
		// current within the pass by inserting every launched head below. Dropped at pass end.
		let mut pending: HashMap<(Hash, ParaId), HashSet<Hash>> = HashMap::new();

		// Build per-(leaf, core) capacity views once, with all current consumers already
		// allocated. Each `LeafCoreCq` is a self-contained answer to "what's still free on
		// this core's CQ at this leaf?".
		let mut leaf_core_cqs = self.build_leaf_core_cqs();

		// Fill claim queue positions for each (leaf, core), starting at the back for best
		// utilization.
		for lc_idx in 0..leaf_core_cqs.len() {
			let cq_len = leaf_core_cqs[lc_idx].cq.len();
			for idx in (0..cq_len).rev() {
				let Some(para_id) = leaf_core_cqs[lc_idx].cq[idx] else { continue };
				let leaf = leaf_core_cqs[lc_idx].leaf();
				let pending_heads: &HashSet<_> =
					pending.entry((leaf, para_id)).or_insert_with(|| {
						self.pending_output_heads(para_id, &leaf_core_cqs[lc_idx].ancestry)
					});

				let candidate_sps = leaf_core_cqs[lc_idx].sps_reaching(idx);
				let highest_rep_of_para = max_scores.get(&para_id).copied().unwrap_or_default();

				let outcome = self.pick_best_advertisement(
					now,
					para_id,
					candidate_sps,
					pending_heads,
					pp_known.get(&leaf).and_then(|per_para| per_para.get(&para_id)),
					highest_rep_of_para,
					&connected_rep_query_fn,
				);

				let advertisement = match outcome {
					PickOutcome::Fetch(adv) => adv,
					PickOutcome::Nothing => continue,
					PickOutcome::Delayed(delay) => {
						maybe_min_delay = Some(
							maybe_min_delay
								.map_or(delay, |min: Duration| std::cmp::min(min, delay)),
						);
						continue;
					},
				};

				gum::trace!(
					target: LOG_TARGET,
					peer_id = ?advertisement.peer_id,
					?para_id,
					scheduling_parent = ?advertisement.scheduling_parent,
					maybe_candidate_hash = ?advertisement.candidate_hash(),
					"Requesting collation",
				);
				let req = self.fetching.launch(&advertisement, create_timer_fn());
				requests.push(req);

				// Keep the pass-local known-set current: insert what we launch. ByHash
				// launches return None here — their output head is unknowable until the
				// response arrives (the documented transitional gap; closed at conclusion
				// when note_fetched records it).
				if let Some(oh) =
					advertisement.prospective_candidate.and_then(|pc| pc.output_head_data_hash())
				{
					for lc in leaf_core_cqs
						.iter()
						.filter(|lc| lc.ancestry.contains(&advertisement.scheduling_parent))
					{
						if let Some(heads) = pending.get_mut(&(lc.leaf(), para_id)) {
							heads.insert(oh);
						}
					}
				}

				// Reserve on _all_ reachable leaf-core views. `reserve_slot` is a no-op for views
				// whose `path` doesn't contain this SP — including cross-core views.
				for lc in leaf_core_cqs.iter_mut() {
					lc.reserve_slot(&advertisement.scheduling_parent, para_id);
				}
			}
		}

		(requests, maybe_min_delay)
	}

	/// One LeafCoreCq per (leaf, core) pair we need to reason about. After rotation a single
	/// chain may yield LeafCoreCqs under two different cores.
	///
	/// Each LeafCoreCq comes back with all current consumers (in-flight + fetched candidates
	/// whose SP lies on its chain *and* uses its core) already allocated into the CQ via
	/// greedy matching: narrowest window first, latest still-free position in window —
	/// pushing wide-window consumers to later positions so narrower SPs keep access to their
	/// (only) reachable positions.
	fn build_leaf_core_cqs(&self) -> Vec<LeafCoreCq> {
		// One LeafCoreCq per (leaf, core) pair where some tracked SP lives on `core`.
		let cores: BTreeSet<CoreIndex> =
			self.per_scheduling_parent.values().map(|p| p.core_index).collect();
		let leaves: BTreeSet<Hash> = self.implicit_view.leaves().copied().collect();

		let mut out: Vec<LeafCoreCq> = Vec::new();
		for leaf in leaves {
			for &core in &cores {
				let Some(leaf_cqs) = self.leaf_claim_queues.get(&leaf) else { continue };
				let Some(ancestry) = self.implicit_view.known_allowed_relay_parents_under(&leaf)
				else {
					continue;
				};
				let Some(mut cq) = leaf_cqs.slots(core) else { continue };
				// SPs by depth from the leaf (leaf = 0). Cross-core ancestors are masked as
				// `None` so `sps_reaching` and `reserve_slot` automatically skip them.
				let sps_by_depth: Vec<Option<Hash>> = ancestry
					.iter()
					.map(|sp_hash| {
						self.per_scheduling_parent
							.get(sp_hash)
							.is_some_and(|per_sp| per_sp.core_index == core)
							.then_some(*sp_hash)
					})
					.collect();

				// Collect consumers as `(para, valid_len)` for every same-core SP on the path.
				let mut consumers: Vec<(ParaId, usize)> = Vec::new();
				for (depth, sp_hash) in
					sps_by_depth.iter().enumerate().filter_map(|(i, x)| x.map(|h| (i, h)))
				{
					let Some(per_sp) = self.per_scheduling_parent.get(&sp_hash) else { continue };
					let valid_len = cq.len().saturating_sub(depth);
					let in_flight = self
						.fetching
						.iter()
						.filter(|adv| adv.scheduling_parent == sp_hash)
						.map(|adv| adv.para_id);
					let fetched = per_sp.fetched_collations.values().map(|info| info.para_id);
					for para in in_flight.chain(fetched) {
						consumers.push((para, valid_len));
					}
				}

				// Allocate narrowest-first, latest-position-in-window. Overflow (no free
				// position in window — typically a stale claim from a CQ change at an older
				// ancestor) is tolerated quietly.
				consumers.sort_by_key(|(_, valid_len)| *valid_len);
				for (para, valid_len) in consumers {
					if let Some(latest) =
						cq[..valid_len].iter().rposition(|slot| *slot == Some(para))
					{
						cq[latest] = None;
					}
				}

				out.push(LeafCoreCq { sps_by_depth, cq, ancestry: ancestry.to_vec() });
			}
		}
		out
	}

	pub fn remove_peer(&mut self, peer: &PeerId) {
		for per_sp in self.per_scheduling_parent.values_mut() {
			// No need to reset now the statuses of claims that were pending fetch for these
			// candidates, or even cancel the futures as the requests will soon conclude with a
			// network error.
			per_sp.remove_peer_advertisements(peer);
		}
	}

	pub fn remove_peers<'a>(&'a mut self, peers_to_remove: impl Iterator<Item = &'a PeerId>) {
		// Remove advertisements from these peers.
		for peer in peers_to_remove {
			self.remove_peer(peer)
		}
	}

	pub async fn note_fetched<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		res: CollationFetchResponse,
		maybe_collation_version: Option<CollationVersion>,
	) -> CanSecond {
		let advertisement = res.0;
		let mut reject_info = SecondingRejectionInfo::from(&advertisement);

		self.fetching.note_completed(&advertisement);
		self.mark_replan();

		// A fetch concluded: reclaim consumed segments everywhere. The flag is the
		// logical removal (consumed segments are already invisible via `live_segments`);
		// this sweep is memory reclamation only. It must run before the early returns
		// below so every conclusion path sweeps — success, failure, cancellation, and
		// SP-out-of-view alike. Segments flagged by a deletion-only planner pass wait
		// here until the next conclusion, which is fine: they are invisible and
		// cap-bounded meanwhile.
		for per_sp in self.per_scheduling_parent.values_mut() {
			for peer_ads in per_sp.peer_advertisements.values_mut() {
				peer_ads.sweep_consumed();
			}
		}

		let Some(per_sp) = self.per_scheduling_parent.get_mut(&advertisement.scheduling_parent)
		else {
			gum::debug!(
				target: LOG_TARGET,
				hash = ?advertisement.scheduling_parent,
				para_id = ?advertisement.para_id,
				peer_id = ?advertisement.peer_id,
				"Collation fetch concluded for scheduling parent out of view"
			);
			return CanSecond::No(None, reject_info);
		};

		let Some(collation_version) = maybe_collation_version else {
			gum::debug!(
				target: LOG_TARGET,
				?advertisement,
				"Peer may not be connected."
			);
			return CanSecond::No(None, reject_info);
		};

		match process_collation_fetch_result(
			res,
			&mut self.network_error_freq,
			&mut self.canceled_freq,
		) {
			Ok(fetched_collation) => {
				let candidate_hash = fetched_collation.candidate_receipt.hash();
				// For ByHash claims duplicates are rejected at accept; ByOutputHead duplicates are
				// possible
				per_sp.fetched_collations.insert(
					candidate_hash,
					FetchedCollationInfo {
						peer_id: advertisement.peer_id,
						para_id: advertisement.para_id,
						output_head_hash: fetched_collation
							.candidate_receipt
							.descriptor()
							.para_head(),
					},
				);

				// Now that the candidate hash is known, populate it on the rejection info so
				// V1 release paths can clean up the right entry too.
				reject_info.maybe_candidate_hash = Some(candidate_hash);
				reject_info.maybe_output_head_hash =
					Some(fetched_collation.candidate_receipt.descriptor.para_head());

				// Some initial sanity checks on the fetched collation, based on the advertisement.
				if let Err(err) = fetched_collation.ensure_matches_advertisement(&advertisement) {
					gum::warn!(
						target: LOG_TARGET,
						?advertisement,
						"Invalid fetched collation: {}",
						err
					);
					return CanSecond::No(Some(FAILED_FETCH_SLASH), reject_info);
				}

				let scheduling_session = per_sp.session_index;

				// Sanity check of the candidate receipt version.
				if let Err(err) = descriptor_version_sanity_check_with_params(
					fetched_collation.candidate_receipt.descriptor(),
					per_sp.core_index,
					scheduling_session,
					collation_version,
				) {
					gum::warn!(
						target: LOG_TARGET,
						?advertisement,
						"Failed descriptor version sanity check for fetched collation: {}",
						err
					);
					return CanSecond::No(Some(FAILED_FETCH_SLASH), reject_info);
				}

				self.can_begin_seconding(
					sender,
					scheduling_session,
					fetched_collation,
					true,
					reject_info,
				)
				.await
			},
			Err(rep_change) => CanSecond::No(rep_change, reject_info),
		}
	}

	/// Frees the slot consumed by a previously-fetched candidate. Called when seconding fails
	/// (validation rejected, blocked-on-parent gave up, etc.). After this, capacity at
	/// `scheduling_parent` for `para_id` increases by one. Returns the peer id of the fetcher
	/// if the slot was actually held.
	///
	/// `maybe_candidate_hash` is `None` only when called for an advertisement that never made
	/// it past acceptance (V1, no descriptor available) — nothing was consumed yet, so
	/// nothing to free.
	pub fn release_slot(
		&mut self,
		scheduling_parent: &Hash,
		para_id: ParaId,
		maybe_candidate_hash: Option<&CandidateHash>,
		maybe_output_head_hash: Option<Hash>,
	) -> Option<PeerId> {
		let released = maybe_candidate_hash.and_then(|candidate_hash| {
			let info = self
				.per_scheduling_parent
				.get_mut(scheduling_parent)?
				.fetched_collations
				.remove(candidate_hash);
			if info.is_none() {
				gum::debug!(
					target: LOG_TARGET,
					?scheduling_parent,
					?candidate_hash,
					?para_id,
					"Could not release slot for candidate, it wasn't fetched",
				);
			}
			info
		});

		if let Some(output_head_hash) = maybe_output_head_hash {
			// Remove any collations that were blocked on this parent.
			self.remove_blocked_collations(BlockedCollationId {
				para_id,
				parent_head_data_hash: output_head_hash,
			});
		}

		released.map(|info| {
			self.mark_replan();
			info.peer_id
		})
	}

	pub async fn note_seconded<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		scheduling_parent: &Hash,
		para_id: &ParaId,
		candidate_hash: &CandidateHash,
		output_head_hash: Hash,
	) -> (Option<PeerId>, Vec<CanSecond>) {
		let peer_id = self
			.per_scheduling_parent
			.get(scheduling_parent)
			.and_then(|per_sp| per_sp.fetched_collations.get(candidate_hash))
			.map(|info| info.peer_id);

		let Some(unblocked) = self.blocked_from_seconding.remove(&BlockedCollationId {
			para_id: *para_id,
			parent_head_data_hash: output_head_hash,
		}) else {
			return (peer_id, vec![]);
		};

		let mut unblocked_can_second = Vec::with_capacity(unblocked.len());
		for fetched_collation in unblocked {
			let reject_info = SecondingRejectionInfo {
				scheduling_parent: fetched_collation.scheduling_parent(),
				peer_id: fetched_collation.peer_id,
				para_id: fetched_collation.candidate_receipt.descriptor.para_id(),
				maybe_output_head_hash: Some(
					fetched_collation.candidate_receipt.descriptor.para_head(),
				),
				maybe_candidate_hash: Some(fetched_collation.candidate_receipt.hash()),
			};
			let Some(per_sp) =
				self.per_scheduling_parent.get(&fetched_collation.scheduling_parent())
			else {
				continue;
			};
			let can_second = self
				.can_begin_seconding(
					sender,
					per_sp.session_index,
					fetched_collation,
					false,
					reject_info,
				)
				.await;
			unblocked_can_second.push(can_second)
		}

		(peer_id, unblocked_can_second)
	}

	// Returns max delay for unknown collators and zero delay if the collator has provided at least
	// one good collation (it's score is >= INSTANT_FETCH_REP_THRESHOLD).
	fn calculate_delay(collator_score: Score, max_score_for_para: Score) -> Duration {
		if collator_score >= INSTANT_FETCH_REP_THRESHOLD || collator_score >= max_score_for_para {
			return Duration::ZERO;
		}

		MAX_FETCH_DELAY
	}

	/// Soft blockers for `para_id` on `path`: output heads already fetched or in flight at a
	/// scheduling parent on `path`.
	fn pending_output_heads(&self, para_id: ParaId, path: &[Hash]) -> HashSet<Hash> {
		let fetched =
			self.per_scheduling_parent.iter().filter(|(sp, _)| path.contains(*sp)).flat_map(
				|(_, per_scheduling_parent)| {
					per_scheduling_parent
						.fetched_collations
						.values()
						.filter(|info| info.para_id == para_id)
						.map(|info| info.output_head_hash)
				},
			);
		let in_flight = self
			.fetching
			.iter()
			.filter(|advertisement| advertisement.para_id == para_id)
			.filter(|adv| path.contains(&adv.scheduling_parent))
			.filter_map(|advertisement| {
				advertisement.prospective_candidate.and_then(|pc| pc.output_head_data_hash())
			});
		fetched.chain(in_flight).collect()
	}

	/// Walk a picked segment containing ByOutputHead entries. Mint the fetch
	/// target from the first entry that is not in fetch, fetched or already
	/// known by prospective parachains.
	fn resolve_segment(
		&self,
		item: &RankedSegment,
		para_id: ParaId,
		pending_heads: &HashSet<Hash>,
		pp_known: Option<&HashSet<Hash>>,
	) -> Resolution {
		let Some(segment) = self
			.per_scheduling_parent
			.get(&item.scheduling_parent)
			.and_then(|per_scheduling_parent| {
				per_scheduling_parent.peer_advertisements.get(&item.peer_id)
			})
			.and_then(|peer_ads| peer_ads.segments.get(&item.segment_id))
		else {
			return Resolution::Exhausted;
		};
		match segment.entries.first() {
			Some(ProspectiveCandidate::ByOutputHead { .. }) => {
				// PP-first: a PP-known head is a hard blocker even if it is
				// also in flight or fetched.
				let mut saw_soft_blocker = false;
				segment
					.entries
					.iter()
					.copied()
					.find(|entry| {
						let output_head_hash = entry
							.output_head_data_hash()
							.expect("homogenous ByOutputHead segment; qed");
						if pp_known.is_some_and(|pp| pp.contains(&output_head_hash)) {
							// Hard: prospective parachains already has this head —
							// PP-first, even if it is also in flight or fetched.
							false
						} else if pending_heads.contains(&output_head_hash) {
							// Soft: blocked by an attempt that may still fail.
							saw_soft_blocker = true;
							false
						} else {
							true
						}
					})
					.map_or(
						if saw_soft_blocker { Resolution::Waiting } else { Resolution::Exhausted },
						|entry| {
							Resolution::Launch(Advertisement {
								scheduling_parent: item.scheduling_parent,
								para_id,
								peer_id: item.peer_id,
								prospective_candidate: Some(entry),
								advertised_descriptor_version: segment.descriptor_version,
							})
						},
					)
			},
			_ => segment
				.as_advertisement(item.peer_id, item.scheduling_parent)
				.map_or(Resolution::Exhausted, Resolution::Launch),
		}
	}

	/// Spend a picked segment's entitlement
	fn consume_segment(
		&mut self,
		scheduling_parent: &Hash,
		peer_id: &PeerId,
		segment_id: SegmentId,
	) {
		if let Some(peer_advertisements) =
			self.per_scheduling_parent.get_mut(scheduling_parent).and_then(
				|per_scheduling_parent| per_scheduling_parent.peer_advertisements.get_mut(peer_id),
			) {
			peer_advertisements.consume(segment_id);
		}
	}

	/// Segments at `sp` for `para_id` that are launchable right now.
	fn eligible_segments<'a>(
		&'a self,
		scheduling_parent: Hash,
		para_id: ParaId,
	) -> impl Iterator<Item = (Instant, SegmentId, PeerId)> + 'a {
		// `Either` unifies the two iterator types into one `impl Iterator`: empty for an
		// untracked SP, the filter chain otherwise.
		let per_sp = match self.per_scheduling_parent.get(&scheduling_parent) {
			Some(p) => p,
			None => return Either::Left(std::iter::empty()),
		};

		// V1 ads have no candidate hash and are only meaningful at the block they were
		// advertised against — they require their SP to be an active leaf.
		let is_active_leaf = self.implicit_view.contains_leaf(&scheduling_parent);

		// V1 has no candidate hash to dedup by, so at most one V1 fetch may be in-flight or
		// already fetched per (sp, para). Multiple peers may hold V1 ads for the same
		// (sp, para); we must filter out *all* V1 ads for that (sp, para) once one is taken.
		let v1_blocked = per_sp.fetched_collations.values().any(|info| info.para_id == para_id) ||
			self.fetching.iter().any(|adv| {
				adv.scheduling_parent == scheduling_parent &&
					adv.para_id == para_id &&
					adv.prospective_candidate.is_none()
			});
		let fetching = &self.fetching;
		Either::Right(per_sp.peer_advertisements.iter().flat_map(move |(peer_id, peer_ads)| {
			peer_ads
				.live_segments()
				.filter(move |(_segment_index, segment)| segment.para_id == para_id)
				.filter_map(move |(idx, segment)| {
					if !matches!(
						segment.entries.first(),
						Some(ProspectiveCandidate::ByOutputHead { .. })
					) {
						let Some(advertisement) =
							segment.as_advertisement(*peer_id, scheduling_parent)
						else {
							gum::error!(
								target: LOG_TARGET,
								?scheduling_parent,
								?peer_id,
								entries = ?segment.entries,
								"Multi-entry by-hash segment; skipping",
							);
							return None;
						};
						if fetching.contains(&advertisement) {
							return None;
						}
						let launchable = match advertisement.prospective_candidate {
							None => is_active_leaf && !v1_blocked,
							Some(prospective_candidate) => prospective_candidate
								.candidate_hash()
								.map_or(true, |candidate_hash| {
									!per_sp.fetched_collations.contains_key(&candidate_hash)
								}),
						};
						if !launchable {
							return None;
						}
					}
					Some((segment.received_at, idx, *peer_id))
				})
		}))
	}

	fn rank_segments<RepQueryFn: Fn(&PeerId, &ParaId) -> Option<Score>>(
		&self,
		para_id: ParaId,
		candidate_sps: impl Iterator<Item = Hash>,
		connected_rep_query_fn: &RepQueryFn,
	) -> BTreeSet<RankedSegment> {
		candidate_sps
			.filter_map(|scheduling_parent| {
				let activated_at = self.per_scheduling_parent.get(&scheduling_parent)?.activated_at;
				Some(self.eligible_segments(scheduling_parent, para_id).filter_map(
					move |(timestamp, segment_id, peer_id)| {
						Some(RankedSegment {
							score: connected_rep_query_fn(&peer_id, &para_id)?,
							timestamp,
							activated_at,
							scheduling_parent,
							peer_id,
							segment_id,
						})
					},
				))
			})
			.flatten()
			.collect()
	}

	/// Picks the best (= highest-scored, earliest, in that order) advertisement for `para_id`
	/// among `candidate_sps`, with delay arithmetic relative to each SP's activation.
	///
	/// Returns:
	/// - `PickOutcome::Fetch(adv)` if a fetchable advertisement was found,
	/// - `PickOutcome::Nothing` if no eligible segment resolved to a fetch target,
	/// - `PickOutcome::Delayed(d)` if the best-ranked segment still has remaining fetch delay
	///   relative to its scheduling parent's activation time.
	fn pick_best_advertisement<RepQueryFn: Fn(&PeerId, &ParaId) -> Option<Score>>(
		&mut self,
		now: Instant,
		para_id: ParaId,
		candidate_sps: impl Iterator<Item = Hash>,
		pending_heads: &HashSet<Hash>,
		pp_known: Option<&HashSet<Hash>>,
		highest_rep_of_para: Score,
		connected_rep_query_fn: &RepQueryFn,
	) -> PickOutcome {
		let ranked = self.rank_segments(para_id, candidate_sps, connected_rep_query_fn);
		// `Ord` is custom: descending by score, so first = best.
		for item in ranked {
			let delay = Self::calculate_delay(item.score, highest_rep_of_para);
			// Delay is relative to the chosen SP's activation, not advertisement arrival — once
			// the SP has been active long enough, even unknown peers' delays elapse and we fetch
			// immediately.
			let elapsed = now.duration_since(item.activated_at);
			let remaining = delay.saturating_sub(elapsed);
			if !remaining.is_zero() {
				gum::trace!(
					target: LOG_TARGET,
					peer_id = ?item.peer_id,
					scheduling_parent = ?item.scheduling_parent,
					?para_id,
					?remaining,
					"Best advertisement is fetch-delayed; will fetch once the delay elapses",
				);
				return PickOutcome::Delayed(remaining);
			}
			gum::debug!(
				target: LOG_TARGET,
				peer_id = ?item.peer_id,
				scheduling_parent = ?item.scheduling_parent,
				para_id = ?para_id,
				?elapsed,
				?delay,
				"Delay elapsed; picking fetch target from the winning segment."
			);
			match self.resolve_segment(&item, para_id, pending_heads, pp_known) {
				Resolution::Launch(fetch_target) => {
					self.consume_segment(&item.scheduling_parent, &item.peer_id, item.segment_id);
					return PickOutcome::Fetch(fetch_target);
				},
				Resolution::Exhausted => {
					self.consume_segment(&item.scheduling_parent, &item.peer_id, item.segment_id);
					continue;
				},
				Resolution::Waiting => {
					gum::trace!(
							target: LOG_TARGET,
							peer_id = ?item.peer_id,
							scheduling_parent = ?item.scheduling_parent,
							?para_id,
							"Picked segment blocked only by pending attempts; holding it",
					);
					continue;
				},
			}
		}
		gum::trace!(
				target: LOG_TARGET,
				?para_id,
				"No fetchable advertisement for a free claim-queue slot",
		);
		PickOutcome::Nothing
	}

	async fn get_our_core<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		parent: &Hash,
		session_index: SessionIndex,
	) -> Result<CoreIndex> {
		let block_number = self
			.implicit_view
			.block_number(parent)
			.ok_or_else(|| Error::BlockNumberNotFoundInImplicitView(*parent))?;
		let session_info = self.get_session_info(sender, parent, session_index).await?;

		Ok(match session_info.our_group {
			Some(group) => {
				let mut rotation_info = session_info.group_rotation_info.clone();
				// The `validator_groups` runtime API adds 1 to the block number, so we need to do
				// the same here.
				rotation_info.now = block_number + 1;
				rotation_info.core_for_group(group, session_info.n_cores)
			},
			None => {
				gum::trace!(target: LOG_TARGET, ?parent, "Not a validator");
				Default::default()
			},
		})
	}

	async fn get_session_info<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		parent: &Hash,
		index: SessionIndex,
	) -> Result<&PerSessionInfo> {
		if self.per_session.get(&index).is_none() {
			let validators = recv_runtime(request_validators(*parent, sender).await).await?;
			let (groups, group_rotation_info) =
				recv_runtime(request_validator_groups(*parent, sender).await).await?;

			let our_group =
				polkadot_node_subsystem_util::signing_key_and_index(&validators, &self.keystore)
					.and_then(|(_, index)| {
						polkadot_node_subsystem_util::find_validator_group(&groups, index)
					});

			self.per_session.insert(
				index,
				PerSessionInfo { our_group, n_cores: groups.len(), group_rotation_info },
			);
		}

		Ok(self.per_session.get(&index).expect("Just inserted"))
	}

	async fn can_begin_seconding<Sender: CollatorProtocolSenderTrait>(
		&mut self,
		sender: &mut Sender,
		scheduling_session: SessionIndex,
		fetched_collation: FetchedCollation,
		queue_blocked_collations: bool,
		reject_info: SecondingRejectionInfo,
	) -> CanSecond {
		let scheduling_parent = fetched_collation.scheduling_parent();
		let candidate_hash = fetched_collation.candidate_receipt.hash();
		let para_id = fetched_collation.candidate_receipt.descriptor.para_id();

		match fetch_pvd(
			sender,
			&fetched_collation.candidate_receipt,
			scheduling_session,
			fetched_collation.maybe_parent_head_data_hash,
			fetched_collation.maybe_parent_head_data.clone(),
		)
		.await
		{
			Ok(pvd) => {
				CanSecond::Yes(fetched_collation.candidate_receipt, fetched_collation.pov, pvd)
			},
			Err(SecondingError::BlockedOnParent(parent)) => {
				gum::debug!(
					target: LOG_TARGET,
					?candidate_hash,
					?scheduling_parent,
					?para_id,
					"Collation with parent head data hash {} is blocked from seconding. \
					 Waiting on its parent to be validated.",
					parent,
				);

				if queue_blocked_collations {
					self.blocked_from_seconding
						.entry(BlockedCollationId { para_id, parent_head_data_hash: parent })
						.or_default()
						.push(fetched_collation);
				}

				CanSecond::BlockedOnParent(parent, reject_info)
			},
			Err(err) => {
				gum::warn!(
					target: LOG_TARGET,
					?candidate_hash,
					?scheduling_parent,
					?para_id,
					"Failed persisted validation data checks: {}",
					err,
				);

				let slash = err.is_malicious().then_some(FAILED_FETCH_SLASH);
				CanSecond::No(slash, reject_info)
			},
		}
	}

	fn remove_blocked_collations(&mut self, id: BlockedCollationId) {
		let Some(blocked) = self.blocked_from_seconding.remove(&id) else { return };
		self.mark_replan();

		for collation in blocked {
			let candidate_hash = collation.candidate_receipt.hash();
			let scheduling_parent = collation.scheduling_parent();
			gum::debug!(
				target: LOG_TARGET,
				?scheduling_parent,
				?candidate_hash,
				para_id = ?id.para_id,
				parent_head_hash = ?id.parent_head_data_hash,
				"Dropping blocked collation because its parent was released",
			);
			if let Some(per_sp) = self.per_scheduling_parent.get_mut(&scheduling_parent) {
				per_sp.fetched_collations.remove(&candidate_hash);
			}
		}
	}

	#[cfg(test)]
	pub fn advertisements(&self) -> BTreeSet<Advertisement> {
		self.per_scheduling_parent
			.iter()
			.flat_map(|(sp, per_sp)| {
				per_sp.peer_advertisements.iter().flat_map(move |(peer_id, peer_ads)| {
					peer_ads
						.live_segments()
						.filter_map(move |(_, segment)| segment.as_advertisement(*peer_id, *sp))
				})
			})
			.collect()
	}

	/// Every stored segment, as (scheduling parent, peer, entries) — the multi-entry view
	/// `advertisements()` deliberately can't provide.
	#[cfg(test)]
	pub fn segments(&self) -> BTreeSet<(Hash, PeerId, Vec<ProspectiveCandidate>)> {
		self.per_scheduling_parent
			.iter()
			.flat_map(|(sp, per_sp)| {
				per_sp.peer_advertisements.iter().flat_map(move |(peer_id, peer_ads)| {
					peer_ads
						.live_segments()
						.map(move |(_, segment)| (*sp, *peer_id, segment.entries.clone()))
				})
			})
			.collect()
	}
}

/// Fetched collation data.
#[derive(Debug, Clone)]
struct FetchedCollation {
	/// Candidate receipt.
	pub candidate_receipt: CandidateReceipt,
	/// Proof of validity.
	pub pov: PoV,
	/// Optional parachain parent head data. This is needed for elastic scaling to work.
	pub maybe_parent_head_data: Option<HeadData>,
	/// Optional parent head data hash. This is needed for async backing to work (sent by v2
	/// protocol).
	pub maybe_parent_head_data_hash: Option<Hash>,
	/// The peer that sent this collation.
	pub peer_id: PeerId,
}

impl FetchedCollation {
	pub fn new(
		candidate_receipt: CandidateReceipt,
		pov: PoV,
		maybe_parent_head_data: Option<HeadData>,
		maybe_parent_head_data_hash: Option<Hash>,
		peer_id: PeerId,
	) -> Self {
		Self {
			candidate_receipt,
			pov,
			maybe_parent_head_data,
			maybe_parent_head_data_hash,
			peer_id,
		}
	}

	pub fn scheduling_parent(&self) -> Hash {
		self.candidate_receipt.descriptor().scheduling_parent()
	}

	/// Performs a sanity check between advertised and fetched collations.
	fn ensure_matches_advertisement(
		&self,
		advertised: &Advertisement,
	) -> std::result::Result<(), SecondingError> {
		let candidate_receipt = &self.candidate_receipt;

		match advertised.prospective_candidate {
			// This implies a check on the declared para if this was a v2 advertisement
			Some(ProspectiveCandidate::ByHash { candidate_hash, .. }) => {
				if candidate_hash != candidate_receipt.hash() {
					return Err(SecondingError::CandidateHashMismatch);
				}
			},
			Some(ProspectiveCandidate::ByOutputHead { output_head_data_hash, .. }) => {
				if output_head_data_hash != candidate_receipt.descriptor().para_head() {
					return Err(SecondingError::OutputHeadHashMismatch);
				}
				if advertised.para_id != candidate_receipt.descriptor.para_id() {
					return Err(SecondingError::ParaIdMismatch);
				}
			},
			// Otherwise, do the explicit check for the para_id.
			None => {
				if advertised.para_id != candidate_receipt.descriptor.para_id() {
					return Err(SecondingError::ParaIdMismatch);
				}
			},
		}

		if advertised.scheduling_parent != candidate_receipt.descriptor.scheduling_parent() {
			return Err(SecondingError::SchedulingParentMismatch);
		}
		if let Some(advertised_version) = &advertised.advertised_descriptor_version {
			let fetched_version = candidate_receipt.descriptor().version();
			if advertised_version != &fetched_version {
				return Err(SecondingError::DescriptorVersionMismatch(
					*advertised_version,
					fetched_version,
				));
			}
		}

		Ok(())
	}
}

/// Outcome of resolving a picked segment to a fetch target.
enum Resolution {
	/// The fetch target minted from the picked segment.
	Launch(Advertisement),
	/// V4 only: no entry is launchable, but at least one blocker is soft — its
	/// output head is in flight or fetched with the verdict pending, an attempt
	/// that may still fail. Nothing is consumed: the segment stays stored as the
	/// retry channel, and the position falls through to the next-ranked segment.
	Waiting,
	/// V4 only: every entry blocked - spend the fetch
	/// entitlement without a launch, fall through to the
	/// next ranked segment
	Exhausted,
}

/// Outcome of a pick
#[derive(Debug, PartialEq)]
enum PickOutcome {
	Fetch(Advertisement),
	/// Best-ranked segment is still fetch delayed
	Delayed(Duration),
	Nothing,
}

/// A stored segment as ranked by the fetch planner. Resolution to a fetch target happens
/// post-pick, on the winner only; the ranked item carries rank keys and the segment's
/// identity, nothing else.
///
/// Ordering priority: score (descending), then timestamp (ascending), then the identity
/// tuple as tiebreaker. Higher scores come first so that `BTreeSet::first()` returns the
/// best segment.
#[derive(PartialEq, Eq)]
struct RankedSegment {
	score: Score,
	timestamp: Instant,
	/// The time at which the scheduling parent was activated
	activated_at: Instant,
	scheduling_parent: Hash,
	peer_id: PeerId,
	segment_id: SegmentId,
}

impl Ord for RankedSegment {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		other
			.score
			.cmp(&self.score) // Descending: higher score comes first
			.then_with(|| self.timestamp.cmp(&other.timestamp)) // Ascending: earlier timestamp comes first
			.then_with(|| self.scheduling_parent.cmp(&other.scheduling_parent))
			.then_with(|| self.peer_id.cmp(&other.peer_id))
			.then_with(|| self.segment_id.cmp(&other.segment_id))
	}
}

impl PartialOrd for RankedSegment {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

struct FetchedCollationInfo {
	peer_id: PeerId,
	para_id: ParaId,
	output_head_hash: Hash,
}

/// Per-(leaf, core) capacity view used by the fetch planner.
///
/// `cq[i]` is `Some(para)` if leaf-CQ position `i` is still free for `para`, or `None` if
/// already consumed (or if the runtime CQ didn't schedule a para there — see padding below).
/// The build pass allocates existing consumers into `cq` so what remains `Some` is residual
/// capacity SPs can fetch into.
///
/// `sps_by_depth[i]` is `Some(sp)` if the chain block at depth `i` from the leaf (leaf at 0)
/// is a scheduling parent on *this* core; cross-core ancestors are `None`. This implicitly
/// scopes both `sps_reaching` and `reserve_slot` to our core: cross-core SPs never appear as
/// candidates for our slots, and cross-core reservations are no-ops because the SP isn't
/// found in `sps_by_depth`.
///
/// `cq` is padded to the scheduling lookahead (`build_leaf_core_cqs`), so the SP-window
/// arithmetic (`cq.len() - depth`) is bounded by the lookahead, not the runtime CQ length
/// (which may be shorter for e.g. on-demand cores).
struct LeafCoreCq {
	sps_by_depth: Vec<Option<Hash>>,
	cq: Vec<Option<ParaId>>,
	/// `ancestry` is the leaf's allowed relay-parent ancestry
	ancestry: Vec<Hash>,
}

impl LeafCoreCq {
	/// Same-core SPs whose window includes leaf-CQ position `idx`.
	///
	/// An SP at depth `d` has a lookahead window covering leaf-CQ positions `0..lookahead - d`,
	/// so position `idx` is reachable from SPs with `d < lookahead - idx`. With `cq` padded to
	/// the lookahead, that's the first `cq.len() - idx` entries of `sps_by_depth`.
	fn sps_reaching(&self, idx: usize) -> impl Iterator<Item = Hash> + '_ {
		self.sps_by_depth.iter().take(self.cq.len() - idx).filter_map(|x| *x)
	}

	/// Mark one CQ position as consumed for `para` reachable from `sp`. Clears the latest
	/// still-free position for `para` in `sp`'s window — same rule the build pass uses for
	/// existing consumers, so newly-launched fetches and prior consumers stay consistently
	/// allocated. No-op if `sp` isn't on this chain *for this core*.
	fn reserve_slot(&mut self, sp: &Hash, para: ParaId) {
		let Some(depth) = self.sps_by_depth.iter().position(|x| x.as_ref() == Some(sp)) else {
			return;
		};
		let valid_len = self.cq.len().saturating_sub(depth);
		if let Some(latest) = self.cq[..valid_len].iter().rposition(|slot| *slot == Some(para)) {
			self.cq[latest] = None;
		}
	}

	/// The leaf this view is built on.
	fn leaf(&self) -> Hash {
		*self
			.ancestry
			.first()
			.expect("the implicit view starts the ancestry with the leaf; qed")
	}
}

struct PerSchedulingParent {
	peer_advertisements: HashMap<PeerId, PeerAdvertisements>,
	// Candidates we have successfully fetched at this scheduling parent. Kept until the
	// scheduling parent leaves view, so that:
	// - duplicate advertisements are rejected (`try_accept_advertisement`),
	// - we know who to punish for supplying an invalid collation (returned by `release_slot`),
	// - and capacity tracking knows which slots are consumed (`build_leaf_core_cqs`).
	// On rejection (validation failure, blocked-on-parent timeout, etc.) entries are removed.
	fetched_collations: HashMap<CandidateHash, FetchedCollationInfo>,
	session_index: SessionIndex,
	// The core our group is assigned to at this scheduling parent. We look this up once at
	// activation (group rotation is per-block) and keep it for the lifetime of this SP.
	core_index: CoreIndex,
	// The time at which this scheduling parent was activated. Used to calculate fetch delays
	// relative to leaf activation.
	activated_at: Instant,
}

impl PerSchedulingParent {
	fn new(session_index: SessionIndex, core_index: CoreIndex, clock: &dyn Clock) -> Self {
		Self {
			session_index,
			core_index,
			peer_advertisements: Default::default(),
			fetched_collations: Default::default(),
			activated_at: clock.now(),
		}
	}

	fn remove_peer_advertisements(&mut self, peer_id: &PeerId) {
		self.peer_advertisements.remove(peer_id);
	}

	/// Whether `segment` may be kept; Bumps the rate-limit counter (`PeerAdvertisements::total`)
	/// even on rejection — by design, so a peer can't spam past their cap with bad advertisements.
	fn can_keep_segment(
		&mut self,
		segment: &StoredSegment,
		max_assignments: usize,
		peer_id: PeerId,
	) -> std::result::Result<(), AdvertisementError> {
		// Rate-limit counter bumps even for advertisements we end up rejecting, so a peer
		// can't spam past their cap. A length-N segment counts as ONE: one entitlement.
		let peer_ads = self.peer_advertisements.entry(peer_id).or_default();
		peer_ads.total += 1;
		if peer_ads.total > max_assignments {
			return Err(AdvertisementError::PeerLimitReached);
		}
		peer_ads.check_for_duplicates(segment)?;
		Ok(())
	}

	fn add_segment(&mut self, segment: StoredSegment, peer_id: PeerId) {
		self.peer_advertisements.entry(peer_id).or_default().insert(segment);
	}

	#[cfg(test)]
	fn add_advertisement(&mut self, advertisement: Advertisement, received_at: Instant) {
		self.peer_advertisements
			.entry(advertisement.peer_id)
			.or_default()
			.insert(StoredSegment {
				descriptor_version: advertisement.advertised_descriptor_version,
				entries: advertisement.prospective_candidate.into_iter().collect(),
				received_at,
				para_id: advertisement.para_id,
				consumed: false,
			});
	}
}

/// Identifies a stored segment within one peer's map, stably across insertions, sweeps and
/// any other mutation of that map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SegmentId(u64);

#[derive(Default)]
struct PeerAdvertisements {
	/// Stored segments keyed by id. Ids are handed out in increasing order, so iteration is
	/// in arrival order.
	segments: BTreeMap<SegmentId, StoredSegment>,
	/// Source of `SegmentId`s. Monotonic per peer, never reused.
	next_segment_id: u64,
	// We increment this even for advertisements that we don't end up accepting, so that we take
	// these into account when rate limiting.
	total: usize,
}

impl PeerAdvertisements {
	fn live_segments(&self) -> impl Iterator<Item = (SegmentId, &StoredSegment)> {
		self.segments
			.iter()
			.filter(|(_, segment)| !segment.consumed)
			.map(|(id, s)| (*id, s))
	}

	/// Store `segment` under a fresh id.
	fn insert(&mut self, segment: StoredSegment) {
		let id = SegmentId(self.next_segment_id);
		self.next_segment_id += 1;
		self.segments.insert(id, segment);
	}

	/// Mark the segment `id` as consumed, if it is still stored.
	fn consume(&mut self, id: SegmentId) {
		if let Some(segment) = self.segments.get_mut(&id) {
			segment.consumed = true;
		}
	}

	fn sweep_consumed(&mut self) {
		self.segments.retain(|_, segment| !segment.consumed);
	}

	fn check_for_duplicates(
		&self,
		segment: &StoredSegment,
	) -> std::result::Result<(), AdvertisementError> {
		// Byte-dedup against currently stored segments only (consumed segments are gone, so
		// a re-advertisement after launch is accepted as a fresh entitlement).
		if self.live_segments().any(|(_, stored_segment)| {
			segment.descriptor_version == stored_segment.descriptor_version &&
				segment.entries == stored_segment.entries
		}) {
			return Err(AdvertisementError::Duplicate);
		}
		Ok(())
	}
}

struct StoredSegment {
	descriptor_version: Option<CandidateDescriptorVersion>,
	entries: Vec<ProspectiveCandidate>,
	received_at: Instant,
	/// Id of the parachain this segment is for
	para_id: ParaId,
	/// Was this segment's fetch entitlement spent?
	consumed: bool,
}

impl StoredSegment {
	fn unchecked_as_advertisement(
		&self,
		peer_id: PeerId,
		scheduling_parent: Hash,
	) -> Advertisement {
		Advertisement {
			scheduling_parent,
			para_id: self.para_id,
			peer_id,
			prospective_candidate: self.entries.last().copied(),
			advertised_descriptor_version: self.descriptor_version,
		}
	}

	/// The `Advertisement` this segment stands for — only meaningful for the
	/// single-claim shapes (V1's empty entries, V2/V3's one by-hash entry). A multi-entry
	/// segment has no single advertisement: which entry gets fetched is the planner's
	/// fetch-time decision, and the advertisement is built from the resolved entry there.
	fn as_advertisement(&self, peer_id: PeerId, scheduling_parent: Hash) -> Option<Advertisement> {
		if self.entries.len() <= 1 {
			return Some(self.unchecked_as_advertisement(peer_id, scheduling_parent));
		}

		None
	}
}

struct PerSessionInfo {
	our_group: Option<GroupIndex>,
	n_cores: usize,
	// The group rotation info changes once per session, apart from the `now` field. The caller
	// must ensure to override it with the right value.
	group_rotation_info: GroupRotationInfo,
}

// Requests backing subsystem to sanity check the advertisement.
async fn backing_allows_seconding<Sender>(
	sender: &mut Sender,
	advertisement: &Advertisement,
) -> bool
where
	Sender: CollatorProtocolSenderTrait,
{
	let (candidate_hash, parent_head_data_hash) = match advertisement.prospective_candidate {
		Some(ProspectiveCandidate::ByHash { candidate_hash, parent_head_data_hash }) => {
			(candidate_hash, parent_head_data_hash)
		},
		Some(ProspectiveCandidate::ByOutputHead { .. }) => {
			// Don't have an candidate hash.
			return true;
		},
		None => {
			// Nothing to check for v1 protocol
			return true;
		},
	};
	let request = CanSecondRequest {
		candidate_para_id: advertisement.para_id,
		candidate_scheduling_parent: advertisement.scheduling_parent,
		candidate_hash,
		parent_head_data_hash,
	};
	let (tx, rx) = oneshot::channel();
	sender.send_message(CandidateBackingMessage::CanSecond(request, tx)).await;

	rx.await.unwrap_or_else(|err| {
		gum::warn!(
			target: LOG_TARGET,
			?err,
			scheduling_parent = ?advertisement.scheduling_parent,
			para_id = ?advertisement.para_id,
			candidate_hash = ?candidate_hash,
			"CanSecond-request responder was dropped",
		);

		false
	})
}

async fn fetch_pvd<Sender: CollatorProtocolSenderTrait>(
	sender: &mut Sender,
	receipt: &CandidateReceipt,
	scheduling_session: SessionIndex,
	maybe_parent_head_data_hash: Option<Hash>,
	maybe_parent_head_data: Option<HeadData>,
) -> std::result::Result<PersistedValidationData, SecondingError> {
	let para_id = receipt.descriptor.para_id();

	let pvd = match maybe_parent_head_data_hash {
		Some(parent_head_data_hash) => {
			let maybe_pvd = request_prospective_validation_data(
				sender,
				receipt.descriptor.relay_parent(),
				receipt.descriptor.session_index().unwrap_or(scheduling_session),
				parent_head_data_hash,
				para_id,
				maybe_parent_head_data.clone(),
			)
			.await?;

			let (expected_hash, pvd) = match (maybe_pvd, &maybe_parent_head_data) {
				(Some(pvd), Some(parent_head)) => (parent_head.hash(), pvd),
				(Some(pvd), None) => (pvd.parent_head.hash(), pvd),
				(None, None) => return Err(SecondingError::BlockedOnParent(parent_head_data_hash)),
				(None, _) => return Err(SecondingError::PersistedValidationDataNotFound),
			};
			if parent_head_data_hash != expected_hash {
				return Err(SecondingError::ParentHeadDataMismatch);
			}
			pvd
		},
		None => {
			let pvd = request_persisted_validation_data(
				sender,
				receipt.descriptor.relay_parent(),
				para_id,
			)
			.await?;
			pvd.ok_or(SecondingError::PersistedValidationDataNotFound)?
		},
	};

	if pvd.hash() != receipt.descriptor.persisted_validation_data_hash() {
		return Err(SecondingError::PersistedValidationDataMismatch);
	}

	Ok(pvd)
}

fn process_collation_fetch_result(
	(advertisement, res): CollationFetchResponse,
	network_error_freq: &mut gum::Freq,
	canceled_freq: &mut gum::Freq,
) -> std::result::Result<FetchedCollation, Option<Score>> {
	match res {
		Err(CollationFetchError::Cancelled) => {
			// Was cancelled by the subsystem.
			Err(None)
		},
		Err(CollationFetchError::Request(RequestError::InvalidResponse(err))) => {
			gum::warn!(
				target: LOG_TARGET,
				?advertisement,
				err = ?err,
				"Collator provided response that could not be decoded"
			);
			Err(Some(FAILED_FETCH_SLASH))
		},
		Err(CollationFetchError::Request(err)) if err.is_timed_out() => {
			gum::debug!(
				target: LOG_TARGET,
				?advertisement,
				"Request timed out"
			);
			Err(Some(FAILED_FETCH_SLASH))
		},
		Err(CollationFetchError::Request(RequestError::NetworkError(err))) => {
			gum::warn_if_frequent!(
				freq: network_error_freq,
				max_rate: gum::Times::PerHour(100),
				target: LOG_TARGET,
				?advertisement,
				err = ?err,
				"Fetching collation failed due to network error"
			);
			Err(None)
		},
		Err(CollationFetchError::Request(RequestError::Canceled(err))) => {
			gum::warn_if_frequent!(
				freq: canceled_freq,
				max_rate: gum::Times::PerHour(100),
				target: LOG_TARGET,
				?advertisement,
				err = ?err,
				"Canceled should be handled by `is_timed_out` above - this is a bug!"
			);
			Err(Some(FAILED_FETCH_SLASH))
		},
		Ok(request_v2::CollationFetchingResponse::Collation(candidate_receipt, pov)) => {
			gum::debug!(
				target: LOG_TARGET,
				?advertisement,
				"Received collation",
			);

			Ok(FetchedCollation::new(
				candidate_receipt,
				pov,
				None,
				advertisement.prospective_candidate.map(|p| p.parent_head_data_hash()),
				advertisement.peer_id,
			))
		},
		Ok(request_v2::CollationFetchingResponse::CollationWithParentHeadData {
			receipt,
			pov,
			parent_head_data,
		}) => {
			gum::debug!(
				target: LOG_TARGET,
				?advertisement,
				"Received collation with parent head data",
			);

			Ok(FetchedCollation::new(
				receipt,
				pov,
				Some(parent_head_data),
				advertisement.prospective_candidate.map(|p| p.parent_head_data_hash()),
				advertisement.peer_id,
			))
		},
	}
}

#[cfg(test)]
mod tests {
	use crate::validator_side_experimental::common::MAX_SCORE;

	use super::*;
	use std::sync::Arc;

	#[test]
	fn calculate_delay_works() {
		let score = |val: u16| Score::new(val);

		// collator score == max score => zero delay
		assert_eq!(
			CollationManager::calculate_delay(score(MAX_SCORE), score(MAX_SCORE)),
			Duration::ZERO
		);

		// collator score >= INSTANT_FETCH_REP_THRESHOLD => zero delay
		assert_eq!(
			CollationManager::calculate_delay(INSTANT_FETCH_REP_THRESHOLD, score(MAX_SCORE)),
			Duration::ZERO
		);

		// collator score > INSTANT_FETCH_REP_THRESHOLD => zero delay
		assert_eq!(CollationManager::calculate_delay(score(100), score(MAX_SCORE)), Duration::ZERO);

		// collator score >= max_score_for_para => zero delay
		assert_eq!(CollationManager::calculate_delay(score(50), score(50)), Duration::ZERO);

		// collator score == 0 and max_score_for_para > 0 => MAX_FETCH_DELAY
		assert_eq!(CollationManager::calculate_delay(score(0), score(MAX_SCORE)), MAX_FETCH_DELAY);

		// collator score == 0 and max_score_for_para == 0 => zero delay (no one has rep yet)
		assert_eq!(CollationManager::calculate_delay(score(0), score(0)), Duration::ZERO);
	}

	#[test]
	fn ranked_segment_ordering() {
		use std::cmp::Ordering;

		let score = |val: u16| Score::new(val);
		let now = Instant::now();
		let later = now + Duration::from_secs(1);

		let scheduling_parent = Hash::random();
		let peer_1 = PeerId::random();
		let peer_2 = PeerId::random();

		let ranked = |score: Score, timestamp: Instant, peer_id: PeerId, segment_id: SegmentId| {
			RankedSegment {
				score,
				timestamp,
				activated_at: now,
				scheduling_parent,
				peer_id,
				segment_id,
			}
		};

		// Different scores - higher score comes first (is "less").
		{
			let high_score = ranked(score(100), now, peer_1, SegmentId(0));
			let low_score = ranked(score(50), now, peer_2, SegmentId(0));

			assert_eq!(high_score.cmp(&low_score), Ordering::Less);
			assert_eq!(low_score.cmp(&high_score), Ordering::Greater);
		}

		// Same score, different timestamps - earlier timestamp comes first.
		{
			let earlier = ranked(score(100), now, peer_1, SegmentId(0));
			let later_item = ranked(score(100), later, peer_2, SegmentId(0));

			assert_eq!(earlier.cmp(&later_item), Ordering::Less);
			assert_eq!(later_item.cmp(&earlier), Ordering::Greater);
		}

		// Same score, same timestamp - falls back to the identity tuple (the peer here).
		{
			let seg_1 = ranked(score(100), now, peer_1, SegmentId(0));
			let seg_2 = ranked(score(100), now, peer_2, SegmentId(0));

			let cmp_result = seg_1.cmp(&seg_2);
			assert_ne!(cmp_result, Ordering::Equal);
			assert_eq!(seg_2.cmp(&seg_1), cmp_result.reverse());
		}

		// Same peer, different segment index - still a total, deterministic order.
		{
			let seg_1 = ranked(score(100), now, peer_1, SegmentId(0));
			let seg_2 = ranked(score(100), now, peer_1, SegmentId(1));

			assert_eq!(seg_1.cmp(&seg_2), Ordering::Less);
			assert_eq!(seg_2.cmp(&seg_1), Ordering::Greater);
		}

		// Same identity, same score, same timestamp - Equal.
		{
			let seg_1 = ranked(score(100), now, peer_1, SegmentId(0));
			let seg_2 = ranked(score(100), now, peer_1, SegmentId(0));

			assert_eq!(seg_1.cmp(&seg_2), Ordering::Equal);
		}

		// BTreeSet ordering - first() returns the highest score.
		{
			let segments: BTreeSet<_> = [
				ranked(score(50), now, peer_1, SegmentId(0)),
				ranked(score(200), now, peer_2, SegmentId(0)),
				ranked(score(100), now, PeerId::random(), SegmentId(0)),
				ranked(score(150), later, PeerId::random(), SegmentId(0)),
			]
			.into_iter()
			.collect();

			assert_eq!(segments.first().unwrap().score, score(200));
		}

		// BTreeSet with same scores - first() returns the earliest timestamp.
		{
			let segments: BTreeSet<_> = [
				ranked(score(100), later, peer_1, SegmentId(0)),
				ranked(score(100), now, peer_2, SegmentId(0)),
				ranked(score(50), now, PeerId::random(), SegmentId(0)),
			]
			.into_iter()
			.collect();

			let first = segments.first().unwrap();
			assert_eq!(first.score, score(100), "First should have score 100");
			assert_eq!(first.timestamp, now, "First should have earlier timestamp");
		}
	}

	#[test]
	fn pick_best_advertisement_works() {
		let scheduling_parent = Hash::random();
		let para_id = ParaId::new(1);
		let score = |val: u16| Score::new(val);

		let now = Instant::now();
		// Timestamp far enough in the past that any delay has passed.
		let old_timestamp = now.checked_sub(MAX_FETCH_DELAY).unwrap();
		// Timestamp recent enough that delay hasn't passed.
		let recent_timestamp = now;

		let peer_a = PeerId::random();
		let peer_b = PeerId::random();
		let peer_c = PeerId::random();

		// V2 ad: fetchable from any in-view scheduling parent. V1 (`None`) is only fetchable on
		// active leaves, which would require implicit_view setup the unit test doesn't do.
		let prospective_candidate = Some(ProspectiveCandidate::ByHash {
			candidate_hash: CandidateHash(Hash::repeat_byte(0xab)),
			parent_head_data_hash: Hash::repeat_byte(0xcd),
		});
		let make_adv = |peer: PeerId| Advertisement {
			scheduling_parent,
			para_id,
			peer_id: peer,
			prospective_candidate,
			advertised_descriptor_version: None,
		};

		let new_collation_manager_instance = || CollationManager {
			implicit_view: ImplicitView::new(),
			leaf_claim_queues: HashMap::new(),
			per_scheduling_parent: HashMap::from([(
				scheduling_parent,
				PerSchedulingParent::new(0, CoreIndex(0), &*polkadot_node_clock::system_clock()),
			)]),
			blocked_from_seconding: HashMap::new(),
			per_session: LruMap::new(ByLength::new(2)),
			fetching: PendingRequests::default(),
			keystore: Arc::new(sc_keystore::LocalKeystore::in_memory()),
			leaf_scheduling_info: HashMap::default(),
			clock: polkadot_node_clock::system_clock(),
			network_error_freq: gum::Freq::new(),
			canceled_freq: gum::Freq::new(),
			replan: false,
		};

		// No advertisements - returns Left(None).
		{
			let mut collation_manager = new_collation_manager_instance();
			let get_rep = |_: &PeerId, _: &ParaId| Some(score(100));

			assert_eq!(
				collation_manager.pick_best_advertisement(
					now,
					para_id,
					std::iter::once(scheduling_parent),
					&HashSet::new(),
					None,
					score(100),
					&get_rep,
				),
				PickOutcome::Nothing
			);
		}

		// Single advertisement with delay passed - returns the advertisement.
		{
			let mut collation_manager = new_collation_manager_instance();
			let get_rep = |_: &PeerId, _: &ParaId| Some(score(100));

			collation_manager
				.per_scheduling_parent
				.get_mut(&scheduling_parent)
				.unwrap()
				.add_advertisement(make_adv(peer_a), old_timestamp);

			assert_eq!(
				collation_manager.pick_best_advertisement(
					now,
					para_id,
					std::iter::once(scheduling_parent),
					&HashSet::new(),
					None,
					score(100), // highest_rep == peer's score, so delay = 0
					&get_rep,
				),
				PickOutcome::Fetch(make_adv(peer_a))
			);
		}

		// Single advertisement with delay not passed - returns Right(delay).
		{
			let mut collation_manager = new_collation_manager_instance();
			let get_rep = |_: &PeerId, _: &ParaId| Some(score(0));

			collation_manager
				.per_scheduling_parent
				.get_mut(&scheduling_parent)
				.unwrap()
				.add_advertisement(make_adv(peer_a), recent_timestamp);

			// highest_rep = 100, peer's score = 0 (< INSTANT_FETCH_REP_THRESHOLD), so delay =
			// MAX_FETCH_DELAY
			let result = collation_manager.pick_best_advertisement(
				now,
				para_id,
				std::iter::once(scheduling_parent),
				&HashSet::new(),
				None,
				score(100),
				&get_rep,
			);

			assert_eq!(result, PickOutcome::Delayed(MAX_FETCH_DELAY));
		}

		// Multiple advertisements - picks highest score.
		{
			let mut collation_manager = new_collation_manager_instance();
			let peer_a_clone = peer_a;
			let peer_b_clone = peer_b;
			let peer_c_clone = peer_c;
			let get_rep = move |peer: &PeerId, _: &ParaId| {
				if *peer == peer_a_clone {
					Some(score(50))
				} else if *peer == peer_b_clone {
					Some(score(100))
				} else if *peer == peer_c_clone {
					Some(score(75))
				} else {
					None
				}
			};

			let per_sp =
				collation_manager.per_scheduling_parent.get_mut(&scheduling_parent).unwrap();
			per_sp.add_advertisement(make_adv(peer_a), old_timestamp);
			per_sp.add_advertisement(make_adv(peer_b), old_timestamp);
			per_sp.add_advertisement(make_adv(peer_c), old_timestamp);

			// All have old timestamps, so delay has passed. Should pick peer_b (highest score).
			assert_eq!(
				collation_manager.pick_best_advertisement(
					now,
					para_id,
					std::iter::once(scheduling_parent),
					&HashSet::new(),
					None,
					score(100),
					&get_rep,
				),
				PickOutcome::Fetch(make_adv(peer_b))
			);
		}

		// Same score - picks earlier timestamp.
		{
			let mut collation_manager = new_collation_manager_instance();
			let get_rep = |_: &PeerId, _: &ParaId| Some(score(100));

			let earlier = old_timestamp;
			let later = old_timestamp + Duration::from_secs(1);

			let per_sp =
				collation_manager.per_scheduling_parent.get_mut(&scheduling_parent).unwrap();
			per_sp.add_advertisement(make_adv(peer_a), later);
			per_sp.add_advertisement(make_adv(peer_b), earlier);

			// Same score, peer_b has earlier timestamp.
			assert_eq!(
				collation_manager.pick_best_advertisement(
					now,
					para_id,
					std::iter::once(scheduling_parent),
					&HashSet::new(),
					None,
					score(100),
					&get_rep,
				),
				PickOutcome::Fetch(make_adv(peer_b))
			);
		}

		// Unknown peer (get_rep returns None) - advertisement is filtered out.
		{
			let mut collation_manager = new_collation_manager_instance();
			let get_rep = |_: &PeerId, _: &ParaId| -> Option<Score> { None };

			collation_manager
				.per_scheduling_parent
				.get_mut(&scheduling_parent)
				.unwrap()
				.add_advertisement(make_adv(peer_a), old_timestamp);

			assert_eq!(
				collation_manager.pick_best_advertisement(
					now,
					para_id,
					std::iter::once(scheduling_parent),
					&HashSet::new(),
					None,
					score(100),
					&get_rep,
				),
				PickOutcome::Nothing
			);
		}

		// Unknown scheduling parent - returns Left(None).
		{
			let mut collation_manager = new_collation_manager_instance();
			let get_rep = |_: &PeerId, _: &ParaId| Some(score(100));
			let unknown_scheduling_parent = Hash::random();

			assert_eq!(
				collation_manager.pick_best_advertisement(
					now,
					para_id,
					std::iter::once(unknown_scheduling_parent),
					&HashSet::new(),
					None,
					score(100),
					&get_rep,
				),
				PickOutcome::Nothing
			);
		}

		// Delay passed because leaf has been active long enough, even though advertisement arrived
		// recently. Tests that the delay is relative to activation time, not advertisement
		// arrival time. When the scheduling parent (leaf) has been active longer than the full
		// delay, the remaining delay should be zero and the advertisement should be fetched
		// immediately.
		{
			let mut collation_manager = new_collation_manager_instance();
			let get_rep = |_: &PeerId, _: &ParaId| Some(score(0));

			// Set activated_at far enough in the past that any delay has elapsed.
			let per_sp =
				collation_manager.per_scheduling_parent.get_mut(&scheduling_parent).unwrap();
			per_sp.activated_at = now.checked_sub(MAX_FETCH_DELAY * 2).unwrap();

			// Advertisement arrives now (recent), but the leaf has been active long enough.
			per_sp.add_advertisement(make_adv(peer_a), recent_timestamp);

			// highest_rep = 100, peer's score = 0 (< INSTANT_FETCH_REP_THRESHOLD), so delay =
			// MAX_FETCH_DELAY. But activated_at is 2*MAX_FETCH_DELAY ago, so remaining_delay = 0.
			assert_eq!(
				collation_manager.pick_best_advertisement(
					now,
					para_id,
					std::iter::once(scheduling_parent),
					&HashSet::new(),
					None,
					score(100),
					&get_rep,
				),
				PickOutcome::Fetch(make_adv(peer_a))
			);
		}

		// Advertisement with partial delay elapsed returns remaining delay.
		{
			let mut collation_manager = new_collation_manager_instance();
			let get_rep = |_: &PeerId, _: &ParaId| Some(score(0));

			// Set activated_at so that only part of the delay has elapsed.
			// score(0) < INSTANT_FETCH_REP_THRESHOLD and < highest_rep => delay = MAX_FETCH_DELAY
			// activated_at = MAX_FETCH_DELAY / 4 ago => remaining = MAX_FETCH_DELAY * 3/4
			let per_sp =
				collation_manager.per_scheduling_parent.get_mut(&scheduling_parent).unwrap();
			per_sp.activated_at = now.checked_sub(MAX_FETCH_DELAY / 4).unwrap();

			per_sp.add_advertisement(make_adv(peer_a), recent_timestamp);

			let result = collation_manager.pick_best_advertisement(
				now,
				para_id,
				std::iter::once(scheduling_parent),
				&HashSet::new(),
				None,
				score(100),
				&get_rep,
			);

			assert_eq!(result, PickOutcome::Delayed(MAX_FETCH_DELAY / 4 * 3));
		}
	}

	fn test_collation_manager(scheduling_parent: Hash) -> CollationManager {
		CollationManager {
			implicit_view: ImplicitView::new(),
			leaf_claim_queues: HashMap::new(),
			per_scheduling_parent: HashMap::from([(
				scheduling_parent,
				PerSchedulingParent::new(0, CoreIndex(0), &*polkadot_node_clock::system_clock()),
			)]),
			blocked_from_seconding: HashMap::new(),
			per_session: LruMap::new(ByLength::new(2)),
			fetching: PendingRequests::default(),
			keystore: Arc::new(sc_keystore::LocalKeystore::in_memory()),
			leaf_scheduling_info: HashMap::default(),
			clock: polkadot_node_clock::system_clock(),
			network_error_freq: gum::Freq::new(),
			canceled_freq: gum::Freq::new(),
			replan: false,
		}
	}

	/// A ByOutputHead entry with output head `repeat_byte(byte)` and parent head
	/// `repeat_byte(byte - 1)`, so consecutive bytes form a chained segment.
	fn v4_entry(byte: u8) -> ProspectiveCandidate {
		ProspectiveCandidate::ByOutputHead {
			output_head_data_hash: Hash::repeat_byte(byte),
			parent_head_data_hash: Hash::repeat_byte(byte.wrapping_sub(1)),
		}
	}

	fn push_segment(
		manager: &mut CollationManager,
		scheduling_parent: Hash,
		peer_id: PeerId,
		para_id: ParaId,
		entries: Vec<ProspectiveCandidate>,
	) {
		manager
			.per_scheduling_parent
			.get_mut(&scheduling_parent)
			.unwrap()
			.peer_advertisements
			.entry(peer_id)
			.or_default()
			.insert(StoredSegment {
				descriptor_version: Some(CandidateDescriptorVersion::V3),
				entries,
				received_at: Instant::now(),
				para_id,
				consumed: false,
			});
	}

	/// The ticket the walk mints for `entry` — for asserting pick outcomes.
	fn v4_ticket(
		scheduling_parent: Hash,
		para_id: ParaId,
		peer_id: PeerId,
		entry: ProspectiveCandidate,
	) -> Advertisement {
		Advertisement {
			scheduling_parent,
			para_id,
			peer_id,
			prospective_candidate: Some(entry),
			advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
		}
	}

	// The walk resolves the OLDEST entry that passes the gates — never the tip — and the
	// injected known-set (gates 1-2, as the caller's pass-local cache holds them) advances it.
	#[test]
	fn walk_picks_oldest_unknown_entry() {
		let scheduling_parent = Hash::random();
		let para_id = ParaId::new(1);
		let peer_id = PeerId::random();
		let now = Instant::now();
		let get_rep = |_: &PeerId, _: &ParaId| Some(Score::new(100));
		let entries = vec![v4_entry(0xa1), v4_entry(0xa2), v4_entry(0xa3)];

		// Empty known-set: index 0 (the oldest) wins.
		{
			let mut manager = test_collation_manager(scheduling_parent);
			push_segment(&mut manager, scheduling_parent, peer_id, para_id, entries.clone());

			assert_eq!(
				manager.pick_best_advertisement(
					now,
					para_id,
					std::iter::once(scheduling_parent),
					&HashSet::new(),
					None,
					Score::new(100),
					&get_rep,
				),
				PickOutcome::Fetch(v4_ticket(scheduling_parent, para_id, peer_id, entries[0]))
			);
			// Picked ⇒ consumed: the segment is spent on the launch.
			assert!(manager.segments().is_empty());
		}

		// First entry known (fetched or in flight): the walk advances to the second.
		{
			let mut manager = test_collation_manager(scheduling_parent);
			push_segment(&mut manager, scheduling_parent, peer_id, para_id, entries.clone());
			let known: HashSet<Hash> = [Hash::repeat_byte(0xa1)].into();

			assert_eq!(
				manager.pick_best_advertisement(
					now,
					para_id,
					std::iter::once(scheduling_parent),
					&known,
					None,
					Score::new(100),
					&get_rep,
				),
				PickOutcome::Fetch(v4_ticket(scheduling_parent, para_id, peer_id, entries[1]))
			);
		}
	}

	// A segment blocked only by SOFT blockers (the injected known-set: fetched or
	// in-flight heads — attempts that may still fail) is HELD on pick: nothing is
	// consumed, the position falls through to the next-ranked segment in the same
	// call, and the held segment stays stored as the retry channel.
	#[test]
	fn soft_blocked_segment_is_held_and_falls_through() {
		let scheduling_parent = Hash::random();
		let para_id = ParaId::new(1);
		let peer_hi = PeerId::random();
		let peer_lo = PeerId::random();
		let now = Instant::now();
		let get_rep = move |peer: &PeerId, _: &ParaId| {
			if *peer == peer_hi {
				Some(Score::new(100))
			} else {
				Some(Score::new(50))
			}
		};

		let blocked_entry = v4_entry(0xb1);
		let free_entry = v4_entry(0xb5);

		let mut manager = test_collation_manager(scheduling_parent);
		push_segment(&mut manager, scheduling_parent, peer_hi, para_id, vec![blocked_entry]);
		push_segment(&mut manager, scheduling_parent, peer_lo, para_id, vec![free_entry]);
		let known: HashSet<Hash> = [Hash::repeat_byte(0xb1)].into();

		// peer_hi ranks first but resolves all-soft-blocked → held → peer_lo launches.
		assert_eq!(
			manager.pick_best_advertisement(
				now,
				para_id,
				std::iter::once(scheduling_parent),
				&known,
				None,
				Score::new(100),
				&get_rep,
			),
			PickOutcome::Fetch(v4_ticket(scheduling_parent, para_id, peer_lo, free_entry))
		);
		// The winner was spent at launch; the soft-blocked segment survives.
		assert_eq!(manager.segments(), [(scheduling_parent, peer_hi, vec![blocked_entry])].into());
	}

	// A segment whose every entry is HARD-blocked (prospective parachains knows the
	// heads) is consumed on pick and the position falls through to the next-ranked
	// segment IN THE SAME CALL.
	#[test]
	fn hard_blocked_segment_is_consumed_and_falls_through() {
		let scheduling_parent = Hash::random();
		let para_id = ParaId::new(1);
		let peer_hi = PeerId::random();
		let peer_lo = PeerId::random();
		let now = Instant::now();
		let get_rep = move |peer: &PeerId, _: &ParaId| {
			if *peer == peer_hi {
				Some(Score::new(100))
			} else {
				Some(Score::new(50))
			}
		};

		let blocked_entry = v4_entry(0xb1);
		let free_entry = v4_entry(0xb5);

		let mut manager = test_collation_manager(scheduling_parent);
		push_segment(&mut manager, scheduling_parent, peer_hi, para_id, vec![blocked_entry]);
		push_segment(&mut manager, scheduling_parent, peer_lo, para_id, vec![free_entry]);
		let pp_known: HashSet<Hash> = [Hash::repeat_byte(0xb1)].into();

		// peer_hi ranks first but resolves all-hard-blocked → consumed → peer_lo launches.
		assert_eq!(
			manager.pick_best_advertisement(
				now,
				para_id,
				std::iter::once(scheduling_parent),
				&HashSet::new(),
				Some(&pp_known),
				Score::new(100),
				&get_rep,
			),
			PickOutcome::Fetch(v4_ticket(scheduling_parent, para_id, peer_lo, free_entry))
		);
		// Both spent: the hard-blocked one by deletion, the winner by launch.
		assert!(manager.segments().is_empty());
	}

	// One hard-blocked entry does not exhaust a segment while a sibling entry is only
	// soft-blocked: any soft blocker makes the segment Waiting, so it is held.
	#[test]
	fn mixed_hard_and_soft_blocked_segment_is_held() {
		let scheduling_parent = Hash::random();
		let para_id = ParaId::new(1);
		let peer_id = PeerId::random();
		let now = Instant::now();
		let get_rep = |_: &PeerId, _: &ParaId| Some(Score::new(100));
		let entries = vec![v4_entry(0xc1), v4_entry(0xc2)];

		let mut manager = test_collation_manager(scheduling_parent);
		push_segment(&mut manager, scheduling_parent, peer_id, para_id, entries.clone());
		let pp_known: HashSet<Hash> = [Hash::repeat_byte(0xc1)].into();
		let known: HashSet<Hash> = [Hash::repeat_byte(0xc2)].into();

		assert_eq!(
			manager.pick_best_advertisement(
				now,
				para_id,
				std::iter::once(scheduling_parent),
				&known,
				Some(&pp_known),
				Score::new(100),
				&get_rep,
			),
			PickOutcome::Nothing
		);
		assert_eq!(manager.segments(), [(scheduling_parent, peer_id, entries)].into());
	}

	// Hard and soft blockers in one segment do not stop the walk: it advances past both to
	// the first free entry, whichever order the blockers come in, and the launch consumes
	// the segment.
	#[test]
	fn walk_skips_hard_and_soft_blocked_entries_to_a_free_one() {
		let scheduling_parent = Hash::random();
		let para_id = ParaId::new(1);
		let peer_id = PeerId::random();
		let now = Instant::now();
		let get_rep = |_: &PeerId, _: &ParaId| Some(Score::new(100));
		let entries = vec![v4_entry(0xe1), v4_entry(0xe2), v4_entry(0xe3)];

		// Hard, soft, free.
		{
			let mut manager = test_collation_manager(scheduling_parent);
			push_segment(&mut manager, scheduling_parent, peer_id, para_id, entries.clone());
			let pp_known: HashSet<Hash> = [Hash::repeat_byte(0xe1)].into();
			let known: HashSet<Hash> = [Hash::repeat_byte(0xe2)].into();

			assert_eq!(
				manager.pick_best_advertisement(
					now,
					para_id,
					std::iter::once(scheduling_parent),
					&known,
					Some(&pp_known),
					Score::new(100),
					&get_rep,
				),
				PickOutcome::Fetch(v4_ticket(scheduling_parent, para_id, peer_id, entries[2]))
			);
			assert!(manager.segments().is_empty());
		}

		// Soft, hard, free.
		{
			let mut manager = test_collation_manager(scheduling_parent);
			push_segment(&mut manager, scheduling_parent, peer_id, para_id, entries.clone());
			let known: HashSet<Hash> = [Hash::repeat_byte(0xe1)].into();
			let pp_known: HashSet<Hash> = [Hash::repeat_byte(0xe2)].into();

			assert_eq!(
				manager.pick_best_advertisement(
					now,
					para_id,
					std::iter::once(scheduling_parent),
					&known,
					Some(&pp_known),
					Score::new(100),
					&get_rep,
				),
				PickOutcome::Fetch(v4_ticket(scheduling_parent, para_id, peer_id, entries[2]))
			);
			assert!(manager.segments().is_empty());
		}
	}

	// PP-first classification: an entry that is BOTH soft-known (fetched/in-flight)
	// AND PP-known counts as hard — a segment of such entries is exhausted and
	// consumed, not held.
	#[test]
	fn pp_known_wins_over_soft_for_the_same_entry() {
		let scheduling_parent = Hash::random();
		let para_id = ParaId::new(1);
		let peer_id = PeerId::random();
		let now = Instant::now();
		let get_rep = |_: &PeerId, _: &ParaId| Some(Score::new(100));
		let entry = v4_entry(0xd1);

		let mut manager = test_collation_manager(scheduling_parent);
		push_segment(&mut manager, scheduling_parent, peer_id, para_id, vec![entry]);
		let pp_known: HashSet<Hash> = [Hash::repeat_byte(0xd1)].into();
		let known: HashSet<Hash> = [Hash::repeat_byte(0xd1)].into();

		assert_eq!(
			manager.pick_best_advertisement(
				now,
				para_id,
				std::iter::once(scheduling_parent),
				&known,
				Some(&pp_known),
				Score::new(100),
				&get_rep,
			),
			PickOutcome::Nothing
		);
		assert!(manager.segments().is_empty());
	}

	// Gate 3: entries whose output head prospective-parachains already knows are skipped —
	// independently of the caller's known-set (passed empty on purpose).
	#[test]
	fn walk_skips_pp_known_entries() {
		let scheduling_parent = Hash::random();
		let para_id = ParaId::new(1);
		let peer_id = PeerId::random();
		let now = Instant::now();
		let get_rep = |_: &PeerId, _: &ParaId| Some(Score::new(100));
		let entries = vec![v4_entry(0xc1), v4_entry(0xc2)];

		// First entry PP-known: the walk advances to the second.
		{
			let mut manager = test_collation_manager(scheduling_parent);
			push_segment(&mut manager, scheduling_parent, peer_id, para_id, entries.clone());
			let pp_known: HashSet<Hash> = [Hash::repeat_byte(0xc1)].into();

			assert_eq!(
				manager.pick_best_advertisement(
					now,
					para_id,
					std::iter::once(scheduling_parent),
					&HashSet::new(),
					Some(&pp_known),
					Score::new(100),
					&get_rep,
				),
				PickOutcome::Fetch(v4_ticket(scheduling_parent, para_id, peer_id, entries[1]))
			);
		}

		// Every entry PP-known: all-blocked → consumed without a launch.
		{
			let mut manager = test_collation_manager(scheduling_parent);
			push_segment(&mut manager, scheduling_parent, peer_id, para_id, entries.clone());
			let pp_known: HashSet<Hash> = [Hash::repeat_byte(0xc1), Hash::repeat_byte(0xc2)].into();

			assert_eq!(
				manager.pick_best_advertisement(
					now,
					para_id,
					std::iter::once(scheduling_parent),
					&HashSet::new(),
					Some(&pp_known),
					Score::new(100),
					&get_rep,
				),
				PickOutcome::Nothing
			);
			assert!(manager.segments().is_empty());
		}
	}

	// Path [sp_1, sp_2] yields 0xd1 (fetched at sp_1) and 0xd3 (in flight at sp_2); path [sp_1]
	// yields 0xd1 only. The other para's 0xd2 and the ByHash ticket never land.
	#[test]
	fn known_output_heads_projects_attempts_on_the_path_only() {
		let sp_1 = Hash::repeat_byte(0x01);
		let sp_2 = Hash::repeat_byte(0x02);
		let para_id = ParaId::new(1);
		let other_para = ParaId::new(2);
		let peer_id = PeerId::random();

		let mut manager = test_collation_manager(sp_1);
		manager.per_scheduling_parent.insert(
			sp_2,
			PerSchedulingParent::new(0, CoreIndex(0), &*polkadot_node_clock::system_clock()),
		);

		// Fetched at sp_1 for our para; fetched at sp_2 for ANOTHER para (must be excluded).
		manager.per_scheduling_parent.get_mut(&sp_1).unwrap().fetched_collations.insert(
			CandidateHash(Hash::repeat_byte(0x11)),
			FetchedCollationInfo { peer_id, para_id, output_head_hash: Hash::repeat_byte(0xd1) },
		);
		manager.per_scheduling_parent.get_mut(&sp_2).unwrap().fetched_collations.insert(
			CandidateHash(Hash::repeat_byte(0x12)),
			FetchedCollationInfo {
				peer_id,
				para_id: other_para,
				output_head_hash: Hash::repeat_byte(0xd2),
			},
		);

		// In flight: a ByOutputHead ticket at sp_2 (lands) and a ByHash ticket (invisible).
		let _v4_req = manager
			.fetching
			.launch(&v4_ticket(sp_2, para_id, peer_id, v4_entry(0xd3)), None);
		let _by_hash_req = manager.fetching.launch(
			&Advertisement {
				scheduling_parent: sp_1,
				para_id,
				peer_id,
				prospective_candidate: Some(ProspectiveCandidate::ByHash {
					candidate_hash: CandidateHash(Hash::repeat_byte(0x13)),
					parent_head_data_hash: Hash::repeat_byte(0xd0),
				}),
				advertised_descriptor_version: None,
			},
			None,
		);

		assert_eq!(
			manager.pending_output_heads(para_id, &[sp_1, sp_2]),
			[Hash::repeat_byte(0xd1), Hash::repeat_byte(0xd3)].into()
		);
		assert_eq!(
			manager.pending_output_heads(para_id, &[sp_1]),
			[Hash::repeat_byte(0xd1)].into()
		);
	}

	#[test]
	fn replan_flag_mutators() {
		let sp = Hash::repeat_byte(1);
		let para_id: ParaId = 100.into();
		let mut manager = test_collation_manager(sp);

		// Not set by construction; mark/take round-trip.
		assert!(!manager.take_replan());

		// release_slot marks only when an entry is actually freed.
		let candidate_hash = CandidateHash(Hash::repeat_byte(2));
		manager.release_slot(&sp, para_id, Some(&candidate_hash), None);
		assert!(!manager.take_replan());

		manager.per_scheduling_parent.get_mut(&sp).unwrap().fetched_collations.insert(
			candidate_hash,
			FetchedCollationInfo {
				peer_id: PeerId::random(),
				para_id,
				output_head_hash: Hash::repeat_byte(3),
			},
		);
		manager.release_slot(&sp, para_id, Some(&candidate_hash), None);
		assert!(manager.take_replan());

		//(blocked children, no fetched entry) must also re-arm.
		let parent_head = Hash::repeat_byte(4);
		manager
			.blocked_from_seconding
			.insert(BlockedCollationId { para_id, parent_head_data_hash: parent_head }, vec![]);
		manager.release_slot(&sp, para_id, None, Some(parent_head));
		assert!(manager.take_replan());

		manager.remove_peer(&PeerId::random());
		assert!(!manager.take_replan());
	}
}
