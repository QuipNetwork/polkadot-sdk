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

//! [`ApprovalDistribution`] implementation.
//!
//! See the documentation on [approval distribution][approval-distribution-page] in the
//! implementers' guide.
//!
//! [approval-distribution-page]: https://paritytech.github.io/polkadot-sdk/book/node/approval/approval-distribution.html

#![warn(missing_docs)]

use self::metrics::Metrics;
use futures::{select, FutureExt as _};
use itertools::Itertools;
use net_protocol::peer_set::{ProtocolVersion, ValidationVersion};
use polkadot_node_network_protocol::{
	self as net_protocol, filter_by_peer_version,
	grid_topology::{RandomRouting, RequiredRouting, SessionGridTopologies, SessionGridTopology},
	peer_set::MAX_NOTIFICATION_SIZE,
	v3 as protocol_v3, PeerId, UnifiedReputationChange as Rep, ValidationProtocols, View,
};
use polkadot_node_primitives::{
	approval::{
		criteria::{AssignmentCriteria, InvalidAssignment},
		time::{Clock, ClockExt, SystemClock, TICK_TOO_FAR_IN_FUTURE},
		v1::{BlockApprovalMeta, DelayTranche, RelayVRFStory},
		v2::{
			AsBitIndex, AssignmentCertKindV2, CandidateBitfield, IndirectAssignmentCertV2,
			IndirectSignedApprovalVoteV2,
		},
	},
	DISPUTE_WINDOW,
};
use polkadot_node_subsystem::{
	messages::{
		ApprovalDistributionMessage, ApprovalVotingMessage, CheckedIndirectAssignment,
		CheckedIndirectSignedApprovalVote, NetworkBridgeEvent, NetworkBridgeTxMessage,
		RuntimeApiMessage,
	},
	overseer, FromOrchestra, OverseerSignal, SpawnedSubsystem, SubsystemError,
};
use polkadot_node_subsystem_util::{
	reputation::{ReputationAggregator, REPUTATION_CHANGE_INTERVAL},
	runtime::{Config as RuntimeInfoConfig, ExtendedSessionInfo, RuntimeInfo},
};
use polkadot_primitives::{
	BlockNumber, CandidateHash, CandidateIndex, CoreIndex, DisputeStatement, GroupIndex, Hash,
	SessionIndex, Slot, ValidDisputeStatementKind, ValidatorIndex, ValidatorSignature,
	MAX_COALESCE_APPROVALS,
};
use rand::{CryptoRng, Rng, SeedableRng};
use sp_core::{bounded::BoundedVec, ConstU32};
use std::{
	collections::{hash_map, BTreeMap, HashMap, HashSet, VecDeque},
	sync::Arc,
	time::Duration,
};

/// Approval distribution metrics.
pub mod metrics;

#[cfg(test)]
mod tests;

const LOG_TARGET: &str = "parachain::approval-distribution";

const COST_UNEXPECTED_MESSAGE: Rep =
	Rep::CostMinor("Peer sent an out-of-view assignment or approval");
const COST_DUPLICATE_MESSAGE: Rep = Rep::CostMinorRepeated("Peer sent identical messages");
const COST_ASSIGNMENT_TOO_FAR_IN_THE_FUTURE: Rep =
	Rep::CostMinor("The vote was valid but too far in the future");
const COST_INVALID_MESSAGE: Rep = Rep::CostMajor("The vote was bad");
const COST_OVERSIZED_BITFIELD: Rep = Rep::CostMajor("Oversized certificate or candidate bitfield");

const BENEFIT_VALID_MESSAGE: Rep = Rep::BenefitMinor("Peer sent a valid message");
const BENEFIT_VALID_MESSAGE_FIRST: Rep =
	Rep::BenefitMinorFirst("Valid message with new information");

// Maximum valid size for the `CandidateBitfield` in the assignment messages.
const MAX_BITFIELD_SIZE: usize = 500;

/// The Approval Distribution subsystem.
pub struct ApprovalDistribution {
	metrics: Metrics,
	slot_duration_millis: u64,
	clock: Arc<dyn Clock + Send + Sync>,
	assignment_criteria: Arc<dyn AssignmentCriteria + Send + Sync>,
}

/// Contains recently finalized
/// or those pruned due to finalization.
#[derive(Default)]
struct RecentlyOutdated {
	buf: VecDeque<Hash>,
}

impl RecentlyOutdated {
	fn note_outdated(&mut self, hash: Hash) {
		const MAX_BUF_LEN: usize = 20;

		self.buf.push_back(hash);

		while self.buf.len() > MAX_BUF_LEN {
			let _ = self.buf.pop_front();
		}
	}

	fn is_recent_outdated(&self, hash: &Hash) -> bool {
		self.buf.contains(hash)
	}
}

// Contains topology routing information for assignments and approvals.
struct ApprovalRouting {
	required_routing: RequiredRouting,
	local: bool,
	random_routing: RandomRouting,
	peers_randomly_routed: Vec<PeerId>,
}

impl ApprovalRouting {
	fn mark_randomly_sent(&mut self, peer: PeerId) {
		self.random_routing.inc_sent();
		self.peers_randomly_routed.push(peer);
	}
}

// This struct is responsible for tracking the full state of an assignment and grid routing
// information.
struct ApprovalEntry {
	// The assignment certificate.
	assignment: IndirectAssignmentCertV2,
	// The candidates claimed by the certificate. A mapping between bit index and candidate index.
	assignment_claimed_candidates: CandidateBitfield,
	// The approval signatures for each `CandidateIndex` claimed by the assignment certificate.
	approvals: HashMap<CandidateBitfield, IndirectSignedApprovalVoteV2>,
	// The validator index of the assignment signer.
	validator_index: ValidatorIndex,
	// Information required for gossiping to other peers using the grid topology.
	routing_info: ApprovalRouting,
}

#[derive(Debug)]
enum ApprovalEntryError {
	InvalidValidatorIndex,
	CandidateIndexOutOfBounds,
	InvalidCandidateIndex,
	DuplicateApproval,
	UnknownAssignment,
}

impl ApprovalEntry {
	pub fn new(
		assignment: IndirectAssignmentCertV2,
		candidates: CandidateBitfield,
		routing_info: ApprovalRouting,
	) -> ApprovalEntry {
		Self {
			validator_index: assignment.validator,
			assignment,
			approvals: HashMap::new(),
			assignment_claimed_candidates: candidates,
			routing_info,
		}
	}

	// Create a `MessageSubject` to reference the assignment.
	pub fn create_assignment_knowledge(&self, block_hash: Hash) -> (MessageSubject, MessageKind) {
		(
			MessageSubject(
				block_hash,
				self.assignment_claimed_candidates.clone(),
				self.validator_index,
			),
			MessageKind::Assignment,
		)
	}

	// Updates routing information and returns the previous information if any.
	pub fn routing_info_mut(&mut self) -> &mut ApprovalRouting {
		&mut self.routing_info
	}

	// Get the routing information.
	pub fn routing_info(&self) -> &ApprovalRouting {
		&self.routing_info
	}

	// Update routing information.
	pub fn update_required_routing(&mut self, required_routing: RequiredRouting) {
		self.routing_info.required_routing = required_routing;
	}

	// Tells if this entry assignment covers at least one candidate in the approval
	pub fn includes_approval_candidates(&self, approval: &IndirectSignedApprovalVoteV2) -> bool {
		for candidate_index in approval.candidate_indices.iter_ones() {
			if self.assignment_claimed_candidates.bit_at((candidate_index).as_bit_index()) {
				return true;
			}
		}
		return false;
	}

	// Records a new approval. Returns error if the claimed candidate is not found or we already
	// have received the approval.
	pub fn note_approval(
		&mut self,
		approval: IndirectSignedApprovalVoteV2,
	) -> Result<(), ApprovalEntryError> {
		// First do some sanity checks:
		// - check validator index matches
		// - check claimed candidate
		// - check for duplicate approval
		if self.validator_index != approval.validator {
			return Err(ApprovalEntryError::InvalidValidatorIndex);
		}

		// We need at least one of the candidates in the approval to be in this assignment
		if !self.includes_approval_candidates(&approval) {
			return Err(ApprovalEntryError::InvalidCandidateIndex);
		}

		if self.approvals.contains_key(&approval.candidate_indices) {
			return Err(ApprovalEntryError::DuplicateApproval);
		}

		self.approvals.insert(approval.candidate_indices.clone(), approval.clone());
		Ok(())
	}

	// Get the assignment certificate and claimed candidates.
	pub fn assignment(&self) -> (IndirectAssignmentCertV2, CandidateBitfield) {
		(self.assignment.clone(), self.assignment_claimed_candidates.clone())
	}

	// Get all approvals for all candidates claimed by the assignment.
	pub fn approvals(&self) -> Vec<IndirectSignedApprovalVoteV2> {
		self.approvals.values().cloned().collect::<Vec<_>>()
	}

	// Get validator index.
	pub fn validator_index(&self) -> ValidatorIndex {
		self.validator_index
	}
}

// We keep track of each peer view and protocol version using this struct.
struct PeerEntry {
	pub view: View,
	pub version: ProtocolVersion,
}

// In case the original grid topology mechanisms don't work on their own, we need to trade bandwidth
// for protocol liveliness by introducing aggression.
//
// Aggression has 3 levels:
//
//  * Aggression Level 0: The basic behaviors described above.
//  * Aggression Level 1: The originator of a message sends to all peers. Other peers follow the
//    rules above.
//  * Aggression Level 2: All peers send all messages to all their row and column neighbors. This
//    means that each validator will, on average, receive each message approximately `2*sqrt(n)`
//    times.
// The aggression level of messages pertaining to a block increases when that block is unfinalized
// and is a child of the finalized block.
// This means that only one block at a time has its messages propagated with aggression > 0.
//
// A note on aggression thresholds: changes in propagation apply only to blocks which are the
// _direct descendants_ of the finalized block which are older than the given threshold,
// not to all blocks older than the threshold. Most likely, a few assignments struggle to
// be propagated in a single block and this holds up all of its descendants blocks.
// Accordingly, we only step on the gas for the block which is most obviously holding up finality.
/// Aggression configuration representation
#[derive(Clone)]
struct AggressionConfig {
	/// Aggression level 1: all validators send all their own messages to all peers.
	l1_threshold: Option<BlockNumber>,
	/// Aggression level 2: level 1 + all validators send all messages to all peers in the X and Y
	/// dimensions.
	l2_threshold: Option<BlockNumber>,
	/// How often to re-send messages to all targeted recipients.
	/// This applies to all unfinalized blocks.
	resend_unfinalized_period: Option<BlockNumber>,
}

impl AggressionConfig {
	/// Returns `true` if age is past threshold depending on the aggression level
	fn should_trigger_aggression(&self, age: BlockNumber) -> bool {
		if let Some(t) = self.l1_threshold {
			age >= t
		} else if let Some(t) = self.resend_unfinalized_period {
			age > 0 && age.is_multiple_of(t)
		} else {
			false
		}
	}
}

impl Default for AggressionConfig {
	fn default() -> Self {
		AggressionConfig {
			l1_threshold: Some(16),
			l2_threshold: Some(64),
			resend_unfinalized_period: Some(8),
		}
	}
}

#[derive(PartialEq)]
enum Resend {
	Yes,
	No,
}

/// The [`State`] struct is responsible for tracking the overall state of the subsystem.
///
/// It tracks metadata about our view of the unfinalized chain,
/// which assignments and approvals we have seen, and our peers' views.
#[derive(Default)]
pub struct State {
	/// These two fields are used in conjunction to construct a view over the unfinalized chain.
	blocks_by_number: BTreeMap<BlockNumber, Vec<Hash>>,
	blocks: HashMap<Hash, BlockEntry>,

	/// Our view updates to our peers can race with `NewBlocks` updates. We store messages received
	/// against the directly mentioned blocks in our view in this map until `NewBlocks` is
	/// received.
	///
	/// As long as the parent is already in the `blocks` map and `NewBlocks` messages aren't
	/// delayed by more than a block length, this strategy will work well for mitigating the race.
	/// This is also a race that occurs typically on local networks.
	pending_known: HashMap<Hash, Vec<(PeerId, PendingMessage)>>,

	/// Peer data is partially stored here, and partially inline within the [`BlockEntry`]s
	peer_views: HashMap<PeerId, PeerEntry>,

	/// Keeps a topology for various different sessions.
	topologies: SessionGridTopologies,

	/// Tracks recently finalized blocks.
	recent_outdated_blocks: RecentlyOutdated,

	/// Aggression configuration.
	aggression_config: AggressionConfig,

	/// Current approval checking finality lag.
	approval_checking_lag: BlockNumber,

	/// Aggregated reputation change
	reputation: ReputationAggregator,

	/// Slot duration in millis
	slot_duration_millis: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageKind {
	Assignment,
	Approval,
}

// Utility structure to identify assignments and approvals for specific candidates.
// Assignments can span multiple candidates, while approvals refer to only one candidate.
//
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct MessageSubject(Hash, pub CandidateBitfield, ValidatorIndex);

#[derive(Debug, Clone, Default)]
struct Knowledge {
	// When there is no entry, this means the message is unknown
	// When there is an entry with `MessageKind::Assignment`, the assignment is known.
	// When there is an entry with `MessageKind::Approval`, the assignment and approval are known.
	known_messages: HashMap<MessageSubject, MessageKind>,
}

impl Knowledge {
	fn contains(&self, message: &MessageSubject, kind: MessageKind) -> bool {
		match (kind, self.known_messages.get(message)) {
			(_, None) => false,
			(MessageKind::Assignment, Some(_)) => true,
			(MessageKind::Approval, Some(MessageKind::Assignment)) => false,
			(MessageKind::Approval, Some(MessageKind::Approval)) => true,
		}
	}

	fn insert(&mut self, message: MessageSubject, kind: MessageKind) -> bool {
		let mut success = match self.known_messages.entry(message.clone()) {
			hash_map::Entry::Vacant(vacant) => {
				vacant.insert(kind);
				// If there are multiple candidates assigned in the message, create
				// separate entries for each one.
				true
			},
			hash_map::Entry::Occupied(mut occupied) => match (*occupied.get(), kind) {
				(MessageKind::Assignment, MessageKind::Assignment) => false,
				(MessageKind::Approval, MessageKind::Approval) => false,
				(MessageKind::Approval, MessageKind::Assignment) => false,
				(MessageKind::Assignment, MessageKind::Approval) => {
					*occupied.get_mut() = MessageKind::Approval;
					true
				},
			},
		};

		// In case of successful insertion of multiple candidate assignments create additional
		// entries for each assigned candidate. This fakes knowledge of individual assignments, but
		// we need to share the same `MessageSubject` with the followup approval candidate index.
		if kind == MessageKind::Assignment && success && message.1.count_ones() > 1 {
			for candidate_index in message.1.iter_ones() {
				success = success &&
					self.insert(
						MessageSubject(
							message.0,
							vec![candidate_index as u32].try_into().expect("Non-empty vec; qed"),
							message.2,
						),
						kind,
					);
			}
		}
		success
	}
}

/// Information that has been circulated to and from a peer.
#[derive(Debug, Clone, Default)]
struct PeerKnowledge {
	/// The knowledge we've sent to the peer.
	sent: Knowledge,
	/// The knowledge we've received from the peer.
	received: Knowledge,
}

impl PeerKnowledge {
	fn contains(&self, message: &MessageSubject, kind: MessageKind) -> bool {
		self.sent.contains(message, kind) || self.received.contains(message, kind)
	}

	// Generate the knowledge keys for querying if all assignments of an approval are known
	// by this peer.
	fn generate_assignments_keys(
		approval: &IndirectSignedApprovalVoteV2,
	) -> Vec<(MessageSubject, MessageKind)> {
		approval
			.candidate_indices
			.iter_ones()
			.map(|candidate_index| {
				(
					MessageSubject(
						approval.block_hash,
						(candidate_index as CandidateIndex).into(),
						approval.validator,
					),
					MessageKind::Assignment,
				)
			})
			.collect_vec()
	}

	// Generate the knowledge keys for querying if an approval is known by peer.
	fn generate_approval_key(
		approval: &IndirectSignedApprovalVoteV2,
	) -> (MessageSubject, MessageKind) {
		(
			MessageSubject(
				approval.block_hash,
				approval.candidate_indices.clone(),
				approval.validator,
			),
			MessageKind::Approval,
		)
	}
}

/// Information about blocks in our current view as well as whether peers know of them.
struct BlockEntry {
	/// Peers who we know are aware of this block and thus, the candidates within it.
	/// This maps to their knowledge of messages.
	known_by: HashMap<PeerId, PeerKnowledge>,
	/// The number of the block.
	number: BlockNumber,
	/// The parent hash of the block.
	parent_hash: Hash,
	/// Our knowledge of messages.
	knowledge: Knowledge,
	/// A votes entry for each candidate indexed by [`CandidateIndex`].
	candidates: Vec<CandidateEntry>,
	/// Information about candidate metadata.
	candidates_metadata: Vec<(CandidateHash, CoreIndex, GroupIndex)>,
	/// The session index of this block.
	session: SessionIndex,
	/// Approval entries for whole block. These also contain all approvals in the case of multiple
	/// candidates being claimed by assignments.
	approval_entries: HashMap<(ValidatorIndex, CandidateBitfield), ApprovalEntry>,
	/// The block vrf story.
	vrf_story: RelayVRFStory,
	/// The block slot.
	slot: Slot,
	/// Backing off from re-sending messages to peers.
	last_resent_at_block_number: Option<u32>,
}

impl BlockEntry {
	// Returns the peer which currently know this block.
	pub fn known_by(&self) -> Vec<PeerId> {
		self.known_by.keys().cloned().collect::<Vec<_>>()
	}

	pub fn insert_approval_entry(&mut self, entry: ApprovalEntry) -> &mut ApprovalEntry {
		// First map one entry per candidate to the same key we will use in `approval_entries`.
		// Key is (Validator_index, CandidateBitfield) that links the `ApprovalEntry` to the (K,V)
		// entry in `candidate_entry.messages`.
		for claimed_candidate_index in entry.assignment_claimed_candidates.iter_ones() {
			match self.candidates.get_mut(claimed_candidate_index) {
				Some(candidate_entry) => {
					candidate_entry
						.assignments
						.entry(entry.validator_index())
						.or_insert(entry.assignment_claimed_candidates.clone());
				},
				None => {
					// This should never happen, but if it happens, it means the subsystem is
					// broken.
					gum::warn!(
						target: LOG_TARGET,
						hash = ?entry.assignment.block_hash,
						?claimed_candidate_index,
						"Missing candidate entry on `import_and_circulate_assignment`",
					);
				},
			};
		}

		self.approval_entries
			.entry((entry.validator_index, entry.assignment_claimed_candidates.clone()))
			.or_insert(entry)
	}

	// Tels if all candidate_indices are valid candidates
	pub fn contains_candidates(&self, candidate_indices: &CandidateBitfield) -> bool {
		candidate_indices
			.iter_ones()
			.all(|candidate_index| self.candidates.get(candidate_index as usize).is_some())
	}

	// Saves the given approval in all ApprovalEntries that contain an assignment for any of the
	// candidates in the approval.
	//
	// Returns the required routing needed for this approval and the lit of random peers the
	// covering assignments were sent.
	pub fn note_approval(
		&mut self,
		approval: IndirectSignedApprovalVoteV2,
	) -> Result<(RequiredRouting, HashSet<PeerId>), ApprovalEntryError> {
		let mut required_routing: Option<RequiredRouting> = None;
		let mut peers_randomly_routed_to = HashSet::new();

		if self.candidates.len() < approval.candidate_indices.len() as usize {
			return Err(ApprovalEntryError::CandidateIndexOutOfBounds);
		}

		// First determine all assignments bitfields that might be covered by this approval
		let covered_assignments_bitfields: HashSet<CandidateBitfield> = approval
			.candidate_indices
			.iter_ones()
			.filter_map(|candidate_index| {
				self.candidates.get_mut(candidate_index).map_or(None, |candidate_entry| {
					candidate_entry.assignments.get(&approval.validator).cloned()
				})
			})
			.collect();

		// Mark the vote in all approval entries
		for assignment_bitfield in covered_assignments_bitfields {
			if let Some(approval_entry) =
				self.approval_entries.get_mut(&(approval.validator, assignment_bitfield))
			{
				approval_entry.note_approval(approval.clone())?;
				peers_randomly_routed_to
					.extend(approval_entry.routing_info().peers_randomly_routed.iter());

				if let Some(current_required_routing) = required_routing {
					required_routing = Some(
						current_required_routing
							.combine(approval_entry.routing_info().required_routing),
					);
				} else {
					required_routing = Some(approval_entry.routing_info().required_routing)
				}
			}
		}

		if let Some(required_routing) = required_routing {
			Ok((required_routing, peers_randomly_routed_to))
		} else {
			Err(ApprovalEntryError::UnknownAssignment)
		}
	}

	/// Returns the list of approval votes covering this candidate
	pub fn approval_votes(
		&self,
		candidate_index: CandidateIndex,
	) -> Vec<IndirectSignedApprovalVoteV2> {
		let result: Option<
			HashMap<(ValidatorIndex, CandidateBitfield), IndirectSignedApprovalVoteV2>,
		> = self.candidates.get(candidate_index as usize).map(|candidate_entry| {
			candidate_entry
				.assignments
				.iter()
				.filter_map(|(validator, assignment_bitfield)| {
					self.approval_entries.get(&(*validator, assignment_bitfield.clone()))
				})
				.flat_map(|approval_entry| {
					approval_entry
						.approvals
						.clone()
						.into_iter()
						.filter(|(approved_candidates, _)| {
							approved_candidates.bit_at(candidate_index.as_bit_index())
						})
						.map(|(approved_candidates, vote)| {
							((approval_entry.validator_index, approved_candidates), vote)
						})
				})
				.collect()
		});

		result.map(|result| result.into_values().collect_vec()).unwrap_or_default()
	}
}

// Information about candidates in the context of a particular block they are included in.
// In other words, multiple `CandidateEntry`s may exist for the same candidate,
// if it is included by multiple blocks - this is likely the case when there are forks.
#[derive(Debug, Default)]
struct CandidateEntry {
	// The value represents part of the lookup key in `approval_entries` to fetch the assignment
	// and existing votes.
	assignments: HashMap<ValidatorIndex, CandidateBitfield>,
}

#[derive(Debug, Clone, PartialEq)]
enum MessageSource {
	Peer(PeerId),
	Local,
}

// Encountered error while validating an assignment.
#[derive(Debug)]
enum InvalidAssignmentError {
	// The vrf check for the assignment failed.
	#[allow(dead_code)]
	CryptoCheckFailed(InvalidAssignment),
	// The assignment did not claim any valid candidate.
	NoClaimedCandidates,
	// Claimed invalid candidate.
	#[allow(dead_code)]
	ClaimedInvalidCandidateIndex {
		claimed_index: usize,
		max_index: usize,
	},
	// The assignment claimes more candidates than the maximum allowed.
	OversizedClaimedBitfield,
	// `SessionInfo`  was not found for the block hash in the assignment.
	#[allow(dead_code)]
	SessionInfoNotFound(polkadot_node_subsystem_util::runtime::Error),
}

// Encountered error while validating an approval.
#[derive(Debug)]
enum InvalidVoteError {
	// The candidate index was out of bounds.
	CandidateIndexOutOfBounds,
	// The candidate hash was not found in the block's candidate list.
	CandidateHashNotFound,
	// The validator index was out of bounds.
	ValidatorIndexOutOfBounds,
	// The signature of the vote was invalid.
	InvalidSignature,
	// The approval coalesces more candidates than the allowed maximum.
	TooManyCandidates,
	// `SessionInfo` was not found for the block hash in the approval.
	#[allow(dead_code)]
	SessionInfoNotFound(polkadot_node_subsystem_util::runtime::Error),
}

impl MessageSource {
	fn peer_id(&self) -> Option<PeerId> {
		match self {
			Self::Peer(id) => Some(*id),
			Self::Local => None,
		}
	}
}

enum PendingMessage {
	Assignment(IndirectAssignmentCertV2, CandidateBitfield),
	Approval(IndirectSignedApprovalVoteV2),
}

#[overseer::contextbounds(ApprovalDistribution, prefix = self::overseer)]
impl State {
	/// Build State with specified slot duration.
	pub fn with_config(slot_duration_millis: u64) -> Self {
		Self { slot_duration_millis, ..Default::default() }
	}

	async fn handle_network_msg<
		N: overseer::SubsystemSender<NetworkBridgeTxMessage>,
		A: overseer::SubsystemSender<ApprovalVotingMessage>,
		RA: overseer::SubsystemSender<RuntimeApiMessage>,
	>(
		&mut self,
		approval_voting_sender: &mut A,
		network_sender: &mut N,
		runtime_api_sender: &mut RA,
		metrics: &Metrics,
		event: NetworkBridgeEvent<net_protocol::ApprovalDistributionMessage>,
		rng: &mut (impl CryptoRng + Rng),
		assignment_criteria: &(impl AssignmentCriteria + ?Sized),
		clock: &(impl Clock + ?Sized),
		session_info_provider: &mut RuntimeInfo,
	) {
		match event {
			NetworkBridgeEvent::PeerConnected(peer_id, role, version, authority_ids) => {
				gum::trace!(target: LOG_TARGET, ?peer_id, ?role, ?authority_ids, "Peer connected");
				if let Some(authority_ids) = authority_ids {
					self.topologies.update_authority_ids(peer_id, &authority_ids);
				}
				// insert a blank view if none already present
				self.peer_views
					.entry(peer_id)
					.or_insert(PeerEntry { view: Default::default(), version });
			},
			NetworkBridgeEvent::PeerDisconnected(peer_id) => {
				gum::trace!(target: LOG_TARGET, ?peer_id, "Peer disconnected");
				self.peer_views.remove(&peer_id);
				self.blocks.iter_mut().for_each(|(_hash, entry)| {
					entry.known_by.remove(&peer_id);
				})
			},
			NetworkBridgeEvent::NewGossipTopology(topology) => {
				self.handle_new_session_topology(
					network_sender,
					topology.session,
					topology.topology,
					topology.local_index,
				)
				.await;
			},
			NetworkBridgeEvent::PeerViewChange(peer_id, view) => {
				self.handle_peer_view_change(network_sender, metrics, peer_id, view, rng).await;
			},
			NetworkBridgeEvent::OurViewChange(view) => {
				gum::trace!(target: LOG_TARGET, ?view, "Own view change");
				for head in view.iter() {
					if !self.blocks.contains_key(head) {
						self.pending_known.entry(*head).or_default();
					}
				}

				self.pending_known.retain(|h, _| {
					let live = view.contains(h);
					if !live {
						gum::trace!(
							target: LOG_TARGET,
							block_hash = ?h,
							"Cleaning up stale pending messages",
						);
					}
					live
				});
			},
			NetworkBridgeEvent::PeerMessage(peer_id, message) => {
				self.process_incoming_peer_message(
					approval_voting_sender,
					network_sender,
					runtime_api_sender,
					metrics,
					peer_id,
					message,
					rng,
					assignment_criteria,
					clock,
					session_info_provider,
				)
				.await;
			},
			NetworkBridgeEvent::UpdatedAuthorityIds(peer_id, authority_ids) => {
				gum::debug!(target: LOG_TARGET, ?peer_id, ?authority_ids, "Update Authority Ids");
				// If we learn about a new PeerId for an authority ids we need to try to route the
				// messages that should have sent to that validator according to the topology.
				if self.topologies.update_authority_ids(peer_id, &authority_ids) {
					if let Some(PeerEntry { view, version }) = self.peer_views.get(&peer_id) {
						let intersection = self
							.blocks_by_number
							.iter()
							.filter(|(block_number, _)| *block_number > &view.finalized_number)
							.flat_map(|(_, hashes)| {
								hashes.iter().filter(|hash| {
									self.blocks
										.get(&hash)
										.map(|block| block.known_by.get(&peer_id).is_some())
										.unwrap_or_default()
								})
							});
						let view_intersection =
							View::new(intersection.cloned(), view.finalized_number);
						Self::unify_with_peer(
							network_sender,
							metrics,
							&mut self.blocks,
							&self.topologies,
							self.peer_views.len(),
							peer_id,
							*version,
							view_intersection,
							rng,
							true,
						)
						.await;
					}
				}
			},
		}
	}

	async fn handle_new_blocks<
		N: overseer::SubsystemSender<NetworkBridgeTxMessage>,
		A: overseer::SubsystemSender<ApprovalVotingMessage>,
		RA: overseer::SubsystemSender<RuntimeApiMessage>,
	>(
		&mut self,
		approval_voting_sender: &mut A,
		network_sender: &mut N,
		runtime_api_sender: &mut RA,
		metrics: &Metrics,
		metas: Vec<BlockApprovalMeta>,
		rng: &mut (impl CryptoRng + Rng),
		assignment_criteria: &(impl AssignmentCriteria + ?Sized),
		clock: &(impl Clock + ?Sized),
		session_info_provider: &mut RuntimeInfo,
	) {
		let mut new_hashes = HashSet::new();

		gum::debug!(
			target: LOG_TARGET,
			"Got new blocks {:?}",
			metas.iter().map(|m| (m.hash, m.number)).collect::<Vec<_>>(),
		);

		for meta in metas {
			match self.blocks.entry(meta.hash) {
				hash_map::Entry::Vacant(entry) => {
					let candidates_count = meta.candidates.len();
					let mut candidates = Vec::with_capacity(candidates_count);
					candidates.resize_with(candidates_count, Default::default);

					entry.insert(BlockEntry {
						known_by: HashMap::new(),
						number: meta.number,
						parent_hash: meta.parent_hash,
						knowledge: Knowledge::default(),
						candidates,
						session: meta.session,
						approval_entries: HashMap::new(),
						candidates_metadata: meta.candidates,
						vrf_story: meta.vrf_story,
						slot: meta.slot,
						last_resent_at_block_number: None,
					});

					self.topologies.inc_session_refs(meta.session);

					new_hashes.insert(meta.hash);

					// In case there are duplicates, we should only set this if the entry
					// was vacant.
					self.blocks_by_number.entry(meta.number).or_default().push(meta.hash);
				},
				_ => continue,
			}
		}

		{
			for (peer_id, PeerEntry { view, version }) in self.peer_views.iter() {
				let intersection = view.iter().filter(|h| new_hashes.contains(h));
				let view_intersection = View::new(intersection.cloned(), view.finalized_number);
				Self::unify_with_peer(
					network_sender,
					metrics,
					&mut self.blocks,
					&self.topologies,
					self.peer_views.len(),
					*peer_id,
					*version,
					view_intersection,
					rng,
					false,
				)
				.await;
			}

			let pending_now_known = self
				.pending_known
				.keys()
				.filter(|k| self.blocks.contains_key(k))
				.copied()
				.collect::<Vec<_>>();

			let to_import = pending_now_known
				.into_iter()
				.inspect(|h| {
					gum::trace!(
						target: LOG_TARGET,
						block_hash = ?h,
						"Extracting pending messages for new block"
					)
				})
				.filter_map(|k| self.pending_known.remove(&k))
				.flatten()
				.collect::<Vec<_>>();

			if !to_import.is_empty() {
				gum::debug!(
					target: LOG_TARGET,
					num = to_import.len(),
					"Processing pending assignment/approvals",
				);

				let _timer = metrics.time_import_pending_now_known();

				for (peer_id, message) in to_import {
					match message {
						PendingMessage::Assignment(assignment, claimed_indices) => {
							self.import_and_circulate_assignment(
								approval_voting_sender,
								network_sender,
								runtime_api_sender,
								metrics,
								MessageSource::Peer(peer_id),
								assignment,
								claimed_indices,
								rng,
								assignment_criteria,
								clock,
								session_info_provider,
							)
							.await;
						},
						PendingMessage::Approval(approval_vote) => {
							self.import_and_circulate_approval(
								approval_voting_sender,
								network_sender,
								runtime_api_sender,
								metrics,
								MessageSource::Peer(peer_id),
								approval_vote,
								session_info_provider,
							)
							.await;
						},
					}
				}
			}
		}

		self.enable_aggression(network_sender, Resend::Yes, metrics).await;
	}

	async fn handle_new_session_topology<N: overseer::SubsystemSender<NetworkBridgeTxMessage>>(
		&mut self,
		network_sender: &mut N,
		session: SessionIndex,
		topology: SessionGridTopology,
		local_index: Option<ValidatorIndex>,
	) {
		if local_index.is_none() {
			// this subsystem only matters to validators.
			return;
		}

		self.topologies.insert_topology(session, topology, local_index);
		let topology = self.topologies.get_topology(session).expect("just inserted above; qed");

		adjust_required_routing_and_propagate(
			network_sender,
			&mut self.blocks,
			&self.topologies,
			|block_entry| block_entry.session == session,
			|required_routing, local, validator_index| {
				if required_routing == &RequiredRouting::PendingTopology {
					topology
						.local_grid_neighbors()
						.required_routing_by_index(*validator_index, local)
				} else {
					*required_routing
				}
			},
			&self.peer_views,
		)
		.await;
	}

	async fn process_incoming_assignments<A, N, R, RA>(
		&mut self,
		approval_voting_sender: &mut A,
		network_sender: &mut N,
		runtime_api_sender: &mut RA,
		metrics: &Metrics,
		peer_id: PeerId,
		assignments: Vec<(IndirectAssignmentCertV2, CandidateBitfield)>,
		rng: &mut R,
		assignment_criteria: &(impl AssignmentCriteria + ?Sized),
		clock: &(impl Clock + ?Sized),
		session_info_provider: &mut RuntimeInfo,
	) where
		A: overseer::SubsystemSender<ApprovalVotingMessage>,
		N: overseer::SubsystemSender<NetworkBridgeTxMessage>,
		RA: overseer::SubsystemSender<RuntimeApiMessage>,
		R: CryptoRng + Rng,
	{
		for (assignment, claimed_indices) in assignments {
			if let Some(pending) = self.pending_known.get_mut(&assignment.block_hash) {
				let block_hash = &assignment.block_hash;
				let validator_index = assignment.validator;

				gum::trace!(
					target: LOG_TARGET,
					%peer_id,
					?block_hash,
					?claimed_indices,
					?validator_index,
					"Pending assignment",
				);

				pending.push((peer_id, PendingMessage::Assignment(assignment, claimed_indices)));

				continue;
			}

			self.import_and_circulate_assignment(
				approval_voting_sender,
				network_sender,
				runtime_api_sender,
				metrics,
				MessageSource::Peer(peer_id),
				assignment,
				claimed_indices,
				rng,
				assignment_criteria,
				clock,
				session_info_provider,
			)
			.await;
		}
	}

	// Entry point for processing an approval coming from a peer.
	async fn process_incoming_approvals<
		N: overseer::SubsystemSender<NetworkBridgeTxMessage>,
		A: overseer::SubsystemSender<ApprovalVotingMessage>,
		RA: overseer::SubsystemSender<RuntimeApiMessage>,
	>(
		&mut self,
		approval_voting_sender: &mut A,
		network_sender: &mut N,
		runtime_api_sender: &mut RA,
		metrics: &Metrics,
		peer_id: PeerId,
		approvals: Vec<IndirectSignedApprovalVoteV2>,
		session_info_provider: &mut RuntimeInfo,
	) {
		gum::trace!(
			target: LOG_TARGET,
			peer_id = %peer_id,
			num = approvals.len(),
			"Processing approvals from a peer",
		);
		for approval_vote in approvals.into_iter() {
			if let Some(pending) = self.pending_known.get_mut(&approval_vote.block_hash) {
				let block_hash = approval_vote.block_hash;
				let validator_index = approval_vote.validator;

				gum::trace!(
					target: LOG_TARGET,
					%peer_id,
					?block_hash,
					?validator_index,
					"Pending assignment candidates {:?}",
					approval_vote.candidate_indices,
				);

				pending.push((peer_id, PendingMessage::Approval(approval_vote)));

				continue;
			}

			self.import_and_circulate_approval(
				approval_voting_sender,
				network_sender,
				runtime_api_sender,
				metrics,
				MessageSource::Peer(peer_id),
				approval_vote,
				session_info_provider,
			)
			.await;
		}
	}

	async fn process_incoming_peer_message<A, N, RA, R>(
		&mut self,
		approval_voting_sender: &mut A,
		network_sender: &mut N,
		runtime_api_sender: &mut RA,
		metrics: &Metrics,
		peer_id: PeerId,
		msg: ValidationProtocols<protocol_v3::ApprovalDistributionMessage>,
		rng: &mut R,
		assignment_criteria: &(impl AssignmentCriteria + ?Sized),
		clock: &(impl Clock + ?Sized),
		session_info_provider: &mut RuntimeInfo,
	) where
		A: overseer::SubsystemSender<ApprovalVotingMessage>,
		N: overseer::SubsystemSender<NetworkBridgeTxMessage>,
		RA: overseer::SubsystemSender<RuntimeApiMessage>,
		R: CryptoRng + Rng,
	{
		match msg {
			ValidationProtocols::V3(protocol_v3::ApprovalDistributionMessage::Assignments(
				assignments,
			)) => {
				gum::trace!(
					target: LOG_TARGET,
					peer_id = %peer_id,
					num = assignments.len(),
					"Processing assignments from a peer",
				);
				let sanitized_assignments =
					self.sanitize_v2_assignments(peer_id, network_sender, assignments).await;

				self.process_incoming_assignments(
					approval_voting_sender,
					network_sender,
					runtime_api_sender,
					metrics,
					peer_id,
					sanitized_assignments,
					rng,
					assignment_criteria,
					clock,
					session_info_provider,
				)
				.await;
			},
			ValidationProtocols::V3(protocol_v3::ApprovalDistributionMessage::Approvals(
				approvals,
			)) => {
				let sanitized_approvals =
					self.sanitize_v2_approvals(peer_id, network_sender, approvals).await;
				self.process_incoming_approvals(
					approval_voting_sender,
					network_sender,
					runtime_api_sender,
					metrics,
					peer_id,
					sanitized_approvals,
					session_info_provider,
				)
				.await;
			},
		}
	}

	// handle a peer view change: requires that the peer is already connected
	// and has an entry in the `PeerData` struct.
	async fn handle_peer_view_change<N: overseer::SubsystemSender<NetworkBridgeTxMessage>, R>(
		&mut self,
		network_sender: &mut N,
		metrics: &Metrics,
		peer_id: PeerId,
		view: View,
		rng: &mut R,
	) where
		R: CryptoRng + Rng,
	{
		gum::trace!(target: LOG_TARGET, ?view, "Peer view change");
		let finalized_number = view.finalized_number;

		let (old_view, protocol_version) =
			if let Some(peer_entry) = self.peer_views.get_mut(&peer_id) {
				(Some(std::mem::replace(&mut peer_entry.view, view.clone())), peer_entry.version)
			} else {
				// This shouldn't happen, but if it does we assume protocol version 3.
				gum::warn!(
					target: LOG_TARGET,
					?peer_id,
					?view,
					"Peer view change for missing `peer_entry`"
				);

				(None, ValidationVersion::V3.into())
			};

		let old_finalized_number = old_view.map(|v| v.finalized_number).unwrap_or(0);

		// we want to prune every block known_by peer up to (including) view.finalized_number
		let blocks = &mut self.blocks;
		// the `BTreeMap::range` is constrained by stored keys
		// so the loop won't take ages if the new finalized_number skyrockets
		// but we need to make sure the range is not empty, otherwise it will panic
		// it shouldn't be, we make sure of this in the network bridge
		let range = old_finalized_number..=finalized_number;
		if !range.is_empty() && !blocks.is_empty() {
			self.blocks_by_number
				.range(range)
				.flat_map(|(_number, hashes)| hashes)
				.for_each(|hash| {
					if let Some(entry) = blocks.get_mut(hash) {
						entry.known_by.remove(&peer_id);
					}
				});
		}

		Self::unify_with_peer(
			network_sender,
			metrics,
			&mut self.blocks,
			&self.topologies,
			self.peer_views.len(),
			peer_id,
			protocol_version,
			view,
			rng,
			false,
		)
		.await;
	}

	async fn handle_block_finalized<N: overseer::SubsystemSender<NetworkBridgeTxMessage>>(
		&mut self,
		network_sender: &mut N,
		metrics: &Metrics,
		finalized_number: BlockNumber,
	) {
		// we want to prune every block up to (including) finalized_number
		// why +1 here?
		// split_off returns everything after the given key, including the key
		let split_point = finalized_number.saturating_add(1);
		let mut old_blocks = self.blocks_by_number.split_off(&split_point);

		// after split_off old_blocks actually contains new blocks, we need to swap
		std::mem::swap(&mut self.blocks_by_number, &mut old_blocks);

		// now that we pruned `self.blocks_by_number`, let's clean up `self.blocks` too
		old_blocks.values().flatten().for_each(|relay_block| {
			self.recent_outdated_blocks.note_outdated(*relay_block);
			if let Some(block_entry) = self.blocks.remove(relay_block) {
				self.topologies.dec_session_refs(block_entry.session);
			}
		});

		// If a block was finalized, this means we may need to move our aggression
		// forward to the now oldest block(s).
		self.enable_aggression(network_sender, Resend::No, metrics).await;
	}

	// When finality is lagging as a last resort nodes start sending the messages they have
	// multiples times. This means it is safe to accept duplicate messages without punishing the
	// peer and reduce the reputation and can end up banning the Peer, which in turn will create
	// more no-shows.
	fn accept_duplicates_from_validators(
		blocks_by_number: &BTreeMap<BlockNumber, Vec<Hash>>,
		topologies: &SessionGridTopologies,
		aggression_config: &AggressionConfig,
		entry: &BlockEntry,
		peer: PeerId,
	) -> bool {
		let topology = topologies.get_topology(entry.session);
		let min_age = blocks_by_number.iter().next().map(|(num, _)| num);
		let max_age = blocks_by_number.iter().rev().next().map(|(num, _)| num);

		// Return if we don't have at least 1 block.
		let (min_age, max_age) = match (min_age, max_age) {
			(Some(min), Some(max)) => (*min, *max),
			_ => return false,
		};

		let age = max_age.saturating_sub(min_age);

		aggression_config.should_trigger_aggression(age) &&
			topology.map(|topology| topology.is_validator(&peer)).unwrap_or(false)
	}

	async fn import_and_circulate_assignment<A, N, RA, R>(
		&mut self,
		approval_voting_sender: &mut A,
		network_sender: &mut N,
		runtime_api_sender: &mut RA,
		metrics: &Metrics,
		source: MessageSource,
		assignment: IndirectAssignmentCertV2,
		claimed_candidate_indices: CandidateBitfield,
		rng: &mut R,
		assignment_criteria: &(impl AssignmentCriteria + ?Sized),
		clock: &(impl Clock + ?Sized),
		session_info_provider: &mut RuntimeInfo,
	) where
		A: overseer::SubsystemSender<ApprovalVotingMessage>,
		N: overseer::SubsystemSender<NetworkBridgeTxMessage>,
		RA: overseer::SubsystemSender<RuntimeApiMessage>,
		R: CryptoRng + Rng,
	{
		let block_hash = assignment.block_hash;
		let validator_index = assignment.validator;

		let entry = match self.blocks.get_mut(&block_hash) {
			Some(entry) => entry,
			None => {
				if let Some(peer_id) = source.peer_id() {
					gum::trace!(
						target: LOG_TARGET,
						?peer_id,
						hash = ?block_hash,
						?validator_index,
						"Unexpected assignment",
					);
					if !self.recent_outdated_blocks.is_recent_outdated(&block_hash) {
						modify_reputation(
							&mut self.reputation,
							network_sender,
							peer_id,
							COST_UNEXPECTED_MESSAGE,
						)
						.await;
						gum::debug!(target: LOG_TARGET, "Received assignment for invalid block");
						metrics.on_assignment_recent_outdated();
					}
				}
				metrics.on_assignment_invalid_block();
				return;
			},
		};

		// Compute metadata on the assignment.
		let (message_subject, message_kind) = (
			MessageSubject(block_hash, claimed_candidate_indices.clone(), validator_index),
			MessageKind::Assignment,
		);

		if let Some(peer_id) = source.peer_id() {
			// check if our knowledge of the peer already contains this assignment
			match entry.known_by.entry(peer_id) {
				hash_map::Entry::Occupied(mut peer_knowledge) => {
					let peer_knowledge = peer_knowledge.get_mut();
					if peer_knowledge.contains(&message_subject, message_kind) {
						// wasn't included before
						if !peer_knowledge.received.insert(message_subject.clone(), message_kind) {
							if !Self::accept_duplicates_from_validators(
								&self.blocks_by_number,
								&self.topologies,
								&self.aggression_config,
								entry,
								peer_id,
							) {
								gum::debug!(
									target: LOG_TARGET,
									?peer_id,
									?message_subject,
									"Duplicate assignment",
								);

								modify_reputation(
									&mut self.reputation,
									network_sender,
									peer_id,
									COST_DUPLICATE_MESSAGE,
								)
								.await;
							}

							metrics.on_assignment_duplicate();
						} else {
							gum::trace!(
								target: LOG_TARGET,
								?peer_id,
								hash = ?block_hash,
								?validator_index,
								?message_subject,
								"We sent the message to the peer while peer was sending it to us. Known race condition.",
							);
						}
						return;
					}
				},
				hash_map::Entry::Vacant(_) => {
					gum::debug!(
						target: LOG_TARGET,
						?peer_id,
						?message_subject,
						"Assignment from a peer is out of view",
					);
					modify_reputation(
						&mut self.reputation,
						network_sender,
						peer_id,
						COST_UNEXPECTED_MESSAGE,
					)
					.await;
					metrics.on_assignment_out_of_view();
				},
			}

			// if the assignment is known to be valid, reward the peer
			if entry.knowledge.contains(&message_subject, message_kind) {
				modify_reputation(
					&mut self.reputation,
					network_sender,
					peer_id,
					BENEFIT_VALID_MESSAGE,
				)
				.await;
				if let Some(peer_knowledge) = entry.known_by.get_mut(&peer_id) {
					gum::trace!(target: LOG_TARGET, ?peer_id, ?message_subject, "Known assignment");
					peer_knowledge.received.insert(message_subject, message_kind);
				}
				metrics.on_assignment_good_known();
				return;
			}

			let result = Self::check_assignment_valid(
				assignment_criteria,
				&entry,
				&assignment,
				&claimed_candidate_indices,
				session_info_provider,
				runtime_api_sender,
			)
			.await;

			match result {
				Ok(checked_assignment) => {
					let current_tranche = clock.tranche_now(self.slot_duration_millis, entry.slot);
					let too_far_in_future =
						current_tranche + TICK_TOO_FAR_IN_FUTURE as DelayTranche;

					if checked_assignment.tranche() >= too_far_in_future {
						gum::debug!(
							target: LOG_TARGET,
							hash = ?block_hash,
							?peer_id,
							"Got an assignment too far in the future",
						);
						modify_reputation(
							&mut self.reputation,
							network_sender,
							peer_id,
							COST_ASSIGNMENT_TOO_FAR_IN_THE_FUTURE,
						)
						.await;
						metrics.on_assignment_far();

						return;
					}

					approval_voting_sender
						.send_message(ApprovalVotingMessage::ImportAssignment(
							checked_assignment,
							None,
						))
						.await;
					modify_reputation(
						&mut self.reputation,
						network_sender,
						peer_id,
						BENEFIT_VALID_MESSAGE_FIRST,
					)
					.await;
					entry.knowledge.insert(message_subject.clone(), message_kind);
					if let Some(peer_knowledge) = entry.known_by.get_mut(&peer_id) {
						peer_knowledge.received.insert(message_subject.clone(), message_kind);
					}
				},
				Err(error) => {
					gum::info!(
						target: LOG_TARGET,
						hash = ?block_hash,
						?peer_id,
						?error,
						"Got a bad assignment from peer",
					);
					modify_reputation(
						&mut self.reputation,
						network_sender,
						peer_id,
						COST_INVALID_MESSAGE,
					)
					.await;
					metrics.on_assignment_bad();
					return;
				},
			}
		} else {
			if !entry.knowledge.insert(message_subject.clone(), message_kind) {
				// if we already imported an assignment, there is no need to distribute it again
				gum::warn!(
					target: LOG_TARGET,
					?message_subject,
					"Importing locally an already known assignment",
				);
				return;
			} else {
				gum::debug!(
					target: LOG_TARGET,
					?message_subject,
					"Importing locally a new assignment",
				);
			}
		}

		// Invariant: to our knowledge, none of the peers except for the `source` know about the
		// assignment.
		metrics.on_assignment_imported(&assignment.cert.kind);

		let topology = self.topologies.get_topology(entry.session);
		let local = source == MessageSource::Local;

		let required_routing = topology.map_or(RequiredRouting::PendingTopology, |t| {
			t.local_grid_neighbors().required_routing_by_index(validator_index, local)
		});
		// Peers that we will send the assignment to.
		let mut peers = HashSet::new();

		let peers_to_route_to = topology
			.as_ref()
			.map(|t| t.peers_to_route(required_routing))
			.unwrap_or_default();

		for peer in peers_to_route_to {
			if !entry.known_by.contains_key(&peer) {
				continue;
			}

			peers.insert(peer);
		}

		// All the peers that know the relay chain block.
		let peers_to_filter = entry.known_by();

		let approval_entry = entry.insert_approval_entry(ApprovalEntry::new(
			assignment.clone(),
			claimed_candidate_indices.clone(),
			ApprovalRouting {
				required_routing,
				local,
				random_routing: Default::default(),
				peers_randomly_routed: Default::default(),
			},
		));

		// Dispatch the message to all peers in the routing set which
		// know the block.
		//
		// If the topology isn't known yet (race with networking subsystems)
		// then messages will be sent when we get it.

		let assignments = vec![(assignment, claimed_candidate_indices.clone())];
		let n_peers_total = self.peer_views.len();
		let source_peer = source.peer_id();

		// Filter destination peers
		for peer in peers_to_filter.into_iter() {
			if Some(peer) == source_peer {
				continue;
			}

			if peers.contains(&peer) {
				continue;
			}

			if !topology.map(|topology| topology.is_validator(&peer)).unwrap_or(false) {
				continue;
			}

			// Note: at this point, we haven't received the message from any peers
			// other than the source peer, and we just got it, so we haven't sent it
			// to any peers either.
			let route_random =
				approval_entry.routing_info().random_routing.sample(n_peers_total, rng);

			if route_random {
				approval_entry.routing_info_mut().mark_randomly_sent(peer);
				peers.insert(peer);
			}

			if approval_entry.routing_info().random_routing.is_complete() {
				break;
			}
		}

		// Add the metadata of the assignment to the knowledge of each peer.
		for peer in peers.iter() {
			// we already filtered peers above, so this should always be Some
			if let Some(peer_knowledge) = entry.known_by.get_mut(peer) {
				peer_knowledge.sent.insert(message_subject.clone(), message_kind);
			}
		}

		if !peers.is_empty() {
			gum::trace!(
				target: LOG_TARGET,
				?block_hash,
				?claimed_candidate_indices,
				local = source.peer_id().is_none(),
				num_peers = peers.len(),
				"Sending an assignment to peers",
			);

			let peers = peers
				.iter()
				.filter_map(|peer_id| {
					self.peer_views.get(peer_id).map(|peer_entry| (*peer_id, peer_entry.version))
				})
				.collect::<Vec<_>>();

			send_assignments_batched(network_sender, assignments, &peers).await;
		}
	}

	async fn check_assignment_valid<RA: overseer::SubsystemSender<RuntimeApiMessage>>(
		assignment_criteria: &(impl AssignmentCriteria + ?Sized),
		entry: &BlockEntry,
		assignment: &IndirectAssignmentCertV2,
		claimed_candidate_indices: &CandidateBitfield,
		runtime_info: &mut RuntimeInfo,
		runtime_api_sender: &mut RA,
	) -> Result<CheckedIndirectAssignment, InvalidAssignmentError> {
		let ExtendedSessionInfo { ref session_info, .. } = runtime_info
			.get_session_info_by_index(runtime_api_sender, assignment.block_hash, entry.session)
			.await
			.map_err(|err| InvalidAssignmentError::SessionInfoNotFound(err))?;

		if claimed_candidate_indices.len() > session_info.n_cores as usize {
			return Err(InvalidAssignmentError::OversizedClaimedBitfield);
		}

		let claimed_cores: Vec<CoreIndex> = claimed_candidate_indices
			.iter_ones()
			.map(|candidate_index| {
				entry.candidates_metadata.get(candidate_index).map(|(_, core, _)| *core).ok_or(
					InvalidAssignmentError::ClaimedInvalidCandidateIndex {
						claimed_index: candidate_index,
						max_index: entry.candidates_metadata.len(),
					},
				)
			})
			.collect::<Result<Vec<_>, InvalidAssignmentError>>()?;

		let Ok(claimed_cores) = claimed_cores.try_into() else {
			return Err(InvalidAssignmentError::NoClaimedCandidates);
		};

		let backing_groups = claimed_candidate_indices
			.iter_ones()
			.flat_map(|candidate_index| {
				entry.candidates_metadata.get(candidate_index).map(|(_, _, group)| *group)
			})
			.collect::<Vec<_>>();

		assignment_criteria
			.check_assignment_cert(
				claimed_cores,
				assignment.validator,
				&polkadot_node_primitives::approval::criteria::Config::from(session_info),
				entry.vrf_story.clone(),
				&assignment.cert,
				backing_groups,
			)
			.map_err(|err| InvalidAssignmentError::CryptoCheckFailed(err))
			.map(|tranche| {
				CheckedIndirectAssignment::from_checked(
					assignment.clone(),
					claimed_candidate_indices.clone(),
					tranche,
				)
			})
	}
	// Checks if an approval can be processed.
	// Returns true if we can continue with processing the approval and false otherwise.
	async fn check_approval_can_be_processed<
		N: overseer::SubsystemSender<NetworkBridgeTxMessage>,
	>(
		network_sender: &mut N,
		assignments_knowledge_key: &Vec<(MessageSubject, MessageKind)>,
		approval_knowledge_key: &(MessageSubject, MessageKind),
		entry: &mut BlockEntry,
		blocks_by_number: &BTreeMap<BlockNumber, Vec<Hash>>,
		topologies: &SessionGridTopologies,
		aggression_config: &AggressionConfig,
		reputation: &mut ReputationAggregator,
		peer_id: PeerId,
		metrics: &Metrics,
	) -> bool {
		for message_subject in assignments_knowledge_key {
			if !entry.knowledge.contains(&message_subject.0, message_subject.1) {
				gum::trace!(
					target: LOG_TARGET,
					?peer_id,
					?message_subject,
					"Unknown approval assignment",
				);
				modify_reputation(reputation, network_sender, peer_id, COST_UNEXPECTED_MESSAGE)
					.await;
				metrics.on_approval_unknown_assignment();
				return false;
			}
		}

		// check if our knowledge of the peer already contains this approval
		match entry.known_by.entry(peer_id) {
			hash_map::Entry::Occupied(mut knowledge) => {
				let peer_knowledge = knowledge.get_mut();
				if peer_knowledge.contains(&approval_knowledge_key.0, approval_knowledge_key.1) {
					if !peer_knowledge
						.received
						.insert(approval_knowledge_key.0.clone(), approval_knowledge_key.1)
					{
						if !Self::accept_duplicates_from_validators(
							blocks_by_number,
							topologies,
							aggression_config,
							entry,
							peer_id,
						) {
							gum::trace!(
								target: LOG_TARGET,
								?peer_id,
								?approval_knowledge_key,
								"Duplicate approval",
							);
							modify_reputation(
								reputation,
								network_sender,
								peer_id,
								COST_DUPLICATE_MESSAGE,
							)
							.await;
						}
						metrics.on_approval_duplicate();
					}
					return false;
				}
			},
			hash_map::Entry::Vacant(_) => {
				gum::debug!(
					target: LOG_TARGET,
					?peer_id,
					?approval_knowledge_key,
					"Approval from a peer is out of view",
				);
				modify_reputation(reputation, network_sender, peer_id, COST_UNEXPECTED_MESSAGE)
					.await;
				metrics.on_approval_out_of_view();
			},
		}

		if entry.knowledge.contains(&approval_knowledge_key.0, approval_knowledge_key.1) {
			if let Some(peer_knowledge) = entry.known_by.get_mut(&peer_id) {
				peer_knowledge
					.received
					.insert(approval_knowledge_key.0.clone(), approval_knowledge_key.1);
			}

			// We already processed this approval no need to continue.
			gum::trace!(target: LOG_TARGET, ?peer_id, ?approval_knowledge_key, "Known approval");
			metrics.on_approval_good_known();
			modify_reputation(reputation, network_sender, peer_id, BENEFIT_VALID_MESSAGE).await;
			false
		} else {
			true
		}
	}

	async fn import_and_circulate_approval<
		N: overseer::SubsystemSender<NetworkBridgeTxMessage>,
		A: overseer::SubsystemSender<ApprovalVotingMessage>,
		RA: overseer::SubsystemSender<RuntimeApiMessage>,
	>(
		&mut self,
		approval_voting_sender: &mut A,
		network_sender: &mut N,
		runtime_api_sender: &mut RA,
		metrics: &Metrics,
		source: MessageSource,
		vote: IndirectSignedApprovalVoteV2,
		session_info_provider: &mut RuntimeInfo,
	) {
		let block_hash = vote.block_hash;
		let validator_index = vote.validator;
		let candidate_indices = &vote.candidate_indices;
		let entry = match self.blocks.get_mut(&block_hash) {
			Some(entry) if entry.contains_candidates(&vote.candidate_indices) => entry,
			_ => {
				if let Some(peer_id) = source.peer_id() {
					if !self.recent_outdated_blocks.is_recent_outdated(&block_hash) {
						gum::debug!(
							target: LOG_TARGET,
							?peer_id,
							?block_hash,
							?validator_index,
							?candidate_indices,
							"Approval from a peer is out of view",
						);
						modify_reputation(
							&mut self.reputation,
							network_sender,
							peer_id,
							COST_UNEXPECTED_MESSAGE,
						)
						.await;
						metrics.on_approval_invalid_block();
					} else {
						metrics.on_approval_recent_outdated();
					}
				}
				return;
			},
		};

		// compute metadata on the assignment.
		let assignments_knowledge_keys = PeerKnowledge::generate_assignments_keys(&vote);
		let approval_knwowledge_key = PeerKnowledge::generate_approval_key(&vote);

		if let Some(peer_id) = source.peer_id() {
			if !Self::check_approval_can_be_processed(
				network_sender,
				&assignments_knowledge_keys,
				&approval_knwowledge_key,
				entry,
				&self.blocks_by_number,
				&self.topologies,
				&self.aggression_config,
				&mut self.reputation,
				peer_id,
				metrics,
			)
			.await
			{
				return;
			}

			let result =
				Self::check_vote_valid(&vote, &entry, session_info_provider, runtime_api_sender)
					.await;

			match result {
				Ok(vote) => {
					approval_voting_sender
						.send_message(ApprovalVotingMessage::ImportApproval(vote, None))
						.await;

					modify_reputation(
						&mut self.reputation,
						network_sender,
						peer_id,
						BENEFIT_VALID_MESSAGE_FIRST,
					)
					.await;

					entry
						.knowledge
						.insert(approval_knwowledge_key.0.clone(), approval_knwowledge_key.1);
					if let Some(peer_knowledge) = entry.known_by.get_mut(&peer_id) {
						peer_knowledge
							.received
							.insert(approval_knwowledge_key.0.clone(), approval_knwowledge_key.1);
					}
				},
				Err(err) => {
					modify_reputation(
						&mut self.reputation,
						network_sender,
						peer_id,
						COST_INVALID_MESSAGE,
					)
					.await;

					gum::info!(
						target: LOG_TARGET,
						?peer_id,
						?err,
						"Got a bad approval from peer",
					);
					metrics.on_approval_bad();
					return;
				},
			}
		} else {
			if !entry
				.knowledge
				.insert(approval_knwowledge_key.0.clone(), approval_knwowledge_key.1)
			{
				// if we already imported all approvals, there is no need to distribute it again
				gum::warn!(
					target: LOG_TARGET,
					"Importing locally an already known approval",
				);
				return;
			} else {
				gum::debug!(
					target: LOG_TARGET,
					"Importing locally a new approval",
				);
			}
		}

		let (required_routing, peers_randomly_routed_to) = match entry.note_approval(vote.clone()) {
			Ok(required_routing) => required_routing,
			Err(err) => {
				gum::warn!(
					target: LOG_TARGET,
					hash = ?block_hash,
					validator_index = ?vote.validator,
					candidate_bitfield = ?vote.candidate_indices,
					?err,
					"Possible bug: Vote import failed",
				);
				metrics.on_approval_bug();
				return;
			},
		};

		// Invariant: to our knowledge, none of the peers except for the `source` know about the
		// approval.
		metrics.on_approval_imported();

		// Dispatch a ApprovalDistributionV3Message::Approval(vote)
		// to all peers required by the topology, with the exception of the source peer.
		let topology = self.topologies.get_topology(entry.session);
		let source_peer = source.peer_id();

		let peer_filter = move |peer| {
			if Some(peer) == source_peer.as_ref() {
				return false;
			}

			// Here we're leaning on a few behaviors of assignment propagation:
			//   1. At this point, the only peer we're aware of which has the approval message is
			//      the source peer.
			//   2. We have sent the assignment message to every peer in the required routing which
			//      is aware of this block _unless_ the peer we originally received the assignment
			//      from was part of the required routing. In that case, we've sent the assignment
			//      to all aware peers in the required routing _except_ the original source of the
			//      assignment. Hence the `in_topology_check`.
			//   3. Any randomly selected peers have been sent the assignment already.
			let in_topology = topology
				.map_or(false, |t| t.local_grid_neighbors().route_to_peer(required_routing, peer));
			in_topology || peers_randomly_routed_to.contains(peer)
		};

		let peers = entry
			.known_by
			.iter()
			.filter(|(p, _)| peer_filter(p))
			.filter_map(|(p, _)| self.peer_views.get(p).map(|entry| (*p, entry.version)))
			.collect::<Vec<_>>();

		// Add the metadata of the assignment to the knowledge of each peer.
		for peer in peers.iter() {
			// we already filtered peers above, so this should always be Some
			if let Some(entry) = entry.known_by.get_mut(&peer.0) {
				entry.sent.insert(approval_knwowledge_key.0.clone(), approval_knwowledge_key.1);
			}
		}

		if !peers.is_empty() {
			let approvals = vec![vote];
			gum::trace!(
				target: LOG_TARGET,
				?block_hash,
				local = source.peer_id().is_none(),
				num_peers = peers.len(),
				"Sending an approval to peers",
			);
			send_approvals_batched(network_sender, approvals, &peers).await;
		}
	}

	// Checks if the approval vote is valid.
	async fn check_vote_valid<RA: overseer::SubsystemSender<RuntimeApiMessage>>(
		vote: &IndirectSignedApprovalVoteV2,
		entry: &BlockEntry,
		runtime_info: &mut RuntimeInfo,
		runtime_api_sender: &mut RA,
	) -> Result<CheckedIndirectSignedApprovalVote, InvalidVoteError> {
		if vote.candidate_indices.len() > entry.candidates_metadata.len() {
			return Err(InvalidVoteError::CandidateIndexOutOfBounds);
		}

		let candidate_hashes = vote
			.candidate_indices
			.iter_ones()
			.flat_map(|candidate_index| {
				entry
					.candidates_metadata
					.get(candidate_index)
					.map(|(candidate_hash, _, _)| *candidate_hash)
			})
			.collect::<Vec<_>>();

		let ExtendedSessionInfo { ref session_info, ref approval_voting_params, .. } = runtime_info
			.get_session_info_by_index(runtime_api_sender, vote.block_hash, entry.session)
			.await
			.map_err(|err| InvalidVoteError::SessionInfoNotFound(err))?;

		// Enforce the runtime's coalescing limit: reject votes coalescing more candidates than
		// `max_approval_coalesce_count`.
		if candidate_hashes.len() > approval_voting_params.max_approval_coalesce_count as usize {
			return Err(InvalidVoteError::TooManyCandidates);
		}

		let pubkey = session_info
			.validators
			.get(vote.validator)
			.ok_or(InvalidVoteError::ValidatorIndexOutOfBounds)?;
		let candidate_hashes: BoundedVec<CandidateHash, ConstU32<{ MAX_COALESCE_APPROVALS }>> =
			candidate_hashes.try_into().map_err(|_| InvalidVoteError::TooManyCandidates)?;
		let first_candidate =
			*candidate_hashes.first().ok_or(InvalidVoteError::CandidateHashNotFound)?;
		DisputeStatement::Valid(ValidDisputeStatementKind::ApprovalCheckingMultipleCandidates(
			candidate_hashes,
		))
		.check_signature(&pubkey, first_candidate, entry.session, &vote.signature)
		.map_err(|_| InvalidVoteError::InvalidSignature)
		.map(|_| CheckedIndirectSignedApprovalVote::from_checked(vote.clone()))
	}

	/// Retrieve approval signatures from state for the given relay block/indices:
	fn get_approval_signatures(
		&mut self,
		indices: HashSet<(Hash, CandidateIndex)>,
	) -> HashMap<ValidatorIndex, (Hash, Vec<CandidateIndex>, ValidatorSignature)> {
		let mut all_sigs = HashMap::new();
		for (hash, index) in indices {
			let block_entry = match self.blocks.get(&hash) {
				None => {
					gum::debug!(
						target: LOG_TARGET,
						?hash,
						"`get_approval_signatures`: could not find block entry for given hash!"
					);
					continue;
				},
				Some(e) => e,
			};

			let sigs = block_entry.approval_votes(index).into_iter().map(|approval| {
				(
					approval.validator,
					(
						hash,
						approval
							.candidate_indices
							.iter_ones()
							.map(|val| val as CandidateIndex)
							.collect_vec(),
						approval.signature,
					),
				)
			});
			all_sigs.extend(sigs);
		}
		all_sigs
	}

	async fn unify_with_peer(
		sender: &mut impl overseer::SubsystemSender<NetworkBridgeTxMessage>,
		metrics: &Metrics,
		entries: &mut HashMap<Hash, BlockEntry>,
		topologies: &SessionGridTopologies,
		total_peers: usize,
		peer_id: PeerId,
		protocol_version: ProtocolVersion,
		view: View,
		rng: &mut (impl CryptoRng + Rng),
		retry_known_blocks: bool,
	) {
		metrics.on_unify_with_peer();
		let _timer = metrics.time_unify_with_peer();

		let mut assignments_to_send = Vec::new();
		let mut approvals_to_send = Vec::new();

		let view_finalized_number = view.finalized_number;
		for head in view.into_iter() {
			let mut block = head;

			// Walk the chain back to last finalized block of the peer view.
			loop {
				let entry = match entries.get_mut(&block) {
					Some(entry) if entry.number > view_finalized_number => entry,
					_ => break,
				};

				// Any peer which is in the `known_by` see and we know its peer_id authority id
				// mapping has already been sent all messages it's meant to get for that block and
				// all in-scope prior blocks. In case, we just learnt about its peer_id
				// authority-id mapping we have to retry sending the messages that should be sent
				// to it for all un-finalized blocks.
				if entry.known_by.contains_key(&peer_id) && !retry_known_blocks {
					break;
				}

				let peer_knowledge = entry.known_by.entry(peer_id).or_default();
				let topology = topologies.get_topology(entry.session);

				// We want to iterate the `approval_entries` of the block entry as these contain
				// all assignments that also link all approval votes.
				for approval_entry in entry.approval_entries.values_mut() {
					// Propagate the message to all peers in the required routing set OR
					// randomly sample peers.
					{
						let required_routing = approval_entry.routing_info().required_routing;
						let routing_info = &mut approval_entry.routing_info_mut();
						let rng = &mut *rng;
						let mut peer_filter = move |peer_id| {
							let in_topology = topology.as_ref().map_or(false, |t| {
								t.local_grid_neighbors().route_to_peer(required_routing, peer_id)
							});
							in_topology || {
								if !topology
									.map(|topology| topology.is_validator(peer_id))
									.unwrap_or(false)
								{
									return false;
								}

								let route_random =
									routing_info.random_routing.sample(total_peers, rng);
								if route_random {
									routing_info.mark_randomly_sent(*peer_id);
								}

								route_random
							}
						};

						if !peer_filter(&peer_id) {
							continue;
						}
					}

					let assignment_message = approval_entry.assignment();
					let approval_messages = approval_entry.approvals();
					let (assignment_knowledge, message_kind) =
						approval_entry.create_assignment_knowledge(block);

					// Only send stuff a peer doesn't know in the context of a relay chain
					// block.
					if !peer_knowledge.contains(&assignment_knowledge, message_kind) {
						peer_knowledge.sent.insert(assignment_knowledge, message_kind);
						assignments_to_send.push(assignment_message);
					}

					// Filter approval votes.
					for approval_message in approval_messages {
						let approval_knowledge =
							PeerKnowledge::generate_approval_key(&approval_message);

						if !peer_knowledge.contains(&approval_knowledge.0, approval_knowledge.1) {
							approvals_to_send.push(approval_message);
							peer_knowledge.sent.insert(approval_knowledge.0, approval_knowledge.1);
						}
					}
				}

				block = entry.parent_hash;
			}
		}

		if !assignments_to_send.is_empty() {
			gum::trace!(
				target: LOG_TARGET,
				?peer_id,
				?protocol_version,
				num = assignments_to_send.len(),
				"Sending assignments to unified peer",
			);

			send_assignments_batched(
				sender,
				assignments_to_send,
				&vec![(peer_id, protocol_version)],
			)
			.await;
		}

		if !approvals_to_send.is_empty() {
			gum::trace!(
				target: LOG_TARGET,
				?peer_id,
				?protocol_version,
				num = approvals_to_send.len(),
				"Sending approvals to unified peer",
			);

			send_approvals_batched(sender, approvals_to_send, &vec![(peer_id, protocol_version)])
				.await;
		}
	}

	// It is very important that aggression starts with oldest unfinalized block, rather than oldest
	// unapproved block. Using the gossip approach to distribute potentially
	// missing votes to validators requires that we always trigger on finality lag, even if
	// we have have the approval lag value. The reason for this, is to avoid finality stall
	// when more than 1/3 nodes go offline for a period o time. When they come back
	// there wouldn't get any of the approvals since the on-line nodes would never trigger
	// aggression as they have approved all the candidates and don't detect any approval lag.
	//
	// In order to switch to using approval lag as a trigger we need a request/response protocol
	// to fetch votes from validators rather than use gossip.
	async fn enable_aggression<N: overseer::SubsystemSender<NetworkBridgeTxMessage>>(
		&mut self,
		network_sender: &mut N,
		resend: Resend,
		metrics: &Metrics,
	) {
		let config = self.aggression_config.clone();
		let min_age = self.blocks_by_number.iter().next().map(|(num, _)| num);
		let max_age = self.blocks_by_number.iter().rev().next().map(|(num, _)| num);

		// Return if we don't have at least 1 block.
		let (min_age, max_age) = match (min_age, max_age) {
			(Some(min), Some(max)) => (*min, *max),
			_ => return, // empty.
		};

		let age = max_age.saturating_sub(min_age);

		// Trigger on approval checking lag.
		if !self.aggression_config.should_trigger_aggression(age) {
			gum::trace!(
				target: LOG_TARGET,
				approval_checking_lag = self.approval_checking_lag,
				age,
				"Aggression not enabled",
			);
			return;
		}
		gum::debug!(target: LOG_TARGET, min_age, max_age, "Aggression enabled",);

		adjust_required_routing_and_propagate(
			network_sender,
			&mut self.blocks,
			&self.topologies,
			|block_entry| {
				let block_age = max_age - block_entry.number;
				// We want to resend only for blocks of min_age, there is no point in
				// resending for blocks newer than that, because we are just going to create load
				// and not gain anything.
				let diff_from_min_age = block_entry.number - min_age;

				// We want to back-off on resending for blocks that have been resent recently, to
				// give time for nodes to process all the extra messages, if we still have not
				// finalized we are going to resend again after unfinalized_period * 2 since the
				// last resend.
				let blocks_since_last_sent = block_entry
					.last_resent_at_block_number
					.map(|last_resent_at_block_number| max_age - last_resent_at_block_number);

				let can_resend_at_this_age = blocks_since_last_sent
					.zip(config.resend_unfinalized_period)
					.map(|(blocks_since_last_sent, unfinalized_period)| {
						blocks_since_last_sent >= unfinalized_period * 2
					})
					.unwrap_or(true);

				if resend == Resend::Yes &&
					config.resend_unfinalized_period.as_ref().map_or(false, |p| {
						block_age > 0 &&
							block_age % p == 0 && diff_from_min_age == 0 &&
							can_resend_at_this_age
					}) {
					// Retry sending to all peers.
					for (_, knowledge) in block_entry.known_by.iter_mut() {
						knowledge.sent = Knowledge::default();
					}
					block_entry.last_resent_at_block_number = Some(max_age);
					gum::debug!(
						target: LOG_TARGET,
						block_number = ?block_entry.number,
						?max_age,
						"Aggression enabled with resend for block",
					);
					true
				} else {
					false
				}
			},
			|required_routing, _, _| *required_routing,
			&self.peer_views,
		)
		.await;

		adjust_required_routing_and_propagate(
			network_sender,
			&mut self.blocks,
			&self.topologies,
			|block_entry| {
				// Ramp up aggression only for the very oldest block(s).
				// Approval voting can get stuck on a single block preventing
				// its descendants from being finalized. Waste minimal bandwidth
				// this way. Also, disputes might prevent finality - again, nothing
				// to waste bandwidth on newer blocks for.
				block_entry.number == min_age
			},
			|required_routing, local, _| {
				// It's a bit surprising not to have a topology at this age.
				if *required_routing == RequiredRouting::PendingTopology {
					gum::debug!(
						target: LOG_TARGET,
						lag = ?self.approval_checking_lag,
						"Encountered old block pending gossip topology",
					);
					return *required_routing;
				}

				let mut new_required_routing = *required_routing;

				if config.l1_threshold.as_ref().map_or(false, |t| &age >= t) {
					// Message originator sends to everyone.
					if local && new_required_routing != RequiredRouting::All {
						metrics.on_aggression_l1();
						new_required_routing = RequiredRouting::All;
					}
				}

				if config.l2_threshold.as_ref().map_or(false, |t| &age >= t) {
					// Message originator sends to everyone. Everyone else sends to XY.
					if !local && new_required_routing != RequiredRouting::GridXY {
						metrics.on_aggression_l2();
						new_required_routing = RequiredRouting::GridXY;
					}
				}
				new_required_routing
			},
			&self.peer_views,
		)
		.await;
	}

	// Filter out oversized candidate and certificate core bitfields.
	// For each invalid assignment we also punish the peer.
	async fn sanitize_v2_assignments(
		&mut self,
		peer_id: PeerId,
		sender: &mut impl overseer::SubsystemSender<NetworkBridgeTxMessage>,
		assignments: Vec<(IndirectAssignmentCertV2, CandidateBitfield)>,
	) -> Vec<(IndirectAssignmentCertV2, CandidateBitfield)> {
		let mut sanitized_assignments = Vec::new();
		for (cert, candidate_bitfield) in assignments.into_iter() {
			let cert_bitfield_bits = match &cert.cert.kind {
				AssignmentCertKindV2::RelayVRFDelay { core_index } => core_index.0 as usize + 1,
				AssignmentCertKindV2::RelayVRFModuloCompact { core_bitfield } => {
					core_bitfield.len()
				},
			};

			let candidate_bitfield_bits = candidate_bitfield.len();

			// Our bitfield has `Lsb0`.
			let msb = candidate_bitfield_bits - 1;

			// Ensure bitfields length under hard limit.
			if cert_bitfield_bits > MAX_BITFIELD_SIZE
				|| candidate_bitfield_bits > MAX_BITFIELD_SIZE
				// Ensure minimum bitfield size - MSB needs to be one.
				|| !candidate_bitfield.bit_at(msb.as_bit_index())
			{
				// Punish the peer for the invalid message.
				modify_reputation(&mut self.reputation, sender, peer_id, COST_OVERSIZED_BITFIELD)
					.await;
				for candidate_index in candidate_bitfield.iter_ones() {
					gum::debug!(target: LOG_TARGET, block_hash = ?cert.block_hash, ?candidate_index, validator_index = ?cert.validator, "Bad assignment v2, oversized bitfield");
				}
			} else {
				sanitized_assignments.push((cert, candidate_bitfield))
			}
		}

		sanitized_assignments
	}

	// Filter out obviously invalid candidate indices.
	async fn sanitize_v2_approvals(
		&mut self,
		peer_id: PeerId,
		sender: &mut impl overseer::SubsystemSender<NetworkBridgeTxMessage>,
		approval: Vec<IndirectSignedApprovalVoteV2>,
	) -> Vec<IndirectSignedApprovalVoteV2> {
		let mut sanitized_approvals = Vec::new();
		for approval in approval.into_iter() {
			let has_no_approved_candidates = approval.candidate_indices.first_one().is_none();
			if approval.candidate_indices.len() as usize > MAX_BITFIELD_SIZE ||
				has_no_approved_candidates
			{
				// Punish the peer for the invalid message.
				modify_reputation(
					&mut self.reputation,
					sender,
					peer_id,
					if has_no_approved_candidates {
						COST_INVALID_MESSAGE
					} else {
						COST_OVERSIZED_BITFIELD
					},
				)
				.await;
				gum::debug!(
					target: LOG_TARGET,
					block_hash = ?approval.block_hash,
					candidate_indices_len = ?approval.candidate_indices.len(),
					"Bad approval v2, invalid candidate indices size"
				);
			} else {
				sanitized_approvals.push(approval)
			}
		}

		sanitized_approvals
	}
}

// This adjusts the required routing of messages in blocks that pass the block filter
// according to the modifier function given.
//
// The modifier accepts as inputs the current required-routing state, whether
// the message is locally originating, and the validator index of the message issuer.
//
// Then, if the topology is known, this propagates messages to all peers in the required
// routing set which are aware of the block. Peers which are unaware of the block
// will have the message sent when it enters their view in `unify_with_peer`.
//
// Note that the required routing of a message can be modified even if the
// topology is unknown yet.
#[overseer::contextbounds(ApprovalDistribution, prefix = self::overseer)]
async fn adjust_required_routing_and_propagate<
	N: overseer::SubsystemSender<NetworkBridgeTxMessage>,
	BlockFilter,
	RoutingModifier,
>(
	network_sender: &mut N,
	blocks: &mut HashMap<Hash, BlockEntry>,
	topologies: &SessionGridTopologies,
	block_filter: BlockFilter,
	routing_modifier: RoutingModifier,
	peer_views: &HashMap<PeerId, PeerEntry>,
) where
	BlockFilter: Fn(&mut BlockEntry) -> bool,
	RoutingModifier: Fn(&RequiredRouting, bool, &ValidatorIndex) -> RequiredRouting,
{
	let mut peer_assignments = HashMap::new();
	let mut peer_approvals = HashMap::new();

	// Iterate all blocks in the session, producing payloads
	// for each connected peer.
	for (block_hash, block_entry) in blocks {
		if !block_filter(block_entry) {
			continue;
		}

		let topology = match topologies.get_topology(block_entry.session) {
			Some(t) => t,
			None => continue,
		};

		// We just need to iterate the `approval_entries` of the block entry as these contain all
		// assignments that also link all approval votes.
		for approval_entry in block_entry.approval_entries.values_mut() {
			let new_required_routing = routing_modifier(
				&approval_entry.routing_info().required_routing,
				approval_entry.routing_info().local,
				&approval_entry.validator_index(),
			);

			approval_entry.update_required_routing(new_required_routing);

			if approval_entry.routing_info().required_routing.is_empty() {
				continue;
			}

			let assignment_message = approval_entry.assignment();
			let approval_messages = approval_entry.approvals();
			let (assignment_knowledge, message_kind) =
				approval_entry.create_assignment_knowledge(*block_hash);

			for (peer, peer_knowledge) in &mut block_entry.known_by {
				if !topology
					.local_grid_neighbors()
					.route_to_peer(approval_entry.routing_info().required_routing, peer)
				{
					continue;
				}

				// Only send stuff a peer doesn't know in the context of a relay chain block.
				if !peer_knowledge.contains(&assignment_knowledge, message_kind) {
					peer_knowledge.sent.insert(assignment_knowledge.clone(), message_kind);
					peer_assignments
						.entry(*peer)
						.or_insert_with(Vec::new)
						.push(assignment_message.clone());
				}

				// Filter approval votes.
				for approval_message in &approval_messages {
					let approval_knowledge = PeerKnowledge::generate_approval_key(approval_message);

					if !peer_knowledge.contains(&approval_knowledge.0, approval_knowledge.1) {
						peer_knowledge.sent.insert(approval_knowledge.0, approval_knowledge.1);
						peer_approvals
							.entry(*peer)
							.or_insert_with(Vec::new)
							.push(approval_message.clone());
					}
				}
			}
		}
	}

	// Send messages in accumulated packets, assignments preceding approvals.
	for (peer, assignments_packet) in peer_assignments {
		if let Some(peer_view) = peer_views.get(&peer) {
			send_assignments_batched(
				network_sender,
				assignments_packet,
				&vec![(peer, peer_view.version)],
			)
			.await;
		} else {
			// This should never happen.
			gum::warn!(target: LOG_TARGET, ?peer, "Unknown protocol version for peer",);
		}
	}

	for (peer, approvals_packet) in peer_approvals {
		if let Some(peer_view) = peer_views.get(&peer) {
			send_approvals_batched(
				network_sender,
				approvals_packet,
				&vec![(peer, peer_view.version)],
			)
			.await;
		} else {
			// This should never happen.
			gum::warn!(target: LOG_TARGET, ?peer, "Unknown protocol version for peer",);
		}
	}
}

/// Modify the reputation of a peer based on its behavior.
async fn modify_reputation(
	reputation: &mut ReputationAggregator,
	sender: &mut impl overseer::SubsystemSender<NetworkBridgeTxMessage>,
	peer_id: PeerId,
	rep: Rep,
) {
	gum::trace!(
		target: LOG_TARGET,
		reputation = ?rep,
		?peer_id,
		"Reputation change for peer",
	);
	reputation.modify(sender, peer_id, rep).await;
}

#[overseer::contextbounds(ApprovalDistribution, prefix = self::overseer)]
impl ApprovalDistribution {
	/// Create a new instance of the [`ApprovalDistribution`] subsystem.
	pub fn new(
		metrics: Metrics,
		slot_duration_millis: u64,
		assignment_criteria: Arc<dyn AssignmentCriteria + Send + Sync>,
	) -> Self {
		Self::new_with_clock(
			metrics,
			slot_duration_millis,
			Arc::new(SystemClock),
			assignment_criteria,
		)
	}

	/// Create a new instance of the [`ApprovalDistribution`] subsystem, with a custom clock.
	pub fn new_with_clock(
		metrics: Metrics,
		slot_duration_millis: u64,
		clock: Arc<dyn Clock + Send + Sync>,
		assignment_criteria: Arc<dyn AssignmentCriteria + Send + Sync>,
	) -> Self {
		Self { metrics, slot_duration_millis, clock, assignment_criteria }
	}

	async fn run<Context>(self, ctx: Context) {
		let mut state =
			State { slot_duration_millis: self.slot_duration_millis, ..Default::default() };
		// According to the docs of `rand`, this is a ChaCha12 RNG in practice
		// and will always be chosen for strong performance and security properties.
		let mut rng = rand::rngs::StdRng::from_entropy();
		let mut session_info_provider = RuntimeInfo::new_with_config(RuntimeInfoConfig {
			keystore: None,
			session_cache_lru_size: DISPUTE_WINDOW.get(),
		});

		self.run_inner(
			ctx,
			&mut state,
			REPUTATION_CHANGE_INTERVAL,
			&mut rng,
			&mut session_info_provider,
		)
		.await
	}

	/// Used for testing.
	async fn run_inner<Context>(
		self,
		mut ctx: Context,
		state: &mut State,
		reputation_interval: Duration,
		rng: &mut (impl CryptoRng + Rng),
		session_info_provider: &mut RuntimeInfo,
	) {
		let new_reputation_delay = || futures_timer::Delay::new(reputation_interval).fuse();
		let mut reputation_delay = new_reputation_delay();
		let mut approval_voting_sender = ctx.sender().clone();
		let mut network_sender = ctx.sender().clone();
		let mut runtime_api_sender = ctx.sender().clone();

		loop {
			select! {
				_ = reputation_delay => {
					state.reputation.send(ctx.sender()).await;
					reputation_delay = new_reputation_delay();
				},
				message = ctx.recv().fuse() => {
					let message = match message {
						Ok(message) => message,
						Err(e) => {
							gum::debug!(target: LOG_TARGET, err = ?e, "Failed to receive a message from Overseer, exiting");
							return
						},
					};

					if self.handle_from_orchestra(message, &mut approval_voting_sender, &mut network_sender, &mut runtime_api_sender, state, rng, session_info_provider).await {
						return;
					}

				},
			}
		}
	}

	/// Handles a from orchestra message received by approval distribution subystem.
	///
	/// Returns `true` if the subsystem should be stopped.
	pub async fn handle_from_orchestra<
		N: overseer::SubsystemSender<NetworkBridgeTxMessage>,
		A: overseer::SubsystemSender<ApprovalVotingMessage>,
		RA: overseer::SubsystemSender<RuntimeApiMessage>,
	>(
		&self,
		message: FromOrchestra<ApprovalDistributionMessage>,
		approval_voting_sender: &mut A,
		network_sender: &mut N,
		runtime_api_sender: &mut RA,
		state: &mut State,
		rng: &mut (impl CryptoRng + Rng),
		session_info_provider: &mut RuntimeInfo,
	) -> bool {
		match message {
			FromOrchestra::Communication { msg } => {
				Self::handle_incoming(
					approval_voting_sender,
					network_sender,
					runtime_api_sender,
					state,
					msg,
					&self.metrics,
					rng,
					self.assignment_criteria.as_ref(),
					self.clock.as_ref(),
					session_info_provider,
				)
				.await
			},
			FromOrchestra::Signal(OverseerSignal::ActiveLeaves(_update)) => {
				gum::trace!(target: LOG_TARGET, "active leaves signal (ignored)");
				// the relay chain blocks relevant to the approval subsystems
				// are those that are available, but not finalized yet
				// activated and deactivated heads hence are irrelevant to this subsystem, other
				// than for tracing purposes.
			},
			FromOrchestra::Signal(OverseerSignal::BlockFinalized(_hash, number)) => {
				gum::trace!(target: LOG_TARGET, number = %number, "finalized signal");
				state.handle_block_finalized(network_sender, &self.metrics, number).await;
			},
			FromOrchestra::Signal(OverseerSignal::Conclude) => return true,
		}
		false
	}

	async fn handle_incoming<
		N: overseer::SubsystemSender<NetworkBridgeTxMessage>,
		A: overseer::SubsystemSender<ApprovalVotingMessage>,
		RA: overseer::SubsystemSender<RuntimeApiMessage>,
	>(
		approval_voting_sender: &mut A,
		network_sender: &mut N,
		runtime_api_sender: &mut RA,
		state: &mut State,
		msg: ApprovalDistributionMessage,
		metrics: &Metrics,
		rng: &mut (impl CryptoRng + Rng),
		assignment_criteria: &(impl AssignmentCriteria + ?Sized),
		clock: &(impl Clock + ?Sized),
		session_info_provider: &mut RuntimeInfo,
	) {
		match msg {
			ApprovalDistributionMessage::NetworkBridgeUpdate(event) => {
				state
					.handle_network_msg(
						approval_voting_sender,
						network_sender,
						runtime_api_sender,
						metrics,
						event,
						rng,
						assignment_criteria,
						clock,
						session_info_provider,
					)
					.await;
			},
			ApprovalDistributionMessage::NewBlocks(metas) => {
				state
					.handle_new_blocks(
						approval_voting_sender,
						network_sender,
						runtime_api_sender,
						metrics,
						metas,
						rng,
						assignment_criteria,
						clock,
						session_info_provider,
					)
					.await;
			},
			ApprovalDistributionMessage::DistributeAssignment(cert, candidate_indices) => {
				gum::debug!(
					target: LOG_TARGET,
					?candidate_indices,
					block_hash = ?cert.block_hash,
					assignment_kind = ?cert.cert.kind,
					"Distributing our assignment on candidates",
				);

				state
					.import_and_circulate_assignment(
						approval_voting_sender,
						network_sender,
						runtime_api_sender,
						&metrics,
						MessageSource::Local,
						cert,
						candidate_indices,
						rng,
						assignment_criteria,
						clock,
						session_info_provider,
					)
					.await;
			},
			ApprovalDistributionMessage::DistributeApproval(vote) => {
				gum::debug!(
					target: LOG_TARGET,
					"Distributing our approval vote on candidate (block={}, index={:?})",
					vote.block_hash,
					vote.candidate_indices,
				);

				state
					.import_and_circulate_approval(
						approval_voting_sender,
						network_sender,
						runtime_api_sender,
						metrics,
						MessageSource::Local,
						vote,
						session_info_provider,
					)
					.await;
			},
			ApprovalDistributionMessage::GetApprovalSignatures(indices, tx) => {
				let sigs = state.get_approval_signatures(indices);
				if let Err(_) = tx.send(sigs) {
					gum::debug!(
						target: LOG_TARGET,
						"Sending back approval signatures failed, oneshot got closed"
					);
				}
			},
			ApprovalDistributionMessage::ApprovalCheckingLagUpdate(lag) => {
				gum::debug!(target: LOG_TARGET, lag, "Received `ApprovalCheckingLagUpdate`");
				state.approval_checking_lag = lag;
			},
		}
	}
}

#[overseer::subsystem(ApprovalDistribution, error=SubsystemError, prefix=self::overseer)]
impl<Context> ApprovalDistribution {
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = self.run(ctx).map(|_| Ok(())).boxed();

		SpawnedSubsystem { name: "approval-distribution-subsystem", future }
	}
}

/// Ensures the batch size is always at least 1 element.
const fn ensure_size_not_zero(size: usize) -> usize {
	if 0 == size {
		panic!("Batch size must be at least 1 (MAX_NOTIFICATION_SIZE constant is too low)",);
	}

	size
}

/// The maximum amount of assignments per batch is 33% of maximum allowed by protocol.
/// This is an arbitrary value. Bumping this up increases the maximum amount of approvals or
/// assignments we send in a single message to peers. Exceeding `MAX_NOTIFICATION_SIZE` will violate
/// the protocol configuration.
pub const MAX_ASSIGNMENT_BATCH_SIZE: usize = ensure_size_not_zero(
	MAX_NOTIFICATION_SIZE as usize /
		std::mem::size_of::<(IndirectAssignmentCertV2, CandidateIndex)>() /
		3,
);

/// The maximum amount of approvals per batch is 33% of maximum allowed by protocol.
pub const MAX_APPROVAL_BATCH_SIZE: usize = ensure_size_not_zero(
	MAX_NOTIFICATION_SIZE as usize / std::mem::size_of::<IndirectSignedApprovalVoteV2>() / 3,
);

// Low level helper for sending assignments.
async fn send_assignments_batched_inner(
	sender: &mut impl overseer::SubsystemSender<NetworkBridgeTxMessage>,
	batch: impl IntoIterator<Item = (IndirectAssignmentCertV2, CandidateBitfield)>,
	peers: Vec<PeerId>,
	_peer_version: ValidationVersion,
) {
	sender
		.send_message(NetworkBridgeTxMessage::SendValidationMessage(
			peers,
			ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
				protocol_v3::ApprovalDistributionMessage::Assignments(batch.into_iter().collect()),
			)),
		))
		.await;
}

/// Send assignments while honoring the `max_notification_size` of the protocol.
///
/// Splitting the messages into multiple notifications allows more granular processing at the
/// destination, such that the subsystem doesn't get stuck for long processing a batch
/// of assignments and can `select!` other tasks.
pub(crate) async fn send_assignments_batched(
	network_sender: &mut impl overseer::SubsystemSender<NetworkBridgeTxMessage>,
	v2_assignments: impl IntoIterator<Item = (IndirectAssignmentCertV2, CandidateBitfield)> + Clone,
	peers: &[(PeerId, ProtocolVersion)],
) {
	let v3_peers = filter_by_peer_version(peers, ValidationVersion::V3.into());

	if !v3_peers.is_empty() {
		let mut v3 = v2_assignments.into_iter().peekable();

		while v3.peek().is_some() {
			let batch = v3.by_ref().take(MAX_ASSIGNMENT_BATCH_SIZE).collect::<Vec<_>>();
			send_assignments_batched_inner(
				network_sender,
				batch,
				v3_peers.clone(),
				ValidationVersion::V3,
			)
			.await;
		}
	}
}

/// Send approvals while honoring the `max_notification_size` of the protocol and peer version.
pub(crate) async fn send_approvals_batched(
	sender: &mut impl overseer::SubsystemSender<NetworkBridgeTxMessage>,
	approvals: impl IntoIterator<Item = IndirectSignedApprovalVoteV2> + Clone,
	peers: &[(PeerId, ProtocolVersion)],
) {
	let v3_peers = filter_by_peer_version(peers, ValidationVersion::V3.into());

	if !v3_peers.is_empty() {
		let mut batches = approvals.into_iter().peekable();

		while batches.peek().is_some() {
			let batch: Vec<_> = batches.by_ref().take(MAX_APPROVAL_BATCH_SIZE).collect();

			sender
				.send_message(NetworkBridgeTxMessage::SendValidationMessage(
					v3_peers.clone(),
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Approvals(batch),
					)),
				))
				.await;
		}
	}
}
