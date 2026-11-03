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

//! Collator-flavoured wire-frame builders, plus convenience re-exports of the generic
//! chain/candidate builders that live in `polkadot_subsystem_test_sim::builders`. Scenarios
//! import via `crate::common::builders::*` so this module is the single re-export point.

pub mod peer;

pub use peer::{Peer, ProtocolVersion};
pub use polkadot_subsystem_test_sim::builders::{fixtures, Candidate, CandidateBuilder};
