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
use crate::{
	configuration::HostConfiguration,
	mock::{new_test_ext, MockGenesisConfig, ParasShared, Test},
	shared,
};
use assert_matches::assert_matches;
use polkadot_primitives::Hash;
use polkadot_primitives_test_helpers::validator_pubkeys;
use sp_keyring::Sr25519Keyring;

#[test]
fn minimum_relay_parent_number() {
	new_test_ext(MockGenesisConfig::default()).execute_with(|| {
		let mut config = HostConfiguration::default();

		// No entries yet — returns None.
		assert!(Pallet::<Test>::get_minimum_relay_parent_number().is_none());

		let session = 0;
		// Add a relay parent at block 9. This is the first entry for session 0,
		// so MinimumRelayParentNumber is set to 9.
		Pallet::<Test>::new_block(
			Hash::repeat_byte(1),
			Default::default(),
			9,
			5,
			Default::default(),
			session,
		);
		assert_eq!(Pallet::<Test>::get_minimum_relay_parent_number(), Some(9));

		// Add another relay parent at block 10. Minimum stays at 9.
		Pallet::<Test>::new_block(
			Hash::repeat_byte(2),
			Default::default(),
			10,
			5,
			Default::default(),
			session,
		);
		assert_eq!(Pallet::<Test>::get_minimum_relay_parent_number(), Some(9));

		// New session 1 with max_relay_parent_session_age=1 (keep both sessions).
		let session = 1;
		config.max_relay_parent_session_age = 1;
		Pallet::<Test>::initializer_on_new_session(session, [0; 32], &config, vec![]);
		Pallet::<Test>::new_block(
			Hash::repeat_byte(3),
			Default::default(),
			20,
			5,
			Default::default(),
			session,
		);
		// Minimum is still 9 from session 0 (oldest session).
		assert_eq!(Pallet::<Test>::get_minimum_relay_parent_number(), Some(9));

		// New session 2 with max_relay_parent_session_age=1. Session 0 gets pruned.
		let session = 2;
		config.max_relay_parent_session_age = 1;
		Pallet::<Test>::initializer_on_new_session(session, [0; 32], &config, vec![]);
		Pallet::<Test>::new_block(
			Hash::repeat_byte(4),
			Default::default(),
			30,
			5,
			Default::default(),
			session,
		);
		// Session 0 pruned, oldest is now session 1 with min block 20.
		assert_eq!(Pallet::<Test>::get_minimum_relay_parent_number(), Some(20));

		// New session 3 with max_relay_parent_session_age=0. Sessions 1 and 2 get pruned.
		let session = 3;
		config.max_relay_parent_session_age = 0;
		Pallet::<Test>::initializer_on_new_session(session, [0; 32], &config, vec![]);
		Pallet::<Test>::new_block(
			Hash::repeat_byte(5),
			Default::default(),
			40,
			5,
			Default::default(),
			session,
		);
		// Only session 3 remains, min is 40.
		assert_eq!(Pallet::<Test>::get_minimum_relay_parent_number(), Some(40));
	});
}

#[test]
fn tracker_claim_queue_transpose() {
	let mut tracker = AllowedSchedulingParentsTracker::<Hash, u32>::default();

	let mut claim_queue = BTreeMap::new();
	claim_queue.insert(CoreIndex(0), vec![Id::from(0), Id::from(1), Id::from(2)].into());
	claim_queue.insert(CoreIndex(1), vec![Id::from(0), Id::from(0), Id::from(100)].into());
	claim_queue.insert(CoreIndex(2), vec![Id::from(1), Id::from(2), Id::from(100)].into());

	tracker.update(Hash::zero(), claim_queue, 1u32, 4);

	let (info, _block_num) = tracker.acquire_info(Hash::zero()).unwrap();
	assert_eq!(
		info.claim_queue.get(&Id::from(0)).unwrap()[&0],
		vec![CoreIndex(0), CoreIndex(1)].into_iter().collect::<BTreeSet<_>>()
	);
	assert_eq!(
		info.claim_queue.get(&Id::from(1)).unwrap()[&0],
		vec![CoreIndex(2)].into_iter().collect::<BTreeSet<_>>()
	);
	assert_eq!(info.claim_queue.get(&Id::from(2)).unwrap().get(&0), None);
	assert_eq!(info.claim_queue.get(&Id::from(100)).unwrap().get(&0), None);

	assert_eq!(
		info.claim_queue.get(&Id::from(0)).unwrap()[&1],
		vec![CoreIndex(1)].into_iter().collect::<BTreeSet<_>>()
	);
	assert_eq!(
		info.claim_queue.get(&Id::from(1)).unwrap()[&1],
		vec![CoreIndex(0)].into_iter().collect::<BTreeSet<_>>()
	);
	assert_eq!(
		info.claim_queue.get(&Id::from(2)).unwrap()[&1],
		vec![CoreIndex(2)].into_iter().collect::<BTreeSet<_>>()
	);
	assert_eq!(info.claim_queue.get(&Id::from(100)).unwrap().get(&1), None);

	assert_eq!(info.claim_queue.get(&Id::from(0)).unwrap().get(&2), None);
	assert_eq!(info.claim_queue.get(&Id::from(1)).unwrap().get(&2), None);
	assert_eq!(
		info.claim_queue.get(&Id::from(2)).unwrap()[&2],
		vec![CoreIndex(0)].into_iter().collect::<BTreeSet<_>>()
	);
	assert_eq!(
		info.claim_queue.get(&Id::from(100)).unwrap()[&2],
		vec![CoreIndex(1), CoreIndex(2)].into_iter().collect::<BTreeSet<_>>()
	);
}

#[test]
fn scheduling_tracker_acquire_info() {
	let mut tracker = AllowedSchedulingParentsTracker::<Hash, u32>::default();
	let max_ancestry_len = 2;

	let blocks = &[Hash::repeat_byte(0), Hash::repeat_byte(1), Hash::repeat_byte(2)];

	tracker.update(blocks[0], Default::default(), 0, max_ancestry_len + 1);
	assert_matches!(
		tracker.acquire_info(blocks[0]),
		Some((s, b)) if s.scheduling_parent == blocks[0] && b == 0
	);

	// Try to push a duplicate. Should be ignored.
	tracker.update(blocks[0], Default::default(), 0, max_ancestry_len + 1);
	assert_eq!(tracker.buffer.len(), 1);
	assert_matches!(
		tracker.acquire_info(blocks[0]),
		Some((s, b)) if s.scheduling_parent == blocks[0] && b == 0
	);

	tracker.update(blocks[1], Default::default(), 1u32, max_ancestry_len + 1);
	tracker.update(blocks[2], Default::default(), 2u32, max_ancestry_len + 1);
	for (block_num, hash) in blocks.iter().enumerate() {
		assert_matches!(
			tracker.acquire_info(*hash),
			Some((s, b)) if s.scheduling_parent == *hash && b == block_num as u32
		);
	}
}

#[test]
fn new_block_inserts_relay_parent_and_scheduling_parent() {
	new_test_ext(MockGenesisConfig::default()).execute_with(|| {
		let hash = Hash::repeat_byte(1);
		let block_number = 5u32;
		let session = 0u32;
		let state_root = Hash::repeat_byte(0xBB);

		Pallet::<Test>::new_block(hash, Default::default(), block_number, 10, state_root, session);

		// Relay parent should be in the DoubleMap.
		let info = Pallet::<Test>::get_relay_parent_info(session, hash)
			.expect("relay parent should exist");
		assert_eq!(info.number, block_number);
		assert_eq!(info.state_root, state_root);

		// Scheduling parent should be in the tracker.
		let tracker = AllowedSchedulingParents::<Test>::get();
		assert_eq!(tracker.buffer.len(), 1);
		assert_eq!(tracker.buffer[0].scheduling_parent, hash);
	});
}

#[test]
fn session_change_prunes_old_relay_parents() {
	new_test_ext(MockGenesisConfig::default()).execute_with(|| {
		let mut config = HostConfiguration::default();

		// At genesis, OldestRelayParentSession starts at 0.
		assert_eq!(OldestRelayParentSession::<Test>::get(), 0);

		// Insert multiple entries per session in sessions 0, 1, 2.
		config.max_relay_parent_session_age = 2;
		for session in 0..3u32 {
			Pallet::<Test>::initializer_on_new_session(session, [0; 32], &config, vec![]);
			for i in 1..=3u32 {
				let hash = Hash::repeat_byte((session * 10 + i) as u8);
				Pallet::<Test>::new_block(
					hash,
					Default::default(),
					session * 10 + i,
					10,
					Default::default(),
					session,
				);
			}
		}

		// With max_relay_parent_session_age=2 at session 2,
		// oldest_allowed = 2 - 2 = 0. All sessions should still exist.
		for i in 1..=3u32 {
			assert!(Pallet::<Test>::get_relay_parent_info(0, Hash::repeat_byte(i as u8)).is_some());
			assert!(Pallet::<Test>::get_relay_parent_info(1, Hash::repeat_byte((10 + i) as u8))
				.is_some());
			assert!(Pallet::<Test>::get_relay_parent_info(2, Hash::repeat_byte((20 + i) as u8))
				.is_some());
		}
		assert_eq!(OldestRelayParentSession::<Test>::get(), 0);

		// Now move to session 3 with max_age=2. oldest_allowed = 3 - 2 = 1.
		// All entries from session 0 should be pruned.
		Pallet::<Test>::initializer_on_new_session(3, [0; 32], &config, vec![]);
		assert_eq!(AllowedRelayParents::<Test>::iter_prefix(0).count(), 0);
		for i in 1..=3u32 {
			assert!(Pallet::<Test>::get_relay_parent_info(1, Hash::repeat_byte((10 + i) as u8))
				.is_some());
			assert!(Pallet::<Test>::get_relay_parent_info(2, Hash::repeat_byte((20 + i) as u8))
				.is_some());
		}

		Pallet::<Test>::new_block(
			Hash::repeat_byte(31),
			Default::default(),
			31,
			10,
			Default::default(),
			3,
		);
		assert!(Pallet::<Test>::get_relay_parent_info(3, Hash::repeat_byte(31)).is_some());
		assert_eq!(OldestRelayParentSession::<Test>::get(), 1);

		// Session 5 with max_age=2. oldest_allowed = 5 - 2 = 3.
		// All entries from sessions 1 and 2 should be pruned.
		Pallet::<Test>::initializer_on_new_session(5, [0; 32], &config, vec![]);
		assert_eq!(AllowedRelayParents::<Test>::iter_prefix(1).count(), 0);
		assert_eq!(AllowedRelayParents::<Test>::iter_prefix(2).count(), 0);
		assert!(Pallet::<Test>::get_relay_parent_info(3, Hash::repeat_byte(31)).is_some());
		assert_eq!(OldestRelayParentSession::<Test>::get(), 3);
	});
}

#[test]
fn max_age_zero_keeps_only_current_session() {
	new_test_ext(MockGenesisConfig::default()).execute_with(|| {
		let config = HostConfiguration::default();

		// Insert into session 0.
		Pallet::<Test>::new_block(
			Hash::repeat_byte(0),
			Default::default(),
			0,
			10,
			Default::default(),
			0,
		);
		assert!(Pallet::<Test>::get_relay_parent_info(0, Hash::repeat_byte(0)).is_some());

		// Move to session 1 with max_age=0. Session 0 should be pruned.
		Pallet::<Test>::initializer_on_new_session(1, [0; 32], &config, vec![]);
		assert!(Pallet::<Test>::get_relay_parent_info(0, Hash::repeat_byte(0)).is_none());
		assert_eq!(OldestRelayParentSession::<Test>::get(), 1);
	});
}

#[test]
fn increasing_max_age() {
	new_test_ext(MockGenesisConfig::default()).execute_with(|| {
		let mut config = HostConfiguration::default();

		// Build up sessions 0..5 with max_age=1.
		config.max_relay_parent_session_age = 1;
		for session in 0..5u32 {
			Pallet::<Test>::initializer_on_new_session(session, [0; 32], &config, vec![]);
			Pallet::<Test>::new_block(
				Hash::repeat_byte(session as u8),
				Default::default(),
				session * 10 + 1,
				10,
				Default::default(),
				session,
			);
		}

		// After session 4 with max_age=1, oldest_allowed = 3.
		// Sessions 0, 1, 2 are pruned.
		assert_eq!(OldestRelayParentSession::<Test>::get(), 3);
		assert!(Pallet::<Test>::get_relay_parent_info(2, Hash::repeat_byte(2)).is_none());
		assert!(Pallet::<Test>::get_relay_parent_info(3, Hash::repeat_byte(3)).is_some());

		// Now increase max_age to 10. oldest_allowed = 4 - 10 = 0 (saturating).
		// OldestRelayParentSession must NOT move backward to 0 — sessions 0, 1, 2
		// are already gone.
		config.max_relay_parent_session_age = 10;
		Pallet::<Test>::initializer_on_new_session(4, [0; 32], &config, vec![]);
		assert_eq!(OldestRelayParentSession::<Test>::get(), 3);
		// Sessions 0, 1, 2 are still gone.
		assert!(Pallet::<Test>::get_relay_parent_info(0, Hash::repeat_byte(0)).is_none());
		assert!(Pallet::<Test>::get_relay_parent_info(1, Hash::repeat_byte(1)).is_none());
		assert!(Pallet::<Test>::get_relay_parent_info(2, Hash::repeat_byte(2)).is_none());
		// But sessions 3 and 4 are still there.
		assert!(Pallet::<Test>::get_relay_parent_info(3, Hash::repeat_byte(3)).is_some());
		assert!(Pallet::<Test>::get_relay_parent_info(4, Hash::repeat_byte(4)).is_some());
	});
}

#[test]
fn decreasing_max_age_prunes_multiple_sessions() {
	new_test_ext(MockGenesisConfig::default()).execute_with(|| {
		let mut config = HostConfiguration::default();

		// Build up sessions 0..5 with max_age=5 (no pruning at all).
		config.max_relay_parent_session_age = 5;
		for session in 0..5u32 {
			ParasShared::initializer_on_new_session(session, [0; 32], &config, vec![]);
			Pallet::<Test>::new_block(
				Hash::repeat_byte(session as u8),
				Default::default(),
				session * 10 + 1,
				10,
				Default::default(),
				session,
			);
		}
		assert_eq!(OldestRelayParentSession::<Test>::get(), 0);

		// All sessions present.
		for session in 0..5u32 {
			assert!(Pallet::<Test>::get_relay_parent_info(
				session,
				Hash::repeat_byte(session as u8)
			)
			.is_some());
		}

		// Now decrease max_age to 1 at session 5. oldest_allowed = 5 - 1 = 4.
		// Sessions 0, 1, 2, 3 should all be pruned in one go.
		config.max_relay_parent_session_age = 1;
		ParasShared::initializer_on_new_session(5, [0; 32], &config, vec![]);
		assert_eq!(OldestRelayParentSession::<Test>::get(), 4);

		for session in 0..4u32 {
			assert!(Pallet::<Test>::get_relay_parent_info(
				session,
				Hash::repeat_byte(session as u8)
			)
			.is_none());
		}
		assert!(Pallet::<Test>::get_relay_parent_info(4, Hash::repeat_byte(4)).is_some());
	});
}

#[test]
fn cross_session_relay_parents_are_accessible() {
	new_test_ext(MockGenesisConfig::default()).execute_with(|| {
		// Insert relay parents across 3 sessions.
		let hashes: Vec<Hash> = (0..3u8).map(|i| Hash::repeat_byte(i + 1)).collect();

		Pallet::<Test>::new_block(hashes[0], Default::default(), 10, 10, Default::default(), 0);
		Pallet::<Test>::new_block(hashes[1], Default::default(), 20, 10, Default::default(), 1);
		Pallet::<Test>::new_block(hashes[2], Default::default(), 30, 10, Default::default(), 2);

		// All relay parents from all sessions should be accessible.
		for (session, hash) in hashes.iter().enumerate() {
			let info = Pallet::<Test>::get_relay_parent_info(session as u32, *hash)
				.expect("relay parent should be accessible from older session");
			assert_eq!(info.number, (session as u32 + 1) * 10);
		}

		// Wrong session returns None.
		assert!(Pallet::<Test>::get_relay_parent_info(1, hashes[0]).is_none());
		// Wrong hash returns None.
		assert!(Pallet::<Test>::get_relay_parent_info(0, Hash::repeat_byte(99)).is_none());
	});
}

#[test]
fn session_change_clears_scheduling_parents_but_not_relay_parents() {
	new_test_ext(MockGenesisConfig::default()).execute_with(|| {
		let mut config = HostConfiguration::default();

		// Session 0: insert some relay parents.
		Pallet::<Test>::new_block(
			Hash::repeat_byte(1),
			Default::default(),
			1,
			10,
			Default::default(),
			0,
		);
		Pallet::<Test>::new_block(
			Hash::repeat_byte(2),
			Default::default(),
			2,
			10,
			Default::default(),
			0,
		);

		let tracker = AllowedSchedulingParents::<Test>::get();
		assert_eq!(tracker.buffer.len(), 2);

		// Simulate session change — this clears the scheduling parents buffer.
		config.max_relay_parent_session_age = 1;
		ParasShared::initializer_on_new_session(1, [0; 32], &config, vec![]);

		// Scheduling parents buffer should be empty.
		let tracker = AllowedSchedulingParents::<Test>::get();
		assert!(tracker.buffer.is_empty());

		// But relay parents from session 0 should still be in the DoubleMap.
		assert!(Pallet::<Test>::get_relay_parent_info(0, Hash::repeat_byte(1)).is_some());
		assert!(Pallet::<Test>::get_relay_parent_info(0, Hash::repeat_byte(2)).is_some());
	});
}

#[test]
fn sets_and_shuffles_validators() {
	let validators = vec![
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Ferdie,
	];

	let mut config = HostConfiguration::default();
	config.max_validators = None;

	let pubkeys = validator_pubkeys(&validators);

	new_test_ext(MockGenesisConfig::default()).execute_with(|| {
		let validators = ParasShared::initializer_on_new_session(1, [1; 32], &config, pubkeys);

		assert_eq!(
			validators,
			validator_pubkeys(&[
				Sr25519Keyring::Ferdie,
				Sr25519Keyring::Bob,
				Sr25519Keyring::Charlie,
				Sr25519Keyring::Dave,
				Sr25519Keyring::Alice,
			])
		);

		assert_eq!(shared::ActiveValidatorKeys::<Test>::get(), validators);

		assert_eq!(
			shared::ActiveValidatorIndices::<Test>::get(),
			vec![
				ValidatorIndex(4),
				ValidatorIndex(1),
				ValidatorIndex(2),
				ValidatorIndex(3),
				ValidatorIndex(0),
			]
		);
	});
}

#[test]
fn sets_truncates_and_shuffles_validators() {
	let validators = vec![
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Ferdie,
	];

	let mut config = HostConfiguration::default();
	config.max_validators = Some(2);

	let pubkeys = validator_pubkeys(&validators);

	new_test_ext(MockGenesisConfig::default()).execute_with(|| {
		let validators = ParasShared::initializer_on_new_session(1, [1; 32], &config, pubkeys);

		assert_eq!(validators, validator_pubkeys(&[Sr25519Keyring::Ferdie, Sr25519Keyring::Bob,]));

		assert_eq!(shared::ActiveValidatorKeys::<Test>::get(), validators);

		assert_eq!(
			shared::ActiveValidatorIndices::<Test>::get(),
			vec![ValidatorIndex(4), ValidatorIndex(1),]
		);
	});
}
