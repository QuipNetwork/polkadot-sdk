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

//! Shared world-building primitives. Every scenario boots a [`World`] via one of the
//! `build_*` helpers, then drives the `Sim` through stimuli and assertions on outbound
//! `Effect`s. The fluent surface lives in [`super::world`].

use crate::common::{
	aux::{
		AvailabilityDistributionNoop, AvailabilityStoreStub, CandidateBackingAux, CandidateOutputs,
		CandidateValidationStub, ProspectiveParachainsAux, ProvisionerNoop, SharedInvalidSet,
		StatementDistributionNoop,
	},
	chain::CoreSchedule,
	harness::{LayeredResponder, SubsystemUnderTest},
	responder::PanicResponder,
};
use polkadot_node_subsystem::messages::{AllMessages, CollatorProtocolMessage};
use polkadot_primitives::{CoreIndex, Id as ParaId, ValidatorIndex};
use polkadot_subsystem_test_sim::world_base::{build_chain_model, HasBase, WorldBase, WorldConfig};

/// Collator-flavoured test world. Composes [`WorldBase`] for shared scaffolding (`Sim`,
/// `SharedChain`, leaf bookkeeping) and adds the collator-specific `outputs` registry.
///
/// Scenarios access `Sim` via `world.base.sim`, the chain via `world.base.chain`,
/// leaves via `world.base.leaves`, leaf-derived helpers (`world.leaf()`,
/// `world.ancestors()`) via the [`HasBase`] trait, and direct collator-specific state
/// via `world.outputs`.
pub struct World<S: SubsystemUnderTest>
where
	AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	/// Shared base: `Sim`, `SharedChain`, leaf bookkeeping. Shared with every other
	/// per-tenant `World` across the workspace via [`WorldBase`] / [`HasBase`].
	pub base: WorldBase<S>,
	/// Validation-stub registry: maps a candidate's hash to the
	/// `(commitments, PVD)` the stub returns when the validator validates that candidate.
	pub outputs: CandidateOutputs,
}

impl<S: SubsystemUnderTest> HasBase for World<S>
where
	AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	type Sut = S;
	fn base(&self) -> &WorldBase<Self::Sut> {
		&self.base
	}
	fn base_mut(&mut self) -> &mut WorldBase<Self::Sut> {
		&mut self.base
	}
}

/// Re-export `HasBase` so scenarios' `use ...world::WorldExt` brings trait methods
/// (`new_block`, `signal_active_leaves`, `deactivate_leaf`, `finalize`, `leaf`,
/// `leaf_number`, `ancestors`, `ancestors_of`, config accessors) into scope.
pub use polkadot_subsystem_test_sim::world_base::HasBase as WorldExt;

/// Default [`WorldConfig`] for collator scenarios. Sets:
/// * `session_index = 0` — the whole synthetic chain runs in session 0; collator tests'
///   validator-side infra (group rotation, etc.) was tuned against that.
/// * `validators` = the standard test fixture (Alice, Bob, …).
/// * `validator_groups = [[0, 1]]` — Alice + Bob in one group.
///
/// Tests further nudge fields via the chained [`WorldConfig::with_*`] API.
pub fn collator_world_config() -> WorldConfig {
	WorldConfig::default()
		.with_session_index(0)
		.with_validators(crate::common::builders::fixtures::default_validators())
		.with_validator_groups(vec![vec![ValidatorIndex(0), ValidatorIndex(1)]])
}

/// Bootstrap a `World<S>` from a [`WorldConfig`] and spawn the standard
/// collator-side aux subsystem graph on it. Returns a fully-wired world with no
/// active leaves yet — tests build the chain via `world.new_block().activate()` /
/// `.register()`. The new view is broadcast to the SUT automatically on every
/// `.activate()` via [`SubsystemUnderTest::our_view_change`] (the adapter wraps
/// it as the right `NetworkBridgeUpdate` message).
///
/// `can_second_verdict` controls how the `CandidateBacking::CanSecond` query
/// resolves:
/// * `None` — spawn the real `candidate-backing` subsystem; CanSecond is answered by the production
///   code path against the chain's actual claim-queue + constraints state. The default for almost
///   every scenario.
/// * `Some(true)` / `Some(false)` — replace `candidate-backing` with a stub that answers every
///   `CanSecond` query with the given verdict and drops every other `CandidateBacking` message. Use
///   only when a scenario specifically needs a verdict that real backing would not produce in our
///   minimal chain shape (e.g. forcing a "would-not-second" path that depends on chain state we
///   don't model).
///
/// The standard aux graph: real `prospective-parachains`, real `candidate-backing`
/// (or the `CanSecond` stub above), `CandidateValidationStub::always_valid`, an
/// `AvailabilityStoreStub`, and noop stubs for `statement-distribution`,
/// `provisioner`, and `availability-distribution` (they receive collator-side
/// fan-out but don't drive any contract under test).
pub fn bootstrap_world<S>(config: WorldConfig, can_second_verdict: Option<bool>) -> World<S>
where
	S: SubsystemUnderTest<Message = CollatorProtocolMessage>,
	AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	bootstrap_world_inner(config, can_second_verdict, None)
}

/// [`bootstrap_world`] variant whose validation stub rejects the candidates in
/// `invalid`: real backing then reports them back to the collator protocol as invalid
/// — the production-verdict path for "fetched fine, failed validation" scenarios.
/// Everything not in the set validates exactly as under [`bootstrap_world`].
pub fn bootstrap_world_with_invalid_set<S>(
	config: WorldConfig,
	can_second_verdict: Option<bool>,
	invalid: SharedInvalidSet,
) -> World<S>
where
	S: SubsystemUnderTest<Message = CollatorProtocolMessage>,
	AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	bootstrap_world_inner(config, can_second_verdict, Some(invalid))
}

fn bootstrap_world_inner<S>(
	config: WorldConfig,
	can_second_verdict: Option<bool>,
	invalid_set: Option<SharedInvalidSet>,
) -> World<S>
where
	S: SubsystemUnderTest<Message = CollatorProtocolMessage>,
	AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	let chain = build_chain_model(&config);

	let mut responder = LayeredResponder::new();
	responder.push(chain.clone());
	responder.push(PanicResponder);

	let base = WorldBase::<S>::start_with_responder(responder, chain, config);
	let mut world = World { base, outputs: CandidateOutputs::default() };
	spawn_default_aux(&mut world, can_second_verdict, invalid_set);
	world
}

/// Spawn the standard collator-side aux subsystems on `world.base.sim`. Internal
/// helper for [`bootstrap_world`]; tests don't call it directly.
fn spawn_default_aux<S>(
	world: &mut World<S>,
	can_second_verdict: Option<bool>,
	invalid_set: Option<SharedInvalidSet>,
) where
	S: SubsystemUnderTest<Message = CollatorProtocolMessage>,
	AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	let sim = &mut world.base.sim;
	let (psp, psp_rx) = ProspectiveParachainsAux::spawn(sim);
	sim.register_aux(psp, psp_rx);

	// Either install a CanSecond stub (registered FIRST so it wins the slot order
	// against any later backing aux) or spawn real candidate-backing.
	if let Some(verdict) = can_second_verdict {
		sim.register_aux_slot_only(crate::common::aux::CanSecondStub::new(verdict));
	} else {
		let (cb, cb_rx) = CandidateBackingAux::spawn(sim);
		sim.register_aux(cb, cb_rx);
	}

	let cv = match invalid_set {
		Some(invalid) => CandidateValidationStub::valid_unless(sim, world.outputs.clone(), invalid),
		None => CandidateValidationStub::always_valid(sim, world.outputs.clone()),
	};
	let av = AvailabilityStoreStub::spawn(sim);
	sim.register_aux_slot_only(cv);
	sim.register_aux_slot_only(av);
	sim.register_aux_slot_only(StatementDistributionNoop::new());
	sim.register_aux_slot_only(ProvisionerNoop::new());
	sim.register_aux_slot_only(AvailabilityDistributionNoop::new());
}

/// Convenience: bootstrap + extend a single block + activate it. View update is
/// auto-broadcast by `.activate()` via the adapter's `our_view_change`. Terse path
/// for tests that don't need ancestors or claim-queue overrides.
pub fn activated_world<S>(paras: &[(CoreIndex, ParaId)]) -> World<S>
where
	S: SubsystemUnderTest<Message = CollatorProtocolMessage>,
	AllMessages: From<<S::Message as polkadot_overseer::AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	let mut config = collator_world_config();
	for (core, para) in paras {
		config = config.with_schedule(*core, CoreSchedule::always(*para));
	}
	let mut world = bootstrap_world::<S>(config, None);
	world.new_block().activate();
	world
}
