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

//! V3 candidates whose `relay_parent` is older than the scheduling lookahead.

use crate::common::world::{default_world_config, World, WorldExt as _};
use polkadot_node_subsystem::messages::{Ancestors, BackableCandidateRef};
use polkadot_primitives::{
	BlockNumber, Hash, HeadData, Id as ParaId, PersistedValidationData,
	DEFAULT_SCHEDULING_LOOKAHEAD,
};
use polkadot_primitives_test_helpers::make_candidate_v3;
use std::collections::HashSet;

const MAX_POV_SIZE: u32 = 1_000_000;
const LEAF_NUMBER: BlockNumber = 100;
const OLDER_RELAY_PARENT_NUMBER: BlockNumber = LEAF_NUMBER - 4 * DEFAULT_SCHEDULING_LOOKAHEAD;

#[test]
fn introduce_v3_candidate_with_older_relay_parent() {
	let para_id = ParaId::from(1);
	let mut config = default_world_config();
	// Allow relay parents back to the older block via constraints' min_relay_parent_number.
	config.min_relay_parent_number_override = Some(OLDER_RELAY_PARENT_NUMBER);
	let mut world = World::start(config);

	let leaf_a = world
		.new_block()
		.with_head_data(para_id, HeadData(vec![1, 2, 3]))
		.with_head_data(ParaId::from(2), HeadData(vec![2, 3, 4]))
		.activate();

	// Older relay parent: register it in the chain so prospective's
	// AncestorRelayParentInfo / SessionIndexForChild lookups resolve.
	let older_relay_parent = Hash::from_low_u64_be(9999);
	{
		let mut chain = world.base.chain.lock();
		chain.register_block_with_session(
			older_relay_parent,
			Hash::zero(),
			OLDER_RELAY_PARENT_NUMBER,
			Some(world.session_index()),
		);
	}

	let (candidate_a, pvd_a) = make_candidate_v3(
		older_relay_parent,
		OLDER_RELAY_PARENT_NUMBER,
		leaf_a.hash,
		para_id,
		HeadData(vec![1, 2, 3]),
		HeadData(vec![1]),
		world.validation_code_hash(),
	);
	let candidate_hash_a = candidate_a.hash();

	assert_eq!(candidate_a.descriptor.relay_parent(), older_relay_parent);
	assert_eq!(candidate_a.descriptor.scheduling_parent(), leaf_a.hash);

	assert!(world.introduce_seconded_candidate(candidate_a.clone(), pvd_a.clone()));
	world.back_candidate(para_id, candidate_hash_a);

	assert_eq!(
		world.get_backable_candidates(leaf_a.hash, para_id, 1, Ancestors::default()),
		vec![BackableCandidateRef {
			candidate_hash: candidate_hash_a,
			scheduling_parent: leaf_a.hash,
		}],
	);

	let resp = world.get_hypothetical_membership(candidate_hash_a, candidate_a, pvd_a);
	assert_eq!(resp.len(), 1);
	let (_, membership) = &resp[0];
	assert_eq!(
		membership.iter().copied().collect::<HashSet<_>>(),
		[leaf_a.hash].into_iter().collect::<HashSet<_>>(),
	);

	assert_eq!(world.base.leaves.len(), 1);
}

// Contract: `GetProspectiveValidationData` for a child of an older-RP candidate
// returns the same PVD regardless of which underlying runtime API path resolved the
// older relay-parent's block info — the modern `AncestorRelayParentInfo` runtime API
// or the chain-header fallback for runtimes that don't support it. Pinning both
// branches catches regressions where the dispatch logic diverges.
#[test]
fn get_pvd_for_candidate_with_older_relay_parent() {
	use polkadot_node_subsystem::messages::RuntimeApiRequest;

	for runtime_api_version in [
		// Pre-`AncestorRelayParentInfo`: forces the chain-header fallback path.
		RuntimeApiRequest::CONSTRAINTS_RUNTIME_REQUIREMENT,
		// Has `AncestorRelayParentInfo`: exercises the runtime-API path.
		RuntimeApiRequest::ANCESTOR_RELAY_PARENT_INFO_RUNTIME_REQUIREMENT,
	] {
		let para_id = ParaId::from(1);
		let mut config = default_world_config();
		config.min_relay_parent_number_override = Some(OLDER_RELAY_PARENT_NUMBER);
		config.runtime_api_version = runtime_api_version;
		let mut world = World::start(config);

		let leaf_a = world
			.new_block()
			.with_head_data(para_id, HeadData(vec![1, 2, 3]))
			.with_head_data(ParaId::from(2), HeadData(vec![2, 3, 4]))
			.activate();

		let older_relay_parent = Hash::from_low_u64_be(9999);
		{
			let mut chain = world.base.chain.lock();
			chain.register_block_with_session(
				older_relay_parent,
				Hash::zero(),
				OLDER_RELAY_PARENT_NUMBER,
				Some(world.session_index()),
			);
		}

		let (candidate_a, pvd_a) = make_candidate_v3(
			older_relay_parent,
			OLDER_RELAY_PARENT_NUMBER,
			leaf_a.hash,
			para_id,
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			world.validation_code_hash(),
		);
		assert!(
			world.introduce_seconded_candidate(candidate_a, pvd_a),
			"introduce must succeed at runtime_api_version = {}",
			runtime_api_version
		);

		let pvd =
			world.get_pvd(para_id, older_relay_parent, HeadData(vec![1]), world.session_index());
		assert_eq!(
			pvd,
			Some(PersistedValidationData {
				parent_head: HeadData(vec![1]),
				relay_parent_number: OLDER_RELAY_PARENT_NUMBER,
				relay_parent_storage_root: Hash::zero(),
				max_pov_size: MAX_POV_SIZE,
			}),
			"PVD must match across runtime API versions (got mismatch at version {})",
			runtime_api_version,
		);
	}
}
