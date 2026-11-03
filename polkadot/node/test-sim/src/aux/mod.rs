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

//! Auxiliary subsystem helpers.
//!
//! - Stubs / noops with no production-crate deps live here in the test-sim core (they're reusable
//!   across tenants).
//! - Real-subsystem spawners (prospective-parachains, candidate-backing) live in their per-tenant
//!   consumer crates. The generic [`spawn_aux`] helper builds the boilerplate so each consumer's
//!   spawn function is a one-liner.
//!
//! [`Sim`]: crate::harness::Sim
//! [`Sim::register_aux`]: crate::harness::Sim::register_aux

pub mod availability_store;
pub mod can_second_stub;
pub mod candidate_validation;
pub mod noop;
pub mod spawn;

pub use availability_store::AvailabilityStoreStub;
pub use can_second_stub::CanSecondStub;
pub use candidate_validation::{
	CandidateOutputs, CandidateValidationStub, SharedInvalidSet, Verdict,
};
pub use noop::{AvailabilityDistributionNoop, ProvisionerNoop, StatementDistributionNoop};
pub use spawn::{spawn_aux, AuxSlot};
