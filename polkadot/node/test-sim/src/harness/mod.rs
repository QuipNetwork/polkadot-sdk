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

//! The harness layer: Sim struct, observation recorder, query dispatcher.
//!
//! Subsystem-agnostic. Per-subsystem consumers provide a `SubsystemUnderTest` adapter; the
//! harness's `Arc<MockClock>` is passed to the subsystem directly (it impls `Clock`).

pub mod dispatcher;
pub mod observation;
pub mod pending_fetches;
pub mod recorder;
pub mod router;
pub mod sim;

pub use dispatcher::{AnswerQuery, Dispatcher, LayeredResponder};
pub use observation::{Observation, Stamped};
pub use pending_fetches::{PendingFetches, RawResponse};
pub use recorder::Recorder;
pub use router::{RouteAttempt, SubsystemSlot, UutRoute, UutSlot};
pub use sim::{Sim, SimConfig, SubsystemUnderTest};
