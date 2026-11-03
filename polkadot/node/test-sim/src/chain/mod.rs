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

//! In-memory chain model that stands in for `runtime-api` and `chain-api`.
//!
//! Replaces the real `Client<Block>`-backed subsystems with a tiny model the test drives:
//! blocks, sessions, validator groups, and per-block claim queues. Implements the harness's
//! [`AnswerQuery`] surface so it slots into the [`Sim`]'s router as the responder for
//! `Runtime` and `ChainApi` query families.
//!
//! Mutators (`extend`, `set_claim_queue_at`, ...) are explicit: tests advance the chain a
//! block at a time, set the claim queue however they need, and let the responder serve
//! whatever the subsystem-under-test asks. This keeps test scenarios honest about which
//! relay-chain state the subsystem depends on.
//!
//! [`AnswerQuery`]: crate::harness::dispatcher::AnswerQuery
//! [`Sim`]: crate::harness::Sim

pub mod model;

pub use model::{BlockInfo, ChainModel, CoreSchedule, SessionInfo, SharedChain};
