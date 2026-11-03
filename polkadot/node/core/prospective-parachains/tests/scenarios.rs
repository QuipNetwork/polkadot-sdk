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

//! Test-sim-driven integration tests for `polkadot-node-core-prospective-parachains`.
//!
//! Single integration target — Cargo's "1 file = 1 binary" rule applies to top-level
//! `tests/*.rs`; subdirectories below `tests/` are referenced via `mod` from this file
//! and compile into the same binary. Saves on link time + keeps `cargo test` output
//! readable.
//!
//! # Topics
//!
//! - `introduce.rs`           tests of `IntroduceSecondedCandidate` accept/reject behaviour and
//!   idempotence.
//! - `backable_query.rs`      tests of what `GetBackableCandidates` returns under various
//!   fragment-chain shapes, ancestor sets, and counts.
//! - `membership_and_pvd.rs`  tests of read-only queries: `GetHypotheticalMembership` and
//!   `GetProspectiveValidationData`.
//! - `leaf_lifecycle.rs`      `ActiveLeavesUpdate` handling — simple activate/deactivate,
//!   parent-inheritance, implicit-view bound, pending-availability persistence across
//!   RP-out-of-scope, session-boundary ancestry stops.
//! - `older_relay_parent.rs`  V3 candidates whose `relay_parent` is older than the scheduling
//!   lookahead.
//! - `smoke.rs`               framework wiring smoke (spawn + conclude).

mod common;

#[path = "scenarios/backable_query.rs"]
mod backable_query;
#[path = "scenarios/introduce.rs"]
mod introduce;
#[path = "scenarios/leaf_lifecycle.rs"]
mod leaf_lifecycle;
#[path = "scenarios/membership_and_pvd.rs"]
mod membership_and_pvd;
#[path = "scenarios/older_relay_parent.rs"]
mod older_relay_parent;
#[path = "scenarios/smoke.rs"]
mod smoke;
