// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Tests for Sassafras pallet.

use crate::*;
use mock::*;

use sp_consensus_sassafras::Slot;

fn h2b<const N: usize>(hex: &str) -> [u8; N] {
	array_bytes::hex2array_unchecked(hex)
}

fn b2h<const N: usize>(bytes: [u8; N]) -> String {
	array_bytes::bytes2hex("", &bytes)
}

#[test]
fn genesis_values_assumptions_check() {
	new_test_ext(3).execute_with(|| {
		assert_eq!(Authorities::<Test>::get().len(), 3);
		assert_eq!(EpochConfig::<Test>::get(), TEST_EPOCH_CONFIGURATION);
	});
}

#[test]
fn post_genesis_randomness_initialization() {
	let (pairs, mut ext) = new_test_ext_with_pairs(1, false);
	let pair = &pairs[0];

	ext.execute_with(|| {
		assert_eq!(CurrentRandomness::<Test>::get(), [0; 32]);
		assert_eq!(NextRandomness::<Test>::get(), [0; 32]);
		assert_eq!(RandomnessAccumulator::<Test>::get(), [0; 32]);

		// Test the values with a zero genesis block hash
		let _ = initialize_block(1, 123.into(), [0x00; 32].into(), pair);

		assert_eq!(CurrentRandomness::<Test>::get(), [0; 32]);
		println!("[DEBUG] {}", b2h(NextRandomness::<Test>::get()));
		assert_eq!(
			NextRandomness::<Test>::get(),
			h2b("b9497550deeeb4adc134555930de61968a0558f8947041eb515b2f5fa68ffaf7")
		);
		println!("[DEBUG] {}", b2h(RandomnessAccumulator::<Test>::get()));
		assert_eq!(
			RandomnessAccumulator::<Test>::get(),
			h2b("febcc7fe9539fe17ed29f525831394edfb30b301755dc9bd91584a1f065faf87")
		);
		let (id1, _) = make_ticket_bodies(1, Some(pair))[0];

		// Reset what is relevant
		System::set_block_number(0);
		NextRandomness::<Test>::set([0; 32]);
		RandomnessAccumulator::<Test>::set([0; 32]);

		// Test the values with a non-zero genesis block hash
		let _ = initialize_block(1, 123.into(), [0xff; 32].into(), pair);

		assert_eq!(CurrentRandomness::<Test>::get(), [0; 32]);
		println!("[DEBUG] {}", b2h(NextRandomness::<Test>::get()));
		assert_eq!(
			NextRandomness::<Test>::get(),
			h2b("51c1e3b3a73d2043b3cabae98ff27bdd4aad8967c21ecda7b9465afaa0e70f37")
		);
		println!("[DEBUG] {}", b2h(RandomnessAccumulator::<Test>::get()));
		assert_eq!(
			RandomnessAccumulator::<Test>::get(),
			h2b("466bf3007f2e17bffee0b3c42c90f33d654f5ff61eff28b0cc650825960abd52")
		);
		let (id2, _) = make_ticket_bodies(1, Some(pair))[0];

		// Ticket ids should be different when next epoch randomness is different
		assert_ne!(id1, id2);

		// Reset what is relevant
		System::set_block_number(0);
		NextRandomness::<Test>::set([0; 32]);
		RandomnessAccumulator::<Test>::set([0; 32]);

		// Test the values with a non-zero genesis block hash
		let _ = initialize_block(1, 321.into(), [0x00; 32].into(), pair);

		println!("[DEBUG] {}", b2h(NextRandomness::<Test>::get()));
		assert_eq!(
			NextRandomness::<Test>::get(),
			h2b("d85d84a54f79453000eb62e8a17b30149bd728d3232bc2787a89d51dc9a36008")
		);
		println!("[DEBUG] {}", b2h(RandomnessAccumulator::<Test>::get()));
		assert_eq!(
			RandomnessAccumulator::<Test>::get(),
			h2b("8a035eed02b5b8642b1515ed19752df8df156627aea45c4ef6e3efa88be9a74d")
		);
		let (id2, _) = make_ticket_bodies(1, Some(pair))[0];

		// Ticket ids should be different when next epoch randomness is different
		assert_ne!(id1, id2);
	});
}

// Tests if the sorted tickets are assigned to each slot outside-in.
#[test]
fn slot_ticket_id_outside_in_fetch() {
	let genesis_slot = Slot::from(100);
	let tickets_count = 6;

	// Current epoch tickets
	let curr_tickets: Vec<TicketId> = (0..tickets_count).map(|i| i as TicketId).collect();

	// Next epoch tickets
	let next_tickets: Vec<TicketId> =
		(0..tickets_count - 1).map(|i| (i + tickets_count) as TicketId).collect();

	new_test_ext(0).execute_with(|| {
		// Some corner cases
		TicketsIds::<Test>::insert((0, 0_u32), 1_u128);

		// Cleanup
		(0..3).for_each(|i| TicketsIds::<Test>::remove((0, i as u32)));

		curr_tickets
			.iter()
			.enumerate()
			.for_each(|(i, id)| TicketsIds::<Test>::insert((0, i as u32), id));

		next_tickets
			.iter()
			.enumerate()
			.for_each(|(i, id)| TicketsIds::<Test>::insert((1, i as u32), id));

		TicketsMeta::<Test>::set(TicketsMetadata {
			tickets_count: [curr_tickets.len() as u32, next_tickets.len() as u32],
			unsorted_tickets_count: 0,
		});

		// Before importing the first block the pallet always return `None`
		// This is a kind of special hardcoded case that should never happen in practice
		// as the first thing the pallet does is to initialize the genesis slot.

		assert_eq!(Sassafras::slot_ticket_id(0.into()), None);
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 0), None);
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 1), None);
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 100), None);

		// Initialize genesis slot..
		GenesisSlot::<Test>::set(genesis_slot);
		frame_system::Pallet::<Test>::set_block_number(One::one());

		// Try to fetch a ticket for a slot before current epoch.
		assert_eq!(Sassafras::slot_ticket_id(0.into()), None);

		// Current epoch tickets.
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 0), Some(curr_tickets[1]));
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 1), Some(curr_tickets[3]));
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 2), Some(curr_tickets[5]));
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 3), None);
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 4), None);
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 5), None);
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 6), None);
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 7), Some(curr_tickets[4]));
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 8), Some(curr_tickets[2]));
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 9), Some(curr_tickets[0]));

		// Next epoch tickets (note that only 5 tickets are available)
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 10), Some(next_tickets[1]));
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 11), Some(next_tickets[3]));
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 12), None);
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 13), None);
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 14), None);
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 15), None);
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 16), None);
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 17), Some(next_tickets[4]));
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 18), Some(next_tickets[2]));
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 19), Some(next_tickets[0]));

		// Try to fetch the tickets for slots beyond the next epoch.
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 20), None);
		assert_eq!(Sassafras::slot_ticket_id(genesis_slot + 42), None);
	});
}

// Different test for outside-in test with more focus on corner case correctness.
#[test]
fn slot_ticket_id_outside_in_fetch_corner_cases() {
	new_test_ext(0).execute_with(|| {
		frame_system::Pallet::<Test>::set_block_number(One::one());

		let mut meta = TicketsMetadata { tickets_count: [0, 0], unsorted_tickets_count: 0 };
		let curr_epoch_idx = EpochIndex::<Test>::get();

		let mut epoch_test = |epoch_idx| {
			let tag = (epoch_idx & 1) as u8;
			let epoch_start = Sassafras::epoch_start(epoch_idx);

			// cleanup
			meta.tickets_count = [0, 0];
			TicketsMeta::<Test>::set(meta);
			assert!((0..10).all(|i| Sassafras::slot_ticket_id((epoch_start + i).into()).is_none()));

			meta.tickets_count[tag as usize] += 1;
			TicketsMeta::<Test>::set(meta);
			TicketsIds::<Test>::insert((tag, 0_u32), 1_u128);
			assert_eq!(Sassafras::slot_ticket_id((epoch_start + 9).into()), Some(1_u128));
			assert!((0..9).all(|i| Sassafras::slot_ticket_id((epoch_start + i).into()).is_none()));

			meta.tickets_count[tag as usize] += 1;
			TicketsMeta::<Test>::set(meta);
			TicketsIds::<Test>::insert((tag, 1_u32), 2_u128);
			assert_eq!(Sassafras::slot_ticket_id((epoch_start + 0).into()), Some(2_u128));
			assert!((1..9).all(|i| Sassafras::slot_ticket_id((epoch_start + i).into()).is_none()));

			meta.tickets_count[tag as usize] += 2;
			TicketsMeta::<Test>::set(meta);
			TicketsIds::<Test>::insert((tag, 2_u32), 3_u128);
			assert_eq!(Sassafras::slot_ticket_id((epoch_start + 8).into()), Some(3_u128));
			assert!((1..8).all(|i| Sassafras::slot_ticket_id((epoch_start + i).into()).is_none()));
		};

		// Even epoch
		epoch_test(curr_epoch_idx);
		epoch_test(curr_epoch_idx + 1);
	});
}

#[test]
fn on_first_block_after_genesis() {
	let (pairs, mut ext) = new_test_ext_with_pairs(4, false);

	ext.execute_with(|| {
		let start_slot = Slot::from(100);
		let start_block = 1;

		let digest = initialize_block(start_block, start_slot, Default::default(), &pairs[0]);

		let common_assertions = || {
			assert_eq!(GenesisSlot::<Test>::get(), start_slot);
			assert_eq!(CurrentSlot::<Test>::get(), start_slot);
			assert_eq!(EpochIndex::<Test>::get(), 0);
			assert_eq!(Sassafras::current_epoch_start(), start_slot);
			assert_eq!(Sassafras::current_slot_index(), 0);
			assert_eq!(CurrentRandomness::<Test>::get(), [0; 32]);
			println!("[DEBUG] {}", b2h(NextRandomness::<Test>::get()));
			assert_eq!(
				NextRandomness::<Test>::get(),
				h2b("a49592ef190b96f3eb87bde4c8355e33df28c75006156e8c81998158de2ed49e")
			);
		};

		// Post-initialization status

		assert!(SlotRandomness::<Test>::exists());
		common_assertions();
		println!("[DEBUG] {}", b2h(RandomnessAccumulator::<Test>::get()));
		assert_eq!(
			RandomnessAccumulator::<Test>::get(),
			h2b("f0d42f6b7c0d157ecbd788be44847b80a96c290c04b5dfa5d1d40c98aa0c04ed")
		);

		let header = finalize_block(start_block);

		// Post-finalization status

		assert!(!SlotRandomness::<Test>::exists());
		common_assertions();
		assert_eq!(
			RandomnessAccumulator::<Test>::get(),
			h2b("ada0f5230956c389b378bf2ddb898b88b22ae76f17e860b7ed22d98121a4506f"),
		);

		// Header data check

		assert_eq!(header.digest.logs.len(), 2);
		assert_eq!(header.digest.logs[0], digest.logs[0]);

		// Genesis epoch start deposits consensus
		let consensus_log = sp_consensus_sassafras::digests::ConsensusLog::NextEpochData(
			sp_consensus_sassafras::digests::NextEpochDescriptor {
				authorities: NextAuthorities::<Test>::get().into_inner(),
				randomness: NextRandomness::<Test>::get(),
				config: None,
			},
		);
		let consensus_digest = DigestItem::Consensus(SASSAFRAS_ENGINE_ID, consensus_log.encode());
		assert_eq!(header.digest.logs[1], consensus_digest)
	})
}

#[test]
fn on_normal_block() {
	let (pairs, mut ext) = new_test_ext_with_pairs(4, false);
	let start_slot = Slot::from(100);
	let start_block = 1;
	let end_block = start_block + 1;

	ext.execute_with(|| {
		initialize_block(start_block, start_slot, Default::default(), &pairs[0]);

		// We don't want to trigger an epoch change in this test.
		let epoch_length = Sassafras::epoch_length() as u64;
		assert!(epoch_length > end_block);

		// Progress to block 2
		let digest = progress_to_block(end_block, &pairs[0]).unwrap();

		let common_assertions = || {
			assert_eq!(GenesisSlot::<Test>::get(), start_slot);
			assert_eq!(CurrentSlot::<Test>::get(), start_slot + 1);
			assert_eq!(EpochIndex::<Test>::get(), 0);
			assert_eq!(Sassafras::current_epoch_start(), start_slot);
			assert_eq!(Sassafras::current_slot_index(), 1);
			assert_eq!(CurrentRandomness::<Test>::get(), [0; 32]);
			println!("[DEBUG] {}", b2h(NextRandomness::<Test>::get()));
			assert_eq!(
				NextRandomness::<Test>::get(),
				h2b("a49592ef190b96f3eb87bde4c8355e33df28c75006156e8c81998158de2ed49e")
			);
		};

		// Post-initialization status

		assert!(SlotRandomness::<Test>::exists());
		common_assertions();
		println!("[DEBUG] {}", b2h(RandomnessAccumulator::<Test>::get()));
		assert_eq!(
			RandomnessAccumulator::<Test>::get(),
			h2b("ada0f5230956c389b378bf2ddb898b88b22ae76f17e860b7ed22d98121a4506f"),
		);

		let header = finalize_block(end_block);

		// Post-finalization status

		assert!(!SlotRandomness::<Test>::exists());
		common_assertions();
		println!("[DEBUG] {}", b2h(RandomnessAccumulator::<Test>::get()));
		assert_eq!(
			RandomnessAccumulator::<Test>::get(),
			h2b("3c7aa073a703dfd885c60b30fcf0468176e6d98733891542d42340fc1e312e55"),
		);

		// Header data check

		assert_eq!(header.digest.logs.len(), 1);
		assert_eq!(header.digest.logs[0], digest.logs[0]);
	});
}

#[test]
fn produce_epoch_change_digest_no_config() {
	let (pairs, mut ext) = new_test_ext_with_pairs(4, false);

	ext.execute_with(|| {
		let start_slot = Slot::from(100);
		let start_block = 1;

		initialize_block(start_block, start_slot, Default::default(), &pairs[0]);

		// We want to trigger an epoch change in this test.
		let epoch_length = Sassafras::epoch_length() as u64;
		let end_block = start_block + epoch_length;

		let digest = progress_to_block(end_block, &pairs[0]).unwrap();

		let common_assertions = || {
			assert_eq!(GenesisSlot::<Test>::get(), start_slot);
			assert_eq!(CurrentSlot::<Test>::get(), start_slot + epoch_length);
			assert_eq!(EpochIndex::<Test>::get(), 1);
			assert_eq!(Sassafras::current_epoch_start(), start_slot + epoch_length);
			assert_eq!(Sassafras::current_slot_index(), 0);
			println!("[DEBUG] {}", b2h(CurrentRandomness::<Test>::get()));
			assert_eq!(
				CurrentRandomness::<Test>::get(),
				h2b("a49592ef190b96f3eb87bde4c8355e33df28c75006156e8c81998158de2ed49e")
			);
		};

		// Post-initialization status

		assert!(SlotRandomness::<Test>::exists());
		common_assertions();
		println!("[DEBUG] {}", b2h(NextRandomness::<Test>::get()));
		assert_eq!(
			NextRandomness::<Test>::get(),
			h2b("c87c38cdf6b41cf58d1af07eb0109f8fcbe6b655ae755d015ff41a786c3768db"),
		);
		println!("[DEBUG] {}", b2h(RandomnessAccumulator::<Test>::get()));
		assert_eq!(
			RandomnessAccumulator::<Test>::get(),
			h2b("18d427ef033048a6f8dad587af5438a042244f820f58b5b2a6023fffd18a20de"),
		);

		let header = finalize_block(end_block);

		// Post-finalization status

		assert!(!SlotRandomness::<Test>::exists());
		common_assertions();
		println!("[DEBUG] {}", b2h(NextRandomness::<Test>::get()));
		assert_eq!(
			NextRandomness::<Test>::get(),
			h2b("c87c38cdf6b41cf58d1af07eb0109f8fcbe6b655ae755d015ff41a786c3768db"),
		);
		println!("[DEBUG] {}", b2h(RandomnessAccumulator::<Test>::get()));
		assert_eq!(
			RandomnessAccumulator::<Test>::get(),
			h2b("5ff1fc025393b8ad77f2223e4345d3d8c608b435e1ff2654f98a51747a1df1a9"),
		);

		// Header data check

		assert_eq!(header.digest.logs.len(), 2);
		assert_eq!(header.digest.logs[0], digest.logs[0]);
		// Deposits consensus log on epoch change
		let consensus_log = sp_consensus_sassafras::digests::ConsensusLog::NextEpochData(
			sp_consensus_sassafras::digests::NextEpochDescriptor {
				authorities: NextAuthorities::<Test>::get().into_inner(),
				randomness: NextRandomness::<Test>::get(),
				config: None,
			},
		);
		let consensus_digest = DigestItem::Consensus(SASSAFRAS_ENGINE_ID, consensus_log.encode());
		assert_eq!(header.digest.logs[1], consensus_digest)
	})
}

#[test]
fn produce_epoch_change_digest_with_config() {
	let (pairs, mut ext) = new_test_ext_with_pairs(4, false);

	ext.execute_with(|| {
		let start_slot = Slot::from(100);
		let start_block = 1;

		initialize_block(start_block, start_slot, Default::default(), &pairs[0]);

		let config = EpochConfiguration { redundancy_factor: 1, attempts_number: 123 };
		Sassafras::plan_config_change(RuntimeOrigin::root(), config).unwrap();

		// We want to trigger an epoch change in this test.
		let epoch_length = Sassafras::epoch_length() as u64;
		let end_block = start_block + epoch_length;

		let digest = progress_to_block(end_block, &pairs[0]).unwrap();

		let header = finalize_block(end_block);

		// Header data check.
		// Skip pallet status checks that were already performed by other tests.

		assert_eq!(header.digest.logs.len(), 2);
		assert_eq!(header.digest.logs[0], digest.logs[0]);
		// Deposits consensus log on epoch change
		let consensus_log = sp_consensus_sassafras::digests::ConsensusLog::NextEpochData(
			sp_consensus_sassafras::digests::NextEpochDescriptor {
				authorities: NextAuthorities::<Test>::get().into_inner(),
				randomness: NextRandomness::<Test>::get(),
				config: Some(config),
			},
		);
		let consensus_digest = DigestItem::Consensus(SASSAFRAS_ENGINE_ID, consensus_log.encode());
		assert_eq!(header.digest.logs[1], consensus_digest)
	})
}

#[test]
fn segments_incremental_sort_works() {
	let (pairs, mut ext) = new_test_ext_with_pairs(1, false);
	let pair = &pairs[0];
	let segments_count = 14;
	let start_slot = Slot::from(100);
	let start_block = 1;

	ext.execute_with(|| {
		let epoch_length = Sassafras::epoch_length() as u64;
		// -3 just to have the last segment not full...
		let submitted_tickets_count = segments_count * SEGMENT_MAX_SIZE - 3;

		initialize_block(start_block, start_slot, Default::default(), pair);

		// Manually populate the segments to skip the threshold check
		let mut tickets = make_ticket_bodies(submitted_tickets_count, None);
		persist_next_epoch_tickets_as_segments(&tickets);

		// Proceed to half of the epoch (sortition should not have been started yet)
		let half_epoch_block = start_block + epoch_length / 2;
		progress_to_block(half_epoch_block, pair);

		let mut unsorted_tickets_count = submitted_tickets_count;

		// Check that next epoch tickets sortition is not started yet
		let meta = TicketsMeta::<Test>::get();
		assert_eq!(meta.unsorted_tickets_count, unsorted_tickets_count);
		assert_eq!(meta.tickets_count, [0, 0]);

		// Follow the incremental sortition block by block

		progress_to_block(half_epoch_block + 1, pair);
		unsorted_tickets_count -= 3 * SEGMENT_MAX_SIZE - 3;
		let meta = TicketsMeta::<Test>::get();
		assert_eq!(meta.unsorted_tickets_count, unsorted_tickets_count,);
		assert_eq!(meta.tickets_count, [0, 0]);

		progress_to_block(half_epoch_block + 2, pair);
		unsorted_tickets_count -= 3 * SEGMENT_MAX_SIZE;
		let meta = TicketsMeta::<Test>::get();
		assert_eq!(meta.unsorted_tickets_count, unsorted_tickets_count);
		assert_eq!(meta.tickets_count, [0, 0]);

		progress_to_block(half_epoch_block + 3, pair);
		unsorted_tickets_count -= 3 * SEGMENT_MAX_SIZE;
		let meta = TicketsMeta::<Test>::get();
		assert_eq!(meta.unsorted_tickets_count, unsorted_tickets_count);
		assert_eq!(meta.tickets_count, [0, 0]);

		progress_to_block(half_epoch_block + 4, pair);
		unsorted_tickets_count -= 3 * SEGMENT_MAX_SIZE;
		let meta = TicketsMeta::<Test>::get();
		assert_eq!(meta.unsorted_tickets_count, unsorted_tickets_count);
		assert_eq!(meta.tickets_count, [0, 0]);

		let header = finalize_block(half_epoch_block + 4);

		// Sort should be finished now.
		// Check that next epoch tickets count have the correct value.
		// Bigger ticket ids were discarded during sortition.
		unsorted_tickets_count -= 2 * SEGMENT_MAX_SIZE;
		assert_eq!(unsorted_tickets_count, 0);
		let meta = TicketsMeta::<Test>::get();
		assert_eq!(meta.unsorted_tickets_count, unsorted_tickets_count);
		assert_eq!(meta.tickets_count, [0, epoch_length as u32]);
		// Epoch change log should have been pushed as well
		assert_eq!(header.digest.logs.len(), 1);
		// No tickets for the current epoch
		assert_eq!(TicketsIds::<Test>::get((0, 0)), None);

		// Check persistence of "winning" tickets
		tickets.sort_by_key(|t| t.0);
		(0..epoch_length as usize).into_iter().for_each(|i| {
			let id = TicketsIds::<Test>::get((1, i as u32)).unwrap();
			let body = TicketsData::<Test>::get(id).unwrap();
			assert_eq!((id, body), tickets[i]);
		});
		// Check removal of "loosing" tickets
		(epoch_length as usize..tickets.len()).into_iter().for_each(|i| {
			assert!(TicketsIds::<Test>::get((1, i as u32)).is_none());
			assert!(TicketsData::<Test>::get(tickets[i].0).is_none());
		});

		// The next block will be the first produced on the new epoch.
		// At this point the tickets are found already sorted and ready to be used.
		let slot = CurrentSlot::<Test>::get() + 1;
		let number = System::block_number() + 1;
		initialize_block(number, slot, header.hash(), pair);
		let header = finalize_block(number);
		// Epoch changes digest is also produced
		assert_eq!(header.digest.logs.len(), 2);
	});
}

#[test]
fn tickets_fetch_works_after_epoch_change() {
	let (pairs, mut ext) = new_test_ext_with_pairs(4, false);
	let pair = &pairs[0];
	let start_slot = Slot::from(100);
	let start_block = 1;
	let submitted_tickets = 300;

	ext.execute_with(|| {
		initialize_block(start_block, start_slot, Default::default(), pair);

		// We don't want to trigger an epoch change in this test.
		let epoch_length = Sassafras::epoch_length() as u64;
		assert!(epoch_length > 2);
		progress_to_block(2, &pairs[0]).unwrap();

		// Persist tickets as three different segments.
		let tickets = make_ticket_bodies(submitted_tickets, None);
		persist_next_epoch_tickets_as_segments(&tickets);

		let meta = TicketsMeta::<Test>::get();
		assert_eq!(meta.unsorted_tickets_count, submitted_tickets);
		assert_eq!(meta.tickets_count, [0, 0]);

		// Progress up to the last epoch slot (do not enact epoch change)
		progress_to_block(epoch_length, &pairs[0]).unwrap();

		// At this point next epoch tickets should have been sorted and ready to be used
		let meta = TicketsMeta::<Test>::get();
		assert_eq!(meta.unsorted_tickets_count, 0);
		assert_eq!(meta.tickets_count, [0, epoch_length as u32]);

		// Compute and sort the tickets ids (aka tickets scores)
		let mut expected_ids: Vec<_> = tickets.into_iter().map(|(id, _)| id).collect();
		expected_ids.sort();
		expected_ids.truncate(epoch_length as usize);

		// Check if we can fetch next epoch tickets ids (outside-in).
		let slot = CurrentSlot::<Test>::get();
		assert_eq!(Sassafras::slot_ticket_id(slot + 1).unwrap(), expected_ids[1]);
		assert_eq!(Sassafras::slot_ticket_id(slot + 2).unwrap(), expected_ids[3]);
		assert_eq!(Sassafras::slot_ticket_id(slot + 3).unwrap(), expected_ids[5]);
		assert_eq!(Sassafras::slot_ticket_id(slot + 4).unwrap(), expected_ids[7]);
		assert_eq!(Sassafras::slot_ticket_id(slot + 7).unwrap(), expected_ids[6]);
		assert_eq!(Sassafras::slot_ticket_id(slot + 8).unwrap(), expected_ids[4]);
		assert_eq!(Sassafras::slot_ticket_id(slot + 9).unwrap(), expected_ids[2]);
		assert_eq!(Sassafras::slot_ticket_id(slot + 10).unwrap(), expected_ids[0]);
		assert!(Sassafras::slot_ticket_id(slot + 11).is_none());

		// Enact epoch change by progressing one more block

		progress_to_block(epoch_length + 1, &pairs[0]).unwrap();

		let meta = TicketsMeta::<Test>::get();
		assert_eq!(meta.unsorted_tickets_count, 0);
		assert_eq!(meta.tickets_count, [0, 10]);

		// Check if we can fetch current epoch tickets ids (outside-in).
		let slot = CurrentSlot::<Test>::get();
		assert_eq!(Sassafras::slot_ticket_id(slot).unwrap(), expected_ids[1]);
		assert_eq!(Sassafras::slot_ticket_id(slot + 1).unwrap(), expected_ids[3]);
		assert_eq!(Sassafras::slot_ticket_id(slot + 2).unwrap(), expected_ids[5]);
		assert_eq!(Sassafras::slot_ticket_id(slot + 3).unwrap(), expected_ids[7]);
		assert_eq!(Sassafras::slot_ticket_id(slot + 6).unwrap(), expected_ids[6]);
		assert_eq!(Sassafras::slot_ticket_id(slot + 7).unwrap(), expected_ids[4]);
		assert_eq!(Sassafras::slot_ticket_id(slot + 8).unwrap(), expected_ids[2]);
		assert_eq!(Sassafras::slot_ticket_id(slot + 9).unwrap(), expected_ids[0]);
		assert!(Sassafras::slot_ticket_id(slot + 10).is_none());

		// Enact another epoch change, for which we don't have any ticket
		progress_to_block(2 * epoch_length + 1, &pairs[0]).unwrap();
		let meta = TicketsMeta::<Test>::get();
		assert_eq!(meta.unsorted_tickets_count, 0);
		assert_eq!(meta.tickets_count, [0, 0]);
	});
}

#[test]
fn block_allowed_to_skip_epochs() {
	let (pairs, mut ext) = new_test_ext_with_pairs(4, false);
	let pair = &pairs[0];
	let start_slot = Slot::from(100);
	let start_block = 1;

	ext.execute_with(|| {
		let epoch_length = Sassafras::epoch_length() as u64;

		initialize_block(start_block, start_slot, Default::default(), pair);

		let tickets = make_ticket_bodies(3, Some(pair));
		persist_next_epoch_tickets(&tickets);

		let next_random = NextRandomness::<Test>::get();

		// We want to skip 3 epochs in this test.
		let offset = 4 * epoch_length;
		System::set_block_number(start_block + offset - 1);
		go_to_block(start_block + offset, start_slot + offset, &pairs[0]);

		// Post-initialization status

		assert!(SlotRandomness::<Test>::exists());
		assert_eq!(GenesisSlot::<Test>::get(), start_slot);
		assert_eq!(CurrentSlot::<Test>::get(), start_slot + offset);
		assert_eq!(EpochIndex::<Test>::get(), 4);
		assert_eq!(Sassafras::current_epoch_start(), start_slot + offset);
		assert_eq!(Sassafras::current_slot_index(), 0);

		// Tickets data has been discarded
		assert_eq!(TicketsMeta::<Test>::get(), TicketsMetadata::default());
		assert!(tickets.iter().all(|(id, _)| TicketsData::<Test>::get(id).is_none()));
		assert_eq!(SortedCandidates::<Test>::get().len(), 0);

		// We used the last known next epoch randomness as a fallback
		assert_eq!(next_random, CurrentRandomness::<Test>::get());
	});
}

#[test]
fn obsolete_tickets_are_removed_on_epoch_change() {
	let (pairs, mut ext) = new_test_ext_with_pairs(4, false);
	let pair = &pairs[0];
	let start_slot = Slot::from(100);
	let start_block = 1;

	ext.execute_with(|| {
		let epoch_length = Sassafras::epoch_length() as u64;

		initialize_block(start_block, start_slot, Default::default(), pair);

		let tickets = make_ticket_bodies(10, Some(pair));
		let mut epoch1_tickets = tickets[..4].to_vec();
		let mut epoch2_tickets = tickets[4..].to_vec();

		// Persist some tickets for next epoch (N)
		persist_next_epoch_tickets(&epoch1_tickets);
		assert_eq!(TicketsMeta::<Test>::get().tickets_count, [0, 4]);
		// Check next epoch tickets presence
		epoch1_tickets.sort_by_key(|t| t.0);
		(0..epoch1_tickets.len()).into_iter().for_each(|i| {
			let id = TicketsIds::<Test>::get((1, i as u32)).unwrap();
			let body = TicketsData::<Test>::get(id).unwrap();
			assert_eq!((id, body), epoch1_tickets[i]);
		});

		// Advance one epoch to enact the tickets
		progress_to_block(start_block + epoch_length, pair).unwrap();
		assert_eq!(TicketsMeta::<Test>::get().tickets_count, [0, 4]);

		// Persist some tickets for next epoch (N+1)
		persist_next_epoch_tickets(&epoch2_tickets);
		assert_eq!(TicketsMeta::<Test>::get().tickets_count, [6, 4]);
		epoch2_tickets.sort_by_key(|t| t.0);
		// Check for this epoch and next epoch tickets presence
		(0..epoch1_tickets.len()).into_iter().for_each(|i| {
			let id = TicketsIds::<Test>::get((1, i as u32)).unwrap();
			let body = TicketsData::<Test>::get(id).unwrap();
			assert_eq!((id, body), epoch1_tickets[i]);
		});
		(0..epoch2_tickets.len()).into_iter().for_each(|i| {
			let id = TicketsIds::<Test>::get((0, i as u32)).unwrap();
			let body = TicketsData::<Test>::get(id).unwrap();
			assert_eq!((id, body), epoch2_tickets[i]);
		});

		// Advance to epoch 2 and check for cleanup

		progress_to_block(start_block + 2 * epoch_length, pair).unwrap();
		assert_eq!(TicketsMeta::<Test>::get().tickets_count, [6, 0]);

		(0..epoch1_tickets.len()).into_iter().for_each(|i| {
			let id = TicketsIds::<Test>::get((1, i as u32)).unwrap();
			assert!(TicketsData::<Test>::get(id).is_none());
		});
		(0..epoch2_tickets.len()).into_iter().for_each(|i| {
			let id = TicketsIds::<Test>::get((0, i as u32)).unwrap();
			let body = TicketsData::<Test>::get(id).unwrap();
			assert_eq!((id, body), epoch2_tickets[i]);
		});
	})
}

const TICKETS_FILE: &str = "src/data/25_tickets_100_auths.bin";

fn data_read<T: Decode>(filename: &str) -> T {
	use std::{fs::File, io::Read};
	let mut file = File::open(filename).unwrap();
	let mut buf = Vec::new();
	file.read_to_end(&mut buf).unwrap();
	T::decode(&mut &buf[..]).unwrap()
}

fn data_write<T: Encode>(filename: &str, data: T) {
	use std::{fs::File, io::Write};
	let mut file = File::create(filename).unwrap();
	let buf = data.encode();
	file.write_all(&buf).unwrap();
}

// We don't want to implement anything secure here.
// Just a trivial shuffle for the tests.
fn trivial_fisher_yates_shuffle<T>(vector: &mut Vec<T>, random_seed: u64) {
	let mut rng = random_seed as usize;
	for i in (1..vector.len()).rev() {
		let j = rng % (i + 1);
		vector.swap(i, j);
		rng = (rng.wrapping_mul(6364793005) + 1) as usize; // Some random number generation
	}
}

// For this test we use a set of pre-constructed tickets from a file.
// Creating a large set of tickets on the fly takes time, and may be annoying
// for test execution.
//
// A valid ring-context is required for this test since we are passing through the
// `submit_ticket` call which tests for ticket validity.
#[test]
fn submit_tickets_with_ring_proof_check_works() {
	use sp_core::Pair as _;

	let (authorities, mut tickets): (Vec<AuthorityId>, Vec<TicketEnvelope>) =
		data_read(TICKETS_FILE);

	// Also checks that duplicates are discarded
	tickets.extend(tickets.clone());
	trivial_fisher_yates_shuffle(&mut tickets, 321);

	let (pairs, mut ext) = new_test_ext_with_pairs(authorities.len(), true);
	let pair = &pairs[0];
	// Check if deserialized data has been generated for the correct set of authorities...
	assert!(authorities.iter().zip(pairs.iter()).all(|(auth, pair)| auth == &pair.public()));

	ext.execute_with(|| {
		let start_slot = Slot::from(0);
		let start_block = 1;

		// Tweak the config to discard ~half of the tickets.
		let mut config = EpochConfig::<Test>::get();
		config.redundancy_factor = 25;
		EpochConfig::<Test>::set(config);

		initialize_block(start_block, start_slot, Default::default(), pair);
		NextRandomness::<Test>::set([0; 32]);

		// Check state before tickets submission
		assert_eq!(
			TicketsMeta::<Test>::get(),
			TicketsMetadata { unsorted_tickets_count: 0, tickets_count: [0, 0] },
		);

		// Submit the tickets
		let max_tickets_per_call = Sassafras::epoch_length() as usize;
		tickets.chunks(max_tickets_per_call).for_each(|chunk| {
			let chunk = BoundedVec::truncate_from(chunk.to_vec());
			Sassafras::submit_tickets(RuntimeOrigin::none(), chunk).unwrap();
		});

		// Check state after submission
		assert_eq!(
			TicketsMeta::<Test>::get(),
			TicketsMetadata { unsorted_tickets_count: 15, tickets_count: [0, 0] },
		);
		assert_eq!(UnsortedSegments::<Test>::get(0).len(), 15);
		assert_eq!(UnsortedSegments::<Test>::get(1).len(), 0);

		finalize_block(start_block);
	})
}

#[test]
#[ignore = "test tickets data generator"]
fn make_tickets_data() {
	use super::*;
	use sp_core::crypto::Pair;

	// Number of authorities who produces tickets (for the sake of this test)
	let tickets_authors_count = 5;
	// Total number of authorities (the ring)
	let authorities_count = 100;
	let (pairs, mut ext) = new_test_ext_with_pairs(authorities_count, true);

	let authorities: Vec<_> = pairs.iter().map(|sk| sk.public()).collect();

	ext.execute_with(|| {
		let config = EpochConfig::<Test>::get();

		let tickets_count = tickets_authors_count * config.attempts_number as usize;
		let mut tickets = Vec::with_capacity(tickets_count);

		// Construct pre-built tickets with a well known `NextRandomness` value.
		NextRandomness::<Test>::set([0; 32]);

		println!("Constructing {} tickets", tickets_count);
		pairs.iter().take(tickets_authors_count).enumerate().for_each(|(i, pair)| {
			let t = make_tickets(config.attempts_number, pair);
			tickets.extend(t);
			println!("{:.2}%", 100f32 * ((i + 1) as f32 / tickets_authors_count as f32));
		});

		data_write(TICKETS_FILE, (authorities, tickets));
	});
}
