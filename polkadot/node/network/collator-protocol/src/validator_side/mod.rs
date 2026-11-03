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

//! # Collator Protocol - Validator Side
//!
//! This module implements the validator side of the collator protocol, handling collation
//! advertisements from collators and coordinating with the backing subsystem to validate
//! and second candidates.
//!
//! ## Advertisement Validation Pipeline
//!
//! When a collation advertisement arrives, it goes through multiple validation layers:
//!
//! ### Layer 1: Basic Validation (in `handle_advertisement`)
//! - **Peer validation:** Is the peer known and declared as a collator?
//! - **Protocol version check:** V1 advertisements only accepted for active leaves
//!
//! ### Layer 2: AssetHub Hold-off (in `handle_advertisement`)
//! - **Purpose:** Rate-limit permissionless AssetHub collators to prefer invulnerable collators
//! - **Behavior:** Non-invulnerable collators' advertisements are delayed by HOLD_OFF_DURATION
//! - **Note:** This happens BEFORE capacity checks, so held-off ads can be queued even when full
//!
//! ### Layer 3: Capacity and Position Validation (in `process_advertisement`)
//! - **Function:** `is_slot_available()`
//! - **Checks:**
//!   - Para appears in claim queue for our specific core (implicit core assignment check)
//!   - Para has non-obsolete positions (accounting for produced blocks via offset)
//!   - Para's positions are within relay_parent's lookahead window
//!   - Total capacity not exceeded by in-flight candidates
//! - **Why here:** Runs AFTER hold-off so held-off advertisements can be queued even when full
//! - **Error handling:** Returns `SecondedLimitReached` (no punishment) - fair because claim queue
//!   may have changed since collator made the advertisement. Advertisements are cheap to process.
//! - **Cost model:** Prevents fetching PoVs (up to 10 MB each) for candidates that can't be backed
//!
//! ### Layer 4: Fragment Chain Validation (in `process_advertisement`)
//! - **Function:** `can_second()` → backing subsystem → prospective-parachain
//! - **Checks:**
//!   - Para is in claim queue (scheduled)
//!   - Can candidate extend the fragment chain? (parent-child relationships, constraints)
//!   - Is relay parent in scope?
//!   - Does fragment chain have room? (count-based: total candidates ≤ unique cores para appears
//!     on)
//! - **What PP doesn't validate:**
//!   - Specific claim queue positions (position-agnostic, only cares about count)
//!   - Obsolete positions (uses time-based pruning when relay parents leave view)
//!   - Core assignment for THIS validator (builds chains for all scheduled paras/cores)
//! - **Why PP is position-agnostic:**
//!   - Fragment chains are per-para, not per-core (serves all cores)
//!   - Receives candidates from all validators via statement distribution
//!   - Core assignment validated via signature verification (validator group → core mapping)
//!   - Position-aware filtering is collator protocol's job (prevents expensive PoV fetches)
//!
//! ### Layer 5: Final Validation (in backing subsystem's `handle_second_message`)
//! - **Checks:** Core assignment, claim queue membership
//! - **Why needed:** Defense in depth, happens AFTER fetching PoV
//! - **Trade-off:** Late check means wasted fetch if invalid, but provides safety net
//!
//! ## Why Multiple Layers?
//!
//! Each layer serves a distinct purpose:
//! - **Layer 1:** Fast reject of obviously invalid advertisements (wrong para, wrong core)
//! - **Layer 2:** Rate limiting for fair access (AssetHub-specific)
//! - **Layer 3:** Pre-fetch filtering based on claim queue state (avoids expensive PoV fetches)
//! - **Layer 4:** Structural validation of candidate chain (only PP has fragment chain state)
//! - **Layer 5:** Final safety check post-fetch
//!
//!
//! ## Claim Queue Position Tracking
//!
//! ### The Leaf-Based Approach
//!
//! This implementation uses a **leaf-based** approach for claim queue validation:
//! - Store full claim queues for active leaves only (`leaf_claim_queues`)
//! - Use offset arithmetic to determine valid positions for each relay parent
//! - Stateless validation: recompute on each check using fresh leaf data
//!
//! ### Why Leaf-Based Instead of Per-Relay-Parent?
//!
//! **Previous approach (per-relay-parent):**
//! - Stored claim queue snapshot for each relay parent in implicit view
//! - Used complex `ClaimQueueState` to track position sliding (~600 lines)
//! - Bug: treated obsolete positions as available
//! - Stale: used claim queue from when relay parent was created, not current state
//!
//! **New approach (leaf-based with offset):**
//! - Store claim queue only for active leaves (minimal storage)
//! - Offset = number of blocks produced since relay parent
//! - Obsolete positions naturally skipped via offset
//! - Fresh: always uses current leaf's claim queue
//! - Simpler: no position migration on leaf transitions
//!
//! ### Offset and Position Semantics
//!
//! **Claim queue shifting:** As blocks are produced, position 0 is consumed and the queue shifts.
//! - At block N, CQ `[A, B, C]` represents: N+1→A, N+2→B, N+3→C
//! - At block N+1, CQ becomes `[B, C, X]`: position 0 consumed, new entry added
//!
//! **Offset calculation:** For relay_parent at offset O from leaf:
//! - O blocks have been produced since relay_parent
//! - These consumed the first O positions of the original claim queue
//! - Additionally, relay_parent's lookahead limits visibility
//! - Valid CQ range at leaf: `[0, lookahead - offset - 1]`
//!
//! **Example** (lookahead=3):
//! - Leaf (offset=0): sees CQ positions [0,1,2] (all 3)
//! - Parent (offset=1): sees CQ positions [0,1] (last position beyond lookahead)
//! - Grandparent (offset=2): sees CQ position [0] only
//! - Great-grandparent (offset=3): no valid positions (too far back)
//!
//! ### Capacity Accounting
//!
//! **Total capacity:** Count of para occurrences in leaf's claim queue (shared pool)
//! **Consumed:** Sum of seconded/pending/waiting candidates across ALL relay parents in path
//! **Available:** If consumed < total_capacity, can accept more
//!
//! Note: All candidates compete for the same capacity regardless of which relay parent
//! they're built on, because they all need backing slots from the same claim queue.

use bitvec::vec::BitVec;
use futures::{
	channel::oneshot, future::BoxFuture, select, stream::FuturesUnordered, FutureExt, StreamExt,
};
#[cfg(test)]
use futures_timer::Delay;
use std::{
	collections::{hash_map::Entry, HashMap, HashSet, VecDeque},
	future::Future,
	sync::Arc,
	time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

use sp_keystore::KeystorePtr;

use crate::{
	extract_leaf_scheduling_info, is_scheduling_parent_valid, LeafClaimQueues, LeafSchedulingInfo,
};
use polkadot_node_clock::Clock;
use polkadot_node_network_protocol::{
	self as net_protocol,
	peer_set::{CollationVersion, PeerSet, MAX_AUTHORITY_INCOMING_STREAMS},
	request_response::{
		outgoing::{Recipient, RequestError},
		v1 as request_v1, v2 as request_v2, OutgoingRequest, Requests,
	},
	v1 as protocol_v1, v2 as protocol_v2, v3_collation as protocol_v3,
	v4_collation::{self as protocol_v4},
	CollationProtocols, OurView, PeerId, UnifiedReputationChange as Rep, View,
};
use polkadot_node_primitives::{SignedFullStatement, Statement};
use polkadot_node_subsystem::{
	messages::{
		CanSecondRequest, CandidateBackingMessage, CollatorProtocolMessage, IfDisconnected,
		NetworkBridgeEvent, NetworkBridgeTxMessage, ParentHeadData, ProspectiveParachainsMessage,
		ProspectiveValidationDataRequest,
	},
	overseer, CollatorProtocolSenderTrait, FromOrchestra, OverseerSignal, SubsystemError,
};
use polkadot_node_subsystem_util::{
	backing_implicit_view::View as ImplicitView,
	reputation::{ReputationAggregator, REPUTATION_CHANGE_INTERVAL},
	request_session_index_for_child,
};
use polkadot_primitives::{
	CandidateDescriptorV2, CandidateDescriptorVersion, CandidateHash, CollatorId, CoreIndex, Hash,
	HeadData, Id as ParaId, OccupiedCoreAssumption, PersistedValidationData, SessionIndex,
};

use super::{modify_reputation, tick_stream, LOG_TARGET};

mod collation;
pub mod error;

pub use collation::BlockedCollationId;
use collation::{
	fetched_collation_sanity_check, CollationEvent, CollationFetchError, CollationFetchRequest,
	CollationStatus, Collations, FetchedCollation, PendingCollation, PendingCollationFetch,
	ProspectiveCandidate,
};
use error::{Error, FetchError, HoldOffError, Result, SecondingError};

const ASSET_HUB_PARA_ID: ParaId = ParaId::new(1000); // Asset Hub's para id is 1000 on both Kusama and Polkadot.

#[cfg(test)]
mod tests;

pub use crate::validator_side_metrics::Metrics;

const COST_UNEXPECTED_MESSAGE: Rep = Rep::CostMinor("An unexpected message");
/// Message could not be decoded properly.
const COST_CORRUPTED_MESSAGE: Rep = Rep::CostMinor("Message was corrupt");
/// Network errors that originated at the remote host should have same cost as timeout.
const COST_NETWORK_ERROR: Rep = Rep::CostMinor("Some network error");
const COST_INVALID_SIGNATURE: Rep = Rep::Malicious("Invalid network message signature");
const COST_REPORT_BAD: Rep = Rep::Malicious("A collator was reported by another subsystem");
const COST_WRONG_PARA: Rep = Rep::Malicious("A collator provided a collation for the wrong para");
const COST_PROTOCOL_MISUSE: Rep =
	Rep::Malicious("A collator advertising a collation for an async backing relay parent using V1");
const COST_UNNEEDED_COLLATOR: Rep = Rep::CostMinor("An unneeded collator connected");
/// Minor penalty for V3 advertisements whose scheduling parent doesn't match the
/// last finished relay chain slot. This deters spamming invalid advertisements while
/// remaining safe for honest collators that occasionally hit slot-boundary timing edges.
///
/// `CostMinor` = -100,000 reputation per event. The ban threshold is ~-1.52 billion
/// (`71% * i32::MIN`). Reputation decays toward 0 at `1/200` per second (~139s half-life).
/// An honest collator hitting this once per 6s slot reaches a steady-state of roughly
/// -3.4 million — about 450x away from the ban threshold. Only sustained rapid-fire
/// spamming (thousands of bad advertisements) can accumulate enough to trigger a disconnect.
const COST_INVALID_SCHEDULING_PARENT: Rep =
	Rep::CostMinor("V3 advertisement with invalid scheduling parent");
const BENEFIT_NOTIFY_GOOD: Rep =
	Rep::BenefitMinor("A collator was noted good by another subsystem");

/// Time after starting a collation download from a collator we will start another one from the
/// next collator even if the upload was not finished yet.
///
/// This is to protect from a single slow collator preventing collations from happening.
///
/// With a collation size of 5MB and bandwidth of 500Mbit/s (requirement for Kusama validators),
/// the transfer should be possible within 0.1 seconds. 400 milliseconds should therefore be
/// plenty, even with multiple heads and should be low enough for later collators to still be able
/// to finish on time.
///
/// There is debug logging output, so we can adjust this value based on production results.
#[cfg(not(test))]
const MAX_UNSHARED_DOWNLOAD_TIME: Duration = Duration::from_millis(400);

// How often to check all peers with activity.
#[cfg(not(test))]
const ACTIVITY_POLL: Duration = Duration::from_secs(1);

#[cfg(test)]
const MAX_UNSHARED_DOWNLOAD_TIME: Duration = Duration::from_millis(100);

#[cfg(test)]
const ACTIVITY_POLL: Duration = Duration::from_millis(10);

// How long to hold off AssetHub advertisements from permissionless collators.
#[cfg(not(test))]
const HOLD_OFF_DURATION_DEFAULT_VALUE: Duration = Duration::from_millis(300);
#[cfg(test)]
const HOLD_OFF_DURATION_DEFAULT_VALUE: Duration = Duration::from_millis(50);

#[derive(Debug)]
struct CollatingPeerState {
	collator_id: CollatorId,
	para_id: ParaId,
	/// Collations advertised by peer per relay parent.
	///
	/// V1 network protocol doesn't include candidate hash in
	/// advertisements, we store an empty set in this case to occupy
	/// a slot in map.
	advertisements: HashMap<Hash, HashSet<CandidateHash>>,
	last_active: Instant,
}

#[derive(Debug)]
enum PeerState {
	// The peer has connected at the given instant.
	Connected(Instant),
	// Peer is collating.
	Collating(CollatingPeerState),
}

#[derive(Debug)]
enum InsertAdvertisementError {
	/// Advertisement is already known.
	Duplicate,
	/// Collation relay parent is out of our view.
	OutOfOurView,
	/// No prior declare message received.
	UndeclaredCollator,
	/// A limit for announcements per peer is reached.
	PeerLimitReached,
}

#[derive(Debug)]
struct PeerData {
	view: View,
	state: PeerState,
	version: CollationVersion,
}

impl PeerData {
	/// Update the view, clearing all advertisements that are no longer in the
	/// current view.
	fn update_view<'a>(
		&mut self,
		implicit_view: &ImplicitView,
		active_leaves: impl Iterator<Item = &'a Hash> + Clone,
		new_view: View,
	) {
		let old_view = std::mem::replace(&mut self.view, new_view);
		if let PeerState::Collating(ref mut peer_state) = self.state {
			for removed in old_view.difference(&self.view) {
				// Remove relay parent advertisements if it went out of our (implicit) view.
				let keep = is_scheduling_parent_in_implicit_view(
					removed,
					implicit_view,
					active_leaves.clone(),
				);

				if !keep {
					peer_state.advertisements.remove(&removed);
				}
			}
		}
	}

	/// Prune old advertisements relative to our view.
	fn prune_old_advertisements<'a>(
		&mut self,
		implicit_view: &ImplicitView,
		active_leaves: impl Iterator<Item = &'a Hash> + Clone,
	) {
		if let PeerState::Collating(ref mut peer_state) = self.state {
			peer_state.advertisements.retain(|hash, _| {
				// Either
				// - Relay parent is an active leaf
				// - It belongs to allowed ancestry under some leaf
				// Discard otherwise.
				is_scheduling_parent_in_implicit_view(hash, implicit_view, active_leaves.clone())
			});
		}
	}

	/// Performs sanity check for an advertisement and notes it as advertised.
	///
	/// This stores the advertisement for duplicate detection and spam tracking.
	/// Note: This does NOT mean the collation will be fetched - that decision happens
	/// later in `is_slot_available()` based on claim queue capacity.
	///
	/// Advertisements are stored even if they won't be fetched (e.g., capacity full,
	/// or held off) to enable duplicate detection and per-collator spam limiting.
	fn insert_advertisement(
		&mut self,
		on_scheduling_parent: Hash,
		candidate_hash: Option<CandidateHash>,
		implicit_view: &ImplicitView,
		leaf_claim_queues: &HashMap<Hash, LeafClaimQueues>,
		clock: &dyn Clock,
	) -> std::result::Result<(CollatorId, ParaId), InsertAdvertisementError> {
		match self.state {
			PeerState::Connected(_) => Err(InsertAdvertisementError::UndeclaredCollator),
			PeerState::Collating(ref mut state) => {
				if !is_scheduling_parent_in_implicit_view(
					&on_scheduling_parent,
					implicit_view,
					leaf_claim_queues.keys(),
				) {
					return Err(InsertAdvertisementError::OutOfOurView);
				}

				if let Some(candidate_hash) = candidate_hash {
					if state
						.advertisements
						.get(&on_scheduling_parent)
						.map_or(false, |candidates| candidates.contains(&candidate_hash))
					{
						return Err(InsertAdvertisementError::Duplicate);
					}

					let candidates = state.advertisements.entry(on_scheduling_parent).or_default();

					// Spam protection: limit the stored advertisements per scheduling parent to the
					// scheduling lookahead.
					let max_ads = leaf_claim_queues
						.values()
						.next()
						.map(|l| l.scheduling_lookahead)
						.unwrap_or(0);

					if candidates.len() > max_ads {
						return Err(InsertAdvertisementError::PeerLimitReached);
					}

					gum::debug!(
						target: LOG_TARGET,
						?on_scheduling_parent,
						?candidate_hash,
						current_ads = candidates.len(),
						max_ads,
						"Tracking new advertisement from peer",
					);

					candidates.insert(candidate_hash);
				} else {
					if self.version != CollationVersion::V1 {
						gum::error!(
							target: LOG_TARGET,
							"Programming error, `candidate_hash` can not be `None` \
							 for non `V1` networking.",
						);
					}

					if state.advertisements.contains_key(&on_scheduling_parent) {
						return Err(InsertAdvertisementError::Duplicate);
					}

					state
						.advertisements
						.insert(on_scheduling_parent, HashSet::from_iter(candidate_hash));
				};

				state.last_active = clock.now();
				Ok((state.collator_id.clone(), state.para_id))
			},
		}
	}

	/// Whether a peer is collating.
	fn is_collating(&self) -> bool {
		match self.state {
			PeerState::Connected(_) => false,
			PeerState::Collating(_) => true,
		}
	}

	/// Note that a peer is now collating with the given collator and para id.
	///
	/// This will overwrite any previous call to `set_collating` and should only be called
	/// if `is_collating` is false.
	fn set_collating(&mut self, collator_id: CollatorId, para_id: ParaId, clock: &dyn Clock) {
		self.state = PeerState::Collating(CollatingPeerState {
			collator_id,
			para_id,
			advertisements: HashMap::new(),
			last_active: clock.now(),
		});
	}

	fn collator_id(&self) -> Option<&CollatorId> {
		match self.state {
			PeerState::Connected(_) => None,
			PeerState::Collating(ref state) => Some(&state.collator_id),
		}
	}

	fn collating_para(&self) -> Option<ParaId> {
		match self.state {
			PeerState::Connected(_) => None,
			PeerState::Collating(ref state) => Some(state.para_id),
		}
	}

	/// Whether the peer has advertised the given collation.
	fn has_advertised(
		&self,
		relay_parent: &Hash,
		maybe_candidate_hash: Option<CandidateHash>,
	) -> bool {
		let collating_state = match self.state {
			PeerState::Connected(_) => return false,
			PeerState::Collating(ref state) => state,
		};

		if let Some(ref candidate_hash) = maybe_candidate_hash {
			collating_state
				.advertisements
				.get(relay_parent)
				.map_or(false, |candidates| candidates.contains(candidate_hash))
		} else {
			collating_state.advertisements.contains_key(relay_parent)
		}
	}

	/// Whether the peer is now inactive according to the current instant and the eviction policy.
	fn is_inactive(&self, clock: &dyn Clock, policy: &crate::CollatorEvictionPolicy) -> bool {
		let now = clock.now();
		match self.state {
			PeerState::Connected(connected_at) => {
				now.saturating_duration_since(connected_at) >= policy.undeclared
			},
			PeerState::Collating(ref state) => {
				now.saturating_duration_since(state.last_active) >= policy.inactive_collator
			},
		}
	}
}

#[derive(Debug)]
/// Represents the result from a hold off operation.
enum HoldOffOperationOutcome {
	/// The advertisement was held off and this is the first hold off for the relay parent.
	FirstHoldOff,
	/// The advertisement was held off and this is a subsequent hold off for this relay parent.
	SubsequentHoldOff,
	/// The advertisement was not held off. The hold off period for this relay parent has already
	/// expired.
	NoHoldOff,
}

/// Represents the hold off state of a relay parent.
enum RelayParentHoldOffState {
	/// No Advertisements from non-invulnerable collators were received so far. Nothing was held
	/// off.
	NotStarted,
	/// One or more advertisements from non-invulnerable collators are being held off. Any new
	/// advertisements received in this state are added to the queue.
	HoldingOff(VecDeque<HeldOffAdvertisement>),
	/// One or more advertisements were held off. We are not holding off any new advertisements.
	Done,
}

impl RelayParentHoldOffState {
	/// If called in `HoldingOff` state - returns all held off advertisements (there should be at
	/// least one). If the function is called in a bad state - an error is returned.
	fn on_hold_off_complete(
		&mut self,
	) -> std::result::Result<VecDeque<HeldOffAdvertisement>, HoldOffError> {
		match std::mem::replace(self, RelayParentHoldOffState::Done) {
			Self::NotStarted => {
				*self = Self::NotStarted;
				Err(HoldOffError::InvalidStateNotStarted)
			},
			Self::HoldingOff(held_off) => {
				*self = RelayParentHoldOffState::Done;
				if held_off.is_empty() {
					// the queue should never be empty
					Err(HoldOffError::QueueEmpty)
				} else {
					Ok(held_off)
				}
			},
			Self::Done => {
				*self = Self::Done;
				Err(HoldOffError::InvalidStateDone)
			},
		}
	}

	/// Holds off the advertisement, if nothing was held off for this relay parent or there are
	/// other advertisements being held off. Return false if the hold off was complete for the
	/// relay parent.
	fn hold_off_if_necessary(
		&mut self,
		advertisement: HeldOffAdvertisement,
	) -> HoldOffOperationOutcome {
		match self {
			Self::NotStarted => {
				*self = Self::HoldingOff(vec![advertisement].into());
				HoldOffOperationOutcome::FirstHoldOff
			},
			Self::HoldingOff(advertisements) => {
				advertisements.push_back(advertisement);
				HoldOffOperationOutcome::SubsequentHoldOff
			},
			Self::Done => HoldOffOperationOutcome::NoHoldOff,
		}
	}
}

/// State tracked for each scheduling parent in the implicit view.
struct PerSchedulingParent {
	collations: Collations,
	/// The core index assigned to this validator at this scheduling parent's block height.
	/// Used to look up the relevant claim queue from the leaf.
	current_core: CoreIndex,
	session_index: SessionIndex,
	ah_held_off_advertisements: RelayParentHoldOffState,
}

/// Information about a held off advertisement
struct HeldOffAdvertisement {
	/// The relay parent of the candidate.
	relay_parent: Hash,
	/// The scheduling parent it's based on.
	scheduling_parent: Hash,
	/// The peer id of the collator that has sent the advertisement.
	peer_id: PeerId,
	/// The public key which the collator has sent us with the `Declare` message.
	collator_id: CollatorId,
	/// The prospective candidate hash and its relay parent, if available. Will be none if collator
	/// protocol v1 is used.
	prospective_candidate: Option<(CandidateHash, Hash)>,
	/// Advertised candidate descriptor version (for V3 protocol).
	/// None for V1/V2 protocols.
	advertised_descriptor_version: Option<CandidateDescriptorVersion>,
}

/// All state relevant for the validator side of the protocol lives here.
struct State {
	/// Clock used for all time reads. Production passes [`polkadot_node_clock::SystemClock`];
	/// tests inject a mock.
	clock: Arc<dyn Clock>,
	/// Leaves that do support asynchronous backing along with
	/// implicit ancestry. Leaves from the implicit view are present in
	/// `active_leaves`, the opposite doesn't hold true.
	///
	/// Relay-chain blocks which don't support prospective parachains are
	/// never included in the fragment chains of active leaves which do. In
	/// particular, this means that if a given relay parent belongs to implicit
	/// ancestry of some active leaf, then it does support prospective parachains.
	implicit_view: ImplicitView,

	/// Claim queues for each active leaf. Stores the full claim queue (all cores) per leaf.
	///
	/// **Why per-leaf instead of per-relay-parent:**
	/// - Authoritative: uses current runtime state, not stale snapshots
	/// - Minimal storage: only active leaves, not all relay parents in implicit view
	/// - Fresh data: always reflects current claim queue state
	/// - Offset arithmetic handles obsolete position detection automaticallny
	///
	/// **Lifecycle:**
	/// - Added: when leaf is activated in `handle_our_view_change()`
	/// - Removed: when leaf is pruned from implicit view
	/// - Updated: automatically via view changes (old leaf removed, new leaf added)
	///
	/// **Usage:**
	/// - `is_slot_available()`: validates advertisements using offset-based position checks
	/// - `unfulfilled_claim_queue_entries()`: determines fetch priority based on CQ order
	///
	/// Per-core claim queues and the runtime scheduling lookahead.
	leaf_claim_queues: HashMap<Hash, LeafClaimQueues>,

	/// Per active leaf scheduling info for V3 scheduling parent validation.
	leaf_scheduling_info: HashMap<Hash, LeafSchedulingInfo>,

	/// State tracked per scheduling parent in the implicit view.
	/// Includes collation tracking, core assignment, and hold-off state.
	/// See `PerSchedulingParent` struct for details.
	per_scheduling_parent: HashMap<Hash, PerSchedulingParent>,

	/// Active collator peers and their declared para assignments.
	/// Tracks connection state, protocol version, and advertisement history.
	peer_data: HashMap<PeerId, PeerData>,

	/// Reference count of cores we're currently assigned to across relay parents in the view.
	/// Used for: determining which cores' claim queues to check for collator disconnection.
	/// Value is count of relay parents where we're assigned to this core.
	/// When a para disappears from all our assigned cores' claim queues, we disconnect its
	/// collators.
	assigned_cores: HashMap<CoreIndex, usize>,

	/// The collations we have requested from collators.
	collation_requests: FuturesUnordered<CollationFetchRequest>,

	/// Cancellation handles for the collation fetch requests.
	collation_requests_cancel_handles: HashMap<PendingCollation, CancellationToken>,

	/// Metrics.
	metrics: Metrics,

	/// When a timer in this `FuturesUnordered` triggers, we should dequeue the next request
	/// attempt in the corresponding `collations_per_relay_parent`.
	///
	/// A triggering timer means that the fetching took too long for our taste and we should give
	/// another collator the chance to be faster (dequeue next fetch request as well).
	collation_fetch_timeouts:
		FuturesUnordered<BoxFuture<'static, (CollatorId, Option<CandidateHash>, Hash)>>,

	/// Collations that we have successfully requested from peers and waiting
	/// on validation.
	fetched_candidates: HashMap<FetchedCollation, CollationEvent>,

	/// Collations which we haven't been able to second due to their parent not being known by
	/// prospective-parachains. Mapped from the paraid and parent_head_hash to the fetched
	/// collation data. Only needed for async backing. For elastic scaling, the fetched collation
	/// must contain the full parent head data.
	blocked_from_seconding: HashMap<BlockedCollationId, Vec<PendingCollationFetch>>,

	/// Aggregated reputation change
	reputation: ReputationAggregator,

	/// List of invulnerable AssetHub collators. They are handled with priority.
	ah_invulnerables: HashSet<PeerId>,

	/// Timers for AH held off advertisements. All advertisements are grouped by relay parent. Once
	/// we get the first collation for a relay parent which should be held off we save it and start
	/// a delay timer. When the timer fires we process all advertisements.
	ah_held_off_rp_timers: FuturesUnordered<BoxFuture<'static, Hash>>,

	/// For how long to hold off AssetHub collations from non-invulnerable collators
	hold_off_duration: Duration,
}

impl State {
	fn new(
		clock: Arc<dyn Clock>,
		metrics: Metrics,
		reputation: ReputationAggregator,
		ah_invulnerables: HashSet<PeerId>,
		hold_off_duration: Duration,
	) -> Self {
		Self {
			clock,
			implicit_view: Default::default(),
			leaf_claim_queues: Default::default(),
			leaf_scheduling_info: Default::default(),
			per_scheduling_parent: Default::default(),
			peer_data: Default::default(),
			assigned_cores: Default::default(),
			collation_requests: Default::default(),
			collation_requests_cancel_handles: Default::default(),
			metrics,
			collation_fetch_timeouts: Default::default(),
			fetched_candidates: Default::default(),
			blocked_from_seconding: Default::default(),
			reputation,
			ah_invulnerables,
			ah_held_off_rp_timers: Default::default(),
			hold_off_duration,
		}
	}

	// Returns the number of seconded and pending collations for a specific `ParaId`. Pending
	// collations are:
	// 1. Collations being fetched from a collator.
	// 2. Collations waiting for validation from backing subsystem.
	// 3. Collations blocked from seconding due to parent not being known by backing subsystem.
	fn seconded_and_pending_for_para(&self, scheduling_parent: &Hash, para_id: &ParaId) -> usize {
		let seconded = self
			.per_scheduling_parent
			.get(scheduling_parent)
			.map_or(0, |per_relay_parent| per_relay_parent.collations.seconded_for_para(para_id));

		let pending_fetch =
			self.per_scheduling_parent
				.get(scheduling_parent)
				.map_or(0, |sp_state| match sp_state.collations.status {
					CollationStatus::Fetching(pending_para_id) if pending_para_id == *para_id => 1,
					_ => 0,
				});

		let waiting_for_validation = self
			.fetched_candidates
			.keys()
			.filter(|fc| fc.scheduling_parent == *scheduling_parent && fc.para_id == *para_id)
			.count();

		let blocked_from_seconding =
			self.blocked_from_seconding.values().fold(0, |acc, blocked_collations| {
				acc + blocked_collations
					.iter()
					.filter(|pc| {
						pc.candidate_receipt.descriptor.para_id() == *para_id &&
							pc.candidate_receipt.descriptor.scheduling_parent() ==
								*scheduling_parent
					})
					.count()
			});

		let total = seconded + pending_fetch + waiting_for_validation + blocked_from_seconding;

		gum::trace!(
			target: LOG_TARGET,
			?scheduling_parent,
			?para_id,
			seconded,
			pending_fetch,
			waiting_for_validation,
			blocked_from_seconding,
			total,
			"Seconded and pending collations for para",
		);

		total
	}

	/// Returns the number of collations pending to be fetched for a `ParaId`
	fn in_waiting_queue_for_para(&self, relay_parent: &Hash, para_id: &ParaId) -> usize {
		self.per_scheduling_parent
			.get(relay_parent)
			.map_or(0, |rp_state| rp_state.collations.queued_for_para(para_id))
	}
}

fn is_scheduling_parent_in_implicit_view<'a>(
	relay_parent: &Hash,
	implicit_view: &ImplicitView,
	mut active_leaves: impl Iterator<Item = &'a Hash>,
) -> bool {
	active_leaves.any(|hash| {
		implicit_view
			.known_allowed_relay_parents_under(hash)
			.unwrap_or_default()
			.contains(relay_parent)
	})
}

async fn construct_per_scheduling_parent<Sender>(
	sender: &mut Sender,
	assigned_cores: &mut HashMap<CoreIndex, usize>,
	keystore: &KeystorePtr,
	scheduling_parent: Hash,
	session_index: SessionIndex,
) -> Result<Option<PerSchedulingParent>>
where
	Sender: CollatorProtocolSenderTrait,
{
	let validators = polkadot_node_subsystem_util::request_validators(scheduling_parent, sender)
		.await
		.await
		.map_err(Error::CancelledActiveValidators)??;

	let (groups, rotation_info) =
		polkadot_node_subsystem_util::request_validator_groups(scheduling_parent, sender)
			.await
			.await
			.map_err(Error::CancelledValidatorGroups)??;

	let core_now = if let Some(group) =
		polkadot_node_subsystem_util::signing_key_and_index(&validators, keystore).and_then(
			|(_, index)| polkadot_node_subsystem_util::find_validator_group(&groups, index),
		) {
		rotation_info.core_for_group(group, groups.len())
	} else {
		gum::trace!(target: LOG_TARGET, ?scheduling_parent, "Not a validator");
		return Ok(None);
	};

	// Update assigned_cores tracking
	let entry = assigned_cores.entry(core_now).or_default();
	*entry += 1;
	if *entry == 1 {
		gum::debug!(
			target: LOG_TARGET,
			?scheduling_parent,
			?core_now,
			"Assigned to core",
		);
	}

	let collations = Collations::new();

	Ok(Some(PerSchedulingParent {
		collations,
		current_core: core_now,
		session_index,
		ah_held_off_advertisements: RelayParentHoldOffState::NotStarted,
	}))
}

fn remove_outgoing(
	assigned_cores: &mut HashMap<CoreIndex, usize>,
	per_scheduling_parent: PerSchedulingParent,
) {
	let core = per_scheduling_parent.current_core;
	if let Entry::Occupied(mut occupied) = assigned_cores.entry(core) {
		*occupied.get_mut() -= 1;
		if *occupied.get() == 0 {
			occupied.remove_entry();
			gum::debug!(
				target: LOG_TARGET,
				?core,
				"Unassigned from core",
			);
		}
	}
}

// O(n) search for collator ID by iterating through the peers map. This should be fast enough
// unless a large amount of peers is expected.
fn collator_peer_id(
	peer_data: &HashMap<PeerId, PeerData>,
	collator_id: &CollatorId,
) -> Option<PeerId> {
	peer_data
		.iter()
		.find_map(|(peer, data)| data.collator_id().filter(|c| c == &collator_id).map(|_| *peer))
}

async fn disconnect_peer(sender: &mut impl overseer::CollatorProtocolSenderTrait, peer_id: PeerId) {
	sender
		.send_message(NetworkBridgeTxMessage::DisconnectPeers(vec![peer_id], PeerSet::Collation))
		.await
}

/// Another subsystem has requested to fetch collations on a particular leaf for some para.
async fn fetch_collation(
	sender: &mut impl overseer::CollatorProtocolSenderTrait,
	state: &mut State,
	pc: PendingCollation,
	id: CollatorId,
) -> std::result::Result<(), FetchError> {
	let PendingCollation {
		scheduling_parent: relay_parent, peer_id, prospective_candidate, ..
	} = pc;
	let candidate_hash = prospective_candidate.as_ref().map(ProspectiveCandidate::candidate_hash);

	let peer_data = state.peer_data.get(&peer_id).ok_or(FetchError::UnknownPeer)?;

	if peer_data.has_advertised(&relay_parent, candidate_hash) {
		request_collation(sender, state, pc, id.clone(), peer_data.version).await?;
		let clock = state.clock.clone();
		let timeout = |collator_id, candidate_hash, relay_parent| async move {
			clock.delay(MAX_UNSHARED_DOWNLOAD_TIME).await;
			(collator_id, candidate_hash, relay_parent)
		};
		state
			.collation_fetch_timeouts
			.push(timeout(id.clone(), candidate_hash, relay_parent).boxed());

		Ok(())
	} else {
		Err(FetchError::NotAdvertised)
	}
}

/// Report a collator for some malicious actions.
async fn report_collator(
	reputation: &mut ReputationAggregator,
	sender: &mut impl overseer::CollatorProtocolSenderTrait,
	peer_data: &HashMap<PeerId, PeerData>,
	id: CollatorId,
) {
	if let Some(peer_id) = collator_peer_id(peer_data, &id) {
		modify_reputation(reputation, sender, peer_id, COST_REPORT_BAD).await;
	}
}

/// Some other subsystem has reported a collator as a good one, bump reputation.
async fn note_good_collation(
	reputation: &mut ReputationAggregator,
	sender: &mut impl overseer::CollatorProtocolSenderTrait,
	peer_data: &HashMap<PeerId, PeerData>,
	id: CollatorId,
) {
	if let Some(peer_id) = collator_peer_id(peer_data, &id) {
		modify_reputation(reputation, sender, peer_id, BENEFIT_NOTIFY_GOOD).await;
	}
}

/// Notify a collator that its collation got seconded.
pub async fn notify_collation_seconded(
	sender: &mut impl overseer::CollatorProtocolSenderTrait,
	peer_id: PeerId,
	version: CollationVersion,
	scheduling_parent: Hash,
	statement: SignedFullStatement,
) {
	let statement = statement.into();
	let wire_message = match version {
		CollationVersion::V1 => {
			CollationProtocols::V1(protocol_v1::CollationProtocol::CollatorProtocol(
				protocol_v1::CollatorProtocolMessage::CollationSeconded(
					scheduling_parent,
					statement,
				),
			))
		},
		CollationVersion::V2 => {
			CollationProtocols::V2(protocol_v2::CollationProtocol::CollatorProtocol(
				protocol_v2::CollatorProtocolMessage::CollationSeconded(
					scheduling_parent,
					statement,
				),
			))
		},
		CollationVersion::V3 => {
			CollationProtocols::V3(protocol_v3::CollationProtocol::CollatorProtocol(
				protocol_v3::CollatorProtocolMessage::CollationSeconded(
					scheduling_parent,
					statement,
				),
			))
		},
		CollationVersion::V4 => {
			// This is not supported for v4
			return;
		},
	};
	sender
		.send_message(NetworkBridgeTxMessage::SendCollationMessage(vec![peer_id], wire_message))
		.await;
}

/// A peer's view has changed. A number of things should be done:
///  - Ongoing collation requests have to be canceled.
///  - Advertisements by this peer that are no longer relevant have to be removed.
fn handle_peer_view_change(state: &mut State, peer_id: PeerId, view: View) {
	let peer_data = match state.peer_data.get_mut(&peer_id) {
		Some(peer_data) => peer_data,
		None => return,
	};

	peer_data.update_view(&state.implicit_view, state.leaf_claim_queues.keys(), view);
	state.collation_requests_cancel_handles.retain(|pc, handle| {
		let keep = pc.peer_id != peer_id || peer_data.has_advertised(&pc.scheduling_parent, None);
		if !keep {
			handle.cancel();
		}
		keep
	});
}

/// Request a collation from the network.
/// This function will
///  - Check for duplicate requests.
///  - Check if the requested collation is in our view.
/// And as such invocations of this function may rely on that.
async fn request_collation(
	sender: &mut impl overseer::CollatorProtocolSenderTrait,
	state: &mut State,
	pending_collation: PendingCollation,
	collator_id: CollatorId,
	peer_protocol_version: CollationVersion,
) -> std::result::Result<(), FetchError> {
	if state.collation_requests_cancel_handles.contains_key(&pending_collation) {
		return Err(FetchError::AlreadyRequested);
	}

	// Borrow fields we need from pending_collation
	let scheduling_parent = pending_collation.scheduling_parent;
	let para_id = pending_collation.para_id;
	let peer_id = pending_collation.peer_id;
	let prospective_candidate = pending_collation.prospective_candidate;

	let per_scheduling_parent = state
		.per_scheduling_parent
		.get_mut(&scheduling_parent)
		.ok_or(FetchError::RelayParentOutOfView)?;

	let (requests, response_recv) = match (peer_protocol_version, prospective_candidate) {
		(CollationVersion::V1, _) => {
			let (req, response_recv) = OutgoingRequest::new(
				Recipient::Peer(peer_id),
				request_v1::CollationFetchingRequest { scheduling_parent, para_id },
			);
			let requests = Requests::CollationFetchingV1(req);
			(requests, response_recv.boxed())
		},
		(CollationVersion::V2, Some(ProspectiveCandidate { candidate_hash, .. })) |
		(CollationVersion::V3, Some(ProspectiveCandidate { candidate_hash, .. })) => {
			let (req, response_recv) = OutgoingRequest::new(
				Recipient::Peer(peer_id),
				request_v2::CollationFetchingRequest { scheduling_parent, para_id, candidate_hash },
			);
			let requests = Requests::CollationFetchingV2(req);
			(requests, response_recv.boxed())
		},
		_ => return Err(FetchError::ProtocolMismatch),
	};

	let cancellation_token = CancellationToken::new();
	let collation_request = CollationFetchRequest {
		pending_collation: pending_collation.clone(),
		collator_id: collator_id.clone(),
		collator_protocol_version: peer_protocol_version,
		from_collator: response_recv,
		cancellation_token: cancellation_token.clone(),
		_lifetime_timer: state.metrics.time_collation_request_duration(),
	};

	state.collation_requests.push(collation_request);
	state
		.collation_requests_cancel_handles
		.insert(pending_collation, cancellation_token);

	gum::debug!(
		target: LOG_TARGET,
		peer_id = %peer_id,
		%para_id,
		?scheduling_parent,
		"Requesting collation",
	);

	let maybe_candidate_hash =
		prospective_candidate.as_ref().map(ProspectiveCandidate::candidate_hash);
	per_scheduling_parent.collations.status = CollationStatus::Fetching(para_id);
	per_scheduling_parent
		.collations
		.fetching_from
		.replace((collator_id, maybe_candidate_hash));

	gum::debug!(
		target: LOG_TARGET,
		?scheduling_parent,
		%para_id,
		"Status set to Fetching",
	);

	sender
		.send_message(NetworkBridgeTxMessage::SendRequests(
			vec![requests],
			IfDisconnected::ImmediateError,
		))
		.await;
	Ok(())
}

/// Networking message has been received.
#[overseer::contextbounds(CollatorProtocol, prefix = overseer)]
async fn process_incoming_peer_message<Context>(
	ctx: &mut Context,
	state: &mut State,
	origin: PeerId,
	msg: CollationProtocols<
		protocol_v1::CollatorProtocolMessage,
		protocol_v2::CollatorProtocolMessage,
		protocol_v3::CollatorProtocolMessage,
		protocol_v4::AdvertiseSegment,
	>,
) {
	use protocol_v1::CollatorProtocolMessage as V1;
	use protocol_v2::CollatorProtocolMessage as V2;
	use protocol_v3::CollatorProtocolMessage as V3;
	use sp_runtime::traits::AppVerify;

	match msg {
		CollationProtocols::V1(V1::Declare(collator_id, para_id, signature)) |
		CollationProtocols::V2(V2::Declare(collator_id, para_id, signature)) |
		CollationProtocols::V3(V3::Declare(collator_id, para_id, signature)) => {
			if collator_peer_id(&state.peer_data, &collator_id).is_some() {
				modify_reputation(
					&mut state.reputation,
					ctx.sender(),
					origin,
					COST_UNEXPECTED_MESSAGE,
				)
				.await;
				return;
			}

			let peer_data = match state.peer_data.get_mut(&origin) {
				Some(p) => p,
				None => {
					gum::debug!(
						target: LOG_TARGET,
						peer_id = ?origin,
						?para_id,
						"Unknown peer",
					);
					modify_reputation(
						&mut state.reputation,
						ctx.sender(),
						origin,
						COST_UNEXPECTED_MESSAGE,
					)
					.await;
					return;
				},
			};

			if peer_data.is_collating() {
				gum::debug!(
					target: LOG_TARGET,
					peer_id = ?origin,
					?para_id,
					"Peer is already in the collating state",
				);
				modify_reputation(
					&mut state.reputation,
					ctx.sender(),
					origin,
					COST_UNEXPECTED_MESSAGE,
				)
				.await;
				return;
			}

			if !signature.verify(&*protocol_v1::declare_signature_payload(&origin), &collator_id) {
				gum::debug!(
					target: LOG_TARGET,
					peer_id = ?origin,
					?para_id,
					"Signature verification failure",
				);
				modify_reputation(
					&mut state.reputation,
					ctx.sender(),
					origin,
					COST_INVALID_SIGNATURE,
				)
				.await;
				return;
			}

			// Check if para appears in any of our assigned cores' claim queues
			let para_is_assigned = state.assigned_cores.keys().any(|core| {
				state.leaf_claim_queues.values().any(|lcq| {
					lcq.claim_queues.get(core).map_or(false, |paras| paras.contains(&para_id))
				})
			});

			if para_is_assigned {
				gum::debug!(
					target: LOG_TARGET,
					peer_id = ?origin,
					?collator_id,
					?para_id,
					"Declared as collator for current para",
				);

				peer_data.set_collating(collator_id, para_id, &*state.clock);
			} else {
				gum::debug!(
					target: LOG_TARGET,
					peer_id = ?origin,
					?collator_id,
					?para_id,
					"Declared as collator for unneeded para. Assigned cores: {:?}",
					&state.assigned_cores
				);

				modify_reputation(
					&mut state.reputation,
					ctx.sender(),
					origin,
					COST_UNNEEDED_COLLATOR,
				)
				.await;
				gum::trace!(target: LOG_TARGET, "Disconnecting unneeded collator");
				disconnect_peer(ctx.sender(), origin).await;
			}
		},
		CollationProtocols::V1(V1::AdvertiseCollation(relay_parent)) => {
			if let Err(err) =
				handle_advertisement(ctx.sender(), state, relay_parent, origin, None).await
			{
				gum::debug!(
					target: LOG_TARGET,
					peer_id = ?origin,
					?relay_parent,
					error = ?err,
					"Rejected v1 advertisement",
				);

				if let Some(rep) = err.reputation_changes() {
					modify_reputation(&mut state.reputation, ctx.sender(), origin, rep).await;
				}
			}
		},
		CollationProtocols::V2(V2::AdvertiseCollation {
			scheduling_parent,
			candidate_hash,
			parent_head_data_hash,
		}) => {
			if let Err(err) = handle_advertisement(
				ctx.sender(),
				state,
				scheduling_parent,
				origin,
				Some((candidate_hash, parent_head_data_hash)),
			)
			.await
			{
				gum::debug!(
					target: LOG_TARGET,
					peer_id = ?origin,
					?scheduling_parent,
					?candidate_hash,
					error = ?err,
					"Rejected v2 advertisement",
				);

				if let Some(rep) = err.reputation_changes() {
					modify_reputation(&mut state.reputation, ctx.sender(), origin, rep).await;
				}
			}
		},
		CollationProtocols::V3(protocol_v3::CollatorProtocolMessage::AdvertiseCollation {
			scheduling_parent,
			candidate_hash,
			parent_head_data_hash,
			candidate_descriptor_version,
			relay_parent,
		}) => {
			if let Err(err) = handle_advertisement_v3(
				ctx.sender(),
				state,
				scheduling_parent,
				origin,
				candidate_hash,
				parent_head_data_hash,
				candidate_descriptor_version,
				relay_parent,
			)
			.await
			{
				gum::debug!(
					target: LOG_TARGET,
					peer_id = ?origin,
					?relay_parent,
					?scheduling_parent,
					?candidate_hash,
					?candidate_descriptor_version,
					error = ?err,
					"Rejected v3 advertisement",
				);

				if let Some(rep) = err.reputation_changes() {
					modify_reputation(&mut state.reputation, ctx.sender(), origin, rep).await;
				}
			}
		},
		// Nodes that run this validator side pin their main collation protocol version to V3
		// (see `main_collation_version` in the service builder), so V4 is never negotiated
		// here — hence a log, not a panic, on this config-level guarantee.
		// `AdvertiseSegment` is the only V4 message, so this arm covers the whole
		// version rather than one variant of it.
		CollationProtocols::V4(_) => {
			gum::error!(
				target: LOG_TARGET,
				peer_id = ?origin,
				"Received a V4 segment advertisement on the non-experimental validator side",
			);
		},
		CollationProtocols::V1(V1::CollationSeconded(..)) |
		CollationProtocols::V2(V2::CollationSeconded(..)) |
		CollationProtocols::V3(V3::CollationSeconded(..)) => {
			gum::warn!(
				target: LOG_TARGET,
				peer_id = ?origin,
				"Unexpected `CollationSeconded` message, decreasing reputation",
			);

			modify_reputation(&mut state.reputation, ctx.sender(), origin, COST_UNEXPECTED_MESSAGE)
				.await;
		},
	}
}

// Holds off an AssetHub collation, if it's not from an invulnerable AssetHub collator. Returns
// `true` if the collation was held off and `false` otherwise.
fn hold_off_asset_hub_collation_if_needed(
	state: &mut State,
	peer_id: PeerId,
	collator_id: &CollatorId,
	scheduling_parent: Hash,
	prospective_candidate: Option<(CandidateHash, Hash)>,
	relay_parent: Hash,
	advertised_descriptor_version: Option<CandidateDescriptorVersion>,
) -> bool {
	// If we don't know the peer we should reject the advertisement but to avoid verbosity and
	// copy-pasted logic we'll just return `false` and let the caller handle it.
	let maybe_para_id = state.peer_data.get(&peer_id).and_then(|pd| pd.collating_para());
	let hold_off_duration = state.hold_off_duration;
	let collator_id = collator_id.clone();
	let peer_is_invulnerable = state.ah_invulnerables.contains(&peer_id);
	let invulnerables_set_is_empty = state.ah_invulnerables.is_empty();

	if maybe_para_id != Some(ASSET_HUB_PARA_ID) ||
		peer_is_invulnerable ||
		invulnerables_set_is_empty
	{
		gum::trace!(
			target: LOG_TARGET,
			?maybe_para_id,
			peer_is_invulnerable,
			invulnerables_set_is_empty,
			?peer_id,
			?scheduling_parent,
			?prospective_candidate,
			"Collation not held off",
		);

		return false;
	}

	let Some(rp_state) = state.per_scheduling_parent.get_mut(&scheduling_parent) else {
		// this should never happen
		gum::warn!(
			target: LOG_TARGET,
			?peer_id,
			?scheduling_parent,
			"Trying to hold off AssetHub collation, but the relay parent is not known",
		);
		return false;
	};

	let hold_off_outcome =
		rp_state.ah_held_off_advertisements.hold_off_if_necessary(HeldOffAdvertisement {
			relay_parent,
			scheduling_parent,
			peer_id,
			collator_id,
			prospective_candidate,
			advertised_descriptor_version,
		});

	match hold_off_outcome {
		HoldOffOperationOutcome::FirstHoldOff => {
			let clock = state.clock.clone();
			state.ah_held_off_rp_timers.push(Box::pin(async move {
				clock.delay(hold_off_duration).await;
				scheduling_parent
			}));

			gum::debug!(
				target: LOG_TARGET,
				?peer_id,
				?scheduling_parent,
				?prospective_candidate,
				"AssetHub collation held off, not from invulnerable collator. First hold off.",
			);

			true
		},
		HoldOffOperationOutcome::SubsequentHoldOff => {
			gum::debug!(
				target: LOG_TARGET,
				?peer_id,
				?scheduling_parent,
				?prospective_candidate,
				"AssetHub collation held off, not from invulnerable collator. Subsequent hold off.",
			);
			true
		},
		HoldOffOperationOutcome::NoHoldOff => {
			gum::debug!(
				target: LOG_TARGET,
				?peer_id,
				?scheduling_parent,
				?prospective_candidate,
				"AssetHub collation from non-invulnerable collator not held off - already done for this relay parent",
			);
			false
		},
	}
}

#[derive(Debug)]
enum AdvertisementError {
	/// Scheduling parent is unknown or not in our view.
	SchedulingParentUnknown,
	/// Peer is not present in the subsystem state.
	UnknownPeer,
	/// Peer has not declared its para id.
	UndeclaredCollator,
	/// Para reached a limit of seconded candidates for this scheduling parent.
	SecondedLimitReached,
	/// For V1 protocol, relay_parent must be an active leaf (no async backing support).
	ProtocolMisuse,
	/// For V3 candidate descriptors, the scheduling parent does not match any active leaf's
	/// expected scheduling parent based on its slot state.
	SchedulingParentNotValid,
	/// Advertisement is invalid.
	#[allow(dead_code)]
	Invalid(InsertAdvertisementError),
	/// Seconding not allowed by backing subsystem
	BlockedByBacking,
	/// For non-V3 descriptors, the relay parent must equal the scheduling parent.
	RelayParentMismatch,
}

impl AdvertisementError {
	fn reputation_changes(&self) -> Option<Rep> {
		use AdvertisementError::*;
		match self {
			ProtocolMisuse => Some(COST_PROTOCOL_MISUSE),
			SchedulingParentUnknown | UndeclaredCollator | Invalid(_) => {
				Some(COST_UNEXPECTED_MESSAGE)
			},
			SchedulingParentNotValid | RelayParentMismatch => Some(COST_INVALID_SCHEDULING_PARENT),
			UnknownPeer | SecondedLimitReached | BlockedByBacking => None,
		}
	}
}

// Requests backing to sanity check the advertisement.
async fn can_second<Sender>(
	sender: &mut Sender,
	candidate_para_id: ParaId,
	candidate_scheduling_parent: Hash,
	candidate_hash: CandidateHash,
	parent_head_data_hash: Hash,
) -> bool
where
	Sender: CollatorProtocolSenderTrait,
{
	let request = CanSecondRequest {
		candidate_para_id,
		candidate_scheduling_parent,
		candidate_hash,
		parent_head_data_hash,
	};
	let (tx, rx) = oneshot::channel();
	sender.send_message(CandidateBackingMessage::CanSecond(request, tx)).await;

	rx.await.unwrap_or_else(|err| {
		gum::warn!(
			target: LOG_TARGET,
			?err,
			?candidate_scheduling_parent,
			?candidate_para_id,
			?candidate_hash,
			"CanSecond-request responder was dropped",
		);

		false
	})
}

// Try seconding any collations which were waiting on the validation of their parent
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn second_unblocked_collations<Context>(
	ctx: &mut Context,
	state: &mut State,
	para_id: ParaId,
	head_data: HeadData,
	head_data_hash: Hash,
) {
	if let Some(unblocked_collations) = state
		.blocked_from_seconding
		.remove(&BlockedCollationId { para_id, parent_head_data_hash: head_data_hash })
	{
		if !unblocked_collations.is_empty() {
			gum::debug!(
				target: LOG_TARGET,
				"Candidate outputting head data with hash {} unblocked {} collations for seconding.",
				head_data_hash,
				unblocked_collations.len()
			);
		}

		for mut unblocked_collation in unblocked_collations {
			unblocked_collation.maybe_parent_head_data = Some(head_data.clone());
			let peer_id = unblocked_collation.collation_event.pending_collation.peer_id;
			let scheduling_parent =
				unblocked_collation.collation_event.pending_collation.scheduling_parent;

			if let Err(err) = kick_off_seconding(ctx, state, unblocked_collation).await {
				gum::warn!(
					target: LOG_TARGET,
					?scheduling_parent,
					?para_id,
					?peer_id,
					error = %err,
					"Seconding aborted due to an error",
				);

				if err.is_malicious() {
					// Report malicious peer.
					modify_reputation(
						&mut state.reputation,
						ctx.sender(),
						peer_id,
						COST_REPORT_BAD,
					)
					.await;
				}
			}
		}
	}
}

/// Check if a slot is available for a candidate at the given relay parent.
///
/// This function validates claim queue capacity and position availability using the
/// **leaf-based offset model**. It ensures that:
/// 1. The para appears in the claim queue for the validator's assigned core
/// 2. The para has positions within the relay_parent's valid range (accounting for offset and
///    lookahead)
/// 3. Total claim queue capacity is not exceeded by in-flight candidates
///
/// ## The Offset Model
///
/// The offset represents how many blocks have been produced since the relay_parent:
/// - `offset = 0`: relay_parent is the current leaf itself
/// - `offset = 1`: relay_parent is the parent of the leaf (1 block produced)
/// - `offset = N`: N blocks produced since relay_parent
///
/// **Valid CQ range:** At offset O, relay_parent can use positions `[0, lookahead - O - 1]`
/// - Lookahead limits how far ahead relay_parent can schedule
/// - After O blocks, only (lookahead - O) future blocks remain visible
/// - These map to CQ positions 0 to (lookahead - O - 1) at the current leaf
///
/// ## Capacity Accounting
///
/// Uses a **position-based bitfield** approach rather than simple counting:
/// - A BitVec tracks which CQ positions are occupied (by other paras or already-allocated work)
/// - For each ancestor in the path, we allocate its seconded/pending candidates to free positions
///   within that ancestor's valid range `[0, lookahead - offset)`
/// - At the relay_parent itself, we additionally try to allocate +1 for the incoming advertisement
/// - If allocation overflows at relay_parent or any newer ancestor, the path is rejected
///
/// ## Fork Handling
///
/// When there are multiple active leaves (forks), we check all paths and accept if
/// valid on any path.
///
/// ## Leaf Transitions
///
/// When a new leaf arrives, the offset automatically updates (path length increases).
/// No migration needed - validation always uses current leaf data with recalculated offset.
/// In-flight candidates are not re-validated (bounded failure if claim queue changes).
///
/// Returns Ok if there's a free slot on at least one path, Err otherwise.
fn is_slot_available(
	scheduling_parent: &Hash,
	para_id: ParaId,
	state: &State,
) -> std::result::Result<(), AdvertisementError> {
	let paths = state.implicit_view.paths_via_relay_parent(scheduling_parent);

	let per_scheduling_parent = state
		.per_scheduling_parent
		.get(scheduling_parent)
		.ok_or(AdvertisementError::SchedulingParentUnknown)?;
	let current_core = per_scheduling_parent.current_core;

	gum::trace!(
		target: LOG_TARGET,
		?scheduling_parent,
		?para_id,
		?current_core,
		?paths,
		"Checking if slot is available",
	);

	// Check if valid on at least one path (handles forks)
	'paths: for path in paths {
		// Path is ordered oldest to newest: [oldest_ancestor, ..., relay_parent, ..., leaf]
		// The leaf is the last element
		let leaf = path.last().ok_or(AdvertisementError::SchedulingParentUnknown)?;

		// Get the claim queue for our core at the leaf
		let Some(leaf_claim_queues) = state.leaf_claim_queues.get(leaf) else {
			gum::warn!(
				target: LOG_TARGET,
				?scheduling_parent,
				?leaf,
				?current_core,
				"Leaf claim queue not found, skipping path",
			);
			continue;
		};
		let Some(leaf_cq) = leaf_claim_queues.claim_queues.get(&current_core) else {
			gum::warn!(
				target: LOG_TARGET,
				?scheduling_parent,
				?leaf,
				?current_core,
				"Leaf claim queue not found for our core, skipping path",
			);
			continue;
		};

		let lookahead = leaf_claim_queues.scheduling_lookahead;

		// Mark positions occupied by other paras (not available for our para). Pad up to the
		// scheduling lookahead so every `occupied[pos]` below (pos < ancestor_valid_len ≤
		// lookahead) is in bounds without a per-ancestor guard. Padded positions are beyond the
		// runtime CQ — unscheduled, so assigned to no para — hence marked occupied: our para must
		// not be able to allocate into a slot that does not exist.
		let mut occupied = leaf_cq
			.iter()
			.map(|p| p != &para_id)
			.chain(std::iter::repeat(true))
			.take(lookahead)
			.collect::<bitvec::vec::BitVec>();

		// Allocate positions for each ancestor along the path.
		//
		// Ancestors before relay_parent (lower index = older) may have stale claims
		// from a previous claim queue — overflow is tolerated for those.
		// At relay_parent we add +1 for the incoming advertisement.
		// At relay_parent and after (newer), overflow means the incoming ad genuinely
		// can't fit — fail this path.
		let mut found_relay_parent = false;
		for (idx, ancestor) in path.iter().enumerate() {
			let ancestor_offset = path.len().saturating_sub(1 + idx);
			let ancestor_valid_len = lookahead.saturating_sub(ancestor_offset);
			let seconded_pending = state.seconded_and_pending_for_para(ancestor, &para_id);
			let waiting = state.in_waiting_queue_for_para(ancestor, &para_id);
			let is_relay_parent = ancestor == scheduling_parent;
			if is_relay_parent {
				found_relay_parent = true;
			}
			let mut to_allocate = seconded_pending + waiting + if is_relay_parent { 1 } else { 0 };

			for pos in 0..ancestor_valid_len {
				if to_allocate == 0 {
					break;
				}
				if !occupied[pos] {
					gum::trace!(target: LOG_TARGET, pos, "Marking position");
					occupied.set(pos, true);
					to_allocate -= 1;
				}
			}

			// Overflow at relay_parent or newer ancestors means the incoming
			// ad can't fit — try the next path.
			// Overflow at older ancestors is tolerated (stale claims from CQ
			// evolution between views).
			if to_allocate > 0 && found_relay_parent {
				gum::trace!(
					target: LOG_TARGET,
					?ancestor,
					?scheduling_parent,
					?para_id,
					?leaf,
					to_allocate,
					"Allocation overflow at relay parent or newer ancestor",
				);
				continue 'paths;
			}
		}
		gum::trace!(
			target: LOG_TARGET,
			?scheduling_parent,
			?para_id,
			?leaf,
			"Slot is available on this path",
		);
		return Ok(());
	}

	gum::trace!(
		target: LOG_TARGET,
		?scheduling_parent,
		?para_id,
		"No slot available on any path",
	);
	Err(AdvertisementError::SecondedLimitReached)
}

async fn handle_advertisement<Sender>(
	sender: &mut Sender,
	state: &mut State,
	scheduling_parent: Hash,
	peer_id: PeerId,
	prospective_candidate: Option<(CandidateHash, Hash)>,
) -> std::result::Result<(), AdvertisementError>
where
	Sender: CollatorProtocolSenderTrait,
{
	// Basic peer and protocol validation
	let peer_data = state.peer_data.get_mut(&peer_id).ok_or(AdvertisementError::UnknownPeer)?;

	// V1 protocol requires relay_parent to be an active leaf (no async backing support)
	if peer_data.version == CollationVersion::V1 &&
		!state.leaf_claim_queues.contains_key(&scheduling_parent)
	{
		gum::debug!(
			target: LOG_TARGET,
			?peer_id,
			?scheduling_parent,
			?prospective_candidate,
			"V1 advertisement must use an active leaf as relay parent",
		);

		return Err(AdvertisementError::ProtocolMisuse);
	}

	// Ensure peer has declared as a collator
	peer_data.collating_para().ok_or(AdvertisementError::UndeclaredCollator)?;

	// Validate the scheduling parent is known (the value itself is no longer needed here).
	state
		.per_scheduling_parent
		.get(&scheduling_parent)
		.ok_or(AdvertisementError::SchedulingParentUnknown)?;

	// Always insert advertisements that pass all the checks for spam protection.
	let candidate_hash = prospective_candidate.map(|(hash, ..)| hash);
	let (collator_id, para_id) = peer_data
		.insert_advertisement(
			scheduling_parent,
			candidate_hash,
			&state.implicit_view,
			&state.leaf_claim_queues,
			&*state.clock,
		)
		.map_err(|error| {
			gum::debug!(
				target: LOG_TARGET,
				?peer_id,
				?scheduling_parent,
				?prospective_candidate,
				?error,
				"Failed to insert advertisement",
			);

			AdvertisementError::Invalid(error)
		})?;

	if hold_off_asset_hub_collation_if_needed(
		state,
		peer_id,
		&collator_id,
		scheduling_parent,
		prospective_candidate,
		scheduling_parent, // For V1/V2, the relay parent is the same as the scheduling parent
		None,              // V1/V2 don't have advertised descriptor version
	) {
		gum::debug!(
			target: LOG_TARGET,
			?peer_id,
			?scheduling_parent,
			?prospective_candidate,
			"Collation held off, not processing advertisement further for now",
		);

		return Ok(());
	}

	process_advertisement(
		sender,
		state,
		scheduling_parent,
		scheduling_parent,
		para_id,
		peer_id,
		collator_id,
		prospective_candidate,
		None, // V1/V2 don't have advertised descriptor version
	)
	.await
}

async fn handle_advertisement_v3<Sender>(
	sender: &mut Sender,
	state: &mut State,
	scheduling_parent: Hash,
	peer_id: PeerId,
	candidate_hash: CandidateHash,
	parent_head_data_hash: Hash,
	candidate_descriptor_version: CandidateDescriptorVersion,
	relay_parent: Hash,
) -> std::result::Result<(), AdvertisementError>
where
	Sender: CollatorProtocolSenderTrait,
{
	let peer_data = state.peer_data.get_mut(&peer_id).ok_or(AdvertisementError::UnknownPeer)?;

	// For non-V3 descriptors, the relay parent must equal the scheduling parent.
	if candidate_descriptor_version != CandidateDescriptorVersion::V3 &&
		relay_parent != scheduling_parent
	{
		return Err(AdvertisementError::RelayParentMismatch);
	}

	// Fail fast if the scheduling parent is completely unknown.
	// Validate the scheduling parent is known (the value itself is no longer needed here).
	state
		.per_scheduling_parent
		.get(&scheduling_parent)
		.ok_or(AdvertisementError::SchedulingParentUnknown)?;

	// V3 candidate descriptors require the scheduling_parent to be the block from the last
	// finished relay chain slot. We compare slot numbers rather than timestamps to keep
	// the logic simple and aligned with how BABE/Aura reason about slots.
	if candidate_descriptor_version == CandidateDescriptorVersion::V3 {
		if !is_scheduling_parent_valid(
			&*state.clock,
			&scheduling_parent,
			&state.leaf_scheduling_info,
		) {
			return Err(AdvertisementError::SchedulingParentNotValid);
		}
	}

	// Ensure peer has declared as a collator
	peer_data.collating_para().ok_or(AdvertisementError::UndeclaredCollator)?;

	// Insert advertisement and check if we should hold off
	let (collator_id, para_id) = peer_data
		.insert_advertisement(
			scheduling_parent,
			Some(candidate_hash),
			&state.implicit_view,
			&state.leaf_claim_queues,
			&*state.clock,
		)
		.map_err(AdvertisementError::Invalid)?;

	if hold_off_asset_hub_collation_if_needed(
		state,
		peer_id,
		&collator_id,
		scheduling_parent,
		Some((candidate_hash, parent_head_data_hash)),
		relay_parent,
		Some(candidate_descriptor_version),
	) {
		return Ok(());
	}

	process_advertisement(
		sender,
		state,
		relay_parent,
		scheduling_parent,
		para_id,
		peer_id,
		collator_id,
		Some((candidate_hash, parent_head_data_hash)),
		Some(candidate_descriptor_version),
	)
	.await
}

async fn process_advertisement<Sender>(
	sender: &mut Sender,
	state: &mut State,
	relay_parent: Hash,
	scheduling_parent: Hash,
	para_id: ParaId,
	peer_id: PeerId,
	collator_id: CollatorId,
	prospective_candidate: Option<(CandidateHash, Hash)>,
	advertised_descriptor_version: Option<CandidateDescriptorVersion>,
) -> std::result::Result<(), AdvertisementError>
where
	Sender: CollatorProtocolSenderTrait,
{
	// Check if there's a free slot accounting for obsolete positions and capacity.
	// This happens AFTER hold-off logic (for AssetHub) has run, so held-off advertisements
	// can be queued even when capacity is temporarily full.
	is_slot_available(&scheduling_parent, para_id, state).inspect_err(|error| {
		gum::debug!(
			target: LOG_TARGET,
			?peer_id,
			?scheduling_parent,
			?para_id,
			?error,
			"Slot is not available",
		);
	})?;

	if let Some((candidate_hash, parent_head_data_hash)) = prospective_candidate {
		// Check if backing subsystem allows to second this candidate.
		//
		// This is also only important when async backing or elastic scaling is enabled.
		let can_second =
			can_second(sender, para_id, scheduling_parent, candidate_hash, parent_head_data_hash)
				.await;

		if !can_second {
			gum::debug!(
				target: LOG_TARGET,
				?peer_id,
				?scheduling_parent,
				?para_id,
				?candidate_hash,
				?parent_head_data_hash,
				"Backing subsystem does not allow seconding this candidate",
			);

			return Err(AdvertisementError::BlockedByBacking);
		}
	}

	let result = enqueue_collation(
		sender,
		state,
		relay_parent,
		scheduling_parent,
		para_id,
		peer_id,
		collator_id,
		prospective_candidate,
		advertised_descriptor_version,
	)
	.await;

	if let Err(fetch_error) = result {
		gum::debug!(
			target: LOG_TARGET,
			scheduling_parent = ?scheduling_parent,
			para_id = ?para_id,
			peer_id = ?peer_id,
			error = %fetch_error,
			"Failed to request advertised collation",
		);
	}

	Ok(())
}

/// Enqueue collation for fetching. The advertisement is expected to be validated and the seconding
/// limit checked.
async fn enqueue_collation<Sender>(
	sender: &mut Sender,
	state: &mut State,
	relay_parent: Hash,
	scheduling_parent: Hash,
	para_id: ParaId,
	peer_id: PeerId,
	collator_id: CollatorId,
	prospective_candidate: Option<(CandidateHash, Hash)>,
	advertised_descriptor_version: Option<CandidateDescriptorVersion>,
) -> std::result::Result<(), FetchError>
where
	Sender: CollatorProtocolSenderTrait,
{
	gum::debug!(
		target: LOG_TARGET,
		peer_id = ?peer_id,
		%para_id,
		?scheduling_parent,
		"Received advertise collation",
	);
	let per_scheduling_parent = match state.per_scheduling_parent.get_mut(&scheduling_parent) {
		Some(sp_state) => sp_state,
		None => {
			// Race happened, not an error.
			gum::trace!(
				target: LOG_TARGET,
				peer_id = ?peer_id,
				%para_id,
				?scheduling_parent,
				?prospective_candidate,
				"Candidate relay parent went out of view for valid advertisement",
			);
			return Ok(());
		},
	};
	let prospective_candidate =
		prospective_candidate.map(|(candidate_hash, parent_head_data_hash)| ProspectiveCandidate {
			candidate_hash,
			parent_head_data_hash,
		});

	let collations = &mut per_scheduling_parent.collations;
	let pending_collation = PendingCollation::new(
		scheduling_parent,
		relay_parent,
		para_id,
		&peer_id,
		prospective_candidate,
		advertised_descriptor_version,
	);

	gum::debug!(
		target: LOG_TARGET,
		peer_id = ?peer_id,
		%para_id,
		?scheduling_parent,
		status = ?collations.status,
		"Enqueue: status check",
	);

	match collations.status {
		CollationStatus::Fetching(_) | CollationStatus::WaitingOnValidation => {
			gum::trace!(
				target: LOG_TARGET,
				peer_id = ?peer_id,
				%para_id,
				?scheduling_parent,
				"Added collation to the pending list"
			);
			collations.add_to_waiting_queue((pending_collation, collator_id));
		},
		CollationStatus::Waiting => {
			// We were waiting for a collation to be advertised to us (we were idle) so we can fetch
			// the new collation immediately
			fetch_collation(sender, state, pending_collation, collator_id).await?;
		},
	}

	Ok(())
}

/// Our view has changed.
async fn handle_our_view_change<Sender>(
	sender: &mut Sender,
	state: &mut State,
	keystore: &KeystorePtr,
	view: OurView,
) -> Result<()>
where
	Sender: CollatorProtocolSenderTrait,
{
	let current_leaves: HashSet<Hash> = state.leaf_claim_queues.keys().copied().collect();

	let removed = current_leaves.iter().filter(|h| !view.contains(h));
	let added = view.iter().filter(|h| !current_leaves.contains(h));

	for leaf in added {
		let session_index = request_session_index_for_child(*leaf, sender)
			.await
			.await
			.map_err(Error::CancelledSessionIndex)??;

		// Fetch the leaf's per-core claim queues and scheduling lookahead. Both come from the
		// runtime so the lookahead (the window length for the CQ-offset arithmetic in
		// `is_slot_available`) is never inferred from the claim-queue or allowed-ancestry length;
		// see [`LeafClaimQueues`].
		let leaf_claim_queues = LeafClaimQueues::fetch(*leaf, session_index, sender)
			.await
			.map_err(Error::FetchLeafClaimQueues)?;

		let Some(per_scheduling_parent) = construct_per_scheduling_parent(
			sender,
			&mut state.assigned_cores,
			keystore,
			*leaf,
			session_index,
		)
		.await?
		else {
			continue;
		};

		state.per_scheduling_parent.insert(*leaf, per_scheduling_parent);
		state.leaf_claim_queues.insert(*leaf, leaf_claim_queues);

		match extract_leaf_scheduling_info(sender, *leaf).await {
			Some(info) => {
				state.leaf_scheduling_info.insert(*leaf, info);
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

		state
			.implicit_view
			.activate_leaf(sender, *leaf)
			.await
			.map_err(Error::ImplicitViewFetchError)?;

		// Order is always descending.
		let allowed_ancestry =
			state.implicit_view.known_allowed_relay_parents_under(leaf).unwrap_or_default();
		for block_hash in allowed_ancestry {
			if let Entry::Vacant(entry) = state.per_scheduling_parent.entry(*block_hash) {
				// Safe to use the same session index for the allowed scheduling parents as well
				// since they must be in the same session.
				if let Some(per_scheduling_parent) = construct_per_scheduling_parent(
					sender,
					&mut state.assigned_cores,
					keystore,
					*block_hash,
					session_index,
				)
				.await?
				{
					entry.insert(per_scheduling_parent);
				}
			}
		}
	}

	for removed in removed {
		gum::trace!(
			target: LOG_TARGET,
			?view,
			?removed,
			"handle_our_view_change - removed",
		);

		state.leaf_claim_queues.remove(&removed);
		state.leaf_scheduling_info.remove(&removed);

		// If the leaf is deactivated it still may stay in the view as a part
		// of implicit ancestry. Only update the state after the hash is actually
		// pruned from the block info storage.
		let pruned_ancestry = state.implicit_view.deactivate_leaf(*removed);

		for pruned in pruned_ancestry {
			if let Some(per_scheduling_parent) = state.per_scheduling_parent.remove(&pruned) {
				remove_outgoing(&mut state.assigned_cores, per_scheduling_parent);
			}

			state.collation_requests_cancel_handles.retain(|pc, handle| {
				let keep = pc.scheduling_parent != pruned;
				if !keep {
					handle.cancel();
				}
				keep
			});
			state.fetched_candidates.retain(|k, _| k.scheduling_parent != pruned);
		}
	}

	// Remove blocked seconding requests that left the view.
	state.blocked_from_seconding.retain(|_, collations| {
		collations.retain(|collation| {
			let scheduling_parent = collation.collation_event.pending_collation.scheduling_parent;
			state.per_scheduling_parent.contains_key(&scheduling_parent)
		});

		!collations.is_empty()
	});

	for (peer_id, peer_data) in state.peer_data.iter_mut() {
		peer_data.prune_old_advertisements(&state.implicit_view, state.leaf_claim_queues.keys());

		// Disconnect peers who are not relevant to our current or next para.
		//
		// If the peer hasn't declared yet, they will be disconnected if they do not declare.
		// Check if the para appears in any of our assigned cores' claim queues.
		if let Some(para_id) = peer_data.collating_para() {
			let mut para_still_assigned = false;
			for core in state.assigned_cores.keys() {
				// Check if para appears in any active leaf's claim queue for this core
				if state.leaf_claim_queues.values().any(|lcq| {
					lcq.claim_queues.get(core).map_or(false, |paras| paras.contains(&para_id))
				}) {
					para_still_assigned = true;
					break;
				}
			}

			if !para_still_assigned {
				gum::trace!(
					target: LOG_TARGET,
					?peer_id,
					?para_id,
					"Disconnecting peer on view change (not current parachain id)"
				);
				disconnect_peer(sender, *peer_id).await;
			}
		}
	}

	Ok(())
}

/// Bridge event switch.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn handle_network_msg<Context>(
	ctx: &mut Context,
	state: &mut State,
	keystore: &KeystorePtr,
	bridge_message: NetworkBridgeEvent<net_protocol::CollatorProtocolMessage>,
) -> Result<()> {
	use NetworkBridgeEvent::*;

	match bridge_message {
		PeerConnected(peer_id, observed_role, protocol_version, _) => {
			let version = match protocol_version.try_into() {
				Ok(version) => version,
				Err(err) => {
					// Network bridge is expected to handle this.
					gum::error!(
						target: LOG_TARGET,
						?peer_id,
						?observed_role,
						?err,
						"Unsupported protocol version"
					);
					return Ok(());
				},
			};

			// Since the AssetHub system parachain is very important for the operation of the
			// relay chain we need to ensure that we always accept connections from the invulnerable
			// AssetHub collators.
			// However there is a connection limit in `polkadot-node-network-protocol` of the number
			// of peers which can connect to an authority node. Once this limit is reached all new
			// connections are dropped.
			// This means we need to be extra careful when accepting connections from new collators.
			// If we are at the point where there are not enough slots for
			// invulnerable AssetHub collators we need to drop new connections.
			if !can_accept_new_collator_connection(
				&peer_id,
				&state.peer_data,
				&state.ah_invulnerables,
			) {
				gum::debug!(
					target: LOG_TARGET,
					?peer_id,
					?observed_role,
					"Peer connection refused, no slots for invulnerable AssetHub collators",
				);
				disconnect_peer(ctx.sender(), peer_id).await;
				return Ok(());
			}

			let now = state.clock.now();
			state.peer_data.entry(peer_id).or_insert_with(|| PeerData {
				view: View::default(),
				state: PeerState::Connected(now),
				version,
			});
			state.metrics.note_collator_peer_count(state.peer_data.len());
		},
		PeerDisconnected(peer_id) => {
			state.peer_data.remove(&peer_id);
			// Clean up any pending collations from this peer in all waiting queues.
			// This prevents stale entries when the peer reconnects with empty advertisements.
			for per_scheduling_parent in state.per_scheduling_parent.values_mut() {
				per_scheduling_parent.collations.remove_pending_for_peer(&peer_id);
			}
			state.metrics.note_collator_peer_count(state.peer_data.len());
		},
		NewGossipTopology { .. } => {
			// impossible!
		},
		PeerViewChange(peer_id, view) => {
			handle_peer_view_change(state, peer_id, view);
		},
		OurViewChange(view) => {
			handle_our_view_change(ctx.sender(), state, keystore, view).await?;
		},
		PeerMessage(remote, msg) => {
			process_incoming_peer_message(ctx, state, remote, msg).await;
		},
		UpdatedAuthorityIds { .. } => {
			// The validator side doesn't deal with `AuthorityDiscoveryId`s.
		},
	}

	Ok(())
}

/// The main message receiver switch.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn process_msg<Context>(
	ctx: &mut Context,
	keystore: &KeystorePtr,
	msg: CollatorProtocolMessage,
	state: &mut State,
) {
	use CollatorProtocolMessage::*;

	let _timer = state.metrics.time_process_msg();

	match msg {
		ConnectToBackingGroups => {
			gum::warn!(
				target: LOG_TARGET,
				"PreConnectToBackingGroups message is not expected on the validator side of the protocol",
			);
		},
		DisconnectFromBackingGroups => {
			gum::warn!(
				target: LOG_TARGET,
				"DisconnectFromBackingGroups message is not expected on the validator side of the protocol",
			);
		},
		CollateOn(id) => {
			gum::warn!(
				target: LOG_TARGET,
				para_id = %id,
				"CollateOn message is not expected on the validator side of the protocol",
			);
		},
		DistributeSegment { .. } => {
			gum::warn!(
				target: LOG_TARGET,
				"DistributeSegment message is not expected on the validator side of the protocol",
			);
		},
		NetworkBridgeUpdate(event) => {
			if let Err(e) = handle_network_msg(ctx, state, keystore, event).await {
				gum::warn!(
					target: LOG_TARGET,
					err = ?e,
					"Failed to handle incoming network message",
				);
			}
		},
		Seconded(parent, stmt) => {
			let receipt = match stmt.payload() {
				Statement::Seconded(receipt) => receipt,
				Statement::Valid(_) => {
					gum::warn!(
						target: LOG_TARGET,
						?stmt,
						relay_parent = %parent,
						"Seconded message received with a `Valid` statement",
					);
					return;
				},
			};
			let output_head_data = receipt.commitments.head_data.clone();
			let output_head_data_hash = receipt.descriptor.para_head();
			let fetched_collation = FetchedCollation::new(&receipt.to_plain());
			if let Some(CollationEvent { collator_id, pending_collation, .. }) =
				state.fetched_candidates.remove(&fetched_collation)
			{
				let PendingCollation {
					scheduling_parent,
					peer_id,
					prospective_candidate,
					para_id,
					..
				} = pending_collation;
				note_good_collation(
					&mut state.reputation,
					ctx.sender(),
					&state.peer_data,
					collator_id.clone(),
				)
				.await;
				if let Some(peer_data) = state.peer_data.get(&peer_id) {
					notify_collation_seconded(
						ctx.sender(),
						peer_id,
						peer_data.version,
						scheduling_parent,
						stmt,
					)
					.await;
				}

				if let Some(rp_state) = state.per_scheduling_parent.get_mut(&parent) {
					rp_state.collations.note_seconded(para_id);
				}

				// See if we've unblocked other collations for seconding.
				second_unblocked_collations(
					ctx,
					state,
					fetched_collation.para_id,
					output_head_data,
					output_head_data_hash,
				)
				.await;

				// If async backing is enabled, make an attempt to fetch next collation.
				let maybe_candidate_hash =
					prospective_candidate.as_ref().map(ProspectiveCandidate::candidate_hash);
				dequeue_next_collation_and_fetch(
					ctx,
					state,
					parent,
					(collator_id, maybe_candidate_hash),
				)
				.await;
			} else {
				gum::debug!(
					target: LOG_TARGET,
					relay_parent = ?parent,
					"Collation has been seconded, but the relay parent is deactivated",
				);
			}
		},
		Invalid(parent, candidate_receipt) => {
			// Remove collations which were blocked from seconding and had this candidate as parent.
			state.blocked_from_seconding.remove(&BlockedCollationId {
				para_id: candidate_receipt.descriptor.para_id(),
				parent_head_data_hash: candidate_receipt.descriptor.para_head(),
			});

			let fetched_collation = FetchedCollation::new(&candidate_receipt);
			let candidate_hash = fetched_collation.candidate_hash;
			let id = match state.fetched_candidates.entry(fetched_collation) {
				Entry::Occupied(entry)
					if entry.get().pending_collation.commitments_hash ==
						Some(candidate_receipt.commitments_hash) =>
				{
					entry.remove().collator_id
				},
				Entry::Occupied(_) => {
					gum::error!(
						target: LOG_TARGET,
						relay_parent = ?parent,
						candidate = ?candidate_receipt.hash(),
						"Reported invalid candidate for unknown `pending_candidate`!",
					);
					return;
				},
				Entry::Vacant(_) => return,
			};

			report_collator(&mut state.reputation, ctx.sender(), &state.peer_data, id.clone())
				.await;

			dequeue_next_collation_and_fetch(ctx, state, parent, (id, Some(candidate_hash))).await;
		},
	}
}

/// The main run loop.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
pub(crate) async fn run<Context>(
	ctx: Context,
	keystore: KeystorePtr,
	eviction_policy: crate::CollatorEvictionPolicy,
	metrics: Metrics,
	ah_invulnerables: HashSet<PeerId>,
	hold_off_duration: Option<Duration>,
	clock: Arc<dyn Clock>,
) -> std::result::Result<(), SubsystemError> {
	gum::info!(target: LOG_TARGET, "Running legacy collator protocol");
	run_inner(
		ctx,
		keystore,
		eviction_policy,
		metrics,
		ReputationAggregator::default(),
		REPUTATION_CHANGE_INTERVAL,
		ah_invulnerables,
		hold_off_duration.unwrap_or(HOLD_OFF_DURATION_DEFAULT_VALUE),
		clock,
	)
	.await
}

#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn run_inner<Context>(
	mut ctx: Context,
	keystore: KeystorePtr,
	eviction_policy: crate::CollatorEvictionPolicy,
	metrics: Metrics,
	reputation: ReputationAggregator,
	reputation_interval: Duration,
	ah_invulnerables: HashSet<PeerId>,
	hold_off_duration: Duration,
	clock: Arc<dyn Clock>,
) -> std::result::Result<(), SubsystemError> {
	let new_reputation_delay = || clock.delay(reputation_interval).fuse();
	let mut reputation_delay = new_reputation_delay();

	let mut state =
		State::new(clock.clone(), metrics, reputation, ah_invulnerables, hold_off_duration);

	let next_inactivity_stream = tick_stream(clock.clone(), ACTIVITY_POLL);
	futures::pin_mut!(next_inactivity_stream);

	let mut network_error_freq = gum::Freq::new();
	let mut canceled_freq = gum::Freq::new();

	loop {
		select! {
			_ = reputation_delay => {
				state.reputation.send(ctx.sender()).await;
				reputation_delay = new_reputation_delay();
			},
			res = ctx.recv().fuse() => {
				match res {
					Ok(FromOrchestra::Communication { msg }) => {
						gum::trace!(target: LOG_TARGET, msg = ?msg, "received a message");
						process_msg(
							&mut ctx,
							&keystore,
							msg,
							&mut state,
						).await;
					}
					Ok(FromOrchestra::Signal(OverseerSignal::Conclude)) | Err(_) => break,
					Ok(FromOrchestra::Signal(_)) => continue,
				}
			},
			_ = next_inactivity_stream.next() => {
				disconnect_inactive_peers(ctx.sender(), &*clock, &eviction_policy, &state.peer_data).await;
			},
			resp = state.collation_requests.select_next_some() => {
				let relay_parent = resp.0.pending_collation.scheduling_parent;
				let res = match handle_collation_fetch_response(
					&mut state,
					resp,
					&mut network_error_freq,
					&mut canceled_freq,
				).await {
					Err(Some((peer_id, rep))) => {
						modify_reputation(&mut state.reputation, ctx.sender(), peer_id, rep).await;
						// Reset the status for the relay parent
						state.per_scheduling_parent.get_mut(&relay_parent).map(|rp| {
							rp.collations.status.back_to_waiting();
						});
						continue
					},
					Err(None) => {
						// Reset the status for the relay parent
						state.per_scheduling_parent.get_mut(&relay_parent).map(|rp| {
							rp.collations.status.back_to_waiting();
						});
						continue
					},
					Ok(res) => res
				};

				let CollationEvent {collator_id, pending_collation, .. } = res.collation_event.clone();

				match kick_off_seconding(&mut ctx, &mut state, res).await {
					Err(err) => {
						gum::warn!(
							target: LOG_TARGET,
							relay_parent = ?pending_collation.scheduling_parent,
							para_id = ?pending_collation.para_id,
							peer_id = ?pending_collation.peer_id,
							error = %err,
							"Seconding aborted due to an error",
						);

						if err.is_malicious() {
							// Report malicious peer.
							modify_reputation(&mut state.reputation, ctx.sender(), pending_collation.peer_id, COST_REPORT_BAD).await;
						}
						let maybe_candidate_hash =
						pending_collation.prospective_candidate.as_ref().map(ProspectiveCandidate::candidate_hash);
						dequeue_next_collation_and_fetch(
							&mut ctx,
							&mut state,
							pending_collation.scheduling_parent,
							(collator_id, maybe_candidate_hash),
						)
						.await;
					},
					Ok(false) => {
						// No hard error occurred, but we can try fetching another collation.
						let maybe_candidate_hash =
						pending_collation.prospective_candidate.as_ref().map(ProspectiveCandidate::candidate_hash);
						dequeue_next_collation_and_fetch(
							&mut ctx,
							&mut state,
							pending_collation.scheduling_parent,
							(collator_id, maybe_candidate_hash),
						)
						.await;
					}
					Ok(true) => {}
				}
			},
			res = state.collation_fetch_timeouts.select_next_some() => {
				let (collator_id, maybe_candidate_hash, relay_parent) = res;
				gum::debug!(
					target: LOG_TARGET,
					?relay_parent,
					?collator_id,
					"Timeout hit - already seconded?"
				);
				dequeue_next_collation_and_fetch(
					&mut ctx,
					&mut state,
					relay_parent,
					(collator_id, maybe_candidate_hash),
				)
				.await;
			},
			rp = state.ah_held_off_rp_timers.select_next_some() => {
				let Some(held_off_advertisements) = state.per_scheduling_parent.get_mut(&rp).map(|rp_state| rp_state.ah_held_off_advertisements.on_hold_off_complete()) else {
					gum::debug!(
						target: LOG_TARGET,
						?rp,
						"Received AssetHub held off advertisements timeout for unknown relay parent",
					);
					continue
				};

				let held_off_advertisements = match held_off_advertisements {
					Ok(held_off) => held_off,
					Err(err) => {
						gum::warn!(
							target: LOG_TARGET,
							?rp,
							?err,
							"Failed to get held off advertisements for relay parent",
						);
						continue;
					}
				};

				for held_off_advertisement in held_off_advertisements {
					let HeldOffAdvertisement{relay_parent, scheduling_parent, peer_id, collator_id, prospective_candidate, advertised_descriptor_version} = held_off_advertisement;
					gum::debug!(
						target: LOG_TARGET,
						?scheduling_parent,
						?peer_id,
						?prospective_candidate,
						"Processing held off advertisement for AssetHub",
					);

					if let Err(err) = process_advertisement(
						ctx.sender(),
						&mut state,
						relay_parent,
						scheduling_parent,
						ASSET_HUB_PARA_ID,
						peer_id,
						collator_id,
						prospective_candidate,
						advertised_descriptor_version
					)
					.await {
						gum::debug!(
								target: LOG_TARGET,
								?err,
								?scheduling_parent,
								?peer_id,
								?prospective_candidate,
								"Failed to handle held off advertisement",
							);
						if let Some(rep) = err.reputation_changes() {
							modify_reputation(&mut state.reputation, ctx.sender(), peer_id, rep).await;
						}
					}
				}
			},
		}
	}

	Ok(())
}

/// Dequeue another collation and fetch.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn dequeue_next_collation_and_fetch<Context>(
	ctx: &mut Context,
	state: &mut State,
	relay_parent: Hash,
	// The collator we tried to fetch from last, optionally which candidate.
	previous_fetch: (CollatorId, Option<CandidateHash>),
) {
	while let Some((next, id)) = get_next_collation_to_fetch(&previous_fetch, relay_parent, state) {
		gum::debug!(
			target: LOG_TARGET,
			?relay_parent,
			?id,
			"Successfully dequeued next advertisement - fetching ..."
		);
		let next_scheduling_parent = next.scheduling_parent;
		let next_para_id = next.para_id;
		let next_peer_id = next.peer_id;
		if let Err(err) = fetch_collation(ctx.sender(), state, next, id).await {
			gum::debug!(
				target: LOG_TARGET,
				relay_parent = ?next_scheduling_parent,
				para_id = ?next_para_id,
				peer_id = ?next_peer_id,
				error = %err,
				"Failed to request a collation, dequeueing next one",
			);
		} else {
			break;
		}
	}
}

pub async fn request_persisted_validation_data<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
	para_id: ParaId,
) -> std::result::Result<Option<PersistedValidationData>, SecondingError>
where
	Sender: CollatorProtocolSenderTrait,
{
	// The core is guaranteed to be scheduled since we accepted the advertisement.
	polkadot_node_subsystem_util::request_persisted_validation_data(
		relay_parent,
		para_id,
		OccupiedCoreAssumption::Free,
		sender,
	)
	.await
	.await
	.map_err(SecondingError::CancelledRuntimePersistedValidationData)?
	.map_err(SecondingError::RuntimeApi)
}

pub async fn request_prospective_validation_data<Sender>(
	sender: &mut Sender,
	candidate_relay_parent: Hash,
	session_index: SessionIndex,
	parent_head_data_hash: Hash,
	para_id: ParaId,
	maybe_parent_head_data: Option<HeadData>,
) -> std::result::Result<Option<PersistedValidationData>, SecondingError>
where
	Sender: CollatorProtocolSenderTrait,
{
	let (tx, rx) = oneshot::channel();

	let parent_head_data = if let Some(head_data) = maybe_parent_head_data {
		ParentHeadData::WithData { head_data, hash: parent_head_data_hash }
	} else {
		ParentHeadData::OnlyHash(parent_head_data_hash)
	};

	let request = ProspectiveValidationDataRequest {
		para_id,
		candidate_relay_parent,
		session_index,
		parent_head_data,
	};

	sender
		.send_message(ProspectiveParachainsMessage::GetProspectiveValidationData(request, tx))
		.await;

	rx.await.map_err(SecondingError::CancelledProspectiveValidationData)
}

/// Handle a fetched collation result.
/// Returns whether or not seconding has begun.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn kick_off_seconding<Context>(
	ctx: &mut Context,
	state: &mut State,
	PendingCollationFetch { mut collation_event, candidate_receipt, pov, maybe_parent_head_data }: PendingCollationFetch,
) -> std::result::Result<bool, SecondingError> {
	let scheduling_parent = collation_event.pending_collation.scheduling_parent;
	let para_id = collation_event.pending_collation.para_id;

	let per_scheduling_parent = match state.per_scheduling_parent.get_mut(&scheduling_parent) {
		Some(state) => state,
		None => {
			// Scheduling parent went out of view, not an error.
			gum::trace!(
				target: LOG_TARGET,
				?scheduling_parent,
				"Fetched collation for a scheduling parent out of view",
			);
			return Ok(false);
		},
	};

	// Sanity check of the candidate receipt version.
	descriptor_version_sanity_check(
		candidate_receipt.descriptor(),
		per_scheduling_parent,
		collation_event.collator_protocol_version,
	)?;

	let collations = &mut per_scheduling_parent.collations;

	let fetched_collation = FetchedCollation::new(&candidate_receipt);
	if let Entry::Vacant(entry) = state.fetched_candidates.entry(fetched_collation) {
		collation_event.pending_collation.commitments_hash =
			Some(candidate_receipt.commitments_hash);

		let (maybe_pvd, maybe_parent_head, maybe_parent_head_hash) = match (
			collation_event.collator_protocol_version,
			collation_event.pending_collation.prospective_candidate,
		) {
			(CollationVersion::V2, Some(ProspectiveCandidate { parent_head_data_hash, .. })) |
			(CollationVersion::V3, Some(ProspectiveCandidate { parent_head_data_hash, .. })) => {
				// PVD contains relay_parent_number and relay_parent_storage_root, so
				// we must pass the actual relay_parent (execution context), not the
				// scheduling_parent. For V1/V2 these are identical; for V3 the
				// relay_parent may be older.
				let session_index = candidate_receipt
					.descriptor()
					.session_index()
					.unwrap_or(per_scheduling_parent.session_index);

				let pvd = request_prospective_validation_data(
					ctx.sender(),
					candidate_receipt.descriptor().relay_parent(),
					session_index,
					parent_head_data_hash,
					para_id,
					maybe_parent_head_data.clone(),
				)
				.await?;

				(pvd, maybe_parent_head_data, Some(parent_head_data_hash))
			},
			(CollationVersion::V1, _) => {
				// V1 protocol only carries V1 descriptors (enforced by
				// descriptor_version_sanity_check), so the legacy PVD API is correct.
				let pvd = request_persisted_validation_data(
					ctx.sender(),
					candidate_receipt.descriptor().relay_parent(),
					candidate_receipt.descriptor().para_id(),
				)
				.await?;
				(
					Some(pvd.ok_or(SecondingError::PersistedValidationDataNotFound)?),
					maybe_parent_head_data,
					None,
				)
			},
			_ => {
				// `handle_advertisement` checks for protocol mismatch.
				return Ok(false);
			},
		};

		let pvd = match (maybe_pvd, maybe_parent_head.clone(), maybe_parent_head_hash) {
			(Some(pvd), _, _) => pvd,
			(None, None, Some(parent_head_data_hash)) => {
				// In this case, the collator did not supply the head data and neither could
				// prospective-parachains. We add this to the blocked_from_seconding collection
				// until we second its parent.
				let blocked_collation = PendingCollationFetch {
					collation_event,
					candidate_receipt,
					pov,
					maybe_parent_head_data: None,
				};
				gum::debug!(
					target: LOG_TARGET,
					candidate_hash = ?blocked_collation.candidate_receipt.hash(),
					?scheduling_parent,
					"Collation having parent head data hash {} is blocked from seconding. Waiting on its parent to be validated.",
					parent_head_data_hash
				);
				state
					.blocked_from_seconding
					.entry(BlockedCollationId {
						para_id: blocked_collation.candidate_receipt.descriptor.para_id(),
						parent_head_data_hash,
					})
					.or_insert_with(Vec::new)
					.push(blocked_collation);

				return Ok(false);
			},
			(None, _, _) => {
				// Even though we already have the parent head data, the pvd fetching failed. We
				// don't need to wait for seconding another collation outputting this head data.
				return Err(SecondingError::PersistedValidationDataNotFound);
			},
		};

		fetched_collation_sanity_check(
			&collation_event.pending_collation,
			&candidate_receipt,
			&pvd,
			maybe_parent_head.and_then(|head| maybe_parent_head_hash.map(|hash| (head, hash))),
		)?;

		ctx.send_message(CandidateBackingMessage::Second {
			scheduling_parent,
			candidate: candidate_receipt,
			pvd,
			pov,
		})
		.await;
		// There's always a single collation being fetched at any moment of time.
		// In case of a failure, we reset the status back to waiting.
		collations.status = CollationStatus::WaitingOnValidation;

		entry.insert(collation_event);
		Ok(true)
	} else {
		Err(SecondingError::Duplicate)
	}
}

// This issues `NetworkBridge` notifications to disconnect from all inactive peers at the
// earliest possible point. This does not yet clean up any metadata, as that will be done upon
// receipt of the `PeerDisconnected` event.
async fn disconnect_inactive_peers(
	sender: &mut impl overseer::CollatorProtocolSenderTrait,
	clock: &dyn Clock,
	eviction_policy: &crate::CollatorEvictionPolicy,
	peers: &HashMap<PeerId, PeerData>,
) {
	for (peer, peer_data) in peers {
		if peer_data.is_inactive(clock, &eviction_policy) {
			gum::trace!(target: LOG_TARGET, ?peer, "Disconnecting inactive peer");
			disconnect_peer(sender, *peer).await;
		}
	}
}

/// Handle a collation fetch response.
async fn handle_collation_fetch_response(
	state: &mut State,
	response: <CollationFetchRequest as Future>::Output,
	network_error_freq: &mut gum::Freq,
	canceled_freq: &mut gum::Freq,
) -> std::result::Result<PendingCollationFetch, Option<(PeerId, Rep)>> {
	let (CollationEvent { collator_id, collator_protocol_version, pending_collation }, response) =
		response;
	// Remove the cancellation handle, as the future already completed.
	state.collation_requests_cancel_handles.remove(&pending_collation);

	let response = match response {
		Err(CollationFetchError::Cancelled) => {
			gum::debug!(
				target: LOG_TARGET,
				hash = ?pending_collation.scheduling_parent,
				para_id = ?pending_collation.para_id,
				peer_id = ?pending_collation.peer_id,
				"Request was cancelled from the validator side"
			);
			return Err(None);
		},
		Err(CollationFetchError::Request(req_error)) => Err(req_error),
		Ok(resp) => Ok(resp),
	};

	let _timer = state.metrics.time_handle_collation_request_result();

	let mut metrics_result = Err(());

	let result = match response {
		Err(RequestError::InvalidResponse(err)) => {
			gum::warn!(
				target: LOG_TARGET,
				hash = ?pending_collation.scheduling_parent,
				para_id = ?pending_collation.para_id,
				peer_id = ?pending_collation.peer_id,
				err = ?err,
				"Collator provided response that could not be decoded"
			);
			Err(Some((pending_collation.peer_id, COST_CORRUPTED_MESSAGE)))
		},
		Err(err) if err.is_timed_out() => {
			gum::debug!(
				target: LOG_TARGET,
				hash = ?pending_collation.scheduling_parent,
				para_id = ?pending_collation.para_id,
				peer_id = ?pending_collation.peer_id,
				"Request timed out"
			);
			// For now we don't want to change reputation on timeout, to mitigate issues like
			// this: https://github.com/paritytech/polkadot/issues/4617
			Err(None)
		},
		Err(RequestError::NetworkError(err)) => {
			gum::warn_if_frequent!(
				freq: network_error_freq,
				max_rate: gum::Times::PerHour(100),
				target: LOG_TARGET,
				hash = ?pending_collation.scheduling_parent,
				para_id = ?pending_collation.para_id,
				peer_id = ?pending_collation.peer_id,
				err = ?err,
				"Fetching collation failed due to network error"
			);
			// A minor decrease in reputation for any network failure seems
			// sensible. In theory this could be exploited, by DoSing this node,
			// which would result in reduced reputation for proper nodes, but the
			// same can happen for penalties on timeouts, which we also have.
			Err(Some((pending_collation.peer_id, COST_NETWORK_ERROR)))
		},
		Err(RequestError::Canceled(err)) => {
			gum::warn_if_frequent!(
				freq: canceled_freq,
				max_rate: gum::Times::PerHour(100),
				target: LOG_TARGET,
				hash = ?pending_collation.scheduling_parent,
				para_id = ?pending_collation.para_id,
				peer_id = ?pending_collation.peer_id,
				err = ?err,
				"Canceled should be handled by `is_timed_out` above - this is a bug!"
			);
			Err(None)
		},
		Ok(
			request_v1::CollationFetchingResponse::Collation(receipt, _) |
			request_v2::CollationFetchingResponse::Collation(receipt, _) |
			request_v1::CollationFetchingResponse::CollationWithParentHeadData { receipt, .. } |
			request_v2::CollationFetchingResponse::CollationWithParentHeadData { receipt, .. },
		) if receipt.descriptor().para_id() != pending_collation.para_id => {
			gum::debug!(
				target: LOG_TARGET,
				expected_para_id = ?pending_collation.para_id,
				got_para_id = ?receipt.descriptor().para_id(),
				peer_id = ?pending_collation.peer_id,
				"Got wrong para ID for requested collation."
			);

			Err(Some((pending_collation.peer_id, COST_WRONG_PARA)))
		},
		Ok(request_v1::CollationFetchingResponse::Collation(candidate_receipt, pov)) => {
			gum::debug!(
				target: LOG_TARGET,
				para_id = %pending_collation.para_id,
				hash = ?pending_collation.scheduling_parent,
				candidate_hash = ?candidate_receipt.hash(),
				"Received collation",
			);

			metrics_result = Ok(());
			Ok(PendingCollationFetch {
				collation_event: CollationEvent {
					collator_id,
					pending_collation,
					collator_protocol_version,
				},
				candidate_receipt,
				pov,
				maybe_parent_head_data: None,
			})
		},
		Ok(request_v2::CollationFetchingResponse::CollationWithParentHeadData {
			receipt,
			pov,
			parent_head_data,
		}) => {
			gum::debug!(
				target: LOG_TARGET,
				para_id = %pending_collation.para_id,
				hash = ?pending_collation.scheduling_parent,
				candidate_hash = ?receipt.hash(),
				"Received collation (v3)",
			);

			metrics_result = Ok(());
			Ok(PendingCollationFetch {
				collation_event: CollationEvent {
					collator_id,
					pending_collation,
					collator_protocol_version,
				},
				candidate_receipt: receipt,
				pov,
				maybe_parent_head_data: Some(parent_head_data),
			})
		},
	};
	state.metrics.on_request(metrics_result);
	result
}

// Returns the claim queue without fetched or pending advertisement. The resulting `Vec` keeps the
// order in the claim queue so the earlier an element is located in the `Vec` the higher its
// priority is.
//
// Uses the leaf-based approach: gets the claim queue from an active leaf and filters out
// fulfilled positions based on seconded/pending candidates.
fn unfulfilled_claim_queue_entries(
	scheduling_parent: &Hash,
	state: &State,
) -> Result<VecDeque<ParaId>> {
	let scheduling_parent_state = state
		.per_scheduling_parent
		.get(scheduling_parent)
		.ok_or(Error::RelayParentStateNotFound)?;
	let current_core = scheduling_parent_state.current_core;

	let paths = state.implicit_view.paths_via_relay_parent(scheduling_parent);

	// Collect unfulfilled entries from all paths and take the longest
	// Use the longest unfulfilled we find:
	let mut best_unfulfilled = VecDeque::new();

	for path in paths {
		let leaf = match path.last() {
			Some(l) => l,
			None => continue,
		};

		let Some(leaf_cq) = state
			.leaf_claim_queues
			.get(leaf)
			.and_then(|lcq| lcq.claim_queues.get(&current_core))
		else {
			continue;
		};

		// Get a BitVec per para showing where it is assigned:
		let mut para_schedules: HashMap<ParaId, BitVec> = HashMap::new();
		for (idx, para) in leaf_cq.iter().enumerate() {
			let schedule = para_schedules
				.entry(*para)
				.or_insert_with(|| BitVec::repeat(false, leaf_cq.len()));
			schedule.set(idx, true);
		}

		// Now eat up our assignments for the pending work:
		for (para, schedule) in para_schedules.iter_mut() {
			let mut used: usize = path
				.iter()
				.map(|ancestor| state.seconded_and_pending_for_para(ancestor, para))
				.sum();
			for idx in 0..schedule.len() {
				if used == 0 {
					break;
				}
				if schedule[idx] {
					used -= 1;
					schedule.set(idx, false);
				}
			}
		}

		// Get free spots in order assigned to the corresponding para in order:
		let mut unfulfilled: Vec<Option<ParaId>> = vec![None; leaf_cq.len()];
		for (para, schedule) in para_schedules {
			for (idx, is_available) in schedule.iter().enumerate() {
				if *is_available && unfulfilled[idx].is_none() {
					unfulfilled[idx] = Some(para);
				}
			}
		}

		let unfulfilled = unfulfilled.into_iter().flatten().collect::<VecDeque<ParaId>>();
		if unfulfilled.len() > best_unfulfilled.len() {
			best_unfulfilled = unfulfilled;
		}
	}
	Ok(best_unfulfilled)
}

/// Returns the next collation to fetch from the `waiting_queue` and reset the status back to
/// `Waiting`.
fn get_next_collation_to_fetch(
	finished_one: &(CollatorId, Option<CandidateHash>),
	relay_parent: Hash,
	state: &mut State,
) -> Option<(PendingCollation, CollatorId)> {
	let unfulfilled_entries = match unfulfilled_claim_queue_entries(&relay_parent, &state) {
		Ok(entries) => entries,
		Err(err) => {
			gum::error!(
				target: LOG_TARGET,
				?relay_parent,
				?err,
				"Failed to get unfulfilled claim queue entries"
			);
			return None;
		},
	};
	let rp_state = match state.per_scheduling_parent.get_mut(&relay_parent) {
		Some(rp_state) => rp_state,
		None => {
			gum::error!(
				target: LOG_TARGET,
				?relay_parent,
				"Failed to get relay parent state"
			);
			return None;
		},
	};

	// If finished one does not match waiting_collation, then we already dequeued another fetch
	// to replace it.
	if let Some((collator_id, maybe_candidate_hash)) = rp_state.collations.fetching_from.as_ref() {
		// If a candidate hash was saved previously, `finished_one` must include this too.
		if collator_id != &finished_one.0 &&
			maybe_candidate_hash.map_or(true, |hash| Some(&hash) != finished_one.1.as_ref())
		{
			gum::trace!(
				target: LOG_TARGET,
				waiting_collation = ?rp_state.collations.fetching_from,
				?finished_one,
				"Not proceeding to the next collation - has already been done."
			);
			return None;
		}
	}
	rp_state.collations.status.back_to_waiting();
	rp_state.collations.pick_a_collation_to_fetch(unfulfilled_entries)
}

// Sanity check the candidate descriptor version using individual parameters.
pub fn descriptor_version_sanity_check_with_params(
	descriptor: &CandidateDescriptorV2,
	expected_core: CoreIndex,
	expected_session: SessionIndex,
	collator_protocol_version: CollationVersion,
) -> std::result::Result<(), SecondingError> {
	match descriptor.version() {
		CandidateDescriptorVersion::V1 => Ok(()),
		CandidateDescriptorVersion::V2 | CandidateDescriptorVersion::V3 => {
			// V3 descriptors must only arrive via V3 or V4 protocol.
			if descriptor.version() == CandidateDescriptorVersion::V3 &&
				!matches!(collator_protocol_version, CollationVersion::V3 | CollationVersion::V4)
			{
				return Err(SecondingError::InvalidReceiptVersion(CandidateDescriptorVersion::V3));
			}

			if let Some(core_index) = descriptor.core_index() {
				if core_index != expected_core {
					return Err(SecondingError::InvalidCoreIndex(core_index.0, expected_core.0));
				}
			}

			// For V2: session_index() == scheduling_session() (relay_parent == scheduling_parent).
			// For V3: session_index() is the relay_parent's session which may differ from the
			// scheduling session when the relay_parent is from a previous session. Check the
			// scheduling_session instead, which must always match the current session.
			if let Some(scheduling_session) = descriptor.scheduling_session() {
				if scheduling_session != expected_session {
					return Err(SecondingError::InvalidSessionIndex(
						scheduling_session,
						expected_session,
					));
				}
			}

			Ok(())
		},
		descriptor_version => Err(SecondingError::InvalidReceiptVersion(descriptor_version)),
	}
}

// Sanity check the candidate descriptor version.
fn descriptor_version_sanity_check(
	descriptor: &CandidateDescriptorV2,
	per_scheduling_parent: &PerSchedulingParent,
	collator_protocol_version: CollationVersion,
) -> std::result::Result<(), SecondingError> {
	descriptor_version_sanity_check_with_params(
		descriptor,
		per_scheduling_parent.current_core,
		per_scheduling_parent.session_index,
		collator_protocol_version,
	)
}

// Checks that there are enough free connection slots for AssetHub invulnerable collators.
fn can_accept_new_collator_connection(
	new_peer_id: &PeerId,
	currently_connected: &HashMap<PeerId, PeerData>,
	ah_invulnerables: &HashSet<PeerId>,
) -> bool {
	if ah_invulnerables.contains(new_peer_id) || ah_invulnerables.is_empty() {
		// If the new peer is AH invulnerable, we always accept it.
		return true;
	}

	// We want to keep some space for new connections. If we use the whole limit we could end up in
	// the case when a collator keeps on making a connection requests and with some luck (or
	// something else I don't anticipate right now) it keeps on connecting before the other peers
	// and thus blocking AssetHub invulnerables from connecting.
	let max_accepted_connections = MAX_AUTHORITY_INCOMING_STREAMS as usize - 10;
	let ah_invulnerables_connected = currently_connected
		.keys()
		.filter(|peer_id| ah_invulnerables.contains(peer_id))
		.count();

	// Reserve space for any unconnected AH invulnerable which might want to connect to us.
	let connection_limit =
		max_accepted_connections - ah_invulnerables.len() + ah_invulnerables_connected;

	gum::trace!(
		target: LOG_TARGET,
		?new_peer_id,
		?ah_invulnerables,
		currently_connected=currently_connected.len(),
		?connection_limit,
		"Checking if we can accept a new collator connection",
	);

	currently_connected.len() < connection_limit
}
