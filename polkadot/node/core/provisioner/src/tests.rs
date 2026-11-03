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
use bitvec::bitvec;
use polkadot_primitives::{MutateDescriptorV2, OccupiedCore, ScheduledCore};
use polkadot_primitives_test_helpers::{dummy_candidate_descriptor_v2, dummy_hash};

const MOCK_GROUP_SIZE: usize = 5;

pub fn occupied_core(para_id: u32) -> CoreState {
	let mut candidate_descriptor = dummy_candidate_descriptor_v2(dummy_hash());
	candidate_descriptor.set_para_id(para_id.into());

	CoreState::Occupied(OccupiedCore {
		group_responsible: para_id.into(),
		next_up_on_available: None,
		occupied_since: 100_u32,
		time_out_at: 200_u32,
		next_up_on_time_out: None,
		availability: bitvec![u8, bitvec::order::Lsb0; 0; 32],
		candidate_descriptor: candidate_descriptor.into(),
		candidate_hash: Default::default(),
	})
}

pub fn build_occupied_core<Builder>(para_id: u32, builder: Builder) -> CoreState
where
	Builder: FnOnce(&mut OccupiedCore),
{
	let mut core = match occupied_core(para_id) {
		CoreState::Occupied(core) => core,
		_ => unreachable!(),
	};

	builder(&mut core);

	CoreState::Occupied(core)
}

pub fn default_bitvec(size: usize) -> CoreAvailability {
	bitvec![u8, bitvec::order::Lsb0; 0; size]
}

pub fn scheduled_core(id: u32) -> ScheduledCore {
	ScheduledCore { para_id: id.into(), collator: None }
}

mod select_availability_bitfields {
	use super::{super::*, default_bitvec, occupied_core};
	use polkadot_primitives::{ScheduledCore, SigningContext, ValidatorId, ValidatorIndex};
	use sp_application_crypto::AppCrypto;
	use sp_keystore::{testing::MemoryKeystore, Keystore, KeystorePtr};
	use std::sync::Arc;

	fn signed_bitfield(
		keystore: &KeystorePtr,
		field: CoreAvailability,
		validator_idx: ValidatorIndex,
	) -> SignedAvailabilityBitfield {
		let public = Keystore::sr25519_generate_new(&**keystore, ValidatorId::ID, None)
			.expect("generated sr25519 key");
		SignedAvailabilityBitfield::sign(
			&keystore,
			field.into(),
			&<SigningContext<Hash>>::default(),
			validator_idx,
			&public.into(),
		)
		.ok()
		.flatten()
		.expect("Should be signed")
	}

	#[test]
	fn not_more_than_one_per_validator() {
		let keystore: KeystorePtr = Arc::new(MemoryKeystore::new());
		let mut bitvec = default_bitvec(2);
		bitvec.set(0, true);
		bitvec.set(1, true);

		let cores = vec![occupied_core(0), occupied_core(1)];

		// we pass in three bitfields with two validators
		// this helps us check the postcondition that we get two bitfields back, for which the
		// validators differ
		let bitfields = vec![
			signed_bitfield(&keystore, bitvec.clone(), ValidatorIndex(0)),
			signed_bitfield(&keystore, bitvec.clone(), ValidatorIndex(1)),
			signed_bitfield(&keystore, bitvec, ValidatorIndex(1)),
		];

		let mut selected_bitfields =
			select_availability_bitfields(&cores, &bitfields, &Hash::repeat_byte(0));
		selected_bitfields.sort_by_key(|bitfield| bitfield.validator_index());

		assert_eq!(selected_bitfields.len(), 2);
		assert_eq!(selected_bitfields[0], bitfields[0]);
		// we don't know which of the (otherwise equal) bitfields will be selected
		assert!(selected_bitfields[1] == bitfields[1] || selected_bitfields[1] == bitfields[2]);
	}

	#[test]
	fn each_corresponds_to_an_occupied_core() {
		let keystore: KeystorePtr = Arc::new(MemoryKeystore::new());
		let bitvec = default_bitvec(3);

		// invalid: bit on free core
		let mut bitvec0 = bitvec.clone();
		bitvec0.set(0, true);

		// invalid: bit on scheduled core
		let mut bitvec1 = bitvec.clone();
		bitvec1.set(1, true);

		// valid: bit on occupied core.
		let mut bitvec2 = bitvec.clone();
		bitvec2.set(2, true);

		let cores = vec![
			CoreState::Free,
			CoreState::Scheduled(ScheduledCore { para_id: Default::default(), collator: None }),
			occupied_core(2),
		];

		let bitfields = vec![
			signed_bitfield(&keystore, bitvec0, ValidatorIndex(0)),
			signed_bitfield(&keystore, bitvec1, ValidatorIndex(1)),
			signed_bitfield(&keystore, bitvec2.clone(), ValidatorIndex(2)),
		];

		let selected_bitfields =
			select_availability_bitfields(&cores, &bitfields, &Hash::repeat_byte(0));

		// selects only the valid bitfield
		assert_eq!(selected_bitfields.len(), 1);
		assert_eq!(selected_bitfields[0].payload().0, bitvec2);
	}

	#[test]
	fn more_set_bits_win_conflicts() {
		let keystore: KeystorePtr = Arc::new(MemoryKeystore::new());
		let mut bitvec = default_bitvec(2);
		bitvec.set(0, true);

		let mut bitvec1 = bitvec.clone();
		bitvec1.set(1, true);

		let cores = vec![occupied_core(0), occupied_core(1)];

		let bitfields = vec![
			signed_bitfield(&keystore, bitvec, ValidatorIndex(1)),
			signed_bitfield(&keystore, bitvec1.clone(), ValidatorIndex(1)),
		];

		let selected_bitfields =
			select_availability_bitfields(&cores, &bitfields, &Hash::repeat_byte(0));
		assert_eq!(selected_bitfields.len(), 1);
		assert_eq!(selected_bitfields[0].payload().0, bitvec1.clone());
	}

	#[test]
	fn more_complex_bitfields() {
		let keystore: KeystorePtr = Arc::new(MemoryKeystore::new());

		let cores = vec![occupied_core(0), occupied_core(1), occupied_core(2), occupied_core(3)];

		let mut bitvec0 = default_bitvec(4);
		bitvec0.set(0, true);
		bitvec0.set(2, true);

		let mut bitvec1 = default_bitvec(4);
		bitvec1.set(1, true);

		let mut bitvec2 = default_bitvec(4);
		bitvec2.set(2, true);

		let mut bitvec3 = default_bitvec(4);
		bitvec3.set(0, true);
		bitvec3.set(1, true);
		bitvec3.set(2, true);
		bitvec3.set(3, true);

		// these are out of order but will be selected in order. The better
		// bitfield for 3 will be selected.
		let bitfields = vec![
			signed_bitfield(&keystore, bitvec2.clone(), ValidatorIndex(3)),
			signed_bitfield(&keystore, bitvec3.clone(), ValidatorIndex(3)),
			signed_bitfield(&keystore, bitvec0.clone(), ValidatorIndex(0)),
			signed_bitfield(&keystore, bitvec2.clone(), ValidatorIndex(2)),
			signed_bitfield(&keystore, bitvec1.clone(), ValidatorIndex(1)),
		];

		let selected_bitfields =
			select_availability_bitfields(&cores, &bitfields, &Hash::repeat_byte(0));
		assert_eq!(selected_bitfields.len(), 4);
		assert_eq!(selected_bitfields[0].payload().0, bitvec0);
		assert_eq!(selected_bitfields[1].payload().0, bitvec1);
		assert_eq!(selected_bitfields[2].payload().0, bitvec2);
		assert_eq!(selected_bitfields[3].payload().0, bitvec3);
	}
}

pub(crate) mod common {
	use super::super::*;
	use futures::channel::mpsc;
	use polkadot_node_subsystem::messages::AllMessages;
	use polkadot_node_subsystem_test_helpers::TestSubsystemSender;

	pub fn test_harness<OverseerFactory, Overseer, TestFactory, Test>(
		overseer_factory: OverseerFactory,
		test_factory: TestFactory,
	) where
		OverseerFactory: FnOnce(mpsc::UnboundedReceiver<AllMessages>) -> Overseer,
		Overseer: Future<Output = ()>,
		TestFactory: FnOnce(TestSubsystemSender) -> Test,
		Test: Future<Output = ()>,
	{
		let (tx, rx) = polkadot_node_subsystem_test_helpers::sender_receiver();
		let overseer = overseer_factory(rx);
		let test = test_factory(tx);

		futures::pin_mut!(overseer, test);

		let _ = futures::executor::block_on(future::join(overseer, test));
	}
}

mod select_candidates {
	use super::{
		super::*, build_occupied_core, common::test_harness, default_bitvec, occupied_core,
		scheduled_core, MOCK_GROUP_SIZE,
	};
	use futures::channel::mpsc;
	use polkadot_node_subsystem::messages::{
		AllMessages, BackableCandidateRef, RuntimeApiMessage,
		RuntimeApiRequest::{
			AvailabilityCores, PersistedValidationData as PersistedValidationDataReq,
		},
	};
	use polkadot_node_subsystem_test_helpers::{mock::new_leaf, TestSubsystemSender};
	use polkadot_primitives::{
		BlockNumber, CandidateCommitments, CandidateHash, CandidateReceiptV2 as CandidateReceipt,
		CommittedCandidateReceiptV2 as CommittedCandidateReceipt, MutateDescriptorV2,
		PersistedValidationData,
	};
	use polkadot_primitives_test_helpers::{dummy_candidate_descriptor_v2, dummy_hash};
	use std::ops::Not;
	use CoreState::{Free, Scheduled};

	const BLOCK_UNDER_PRODUCTION: BlockNumber = 128;

	fn dummy_candidate_template() -> CandidateReceipt {
		let empty_hash = PersistedValidationData::<Hash, BlockNumber>::default().hash();

		let mut descriptor_template = dummy_candidate_descriptor_v2(dummy_hash());
		descriptor_template.set_persisted_validation_data_hash(empty_hash);
		CandidateReceipt {
			descriptor: descriptor_template,
			commitments_hash: CandidateCommitments::default().hash(),
		}
	}

	fn make_candidates(
		core_count: usize,
		expected_backed_indices: Vec<usize>,
	) -> (Vec<CandidateHash>, Vec<BackedCandidate>) {
		let candidate_template = dummy_candidate_template();
		let candidates: Vec<_> = std::iter::repeat(candidate_template)
			.take(core_count)
			.enumerate()
			.map(|(idx, mut candidate)| {
				candidate.descriptor.set_para_id(idx.into());
				candidate
			})
			.collect();

		let expected_backed = expected_backed_indices
			.iter()
			.map(|&idx| candidates[idx].clone())
			.map(|c| {
				BackedCandidate::new(
					CommittedCandidateReceipt {
						descriptor: c.descriptor.clone(),
						commitments: Default::default(),
					},
					Vec::new(),
					default_bitvec(MOCK_GROUP_SIZE),
					CoreIndex(0),
				)
			})
			.collect();
		let candidate_hashes = candidates.into_iter().map(|c| c.hash()).collect();

		(candidate_hashes, expected_backed)
	}

	// For testing only one core assigned to a parachain, we return this set of availability cores:
	//
	//   [
	//      0: Free,
	//      1: Scheduled(default),
	//      2: Occupied(no next_up set),
	//      3: Occupied(next_up_on_available set but not available),
	//      4: Occupied(next_up_on_available set and available),
	//      5: Occupied(next_up_on_time_out set but not timeout),
	//      6: Occupied(next_up_on_time_out set and timeout but available),
	//      7: Occupied(next_up_on_time_out set and timeout and not available),
	//      8: Occupied(both next_up set, available),
	//      9: Occupied(both next_up set, not available, no timeout),
	//     10: Occupied(both next_up set, not available, timeout),
	//     11: Occupied(next_up_on_available and available, but different successor para_id)
	//   ]
	fn mock_availability_cores_one_per_para() -> Vec<CoreState> {
		vec![
			// 0: Free,
			Free,
			// 1: Scheduled(default),
			Scheduled(scheduled_core(1)),
			// 2: Occupied(no next_up set),
			occupied_core(2),
			// 3: Occupied(next_up_on_available set but not available),
			build_occupied_core(3, |core| {
				core.next_up_on_available = Some(scheduled_core(3));
			}),
			// 4: Occupied(next_up_on_available set and available),
			build_occupied_core(4, |core| {
				core.next_up_on_available = Some(scheduled_core(4));
				core.availability = core.availability.clone().not();
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(41));
			}),
			// 5: Occupied(next_up_on_time_out set but not timeout),
			build_occupied_core(5, |core| {
				core.next_up_on_time_out = Some(scheduled_core(5));
			}),
			// 6: Occupied(next_up_on_time_out set and timeout but available),
			build_occupied_core(6, |core| {
				core.next_up_on_time_out = Some(scheduled_core(6));
				core.time_out_at = BLOCK_UNDER_PRODUCTION;
				core.availability = core.availability.clone().not();
			}),
			// 7: Occupied(next_up_on_time_out set and timeout and not available),
			build_occupied_core(7, |core| {
				core.next_up_on_time_out = Some(scheduled_core(7));
				core.time_out_at = BLOCK_UNDER_PRODUCTION;
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(71));
			}),
			// 8: Occupied(both next_up set, available),
			build_occupied_core(8, |core| {
				core.next_up_on_available = Some(scheduled_core(8));
				core.next_up_on_time_out = Some(scheduled_core(8));
				core.availability = core.availability.clone().not();
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(81));
			}),
			// 9: Occupied(both next_up set, not available, no timeout),
			build_occupied_core(9, |core| {
				core.next_up_on_available = Some(scheduled_core(9));
				core.next_up_on_time_out = Some(scheduled_core(9));
			}),
			// 10: Occupied(both next_up set, not available, timeout),
			build_occupied_core(10, |core| {
				core.next_up_on_available = Some(scheduled_core(10));
				core.next_up_on_time_out = Some(scheduled_core(10));
				core.time_out_at = BLOCK_UNDER_PRODUCTION;
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(101));
			}),
			// 11: Occupied(next_up_on_available and available, but different successor para_id)
			build_occupied_core(11, |core| {
				core.next_up_on_available = Some(scheduled_core(12));
				core.availability = core.availability.clone().not();
			}),
		]
	}

	// For test purposes with multiple possible cores assigned to a para, we always return this set
	// of availability cores:
	fn mock_availability_cores_multiple_per_para() -> Vec<CoreState> {
		vec![
			// 0: Free,
			Free,
			// 1: Scheduled(default),
			Scheduled(scheduled_core(1)),
			// 2: Occupied(no next_up set),
			occupied_core(2),
			// 3: Occupied(next_up_on_available set but not available),
			build_occupied_core(3, |core| {
				core.next_up_on_available = Some(scheduled_core(3));
			}),
			// 4: Occupied(next_up_on_available set and available),
			build_occupied_core(4, |core| {
				core.next_up_on_available = Some(scheduled_core(4));
				core.availability = core.availability.clone().not();
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(41));
			}),
			// 5: Occupied(next_up_on_time_out set but not timeout),
			build_occupied_core(5, |core| {
				core.next_up_on_time_out = Some(scheduled_core(5));
			}),
			// 6: Occupied(next_up_on_time_out set and timeout but available),
			build_occupied_core(6, |core| {
				core.next_up_on_time_out = Some(scheduled_core(6));
				core.time_out_at = BLOCK_UNDER_PRODUCTION;
				core.availability = core.availability.clone().not();
			}),
			// 7: Occupied(next_up_on_time_out set and timeout and not available),
			build_occupied_core(7, |core| {
				core.next_up_on_time_out = Some(scheduled_core(7));
				core.time_out_at = BLOCK_UNDER_PRODUCTION;
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(71));
			}),
			// 8: Occupied(both next_up set, available),
			build_occupied_core(8, |core| {
				core.next_up_on_available = Some(scheduled_core(8));
				core.next_up_on_time_out = Some(scheduled_core(8));
				core.availability = core.availability.clone().not();
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(81));
			}),
			// 9: Occupied(both next_up set, not available, no timeout),
			build_occupied_core(9, |core| {
				core.next_up_on_available = Some(scheduled_core(9));
				core.next_up_on_time_out = Some(scheduled_core(9));
			}),
			// 10: Occupied(both next_up set, not available, timeout),
			build_occupied_core(10, |core| {
				core.next_up_on_available = Some(scheduled_core(10));
				core.next_up_on_time_out = Some(scheduled_core(10));
				core.time_out_at = BLOCK_UNDER_PRODUCTION;
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(101));
			}),
			// 11: Occupied(next_up_on_available and available, but different successor para_id)
			build_occupied_core(11, |core| {
				core.next_up_on_available = Some(scheduled_core(12));
				core.availability = core.availability.clone().not();
			}),
			// 12-14: Occupied(next_up_on_available and available, same para_id).
			build_occupied_core(12, |core| {
				core.next_up_on_available = Some(scheduled_core(12));
				core.availability = core.availability.clone().not();
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(121));
			}),
			build_occupied_core(12, |core| {
				core.next_up_on_available = Some(scheduled_core(12));
				core.availability = core.availability.clone().not();
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(122));
			}),
			build_occupied_core(12, |core| {
				core.next_up_on_available = Some(scheduled_core(12));
				core.availability = core.availability.clone().not();
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(123));
			}),
			// 15: Scheduled on same para_id as 12-14.
			Scheduled(scheduled_core(12)),
			// 16: Occupied(13, no next_up set, not available)
			build_occupied_core(13, |core| {
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(131));
			}),
			// 17: Occupied(13, no next_up set, available)
			build_occupied_core(13, |core| {
				core.availability = core.availability.clone().not();
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(132));
			}),
			// 18: Occupied(13, next_up_on_available set to 13 but not available)
			build_occupied_core(13, |core| {
				core.next_up_on_available = Some(scheduled_core(13));
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(133));
			}),
			// 19: Occupied(13, next_up_on_available set to 13 and available)
			build_occupied_core(13, |core| {
				core.next_up_on_available = Some(scheduled_core(13));
				core.availability = core.availability.clone().not();
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(134));
			}),
			// 20: Occupied(13, next_up_on_time_out set to 13 but not timeout)
			build_occupied_core(13, |core| {
				core.next_up_on_time_out = Some(scheduled_core(13));
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(135));
			}),
			// 21: Occupied(13, next_up_on_available set to 14 and available)
			build_occupied_core(13, |core| {
				core.next_up_on_available = Some(scheduled_core(14));
				core.availability = core.availability.clone().not();
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(136));
			}),
			// 22: Occupied(13, next_up_on_available set to 14 but not available)
			build_occupied_core(13, |core| {
				core.next_up_on_available = Some(scheduled_core(14));
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(137));
			}),
			// 23: Occupied(13, both next_up set to 14, available)
			build_occupied_core(13, |core| {
				core.next_up_on_available = Some(scheduled_core(14));
				core.next_up_on_time_out = Some(scheduled_core(14));
				core.availability = core.availability.clone().not();
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(138));
			}),
			// 24: Occupied(13, both next_up set to 14, not available, timeout)
			build_occupied_core(13, |core| {
				core.next_up_on_available = Some(scheduled_core(14));
				core.next_up_on_time_out = Some(scheduled_core(14));
				core.time_out_at = BLOCK_UNDER_PRODUCTION;
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(1399));
			}),
			// 25: Occupied(13, next_up_on_available and available, but successor para_id 15)
			build_occupied_core(13, |core| {
				core.next_up_on_available = Some(scheduled_core(15));
				core.availability = core.availability.clone().not();
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(139));
			}),
			// 26: Occupied(15, next_up_on_available and available, but successor para_id 13)
			build_occupied_core(15, |core| {
				core.next_up_on_available = Some(scheduled_core(13));
				core.availability = core.availability.clone().not();
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(151));
			}),
			// 27: Occupied(15, both next_up, both available and timed out)
			build_occupied_core(15, |core| {
				core.next_up_on_available = Some(scheduled_core(15));
				core.availability = core.availability.clone().not();
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(152));
				core.time_out_at = BLOCK_UNDER_PRODUCTION;
			}),
			// 28: Occupied(13, both next_up set to 13, not available)
			build_occupied_core(13, |core| {
				core.next_up_on_available = Some(scheduled_core(13));
				core.next_up_on_time_out = Some(scheduled_core(13));
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(1398));
			}),
			// 29: Occupied(13, both next_up set to 13, not available, timeout)
			build_occupied_core(13, |core| {
				core.next_up_on_available = Some(scheduled_core(13));
				core.next_up_on_time_out = Some(scheduled_core(13));
				core.time_out_at = BLOCK_UNDER_PRODUCTION;
				core.candidate_hash = CandidateHash(Hash::from_low_u64_be(1397));
			}),
		]
	}

	async fn mock_overseer(
		mut receiver: mpsc::UnboundedReceiver<AllMessages>,
		mock_availability_cores: Vec<CoreState>,
		mut expected: Vec<BackedCandidate>,
		mut expected_ancestors: HashMap<Vec<CandidateHash>, Ancestors>,
	) {
		use RuntimeApiMessage::Request;

		let mut backed = expected.clone().into_iter().fold(HashMap::new(), |mut acc, candidate| {
			acc.entry(candidate.descriptor().para_id()).or_insert(vec![]).push(candidate);
			acc
		});

		expected.sort_by_key(|c| c.candidate().descriptor.para_id());
		let mut candidates_iter = expected.iter().map(|candidate| BackableCandidateRef {
			candidate_hash: candidate.hash(),
			scheduling_parent: candidate.descriptor().scheduling_parent(),
		});

		while let Some(from_job) = receiver.next().await {
			match from_job {
				AllMessages::RuntimeApi(Request(
					_parent_hash,
					PersistedValidationDataReq(_para_id, _assumption, tx),
				)) => tx.send(Ok(Some(Default::default()))).unwrap(),
				AllMessages::RuntimeApi(Request(_parent_hash, AvailabilityCores(tx))) => {
					tx.send(Ok(mock_availability_cores.clone())).unwrap()
				},
				AllMessages::CandidateBacking(CandidateBackingMessage::GetBackableCandidates {
					candidates: hashes,
					sender,
				}) => {
					let mut response: HashMap<ParaId, Vec<BackedCandidate>> = HashMap::new();
					for (para_id, requested_candidates) in hashes.clone() {
						response.insert(
							para_id,
							backed
								.get_mut(&para_id)
								.unwrap()
								.drain(0..requested_candidates.len())
								.collect(),
						);
					}
					let expected_hashes: HashMap<ParaId, Vec<BackableCandidateRef>> = response
						.iter()
						.map(|(para_id, candidates)| {
							(
								*para_id,
								candidates
									.iter()
									.map(|candidate| BackableCandidateRef {
										candidate_hash: candidate.hash(),
										scheduling_parent: candidate
											.descriptor()
											.scheduling_parent(),
									})
									.collect(),
							)
						})
						.collect();

					assert_eq!(expected_hashes, hashes);

					let _ = sender.send(response);
				},
				AllMessages::ProspectiveParachains(
					ProspectiveParachainsMessage::GetBackableCandidates {
						leaf: _,
						para_id: _para_id,
						count,
						ancestors: actual_ancestors,
						sender: tx,
					},
				) => {
					assert!(count > 0);
					let candidates =
						(&mut candidates_iter).take(count as usize).collect::<Vec<_>>();
					assert_eq!(candidates.len(), count as usize);

					if !expected_ancestors.is_empty() {
						if let Some(expected_required_ancestors) = expected_ancestors.remove(
							&(candidates
								.clone()
								.into_iter()
								.take(actual_ancestors.len())
								.map(|c| c.candidate_hash)
								.collect::<Vec<_>>()),
						) {
							assert_eq!(expected_required_ancestors, actual_ancestors);
						} else {
							assert_eq!(actual_ancestors.len(), 0);
						}
					}

					let _ = tx.send(candidates);
				},
				_ => panic!("Unexpected message: {:?}", from_job),
			}
		}

		assert_eq!(candidates_iter.next(), None);
		assert_eq!(expected_ancestors.len(), 0);
	}

	#[test]
	fn can_succeed() {
		test_harness(
			|r| mock_overseer(r, Vec::new(), Vec::new(), HashMap::new()),
			|mut tx: TestSubsystemSender| async move {
				select_candidates(
					&[],
					&[],
					&new_leaf(Default::default(), BLOCK_UNDER_PRODUCTION - 1),
					&mut tx,
				)
				.await
				.unwrap();
			},
		)
	}

	#[test]
	fn selects_max_one_code_upgrade_one_core_per_para() {
		let mock_cores = mock_availability_cores_one_per_para();

		let empty_hash = PersistedValidationData::<Hash, BlockNumber>::default().hash();

		// why those particular indices? see the comments on mock_availability_cores()
		// the first candidate with code is included out of [1, 4, 7, 8, 10, 12].
		let cores = [1, 4, 7, 8, 10, 12];
		let cores_with_code = [1, 4, 8];

		// We can't be sure which one code upgrade the provisioner will pick. We can only assert
		// that it only picks one. These are the possible cores for which the provisioner will
		// supply candidates.
		// There are multiple possibilities depending on which code upgrade it
		// chooses.
		let possible_expected_cores = [[1, 7, 10, 12], [4, 7, 10, 12], [7, 8, 10, 12]];

		let committed_receipts: Vec<_> = (0..=mock_cores.len())
			.map(|i| {
				let mut descriptor = dummy_candidate_descriptor_v2(dummy_hash());
				descriptor.set_para_id(i.into());
				descriptor.set_persisted_validation_data_hash(empty_hash);
				CommittedCandidateReceipt {
					descriptor,
					commitments: CandidateCommitments {
						new_validation_code: if cores_with_code.contains(&i) {
							Some(vec![].into())
						} else {
							None
						},
						..Default::default()
					},
				}
			})
			.collect();

		// Input to select_candidates
		let candidates: Vec<_> = committed_receipts.iter().map(|r| r.to_plain()).collect();
		// Build possible outputs from select_candidates
		let backed_candidates: Vec<_> = committed_receipts
			.iter()
			.map(|committed_receipt| {
				BackedCandidate::new(
					committed_receipt.clone(),
					Vec::new(),
					default_bitvec(MOCK_GROUP_SIZE),
					CoreIndex(0),
				)
			})
			.collect();

		// First, provisioner will request backable candidates for each scheduled core.
		// Then, some of them get filtered due to new validation code rule.
		let expected_backed: Vec<_> =
			cores.iter().map(|&idx| backed_candidates[idx].clone()).collect();
		let expected_backed_filtered: Vec<Vec<_>> = possible_expected_cores
			.iter()
			.map(|indices| indices.iter().map(|&idx| candidates[idx].clone()).collect())
			.collect();

		let mock_cores_clone = mock_cores.clone();

		test_harness(
			|r| mock_overseer(r, mock_cores_clone, expected_backed, HashMap::new()),
			|mut tx: TestSubsystemSender| async move {
				let result = select_candidates(
					&mock_cores,
					&[],
					&new_leaf(Default::default(), BLOCK_UNDER_PRODUCTION - 1),
					&mut tx,
				)
				.await
				.unwrap();

				assert_eq!(result.len(), 4);

				assert!(expected_backed_filtered.iter().any(|expected_backed_filtered| {
					result.clone().into_iter().all(|c| {
						expected_backed_filtered.iter().any(|c2| c.candidate().corresponds_to(c2))
					})
				}));
			},
		)
	}

	#[test]
	fn selects_max_one_code_upgrade_multiple_cores_per_para() {
		let mock_cores = vec![
			// 0: Scheduled(default),
			Scheduled(scheduled_core(1)),
			// 1: Scheduled(default),
			Scheduled(scheduled_core(2)),
			// 2: Scheduled(default),
			Scheduled(scheduled_core(2)),
			// 3: Scheduled(default),
			Scheduled(scheduled_core(2)),
			// 4: Scheduled(default),
			Scheduled(scheduled_core(3)),
			// 5: Scheduled(default),
			Scheduled(scheduled_core(3)),
			// 6: Scheduled(default),
			Scheduled(scheduled_core(3)),
		];

		let empty_hash = PersistedValidationData::<Hash, BlockNumber>::default().hash();
		let cores_with_code = [0, 2, 4, 5];

		// We can't be sure which one code upgrade the provisioner will pick. We can only assert
		// that it only picks one.
		// These are the possible cores for which the provisioner will
		// supply candidates. There are multiple possibilities depending on which code upgrade it
		// chooses.
		let possible_expected_cores = [vec![0, 1], vec![1, 2, 3], vec![4, 1]];

		let committed_receipts: Vec<_> = (0..mock_cores.len())
			.map(|i| {
				let mut descriptor = dummy_candidate_descriptor_v2(dummy_hash());
				descriptor.set_para_id(if let Scheduled(scheduled_core) = &mock_cores[i] {
					scheduled_core.para_id
				} else {
					panic!("`mock_cores` is not initialized with `Scheduled`?")
				});
				descriptor.set_persisted_validation_data_hash(empty_hash);
				descriptor.set_pov_hash(Hash::from_low_u64_be(i as u64));
				CommittedCandidateReceipt {
					descriptor,
					commitments: CandidateCommitments {
						new_validation_code: if cores_with_code.contains(&i) {
							Some(vec![].into())
						} else {
							None
						},
						..Default::default()
					},
				}
			})
			.collect();

		// Input to select_candidates
		let candidates: Vec<_> = committed_receipts.iter().map(|r| r.to_plain()).collect();
		// Build possible outputs from select_candidates
		let backed_candidates: Vec<_> = committed_receipts
			.iter()
			.map(|committed_receipt| {
				BackedCandidate::new(
					committed_receipt.clone(),
					Vec::new(),
					default_bitvec(MOCK_GROUP_SIZE),
					CoreIndex(0),
				)
			})
			.collect();

		// First, provisioner will request backable candidates for each scheduled core.
		// Then, some of them get filtered due to new validation code rule.
		let expected_backed: Vec<_> =
			(0..mock_cores.len()).map(|idx| backed_candidates[idx].clone()).collect();
		let expected_backed_filtered: Vec<Vec<_>> = possible_expected_cores
			.iter()
			.map(|indices| indices.iter().map(|&idx| candidates[idx].clone()).collect())
			.collect();

		let mock_cores_clone = mock_cores.clone();

		test_harness(
			|r| mock_overseer(r, mock_cores_clone, expected_backed, HashMap::new()),
			|mut tx: TestSubsystemSender| async move {
				let result = select_candidates(
					&mock_cores,
					&[],
					&new_leaf(Default::default(), BLOCK_UNDER_PRODUCTION - 1),
					&mut tx,
				)
				.await
				.unwrap();

				assert!(expected_backed_filtered.iter().any(|expected_backed_filtered| {
					result.clone().into_iter().all(|c| {
						expected_backed_filtered.iter().any(|c2| c.candidate().corresponds_to(c2))
					}) && (expected_backed_filtered.len() == result.len())
				}));
			},
		)
	}
	#[test]
	fn one_core_per_para() {
		let mock_cores = mock_availability_cores_one_per_para();

		// why those particular indices? see the comments on mock_availability_cores()
		let expected_candidates: Vec<_> = vec![1, 4, 7, 8, 10, 12];
		let (candidates, expected_candidates) =
			make_candidates(mock_cores.len() + 1, expected_candidates);

		let mut required_ancestors: HashMap<Vec<CandidateHash>, Ancestors> = HashMap::new();
		required_ancestors.insert(
			vec![candidates[4]],
			vec![CandidateHash(Hash::from_low_u64_be(41))].into_iter().collect(),
		);
		required_ancestors.insert(
			vec![candidates[8]],
			vec![CandidateHash(Hash::from_low_u64_be(81))].into_iter().collect(),
		);

		let mock_cores_clone = mock_cores.clone();
		let expected_candidates_clone = expected_candidates.clone();
		test_harness(
			|r| mock_overseer(r, mock_cores_clone, expected_candidates_clone, required_ancestors),
			|mut tx: TestSubsystemSender| async move {
				let result = select_candidates(
					&mock_cores,
					&[],
					&new_leaf(Default::default(), BLOCK_UNDER_PRODUCTION - 1),
					&mut tx,
				)
				.await
				.unwrap();

				assert_eq!(result.len(), expected_candidates.len());
				result.into_iter().for_each(|c| {
					assert!(
						expected_candidates
							.iter()
							.any(|c2| c.candidate().corresponds_to(&c2.receipt())),
						"Failed to find candidate: {:?}",
						c,
					)
				});
			},
		)
	}

	#[test]
	fn multiple_cores_per_para_elastic_scaling() {
		let mock_cores = mock_availability_cores_multiple_per_para();

		// why those particular indices? see the comments on mock_availability_cores()
		let expected_candidates: Vec<_> =
			vec![1, 4, 7, 8, 10, 12, 12, 12, 12, 12, 13, 13, 13, 14, 14, 14, 15, 15];

		let (candidates, expected_candidates) =
			make_candidates(mock_cores.len(), expected_candidates);

		let mut required_ancestors: HashMap<Vec<CandidateHash>, Ancestors> = HashMap::new();
		required_ancestors.insert(
			vec![candidates[4]],
			vec![CandidateHash(Hash::from_low_u64_be(41))].into_iter().collect(),
		);
		required_ancestors.insert(
			vec![candidates[8]],
			vec![CandidateHash(Hash::from_low_u64_be(81))].into_iter().collect(),
		);
		required_ancestors.insert(
			[12, 12, 12].iter().map(|&idx| candidates[idx]).collect::<Vec<_>>(),
			vec![
				CandidateHash(Hash::from_low_u64_be(121)),
				CandidateHash(Hash::from_low_u64_be(122)),
				CandidateHash(Hash::from_low_u64_be(123)),
			]
			.into_iter()
			.collect(),
		);
		required_ancestors.insert(
			[13, 13, 13].iter().map(|&idx| candidates[idx]).collect::<Vec<_>>(),
			(131..=139)
				.map(|num| CandidateHash(Hash::from_low_u64_be(num)))
				.chain(std::iter::once(CandidateHash(Hash::from_low_u64_be(1398))))
				.collect(),
		);

		required_ancestors.insert(
			[15, 15].iter().map(|&idx| candidates[idx]).collect::<Vec<_>>(),
			vec![
				CandidateHash(Hash::from_low_u64_be(151)),
				CandidateHash(Hash::from_low_u64_be(152)),
			]
			.into_iter()
			.collect(),
		);

		let mock_cores_clone = mock_cores.clone();
		let expected_candidates_clone = expected_candidates.clone();
		test_harness(
			|r| mock_overseer(r, mock_cores_clone, expected_candidates, required_ancestors),
			|mut tx: TestSubsystemSender| async move {
				let result = select_candidates(
					&mock_cores,
					&[],
					&new_leaf(Default::default(), BLOCK_UNDER_PRODUCTION - 1),
					&mut tx,
				)
				.await
				.unwrap();

				assert_eq!(result.len(), expected_candidates_clone.len());
				result.into_iter().for_each(|c| {
					assert!(
						expected_candidates_clone
							.iter()
							.any(|c2| c.candidate().corresponds_to(&c2.receipt())),
						"Failed to find candidate: {:?}",
						c,
					)
				});
			},
		)
	}

	#[test]
	fn request_receipts_based_on_scheduling_parent() {
		let mock_cores = mock_availability_cores_one_per_para();
		let candidate_template = dummy_candidate_template();

		let candidates: Vec<_> = std::iter::repeat(candidate_template)
			.take(mock_cores.len() + 1)
			.enumerate()
			.map(|(idx, mut candidate)| {
				candidate.descriptor.set_para_id(idx.into());
				candidate.descriptor.set_relay_parent(Hash::repeat_byte(idx as u8));
				candidate
			})
			.collect();

		// why those particular indices? see the comments on mock_availability_cores()
		let expected_candidates: Vec<_> =
			[1, 4, 7, 8, 10, 12].iter().map(|&idx| candidates[idx].clone()).collect();

		let expected_backed = expected_candidates
			.iter()
			.map(|c| {
				BackedCandidate::new(
					CommittedCandidateReceipt {
						descriptor: c.descriptor().clone(),
						commitments: Default::default(),
					},
					Vec::new(),
					default_bitvec(MOCK_GROUP_SIZE),
					CoreIndex(0),
				)
			})
			.collect();

		let mock_cores_clone = mock_cores.clone();
		test_harness(
			|r| mock_overseer(r, mock_cores_clone, expected_backed, HashMap::new()),
			|mut tx: TestSubsystemSender| async move {
				let result = select_candidates(
					&mock_cores,
					&[],
					&new_leaf(Default::default(), BLOCK_UNDER_PRODUCTION - 1),
					&mut tx,
				)
				.await
				.unwrap();

				result.into_iter().for_each(|c| {
					assert!(
						expected_candidates.iter().any(|c2| c.candidate().corresponds_to(c2)),
						"Failed to find candidate: {:?}",
						c,
					)
				});
			},
		)
	}
}
