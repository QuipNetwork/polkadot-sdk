// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Cumulus related core primitive types and traits.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use codec::{Compact, Decode, DecodeAll, DecodeWithMemTracking, Encode, MaxEncodedLen};
use polkadot_parachain_primitives::primitives::HeadData;
use scale_info::TypeInfo;
use Debug;

/// The ref time per core in seconds.
///
/// This is the execution time each PoV gets on a core on the relay chain.
pub const REF_TIME_PER_CORE_IN_SECS: u64 = 2;

pub mod parachain_block_data;
pub mod scheduling;

pub use parachain_block_data::ParachainBlockData;
pub use polkadot_core_primitives::InboundDownwardMessage;
pub use polkadot_parachain_primitives::primitives::{
	DmpMessageHandler, Id as ParaId, IsSystem, UpwardMessage, ValidationParams, XcmpMessageFormat,
	XcmpMessageHandler,
};
pub use polkadot_primitives::{
	AbridgedHostConfiguration, AbridgedHrmpChannel, ClaimQueueOffset, CoreSelector,
	PersistedValidationData,
};
pub use scheduling::{
	SchedulingInfoPayload, SchedulingProof, SignedSchedulingInfo, VerifySchedulingSignature,
};
pub use sp_runtime::{
	generic::{Digest, DigestItem},
	traits::Block as BlockT,
	ConsensusEngineId,
};
pub use xcm::latest::prelude::*;

/// A module that re-exports relevant relay chain definitions.
pub mod relay_chain {
	pub use polkadot_core_primitives::*;
	pub use polkadot_primitives::*;
}

/// An inbound HRMP message.
pub type InboundHrmpMessage = polkadot_primitives::InboundHrmpMessage<relay_chain::BlockNumber>;

/// And outbound HRMP message
pub type OutboundHrmpMessage = polkadot_primitives::OutboundHrmpMessage<ParaId>;

/// Error description of a message send failure.
#[derive(Eq, PartialEq, Copy, Clone, Debug, Encode, Decode)]
pub enum MessageSendError {
	/// The dispatch queue is full.
	QueueFull,
	/// There does not exist a channel for sending the message.
	NoChannel,
	/// The message is too big to ever fit in a channel.
	TooBig,
	/// Some other error.
	Other,
	/// There are too many channels open at once.
	TooManyChannels,
}

impl From<MessageSendError> for &'static str {
	fn from(e: MessageSendError) -> Self {
		use MessageSendError::*;
		match e {
			QueueFull => "QueueFull",
			NoChannel => "NoChannel",
			TooBig => "TooBig",
			Other => "Other",
			TooManyChannels => "TooManyChannels",
		}
	}
}

/// The origin of an inbound message.
#[derive(
	Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, Clone, Eq, PartialEq, TypeInfo, Debug,
)]
pub enum AggregateMessageOrigin {
	/// The message came from the para-chain itself.
	Here,
	/// The message came from the relay-chain.
	///
	/// This is used by the DMP queue.
	Parent,
	/// The message came from a sibling para-chain.
	///
	/// This is used by the HRMP queue.
	Sibling(ParaId),
}

impl From<AggregateMessageOrigin> for Location {
	fn from(origin: AggregateMessageOrigin) -> Self {
		match origin {
			AggregateMessageOrigin::Here => Location::here(),
			AggregateMessageOrigin::Parent => Location::parent(),
			AggregateMessageOrigin::Sibling(id) => Location::new(1, Junction::Parachain(id.into())),
		}
	}
}

#[cfg(feature = "runtime-benchmarks")]
impl From<u32> for AggregateMessageOrigin {
	fn from(x: u32) -> Self {
		match x {
			0 => Self::Here,
			1 => Self::Parent,
			p => Self::Sibling(ParaId::from(p)),
		}
	}
}

/// Information about an XCMP channel.
pub struct ChannelInfo {
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
}

pub trait GetChannelInfo {
	fn get_channel_status(id: ParaId) -> ChannelStatus;
	fn get_channel_info(id: ParaId) -> Option<ChannelInfo>;
}

/// List all open outgoing channels.
pub trait ListChannelInfos {
	fn outgoing_channels() -> Vec<ParaId>;
}

/// Something that should be called when sending an upward message.
pub trait UpwardMessageSender {
	/// Send the given UMP message; return the expected number of blocks before the message will
	/// be dispatched or an error if the message cannot be sent.
	/// return the hash of the message sent
	fn send_upward_message(message: UpwardMessage) -> Result<(u32, XcmHash), MessageSendError>;

	/// Pre-check the given UMP message.
	fn can_send_upward_message(message: &UpwardMessage) -> Result<(), MessageSendError>;

	/// Ensure `[Self::send_upward_message]` is successful when called in benchmarks/tests.
	#[cfg(any(feature = "std", feature = "runtime-benchmarks", test))]
	fn ensure_successful_delivery() {}
}

impl UpwardMessageSender for () {
	fn send_upward_message(_message: UpwardMessage) -> Result<(u32, XcmHash), MessageSendError> {
		Err(MessageSendError::NoChannel)
	}

	fn can_send_upward_message(_message: &UpwardMessage) -> Result<(), MessageSendError> {
		Err(MessageSendError::Other)
	}
}

/// The status of a channel.
pub enum ChannelStatus {
	/// Channel doesn't exist/has been closed.
	Closed,
	/// Channel is completely full right now.
	Full,
	/// Channel is ready for sending; the two parameters are the maximum size a valid message may
	/// have right now, and the maximum size a message may ever have (this will generally have been
	/// available during message construction, but it's possible the channel parameters changed in
	/// the meantime).
	Ready(usize, usize),
}

/// A means of figuring out what outbound XCMP messages should be being sent.
pub trait XcmpMessageSource {
	/// Take outbound XCMP messages from the queue.
	///
	/// `excluded_recipients` contains para IDs that must be skipped.
	fn take_outbound_messages(
		maximum_channels: usize,
		excluded_recipients: &[ParaId],
	) -> Vec<(ParaId, Vec<u8>)>;
}

impl XcmpMessageSource for () {
	fn take_outbound_messages(
		_maximum_channels: usize,
		_excluded_recipients: &[ParaId],
	) -> Vec<(ParaId, Vec<u8>)> {
		Vec::new()
	}
}

/// The "quality of service" considerations for message sending.
#[derive(Eq, PartialEq, Clone, Copy, Encode, Decode, Debug)]
pub enum ServiceQuality {
	/// Ensure that this message is dispatched in the same relative order as any other messages
	/// that were also sent with `Ordered`. This only guarantees message ordering on the dispatch
	/// side, and not necessarily on the execution side.
	Ordered,
	/// Ensure that the message is dispatched as soon as possible, which could result in it being
	/// dispatched before other messages which are larger and/or rely on relative ordering.
	Fast,
}

/// A consensus engine ID indicating that this is a Cumulus Parachain.
pub const CUMULUS_CONSENSUS_ID: ConsensusEngineId = *b"CMLS";

/// Information about the core on the relay chain this block will be validated on.
#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub struct CoreInfo {
	/// The selector that determines the actual core at `claim_queue_offset`.
	pub selector: CoreSelector,
	/// The claim queue offset that determines how far "into the future" the core is selected.
	pub claim_queue_offset: ClaimQueueOffset,
	/// The number of cores assigned to the parachain at `claim_queue_offset`.
	pub number_of_cores: Compact<u16>,
}

impl core::hash::Hash for CoreInfo {
	fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
		state.write_u8(self.selector.0);
		state.write_u8(self.claim_queue_offset.0);
		state.write_u16(self.number_of_cores.0);
	}
}

impl CoreInfo {
	/// Puts this into a [`CumulusDigestItem::CoreInfo`] and then encodes it as a Substrate
	/// [`DigestItem`].
	pub fn to_digest_item(&self) -> DigestItem {
		CumulusDigestItem::CoreInfo(self.clone()).to_digest_item()
	}
}

/// Information about a block that is part of a PoV bundle.
#[derive(Clone, Debug, Decode, Encode, PartialEq)]
pub struct BlockBundleInfo {
	/// The index of the block in the bundle.
	pub index: u8,
	/// Is this the last block in the bundle from the point of view of the node?
	///
	/// It is possible that the runtime outputs the
	/// [`CumulusDigestItem::UseFullCore`] to inform the node to use an entire for one block
	/// only.
	pub is_last: bool,
}

impl BlockBundleInfo {
	/// Puts this into a [`CumulusDigestItem::BlockBundleInfo`] and then encodes it as a Substrate
	/// [`DigestItem`].
	pub fn to_digest_item(&self) -> DigestItem {
		CumulusDigestItem::BlockBundleInfo(self.clone()).to_digest_item()
	}
}

/// Return value of [`CumulusDigestItem::core_info_exists_at_max_once`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreInfoExistsAtMaxOnce {
	/// Exists exactly once.
	Once(CoreInfo),
	/// Not found.
	NotFound,
	/// Found more than once.
	MoreThanOnce,
}

/// Identifier for a relay chain block used by [`CumulusDigestItem`].
#[derive(Clone, Debug, PartialEq, Hash, Eq)]
pub enum RelayBlockIdentifier {
	/// The block is identified using its block hash.
	ByHash(relay_chain::Hash),
	/// The block is identified using its storage root and block number.
	ByStorageRoot { storage_root: relay_chain::Hash, block_number: relay_chain::BlockNumber },
}

/// Consensus header digests for Cumulus parachains.
#[derive(Clone, Debug, Decode, Encode, PartialEq)]
pub enum CumulusDigestItem {
	/// A digest item indicating the relay-parent a parachain block was built against.
	#[codec(index = 0)]
	RelayParent(relay_chain::Hash),
	/// A digest item providing information about the core selected on the relay chain for this
	/// block.
	#[codec(index = 1)]
	CoreInfo(CoreInfo),
	/// A digest item providing information about the position of the block in the bundle.
	#[codec(index = 2)]
	BlockBundleInfo(BlockBundleInfo),
	/// A digest item informing the node that this block should be put alone onto a core.
	///
	/// In other words, the core should not be shared with other blocks.
	///
	/// Under certain conditions (mainly runtime misconfigurations) the digest is still set when
	/// there are muliple blocks per core. This is done to communicate to the collator that block
	/// production for this core should be stopped.
	#[codec(index = 3)]
	UseFullCore,
}

impl CumulusDigestItem {
	/// Encode this as a Substrate [`DigestItem`].
	pub fn to_digest_item(&self) -> DigestItem {
		let encoded = self.encode();

		match self {
			Self::RelayParent(_) | Self::UseFullCore => {
				DigestItem::Consensus(CUMULUS_CONSENSUS_ID, encoded)
			},
			_ => DigestItem::PreRuntime(CUMULUS_CONSENSUS_ID, encoded),
		}
	}

	/// Find [`CumulusDigestItem::CoreInfo`] in the given `digest`.
	///
	/// If there are multiple valid digests, this returns the value of the first one.
	pub fn find_core_info(digest: &Digest) -> Option<CoreInfo> {
		digest.convert_first(|d| match d {
			DigestItem::PreRuntime(id, val) if id == &CUMULUS_CONSENSUS_ID => {
				let Ok(CumulusDigestItem::CoreInfo(core_info)) =
					CumulusDigestItem::decode_all(&mut &val[..])
				else {
					return None;
				};

				Some(core_info)
			},
			_ => None,
		})
	}

	/// Returns the found [`CoreInfo`] and iff [`Self::CoreInfo`] exists at max once in the given
	/// `digest`.
	pub fn core_info_exists_at_max_once(digest: &Digest) -> CoreInfoExistsAtMaxOnce {
		let mut core_info = None;
		if digest
			.logs()
			.iter()
			.filter(|l| match l {
				DigestItem::PreRuntime(CUMULUS_CONSENSUS_ID, d) => {
					if let Ok(Self::CoreInfo(ci)) = Self::decode_all(&mut &d[..]) {
						core_info = Some(ci);
						true
					} else {
						false
					}
				},
				_ => false,
			})
			.count() <= 1
		{
			core_info
				.map(CoreInfoExistsAtMaxOnce::Once)
				.unwrap_or(CoreInfoExistsAtMaxOnce::NotFound)
		} else {
			CoreInfoExistsAtMaxOnce::MoreThanOnce
		}
	}

	/// Returns the [`RelayBlockIdentifier`] from the given `digest`.
	///
	/// The identifier corresponds to the relay parent used to build the parachain block.
	pub fn find_relay_block_identifier(digest: &Digest) -> Option<RelayBlockIdentifier> {
		digest.convert_first(|d| match d {
			DigestItem::Consensus(id, val) if id == &CUMULUS_CONSENSUS_ID => {
				let Ok(CumulusDigestItem::RelayParent(hash)) =
					CumulusDigestItem::decode_all(&mut &val[..])
				else {
					return None;
				};

				Some(RelayBlockIdentifier::ByHash(hash))
			},
			DigestItem::Consensus(id, val) if id == &rpsr_digest::RPSR_CONSENSUS_ID => {
				let Ok((storage_root, block_number)) =
					rpsr_digest::RpsrType::decode_all(&mut &val[..])
				else {
					return None;
				};

				Some(RelayBlockIdentifier::ByStorageRoot {
					storage_root,
					block_number: block_number.into(),
				})
			},
			_ => None,
		})
	}

	/// Returns the [`BlockBundleInfo`] from the given `digest`.
	pub fn find_block_bundle_info(digest: &Digest) -> Option<BlockBundleInfo> {
		digest.convert_first(|d| match d {
			DigestItem::PreRuntime(id, val) if id == &CUMULUS_CONSENSUS_ID => {
				let Ok(CumulusDigestItem::BlockBundleInfo(bundle_info)) =
					CumulusDigestItem::decode_all(&mut &val[..])
				else {
					return None;
				};

				Some(bundle_info)
			},
			_ => None,
		})
	}

	/// Returns `true` if the given `digest` contains the [`Self::UseFullCore`] item.
	pub fn contains_use_full_core(digest: &Digest) -> bool {
		digest
			.convert_first(|d| match d {
				DigestItem::Consensus(id, val) if id == &CUMULUS_CONSENSUS_ID => {
					let Ok(CumulusDigestItem::UseFullCore) =
						CumulusDigestItem::decode_all(&mut &val[..])
					else {
						return None;
					};

					Some(true)
				},
				_ => None,
			})
			.unwrap_or_default()
	}

	/// Returns `true` if the given `digest` is from a block that is the last block in a core.
	///
	/// Checks the following conditions:
	///
	/// - Is [`BlockBundleInfo::is_last`] set to true?
	/// - Or is [`Self::UseFullCore`] digest present?
	/// - Or is [`DigestItem::RuntimeEnvironmentUpdated`] digest present?
	///
	/// If any of these conditions is `true`, this function will return `true`.
	///
	/// Returns `None` if the `BlockBundleInfo` digest is not present, which is interpreted as the
	/// associated block is not using block bundling.
	pub fn is_last_block_in_core(digest: &Digest) -> Option<bool> {
		let bundle_info = Self::find_block_bundle_info(digest)?;

		Some(
			bundle_info.is_last ||
				Self::contains_use_full_core(digest) ||
				digest.logs.iter().any(|l| matches!(l, DigestItem::RuntimeEnvironmentUpdated)),
		)
	}
}

/// If there are multiple valid digests, this returns the value of the first one, although
/// well-behaving runtimes should not produce headers with more than one.
pub fn extract_relay_parent(digest: &Digest) -> Option<relay_chain::Hash> {
	digest.convert_first(|d| match d {
		DigestItem::Consensus(id, val) if id == &CUMULUS_CONSENSUS_ID => {
			match CumulusDigestItem::decode(&mut &val[..]) {
				Ok(CumulusDigestItem::RelayParent(hash)) => Some(hash),
				_ => None,
			}
		},
		_ => None,
	})
}

/// Utilities for handling the relay-parent storage root as a digest item.
///
/// This is not intended to be part of the public API, as it is a workaround for
/// <https://github.com/paritytech/cumulus/issues/303> via
/// <https://github.com/paritytech/polkadot/issues/7191>.
///
/// Runtimes using the parachain-system pallet are expected to produce this digest item,
/// but will stop as soon as they are able to provide the relay-parent hash directly.
///
/// The relay-chain storage root is, in practice, a unique identifier of a block
/// in the absence of equivocations (which are slashable). This assumes that the relay chain
/// uses BABE or SASSAFRAS, because the slot and the author's VRF randomness are both included
/// in the relay-chain storage root in both cases.
///
/// Therefore, the relay-parent storage root is a suitable identifier of unique relay chain
/// blocks in low-value scenarios such as performance optimizations.
#[doc(hidden)]
pub mod rpsr_digest {
	use super::{relay_chain, ConsensusEngineId, DecodeAll, Digest, DigestItem, Encode};
	use codec::Compact;

	/// The type used to store the relay-parent storage root and number.
	pub type RpsrType = (relay_chain::Hash, Compact<relay_chain::BlockNumber>);

	/// A consensus engine ID for relay-parent storage root digests.
	pub const RPSR_CONSENSUS_ID: ConsensusEngineId = *b"RPSR";

	/// Construct a digest item for relay-parent storage roots.
	pub fn relay_parent_storage_root_item(
		storage_root: relay_chain::Hash,
		number: impl Into<Compact<relay_chain::BlockNumber>>,
	) -> DigestItem {
		DigestItem::Consensus(
			RPSR_CONSENSUS_ID,
			RpsrType::from((storage_root, number.into())).encode(),
		)
	}

	/// Extract the relay-parent storage root and number from the provided header digest. Returns
	/// `None` if none were found.
	pub fn extract_relay_parent_storage_root(
		digest: &Digest,
	) -> Option<(relay_chain::Hash, relay_chain::BlockNumber)> {
		digest.convert_first(|d| match d {
			DigestItem::Consensus(id, val) if id == &RPSR_CONSENSUS_ID => {
				let (h, n) = RpsrType::decode_all(&mut &val[..]).ok()?;

				Some((h, n.0))
			},
			_ => None,
		})
	}
}

/// Information about a collation.
///
/// This was used in version 1 of the [`CollectCollationInfo`] runtime api.
#[derive(Clone, Debug, codec::Decode, codec::Encode, PartialEq)]
pub struct CollationInfoV1 {
	/// Messages destined to be interpreted by the Relay chain itself.
	pub upward_messages: Vec<UpwardMessage>,
	/// The horizontal messages sent by the parachain.
	pub horizontal_messages: Vec<OutboundHrmpMessage>,
	/// New validation code.
	pub new_validation_code: Option<relay_chain::ValidationCode>,
	/// The number of messages processed from the DMQ.
	pub processed_downward_messages: u32,
	/// The mark which specifies the block number up to which all inbound HRMP messages are
	/// processed.
	pub hrmp_watermark: relay_chain::BlockNumber,
}

impl CollationInfoV1 {
	/// Convert into the latest version of the [`CollationInfo`] struct.
	pub fn into_latest(self, head_data: HeadData) -> CollationInfo {
		CollationInfo {
			upward_messages: self.upward_messages,
			horizontal_messages: self.horizontal_messages,
			new_validation_code: self.new_validation_code,
			processed_downward_messages: self.processed_downward_messages,
			hrmp_watermark: self.hrmp_watermark,
			head_data,
		}
	}
}

/// Information about a collation.
#[derive(Clone, Debug, codec::Decode, codec::Encode, PartialEq, TypeInfo)]
pub struct CollationInfo {
	/// Messages destined to be interpreted by the Relay chain itself.
	pub upward_messages: Vec<UpwardMessage>,
	/// The horizontal messages sent by the parachain.
	pub horizontal_messages: Vec<OutboundHrmpMessage>,
	/// New validation code.
	pub new_validation_code: Option<relay_chain::ValidationCode>,
	/// The number of messages processed from the DMQ.
	pub processed_downward_messages: u32,
	/// The mark which specifies the block number up to which all inbound HRMP messages are
	/// processed.
	pub hrmp_watermark: relay_chain::BlockNumber,
	/// The head data, aka encoded header, of the block that corresponds to the collation.
	pub head_data: HeadData,
}

/// A relay chain storage key to be included in the storage proof.
#[derive(Clone, Debug, Encode, Decode, TypeInfo, PartialEq, Eq)]
pub enum RelayStorageKey {
	/// Top-level relay chain storage key.
	Top(Vec<u8>),
	/// Child trie storage key.
	Child {
		/// Unprefixed storage key identifying the child trie root location.
		/// Prefix `:child_storage:default:` is added when accessing storage.
		/// Used to derive `ChildInfo` for reading child trie data.
		/// Usage: let child_info = ChildInfo::new_default(&storage_key);
		storage_key: Vec<u8>,
		/// Key within the child trie.
		key: Vec<u8>,
	},
}

/// Request for proving relay chain storage data.
///
/// Contains a list of storage keys (either top-level or child trie keys)
/// to be included in the relay chain state proof.
#[derive(Clone, Debug, Encode, Decode, TypeInfo, PartialEq, Eq, Default)]
pub struct RelayProofRequest {
	/// Storage keys to include in the relay chain state proof.
	pub keys: Vec<RelayStorageKey>,
}

sp_api::decl_runtime_apis! {
	/// Runtime api to collect information about a collation.
	///
	/// Version history:
	/// - Version 2: Changed [`Self::collect_collation_info`] signature
	/// - Version 3: Signals to the node to use version 1 of [`ParachainBlockData`].
	#[api_version(3)]
	pub trait CollectCollationInfo {
		/// Collect information about a collation.
		#[changed_in(2)]
		fn collect_collation_info() -> CollationInfoV1;
		/// Collect information about a collation.
		///
		/// The given `header` is the header of the built block for that
		/// we are collecting the collation info for.
		fn collect_collation_info(header: &Block::Header) -> CollationInfo;
	}

	/// Runtime api used to access general info about a parachain runtime.
	pub trait GetParachainInfo {
		/// Retrieve the parachain id used for runtime.
		fn parachain_id() -> ParaId;
  }

	/// API to tell the node side how the relay parent should be chosen and how claim queue
	/// offsets are determined.
	///
	/// A larger relay parent offset indicates that the relay parent should not be the tip of
	/// the relay chain, but `N` blocks behind the tip. This offset is then enforced by the
	/// runtime.
	///
	/// The max claim queue offset determines how far "into the future" collators target when
	/// selecting cores from the claim queue. This provides async backing flexibility while
	/// preventing collators from skipping slots.
	/// See: <https://github.com/paritytech/polkadot-sdk/issues/8893>
	///
	/// WARNING: collators call these methods in the on-chain context, which resolves
	/// `:pending_code`: while an upgrade is pending the answer comes from the new blob, before its
	/// migrations have run. Implementations must NOT read state that a migration changes.
	///
	/// Version history:
	/// - Version 1: Initial version with `relay_parent_offset` only
	/// - Version 2: Added `max_claim_queue_offset` method
	#[api_version(2)]
	pub trait RelayParentOffsetApi {
		/// Fetch the relay parent offset that is expected from the relay chain.
		///
		/// This determines how many blocks behind the relay chain tip the relay parent should be.
		fn relay_parent_offset() -> u32;

		/// Maximum claim queue offset for async backing flexibility.
		///
		/// Bounds how far "into the future" a candidate may look in the claim queue when
		/// selecting a core. The effective claim queue depth depends on the candidate version:
		///
		/// - **V1/V2 candidates**: the claim queue is looked up at the candidate's `relay_parent`,
		///   which is `relay_parent_offset` blocks behind the relay-chain tip. The effective
		///   depth is `relay_parent_offset + max_claim_queue_offset`.
		///
		/// - **V3 candidates**: the claim queue is looked up at the candidate's
		///   `scheduling_parent` — the relay-chain block of the *last finished* slot, decoupled
		///   from the execution-context `relay_parent`. The effective depth is just
		///   `max_claim_queue_offset`.
		///
		/// Collators select a core via an offset in `[0, max_claim_queue_offset]`.
		///
		/// - **V2 candidates**: `max_claim_queue_offset = 1` is sufficient. The claim queue is
		///   looked up at `relay_parent`, which sits behind the tip. Offset 0 covers synchronous
		///   backing in the next relay block; offset 1 covers asynchronous backing in the relay
		///   block after that.
		///
		/// - **V3 candidates**: offset 0 is not reachable — the `scheduling_parent`
		///   is usually the leaf when picked, but its child is already being built, so there is
		///   no opportunity to land in the next relay block. Offset 1 is reachable under
		///   synchronous-backing semantics. For elastic scaling the last block in the bundle is
		///   built near the end of the current slot, which makes offset 1 too tight —
		///   `max_claim_queue_offset = 2` is the minimum cap that keeps elastic scaling viable.
		///
		/// Note: this method was added in `api_version = 2`. Collators calling on runtimes that
		/// only implement `api_version = 1` of [`RelayParentOffsetApi`] will receive an error
		/// and should fall back to a sensible default (current collator defaults: `1` on the
		/// V3 path, `0` on the V1/V2 path).
		///
		/// See: <https://github.com/paritytech/polkadot-sdk/issues/8893>
		#[api_version(2)]
		fn max_claim_queue_offset() -> u8;
	}

	/// API to tell the node side whether V3 scheduling is enabled.
	///
	/// When enabled, collators must produce V3 candidates with:
	/// - ParachainBlockData::V2 containing the scheduling proof
	/// - CandidateDescriptorV3 with scheduling_parent
	///
	/// This is mutually exclusive with relay parent offset (building on older
	/// relay parents). A parachain enables V3 when it wants low-latency block
	/// production with the dual-parent model.
	///
	/// WARNING: collators call this in the on-chain context, which resolves `:pending_code`: while
	/// an upgrade is pending the answer comes from the new blob, before its migrations have run.
	/// Implementations must NOT read state that a migration changes.
	pub trait SchedulingV3EnabledApi {
		/// Returns true if V3 scheduling is enabled for this parachain.
		fn scheduling_v3_enabled() -> bool;
	}

	/// API for parachain target block rate.
	///
	/// This runtime API allows the parachain runtime to communicate the target block rate
	/// to the node side. The target block rate is always valid for the next relay chain slot.
	///
	/// The runtime can not enforce this target block rate. It only acts as a maximum, but not more.
	/// In the end it depends on the collator how many blocks will be produced. If there are no cores
	/// available or the collator is offline, no blocks at all will be produced.
	pub trait TargetBlockRate {
		/// Get the target block rate for this parachain.
		///
		/// Returns the target number of blocks per relay chain slot.
		fn target_block_rate() -> u32;
	}

	/// API for specifying which relay chain storage data to include in storage proofs.
	///
	/// This API allows parachains to request both top-level relay chain storage keys
	/// and child trie storage keys to be included in the relay chain state proof.
	pub trait KeyToIncludeInRelayProof {
		/// Returns relay chain storage proof requests.
		///
		/// The collator will include them in the relay chain proof that is passed alongside the parachain inherent into the runtime.
		fn keys_to_prove() -> RelayProofRequest;
	}
}
