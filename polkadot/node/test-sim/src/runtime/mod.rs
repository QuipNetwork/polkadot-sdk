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

//! Deterministic-runtime primitives: clock, RNG, executor harness.
//!
//! This module is intentionally subsystem-agnostic. When the framework is generalized to a second
//! subsystem, this module can be lifted into a shared crate without modification.

pub mod executor;
pub mod local_spawner;

pub use executor::Executor;
pub use local_spawner::{LocalPoolSpawnDrain, LocalPoolSpawner};
pub use polkadot_node_clock::MockClock;
