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

//! Assignment criteria VRF generation and checking interfaces.

use crate::approval::{
	v1::{DelayTranche, RelayVRFStory},
	v2::{AssignmentCertV2, CoreBitfield},
};
use codec::{Decode, Encode};
use polkadot_primitives::{
	AssignmentId, CandidateHash, CoreIndex, GroupIndex, IndexedVec, SessionInfo, ValidatorIndex,
};
use sc_keystore::LocalKeystore;

use std::collections::HashMap;

/// Details pertaining to our assignment on a block.
#[derive(Debug, Clone, Encode, Decode, PartialEq)]
pub struct OurAssignment {
	cert: AssignmentCertV2,
	tranche: DelayTranche,
	validator_index: ValidatorIndex,
	// Whether the assignment has been triggered already.
	triggered: bool,
}

impl OurAssignment {
	/// Create a new `OurAssignment`.
	pub fn new(
		cert: AssignmentCertV2,
		tranche: DelayTranche,
		validator_index: ValidatorIndex,
		triggered: bool,
	) -> Self {
		OurAssignment { cert, tranche, validator_index, triggered }
	}
	/// Returns a reference to the assignment cert.
	pub fn cert(&self) -> &AssignmentCertV2 {
		&self.cert
	}

	/// Returns the assignment cert.
	pub fn into_cert(self) -> AssignmentCertV2 {
		self.cert
	}

	/// Returns the delay tranche of the assignment.
	pub fn tranche(&self) -> DelayTranche {
		self.tranche
	}

	/// Returns the validator index of the assignment.
	pub fn validator_index(&self) -> ValidatorIndex {
		self.validator_index
	}

	/// Returns whether the assignment has been triggered.
	pub fn triggered(&self) -> bool {
		self.triggered
	}

	/// Marks the assignment as triggered.
	pub fn mark_triggered(&mut self) {
		self.triggered = true;
	}
}

/// Information about the world assignments are being produced in.
#[derive(Clone, Debug)]
pub struct Config {
	/// The assignment public keys for validators.
	pub assignment_keys: Vec<AssignmentId>,
	/// The groups of validators assigned to each core.
	pub validator_groups: IndexedVec<GroupIndex, Vec<ValidatorIndex>>,
	/// The number of availability cores used by the protocol during this session.
	pub n_cores: u32,
	/// The zeroth delay tranche width.
	pub zeroth_delay_tranche_width: u32,
	/// The number of samples we do of `relay_vrf_modulo`.
	pub relay_vrf_modulo_samples: u32,
	/// The number of delay tranches in total.
	pub n_delay_tranches: u32,
}

impl<'a> From<&'a SessionInfo> for Config {
	fn from(s: &'a SessionInfo) -> Self {
		Config {
			assignment_keys: s.assignment_keys.clone(),
			validator_groups: s.validator_groups.clone(),
			n_cores: s.n_cores,
			zeroth_delay_tranche_width: s.zeroth_delay_tranche_width,
			relay_vrf_modulo_samples: s.relay_vrf_modulo_samples,
			n_delay_tranches: s.n_delay_tranches,
		}
	}
}

/// A trait for producing and checking assignments.
///
/// Approval voting subsystem implements a a real implemention
/// for it and tests use a mock implementation.
pub trait AssignmentCriteria {
	/// Compute the assignments for the given relay VRF story.
	fn compute_assignments(
		&self,
		keystore: &LocalKeystore,
		relay_vrf_story: RelayVRFStory,
		config: &Config,
		leaving_cores: Vec<(CandidateHash, CoreIndex, GroupIndex)>,
	) -> HashMap<CoreIndex, OurAssignment>;

	/// Check the assignment cert for the given relay VRF story and returns the delay tranche.
	fn check_assignment_cert(
		&self,
		claimed_core_bitfield: CoreBitfield,
		validator_index: ValidatorIndex,
		config: &Config,
		relay_vrf_story: RelayVRFStory,
		assignment: &AssignmentCertV2,
		// Backing groups for each "leaving core".
		backing_groups: Vec<GroupIndex>,
	) -> Result<DelayTranche, InvalidAssignment>;
}

/// Assignment invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidAssignment(pub InvalidAssignmentReason);

impl std::fmt::Display for InvalidAssignment {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		write!(f, "Invalid Assignment: {:?}", self.0)
	}
}

impl std::error::Error for InvalidAssignment {}

/// Failure conditions when checking an assignment cert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidAssignmentReason {
	/// The validator index is out of bounds.
	ValidatorIndexOutOfBounds,
	/// Core index is out of bounds.
	CoreIndexOutOfBounds,
	/// Invalid assignment key.
	InvalidAssignmentKey,
	/// Node is in backing group.
	IsInBackingGroup,
	/// Modulo core index mismatch.
	VRFModuloCoreIndexMismatch,
	/// Modulo output mismatch.
	VRFModuloOutputMismatch,
	/// Delay core index mismatch.
	VRFDelayCoreIndexMismatch,
	/// Delay output mismatch.
	VRFDelayOutputMismatch,
	/// Invalid arguments
	InvalidArguments,
	/// Assignment vrf check resulted in 0 assigned cores.
	NullAssignment,
}
