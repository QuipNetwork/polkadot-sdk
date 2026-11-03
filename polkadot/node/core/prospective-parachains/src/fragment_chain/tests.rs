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

use super::*;
use assert_matches::assert_matches;
use polkadot_node_subsystem_util::inclusion_emulator::{
	InboundHrmpLimitations, RelayChainBlockInfo,
};
use polkadot_primitives::{
	BlockNumber, CandidateCommitments, CandidateDescriptorV2, CoreIndex, HeadData, Id as ParaId,
	MutateDescriptorV2,
};
use polkadot_primitives_test_helpers as test_helpers;
use polkadot_primitives_test_helpers::CandidateDescriptor;
use rand::{seq::SliceRandom, thread_rng};
use std::ops::Range;

fn make_constraints(
	min_relay_parent_number: BlockNumber,
	valid_watermarks: Vec<BlockNumber>,
	required_parent: HeadData,
) -> Constraints {
	Constraints {
		min_relay_parent_number,
		max_pov_size: 1_000_000,
		max_code_size: 1_000_000,
		max_head_data_size: 20480,
		ump_remaining: 10,
		ump_remaining_bytes: 1_000,
		max_ump_num_per_candidate: 10,
		dmp_remaining_messages: [0; 10].into(),
		hrmp_inbound: InboundHrmpLimitations { valid_watermarks },
		hrmp_channels_out: HashMap::new(),
		max_hrmp_num_per_candidate: 0,
		required_parent,
		validation_code_hash: Hash::repeat_byte(42).into(),
		upgrade_restriction: None,
		future_validation_code: None,
	}
}

// Helper to create both SchedulingScope and Scope.
fn make_scope(
	relay_parent: RelayChainBlockInfo,
	base_constraints: Constraints,
	pending_availability: Vec<PendingAvailability>,
	max_backable_len: usize,
	ancestors: Vec<RelayChainBlockInfo>,
) -> (SchedulingScope, Scope) {
	let scheduling_scope = SchedulingScope::new(
		(relay_parent.hash, relay_parent.number),
		ancestors.iter().map(|a| (a.hash, a.number)),
	)
	.unwrap();

	let scope = Scope::new(base_constraints, pending_availability, max_backable_len);

	(scheduling_scope, scope)
}

struct CandidateBuilder {
	para_id: ParaId,
	relay_parent: Hash,
	relay_parent_number: BlockNumber,
	parent_head: HeadData,
	para_head: HeadData,
	hrmp_watermark: BlockNumber,
	scheduling_parent: Option<Hash>,
}

impl CandidateBuilder {
	fn new(para_id: ParaId, relay_parent: Hash) -> Self {
		Self {
			para_id,
			relay_parent,
			relay_parent_number: 0,
			parent_head: HeadData::default(),
			para_head: HeadData::default(),
			hrmp_watermark: 0,
			scheduling_parent: None,
		}
	}

	fn relay_parent_number(mut self, n: BlockNumber) -> Self {
		self.relay_parent_number = n;
		self
	}

	fn parent_head(mut self, head: HeadData) -> Self {
		self.parent_head = head;
		self
	}

	fn para_head(mut self, head: HeadData) -> Self {
		self.para_head = head;
		self
	}

	fn hrmp_watermark(mut self, n: BlockNumber) -> Self {
		self.hrmp_watermark = n;
		self
	}

	fn scheduling_parent(mut self, hash: Hash) -> Self {
		self.scheduling_parent = Some(hash);
		self
	}

	fn build(self) -> (PersistedValidationData, CommittedCandidateReceipt) {
		let persisted_validation_data = PersistedValidationData {
			parent_head: self.parent_head,
			relay_parent_number: self.relay_parent_number,
			relay_parent_storage_root: Hash::zero(),
			max_pov_size: 1_000_000,
		};

		let descriptor = if let Some(scheduling_parent) = self.scheduling_parent {
			// V3 descriptors must be constructed directly (not via V1→V2 conversion)
			// because the conversion puts collator bytes into reserved1, which would
			// cause v3_version() to misdetect the descriptor as V1.
			test_helpers::make_valid_candidate_descriptor_v3(
				self.para_id,
				self.relay_parent,
				CoreIndex(0),
				1,
				1,
				persisted_validation_data.hash(),
				Hash::repeat_byte(1),
				Hash::repeat_byte(42),
				self.para_head.hash(),
				Hash::repeat_byte(1),
				scheduling_parent,
			)
		} else {
			let d: CandidateDescriptorV2<Hash> = CandidateDescriptor {
				para_id: self.para_id,
				relay_parent: self.relay_parent,
				collator: test_helpers::dummy_collator(),
				persisted_validation_data_hash: persisted_validation_data.hash(),
				pov_hash: Hash::repeat_byte(1),
				erasure_root: Hash::repeat_byte(1),
				signature: test_helpers::zero_collator_signature(),
				para_head: self.para_head.hash(),
				validation_code_hash: Hash::repeat_byte(42).into(),
			}
			.into();
			d
		};

		let candidate = CommittedCandidateReceipt {
			descriptor,
			commitments: CandidateCommitments {
				upward_messages: Default::default(),
				horizontal_messages: Default::default(),
				new_validation_code: None,
				head_data: self.para_head,
				processed_downward_messages: 1,
				hrmp_watermark: self.hrmp_watermark,
			},
		};

		(persisted_validation_data, candidate)
	}
}

fn populate_chain_from_previous_storage(
	scheduling_scope: &SchedulingScope,
	scope: &Scope,
	storage: &CandidateStorage,
) -> FragmentChain {
	let mut chain =
		FragmentChain::init(scheduling_scope, scope.clone(), CandidateStorage::default());
	let mut prev_chain = chain.clone();
	prev_chain.unconnected = storage.clone();

	chain.populate_from_previous(scheduling_scope, &prev_chain);
	chain
}

#[test]
fn scope_rejects_ancestors_that_skip_blocks() {
	assert_matches!(
		SchedulingScope::new((Hash::repeat_byte(10), 10), vec![(Hash::repeat_byte(8), 8)],),
		Err(UnexpectedAncestor { number: 8, prev: 10 })
	);
}

#[test]
fn scope_rejects_ancestor_for_0_block() {
	assert_matches!(
		SchedulingScope::new((Hash::repeat_byte(0), 0), vec![(Hash::repeat_byte(99), 99999)],),
		Err(UnexpectedAncestor { number: 99999, prev: 0 })
	);
}

#[test]
fn scope_takes_all_ancestors() {
	let scheduling_scope = SchedulingScope::new(
		(Hash::repeat_byte(0), 5),
		vec![(Hash::repeat_byte(4), 4), (Hash::repeat_byte(3), 3), (Hash::repeat_byte(2), 2)],
	)
	.unwrap();

	// Should include all provided ancestors + the scheduling parent itself.
	assert_eq!(scheduling_scope.scheduling_parents.len(), 4);
}

#[test]
fn scope_rejects_unordered_ancestors() {
	assert_matches!(
		SchedulingScope::new(
			(Hash::repeat_byte(0), 5),
			vec![(Hash::repeat_byte(4), 4), (Hash::repeat_byte(2), 2), (Hash::repeat_byte(3), 3),],
		),
		Err(UnexpectedAncestor { number: 2, prev: 4 })
	);
}

#[test]
fn candidate_storage_methods() {
	let mut storage = CandidateStorage::default();
	let relay_parent = Hash::repeat_byte(69);

	let (pvd, candidate) = CandidateBuilder::new(ParaId::from(5u32), relay_parent)
		.relay_parent_number(8)
		.parent_head(vec![4, 5, 6].into())
		.para_head(vec![1, 2, 3].into())
		.hrmp_watermark(7)
		.build();

	let candidate_hash = candidate.hash();
	let parent_head_hash = pvd.parent_head.hash();

	// Invalid pvd hash
	let mut wrong_pvd = pvd.clone();
	wrong_pvd.max_pov_size = 0;
	assert_matches!(
		CandidateEntry::new(
			candidate_hash,
			candidate.clone(),
			wrong_pvd.clone(),
			CandidateState::Seconded,
		),
		Err(CandidateEntryError::PersistedValidationDataMismatch)
	);
	assert_matches!(
		CandidateEntry::new_seconded(candidate_hash, candidate.clone(), wrong_pvd),
		Err(CandidateEntryError::PersistedValidationDataMismatch)
	);
	// Zero-length cycle.
	{
		let mut candidate = candidate.clone();
		candidate.commitments.head_data = HeadData(vec![1; 10]);
		let mut pvd = pvd.clone();
		pvd.parent_head = HeadData(vec![1; 10]);
		candidate.descriptor.set_persisted_validation_data_hash(pvd.hash());
		assert_matches!(
			CandidateEntry::new_seconded(candidate_hash, candidate, pvd),
			Err(CandidateEntryError::ZeroLengthCycle)
		);
	}
	assert!(!storage.contains(&candidate_hash));
	assert_eq!(storage.possible_backed_para_children(&parent_head_hash).count(), 0);
	assert_eq!(storage.head_data_by_hash(&candidate.descriptor.para_head()), None);
	assert_eq!(storage.head_data_by_hash(&parent_head_hash), None);

	// Add a valid candidate.
	let candidate_entry = CandidateEntry::new(
		candidate_hash,
		candidate.clone(),
		pvd.clone(),
		CandidateState::Seconded,
	)
	.unwrap();
	storage.add_candidate_entry(candidate_entry.clone()).unwrap();
	assert!(storage.contains(&candidate_hash));
	assert_eq!(storage.possible_backed_para_children(&parent_head_hash).count(), 0);
	assert_eq!(storage.possible_backed_para_children(&candidate.descriptor.para_head()).count(), 0);
	assert_eq!(
		storage.head_data_by_hash(&candidate.descriptor.para_head()).unwrap(),
		&candidate.commitments.head_data
	);
	assert_eq!(storage.head_data_by_hash(&parent_head_hash).unwrap(), &pvd.parent_head);

	// Now mark it as backed
	storage.mark_backed(&candidate_hash);
	// Marking it twice is fine.
	storage.mark_backed(&candidate_hash);
	assert_eq!(
		storage
			.possible_backed_para_children(&parent_head_hash)
			.map(|c| c.candidate_hash)
			.collect::<Vec<_>>(),
		vec![candidate_hash]
	);
	assert_eq!(storage.possible_backed_para_children(&candidate.descriptor.para_head()).count(), 0);

	// Re-adding a candidate fails.
	assert_matches!(
		storage.add_candidate_entry(candidate_entry),
		Err(Error::CandidateAlreadyKnown)
	);

	// Remove candidate and re-add it later in backed state.
	storage.remove_candidate(&candidate_hash);
	assert!(!storage.contains(&candidate_hash));

	// Removing it twice is fine.
	storage.remove_candidate(&candidate_hash);
	assert!(!storage.contains(&candidate_hash));
	assert_eq!(storage.possible_backed_para_children(&parent_head_hash).count(), 0);
	assert_eq!(storage.head_data_by_hash(&candidate.descriptor.para_head()), None);
	assert_eq!(storage.head_data_by_hash(&parent_head_hash), None);

	storage
		.add_pending_availability_candidate(candidate_hash, candidate.clone(), pvd)
		.unwrap();
	assert!(storage.contains(&candidate_hash));

	assert_eq!(
		storage
			.possible_backed_para_children(&parent_head_hash)
			.map(|c| c.candidate_hash)
			.collect::<Vec<_>>(),
		vec![candidate_hash]
	);
	assert_eq!(storage.possible_backed_para_children(&candidate.descriptor.para_head()).count(), 0);

	// Now add a second candidate in Seconded state. This will be a fork.
	let (pvd_2, candidate_2) = CandidateBuilder::new(ParaId::from(5u32), relay_parent)
		.relay_parent_number(8)
		.parent_head(vec![4, 5, 6].into())
		.para_head(vec![2, 3, 4].into())
		.hrmp_watermark(7)
		.build();
	let candidate_hash_2 = candidate_2.hash();
	let candidate_entry_2 =
		CandidateEntry::new_seconded(candidate_hash_2, candidate_2, pvd_2).unwrap();

	storage.add_candidate_entry(candidate_entry_2).unwrap();
	assert_eq!(
		storage
			.possible_backed_para_children(&parent_head_hash)
			.map(|c| c.candidate_hash)
			.collect::<Vec<_>>(),
		vec![candidate_hash]
	);

	// Now mark it as backed.
	storage.mark_backed(&candidate_hash_2);
	assert_eq!(
		storage
			.possible_backed_para_children(&parent_head_hash)
			.map(|c| c.candidate_hash)
			.collect::<HashSet<_>>(),
		[candidate_hash, candidate_hash_2].into_iter().collect()
	);
}

#[test]
fn init_and_populate_from_empty() {
	// Empty chain and empty storage.
	let base_constraints = make_constraints(0, vec![0], vec![0x0a].into());

	let (relay_chain_scope, scope) = make_scope(
		RelayChainBlockInfo {
			number: 1,
			hash: Hash::repeat_byte(1),
			storage_root: Hash::repeat_byte(2),
		},
		base_constraints,
		Vec::new(),
		4,
		vec![],
	);
	let chain = FragmentChain::init(&relay_chain_scope, scope.clone(), CandidateStorage::default());
	assert_eq!(chain.len(), 0);
	assert_eq!(chain.unconnected_len(), 0);

	let mut new_chain = FragmentChain::init(&relay_chain_scope, scope, CandidateStorage::default());
	new_chain.populate_from_previous(&relay_chain_scope, &chain);
	assert_eq!(chain.len(), 0);
	assert_eq!(chain.unconnected_len(), 0);
}

#[test]
fn test_populate_and_check_potential() {
	let mut storage = CandidateStorage::default();

	let para_id = ParaId::from(5u32);
	let relay_parent_x = Hash::repeat_byte(1);
	let relay_parent_y = Hash::repeat_byte(2);
	let relay_parent_z = Hash::repeat_byte(3);
	let relay_parent_x_info =
		RelayChainBlockInfo { number: 0, hash: relay_parent_x, storage_root: Hash::zero() };
	let relay_parent_y_info =
		RelayChainBlockInfo { number: 1, hash: relay_parent_y, storage_root: Hash::zero() };
	let relay_parent_z_info =
		RelayChainBlockInfo { number: 2, hash: relay_parent_z, storage_root: Hash::zero() };

	let ancestors = vec![
		// These need to be ordered in reverse.
		relay_parent_y_info.clone(),
		relay_parent_x_info.clone(),
	];

	let base_constraints = make_constraints(0, vec![0], vec![0x0a].into());

	// Candidates A -> B -> C. They are all backed
	let (pvd_a, candidate_a) = CandidateBuilder::new(para_id, relay_parent_x_info.hash)
		.relay_parent_number(relay_parent_x_info.number)
		.parent_head(vec![0x0a].into())
		.para_head(vec![0x0b].into())
		.hrmp_watermark(relay_parent_x_info.number)
		.build();
	let candidate_a_hash = candidate_a.hash();
	let candidate_a_entry =
		CandidateEntry::new(candidate_a_hash, candidate_a, pvd_a.clone(), CandidateState::Backed)
			.unwrap();
	storage.add_candidate_entry(candidate_a_entry.clone()).unwrap();
	let (pvd_b, candidate_b) = CandidateBuilder::new(para_id, relay_parent_y_info.hash)
		.relay_parent_number(relay_parent_y_info.number)
		.parent_head(vec![0x0b].into())
		.para_head(vec![0x0c].into())
		.hrmp_watermark(relay_parent_y_info.number)
		.build();
	let candidate_b_hash = candidate_b.hash();
	let candidate_b_entry =
		CandidateEntry::new(candidate_b_hash, candidate_b, pvd_b, CandidateState::Backed).unwrap();
	storage.add_candidate_entry(candidate_b_entry.clone()).unwrap();
	let (pvd_c, candidate_c) = CandidateBuilder::new(para_id, relay_parent_z_info.hash)
		.relay_parent_number(relay_parent_z_info.number)
		.parent_head(vec![0x0c].into())
		.para_head(vec![0x0d].into())
		.hrmp_watermark(relay_parent_z_info.number)
		.build();
	let candidate_c_hash = candidate_c.hash();
	let candidate_c_entry =
		CandidateEntry::new(candidate_c_hash, candidate_c, pvd_c, CandidateState::Backed).unwrap();
	storage.add_candidate_entry(candidate_c_entry.clone()).unwrap();

	// Candidate A doesn't adhere to the base constraints.
	{
		for wrong_constraints in [
			// Different required parent
			make_constraints(
				relay_parent_x_info.number,
				vec![relay_parent_x_info.number],
				vec![0x0e].into(),
			),
			// Min relay parent number is wrong
			make_constraints(relay_parent_y_info.number, vec![0], vec![0x0a].into()),
		] {
			// If min_relay_parent_number is 1, only include ancestors down to block 1
			let ancestors_for_scope =
				if wrong_constraints.min_relay_parent_number == relay_parent_y_info.number {
					vec![relay_parent_y_info.clone()]
				} else {
					ancestors.clone()
				};
			let (relay_chain_scope, scope) = make_scope(
				relay_parent_z_info.clone(),
				wrong_constraints.clone(),
				vec![],
				5,
				ancestors_for_scope,
			);
			let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);

			assert!(chain.candidate_hashes().is_empty());

			// If the min relay parent number is wrong, candidate A can never become valid.
			// Otherwise, if only the required parent doesn't match, candidate A is still a
			// potential candidate.
			if wrong_constraints.min_relay_parent_number == relay_parent_y_info.number {
				// If A is not a potential candidate, its descendants will also not be added.
				assert_eq!(chain.unconnected_len(), 0);
				assert_matches!(
					chain.can_add_candidate_as_potential(&relay_chain_scope, &candidate_a_entry),
					Err(Error::SchedulingParentNotInScope(_))
				);
				// However, if taken independently, both B and C still have potential, since we
				// don't know that A doesn't.
				assert!(chain
					.can_add_candidate_as_potential(&relay_chain_scope, &candidate_b_entry)
					.is_ok());
				assert!(chain
					.can_add_candidate_as_potential(&relay_chain_scope, &candidate_c_entry)
					.is_ok());
			} else {
				assert_eq!(
					chain.unconnected().map(|c| c.candidate_hash).collect::<HashSet<_>>(),
					[candidate_a_hash, candidate_b_hash, candidate_c_hash].into_iter().collect()
				);
			}
		}
	}

	// Various depths
	{
		// Depth is 0, doesn't allow any candidate, but the others will be kept as potential.
		let (relay_chain_scope, scope) = make_scope(
			relay_parent_z_info.clone(),
			base_constraints.clone(),
			vec![],
			0,
			ancestors.clone(),
		);
		let chain =
			FragmentChain::init(&relay_chain_scope, scope.clone(), CandidateStorage::default());
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_a_entry)
			.is_ok());
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_b_entry)
			.is_ok());
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_c_entry)
			.is_ok());

		let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);
		assert!(chain.candidate_hashes().is_empty());
		assert_eq!(
			chain.unconnected().map(|c| c.candidate_hash).collect::<HashSet<_>>(),
			[candidate_a_hash, candidate_b_hash, candidate_c_hash].into_iter().collect()
		);

		// Depth is 1, only allows one candidate, but the others will be kept as potential.
		let (relay_chain_scope, scope) = make_scope(
			relay_parent_z_info.clone(),
			base_constraints.clone(),
			vec![],
			1,
			ancestors.clone(),
		);
		let chain =
			FragmentChain::init(&relay_chain_scope, scope.clone(), CandidateStorage::default());
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_a_entry)
			.is_ok());
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_b_entry)
			.is_ok());
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_c_entry)
			.is_ok());

		let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);
		assert_eq!(chain.candidate_hashes(), vec![candidate_a_hash]);
		assert_eq!(
			chain.unconnected().map(|c| c.candidate_hash).collect::<HashSet<_>>(),
			[candidate_b_hash, candidate_c_hash].into_iter().collect()
		);

		// depth is 2, allows two candidates
		let (relay_chain_scope, scope) = make_scope(
			relay_parent_z_info.clone(),
			base_constraints.clone(),
			vec![],
			2,
			ancestors.clone(),
		);
		let chain =
			FragmentChain::init(&relay_chain_scope, scope.clone(), CandidateStorage::default());
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_a_entry)
			.is_ok());
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_b_entry)
			.is_ok());
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_c_entry)
			.is_ok());

		let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);
		assert_eq!(chain.candidate_hashes(), vec![candidate_a_hash, candidate_b_hash]);
		assert_eq!(
			chain.unconnected().map(|c| c.candidate_hash).collect::<HashSet<_>>(),
			[candidate_c_hash].into_iter().collect()
		);

		// depth is at least 3, allows all three candidates
		for depth in 3..6 {
			let (relay_chain_scope, scope) = make_scope(
				relay_parent_z_info.clone(),
				base_constraints.clone(),
				vec![],
				depth,
				ancestors.clone(),
			);
			let chain =
				FragmentChain::init(&relay_chain_scope, scope.clone(), CandidateStorage::default());
			assert!(chain
				.can_add_candidate_as_potential(&relay_chain_scope, &candidate_a_entry)
				.is_ok());
			assert!(chain
				.can_add_candidate_as_potential(&relay_chain_scope, &candidate_b_entry)
				.is_ok());
			assert!(chain
				.can_add_candidate_as_potential(&relay_chain_scope, &candidate_c_entry)
				.is_ok());

			let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);
			assert_eq!(
				chain.candidate_hashes(),
				vec![candidate_a_hash, candidate_b_hash, candidate_c_hash]
			);
			assert_eq!(chain.unconnected_len(), 0);
		}
	}

	// Scheduling parents out of scope
	{
		// Candidate A has scheduling parent out of scope (for V1/V2, scheduling_parent ==
		// relay_parent). Candidates B and C will also be deleted since they form a chain with A.
		let ancestors_without_x = vec![relay_parent_y_info.clone()];
		let (relay_chain_scope, scope) = make_scope(
			relay_parent_z_info.clone(),
			base_constraints.clone(),
			vec![],
			5,
			ancestors_without_x,
		);
		let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);
		assert!(chain.candidate_hashes().is_empty());
		assert_eq!(chain.unconnected_len(), 0);

		assert_matches!(
			chain.can_add_candidate_as_potential(&relay_chain_scope, &candidate_a_entry),
			Err(Error::SchedulingParentNotInScope(_))
		);
		// However, if taken independently, both B and C still have potential, since we
		// don't know that A doesn't.
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_b_entry)
			.is_ok());
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_c_entry)
			.is_ok());

		// Candidates A and B have scheduling parents out of scope. Candidate C will also be
		// deleted since it forms a chain with A and B.
		let (relay_chain_scope, scope) =
			make_scope(relay_parent_z_info.clone(), base_constraints.clone(), vec![], 5, vec![]);
		let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);

		assert!(chain.candidate_hashes().is_empty());
		assert_eq!(chain.unconnected_len(), 0);

		assert_matches!(
			chain.can_add_candidate_as_potential(&relay_chain_scope, &candidate_a_entry),
			Err(Error::SchedulingParentNotInScope(_))
		);
		assert_matches!(
			chain.can_add_candidate_as_potential(&relay_chain_scope, &candidate_b_entry),
			Err(Error::SchedulingParentNotInScope(_))
		);
		// However, if taken independently, C still has potential, since we
		// don't know that A and B don't
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_c_entry)
			.is_ok());
	}

	// Parachain cycle is not allowed. Make C have the same parent as A.
	{
		let mut modified_storage = storage.clone();
		modified_storage.remove_candidate(&candidate_c_hash);
		let (wrong_pvd_c, wrong_candidate_c) =
			CandidateBuilder::new(para_id, relay_parent_z_info.hash)
				.relay_parent_number(relay_parent_z_info.number)
				.parent_head(vec![0x0c].into())
				.para_head(vec![0x0a].into())
				.hrmp_watermark(relay_parent_z_info.number)
				.build();
		let wrong_candidate_c_entry = CandidateEntry::new(
			wrong_candidate_c.hash(),
			wrong_candidate_c,
			wrong_pvd_c,
			CandidateState::Backed,
		)
		.unwrap();
		modified_storage.add_candidate_entry(wrong_candidate_c_entry.clone()).unwrap();
		let (relay_chain_scope, scope) = make_scope(
			relay_parent_z_info.clone(),
			base_constraints.clone(),
			vec![],
			5,
			ancestors.clone(),
		);

		let chain =
			populate_chain_from_previous_storage(&relay_chain_scope, &scope, &modified_storage);
		assert_eq!(chain.candidate_hashes(), vec![candidate_a_hash, candidate_b_hash]);
		assert_eq!(chain.unconnected_len(), 0);

		assert_matches!(
			chain.can_add_candidate_as_potential(&relay_chain_scope, &wrong_candidate_c_entry),
			Err(Error::Cycle)
		);
		// However, if taken independently, C still has potential, since we don't know A and B.
		let chain =
			FragmentChain::init(&relay_chain_scope, scope.clone(), CandidateStorage::default());
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &wrong_candidate_c_entry)
			.is_ok());
	}

	// Candidate C has the same relay parent as candidate A's parent. Relay parent not allowed
	// to move backwards
	let mut modified_storage = storage.clone();
	modified_storage.remove_candidate(&candidate_c_hash);
	let (wrong_pvd_c, wrong_candidate_c) = CandidateBuilder::new(para_id, relay_parent_x_info.hash)
		.relay_parent_number(relay_parent_x_info.number)
		.parent_head(vec![0x0c].into())
		.para_head(vec![0x0d].into())
		.hrmp_watermark(0)
		.build();
	let wrong_candidate_c_entry = CandidateEntry::new(
		wrong_candidate_c.hash(),
		wrong_candidate_c,
		wrong_pvd_c,
		CandidateState::Backed,
	)
	.unwrap();
	modified_storage.add_candidate_entry(wrong_candidate_c_entry.clone()).unwrap();
	let (relay_chain_scope, scope) = make_scope(
		relay_parent_z_info.clone(),
		base_constraints.clone(),
		vec![],
		5,
		ancestors.clone(),
	);

	let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &modified_storage);

	assert_eq!(chain.candidate_hashes(), vec![candidate_a_hash, candidate_b_hash]);
	assert_eq!(chain.unconnected_len(), 0);
	assert_matches!(
		chain.can_add_candidate_as_potential(&relay_chain_scope, &wrong_candidate_c_entry),
		Err(Error::RelayParentMovedBackwards)
	);

	// Candidate C is an unconnected candidate.
	// C's relay parent is allowed to move backwards from B's relay parent, because C may later on
	// trigger a reorg and B may get removed.
	let mut modified_storage = storage.clone();
	modified_storage.remove_candidate(&candidate_c_hash);
	let (unconnected_pvd_c, unconnected_candidate_c) =
		CandidateBuilder::new(para_id, relay_parent_x_info.hash)
			.relay_parent_number(relay_parent_x_info.number)
			.parent_head(vec![0x0d].into())
			.para_head(vec![0x0e].into())
			.hrmp_watermark(0)
			.build();
	let unconnected_candidate_c_hash = unconnected_candidate_c.hash();
	let unconnected_candidate_c_entry = CandidateEntry::new(
		unconnected_candidate_c_hash,
		unconnected_candidate_c,
		unconnected_pvd_c,
		CandidateState::Backed,
	)
	.unwrap();
	modified_storage
		.add_candidate_entry(unconnected_candidate_c_entry.clone())
		.unwrap();
	let (relay_chain_scope, scope) = make_scope(
		relay_parent_z_info.clone(),
		base_constraints.clone(),
		vec![],
		5,
		ancestors.clone(),
	);
	let chain = FragmentChain::init(&relay_chain_scope, scope.clone(), CandidateStorage::default());
	assert!(chain
		.can_add_candidate_as_potential(&relay_chain_scope, &unconnected_candidate_c_entry)
		.is_ok());

	let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &modified_storage);

	assert_eq!(chain.candidate_hashes(), vec![candidate_a_hash, candidate_b_hash]);
	assert_eq!(
		chain.unconnected().map(|c| c.candidate_hash).collect::<HashSet<_>>(),
		[unconnected_candidate_c_hash].into_iter().collect()
	);

	// Candidate A is a pending availability candidate and Candidate C is an unconnected candidate,
	// C's relay parent is not allowed to move backwards from A's relay parent because we're sure A
	// will not get removed in the future, as it's already on-chain (unless it times out
	// availability, a case for which we don't care to optimise for)

	modified_storage.remove_candidate(&candidate_a_hash);
	let (modified_pvd_a, modified_candidate_a) =
		CandidateBuilder::new(para_id, relay_parent_y_info.hash)
			.relay_parent_number(relay_parent_y_info.number)
			.parent_head(vec![0x0a].into())
			.para_head(vec![0x0b].into())
			.hrmp_watermark(relay_parent_y_info.number)
			.build();
	let modified_candidate_a_hash = modified_candidate_a.hash();
	modified_storage
		.add_candidate_entry(
			CandidateEntry::new(
				modified_candidate_a_hash,
				modified_candidate_a,
				modified_pvd_a,
				CandidateState::Backed,
			)
			.unwrap(),
		)
		.unwrap();

	let (relay_chain_scope, scope) = make_scope(
		relay_parent_z_info.clone(),
		base_constraints.clone(),
		vec![PendingAvailability {
			candidate_hash: modified_candidate_a_hash,
			relay_parent: relay_parent_y_info.clone(),
		}],
		4,
		ancestors.clone(),
	);

	let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &modified_storage);
	assert_eq!(chain.candidate_hashes(), vec![modified_candidate_a_hash, candidate_b_hash]);
	assert_eq!(chain.unconnected_len(), 0);
	assert_matches!(
		chain.can_add_candidate_as_potential(&relay_chain_scope, &unconnected_candidate_c_entry),
		Err(Error::RelayParentPrecedesCandidatePendingAvailability(_, _))
	);

	// Not allowed to fork from a candidate pending availability
	let (wrong_pvd_c, wrong_candidate_c) = CandidateBuilder::new(para_id, relay_parent_y_info.hash)
		.relay_parent_number(relay_parent_y_info.number)
		.parent_head(vec![0x0a].into())
		.para_head(vec![0x0b2].into())
		.hrmp_watermark(0)
		.build();
	let wrong_candidate_c_hash = wrong_candidate_c.hash();
	let wrong_candidate_c_entry = CandidateEntry::new(
		wrong_candidate_c_hash,
		wrong_candidate_c,
		wrong_pvd_c,
		CandidateState::Backed,
	)
	.unwrap();
	modified_storage.add_candidate_entry(wrong_candidate_c_entry.clone()).unwrap();

	// Does not even matter if the fork selection rule would have picked up the new candidate, as
	// the other is already pending availability.
	assert_eq!(
		fork_selection_rule(&wrong_candidate_c_hash, &modified_candidate_a_hash),
		Ordering::Less
	);

	let (relay_chain_scope, scope) = make_scope(
		relay_parent_z_info.clone(),
		base_constraints.clone(),
		vec![PendingAvailability {
			candidate_hash: modified_candidate_a_hash,
			relay_parent: relay_parent_y_info.clone(),
		}],
		4,
		ancestors.clone(),
	);

	let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &modified_storage);
	assert_eq!(chain.candidate_hashes(), vec![modified_candidate_a_hash, candidate_b_hash]);
	assert_eq!(chain.unconnected_len(), 0);
	assert_matches!(
		chain.can_add_candidate_as_potential(&relay_chain_scope, &wrong_candidate_c_entry),
		Err(Error::ForkWithCandidatePendingAvailability(_))
	);

	// Test with candidates pending availability
	{
		// Valid options
		for pending in [
			vec![PendingAvailability {
				candidate_hash: candidate_a_hash,
				relay_parent: relay_parent_x_info.clone(),
			}],
			vec![
				PendingAvailability {
					candidate_hash: candidate_a_hash,
					relay_parent: relay_parent_x_info.clone(),
				},
				PendingAvailability {
					candidate_hash: candidate_b_hash,
					relay_parent: relay_parent_y_info.clone(),
				},
			],
			vec![
				PendingAvailability {
					candidate_hash: candidate_a_hash,
					relay_parent: relay_parent_x_info.clone(),
				},
				PendingAvailability {
					candidate_hash: candidate_b_hash,
					relay_parent: relay_parent_y_info.clone(),
				},
				PendingAvailability {
					candidate_hash: candidate_c_hash,
					relay_parent: relay_parent_z_info.clone(),
				},
			],
		] {
			let (relay_chain_scope, scope) = make_scope(
				relay_parent_z_info.clone(),
				base_constraints.clone(),
				pending,
				3,
				ancestors.clone(),
			);
			let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);
			assert_eq!(
				chain.candidate_hashes(),
				vec![candidate_a_hash, candidate_b_hash, candidate_c_hash]
			);
			assert_eq!(chain.unconnected_len(), 0);
		}

		// Relay parents of pending availability candidates can be out of scope
		// Relay parent of candidate A is out of scope.
		let ancestors_without_x = vec![relay_parent_y_info.clone()];
		let (relay_chain_scope, scope) = make_scope(
			relay_parent_z_info.clone(),
			base_constraints.clone(),
			vec![PendingAvailability {
				candidate_hash: candidate_a_hash,
				relay_parent: relay_parent_x_info.clone(),
			}],
			4,
			ancestors_without_x,
		);
		let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);

		assert_eq!(
			chain.candidate_hashes(),
			vec![candidate_a_hash, candidate_b_hash, candidate_c_hash]
		);
		assert_eq!(chain.unconnected_len(), 0);

		// Even relay parents of pending availability candidates which are out of scope cannot
		// move backwards.
		let (relay_chain_scope, scope) = make_scope(
			relay_parent_z_info.clone(),
			base_constraints.clone(),
			vec![
				PendingAvailability {
					candidate_hash: candidate_a_hash,
					relay_parent: RelayChainBlockInfo {
						hash: relay_parent_x_info.hash,
						number: 1,
						storage_root: relay_parent_x_info.storage_root,
					},
				},
				PendingAvailability {
					candidate_hash: candidate_b_hash,
					relay_parent: RelayChainBlockInfo {
						hash: relay_parent_y_info.hash,
						number: 0,
						storage_root: relay_parent_y_info.storage_root,
					},
				},
			],
			4,
			vec![],
		);
		let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);
		assert!(chain.candidate_hashes().is_empty());
		assert_eq!(chain.unconnected_len(), 0);
	}

	// More complex case:
	// max_depth is 2 (a chain of max depth 3).
	// A -> B -> C are the backable chain.
	// D is backed but would exceed the max depth.
	// F is unconnected and seconded.
	// A1 has same parent as A, is backed but has a higher candidate hash. It'll therefore be
	// deleted.
	// 	A1 has underneath a subtree that will all need to be trimmed. A1 -> B1. B1 -> C1
	// 	and B1 -> C2. (C1 is backed).
	// A2 is seconded but is kept because it has a lower candidate hash than A.
	// A2 points to B2, which is backed.
	//
	// Check that D, F, A2 and B2 are kept as unconnected potential candidates.

	let (relay_chain_scope, scope) = make_scope(
		relay_parent_z_info.clone(),
		base_constraints.clone(),
		vec![],
		3,
		ancestors.clone(),
	);

	// Candidate D
	let (pvd_d, candidate_d) = CandidateBuilder::new(para_id, relay_parent_z_info.hash)
		.relay_parent_number(relay_parent_z_info.number)
		.parent_head(vec![0x0d].into())
		.para_head(vec![0x0e].into())
		.hrmp_watermark(relay_parent_z_info.number)
		.build();
	let candidate_d_hash = candidate_d.hash();
	let candidate_d_entry =
		CandidateEntry::new(candidate_d_hash, candidate_d, pvd_d, CandidateState::Backed).unwrap();
	assert!(populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage)
		.can_add_candidate_as_potential(&relay_chain_scope, &candidate_d_entry)
		.is_ok());
	storage.add_candidate_entry(candidate_d_entry).unwrap();

	// Candidate F
	let (pvd_f, candidate_f) = CandidateBuilder::new(para_id, relay_parent_z_info.hash)
		.relay_parent_number(relay_parent_z_info.number)
		.parent_head(vec![0x0f].into())
		.para_head(vec![0xf1].into())
		.hrmp_watermark(1000)
		.build();
	let candidate_f_hash = candidate_f.hash();
	let candidate_f_entry =
		CandidateEntry::new(candidate_f_hash, candidate_f, pvd_f, CandidateState::Seconded)
			.unwrap();
	assert!(populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage)
		.can_add_candidate_as_potential(&relay_chain_scope, &candidate_f_entry)
		.is_ok());
	storage.add_candidate_entry(candidate_f_entry.clone()).unwrap();

	// Candidate A1
	let (pvd_a1, candidate_a1) = CandidateBuilder::new(para_id, relay_parent_x_info.hash)
		.relay_parent_number(relay_parent_x_info.number)
		.parent_head(vec![0x0a].into())
		.para_head(vec![0xb1].into())
		.hrmp_watermark(relay_parent_x_info.number)
		.build();
	let candidate_a1_hash = candidate_a1.hash();
	let candidate_a1_entry =
		CandidateEntry::new(candidate_a1_hash, candidate_a1, pvd_a1, CandidateState::Backed)
			.unwrap();
	// Candidate A1 is created so that its hash is greater than the candidate A hash.
	assert_eq!(fork_selection_rule(&candidate_a_hash, &candidate_a1_hash), Ordering::Less);

	assert_matches!(
		populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage)
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_a1_entry),
		Err(Error::ForkChoiceRule(other)) if candidate_a_hash == other
	);

	storage.add_candidate_entry(candidate_a1_entry.clone()).unwrap();

	// Candidate B1.
	let (pvd_b1, candidate_b1) = CandidateBuilder::new(para_id, relay_parent_x_info.hash)
		.relay_parent_number(relay_parent_x_info.number)
		.parent_head(vec![0xb1].into())
		.para_head(vec![0xc1].into())
		.hrmp_watermark(relay_parent_x_info.number)
		.build();
	let candidate_b1_hash = candidate_b1.hash();
	let candidate_b1_entry =
		CandidateEntry::new(candidate_b1_hash, candidate_b1, pvd_b1, CandidateState::Seconded)
			.unwrap();
	assert!(populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage)
		.can_add_candidate_as_potential(&relay_chain_scope, &candidate_b1_entry)
		.is_ok());

	storage.add_candidate_entry(candidate_b1_entry).unwrap();

	// Candidate C1.
	let (pvd_c1, candidate_c1) = CandidateBuilder::new(para_id, relay_parent_x_info.hash)
		.relay_parent_number(relay_parent_x_info.number)
		.parent_head(vec![0xc1].into())
		.para_head(vec![0xd1].into())
		.hrmp_watermark(relay_parent_x_info.number)
		.build();
	let candidate_c1_hash = candidate_c1.hash();
	let candidate_c1_entry =
		CandidateEntry::new(candidate_c1_hash, candidate_c1, pvd_c1, CandidateState::Backed)
			.unwrap();
	assert!(populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage)
		.can_add_candidate_as_potential(&relay_chain_scope, &candidate_c1_entry)
		.is_ok());

	storage.add_candidate_entry(candidate_c1_entry).unwrap();

	// Candidate C2.
	let (pvd_c2, candidate_c2) = CandidateBuilder::new(para_id, relay_parent_x_info.hash)
		.relay_parent_number(relay_parent_x_info.number)
		.parent_head(vec![0xc1].into())
		.para_head(vec![0xd2].into())
		.hrmp_watermark(relay_parent_x_info.number)
		.build();
	let candidate_c2_hash = candidate_c2.hash();
	let candidate_c2_entry =
		CandidateEntry::new(candidate_c2_hash, candidate_c2, pvd_c2, CandidateState::Seconded)
			.unwrap();
	assert!(populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage)
		.can_add_candidate_as_potential(&relay_chain_scope, &candidate_c2_entry)
		.is_ok());
	storage.add_candidate_entry(candidate_c2_entry).unwrap();

	// Candidate A2.
	let (pvd_a2, candidate_a2) = CandidateBuilder::new(para_id, relay_parent_x_info.hash)
		.relay_parent_number(relay_parent_x_info.number)
		.parent_head(vec![0x0a].into())
		.para_head(vec![0xb3].into())
		.hrmp_watermark(relay_parent_x_info.number)
		.build();
	let candidate_a2_hash = candidate_a2.hash();
	let candidate_a2_entry =
		CandidateEntry::new(candidate_a2_hash, candidate_a2, pvd_a2, CandidateState::Seconded)
			.unwrap();
	// Candidate A2 is created so that its hash is greater than the candidate A hash.
	assert_eq!(fork_selection_rule(&candidate_a2_hash, &candidate_a_hash), Ordering::Less);

	assert!(populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage)
		.can_add_candidate_as_potential(&relay_chain_scope, &candidate_a2_entry)
		.is_ok());

	storage.add_candidate_entry(candidate_a2_entry).unwrap();

	// Candidate B2.
	let (pvd_b2, candidate_b2) = CandidateBuilder::new(para_id, relay_parent_y_info.hash)
		.relay_parent_number(relay_parent_y_info.number)
		.parent_head(vec![0xb3].into())
		.para_head(vec![0xb4].into())
		.hrmp_watermark(relay_parent_y_info.number)
		.build();
	let candidate_b2_hash = candidate_b2.hash();
	let candidate_b2_entry =
		CandidateEntry::new(candidate_b2_hash, candidate_b2, pvd_b2, CandidateState::Backed)
			.unwrap();
	assert!(populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage)
		.can_add_candidate_as_potential(&relay_chain_scope, &candidate_b2_entry)
		.is_ok());
	storage.add_candidate_entry(candidate_b2_entry).unwrap();

	let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);
	assert_eq!(
		chain.candidate_hashes(),
		vec![candidate_a_hash, candidate_b_hash, candidate_c_hash]
	);
	assert_eq!(
		chain.unconnected().map(|c| c.candidate_hash).collect::<HashSet<_>>(),
		[candidate_d_hash, candidate_f_hash, candidate_a2_hash, candidate_b2_hash]
			.into_iter()
			.collect()
	);
	// Cannot add as potential an already present candidate (whether it's in the chain or in
	// unconnected storage)
	assert_matches!(
		chain.can_add_candidate_as_potential(&relay_chain_scope, &candidate_a_entry),
		Err(Error::CandidateAlreadyKnown)
	);
	assert_matches!(
		chain.can_add_candidate_as_potential(&relay_chain_scope, &candidate_f_entry),
		Err(Error::CandidateAlreadyKnown)
	);

	// Simulate some chain reorgs.
	{
		// Back A2. The reversion should happen right at the root.
		let mut chain = chain.clone();
		chain.candidate_backed(&relay_chain_scope, &candidate_a2_hash);
		assert_eq!(chain.candidate_hashes(), vec![candidate_a2_hash, candidate_b2_hash]);
		// F is kept as it was truly unconnected. The rest will be trimmed.
		assert_eq!(
			chain.unconnected().map(|c| c.candidate_hash).collect::<HashSet<_>>(),
			[candidate_f_hash].into_iter().collect()
		);

		// A and A1 will never have potential again.
		assert_matches!(
			chain.can_add_candidate_as_potential(&relay_chain_scope, &candidate_a1_entry),
			Err(Error::ForkChoiceRule(_))
		);
		assert_matches!(
			chain.can_add_candidate_as_potential(&relay_chain_scope, &candidate_a_entry),
			Err(Error::ForkChoiceRule(_))
		);

		// Simulate a more complex chain reorg.
		// A2 points to B2, which is backed.
		// A2 has underneath a subtree A2 -> B2 -> C3 and A2 -> B2 -> C4. B2 and C3 are backed. C4
		// is kept because it has a lower candidate hash than C3. Backing C4 will cause a chain
		// reorg.

		// Candidate C3.
		let (pvd_c3, candidate_c3) = CandidateBuilder::new(para_id, relay_parent_y_info.hash)
			.relay_parent_number(relay_parent_y_info.number)
			.parent_head(vec![0xb4].into())
			.para_head(vec![0xc3].into())
			.hrmp_watermark(relay_parent_y_info.number)
			.build();
		let candidate_c3_hash = candidate_c3.hash();
		let candidate_c3_entry =
			CandidateEntry::new(candidate_c3_hash, candidate_c3, pvd_c3, CandidateState::Seconded)
				.unwrap();

		// Candidate C4.
		let (pvd_c4, candidate_c4) = CandidateBuilder::new(para_id, relay_parent_y_info.hash)
			.relay_parent_number(relay_parent_y_info.number)
			.parent_head(vec![0xb4].into())
			.para_head(vec![0xc2].into())
			.hrmp_watermark(relay_parent_y_info.number)
			.build();
		let candidate_c4_hash = candidate_c4.hash();
		// C4 should have a lower candidate hash than C3.
		assert_eq!(fork_selection_rule(&candidate_c4_hash, &candidate_c3_hash), Ordering::Less);
		let candidate_c4_entry =
			CandidateEntry::new(candidate_c4_hash, candidate_c4, pvd_c4, CandidateState::Seconded)
				.unwrap();

		let mut storage = storage.clone();
		storage.add_candidate_entry(candidate_c3_entry).unwrap();
		storage.add_candidate_entry(candidate_c4_entry).unwrap();
		let mut chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);
		chain.candidate_backed(&relay_chain_scope, &candidate_a2_hash);
		chain.candidate_backed(&relay_chain_scope, &candidate_c3_hash);

		assert_eq!(
			chain.candidate_hashes(),
			vec![candidate_a2_hash, candidate_b2_hash, candidate_c3_hash]
		);

		// Backing C4 will cause a reorg.
		chain.candidate_backed(&relay_chain_scope, &candidate_c4_hash);
		assert_eq!(
			chain.candidate_hashes(),
			vec![candidate_a2_hash, candidate_b2_hash, candidate_c4_hash]
		);

		assert_eq!(
			chain.unconnected().map(|c| c.candidate_hash).collect::<HashSet<_>>(),
			[candidate_f_hash].into_iter().collect()
		);
	}

	// Candidate F has an invalid hrmp watermark. however, it was not checked beforehand as we don't
	// have its parent yet. Add its parent now. This will not impact anything as E is not yet part
	// of the chain.

	let (pvd_e, candidate_e) = CandidateBuilder::new(para_id, relay_parent_z_info.hash)
		.relay_parent_number(relay_parent_z_info.number)
		.parent_head(vec![0x0e].into())
		.para_head(vec![0x0f].into())
		.hrmp_watermark(relay_parent_z_info.number)
		.build();
	let candidate_e_hash = candidate_e.hash();
	storage
		.add_candidate_entry(
			CandidateEntry::new(candidate_e_hash, candidate_e, pvd_e, CandidateState::Seconded)
				.unwrap(),
		)
		.unwrap();

	let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);
	assert_eq!(
		chain.candidate_hashes(),
		vec![candidate_a_hash, candidate_b_hash, candidate_c_hash]
	);
	assert_eq!(
		chain.unconnected().map(|c| c.candidate_hash).collect::<HashSet<_>>(),
		[
			candidate_d_hash,
			candidate_f_hash,
			candidate_a2_hash,
			candidate_b2_hash,
			candidate_e_hash
		]
		.into_iter()
		.collect()
	);

	// Simulate the fact that candidates A, B, C are now pending availability.
	let (relay_chain_scope, scope) = make_scope(
		relay_parent_z_info.clone(),
		base_constraints.clone(),
		vec![
			PendingAvailability {
				candidate_hash: candidate_a_hash,
				relay_parent: relay_parent_x_info,
			},
			PendingAvailability {
				candidate_hash: candidate_b_hash,
				relay_parent: relay_parent_y_info,
			},
			PendingAvailability {
				candidate_hash: candidate_c_hash,
				relay_parent: relay_parent_z_info.clone(),
			},
		],
		0,
		ancestors.clone(),
	);

	// A2 and B2 will now be trimmed
	let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);
	assert_eq!(
		chain.candidate_hashes(),
		vec![candidate_a_hash, candidate_b_hash, candidate_c_hash]
	);
	assert_eq!(
		chain.unconnected().map(|c| c.candidate_hash).collect::<HashSet<_>>(),
		[candidate_d_hash, candidate_f_hash, candidate_e_hash].into_iter().collect()
	);
	// Cannot add as potential an already pending availability candidate
	assert_matches!(
		chain.can_add_candidate_as_potential(&relay_chain_scope, &candidate_a_entry),
		Err(Error::CandidateAlreadyKnown)
	);

	// Simulate the fact that candidates A, B and C have been included.

	let base_constraints = make_constraints(0, vec![0], HeadData(vec![0x0d]));
	let (relay_chain_scope, scope) = make_scope(
		relay_parent_z_info.clone(),
		base_constraints.clone(),
		vec![],
		3,
		ancestors.clone(),
	);

	let prev_chain = chain;
	let mut chain = FragmentChain::init(&relay_chain_scope, scope, CandidateStorage::default());
	chain.populate_from_previous(&relay_chain_scope, &prev_chain);
	assert_eq!(chain.candidate_hashes(), vec![candidate_d_hash]);
	assert_eq!(
		chain.unconnected().map(|c| c.candidate_hash).collect::<HashSet<_>>(),
		[candidate_e_hash, candidate_f_hash].into_iter().collect()
	);

	// Mark E as backed. F will be dropped for invalid watermark. No other unconnected candidates.
	chain.candidate_backed(&relay_chain_scope, &candidate_e_hash);
	assert_eq!(chain.candidate_hashes(), vec![candidate_d_hash, candidate_e_hash]);
	assert_eq!(chain.unconnected_len(), 0);

	assert_matches!(
		chain.can_add_candidate_as_potential(&relay_chain_scope, &candidate_f_entry),
		Err(Error::CheckAgainstConstraints(_))
	);
}

#[test]
fn test_find_ancestor_path_and_find_backable_chain_empty_chain() {
	let relay_parent = Hash::repeat_byte(1);
	let required_parent: HeadData = vec![0xff].into();
	let max_depth = 11;

	// Empty chain
	let base_constraints = make_constraints(0, vec![0], required_parent.clone());

	let relay_parent_info =
		RelayChainBlockInfo { number: 0, hash: relay_parent, storage_root: Hash::zero() };

	let (relay_chain_scope, scope) =
		make_scope(relay_parent_info, base_constraints, vec![], max_depth, vec![]);
	let chain = FragmentChain::init(&relay_chain_scope, scope, CandidateStorage::default());
	assert_eq!(chain.len(), 0);

	assert_eq!(chain.find_ancestor_path(Ancestors::new()), 0);
	assert_eq!(chain.find_backable_chain(Ancestors::new(), 2), vec![]);
	// Invalid candidate.
	let ancestors: Ancestors = [CandidateHash::default()].into_iter().collect();
	assert_eq!(chain.find_ancestor_path(ancestors.clone()), 0);
	assert_eq!(chain.find_backable_chain(ancestors, 2), vec![]);
}

#[test]
fn test_find_ancestor_path_and_find_backable_chain() {
	let para_id = ParaId::from(5u32);
	let relay_parent = Hash::repeat_byte(1);
	let required_parent: HeadData = vec![0xff].into();
	let max_depth = 6;
	let relay_parent_number = 0;
	let relay_parent_storage_root = Hash::zero();

	let mut candidates = vec![];

	// Candidate 0
	candidates.push(
		CandidateBuilder::new(para_id, relay_parent)
			.relay_parent_number(0)
			.parent_head(required_parent.clone())
			.para_head(vec![0].into())
			.hrmp_watermark(0)
			.build(),
	);

	// Candidates 1..=5
	for index in 1..=5 {
		candidates.push(
			CandidateBuilder::new(para_id, relay_parent)
				.relay_parent_number(0)
				.parent_head(vec![index - 1].into())
				.para_head(vec![index].into())
				.hrmp_watermark(0)
				.build(),
		);
	}

	let mut storage = CandidateStorage::default();

	for (pvd, candidate) in candidates.iter() {
		storage
			.add_candidate_entry(
				CandidateEntry::new_seconded(candidate.hash(), candidate.clone(), pvd.clone())
					.unwrap(),
			)
			.unwrap();
	}

	let candidates = candidates
		.into_iter()
		.map(|(_pvd, candidate)| candidate.hash())
		.collect::<Vec<_>>();
	let hashes = |range: Range<usize>| {
		range
			.map(|i| BackableCandidateRef {
				candidate_hash: candidates[i],
				scheduling_parent: relay_parent,
			})
			.collect::<Vec<_>>()
	};

	let relay_parent_info = RelayChainBlockInfo {
		number: relay_parent_number,
		hash: relay_parent,
		storage_root: relay_parent_storage_root,
	};

	let base_constraints = make_constraints(0, vec![0], required_parent.clone());
	let (relay_chain_scope, scope) =
		make_scope(relay_parent_info.clone(), base_constraints.clone(), vec![], max_depth, vec![]);
	let mut chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);

	// For now, candidates are only seconded, not backed. So the chain is empty and no
	// candidate will be returned.
	assert_eq!(candidates.len(), 6);
	assert_eq!(chain.len(), 0);
	assert_eq!(chain.unconnected_len(), 6);

	for count in 0..10 {
		assert_eq!(chain.find_backable_chain(Ancestors::new(), count).len(), 0);
	}

	// Do tests with only a couple of candidates being backed.
	{
		let mut chain = chain.clone();
		chain.candidate_backed(&relay_chain_scope, &&candidates[5]);
		for count in 0..10 {
			assert_eq!(chain.find_backable_chain(Ancestors::new(), count).len(), 0);
		}
		chain.candidate_backed(&relay_chain_scope, &&candidates[3]);
		chain.candidate_backed(&relay_chain_scope, &&candidates[4]);
		for count in 0..10 {
			assert_eq!(chain.find_backable_chain(Ancestors::new(), count).len(), 0);
		}

		chain.candidate_backed(&relay_chain_scope, &&candidates[1]);
		for count in 0..10 {
			assert_eq!(chain.find_backable_chain(Ancestors::new(), count).len(), 0);
		}

		chain.candidate_backed(&relay_chain_scope, &&candidates[0]);
		assert_eq!(chain.find_backable_chain(Ancestors::new(), 1), hashes(0..1));
		for count in 2..10 {
			assert_eq!(chain.find_backable_chain(Ancestors::new(), count), hashes(0..2));
		}

		// Now back the missing piece.
		chain.candidate_backed(&relay_chain_scope, &&candidates[2]);
		assert_eq!(chain.len(), 6);
		for count in 0..10 {
			assert_eq!(
				chain.find_backable_chain(Ancestors::new(), count),
				(0..6)
					.take(count as usize)
					.map(|i| BackableCandidateRef {
						candidate_hash: candidates[i],
						scheduling_parent: relay_parent
					})
					.collect::<Vec<_>>()
			);
		}
	}

	// Now back all candidates. Back them in a random order. The result should always be the same.
	let mut candidates_shuffled = candidates.clone();
	candidates_shuffled.shuffle(&mut thread_rng());
	for candidate in candidates_shuffled.iter() {
		chain.candidate_backed(&relay_chain_scope, candidate);
		storage.mark_backed(candidate);
	}

	// No ancestors supplied.
	assert_eq!(chain.find_ancestor_path(Ancestors::new()), 0);
	assert_eq!(chain.find_backable_chain(Ancestors::new(), 0), vec![]);
	assert_eq!(chain.find_backable_chain(Ancestors::new(), 1), hashes(0..1));
	assert_eq!(chain.find_backable_chain(Ancestors::new(), 2), hashes(0..2));
	assert_eq!(chain.find_backable_chain(Ancestors::new(), 5), hashes(0..5));

	for count in 6..10 {
		assert_eq!(chain.find_backable_chain(Ancestors::new(), count), hashes(0..6));
	}

	assert_eq!(chain.find_backable_chain(Ancestors::new(), 7), hashes(0..6));
	assert_eq!(chain.find_backable_chain(Ancestors::new(), 10), hashes(0..6));

	// Ancestor which is not part of the chain. Will be ignored.
	let ancestors: Ancestors = [CandidateHash::default()].into_iter().collect();
	assert_eq!(chain.find_ancestor_path(ancestors.clone()), 0);
	assert_eq!(chain.find_backable_chain(ancestors, 4), hashes(0..4));

	let ancestors: Ancestors = [candidates[1], CandidateHash::default()].into_iter().collect();
	assert_eq!(chain.find_ancestor_path(ancestors.clone()), 0);
	assert_eq!(chain.find_backable_chain(ancestors, 4), hashes(0..4));

	let ancestors: Ancestors = [candidates[0], CandidateHash::default()].into_iter().collect();
	assert_eq!(chain.find_ancestor_path(ancestors.clone()), 1);
	assert_eq!(chain.find_backable_chain(ancestors, 4), hashes(1..5));

	// Ancestors which are part of the chain but don't form a path from root. Will be ignored.
	let ancestors: Ancestors = [candidates[1], candidates[2]].into_iter().collect();
	assert_eq!(chain.find_ancestor_path(ancestors.clone()), 0);
	assert_eq!(chain.find_backable_chain(ancestors, 4), hashes(0..4));

	// Valid ancestors.
	let ancestors: Ancestors = [candidates[2], candidates[0], candidates[1]].into_iter().collect();
	assert_eq!(chain.find_ancestor_path(ancestors.clone()), 3);
	assert_eq!(chain.find_backable_chain(ancestors.clone(), 2), hashes(3..5));
	for count in 3..10 {
		assert_eq!(chain.find_backable_chain(ancestors.clone(), count), hashes(3..6));
	}

	// Valid ancestors with candidates which have been omitted due to timeouts
	let ancestors: Ancestors = [candidates[0], candidates[2]].into_iter().collect();
	assert_eq!(chain.find_ancestor_path(ancestors.clone()), 1);
	assert_eq!(chain.find_backable_chain(ancestors.clone(), 3), hashes(1..4));
	assert_eq!(chain.find_backable_chain(ancestors.clone(), 4), hashes(1..5));
	for count in 5..10 {
		assert_eq!(chain.find_backable_chain(ancestors.clone(), count), hashes(1..6));
	}

	let ancestors: Ancestors = [candidates[0], candidates[1], candidates[3]].into_iter().collect();
	assert_eq!(chain.find_ancestor_path(ancestors.clone()), 2);
	assert_eq!(chain.find_backable_chain(ancestors.clone(), 4), hashes(2..6));

	// Requested count is 0.
	assert_eq!(chain.find_backable_chain(ancestors, 0), vec![]);

	// Stop when we've found a candidate which is pending availability
	{
		let (relay_chain_scope, scope) = make_scope(
			relay_parent_info.clone(),
			base_constraints,
			// Mark the third candidate as pending availability
			vec![PendingAvailability {
				candidate_hash: candidates[3],
				relay_parent: relay_parent_info,
			}],
			max_depth - 1,
			vec![],
		);
		let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);
		let ancestors: Ancestors = [candidates[0], candidates[1]].into_iter().collect();
		assert_eq!(
			// Stop at 4.
			chain.find_backable_chain(ancestors.clone(), 3),
			hashes(2..3)
		);
	}
}

#[test]
fn test_v3_scheduling_parent_validation() {
	let mut storage = CandidateStorage::default();

	let para_id = ParaId::from(5u32);
	let relay_parent_x = Hash::repeat_byte(1);
	let relay_parent_y = Hash::repeat_byte(2);
	let relay_parent_z = Hash::repeat_byte(3);
	let out_of_scope_parent = Hash::repeat_byte(99);

	let relay_parent_x_info =
		RelayChainBlockInfo { number: 0, hash: relay_parent_x, storage_root: Hash::zero() };
	let relay_parent_y_info =
		RelayChainBlockInfo { number: 1, hash: relay_parent_y, storage_root: Hash::zero() };
	let relay_parent_z_info =
		RelayChainBlockInfo { number: 2, hash: relay_parent_z, storage_root: Hash::zero() };

	let ancestors = vec![relay_parent_y_info.clone(), relay_parent_x_info.clone()];

	let base_constraints = make_constraints(0, vec![0], vec![0x0a].into());

	// Test 1: V3 candidate with scheduling_parent == relay_parent (should work like V1/V2)
	{
		let (pvd, candidate) = CandidateBuilder::new(para_id, relay_parent_x)
			.relay_parent_number(relay_parent_x_info.number)
			.scheduling_parent(relay_parent_x) // scheduling_parent == relay_parent
			.parent_head(vec![0x0a].into())
			.para_head(vec![0x0b].into())
			.hrmp_watermark(relay_parent_x_info.number)
			.build();
		let candidate_hash = candidate.hash();
		let candidate_entry =
			CandidateEntry::new(candidate_hash, candidate, pvd, CandidateState::Backed).unwrap();

		let (relay_chain_scope, scope) = make_scope(
			relay_parent_z_info.clone(),
			base_constraints.clone(),
			vec![],
			5,
			ancestors.clone(),
		);

		let chain = FragmentChain::init(&relay_chain_scope, scope, CandidateStorage::default());
		// Should succeed - scheduling_parent is in scope
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_entry)
			.is_ok());
	}

	// Test 2: V3 candidate with scheduling_parent != relay_parent, both in scope
	// This is the key V3 feature: relay_parent can be older than scheduling_parent
	{
		let (pvd, candidate) = CandidateBuilder::new(para_id, relay_parent_x) // older relay_parent (block 0)
			.relay_parent_number(relay_parent_x_info.number)
			.scheduling_parent(relay_parent_y) // newer scheduling_parent (block 1)
			.parent_head(vec![0x0a].into())
			.para_head(vec![0x0b].into())
			.hrmp_watermark(relay_parent_x_info.number)
			.build();
		let candidate_hash = candidate.hash();
		let candidate_entry =
			CandidateEntry::new(candidate_hash, candidate, pvd, CandidateState::Backed).unwrap();

		let (relay_chain_scope, scope) = make_scope(
			relay_parent_z_info.clone(),
			base_constraints.clone(),
			vec![],
			5,
			ancestors.clone(),
		);

		let chain = FragmentChain::init(&relay_chain_scope, scope, CandidateStorage::default());
		// Should succeed - both parents are in scope
		assert!(chain
			.can_add_candidate_as_potential(&relay_chain_scope, &candidate_entry)
			.is_ok());
	}

	// Test 3: V3 candidate with scheduling_parent out of scope (should fail)
	{
		let (pvd, candidate) = CandidateBuilder::new(para_id, relay_parent_x)
			.relay_parent_number(relay_parent_x_info.number)
			.scheduling_parent(out_of_scope_parent) // scheduling_parent not in ancestors
			.parent_head(vec![0x0a].into())
			.para_head(vec![0x0b].into())
			.hrmp_watermark(relay_parent_x_info.number)
			.build();
		let candidate_hash = candidate.hash();
		let candidate_entry =
			CandidateEntry::new(candidate_hash, candidate, pvd, CandidateState::Backed).unwrap();

		let (relay_chain_scope, scope) = make_scope(
			relay_parent_z_info.clone(),
			base_constraints.clone(),
			vec![],
			5,
			ancestors.clone(),
		);

		let chain = FragmentChain::init(&relay_chain_scope, scope, CandidateStorage::default());
		// Should fail - scheduling_parent is not in scope
		assert_matches!(
			chain.can_add_candidate_as_potential(&relay_chain_scope, &candidate_entry),
			Err(Error::SchedulingParentNotInScope(hash)) if hash == out_of_scope_parent
		);
	}

	// Test 4: V3 candidate in fragment chain - verify scheduling_parent is tracked
	{
		let (pvd, candidate) = CandidateBuilder::new(para_id, relay_parent_x) // older relay_parent
			.relay_parent_number(relay_parent_x_info.number)
			.scheduling_parent(relay_parent_y) // newer scheduling_parent
			.parent_head(vec![0x0a].into())
			.para_head(vec![0x0b].into())
			.hrmp_watermark(relay_parent_x_info.number)
			.build();
		let candidate_hash = candidate.hash();
		let candidate_entry =
			CandidateEntry::new(candidate_hash, candidate, pvd, CandidateState::Backed).unwrap();

		// Verify the entry correctly tracks both parents
		assert_eq!(candidate_entry.relay_parent, relay_parent_x);
		assert_eq!(candidate_entry.scheduling_parent, relay_parent_y);

		storage.add_candidate_entry(candidate_entry).unwrap();

		let (relay_chain_scope, scope) = make_scope(
			relay_parent_z_info.clone(),
			base_constraints.clone(),
			vec![],
			5,
			ancestors.clone(),
		);

		let chain = populate_chain_from_previous_storage(&relay_chain_scope, &scope, &storage);
		// The candidate should be in the chain
		assert_eq!(chain.candidate_hashes(), vec![candidate_hash]);
	}
}

#[test]
fn test_relay_parent_not_in_scheduling_scope_but_valid() {
	// Core new behavior: relay parent is NOT in the scheduling scope but has a valid
	// min_relay_parent_number. The candidate should be accepted.
	let para_id = ParaId::from(5u32);

	// Scheduling scope only contains blocks 8, 9, 10.
	let scheduling_parent_info =
		RelayChainBlockInfo { number: 10, hash: Hash::repeat_byte(10), storage_root: Hash::zero() };
	let ancestor_9 =
		RelayChainBlockInfo { number: 9, hash: Hash::repeat_byte(9), storage_root: Hash::zero() };
	let ancestor_8 =
		RelayChainBlockInfo { number: 8, hash: Hash::repeat_byte(8), storage_root: Hash::zero() };

	// Relay parent is block 5 — outside the scheduling scope but above min_relay_parent_number.
	let old_relay_parent = Hash::repeat_byte(5);

	// min_relay_parent_number = 3, so relay parent at block 5 is valid.
	let base_constraints = make_constraints(3, vec![5], vec![0x0a].into());

	let (scheduling_scope, scope) = make_scope(
		scheduling_parent_info.clone(),
		base_constraints,
		vec![],
		5,
		vec![ancestor_9.clone(), ancestor_8.clone()],
	);

	// Candidate with relay_parent=block5 (out of scheduling scope), scheduling_parent=block9 (in
	// scope).
	let (pvd, candidate) = CandidateBuilder::new(para_id, old_relay_parent)
		.relay_parent_number(5)
		.scheduling_parent(ancestor_9.hash)
		.parent_head(vec![0x0a].into())
		.para_head(vec![0x0b].into())
		.hrmp_watermark(5)
		.build();
	let candidate_hash = candidate.hash();
	let candidate_entry =
		CandidateEntry::new(candidate_hash, candidate, pvd, CandidateState::Backed).unwrap();

	// The relay parent is not in scheduling scope, but the scheduling parent is, and the relay
	// parent number is >= min_relay_parent_number. Should be accepted.
	let chain = FragmentChain::init(&scheduling_scope, scope.clone(), CandidateStorage::default());
	assert!(chain
		.can_add_candidate_as_potential(&scheduling_scope, &candidate_entry)
		.is_ok());

	// Also verify it gets into the chain when populated.
	let mut storage = CandidateStorage::default();
	storage.add_candidate_entry(candidate_entry).unwrap();
	let chain = populate_chain_from_previous_storage(&scheduling_scope, &scope, &storage);
	assert_eq!(chain.candidate_hashes(), vec![candidate_hash]);
}

#[test]
fn test_relay_parent_below_min_relay_parent_number_rejected() {
	// Relay parent is out of scheduling scope AND below min_relay_parent_number.
	let para_id = ParaId::from(5u32);

	let scheduling_parent_info =
		RelayChainBlockInfo { number: 10, hash: Hash::repeat_byte(10), storage_root: Hash::zero() };
	let ancestor_9 =
		RelayChainBlockInfo { number: 9, hash: Hash::repeat_byte(9), storage_root: Hash::zero() };

	let old_relay_parent = Hash::repeat_byte(2);

	// min_relay_parent_number = 5, relay parent at block 2 is too old.
	let base_constraints = make_constraints(5, vec![2], vec![0x0a].into());

	let (scheduling_scope, scope) =
		make_scope(scheduling_parent_info, base_constraints, vec![], 5, vec![ancestor_9.clone()]);

	let (pvd, candidate) = CandidateBuilder::new(para_id, old_relay_parent)
		.relay_parent_number(2)
		.scheduling_parent(ancestor_9.hash)
		.parent_head(vec![0x0a].into())
		.para_head(vec![0x0b].into())
		.hrmp_watermark(2)
		.build();
	let candidate_hash = candidate.hash();
	let candidate_entry =
		CandidateEntry::new(candidate_hash, candidate, pvd, CandidateState::Backed).unwrap();

	let chain = FragmentChain::init(&scheduling_scope, scope, CandidateStorage::default());
	assert_matches!(
		chain.can_add_candidate_as_potential(&scheduling_scope, &candidate_entry),
		Err(Error::RelayParentNotInScope(hash)) if hash == old_relay_parent
	);
}

#[test]
fn known_output_heads_unions_chain_and_unconnected() {
	let mut storage = CandidateStorage::default();

	let para_id = ParaId::from(5u32);
	let relay_parent_x = Hash::repeat_byte(1);
	let relay_parent_y = Hash::repeat_byte(2);
	let relay_parent_z = Hash::repeat_byte(3);
	let relay_parent_x_info =
		RelayChainBlockInfo { number: 0, hash: relay_parent_x, storage_root: Hash::zero() };
	let relay_parent_y_info =
		RelayChainBlockInfo { number: 1, hash: relay_parent_y, storage_root: Hash::zero() };
	let relay_parent_z_info =
		RelayChainBlockInfo { number: 2, hash: relay_parent_z, storage_root: Hash::zero() };
	// Ancestors must be in reverse order.
	let ancestors = vec![relay_parent_y_info.clone(), relay_parent_x_info.clone()];

	let base_constraints = make_constraints(0, vec![0], vec![0x0a].into());

	// A -> B -> C, chained by head data: A outputs 0x0b, B outputs 0x0c, C outputs 0x0d.
	let (pvd_a, candidate_a) = CandidateBuilder::new(para_id, relay_parent_x_info.hash)
		.relay_parent_number(relay_parent_x_info.number)
		.parent_head(vec![0x0a].into())
		.para_head(vec![0x0b].into())
		.hrmp_watermark(relay_parent_x_info.number)
		.build();
	let candidate_a_hash = candidate_a.hash();
	storage
		.add_candidate_entry(
			CandidateEntry::new(candidate_a_hash, candidate_a, pvd_a, CandidateState::Backed)
				.unwrap(),
		)
		.unwrap();

	let (pvd_b, candidate_b) = CandidateBuilder::new(para_id, relay_parent_y_info.hash)
		.relay_parent_number(relay_parent_y_info.number)
		.parent_head(vec![0x0b].into())
		.para_head(vec![0x0c].into())
		.hrmp_watermark(relay_parent_y_info.number)
		.build();
	let candidate_b_hash = candidate_b.hash();
	storage
		.add_candidate_entry(
			CandidateEntry::new(candidate_b_hash, candidate_b, pvd_b, CandidateState::Backed)
				.unwrap(),
		)
		.unwrap();

	let (pvd_c, candidate_c) = CandidateBuilder::new(para_id, relay_parent_z_info.hash)
		.relay_parent_number(relay_parent_z_info.number)
		.parent_head(vec![0x0c].into())
		.para_head(vec![0x0d].into())
		.hrmp_watermark(relay_parent_z_info.number)
		.build();
	let candidate_c_hash = candidate_c.hash();
	storage
		.add_candidate_entry(
			CandidateEntry::new(candidate_c_hash, candidate_c, pvd_c, CandidateState::Backed)
				.unwrap(),
		)
		.unwrap();

	// Depth 1: A becomes a connected chain member (chain.by_output_head); B and C are retained
	// as unconnected potential candidates (unconnected.by_output_head).
	let (scheduling_scope, scope) =
		make_scope(relay_parent_z_info.clone(), base_constraints, vec![], 1, ancestors);
	let chain = populate_chain_from_previous_storage(&scheduling_scope, &scope, &storage);

	// Precondition guard: the split really is connected={A}, unconnected={B,C}.
	assert_eq!(chain.candidate_hashes(), vec![candidate_a_hash]);
	assert_eq!(
		chain.unconnected().map(|c| c.candidate_hash).collect::<HashSet<_>>(),
		[candidate_b_hash, candidate_c_hash].into_iter().collect()
	);

	// known_output_heads() unions both indices: A's head from the chain, B+C's from unconnected.
	let head_a = HeadData::from(vec![0x0b]).hash(); // candidate A's output head
	let head_b = HeadData::from(vec![0x0c]).hash(); // candidate B's output head
	let head_c = HeadData::from(vec![0x0d]).hash(); // candidate C's output head
	let base_head = HeadData::from(vec![0x0a]).hash();
	assert_eq!(
		chain.known_output_heads(),
		[base_head, head_a, head_b, head_c].into_iter().collect::<HashSet<_>>()
	);
}

#[test]
fn known_output_heads_is_base_head_when_no_candidates() {
	let relay_parent_info =
		RelayChainBlockInfo { number: 0, hash: Hash::repeat_byte(1), storage_root: Hash::zero() };
	let base_constraints = make_constraints(0, vec![0], vec![0x0a].into());
	let (scheduling_scope, scope) =
		make_scope(relay_parent_info, base_constraints, vec![], 5, vec![]);

	let chain = FragmentChain::init(&scheduling_scope, scope, CandidateStorage::default());

	let base_head = HeadData::from(vec![0x0a]).hash();
	assert_eq!(chain.known_output_heads(), [base_head].into_iter().collect::<HashSet<_>>());
}
