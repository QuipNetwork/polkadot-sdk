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

//! Polkadot types shared between the runtime and the Node-side code.

#![warn(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

// `v13` is currently the latest stable version of the runtime API, the number below
// should be incremented when a new version is released and is independent of the
// released version of the Polkadot runtime.
pub mod v9;

// The 'staging' version is special - it contains primitives which are
// still in development. Once they are considered stable, they will be
// moved to a new versioned module.
pub mod vstaging;

// `runtime_api` contains the actual API implementation. It contains stable and
// unstable functions.
pub mod runtime_api;

extern crate alloc;

// Current primitives not requiring versioning are exported here.
// Primitives requiring versioning must not be exported and must be referred by an exact version.
pub use v9::{
	async_backing, byzantine_threshold, check_candidate_backing, effective_minimum_backing_votes,
	executor_params, metric_definitions, node_features, skip_ump_signals, slashing,
	supermajority_threshold, transpose_claim_queue, well_known_keys, AbridgedHostConfiguration,
	AbridgedHrmpChannel, AccountId, AccountIndex, AccountPublic, ApprovalVote,
	ApprovalVoteMultipleCandidates, ApprovalVotingParams, ApprovedPeerId, AssignmentId,
	AsyncBackingParams, AuthorityDiscoveryId, AvailabilityBitfield, BackedCandidate, Balance,
	BlakeTwo256, Block, BlockId, BlockNumber, CandidateCommitments, CandidateDescriptorV2,
	CandidateDescriptorVersion, CandidateDescriptorVersionCheckError, CandidateEvent,
	CandidateHash, CandidateIndex, CandidateReceiptV2, CheckedDisputeStatementSet,
	CheckedMultiDisputeStatementSet, ChunkIndex, ClaimQueueOffset,
	CoalescedApprovalCandidateHashes, CollatorId, CollatorSignature,
	CommittedCandidateReceiptError, CommittedCandidateReceiptV2, CompactStatement, ConsensusLog,
	CoreIndex, CoreSelector, CoreState, DisputeOffenceKind, DisputeState, DisputeStatement,
	DisputeStatementSet, DownwardMessage, EncodeAs, ExecutorHostFunction, ExecutorParam,
	ExecutorParamError, ExecutorParams, ExecutorParamsHash, ExecutorParamsPrepHash,
	ExplicitDisputeStatement, GroupIndex, GroupRotationInfo, Hash, HashT, HeadData, Header,
	HorizontalMessages, HrmpChannelId, Id, InboundDownwardMessage, InboundHrmpMessage, IndexedVec,
	InherentData, InvalidDisputeStatementKind, Moment, MultiDisputeStatementSet, NodeFeatures,
	Nonce, OccupiedCore, OccupiedCoreAssumption, OutboundHrmpMessage, ParathreadClaim,
	ParathreadEntry, PersistedValidationData, PvfCheckStatement, PvfExecKind, PvfPrepKind,
	RuntimeMetricLabel, RuntimeMetricLabelValue, RuntimeMetricLabelValues, RuntimeMetricLabels,
	RuntimeMetricOp, RuntimeMetricUpdate, ScheduledCore, SchedulerParams, ScrapedOnChainVotes,
	SessionIndex, SessionInfo, Signature, Signed, SignedAvailabilityBitfield,
	SignedAvailabilityBitfields, SignedStatement, SigningContext, Slot, TransposedClaimQueue,
	UMPSignal, UncheckedSigned, UncheckedSignedAvailabilityBitfield,
	UncheckedSignedAvailabilityBitfields, UncheckedSignedStatement, UpgradeGoAhead,
	UpgradeRestriction, UpwardMessage, ValidDisputeStatementKind, ValidationCode,
	ValidationCodeHash, ValidatorId, ValidatorIndex, ValidatorSignature, ValidityAttestation,
	ValidityError, ASSIGNMENT_KEY_TYPE_ID, DEFAULT_CLAIM_QUEUE_OFFSET,
	DEFAULT_SCHEDULING_LOOKAHEAD, LEGACY_MIN_BACKING_VOTES, LOWEST_PUBLIC_ID,
	MAX_COALESCE_APPROVALS, MAX_CODE_SIZE, MAX_HEAD_DATA_SIZE, MAX_POV_SIZE, MIN_CODE_SIZE,
	ON_DEMAND_DEFAULT_QUEUE_MAX_SIZE, ON_DEMAND_MAX_QUEUE_MAX_SIZE, PARACHAINS_INHERENT_IDENTIFIER,
	PARACHAIN_KEY_TYPE_ID, RELAY_CHAIN_SLOT_DURATION_MILLIS, UMP_SEPARATOR,
};

#[cfg(feature = "test")]
pub use v9::{AppVerify, MutateDescriptorV2};

#[cfg(feature = "std")]
pub use v9::{AssignmentPair, CollatorPair, ValidatorPair};
