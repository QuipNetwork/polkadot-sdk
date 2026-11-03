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

//! `CanSecond` stub: answers `CandidateBackingMessage::CanSecond` with a fixed verdict
//! synchronously. Drops all other CandidateBacking messages on the floor. Used in scenarios
//! that need a specific verdict (especially `false`) that real candidate-backing would not
//! produce in our minimal chain shape.

use crate::harness::router::{RouteAttempt, SubsystemSlot};
use futures::future::{ready, BoxFuture};
use polkadot_node_subsystem::{
	messages::{AllMessages, CandidateBackingMessage},
	OverseerSignal,
};

/// Stub that answers `CandidateBackingMessage::CanSecond` with a fixed boolean.
pub struct CanSecondStub {
	verdict: bool,
}

impl CanSecondStub {
	/// New stub returning `verdict` for every CanSecond.
	pub fn new(verdict: bool) -> Self {
		Self { verdict }
	}
}

impl SubsystemSlot for CanSecondStub {
	fn name(&self) -> &'static str {
		"can-second-stub"
	}

	fn send_signal(&self, _signal: OverseerSignal) -> BoxFuture<'static, ()> {
		Box::pin(ready(()))
	}

	fn try_route(&self, msg: AllMessages) -> RouteAttempt {
		match msg {
			AllMessages::CandidateBacking(CandidateBackingMessage::CanSecond(_, tx)) => {
				let _ = tx.send(self.verdict);
				RouteAttempt::Accepted(Box::pin(ready(())))
			},
			AllMessages::CandidateBacking(_) => {
				// Drop other CandidateBacking variants on the floor.
				RouteAttempt::Accepted(Box::pin(ready(())))
			},
			other => RouteAttempt::Declined(other),
		}
	}
}
