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

//! Subsystem-agnostic deterministic simulator for testing Polkadot subsystems.
//!
//! Provides the runtime, harness, recorder, classifier, query responders, real auxiliary
//! subsystems, builders, and report rendering used to exercise a *subsystem under test* (SUT)
//! against a controlled chain model and a recorded observable contract.
//!
//! # How a consumer crate wires up
//!
//! 1. Implement [`harness::SubsystemUnderTest`] for the production subsystem's `ProtocolSide` or
//!    equivalent constructor. The impl describes how to spawn the subsystem and how to extract its
//!    inbound message variant from `AllMessages`. Subsystems take `Arc<dyn Clock>`; pass the
//!    harness's `Arc<MockClock>` directly (it impls [`Clock`]).
//! 2. Write scenarios that drive `Sim` through stimuli and assert on outbound [`contract::Effect`]
//!    entries via `Sim::expect` / `expect_no` / `count_effects`.
//!
//! # Why a hand-rolled harness instead of a real `polkadot-overseer`?
//!
//! Considered and rejected. Two load-bearing reasons:
//!
//! 1. **`tokio::time::pause()` does not control `futures_timer::Delay`**, and orchestra's
//!    `TimeoutExt::timeout` plus the overseer's metrics metronome both use `futures_timer::Delay`.
//!    Running the harness on a paused tokio runtime would leave multiple time sources still ticking
//!    against real wall-clock — kills determinism.
//! 2. **Precise quiescence.** `LocalPool::run_until_stalled()` polls every spawned task until each
//!    returns `Pending`. Tokio current_thread has no equivalent; the folklore is `yield_now` loops
//!    or sleeps, both heuristic. Scenario tests that read like specs require deterministic
//!    ordering, not best-effort settling.
//!
//! Subsystem-bench's mock subsystems are real `overseer::Subsystem` impls designed for a
//! multi-thread tokio runtime against a benchmark workload — different problem, not
//! reusable. Malus's `MessageInterceptor` wraps a real subsystem inside a real overseer
//! driven by `polkadot_cli::run_node` — wrong layer entirely.

#![deny(missing_docs)]
#![deny(unused_crate_dependencies)]

pub mod aux;
pub mod builders;
pub mod chain;
pub mod contract;
pub mod harness;
pub mod known_bug;
pub mod report;
pub mod responder;
pub mod runtime;
pub mod world_base;

pub use known_bug::run_known_bug;
pub use polkadot_node_clock::{BoxedDelay, Clock, MockClock};
pub use world_base::{build_chain_model, BlockBuilder, HasBase, LeafRef, WorldBase, WorldConfig};
