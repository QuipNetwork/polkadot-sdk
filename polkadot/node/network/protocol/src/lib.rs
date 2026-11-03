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

//! Network protocol types for parachains.

#![deny(unused_crate_dependencies)]
#![warn(missing_docs)]

use codec::{Decode, Encode};
use polkadot_primitives::{BlockNumber, Hash};
use std::fmt;

#[doc(hidden)]
pub use sc_network::IfDisconnected;
pub use sc_network_types::PeerId;
#[doc(hidden)]
pub use std::sync::Arc;

mod reputation;
pub use self::reputation::{ReputationChange, UnifiedReputationChange};

/// Peer-sets and protocols used for parachains.
pub mod peer_set;

/// Request/response protocols used in Polkadot.
pub mod request_response;

/// Accessing authority discovery service
pub mod authority_discovery;
/// Grid topology support module
pub mod grid_topology;

/// The minimum amount of peers to send gossip messages to.
pub const MIN_GOSSIP_PEERS: usize = 25;

/// An error indicating that this the over-arching message type had the wrong variant
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WrongVariant;

impl fmt::Display for WrongVariant {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "Wrong message variant")
	}
}

impl std::error::Error for WrongVariant {}

/// The advertised role of a node.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ObservedRole {
	/// A light node.
	Light,
	/// A full node.
	Full,
	/// A node claiming to be an authority (unauthenticated)
	Authority,
}

impl From<sc_network::ObservedRole> for ObservedRole {
	fn from(role: sc_network::ObservedRole) -> ObservedRole {
		match role {
			sc_network::ObservedRole::Light => ObservedRole::Light,
			sc_network::ObservedRole::Authority => ObservedRole::Authority,
			sc_network::ObservedRole::Full => ObservedRole::Full,
		}
	}
}

impl Into<sc_network::ObservedRole> for ObservedRole {
	fn into(self) -> sc_network::ObservedRole {
		match self {
			ObservedRole::Light => sc_network::ObservedRole::Light,
			ObservedRole::Full => sc_network::ObservedRole::Full,
			ObservedRole::Authority => sc_network::ObservedRole::Authority,
		}
	}
}

/// Specialized wrapper around [`View`].
#[derive(Debug, Clone, Default)]
pub struct OurView {
	view: View,
}

impl OurView {
	/// Creates a new instance.
	pub fn new(heads: impl IntoIterator<Item = Hash>, finalized_number: BlockNumber) -> Self {
		let view = View::new(heads, finalized_number);
		Self { view }
	}
}

impl PartialEq for OurView {
	fn eq(&self, other: &Self) -> bool {
		self.view == other.view
	}
}

impl std::ops::Deref for OurView {
	type Target = View;

	fn deref(&self) -> &View {
		&self.view
	}
}

/// Construct a new [`OurView`] with the given chain heads, finalized number 0
///
/// NOTE: Use for tests only.
///
/// # Example
///
/// ```
/// # use polkadot_node_network_protocol::our_view;
/// # use polkadot_primitives::Hash;
/// let our_view = our_view![Hash::repeat_byte(1), Hash::repeat_byte(2)];
/// ```
#[macro_export]
macro_rules! our_view {
	( $( $hash:expr ),* $(,)? ) => {
		$crate::OurView::new(
			vec![ $( $hash.clone() ),* ].into_iter().map(|h| h),
			0,
		)
	};
}

/// A succinct representation of a peer's view. This consists of a bounded amount of chain heads
/// and the highest known finalized block number.
///
/// Up to `N` (5?) chain heads.
#[derive(Default, Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct View {
	/// A bounded amount of chain heads.
	/// Invariant: Sorted.
	heads: Vec<Hash>,
	/// The highest known finalized block number.
	pub finalized_number: BlockNumber,
}

/// Construct a new view with the given chain heads and finalized number 0.
///
/// NOTE: Use for tests only.
///
/// # Example
///
/// ```
/// # use polkadot_node_network_protocol::view;
/// # use polkadot_primitives::Hash;
/// let view = view![Hash::repeat_byte(1), Hash::repeat_byte(2)];
/// ```
#[macro_export]
macro_rules! view {
	( $( $hash:expr ),* $(,)? ) => {
		$crate::View::new(vec![ $( $hash.clone() ),* ], 0)
	};
}

impl View {
	/// Construct a new view based on heads and a finalized block number.
	pub fn new(heads: impl IntoIterator<Item = Hash>, finalized_number: BlockNumber) -> Self {
		let mut heads = heads.into_iter().collect::<Vec<Hash>>();
		heads.sort();
		Self { heads, finalized_number }
	}

	/// Start with no heads, but only a finalized block number.
	pub fn with_finalized(finalized_number: BlockNumber) -> Self {
		Self { heads: Vec::new(), finalized_number }
	}

	/// Obtain the number of heads that are in view.
	pub fn len(&self) -> usize {
		self.heads.len()
	}

	/// Check if the number of heads contained, is null.
	pub fn is_empty(&self) -> bool {
		self.heads.is_empty()
	}

	/// Obtain an iterator over all heads.
	pub fn iter(&self) -> impl Iterator<Item = &Hash> {
		self.heads.iter()
	}

	/// Obtain an iterator over all heads.
	pub fn into_iter(self) -> impl Iterator<Item = Hash> {
		self.heads.into_iter()
	}

	/// Replace `self` with `new`.
	///
	/// Returns an iterator that will yield all elements of `new` that were not part of `self`.
	pub fn replace_difference(&mut self, new: View) -> impl Iterator<Item = &Hash> {
		let old = std::mem::replace(self, new);

		self.heads.iter().filter(move |h| !old.contains(h))
	}

	/// Returns an iterator of the hashes present in `Self` but not in `other`.
	pub fn difference<'a>(&'a self, other: &'a View) -> impl Iterator<Item = &'a Hash> + 'a {
		self.heads.iter().filter(move |h| !other.contains(h))
	}

	/// An iterator containing hashes present in both `Self` and in `other`.
	pub fn intersection<'a>(&'a self, other: &'a View) -> impl Iterator<Item = &'a Hash> + 'a {
		self.heads.iter().filter(move |h| other.contains(h))
	}

	/// Whether the view contains a given hash.
	pub fn contains(&self, hash: &Hash) -> bool {
		self.heads.contains(hash)
	}

	/// Check if two views have the same heads.
	///
	/// Equivalent to the `PartialEq` function,
	/// but ignores the `finalized_number` field.
	pub fn check_heads_eq(&self, other: &Self) -> bool {
		self.heads == other.heads
	}
}

/// A protocol-versioned type for validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationProtocols<V3> {
	/// V3 type.
	V3(V3),
}

/// A protocol-versioned type for collation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollationProtocols<V1, V2, V3, V4> {
	/// V1 type.
	V1(V1),
	/// V2 type.
	V2(V2),
	/// V3 type.
	V3(V3),
	/// V4 type.
	V4(V4),
}

impl<V3: Clone> ValidationProtocols<&'_ V3> {
	/// Convert to a fully-owned version of the message.
	pub fn clone_inner(&self) -> ValidationProtocols<V3> {
		match *self {
			ValidationProtocols::V3(inner) => ValidationProtocols::V3(inner.clone()),
		}
	}
}

impl<V1: Clone, V2: Clone, V3: Clone, V4: Clone>
	CollationProtocols<&'_ V1, &'_ V2, &'_ V3, &'_ V4>
{
	/// Convert to a fully-owned version of the message.
	pub fn clone_inner(&self) -> CollationProtocols<V1, V2, V3, V4> {
		match *self {
			CollationProtocols::V1(inner) => CollationProtocols::V1(inner.clone()),
			CollationProtocols::V2(inner) => CollationProtocols::V2(inner.clone()),
			CollationProtocols::V3(inner) => CollationProtocols::V3(inner.clone()),
			CollationProtocols::V4(inner) => CollationProtocols::V4(inner.clone()),
		}
	}
}

/// All supported versions of the validation protocol message.
pub type VersionedValidationProtocol = ValidationProtocols<v3::ValidationProtocol>;

impl From<v3::ValidationProtocol> for VersionedValidationProtocol {
	fn from(v3: v3::ValidationProtocol) -> Self {
		VersionedValidationProtocol::V3(v3)
	}
}

/// All supported versions of the collation protocol message.
pub type VersionedCollationProtocol = CollationProtocols<
	v1::CollationProtocol,
	v2::CollationProtocol,
	v3_collation::CollationProtocol,
	v4_collation::CollationProtocol,
>;

impl From<v1::CollationProtocol> for VersionedCollationProtocol {
	fn from(v1: v1::CollationProtocol) -> Self {
		VersionedCollationProtocol::V1(v1)
	}
}

impl From<v2::CollationProtocol> for VersionedCollationProtocol {
	fn from(v2: v2::CollationProtocol) -> Self {
		VersionedCollationProtocol::V2(v2)
	}
}

impl From<v3_collation::CollationProtocol> for VersionedCollationProtocol {
	fn from(v3: v3_collation::CollationProtocol) -> Self {
		VersionedCollationProtocol::V3(v3)
	}
}

impl From<v4_collation::CollationProtocol> for VersionedCollationProtocol {
	fn from(v4: v4_collation::CollationProtocol) -> Self {
		VersionedCollationProtocol::V4(v4)
	}
}

macro_rules! impl_versioned_validation_full_protocol_from {
	($from:ty, $out:ty, $variant:ident) => {
		impl From<$from> for $out {
			fn from(versioned_from: $from) -> $out {
				match versioned_from {
					ValidationProtocols::V3(x) => ValidationProtocols::V3(x.into()),
				}
			}
		}
	};
}

macro_rules! impl_versioned_collation_full_protocol_from {
	($from:ty, $out:ty, $variant:ident) => {
		impl From<$from> for $out {
			fn from(versioned_from: $from) -> $out {
				match versioned_from {
					CollationProtocols::V1(x) => CollationProtocols::V1(x.into()),
					CollationProtocols::V2(x) => CollationProtocols::V2(x.into()),
					CollationProtocols::V3(x) => CollationProtocols::V3(x.into()),
					CollationProtocols::V4(x) => CollationProtocols::V4(x.into()),
				}
			}
		}
	};
}

/// Implement `TryFrom` for one versioned validation enum variant into the inner type.
/// `$m_ty::$variant(inner) -> Ok(inner)`
macro_rules! impl_versioned_validation_try_from {
	(
		$from:ty,
		$out:ty,
		$v3_pat:pat => $v3_out:expr
	) => {
		impl TryFrom<$from> for $out {
			type Error = crate::WrongVariant;

			fn try_from(x: $from) -> Result<$out, Self::Error> {
				#[allow(unreachable_patterns)] // when there is only one variant
				match x {
					ValidationProtocols::V3($v3_pat) => Ok(ValidationProtocols::V3($v3_out)),
					_ => Err(crate::WrongVariant),
				}
			}
		}

		impl<'a> TryFrom<&'a $from> for $out {
			type Error = crate::WrongVariant;

			fn try_from(x: &'a $from) -> Result<$out, Self::Error> {
				#[allow(unreachable_patterns)] // when there is only one variant
				match x {
					ValidationProtocols::V3($v3_pat) => {
						Ok(ValidationProtocols::V3($v3_out.clone()))
					},
					_ => Err(crate::WrongVariant),
				}
			}
		}
	};
}

/// Implement `TryFrom` for one versioned collation enum variant into the inner type.
/// `$m_ty::$variant(inner) -> Ok(inner)`
macro_rules! impl_versioned_collation_try_from {
	(
		$from:ty,
		$out:ty,
		$v1_pat:pat => $v1_out:expr,
		$v2_pat:pat => $v2_out:expr,
		$v3_pat:pat => $v3_out:expr,
		$v4_pat:pat => $v4_out:expr
	) => {
		impl TryFrom<$from> for $out {
			type Error = crate::WrongVariant;

			fn try_from(x: $from) -> Result<$out, Self::Error> {
				#[allow(unreachable_patterns)] // when there is only one variant
				match x {
					CollationProtocols::V1($v1_pat) => Ok(CollationProtocols::V1($v1_out)),
					CollationProtocols::V2($v2_pat) => Ok(CollationProtocols::V2($v2_out)),
					CollationProtocols::V3($v3_pat) => Ok(CollationProtocols::V3($v3_out)),
					CollationProtocols::V4($v4_pat) => Ok(CollationProtocols::V4($v4_out)),
					_ => Err(crate::WrongVariant),
				}
			}
		}

		impl<'a> TryFrom<&'a $from> for $out {
			type Error = crate::WrongVariant;

			fn try_from(x: &'a $from) -> Result<$out, Self::Error> {
				#[allow(unreachable_patterns)] // when there is only one variant
				match x {
					CollationProtocols::V1($v1_pat) => Ok(CollationProtocols::V1($v1_out.clone())),
					CollationProtocols::V2($v2_pat) => Ok(CollationProtocols::V2($v2_out.clone())),
					CollationProtocols::V3($v3_pat) => Ok(CollationProtocols::V3($v3_out.clone())),
					CollationProtocols::V4($v4_pat) => Ok(CollationProtocols::V4($v4_out.clone())),
					_ => Err(crate::WrongVariant),
				}
			}
		}
	};
}

/// Version-annotated messages used by the bitfield distribution subsystem.
pub type BitfieldDistributionMessage = ValidationProtocols<v3::BitfieldDistributionMessage>;
impl_versioned_validation_full_protocol_from!(
	BitfieldDistributionMessage,
	VersionedValidationProtocol,
	BitfieldDistribution
);
impl_versioned_validation_try_from!(
	VersionedValidationProtocol,
	BitfieldDistributionMessage,
	v3::ValidationProtocol::BitfieldDistribution(x) => x
);

/// Version-annotated messages used by the statement distribution subsystem.
pub type StatementDistributionMessage = ValidationProtocols<v3::StatementDistributionMessage>;
impl_versioned_validation_full_protocol_from!(
	StatementDistributionMessage,
	VersionedValidationProtocol,
	StatementDistribution
);
impl_versioned_validation_try_from!(
	VersionedValidationProtocol,
	StatementDistributionMessage,
	v3::ValidationProtocol::StatementDistribution(x) => x
);

/// Version-annotated messages used by the approval distribution subsystem.
pub type ApprovalDistributionMessage = ValidationProtocols<v3::ApprovalDistributionMessage>;
impl_versioned_validation_full_protocol_from!(
	ApprovalDistributionMessage,
	VersionedValidationProtocol,
	ApprovalDistribution
);
impl_versioned_validation_try_from!(
	VersionedValidationProtocol,
	ApprovalDistributionMessage,
	v3::ValidationProtocol::ApprovalDistribution(x) => x

);

/// Version-annotated messages used by the gossip-support subsystem (this is void).
pub type GossipSupportNetworkMessage = ValidationProtocols<v3::GossipSupportNetworkMessage>;

// This is a void enum placeholder, so never gets sent over the wire.
impl TryFrom<VersionedValidationProtocol> for GossipSupportNetworkMessage {
	type Error = WrongVariant;
	fn try_from(_: VersionedValidationProtocol) -> Result<Self, Self::Error> {
		Err(WrongVariant)
	}
}

impl<'a> TryFrom<&'a VersionedValidationProtocol> for GossipSupportNetworkMessage {
	type Error = WrongVariant;
	fn try_from(_: &'a VersionedValidationProtocol) -> Result<Self, Self::Error> {
		Err(WrongVariant)
	}
}

/// Version-annotated messages used by the collator protocol subsystem.
pub type CollatorProtocolMessage = CollationProtocols<
	v1::CollatorProtocolMessage,
	v2::CollatorProtocolMessage,
	v3_collation::CollatorProtocolMessage,
	v4_collation::AdvertiseSegment,
>;
impl_versioned_collation_full_protocol_from!(
	CollatorProtocolMessage,
	VersionedCollationProtocol,
	CollatorProtocol
);
impl_versioned_collation_try_from!(
	VersionedCollationProtocol,
	CollatorProtocolMessage,
	v1::CollationProtocol::CollatorProtocol(x) => x,
	v2::CollationProtocol::CollatorProtocol(x) => x,
	v3_collation::CollationProtocol::CollatorProtocol(x) => x,
	x => x
);

/// v1 notification protocol types.
pub mod v1 {
	use codec::{Decode, Encode};

	use polkadot_primitives::{CollatorId, CollatorSignature, Hash, Id as ParaId};

	use polkadot_node_primitives::UncheckedSignedFullStatement;

	/// Network messages used by the collator protocol subsystem
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub enum CollatorProtocolMessage {
		/// Declare the intent to advertise collations under a collator ID, attaching a
		/// signature of the `PeerId` of the node using the given collator ID key.
		#[codec(index = 0)]
		Declare(CollatorId, ParaId, CollatorSignature),
		/// Advertise a collation to a validator. Can only be sent once the peer has
		/// declared that they are a collator with given ID.
		#[codec(index = 1)]
		AdvertiseCollation(Hash),
		/// A collation sent to a validator was seconded.
		#[codec(index = 4)]
		CollationSeconded(Hash, UncheckedSignedFullStatement),
	}

	/// All network messages on the collation peer-set.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq, derive_more::From)]
	pub enum CollationProtocol {
		/// Collator protocol messages
		#[codec(index = 0)]
		#[from]
		CollatorProtocol(CollatorProtocolMessage),
	}

	/// Get the payload that should be signed and included in a `Declare` message.
	///
	/// The payload is the local peer id of the node, which serves to prove that it
	/// controls the collator key it is declaring an intention to collate under.
	pub fn declare_signature_payload(peer_id: &sc_network_types::PeerId) -> Vec<u8> {
		let mut payload = peer_id.to_bytes();
		payload.extend_from_slice(b"COLL");
		payload
	}
}

/// v2 network protocol types.
pub mod v2 {
	use codec::{Decode, Encode};

	use polkadot_primitives::{CandidateHash, CollatorId, CollatorSignature, Hash, Id as ParaId};

	use polkadot_node_primitives::UncheckedSignedFullStatement;

	/// This parts of the protocol did not change from v1, so just alias them in v2.
	pub use super::v1::declare_signature_payload;

	/// Network messages used by the collator protocol subsystem
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub enum CollatorProtocolMessage {
		/// Declare the intent to advertise collations under a collator ID, attaching a
		/// signature of the `PeerId` of the node using the given collator ID key.
		#[codec(index = 0)]
		Declare(CollatorId, ParaId, CollatorSignature),
		/// Advertise a collation to a validator. Can only be sent once the peer has
		/// declared that they are a collator with given ID.
		#[codec(index = 1)]
		AdvertiseCollation {
			/// Hash of the scheduling parent - used for validator assignment.
			scheduling_parent: Hash,
			/// Candidate hash.
			candidate_hash: CandidateHash,
			/// Parachain head data hash before candidate execution.
			parent_head_data_hash: Hash,
		},
		/// A collation sent to a validator was seconded.
		#[codec(index = 4)]
		CollationSeconded(Hash, UncheckedSignedFullStatement),
	}

	/// All network messages on the collation peer-set.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq, derive_more::From)]
	pub enum CollationProtocol {
		/// Collator protocol messages
		#[codec(index = 0)]
		#[from]
		CollatorProtocol(CollatorProtocolMessage),
	}
}

/// v3 collation protocol types.
pub mod v3_collation {
	use codec::{Decode, Encode};

	use polkadot_primitives::{
		CandidateDescriptorVersion, CandidateHash, CollatorId, CollatorSignature, Hash,
		Id as ParaId,
	};

	use polkadot_node_primitives::UncheckedSignedFullStatement;

	/// This part of the protocol did not change from v2, so just alias it in v3.
	pub use super::v2::declare_signature_payload;

	/// Network messages used by the collator protocol subsystem
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub enum CollatorProtocolMessage {
		/// Declare the intent to advertise collations under a collator ID, attaching a
		/// signature of the `PeerId` of the node using the given collator ID key.
		#[codec(index = 0)]
		Declare(CollatorId, ParaId, CollatorSignature),
		/// Advertise a collation to a validator. Can only be sent once the peer has
		/// declared that they are a collator with given ID.
		#[codec(index = 1)]
		AdvertiseCollation {
			/// Hash of the scheduling parent - used for validator assignment.
			/// For non-v3 descriptors, this must be equal to the relay parent.
			scheduling_parent: Hash,
			/// Candidate hash.
			candidate_hash: CandidateHash,
			/// Parachain head data hash before candidate execution.
			parent_head_data_hash: Hash,
			/// The version of the candidate descriptor.
			candidate_descriptor_version: CandidateDescriptorVersion,
			/// The relay parent of the candidate.
			relay_parent: Hash,
		},
		/// A collation sent to a validator was seconded.
		#[codec(index = 4)]
		CollationSeconded(Hash, UncheckedSignedFullStatement),
	}

	/// All network messages on the collation peer-set.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq, derive_more::From)]
	pub enum CollationProtocol {
		/// Collator protocol messages
		#[codec(index = 0)]
		#[from]
		CollatorProtocol(CollatorProtocolMessage),
	}
}

/// v4 collation protocol types.
pub mod v4_collation {
	use codec::{Decode, Encode};
	// Re-exported so external code can name the bound on `AdvertiseSegment::candidates`.
	pub use polkadot_node_primitives::MAX_SEGMENT_LEN;
	use polkadot_primitives::{CandidateDescriptorVersion, Hash, Id as ParaId};
	use sp_runtime::{traits::ConstU32, BoundedVec};

	/// Advertise an ordered list of unincluded candidates. The list
	/// is ordered by age. A length 1 segment is the V3 single-candidate
	/// equivalent.
	///
	/// This is the *only* message on the V4 collation peer-set, so it is a struct
	/// rather than a single-variant enum: there is nothing to discriminate, and a
	/// tag byte would carry no information. If V4 ever gains a second message, this
	/// becomes an enum again and the match sites below regain their inner pattern.
	///
	/// V4 has no `Declare` message. Although every `AdvertiseSegment`
	/// carries a `para_id`, the peer is bound to a single para by its
	/// first advertisement. Sending a different `para_id` in a
	/// subsequent message results in disconnection.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub struct AdvertiseSegment {
		/// Hash of the scheduling parent
		pub scheduling_parent: Hash,
		/// The para this segment collates for.
		pub para_id: ParaId,
		/// Descriptor version for the candidate.
		pub candidates_descriptor_version: CandidateDescriptorVersion,
		/// Candidates ordered by age; the list may have gaps.
		pub candidates: BoundedVec<CandidateFingerprint, ConstU32<MAX_SEGMENT_LEN>>,
	}

	/// A single entry in the segment advertised by the collator.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub struct CandidateFingerprint {
		/// Unique and stable identifier of the underlying parachain
		/// block. Because it's stable across resubmissions this will
		/// be used for deduplication against validator's fragment chain.
		pub output_head_data_hash: Hash,
		/// Parachain head data hash before candidate execution.
		pub parent_head_data_hash: Hash,
		/// The claim queue offset.
		pub claim_queue_offset: u8,
	}

	/// All network messages on the collation peer-set.
	///
	/// V4 carries exactly one message, so the peer-set type is that message itself:
	/// neither a wrapper enum nor a message enum earns its tag byte here.
	pub type CollationProtocol = AdvertiseSegment;
}

/// v3 network protocol types.
/// Purpose is for changing ApprovalDistributionMessage to
/// include more than one assignment and approval in a message.
pub mod v3 {
	use bitvec::{order::Lsb0, slice::BitSlice, vec::BitVec};
	use codec::{Decode, Encode};

	use polkadot_primitives::{
		CandidateHash, GroupIndex, Hash, Id as ParaId, UncheckedSignedAvailabilityBitfield,
		UncheckedSignedStatement,
	};

	use polkadot_node_primitives::approval::v2::{
		CandidateBitfield, IndirectAssignmentCertV2, IndirectSignedApprovalVoteV2,
	};

	/// This parts of the protocol did not change from v2, so just alias them in v3.
	pub use super::v2::declare_signature_payload;

	/// Network messages used by the bitfield distribution subsystem.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub enum BitfieldDistributionMessage {
		/// A signed availability bitfield for a given relay-parent hash.
		#[codec(index = 0)]
		Bitfield(Hash, UncheckedSignedAvailabilityBitfield),
	}

	/// Bitfields indicating the statements that are known or undesired
	/// about a candidate.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub struct StatementFilter {
		/// Seconded statements. '1' is known or undesired.
		pub seconded_in_group: BitVec<u8, Lsb0>,
		/// Valid statements. '1' is known or undesired.
		pub validated_in_group: BitVec<u8, Lsb0>,
	}

	impl StatementFilter {
		/// Create a new blank filter with the given group size.
		pub fn blank(group_size: usize) -> Self {
			StatementFilter {
				seconded_in_group: BitVec::repeat(false, group_size),
				validated_in_group: BitVec::repeat(false, group_size),
			}
		}

		/// Create a new full filter with the given group size.
		pub fn full(group_size: usize) -> Self {
			StatementFilter {
				seconded_in_group: BitVec::repeat(true, group_size),
				validated_in_group: BitVec::repeat(true, group_size),
			}
		}

		/// Whether the filter has a specific expected length, consistent across both
		/// bitfields.
		pub fn has_len(&self, len: usize) -> bool {
			self.seconded_in_group.len() == len && self.validated_in_group.len() == len
		}

		/// Determine the number of backing validators in the statement filter.
		pub fn backing_validators(&self) -> usize {
			self.seconded_in_group
				.iter()
				.by_vals()
				.zip(self.validated_in_group.iter().by_vals())
				.filter(|&(s, v)| s || v) // no double-counting
				.count()
		}

		/// Whether the statement filter has at least one seconded statement.
		pub fn has_seconded(&self) -> bool {
			self.seconded_in_group.iter().by_vals().any(|x| x)
		}

		/// Mask out `Seconded` statements in `self` according to the provided
		/// bitvec. Bits appearing in `mask` will not appear in `self` afterwards.
		pub fn mask_seconded(&mut self, mask: &BitSlice<u8, Lsb0>) {
			for (mut x, mask) in self
				.seconded_in_group
				.iter_mut()
				.zip(mask.iter().by_vals().chain(std::iter::repeat(false)))
			{
				// (x, mask) => x
				// (true, true) => false
				// (true, false) => true
				// (false, true) => false
				// (false, false) => false
				*x = *x && !mask;
			}
		}

		/// Mask out `Valid` statements in `self` according to the provided
		/// bitvec. Bits appearing in `mask` will not appear in `self` afterwards.
		pub fn mask_valid(&mut self, mask: &BitSlice<u8, Lsb0>) {
			for (mut x, mask) in self
				.validated_in_group
				.iter_mut()
				.zip(mask.iter().by_vals().chain(std::iter::repeat(false)))
			{
				// (x, mask) => x
				// (true, true) => false
				// (true, false) => true
				// (false, true) => false
				// (false, false) => false
				*x = *x && !mask;
			}
		}
	}

	/// A manifest of a known backed candidate, along with a description
	/// of the statements backing it.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub struct BackedCandidateManifest {
		/// The scheduling-parent of the candidate.
		pub scheduling_parent: Hash,
		/// The hash of the candidate.
		pub candidate_hash: CandidateHash,
		/// The group index backing the candidate at the scheduling-parent.
		pub group_index: GroupIndex,
		/// The para ID of the candidate. It is illegal for this to
		/// be a para ID which is not assigned to the group indicated
		/// in this manifest.
		pub para_id: ParaId,
		/// The head-data corresponding to the candidate.
		pub parent_head_data_hash: Hash,
		/// A statement filter which indicates which validators in the
		/// para's group at the scheduling-parent have validated this candidate
		/// and issued statements about it, to the advertiser's knowledge.
		///
		/// This MUST have exactly the minimum amount of bytes
		/// necessary to represent the number of validators in the assigned
		/// backing group as-of the scheduling-parent.
		pub statement_knowledge: StatementFilter,
	}

	/// An acknowledgement of a backed candidate being known.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub struct BackedCandidateAcknowledgement {
		/// The hash of the candidate.
		pub candidate_hash: CandidateHash,
		/// A statement filter which indicates which validators in the
		/// para's group at the scheduling-parent have validated this candidate
		/// and issued statements about it, to the advertiser's knowledge.
		///
		/// This MUST have exactly the minimum amount of bytes
		/// necessary to represent the number of validators in the assigned
		/// backing group as-of the scheduling-parent.
		pub statement_knowledge: StatementFilter,
	}

	/// Network messages used by the statement distribution subsystem.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub enum StatementDistributionMessage {
		/// A notification of a signed statement in compact form, for a given relay parent.
		#[codec(index = 0)]
		Statement(Hash, UncheckedSignedStatement),

		/// A notification of a backed candidate being known by the
		/// sending node, for the purpose of being requested by the receiving node
		/// if needed.
		#[codec(index = 1)]
		BackedCandidateManifest(BackedCandidateManifest),

		/// A notification of a backed candidate being known by the sending node,
		/// for the purpose of informing a receiving node which already has the candidate.
		#[codec(index = 2)]
		BackedCandidateKnown(BackedCandidateAcknowledgement),
	}

	/// Network messages used by the approval distribution subsystem.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub enum ApprovalDistributionMessage {
		/// Assignments for candidates in recent, unfinalized blocks.
		/// We use a bitfield to reference claimed candidates, where the bit index is equal to
		/// candidate index.
		///
		/// Actually checking the assignment may yield a different result.
		///
		/// TODO at next protocol upgrade opportunity:
		/// - remove redundancy `candidate_index` vs `core_index`
		/// - `<https://github.com/paritytech/polkadot-sdk/issues/675>`
		#[codec(index = 0)]
		Assignments(Vec<(IndirectAssignmentCertV2, CandidateBitfield)>),
		/// Approvals for candidates in some recent, unfinalized block.
		#[codec(index = 1)]
		Approvals(Vec<IndirectSignedApprovalVoteV2>),
	}

	/// Dummy network message type, so we will receive connect/disconnect events.
	#[derive(Debug, Clone, PartialEq, Eq)]
	pub enum GossipSupportNetworkMessage {}

	/// All network messages on the validation peer-set.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq, derive_more::From)]
	pub enum ValidationProtocol {
		/// Bitfield distribution messages
		#[codec(index = 1)]
		#[from]
		BitfieldDistribution(BitfieldDistributionMessage),
		/// Statement distribution messages
		#[codec(index = 3)]
		#[from]
		StatementDistribution(StatementDistributionMessage),
		/// Approval distribution messages
		#[codec(index = 4)]
		#[from]
		ApprovalDistribution(ApprovalDistributionMessage),
	}
}

/// Returns the subset of `peers` with the specified `version`.
pub fn filter_by_peer_version(
	peers: &[(PeerId, peer_set::ProtocolVersion)],
	version: peer_set::ProtocolVersion,
) -> Vec<PeerId> {
	peers.iter().filter(|(_, v)| v == &version).map(|(p, _)| *p).collect::<Vec<_>>()
}
