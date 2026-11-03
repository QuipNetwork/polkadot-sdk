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

//! Version 2 of the DB schema.

use codec::{Decode, Encode};
use polkadot_node_primitives::approval::{v1::DelayTranche, v2::AssignmentCertV2};
use polkadot_primitives::{
	BlockNumber, CandidateHash, CandidateReceiptV2 as CandidateReceipt, CoreIndex, GroupIndex,
	Hash, SessionIndex, ValidatorIndex, ValidatorSignature,
};

use sp_consensus_slots::Slot;

use bitvec::{order::Lsb0 as BitOrderLsb0, vec::BitVec};
use std::collections::BTreeMap;

#[cfg(test)]
pub mod tests;

// slot_duration * 2 + DelayTranche gives the number of delay tranches since the
// unix epoch.
#[derive(Encode, Decode, Clone, Copy, Debug, PartialEq)]
pub struct Tick(u64);

/// Convenience type definition
pub type Bitfield = BitVec<u8, BitOrderLsb0>;

/// Details pertaining to our assignment on a block.
#[derive(Encode, Decode, Debug, Clone, PartialEq)]
pub struct OurAssignment {
	/// Our assignment certificate.
	pub cert: AssignmentCertV2,
	/// The tranche for which the assignment refers to.
	pub tranche: DelayTranche,
	/// Our validator index for the session in which the candidates were included.
	pub validator_index: ValidatorIndex,
	/// Whether the assignment has been triggered already.
	pub triggered: bool,
}

/// Metadata regarding a specific tranche of assignments for a specific candidate.
#[derive(Encode, Decode, Debug, Clone, PartialEq)]
pub struct TrancheEntry {
	pub tranche: DelayTranche,
	// Assigned validators, and the instant we received their assignment, rounded
	// to the nearest tick.
	pub assignments: Vec<(ValidatorIndex, Tick)>,
}

/// Metadata regarding approval of a particular candidate within the context of some
/// particular block.
#[derive(Encode, Decode, Debug, Clone, PartialEq)]
pub struct ApprovalEntry {
	pub tranches: Vec<TrancheEntry>,
	pub backing_group: GroupIndex,
	pub our_assignment: Option<OurAssignment>,
	pub our_approval_sig: Option<ValidatorSignature>,
	// `n_validators` bits.
	pub assigned_validators: Bitfield,
	pub approved: bool,
}

/// Metadata regarding approval of a particular candidate.
#[derive(Encode, Decode, Debug, Clone, PartialEq)]
pub struct CandidateEntry {
	pub candidate: CandidateReceipt,
	pub session: SessionIndex,
	// Assignments are based on blocks, so we need to track assignments separately
	// based on the block we are looking at.
	pub block_assignments: BTreeMap<Hash, ApprovalEntry>,
	pub approvals: Bitfield,
}

/// Metadata regarding approval of a particular block, by way of approval of the
/// candidates contained within it.
#[derive(Encode, Decode, Debug, Clone, PartialEq)]
pub struct BlockEntry {
	pub block_hash: Hash,
	pub block_number: BlockNumber,
	pub parent_hash: Hash,
	pub session: SessionIndex,
	pub slot: Slot,
	/// Random bytes derived from the VRF submitted within the block by the block
	/// author as a credential and used as input to approval assignment criteria.
	pub relay_vrf_story: [u8; 32],
	// The candidates included as-of this block and the index of the core they are
	// leaving. Sorted ascending by core index.
	pub candidates: Vec<(CoreIndex, CandidateHash)>,
	// A bitfield where the i'th bit corresponds to the i'th candidate in `candidates`.
	// The i'th bit is `true` iff the candidate has been approved in the context of this
	// block. The block can be considered approved if the bitfield has all bits set to `true`.
	pub approved_bitfield: Bitfield,
	pub children: Vec<Hash>,
	// Assignments we already distributed. A 1 bit means the candidate index for which
	// we already have sent out an assignment. We need this to avoid distributing
	// multiple core assignments more than once.
	pub distributed_assignments: Bitfield,
}

impl From<crate::Tick> for Tick {
	fn from(tick: crate::Tick) -> Tick {
		Tick(tick)
	}
}

impl From<Tick> for crate::Tick {
	fn from(tick: Tick) -> crate::Tick {
		tick.0
	}
}
