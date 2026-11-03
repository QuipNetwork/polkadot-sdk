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

//! Auxiliary slot wiring for the real `prospective-parachains` subsystem, used by
//! collator-protocol scenarios that drive the full hybrid harness (real prospective +
//! real backing wired alongside the SUT).
//!
//! Lives in the collator consumer crate — not in the test-sim core — because pulling the
//! prospective production crate into the test-sim core would form a Cargo dep cycle when
//! prospective-parachains itself wants to dev-dep test-sim for its own scenarios.

use futures::channel::mpsc;
use polkadot_node_core_prospective_parachains::ProspectiveParachainsSubsystem;
use polkadot_node_subsystem::{
	messages::{AllMessages, ProspectiveParachainsMessage},
	overseer::Subsystem,
};
use polkadot_subsystem_test_sim::{
	aux::{spawn_aux, AuxSlot},
	harness::{Sim, SubsystemUnderTest},
};

/// Spawn the real `prospective-parachains` subsystem on `sim`'s executor and return the
/// slot + outbound `AllMessages` receiver. Hand the pair to [`Sim::register_aux`].
///
/// [`Sim::register_aux`]: polkadot_subsystem_test_sim::harness::Sim::register_aux
pub fn spawn_prospective_aux<S: SubsystemUnderTest>(
	sim: &mut Sim<S>,
) -> (AuxSlot<ProspectiveParachainsMessage>, mpsc::UnboundedReceiver<AllMessages>)
where
	AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	spawn_aux(
		sim,
		"prospective-parachains",
		|msg| match msg {
			AllMessages::ProspectiveParachains(inner) => Ok(inner),
			other => Err(other),
		},
		|ctx| ProspectiveParachainsSubsystem::new(Default::default()).start(ctx),
	)
}

/// Compatibility alias preserving the previous `ProspectiveParachainsAux::spawn(...)`
/// callsites in collator scenarios. Calls into [`spawn_prospective_aux`].
pub struct ProspectiveParachainsAux;

impl ProspectiveParachainsAux {
	/// See [`spawn_prospective_aux`].
	pub fn spawn<S: SubsystemUnderTest>(
		sim: &mut Sim<S>,
	) -> (AuxSlot<ProspectiveParachainsMessage>, mpsc::UnboundedReceiver<AllMessages>)
	where
		AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
		AllMessages: From<S::Message>,
	{
		spawn_prospective_aux(sim)
	}
}
