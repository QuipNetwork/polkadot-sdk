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

//! `SubsystemUnderTest` adapter for the production
//! [`polkadot_node_core_prospective_parachains::ProspectiveParachainsSubsystem`].

use futures::{future::BoxFuture, FutureExt};
use polkadot_node_core_prospective_parachains::ProspectiveParachainsSubsystem;
use polkadot_node_subsystem::{
	messages::{AllMessages, ProspectiveParachainsMessage},
	overseer::Subsystem,
	SpawnGlue,
};
use polkadot_node_subsystem_test_helpers::TestSubsystemContext;
use polkadot_subsystem_test_sim::{
	harness::SubsystemUnderTest,
	runtime::{LocalPoolSpawner, MockClock},
};
use std::sync::Arc;

/// Adapter for the production `ProspectiveParachainsSubsystem`.
pub struct ProspectiveParachains;

impl SubsystemUnderTest for ProspectiveParachains {
	type Message = ProspectiveParachainsMessage;

	fn spawn(
		ctx: TestSubsystemContext<Self::Message, SpawnGlue<LocalPoolSpawner>>,
		_clock: Arc<MockClock>,
	) -> BoxFuture<'static, ()> {
		// `ProspectiveParachainsSubsystem` takes no clock; its time behaviour is driven
		// entirely by `OverseerSignal::ActiveLeaves` and the chain model. The harness still
		// hands every SUT a `MockClock` for uniformity; we ignore it.
		let subsystem = ProspectiveParachainsSubsystem::new(Default::default());
		let spawned = subsystem.start(ctx);
		spawned.future.map(|_| ()).boxed()
	}

	fn try_extract_inbound(msg: AllMessages) -> Result<Self::Message, AllMessages> {
		match msg {
			AllMessages::ProspectiveParachains(inner) => Ok(inner),
			other => Err(other),
		}
	}
}
