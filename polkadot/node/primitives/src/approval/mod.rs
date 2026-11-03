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

//! Types relevant for approval.

/// Criteria for assignment.
pub mod criteria;

/// Time utilities for approval voting.
pub mod time;

/// A list of primitives introduced in v1.
pub mod v1 {
	use sp_consensus_babe as babe_primitives;
	pub use sp_application_crypto::sr25519::vrf::{
		VrfPreOutput, VrfProof, VrfSignature, VrfTranscript,
	};
	pub use sp_consensus_babe::{Randomness, Slot};

	use codec::{Decode, Encode};
	use polkadot_primitives::{
		BlockNumber, CandidateHash, CoreIndex, GroupIndex, Hash, Header, SessionIndex,
	};
	/// Validators assigning to check a particular candidate are split up into tranches.
	/// Earlier tranches of validators check first, with later tranches serving as backup.
	pub type DelayTranche = u32;

	/// A static context used to compute the Relay VRF story based on the
	/// VRF output included in the header-chain.
	pub const RELAY_VRF_STORY_CONTEXT: &[u8] = b"A&V RC-VRF";

	/// A static context used for all relay-vrf-modulo VRFs.
	pub const RELAY_VRF_MODULO_CONTEXT: &[u8] = b"A&V MOD";

	/// A static context used for all relay-vrf-modulo VRFs.
	pub const RELAY_VRF_DELAY_CONTEXT: &[u8] = b"A&V DELAY";

	/// A static context used for transcripts indicating assigned availability core.
	pub const ASSIGNED_CORE_CONTEXT: &[u8] = b"A&V ASSIGNED";

	/// A static context associated with producing randomness for a core.
	pub const CORE_RANDOMNESS_CONTEXT: &[u8] = b"A&V CORE";

	/// A static context associated with producing randomness for a tranche.
	pub const TRANCHE_RANDOMNESS_CONTEXT: &[u8] = b"A&V TRANCHE";

	/// random bytes derived from the VRF submitted within the block by the
	/// block author as a credential and used as input to approval assignment criteria.
	#[derive(Debug, Clone, Encode, Decode, PartialEq)]
	pub struct RelayVRFStory(pub [u8; 32]);

	/// Metadata about a block which is now live in the approval protocol.
	#[derive(Debug, Clone)]
	pub struct BlockApprovalMeta {
		/// The hash of the block.
		pub hash: Hash,
		/// The number of the block.
		pub number: BlockNumber,
		/// The hash of the parent block.
		pub parent_hash: Hash,
		/// The candidates included by the block.
		/// Note that these are not the same as the candidates that appear within the block body.
		pub candidates: Vec<(CandidateHash, CoreIndex, GroupIndex)>,
		/// The consensus slot of the block.
		pub slot: Slot,
		/// The session of the block.
		pub session: SessionIndex,
		/// The vrf story.
		pub vrf_story: RelayVRFStory,
	}

	/// Errors that can occur during the approvals protocol.
	#[derive(Debug, thiserror::Error)]
	#[allow(missing_docs)]
	pub enum ApprovalError {
		#[error("Schnorrkel signature error")]
		SchnorrkelSignature(schnorrkel::errors::SignatureError),
		#[error("Authority index {0} out of bounds")]
		AuthorityOutOfBounds(usize),
	}

	/// An unsafe VRF pre-output. Provide BABE Epoch info to create a `RelayVRFStory`.
	pub struct UnsafeVRFPreOutput {
		vrf_output: babe_primitives::VrfOutput,
		slot: Slot,
		authority_index: u32,
	}

	impl UnsafeVRFPreOutput {
		/// Get the slot.
		pub fn slot(&self) -> Slot {
			self.slot
		}

		/// Compute the randomness associated with this VRF output.
		pub fn compute_randomness(
			self,
			_authorities: &[(babe_primitives::AuthorityId, babe_primitives::BabeAuthorityWeight)],
			_randomness: &babe_primitives::Randomness,
			_epoch_index: u64,
		) -> Result<RelayVRFStory, ApprovalError> {
			Ok(RelayVRFStory(
				self.vrf_output.make_bytes(super::v1::RELAY_VRF_STORY_CONTEXT),
			))
		}
	}

	/// Extract the slot number and relay VRF from a header.
	///
	/// This fails if either there is no BABE `PreRuntime` digest or
	/// the digest has type `SecondaryPlain`, which Substrate nodes do
	/// not produce or accept anymore.
	pub fn babe_unsafe_vrf_info(header: &Header) -> Option<UnsafeVRFPreOutput> {
		use babe_primitives::digests::CompatibleDigestItem;

		for digest in &header.digest.logs {
			if let Some(pre) = digest.as_babe_pre_digest() {
				let slot = pre.slot();
				let authority_index = pre.authority_index();

				return pre.vrf_signature().map(|sig| UnsafeVRFPreOutput {
					vrf_output: sig.output(),
					slot,
					authority_index,
				});
			}
		}

		None
	}
}

/// A list of primitives introduced by v2.
pub mod v2 {
	use codec::{Decode, Encode};
	pub use sp_application_crypto::sr25519::vrf::{
		VrfPreOutput, VrfProof, VrfSignature, VrfTranscript,
	};
	pub use sp_consensus_babe::{Randomness, Slot};
	use std::ops::BitOr;

	use bitvec::{prelude::Lsb0, vec::BitVec};
	use polkadot_primitives::{
		CandidateIndex, CoreIndex, Hash, ValidatorIndex, ValidatorSignature,
	};

	/// A static context associated with producing randomness for a core.
	pub const CORE_RANDOMNESS_CONTEXT: &[u8] = b"A&V CORE v2";
	/// A static context associated with producing randomness for v2 multi-core assignments.
	pub const ASSIGNED_CORE_CONTEXT: &[u8] = b"A&V ASSIGNED v2";
	/// A static context used for all relay-vrf-modulo VRFs for v2 multi-core assignments.
	pub const RELAY_VRF_MODULO_CONTEXT: &[u8] = b"A&V MOD v2";
	/// A read-only bitvec wrapper
	#[derive(Clone, Debug, Encode, Decode, Hash, PartialEq, Eq)]
	pub struct Bitfield<T>(BitVec<u8, bitvec::order::Lsb0>, std::marker::PhantomData<T>);

	/// A `read-only`, `non-zero` bitfield.
	/// Each 1 bit identifies a candidate by the bitfield bit index.
	pub type CandidateBitfield = Bitfield<CandidateIndex>;
	/// A bitfield of core assignments.
	pub type CoreBitfield = Bitfield<CoreIndex>;

	/// Errors that can occur when creating and manipulating bitfields.
	#[derive(Debug)]
	pub enum BitfieldError {
		/// All bits are zero.
		NullAssignment,
	}

	/// A bit index in `Bitfield`.
	#[cfg_attr(test, derive(PartialEq, Clone))]
	pub struct BitIndex(pub usize);

	/// Helper trait to convert primitives to `BitIndex`.
	pub trait AsBitIndex {
		/// Returns the index of the corresponding bit in `Bitfield`.
		fn as_bit_index(&self) -> BitIndex;
	}

	impl<T> Bitfield<T> {
		/// Returns the bit value at specified `index`. If `index` is greater than bitfield size,
		/// returns `false`.
		pub fn bit_at(&self, index: BitIndex) -> bool {
			if self.0.len() <= index.0 {
				false
			} else {
				self.0[index.0]
			}
		}

		/// Returns number of bits.
		pub fn len(&self) -> usize {
			self.0.len()
		}

		/// Returns the number of 1 bits.
		pub fn count_ones(&self) -> usize {
			self.0.count_ones()
		}

		/// Returns the index of the first 1 bit.
		pub fn first_one(&self) -> Option<usize> {
			self.0.first_one()
		}

		/// Returns an iterator over inner bits.
		pub fn iter_ones(&self) -> bitvec::slice::IterOnes<'_, u8, bitvec::order::Lsb0> {
			self.0.iter_ones()
		}

		/// For testing purpose, we want a inner mutable ref.
		pub fn inner_mut(&mut self) -> &mut BitVec<u8, bitvec::order::Lsb0> {
			&mut self.0
		}

		/// Returns the inner bitfield and consumes `self`.
		pub fn into_inner(self) -> BitVec<u8, bitvec::order::Lsb0> {
			self.0
		}
	}

	impl AsBitIndex for CandidateIndex {
		fn as_bit_index(&self) -> BitIndex {
			BitIndex(*self as usize)
		}
	}

	impl AsBitIndex for CoreIndex {
		fn as_bit_index(&self) -> BitIndex {
			BitIndex(self.0 as usize)
		}
	}

	impl AsBitIndex for usize {
		fn as_bit_index(&self) -> BitIndex {
			BitIndex(*self)
		}
	}

	impl<T> From<T> for Bitfield<T>
	where
		T: AsBitIndex,
	{
		fn from(value: T) -> Self {
			Self(
				{
					let mut bv = bitvec::bitvec![u8, Lsb0; 0; value.as_bit_index().0 + 1];
					bv.set(value.as_bit_index().0, true);
					bv
				},
				Default::default(),
			)
		}
	}

	impl<T> TryFrom<Vec<T>> for Bitfield<T>
	where
		T: Into<Bitfield<T>>,
	{
		type Error = BitfieldError;

		fn try_from(mut value: Vec<T>) -> Result<Self, Self::Error> {
			if value.is_empty() {
				return Err(BitfieldError::NullAssignment);
			}

			let initial_bitfield =
				value.pop().expect("Just checked above it's not empty; qed").into();

			Ok(Self(
				value.into_iter().fold(initial_bitfield.0, |initial_bitfield, element| {
					let mut bitfield: Bitfield<T> = element.into();
					bitfield
						.0
						.resize(std::cmp::max(initial_bitfield.len(), bitfield.0.len()), false);
					bitfield.0.bitor(initial_bitfield)
				}),
				Default::default(),
			))
		}
	}

	/// The criterion which is claimed to be met by an assignment cert.
	///
	/// Note: SCALE codec index 2 was previously the deprecated non-compact `RelayVRFModulo`
	/// variant and has been removed. Do not reuse index 2 for any future variant.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub enum AssignmentCertKindV2 {
		/// Multiple assignment stories based on the VRF that authorized the relay-chain block
		/// where the candidates were included.
		///
		/// The context is [`super::v2::RELAY_VRF_MODULO_CONTEXT`]
		#[codec(index = 0)]
		RelayVRFModuloCompact {
			/// A bitfield representing the core indices claimed by this assignment.
			core_bitfield: CoreBitfield,
		},
		/// An assignment story based on the VRF that authorized the relay-chain block where the
		/// candidate was included combined with the index of a particular core.
		///
		/// The context is [`super::v1::RELAY_VRF_DELAY_CONTEXT`]
		#[codec(index = 1)]
		RelayVRFDelay {
			/// The core index chosen in this cert.
			core_index: CoreIndex,
		},
	}

	/// A certification of assignment.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub struct AssignmentCertV2 {
		/// The criterion which is claimed to be met by this cert.
		pub kind: AssignmentCertKindV2,
		/// The VRF showing the criterion is met.
		pub vrf: VrfSignature,
	}

	/// An assignment criterion which refers to the candidate under which the assignment is
	/// relevant by block hash.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub struct IndirectAssignmentCertV2 {
		/// A block hash where the candidate appears.
		pub block_hash: Hash,
		/// The validator index.
		pub validator: ValidatorIndex,
		/// The cert itself.
		pub cert: AssignmentCertV2,
	}

	/// A signed approval vote which references the candidate indirectly via the block.
	///
	/// In practice, we have a look-up from block hash and candidate index to candidate hash,
	/// so this can be transformed into a `SignedApprovalVote`.
	#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
	pub struct IndirectSignedApprovalVoteV2 {
		/// A block hash where the candidate appears.
		pub block_hash: Hash,
		/// The index of the candidate in the list of candidates fully included as-of the block.
		pub candidate_indices: CandidateBitfield,
		/// The validator index.
		pub validator: ValidatorIndex,
		/// The signature by the validator.
		pub signature: ValidatorSignature,
	}
}

#[cfg(test)]
mod test {
	use super::v2::{BitIndex, Bitfield};

	use polkadot_primitives::{CandidateIndex, CoreIndex};

	#[test]
	fn test_assignment_bitfield_from_vec() {
		let candidate_indices = vec![1u32, 7, 3, 10, 45, 8, 200, 2];
		let max_index = *candidate_indices.iter().max().unwrap();
		let bitfield = Bitfield::try_from(candidate_indices.clone()).unwrap();
		let candidate_indices =
			candidate_indices.into_iter().map(|i| BitIndex(i as usize)).collect::<Vec<_>>();

		// Test 1 bits.
		for index in candidate_indices.clone() {
			assert!(bitfield.bit_at(index));
		}

		// Test 0 bits.
		for index in 0..max_index {
			if candidate_indices.contains(&BitIndex(index as usize)) {
				continue;
			}
			assert!(!bitfield.bit_at(BitIndex(index as usize)));
		}
	}

	#[test]
	fn test_assignment_bitfield_invariant_msb() {
		let core_indices = vec![CoreIndex(1), CoreIndex(3), CoreIndex(10), CoreIndex(20)];
		let mut bitfield = Bitfield::try_from(core_indices.clone()).unwrap();
		assert!(bitfield.inner_mut().pop().unwrap());

		for i in 0..1024 {
			assert!(Bitfield::try_from(CoreIndex(i)).unwrap().inner_mut().pop().unwrap());
			assert!(Bitfield::try_from(i).unwrap().inner_mut().pop().unwrap());
		}
	}

	#[test]
	fn test_assignment_bitfield_basic() {
		let bitfield = Bitfield::try_from(CoreIndex(0)).unwrap();
		assert!(bitfield.bit_at(BitIndex(0)));
		assert!(!bitfield.bit_at(BitIndex(1)));
		assert_eq!(bitfield.len(), 1);

		let mut bitfield = Bitfield::try_from(20 as CandidateIndex).unwrap();
		assert!(bitfield.bit_at(BitIndex(20)));
		assert_eq!(bitfield.inner_mut().count_ones(), 1);
		assert_eq!(bitfield.len(), 21);
	}

	#[test]
	fn assignment_cert_kind_v2_codec_indices_are_stable() {
		use super::v2::{AssignmentCertKindV2, CoreBitfield};
		use codec::Encode;

		// `RelayVRFModuloCompact` must stay at codec index 0 and `RelayVRFDelay` at index 1.
		// The deprecated non-compact `RelayVRFModulo` (previously index 2) has been removed; this
		// guards against accidentally shifting the surviving wire-format discriminants, which would
		// break decoding of assignments from peers that have not yet upgraded.
		let compact = AssignmentCertKindV2::RelayVRFModuloCompact {
			core_bitfield: CoreBitfield::try_from(vec![CoreIndex(0)]).unwrap(),
		};
		let delay = AssignmentCertKindV2::RelayVRFDelay { core_index: CoreIndex(0) };

		assert_eq!(compact.encode()[0], 0);
		assert_eq!(delay.encode()[0], 1);
	}
}
