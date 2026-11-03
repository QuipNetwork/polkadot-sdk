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

//! Subsystem-agnostic test world base + the [`HasBase`] trait that gives every
//! per-tenant `World` the shared scaffolding (`Sim`, `SharedChain`, leaf bookkeeping)
//! plus a fluent leaf builder.
//!
//! Per-tenant consumer crates compose `WorldBase` as a field of their own `World`,
//! impl [`HasBase`] (two accessor functions), and gain every base method directly:
//!
//! ```ignore
//! pub struct World {
//!     pub base: WorldBase<MySut>,
//!     pub tenant_specific_field: ...,
//! }
//!
//! impl HasBase for World {
//!     type Sut = MySut;
//!     fn base(&self) -> &WorldBase<Self::Sut> { &self.base }
//!     fn base_mut(&mut self) -> &mut WorldBase<Self::Sut> { &mut self.base }
//! }
//!
//! // Tenant scenarios call `world.new_block().with_head_data(...).activate()` —
//! // single fluent builder. Forks via `.from_parent(prev_leaf.hash)`.
//! ```

use crate::{
	chain::{ChainModel, CoreSchedule, SessionInfo, SharedChain},
	harness::{LayeredResponder, Sim, SimConfig, SubsystemUnderTest},
	responder::PanicResponder,
};
use polkadot_node_subsystem::{messages::AllMessages, ActiveLeavesUpdate, OverseerSignal};
use polkadot_node_subsystem_test_helpers::mock::new_leaf;
use polkadot_overseer::AssociateOutgoing;
use polkadot_primitives::{
	async_backing::{CandidatePendingAvailability, Constraints, InboundHrmpLimitations},
	BlockNumber, CommittedCandidateReceiptV2 as CommittedCandidateReceipt, CoreIndex, Hash,
	HeadData, Id as ParaId, SessionIndex, ValidationCodeHash, ValidatorId, ValidatorIndex,
	DEFAULT_SCHEDULING_LOOKAHEAD,
};
use polkadot_primitives_test_helpers::dummy_validation_code;
use sp_consensus_slots::Slot;
use std::collections::{BTreeMap, VecDeque};

/// Identity of a leaf the harness has signalled `ActiveLeaves::start_work` for. Returned
/// from [`BlockBuilder::activate`] / [`BlockBuilder::register`] and held by tests.
#[derive(Clone, Copy, Debug)]
pub struct LeafRef {
	/// Leaf hash.
	pub hash: Hash,
	/// Leaf number.
	pub number: BlockNumber,
}

/// Suite-wide world configuration consumed once at [`WorldBase::start`]. Mid-test
/// changes flow directly through [`crate::chain::ChainModel`] (`add_session`,
/// `set_claim_queue_at`, `set_runtime_api_version`, etc.) — single source of truth for
/// chain/runtime state.
#[derive(Clone, Debug)]
pub struct WorldConfig {
	/// Session index applied to every block produced via `chain.extend(...)`. Mid-test
	/// session changes go through `chain.add_session(...)` + per-block session overrides.
	pub session_index: SessionIndex,
	/// Per-core schedule — write-side scheduling primitive. Each entry is a
	/// [`CoreSchedule`] that defines how the chain model derives the runtime's
	/// `ClaimQueue` at every block (the queue rotates per block according to the
	/// cycle pattern). Per-block claim-queue overrides take precedence; set them
	/// via [`crate::chain::ChainModel::set_claim_queue_at`] on the chain.
	pub schedule: Vec<(CoreIndex, CoreSchedule)>,
	/// Validation-code hash baked into the synthesised backing constraints — must
	/// match what `make_candidate(.., validation_code_hash)` produces or candidates
	/// fail constraints' `ValidationCodeMismatch` check.
	pub validation_code_hash: ValidationCodeHash,
	/// Optional override for `min_relay_parent_number` in the synthesised backing
	/// constraints. Defaults to `leaf.number - (scheduling_lookahead - 1)`.
	pub min_relay_parent_number_override: Option<BlockNumber>,
	/// Runtime API version reported by the chain model. Lets tests exercise the
	/// `NotSupported` fallback path for APIs the configured runtime does not yet
	/// implement (e.g. `AncestorRelayParentInfo`). Defaults to the highest version
	/// the chain model implements end-to-end.
	pub runtime_api_version: u32,
	/// Validators reported in the runtime's session info. Empty by default —
	/// scenarios that exercise validator-side behaviour (groups, signatures,
	/// rotation) populate this.
	pub validators: Vec<ValidatorId>,
	/// Validator groups reported in the runtime's session info. Empty by default.
	/// Multiple groups + matching `group_rotation_frequency` enables per-block
	/// group rotation across cores.
	pub validator_groups: Vec<Vec<ValidatorIndex>>,
	/// Group rotation frequency reported in the runtime's session info. Defaults
	/// to 1 (rotates every block). Set to a large number to keep group 0 stable
	/// across an ancestry chain.
	pub group_rotation_frequency: u32,
	/// Slot of the genesis block. Each subsequent `chain.extend(...)` bumps the
	/// slot by one. Defaults to 0. Increase this for V3 tests that need
	/// leaf-parent / leaf at specific slots.
	pub genesis_slot: Slot,
	/// Node feature bits the chain's runtime reports (`NodeFeatures`), stored as raw
	/// bit indices (`FeatureIndex as u8` — the enum itself doesn't impl `Debug`).
	/// Empty by default. Populate via [`Self::with_node_feature`]: V3 candidate
	/// descriptors are gated on `CandidateReceiptV2`; V4 segment scenarios
	/// additionally need `CandidateReceiptV3`.
	pub node_features: Vec<u8>,
}

impl Default for WorldConfig {
	fn default() -> Self {
		Self {
			session_index: 1,
			schedule: Vec::new(),
			validation_code_hash: dummy_validation_code().hash(),
			min_relay_parent_number_override: None,
			runtime_api_version:
				polkadot_node_subsystem::messages::RuntimeApiRequest::ANCESTOR_RELAY_PARENT_INFO_RUNTIME_REQUIREMENT,
			validators: Vec::new(),
			validator_groups: Vec::new(),
			group_rotation_frequency: 1,
			genesis_slot: Slot::from(0),
			node_features: Vec::new(),
		}
	}
}

impl WorldConfig {
	/// Add a per-core schedule entry. Convenience over `self.schedule.push(...)`.
	pub fn with_schedule(mut self, core: CoreIndex, schedule: CoreSchedule) -> Self {
		self.schedule.push((core, schedule));
		self
	}

	/// Set the session index applied to every leaf-registered block.
	pub fn with_session_index(mut self, session_index: SessionIndex) -> Self {
		self.session_index = session_index;
		self
	}

	/// Set the validators reported in the runtime's session info.
	pub fn with_validators(mut self, validators: Vec<ValidatorId>) -> Self {
		self.validators = validators;
		self
	}

	/// Override the validator groups list. Used by multi-core, multi-group tests
	/// where per-block group rotation matters.
	pub fn with_validator_groups(mut self, groups: Vec<Vec<ValidatorIndex>>) -> Self {
		self.validator_groups = groups;
		self
	}

	/// Override the group rotation frequency. Defaults to 1 (rotates every block);
	/// set to a large number to keep group 0 stable across an ancestry chain.
	pub fn with_group_rotation_frequency(mut self, freq: u32) -> Self {
		self.group_rotation_frequency = freq;
		self
	}

	/// Set the slot of the genesis block. Each subsequent `chain.extend(...)`
	/// bumps the slot by one.
	pub fn with_genesis_slot(mut self, slot: Slot) -> Self {
		self.genesis_slot = slot;
		self
	}

	/// Enable one node feature bit on the chain's reported `NodeFeatures`. Chainable;
	/// call once per feature.
	pub fn with_node_feature(
		mut self,
		feature: polkadot_primitives::node_features::FeatureIndex,
	) -> Self {
		self.node_features.push(feature as u8);
		self
	}

	/// Enable the `CandidateReceiptV2` node feature on the chain. Required for any
	/// scenario that exercises a V3 candidate descriptor. Sugar over
	/// [`Self::with_node_feature`].
	pub fn with_v3_descriptors_enabled(self) -> Self {
		self.with_node_feature(polkadot_primitives::node_features::FeatureIndex::CandidateReceiptV2)
	}

	/// Override the runtime API version reported by the chain model. Lets tests
	/// exercise the `NotSupported` fallback path for APIs the configured runtime
	/// does not yet implement.
	pub fn with_runtime_api_version(mut self, version: u32) -> Self {
		self.runtime_api_version = version;
		self
	}

	/// Override `min_relay_parent_number` in the synthesised backing constraints.
	pub fn with_min_relay_parent_number(mut self, n: BlockNumber) -> Self {
		self.min_relay_parent_number_override = Some(n);
		self
	}
}

/// Subsystem-agnostic shared test-world state: the running `Sim`, the chain model,
/// the activated leaves, and the suite-wide [`WorldConfig`].
///
/// Per-tenant `World` types compose this as a field and impl [`HasBase`] to gain the
/// shared methods (`new_block`, `deactivate_leaf`, `finalize`, etc.) directly on `world.foo()`.
pub struct WorldBase<S: SubsystemUnderTest>
where
	AllMessages: From<<S::Message as AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	/// The driving simulation.
	pub sim: Sim<S>,
	/// The chain model — answers Runtime/ChainApi queries. Mutated via
	/// [`BlockBuilder`] (per-leaf head data + pending availability) and direct
	/// `chain.lock()` (mid-test session changes, claim-queue overrides, etc.).
	pub chain: SharedChain,
	/// All leaves the harness has signalled `ActiveLeaves::start_work` for, in
	/// activation order.
	pub leaves: Vec<LeafRef>,
	/// Suite-wide config. Read-only after `start`; mid-test state changes go through
	/// `chain` directly.
	pub config: WorldConfig,
}

impl<S: SubsystemUnderTest> WorldBase<S>
where
	AllMessages: From<<S::Message as AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	/// Start a new world from a [`WorldConfig`]. Seeds the chain model with the
	/// configured genesis slot, session info (validators, groups, rotation), per-core
	/// claim queue, runtime API version, and node features; installs the chain model +
	/// a fallback `PanicResponder` as the simulation's responder graph; and spins up
	/// the `Sim`. No leaves are active until [`HasBase::new_block`] is called.
	pub fn start(config: WorldConfig) -> Self {
		let chain = build_chain_model(&config);
		let mut responder = LayeredResponder::new();
		responder.push(chain.clone());
		responder.push(PanicResponder);
		Self::start_with_responder(responder, chain, config)
	}

	/// Start a new world with a caller-supplied responder chain. Use when the chain
	/// model isn't enough — typically because a tenant pushes a tenant-specific
	/// `AnswerQuery` layer in front of the chain (e.g. a `CanSecond` script).
	pub fn start_with_responder(
		responder: LayeredResponder,
		chain: SharedChain,
		config: WorldConfig,
	) -> Self {
		let sim = Sim::<S>::start(SimConfig::default(), responder);
		Self { sim, chain, leaves: Vec::new(), config }
	}
}

/// Build a `SharedChain` from a [`WorldConfig`]. Tenants whose responder graph needs
/// extra layers in front of the chain (e.g. a `CanSecond` query stub) build their
/// own [`LayeredResponder`] and pass `chain.clone()` into [`WorldBase::start_with_responder`].
/// Most tenants use [`WorldBase::start`] and don't touch this directly.
pub fn build_chain_model(config: &WorldConfig) -> SharedChain {
	let mut chain = ChainModel::new(config.genesis_slot);
	let session_info = SessionInfo {
		validators: config.validators.clone(),
		validator_groups: config.validator_groups.clone(),
		group_rotation_info: polkadot_primitives::GroupRotationInfo {
			session_start_block: 0,
			group_rotation_frequency: config.group_rotation_frequency,
			now: 0,
		},
	};
	// Register session info at both `0` (default for genesis-disconnected synthetic
	// ancestors) and `config.session_index` (for blocks the test activates).
	chain.add_session(0, session_info.clone());
	if config.session_index != 0 {
		chain.add_session(config.session_index, session_info);
	}
	// Align the genesis block's session with the configured world session so that
	// `chain.extend(...)` (which inherits the parent's session) produces blocks in
	// `config.session_index`. Without this, auto-allocated leaves report session 0,
	// out of sync with `world.session_index()`.
	let genesis = chain.genesis();
	chain.set_block_session(genesis, config.session_index);
	for (core, schedule) in &config.schedule {
		chain.set_core_schedule(*core, schedule.clone());
	}
	chain.set_runtime_api_version(config.runtime_api_version);
	if !config.node_features.is_empty() {
		let mut features = polkadot_primitives::NodeFeatures::EMPTY;
		features.resize(
			polkadot_primitives::node_features::FeatureIndex::FirstUnassigned as u8 as usize,
			false,
		);
		for feature in &config.node_features {
			features.set(*feature as usize, true);
		}
		chain.set_node_features(features);
	}
	SharedChain::new(chain)
}

/// Synthesise a permissive backing-constraints record. `valid_watermarks =
/// vec![leaf.number]` matches `hrmp_watermark = relay_parent_number` in default
/// `make_candidate(...)` output; `required_parent = head_data` ties acceptance to the
/// leaf-flavoured head data the test declares.
pub fn synthesise_constraints(
	min_relay_parent_number: BlockNumber,
	valid_watermarks: Vec<BlockNumber>,
	required_parent: HeadData,
	validation_code_hash: ValidationCodeHash,
) -> Constraints {
	const MAX_POV_SIZE: u32 = 1_000_000;
	Constraints {
		min_relay_parent_number,
		max_pov_size: MAX_POV_SIZE,
		max_head_data_size: 20480,
		max_code_size: 1_000_000,
		ump_remaining: 10,
		ump_remaining_bytes: 1_000,
		max_ump_num_per_candidate: 10,
		dmp_remaining_messages: vec![],
		hrmp_inbound: InboundHrmpLimitations { valid_watermarks },
		hrmp_channels_out: vec![],
		max_hrmp_num_per_candidate: 0,
		required_parent,
		validation_code_hash,
		upgrade_restriction: None,
		future_validation_code: None,
	}
}

/// Trait every per-tenant `World` impls. Two accessor methods (`base` + `base_mut`)
/// plus default-impl convenience methods: leaf builder, leaf deactivation, raw
/// active-leaves signal, suite-wide config accessors.
pub trait HasBase
where
	AllMessages:
		From<<<Self::Sut as SubsystemUnderTest>::Message as AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<<Self::Sut as SubsystemUnderTest>::Message>,
{
	/// The subsystem-under-test type the `Sim<S>` inside `WorldBase` is parameterised by.
	type Sut: SubsystemUnderTest;

	/// Shared base, immutable.
	fn base(&self) -> &WorldBase<Self::Sut>;

	/// Shared base, mutable.
	fn base_mut(&mut self) -> &mut WorldBase<Self::Sut>;

	/// Begin building a new block. The block is extended onto the chain's current
	/// tip via [`ChainModel::extend`]; use [`BlockBuilder::from_parent`] to extend
	/// from a non-tip parent (forks, or non-linear extensions). Per-(rp, para) head
	/// data + pending availability + claim queue accumulate on the builder and are
	/// written to the chain model when the builder finalises.
	///
	/// Two terminal verbs:
	///
	/// * [`BlockBuilder::register`] — writes the block to the chain model only. The subsystem is
	///   **not** told. Use this for ancestor blocks that should exist in the chain history but were
	///   never independently signalled as active leaves (skipped activations, fast-sync gaps,
	///   blocks that were never tip).
	///
	/// * [`BlockBuilder::activate`] — `register` + signal
	///   `OverseerSignal::ActiveLeaves(start_work(new), deactivated=[parent if it was an active
	///   leaf])`. Mirrors the production `block_imported` signal: when a child of an active leaf is
	///   imported, the parent stops being a leaf in the same `ActiveLeavesUpdate`.
	fn new_block(&mut self) -> BlockBuilder<'_, Self::Sut> {
		BlockBuilder::new(self.base_mut())
	}

	/// Deactivate a leaf via `ActiveLeavesUpdate::stop_work`. Mostly useful for
	/// tests that drive deactivation without an accompanying activation; normal
	/// linear chain progression deactivates the parent automatically via
	/// [`BlockBuilder::activate`].
	fn deactivate_leaf(&mut self, hash: Hash) {
		let base = self.base_mut();
		base.sim
			.signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::stop_work(hash)));
		base.leaves.retain(|l| l.hash != hash);
	}

	/// Signal `OverseerSignal::BlockFinalized(hash, number)` followed by an
	/// `ActiveLeavesUpdate` that prunes orphaned leaves. Mirrors the production
	/// `block_finalized` path:
	///
	/// * any active leaf at `number <= finalized.number` other than the finalized block itself is
	///   removed (those leaves are on branches that can no longer extend the finalized chain);
	/// * the finalized block itself stays an active leaf if it currently is one;
	/// * higher-numbered orphan leaves stay until they fall below a future finalized number —
	///   production accepts that and so do we.
	///
	/// The empty-update case (no leaves to prune) skips the second signal,
	/// matching production behaviour.
	fn finalize(&mut self, hash: Hash) {
		let number = {
			let chain = self.base().chain.lock();
			chain.block(&hash).expect("finalized hash must be a registered block").number
		};
		let base = self.base_mut();
		base.sim.signal(OverseerSignal::BlockFinalized(hash, number));

		let mut deactivated = Vec::new();
		base.leaves.retain(|l| {
			if l.number <= number && l.hash != hash {
				deactivated.push(l.hash);
				false
			} else {
				true
			}
		});
		if !deactivated.is_empty() {
			let update = ActiveLeavesUpdate { activated: None, deactivated: deactivated.into() };
			base.sim.signal(OverseerSignal::ActiveLeaves(update));
		}
	}

	/// Send a raw `OverseerSignal::ActiveLeaves` update. Escape hatch for tests
	/// that need to construct an update shape outside the [`BlockBuilder`] /
	/// [`HasBase::deactivate_leaf`] / [`HasBase::finalize`] vocabulary (rare).
	fn signal_active_leaves(&mut self, update: ActiveLeavesUpdate) {
		let base = self.base_mut();
		if let Some(activated) = &update.activated {
			base.leaves.push(LeafRef { hash: activated.hash, number: activated.number });
		}
		for hash in update.deactivated.iter() {
			base.leaves.retain(|l| l.hash != *hash);
		}
		base.sim.signal(OverseerSignal::ActiveLeaves(update));
	}

	// =====================================================================================
	// Active-leaf accessors. Subsystem-agnostic: read `base().leaves` + `base().chain`.
	// =====================================================================================

	/// Hash of the first (and, for most scenarios, only) active leaf. Panics if no
	/// leaf has been activated.
	fn leaf(&self) -> Hash {
		self.base().leaves[0].hash
	}

	/// Block number of the first active leaf. Panics if no leaf has been activated.
	fn leaf_number(&self) -> BlockNumber {
		self.base().leaves[0].number
	}

	/// Walk back from the first active leaf, returning up to
	/// `chain.allowed_ancestry_len()` ancestors. `ancestors()[0]` is the leaf's
	/// parent. Mirrors what real `prospective-parachains` returns for
	/// `known_allowed_relay_parents_under(leaf)` (excluding the leaf itself).
	fn ancestors(&self) -> Vec<Hash> {
		self.ancestors_of(0)
	}

	/// Walk back from `leaves[idx]`, returning up to
	/// `chain.allowed_ancestry_len()` ancestors.
	fn ancestors_of(&self, idx: usize) -> Vec<Hash> {
		let leaf_hash = self.base().leaves[idx].hash;
		let chain = self.base().chain.lock();
		// `allowed_ancestry_len + 1` is the depth budget — `chain.ancestors(_, k)`
		// returns up to k blocks excluding the queried hash; the implicit view
		// typically resolves `allowed_ancestry_len` of those.
		let k = chain.allowed_ancestry_len() as usize + 1;
		chain.ancestors(leaf_hash, k)
	}

	// =====================================================================================
	// Read accessors for suite-wide config — hide field layout of `WorldConfig` so tests
	// don't break when fields are added or moved.
	// =====================================================================================

	/// Validation-code hash baked into the synthesised backing constraints.
	fn validation_code_hash(&self) -> ValidationCodeHash {
		self.base().config.validation_code_hash
	}

	/// Session index applied to every block registered by [`BlockBuilder`].
	fn session_index(&self) -> SessionIndex {
		self.base().config.session_index
	}

	/// Optional override for `min_relay_parent_number` in the synthesised backing
	/// constraints.
	fn min_relay_parent_number_override(&self) -> Option<BlockNumber> {
		self.base().config.min_relay_parent_number_override
	}
}

/// Fluent builder for a new block. Accumulates per-para head data + pending
/// availability, then on `.register()` / `.activate()`:
///
/// 1. Allocates the block's hash + number via [`ChainModel::extend`] (or honours an
///    explicitly-pinned hash via [`Self::with_hash_and_number`]).
/// 2. Writes per-para head data + pending availability to the chain model.
/// 3. Synthesises permissive backing constraints (using `world.config`'s `validation_code_hash` +
///    optional `min_relay_parent_number_override`) and writes them to the chain model.
/// 4. (`activate` only) Signals `OverseerSignal::ActiveLeaves` with `start_work(new)` plus
///    `deactivated=[parent]` if the parent was an active leaf — mirroring the production
///    `block_imported` signal.
///
/// Two real-world flows the API mirrors:
///
/// * **Tip-following** = repeated `.activate()` calls. Each new block becomes the active leaf; its
///   parent (the previous tip) is deactivated in the same signal. Models normal block production
///   where the overseer always emits one `ActiveLeavesUpdate` per imported block.
///
/// * **Skipping** = `.register()` for intermediate blocks, `.activate()` only for the next tip the
///   overseer announces. Models scenarios where the overseer doesn't emit a per-block signal — e.g.
///   fast-syncing, restart catch-up, the parachains-API gap. Tests use this to verify the subsystem
///   handles gaps in the activation sequence correctly via implicit view.
pub struct BlockBuilder<'w, S: SubsystemUnderTest>
where
	AllMessages: From<<S::Message as AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	base: &'w mut WorldBase<S>,
	/// Optional explicit (hash, number). When `None`, the builder extends the chain
	/// at activation time via `chain.extend(parent)`.
	hash_and_number: Option<(Hash, BlockNumber)>,
	/// Optional explicit parent for the auto-extend path. `None` means "extend the
	/// chain's current tip" — the common linear case.
	parent: Option<Hash>,
	/// `(para, head_data)` pairs to write via
	/// `chain.set_backing_constraints_at(block, para, ..)` (head data becomes
	/// `Constraints::required_parent`).
	head_data: Vec<(ParaId, HeadData)>,
	/// `(para, pending)` pairs to write via
	/// `chain.set_pending_availability_at(block, para, ..)`.
	pending: Vec<(ParaId, Vec<CandidatePendingAvailability>)>,
	/// Optional per-block claim-queue override. When set, written via
	/// `chain.set_claim_queue_at(block, queue)` and takes precedence over the
	/// suite-wide schedule for this block only — subsequent blocks revert to the
	/// schedule unless they too set an override.
	claim_queue: Option<BTreeMap<CoreIndex, VecDeque<ParaId>>>,
}

impl<'w, S: SubsystemUnderTest> BlockBuilder<'w, S>
where
	AllMessages: From<<S::Message as AssociateOutgoing>::OutgoingMessages>,
	AllMessages: From<S::Message>,
{
	fn new(base: &'w mut WorldBase<S>) -> Self {
		Self {
			base,
			hash_and_number: None,
			parent: None,
			head_data: Vec::new(),
			pending: Vec::new(),
			claim_queue: None,
		}
	}

	/// Pin a literal hash + number for this block instead of having the chain
	/// auto-allocate one. Use only when the test asserts on a specific hash value
	/// (rare). When pinning, the caller is responsible for any required ancestor
	/// registration via direct `chain.lock()` mutation.
	pub fn with_hash_and_number(mut self, hash: Hash, number: BlockNumber) -> Self {
		self.hash_and_number = Some((hash, number));
		self
	}

	/// Extend from a specific parent block instead of the chain's current tip. Use
	/// for non-linear extensions: the chain assigns the new block a sibling-distinct
	/// hash via the existing sibling-index mechanism.
	pub fn from_parent(mut self, parent: Hash) -> Self {
		self.parent = Some(parent);
		self
	}

	/// Seed the para's head data at this block's relay parent. Becomes
	/// `Constraints::required_parent` in the synthesised backing constraints.
	pub fn with_head_data(mut self, para: ParaId, head_data: HeadData) -> Self {
		self.head_data.push((para, head_data));
		self
	}

	/// Seed the para's pending-availability list at this block's relay parent.
	pub fn with_pending(
		mut self,
		para: ParaId,
		candidates: Vec<CandidatePendingAvailability>,
	) -> Self {
		self.pending.push((para, candidates));
		self
	}

	/// Override the claim queue at this block for a single core. Takes precedence
	/// over the suite-wide schedule for this block only — subsequent blocks revert
	/// to the schedule unless they too set an override. Use when modelling
	/// exceptions to the suite-wide cycle (coretime expiry, on-demand cores,
	/// scheduler edge cases). Call repeatedly for multi-core overrides.
	pub fn with_claim_queue_at(
		mut self,
		core: CoreIndex,
		paras: impl IntoIterator<Item = ParaId>,
	) -> Self {
		let queue = self.claim_queue.get_or_insert_with(BTreeMap::new);
		queue.insert(core, paras.into_iter().collect());
		self
	}

	/// Finalise the block on the chain model **without** telling the subsystem.
	/// Use for ancestor blocks the test wants the chain to know about (so queries
	/// for them resolve) but that were never independently signalled as active
	/// leaves — e.g. blocks that were skipped during a fast-sync gap. Returns the
	/// block's identity.
	pub fn register(self) -> LeafRef {
		let (_base, block) = self.flush_to_chain();
		block
	}

	/// Finalise the block + signal `OverseerSignal::ActiveLeaves` with
	/// `start_work(new)` plus `deactivated=[parent]` if the parent is currently an
	/// active leaf in the harness mirror. Mirrors the production `block_imported`
	/// signal exactly: a child of the current tip pushes the parent off the active
	/// set in the same update.
	///
	/// If the subsystem under test consumes network-bridge events (i.e.
	/// [`SubsystemUnderTest::our_view_change`] returns `Some`), this also publishes
	/// the new view to the subsystem — mirroring the production network bridge's
	/// view-update fan-out that follows block import.
	///
	/// Returns the new active leaf's identity.
	pub fn activate(self) -> LeafRef {
		let (base, leaf) = self.flush_to_chain();
		// Auto-deactivate parent if it's currently an active leaf. Production
		// `block_imported` does the same: importing a child of an active leaf
		// emits one `ActiveLeavesUpdate` carrying both `activated: Some(child)`
		// and `deactivated: [parent]`.
		let parent_hash = base
			.chain
			.lock()
			.block(&leaf.hash)
			.expect("just-extended block has a parent_hash")
			.parent_hash;
		let mut deactivated: Vec<Hash> = Vec::new();
		base.leaves.retain(|l| {
			if l.hash == parent_hash {
				deactivated.push(l.hash);
				false
			} else {
				true
			}
		});

		let update = ActiveLeavesUpdate {
			activated: Some(new_leaf(leaf.hash, leaf.number)),
			deactivated: deactivated.into(),
		};
		base.sim.signal(OverseerSignal::ActiveLeaves(update));
		base.leaves.push(leaf);

		// Publish the new view to the subsystem under test, if the adapter consumes
		// network-bridge events. Mirrors the production sequence: overseer emits
		// `ActiveLeaves`, network bridge separately broadcasts the new view.
		let view =
			polkadot_node_network_protocol::OurView::new(base.leaves.iter().map(|l| l.hash), 0);
		if let Some(msg) = S::our_view_change(view) {
			base.sim.send(msg);
		}

		leaf
	}

	/// Resolve the block's hash + number (extending the chain if needed) and write
	/// per-para state + synthesised constraints to the chain model. Returns
	/// `&mut WorldBase` so `.activate()` can re-borrow the sim.
	fn flush_to_chain(self) -> (&'w mut WorldBase<S>, LeafRef) {
		let BlockBuilder { base, hash_and_number, parent, head_data, pending, claim_queue } = self;
		let validation_code_hash = base.config.validation_code_hash;
		let min_relay_parent_number_override = base.config.min_relay_parent_number_override;

		let leaf = {
			let mut chain = base.chain.lock();
			let (hash, number) = if let Some((hash, number)) = hash_and_number {
				(hash, number)
			} else {
				let parent = parent.unwrap_or_else(|| chain.tip());
				let hash = chain.extend(parent);
				let number =
					chain.block(&hash).expect("just-extended block must be registered").number;
				(hash, number)
			};
			let ancestry_len = (DEFAULT_SCHEDULING_LOOKAHEAD - 1) as u32;
			let min_relay_parent_number = min_relay_parent_number_override
				.unwrap_or_else(|| number.saturating_sub(ancestry_len));
			for (para, head_data) in &head_data {
				let constraints = synthesise_constraints(
					min_relay_parent_number,
					vec![number],
					head_data.clone(),
					validation_code_hash,
				);
				chain.set_backing_constraints_at(hash, *para, constraints);
			}
			for (para, candidates) in pending {
				let receipts: Vec<CommittedCandidateReceipt> = candidates
					.iter()
					.map(|p| CommittedCandidateReceipt {
						descriptor: p.descriptor.clone(),
						commitments: p.commitments.clone(),
					})
					.collect();
				chain.set_pending_availability_at(hash, para, receipts);
			}
			if let Some(queue) = claim_queue {
				chain.set_claim_queue_at(hash, queue);
			}
			LeafRef { hash, number }
		};
		(base, leaf)
	}
}
