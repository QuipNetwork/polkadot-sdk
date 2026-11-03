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

//! Read-only queries: `GetHypotheticalMembership` and `GetProspectiveValidationData`.

use crate::common::world::{default_world_config, World, WorldExt as _};
use polkadot_primitives::{Hash, HeadData, Id as ParaId};
use polkadot_primitives_test_helpers::make_candidate;
use std::collections::HashSet;

#[test]
fn check_hypothetical_membership_query() {
	let config = default_world_config();
	let mut world = World::start(config);

	// Two coexisting active leaves require sibling forks of a common non-leaf ancestor —
	// a linear chain of `.activate()` calls auto-deactivates the previous leaf. The
	// candidates are anchored at `common` so they sit in both forks' implicit views and
	// hypothetical-membership queries return both leaves.
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
		.with_head_data(ParaId::from(2), HeadData(vec![2, 3, 4]))
		.activate();

	let (candidate_a, pvd_a) = make_candidate(
		common.hash,
		common.number,
		ParaId::from(1),
		HeadData(vec![1, 2, 3]),
		HeadData(vec![1]),
		world.validation_code_hash(),
	);
	let (candidate_b, pvd_b) = make_candidate(
		common.hash,
		common.number,
		ParaId::from(1),
		HeadData(vec![1]),
		HeadData(vec![2]),
		world.validation_code_hash(),
	);
	let (candidate_c, pvd_c) = make_candidate(
		common.hash,
		common.number,
		ParaId::from(1),
		HeadData(vec![2]),
		HeadData(vec![3]),
		world.validation_code_hash(),
	);

	let assert_membership = |world: &mut World,
	                         candidate: polkadot_primitives::CommittedCandidateReceiptV2,
	                         pvd: polkadot_primitives::PersistedValidationData,
	                         expected: Vec<Hash>| {
		let hash = candidate.hash();
		let resp = world.get_hypothetical_membership(hash, candidate, pvd);
		assert_eq!(resp.len(), 1);
		let (_, membership) = &resp[0];
		assert_eq!(
			membership.iter().copied().collect::<HashSet<_>>(),
			expected.into_iter().collect::<HashSet<_>>(),
		);
	};

	// Before adding any candidate, A is directly addable; B and C are potential.
	for (candidate, pvd) in [
		(candidate_a.clone(), pvd_a.clone()),
		(candidate_b.clone(), pvd_b.clone()),
		(candidate_c.clone(), pvd_c.clone()),
	] {
		assert_membership(&mut world, candidate, pvd, vec![leaf_a.hash, leaf_b.hash]);
	}

	// Introduce A; all three remain visible (unconnected so far).
	assert!(world.introduce_seconded_candidate(candidate_a.clone(), pvd_a.clone()));
	for (candidate, pvd) in [
		(candidate_a.clone(), pvd_a.clone()),
		(candidate_b.clone(), pvd_b.clone()),
		(candidate_c.clone(), pvd_c.clone()),
	] {
		assert_membership(&mut world, candidate, pvd, vec![leaf_a.hash, leaf_b.hash]);
	}

	// Back A; chain root anchors here. All three remain.
	world.back_candidate(ParaId::from(1), candidate_a.hash());
	for (candidate, pvd) in [
		(candidate_a.clone(), pvd_a.clone()),
		(candidate_b.clone(), pvd_b.clone()),
		(candidate_c.clone(), pvd_c.clone()),
	] {
		assert_membership(&mut world, candidate, pvd, vec![leaf_a.hash, leaf_b.hash]);
	}

	// Candidate D has invalid relay parent → reject.
	let (candidate_d, pvd_d) = make_candidate(
		Hash::from_low_u64_be(200),
		leaf_a.number,
		ParaId::from(1),
		HeadData(vec![1]),
		HeadData(vec![2]),
		world.validation_code_hash(),
	);
	assert!(!world.introduce_seconded_candidate(candidate_d, pvd_d));

	// Candidate E has invalid head data → reject.
	let (candidate_e, pvd_e) = make_candidate(
		leaf_a.hash,
		leaf_a.number,
		ParaId::from(1),
		HeadData(vec![2]),
		HeadData(vec![0; 20481]),
		world.validation_code_hash(),
	);
	assert!(!world.introduce_seconded_candidate(candidate_e, pvd_e));

	// Add B + back. Membership unchanged for the three legit candidates.
	assert!(world.introduce_seconded_candidate(candidate_b.clone(), pvd_b.clone()));
	world.back_candidate(ParaId::from(1), candidate_b.hash());

	for (candidate, pvd) in [
		(candidate_a.clone(), pvd_a.clone()),
		(candidate_b.clone(), pvd_b.clone()),
		(candidate_c.clone(), pvd_c.clone()),
	] {
		assert_membership(&mut world, candidate, pvd, vec![leaf_a.hash, leaf_b.hash]);
	}

	assert_eq!(world.base.leaves.len(), 2);
}

#[test]
fn check_pvd_query() {
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
	let (candidate_b, pvd_b) = make_candidate(
		leaf_a.hash,
		leaf_a.number,
		ParaId::from(1),
		HeadData(vec![1]),
		HeadData(vec![2]),
		world.validation_code_hash(),
	);
	let (candidate_c, pvd_c) = make_candidate(
		leaf_a.hash,
		leaf_a.number,
		ParaId::from(1),
		HeadData(vec![2]),
		HeadData(vec![3]),
		world.validation_code_hash(),
	);
	let (candidate_e, pvd_e) = make_candidate(
		leaf_a.hash,
		leaf_a.number,
		ParaId::from(1),
		HeadData(vec![5]),
		HeadData(vec![6]),
		world.validation_code_hash(),
	);

	// PVD of A before adding (parent_head matches the leaf's required_parent).
	assert_eq!(
		world.get_pvd(ParaId::from(1), leaf_a.hash, HeadData(vec![1, 2, 3]), world.session_index()),
		Some(pvd_a.clone()),
	);

	assert!(world.introduce_seconded_candidate(candidate_a.clone(), pvd_a.clone()));
	world.back_candidate(ParaId::from(1), candidate_a.hash());

	// PVD of A after adding.
	assert_eq!(
		world.get_pvd(ParaId::from(1), leaf_a.hash, HeadData(vec![1, 2, 3]), world.session_index()),
		Some(pvd_a.clone()),
	);

	// PVD of B before adding (parent is A's head_data).
	assert_eq!(
		world.get_pvd(ParaId::from(1), leaf_a.hash, HeadData(vec![1]), world.session_index()),
		Some(pvd_b.clone()),
	);
	assert!(world.introduce_seconded_candidate(candidate_b, pvd_b.clone()));
	assert_eq!(
		world.get_pvd(ParaId::from(1), leaf_a.hash, HeadData(vec![1]), world.session_index()),
		Some(pvd_b.clone()),
	);

	// PVD of C before adding.
	assert_eq!(
		world.get_pvd(ParaId::from(1), leaf_a.hash, HeadData(vec![2]), world.session_index()),
		Some(pvd_c.clone()),
	);
	assert!(world.introduce_seconded_candidate(candidate_c, pvd_c.clone()));
	assert_eq!(
		world.get_pvd(ParaId::from(1), leaf_a.hash, HeadData(vec![2]), world.session_index()),
		Some(pvd_c),
	);

	// E's parent isn't known yet.
	assert_eq!(
		world.get_pvd(ParaId::from(1), leaf_a.hash, HeadData(vec![5]), world.session_index()),
		None,
	);

	// Add E and re-query.
	assert!(world.introduce_seconded_candidate(candidate_e, pvd_e.clone()));
	assert_eq!(
		world.get_pvd(ParaId::from(1), leaf_a.hash, HeadData(vec![5]), world.session_index()),
		Some(pvd_e),
	);

	assert_eq!(world.base.leaves.len(), 1);
}
