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

//! `ChainModel`: an in-memory replacement for `runtime-api` + `chain-api`.

use crate::{contract::Query, harness::dispatcher::AnswerQuery};
use polkadot_node_subsystem::messages::{ChainApiMessage, RuntimeApiMessage, RuntimeApiRequest};
use polkadot_primitives::{
	async_backing::{AsyncBackingParams, Constraints, InboundHrmpLimitations},
	BlockNumber, CandidateEvent, CommittedCandidateReceiptV2 as CommittedCandidateReceipt,
	CoreIndex, GroupRotationInfo, Hash, HeadData, Header, Id as ParaId, NodeFeatures,
	PersistedValidationData, SessionIndex, ValidatorId, ValidatorIndex,
};
use sp_consensus_babe::digests::{CompatibleDigestItem, PreDigest, SecondaryPlainPreDigest};
use sp_consensus_slots::Slot;
use sp_runtime::{Digest, DigestItem};
use std::collections::{BTreeMap, VecDeque};

/// Per-session validator config.
#[derive(Clone, Debug)]
pub struct SessionInfo {
	/// Validator public keys for the session.
	pub validators: Vec<ValidatorId>,
	/// Group memberships.
	pub validator_groups: Vec<Vec<ValidatorIndex>>,
	/// Group rotation info as the runtime would report it.
	pub group_rotation_info: GroupRotationInfo,
}

/// Per-block facts the chain model knows.
#[derive(Clone, Debug)]
pub struct BlockInfo {
	/// Hash of this block.
	pub hash: Hash,
	/// Hash of the parent block. `Hash::zero()` for genesis.
	pub parent_hash: Hash,
	/// Block number.
	pub number: BlockNumber,
	/// BABE slot.
	pub slot: Slot,
	/// Session this block belongs to.
	pub session_index: SessionIndex,
}

impl BlockInfo {
	/// Materialise a `Header` for this block. Includes the BABE pre-digest derived from
	/// `slot` so V3 scheduling-parent validation can extract the slot from the header.
	pub fn header(&self) -> Header {
		let pre_digest = PreDigest::SecondaryPlain(SecondaryPlainPreDigest {
			authority_index: 0,
			slot: self.slot,
		});
		Header {
			parent_hash: self.parent_hash,
			number: self.number,
			state_root: Default::default(),
			extrinsics_root: Default::default(),
			digest: Digest { logs: vec![DigestItem::babe_pre_digest(pre_digest)] },
		}
	}
}

/// In-memory model of the relay chain.
///
/// Constructed via [`ChainModel::new`] (genesis only) and grown via [`extend`]. Sessions are
/// added via [`add_session`]; per-block claim queues via [`set_claim_queue_at`].
///
/// The model is single-threaded and owned by the [`Sim`]. Test code mutates it directly.
///
/// [`extend`]: ChainModel::extend
/// [`add_session`]: ChainModel::add_session
/// [`set_claim_queue_at`]: ChainModel::set_claim_queue_at
/// [`Sim`]: crate::harness::Sim
/// Per-core schedule: a finite cycle of paras that repeats. The claim queue at block N
/// shows positions `[N, N+1, ..., N+lookahead-1]` sampled from `cycle[(start + offset) %
/// cycle.len()]`. Empty `cycle` means no para is ever scheduled on this core.
#[derive(Clone, Debug, Default)]
pub struct CoreSchedule {
	/// The repeating sequence of paras for this core. `cycle[i]` is the para scheduled at
	/// block-index `start + i` modulo `cycle.len()`.
	pub cycle: Vec<ParaId>,
	/// Block index at which the cycle is anchored. The para at block `n` is
	/// `cycle[(n - start) % cycle.len()]`, so with the default `start = 0`, `cycle[0]` is the
	/// para at genesis (block 0), `cycle[1]` at block 1, and so on. Most tests leave this at 0.
	pub start: u32,
}

impl CoreSchedule {
	/// Static schedule: the same para repeats forever.
	pub fn always(para: ParaId) -> Self {
		Self { cycle: vec![para], start: 0 }
	}

	/// Cycling schedule: `cycle[i]` is scheduled at block index `i, len, 2*len, ...`.
	pub fn cycling(cycle: Vec<ParaId>) -> Self {
		Self { cycle, start: 0 }
	}

	/// Para scheduled at block index `block_number`. Returns `None` if cycle is empty.
	pub fn at(&self, block_number: u32) -> Option<ParaId> {
		if self.cycle.is_empty() {
			return None;
		}
		let idx = ((block_number.wrapping_sub(self.start)) as usize) % self.cycle.len();
		Some(self.cycle[idx])
	}
}

/// In-memory model of the relay chain.
///
/// Constructed via [`ChainModel::new`] (genesis only) and grown via
/// [`ChainModel::extend`]. Sessions are added via [`ChainModel::add_session`]; per-core
/// schedule via [`ChainModel::set_core_schedule`]. Per-block claim-queue overrides are
/// available via [`ChainModel::set_claim_queue_at`].
#[derive(Debug)]
pub struct ChainModel {
	blocks: BTreeMap<Hash, BlockInfo>,
	children: BTreeMap<Hash, Vec<Hash>>,
	sessions: BTreeMap<SessionIndex, SessionInfo>,
	/// Global schedule: per-core para rotation. The claim queue at any block is computed by
	/// sampling each core's schedule starting at the block's number.
	schedule: BTreeMap<CoreIndex, CoreSchedule>,
	/// Per-block claim-queue overrides. If set, takes precedence over `schedule`. Used by
	/// tests that need exotic shapes the cycle abstraction can't express.
	claim_queue_overrides: BTreeMap<Hash, BTreeMap<CoreIndex, VecDeque<ParaId>>>,
	scheduling_lookahead: u32,
	async_backing_params: AsyncBackingParams,
	node_features: NodeFeatures,
	minimum_backing_votes: u32,
	pending_availability: BTreeMap<ParaId, Vec<CommittedCandidateReceipt>>,
	/// Per-relay-parent pending-availability overrides. When set for `(rp, para)`, takes
	/// precedence over the global `pending_availability` table. Used by tests where
	/// different leaves report different pending-availability snapshots (e.g. a candidate
	/// is pending under leaf_b but not yet seeded under sibling leaf_c).
	pending_availability_at: BTreeMap<(Hash, ParaId), Vec<CommittedCandidateReceipt>>,
	backing_constraints: BTreeMap<ParaId, Constraints>,
	/// Per-relay-parent backing-constraint overrides. Takes precedence over the global
	/// `backing_constraints` table. Used by tests where the same para has different
	/// `required_parent` head data at different leaves (as in real prospective tests where
	/// each leaf carries its own per-para `head_data` snapshot).
	backing_constraints_at: BTreeMap<(Hash, ParaId), Constraints>,
	/// Per-relay-parent `CandidateEvent` log returned by
	/// `RuntimeApiRequest::CandidateEvents`. Tests seed this when driving finalization to
	/// trigger experimental's `+VALID_INCLUDED_CANDIDATE_BUMP` path.
	candidate_events: BTreeMap<Hash, Vec<CandidateEvent>>,
	/// Latest finalized block — controls `ChainApiMessage::FinalizedBlockNumber` /
	/// `FinalizedBlockHash`. Initialised to genesis.
	finalized: Hash,
	genesis: Hash,
	tip: Hash,
	/// Effective `RuntimeApiRequest` version reported by the model. Controls
	/// `NotSupported` dispatch for newer APIs. Tests parameterising over runtime
	/// versions (e.g. exercising both the `AncestorRelayParentInfo` path and the
	/// chain-header fallback path) override this. Defaults to the highest version the
	/// chain model implements end-to-end.
	runtime_api_version: u32,
}

impl ChainModel {
	/// New chain model with a single genesis block at slot `genesis_slot`, session 0.
	/// Genesis hash is fixed at `Hash::from_low_u64_be(0xC0FFEE)` so two chain models in
	/// the same test process don't accidentally share / collide.
	pub fn new(genesis_slot: Slot) -> Self {
		let genesis_hash = Hash::from_low_u64_be(0xC0FFEE);
		let genesis_info = BlockInfo {
			hash: genesis_hash,
			parent_hash: Hash::zero(),
			number: 0,
			slot: genesis_slot,
			session_index: 0,
		};
		let mut blocks = BTreeMap::new();
		blocks.insert(genesis_hash, genesis_info);
		Self {
			blocks,
			children: BTreeMap::new(),
			sessions: BTreeMap::new(),
			schedule: BTreeMap::new(),
			claim_queue_overrides: BTreeMap::new(),
			scheduling_lookahead: 3,
			async_backing_params: AsyncBackingParams {
				max_candidate_depth: 4,
				allowed_ancestry_len: 2,
			},
			node_features: NodeFeatures::EMPTY,
			minimum_backing_votes: 1,
			pending_availability: BTreeMap::new(),
			pending_availability_at: BTreeMap::new(),
			backing_constraints: BTreeMap::new(),
			backing_constraints_at: BTreeMap::new(),
			candidate_events: BTreeMap::new(),
			finalized: genesis_hash,
			genesis: genesis_hash,
			tip: genesis_hash,
			runtime_api_version:
				polkadot_node_subsystem::messages::RuntimeApiRequest::ANCESTOR_RELAY_PARENT_INFO_RUNTIME_REQUIREMENT,
		}
	}

	/// Override the runtime API version reported. Lets a test exercise the
	/// `NotSupported` fallback path for newer APIs (e.g.
	/// `AncestorRelayParentInfo`).
	pub fn set_runtime_api_version(&mut self, version: u32) {
		self.runtime_api_version = version;
	}

	/// Genesis hash.
	pub fn genesis(&self) -> Hash {
		self.genesis
	}

	/// Current tip (most recently extended block).
	pub fn tip(&self) -> Hash {
		self.tip
	}

	/// Look up a block by hash.
	pub fn block(&self, hash: &Hash) -> Option<&BlockInfo> {
		self.blocks.get(hash)
	}

	/// Register a block with a caller-chosen hash, parent, and number. Use this when the
	/// test wants specific hash literals (e.g. `Hash::from_low_u64_be(130)`) instead of the
	/// synthetic hashes [`Self::extend`] generates.
	///
	/// Slot and session are derived from the parent: slot is `parent_slot + 1` and session is
	/// inherited, **but only if the parent is already registered**. There is no later fixup —
	/// register parents before children. If the parent is absent at call time (including a
	/// deliberately detached block with `parent_hash == Hash::zero()`), slot falls back to
	/// `Slot::from(number)` and session to `0`; pass [`Self::register_block_with_session`] to
	/// pin the session explicitly in that case.
	///
	/// If `hash` is already registered this is a no-op (the test may call `register_block`
	/// idempotently when walking ancestor chains).
	///
	/// Used by per-subsystem test-sim consumer crates (e.g. prospective-parachains) whose
	/// faithful ports of pre-existing tests use literal block hashes.
	pub fn register_block(&mut self, hash: Hash, parent_hash: Hash, number: BlockNumber) {
		self.register_block_with_session(hash, parent_hash, number, None);
	}

	/// Like [`Self::register_block`] but with an explicit session override. Used when the
	/// test's whole synthetic chain should live in a specific session (e.g. session 1)
	/// instead of inheriting from a synthesised genesis-default of 0.
	pub fn register_block_with_session(
		&mut self,
		hash: Hash,
		parent_hash: Hash,
		number: BlockNumber,
		session_override: Option<SessionIndex>,
	) {
		if self.blocks.contains_key(&hash) {
			return;
		}
		let (slot, default_session) = match self.blocks.get(&parent_hash) {
			Some(parent) => (parent.slot + 1, parent.session_index),
			None => (Slot::from(number as u64), 0),
		};
		let session_index = session_override.unwrap_or(default_session);
		let info = BlockInfo { hash, parent_hash, number, slot, session_index };
		self.blocks.insert(hash, info);
		self.children.entry(parent_hash).or_default().push(hash);
		if number > self.blocks.get(&self.tip).map(|t| t.number).unwrap_or(0) {
			self.tip = hash;
		}
	}

	/// Append a child block onto `parent`. Slot increments by one, session is inherited from
	/// the parent. Returns the new block's hash.
	///
	/// Calling `extend` multiple times from the same parent produces siblings — each gets
	/// a distinct hash via a sibling-index mixed into the synthetic hash. The chain model
	/// supports forks; tests model fork choice by extending different children from a
	/// shared ancestor and activating each via `extend_and_activate_with`.
	///
	/// Panics if `parent` is not known.
	pub fn extend(&mut self, parent: Hash) -> Hash {
		let parent_info = self.blocks.get(&parent).cloned().expect("parent block must exist");
		let number = parent_info.number + 1;
		let slot = parent_info.slot + 1;
		let session_index = parent_info.session_index;
		let sibling_idx = self.children.get(&parent).map(|c| c.len() as u64).unwrap_or(0);
		let hash = synthetic_child_hash_with_sibling(parent, number, sibling_idx);
		let info = BlockInfo { hash, parent_hash: parent, number, slot, session_index };
		self.blocks.insert(hash, info);
		self.children.entry(parent).or_default().push(hash);
		self.tip = hash;
		hash
	}

	/// Install or replace the session info for a session index.
	pub fn add_session(&mut self, session_index: SessionIndex, info: SessionInfo) {
		self.sessions.insert(session_index, info);
	}

	/// Update the session index recorded for an already-registered block. Used by
	/// `WorldBase::start` to align the genesis block with the configured world session
	/// so subsequent `extend(...)` calls inherit the test-suite session, not the
	/// hardcoded genesis-default 0.
	pub fn set_block_session(&mut self, hash: Hash, session: SessionIndex) {
		if let Some(info) = self.blocks.get_mut(&hash) {
			info.session_index = session;
		}
	}

	/// Install a per-core schedule. The claim queue at any block is derived from this
	/// schedule at the block's number unless [`Self::set_claim_queue_at`] sets an explicit
	/// override.
	pub fn set_core_schedule(&mut self, core: CoreIndex, schedule: CoreSchedule) {
		self.schedule.insert(core, schedule);
	}

	/// Convenience: the same para is scheduled on `core` at every block.
	pub fn schedule_para_on_core(&mut self, core: CoreIndex, para: ParaId) {
		self.set_core_schedule(core, CoreSchedule::always(para));
	}

	/// Override the claim queue at a specific block. Single-block snapshot — does
	/// **not** propagate to subsequent blocks; the next block reverts to the
	/// per-core schedule unless it too sets an override. Models exceptions to the
	/// suite-wide cycle (coretime expiry, on-demand cores, scheduler edge cases).
	/// For evolving shapes, set an override at every block where the queue
	/// diverges from the schedule.
	pub fn set_claim_queue_at(
		&mut self,
		block: Hash,
		queue: BTreeMap<CoreIndex, VecDeque<ParaId>>,
	) {
		assert!(self.blocks.contains_key(&block), "claim queue set on unknown block");
		self.claim_queue_overrides.insert(block, queue);
	}

	/// Compute the claim queue at a block: per-core, populate `scheduling_lookahead`
	/// positions sampled from the schedule starting at the block's number. Returns
	/// `claim_queue_overrides[block]` if explicitly set.
	fn derive_claim_queue(&self, block: &Hash) -> BTreeMap<CoreIndex, VecDeque<ParaId>> {
		if let Some(q) = self.claim_queue_overrides.get(block) {
			return q.clone();
		}
		let info = match self.blocks.get(block) {
			Some(i) => i,
			None => return BTreeMap::new(),
		};
		let mut out = BTreeMap::new();
		for (core, sched) in &self.schedule {
			let mut q = VecDeque::with_capacity(self.scheduling_lookahead as usize);
			for offset in 0..self.scheduling_lookahead {
				if let Some(para) = sched.at(info.number + offset) {
					q.push_back(para);
				}
			}
			if !q.is_empty() {
				out.insert(*core, q);
			}
		}
		out
	}

	/// Override the scheduling lookahead value runtime returns.
	pub fn set_scheduling_lookahead(&mut self, lookahead: u32) {
		self.scheduling_lookahead = lookahead;
	}

	/// Install backing constraints for a para. If unset the model returns a permissive
	/// default that lets advertisements pass the prospective-parachains acceptance check.
	pub fn set_backing_constraints(&mut self, para: ParaId, constraints: Constraints) {
		self.backing_constraints.insert(para, constraints);
	}

	/// Install backing constraints for a `(relay_parent, para)` pair. Takes precedence over
	/// the global per-para entry installed via [`Self::set_backing_constraints`].
	///
	/// Used by tests where the same para has different `required_parent` head data at
	/// different relay parents (per-leaf head-data snapshot, as in
	/// `prospective-parachains`'s test fixtures).
	pub fn set_backing_constraints_at(
		&mut self,
		relay_parent: Hash,
		para: ParaId,
		constraints: Constraints,
	) {
		self.backing_constraints_at.insert((relay_parent, para), constraints);
	}

	/// Override the async backing params (max candidate depth, allowed ancestry length).
	/// Read the configured `allowed_ancestry_len` (mirrors what
	/// `RuntimeApi::AsyncBackingParams` would return).
	pub fn allowed_ancestry_len(&self) -> u32 {
		self.async_backing_params.allowed_ancestry_len
	}

	/// Replace the `AsyncBackingParams` returned by `RuntimeApi::AsyncBackingParams`.
	pub fn set_async_backing_params(&mut self, params: AsyncBackingParams) {
		self.async_backing_params = params;
	}

	/// Replace the node-features bitvec returned by `RuntimeApi::NodeFeatures`.
	pub fn set_node_features(&mut self, features: NodeFeatures) {
		self.node_features = features;
	}

	/// Override the minimum-backing-votes value returned by
	/// `RuntimeApi::MinimumBackingVotes`.
	pub fn set_minimum_backing_votes(&mut self, votes: u32) {
		self.minimum_backing_votes = votes;
	}

	/// Install the candidates-pending-availability list for a para. Applies globally;
	/// every `RuntimeApiRequest::CandidatesPendingAvailability(para, _)` query returns
	/// these unless a per-relay-parent override exists (see
	/// [`Self::set_pending_availability_at`]).
	pub fn set_pending_availability(
		&mut self,
		para: ParaId,
		candidates: Vec<CommittedCandidateReceipt>,
	) {
		self.pending_availability.insert(para, candidates);
	}

	/// Install the candidates-pending-availability list for a `(relay_parent, para)`
	/// pair. Takes precedence over the global per-para entry installed via
	/// [`Self::set_pending_availability`]. Use when sibling-fork tests need different
	/// pending-availability snapshots at different leaves (e.g. a candidate is pending
	/// under leaf_b but not yet seeded under sibling leaf_c, or leaf_c is supposed to
	/// inherit leaf_a's storage with no extra pending-availability of its own).
	pub fn set_pending_availability_at(
		&mut self,
		relay_parent: Hash,
		para: ParaId,
		candidates: Vec<CommittedCandidateReceipt>,
	) {
		self.pending_availability_at.insert((relay_parent, para), candidates);
	}

	/// Install the `CandidateEvents` log returned by
	/// `RuntimeApiRequest::CandidateEvents(rp, _)`. Tests driving experimental's
	/// finalization-driven score bump seed these alongside [`Self::set_pending_availability`].
	pub fn set_candidate_events(&mut self, rp: Hash, events: Vec<CandidateEvent>) {
		self.candidate_events.insert(rp, events);
	}

	/// Update the latest finalized block. Drives `ChainApiMessage::FinalizedBlockNumber`
	/// / `FinalizedBlockHash` lookups. Defaults to genesis until called.
	///
	/// Panics if `hash` is not a known block.
	pub fn set_finalized(&mut self, hash: Hash) {
		assert!(
			self.blocks.contains_key(&hash),
			"ChainModel::set_finalized: unknown block {:?}",
			hash,
		);
		self.finalized = hash;
	}

	/// Walk ancestry of `from`. Yields parent, grandparent, ... up to (but not including) the
	/// genesis pre-image. Used by `ChainApi::Ancestors`.
	pub fn ancestors(&self, from: Hash, k: usize) -> Vec<Hash> {
		let mut out = Vec::with_capacity(k);
		let mut cursor = from;
		for _ in 0..k {
			match self.blocks.get(&cursor) {
				Some(info) if info.parent_hash != Hash::zero() => {
					out.push(info.parent_hash);
					cursor = info.parent_hash;
				},
				_ => break,
			}
		}
		out
	}

	/// Hash of the finalized block at `number`, found by walking back from the finalized tip
	/// along parent links. Returns `None` when `number` is above the finalized tip (nothing
	/// is finalized that high yet) or the chain is malformed. Resolving along the finalized
	/// chain — rather than scanning all blocks for one at that height — is what keeps forks
	/// from yielding a non-canonical block.
	fn finalized_ancestor_at(&self, number: BlockNumber) -> Option<Hash> {
		let mut cursor = self.finalized;
		loop {
			let info = self.blocks.get(&cursor)?;
			match info.number.cmp(&number) {
				std::cmp::Ordering::Equal => return Some(info.hash),
				// Walked below the target without hitting it (gap in the chain) — no match.
				std::cmp::Ordering::Less => return None,
				// Still above the target: step to the parent. The genesis pre-image
				// (`Hash::zero()`) is not a real block, so the `?` above ends the walk.
				std::cmp::Ordering::Greater => cursor = info.parent_hash,
			}
		}
	}

	fn session_info(&self, session_index: SessionIndex) -> &SessionInfo {
		self.sessions.get(&session_index).unwrap_or_else(|| {
			panic!("ChainModel: no SessionInfo registered for {}", session_index)
		})
	}

	fn answer_runtime(&self, msg: RuntimeApiMessage) {
		match msg {
			RuntimeApiMessage::Request(parent, req) => self.answer_runtime_req(parent, req),
		}
	}

	fn answer_runtime_req(&self, parent: Hash, req: RuntimeApiRequest) {
		let info = self.blocks.get(&parent).unwrap_or_else(|| {
			panic!("ChainModel: RuntimeApi request for unknown block {:?}", parent)
		});
		match req {
			RuntimeApiRequest::SessionIndexForChild(tx) => {
				let _ = tx.send(Ok(info.session_index));
			},
			RuntimeApiRequest::ClaimQueue(tx) => {
				let queue = self.derive_claim_queue(&parent);
				let _ = tx.send(Ok(queue));
			},
			RuntimeApiRequest::Validators(tx) => {
				let _ = tx.send(Ok(self.session_info(info.session_index).validators.clone()));
			},
			RuntimeApiRequest::ValidatorGroups(tx) => {
				let session = self.session_info(info.session_index);
				let mut rotation = session.group_rotation_info.clone();
				// Mirror the real runtime: `validator_groups` reports `now = block_number + 1`
				// (see `runtime_api_impl::v13::validator_groups`). The legacy validator side
				// trusts this `now` directly; the experimental side recomputes it. Off-by-one
				// here silently shifts every legacy core-ownership calculation by one block.
				rotation.now = info.number + 1;
				let _ = tx.send(Ok((session.validator_groups.clone(), rotation)));
			},
			RuntimeApiRequest::SchedulingLookahead(_session, tx) => {
				let _ = tx.send(Ok(self.scheduling_lookahead));
			},
			RuntimeApiRequest::AsyncBackingParams(tx) => {
				let _ = tx.send(Ok(self.async_backing_params));
			},
			RuntimeApiRequest::NodeFeatures(_session, tx) => {
				let _ = tx.send(Ok(self.node_features.clone()));
			},
			RuntimeApiRequest::MinimumBackingVotes(_session, tx) => {
				let _ = tx.send(Ok(self.minimum_backing_votes));
			},
			RuntimeApiRequest::BackingConstraints(para, tx) => {
				let constraints = self
					.backing_constraints_at
					.get(&(parent, para))
					.cloned()
					.or_else(|| self.backing_constraints.get(&para).cloned())
					.unwrap_or_else(default_constraints);
				let _ = tx.send(Ok(Some(constraints)));
			},
			RuntimeApiRequest::CandidatesPendingAvailability(para, tx) => {
				let candidates = self
					.pending_availability_at
					.get(&(parent, para))
					.cloned()
					.or_else(|| self.pending_availability.get(&para).cloned())
					.unwrap_or_default();
				let _ = tx.send(Ok(candidates));
			},
			RuntimeApiRequest::CandidateEvents(tx) => {
				let events =
					self.candidate_events.get(&parent).cloned().unwrap_or_default();
				let _ = tx.send(Ok(events));
			},
			RuntimeApiRequest::PersistedValidationData(_para, _assumption, tx) => {
				// Synthesise a PVD whose shape matches the seconding sanity check: parent
				// head = empty, relay parent number = block's number, storage root =
				// Hash::zero(), max_pov_size = 5 MB. Tests that need different shapes should
				// pre-set the candidate's persisted_validation_data_hash to match.
				let pvd = PersistedValidationData {
					parent_head: HeadData(Vec::new()),
					relay_parent_number: info.number,
					relay_parent_storage_root: Hash::zero(),
					max_pov_size: 5 * 1024 * 1024,
				};
				let _ = tx.send(Ok(Some(pvd)));
			},
			RuntimeApiRequest::DisabledValidators(tx) => {
				let _ = tx.send(Ok(Vec::new()));
			},
			RuntimeApiRequest::AvailabilityCores(tx) => {
				let _ = tx.send(Ok(Vec::new()));
			},
			RuntimeApiRequest::SessionInfo(_session, tx) => {
				let _ = tx.send(Ok(None));
			},
			RuntimeApiRequest::SessionExecutorParams(_session, tx) => {
				let _ = tx.send(Ok(None));
			},
			RuntimeApiRequest::ParaIds(_session, tx) => {
				// Derive registered paras from the global schedule + per-block claim-queue
				// overrides. Returning empty here triggers experimental's `prune_paras` to
				// wipe every score-store entry on every BlockFinalized — see
				// `peer_manager::PeerManager::prune_registered_paras`.
				let mut paras: std::collections::BTreeSet<ParaId> = self
					.schedule
					.values()
					.flat_map(|sched| sched.cycle.iter().copied())
					.collect();
				for queue in self.claim_queue_overrides.values() {
					for cores in queue.values() {
						paras.extend(cores.iter().copied());
					}
				}
				let _ = tx.send(Ok(paras.into_iter().collect()));
			},
			RuntimeApiRequest::ValidationCodeBombLimit(_session, tx) => {
				let _ = tx.send(Ok(60 * 1024 * 1024));
			},
			RuntimeApiRequest::MaxRelayParentSessionAge(_session, tx) => {
				let _ = tx.send(Ok(8));
			},
			RuntimeApiRequest::ValidationCodeByHash(_hash, tx) => {
				// Return a tiny valid-shaped ValidationCode payload. The
				// always-valid candidate-validation stub doesn't actually run it.
				let _ = tx.send(Ok(Some(polkadot_primitives::ValidationCode(vec![0x00]))));
			},
			RuntimeApiRequest::ValidationCode(_para, _assumption, tx) => {
				let _ = tx.send(Ok(Some(polkadot_primitives::ValidationCode(vec![0x00]))));
			},
			RuntimeApiRequest::ValidationCodeHash(_para, _assumption, tx) => {
				// Default: return None (no upgrade pending).
				let _ = tx.send(Ok(None));
			},
			RuntimeApiRequest::AncestorRelayParentInfo(_session, queried_relay_parent, tx) => {
				// If the configured `runtime_api_version` is below the requirement, the
				// real runtime would return `NotSupported`; mirror that so callers
				// exercise the chain-header fallback path.
				if self.runtime_api_version
					< RuntimeApiRequest::ANCESTOR_RELAY_PARENT_INFO_RUNTIME_REQUIREMENT
				{
					let _ = tx.send(Err(
						polkadot_node_subsystem::errors::RuntimeApiError::NotSupported {
							runtime_api_name: "AncestorRelayParentInfo",
						},
					));
				} else {
					// Return None if querying about self (a block isn't in its own
					// `AllowedRelayParents`); otherwise return the queried block's
					// RelayParentInfo when we know it. Other-session queries are
					// disambiguated by the runtime; our chain model only has one
					// session so we ignore the session arg.
					let answer = if queried_relay_parent == parent {
						None
					} else {
						self.blocks.get(&queried_relay_parent).map(|info| {
							polkadot_primitives::vstaging::RelayParentInfo {
								number: info.number,
								state_root: Hash::zero(),
							}
						})
					};
					let _ = tx.send(Ok(answer));
				}
			},
			other => panic!(
				"ChainModel does not implement RuntimeApiRequest::{:?} yet — extend the model when a subsystem starts asking for it",
				other
			),
		}
	}

	fn answer_chain_api(&self, msg: ChainApiMessage) {
		match msg {
			ChainApiMessage::BlockHeader(hash, tx) => {
				let header = self.blocks.get(&hash).map(BlockInfo::header);
				let _ = tx.send(Ok(header));
			},
			ChainApiMessage::BlockNumber(hash, tx) => {
				let number = self.blocks.get(&hash).map(|info| info.number);
				let _ = tx.send(Ok(number));
			},
			ChainApiMessage::Ancestors { hash, k, response_channel } => {
				let ancestors = self.ancestors(hash, k);
				let _ = response_channel.send(Ok(ancestors));
			},
			ChainApiMessage::FinalizedBlockNumber(tx) => {
				let n = self.blocks.get(&self.finalized).map(|info| info.number).unwrap_or(0);
				let _ = tx.send(Ok(n));
			},
			ChainApiMessage::FinalizedBlockHash(number, tx) => {
				// Resolve along the finalized chain, not by a global scan: with forks,
				// several blocks share a height, and only the one on the path to
				// `self.finalized` is actually finalized. Walk back from the finalized tip to
				// the requested height. A height above the finalized tip has no finalized
				// block yet → `None`, matching the real ChainApi.
				let hash = self.finalized_ancestor_at(number);
				let _ = tx.send(Ok(hash));
			},
			other => panic!(
				"ChainModel does not implement ChainApiMessage::{:?} yet — extend the model when a subsystem starts asking for it",
				other
			),
		}
	}
}

impl AnswerQuery for ChainModel {
	fn try_answer(&mut self, query: Query) -> Option<Query> {
		match query {
			Query::Runtime(msg) => {
				self.answer_runtime(msg);
				None
			},
			Query::ChainApi(msg) => {
				self.answer_chain_api(msg);
				None
			},
			other => Some(other),
		}
	}

	fn answer(&mut self, query: Query) {
		// Direct-answer surface for unit tests; declines fall through to a panic so test
		// authors notice when they hand the chain model a query it doesn't own.
		if let Some(declined) = self.try_answer(query) {
			panic!(
				"ChainModel does not handle non-runtime/chain-api queries; declined: {:?}",
				declined
			);
		}
	}
}

/// Shared ownership of a [`ChainModel`].
///
/// The harness installs this in its responder chain so the model's mutable state stays
/// accessible to the test (via [`SharedChain::lock`]) while queries the subsystem fires
/// against the responder are routed back into the same model.
#[derive(Clone, Debug)]
pub struct SharedChain(std::sync::Arc<std::sync::Mutex<ChainModel>>);

impl SharedChain {
	/// Wrap a chain model for shared use.
	pub fn new(chain: ChainModel) -> Self {
		Self(std::sync::Arc::new(std::sync::Mutex::new(chain)))
	}

	/// Lock the inner model. Panics if poisoned.
	pub fn lock(&self) -> std::sync::MutexGuard<'_, ChainModel> {
		self.0.lock().expect("ChainModel mutex poisoned")
	}
}

impl AnswerQuery for SharedChain {
	fn try_answer(&mut self, query: Query) -> Option<Query> {
		self.lock().try_answer(query)
	}

	fn answer(&mut self, query: Query) {
		self.lock().answer(query)
	}
}

/// Permissive default backing constraints. Tests that need stricter shapes pass their own
/// via [`ChainModel::set_backing_constraints`].
fn default_constraints() -> Constraints {
	Constraints {
		min_relay_parent_number: 0,
		max_pov_size: 5 * 1024 * 1024,
		max_code_size: 3 * 1024 * 1024,
		max_head_data_size: 20 * 1024,
		ump_remaining: 32,
		ump_remaining_bytes: 64 * 1024,
		max_ump_num_per_candidate: 16,
		dmp_remaining_messages: Vec::new(),
		// Real constraints reject candidates whose `hrmp_watermark` isn't in
		// `valid_watermarks` (and isn't strictly greater than the relay parent number).
		// Test commitments default to `hrmp_watermark = 0`; allow that explicitly so
		// real prospective accepts plain candidates as fragment-chain members.
		hrmp_inbound: InboundHrmpLimitations { valid_watermarks: vec![0] },
		hrmp_channels_out: Vec::new(),
		max_hrmp_num_per_candidate: 16,
		required_parent: HeadData(Vec::new()),
		// Match the validation code hash that `dummy_candidate_receipt_v2_bad_sig`
		// (used by `Candidate::for_para_at` / `Candidate::builder`) bakes into
		// every receipt's descriptor. Otherwise real prospective-parachains rejects every
		// candidate as a `ValidationCodeMismatch` against constraints.
		validation_code_hash: polkadot_primitives_test_helpers::dummy_validation_code().hash(),
		upgrade_restriction: None,
		future_validation_code: None,
	}
}

fn synthetic_child_hash_with_sibling(parent: Hash, number: BlockNumber, sibling: u64) -> Hash {
	// Deterministic child hash by mixing parent low-u64 with the child number and the
	// sibling index. Tests do not assert on the exact value; identity is what matters.
	// Sibling index is shifted into a higher byte so primary children retain the
	// historical hash for backwards compatibility with existing tests.
	let parent_low = parent.to_low_u64_be();
	Hash::from_low_u64_be(parent_low.wrapping_add(0x100_0000 + number as u64 + (sibling << 32)))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn empty_session() -> SessionInfo {
		SessionInfo {
			validators: Vec::new(),
			validator_groups: Vec::new(),
			group_rotation_info: GroupRotationInfo {
				session_start_block: 0,
				group_rotation_frequency: 1,
				now: 0,
			},
		}
	}

	#[test]
	fn extend_grows_chain_and_advances_slot() {
		let mut chain = ChainModel::new(Slot::from(100));
		let g = chain.genesis();
		let a = chain.extend(g);
		let b = chain.extend(a);
		assert_eq!(chain.block(&a).unwrap().parent_hash, g);
		assert_eq!(chain.block(&b).unwrap().parent_hash, a);
		assert_eq!(chain.block(&a).unwrap().number, 1);
		assert_eq!(chain.block(&b).unwrap().number, 2);
		assert_eq!(chain.block(&a).unwrap().slot, Slot::from(101));
		assert_eq!(chain.block(&b).unwrap().slot, Slot::from(102));
		assert_eq!(chain.tip(), b);
	}

	#[test]
	fn ancestors_walks_back_until_genesis() {
		let mut chain = ChainModel::new(Slot::from(100));
		let a = chain.extend(chain.genesis());
		let b = chain.extend(a);
		let c = chain.extend(b);
		let anc = chain.ancestors(c, 5);
		assert_eq!(anc, vec![b, a, chain.genesis()]);
	}

	#[test]
	fn runtime_session_index_for_child_returns_block_session() {
		let mut chain = ChainModel::new(Slot::from(0));
		chain.add_session(0, empty_session());
		let leaf = chain.extend(chain.genesis());
		let (tx, rx) = futures::channel::oneshot::channel();
		chain.answer(Query::Runtime(RuntimeApiMessage::Request(
			leaf,
			RuntimeApiRequest::SessionIndexForChild(tx),
		)));
		let got = futures::executor::block_on(rx).unwrap().unwrap();
		assert_eq!(got, 0);
	}

	#[test]
	fn runtime_validator_groups_uses_session_info() {
		let mut chain = ChainModel::new(Slot::from(0));
		let mut session = empty_session();
		session.validator_groups = vec![vec![ValidatorIndex(0)]];
		chain.add_session(0, session);
		let leaf = chain.extend(chain.genesis());
		let (tx, rx) = futures::channel::oneshot::channel();
		chain.answer(Query::Runtime(RuntimeApiMessage::Request(
			leaf,
			RuntimeApiRequest::ValidatorGroups(tx),
		)));
		let (groups, rotation) = futures::executor::block_on(rx).unwrap().unwrap();
		assert_eq!(groups, vec![vec![ValidatorIndex(0)]]);
		assert_eq!(rotation.now, 2); // leaf number (1) + 1, mirroring the real runtime.
	}

	#[test]
	fn runtime_claim_queue_returns_per_block_queue() {
		let mut chain = ChainModel::new(Slot::from(0));
		chain.add_session(0, empty_session());
		let leaf = chain.extend(chain.genesis());
		let mut q: BTreeMap<CoreIndex, VecDeque<ParaId>> = BTreeMap::new();
		q.insert(CoreIndex(0), VecDeque::from_iter(std::iter::repeat(ParaId::from(2000)).take(3)));
		chain.set_claim_queue_at(leaf, q.clone());
		let (tx, rx) = futures::channel::oneshot::channel();
		chain.answer(Query::Runtime(RuntimeApiMessage::Request(
			leaf,
			RuntimeApiRequest::ClaimQueue(tx),
		)));
		let got = futures::executor::block_on(rx).unwrap().unwrap();
		assert_eq!(got, q);
	}

	#[test]
	fn schedule_derives_correct_per_block_queue() {
		let mut chain = ChainModel::new(Slot::from(0));
		chain.add_session(0, empty_session());
		let para_a = ParaId::from(2000);
		let para_b = ParaId::from(3000);
		// Cycle [A, B, A, B, ...]: para_a at even block numbers, para_b at odd.
		chain.set_core_schedule(CoreIndex(0), CoreSchedule::cycling(vec![para_a, para_b]));
		let a = chain.extend(chain.genesis()); // block 1
		let b = chain.extend(a); // block 2

		// Queue at block 1 with lookahead=3: positions [1, 2, 3] → [B, A, B].
		let (tx, rx) = futures::channel::oneshot::channel();
		chain.answer(Query::Runtime(RuntimeApiMessage::Request(
			a,
			RuntimeApiRequest::ClaimQueue(tx),
		)));
		let got = futures::executor::block_on(rx).unwrap().unwrap();
		assert_eq!(got.get(&CoreIndex(0)).unwrap(), &VecDeque::from(vec![para_b, para_a, para_b]));

		// Queue at block 2: positions [2, 3, 4] → [A, B, A].
		let (tx, rx) = futures::channel::oneshot::channel();
		chain.answer(Query::Runtime(RuntimeApiMessage::Request(
			b,
			RuntimeApiRequest::ClaimQueue(tx),
		)));
		let got = futures::executor::block_on(rx).unwrap().unwrap();
		assert_eq!(got.get(&CoreIndex(0)).unwrap(), &VecDeque::from(vec![para_a, para_b, para_a]));
	}

	#[test]
	fn chain_api_block_header_round_trips_slot() {
		let mut chain = ChainModel::new(Slot::from(42));
		let leaf = chain.extend(chain.genesis());
		let (tx, rx) = futures::channel::oneshot::channel();
		chain.answer(Query::ChainApi(ChainApiMessage::BlockHeader(leaf, tx)));
		let header = futures::executor::block_on(rx).unwrap().unwrap().expect("header present");
		// Extract slot back out.
		let slot = header
			.digest
			.logs()
			.iter()
			.find_map(|log| log.as_babe_pre_digest())
			.expect("BABE pre-digest present")
			.slot();
		assert_eq!(slot, Slot::from(43)); // genesis slot 42 + 1.
	}

	#[test]
	fn chain_api_ancestors_returns_walk() {
		let mut chain = ChainModel::new(Slot::from(0));
		let a = chain.extend(chain.genesis());
		let b = chain.extend(a);
		let (tx, rx) = futures::channel::oneshot::channel();
		chain.answer(Query::ChainApi(ChainApiMessage::Ancestors {
			hash: b,
			k: 2,
			response_channel: tx,
		}));
		let got = futures::executor::block_on(rx).unwrap().unwrap();
		assert_eq!(got, vec![a, chain.genesis()]);
	}
}
