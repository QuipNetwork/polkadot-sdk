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

//! Message types for the overseer and subsystems.
//!
//! These messages are intended to define the protocol by which different subsystems communicate
//! with each other and signals that they receive from an overseer to coordinate their work.
//! This is intended for use with the `polkadot-overseer` crate.
//!
//! Subsystems' APIs are defined separately from their implementation, leading to easier mocking.

use futures::channel::oneshot;
use sc_network::{Multiaddr, ReputationChange};
use sp_runtime::{traits::ConstU32, BoundedVec};
use thiserror::Error;

pub use sc_network::IfDisconnected;

use polkadot_node_network_protocol::{
	self as net_protocol, peer_set::PeerSet, request_response::Requests, PeerId,
};
use polkadot_node_primitives::{
	approval::{
		v1::{BlockApprovalMeta, DelayTranche},
		v2::{CandidateBitfield, IndirectAssignmentCertV2, IndirectSignedApprovalVoteV2},
	},
	AvailableData, BabeEpoch, BlockWeight, CandidateVotes, CollationGenerationConfig,
	DisputeMessage, DisputeStatus, ErasureChunk, PoV, SignedDisputeStatement, SignedFullStatement,
	SignedFullStatementWithPVD, SubmitSegmentParams, ValidationResult, MAX_SEGMENT_LEN,
};
use polkadot_primitives::{
	self,
	async_backing::{self, Constraints},
	slashing,
	vstaging::RelayParentInfo,
	ApprovalVotingParams, AuthorityDiscoveryId, BackedCandidate, BlockNumber, CandidateCommitments,
	CandidateEvent, CandidateHash, CandidateIndex, CandidateReceiptV2 as CandidateReceipt,
	CoalescedApprovalCandidateHashes, CommittedCandidateReceiptV2 as CommittedCandidateReceipt,
	CoreIndex, CoreState, DisputeState, ExecutorParams, GroupIndex, GroupRotationInfo, Hash,
	HeadData, Header as BlockHeader, Id as ParaId, InboundDownwardMessage, InboundHrmpMessage,
	MultiDisputeStatementSet, NodeFeatures, OccupiedCoreAssumption, PersistedValidationData,
	PvfCheckStatement, PvfExecKind as RuntimePvfExecKind, SessionIndex, SessionInfo,
	SignedAvailabilityBitfield, SignedAvailabilityBitfields, ValidationCode, ValidationCodeHash,
	ValidatorId, ValidatorIndex, ValidatorSignature,
};
use polkadot_statement_table::v2::Misbehavior;
use std::{
	collections::{BTreeMap, HashMap, HashSet, VecDeque},
	sync::Arc,
};

/// Network events as transmitted to other subsystems, wrapped in their message types.
pub mod network_bridge_event;
pub use network_bridge_event::NetworkBridgeEvent;

/// A request to the candidate backing subsystem to check whether
/// we can second this candidate.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct CanSecondRequest {
	/// Para id of the candidate.
	pub candidate_para_id: ParaId,
	/// The scheduling parent of the candidate (for V3, may differ from execution relay_parent).
	pub candidate_scheduling_parent: Hash,
	/// Hash of the candidate.
	pub candidate_hash: CandidateHash,
	/// Parent head data hash.
	pub parent_head_data_hash: Hash,
}

/// A reference to a backable candidate along with its scheduling parent.
///
/// The scheduling parent determines which validator group is responsible
/// for backing this candidate and is used to look up per-scheduling-parent state.
///
/// This is distinct from `BackedCandidate` which includes the full candidate
/// data and backing signatures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackableCandidateRef {
	/// The hash of the candidate that can be backed.
	pub candidate_hash: CandidateHash,
	/// The scheduling parent hash used for validator group assignment.
	/// For V3 candidates, this may differ from the candidate's relay_parent.
	/// For V1/V2 candidates, this equals the relay_parent.
	pub scheduling_parent: Hash,
}

/// Messages received by the Candidate Backing subsystem.
#[derive(Debug)]
pub enum CandidateBackingMessage {
	/// Requests a set of backable candidates attested by the subsystem.
	///
	/// The input is a map from `ParaId` to a vector of backable candidate references.
	/// Each reference contains the candidate hash and its scheduling parent (used for
	/// validator group assignment).
	///
	/// The order of candidates of the same para must be preserved in the response.
	/// If a backed candidate of a para cannot be retrieved, the response should not contain any
	/// candidates of the same para that follow it in the input vector. In other words, assuming
	/// candidates are supplied in dependency order, we must ensure that this dependency order is
	/// preserved.
	GetBackableCandidates {
		/// Map from para ID to backable candidate references with their scheduling parents.
		candidates: HashMap<ParaId, Vec<BackableCandidateRef>>,
		/// Channel to send the backed candidates (with full signatures).
		sender: oneshot::Sender<HashMap<ParaId, Vec<BackedCandidate>>>,
	},
	/// Request the subsystem to check whether it's allowed to second given candidate.
	/// The rule is to only fetch collations that can either be directly chained to any
	/// FragmentChain in the view or there is at least one FragmentChain where this candidate is a
	/// potentially unconnected candidate (we predict that it may become connected to a
	/// FragmentChain in the future).
	///
	/// Always responds with `false` if async backing is disabled for candidate's relay
	/// parent.
	CanSecond(CanSecondRequest, oneshot::Sender<bool>),
	/// Note that the Candidate Backing subsystem should second the given candidate in the context
	/// of the given scheduling parent (ref. by hash). This candidate must be validated.
	Second {
		/// The scheduling parent hash (determines validator group assignment).
		/// TODO: Once node feature is assumed to be enabled, remove this redundant field and use
		/// scheduling_parent of the descriptor directly: <https://github.com/paritytech/polkadot-sdk/issues/10883#issue-3844123650>
		scheduling_parent: Hash,
		/// The candidate to second.
		candidate: CandidateReceipt,
		/// Persisted validation data.
		pvd: PersistedValidationData,
		/// Proof of validity.
		pov: PoV,
	},
	/// Note a validator's statement about a particular candidate in the context of the given
	/// scheduling parent. Disagreements about validity must be escalated to a broader check by the
	/// Disputes Subsystem, though that escalation is deferred until the approval voting stage to
	/// guarantee availability. Agreements are simply tallied until a quorum is reached.
	Statement {
		/// The scheduling parent hash (determines validator group context).
		scheduling_parent: Hash,
		/// The signed statement with persisted validation data.
		statement: SignedFullStatementWithPVD,
	},
}

/// Blanket error for validation failing for internal reasons.
#[derive(Debug, Error)]
#[error("Validation failed with {0:?}")]
pub struct ValidationFailed(pub String);

/// The outcome of the candidate-validation's PVF pre-check request.
#[derive(Debug, PartialEq)]
pub enum PreCheckOutcome {
	/// The PVF has been compiled successfully within the given constraints.
	Valid,
	/// The PVF could not be compiled. This variant is used when the candidate-validation subsystem
	/// can be sure that the PVF is invalid. To give a couple of examples: a PVF that cannot be
	/// decompressed or that does not represent a structurally valid WebAssembly file.
	Invalid,
	/// This variant is used when the PVF cannot be compiled but for other reasons that are not
	/// included into [`PreCheckOutcome::Invalid`]. This variant can indicate that the PVF in
	/// question is invalid, however it is not necessary that PVF that received this judgement
	/// is invalid.
	///
	/// For example, if during compilation the preparation worker was killed we cannot be sure why
	/// it happened: because the PVF was malicious made the worker to use too much memory or its
	/// because the host machine is under severe memory pressure and it decided to kill the worker.
	Failed,
}

/// Messages received by the Validation subsystem.
///
/// ## Validation Requests
///
/// Validation requests made to the subsystem should return an error only on internal error.
/// Otherwise, they should return either `Ok(ValidationResult::Valid(_))`
/// or `Ok(ValidationResult::Invalid)`.
#[derive(Debug)]
pub enum CandidateValidationMessage {
	/// Validate a candidate with provided, exhaustive parameters for validation.
	///
	/// Explicitly provide the `PersistedValidationData` and `ValidationCode` so this can do full
	/// validation without needing to access the state of the relay-chain.
	///
	/// This request doesn't involve acceptance criteria checking, therefore only useful for the
	/// cases where the validity of the candidate is established. This is the case for the typical
	/// use-case: secondary checkers would use this request relying on the full prior checks
	/// performed by the relay-chain.
	ValidateFromExhaustive {
		/// Persisted validation data
		validation_data: PersistedValidationData,
		/// Validation code
		validation_code: ValidationCode,
		/// The candidate receipt
		candidate_receipt: CandidateReceipt,
		/// The proof-of-validity
		pov: Arc<PoV>,
		/// Scheduling session index for this candidate. For V1 descriptors this
		/// equals the relay-parent session and serves as fallback for both
		/// execution and scheduling session. For V2+, sessions are in the
		/// descriptor and this field is ignored. Can be removed once V1 support
		/// is dropped.
		scheduling_session_index: SessionIndex,
		/// Execution kind, used for timeouts and retries (backing/approvals)
		exec_kind: PvfExecKind,
		/// The sending side of the response channel
		response_sender: oneshot::Sender<Result<ValidationResult, ValidationFailed>>,
	},
	/// Try to compile the given validation code and send back
	/// the outcome.
	///
	/// The validation code is specified by the hash and will be queried from the runtime API at
	/// the given relay-parent.
	PreCheck {
		/// Relay-parent
		relay_parent: Hash,
		/// Validation code hash
		validation_code_hash: ValidationCodeHash,
		/// The sending side of the response channel
		response_sender: oneshot::Sender<PreCheckOutcome>,
	},
}

/// Extends primitives::PvfExecKind, which is a runtime parameter we don't want to change,
/// to separate and prioritize execution jobs by request type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PvfExecKind {
	/// For dispute requests
	Dispute,
	/// For approval requests
	Approval,
	/// For backing requests from system parachains. With relay parent hash
	BackingSystemParas(Hash),
	/// For backing requests. With relay parent hash
	Backing(Hash),
}

impl PvfExecKind {
	/// Converts priority level to &str
	pub fn as_str(&self) -> &str {
		match *self {
			Self::Dispute => "dispute",
			Self::Approval => "approval",
			Self::BackingSystemParas(_) => "backing_system_paras",
			Self::Backing(_) => "backing",
		}
	}
}

impl From<PvfExecKind> for RuntimePvfExecKind {
	fn from(exec: PvfExecKind) -> Self {
		match exec {
			PvfExecKind::Dispute => RuntimePvfExecKind::Approval,
			PvfExecKind::Approval => RuntimePvfExecKind::Approval,
			PvfExecKind::BackingSystemParas(_) => RuntimePvfExecKind::Backing,
			PvfExecKind::Backing(_) => RuntimePvfExecKind::Backing,
		}
	}
}

/// A fully built segment entry.
/// The collator protocol assembles a `CandidateReceipt` from these
/// fields and the segment level commons.
#[derive(Debug)]
pub struct SegmentEntry {
	/// Relay parent the candidate builds on.
	pub relay_parent: Hash,
	/// The relay parent's session index.
	pub session_index: SessionIndex,
	/// Hash of the validation code the candidate is validated against.
	pub validation_code_hash: ValidationCodeHash,
	/// Hash of the candidate's persisted validation data.
	pub persisted_validation_data_hash: Hash,
	/// Erasure root of the candidate's available data.
	pub erasure_root: Hash,
	/// Hash of the candidate commitments.
	pub commitments_hash: Hash,
	/// Hash of the parachain head data produced by the candidate. Stable
	/// across resubmissions; doubles as the fingerprint identity and the
	/// collation storage key.
	pub output_head_data_hash: Hash,
	/// Proof of validity for the candidate.
	pub pov: PoV,
	/// Parachain head data before candidate execution.
	pub parent_head_data: HeadData,
}

/// The candidates of one `DistributeSegment` message, shaped by descriptor
/// version.
#[derive(Debug)]
pub enum Segment {
	/// A V2-descriptor segment: exactly one candidate, always built. The
	/// entry's `relay_parent` doubles as the scheduling parent.
	V2(SegmentEntry),
	/// A V3-descriptor segment: candidates ordered by age, sharing one
	/// scheduling parent.
	V3 {
		/// The scheduling parent shared by all candidates in the segment.
		scheduling_parent: Hash,
		/// The scheduling parent's session index.
		scheduling_session: SessionIndex,
		/// Ordered candidates; the list may have gaps. Every entry is fully
		/// built. When on-demand candidate building lands, entries become
		/// fingerprint advertisements materialized at fetch time, and
		/// `SegmentEntry` leaves this message entirely.
		candidates: BoundedVec<SegmentEntry, ConstU32<MAX_SEGMENT_LEN>>,
	},
}

/// Messages received by the Collator Protocol subsystem.
#[derive(Debug, derive_more::From)]
pub enum CollatorProtocolMessage {
	/// Signal to the collator protocol that it should connect to validators with the expectation
	/// of collating on the given para. This is only expected to be called once, early on, if at
	/// all, and only by the Collation Generation subsystem. As such, it will overwrite the value
	/// of the previous signal.
	///
	/// This should be sent before any `DistributeSegment` message.
	CollateOn(ParaId),
	/// Provide an ordered list of collations to the validators.
	DistributeSegment {
		/// Core index on which every candidate is to be backed on.
		core_index: CoreIndex,
		/// Id of the parachain the candidates are for
		para_id: ParaId,
		/// The segment: scheduling context and candidates, shaped by
		/// descriptor version.
		segment: Segment,
	},
	/// Get a network bridge update.
	#[from]
	NetworkBridgeUpdate(NetworkBridgeEvent<net_protocol::CollatorProtocolMessage>),
	/// We recommended a particular candidate to be seconded, but it was invalid; penalize the
	/// collator.
	///
	/// The hash is the scheduling parent.
	Invalid(Hash, CandidateReceipt),
	/// The candidate we recommended to be seconded was validated successfully.
	///
	/// The hash is the relay parent.
	Seconded(Hash, SignedFullStatement),
	/// A message sent by Cumulus consensus engine to the collator protocol to
	/// pre-connect to backing groups at all allowed relay parents.
	ConnectToBackingGroups,
	/// A message sent by Cumulus consensus engine to the collator protocol to
	/// disconnect from backing groups.
	DisconnectFromBackingGroups,
}

impl Default for CollatorProtocolMessage {
	fn default() -> Self {
		Self::CollateOn(Default::default())
	}
}

/// Messages received by the dispute coordinator subsystem.
///
/// NOTE: Any response oneshots might get cancelled if the `DisputeCoordinator` was not yet
/// properly initialized for some reason.
#[derive(Debug)]
pub enum DisputeCoordinatorMessage {
	/// Import statements by validators about a candidate.
	///
	/// The subsystem will silently discard ancient statements or sets of only dispute-specific
	/// statements for candidates that are previously unknown to the subsystem. The former is
	/// simply because ancient data is not relevant and the latter is as a DoS prevention
	/// mechanism. Both backing and approval statements already undergo anti-DoS procedures in
	/// their respective subsystems, but statements cast specifically for disputes are not
	/// necessarily relevant to any candidate the system is already aware of and thus present a DoS
	/// vector. Our expectation is that nodes will notify each other of disputes over the network
	/// by providing (at least) 2 conflicting statements, of which one is either a backing or
	/// validation statement.
	///
	/// This does not do any checking of the message signature.
	ImportStatements {
		/// The candidate receipt itself.
		candidate_receipt: CandidateReceipt,
		/// The session the candidate appears in.
		session: SessionIndex,
		/// Statements, with signatures checked, by validators participating in disputes.
		///
		/// The validator index passed alongside each statement should correspond to the index
		/// of the validator in the set.
		statements: Vec<(SignedDisputeStatement, ValidatorIndex)>,
		/// Inform the requester once we finished importing (if a sender was provided).
		///
		/// This is:
		/// - we discarded the votes because
		/// 		- they were ancient or otherwise invalid (result: `InvalidImport`)
		/// 		- or we were not able to recover availability for an unknown candidate (result:
		/// 		`InvalidImport`)
		/// 		- or were known already (in that case the result will still be `ValidImport`)
		/// - or we recorded them because (`ValidImport`)
		/// 		- we cast our own vote already on that dispute
		/// 		- or we have approval votes on that candidate
		/// 		- or other explicit votes on that candidate already recorded
		/// 		- or recovered availability for the candidate
		/// 		- or the imported statements are backing/approval votes, which are always accepted.
		pending_confirmation: Option<oneshot::Sender<ImportStatementsResult>>,
	},
	/// Fetch a list of all recent disputes the coordinator is aware of.
	/// These are disputes which have occurred any time in recent sessions,
	/// and which may have already concluded.
	RecentDisputes(oneshot::Sender<BTreeMap<(SessionIndex, CandidateHash), DisputeStatus>>),
	/// Fetch a list of all active disputes that the coordinator is aware of.
	/// These disputes are either not yet concluded or recently concluded.
	ActiveDisputes(oneshot::Sender<BTreeMap<(SessionIndex, CandidateHash), DisputeStatus>>),
	/// Get candidate votes for a candidate.
	QueryCandidateVotes(
		Vec<(SessionIndex, CandidateHash)>,
		oneshot::Sender<Vec<(SessionIndex, CandidateHash, CandidateVotes)>>,
	),
	/// Sign and issue local dispute votes. A value of `true` indicates validity, and `false`
	/// invalidity.
	IssueLocalStatement(SessionIndex, CandidateHash, CandidateReceipt, bool),
	/// Determine the highest undisputed block within the given chain, based on where candidates
	/// were included. If even the base block should not be finalized due to a dispute,
	/// then `None` should be returned on the channel.
	///
	/// The block descriptions begin counting upwards from the block after the given `base_number`.
	/// The `base_number` is typically the number of the last finalized block but may be slightly
	/// higher. This block is inevitably going to be finalized so it is not accounted for by this
	/// function.
	DetermineUndisputedChain {
		/// The lowest possible block to vote on.
		base: (BlockNumber, Hash),
		/// Descriptions of all the blocks counting upwards from the block after the base number
		block_descriptions: Vec<BlockDescription>,
		/// The block to vote on, might be base in case there is no better.
		tx: oneshot::Sender<(BlockNumber, Hash)>,
	},
}

/// The result of `DisputeCoordinatorMessage::ImportStatements`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ImportStatementsResult {
	/// Import was invalid (candidate was not available)  and the sending peer should get banned.
	InvalidImport,
	/// Import was valid and can be confirmed to peer.
	ValidImport,
}

/// Messages going to the dispute distribution subsystem.
#[derive(Debug)]
pub enum DisputeDistributionMessage {
	/// Tell dispute distribution to distribute an explicit dispute statement to
	/// validators.
	SendDispute(DisputeMessage),
}

/// Messages received from other subsystems.
#[derive(Debug)]
pub enum NetworkBridgeRxMessage {
	/// Inform the distribution subsystems about the new
	/// gossip network topology formed.
	///
	/// The only reason to have this here, is the availability of the
	/// authority discovery service, otherwise, the `GossipSupport`
	/// subsystem would make more sense.
	NewGossipTopology {
		/// The session info this gossip topology is concerned with.
		session: SessionIndex,
		/// Our validator index in the session, if any.
		local_index: Option<ValidatorIndex>,
		/// The canonical shuffling of validators for the session.
		canonical_shuffling: Vec<(AuthorityDiscoveryId, ValidatorIndex)>,
		/// The reverse mapping of `canonical_shuffling`: from validator index
		/// to the index in `canonical_shuffling`
		shuffled_indices: Vec<usize>,
	},
	/// Inform the distribution subsystems about `AuthorityDiscoveryId` key rotations.
	UpdatedAuthorityIds {
		/// The `PeerId` of the peer that updated its `AuthorityDiscoveryId`s.
		peer_id: PeerId,
		/// The updated authority discovery keys of the peer.
		authority_ids: HashSet<AuthorityDiscoveryId>,
	},
}

/// Type of peer reporting
#[derive(Debug)]
pub enum ReportPeerMessage {
	/// Single peer report about malicious actions which should be sent right away
	Single(PeerId, ReputationChange),
	/// Delayed report for other actions.
	Batch(HashMap<PeerId, i32>),
}

/// Messages received from other subsystems by the network bridge subsystem.
#[derive(Debug)]
pub enum NetworkBridgeTxMessage {
	/// Report a peer for their actions.
	ReportPeer(ReportPeerMessage),

	/// Disconnect peers from the given peer-set without affecting their reputation.
	DisconnectPeers(Vec<PeerId>, PeerSet),

	/// Send a message to one or more peers on the validation peer-set.
	SendValidationMessage(Vec<PeerId>, net_protocol::VersionedValidationProtocol),

	/// Send a message to one or more peers on the collation peer-set.
	SendCollationMessage(Vec<PeerId>, net_protocol::VersionedCollationProtocol),

	/// Send a batch of validation messages.
	///
	/// NOTE: Messages will be processed in order (at least statement distribution relies on this).
	SendValidationMessages(Vec<(Vec<PeerId>, net_protocol::VersionedValidationProtocol)>),

	/// Send a batch of collation messages.
	///
	/// NOTE: Messages will be processed in order.
	SendCollationMessages(Vec<(Vec<PeerId>, net_protocol::VersionedCollationProtocol)>),

	/// Send requests via substrate request/response.
	/// Second parameter, tells what to do if we are not yet connected to the peer.
	SendRequests(Vec<Requests>, IfDisconnected),

	/// Connect to peers who represent the given `validator_ids`.
	///
	/// Also ask the network to stay connected to these peers at least
	/// until a new request is issued.
	///
	/// Because it overrides the previous request, it must be ensured
	/// that `validator_ids` include all peers the subsystems
	/// are interested in (per `PeerSet`).
	///
	/// A caller can learn about validator connections by listening to the
	/// `PeerConnected` events from the network bridge.
	ConnectToValidators {
		/// Ids of the validators to connect to.
		validator_ids: Vec<AuthorityDiscoveryId>,
		/// The underlying protocol to use for this request.
		peer_set: PeerSet,
		/// Sends back the number of `AuthorityDiscoveryId`s which
		/// authority discovery has failed to resolve.
		failed: oneshot::Sender<usize>,
	},
	/// Alternative to `ConnectToValidators` in case you already know the `Multiaddrs` you want to
	/// be connected to.
	ConnectToResolvedValidators {
		/// Each entry corresponds to the addresses of an already resolved validator.
		validator_addrs: Vec<HashSet<Multiaddr>>,
		/// The peer set we want the connection on.
		peer_set: PeerSet,
	},

	/// Extends the known validators set with new peers we already know the `Multiaddrs`, this is
	/// usually needed for validators that change their address mid-session. It is usually called
	/// after a ConnectToResolvedValidators at the beginning of the session.
	AddToResolvedValidators {
		/// Each entry corresponds to the addresses of an already resolved validator.
		validator_addrs: Vec<HashSet<Multiaddr>>,
		/// The peer set we want the connection on.
		peer_set: PeerSet,
	},
}

/// Availability Distribution Message.
#[derive(Debug)]
pub enum AvailabilityDistributionMessage {
	/// Instruct availability distribution to fetch a remote PoV.
	///
	/// NOTE: The result of this fetch is not yet locally validated and could be bogus.
	FetchPoV {
		/// The relay parent giving the necessary context.
		relay_parent: Hash,
		/// Validator to fetch the PoV from.
		from_validator: ValidatorIndex,
		/// The id of the parachain that produced this PoV.
		/// This field is only used to provide more context when logging errors
		/// from the `AvailabilityDistribution` subsystem.
		para_id: ParaId,
		/// Candidate hash to fetch the PoV for.
		candidate_hash: CandidateHash,
		/// Expected hash of the PoV, a PoV not matching this hash will be rejected.
		pov_hash: Hash,
		/// Sender for getting back the result of this fetch.
		///
		/// The sender will be canceled if the fetching failed for some reason.
		tx: oneshot::Sender<PoV>,
	},
}

/// Availability Recovery Message.
#[derive(Debug, derive_more::From)]
pub enum AvailabilityRecoveryMessage {
	/// Recover available data from validators on the network.
	RecoverAvailableData(
		CandidateReceipt,
		SessionIndex,
		Option<GroupIndex>, // Optional backing group to request from first.
		Option<CoreIndex>,  /* A `CoreIndex` needs to be specified for the recovery process to
		                     * prefer systematic chunk recovery. */
		oneshot::Sender<Result<AvailableData, crate::errors::RecoveryError>>,
	),
}

/// Bitfield distribution message.
#[derive(Debug, derive_more::From)]
pub enum BitfieldDistributionMessage {
	/// Distribute a bitfield via gossip to other validators.
	DistributeBitfield(Hash, SignedAvailabilityBitfield),

	/// Event from the network bridge.
	#[from]
	NetworkBridgeUpdate(NetworkBridgeEvent<net_protocol::BitfieldDistributionMessage>),
}

/// Availability store subsystem message.
#[derive(Debug)]
pub enum AvailabilityStoreMessage {
	/// Query a `AvailableData` from the AV store.
	QueryAvailableData(CandidateHash, oneshot::Sender<Option<AvailableData>>),

	/// Query whether a `AvailableData` exists within the AV Store.
	///
	/// This is useful in cases when existence
	/// matters, but we don't want to necessarily pass around multiple
	/// megabytes of data to get a single bit of information.
	QueryDataAvailability(CandidateHash, oneshot::Sender<bool>),

	/// Query an `ErasureChunk` from the AV store by the candidate hash and validator index.
	QueryChunk(CandidateHash, ValidatorIndex, oneshot::Sender<Option<ErasureChunk>>),

	/// Get the size of an `ErasureChunk` from the AV store by the candidate hash.
	QueryChunkSize(CandidateHash, oneshot::Sender<Option<usize>>),

	/// Query all chunks that we have for the given candidate hash.
	QueryAllChunks(CandidateHash, oneshot::Sender<Vec<(ValidatorIndex, ErasureChunk)>>),

	/// Query whether an `ErasureChunk` exists within the AV Store.
	///
	/// This is useful in cases like bitfield signing, when existence
	/// matters, but we don't want to necessarily pass around large
	/// quantities of data to get a single bit of information.
	QueryChunkAvailability(CandidateHash, ValidatorIndex, oneshot::Sender<bool>),

	/// Store an `ErasureChunk` in the AV store.
	///
	/// Return `Ok(())` if the store operation succeeded, `Err(())` if it failed.
	StoreChunk {
		/// A hash of the candidate this chunk belongs to.
		candidate_hash: CandidateHash,
		/// Validator index. May not be equal to the chunk index.
		validator_index: ValidatorIndex,
		/// The chunk itself.
		chunk: ErasureChunk,
		/// Sending side of the channel to send result to.
		tx: oneshot::Sender<Result<(), ()>>,
	},

	/// Computes and checks the erasure root of `AvailableData` before storing all of its chunks in
	/// the AV store.
	///
	/// Return `Ok(())` if the store operation succeeded, `Err(StoreAvailableData)` if it failed.
	StoreAvailableData {
		/// A hash of the candidate this `available_data` belongs to.
		candidate_hash: CandidateHash,
		/// The number of validators in the session.
		n_validators: u32,
		/// The `AvailableData` itself.
		available_data: AvailableData,
		/// Erasure root we expect to get after chunking.
		expected_erasure_root: Hash,
		/// Core index where the candidate was backed.
		core_index: CoreIndex,
		/// Node features at the candidate relay parent. Used for computing the validator->chunk
		/// mapping.
		node_features: NodeFeatures,
		/// Sending side of the channel to send result to.
		tx: oneshot::Sender<Result<(), StoreAvailableDataError>>,
	},
}

/// The error result type of a [`AvailabilityStoreMessage::StoreAvailableData`] request.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum StoreAvailableDataError {
	#[error("The computed erasure root did not match expected one")]
	InvalidErasureRoot,
}

/// A response channel for the result of a chain API request.
pub type ChainApiResponseChannel<T> = oneshot::Sender<Result<T, crate::errors::ChainApiError>>;

/// Chain API request subsystem message.
#[derive(Debug)]
pub enum ChainApiMessage {
	/// Request the block number by hash.
	/// Returns `None` if a block with the given hash is not present in the db.
	BlockNumber(Hash, ChainApiResponseChannel<Option<BlockNumber>>),
	/// Request the block header by hash.
	/// Returns `None` if a block with the given hash is not present in the db.
	BlockHeader(Hash, ChainApiResponseChannel<Option<BlockHeader>>),
	/// Get the cumulative weight of the given block, by hash.
	/// If the block or weight is unknown, this returns `None`.
	///
	/// Note: this is the weight within the low-level fork-choice rule,
	/// not the high-level one implemented in the chain-selection subsystem.
	///
	/// Weight is used for comparing blocks in a fork-choice rule.
	BlockWeight(Hash, ChainApiResponseChannel<Option<BlockWeight>>),
	/// Request the finalized block hash by number.
	/// Returns `None` if a block with the given number is not present in the db.
	/// Note: the caller must ensure the block is finalized.
	FinalizedBlockHash(BlockNumber, ChainApiResponseChannel<Option<Hash>>),
	/// Request the last finalized block number.
	/// This request always succeeds.
	FinalizedBlockNumber(ChainApiResponseChannel<BlockNumber>),
	/// Request the `k` ancestor block hashes of a block with the given hash.
	/// The response channel may return a `Vec` of size up to `k`
	/// filled with ancestors hashes with the following order:
	/// `parent`, `grandparent`, ... up to the hash of genesis block
	/// with number 0, including it.
	Ancestors {
		/// The hash of the block in question.
		hash: Hash,
		/// The number of ancestors to request.
		k: usize,
		/// The response channel.
		response_channel: ChainApiResponseChannel<Vec<Hash>>,
	},
}

/// Chain selection subsystem messages
#[derive(Debug)]
pub enum ChainSelectionMessage {
	/// Signal to the chain selection subsystem that a specific block has been approved.
	Approved(Hash),
	/// Request the leaves in descending order by score.
	Leaves(oneshot::Sender<Vec<Hash>>),
	/// Request the best leaf containing the given block in its ancestry. Return `None` if
	/// there is no such leaf.
	BestLeafContaining(Hash, oneshot::Sender<Option<Hash>>),
	/// The passed blocks must be marked as reverted, and their children must be marked
	/// as non-viable.
	RevertBlocks(Vec<(BlockNumber, Hash)>),
}

/// A sender for the result of a runtime API request.
pub type RuntimeApiSender<T> = oneshot::Sender<Result<T, crate::errors::RuntimeApiError>>;

/// A request to the Runtime API subsystem.
#[derive(Debug)]
pub enum RuntimeApiRequest {
	/// Get the version of the runtime API, if any.
	Version(RuntimeApiSender<u32>),
	/// Get the next, current and some previous authority discovery set deduplicated.
	Authorities(RuntimeApiSender<Vec<AuthorityDiscoveryId>>),
	/// Get the current validator set.
	Validators(RuntimeApiSender<Vec<ValidatorId>>),
	/// Get the validator groups and group rotation info.
	ValidatorGroups(RuntimeApiSender<(Vec<Vec<ValidatorIndex>>, GroupRotationInfo)>),
	/// Get information on all availability cores.
	AvailabilityCores(RuntimeApiSender<Vec<CoreState>>),
	/// Get the persisted validation data for a particular para, taking the given
	/// `OccupiedCoreAssumption`, which will inform on how the validation data should be computed
	/// if the para currently occupies a core.
	PersistedValidationData(
		ParaId,
		OccupiedCoreAssumption,
		RuntimeApiSender<Option<PersistedValidationData>>,
	),
	/// Get the persisted validation data for a particular para along with the current validation
	/// code hash, matching the data hash against an expected one.
	AssumedValidationData(
		ParaId,
		Hash,
		RuntimeApiSender<Option<(PersistedValidationData, ValidationCodeHash)>>,
	),
	/// Sends back `true` if the validation outputs pass all acceptance criteria checks.
	CheckValidationOutputs(
		ParaId,
		polkadot_primitives::CandidateCommitments,
		RuntimeApiSender<bool>,
	),
	/// Get the session index that a child of the block will have.
	SessionIndexForChild(RuntimeApiSender<SessionIndex>),
	/// Get the validation code for a para, taking the given `OccupiedCoreAssumption`, which
	/// will inform on how the validation data should be computed if the para currently
	/// occupies a core.
	ValidationCode(ParaId, OccupiedCoreAssumption, RuntimeApiSender<Option<ValidationCode>>),
	/// Get validation code by its hash, either past, current or future code can be returned, as
	/// long as state is still available.
	ValidationCodeByHash(ValidationCodeHash, RuntimeApiSender<Option<ValidationCode>>),
	/// Get the candidate pending availability for a particular parachain by parachain / core
	/// index
	CandidatePendingAvailability(ParaId, RuntimeApiSender<Option<CommittedCandidateReceipt>>),
	/// Get all events concerning candidates (backing, inclusion, time-out) in the parent of
	/// the block in whose state this request is executed.
	CandidateEvents(RuntimeApiSender<Vec<CandidateEvent>>),
	/// Get the execution environment parameter set by session index
	SessionExecutorParams(SessionIndex, RuntimeApiSender<Option<ExecutorParams>>),
	/// Get the session info for the given session, if stored.
	SessionInfo(SessionIndex, RuntimeApiSender<Option<SessionInfo>>),
	/// Get all the pending inbound messages in the downward message queue for a para.
	DmqContents(ParaId, RuntimeApiSender<Vec<InboundDownwardMessage<BlockNumber>>>),
	/// Get the contents of all channels addressed to the given recipient. Channels that have no
	/// messages in them are also included.
	InboundHrmpChannelsContents(
		ParaId,
		RuntimeApiSender<BTreeMap<ParaId, Vec<InboundHrmpMessage<BlockNumber>>>>,
	),
	/// Get information about the BABE epoch the block was included in.
	CurrentBabeEpoch(RuntimeApiSender<BabeEpoch>),
	/// Get all disputes in relation to a relay parent.
	FetchOnChainVotes(RuntimeApiSender<Option<polkadot_primitives::ScrapedOnChainVotes>>),
	/// Submits a PVF pre-checking statement into the transaction pool.
	SubmitPvfCheckStatement(PvfCheckStatement, ValidatorSignature, RuntimeApiSender<()>),
	/// Returns code hashes of PVFs that require pre-checking by validators in the active set.
	PvfsRequirePrecheck(RuntimeApiSender<Vec<ValidationCodeHash>>),
	/// Get the validation code used by the specified para, taking the given
	/// `OccupiedCoreAssumption`, which will inform on how the validation data should be computed
	/// if the para currently occupies a core.
	ValidationCodeHash(
		ParaId,
		OccupiedCoreAssumption,
		RuntimeApiSender<Option<ValidationCodeHash>>,
	),
	/// Returns all on-chain disputes at given block number. Available in `v3`.
	Disputes(RuntimeApiSender<Vec<(SessionIndex, CandidateHash, DisputeState<BlockNumber>)>>),
	/// Returns a list of validators that lost a past session dispute and need to be slashed.
	/// `V5`
	UnappliedSlashes(
		RuntimeApiSender<Vec<(SessionIndex, CandidateHash, slashing::LegacyPendingSlashes)>>,
	),
	/// Returns a merkle proof of a validator session key.
	/// `V5`
	KeyOwnershipProof(ValidatorId, RuntimeApiSender<Option<slashing::OpaqueKeyOwnershipProof>>),
	/// Submits an unsigned extrinsic to slash validator who lost a past session dispute.
	/// `V5`
	SubmitReportDisputeLost(
		slashing::DisputeProof,
		slashing::OpaqueKeyOwnershipProof,
		RuntimeApiSender<Option<()>>,
	),
	/// Get the minimum required backing votes.
	MinimumBackingVotes(SessionIndex, RuntimeApiSender<u32>),
	/// Returns all disabled validators at a given block height.
	DisabledValidators(RuntimeApiSender<Vec<ValidatorIndex>>),
	/// Get the backing state of the given para.
	ParaBackingState(ParaId, RuntimeApiSender<Option<async_backing::BackingState>>),
	/// Get candidate's acceptance limitations for asynchronous backing for a relay parent.
	///
	/// If it's not supported by the Runtime, the async backing is said to be disabled.
	AsyncBackingParams(RuntimeApiSender<async_backing::AsyncBackingParams>),
	/// Get the node features.
	NodeFeatures(SessionIndex, RuntimeApiSender<NodeFeatures>),
	/// Approval voting params
	/// `V10`
	ApprovalVotingParams(SessionIndex, RuntimeApiSender<ApprovalVotingParams>),
	/// Fetch the `ClaimQueue` from scheduler pallet
	/// `V11`
	ClaimQueue(RuntimeApiSender<BTreeMap<CoreIndex, VecDeque<ParaId>>>),
	/// Get the candidates pending availability for a particular parachain
	/// `V11`
	CandidatesPendingAvailability(ParaId, RuntimeApiSender<Vec<CommittedCandidateReceipt>>),
	/// Get the backing constraints for a particular parachain.
	/// `V12`
	BackingConstraints(ParaId, RuntimeApiSender<Option<Constraints>>),
	/// Get the lookahead from the scheduler params.
	/// `V12`
	SchedulingLookahead(SessionIndex, RuntimeApiSender<u32>),
	/// Get the maximum uncompressed code size.
	/// `V12`
	ValidationCodeBombLimit(SessionIndex, RuntimeApiSender<u32>),
	/// Get the paraids at the relay parent.
	/// `V14`
	ParaIds(SessionIndex, RuntimeApiSender<Vec<ParaId>>),
	/// Returns a list of validators that lost a past session dispute and need to be slashed (v2).
	/// `V15`
	UnappliedSlashesV2(
		RuntimeApiSender<Vec<(SessionIndex, CandidateHash, slashing::PendingSlashes)>>,
	),
	/// Get the maximum relay parent session age allowed for parachain blocks.
	/// `V16`
	MaxRelayParentSessionAge(SessionIndex, RuntimeApiSender<u32>),
	/// Look up relay parent info for an **ancestor** block. A block is not in its
	/// own `AllowedRelayParents`, so querying a block about itself returns `None`.
	/// Use the node-side `check_relay_parent_session` utility for the general case. `V16`
	AncestorRelayParentInfo(
		SessionIndex,
		Hash,
		RuntimeApiSender<Option<RelayParentInfo<Hash, BlockNumber>>>,
	),
}

impl RuntimeApiRequest {
	/// Runtime version requirements for each message

	/// `Disputes`
	pub const DISPUTES_RUNTIME_REQUIREMENT: u32 = 3;

	/// `ExecutorParams`
	pub const EXECUTOR_PARAMS_RUNTIME_REQUIREMENT: u32 = 4;

	/// `UnappliedSlashes`
	pub const UNAPPLIED_SLASHES_RUNTIME_REQUIREMENT: u32 = 5;

	/// `KeyOwnershipProof`
	pub const KEY_OWNERSHIP_PROOF_RUNTIME_REQUIREMENT: u32 = 5;

	/// `SubmitReportDisputeLost`
	pub const SUBMIT_REPORT_DISPUTE_LOST_RUNTIME_REQUIREMENT: u32 = 5;

	/// `MinimumBackingVotes`
	pub const MINIMUM_BACKING_VOTES_RUNTIME_REQUIREMENT: u32 = 6;

	/// Minimum version to enable asynchronous backing: `AsyncBackingParams` and `ParaBackingState`.
	pub const ASYNC_BACKING_STATE_RUNTIME_REQUIREMENT: u32 = 7;

	/// `DisabledValidators`
	pub const DISABLED_VALIDATORS_RUNTIME_REQUIREMENT: u32 = 8;

	/// `Node features`
	pub const NODE_FEATURES_RUNTIME_REQUIREMENT: u32 = 9;

	/// `approval_voting_params`
	pub const APPROVAL_VOTING_PARAMS_REQUIREMENT: u32 = 10;

	/// `ClaimQueue`
	pub const CLAIM_QUEUE_RUNTIME_REQUIREMENT: u32 = 11;

	/// `candidates_pending_availability`
	pub const CANDIDATES_PENDING_AVAILABILITY_RUNTIME_REQUIREMENT: u32 = 11;

	/// `ValidationCodeBombLimit`
	pub const VALIDATION_CODE_BOMB_LIMIT_RUNTIME_REQUIREMENT: u32 = 12;

	/// `backing_constraints`
	pub const CONSTRAINTS_RUNTIME_REQUIREMENT: u32 = 13;

	/// `SchedulingLookahead`
	pub const SCHEDULING_LOOKAHEAD_RUNTIME_REQUIREMENT: u32 = 13;

	/// `ParaIds`
	pub const PARAIDS_RUNTIME_REQUIREMENT: u32 = 14;

	/// `UnappliedSlashesV2`
	pub const UNAPPLIED_SLASHES_V2_RUNTIME_REQUIREMENT: u32 = 15;

	/// `MaxRelayParentSessionAge`
	pub const MAX_RELAY_PARENT_SESSION_AGE_RUNTIME_REQUIREMENT: u32 = 16;

	/// `AncestorRelayParentInfo`
	pub const ANCESTOR_RELAY_PARENT_INFO_RUNTIME_REQUIREMENT: u32 = 16;
}

/// A message to the Runtime API subsystem.
#[derive(Debug)]
pub enum RuntimeApiMessage {
	/// Make a request of the runtime API against the post-state of the given relay-parent.
	Request(Hash, RuntimeApiRequest),
}

/// Statement distribution message.
#[derive(Debug, derive_more::From)]
pub enum StatementDistributionMessage {
	/// We have originated a signed statement in the context of
	/// given relay-parent hash and it should be distributed to other validators.
	Share(Hash, SignedFullStatementWithPVD),
	/// The candidate received enough validity votes from the backing group.
	///
	/// If the candidate is backed as a result of a local statement, this message MUST
	/// be preceded by a `Share` message for that statement. This ensures that Statement
	/// Distribution is always aware of full candidates prior to receiving the `Backed`
	/// notification, even when the group size is 1 and the candidate is seconded locally.
	Backed(CandidateHash),
	/// Event from the network bridge.
	#[from]
	NetworkBridgeUpdate(NetworkBridgeEvent<net_protocol::StatementDistributionMessage>),
}

/// This data becomes intrinsics or extrinsics which should be included in a future relay chain
/// block.
// It needs to be clonable because multiple potential block authors can request copies.
#[derive(Debug, Clone)]
pub enum ProvisionableData {
	/// This bitfield indicates the availability of various candidate blocks.
	Bitfield(Hash, SignedAvailabilityBitfield),
	/// Misbehavior reports are self-contained proofs of validator misbehavior.
	MisbehaviorReport(Hash, ValidatorIndex, Misbehavior),
	/// Disputes trigger a broad dispute resolution process.
	Dispute(Hash, ValidatorSignature),
}

/// Inherent data returned by the provisioner
#[derive(Debug, Clone)]
pub struct ProvisionerInherentData {
	/// Signed bitfields.
	pub bitfields: SignedAvailabilityBitfields,
	/// Backed candidates.
	pub backed_candidates: Vec<BackedCandidate>,
	/// Dispute statement sets.
	pub disputes: MultiDisputeStatementSet,
}

/// Message to the Provisioner.
///
/// In all cases, the Hash is that of the relay parent.
#[derive(Debug)]
pub enum ProvisionerMessage {
	/// This message allows external subsystems to request the set of bitfields and backed
	/// candidates associated with a particular potential block hash.
	///
	/// This is expected to be used by a proposer, to inject that information into the
	/// `InherentData` where it can be assembled into the `ParaInherent`.
	RequestInherentData(Hash, oneshot::Sender<ProvisionerInherentData>),
	/// This data should become part of a relay chain block
	ProvisionableData(Hash, ProvisionableData),
}

/// Message to the Collation Generation subsystem.
#[derive(Debug)]
pub enum CollationGenerationMessage {
	/// Initialize the collation generation subsystem.
	Initialize(CollationGenerationConfig),
	/// Reinitialize the collation generation subsystem, overriding the existing config.
	Reinitialize(CollationGenerationConfig),
	/// Submit a segment of collations that share a scheduling parent and target core. Each
	/// collation is packaged into a signed [`CommittedCandidateReceipt`] and distributed to
	/// validators, in the order they appear in [`SubmitSegmentParams::collations`].
	///
	/// If sent before `Initialize`, this will be ignored.
	SubmitSegment(SubmitSegmentParams),
}

/// The result type of [`ApprovalVotingMessage::ImportAssignment`] request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignmentCheckResult {
	/// The vote was accepted and should be propagated onwards.
	Accepted,
	/// The vote was valid but duplicate and should not be propagated onwards.
	AcceptedDuplicate,
	/// The vote was valid but too far in the future to accept right now.
	TooFarInFuture,
	/// The vote was bad and should be ignored, reporting the peer who propagated it.
	Bad(AssignmentCheckError),
}

/// The error result type of [`ApprovalVotingMessage::ImportAssignment`] request.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum AssignmentCheckError {
	#[error("Unknown block: {0:?}")]
	UnknownBlock(Hash),
	#[error("Unknown session index: {0}")]
	UnknownSessionIndex(SessionIndex),
	#[error("Invalid candidate index: {0}")]
	InvalidCandidateIndex(CandidateIndex),
	#[error("Invalid candidate {0}: {1:?}")]
	InvalidCandidate(CandidateIndex, CandidateHash),
	#[error("Invalid cert: {0:?}, reason: {1}")]
	InvalidCert(ValidatorIndex, String),
	#[error("Internal state mismatch: {0:?}, {1:?}")]
	Internal(Hash, CandidateHash),
	#[error("Oversized candidate or core bitfield >= {0}")]
	InvalidBitfield(usize),
}

/// The result type of [`ApprovalVotingMessage::ImportApproval`] request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalCheckResult {
	/// The vote was accepted and should be propagated onwards.
	Accepted,
	/// The vote was bad and should be ignored, reporting the peer who propagated it.
	Bad(ApprovalCheckError),
}

/// The error result type of [`ApprovalVotingMessage::ImportApproval`] request.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum ApprovalCheckError {
	#[error("Unknown block: {0:?}")]
	UnknownBlock(Hash),
	#[error("Unknown session index: {0}")]
	UnknownSessionIndex(SessionIndex),
	#[error("Invalid candidate index: {0}")]
	InvalidCandidateIndex(CandidateIndex),
	#[error("Invalid validator index: {0:?}")]
	InvalidValidatorIndex(ValidatorIndex),
	#[error("Invalid candidate {0}: {1:?}")]
	InvalidCandidate(CandidateIndex, CandidateHash),
	#[error("Invalid signature: {0:?}")]
	InvalidSignature(ValidatorIndex),
	#[error("No assignment for {0:?}")]
	NoAssignment(ValidatorIndex),
	#[error("Internal state mismatch: {0:?}, {1:?}")]
	Internal(Hash, CandidateHash),
}

/// Describes a relay-chain block by the para-chain candidates
/// it includes.
#[derive(Clone, Debug)]
pub struct BlockDescription {
	/// The relay-chain block hash.
	pub block_hash: Hash,
	/// The session index of this block.
	pub session: SessionIndex,
	/// The set of para-chain candidates.
	pub candidates: Vec<CandidateHash>,
}

/// Message to the approval voting parallel subsystem running both approval-distribution and
/// approval-voting logic in parallel. This is a combination of all the messages ApprovalVoting and
/// ApprovalDistribution subsystems can receive.
///
/// The reason this exists is, so that we can keep both modes of running in the same polkadot
/// binary, based on the value of `--approval-voting-parallel-enabled`, we decide if we run with two
/// different subsystems for approval-distribution and approval-voting or run the approval-voting
/// parallel which has several parallel workers for the approval-distribution and a worker for
/// approval-voting.
///
/// This is meant to be a temporary state until we can safely remove running the two subsystems
/// individually.
#[derive(Debug, derive_more::From)]
pub enum ApprovalVotingParallelMessage {
	/// Gets mapped into `ApprovalVotingMessage::ApprovedAncestor`
	ApprovedAncestor(Hash, BlockNumber, oneshot::Sender<Option<HighestApprovedAncestorBlock>>),

	/// Gets mapped into `ApprovalVotingMessage::GetApprovalSignaturesForCandidate`
	GetApprovalSignaturesForCandidate(
		CandidateHash,
		oneshot::Sender<
			HashMap<ValidatorIndex, (CoalescedApprovalCandidateHashes, ValidatorSignature)>,
		>,
	),
	/// Gets mapped into `ApprovalDistributionMessage::NewBlocks`
	NewBlocks(Vec<BlockApprovalMeta>),
	/// Gets mapped into `ApprovalDistributionMessage::DistributeAssignment`
	DistributeAssignment(IndirectAssignmentCertV2, CandidateBitfield),
	/// Gets mapped into `ApprovalDistributionMessage::DistributeApproval`
	DistributeApproval(IndirectSignedApprovalVoteV2),
	/// An update from the network bridge, gets mapped into
	/// `ApprovalDistributionMessage::NetworkBridgeUpdate`
	#[from]
	NetworkBridgeUpdate(NetworkBridgeEvent<net_protocol::ApprovalDistributionMessage>),

	/// Gets mapped into `ApprovalDistributionMessage::GetApprovalSignatures`
	GetApprovalSignatures(
		HashSet<(Hash, CandidateIndex)>,
		oneshot::Sender<HashMap<ValidatorIndex, (Hash, Vec<CandidateIndex>, ValidatorSignature)>>,
	),
	/// Gets mapped into `ApprovalDistributionMessage::ApprovalCheckingLagUpdate`
	ApprovalCheckingLagUpdate(BlockNumber),
}

impl TryFrom<ApprovalVotingParallelMessage> for ApprovalVotingMessage {
	type Error = ();

	fn try_from(msg: ApprovalVotingParallelMessage) -> Result<Self, Self::Error> {
		match msg {
			ApprovalVotingParallelMessage::ApprovedAncestor(hash, number, tx) => {
				Ok(ApprovalVotingMessage::ApprovedAncestor(hash, number, tx))
			},
			ApprovalVotingParallelMessage::GetApprovalSignaturesForCandidate(candidate, tx) => {
				Ok(ApprovalVotingMessage::GetApprovalSignaturesForCandidate(candidate, tx))
			},
			_ => Err(()),
		}
	}
}

impl TryFrom<ApprovalVotingParallelMessage> for ApprovalDistributionMessage {
	type Error = ();

	fn try_from(msg: ApprovalVotingParallelMessage) -> Result<Self, Self::Error> {
		match msg {
			ApprovalVotingParallelMessage::NewBlocks(blocks) => {
				Ok(ApprovalDistributionMessage::NewBlocks(blocks))
			},
			ApprovalVotingParallelMessage::DistributeAssignment(assignment, claimed_cores) => {
				Ok(ApprovalDistributionMessage::DistributeAssignment(assignment, claimed_cores))
			},
			ApprovalVotingParallelMessage::DistributeApproval(vote) => {
				Ok(ApprovalDistributionMessage::DistributeApproval(vote))
			},
			ApprovalVotingParallelMessage::NetworkBridgeUpdate(msg) => {
				Ok(ApprovalDistributionMessage::NetworkBridgeUpdate(msg))
			},
			ApprovalVotingParallelMessage::GetApprovalSignatures(candidate_indicies, tx) => {
				Ok(ApprovalDistributionMessage::GetApprovalSignatures(candidate_indicies, tx))
			},
			ApprovalVotingParallelMessage::ApprovalCheckingLagUpdate(lag) => {
				Ok(ApprovalDistributionMessage::ApprovalCheckingLagUpdate(lag))
			},
			_ => Err(()),
		}
	}
}

impl From<ApprovalDistributionMessage> for ApprovalVotingParallelMessage {
	fn from(msg: ApprovalDistributionMessage) -> Self {
		match msg {
			ApprovalDistributionMessage::NewBlocks(blocks) => {
				ApprovalVotingParallelMessage::NewBlocks(blocks)
			},
			ApprovalDistributionMessage::DistributeAssignment(cert, bitfield) => {
				ApprovalVotingParallelMessage::DistributeAssignment(cert, bitfield)
			},
			ApprovalDistributionMessage::DistributeApproval(vote) => {
				ApprovalVotingParallelMessage::DistributeApproval(vote)
			},
			ApprovalDistributionMessage::NetworkBridgeUpdate(msg) => {
				ApprovalVotingParallelMessage::NetworkBridgeUpdate(msg)
			},
			ApprovalDistributionMessage::GetApprovalSignatures(candidate_indicies, tx) => {
				ApprovalVotingParallelMessage::GetApprovalSignatures(candidate_indicies, tx)
			},
			ApprovalDistributionMessage::ApprovalCheckingLagUpdate(lag) => {
				ApprovalVotingParallelMessage::ApprovalCheckingLagUpdate(lag)
			},
		}
	}
}

/// Response type to `ApprovalVotingMessage::ApprovedAncestor`.
#[derive(Clone, Debug)]
pub struct HighestApprovedAncestorBlock {
	/// The block hash of the highest viable ancestor.
	pub hash: Hash,
	/// The block number of the highest viable ancestor.
	pub number: BlockNumber,
	/// Block descriptions in the direct path between the
	/// initially provided hash and the highest viable ancestor.
	/// Primarily for use with `DetermineUndisputedChain`.
	/// Must be sorted from lowest to highest block number.
	pub descriptions: Vec<BlockDescription>,
}

/// A checked indirect assignment, the crypto for the cert has been validated
/// and the `candidate_bitfield` is correctly claimed at `delay_tranche`.
#[derive(Debug)]
pub struct CheckedIndirectAssignment {
	assignment: IndirectAssignmentCertV2,
	candidate_indices: CandidateBitfield,
	tranche: DelayTranche,
}

impl CheckedIndirectAssignment {
	/// Builds a checked assignment from an assignment that was checked to be valid for the
	/// `claimed_candidate_indices` at the give tranche
	pub fn from_checked(
		assignment: IndirectAssignmentCertV2,
		claimed_candidate_indices: CandidateBitfield,
		tranche: DelayTranche,
	) -> Self {
		Self { assignment, candidate_indices: claimed_candidate_indices, tranche }
	}

	/// Returns the indirect assignment.
	pub fn assignment(&self) -> &IndirectAssignmentCertV2 {
		&self.assignment
	}

	/// Returns the candidate bitfield claimed by the assignment.
	pub fn candidate_indices(&self) -> &CandidateBitfield {
		&self.candidate_indices
	}

	/// Returns the tranche this assignment is claimed at.
	pub fn tranche(&self) -> DelayTranche {
		self.tranche
	}
}

/// A checked indirect signed approval vote.
///
/// The crypto for the vote has been validated and the signature can be trusted as being valid and
/// to correspond to the `validator_index` inside the structure.
#[derive(Debug, derive_more::Deref, derive_more::Into)]
pub struct CheckedIndirectSignedApprovalVote(IndirectSignedApprovalVoteV2);

impl CheckedIndirectSignedApprovalVote {
	/// Builds a checked vote from a vote that was checked to be valid and correctly signed.
	pub fn from_checked(vote: IndirectSignedApprovalVoteV2) -> Self {
		Self(vote)
	}
}

/// Message to the Approval Voting subsystem.
#[derive(Debug)]
pub enum ApprovalVotingMessage {
	/// Import an assignment into the approval-voting database.
	///
	/// Should not be sent unless the block hash is known and the VRF assignment checks out.
	ImportAssignment(CheckedIndirectAssignment, Option<oneshot::Sender<AssignmentCheckResult>>),
	/// Import an approval vote into approval-voting database
	///
	/// Should not be sent unless the block hash within the indirect vote is known, vote is
	/// correctly signed and we had a previous assignment for the candidate.
	ImportApproval(CheckedIndirectSignedApprovalVote, Option<oneshot::Sender<ApprovalCheckResult>>),
	/// Returns the highest possible ancestor hash of the provided block hash which is
	/// acceptable to vote on finality for.
	/// The `BlockNumber` provided is the number of the block's ancestor which is the
	/// earliest possible vote.
	///
	/// It can also return the same block hash, if that is acceptable to vote upon.
	/// Return `None` if the input hash is unrecognized.
	ApprovedAncestor(Hash, BlockNumber, oneshot::Sender<Option<HighestApprovedAncestorBlock>>),

	/// Retrieve all available approval signatures for a candidate from approval-voting.
	///
	/// This message involves a linear search for candidates on each relay chain fork and also
	/// requires calling into `approval-distribution`: Calls should be infrequent and bounded.
	GetApprovalSignaturesForCandidate(
		CandidateHash,
		oneshot::Sender<
			HashMap<ValidatorIndex, (CoalescedApprovalCandidateHashes, ValidatorSignature)>,
		>,
	),
}

/// Message to the Approval Distribution subsystem.
#[derive(Debug, derive_more::From)]
pub enum ApprovalDistributionMessage {
	/// Notify the `ApprovalDistribution` subsystem about new blocks
	/// and the candidates contained within them.
	NewBlocks(Vec<BlockApprovalMeta>),
	/// Distribute an assignment cert from the local validator. The cert is assumed
	/// to be valid, relevant, and for the given relay-parent and validator index.
	DistributeAssignment(IndirectAssignmentCertV2, CandidateBitfield),
	/// Distribute an approval vote for the local validator. The approval vote is assumed to be
	/// valid, relevant, and the corresponding approval already issued.
	/// If not, the subsystem is free to drop the message.
	DistributeApproval(IndirectSignedApprovalVoteV2),
	/// An update from the network bridge.
	#[from]
	NetworkBridgeUpdate(NetworkBridgeEvent<net_protocol::ApprovalDistributionMessage>),

	/// Get all approval signatures for all chains a candidate appeared in.
	GetApprovalSignatures(
		HashSet<(Hash, CandidateIndex)>,
		oneshot::Sender<HashMap<ValidatorIndex, (Hash, Vec<CandidateIndex>, ValidatorSignature)>>,
	),
	/// Approval checking lag update measured in blocks.
	ApprovalCheckingLagUpdate(BlockNumber),
}

/// Message to the Gossip Support subsystem.
#[derive(Debug, derive_more::From)]
pub enum GossipSupportMessage {
	/// Dummy constructor, so we can receive networking events.
	#[from]
	NetworkBridgeUpdate(NetworkBridgeEvent<net_protocol::GossipSupportNetworkMessage>),
}

/// Request introduction of a seconded candidate into the prospective parachains subsystem.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct IntroduceSecondedCandidateRequest {
	/// The para-id of the candidate.
	pub candidate_para: ParaId,
	/// The candidate receipt itself.
	pub candidate_receipt: CommittedCandidateReceipt,
	/// The persisted validation data of the candidate.
	pub persisted_validation_data: PersistedValidationData,
}

/// A hypothetical candidate to be evaluated for potential/actual membership
/// in the prospective parachains subsystem.
///
/// Hypothetical candidates are either complete or incomplete.
/// Complete candidates have already had their (potentially heavy)
/// candidate receipt fetched, while incomplete candidates are simply
/// claims about properties that a fetched candidate would have.
///
/// Complete candidates can be evaluated more strictly than incomplete candidates.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum HypotheticalCandidate {
	/// A complete candidate.
	Complete {
		/// The hash of the candidate.
		candidate_hash: CandidateHash,
		/// The receipt of the candidate.
		receipt: Arc<CommittedCandidateReceipt>,
		/// The persisted validation data of the candidate.
		persisted_validation_data: PersistedValidationData,
	},
	/// An incomplete candidate.
	Incomplete {
		/// The claimed hash of the candidate.
		candidate_hash: CandidateHash,
		/// The claimed para-ID of the candidate.
		candidate_para: ParaId,
		/// The claimed head-data hash of the candidate.
		parent_head_data_hash: Hash,
		/// The claimed scheduling parent of the candidate.
		candidate_scheduling_parent: Hash,
	},
}

impl HypotheticalCandidate {
	/// Get the `CandidateHash` of the hypothetical candidate.
	pub fn candidate_hash(&self) -> CandidateHash {
		match *self {
			HypotheticalCandidate::Complete { candidate_hash, .. } => candidate_hash,
			HypotheticalCandidate::Incomplete { candidate_hash, .. } => candidate_hash,
		}
	}

	/// Get the `ParaId` of the hypothetical candidate.
	pub fn candidate_para(&self) -> ParaId {
		match *self {
			HypotheticalCandidate::Complete { ref receipt, .. } => receipt.descriptor.para_id(),
			HypotheticalCandidate::Incomplete { candidate_para, .. } => candidate_para,
		}
	}

	/// Get parent head data hash of the hypothetical candidate.
	pub fn parent_head_data_hash(&self) -> Hash {
		match *self {
			HypotheticalCandidate::Complete { ref persisted_validation_data, .. } => {
				persisted_validation_data.parent_head.hash()
			},
			HypotheticalCandidate::Incomplete { parent_head_data_hash, .. } => {
				parent_head_data_hash
			},
		}
	}

	/// Get candidate's scheduling parent.
	///
	/// For `Complete` candidates, this is the scheduling parent from the descriptor
	/// (which equals relay_parent for V1/V2 descriptors).
	/// For `Incomplete` candidates, this is the claimed scheduling parent.
	pub fn scheduling_parent(&self) -> Hash {
		match *self {
			HypotheticalCandidate::Complete { ref receipt, .. } => {
				receipt.descriptor.scheduling_parent()
			},
			HypotheticalCandidate::Incomplete { candidate_scheduling_parent, .. } => {
				candidate_scheduling_parent
			},
		}
	}

	/// Get the output head data hash, if the candidate is complete.
	pub fn output_head_data_hash(&self) -> Option<Hash> {
		match *self {
			HypotheticalCandidate::Complete { ref receipt, .. } => {
				Some(receipt.descriptor.para_head())
			},
			HypotheticalCandidate::Incomplete { .. } => None,
		}
	}

	/// Get the candidate commitments, if the candidate is complete.
	pub fn commitments(&self) -> Option<&CandidateCommitments> {
		match *self {
			HypotheticalCandidate::Complete { ref receipt, .. } => Some(&receipt.commitments),
			HypotheticalCandidate::Incomplete { .. } => None,
		}
	}

	/// Get the persisted validation data, if the candidate is complete.
	pub fn persisted_validation_data(&self) -> Option<&PersistedValidationData> {
		match *self {
			HypotheticalCandidate::Complete { ref persisted_validation_data, .. } => {
				Some(persisted_validation_data)
			},
			HypotheticalCandidate::Incomplete { .. } => None,
		}
	}

	/// Get the validation code hash, if the candidate is complete.
	pub fn validation_code_hash(&self) -> Option<ValidationCodeHash> {
		match *self {
			HypotheticalCandidate::Complete { ref receipt, .. } => {
				Some(receipt.descriptor.validation_code_hash())
			},
			HypotheticalCandidate::Incomplete { .. } => None,
		}
	}
}

/// Request specifying which candidates are either already included
/// or might become included in fragment chain under a given active leaf (or any active leaf if
/// `fragment_chain_relay_parent` is `None`).
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct HypotheticalMembershipRequest {
	/// Candidates, in arbitrary order, which should be checked for
	/// hypothetical/actual membership in fragment chains.
	pub candidates: Vec<HypotheticalCandidate>,
	/// Either a specific fragment chain to check, otherwise all.
	pub fragment_chain_relay_parent: Option<Hash>,
}

/// A request for the persisted validation data stored in the prospective
/// parachains subsystem.
#[derive(Debug)]
pub struct ProspectiveValidationDataRequest {
	/// The para-id of the candidate.
	pub para_id: ParaId,
	/// The relay-parent of the candidate.
	pub candidate_relay_parent: Hash,
	/// The session index of the candidate's relay parent
	pub session_index: SessionIndex,
	/// The parent head-data.
	pub parent_head_data: ParentHeadData,
}

/// The parent head-data hash with optional data itself.
#[derive(Debug, Clone)]
pub enum ParentHeadData {
	/// Parent head-data hash.
	OnlyHash(Hash),
	/// Parent head-data along with its hash.
	WithData {
		/// This will be provided for collations with elastic scaling enabled.
		head_data: HeadData,
		/// Parent head-data hash.
		hash: Hash,
	},
}

impl ParentHeadData {
	/// Return the hash of the parent head-data.
	pub fn hash(&self) -> Hash {
		match self {
			ParentHeadData::OnlyHash(hash) => *hash,
			ParentHeadData::WithData { hash, .. } => *hash,
		}
	}
}

/// Indicates the relay-parents whose fragment chain a candidate
/// is present in or can be added in (right now or in the future).
pub type HypotheticalMembership = Vec<Hash>;

/// Output heads held by prospective parachains, keyed by scheduling parent and para: the
/// output heads of every candidate in that scheduling parent's fragment chain plus the para's
/// latest included head.
pub type KnownOutputHeads = HashMap<Hash, HashMap<ParaId, HashSet<Hash>>>;

/// A collection of ancestor candidates of a parachain.
pub type Ancestors = HashSet<CandidateHash>;

/// Messages sent to the Prospective Parachains subsystem.
#[derive(Debug)]
pub enum ProspectiveParachainsMessage {
	/// Inform the Prospective Parachains Subsystem of a new seconded candidate.
	///
	/// The response sender returns false if the candidate was rejected by prospective parachains,
	/// true otherwise (if it was accepted or already present)
	IntroduceSecondedCandidate(IntroduceSecondedCandidateRequest, oneshot::Sender<bool>),
	/// Inform the Prospective Parachains Subsystem that a previously introduced candidate
	/// has been backed. This requires that the candidate was successfully introduced in
	/// the past.
	CandidateBacked(ParaId, CandidateHash),
	/// Get N backable candidate references with their scheduling parents for the given
	/// parachain, under the given relay chain leaf hash.
	///
	/// Timed out ancestors should not be included in the collection.
	/// N should represent the number of scheduled cores of this ParaId.
	/// A timed out ancestor frees the cores of all of its descendants, so if there's a hole in the
	/// supplied ancestor path, we'll get candidates that backfill those timed out slots first. It
	/// may also return less/no candidates, if there aren't enough backable candidates recorded.
	GetBackableCandidates {
		/// The relay chain leaf hash under which to query. Must be an active leaf.
		leaf: Hash,
		/// The parachain to get backable candidates for.
		para_id: ParaId,
		/// The maximum number of candidates to return.
		count: u32,
		/// Required ancestor path for the candidates.
		ancestors: Ancestors,
		/// Channel to send the result.
		sender: oneshot::Sender<Vec<BackableCandidateRef>>,
	},
	/// Get the hypothetical or actual membership of candidates with the given properties
	/// under the specified active leave's fragment chain.
	///
	/// For each candidate, we return a vector of leaves where the candidate is present or could be
	/// added. "Could be added" either means that the candidate can be added to the chain right now
	/// or could be added in the future (we may not have its ancestors yet).
	/// Note that even if we think it could be added in the future, we may find out that it was
	/// invalid, as time passes.
	/// If an active leaf is not in the vector, it means that there's no
	/// chance this candidate will become valid under that leaf in the future.
	///
	/// If `fragment_chain_relay_parent` in the request is `Some()`, the return vector can only
	/// contain this relay parent (or none).
	GetHypotheticalMembership(
		HypotheticalMembershipRequest,
		oneshot::Sender<Vec<(HypotheticalCandidate, HypotheticalMembership)>>,
	),
	/// Get the validation data of some prospective candidate. The candidate doesn't need
	/// to be part of any fragment chain, but this only succeeds if the parent head-data and
	/// relay-parent are part of the `CandidateStorage` (meaning that it's a candidate which is
	/// part of some fragment chain or which prospective-parachains predicted will become part of
	/// some fragment chain).
	GetProspectiveValidationData(
		ProspectiveValidationDataRequest,
		oneshot::Sender<Option<PersistedValidationData>>,
	),
	/// Known output heads per scheduling parent in view. The keys are every active leaf plus the
	/// retained ancestors within scheduling lookahead. For each key and requested para: the output
	/// heads of all candidates held in that scheduling parent's fragment chain plus the para's
	/// latest included head. A scheduling parent with nothing to report for any requested para
	/// may be absent.
	GetKnownOutputHeads(Vec<ParaId>, oneshot::Sender<KnownOutputHeads>),
}
