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

//! Shared scaffolding for collator-protocol test-sim scenarios:
//! - `aux_real/` — real-subsystem aux spawners (prospective + backing).
//! - `builders/` — wire-frame builder (`peer.rs`).
//! - `harness/` — `CollatorSut` convenience trait alias.
//! - `impls/` — `LegacyValidator` + `ExperimentalValidator` SUT adapters.
//! - `world.rs` — shared `World<S>` + builders (`build_*`) + `ChainConfig`.
//! - `world_helpers.rs` — fluent verbs (advertise, fetch_request, etc.) on `World<S>`.
//!
//! Cargo treats `tests/common/mod.rs` as a non-target subdirectory module — referenced
//! from `tests/scenarios.rs` via `mod common;`.

#![allow(dead_code)]

pub mod aux_real;
pub mod builders;
pub mod harness;
pub mod impls;
pub mod world;
pub mod world_helpers;

// Re-exports preserving the previous `crate::*` paths the scenarios imported.
pub use polkadot_subsystem_test_sim::{chain, contract, responder, runtime};

/// Aux subsystems: stubs/noops from the test-sim core, plus collator-flavoured
/// real-subsystem spawners (prospective + backing).
pub mod aux {
	pub use crate::common::aux_real::{
		backing::CandidateBackingAux, prospective::ProspectiveParachainsAux,
	};
	pub use polkadot_subsystem_test_sim::aux::*;
}
