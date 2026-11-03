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

//! Drop-on-floor stubs for subsystem families that the seconding flow emits to but whose
//! replies the collator-protocol does not depend on.
//!
//! Each stub claims a single [`AllMessages`] variant family and silently drops every
//! message it receives. Used for `StatementDistribution`, `Provisioner`, and
//! `AvailabilityDistribution` (the latter is only hit on validator-side seconding when the
//! local validator is *not* the original collator — which doesn't happen in our scenarios).

use crate::harness::router::{RouteAttempt, SubsystemSlot};
use futures::future::{ready, BoxFuture};
use polkadot_node_subsystem::{messages::AllMessages, OverseerSignal};

macro_rules! noop_slot {
	($name:ident, $human:literal, $variant:ident) => {
		#[doc = concat!("Drop-on-floor stub for `", $human, "`. Claims every ", $human, " message and discards it.")]
		pub struct $name;

		impl $name {
			/// Construct the stub. Pass to `Sim::register_aux_slot_only`.
			pub fn new() -> Self {
				Self
			}
		}

		impl Default for $name {
			fn default() -> Self {
				Self::new()
			}
		}

		impl SubsystemSlot for $name {
			fn name(&self) -> &'static str {
				$human
			}

			fn send_signal(&self, _signal: OverseerSignal) -> BoxFuture<'static, ()> {
				Box::pin(ready(()))
			}

			fn try_route(&self, msg: AllMessages) -> RouteAttempt {
				match msg {
					AllMessages::$variant(_) => RouteAttempt::Accepted(Box::pin(ready(()))),
					other => RouteAttempt::Declined(other),
				}
			}
		}
	};
}

noop_slot!(StatementDistributionNoop, "statement-distribution-noop", StatementDistribution);
noop_slot!(ProvisionerNoop, "provisioner-noop", Provisioner);
noop_slot!(
	AvailabilityDistributionNoop,
	"availability-distribution-noop",
	AvailabilityDistribution
);
