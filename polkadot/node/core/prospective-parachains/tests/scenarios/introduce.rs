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

//! `IntroduceSecondedCandidate` accept/reject behaviour, idempotence, and visibility
//! across multiple leaves / sibling forks.

use crate::common::world::{default_world_config, World, WorldExt as _};
use polkadot_node_subsystem::messages::{Ancestors, BackableCandidateRef};
use polkadot_primitives::{CoreIndex, HeadData, Id as ParaId};
use polkadot_primitives_test_helpers::make_candidate;
use polkadot_subsystem_test_sim::chain::CoreSchedule;

#[test]
fn introduce_candidates_basic() {
	let mut config = default_world_config();

	let chain_a = ParaId::from(1);
	let chain_b = ParaId::from(2);
	config.schedule = vec![(CoreIndex(0), CoreSchedule::cycling(vec![chain_a, chain_b]))];

	let mut world = World::start(config);

	// Three coexisting active leaves require sibling forks of a common non-leaf ancestor.
	// A linear chain of `.activate()` calls auto-deactivates each previous leaf — the
	// production `block_imported` semantics.
	let common = world.new_block().register();
	let leaf_a = world
		.new_block()
		.from_parent(common.hash)
		.with_head_data(chain_a, HeadData(vec![1, 2, 3]))
		.with_head_data(chain_b, HeadData(vec![2, 3, 4]))
		.activate();
	let leaf_b = world
		.new_block()
		.from_parent(common.hash)
		.with_head_data(chain_a, HeadData(vec![3, 4, 5]))
		.with_head_data(chain_b, HeadData(vec![4, 5, 6]))
		.activate();
	let leaf_c = world
		.new_block()
		.from_parent(common.hash)
		.with_head_data(chain_a, HeadData(vec![5, 6, 7]))
		.with_head_data(chain_b, HeadData(vec![6, 7, 8]))
		.activate();

	let (candidate_a1, pvd_a1) = make_candidate(
		leaf_a.hash,
		leaf_a.number,
		chain_a,
		HeadData(vec![1, 2, 3]),
		HeadData(vec![1]),
		world.validation_code_hash(),
	);
	let candidate_hash_a1 = candidate_a1.hash();
	let response_a1 = vec![BackableCandidateRef {
		candidate_hash: candidate_hash_a1,
		scheduling_parent: leaf_a.hash,
	}];

	let (candidate_a2, pvd_a2) = make_candidate(
		leaf_a.hash,
		leaf_a.number,
		chain_b,
		HeadData(vec![2, 3, 4]),
		HeadData(vec![2]),
		world.validation_code_hash(),
	);
	let candidate_hash_a2 = candidate_a2.hash();
	let response_a2 = vec![BackableCandidateRef {
		candidate_hash: candidate_hash_a2,
		scheduling_parent: leaf_a.hash,
	}];

	let (candidate_b, pvd_b) = make_candidate(
		leaf_b.hash,
		leaf_b.number,
		chain_a,
		HeadData(vec![3, 4, 5]),
		HeadData(vec![3]),
		world.validation_code_hash(),
	);
	let candidate_hash_b = candidate_b.hash();
	let response_b = vec![BackableCandidateRef {
		candidate_hash: candidate_hash_b,
		scheduling_parent: leaf_b.hash,
	}];

	let (candidate_c, pvd_c) = make_candidate(
		leaf_c.hash,
		leaf_c.number,
		chain_b,
		HeadData(vec![6, 7, 8]),
		HeadData(vec![4]),
		world.validation_code_hash(),
	);
	let candidate_hash_c = candidate_c.hash();
	let response_c = vec![BackableCandidateRef {
		candidate_hash: candidate_hash_c,
		scheduling_parent: leaf_c.hash,
	}];

	assert!(world.introduce_seconded_candidate(candidate_a1.clone(), pvd_a1));
	assert!(world.introduce_seconded_candidate(candidate_a2.clone(), pvd_a2));
	assert!(world.introduce_seconded_candidate(candidate_b.clone(), pvd_b));
	assert!(world.introduce_seconded_candidate(candidate_c.clone(), pvd_c));

	world.back_candidate(chain_a, candidate_hash_a1);
	world.back_candidate(chain_b, candidate_hash_a2);
	world.back_candidate(chain_a, candidate_hash_b);
	world.back_candidate(chain_b, candidate_hash_c);

	assert_eq!(
		world.get_backable_candidates(leaf_a.hash, chain_a, 5, Ancestors::default()),
		response_a1,
	);
	assert_eq!(
		world.get_backable_candidates(leaf_a.hash, chain_b, 5, Ancestors::default()),
		response_a2,
	);
	assert_eq!(
		world.get_backable_candidates(leaf_b.hash, chain_a, 5, Ancestors::default()),
		response_b,
	);
	assert_eq!(
		world.get_backable_candidates(leaf_c.hash, chain_b, 5, Ancestors::default()),
		response_c,
	);

	// Cross-leaf membership checks: each candidate is *only* known under its activating leaf.
	assert_eq!(
		world.get_backable_candidates(leaf_b.hash, chain_b, 5, Ancestors::default()),
		Vec::<BackableCandidateRef>::new(),
	);
	assert_eq!(
		world.get_backable_candidates(leaf_c.hash, chain_a, 5, Ancestors::default()),
		Vec::<BackableCandidateRef>::new(),
	);

	// All three leaves were activated successfully — the original test asserted this via
	// `view.active_leaves.len() == 3`. With access only to the public surface, the proof
	// that all three are active is that each leaf-keyed `GetBackableCandidates` query
	// returned the right candidate (would return empty for an unknown leaf).
	assert_eq!(world.base.leaves.len(), 3);
}

#[test]
fn introduce_candidates_error() {
	let mut config = default_world_config();
	config.schedule.push((CoreIndex(2), CoreSchedule::always(ParaId::from(1))));

	let mut world = World::start(config);

	let leaf_a = world
		.new_block()
		.with_head_data(ParaId::from(1), HeadData(vec![1, 2, 3]))
		.with_head_data(ParaId::from(2), HeadData(vec![2, 3, 4]))
		.activate();

	// Candidate A: directly buildable from `[1,2,3]` (the leaf's required_parent).
	let (candidate_a, pvd_a) = make_candidate(
		leaf_a.hash,
		leaf_a.number,
		ParaId::from(1),
		HeadData(vec![1, 2, 3]),
		HeadData(vec![1]),
		world.validation_code_hash(),
	);
	// Candidate B: child of A.
	let (candidate_b, pvd_b) = make_candidate(
		leaf_a.hash,
		leaf_a.number,
		ParaId::from(1),
		HeadData(vec![1]),
		HeadData(vec![1; 20480]),
		world.validation_code_hash(),
	);
	// Candidate C: oversized head data, fails the constraint check.
	let (candidate_c, pvd_c) = make_candidate(
		leaf_a.hash,
		leaf_a.number,
		ParaId::from(1),
		HeadData(vec![1; 20480]),
		HeadData(vec![0; 20485]),
		world.validation_code_hash(),
	);

	// Hypothetical membership: A directly addable, B potential. Both report leaf_a.hash.
	for (candidate, pvd) in
		[(candidate_a.clone(), pvd_a.clone()), (candidate_b.clone(), pvd_b.clone())]
	{
		let hash = candidate.hash();
		let resp = world.get_hypothetical_membership(hash, candidate, pvd);
		assert_eq!(resp.len(), 1);
		let (_, membership) = &resp[0];
		assert_eq!(membership.iter().copied().collect::<Vec<_>>(), vec![leaf_a.hash]);
	}

	// Hypothetical membership of C: empty (fails constraint check).
	{
		let resp = world.get_hypothetical_membership(
			candidate_c.hash(),
			candidate_c.clone(),
			pvd_c.clone(),
		);
		assert_eq!(resp.len(), 1);
		let (_, membership) = &resp[0];
		assert!(membership.is_empty());
	}

	// Introduce A and B successfully.
	assert!(world.introduce_seconded_candidate(candidate_a.clone(), pvd_a.clone()));
	assert!(world.introduce_seconded_candidate(candidate_b.clone(), pvd_b.clone()));
	// Introduce C: rejected.
	assert!(!world.introduce_seconded_candidate(candidate_c.clone(), pvd_c.clone()));

	world.back_candidate(ParaId::from(1), candidate_a.hash());
	world.back_candidate(ParaId::from(1), candidate_b.hash());
	world.back_candidate(ParaId::from(1), candidate_c.hash());

	assert_eq!(
		world.get_backable_candidates(leaf_a.hash, ParaId::from(1), 5, Ancestors::default()),
		vec![
			BackableCandidateRef {
				candidate_hash: candidate_a.hash(),
				scheduling_parent: leaf_a.hash,
			},
			BackableCandidateRef {
				candidate_hash: candidate_b.hash(),
				scheduling_parent: leaf_a.hash,
			},
		],
	);

	assert_eq!(world.base.leaves.len(), 1);
}

#[test]
fn introduce_candidate_multiple_times() {
	let config = default_world_config();
	let mut world = World::start(config);

	let leaf_a = world
		.new_block()
		.with_head_data(ParaId::from(1), HeadData(vec![1, 2, 3]))
		.with_head_data(ParaId::from(2), HeadData(vec![2, 3, 4]))
		.activate();

	let (candidate_a, pvd_a) = make_candidate(
		leaf_a.hash,
		leaf_a.number,
		ParaId::from(1),
		HeadData(vec![1, 2, 3]),
		HeadData(vec![1]),
		world.validation_code_hash(),
	);
	let candidate_hash_a = candidate_a.hash();
	let response_a = vec![BackableCandidateRef {
		candidate_hash: candidate_hash_a,
		scheduling_parent: leaf_a.hash,
	}];

	assert!(world.introduce_seconded_candidate(candidate_a.clone(), pvd_a.clone()));
	world.back_candidate(ParaId::from(1), candidate_hash_a);

	assert_eq!(
		world.get_backable_candidates(leaf_a.hash, ParaId::from(1), 5, Ancestors::default()),
		response_a,
	);

	// Re-introduce the same candidate 5 more times. Each call returns true (already
	// present) and does not duplicate state.
	for _ in 0..5 {
		assert!(world.introduce_seconded_candidate(candidate_a.clone(), pvd_a.clone()));
	}

	assert_eq!(
		world.get_backable_candidates(leaf_a.hash, ParaId::from(1), 5, Ancestors::default()),
		response_a,
	);

	assert_eq!(world.base.leaves.len(), 1);
}

#[test]
fn introduce_candidate_on_multiple_forks() {
	let config = default_world_config();
	let mut world = World::start(config);

	// Two coexisting active leaves require sibling forks of a common non-leaf ancestor —
	// a linear chain of `.activate()` calls auto-deactivates the previous leaf. The
	// shared candidate is rooted at `common` so it lives in both forks' implicit views.
	let common = world
		.new_block()
		.with_head_data(ParaId::from(1), HeadData(vec![1, 2, 3]))
		.with_head_data(ParaId::from(2), HeadData(vec![2, 3, 4]))
		.register();
	let leaf_a = world
		.new_block()
		.from_parent(common.hash)
		.with_head_data(ParaId::from(1), HeadData(vec![1, 2, 3]))
		.with_head_data(ParaId::from(2), HeadData(vec![2, 3, 4]))
		.activate();
	let leaf_b = world
		.new_block()
		.from_parent(common.hash)
		.with_head_data(ParaId::from(1), HeadData(vec![1, 2, 3]))
		.with_head_data(ParaId::from(2), HeadData(vec![4, 5, 6]))
		.activate();

	let (candidate_a, pvd_a) = make_candidate(
		common.hash,
		common.number,
		ParaId::from(1),
		HeadData(vec![1, 2, 3]),
		HeadData(vec![1]),
		world.validation_code_hash(),
	);
	let candidate_hash_a = candidate_a.hash();
	let response_a = vec![BackableCandidateRef {
		candidate_hash: candidate_hash_a,
		scheduling_parent: common.hash,
	}];

	assert!(world.introduce_seconded_candidate(candidate_a.clone(), pvd_a));
	world.back_candidate(ParaId::from(1), candidate_hash_a);

	assert_eq!(
		world.get_backable_candidates(leaf_a.hash, ParaId::from(1), 5, Ancestors::default()),
		response_a,
	);
	assert_eq!(
		world.get_backable_candidates(leaf_b.hash, ParaId::from(1), 5, Ancestors::default()),
		response_a,
	);

	assert_eq!(world.base.leaves.len(), 2);
}
