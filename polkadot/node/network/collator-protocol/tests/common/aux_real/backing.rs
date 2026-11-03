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

//! Auxiliary slot wiring for the real `candidate-backing` subsystem, used by
//! collator-protocol scenarios that drive the full hybrid harness.
//!
//! Lives in the collator consumer crate — not in the test-sim core — because pulling the
//! backing production crate into the test-sim core would form a Cargo dep cycle when
//! collator's lib already depends transitively in the lib graph (via `AssociateOutgoing`).

use futures::channel::mpsc;
use polkadot_node_core_backing::CandidateBackingSubsystem;
use polkadot_node_subsystem::{
	messages::{AllMessages, CandidateBackingMessage},
	overseer::Subsystem,
};
use polkadot_subsystem_test_sim::{
	aux::{spawn_aux, AuxSlot},
	harness::{Sim, SubsystemUnderTest},
};
use sp_keystore::{Keystore, KeystorePtr};
use std::sync::Arc;

/// Spawn the real `candidate-backing` subsystem on `sim`'s executor and return the
/// slot + outbound `AllMessages` receiver. The keystore is pre-populated with an Alice
/// sr25519 parachain key.
pub fn spawn_backing_aux<S: SubsystemUnderTest>(
	sim: &mut Sim<S>,
) -> (AuxSlot<CandidateBackingMessage>, mpsc::UnboundedReceiver<AllMessages>)
where
	AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	let keystore: KeystorePtr = Arc::new(sc_keystore::LocalKeystore::in_memory());
	Keystore::sr25519_generate_new(
		&*keystore,
		polkadot_primitives::PARACHAIN_KEY_TYPE_ID,
		Some(&sp_keyring::Sr25519Keyring::Alice.to_seed()),
	)
	.expect("keystore accepts inserted key");
	spawn_backing_aux_with_keystore(sim, keystore)
}

/// Spawn with an explicit keystore. Useful for scenarios that need a specific validator
/// public key (e.g. signing as a particular group member).
pub fn spawn_backing_aux_with_keystore<S: SubsystemUnderTest>(
	sim: &mut Sim<S>,
	keystore: KeystorePtr,
) -> (AuxSlot<CandidateBackingMessage>, mpsc::UnboundedReceiver<AllMessages>)
where
	AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	spawn_aux(
		sim,
		"candidate-backing",
		|msg| match msg {
			AllMessages::CandidateBacking(inner) => Ok(inner),
			other => Err(other),
		},
		|ctx| CandidateBackingSubsystem::new(keystore, Default::default()).start(ctx),
	)
}

/// Compatibility alias preserving the previous `CandidateBackingAux::spawn(...)`
/// callsites in collator scenarios.
pub struct CandidateBackingAux;

impl CandidateBackingAux {
	/// See [`spawn_backing_aux`].
	pub fn spawn<S: SubsystemUnderTest>(
		sim: &mut Sim<S>,
	) -> (AuxSlot<CandidateBackingMessage>, mpsc::UnboundedReceiver<AllMessages>)
	where
		AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
		AllMessages: From<S::Message>,
	{
		spawn_backing_aux(sim)
	}

	/// See [`spawn_backing_aux_with_keystore`].
	pub fn spawn_with_keystore<S: SubsystemUnderTest>(
		sim: &mut Sim<S>,
		keystore: KeystorePtr,
	) -> (AuxSlot<CandidateBackingMessage>, mpsc::UnboundedReceiver<AllMessages>)
	where
		AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
		AllMessages: From<S::Message>,
	{
		spawn_backing_aux_with_keystore(sim, keystore)
	}
}
