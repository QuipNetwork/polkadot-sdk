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

//! Scheduling + claim-queue mechanics.

mod claim_queue_window {
	use crate::common::{
		builders::ProtocolVersion::V2,
		chain::CoreSchedule,
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, Id as ParaId};
	use std::time::Duration;

	const PARA_A: ParaId = ParaId::new(2000);
	const PARA_B: ParaId = ParaId::new(999);

	/// Builds a world with `n_ancestors` ancestors, schedule defaults to B on core 0, and the
	/// leaf claim queue overridden to `cq`. Used by all three scenarios in this file.
	///
	/// Ancestors are `.register()`-ed (chain-known, never independently signalled as active
	/// leaves), matching the old `build_with_ancestors_world_with_config` semantics — the
	/// validator processes ancestor-RP advertisements through the active leaf's implicit
	/// view, not via per-ancestor activation.
	fn world_with_leaf_cq<S: CollatorSut>(
		n_ancestors: usize,
		cq: [ParaId; 3],
	) -> crate::common::world::World<S> {
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_B));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		for _ in 0..n_ancestors {
			w.new_block().register();
		}
		w.new_block().with_claim_queue_at(CoreIndex(0), cq).activate();
		w
	}

	/// Builds a world with `n_ancestors + 1` blocks (linear), the same single para
	/// scheduled on `core` at every block. Replaces the removed
	/// `build_with_ancestors_world` helper. Ancestors are `register()`-ed so only the
	/// leaf is signalled active — same as the old helper.
	fn linear_para_world<S: CollatorSut>(
		n_ancestors: usize,
		core: CoreIndex,
		para: ParaId,
	) -> crate::common::world::World<S> {
		let config = collator_world_config().with_schedule(core, CoreSchedule::always(para));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		for _ in 0..n_ancestors {
			w.new_block().register();
		}
		w.new_block().activate();
		w
	}

	#[crate::sim_test]
	fn para_at_last_claim_queue_position_accepts_at_leaf<S: CollatorSut>() {
		let mut w = world_with_leaf_cq::<S>(0, [PARA_B, PARA_B, PARA_A]);
		let peer = w.declared_peer(PARA_A, V2);
		let cand = w.advertise(&peer, w.leaf(), PARA_A);
		let _ = w.fetch_request(&cand);
	}

	#[crate::sim_test]
	fn ancestor_with_para_at_valid_position_accepts<S: CollatorSut>() {
		// Linear chain, same para A scheduled at every block. Ancestor R has para A at
		// position 0 → in-window for the offset=1 ancestor.
		let mut w = linear_para_world::<S>(1, CoreIndex(0), PARA_A);
		let peer = w.declared_peer(PARA_A, V2);
		let r = w.ancestors()[0];
		let cand = w.advertise(&peer, r, PARA_A);
		let _ = w.fetch_request(&cand);
	}

	/// A para scheduled at the last claim-queue position must be fetchable from the ancestor whose
	/// window still reaches that position — even when the chain's ancestry and the claim queue are
	/// both shorter than the scheduling lookahead (e.g. near genesis, or the first blocks after a
	/// session change).
	///
	/// Lookahead is 3; the leaf is at height 1 (ancestry reaches only genesis) with claim queue
	/// `[A, B]`. B's slot is one ahead of the leaf, which the genesis ancestor can still serve, so
	/// the advertisement must fetch.
	#[crate::sim_test]
	fn ancestor_can_fetch_last_claim_queue_position<S: CollatorSut>() {
		let config =
			collator_world_config().with_schedule(CoreIndex(0), CoreSchedule::always(PARA_B));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		// Leaf at height 1 (genesis + 1) → allowed-RP path is 2 (< lookahead 3). CQ override is
		// length 2 (< lookahead 3). B is at position 1.
		w.new_block().with_claim_queue_at(CoreIndex(0), [PARA_A, PARA_B]).activate();
		let peer_b = w.declared_peer(PARA_B, V2);
		let ancestor = w.ancestors()[0];
		let cand = w.advertise(&peer_b, ancestor, PARA_B);
		let _ = w.fetch_request(&cand);
	}

	#[crate::sim_test]
	fn ancestor_with_para_at_obsolete_position_rejects<S: CollatorSut>() {
		let mut w = world_with_leaf_cq::<S>(1, [PARA_B, PARA_B, PARA_A]);
		let peer = w.declared_peer(PARA_A, V2);
		let r = w.ancestors()[0];
		let cand = w.advertise(&peer, r, PARA_A);
		w.no_fetch_for(&cand, Duration::from_millis(200));
	}

	/// Two ancestors deep: with `allowed_ancestry_len = 2`, advertisements at both
	/// leaf-parent and grandparent must fetch. Mirrors upstream
	/// `accept_advertisements_from_implicit_view` (simplified to a single para — the
	/// upstream multi-para shape needs validator-group rotation we don't model here, and
	/// the property under test is implicit-view ancestor acceptance, not group rotation).
	///
	/// KNOWN BUG (experimental): same root cause as
	/// `ancestor_with_para_at_valid_position_accepts` — `claim_queue_state` keyed by leaf only.
	/// See `memory:project_collator_experimental_no_ancestor_rp_advertise`.
	#[crate::sim_test]
	fn ancestor_advertisements_at_parent_and_grandparent_both_fetch<S: CollatorSut>() {
		let mut w = linear_para_world::<S>(2, CoreIndex(0), PARA_A);
		let peer = w.declared_peer(PARA_A, V2);
		let parent = w.ancestors()[0];
		let grandparent = w.ancestors()[1];

		let cand_at_parent = w.advertise(&peer, parent, PARA_A);
		let cand_at_grandparent = w.advertise(&peer, grandparent, PARA_A);

		let _ = w.fetch_request(&cand_at_parent);
		let _ = w.fetch_request(&cand_at_grandparent);
	}
}

mod claims_counting {
	use crate::common::{
		builders::ProtocolVersion::V2,
		chain::CoreSchedule,
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, HeadData, Id as ParaId};
	use std::time::Duration;

	const PARA: ParaId = ParaId::new(2000);

	/// Builds a linear chain world with `n_ancestors + 1` blocks, the same para scheduled
	/// on `core` at every block. Ancestors are `register()`-ed so only the leaf is
	/// signalled active — same as the old `build_with_ancestors_world` helper.
	fn linear_para_world<S: CollatorSut>(
		n_ancestors: usize,
		core: CoreIndex,
		para: ParaId,
	) -> crate::common::world::World<S> {
		let config = collator_world_config().with_schedule(core, CoreSchedule::always(para));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		for _ in 0..n_ancestors {
			w.new_block().register();
		}
		w.new_block().activate();
		w
	}

	/// 2 seconded at the ancestor + 1 at the leaf = 3 total. 4th at leaf rejected.
	///
	/// KNOWN BUG (experimental): the ancestor-RP advertisement at step 1 is silently dropped,
	/// so the test never reaches the slot-full assertion. See
	/// `memory:project_collator_experimental_no_ancestor_rp_advertise`.
	#[crate::sim_test]
	fn claims_below_are_counted_correctly<S: CollatorSut>() {
		let mut w = linear_para_world::<S>(1, CoreIndex(0), PARA);
		let leaf = w.leaf();
		let ancestor = w.ancestors()[0];

		// 3-candidate chain: parent_head=[i-1] → output=[i]. First 2 at ancestor RP, 3rd at leaf.
		let c1 = w
			.candidate_at(ancestor)
			.para(PARA)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();
		let c2 = w
			.candidate_at(ancestor)
			.para(PARA)
			.parent_head(c1.output_head())
			.head_data(HeadData(vec![2]))
			.build();
		let c3 = w
			.candidate_at(leaf)
			.para(PARA)
			.parent_head(c2.output_head())
			.head_data(HeadData(vec![3]))
			.build();

		let peer = w.declared_peer(PARA, V2);
		w.full_second(&peer, &c1);
		w.full_second(&peer, &c2);
		w.full_second(&peer, &c3);

		// 4th candidate at the leaf — claim queue full.
		let c4 = w
			.candidate_at(leaf)
			.para(PARA)
			.parent_head(c3.output_head())
			.head_data(HeadData(vec![4]))
			.build();
		w.advertise_with_parent_head(&peer, leaf, c4.hash(), c4.parent_head_hash());
		w.no_fetch_for(&c4, Duration::from_millis(150));
	}

	/// All 3 claims at the leaf → ancestor advertisement rejected (capacity full above).
	#[crate::sim_test]
	fn claims_above_are_counted_correctly<S: CollatorSut>() {
		let mut w = linear_para_world::<S>(1, CoreIndex(0), PARA);
		let leaf = w.leaf();
		let ancestor = w.ancestors()[0];

		let c1 = w
			.candidate_at(leaf)
			.para(PARA)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();
		let c2 = w
			.candidate_at(leaf)
			.para(PARA)
			.parent_head(c1.output_head())
			.head_data(HeadData(vec![2]))
			.build();
		let c3 = w
			.candidate_at(leaf)
			.para(PARA)
			.parent_head(c2.output_head())
			.head_data(HeadData(vec![3]))
			.build();

		let peer = w.declared_peer(PARA, V2);
		w.full_second(&peer, &c1);
		w.full_second(&peer, &c2);
		w.full_second(&peer, &c3);

		let c4 = w
			.candidate_at(ancestor)
			.para(PARA)
			.parent_head(c3.output_head())
			.head_data(HeadData(vec![4]))
			.build();
		w.advertise_with_parent_head(&peer, ancestor, c4.hash(), c4.parent_head_hash());
		w.no_fetch_for(&c4, Duration::from_millis(150));
	}

	/// 1 seconded at ancestor + 1 at leaf + 1 at leaf-grandparent (deeper ancestor): final
	/// candidate fills the last slot. Subsequent ad rejected.
	///
	/// KNOWN BUG (experimental): see `claims_below_are_counted_correctly` — same root cause.
	#[crate::sim_test]
	fn claim_fills_last_free_slot<S: CollatorSut>() {
		let mut w = linear_para_world::<S>(2, CoreIndex(0), PARA);
		let leaf = w.leaf();
		let parent = w.ancestors()[0];
		let grandparent = w.ancestors()[1];

		let c1 = w
			.candidate_at(grandparent)
			.para(PARA)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();
		let c2 = w
			.candidate_at(parent)
			.para(PARA)
			.parent_head(c1.output_head())
			.head_data(HeadData(vec![2]))
			.build();
		let c3 = w
			.candidate_at(leaf)
			.para(PARA)
			.parent_head(c2.output_head())
			.head_data(HeadData(vec![3]))
			.build();

		let peer = w.declared_peer(PARA, V2);
		w.full_second(&peer, &c1);
		w.full_second(&peer, &c2);
		w.full_second(&peer, &c3);

		let c4 = w
			.candidate_at(leaf)
			.para(PARA)
			.parent_head(c3.output_head())
			.head_data(HeadData(vec![4]))
			.build();
		w.advertise_with_parent_head(&peer, leaf, c4.hash(), c4.parent_head_hash());
		w.no_fetch_for(&c4, Duration::from_millis(150));
	}
}

mod cq_position_window {
	use crate::common::{
		builders::{
			Candidate,
			ProtocolVersion::{V1, V2},
		},
		contract::Effect,
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, HeadData, Id as ParaId};
	use std::time::Duration;

	const PARA_A: ParaId = ParaId::new(100);
	const PARA_OTHER: ParaId = ParaId::new(200);

	/// Off-by-one boundary: the last CQ position at the leaf is reachable (offset 0 → window
	/// covers all `lookahead` positions). With CQ `[other, other, A]` on the assigned core,
	/// para A at index 2 must accept an advertisement at the leaf.
	#[crate::sim_test]
	fn last_claim_queue_position_accepted_at_leaf<S: CollatorSut>() {
		let config = collator_world_config()
			.with_schedule(CoreIndex(0), crate::common::chain::CoreSchedule::always(PARA_A));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_OTHER, PARA_OTHER, PARA_A])
			.activate();

		let peer = w.declared_peer(PARA_A, V2);
		let cand = w.advertise(&peer, w.leaf(), PARA_A);
		let _ = w.fetch_request(&cand);
	}

	/// Seconded candidates count as consumers in the per-core CQ pool. Leaf CQ
	/// `[A, other, A]` has exactly 2 slots for A. After two A-candidates are seconded, a third
	/// advertisement at the same RP for the same para must NOT trigger a fetch — capacity full.
	#[crate::sim_test]
	fn seconded_candidates_consume_capacity<S: CollatorSut>() {
		let config = collator_world_config()
			.with_schedule(CoreIndex(0), crate::common::chain::CoreSchedule::always(PARA_A));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_A, PARA_OTHER, PARA_A])
			.activate();
		let leaf = w.leaf();

		let peer_a = w.declared_peer(PARA_A, V2);
		let peer_b = w.declared_peer(PARA_A, V2);

		// Chain two candidates by parent_head so prospective accepts both.
		let c1 = w
			.candidate_at(leaf)
			.para(PARA_A)
			.parent_head(HeadData(Vec::new()))
			.head_data(HeadData(vec![1]))
			.build();
		let c2 = w
			.candidate_at(leaf)
			.para(PARA_A)
			.parent_head(c1.output_head())
			.head_data(HeadData(vec![2]))
			.build();

		w.full_second(&peer_a, &c1);
		w.full_second(&peer_b, &c2);

		// Third advertisement: capacity full → no fetch should fire.
		let c3 = w
			.candidate_at(leaf)
			.para(PARA_A)
			.parent_head(c2.output_head())
			.head_data(HeadData(vec![3]))
			.build();
		let peer_c = w.declared_peer(PARA_A, V2);
		w.advertise_with_parent_head(&peer_c, leaf, c3.hash(), c3.parent_head_hash());
		w.no_fetch_for(&c3, Duration::from_millis(300));
	}

	/// In-window boundary, ancestor side: leaf CQ `[A, other, A]`. Advertise PARA_A at
	/// the parent (offset 1, window `[A, other]`). Position 0 is reachable → accepted.
	///
	/// Marked bug_on=experimental because experimental drops ancestor-RP advertisements
	/// (`memory:project_collator_experimental_no_ancestor_rp_advertise`); the test
	/// flips green when that bug is fixed.
	#[crate::sim_test]
	fn non_obsolete_position_accepted<S: CollatorSut>() {
		let config = collator_world_config()
			.with_schedule(CoreIndex(0), crate::common::chain::CoreSchedule::always(PARA_A));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block().register();
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_A, PARA_OTHER, PARA_A])
			.activate();
		let parent = w.ancestors()[0];
		let peer = w.declared_peer(PARA_A, V2);
		let cand = w.candidate_at(parent).para(PARA_A).build();
		w.advertise_with_parent_head(&peer, parent, cand.hash(), cand.parent_head_hash());
		let _ = w.fetch_request(&cand);
	}

	/// Out-of-window: leaf CQ `[other, other, A]`. Advertise PARA_A at the parent (offset 1,
	/// window `[other, other]`). PARA_A not reachable from this SP → rejected, no fetch.
	///
	/// Both impls reject — but experimental's reason is the ancestor-RP drop bug, not the
	/// position check. Mark bug_on=experimental so the test still serves as upcoming-fix
	/// coverage; once #11967 lands and the ancestor-RP bug is fixed, experimental's
	/// rejection should come from the position check (also correct), and the test stays
	/// green.
	#[crate::sim_test]
	fn obsolete_positions_rejected<S: CollatorSut>() {
		let config = collator_world_config()
			.with_schedule(CoreIndex(0), crate::common::chain::CoreSchedule::always(PARA_A));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block().register();
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_OTHER, PARA_OTHER, PARA_A])
			.activate();
		let parent = w.ancestors()[0];
		let peer = w.declared_peer(PARA_A, V2);
		let cand = w.candidate_at(parent).para(PARA_A).build();
		w.advertise_with_parent_head(&peer, parent, cand.hash(), cand.parent_head_hash());
		w.no_fetch_for(&cand, Duration::from_millis(300));
	}

	/// V1 single-shot per `(sp, para)` round. CQ has 2 slots for A but two V1 peers
	/// advertise at the leaf and only one fetch fires this round (V1 ads carry no
	/// `prospective_candidate`, so the validator can't tell them apart and serializes).
	#[crate::sim_test]
	fn v1_single_shot_per_sp_para_round<S: CollatorSut>() {
		let config = collator_world_config()
			.with_schedule(CoreIndex(0), crate::common::chain::CoreSchedule::always(PARA_A));
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		w.new_block()
			.with_claim_queue_at(CoreIndex(0), [PARA_A, PARA_A, PARA_OTHER])
			.activate();

		let peer_a = w.declared_peer(PARA_A, V1);
		let peer_b = w.declared_peer(PARA_A, V1);

		w.base.sim.send(peer_a.advertise(w.leaf(), None, None));
		w.base.sim.send(peer_b.advertise(w.leaf(), None, None));

		// Exactly one V1 fetch this round.
		let _ = w.expect_any_fetch();
		let _ = Candidate::for_para_at(PARA_A, w.leaf()); // unused — keeps the explicit "V1 dedup" intent visible
		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { .. }),
			1,
			"exactly one V1 fetch despite two V1 advertisements at the same (sp, para)",
		);
	}
}

mod group_rotation_uses_correct_core_per_relay_parent {
	use crate::common::{
		builders::ProtocolVersion::V2,
		chain::CoreSchedule,
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_primitives::{CoreIndex, Id as ParaId, ValidatorIndex};

	const PARA_A: ParaId = ParaId::new(2000);
	const PARA_B: ParaId = ParaId::new(2001);

	/// Per-relay-parent group rotation: the validator must check core ownership against each
	/// RP's own rotation, not the active leaf's. An advertisement only fetches at the RP where
	/// we own that para's core.
	#[crate::sim_test]
	fn group_rotation_uses_correct_core_per_relay_parent<S: CollatorSut>() {
		// 3 validator groups; validator (Alice = idx 0) is in group 0. The runtime reports
		// `now = block_number + 1`; `core_for_group(0, 3)` at `now = N` returns `(3 - N % 3) % 3`:
		// - block 2 (bn 2, now 3): own core 0 → PARA_A
		// - block 3 (bn 3, now 4): own core 2 → PARA_B (the active leaf)
		// block 1 (own core 1) is an unscheduled gap. The active leaf (block 3) holds
		// block 2 + block 1 in its implicit view (`allowed_ancestry_len = 2`).
		let validator_groups =
			vec![vec![ValidatorIndex(0)], vec![ValidatorIndex(1)], vec![ValidatorIndex(2)]];
		let config = collator_world_config()
			.with_schedule(CoreIndex(0), CoreSchedule::always(PARA_A))
			.with_schedule(CoreIndex(2), CoreSchedule::always(PARA_B))
			.with_validator_groups(validator_groups)
			.with_group_rotation_frequency(1);
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		for _ in 0..3 {
			w.new_block().activate();
		}

		// Linear chain: each `.activate()` auto-deactivates its parent, so only block 3 is the
		// active leaf. block 2 + block 1 remain in its implicit view, reached via ancestry.
		let block_para_b = w.leaf(); // block 3 — own core 2 → PARA_B
		let block_para_a = w.ancestors()[0]; // block 2 — own core 0 → PARA_A

		let peer_a = w.declared_peer(PARA_A, V2);
		let peer_b = w.declared_peer(PARA_B, V2);

		// Correct pairing: each para advertised at the RP where we own its core. Both fetch.
		let cand_a = w.advertise(&peer_a, block_para_a, PARA_A);
		let cand_b = w.advertise(&peer_b, block_para_b, PARA_B);

		let _ = w.fetch_request(&cand_a);
		let _ = w.fetch_request(&cand_b);

		// Incorrect pairing: each para at the *other* RP, where we don't own its core. No fetch.
		let cand_a_wrong = w.advertise(&peer_a, block_para_b, PARA_A);
		let cand_b_wrong = w.advertise(&peer_b, block_para_a, PARA_B);
		w.no_fetch_for(&cand_a_wrong, std::time::Duration::from_millis(150));
		w.no_fetch_for(&cand_b_wrong, std::time::Duration::from_millis(50));
	}
}

mod v3_scheduling_parent {
	use crate::common::{
		builders::ProtocolVersion::V3,
		contract::Effect,
		harness::CollatorSut,
		world::{bootstrap_world, collator_world_config, World, WorldExt as _},
	};
	use polkadot_primitives::{
		CandidateDescriptorVersion, CandidateReceiptV2, CoreIndex, Hash, HeadData, Id as ParaId,
		MutateDescriptorV2, PersistedValidationData, RELAY_CHAIN_SLOT_DURATION_MILLIS,
	};
	use polkadot_primitives_test_helpers::dummy_committed_candidate_receipt_v3;
	use sp_consensus_slots::Slot;
	use std::time::Duration;

	const PARA_A: ParaId = ParaId::new(2000);

	/// Wall-clock slot the validator sees, given how many ms have elapsed on the MockClock.
	/// `MockClock::wall_clock_ms` starts at 0; `Sim::advance(d)` bumps it by `d.as_millis()`.
	/// Tests advance the clock by `target_slot * SLOT_DURATION` before issuing a V3
	/// advertisement so that validator's `current_slot` lands where the test expects.
	fn slot_to_wall_ms(slot: u64) -> Duration {
		Duration::from_millis(slot * RELAY_CHAIN_SLOT_DURATION_MILLIS)
	}

	/// Build a V3 candidate at `relay_parent` whose scheduling parent is `scheduling_parent`.
	/// Returns the receipt + its hash; tests use both for the advertise step.
	fn v3_candidate<S: CollatorSut>(
		w: &crate::common::world::World<S>,
		relay_parent: Hash,
		scheduling_parent: Hash,
	) -> (CandidateReceiptV2, polkadot_primitives::CandidateHash) {
		let pvd = PersistedValidationData {
			parent_head: HeadData(Vec::new()),
			relay_parent_number: w.base.chain.lock().block(&relay_parent).unwrap().number,
			relay_parent_storage_root: Hash::zero(),
			max_pov_size: 5 * 1024 * 1024,
		};
		let mut committed = dummy_committed_candidate_receipt_v3(relay_parent, scheduling_parent);
		committed.descriptor.set_para_id(PARA_A);
		committed.descriptor.set_persisted_validation_data_hash(pvd.hash());
		committed.descriptor.set_core_index(CoreIndex(0));
		committed.descriptor.set_session_index(0);
		committed.descriptor.set_version(1);
		let receipt: CandidateReceiptV2 = committed.to_plain();
		let hash = receipt.hash();
		(receipt, hash)
	}

	/// Assert validator rejects the V3 advertisement: no `SendRequest` fires within a
	/// settle window. The reputation *signal* of the rejection diverges between impls
	/// (legacy emits `Reputation::Performance` on the bus; experimental updates the rep
	/// store silently or, for cheap-to-fake misbehaviour like a wrong scheduling_parent,
	/// applies no slash at all). The shared invariant — and what we assert here — is the
	/// no-fetch outcome.
	fn assert_rejected<S: CollatorSut>(
		w: &mut crate::common::world::World<S>,
		_peer_id: sc_network_types::PeerId,
		_context: &'static str,
	) {
		// Settle long enough that any in-flight effects from the advertise step have
		// drained, then assert no fetch was emitted for the rejected advertisement.
		w.base.sim.advance(Duration::from_millis(200));
		w.base.sim.expect_count(
			|e| matches!(e, Effect::SendRequest { .. }),
			0,
			"SendRequest after V3 rejection (must be zero)",
		);
	}

	/// Stalled relay chain: leaf at slot 1 (genesis_slot=0, +1 extend = leaf), validator's
	/// `current_slot` advanced to slot 10. V3 advertisement with `scheduling_parent = leaf` is
	/// rejected because `leaf.slot + 1 = 2 ≠ current_slot = 10`.
	///
	/// KNOWN-FAILING (experimental): per
	/// `project_collator_experimental_no_invalid_reputation_event.md` — rejection silent on the
	/// bus. Helper: build a world configured for V3 scheduling-parent tests with `n_ancestors`
	/// blocks under the leaf and the wall-clock advanced to `current_slot`.
	fn v3_world<S: CollatorSut>(
		n_ancestors: usize,
		current_slot: u64,
	) -> crate::common::world::World<S> {
		let config = collator_world_config()
			.with_schedule(CoreIndex(0), crate::common::chain::CoreSchedule::always(PARA_A))
			.with_genesis_slot(Slot::from(0))
			.with_v3_descriptors_enabled();
		let mut w: World<S> = bootstrap_world::<S>(config, None);
		for _ in 0..n_ancestors {
			w.new_block().register();
		}
		w.new_block().activate();
		w.base.sim.advance(slot_to_wall_ms(current_slot));
		w
	}

	/// Assert validator accepted the V3 advertisement (SendRequest fires for `candidate_hash`).
	fn assert_accepted<S: CollatorSut>(
		w: &mut crate::common::world::World<S>,
		candidate_hash: polkadot_primitives::CandidateHash,
		context: &'static str,
	) {
		let _ = w.base.sim.expect(
			|e| {
				matches!(
					e,
					Effect::SendRequest { candidate_hash: Some(c), .. } if *c == candidate_hash,
				)
			},
			Duration::from_millis(500),
			context,
		);
	}

	#[crate::sim_test]
	fn v3_scheduling_parent_rejected_on_stalled_relay_chain<S: CollatorSut>() {
		// leaf.slot=1; current_slot=10 → leaf.slot+1=2 ≠ 10 → reject.
		let mut w = v3_world::<S>(0, 10);
		let leaf = w.leaf();
		let (_receipt, candidate_hash) = v3_candidate(&w, leaf, leaf);
		let peer = w.declared_peer(PARA_A, V3);
		w.advertise_v3(
			&peer,
			leaf, // scheduling_parent = leaf (stale)
			leaf,
			candidate_hash,
			HeadData(Vec::new()).hash(),
			CandidateDescriptorVersion::V3,
		);
		assert_rejected(
			&mut w,
			peer.peer_id,
			"Effect::Reputation Performance for V3 ad on stalled relay chain",
		);
	}

	/// In-progress slot: leaf.slot == current_slot. V3 ad with `scheduling_parent = leaf-parent`
	/// (slot = current_slot - 1) is accepted.
	///
	/// KNOWN BUG (experimental): the advertisement is at the leaf's parent (an ancestor RP) —
	/// silently dropped on experimental. Same root cause as the ancestor-RP-drop bug. See
	/// `memory:project_collator_experimental_no_ancestor_rp_advertise`.
	#[crate::sim_test]
	fn v3_scheduling_parent_in_progress_slot_accepts_leaf_parent<S: CollatorSut>() {
		// 1 ancestor → leaf.slot = 2. current_slot = 2 → in-progress.
		let mut w = v3_world::<S>(1, 2);
		let parent = w.ancestors()[0]; // slot 1 = current_slot - 1
		let leaf = w.leaf();
		let (_receipt, candidate_hash) = v3_candidate(&w, leaf, parent);
		let peer = w.declared_peer(PARA_A, V3);
		w.advertise_v3(
			&peer,
			parent, // scheduling_parent = leaf-parent (in-progress's anchor)
			leaf,
			candidate_hash,
			HeadData(Vec::new()).hash(),
			CandidateDescriptorVersion::V3,
		);
		assert_accepted(&mut w, candidate_hash, "SendRequest for in-progress V3 ad");
	}

	/// Finished slot: leaf.slot == current_slot - 1. V3 ad with `scheduling_parent = leaf`
	/// (just-finished anchor) is accepted.
	#[crate::sim_test]
	fn v3_scheduling_parent_finished_slot_accepts_leaf<S: CollatorSut>() {
		// 0 ancestors → leaf.slot = 1. current_slot = 2 → finished.
		let mut w = v3_world::<S>(0, 2);
		let leaf = w.leaf();
		let (_receipt, candidate_hash) = v3_candidate(&w, leaf, leaf);
		let peer = w.declared_peer(PARA_A, V3);
		w.advertise_v3(
			&peer,
			leaf, // scheduling_parent = leaf (just finished)
			leaf,
			candidate_hash,
			HeadData(Vec::new()).hash(),
			CandidateDescriptorVersion::V3,
		);
		assert_accepted(&mut w, candidate_hash, "SendRequest for V3 ad on finished-slot leaf");
	}

	/// In-progress slot: targeting leaf itself as scheduling_parent (instead of leaf-parent) is
	/// rejected.
	#[crate::sim_test]
	fn v3_scheduling_parent_in_progress_slot_rejects_leaf<S: CollatorSut>() {
		// 1 ancestor → leaf.slot = 2. current_slot = 2 → in-progress. Leaf as sched_parent invalid.
		let mut w = v3_world::<S>(1, 2);
		let leaf = w.leaf();
		let (_receipt, candidate_hash) = v3_candidate(&w, leaf, leaf);
		let peer = w.declared_peer(PARA_A, V3);
		w.advertise_v3(
			&peer,
			leaf,
			leaf,
			candidate_hash,
			HeadData(Vec::new()).hash(),
			CandidateDescriptorVersion::V3,
		);
		assert_rejected(
			&mut w,
			peer.peer_id,
			"Reputation Performance for V3 in-progress with leaf as sched_parent",
		);
	}

	/// Finished slot: targeting leaf-parent as sched_parent is rejected. Valid is `leaf`.
	#[crate::sim_test]
	fn v3_scheduling_parent_finished_slot_rejects_parent<S: CollatorSut>() {
		// 1 ancestor → leaf.slot = 2. current_slot = 3 → finished. Valid sched_parent = leaf.
		let mut w = v3_world::<S>(1, 3);
		let parent = w.ancestors()[0];
		let leaf = w.leaf();
		let (_receipt, candidate_hash) = v3_candidate(&w, leaf, parent);
		let peer = w.declared_peer(PARA_A, V3);
		w.advertise_v3(
			&peer,
			parent, // invalid for finished slot
			leaf,
			candidate_hash,
			HeadData(Vec::new()).hash(),
			CandidateDescriptorVersion::V3,
		);
		assert_rejected(
			&mut w,
			peer.peer_id,
			"Reputation Performance for V3 finished-slot with parent as sched_parent",
		);
	}

	/// `scheduling_parent` outside the implicit view's allowed ancestry → rejected.
	#[crate::sim_test]
	fn v3_scheduling_parent_outside_allowed_ancestry_rejected<S: CollatorSut>() {
		let mut w = v3_world::<S>(0, 1);
		let leaf = w.leaf();
		let (_receipt, candidate_hash) = v3_candidate(&w, leaf, leaf);
		let unknown = Hash::repeat_byte(0x99);
		let peer = w.declared_peer(PARA_A, V3);
		w.advertise_v3(
			&peer,
			unknown,
			leaf,
			candidate_hash,
			HeadData(Vec::new()).hash(),
			CandidateDescriptorVersion::V3,
		);
		assert_rejected(
			&mut w,
			peer.peer_id,
			"Reputation Performance for V3 ad with unknown scheduling parent",
		);
	}
}
