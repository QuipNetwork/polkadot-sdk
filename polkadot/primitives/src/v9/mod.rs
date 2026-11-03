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

//! `V9` Primitives.

use alloc::{
	collections::{BTreeMap, BTreeSet, VecDeque},
	vec,
	vec::{IntoIter, Vec},
};

use bitvec::{field::BitField, prelude::*, slice::BitSlice};

use codec::{Decode, DecodeWithMemTracking, Encode};
use scale_info::TypeInfo;

use core::{
	marker::PhantomData,
	slice::{Iter, IterMut},
};

#[cfg(feature = "test")]
use sp_application_crypto::ByteArray;
use sp_application_crypto::KeyTypeId;
use sp_arithmetic::{
	traits::{BaseArithmetic, Saturating},
	Perbill,
};

use bounded_collections::BoundedVec;
use serde::{Deserialize, Serialize};
use sp_core::ConstU32;
use sp_inherents::InherentIdentifier;

// ==========
// PUBLIC RE-EXPORTS
// ==========

pub use sp_authority_discovery::AuthorityId as AuthorityDiscoveryId;
pub use sp_consensus_slots::Slot;
pub use sp_runtime::traits::{AppVerify, BlakeTwo256, Hash as HashT, Header as HeaderT};
pub use sp_staking::SessionIndex;

// Export some core primitives.
pub use polkadot_core_primitives::v2::{
	AccountId, AccountIndex, AccountPublic, Balance, Block, BlockId, BlockNumber, CandidateHash,
	ChainId, DownwardMessage, Hash, Header, InboundDownwardMessage, InboundHrmpMessage, Moment,
	Nonce, OutboundHrmpMessage, Remark, Signature, UncheckedExtrinsic,
};

// Export some polkadot-parachain primitives
pub use polkadot_parachain_primitives::primitives::{
	HeadData, HorizontalMessages, HrmpChannelId, Id, Id as ParaId, UpwardMessage, UpwardMessages,
	ValidationCode, ValidationCodeHash, LOWEST_PUBLIC_ID,
};

/// Signed data.
mod signed;
pub use signed::{EncodeAs, Signed, UncheckedSigned};

pub mod async_backing;
pub mod executor_params;
pub mod slashing;

pub use async_backing::AsyncBackingParams;
pub use executor_params::{
	ExecutorHostFunction, ExecutorParam, ExecutorParamError, ExecutorParams, ExecutorParamsHash,
	ExecutorParamsPrepHash,
};

mod metrics;
pub use metrics::{
	metric_definitions, RuntimeMetricLabel, RuntimeMetricLabelValue, RuntimeMetricLabelValues,
	RuntimeMetricLabels, RuntimeMetricOp, RuntimeMetricUpdate,
};

/// The key type ID for a collator key.
pub const COLLATOR_KEY_TYPE_ID: KeyTypeId = KeyTypeId(*b"coll");
const LOG_TARGET: &str = "runtime::primitives";

mod collator_app {
	use sp_application_crypto::{app_crypto, sr25519};
	app_crypto!(sr25519, super::COLLATOR_KEY_TYPE_ID);
}

/// Identity that collators use.
pub type CollatorId = collator_app::Public;

/// A Parachain collator keypair.
#[cfg(feature = "std")]
pub type CollatorPair = collator_app::Pair;

/// Signature on candidate's block data by a collator.
pub type CollatorSignature = collator_app::Signature;

/// The key type ID for a parachain validator key.
pub const PARACHAIN_KEY_TYPE_ID: KeyTypeId = KeyTypeId(*b"para");

mod validator_app {
	use sp_application_crypto::{app_crypto, sr25519};
	app_crypto!(sr25519, super::PARACHAIN_KEY_TYPE_ID);
}

/// Identity that parachain validators use when signing validation messages.
///
/// For now we assert that parachain validator set is exactly equivalent to the authority set, and
/// so we define it to be the same type as `SessionKey`. In the future it may have different crypto.
pub type ValidatorId = validator_app::Public;

/// Trait required for type specific indices e.g. `ValidatorIndex` and `GroupIndex`
pub trait TypeIndex {
	/// Returns the index associated to this value.
	fn type_index(&self) -> usize;
}

/// Index of the validator is used as a lightweight replacement of the `ValidatorId` when
/// appropriate.
#[derive(
	Eq,
	Ord,
	PartialEq,
	PartialOrd,
	Copy,
	Clone,
	Encode,
	Decode,
	DecodeWithMemTracking,
	TypeInfo,
	Debug,
)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize, Hash))]
pub struct ValidatorIndex(pub u32);

/// Index of an availability chunk.
///
/// The underlying type is identical to `ValidatorIndex`, because
/// the number of chunks will always be equal to the number of validators.
/// However, the chunk index held by a validator may not always be equal to its `ValidatorIndex`, so
/// we use a separate type to make code easier to read.
#[derive(Eq, Ord, PartialEq, PartialOrd, Copy, Clone, Encode, Decode, TypeInfo, Debug)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize, Hash))]
pub struct ChunkIndex(pub u32);

impl From<ChunkIndex> for ValidatorIndex {
	fn from(c_index: ChunkIndex) -> Self {
		ValidatorIndex(c_index.0)
	}
}

impl From<ValidatorIndex> for ChunkIndex {
	fn from(v_index: ValidatorIndex) -> Self {
		ChunkIndex(v_index.0)
	}
}

impl From<u32> for ChunkIndex {
	fn from(n: u32) -> Self {
		ChunkIndex(n)
	}
}

// We should really get https://github.com/paritytech/polkadot/issues/2403 going ..
impl From<u32> for ValidatorIndex {
	fn from(n: u32) -> Self {
		ValidatorIndex(n)
	}
}

impl TypeIndex for ValidatorIndex {
	fn type_index(&self) -> usize {
		self.0 as usize
	}
}

sp_application_crypto::with_pair! {
	/// A Parachain validator keypair.
	pub type ValidatorPair = validator_app::Pair;
}

/// Signature with which parachain validators sign blocks.
///
/// For now we assert that parachain validator set is exactly equivalent to the authority set, and
/// so we define it to be the same type as `SessionKey`. In the future it may have different crypto.
pub type ValidatorSignature = validator_app::Signature;

/// A declarations of storage keys where an external observer can find some interesting data.
pub mod well_known_keys {
	use super::{HrmpChannelId, Id, WellKnownKey};
	use alloc::vec::Vec;
	use codec::Encode as _;
	use hex_literal::hex;
	use sp_io::hashing::twox_64;

	// A note on generating these magic values below:
	//
	// The `StorageValue`, such as `ACTIVE_CONFIG` was obtained by calling:
	//
	//     ActiveConfig::<T>::hashed_key()
	//
	// The `StorageMap` values require `prefix`, and for example for `hrmp_egress_channel_index`,
	// it could be obtained like:
	//
	//     HrmpEgressChannelsIndex::<T>::prefix_hash();
	//

	/// The current epoch index.
	///
	/// The storage item should be access as a `u64` encoded value.
	pub const EPOCH_INDEX: &[u8] =
		&hex!["1cb6f36e027abb2091cfb5110ab5087f38316cbf8fa0da822a20ac1c55bf1be3"];

	/// The current relay chain block randomness
	///
	/// The storage item should be accessed as a `schnorrkel::Randomness` encoded value.
	pub const CURRENT_BLOCK_RANDOMNESS: &[u8] =
		&hex!["1cb6f36e027abb2091cfb5110ab5087fd077dfdb8adb10f78f10a5df8742c545"];

	/// The randomness for one epoch ago
	///
	/// The storage item should be accessed as a `schnorrkel::Randomness` encoded value.
	pub const ONE_EPOCH_AGO_RANDOMNESS: &[u8] =
		&hex!["1cb6f36e027abb2091cfb5110ab5087f7ce678799d3eff024253b90e84927cc6"];

	/// The randomness for two epochs ago
	///
	/// The storage item should be accessed as a `schnorrkel::Randomness` encoded value.
	pub const TWO_EPOCHS_AGO_RANDOMNESS: &[u8] =
		&hex!["1cb6f36e027abb2091cfb5110ab5087f7a414cb008e0e61e46722aa60abdd672"];

	/// The current slot number.
	///
	/// The storage entry should be accessed as a `Slot` encoded value.
	pub const CURRENT_SLOT: &[u8] =
		&hex!["1cb6f36e027abb2091cfb5110ab5087f06155b3cd9a8c9e5e9a23fd5dc13a5ed"];

	/// The currently active host configuration.
	///
	/// The storage entry should be accessed as an `AbridgedHostConfiguration` encoded value.
	pub const ACTIVE_CONFIG: &[u8] =
		&hex!["06de3d8a54d27e44a9d5ce189618f22db4b49d95320d9021994c850f25b8e385"];

	/// The authorities for the current epoch.
	///
	/// The storage entry should be accessed as an `Vec<(AuthorityId, BabeAuthorityWeight)>` encoded
	/// value.
	pub const AUTHORITIES: &[u8] =
		&hex!["1cb6f36e027abb2091cfb5110ab5087f5e0621c4869aa60c02be9adcc98a0d1d"];

	/// The authorities for the next epoch.
	///
	/// The storage entry should be accessed as an `Vec<(AuthorityId, BabeAuthorityWeight)>` encoded
	/// value.
	pub const NEXT_AUTHORITIES: &[u8] =
		&hex!["1cb6f36e027abb2091cfb5110ab5087faacf00b9b41fda7a9268821c2a2b3e4c"];

	/// Hash of the committed head data for a given registered para.
	///
	/// The storage entry stores wrapped `HeadData(Vec<u8>)`.
	pub fn para_head(para_id: Id) -> Vec<u8> {
		let prefix = hex!["cd710b30bd2eab0352ddcc26417aa1941b3c252fcb29d88eff4f3de5de4476c3"];

		para_id.using_encoded(|para_id: &[u8]| {
			prefix
				.as_ref()
				.iter()
				.chain(twox_64(para_id).iter())
				.chain(para_id.iter())
				.cloned()
				.collect()
		})
	}

	/// The upward message dispatch queue for the given para id.
	///
	/// The storage entry stores a tuple of two values:
	///
	/// - `count: u32`, the number of messages currently in the queue for given para,
	/// - `total_size: u32`, the total size of all messages in the queue.
	#[deprecated = "Use `relay_dispatch_queue_remaining_capacity` instead"]
	pub fn relay_dispatch_queue_size(para_id: Id) -> Vec<u8> {
		let prefix = hex!["f5207f03cfdce586301014700e2c2593fad157e461d71fd4c1f936839a5f1f3e"];

		para_id.using_encoded(|para_id: &[u8]| {
			prefix
				.as_ref()
				.iter()
				.chain(twox_64(para_id).iter())
				.chain(para_id.iter())
				.cloned()
				.collect()
		})
	}

	/// Type safe version of `relay_dispatch_queue_size`.
	#[deprecated = "Use `relay_dispatch_queue_remaining_capacity` instead"]
	pub fn relay_dispatch_queue_size_typed(para: Id) -> WellKnownKey<(u32, u32)> {
		#[allow(deprecated)]
		relay_dispatch_queue_size(para).into()
	}

	/// The upward message dispatch queue remaining capacity for the given para id.
	///
	/// The storage entry stores a tuple of two values:
	///
	/// - `count: u32`, the number of additional messages which may be enqueued for the given para,
	/// - `total_size: u32`, the total size of additional messages which may be enqueued for the
	/// given para.
	pub fn relay_dispatch_queue_remaining_capacity(para_id: Id) -> WellKnownKey<(u32, u32)> {
		(b":relay_dispatch_queue_remaining_capacity", para_id).encode().into()
	}

	/// The HRMP channel for the given identifier.
	///
	/// The storage entry should be accessed as an `AbridgedHrmpChannel` encoded value.
	pub fn hrmp_channels(channel: HrmpChannelId) -> Vec<u8> {
		let prefix = hex!["6a0da05ca59913bc38a8630590f2627cb6604cff828a6e3f579ca6c59ace013d"];

		channel.using_encoded(|channel: &[u8]| {
			prefix
				.as_ref()
				.iter()
				.chain(twox_64(channel).iter())
				.chain(channel.iter())
				.cloned()
				.collect()
		})
	}

	/// The list of inbound channels for the given para.
	///
	/// The storage entry stores a `Vec<ParaId>`
	pub fn hrmp_ingress_channel_index(para_id: Id) -> Vec<u8> {
		let prefix = hex!["6a0da05ca59913bc38a8630590f2627c1d3719f5b0b12c7105c073c507445948"];

		para_id.using_encoded(|para_id: &[u8]| {
			prefix
				.as_ref()
				.iter()
				.chain(twox_64(para_id).iter())
				.chain(para_id.iter())
				.cloned()
				.collect()
		})
	}

	/// The list of outbound channels for the given para.
	///
	/// The storage entry stores a `Vec<ParaId>`
	pub fn hrmp_egress_channel_index(para_id: Id) -> Vec<u8> {
		let prefix = hex!["6a0da05ca59913bc38a8630590f2627cf12b746dcf32e843354583c9702cc020"];

		para_id.using_encoded(|para_id: &[u8]| {
			prefix
				.as_ref()
				.iter()
				.chain(twox_64(para_id).iter())
				.chain(para_id.iter())
				.cloned()
				.collect()
		})
	}

	/// The MQC head for the downward message queue of the given para. See more in the `Dmp` module.
	///
	/// The storage entry stores a `Hash`. This is polkadot hash which is at the moment
	/// `blake2b-256`.
	pub fn dmq_mqc_head(para_id: Id) -> Vec<u8> {
		let prefix = hex!["63f78c98723ddc9073523ef3beefda0c4d7fefc408aac59dbfe80a72ac8e3ce5"];

		para_id.using_encoded(|para_id: &[u8]| {
			prefix
				.as_ref()
				.iter()
				.chain(twox_64(para_id).iter())
				.chain(para_id.iter())
				.cloned()
				.collect()
		})
	}

	/// The signal that indicates whether the parachain should go-ahead with the proposed validation
	/// code upgrade.
	///
	/// The storage entry stores a value of `UpgradeGoAhead` type.
	pub fn upgrade_go_ahead_signal(para_id: Id) -> Vec<u8> {
		let prefix = hex!["cd710b30bd2eab0352ddcc26417aa1949e94c040f5e73d9b7addd6cb603d15d3"];

		para_id.using_encoded(|para_id: &[u8]| {
			prefix
				.as_ref()
				.iter()
				.chain(twox_64(para_id).iter())
				.chain(para_id.iter())
				.cloned()
				.collect()
		})
	}

	/// The signal that indicates whether the parachain is disallowed to signal an upgrade at this
	/// relay-parent.
	///
	/// The storage entry stores a value of `UpgradeRestriction` type.
	pub fn upgrade_restriction_signal(para_id: Id) -> Vec<u8> {
		let prefix = hex!["cd710b30bd2eab0352ddcc26417aa194f27bbb460270642b5bcaf032ea04d56a"];

		para_id.using_encoded(|para_id: &[u8]| {
			prefix
				.as_ref()
				.iter()
				.chain(twox_64(para_id).iter())
				.chain(para_id.iter())
				.cloned()
				.collect()
		})
	}
}

/// Relay chain slot duration in milliseconds, which is the same
/// value across all networks (e.g. Polkadot, Kusama, Westend, Rococo).
pub const RELAY_CHAIN_SLOT_DURATION_MILLIS: u64 = 6000;

/// Unique identifier for the Parachains Inherent
pub const PARACHAINS_INHERENT_IDENTIFIER: InherentIdentifier = *b"parachn0";

/// The key type ID for parachain assignment key.
pub const ASSIGNMENT_KEY_TYPE_ID: KeyTypeId = KeyTypeId(*b"asgn");

/// Compressed or not the wasm blob can never be less than 9 bytes.
pub const MIN_CODE_SIZE: u32 = 9;

/// Maximum compressed code size we support right now.
/// At the moment we have runtime upgrade on chain, which restricts scalability severely. If we want
/// to have bigger values, we should fix that first.
///
/// Used for:
/// * initial genesis for the Parachains configuration
/// * checking updates to this stored runtime configuration do not exceed this limit
// NOTE: This value is used in the runtime so be careful when changing it.
pub const MAX_CODE_SIZE: u32 = 5 * 1024 * 1024;

/// Maximum head data size we support right now.
///
/// Used for:
/// * initial genesis for the Parachains configuration
/// * checking updates to this stored runtime configuration do not exceed this limit
// NOTE: This value is used in the runtime so be careful when changing it.
pub const MAX_HEAD_DATA_SIZE: u32 = 1 * 1024 * 1024;

/// Maximum PoV size we support right now.
///
/// Used for:
/// * initial genesis for the Parachains configuration
/// * checking updates to this stored runtime configuration do not exceed this limit
/// * when detecting a PoV decompression bomb in the client
// NOTE: This value is used in the runtime so be careful when changing it.
pub const MAX_POV_SIZE: u32 = 10 * 1024 * 1024;

/// Default queue size we use for the on-demand order book.
///
/// Can be adjusted in configuration.
pub const ON_DEMAND_DEFAULT_QUEUE_MAX_SIZE: u32 = 10_000;

/// Maximum for maximum queue size.
///
/// We use this value for benchmarking.
pub const ON_DEMAND_MAX_QUEUE_MAX_SIZE: u32 = 10_000;

/// Backing votes threshold used from the host prior to runtime API version 6 and from the runtime
/// prior to v9 configuration migration.
pub const LEGACY_MIN_BACKING_VOTES: u32 = 2;

/// Default value for `SchedulerParams.lookahead`
pub const DEFAULT_SCHEDULING_LOOKAHEAD: u32 = 3;

// The public key of a keypair used by a validator for determining assignments
/// to approve included parachain candidates.
mod assignment_app {
	use sp_application_crypto::{app_crypto, sr25519};
	app_crypto!(sr25519, super::ASSIGNMENT_KEY_TYPE_ID);
}

/// The public key of a keypair used by a validator for determining assignments
/// to approve included parachain candidates.
pub type AssignmentId = assignment_app::Public;

sp_application_crypto::with_pair! {
	/// The full keypair used by a validator for determining assignments to approve included
	/// parachain candidates.
	pub type AssignmentPair = assignment_app::Pair;
}

/// The index of the candidate in the list of candidates fully included as-of the block.
pub type CandidateIndex = u32;

/// The validation data provides information about how to create the inputs for validation of a
/// candidate. This information is derived from the chain state and will vary from para to para,
/// although some fields may be the same for every para.
///
/// Since this data is used to form inputs to the validation function, it needs to be persisted by
/// the availability system to avoid dependence on availability of the relay-chain state.
///
/// Furthermore, the validation data acts as a way to authorize the additional data the collator
/// needs to pass to the validation function. For example, the validation function can check whether
/// the incoming messages (e.g. downward messages) were actually sent by using the data provided in
/// the validation data using so called MQC heads.
///
/// Since the commitments of the validation function are checked by the relay-chain, secondary
/// checkers can rely on the invariant that the relay-chain only includes para-blocks for which
/// these checks have already been done. As such, there is no need for the validation data used to
/// inform validators and collators about the checks the relay-chain will perform to be persisted by
/// the availability system.
///
/// The `PersistedValidationData` should be relatively lightweight primarily because it is
/// constructed during inclusion for each candidate and therefore lies on the critical path of
/// inclusion.
#[derive(PartialEq, Eq, Clone, Encode, Decode, DecodeWithMemTracking, TypeInfo, Debug)]
#[cfg_attr(feature = "std", derive(Default))]
pub struct PersistedValidationData<H = Hash, N = BlockNumber> {
	/// The parent head-data.
	pub parent_head: HeadData,
	/// The relay-chain block number this is in the context of.
	pub relay_parent_number: N,
	/// The relay-chain block storage root this is in the context of.
	pub relay_parent_storage_root: H,
	/// The maximum legal size of a POV block, in bytes.
	pub max_pov_size: u32,
}

impl<H: Encode, N: Encode> PersistedValidationData<H, N> {
	/// Compute the blake2-256 hash of the persisted validation data.
	pub fn hash(&self) -> Hash {
		BlakeTwo256::hash_of(self)
	}
}

/// Commitments made in a `CandidateReceipt`. Many of these are outputs of validation.
#[derive(PartialEq, Eq, Clone, Encode, Decode, DecodeWithMemTracking, TypeInfo, Debug)]
#[cfg_attr(feature = "std", derive(Default, Hash))]
pub struct CandidateCommitments<N = BlockNumber> {
	/// Messages destined to be interpreted by the Relay chain itself.
	pub upward_messages: UpwardMessages,
	/// Horizontal messages sent by the parachain.
	pub horizontal_messages: HorizontalMessages,
	/// New validation code.
	pub new_validation_code: Option<ValidationCode>,
	/// The head-data produced as a result of execution.
	pub head_data: HeadData,
	/// The number of messages processed from the DMQ.
	pub processed_downward_messages: u32,
	/// The mark which specifies the block number up to which all inbound HRMP messages are
	/// processed.
	pub hrmp_watermark: N,
}

impl CandidateCommitments {
	/// Compute the blake2-256 hash of the commitments.
	pub fn hash(&self) -> Hash {
		BlakeTwo256::hash_of(self)
	}
}

/// A bitfield concerning availability of backed candidates.
///
/// Every bit refers to an availability core index.
#[derive(PartialEq, Eq, Clone, Encode, Decode, DecodeWithMemTracking, Debug, TypeInfo)]
pub struct AvailabilityBitfield(pub BitVec<u8, bitvec::order::Lsb0>);

impl From<BitVec<u8, bitvec::order::Lsb0>> for AvailabilityBitfield {
	fn from(inner: BitVec<u8, bitvec::order::Lsb0>) -> Self {
		AvailabilityBitfield(inner)
	}
}

/// A signed compact statement, suitable to be sent to the chain.
pub type SignedStatement = Signed<CompactStatement>;
/// A signed compact statement, with signature not yet checked.
pub type UncheckedSignedStatement = UncheckedSigned<CompactStatement>;

/// A bitfield signed by a particular validator about the availability of pending candidates.
pub type SignedAvailabilityBitfield = Signed<AvailabilityBitfield>;
/// A signed bitfield with signature not yet checked.
pub type UncheckedSignedAvailabilityBitfield = UncheckedSigned<AvailabilityBitfield>;

/// A set of signed availability bitfields. Should be sorted by validator index, ascending.
pub type SignedAvailabilityBitfields = Vec<SignedAvailabilityBitfield>;
/// A set of unchecked signed availability bitfields. Should be sorted by validator index,
/// ascending.
pub type UncheckedSignedAvailabilityBitfields = Vec<UncheckedSignedAvailabilityBitfield>;

/// Verify the backing of the given candidate.
///
/// Provide a lookup from the index of a validator within the group assigned to this para,
/// as opposed to the index of the validator within the overall validator set, as well as
/// the number of validators in the group.
///
/// Also provide the signing context.
///
/// Returns either an error, indicating that one of the signatures was invalid or that the index
/// was out-of-bounds, or the number of signatures checked.
pub fn check_candidate_backing<H: AsRef<[u8]> + Clone + Encode + core::fmt::Debug>(
	candidate_hash: CandidateHash,
	validity_votes: &[ValidityAttestation],
	validator_indices: &BitSlice<u8, bitvec::order::Lsb0>,
	signing_context: &SigningContext<H>,
	group_len: usize,
	validator_lookup: impl Fn(usize) -> Option<ValidatorId>,
) -> Result<usize, ()> {
	if validator_indices.len() != group_len {
		log::debug!(
			target: LOG_TARGET,
			"Check candidate backing: indices mismatch: group_len = {} , indices_len = {}",
			group_len,
			validator_indices.len(),
		);
		return Err(());
	}

	if validity_votes.len() > group_len {
		log::debug!(
			target: LOG_TARGET,
			"Check candidate backing: Too many votes, expected: {}, found: {}",
			group_len,
			validity_votes.len(),
		);
		return Err(());
	}

	let mut signed = 0;
	for ((val_in_group_idx, _), attestation) in validator_indices
		.iter()
		.enumerate()
		.filter(|(_, signed)| **signed)
		.zip(validity_votes.iter())
	{
		let validator_id = validator_lookup(val_in_group_idx).ok_or(())?;
		let payload = attestation.signed_payload(candidate_hash, signing_context);
		let sig = attestation.signature();

		if sig.verify(&payload[..], &validator_id) {
			signed += 1;
		} else {
			log::debug!(
				target: LOG_TARGET,
				"Check candidate backing: Invalid signature. validator_id = {:?}, validator_index = {} ",
				validator_id,
				val_in_group_idx,
			);
			return Err(());
		}
	}

	if signed != validity_votes.len() {
		log::error!(
			target: LOG_TARGET,
			"Check candidate backing: Too many signatures, expected = {}, found = {}",
			validity_votes.len(),
			signed,
		);
		return Err(());
	}

	Ok(signed)
}

/// The unique (during session) index of a core.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Default,
	PartialOrd,
	Ord,
	Eq,
	PartialEq,
	Clone,
	Copy,
	TypeInfo,
	Debug,
)]
#[cfg_attr(feature = "std", derive(Hash))]
pub struct CoreIndex(pub u32);

impl From<u32> for CoreIndex {
	fn from(i: u32) -> CoreIndex {
		CoreIndex(i)
	}
}

impl TypeIndex for CoreIndex {
	fn type_index(&self) -> usize {
		self.0 as usize
	}
}

/// The unique (during session) index of a validator group.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Default,
	Clone,
	Copy,
	Debug,
	PartialEq,
	Eq,
	TypeInfo,
	PartialOrd,
	Ord,
)]
#[cfg_attr(feature = "std", derive(Hash))]
pub struct GroupIndex(pub u32);

impl From<u32> for GroupIndex {
	fn from(i: u32) -> GroupIndex {
		GroupIndex(i)
	}
}

impl TypeIndex for GroupIndex {
	fn type_index(&self) -> usize {
		self.0 as usize
	}
}

/// A claim on authoring the next block for a given parathread (on-demand parachain).
#[derive(Clone, Encode, Decode, TypeInfo, PartialEq, Debug)]
pub struct ParathreadClaim(pub Id, pub Option<CollatorId>);

/// An entry tracking a claim to ensure it does not pass the maximum number of retries.
#[derive(Clone, Encode, Decode, TypeInfo, PartialEq, Debug)]
pub struct ParathreadEntry {
	/// The claim.
	pub claim: ParathreadClaim,
	/// Number of retries
	pub retries: u32,
}

/// A helper data-type for tracking validator-group rotations.
#[derive(Clone, Encode, Decode, TypeInfo, Debug)]
#[cfg_attr(feature = "std", derive(PartialEq))]
pub struct GroupRotationInfo<N = BlockNumber> {
	/// The block number where the session started.
	pub session_start_block: N,
	/// How often groups rotate. 0 means never.
	pub group_rotation_frequency: N,
	/// The current block number.
	pub now: N,
}

impl GroupRotationInfo {
	/// Returns the index of the group needed to validate the core at the given index, assuming
	/// the given number of cores.
	///
	/// `core_index` should be less than `cores`, which is capped at `u32::max()`.
	pub fn group_for_core(&self, core_index: CoreIndex, cores: usize) -> GroupIndex {
		if self.group_rotation_frequency == 0 {
			return GroupIndex(core_index.0);
		}
		if cores == 0 {
			return GroupIndex(0);
		}

		let cores = core::cmp::min(cores, u32::MAX as usize);
		let blocks_since_start = self.now.saturating_sub(self.session_start_block);
		let rotations = blocks_since_start / self.group_rotation_frequency;

		// g = c + r mod cores

		let idx = (core_index.0 as usize + rotations as usize) % cores;
		GroupIndex(idx as u32)
	}

	/// Returns the index of the group assigned to the given core. This does no checking or
	/// whether the group index is in-bounds.
	///
	/// `core_index` should be less than `cores`, which is capped at `u32::max()`.
	pub fn core_for_group(&self, group_index: GroupIndex, cores: usize) -> CoreIndex {
		if self.group_rotation_frequency == 0 {
			return CoreIndex(group_index.0);
		}
		if cores == 0 {
			return CoreIndex(0);
		}

		let cores = core::cmp::min(cores, u32::MAX as usize);
		let blocks_since_start = self.now.saturating_sub(self.session_start_block);
		let rotations = blocks_since_start / self.group_rotation_frequency;
		let rotations = rotations % cores as u32;

		// g = c + r mod cores
		// c = g - r mod cores
		// x = x + cores mod cores
		// c = (g + cores) - r mod cores

		let idx = (group_index.0 as usize + cores - rotations as usize) % cores;
		CoreIndex(idx as u32)
	}

	/// Create a new `GroupRotationInfo` with one further rotation applied.
	pub fn bump_rotation(&self) -> Self {
		GroupRotationInfo {
			session_start_block: self.session_start_block,
			group_rotation_frequency: self.group_rotation_frequency,
			now: self.next_rotation_at(),
		}
	}
}

impl<N: Saturating + BaseArithmetic + Copy> GroupRotationInfo<N> {
	/// Returns the block number of the next rotation after the current block. If the current block
	/// is 10 and the rotation frequency is 5, this should return 15.
	pub fn next_rotation_at(&self) -> N {
		let cycle_once = self.now + self.group_rotation_frequency;
		cycle_once -
			(cycle_once.saturating_sub(self.session_start_block) % self.group_rotation_frequency)
	}

	/// Returns the block number of the last rotation before or including the current block. If the
	/// current block is 10 and the rotation frequency is 5, this should return 10.
	pub fn last_rotation_at(&self) -> N {
		self.now -
			(self.now.saturating_sub(self.session_start_block) % self.group_rotation_frequency)
	}
}

/// Information about a core which is currently occupied.
#[derive(Clone, Encode, Decode, TypeInfo, Debug)]
#[cfg_attr(feature = "std", derive(PartialEq))]
pub struct ScheduledCore {
	/// The ID of a para scheduled.
	pub para_id: Id,
	/// DEPRECATED: see: <https://github.com/paritytech/polkadot/issues/7575>
	///
	/// Will be removed in a future version.
	pub collator: Option<CollatorId>,
}

/// An assumption being made about the state of an occupied core.
#[derive(Clone, Copy, Encode, Decode, TypeInfo, Debug)]
#[cfg_attr(feature = "std", derive(PartialEq, Eq, Hash))]
pub enum OccupiedCoreAssumption {
	/// The candidate occupying the core was made available and included to free the core.
	#[codec(index = 0)]
	Included,
	/// The candidate occupying the core timed out and freed the core without advancing the para.
	#[codec(index = 1)]
	TimedOut,
	/// The core was not occupied to begin with.
	#[codec(index = 2)]
	Free,
}

/// A vote of approval on a candidate.
#[derive(Clone, Debug)]
pub struct ApprovalVote(pub CandidateHash);

impl ApprovalVote {
	/// Yields the signing payload for this approval vote.
	pub fn signing_payload(&self, session_index: SessionIndex) -> Vec<u8> {
		const MAGIC: [u8; 4] = *b"APPR";

		(MAGIC, &self.0, session_index).encode()
	}
}

/// A vote of approval for multiple candidates.
#[derive(Clone, Debug)]
pub struct ApprovalVoteMultipleCandidates<'a>(pub &'a [CandidateHash]);

impl<'a> ApprovalVoteMultipleCandidates<'a> {
	/// Yields the signing payload for this approval vote.
	pub fn signing_payload(&self, session_index: SessionIndex) -> Vec<u8> {
		const MAGIC: [u8; 4] = *b"APPR";
		// Make this backwards compatible with `ApprovalVote` so if we have just on candidate the
		// signature will look the same.
		// This gives us the nice benefit that old nodes can still check signatures when len is 1
		// and the new node can check the signature coming from old nodes.
		if self.0.len() == 1 {
			(MAGIC, self.0.first().expect("QED: we just checked"), session_index).encode()
		} else {
			(MAGIC, &self.0, session_index).encode()
		}
	}
}

/// Approval voting configuration parameters
#[derive(
	Debug,
	Copy,
	Clone,
	PartialEq,
	Encode,
	Decode,
	DecodeWithMemTracking,
	TypeInfo,
	serde::Serialize,
	serde::Deserialize,
)]
pub struct ApprovalVotingParams {
	/// The maximum number of candidates `approval-voting` can vote for with
	/// a single signatures.
	///
	/// Setting it to 1, means we send the approval as soon as we have it available.
	pub max_approval_coalesce_count: u32,
}

impl Default for ApprovalVotingParams {
	fn default() -> Self {
		Self { max_approval_coalesce_count: 1 }
	}
}

/// Custom validity errors used in Polkadot while validating transactions.
#[repr(u8)]
pub enum ValidityError {
	/// The Ethereum signature is invalid.
	InvalidEthereumSignature = 0,
	/// The signer has no claim.
	SignerHasNoClaim = 1,
	/// No permission to execute the call.
	NoPermission = 2,
	/// An invalid statement was made for a claim.
	InvalidStatement = 3,
}

impl From<ValidityError> for u8 {
	fn from(err: ValidityError) -> Self {
		err as u8
	}
}

/// Abridged version of `HostConfiguration` (from the `Configuration` parachains host runtime
/// module) meant to be used by a parachain or PDK such as cumulus.
#[derive(Clone, Encode, Decode, Debug, TypeInfo)]
#[cfg_attr(feature = "std", derive(PartialEq))]
pub struct AbridgedHostConfiguration {
	/// The maximum validation code size, in bytes.
	pub max_code_size: u32,
	/// The maximum head-data size, in bytes.
	pub max_head_data_size: u32,
	/// Total number of individual messages allowed in the parachain -> relay-chain message queue.
	pub max_upward_queue_count: u32,
	/// Total size of messages allowed in the parachain -> relay-chain message queue before which
	/// no further messages may be added to it. If it exceeds this then the queue may contain only
	/// a single message.
	pub max_upward_queue_size: u32,
	/// The maximum size of an upward message that can be sent by a candidate.
	///
	/// This parameter affects the size upper bound of the `CandidateCommitments`.
	pub max_upward_message_size: u32,
	/// The maximum number of messages that a candidate can contain.
	///
	/// This parameter affects the size upper bound of the `CandidateCommitments`.
	pub max_upward_message_num_per_candidate: u32,
	/// The maximum number of outbound HRMP messages can be sent by a candidate.
	///
	/// This parameter affects the upper bound of size of `CandidateCommitments`.
	pub hrmp_max_message_num_per_candidate: u32,
	/// The minimum period, in blocks, between which parachains can update their validation code.
	pub validation_upgrade_cooldown: BlockNumber,
	/// The delay, in blocks, before a validation upgrade is applied.
	pub validation_upgrade_delay: BlockNumber,
	/// Asynchronous backing parameters.
	pub async_backing_params: AsyncBackingParams,
}

/// Abridged version of `HrmpChannel` (from the `Hrmp` parachains host runtime module) meant to be
/// used by a parachain or PDK such as cumulus.
#[derive(Clone, Encode, Decode, Debug, TypeInfo)]
#[cfg_attr(feature = "std", derive(Default, PartialEq))]
pub struct AbridgedHrmpChannel {
	/// The maximum number of messages that can be pending in the channel at once.
	pub max_capacity: u32,
	/// The maximum total size of the messages that can be pending in the channel at once.
	pub max_total_size: u32,
	/// The maximum message size that could be put into the channel.
	pub max_message_size: u32,
	/// The current number of messages pending in the channel.
	/// Invariant: should be less or equal to `max_capacity`.s`.
	pub msg_count: u32,
	/// The total size in bytes of all message payloads in the channel.
	/// Invariant: should be less or equal to `max_total_size`.
	pub total_size: u32,
	/// A head of the Message Queue Chain for this channel. Each link in this chain has a form:
	/// `(prev_head, B, H(M))`, where
	/// - `prev_head`: is the previous value of `mqc_head` or zero if none.
	/// - `B`: is the [relay-chain] block number in which a message was appended
	/// - `H(M)`: is the hash of the message being appended.
	/// This value is initialized to a special value that consists of all zeroes which indicates
	/// that no messages were previously added.
	pub mqc_head: Option<Hash>,
}

/// A possible upgrade restriction that prevents a parachain from performing an upgrade.
#[derive(Copy, Clone, Encode, Decode, PartialEq, Debug, TypeInfo)]
pub enum UpgradeRestriction {
	/// There is an upgrade restriction and there are no details about its specifics nor how long
	/// it could last.
	#[codec(index = 0)]
	Present,
}

/// A struct that the relay-chain communicates to a parachain indicating what course of action the
/// parachain should take in the coordinated parachain validation code upgrade process.
///
/// This data type appears in the last step of the upgrade process. After the parachain observes it
/// and reacts to it the upgrade process concludes.
#[derive(Copy, Clone, Encode, Decode, PartialEq, Debug, TypeInfo)]
pub enum UpgradeGoAhead {
	/// Abort the upgrade process. There is something wrong with the validation code previously
	/// submitted by the parachain. This variant can also be used to prevent upgrades by the
	/// governance should an emergency emerge.
	///
	/// The expected reaction on this variant is that the parachain will admit this message and
	/// remove all the data about the pending upgrade. Depending on the nature of the problem (to
	/// be examined offchain for now), it can try to send another validation code or just retry
	/// later.
	#[codec(index = 0)]
	Abort,
	/// Apply the pending code change. The parablock that is built on a relay-parent that is
	/// descendant of the relay-parent where the parachain observed this signal must use the
	/// upgraded validation code.
	#[codec(index = 1)]
	GoAhead,
}

/// Consensus engine id for polkadot v1 consensus engine.
pub const POLKADOT_ENGINE_ID: sp_runtime::ConsensusEngineId = *b"POL1";

/// A consensus log item for polkadot validation. To be used with [`POLKADOT_ENGINE_ID`].
#[derive(Decode, Encode, Clone, PartialEq, Eq)]
pub enum ConsensusLog {
	/// A parachain upgraded its code.
	#[codec(index = 1)]
	ParaUpgradeCode(Id, ValidationCodeHash),
	/// A parachain scheduled a code upgrade.
	#[codec(index = 2)]
	ParaScheduleUpgradeCode(Id, ValidationCodeHash, BlockNumber),
	/// Governance requests to auto-approve every candidate included up to the given block
	/// number in the current chain, inclusive.
	#[codec(index = 3)]
	ForceApprove(BlockNumber),
	/// A signal to revert the block number in the same chain as the
	/// header this digest is part of and all of its descendants.
	///
	/// It is a no-op for a block to contain a revert digest targeting
	/// its own number or a higher number.
	///
	/// In practice, these are issued when on-chain logic has detected an
	/// invalid parachain block within its own chain, due to a dispute.
	#[codec(index = 4)]
	Revert(BlockNumber),
}

impl ConsensusLog {
	/// Attempt to convert a reference to a generic digest item into a consensus log.
	pub fn from_digest_item(
		digest_item: &sp_runtime::DigestItem,
	) -> Result<Option<Self>, codec::Error> {
		match digest_item {
			sp_runtime::DigestItem::Consensus(id, encoded) if id == &POLKADOT_ENGINE_ID => {
				Ok(Some(Self::decode(&mut &encoded[..])?))
			},
			_ => Ok(None),
		}
	}
}

impl From<ConsensusLog> for sp_runtime::DigestItem {
	fn from(c: ConsensusLog) -> sp_runtime::DigestItem {
		Self::Consensus(POLKADOT_ENGINE_ID, c.encode())
	}
}

/// A statement about a candidate, to be used within the dispute resolution process.
///
/// Statements are either in favor of the candidate's validity or against it.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Debug, TypeInfo)]
pub enum DisputeStatement {
	/// A valid statement, of the given kind.
	#[codec(index = 0)]
	Valid(ValidDisputeStatementKind),
	/// An invalid statement, of the given kind.
	#[codec(index = 1)]
	Invalid(InvalidDisputeStatementKind),
}

impl DisputeStatement {
	/// Get the payload data for this type of dispute statement.
	///
	/// Returns Error if the candidate_hash is not included in the list of signed
	/// candidate from ApprovalCheckingMultipleCandidate.
	pub fn payload_data(
		&self,
		candidate_hash: CandidateHash,
		session: SessionIndex,
	) -> Result<Vec<u8>, ()> {
		match self {
			DisputeStatement::Valid(ValidDisputeStatementKind::Explicit) => {
				Ok(ExplicitDisputeStatement { valid: true, candidate_hash, session }
					.signing_payload())
			},
			DisputeStatement::Valid(ValidDisputeStatementKind::BackingSeconded(
				inclusion_parent,
			)) => Ok(CompactStatement::Seconded(candidate_hash).signing_payload(&SigningContext {
				session_index: session,
				parent_hash: *inclusion_parent,
			})),
			DisputeStatement::Valid(ValidDisputeStatementKind::BackingValid(inclusion_parent)) => {
				Ok(CompactStatement::Valid(candidate_hash).signing_payload(&SigningContext {
					session_index: session,
					parent_hash: *inclusion_parent,
				}))
			},
			DisputeStatement::Valid(ValidDisputeStatementKind::ApprovalChecking) => {
				Ok(ApprovalVote(candidate_hash).signing_payload(session))
			},
			DisputeStatement::Valid(
				ValidDisputeStatementKind::ApprovalCheckingMultipleCandidates(candidate_hashes),
			) => {
				if candidate_hashes.contains(&candidate_hash) {
					Ok(ApprovalVoteMultipleCandidates(candidate_hashes).signing_payload(session))
				} else {
					Err(())
				}
			},
			DisputeStatement::Invalid(InvalidDisputeStatementKind::Explicit) => {
				Ok(ExplicitDisputeStatement { valid: false, candidate_hash, session }
					.signing_payload())
			},
		}
	}

	/// Check the signature on a dispute statement.
	pub fn check_signature(
		&self,
		validator_public: &ValidatorId,
		candidate_hash: CandidateHash,
		session: SessionIndex,
		validator_signature: &ValidatorSignature,
	) -> Result<(), ()> {
		let payload = self.payload_data(candidate_hash, session)?;

		if validator_signature.verify(&payload[..], &validator_public) {
			Ok(())
		} else {
			Err(())
		}
	}

	/// Whether the statement indicates validity.
	pub fn indicates_validity(&self) -> bool {
		match *self {
			DisputeStatement::Valid(_) => true,
			DisputeStatement::Invalid(_) => false,
		}
	}

	/// Whether the statement indicates invalidity.
	pub fn indicates_invalidity(&self) -> bool {
		match *self {
			DisputeStatement::Valid(_) => false,
			DisputeStatement::Invalid(_) => true,
		}
	}

	/// Statement is backing statement.
	pub fn is_backing(&self) -> bool {
		match self {
			Self::Valid(s) => s.is_backing(),
			Self::Invalid(_) => false,
		}
	}
}

/// The maximum number of candidate approvals that can be coalesced into a single
/// [`ValidDisputeStatementKind::ApprovalCheckingMultipleCandidates`] statement (and the
/// corresponding approval vote). It is a hard, type-level ceiling enforced at decode time, and the
/// maximum value the runtime accepts for `max_approval_coalesce_count`.
pub const MAX_COALESCE_APPROVALS: u32 = 16;

/// The candidate hashes coalesced into a single approval vote, bounded at the type level to at most
/// [`MAX_COALESCE_APPROVALS`]. Used to carry coalesced approval signatures between subsystems while
/// preserving that bound.
pub type CoalescedApprovalCandidateHashes =
	BoundedVec<CandidateHash, ConstU32<{ MAX_COALESCE_APPROVALS }>>;

/// Different kinds of statements of validity on  a candidate.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Debug, TypeInfo)]
pub enum ValidDisputeStatementKind {
	/// An explicit statement issued as part of a dispute.
	#[codec(index = 0)]
	Explicit,
	/// A seconded statement on a candidate from the backing phase.
	#[codec(index = 1)]
	BackingSeconded(Hash),
	/// A valid statement on a candidate from the backing phase.
	#[codec(index = 2)]
	BackingValid(Hash),
	/// An approval vote from the approval checking phase.
	#[codec(index = 3)]
	ApprovalChecking,
	/// An approval vote from the new version.
	/// We can't create this version until all nodes
	/// have been updated to support it and max_approval_coalesce_count
	/// is set to more than 1.
	///
	/// The number of coalesced candidates is bounded at the type level to at most
	/// [`MAX_COALESCE_APPROVALS`]. This is a hard ceiling enforced at decode time; the effective
	/// per-session limit is `max_approval_coalesce_count`, enforced dynamically when the statement
	/// is validated.
	#[codec(index = 4)]
	ApprovalCheckingMultipleCandidates(
		BoundedVec<CandidateHash, ConstU32<{ MAX_COALESCE_APPROVALS }>>,
	),
}

impl ValidDisputeStatementKind {
	/// Whether the statement is from the backing phase.
	pub fn is_backing(&self) -> bool {
		match self {
			ValidDisputeStatementKind::BackingSeconded(_) |
			ValidDisputeStatementKind::BackingValid(_) => true,
			ValidDisputeStatementKind::Explicit |
			ValidDisputeStatementKind::ApprovalChecking |
			ValidDisputeStatementKind::ApprovalCheckingMultipleCandidates(_) => false,
		}
	}
}

/// Different kinds of statements of invalidity on a candidate.
#[derive(Encode, Decode, DecodeWithMemTracking, Copy, Clone, PartialEq, Debug, TypeInfo)]
pub enum InvalidDisputeStatementKind {
	/// An explicit statement issued as part of a dispute.
	#[codec(index = 0)]
	Explicit,
}

/// An explicit statement on a candidate issued as part of a dispute.
#[derive(Clone, PartialEq, Debug)]
pub struct ExplicitDisputeStatement {
	/// Whether the candidate is valid
	pub valid: bool,
	/// The candidate hash.
	pub candidate_hash: CandidateHash,
	/// The session index of the candidate.
	pub session: SessionIndex,
}

impl ExplicitDisputeStatement {
	/// Produce the payload used for signing this type of statement.
	pub fn signing_payload(&self) -> Vec<u8> {
		const MAGIC: [u8; 4] = *b"DISP";

		(MAGIC, self.valid, self.candidate_hash, self.session).encode()
	}
}

/// A set of statements about a specific candidate.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Debug, TypeInfo)]
pub struct DisputeStatementSet {
	/// The candidate referenced by this set.
	pub candidate_hash: CandidateHash,
	/// The session index of the candidate.
	pub session: SessionIndex,
	/// Statements about the candidate.
	pub statements: Vec<(DisputeStatement, ValidatorIndex, ValidatorSignature)>,
}

impl From<CheckedDisputeStatementSet> for DisputeStatementSet {
	fn from(other: CheckedDisputeStatementSet) -> Self {
		other.0
	}
}

impl AsRef<DisputeStatementSet> for DisputeStatementSet {
	fn as_ref(&self) -> &DisputeStatementSet {
		&self
	}
}

/// A set of dispute statements.
pub type MultiDisputeStatementSet = Vec<DisputeStatementSet>;

/// A _checked_ set of dispute statements.
#[derive(Clone, PartialEq, Debug, Encode)]
pub struct CheckedDisputeStatementSet(DisputeStatementSet);

impl AsRef<DisputeStatementSet> for CheckedDisputeStatementSet {
	fn as_ref(&self) -> &DisputeStatementSet {
		&self.0
	}
}

impl core::cmp::PartialEq<DisputeStatementSet> for CheckedDisputeStatementSet {
	fn eq(&self, other: &DisputeStatementSet) -> bool {
		self.0.eq(other)
	}
}

impl CheckedDisputeStatementSet {
	/// Convert from an unchecked, the verification of correctness of the `unchecked` statement set
	/// _must_ be done before calling this function!
	pub fn unchecked_from_unchecked(unchecked: DisputeStatementSet) -> Self {
		Self(unchecked)
	}
}

/// A set of _checked_ dispute statements.
pub type CheckedMultiDisputeStatementSet = Vec<CheckedDisputeStatementSet>;

/// The entire state of a dispute.
#[derive(Encode, Decode, Clone, Debug, PartialEq, TypeInfo)]
pub struct DisputeState<N = BlockNumber> {
	/// A bitfield indicating all validators for the candidate.
	pub validators_for: BitVec<u8, bitvec::order::Lsb0>, // one bit per validator.
	/// A bitfield indicating all validators against the candidate.
	pub validators_against: BitVec<u8, bitvec::order::Lsb0>, // one bit per validator.
	/// The block number at which the dispute started on-chain.
	pub start: N,
	/// The block number at which the dispute concluded on-chain.
	pub concluded_at: Option<N>,
}

/// An either implicit or explicit attestation to the validity of a parachain
/// candidate.
#[derive(Clone, Eq, PartialEq, Decode, DecodeWithMemTracking, Encode, Debug, TypeInfo)]
pub enum ValidityAttestation {
	/// Implicit validity attestation by issuing.
	/// This corresponds to issuance of a `Candidate` statement.
	#[codec(index = 1)]
	Implicit(ValidatorSignature),
	/// An explicit attestation. This corresponds to issuance of a
	/// `Valid` statement.
	#[codec(index = 2)]
	Explicit(ValidatorSignature),
}

impl ValidityAttestation {
	/// Produce the underlying signed payload of the attestation, given the hash of the candidate,
	/// which should be known in context.
	pub fn to_compact_statement(&self, candidate_hash: CandidateHash) -> CompactStatement {
		// Explicit and implicit map directly from
		// `ValidityVote::Valid` and `ValidityVote::Issued`, and hence there is a
		// `1:1` relationship which enables the conversion.
		match *self {
			ValidityAttestation::Implicit(_) => CompactStatement::Seconded(candidate_hash),
			ValidityAttestation::Explicit(_) => CompactStatement::Valid(candidate_hash),
		}
	}

	/// Get a reference to the signature.
	pub fn signature(&self) -> &ValidatorSignature {
		match *self {
			ValidityAttestation::Implicit(ref sig) => sig,
			ValidityAttestation::Explicit(ref sig) => sig,
		}
	}

	/// Produce the underlying signed payload of the attestation, given the hash of the candidate,
	/// which should be known in context.
	pub fn signed_payload<H: Encode>(
		&self,
		candidate_hash: CandidateHash,
		signing_context: &SigningContext<H>,
	) -> Vec<u8> {
		match *self {
			ValidityAttestation::Implicit(_) => {
				(CompactStatement::Seconded(candidate_hash), signing_context).encode()
			},
			ValidityAttestation::Explicit(_) => {
				(CompactStatement::Valid(candidate_hash), signing_context).encode()
			},
		}
	}
}

/// A type returned by runtime with current session index and a parent hash.
#[derive(Clone, Eq, PartialEq, Default, Decode, Encode, Debug)]
pub struct SigningContext<H = Hash> {
	/// Current session index.
	pub session_index: sp_staking::SessionIndex,
	/// Hash of the parent.
	pub parent_hash: H,
}

const BACKING_STATEMENT_MAGIC: [u8; 4] = *b"BKNG";

/// Statements that can be made about parachain candidates. These are the
/// actual values that are signed.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[cfg_attr(feature = "std", derive(Hash))]
pub enum CompactStatement {
	/// Proposal of a parachain candidate.
	Seconded(CandidateHash),
	/// State that a parachain candidate is valid.
	Valid(CandidateHash),
}

impl CompactStatement {
	/// Yields the payload used for validator signatures on this kind
	/// of statement.
	pub fn signing_payload(&self, context: &SigningContext) -> Vec<u8> {
		(self, context).encode()
	}

	/// Get the underlying candidate hash this references.
	pub fn candidate_hash(&self) -> &CandidateHash {
		match *self {
			CompactStatement::Seconded(ref h) | CompactStatement::Valid(ref h) => h,
		}
	}
}

// Inner helper for codec on `CompactStatement`.
#[derive(Encode, Decode, TypeInfo)]
enum CompactStatementInner {
	#[codec(index = 1)]
	Seconded(CandidateHash),
	#[codec(index = 2)]
	Valid(CandidateHash),
}

impl From<CompactStatement> for CompactStatementInner {
	fn from(s: CompactStatement) -> Self {
		match s {
			CompactStatement::Seconded(h) => CompactStatementInner::Seconded(h),
			CompactStatement::Valid(h) => CompactStatementInner::Valid(h),
		}
	}
}

impl codec::Encode for CompactStatement {
	fn size_hint(&self) -> usize {
		// magic + discriminant + payload
		4 + 1 + 32
	}

	fn encode_to<T: codec::Output + ?Sized>(&self, dest: &mut T) {
		dest.write(&BACKING_STATEMENT_MAGIC);
		CompactStatementInner::from(self.clone()).encode_to(dest)
	}
}

impl codec::Decode for CompactStatement {
	fn decode<I: codec::Input>(input: &mut I) -> Result<Self, codec::Error> {
		let maybe_magic = <[u8; 4]>::decode(input)?;
		if maybe_magic != BACKING_STATEMENT_MAGIC {
			return Err(codec::Error::from("invalid magic string"));
		}

		Ok(match CompactStatementInner::decode(input)? {
			CompactStatementInner::Seconded(h) => CompactStatement::Seconded(h),
			CompactStatementInner::Valid(h) => CompactStatement::Valid(h),
		})
	}
}

/// `IndexedVec` struct indexed by type specific indices.
#[derive(Clone, Encode, Decode, Debug, TypeInfo)]
#[cfg_attr(feature = "std", derive(PartialEq))]
pub struct IndexedVec<K, V>(Vec<V>, PhantomData<fn(K) -> K>);

impl<K, V> Default for IndexedVec<K, V> {
	fn default() -> Self {
		Self(vec![], PhantomData)
	}
}

impl<K, V> From<Vec<V>> for IndexedVec<K, V> {
	fn from(validators: Vec<V>) -> Self {
		Self(validators, PhantomData)
	}
}

impl<K, V> FromIterator<V> for IndexedVec<K, V> {
	fn from_iter<T: IntoIterator<Item = V>>(iter: T) -> Self {
		Self(Vec::from_iter(iter), PhantomData)
	}
}

impl<K, V> IndexedVec<K, V>
where
	V: Clone,
{
	/// Returns a reference to an element indexed using `K`.
	pub fn get(&self, index: K) -> Option<&V>
	where
		K: TypeIndex,
	{
		self.0.get(index.type_index())
	}

	/// Returns a mutable reference to an element indexed using `K`.
	pub fn get_mut(&mut self, index: K) -> Option<&mut V>
	where
		K: TypeIndex,
	{
		self.0.get_mut(index.type_index())
	}

	/// Returns number of elements in vector.
	pub fn len(&self) -> usize {
		self.0.len()
	}

	/// Returns contained vector.
	pub fn to_vec(&self) -> Vec<V> {
		self.0.clone()
	}

	/// Returns an iterator over the underlying vector.
	pub fn iter(&self) -> Iter<'_, V> {
		self.0.iter()
	}

	/// Returns a mutable iterator over the underlying vector.
	pub fn iter_mut(&mut self) -> IterMut<'_, V> {
		self.0.iter_mut()
	}

	/// Creates a consuming iterator.
	pub fn into_iter(self) -> IntoIter<V> {
		self.0.into_iter()
	}

	/// Returns true if the underlying container is empty.
	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}
}

/// The maximum number of validators `f` which may safely be faulty.
///
/// The total number of validators is `n = 3f + e` where `e in { 1, 2, 3 }`.
pub const fn byzantine_threshold(n: usize) -> usize {
	n.saturating_sub(1) / 3
}

/// The supermajority threshold of validators which represents a subset
/// guaranteed to have at least f+1 honest validators.
pub const fn supermajority_threshold(n: usize) -> usize {
	n - byzantine_threshold(n)
}

/// Adjust the configured needed backing votes with the size of the backing group.
pub fn effective_minimum_backing_votes(
	group_len: usize,
	configured_minimum_backing_votes: u32,
) -> usize {
	core::cmp::min(group_len, configured_minimum_backing_votes as usize)
}

/// Information about validator sets of a session.
///
/// NOTE: `SessionInfo` is frozen. Do not include new fields, consider creating a separate runtime
/// API. Reasoning and further outlook [here](https://github.com/paritytech/polkadot/issues/6586).
#[derive(Clone, Encode, Decode, Debug, TypeInfo)]
#[cfg_attr(feature = "std", derive(PartialEq))]
pub struct SessionInfo {
	/// **** New in v2 ******
	/// All the validators actively participating in parachain consensus.
	/// Indices are into the broader validator set.
	pub active_validator_indices: Vec<ValidatorIndex>,
	/// A secure random seed for the session, gathered from BABE.
	pub random_seed: [u8; 32],
	/// The amount of sessions to keep for disputes.
	pub dispute_period: SessionIndex,

	/// **** Old fields *****
	/// Validators in canonical ordering.
	///
	/// NOTE: There might be more authorities in the current session, than `validators`
	/// participating in parachain consensus. See
	/// [`max_validators`](https://github.com/paritytech/polkadot/blob/a52dca2be7840b23c19c153cf7e110b1e3e475f8/runtime/parachains/src/configuration.rs#L148).
	///
	/// `SessionInfo::validators` will be limited to `max_validators` when set.
	pub validators: IndexedVec<ValidatorIndex, ValidatorId>,
	/// Validators' authority discovery keys for the session in canonical ordering.
	///
	/// NOTE: The first `validators.len()` entries will match the corresponding validators in
	/// `validators`, afterwards any remaining authorities can be found. This is any authorities
	/// not participating in parachain consensus - see
	/// [`max_validators`](https://github.com/paritytech/polkadot/blob/a52dca2be7840b23c19c153cf7e110b1e3e475f8/runtime/parachains/src/configuration.rs#L148)
	pub discovery_keys: Vec<AuthorityDiscoveryId>,
	/// The assignment keys for validators.
	///
	/// NOTE: There might be more authorities in the current session, than validators participating
	/// in parachain consensus. See
	/// [`max_validators`](https://github.com/paritytech/polkadot/blob/a52dca2be7840b23c19c153cf7e110b1e3e475f8/runtime/parachains/src/configuration.rs#L148).
	///
	/// Therefore:
	/// ```ignore
	/// 	assignment_keys.len() == validators.len() && validators.len() <= discovery_keys.len()
	/// ```
	pub assignment_keys: Vec<AssignmentId>,
	/// Validators in shuffled ordering - these are the validator groups as produced
	/// by the `Scheduler` module for the session and are typically referred to by
	/// `GroupIndex`.
	pub validator_groups: IndexedVec<GroupIndex, Vec<ValidatorIndex>>,
	/// The number of availability cores used by the protocol during this session.
	pub n_cores: u32,
	/// The zeroth delay tranche width.
	pub zeroth_delay_tranche_width: u32,
	/// The number of samples we do of `relay_vrf_modulo`.
	pub relay_vrf_modulo_samples: u32,
	/// The number of delay tranches in total.
	pub n_delay_tranches: u32,
	/// How many slots (BABE / SASSAFRAS) must pass before an assignment is considered a
	/// no-show.
	pub no_show_slots: u32,
	/// The number of validators needed to approve a block.
	pub needed_approvals: u32,
}

/// A statement from the specified validator whether the given validation code passes PVF
/// pre-checking or not anchored to the given session index.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Debug, TypeInfo)]
pub struct PvfCheckStatement {
	/// `true` if the subject passed pre-checking and `false` otherwise.
	pub accept: bool,
	/// The validation code hash that was checked.
	pub subject: ValidationCodeHash,
	/// The index of a session during which this statement is considered valid.
	pub session_index: SessionIndex,
	/// The index of the validator from which this statement originates.
	pub validator_index: ValidatorIndex,
}

impl PvfCheckStatement {
	/// Produce the payload used for signing this type of statement.
	///
	/// It is expected that it will be signed by the validator at `validator_index` in the
	/// `session_index`.
	pub fn signing_payload(&self) -> Vec<u8> {
		const MAGIC: [u8; 4] = *b"VCPC"; // for "validation code pre-checking"
		(MAGIC, self.accept, self.subject, self.session_index, self.validator_index).encode()
	}
}

/// A well-known and typed storage key.
///
/// Allows for type-safe access to raw well-known storage keys.
pub struct WellKnownKey<T> {
	/// The raw storage key.
	pub key: Vec<u8>,
	_p: core::marker::PhantomData<T>,
}

impl<T> From<Vec<u8>> for WellKnownKey<T> {
	fn from(key: Vec<u8>) -> Self {
		Self { key, _p: Default::default() }
	}
}

impl<T> AsRef<[u8]> for WellKnownKey<T> {
	fn as_ref(&self) -> &[u8] {
		self.key.as_ref()
	}
}

impl<T: Decode> WellKnownKey<T> {
	/// Gets the value or `None` if it does not exist or decoding failed.
	pub fn get(&self) -> Option<T> {
		sp_io::storage::get(&self.key)
			.and_then(|raw| codec::DecodeAll::decode_all(&mut raw.as_ref()).ok())
	}
}

impl<T: Encode> WellKnownKey<T> {
	/// Sets the value.
	pub fn set(&self, value: T) {
		sp_io::storage::set(&self.key, &value.encode());
	}
}

/// Type discriminator for PVF preparation.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	TypeInfo,
	Clone,
	Copy,
	Debug,
	PartialEq,
	Eq,
	Serialize,
	Deserialize,
)]
pub enum PvfPrepKind {
	/// For prechecking requests.
	Precheck,

	/// For execution and heads-up requests.
	Prepare,
}

/// Type discriminator for PVF execution.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	TypeInfo,
	Clone,
	Copy,
	Debug,
	PartialEq,
	Eq,
	Serialize,
	Deserialize,
)]
pub enum PvfExecKind {
	/// For backing requests.
	Backing,
	/// For approval and dispute request.
	Approval,
}

/// Bit indices in the `HostConfiguration.node_features` that correspond to different node features.
pub type NodeFeatures = BitVec<u8, bitvec::order::Lsb0>;

/// Module containing feature-specific bit indices into the `NodeFeatures` bitvec.
pub mod node_features {
	use crate::NodeFeatures;

	/// A feature index used to identify a bit into the node_features array stored
	/// in the HostConfiguration.
	#[repr(u8)]
	#[derive(Clone, Copy)]
	pub enum FeatureIndex {
		/// Tells if tranch0 assignments could be sent in a single certificate.
		/// Reserved for: `<https://github.com/paritytech/polkadot-sdk/issues/628>`
		EnableAssignmentsV2 = 0,
		/// This feature enables the extension of `BackedCandidate::validator_indices` by 8 bits.
		/// The value stored there represents the assumed core index where the candidates
		/// are backed. This is needed for the elastic scaling MVP.
		ElasticScalingMVP = 1,
		/// Tells if the chunk mapping feature is enabled.
		/// Enables the implementation of
		/// [RFC-47](https://github.com/polkadot-fellows/RFCs/blob/main/text/0047-assignment-of-availability-chunks.md).
		/// Must not be enabled unless all validators and collators have stopped using `req_chunk`
		/// protocol version 1. If it is enabled, validators can start systematic chunk recovery.
		AvailabilityChunkMapping = 2,
		/// Enables node side support of `CoreIndex` committed candidate receipts.
		/// See [RFC-103](https://github.com/polkadot-fellows/RFCs/pull/103) for details.
		/// Only enable if at least 2/3 of nodes support the feature.
		CandidateReceiptV2 = 3,
		/// Enables support for scheduling information in the Candidate Descriptor.
		CandidateReceiptV3 = 4,
		/// First unassigned feature bit.
		/// Every time a new feature flag is assigned it should take this value.
		/// and this should be incremented.
		FirstUnassigned = 5,
	}

	impl FeatureIndex {
		/// Check wheter the feature is enabled.
		pub fn is_set(self, node_features: &NodeFeatures) -> bool {
			node_features.get(self as usize).map(|v| *v).unwrap_or(false)
		}
	}
}

/// Scheduler configuration parameters. All coretime/ondemand parameters are here.
#[derive(
	Debug,
	Copy,
	Clone,
	PartialEq,
	Encode,
	Decode,
	DecodeWithMemTracking,
	TypeInfo,
	serde::Serialize,
	serde::Deserialize,
)]
pub struct SchedulerParams<BlockNumber> {
	/// How often parachain groups should be rotated across parachains.
	///
	/// Must be non-zero.
	pub group_rotation_frequency: BlockNumber,
	/// Availability timeout for a block on a core, measured in blocks.
	///
	/// This is the maximum amount of blocks after a core became occupied that validators have time
	/// to make the block available.
	///
	/// This value only has effect on group rotations. If backers backed something at the end of
	/// their rotation, the occupied core affects the backing group that comes afterwards. We limit
	/// the effect one backing group can have on the next to `paras_availability_period` blocks.
	///
	/// Within a group rotation there is no timeout as backers are only affecting themselves.
	///
	/// Must be at least 1. With a value of 1, the previous group will not be able to negatively
	/// affect the following group at the expense of a tight availability timeline at group
	/// rotation boundaries.
	pub paras_availability_period: BlockNumber,
	/// The maximum number of validators to have per core.
	///
	/// `None` means no maximum.
	pub max_validators_per_core: Option<u32>,
	/// The amount of blocks ahead to schedule paras.
	pub lookahead: u32,
	/// How many cores are managed by the coretime chain.
	pub num_cores: u32,
	/// Deprecated and no longer used by the runtime.
	/// Removal is tracked by <https://github.com/paritytech/polkadot-sdk/issues/6067>.
	#[deprecated]
	pub max_availability_timeouts: u32,
	/// The maximum queue size of the pay as you go module.
	pub on_demand_queue_max_size: u32,
	/// The target utilization of the spot price queue in percentages.
	pub on_demand_target_queue_utilization: Perbill,
	/// How quickly the fee rises in reaction to increased utilization.
	/// The lower the number the slower the increase.
	pub on_demand_fee_variability: Perbill,
	/// The minimum amount needed to claim a slot in the spot pricing queue.
	pub on_demand_base_fee: Balance,
	/// Deprecated and no longer used by the runtime.
	/// Removal is tracked by <https://github.com/paritytech/polkadot-sdk/issues/6067>.
	#[deprecated]
	pub ttl: BlockNumber,
}

impl<BlockNumber: Default + From<u32>> Default for SchedulerParams<BlockNumber> {
	#[allow(deprecated)]
	fn default() -> Self {
		Self {
			group_rotation_frequency: 1u32.into(),
			paras_availability_period: 1u32.into(),
			max_validators_per_core: Default::default(),
			lookahead: 1,
			num_cores: Default::default(),
			max_availability_timeouts: Default::default(),
			on_demand_queue_max_size: ON_DEMAND_DEFAULT_QUEUE_MAX_SIZE,
			on_demand_target_queue_utilization: Perbill::from_percent(25),
			on_demand_fee_variability: Perbill::from_percent(3),
			on_demand_base_fee: 10_000_000u128,
			ttl: 5u32.into(),
		}
	}
}

/// A type representing the version of the candidate descriptor.
#[derive(PartialEq, Eq, Copy, Clone, Encode, Decode, TypeInfo, Debug, PartialOrd, Ord, Hash)]
pub enum CandidateDescriptorVersion {
	/// with deprecated collator id and collator signature.
	V1,
	/// First properly versioned candidate.
	///
	/// - Removes collator signature and collator id fields.
	/// - Introduces:
	/// -- A version field.
	/// -- session index field.
	/// -- core index field.
	V2,
	/// Candidate with scheduling info.
	V3,
	/// An unknown/not yet supported version.
	///
	/// Such a candidate must be dropped by the runtime and rejected by backers.
	Unknown(u8),
}

/// Error returned by [`CandidateDescriptorV2::check_version_acceptance`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CandidateDescriptorVersionCheckError {
	/// Old-style and new-style version detection disagree, and this is not the
	/// expected V3 disagreement (old rules → V1, new rules → V3) with V3 enabled.
	Inconsistency,
	/// The descriptor is V3 but the V3 feature is not enabled.
	V3NotEnabled,
}

// Manual Display impl required because this type is used in `no_std` runtime
// code (paras_inherent) where thiserror::Error is not available.
impl core::fmt::Display for CandidateDescriptorVersionCheckError {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		match self {
			Self::Inconsistency => {
				write!(f, "Descriptor version detection inconsistency (old vs new rules disagree)")
			},
			Self::V3NotEnabled => write!(f, "V3 candidate descriptor but V3 feature not enabled"),
		}
	}
}

/// A unique descriptor of the candidate receipt.
#[derive(PartialEq, Eq, Clone, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
pub struct CandidateDescriptorV2<H = Hash> {
	/// The ID of the para this is a candidate for.
	pub(super) para_id: ParaId,
	/// The hash of the relay-chain block this is executed in the context of.
	relay_parent: H,
	/// Version field. The raw value here is not exposed, instead it is used
	/// to determine the `CandidateDescriptorVersion`, see `fn version()`.
	/// For the current version this field is set to `0` and will be incremented
	/// by next versions.
	pub(super) version: u8,
	/// The core index where the candidate is backed.
	pub(super) core_index: u16,
	/// The session index of the candidate relay parent.
	session_index: SessionIndex,
	/// Offset from `session_index` to derive the scheduling session (introduced in v3).
	///
	/// Stored as a `u8` offset rather than a full `SessionIndex` to fit within the
	/// descriptor layout: `scheduling_session = session_index + scheduling_session_offset`.
	scheduling_session_offset: u8,
	/// Reserved bytes.
	reserved1: [u8; 24],
	/// The blake2-256 hash of the persisted validation data. This is extra data derived from
	/// relay-chain state which may vary based on bitfields included before the candidate.
	/// Thus it cannot be derived entirely from the relay-parent.
	persisted_validation_data_hash: Hash,
	/// The blake2-256 hash of the PoV.
	pov_hash: Hash,
	/// The root of a block's erasure encoding Merkle tree.
	erasure_root: Hash,
	/// The relay chain block determining scheduling.
	scheduling_parent: H, // Introduced in v3
	/// Reserved bytes.
	reserved2: [u8; 32],
	/// Hash of the para header that is being generated by this candidate.
	para_head: Hash,
	/// The blake2-256 hash of the validation code bytes.
	validation_code_hash: ValidationCodeHash,
}

impl<H: AsRef<[u8]>> CandidateDescriptorV2<H> {
	/// Returns the candidate descriptor version.
	///
	/// NOTE: The candidate descriptor versioning is subtle for as long as we
	/// need to support the unversioned V1. The issue is that by default we
	/// assume a V1 descriptor - as soon as any of the reserved bytes are
	/// non-zero. Now if we introduce any new fields, then there will exist
	/// candidates where any old node will think that descriptors of that new
	/// version are actually V1 (non-zero contents), while upgraded nodes will
	/// either see v3 or an unknown version.
	///
	/// We solve this by completely gating v3 behavior behind the v3 node
	/// feature, which must only be enabled once enough validators have upgraded
	/// to support it. Any backers still running on the old version are
	/// protected by the relay chain runtime, which will drop any illegally
	/// (under v3) backed candidates.
	///
	/// For this to work we now also require a present UMP signal for any
	/// version higher or equal than V3. This is enforced by the runtime.
	///
	/// Via this, if an old node was presented a v3 candidate, which it would
	/// consider a V1, it would either detect itself that it is invalid, because
	/// of present UMP signals - which is illegal on v1 or the candidate would
	/// get rejected by the runtime, because for v3 UMP signals are mandatory.
	/// In both cases the backer wont't be slashed.
	///
	/// There are also candidates that would be treated as v1 by old nodes, but
	/// would result in an Unknown version on updated clients. For this
	/// scenario, also the runtime provides protection:
	///
	/// 1. Before the feature is enabled, all nodes will behave as if no v3
	/// would exist - all nodes would detect a V1.
	/// 2. After the upgrade, the runtime will also (in addition to upgraded
	/// nodes) detect an unknown version and no v1 and thus would drop it.
	///
	/// TL;DR: Yes old nodes will errorneously treat v3 candidates as v1, but we
	/// ensure via the relay chain runtime that this stays harmless for backers.
	/// V2 approval voters would get disabled, which means a super majority must
	/// have updated before enabling the v3 node feature.
	///
	/// Crucially for this to work: Behavior must not change before the node
	/// feature is present and enabled, together with new UMP signal
	/// requirements, the runtime can provide the necessary protection.
	///
	/// To ease future upgrades, we reduced the v1 check once v3 is enabled, so
	/// some actually unused bytes are available (don't affect the v1 version
	/// check).
	///
	/// Always uses the relaxed (v3-capable) detection logic. This means
	/// version detection is self-contained and does not require knowing
	/// whether the V3 node feature is enabled.
	///
	/// The safety invariant is maintained by the runtime and backing
	/// subsystem: they reject candidates where `version()` and
	/// `version_old_rules()` disagree when V3 is not yet enabled, and
	/// reject V3 candidates outright when V3 is not enabled.
	///
	/// During the V3 transition, approval checkers, dispute participants,
	/// and on-chain vote scrapers must use [`Self::version_for_candidate_validation`]
	/// (and the corresponding `scheduling_parent_for_candidate_validation` /
	/// `scheduling_session_for_candidate_validation`) instead of `version()`
	/// directly. This ensures they match old backer semantics before the V3
	/// node feature is confirmed enabled. See those methods for the full
	/// safety argument.
	pub fn version(&self) -> CandidateDescriptorVersion {
		self.v3_version()
	}

	/// Detect the version using the pre-V3 (stricter) rules.
	///
	/// Under these rules, all reserved fields, `scheduling_parent`, and
	/// `scheduling_session_offset` must be zero for a descriptor to be
	/// considered V2. Any non-zero value in those fields causes V1
	/// detection. V3 descriptors appear as V1 under these rules.
	///
	/// Used together with `version()` in consistency checks: if the two
	/// methods disagree, the candidate is ambiguous and must be rejected
	/// when V3 is not enabled.
	pub fn version_old_rules(&self) -> CandidateDescriptorVersion {
		self.v2_version()
	}

	/// Returns `true` if the old-style and new-style version detection agree.
	///
	/// When V3 is not enabled, both runtime and backing must reject candidates
	/// where this returns `false`, preventing ambiguous candidates from landing
	/// on-chain. Once V3 is enabled, disagreement is expected for V3 candidates
	/// (old rules see V1, new rules see V3) and this check is skipped.
	pub fn check_version_consistency(&self) -> bool {
		self.version() == self.version_old_rules()
	}

	/// Validates that the descriptor version is acceptable given whether V3 is enabled.
	///
	/// Used by both the runtime (`check_descriptor_version_and_signals`) and the
	/// backing subsystem. Serves two distinct purposes:
	///
	/// 1. **V2 ambiguity protection (long-lived):** Old-style and new-style version detection must
	///    agree, unless the candidate is V3 and V3 is enabled (the expected disagreement: old rules
	///    see V1, new rules see V3). This prevents a crafted candidate from being treated as V2 (no
	///    mandatory UMP signals) by new nodes but as V1 by old nodes. Needed as long as V1 exists
	///    (maximum safety) or until we could have valiators not yet using the new rules.
	///
	/// 2. **V3 gating (transitional):** V3 candidates are rejected when V3 is not enabled.
	///
	/// Note: Consistent `Unknown` versions are not our concern here — they are caught upstream
	/// by the runtime (`check_descriptor_version_and_signals`) and the collator
	/// protocol (`descriptor_version_sanity_check`).
	pub fn check_version_acceptance(
		&self,
		v3_enabled: bool,
	) -> Result<(), CandidateDescriptorVersionCheckError> {
		let version = self.version();

		// Version consistency: old and new detection must agree, unless this is the
		// expected V3 disagreement (old rules → V1, new rules → V3) with V3 enabled.
		let is_expected_v3_disagreement = version == CandidateDescriptorVersion::V3 && v3_enabled;
		if !self.check_version_consistency() && !is_expected_v3_disagreement {
			return Err(CandidateDescriptorVersionCheckError::Inconsistency);
		}

		// V3 gating: reject V3 candidates before the feature is enabled.
		if version == CandidateDescriptorVersion::V3 && !v3_enabled {
			return Err(CandidateDescriptorVersionCheckError::V3NotEnabled);
		}

		Ok(())
	}

	fn v2_version(&self) -> CandidateDescriptorVersion {
		// V1 detected using the pre-v3 (stricter) check: all reserved and new
		// fields must be zero. Once v3 is enabled, the v1 check is relaxed in
		// `v3_version()` to free up more bytes for future use.
		let old_v1_detected = self.reserved2 != [0u8; 32] ||
			self.reserved1 != [0u8; 24] ||
			self.scheduling_session_offset != 0 ||
			self.scheduling_parent.as_ref() != &[0u8; 32];

		if old_v1_detected {
			return CandidateDescriptorVersion::V1;
		}

		match self.version {
			0 => CandidateDescriptorVersion::V2,
			_ => CandidateDescriptorVersion::Unknown(self.version),
		}
	}
}

impl<H> CandidateDescriptorV2<H> {
	fn v3_version(&self) -> CandidateDescriptorVersion {
		// Reduce checked bits for v1 significantly to make more bytes easier
		// usable in future upgrades. 16 bytes is 32 hexadecimal digits which
		// must all be 0 by accident to cause any issues. Bitcoin hardest
		// difficulty so far has been 24 digits/12 bytes
		//
		// Impact if it still happened would also be fairly minimal: We would
		// drop a parachain block, which is not a big deal on v1, where we are
		// not aiming for perfect block confidence.
		let new_v1_detected = self.reserved1[0..16] != [0u8; 16];

		if new_v1_detected {
			return CandidateDescriptorVersion::V1;
		}
		match self.version {
			0 => CandidateDescriptorVersion::V2,
			1 => CandidateDescriptorVersion::V3,
			_ => CandidateDescriptorVersion::Unknown(self.version),
		}
	}
}

macro_rules! impl_getter {
	($field:ident, $type:ident) => {
		/// Returns the value of `$field` field.
		pub fn $field(&self) -> $type {
			self.$field
		}
	};
}

impl<H: Copy + AsRef<[u8]>> CandidateDescriptorV2<H> {
	impl_getter!(erasure_root, Hash);
	impl_getter!(para_head, Hash);
	impl_getter!(relay_parent, H);
	impl_getter!(para_id, ParaId);
	impl_getter!(persisted_validation_data_hash, Hash);
	impl_getter!(pov_hash, Hash);
	impl_getter!(validation_code_hash, ValidationCodeHash);

	#[cfg(feature = "test")]
	fn rebuild_collator_field(&self) -> CollatorId {
		let mut collator_id = Vec::with_capacity(32);
		let core_index: [u8; 2] = self.core_index.to_ne_bytes();
		let session_index: [u8; 4] = self.session_index.to_ne_bytes();

		collator_id.push(self.version);
		collator_id.extend_from_slice(core_index.as_slice());
		collator_id.extend_from_slice(session_index.as_slice());
		collator_id.push(self.scheduling_session_offset);
		collator_id.extend_from_slice(self.reserved1.as_slice());

		CollatorId::from_slice(&collator_id.as_slice())
			.expect("Slice size is exactly 32 bytes; qed")
	}

	/// Returns the collator id if this is a v1 `CandidateDescriptor`
	#[cfg(feature = "test")]
	pub fn collator(&self) -> Option<CollatorId> {
		if self.version() == CandidateDescriptorVersion::V1 {
			Some(self.rebuild_collator_field())
		} else {
			None
		}
	}

	#[cfg(feature = "test")]
	fn rebuild_signature_field(&self) -> CollatorSignature {
		let mut signature_bytes = Vec::with_capacity(64);
		signature_bytes.extend_from_slice(self.scheduling_parent.as_ref());
		signature_bytes.extend_from_slice(self.reserved2.as_slice());

		CollatorSignature::from_slice(&signature_bytes)
			.expect("Slice size is exactly 64 bytes; qed")
	}

	#[cfg(feature = "test")]
	#[doc(hidden)]
	pub fn rebuild_collator_field_for_tests(&self) -> CollatorId {
		self.rebuild_collator_field()
	}

	#[cfg(feature = "test")]
	#[doc(hidden)]
	pub fn rebuild_signature_field_for_tests(&self) -> CollatorSignature {
		self.rebuild_signature_field()
	}

	/// Returns the collator signature of `V1` candidate descriptors, `None` otherwise.
	#[cfg(feature = "test")]
	pub fn signature(&self) -> Option<CollatorSignature> {
		if self.version() == CandidateDescriptorVersion::V1 {
			return Some(self.rebuild_signature_field());
		}

		None
	}

	/// Returns the `core_index` of `V2` and `V3` candidate descriptors, `None` for `V1`.
	pub fn core_index(&self) -> Option<CoreIndex> {
		if self.version() == CandidateDescriptorVersion::V1 {
			return None;
		}

		Some(CoreIndex(self.core_index as u32))
	}

	/// Returns the `session_index` of `V2` and `V3` candidate descriptors, `None` for `V1`.
	pub fn session_index(&self) -> Option<SessionIndex> {
		if self.version() == CandidateDescriptorVersion::V1 {
			return None;
		}

		Some(self.session_index)
	}

	/// Return the scheduling parent of the descriptor.
	///
	///
	/// On v1 and v2 this function will return the relay parent as under these versions the relay
	/// parent is also the scheduling parent.
	pub fn scheduling_parent(&self) -> H {
		match self.version() {
			CandidateDescriptorVersion::V1 => self.relay_parent,
			CandidateDescriptorVersion::V2 => self.relay_parent,
			CandidateDescriptorVersion::V3 => self.scheduling_parent,
			CandidateDescriptorVersion::Unknown(_) => self.relay_parent,
		}
	}

	/// Return the scheduling session index of the descriptor.
	///
	///
	/// On v1: Return None.
	/// On v2: Return the session index as it equals the scheduling session on v2.
	/// On v3: Return the provided scheduling session index.
	pub fn scheduling_session(&self) -> Option<SessionIndex> {
		match self.version() {
			CandidateDescriptorVersion::V1 => None,
			CandidateDescriptorVersion::V2 => Some(self.session_index),
			CandidateDescriptorVersion::V3 => {
				Some(self.session_index.saturating_add(self.scheduling_session_offset as _))
			},
			CandidateDescriptorVersion::Unknown(_) => None,
		}
	}

	/// Version for use in candidate validation during the V3 transition period.
	///
	/// Before the `CandidateReceiptV3` node feature is observed, uses
	/// [`Self::version_old_rules`] to match old backer behavior. After the feature
	/// is seen, trusts [`Self::version`].
	///
	/// This prevents slashing honest old backers when a malicious collator crafts
	/// a pseudo-V3 descriptor that old nodes interpret as V1 but new nodes would
	/// interpret as V3 (different PVF inputs → dispute → 100% slash).
	///
	/// Safety argument: The node feature can only be enabled well after the runtime upgrade that
	/// adds `check_version_acceptance()` protection at inclusion time. Once the feature is seen,
	/// the runtime has long been upgraded and already rejecting pseudo-V3 candidates (candidates
	/// that are valid v1 under the old rules, but are v3 without UMP signals under the new
	/// rules), so no ambiguous candidates can exist on-chain.
	///
	/// Only needed during the V3 transition. Once V3 is universally deployed,
	/// callers can switch to [`Self::version`] directly.
	pub fn version_for_candidate_validation(
		&self,
		v3_ever_seen: bool,
	) -> CandidateDescriptorVersion {
		if v3_ever_seen {
			self.version()
		} else {
			self.version_old_rules()
		}
	}

	/// Scheduling parent for use in candidate validation.
	///
	/// See [`Self::version_for_candidate_validation`] for the safety argument.
	pub fn scheduling_parent_for_candidate_validation(&self, v3_ever_seen: bool) -> H
	where
		H: Copy,
	{
		match self.version_for_candidate_validation(v3_ever_seen) {
			CandidateDescriptorVersion::V3 => self.scheduling_parent,
			_ => self.relay_parent,
		}
	}

	/// Scheduling session for candidate validation.
	///
	/// See [`Self::version_for_candidate_validation`] for the safety argument.
	pub fn scheduling_session_for_candidate_validation(
		&self,
		v3_ever_seen: bool,
	) -> Option<SessionIndex> {
		match self.version_for_candidate_validation(v3_ever_seen) {
			CandidateDescriptorVersion::V1 => None,
			CandidateDescriptorVersion::V2 => Some(self.session_index),
			CandidateDescriptorVersion::V3 => {
				Some(self.session_index.saturating_add(self.scheduling_session_offset as _))
			},
			CandidateDescriptorVersion::Unknown(_) => None,
		}
	}

	/// Session index (relay parent session) for candidate validation.
	///
	/// See [`Self::version_for_candidate_validation`] for the safety argument.
	pub fn session_index_for_candidate_validation(
		&self,
		v3_ever_seen: bool,
	) -> Option<SessionIndex> {
		match self.version_for_candidate_validation(v3_ever_seen) {
			CandidateDescriptorVersion::V1 | CandidateDescriptorVersion::Unknown(_) => None,
			CandidateDescriptorVersion::V2 | CandidateDescriptorVersion::V3 => {
				Some(self.session_index)
			},
		}
	}
}

impl<H> core::fmt::Debug for CandidateDescriptorV2<H>
where
	H: core::fmt::Debug,
{
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		// A bit imprecise, but should not matter in practice for debug output. (Keeps trait bounds
		// sane.)
		match self.v3_version() {
			CandidateDescriptorVersion::V1 => f
				.debug_struct("CandidateDescriptorV1")
				.field("para_id", &self.para_id)
				.field("relay_parent", &self.relay_parent)
				.field("persisted_validation_hash", &self.persisted_validation_data_hash)
				.field("pov_hash", &self.pov_hash)
				.field("erasure_root", &self.erasure_root)
				.field("para_head", &self.para_head)
				.field("validation_code_hash", &self.validation_code_hash)
				.finish(),
			CandidateDescriptorVersion::V2 => f
				.debug_struct("CandidateDescriptorV2")
				.field("para_id", &self.para_id)
				.field("relay_parent", &self.relay_parent)
				.field("core_index", &self.core_index)
				.field("session_index", &self.session_index)
				.field("persisted_validation_data_hash", &self.persisted_validation_data_hash)
				.field("pov_hash", &self.pov_hash)
				.field("erasure_root", &self.erasure_root)
				.field("para_head", &self.para_head)
				.field("validation_code_hash", &self.validation_code_hash)
				.finish(),
			CandidateDescriptorVersion::V3 => f
				.debug_struct("CandidateDescriptorV3")
				.field("para_id", &self.para_id)
				.field("relay_parent", &self.relay_parent)
				.field("core_index", &self.core_index)
				.field("session_index", &self.session_index)
				.field("scheduling_session_offset", &self.scheduling_session_offset)
				.field("persisted_validation_data_hash", &self.persisted_validation_data_hash)
				.field("pov_hash", &self.pov_hash)
				.field("erasure_root", &self.erasure_root)
				.field("scheduling_parent", &self.scheduling_parent)
				.field("para_head", &self.para_head)
				.field("validation_code_hash", &self.validation_code_hash)
				.finish(),
			CandidateDescriptorVersion::Unknown(raw_version) => {
				write!(f, "CandidateDescriptorV2(unknown version={})", raw_version)
			},
		}
	}
}

impl<H: Copy + AsRef<[u8]>> CandidateDescriptorV2<H> {
	/// Constructor for V2 candidate descriptor (scheduling_parent = zero).
	pub fn new(
		para_id: Id,
		relay_parent: H,
		core_index: CoreIndex,
		session_index: SessionIndex,
		persisted_validation_data_hash: Hash,
		pov_hash: Hash,
		erasure_root: Hash,
		para_head: Hash,
		validation_code_hash: ValidationCodeHash,
	) -> Self
	where
		H: Default,
	{
		Self {
			para_id,
			relay_parent,
			version: 0,
			core_index: core_index.0 as u16,
			session_index,
			scheduling_session_offset: 0,
			reserved1: [0; 24],
			persisted_validation_data_hash,
			pov_hash,
			erasure_root,
			scheduling_parent: H::default(),
			reserved2: [0; 32],
			para_head,
			validation_code_hash,
		}
	}

	/// Constructor for V3 candidate descriptor with explicit scheduling_parent.
	///
	/// V3 descriptors are identified by `version == 1` and have a non-zero scheduling_parent
	/// field, which indicates the relay chain block that was used for scheduling (may differ
	/// from relay_parent). V3 descriptors require UMP signals to be present.
	pub fn new_v3(
		para_id: Id,
		relay_parent: H,
		core_index: CoreIndex,
		session_index: SessionIndex,
		scheduling_session_index: SessionIndex,
		persisted_validation_data_hash: Hash,
		pov_hash: Hash,
		erasure_root: Hash,
		para_head: Hash,
		validation_code_hash: ValidationCodeHash,
		scheduling_parent: H,
	) -> Self {
		Self {
			para_id,
			relay_parent,
			version: 1,
			core_index: core_index.0 as u16,
			session_index,
			scheduling_session_offset: scheduling_session_index
				.saturating_sub(session_index)
				.try_into()
				.expect("scheduling session offset should fit in u8"),
			reserved1: [0; 24],
			persisted_validation_data_hash,
			pov_hash,
			erasure_root,
			scheduling_parent,
			reserved2: [0; 32],
			para_head,
			validation_code_hash,
		}
	}

	/// Constructor for a V1-like candidate descriptor with non-zero collator
	/// fields so that `version()` returns [`CandidateDescriptorVersion::V1`].
	pub fn new_v1(
		para_id: Id,
		relay_parent: H,
		persisted_validation_data_hash: Hash,
		pov_hash: Hash,
		erasure_root: Hash,
		para_head: Hash,
		validation_code_hash: ValidationCodeHash,
	) -> Self
	where
		H: Default,
	{
		Self {
			para_id,
			relay_parent,
			version: 0,
			core_index: 0,
			session_index: 0,
			scheduling_session_offset: 0,
			reserved1: [1u8; 24],
			persisted_validation_data_hash,
			pov_hash,
			erasure_root,
			scheduling_parent: H::default(),
			reserved2: [1u8; 32],
			para_head,
			validation_code_hash,
		}
	}

	#[cfg(feature = "test")]
	#[doc(hidden)]
	pub fn new_from_raw(
		para_id: Id,
		relay_parent: H,
		version: u8,
		core_index: u16,
		session_index: SessionIndex,
		scheduling_session_offset: u8,
		reserved1: [u8; 24],
		persisted_validation_data_hash: Hash,
		pov_hash: Hash,
		erasure_root: Hash,
		scheduling_parent: H,
		reserved2: [u8; 32],
		para_head: Hash,
		validation_code_hash: ValidationCodeHash,
	) -> Self {
		Self {
			para_id,
			relay_parent,
			version,
			core_index,
			session_index,
			scheduling_session_offset,
			reserved1,
			persisted_validation_data_hash,
			pov_hash,
			erasure_root,
			scheduling_parent,
			reserved2,
			para_head,
			validation_code_hash,
		}
	}
}

/// A trait to allow changing the descriptor field values in tests.
#[cfg(feature = "test")]
pub trait MutateDescriptorV2<H> {
	/// Set the relay parent of the descriptor.
	fn set_relay_parent(&mut self, relay_parent: H);
	/// Set the `ParaId` of the descriptor.
	fn set_para_id(&mut self, para_id: Id);
	/// Set the PoV hash of the descriptor.
	fn set_pov_hash(&mut self, pov_hash: Hash);
	/// Set the raw version field of the descriptor.
	fn set_version(&mut self, version: u8);
	/// Set the PVD of the descriptor.
	fn set_persisted_validation_data_hash(&mut self, persisted_validation_data_hash: Hash);
	/// Set the validation code hash of the descriptor.
	fn set_validation_code_hash(&mut self, validation_code_hash: ValidationCodeHash);
	/// Set the erasure root of the descriptor.
	fn set_erasure_root(&mut self, erasure_root: Hash);
	/// Set the para head of the descriptor.
	fn set_para_head(&mut self, para_head: Hash);
	/// Set the core index of the descriptor.
	fn set_core_index(&mut self, core_index: CoreIndex);
	/// Set the session index of the descriptor.
	fn set_session_index(&mut self, session_index: SessionIndex);
	/// Set the reserved2 field of the descriptor.
	fn set_reserved2(&mut self, reserved2: [u8; 32]);
	/// Set the scheduling parent of the descriptor.
	fn set_scheduling_parent(&mut self, scheduling_parent: H);
	/// Set the scheduling session offset of the descriptor.
	fn set_scheduling_session_offset(&mut self, offset: u8);
}

#[cfg(feature = "test")]
impl<H> MutateDescriptorV2<H> for CandidateDescriptorV2<H> {
	fn set_para_id(&mut self, para_id: Id) {
		self.para_id = para_id;
	}

	fn set_relay_parent(&mut self, relay_parent: H) {
		self.relay_parent = relay_parent;
	}

	fn set_pov_hash(&mut self, pov_hash: Hash) {
		self.pov_hash = pov_hash;
	}

	fn set_version(&mut self, version: u8) {
		self.version = version;
	}

	fn set_core_index(&mut self, core_index: CoreIndex) {
		self.core_index = core_index.0 as u16;
	}

	fn set_session_index(&mut self, session_index: SessionIndex) {
		self.session_index = session_index;
	}

	fn set_persisted_validation_data_hash(&mut self, persisted_validation_data_hash: Hash) {
		self.persisted_validation_data_hash = persisted_validation_data_hash;
	}

	fn set_validation_code_hash(&mut self, validation_code_hash: ValidationCodeHash) {
		self.validation_code_hash = validation_code_hash;
	}

	fn set_erasure_root(&mut self, erasure_root: Hash) {
		self.erasure_root = erasure_root;
	}

	fn set_para_head(&mut self, para_head: Hash) {
		self.para_head = para_head;
	}

	fn set_reserved2(&mut self, reserved2: [u8; 32]) {
		self.reserved2 = reserved2;
	}

	fn set_scheduling_parent(&mut self, scheduling_parent: H) {
		self.scheduling_parent = scheduling_parent;
	}

	fn set_scheduling_session_offset(&mut self, offset: u8) {
		self.scheduling_session_offset = offset;
	}
}

/// A candidate-receipt at version 2.
#[derive(PartialEq, Eq, Clone, Encode, Decode, DecodeWithMemTracking, TypeInfo, Debug)]
pub struct CandidateReceiptV2<H = Hash> {
	/// The descriptor of the candidate.
	pub descriptor: CandidateDescriptorV2<H>,
	/// The hash of the encoded commitments made as a result of candidate execution.
	pub commitments_hash: Hash,
}

/// A candidate-receipt with commitments directly included.
#[derive(PartialEq, Eq, Clone, Encode, Decode, DecodeWithMemTracking, TypeInfo, Debug)]
pub struct CommittedCandidateReceiptV2<H = Hash> {
	/// The descriptor of the candidate.
	pub descriptor: CandidateDescriptorV2<H>,
	/// The commitments of the candidate receipt.
	pub commitments: CandidateCommitments,
}

/// An event concerning a candidate.
#[derive(Clone, Encode, Decode, TypeInfo, Debug)]
#[cfg_attr(feature = "std", derive(PartialEq))]
pub enum CandidateEvent<H = Hash> {
	/// This candidate receipt was backed in the most recent block.
	/// This includes the core index the candidate is now occupying.
	#[codec(index = 0)]
	CandidateBacked(CandidateReceiptV2<H>, HeadData, CoreIndex, GroupIndex),
	/// This candidate receipt was included and became a parablock at the most recent block.
	/// This includes the core index the candidate was occupying as well as the group responsible
	/// for backing the candidate.
	#[codec(index = 1)]
	CandidateIncluded(CandidateReceiptV2<H>, HeadData, CoreIndex, GroupIndex),
	/// This candidate receipt was not made available in time and timed out.
	/// This includes the core index the candidate was occupying.
	#[codec(index = 2)]
	CandidateTimedOut(CandidateReceiptV2<H>, HeadData, CoreIndex),
}

impl<H> CandidateReceiptV2<H> {
	/// Get a reference to the candidate descriptor.
	pub fn descriptor(&self) -> &CandidateDescriptorV2<H> {
		&self.descriptor
	}

	/// Computes the blake2-256 hash of the receipt.
	pub fn hash(&self) -> CandidateHash
	where
		H: Encode,
	{
		CandidateHash(BlakeTwo256::hash_of(self))
	}
}

impl<H: Clone> CommittedCandidateReceiptV2<H> {
	/// Transforms this into a plain `CandidateReceipt`.
	pub fn to_plain(&self) -> CandidateReceiptV2<H> {
		CandidateReceiptV2 {
			descriptor: self.descriptor.clone(),
			commitments_hash: self.commitments.hash(),
		}
	}

	/// Computes the hash of the committed candidate receipt.
	///
	/// This computes the canonical hash, not the hash of the directly encoded data.
	/// Thus this is a shortcut for `candidate.to_plain().hash()`.
	pub fn hash(&self) -> CandidateHash
	where
		H: Encode,
	{
		self.to_plain().hash()
	}

	/// Does this committed candidate receipt corresponds to the given [`CandidateReceiptV2`]?
	pub fn corresponds_to(&self, receipt: &CandidateReceiptV2<H>) -> bool
	where
		H: PartialEq,
	{
		receipt.descriptor == self.descriptor && receipt.commitments_hash == self.commitments.hash()
	}
}

impl PartialOrd for CommittedCandidateReceiptV2 {
	fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl Ord for CommittedCandidateReceiptV2 {
	fn cmp(&self, other: &Self) -> core::cmp::Ordering {
		self.descriptor
			.para_id
			.cmp(&other.descriptor.para_id)
			.then_with(|| self.commitments.head_data.cmp(&other.commitments.head_data))
	}
}

/// A strictly increasing sequence number, typically this would be the least significant byte of the
/// block number.
#[derive(PartialEq, Eq, Clone, Encode, Decode, TypeInfo, Debug, Copy)]
pub struct CoreSelector(pub u8);

impl From<u8> for CoreSelector {
	fn from(value: u8) -> Self {
		Self(value)
	}
}

/// An offset in the relay chain claim queue.
#[derive(PartialEq, Eq, Clone, Encode, Decode, TypeInfo, Debug, Copy)]
pub struct ClaimQueueOffset(pub u8);

impl From<u8> for ClaimQueueOffset {
	fn from(value: u8) -> Self {
		Self(value)
	}
}

/// Signals that a parachain can send to the relay chain via the UMP queue.
#[derive(PartialEq, Eq, Clone, Encode, Decode, TypeInfo, Debug)]
pub enum UMPSignal {
	/// A message sent by a parachain to select the core the candidate is committed to.
	/// Relay chain validators, in particular backers, use the `CoreSelector` and
	/// `ClaimQueueOffset` to compute the index of the core the candidate has committed to.
	SelectCore(CoreSelector, ClaimQueueOffset),
	/// A message sent by a parachain to promote the reputation of a given peerid.
	ApprovedPeer(ApprovedPeerId),
}

/// The default claim queue offset to be used if it's not configured/accessible in the parachain
/// runtime
pub const DEFAULT_CLAIM_QUEUE_OFFSET: u8 = 0;

/// Approved PeerId type. PeerIds in polkadot should typically be 32 bytes long but for identity
/// multihash can go up to 64. Cannot reuse the PeerId type definition from the networking code as
/// it's too generic and extensible.
pub type ApprovedPeerId = BoundedVec<u8, ConstU32<64>>;

#[derive(PartialEq, Eq, Clone, Encode, Decode, TypeInfo, Debug, Default)]
/// User-friendly representation of a candidate's UMP signals.
pub struct CandidateUMPSignals {
	pub(super) select_core: Option<(CoreSelector, ClaimQueueOffset)>,
	pub(super) approved_peer: Option<ApprovedPeerId>,
}

impl CandidateUMPSignals {
	/// Get the core selector UMP signal.
	pub fn core_selector(&self) -> Option<(CoreSelector, ClaimQueueOffset)> {
		self.select_core
	}

	/// Get a reference to the approved peer UMP signal.
	pub fn approved_peer(&self) -> Option<&ApprovedPeerId> {
		self.approved_peer.as_ref()
	}

	/// Returns `true` if UMP signals are empty.
	pub fn is_empty(&self) -> bool {
		self.select_core.is_none() && self.approved_peer.is_none()
	}

	fn try_decode_signal(
		&mut self,
		buffer: &mut impl codec::Input,
	) -> Result<(), CommittedCandidateReceiptError> {
		match UMPSignal::decode(buffer)
			.map_err(|_| CommittedCandidateReceiptError::UmpSignalDecode)?
		{
			UMPSignal::ApprovedPeer(approved_peer_id) if self.approved_peer.is_none() => {
				self.approved_peer = Some(approved_peer_id);
			},
			UMPSignal::SelectCore(core_selector, cq_offset) if self.select_core.is_none() => {
				self.select_core = Some((core_selector, cq_offset));
			},
			_ => {
				// This means that we got duplicate UMP signals.
				return Err(CommittedCandidateReceiptError::DuplicateUMPSignal);
			},
		};

		Ok(())
	}

	#[cfg(feature = "test")]
	#[doc(hidden)]
	pub fn dummy(
		select_core: Option<(CoreSelector, ClaimQueueOffset)>,
		approved_peer: Option<ApprovedPeerId>,
	) -> Self {
		Self { select_core, approved_peer }
	}
}

/// Separator between `XCM` and `UMPSignal`.
pub const UMP_SEPARATOR: Vec<u8> = vec![];

/// Utility function for skipping the ump signals.
pub fn skip_ump_signals<'a>(
	upward_messages: impl Iterator<Item = &'a Vec<u8>>,
) -> impl Iterator<Item = &'a Vec<u8>> {
	upward_messages.take_while(|message| *message != &UMP_SEPARATOR)
}

impl CandidateCommitments {
	/// Returns the ump signals of this candidate, if any, or an error if they violate the expected
	/// format.
	pub fn ump_signals(&self) -> Result<CandidateUMPSignals, CommittedCandidateReceiptError> {
		let mut res = CandidateUMPSignals::default();

		let mut signals_iter =
			self.upward_messages.iter().skip_while(|message| *message != &UMP_SEPARATOR);

		if signals_iter.next().is_none() {
			// No UMP separator
			return Ok(res);
		}

		// Process first signal
		let Some(first_signal) = signals_iter.next() else { return Ok(res) };
		res.try_decode_signal(&mut first_signal.as_slice())?;

		// Process second signal
		let Some(second_signal) = signals_iter.next() else { return Ok(res) };
		res.try_decode_signal(&mut second_signal.as_slice())?;

		// At most two signals are allowed
		if signals_iter.next().is_some() {
			return Err(CommittedCandidateReceiptError::TooManyUMPSignals);
		}

		Ok(res)
	}
}

/// CommittedCandidateReceiptError construction errors.
#[derive(PartialEq, Eq, Clone, Encode, Decode, TypeInfo, Debug)]
#[cfg_attr(feature = "std", derive(thiserror::Error))]
pub enum CommittedCandidateReceiptError {
	/// The specified core index is invalid.
	#[cfg_attr(feature = "std", error("The specified core index is invalid"))]
	InvalidCoreIndex,
	/// The core index in commitments doesn't match the one in descriptor
	#[cfg_attr(
		feature = "std",
		error("The core index in commitments ({commitments:?}) doesn't match the one in descriptor ({descriptor:?})")
	)]
	CoreIndexMismatch {
		/// The core index as found in the descriptor.
		descriptor: CoreIndex,
		/// The core index as found in the commitments.
		commitments: CoreIndex,
	},
	/// The core selector or claim queue offset is invalid.
	#[cfg_attr(feature = "std", error("The core selector or claim queue offset is invalid"))]
	InvalidSelectedCore,
	#[cfg_attr(feature = "std", error("Could not decode UMP signal"))]
	/// Could not decode UMP signal.
	UmpSignalDecode,
	/// The parachain is not assigned to any core at specified claim queue offset.
	#[cfg_attr(
		feature = "std",
		error("The parachain is not assigned to any core at specified claim queue offset")
	)]
	NoAssignment,
	/// Unknown version.
	#[cfg_attr(feature = "std", error("Unknown internal version"))]
	UnknownVersion(u8),
	/// The allowed number of `UMPSignal` messages in the queue was exceeded.
	#[cfg_attr(feature = "std", error("Too many UMP signals"))]
	TooManyUMPSignals,
	/// Duplicated UMP signal.
	#[cfg_attr(feature = "std", error("Duplicate UMP signal"))]
	DuplicateUMPSignal,
	/// If the parachain runtime started sending ump signals, v1 descriptors are no longer
	/// allowed.
	#[cfg_attr(feature = "std", error("Version 1 receipt does not support ump signals"))]
	UMPSignalWithV1Descriptor,
	/// Starting with v3 ump signals are mandatory.
	///
	/// This is to avoid nodes only understanding v1 and v2 to getting tricked
	/// into backing a candidate that looks like a valid v1 to them, but is
	/// actually an invalid v3.
	///
	/// This is prevented by the runtime rejecting v3 candidates without ump
	/// signals. Therefore a candidate that was erroneously backed as v1, while
	/// it actually was a v3 would get rejected by the runtime due to missing
	/// signals, thus preventing the backer from getting slashed. This is given,
	/// because v1 and v2 only nodes would not back a v1 candidate with UMP
	/// signals, as that is seen as invalid by them already.
	#[cfg_attr(feature = "std", error("Version 3 receipt requires ump signals"))]
	NoUMPSignalWithV3Descriptor,
}

impl<H: Copy + AsRef<[u8]>> CommittedCandidateReceiptV2<H> {
	/// Performs checks on the UMP signals and returns them.
	///
	/// Also checks if descriptor core index is equal to the committed core index.
	///
	/// Params:
	/// - `cores_per_para` is a claim queue snapshot at the candidate's relay parent, stored as
	/// a mapping between `ParaId` and the cores assigned per depth.
	///
	/// NOTE: This must only be called in the runtime and backing - never in approval voting nor
	/// disputes! At least not as long as nodes exist which don't understand v3 candidate
	/// descriptors. Not checking there is fine, because it is checked by the runtime - if it can be
	/// disputed, it has been checked already!
	pub fn parse_ump_signals(
		&self,
		cores_per_para: &TransposedClaimQueue,
	) -> Result<CandidateUMPSignals, CommittedCandidateReceiptError> {
		parse_ump_signals_for_commitments(
			&self.commitments,
			self.descriptor.version(),
			cores_per_para,
			self.descriptor.para_id(),
			CoreIndex(self.descriptor.core_index as u32),
		)
	}
}

/// Performs the UMP-signal checks and returns the signals.
///
/// The check is on the commitments: it needs the claim queue plus the three descriptor
/// fields the core-index check consumes, and nothing else from a receipt.
/// [`CommittedCandidateReceiptV2::parse_ump_signals`] is the convenience wrapper for
/// callers that already hold a receipt.
pub fn parse_ump_signals_for_commitments(
	commitments: &CandidateCommitments,
	version: CandidateDescriptorVersion,
	cores_per_para: &TransposedClaimQueue,
	para_id: Id,
	core_index: CoreIndex,
) -> Result<CandidateUMPSignals, CommittedCandidateReceiptError> {
	let signals = commitments.ump_signals()?;

	match version {
		CandidateDescriptorVersion::V1 => {
			// If the parachain runtime started sending ump signals, v1 descriptors are no
			// longer allowed.
			if !signals.is_empty() {
				return Err(CommittedCandidateReceiptError::UMPSignalWithV1Descriptor);
			}

			// Nothing else to check for v1 descriptors.
			return Ok(CandidateUMPSignals::default());
		},
		CandidateDescriptorVersion::V2 => {},
		CandidateDescriptorVersion::Unknown(version_raw) => {
			return Err(CommittedCandidateReceiptError::UnknownVersion(version_raw))
		},
		_ if signals.is_empty() => {
			// V3 and above require UMP signals.
			return Err(CommittedCandidateReceiptError::NoUMPSignalWithV3Descriptor);
		},
		_ => {},
	}

	// Check the core index
	let (maybe_core_index_selector, cq_offset) = signals
		.core_selector()
		.map(|(selector, offset)| (Some(selector), offset))
		.unwrap_or_else(|| (None, ClaimQueueOffset(DEFAULT_CLAIM_QUEUE_OFFSET)));

	check_core_index(cores_per_para, maybe_core_index_selector, cq_offset, para_id, core_index)?;

	// Nothing to further check for the approved peer. If everything passed so far, return the
	// signals.
	Ok(signals)
}

/// Checks if descriptor core index is equal to the committed core index.
/// Input `cores_per_para` is a claim queue snapshot at the candidate's relay parent, stored as
/// a mapping between `ParaId` and the cores assigned per depth.
fn check_core_index(
	cores_per_para: &TransposedClaimQueue,
	maybe_core_index_selector: Option<CoreSelector>,
	cq_offset: ClaimQueueOffset,
	para_id: Id,
	descriptor_core_index: CoreIndex,
) -> Result<(), CommittedCandidateReceiptError> {
	let assigned_cores = cores_per_para
		.get(&para_id)
		.ok_or(CommittedCandidateReceiptError::NoAssignment)?
		.get(&cq_offset.0)
		.ok_or(CommittedCandidateReceiptError::NoAssignment)?;

	if assigned_cores.is_empty() {
		return Err(CommittedCandidateReceiptError::NoAssignment);
	}

	let core_index_selector = if let Some(core_index_selector) = maybe_core_index_selector {
		// We have a committed core selector, we can use it.
		core_index_selector
	} else if assigned_cores.len() > 1 {
		// We got more than one assigned core and no core selector. Special care is needed.
		if !assigned_cores.contains(&descriptor_core_index) {
			// core index in the descriptor is not assigned to the para. Error.
			return Err(CommittedCandidateReceiptError::InvalidCoreIndex);
		} else {
			// the descriptor core index is indeed assigned to the para. This is the most we can
			// check for now
			return Ok(());
		}
	} else {
		// No core selector but there's only one assigned core, use it.
		CoreSelector(0)
	};

	let core_index = assigned_cores
		.iter()
		.nth(core_index_selector.0 as usize % assigned_cores.len())
		.ok_or(CommittedCandidateReceiptError::InvalidSelectedCore)
		.copied()?;

	if core_index != descriptor_core_index {
		return Err(CommittedCandidateReceiptError::CoreIndexMismatch {
			descriptor: descriptor_core_index,
			commitments: core_index,
		});
	}

	Ok(())
}

/// A backed (or backable, depending on context) candidate.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo)]
pub struct BackedCandidate<H = Hash> {
	/// The candidate referred to.
	candidate: CommittedCandidateReceiptV2<H>,
	/// The validity votes themselves, expressed as signatures.
	validity_votes: Vec<ValidityAttestation>,
	/// The indices of the validators within the group, expressed as a bitfield. May be extended
	/// beyond the backing group size to contain the assigned core index, if ElasticScalingMVP is
	/// enabled.
	validator_indices: BitVec<u8, bitvec::order::Lsb0>,
}

/// Parachains inherent-data passed into the runtime by a block author
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Debug, TypeInfo)]
pub struct InherentData<HDR: HeaderT = Header> {
	/// Signed bitfields by validators about availability.
	pub bitfields: UncheckedSignedAvailabilityBitfields,
	/// Backed candidates for inclusion in the block.
	pub backed_candidates: Vec<BackedCandidate<HDR::Hash>>,
	/// Sets of dispute votes for inclusion,
	pub disputes: MultiDisputeStatementSet,
	/// The parent block header. Used for checking state proofs.
	pub parent_header: HDR,
}

impl<H> BackedCandidate<H> {
	/// Constructor
	pub fn new(
		candidate: CommittedCandidateReceiptV2<H>,
		validity_votes: Vec<ValidityAttestation>,
		validator_indices: BitVec<u8, bitvec::order::Lsb0>,
		core_index: CoreIndex,
	) -> Self {
		let mut instance = Self { candidate, validity_votes, validator_indices };
		instance.inject_core_index(core_index);
		instance
	}

	/// Get a reference to the committed candidate receipt of the candidate.
	pub fn candidate(&self) -> &CommittedCandidateReceiptV2<H> {
		&self.candidate
	}

	/// Get a mutable reference to the committed candidate receipt of the candidate.
	/// Only for testing.
	#[cfg(feature = "test")]
	pub fn candidate_mut(&mut self) -> &mut CommittedCandidateReceiptV2<H> {
		&mut self.candidate
	}
	/// Get a reference to the descriptor of the candidate.
	pub fn descriptor(&self) -> &CandidateDescriptorV2<H> {
		&self.candidate.descriptor
	}

	/// Get a mutable reference to the descriptor of the candidate. Only for testing.
	#[cfg(feature = "test")]
	pub fn descriptor_mut(&mut self) -> &mut CandidateDescriptorV2<H> {
		&mut self.candidate.descriptor
	}

	/// Get a reference to the validity votes of the candidate.
	pub fn validity_votes(&self) -> &[ValidityAttestation] {
		&self.validity_votes
	}

	/// Get a mutable reference to validity votes of the para.
	pub fn validity_votes_mut(&mut self) -> &mut Vec<ValidityAttestation> {
		&mut self.validity_votes
	}

	/// Compute this candidate's hash.
	pub fn hash(&self) -> CandidateHash
	where
		H: Clone + Encode,
	{
		self.candidate.to_plain().hash()
	}

	/// Get this candidate's receipt.
	pub fn receipt(&self) -> CandidateReceiptV2<H>
	where
		H: Clone,
	{
		self.candidate.to_plain()
	}

	/// Get a copy of the raw validator indices.
	#[cfg(feature = "test")]
	pub fn raw_validator_indices(&self) -> BitVec<u8, bitvec::order::Lsb0> {
		self.validator_indices.clone()
	}

	/// Get a copy of the validator indices and the assumed core index, if any.
	pub fn validator_indices_and_core_index(
		&self,
	) -> (&BitSlice<u8, bitvec::order::Lsb0>, Option<CoreIndex>) {
		// `BackedCandidate::validity_indices` are extended to store a 8 bit core index.
		let core_idx_offset = self.validator_indices.len().saturating_sub(8);
		if core_idx_offset > 0 {
			let (validator_indices_slice, core_idx_slice) =
				self.validator_indices.split_at(core_idx_offset);
			return (validator_indices_slice, Some(CoreIndex(core_idx_slice.load::<u8>() as u32)));
		}

		(&self.validator_indices, None)
	}

	/// Inject a core index in the validator_indices bitvec.
	fn inject_core_index(&mut self, core_index: CoreIndex) {
		let core_index_to_inject: BitVec<u8, bitvec::order::Lsb0> =
			BitVec::from_vec(vec![core_index.0 as u8]);
		self.validator_indices.extend(core_index_to_inject);
	}

	/// Update the validator indices and core index in the candidate.
	pub fn set_validator_indices_and_core_index(
		&mut self,
		new_indices: BitVec<u8, bitvec::order::Lsb0>,
		maybe_core_index: Option<CoreIndex>,
	) {
		self.validator_indices = new_indices;

		if let Some(core_index) = maybe_core_index {
			self.inject_core_index(core_index);
		}
	}
}

/// Scraped runtime backing votes and resolved disputes.
#[derive(Clone, Encode, Decode, Debug, TypeInfo)]
#[cfg_attr(feature = "std", derive(PartialEq))]
pub struct ScrapedOnChainVotes<H: Encode + Decode = Hash> {
	/// The session in which the block was included.
	pub session: SessionIndex,
	/// Set of backing validators for each candidate, represented by its candidate
	/// receipt.
	pub backing_validators_per_candidate:
		Vec<(CandidateReceiptV2<H>, Vec<(ValidatorIndex, ValidityAttestation)>)>,
	/// On-chain-recorded set of disputes.
	/// Note that the above `backing_validators` are
	/// unrelated to the backers of the disputes candidates.
	pub disputes: MultiDisputeStatementSet,
}

/// Information about a core which is currently occupied.
#[derive(Clone, Encode, Decode, TypeInfo, Debug)]
#[cfg_attr(feature = "std", derive(PartialEq))]
pub struct OccupiedCore<H = Hash, N = BlockNumber> {
	// NOTE: this has no ParaId as it can be deduced from the candidate descriptor.
	/// If this core is freed by availability, this is the assignment that is next up on this
	/// core, if any. None if there is nothing queued for this core.
	pub next_up_on_available: Option<ScheduledCore>,
	/// The relay-chain block number this began occupying the core at.
	pub occupied_since: N,
	/// The relay-chain block this will time-out at, if any.
	pub time_out_at: N,
	/// If this core is freed by being timed-out, this is the assignment that is next up on this
	/// core. None if there is nothing queued for this core or there is no possibility of timing
	/// out.
	pub next_up_on_time_out: Option<ScheduledCore>,
	/// A bitfield with 1 bit for each validator in the set. `1` bits mean that the corresponding
	/// validators has attested to availability on-chain. A 2/3+ majority of `1` bits means that
	/// this will be available.
	pub availability: BitVec<u8, bitvec::order::Lsb0>,
	/// The group assigned to distribute availability pieces of this candidate.
	pub group_responsible: GroupIndex,
	/// The hash of the candidate occupying the core.
	pub candidate_hash: CandidateHash,
	/// The descriptor of the candidate occupying the core.
	pub candidate_descriptor: CandidateDescriptorV2<H>,
}

impl<H, N> OccupiedCore<H, N> {
	/// Get the Para currently occupying this core.
	pub fn para_id(&self) -> Id {
		self.candidate_descriptor.para_id
	}
}

/// The state of a particular availability core.
#[derive(Clone, Encode, Decode, TypeInfo, Debug)]
#[cfg_attr(feature = "std", derive(PartialEq))]
pub enum CoreState<H = Hash, N = BlockNumber> {
	/// The core is currently occupied.
	#[codec(index = 0)]
	Occupied(OccupiedCore<H, N>),
	/// The core is currently free, with a para scheduled and given the opportunity
	/// to occupy.
	///
	/// If a particular Collator is required to author this block, that is also present in this
	/// variant.
	#[codec(index = 1)]
	Scheduled(ScheduledCore),
	/// The core is currently free and there is nothing scheduled. This can be the case for
	/// parathread cores when there are no parathread blocks queued. Parachain cores will never be
	/// left idle.
	#[codec(index = 2)]
	Free,
}

impl<N> CoreState<N> {
	/// Returns the scheduled `ParaId` for the core or `None` if nothing is scheduled.
	///
	/// This function is deprecated. `ClaimQueue` should be used to obtain the scheduled `ParaId`s
	/// for each core.
	#[deprecated(
		note = "`para_id` will be removed. Use `ClaimQueue` to query the scheduled `para_id` instead."
	)]
	pub fn para_id(&self) -> Option<Id> {
		match self {
			Self::Occupied(ref core) => core.next_up_on_available.as_ref().map(|n| n.para_id),
			Self::Scheduled(core) => Some(core.para_id),
			Self::Free => None,
		}
	}

	/// Is this core state `Self::Occupied`?
	pub fn is_occupied(&self) -> bool {
		matches!(self, Self::Occupied(_))
	}
}

/// The claim queue mapped by parachain id.
pub type TransposedClaimQueue = BTreeMap<ParaId, BTreeMap<u8, BTreeSet<CoreIndex>>>;

/// Returns a mapping between the para id and the core indices assigned at different
/// depths in the claim queue.
pub fn transpose_claim_queue(
	claim_queue: BTreeMap<CoreIndex, VecDeque<Id>>,
) -> TransposedClaimQueue {
	let mut per_para_claim_queue = BTreeMap::new();

	for (core, paras) in claim_queue {
		// Iterate paras assigned to this core at each depth.
		for (depth, para) in paras.into_iter().enumerate() {
			let depths: &mut BTreeMap<u8, BTreeSet<CoreIndex>> =
				per_para_claim_queue.entry(para).or_insert_with(|| Default::default());

			depths.entry(depth as u8).or_default().insert(core);
		}
	}

	per_para_claim_queue
}

// Approval Slashes primitives
/// Supercedes the old 'SlashingOffenceKind' enum.
#[derive(PartialEq, Eq, Clone, Copy, Encode, Decode, DecodeWithMemTracking, TypeInfo, Debug)]
pub enum DisputeOffenceKind {
	/// A severe offence when a validator backed an invalid block
	/// (backing only)
	#[codec(index = 0)]
	ForInvalidBacked,
	/// A minor offence when a validator disputed a valid block.
	/// (approval checking and dispute vote only)
	#[codec(index = 1)]
	AgainstValid,
	/// A medium offence when a validator approved an invalid block
	/// (approval checking and dispute vote only)
	#[codec(index = 2)]
	ForInvalidApproved,
}

/// impl for a conversion from SlashingOffenceKind to DisputeOffenceKind
/// This creates DisputeOffenceKind that never contains ForInvalidApproved since it was not
/// supported in the past
impl From<super::v9::slashing::SlashingOffenceKind> for DisputeOffenceKind {
	fn from(value: super::v9::slashing::SlashingOffenceKind) -> Self {
		match value {
			super::v9::slashing::SlashingOffenceKind::ForInvalid => Self::ForInvalidBacked,
			super::v9::slashing::SlashingOffenceKind::AgainstValid => Self::AgainstValid,
		}
	}
}

/// impl for a tryFrom conversion from DisputeOffenceKind to SlashingOffenceKind
impl TryFrom<DisputeOffenceKind> for super::v9::slashing::SlashingOffenceKind {
	type Error = ();

	fn try_from(value: DisputeOffenceKind) -> Result<Self, Self::Error> {
		match value {
			DisputeOffenceKind::ForInvalidBacked => Ok(Self::ForInvalid),
			DisputeOffenceKind::AgainstValid => Ok(Self::AgainstValid),
			DisputeOffenceKind::ForInvalidApproved => Err(()),
		}
	}
}

#[cfg(test)]
/// Basic tests
pub mod tests {
	use super::*;

	#[test]
	fn group_rotation_info_calculations() {
		let info =
			GroupRotationInfo { session_start_block: 10u32, now: 15, group_rotation_frequency: 5 };

		assert_eq!(info.next_rotation_at(), 20);
		assert_eq!(info.last_rotation_at(), 15);
	}

	#[test]
	fn group_for_core_is_core_for_group() {
		for cores in 1..=256 {
			for rotations in 0..(cores * 2) {
				let info = GroupRotationInfo {
					session_start_block: 0u32,
					now: rotations,
					group_rotation_frequency: 1,
				};

				for core in 0..cores {
					let group = info.group_for_core(CoreIndex(core), cores as usize);
					assert_eq!(info.core_for_group(group, cores as usize).0, core);
				}
			}
		}
	}

	#[test]
	fn test_byzantine_threshold() {
		assert_eq!(byzantine_threshold(0), 0);
		assert_eq!(byzantine_threshold(1), 0);
		assert_eq!(byzantine_threshold(2), 0);
		assert_eq!(byzantine_threshold(3), 0);
		assert_eq!(byzantine_threshold(4), 1);
		assert_eq!(byzantine_threshold(5), 1);
		assert_eq!(byzantine_threshold(6), 1);
		assert_eq!(byzantine_threshold(7), 2);
	}

	#[test]
	fn test_supermajority_threshold() {
		assert_eq!(supermajority_threshold(0), 0);
		assert_eq!(supermajority_threshold(1), 1);
		assert_eq!(supermajority_threshold(2), 2);
		assert_eq!(supermajority_threshold(3), 3);
		assert_eq!(supermajority_threshold(4), 3);
		assert_eq!(supermajority_threshold(5), 4);
		assert_eq!(supermajority_threshold(6), 5);
		assert_eq!(supermajority_threshold(7), 5);
	}

	#[test]
	fn balance_bigger_than_usize() {
		let zero_b: Balance = 0;
		let zero_u: usize = 0;

		assert!(zero_b.leading_zeros() >= zero_u.leading_zeros());
	}

	fn make_v2_descriptor() -> CandidateDescriptorV2 {
		CandidateDescriptorV2::new(
			Id::from(1u32),
			Hash::repeat_byte(1),
			CoreIndex(0),
			1,
			Hash::repeat_byte(2),
			Hash::repeat_byte(3),
			Hash::repeat_byte(4),
			Hash::repeat_byte(5),
			ValidationCodeHash::from(Hash::repeat_byte(6)),
		)
	}

	fn make_v3_descriptor() -> CandidateDescriptorV2 {
		CandidateDescriptorV2::new_v3(
			Id::from(1u32),
			Hash::repeat_byte(1),
			CoreIndex(0),
			1, // session_index
			1, // scheduling_session_index
			Hash::repeat_byte(2),
			Hash::repeat_byte(3),
			Hash::repeat_byte(4),
			Hash::repeat_byte(5),
			ValidationCodeHash::from(Hash::repeat_byte(6)),
			Hash::repeat_byte(7), // scheduling_parent
		)
	}

	#[test]
	fn check_version_acceptance_v1_consistent() {
		// A V1 descriptor (created from old-style with non-zero collator fields)
		// Both old and new rules agree → passes regardless of v3_enabled.
		let mut desc = make_v2_descriptor();
		// Put non-zero bytes in first 16 bytes of reserved1 to trigger V1 in both
		// old and new detection.
		desc.reserved1[0] = 0xFF;

		assert_eq!(desc.version(), CandidateDescriptorVersion::V1);
		assert_eq!(desc.version_old_rules(), CandidateDescriptorVersion::V1);
		assert!(desc.check_version_consistency());

		assert!(desc.check_version_acceptance(false).is_ok());
		assert!(desc.check_version_acceptance(true).is_ok());
	}

	#[test]
	fn check_version_acceptance_v2_consistent() {
		// A clean V2 descriptor: both rules agree → passes always.
		let desc = make_v2_descriptor();

		assert_eq!(desc.version(), CandidateDescriptorVersion::V2);
		assert_eq!(desc.version_old_rules(), CandidateDescriptorVersion::V2);
		assert!(desc.check_version_consistency());

		assert!(desc.check_version_acceptance(false).is_ok());
		assert!(desc.check_version_acceptance(true).is_ok());
	}

	#[test]
	fn check_version_acceptance_v3_when_enabled() {
		// V3 descriptor with v3_enabled=true → passes.
		let desc = make_v3_descriptor();

		assert_eq!(desc.version(), CandidateDescriptorVersion::V3);
		assert_eq!(desc.version_old_rules(), CandidateDescriptorVersion::V1);
		assert!(!desc.check_version_consistency());

		assert!(desc.check_version_acceptance(true).is_ok());
	}

	#[test]
	fn check_version_acceptance_v3_when_disabled() {
		// V3 descriptor with v3_enabled=false → rejected.
		// The consistency check fires first (old rules see V1, new rules see V3,
		// and V3 disagreement is not expected when v3_enabled=false).
		let desc = make_v3_descriptor();

		assert_eq!(desc.version(), CandidateDescriptorVersion::V3);
		assert_eq!(
			desc.check_version_acceptance(false),
			Err(CandidateDescriptorVersionCheckError::Inconsistency)
		);
	}

	#[test]
	fn check_version_acceptance_ambiguous_rejected() {
		// Craft descriptor where old rules see V1, new rules see V2.
		// reserved1[16..24] non-zero, reserved1[0..16] all zero, version=0.
		let mut desc = make_v2_descriptor();
		desc.reserved1[16] = 0xFF; // triggers old V1 check but not new

		assert_eq!(desc.version(), CandidateDescriptorVersion::V2);
		assert_eq!(desc.version_old_rules(), CandidateDescriptorVersion::V1);
		assert!(!desc.check_version_consistency());

		// Rejected regardless of v3_enabled.
		assert_eq!(
			desc.check_version_acceptance(false),
			Err(CandidateDescriptorVersionCheckError::Inconsistency)
		);
		assert_eq!(
			desc.check_version_acceptance(true),
			Err(CandidateDescriptorVersionCheckError::Inconsistency)
		);
	}

	#[test]
	fn check_version_consistency_v3_expected_disagreement() {
		// V3 descriptor: version() returns V3, version_old_rules() returns V1.
		// check_version_consistency() is false — but this is expected.
		let desc = make_v3_descriptor();

		assert_eq!(desc.version(), CandidateDescriptorVersion::V3);
		assert_eq!(desc.version_old_rules(), CandidateDescriptorVersion::V1);
		assert!(!desc.check_version_consistency());
		// Accepted when V3 is enabled.
		assert!(desc.check_version_acceptance(true).is_ok());
	}

	#[test]
	fn v3_feature_activation_changes_descriptor_interpretation() {
		let desc = make_v3_descriptor();

		// Sanity: the descriptor IS V3 under new rules but looks like V1 under old rules.
		assert_eq!(desc.version(), CandidateDescriptorVersion::V3);
		assert_eq!(desc.version_old_rules(), CandidateDescriptorVersion::V1);

		// Before V3 activation: descriptor is treated as V1 — relay_parent is used.
		assert_eq!(desc.version_for_candidate_validation(false), CandidateDescriptorVersion::V1,);
		assert_eq!(
			desc.scheduling_parent_for_candidate_validation(false),
			Hash::repeat_byte(1), // relay_parent
		);
		assert_eq!(
			desc.scheduling_session_for_candidate_validation(false),
			None,
			"V1 has no embedded session — must be fetched from runtime",
		);

		// After V3 activation: descriptor is correctly identified as V3.
		assert_eq!(desc.version_for_candidate_validation(true), CandidateDescriptorVersion::V3,);
		assert_eq!(
			desc.scheduling_parent_for_candidate_validation(true),
			Hash::repeat_byte(7), // scheduling_parent
		);
		assert_eq!(
			desc.scheduling_session_for_candidate_validation(true),
			Some(1), // session_index from descriptor, offset=0
		);
	}

	#[test]
	fn check_version_acceptance_ambiguous_scheduling_parent_nonzero() {
		// Descriptor with scheduling_parent non-zero but version=0.
		// Old rules: V1 (scheduling_parent non-zero triggers old_v1_detected).
		// New rules: V2 (only checks reserved1[0..16], which is zero).
		let mut desc = make_v2_descriptor();
		desc.scheduling_parent = Hash::repeat_byte(0xAB);

		assert_eq!(desc.version(), CandidateDescriptorVersion::V2);
		assert_eq!(desc.version_old_rules(), CandidateDescriptorVersion::V1);
		assert!(!desc.check_version_consistency());

		assert_eq!(
			desc.check_version_acceptance(false),
			Err(CandidateDescriptorVersionCheckError::Inconsistency)
		);
		assert_eq!(
			desc.check_version_acceptance(true),
			Err(CandidateDescriptorVersionCheckError::Inconsistency)
		);
	}

	/// Codec index of the `ApprovalCheckingMultipleCandidates` variant (`#[codec(index = 4)]`).
	const APPROVAL_MULTIPLE_CANDIDATES_INDEX: u8 = 4;

	fn coalesced_candidate_hashes(n: usize) -> Vec<CandidateHash> {
		(0..n).map(|i| CandidateHash(Hash::repeat_byte(i as u8))).collect()
	}

	/// SCALE bytes for the `ApprovalCheckingMultipleCandidates` variant with a plain
	/// `Vec<CandidateHash>` body: the codec index byte followed by the `Vec` body (compact length
	/// + elements).
	fn approval_multiple_candidates_vec_bytes(hashes: &[CandidateHash]) -> Vec<u8> {
		let mut bytes = vec![APPROVAL_MULTIPLE_CANDIDATES_INDEX];
		bytes.extend(hashes.encode());
		bytes
	}

	#[test]
	fn approval_multiple_candidates_bounded_vec_encodes_like_vec() {
		let limit = MAX_COALESCE_APPROVALS as usize;
		for n in [0, 1, 2, 8, limit - 1, limit] {
			let hashes = coalesced_candidate_hashes(n);
			let bounded: BoundedVec<CandidateHash, ConstU32<{ MAX_COALESCE_APPROVALS }>> =
				hashes.clone().try_into().expect("n <= MAX_COALESCE_APPROVALS; qed");

			// The inner collections encode to identical bytes.
			assert_eq!(hashes.encode(), bounded.encode(), "inner encoding differs for n = {n}");

			// The full enum encodes identically to a plain `Vec` body.
			assert_eq!(
				ValidDisputeStatementKind::ApprovalCheckingMultipleCandidates(bounded).encode(),
				approval_multiple_candidates_vec_bytes(&hashes),
				"enum encoding differs from the plain Vec encoding for n = {n}",
			);
		}
	}

	#[test]
	fn approval_multiple_candidates_decodes_vec_encoding() {
		let limit = MAX_COALESCE_APPROVALS as usize;
		for n in [0, 1, limit - 1, limit] {
			let hashes = coalesced_candidate_hashes(n);
			let vec_bytes = approval_multiple_candidates_vec_bytes(&hashes);

			// A plain `Vec` encoding decodes into the `BoundedVec`-based variant.
			let decoded = ValidDisputeStatementKind::decode(&mut &vec_bytes[..])
				.expect("a Vec encoding within the limit must decode; qed");
			let expected = ValidDisputeStatementKind::ApprovalCheckingMultipleCandidates(
				hashes.clone().try_into().expect("n <= MAX_COALESCE_APPROVALS; qed"),
			);
			assert_eq!(decoded, expected, "decode mismatch for n = {n}");

			// A full round-trip through the type is stable.
			assert_eq!(
				ValidDisputeStatementKind::decode(&mut &expected.encode()[..]).unwrap(),
				expected,
			);
		}
	}

	#[test]
	fn approval_multiple_candidates_rejects_above_limit() {
		let limit = MAX_COALESCE_APPROVALS as usize;

		// One past the limit is rejected at the length prefix, directly as the bounded collection.
		let too_many = coalesced_candidate_hashes(limit + 1);
		assert!(
			BoundedVec::<CandidateHash, ConstU32<{ MAX_COALESCE_APPROVALS }>>::decode(
				&mut &too_many.encode()[..]
			)
			.is_err(),
			"BoundedVec must reject more than MAX_COALESCE_APPROVALS elements",
		);

		// Through the enum, using a plain `Vec` body carrying one too many candidates.
		let vec_bytes = approval_multiple_candidates_vec_bytes(&too_many);
		assert!(
			ValidDisputeStatementKind::decode(&mut &vec_bytes[..]).is_err(),
			"enum must reject more than MAX_COALESCE_APPROVALS coalesced candidates",
		);

		// The boundary value of exactly MAX_COALESCE_APPROVALS is accepted.
		let at_limit = approval_multiple_candidates_vec_bytes(&coalesced_candidate_hashes(limit));
		assert!(ValidDisputeStatementKind::decode(&mut &at_limit[..]).is_ok());
	}

	#[test]
	fn approval_multiple_candidates_signing_payload_matches_vec() {
		let session = 7;
		for n in [1, 2, MAX_COALESCE_APPROVALS as usize] {
			let hashes = coalesced_candidate_hashes(n);
			let bounded: BoundedVec<CandidateHash, ConstU32<{ MAX_COALESCE_APPROVALS }>> =
				hashes.clone().try_into().expect("n <= MAX_COALESCE_APPROVALS; qed");

			// The signing payload is taken over a slice, so `Vec` and `BoundedVec` produce the
			// same payload bytes for the same candidate set.
			assert_eq!(
				ApprovalVoteMultipleCandidates(&hashes).signing_payload(session),
				ApprovalVoteMultipleCandidates(&bounded).signing_payload(session),
				"signing payload differs for n = {n}",
			);
		}
	}
}
