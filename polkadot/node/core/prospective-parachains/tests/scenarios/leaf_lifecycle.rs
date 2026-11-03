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

//! `ActiveLeavesUpdate` handling — simple activate/deactivate, parent-inheritance,
//! implicit-view bound, pending-availability persistence across RP-out-of-scope, and
//! session-boundary ancestry stops.

use crate::common::world::{default_world_config, TestLeaf, World, WorldExt as _};
use polkadot_node_subsystem::{
	messages::{Ancestors, BackableCandidateRef},
	ActiveLeavesUpdate,
};
use polkadot_node_subsystem_test_helpers::mock::new_leaf;
use polkadot_primitives::{
	async_backing::CandidatePendingAvailability, BlockNumber, CandidateHash, CoreIndex, Hash,
	HeadData, Id as ParaId, SessionIndex, DEFAULT_SCHEDULING_LOOKAHEAD,
};
use polkadot_primitives_test_helpers::make_candidate;
use polkadot_subsystem_test_sim::chain::{CoreSchedule, SessionInfo};
use std::collections::HashSet;

const MAX_POV_SIZE: u32 = 1_000_000;

#[test]
fn correctly_updates_leaves() {
	let config = default_world_config();
	let mut world = World::start(config);

	// Linear chain progression: a → b → c. Each `.activate()` mirrors the production
	// `block_imported` signal — `start_work(child) + deactivated=[parent]` in a
	// single `ActiveLeavesUpdate`. Mirror state after each step:
	//   * after leaf_a.activate(): [a]
	//   * after leaf_b.activate(): [b]   (a auto-deactivated as parent)
	//   * after leaf_c.activate(): [c]
	let leaf_a = world
		.new_block()
		.with_head_data(ParaId::from(1), HeadData(vec![1, 2, 3]))
		.with_head_data(ParaId::from(2), HeadData(vec![2, 3, 4]))
		.activate();
	assert_eq!(world.base.leaves.iter().map(|l| l.hash).collect::<Vec<_>>(), vec![leaf_a.hash]);

	let leaf_b = world
		.new_block()
		.with_head_data(ParaId::from(1), HeadData(vec![3, 4, 5]))
		.with_head_data(ParaId::from(2), HeadData(vec![4, 5, 6]))
		.activate();
	assert_eq!(world.base.leaves.iter().map(|l| l.hash).collect::<Vec<_>>(), vec![leaf_b.hash]);

	let leaf_c = world
		.new_block()
		.with_head_data(ParaId::from(1), HeadData(vec![5, 6, 7]))
		.with_head_data(ParaId::from(2), HeadData(vec![6, 7, 8]))
		.activate();
	assert_eq!(world.base.leaves.iter().map(|l| l.hash).collect::<Vec<_>>(), vec![leaf_c.hash]);

	// Finalisation: when leaf_c is finalized while still the active tip, the
	// finalized block stays (it equals the finalized hash and is still a leaf —
	// production keeps it). No leaves get pruned.
	world.finalize(leaf_c.hash);
	assert_eq!(world.base.leaves.iter().map(|l| l.hash).collect::<Vec<_>>(), vec![leaf_c.hash]);

	// A deeper finalisation that prunes orphaned leaves on a divergent fork: build
	// a sibling fork off leaf_b, activate it (so we have two active leaves on
	// different branches at number > leaf_b.number), then finalize on one branch.
	let leaf_d = world
		.new_block()
		.from_parent(leaf_b.hash)
		.with_head_data(ParaId::from(1), HeadData(vec![7, 8, 9]))
		.with_head_data(ParaId::from(2), HeadData(vec![8, 9, 10]))
		.activate();
	assert_eq!(
		world.base.leaves.iter().map(|l| l.hash).collect::<HashSet<_>>(),
		[leaf_c.hash, leaf_d.hash].into_iter().collect::<HashSet<_>>(),
	);

	// Deactivate via stop_work — covers the path where the overseer issues a leaf
	// deactivation without an accompanying activation (e.g. cleanup on shutdown).
	world.deactivate_leaf(leaf_d.hash);
	assert_eq!(world.base.leaves.iter().map(|l| l.hash).collect::<Vec<_>>(), vec![leaf_c.hash]);

	world.deactivate_leaf(leaf_c.hash);
	assert!(world.base.leaves.is_empty());
}

#[test]
fn handle_active_leaves_update_gets_candidates_from_parent() {
	let para_id = ParaId::from(1);

	let mut config = default_world_config();
	config.schedule.clear();
	for i in 0..=4 {
		config.schedule.push((CoreIndex(i), CoreSchedule::always(para_id)));
	}
	let mut world = World::start(config);

	let leaf_a = world.new_block().with_head_data(para_id, HeadData(vec![1, 2, 3])).activate();

	let (candidate_a, pvd_a) = make_candidate(
		leaf_a.hash,
		leaf_a.number,
		para_id,
		HeadData(vec![1, 2, 3]),
		HeadData(vec![1]),
		world.validation_code_hash(),
	);
	let candidate_hash_a = candidate_a.hash();
	assert!(world.introduce_seconded_candidate(candidate_a.clone(), pvd_a));
	world.back_candidate(para_id, candidate_hash_a);

	let (candidate_b, candidate_hash_b) = world.make_and_back_candidate(&leaf_a, &candidate_a, 2);
	let (candidate_c, candidate_hash_c) = world.make_and_back_candidate(&leaf_a, &candidate_b, 3);
	let (candidate_d, candidate_hash_d) = world.make_and_back_candidate(&leaf_a, &candidate_c, 4);

	let mut all_candidates_resp = vec![
		BackableCandidateRef { candidate_hash: candidate_hash_a, scheduling_parent: leaf_a.hash },
		BackableCandidateRef { candidate_hash: candidate_hash_b, scheduling_parent: leaf_a.hash },
		BackableCandidateRef { candidate_hash: candidate_hash_c, scheduling_parent: leaf_a.hash },
		BackableCandidateRef { candidate_hash: candidate_hash_d, scheduling_parent: leaf_a.hash },
	];

	assert_eq!(
		world.get_backable_candidates(leaf_a.hash, para_id, 5, Ancestors::default()),
		all_candidates_resp,
	);

	// Activate leaf B as a child of leaf A so it inherits leaf_a's per-scheduling-parent
	// fragment chain (the original test relies on this implicit ancestry to expose
	// leaf_a's C, D under leaf_b). A and B become pending-availability under it.
	// This activation also auto-deactivates leaf_a — production `block_imported` semantics
	// for a child of an active leaf. Inheritance still works because prospective-parachains
	// walks chain ancestors itself; it doesn't depend on the parent staying active.
	let leaf_b = world
		.new_block()
		.from_parent(leaf_a.hash)
		.with_head_data(para_id, HeadData(vec![1, 2, 3]))
		.with_pending(
			para_id,
			vec![
				CandidatePendingAvailability {
					candidate_hash: candidate_a.hash(),
					descriptor: candidate_a.descriptor.clone(),
					commitments: candidate_a.commitments.clone(),
					relay_parent_number: leaf_a.number,
					max_pov_size: MAX_POV_SIZE,
				},
				CandidatePendingAvailability {
					candidate_hash: candidate_b.hash(),
					descriptor: candidate_b.descriptor.clone(),
					commitments: candidate_b.commitments.clone(),
					relay_parent_number: leaf_a.number,
					max_pov_size: MAX_POV_SIZE,
				},
			],
		)
		.activate();

	// Empty ancestors → empty (A,B are pending availability, not part of chain).
	assert!(world
		.get_backable_candidates(leaf_b.hash, para_id, 5, Ancestors::default())
		.is_empty());

	// Ancestors=[A,B] → C,D remaining.
	assert_eq!(
		world.get_backable_candidates(
			leaf_b.hash,
			para_id,
			5,
			[candidate_a.hash(), candidate_b.hash()].into_iter().collect(),
		),
		vec![
			BackableCandidateRef {
				candidate_hash: candidate_c.hash(),
				scheduling_parent: leaf_a.hash
			},
			BackableCandidateRef {
				candidate_hash: candidate_d.hash(),
				scheduling_parent: leaf_a.hash
			},
		],
	);

	// Empty ancestors at leaf_b → still empty.
	assert!(world
		.get_backable_candidates(leaf_b.hash, para_id, 5, Ancestors::default())
		.is_empty());

	// leaf_a is no longer an active leaf (auto-deactivated when leaf_b — its child — was
	// activated). Querying it returns empty, just like any deactivated leaf.
	assert!(world
		.get_backable_candidates(leaf_a.hash, para_id, 5, Ancestors::default())
		.is_empty());

	// leaf_b still empty without ancestors; with [A,B] → C,D.
	assert!(world
		.get_backable_candidates(leaf_b.hash, para_id, 5, Ancestors::default())
		.is_empty());
	assert_eq!(
		world.get_backable_candidates(
			leaf_b.hash,
			para_id,
			5,
			[candidate_a.hash(), candidate_b.hash()].into_iter().collect(),
		),
		vec![
			BackableCandidateRef {
				candidate_hash: candidate_c.hash(),
				scheduling_parent: leaf_a.hash
			},
			BackableCandidateRef {
				candidate_hash: candidate_d.hash(),
				scheduling_parent: leaf_a.hash
			},
		],
	);

	// Activate leaf_c as a sibling fork of leaf_b (shared parent: leaf_a). leaf_c inherits
	// leaf_a's candidates.
	let leaf_c = world
		.new_block()
		.from_parent(leaf_a.hash)
		.with_head_data(para_id, HeadData(vec![1, 2, 3]))
		.with_pending(para_id, vec![])
		.activate();

	assert_eq!(
		world.get_backable_candidates(
			leaf_b.hash,
			para_id,
			5,
			[candidate_a.hash(), candidate_b.hash()].into_iter().collect(),
		),
		vec![
			BackableCandidateRef {
				candidate_hash: candidate_c.hash(),
				scheduling_parent: leaf_a.hash
			},
			BackableCandidateRef {
				candidate_hash: candidate_d.hash(),
				scheduling_parent: leaf_a.hash
			},
		],
	);
	assert_eq!(
		world.get_backable_candidates(leaf_c.hash, para_id, 5, Ancestors::new()),
		all_candidates_resp,
	);

	// Deactivate leaf_c, add a candidate E on leaf_a, reactivate leaf_c. E should be
	// inherited.
	world.deactivate_leaf(leaf_c.hash);
	let (candidate_e, _) = world.make_and_back_candidate(&leaf_a, &candidate_d, 5);
	// Re-signal `start_work` for the existing leaf_c — chain state is already registered,
	// so we go through `signal_active_leaves` rather than building a new leaf.
	world
		.signal_active_leaves(ActiveLeavesUpdate::start_work(new_leaf(leaf_c.hash, leaf_c.number)));

	assert_eq!(
		world.get_backable_candidates(
			leaf_b.hash,
			para_id,
			5,
			[candidate_a.hash(), candidate_b.hash()].into_iter().collect(),
		),
		vec![
			BackableCandidateRef {
				candidate_hash: candidate_c.hash(),
				scheduling_parent: leaf_a.hash
			},
			BackableCandidateRef {
				candidate_hash: candidate_d.hash(),
				scheduling_parent: leaf_a.hash
			},
			BackableCandidateRef {
				candidate_hash: candidate_e.hash(),
				scheduling_parent: leaf_a.hash
			},
		],
	);

	all_candidates_resp.push(BackableCandidateRef {
		candidate_hash: candidate_e.hash(),
		scheduling_parent: leaf_a.hash,
	});
	assert_eq!(
		world.get_backable_candidates(leaf_c.hash, para_id, 5, Ancestors::new()),
		all_candidates_resp,
	);

	// Querying a deactivated leaf returns empty.
	assert!(world
		.get_backable_candidates(leaf_a.hash, para_id, 5, Ancestors::new())
		.is_empty());
}

// Contract: a leaf's implicit-view depth is bounded at `scheduling_lookahead`
// ancestors. Observable effect: when `get_backable_candidates` is queried on the
// latest leaf, candidates whose `scheduling_parent` (= their relay parent for V2) is
// older than `lookahead - 1` blocks behind the latest leaf are excluded — even if
// those ancestor leaves are themselves still active (so the candidate IS in the
// fragment-chain index, just not visible from this leaf).
//
// Probe: activate 10 linear leaves. Introduce + back two candidates: one at the
// just-in-view boundary (`leaves[N-1 - (lookahead - 1)]`) and one one block past
// (`leaves[N-1 - lookahead]`). Query `get_backable_candidates` at the latest leaf —
// the in-view candidate must appear, the past-boundary one must not.
#[test]
fn handle_active_leaves_update_bounded_implicit_view() {
	let para_id = ParaId::from(1);
	let mut config = default_world_config();
	config.schedule.retain(|(_, s)| s.cycle.first() == Some(&para_id));
	assert_eq!(config.schedule.len(), 1);
	let mut world = World::start(config);

	// Build linear chain of 10 leaves, oldest first. All share the same `parent_head`
	// so candidates rooted at any of them have a consistent required-parent chain.
	// Each `.activate()` auto-deactivates its parent — production `block_imported`
	// semantics. We seed the candidates at the moment their relay parent is the
	// current active leaf, so the introduce path accepts them under that leaf's
	// scheduling-parent slot. Once the chain has progressed past, the candidates
	// remain in the fragment-chain index — only their visibility from later leaves
	// is bounded.
	let head_data = HeadData(vec![1, 2, 3]);
	let lookahead = DEFAULT_SCHEDULING_LOOKAHEAD as usize;
	let vch = world.validation_code_hash();

	let mut leaves: Vec<TestLeaf> = Vec::new();
	let mut cand_in_hash = None;
	let mut cand_out_hash = None;
	for i in 0..10 {
		let leaf = world.new_block().with_head_data(para_id, head_data.clone()).activate();
		leaves.push(leaf);

		// Just-past boundary: introduce + back when this leaf is the *current* tip.
		if i == 9 - lookahead {
			let (cand_out, pvd_out) = make_candidate(
				leaf.hash,
				leaf.number,
				para_id,
				head_data.clone(),
				HeadData(vec![0xBB]),
				vch,
			);
			let h = cand_out.hash();
			assert!(world.introduce_seconded_candidate(cand_out, pvd_out));
			world.back_candidate(para_id, h);
			cand_out_hash = Some(h);
		}
		// In-view boundary: same trick.
		if i == 9 - (lookahead - 1) {
			let (cand_in, pvd_in) = make_candidate(
				leaf.hash,
				leaf.number,
				para_id,
				head_data.clone(),
				HeadData(vec![0xAA]),
				vch,
			);
			let h = cand_in.hash();
			assert!(world.introduce_seconded_candidate(cand_in, pvd_in));
			world.back_candidate(para_id, h);
			cand_in_hash = Some(h);
		}
	}
	let latest = &leaves[9];
	let cand_in_hash = cand_in_hash.expect("in-view candidate introduced");
	let cand_out_hash = cand_out_hash.expect("past-boundary candidate introduced");

	// Querying backables on the latest leaf: in-view candidate appears; past-boundary
	// candidate is filtered out because its scheduling parent is no longer in the
	// latest leaf's bounded implicit view.
	let backables = world.get_backable_candidates(latest.hash, para_id, 10, Ancestors::default());
	let returned: HashSet<CandidateHash> = backables.iter().map(|b| b.candidate_hash).collect();
	assert!(
		returned.contains(&cand_in_hash),
		"in-view candidate must be visible from the latest leaf"
	);
	assert!(
		!returned.contains(&cand_out_hash),
		"past-boundary candidate must NOT be visible from the latest leaf \
		 (implicit view is bounded at `scheduling_lookahead`)"
	);
}

#[test]
fn persists_pending_availability_candidate() {
	let para_id = ParaId::from(1);
	let mut config = default_world_config();
	config.schedule.retain(|(_, s)| s.cycle.first() == Some(&para_id));
	assert_eq!(config.schedule.len(), 1);
	let mut world = World::start(config);

	let para_head = HeadData(vec![1, 2, 3]);
	let candidate_relay_parent_number: BlockNumber = 97;

	let leaf_a_hash = Hash::from_low_u64_be(2);
	let leaf_a_number: BlockNumber =
		candidate_relay_parent_number + (DEFAULT_SCHEDULING_LOOKAHEAD - 1);

	// Manually register a synthetic ancestor chain for leaf_a: candidate_relay_parent at
	// number 97, then `lookahead - 1` intermediate blocks up to (but not including) leaf_a.
	// The leaf itself is pinned via `with_hash_and_number` so its hash matches the test's
	// constants.
	let candidate_relay_parent = Hash::from_low_u64_be(0xC0DE);
	{
		let mut chain = world.base.chain.lock();
		chain.register_block_with_session(
			candidate_relay_parent,
			Hash::zero(),
			candidate_relay_parent_number,
			Some(world.session_index()),
		);
		// Build the ancestor chain from candidate_relay_parent up to (and including) leaf_a.
		let mut prev = candidate_relay_parent;
		for step in 1..DEFAULT_SCHEDULING_LOOKAHEAD {
			let n = candidate_relay_parent_number + step;
			let h = if n == leaf_a_number {
				leaf_a_hash
			} else {
				Hash::from_low_u64_be(0xA0_00 + n as u64)
			};
			chain.register_block_with_session(h, prev, n, Some(world.session_index()));
			prev = h;
		}
	}

	// leaf_b is a sibling fork of leaf_a (same parent: the intermediate block at
	// `leaf_a_number - 1`), so activating leaf_b doesn't auto-deactivate leaf_a — both
	// stay active. Production analogue: a relay-chain reorg surface where two competing
	// children of a common parent both get signalled. The original test wanted leaf_b to
	// be a *child* of leaf_a, but production `block_imported` semantics no longer let a
	// child of an active leaf coexist with its parent on the active set.
	let leaf_b_hash = Hash::from_low_u64_be(1);
	let leaf_b_number = leaf_a_number;
	let shared_parent_hash = Hash::from_low_u64_be(0xA0_00 + (leaf_a_number - 1) as u64);

	let leaf_a = world
		.new_block()
		.with_hash_and_number(leaf_a_hash, leaf_a_number)
		.with_head_data(para_id, para_head.clone())
		.activate();

	let (candidate_a, pvd_a) = make_candidate(
		candidate_relay_parent,
		candidate_relay_parent_number,
		para_id,
		para_head.clone(),
		HeadData(vec![1]),
		world.validation_code_hash(),
	);
	let candidate_hash_a = candidate_a.hash();

	let (candidate_b, pvd_b) = make_candidate(
		leaf_b_hash,
		leaf_b_number,
		para_id,
		HeadData(vec![1]),
		HeadData(vec![2]),
		world.validation_code_hash(),
	);
	let candidate_hash_b = candidate_b.hash();

	assert!(world.introduce_seconded_candidate(candidate_a.clone(), pvd_a.clone()));
	world.back_candidate(para_id, candidate_hash_a);

	let candidate_a_pending_av = CandidatePendingAvailability {
		candidate_hash: candidate_hash_a,
		descriptor: candidate_a.descriptor.clone(),
		commitments: candidate_a.commitments.clone(),
		relay_parent_number: candidate_relay_parent_number,
		max_pov_size: MAX_POV_SIZE,
	};
	// Register leaf_b in the chain as a sibling fork of leaf_a (shared parent at
	// `leaf_a_number - 1`). Pin its literal hash via `with_hash_and_number` so it
	// matches the test's `make_candidate(leaf_b_hash, ...)` call above.
	{
		let mut chain = world.base.chain.lock();
		chain.register_block_with_session(
			leaf_b_hash,
			shared_parent_hash,
			leaf_b_number,
			Some(world.session_index()),
		);
	}
	let leaf_b = world
		.new_block()
		.with_hash_and_number(leaf_b_hash, leaf_b_number)
		.with_head_data(para_id, para_head.clone())
		.with_pending(para_id, vec![candidate_a_pending_av])
		.activate();

	let resp = world.get_hypothetical_membership(candidate_hash_a, candidate_a, pvd_a);
	assert_eq!(resp.len(), 1);
	let (_, membership) = &resp[0];
	let mut got: Vec<Hash> = membership.iter().copied().collect();
	got.sort();
	let mut want = vec![leaf_a.hash, leaf_b.hash];
	want.sort();
	assert_eq!(got, want);

	assert!(world.introduce_seconded_candidate(candidate_b.clone(), pvd_b));
	world.back_candidate(para_id, candidate_hash_b);

	assert_eq!(
		world.get_backable_candidates(
			leaf_b.hash,
			para_id,
			1,
			vec![candidate_hash_a].into_iter().collect(),
		),
		vec![BackableCandidateRef {
			candidate_hash: candidate_hash_b,
			scheduling_parent: leaf_b_hash,
		}],
	);
}

// Contract: subsystem's per-leaf ancestor walk stops at a session boundary, so blocks
// before the session change never enter the leaf's implicit-view scope. Observable
// effect: introducing a seconded candidate whose `relay_parent` lies before the session
// boundary is rejected, even when the chain model would otherwise accept it (claim
// queue + backing constraints are seeded for that RP — only "RP is out of any active
// leaf's scope" can reject it). A positive probe at an in-session ancestor confirms the
// rest of the setup is sound.
#[test]
fn uses_ancestry_only_within_session() {
	let para_id = ParaId::from(1);
	let mut config = default_world_config();
	// Single-para schedule on one core: keep the probe focused on the session-boundary
	// invariant rather than multi-core scheduling.
	config.schedule.clear();
	config.schedule.push((CoreIndex(0), CoreSchedule::always(para_id)));
	config.session_index = 2;
	let mut world = World::start(config);

	let session: SessionIndex = world.session_index();
	let session_minus_one_info = SessionInfo {
		validators: Vec::new(),
		validator_groups: Vec::new(),
		group_rotation_info: polkadot_primitives::GroupRotationInfo {
			session_start_block: 0,
			group_rotation_frequency: 1,
			now: 0,
		},
	};

	// Chain shape: leaf=5 (session 2) → 4 (session 2) → 3 (session 1, the boundary) →
	// 2 (session 1) → 1 (session 1). With `scheduling_lookahead = 3`, the leaf's
	// implicit view reaches blocks `{leaf, parent, grandparent}` *unless* the session
	// boundary cuts it shorter. Hash 4 is in-session; hash 2 is two blocks past the
	// boundary.
	let leaf_hash = Hash::repeat_byte(5);
	let in_session_rp = Hash::repeat_byte(4);
	let past_boundary_rp = Hash::repeat_byte(2);
	let parent_head = HeadData(vec![1, 2, 3]);

	{
		let mut chain = world.base.chain.lock();
		chain.add_session(session - 1, session_minus_one_info);
		// Override the default parent-walk by registering the chain explicitly so we can
		// flip session at hash 3.
		for (hash, parent, number, sess) in [
			(Hash::repeat_byte(5), Hash::repeat_byte(4), 5u32, session),
			(Hash::repeat_byte(4), Hash::repeat_byte(3), 4u32, session),
			(Hash::repeat_byte(3), Hash::repeat_byte(2), 3u32, session - 1),
			(Hash::repeat_byte(2), Hash::repeat_byte(1), 2u32, session - 1),
		] {
			chain.register_block_with_session(hash, parent, number, Some(sess));
		}

		// Seed valid backing constraints at *both* probe RPs (in-session and
		// past-boundary). If the subsystem walks past the session change (the bug we're
		// pinning), it would query these and find them — and the past-boundary candidate
		// would be accepted. The contract under test is that it does NOT walk past.
		let vch = world.base.config.validation_code_hash;
		for (rp, rp_number) in [(in_session_rp, 4u32), (past_boundary_rp, 2u32)] {
			let constraints = polkadot_subsystem_test_sim::world_base::synthesise_constraints(
				rp_number.saturating_sub(DEFAULT_SCHEDULING_LOOKAHEAD - 1),
				vec![rp_number],
				parent_head.clone(),
				vch,
			);
			chain.set_backing_constraints_at(rp, para_id, constraints);
			chain.set_pending_availability_at(rp, para_id, Vec::new());
		}
	}

	let _leaf = world
		.new_block()
		.with_hash_and_number(leaf_hash, 5)
		.with_head_data(para_id, parent_head.clone())
		.activate();
	let vch = world.validation_code_hash();

	// Positive probe: in-session ancestor. The candidate's RP is inside the implicit
	// view, so the subsystem accepts the seconded candidate. This confirms setup is
	// sound — without it, a "negative-only" probe could pass for the wrong reason
	// (e.g. unrelated rejection in the introduce path).
	let (cand_in, pvd_in) =
		make_candidate(in_session_rp, 4, para_id, parent_head.clone(), HeadData(vec![0xAA]), vch);
	assert!(
		world.introduce_seconded_candidate(cand_in, pvd_in),
		"in-session ancestor must be in implicit view (positive control)"
	);

	// Negative probe: past-boundary ancestor. If the subsystem walked the ancestor
	// chain past the session boundary, this RP would also be in scope and the
	// candidate would be accepted. Stopping at the boundary means the RP is out of
	// scope and the introduce is rejected.
	let (cand_out, pvd_out) = make_candidate(
		past_boundary_rp,
		2,
		para_id,
		parent_head.clone(),
		HeadData(vec![0xBB]),
		vch,
	);
	assert!(
		!world.introduce_seconded_candidate(cand_out, pvd_out),
		"past-boundary ancestor must NOT be in implicit view"
	);
}
