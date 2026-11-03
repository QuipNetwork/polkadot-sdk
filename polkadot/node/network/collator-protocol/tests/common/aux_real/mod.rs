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

//! Real-subsystem spawners (prospective-parachains, candidate-backing) implemented via
//! [`polkadot_subsystem_test_sim::aux::spawn_aux`]. Lives here, not in the test-sim
//! core, because pulling these production crates into the core would form a Cargo dep
//! cycle when other subsystems' production crates dev-dep the test-sim core.

pub mod backing;
pub mod prospective;
