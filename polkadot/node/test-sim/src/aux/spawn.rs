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

//! Generic helper for spawning a production subsystem as an auxiliary slot in the harness.
//!
//! Per-tenant code calls [`spawn_aux`] with the production subsystem's message type, an
//! `AllMessages` extractor, and a builder closure that constructs the subsystem given the
//! test context. Returns a ready-to-register slot + outbound receiver pair:
//!
//! ```ignore
//! let (slot, rx) = spawn_aux(
//!     sim,
//!     "prospective-parachains",
//!     |msg| match msg {
//!         AllMessages::ProspectiveParachains(inner) => Ok(inner),
//!         other => Err(other),
//!     },
//!     |ctx| ProspectiveParachainsSubsystem::new(Default::default()).start(ctx),
//! );
//! sim.register_aux(slot, rx);
//! ```
//!
//! Keeps the test-sim core free of any production-subsystem dependency: each
//! `spawn_aux` invocation is owned by the per-tenant code that brings the production
//! crate as its own dev-dep.

use crate::{
	harness::{
		router::{RouteAttempt, SubsystemSlot},
		Sim, SubsystemUnderTest,
	},
	runtime::LocalPoolSpawner,
};
use futures::{channel::mpsc, future::BoxFuture, FutureExt, SinkExt};
use polkadot_node_subsystem::{
	messages::AllMessages, FromOrchestra, OverseerSignal, SpawnGlue, SpawnedSubsystem,
};
use polkadot_node_subsystem_test_helpers::{make_subsystem_context, TestSubsystemContext};
use polkadot_overseer::AssociateOutgoing;

/// Generic auxiliary slot for an arbitrary subsystem of message type `M`. Built by
/// [`spawn_aux`]; tenants don't construct directly.
pub struct AuxSlot<M: 'static + Send + std::fmt::Debug> {
	name: &'static str,
	inbound_tx: mpsc::Sender<FromOrchestra<M>>,
	extract: fn(AllMessages) -> Result<M, AllMessages>,
}

impl<M: 'static + Send + std::fmt::Debug> SubsystemSlot for AuxSlot<M> {
	fn name(&self) -> &'static str {
		self.name
	}

	fn send_signal(&self, signal: OverseerSignal) -> BoxFuture<'static, ()> {
		let mut tx = self.inbound_tx.clone();
		let name = self.name;
		async move {
			tx.send(FromOrchestra::Signal(signal))
				.await
				.unwrap_or_else(|_| panic!("{} inbound channel still open", name));
		}
		.boxed()
	}

	fn try_route(&self, msg: AllMessages) -> RouteAttempt {
		match (self.extract)(msg) {
			Ok(inner) => {
				let mut tx = self.inbound_tx.clone();
				let name = self.name;
				let fut = async move {
					tx.send(FromOrchestra::Communication { msg: inner })
						.await
						.unwrap_or_else(|_| panic!("{} inbound channel still open", name));
				}
				.boxed();
				RouteAttempt::Accepted(fut)
			},
			Err(other) => RouteAttempt::Declined(other),
		}
	}
}

/// Spawn a production subsystem of message type `M` as an auxiliary slot on `sim`.
///
/// `name` — human-readable for diagnostic output.
///
/// `extract` — extracts `M` from `AllMessages`. Typically a `match` over the relevant
/// `AllMessages` variant. Required because the harness drains a single
/// `mpsc::UnboundedReceiver<AllMessages>` and routes outbound messages to slots that
/// claim them.
///
/// `build` — constructs the subsystem given a `TestSubsystemContext<M, _>` and returns
/// the production crate's `SpawnedSubsystem`. Typically `|ctx| MySubsystem::new(...).start(ctx)`.
pub fn spawn_aux<M, S, B>(
	sim: &mut Sim<S>,
	name: &'static str,
	extract: fn(AllMessages) -> Result<M, AllMessages>,
	build: B,
) -> (AuxSlot<M>, mpsc::UnboundedReceiver<AllMessages>)
where
	M: AssociateOutgoing + std::fmt::Debug + Send + 'static,
	AllMessages: From<<M as AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<M>,
	S: SubsystemUnderTest,
	AllMessages: From<<S::Message as AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
	B: FnOnce(TestSubsystemContext<M, SpawnGlue<LocalPoolSpawner>>) -> SpawnedSubsystem,
{
	let (ctx, handle) = make_subsystem_context::<M, _>(sim.spawner());
	let spawned = build(ctx);
	sim.executor_mut().spawn(spawned.future.map(|_| ()).boxed());
	// Let the subsystem reach its initial parked state.
	sim.executor_mut().poll_until_pending();

	let slot = AuxSlot { name, inbound_tx: handle.tx, extract };
	(slot, handle.rx)
}
