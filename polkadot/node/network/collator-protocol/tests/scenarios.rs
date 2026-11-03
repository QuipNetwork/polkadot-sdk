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

//! Test-sim-driven integration tests for `polkadot-collator-protocol`.
//!
//! Single integration target — Cargo's "1 file = 1 binary" rule applies to top-level
//! `tests/*.rs`; subdirectories below `tests/` are referenced via `mod` from this file
//! and compile into the same binary. Saves on link time + keeps `cargo test` output
//! readable.
//!
//! # Topics
//!
//! - `advertisement.rs`        Declare → advertise → accept/reject, spam protection, v1
//!   advertisement-on-non-leaf, AssetHub permissionless.
//! - `fetching.rs`             Fetch queueing, retries, timeouts, fairness, per-RP serialisation.
//! - `seconding.rs`            Full advertise→fetch→respond→second pipelines, fragment-chain
//!   seconding, multi-candidate seconding.
//! - `declare_and_peer.rs`     Declare validation, malicious-para handling, peer-disconnect /
//!   view-change effects.
//! - `scheduling_and_cq.rs`    Per-leaf claim-queue shape, claims counting, group rotation, V3
//!   scheduling-parent semantics.
//! - `segments.rs`             V4 segment advertisements: output-head-keyed fetching, same-head
//!   redundancy across collators (retry-from-second-collator scenarios).
//! - `descriptors.rs`          V1 / V3 descriptor handling: version detection, session-index
//!   checks.
//! - `divergent.rs`            Legacy vs experimental divergences (intended or bug_on-tracked).

#![allow(missing_docs)]

pub use polkadot_collator_protocol_test_sim_macros::sim_test;

mod common;

// `#[sim_test]` expands to `#fn_name::<crate::impls::Legacy/Experimental>(...)`. Mirror
// `crate::impls` here so the macro's hardcoded path resolves without macro changes.
pub use common::impls;

#[path = "scenarios/advertisement.rs"]
mod advertisement;
#[path = "scenarios/declare_and_peer.rs"]
mod declare_and_peer;
#[path = "scenarios/descriptors.rs"]
mod descriptors;
#[path = "scenarios/divergent.rs"]
mod divergent;
#[path = "scenarios/fetching.rs"]
mod fetching;
#[path = "scenarios/scheduling_and_cq.rs"]
mod scheduling_and_cq;
#[path = "scenarios/seconding.rs"]
mod seconding;
#[path = "scenarios/segments.rs"]
mod segments;
