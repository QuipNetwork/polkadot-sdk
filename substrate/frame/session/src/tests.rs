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

// Tests for the Session Pallet

use super::*;
use crate::mock::{
	authorities, before_session_end_called, create_set_keys_proof, force_new_session, new_test_ext,
	reset_before_session_end_called, session_changed, session_events_since_last_call, session_hold,
	set_next_validators, set_session_length, Balances, KeyDeposit, MockSessionKeys,
	PreUpgradeMockSessionKeys, RuntimeOrigin, Session, SessionChanged, System, Test,
	TestSessionChanged, TestValidatorIdOf, ValidatorAccounts,
};

use codec::Encode;
use sp_core::crypto::key_types::DUMMY;
use sp_runtime::{testing::UintAuthorityId, Perbill};

use frame_support::{
	assert_err, assert_noop, assert_ok,
	traits::{ConstU64, OnInitialize},
};

fn initialize_block(block: u64) {
	SessionChanged::mutate(|l| *l = false);
	System::set_block_number(block);
	Session::on_initialize(block);
}

#[test]
fn simple_setup_should_work() {
	new_test_ext().execute_with(|| {
		assert_eq!(authorities(), vec![UintAuthorityId(1), UintAuthorityId(2), UintAuthorityId(3)]);
		assert_eq!(Validators::<Test>::get(), vec![1, 2, 3]);
	});
}

#[test]
fn put_get_keys() {
	new_test_ext().execute_with(|| {
		Session::put_keys(&10, &UintAuthorityId(10).into());
		assert_eq!(Session::load_keys(&10), Some(UintAuthorityId(10).into()));
	})
}

#[test]
fn keys_cleared_on_kill() {
	let mut ext = new_test_ext();
	ext.execute_with(|| {
		assert_eq!(Validators::<Test>::get(), vec![1, 2, 3]);
		assert_eq!(Session::load_keys(&1), Some(UintAuthorityId(1).into()));

		let id = DUMMY;
		assert_eq!(Session::key_owner(id, UintAuthorityId(1).get_raw(id)), Some(1));

		assert!(System::is_provider_required(&1));
		assert_ok!(Session::purge_keys(RuntimeOrigin::signed(1)));
		assert!(!System::is_provider_required(&1));

		assert_eq!(Session::load_keys(&1), None);
		assert_eq!(Session::key_owner(id, UintAuthorityId(1).get_raw(id)), None);
	})
}

#[test]
fn purge_keys_works_for_stash_id() {
	let mut ext = new_test_ext();
	ext.execute_with(|| {
		assert_eq!(Validators::<Test>::get(), vec![1, 2, 3]);
		TestValidatorIdOf::set(vec![(10, 1), (20, 2), (3, 3)].into_iter().collect());
		assert_eq!(Session::load_keys(&1), Some(UintAuthorityId(1).into()));
		assert_eq!(Session::load_keys(&2), Some(UintAuthorityId(2).into()));

		let id = DUMMY;
		assert_eq!(Session::key_owner(id, UintAuthorityId(1).get_raw(id)), Some(1));

		assert_ok!(Session::purge_keys(RuntimeOrigin::signed(1)));
		assert_ok!(Session::purge_keys(RuntimeOrigin::signed(2)));

		assert_eq!(Session::load_keys(&1), None);
		assert_eq!(Session::load_keys(&2), None);
		assert_eq!(Session::key_owner(id, UintAuthorityId(10).get_raw(id)), None);
		assert_eq!(Session::key_owner(id, UintAuthorityId(20).get_raw(id)), None);
	})
}

#[test]
fn authorities_should_track_validators() {
	reset_before_session_end_called();

	new_test_ext().execute_with(|| {
		TestValidatorIdOf::set(vec![(1, 1), (2, 2), (3, 3), (4, 4)].into_iter().collect());

		set_next_validators(vec![1, 2]);
		force_new_session();
		initialize_block(1);
		assert_eq!(
			QueuedKeys::<Test>::get(),
			vec![(1, UintAuthorityId(1).into()), (2, UintAuthorityId(2).into()),]
		);
		assert_eq!(Validators::<Test>::get(), vec![1, 2, 3]);
		assert_eq!(authorities(), vec![UintAuthorityId(1), UintAuthorityId(2), UintAuthorityId(3)]);
		assert!(before_session_end_called());
		reset_before_session_end_called();

		force_new_session();
		initialize_block(2);
		assert_eq!(
			QueuedKeys::<Test>::get(),
			vec![(1, UintAuthorityId(1).into()), (2, UintAuthorityId(2).into()),]
		);
		assert_eq!(Validators::<Test>::get(), vec![1, 2]);
		assert_eq!(authorities(), vec![UintAuthorityId(1), UintAuthorityId(2)]);
		assert!(before_session_end_called());
		reset_before_session_end_called();

		set_next_validators(vec![1, 2, 4]);
		assert_ok!(Session::set_keys(
			RuntimeOrigin::signed(4),
			UintAuthorityId(4).into(),
			create_set_keys_proof(4, &UintAuthorityId(4)),
		));
		force_new_session();
		initialize_block(3);
		assert_eq!(
			QueuedKeys::<Test>::get(),
			vec![
				(1, UintAuthorityId(1).into()),
				(2, UintAuthorityId(2).into()),
				(4, UintAuthorityId(4).into()),
			]
		);
		assert_eq!(Validators::<Test>::get(), vec![1, 2]);
		assert_eq!(authorities(), vec![UintAuthorityId(1), UintAuthorityId(2)]);
		assert!(before_session_end_called());

		force_new_session();
		initialize_block(4);
		assert_eq!(
			QueuedKeys::<Test>::get(),
			vec![
				(1, UintAuthorityId(1).into()),
				(2, UintAuthorityId(2).into()),
				(4, UintAuthorityId(4).into()),
			]
		);
		assert_eq!(Validators::<Test>::get(), vec![1, 2, 4]);
		assert_eq!(authorities(), vec![UintAuthorityId(1), UintAuthorityId(2), UintAuthorityId(4)]);
	});
}

#[test]
fn should_work_with_early_exit() {
	new_test_ext().execute_with(|| {
		set_session_length(10);

		initialize_block(1);
		assert_eq!(CurrentIndex::<Test>::get(), 0);

		initialize_block(2);
		assert_eq!(CurrentIndex::<Test>::get(), 0);

		force_new_session();
		initialize_block(3);
		assert_eq!(CurrentIndex::<Test>::get(), 1);

		initialize_block(9);
		assert_eq!(CurrentIndex::<Test>::get(), 1);

		initialize_block(10);
		assert_eq!(CurrentIndex::<Test>::get(), 2);
	});
}

#[test]
fn session_change_should_work() {
	new_test_ext().execute_with(|| {
		// Block 1: No change
		initialize_block(1);
		assert_eq!(authorities(), vec![UintAuthorityId(1), UintAuthorityId(2), UintAuthorityId(3)]);
		assert_eq!(session_events_since_last_call(), vec![]);

		// Block 2: Session rollover, but no change.
		initialize_block(2);
		assert_eq!(authorities(), vec![UintAuthorityId(1), UintAuthorityId(2), UintAuthorityId(3)]);
		assert_eq!(
			session_events_since_last_call(),
			vec![Event::NewQueued, Event::NewSession { session_index: 1 }]
		);

		// Block 3: Set new key for validator 2; no visible change.
		initialize_block(3);
		assert_ok!(Session::set_keys(
			RuntimeOrigin::signed(2),
			UintAuthorityId(5).into(),
			create_set_keys_proof(2, &UintAuthorityId(5)),
		));
		assert_eq!(authorities(), vec![UintAuthorityId(1), UintAuthorityId(2), UintAuthorityId(3)]);
		assert_eq!(session_events_since_last_call(), vec![]);

		// Block 4: Session rollover; no visible change.
		initialize_block(4);
		assert_eq!(authorities(), vec![UintAuthorityId(1), UintAuthorityId(2), UintAuthorityId(3)]);
		assert_eq!(
			session_events_since_last_call(),
			vec![Event::NewQueued, Event::NewSession { session_index: 2 }]
		);

		// Block 5: No change.
		initialize_block(5);
		assert_eq!(authorities(), vec![UintAuthorityId(1), UintAuthorityId(2), UintAuthorityId(3)]);
		assert_eq!(session_events_since_last_call(), vec![]);

		// Block 6: Session rollover; authority 2 changes.
		initialize_block(6);
		assert_eq!(authorities(), vec![UintAuthorityId(1), UintAuthorityId(5), UintAuthorityId(3)]);
		assert_eq!(
			session_events_since_last_call(),
			vec![Event::NewQueued, Event::NewSession { session_index: 3 }]
		);
	});
}

#[test]
fn duplicates_are_not_allowed() {
	new_test_ext().execute_with(|| {
		TestValidatorIdOf::set(vec![(1, 1), (2, 2), (3, 3), (4, 4)].into_iter().collect());

		System::set_block_number(1);
		Session::on_initialize(1);
		assert_noop!(
			Session::set_keys(
				RuntimeOrigin::signed(4),
				UintAuthorityId(1).into(),
				create_set_keys_proof(4, &UintAuthorityId(1)),
			),
			Error::<Test>::DuplicatedKey,
		);
		assert_ok!(Session::set_keys(
			RuntimeOrigin::signed(1),
			UintAuthorityId(10).into(),
			create_set_keys_proof(1, &UintAuthorityId(10)),
		));

		// is fine now that 1 has migrated off.
		assert_ok!(Session::set_keys(
			RuntimeOrigin::signed(4),
			UintAuthorityId(1).into(),
			create_set_keys_proof(4, &UintAuthorityId(1)),
		));
	});
}

#[test]
fn session_changed_flag_works() {
	reset_before_session_end_called();

	new_test_ext().execute_with(|| {
		TestValidatorIdOf::set(vec![(1, 1), (2, 2), (3, 3), (69, 69)].into_iter().collect());
		TestSessionChanged::mutate(|l| *l = true);

		force_new_session();
		initialize_block(1);
		assert!(!session_changed());
		assert!(before_session_end_called());
		reset_before_session_end_called();

		force_new_session();
		initialize_block(2);
		assert!(!session_changed());
		assert!(before_session_end_called());
		reset_before_session_end_called();

		Session::disable_index(0);
		force_new_session();
		initialize_block(3);
		assert!(!session_changed());
		assert!(before_session_end_called());
		reset_before_session_end_called();

		force_new_session();
		initialize_block(4);
		assert!(session_changed());
		assert!(before_session_end_called());
		reset_before_session_end_called();

		force_new_session();
		initialize_block(5);
		assert!(!session_changed());
		assert!(before_session_end_called());
		reset_before_session_end_called();

		assert_ok!(Session::set_keys(
			RuntimeOrigin::signed(2),
			UintAuthorityId(5).into(),
			create_set_keys_proof(2, &UintAuthorityId(5)),
		));
		force_new_session();
		initialize_block(6);
		assert!(!session_changed());
		assert!(before_session_end_called());
		reset_before_session_end_called();

		// changing the keys of a validator leads to change.
		assert_ok!(Session::set_keys(
			RuntimeOrigin::signed(69),
			UintAuthorityId(69).into(),
			create_set_keys_proof(69, &UintAuthorityId(69)),
		));
		force_new_session();
		initialize_block(7);
		assert!(session_changed());
		assert!(before_session_end_called());
		reset_before_session_end_called();

		// while changing the keys of a non-validator does not.
		force_new_session();
		initialize_block(7);
		assert!(!session_changed());
		assert!(before_session_end_called());
		reset_before_session_end_called();
	});
}

#[test]
fn periodic_session_works() {
	type P = PeriodicSessions<ConstU64<10>, ConstU64<3>>;

	// make sure that offset phase behaves correctly
	for i in 0u64..3 {
		assert!(!P::should_end_session(i));
		assert_eq!(P::estimate_next_session_rotation(i).0.unwrap(), 3);

		// the last block of the session (i.e. the one before session rotation)
		// should have progress 100%.
		if P::estimate_next_session_rotation(i).0.unwrap() - 1 == i {
			assert_eq!(
				P::estimate_current_session_progress(i).0.unwrap(),
				Permill::from_percent(100)
			);
		} else {
			assert!(
				P::estimate_current_session_progress(i).0.unwrap() < Permill::from_percent(100)
			);
		}
	}

	// we end the session at block #3 and we consider this block the first one
	// from the next session. since we're past the offset phase it represents
	// 1/10 of progress.
	assert!(P::should_end_session(3u64));
	assert_eq!(P::estimate_next_session_rotation(3u64).0.unwrap(), 3);
	assert_eq!(P::estimate_current_session_progress(3u64).0.unwrap(), Permill::from_percent(10));

	for i in (1u64..10).map(|i| 3 + i) {
		assert!(!P::should_end_session(i));
		assert_eq!(P::estimate_next_session_rotation(i).0.unwrap(), 13);

		// as with the offset phase the last block of the session must have 100%
		// progress.
		if P::estimate_next_session_rotation(i).0.unwrap() - 1 == i {
			assert_eq!(
				P::estimate_current_session_progress(i).0.unwrap(),
				Permill::from_percent(100)
			);
		} else {
			assert!(
				P::estimate_current_session_progress(i).0.unwrap() < Permill::from_percent(100)
			);
		}
	}

	// the new session starts and we proceed in 1/10 increments.
	assert!(P::should_end_session(13u64));
	assert_eq!(P::estimate_next_session_rotation(13u64).0.unwrap(), 23);
	assert_eq!(P::estimate_current_session_progress(13u64).0.unwrap(), Permill::from_percent(10));

	assert!(!P::should_end_session(14u64));
	assert_eq!(P::estimate_next_session_rotation(14u64).0.unwrap(), 23);
	assert_eq!(P::estimate_current_session_progress(14u64).0.unwrap(), Permill::from_percent(20));
}

#[test]
fn session_keys_generate_output_works_as_set_keys_input() {
	new_test_ext().execute_with(|| {
		let new_keys = mock::MockSessionKeys::generate(&2u64.encode(), None);

		assert_ok!(Session::set_keys(
			RuntimeOrigin::signed(2),
			new_keys.keys,
			new_keys.proof.encode(),
		));
	});
}

#[test]
fn upgrade_keys() {
	use frame_support::storage;
	use sp_core::crypto::key_types::DUMMY;

	// This test assumes certain mocks.
	assert_eq!(mock::NextValidators::get().clone(), vec![1, 2, 3]);
	assert_eq!(mock::Validators::get().clone(), vec![1, 2, 3]);

	new_test_ext().execute_with(|| {
		let pre_one = PreUpgradeMockSessionKeys { a: [1u8; 32], b: [1u8; 64] };

		let pre_two = PreUpgradeMockSessionKeys { a: [2u8; 32], b: [2u8; 64] };

		let pre_three = PreUpgradeMockSessionKeys { a: [3u8; 32], b: [3u8; 64] };

		let val_keys = vec![(1u64, pre_one), (2u64, pre_two), (3u64, pre_three)];

		// Set `QueuedKeys`.
		{
			let storage_key = super::QueuedKeys::<Test>::hashed_key();
			assert!(storage::unhashed::exists(&storage_key));
			storage::unhashed::put(&storage_key, &val_keys);
		}

		// Set `NextKeys`.
		{
			for &(i, ref keys) in val_keys.iter() {
				let storage_key = super::NextKeys::<Test>::hashed_key_for(i);
				assert!(storage::unhashed::exists(&storage_key));
				storage::unhashed::put(&storage_key, keys);
			}
		}

		// Set `KeyOwner`.
		{
			for &(i, ref keys) in val_keys.iter() {
				// clear key owner for `UintAuthorityId` keys set in genesis.
				let presumed = UintAuthorityId(i);
				let raw_prev = presumed.as_ref();

				assert_eq!(Session::key_owner(DUMMY, raw_prev), Some(i));
				Session::clear_key_owner(DUMMY, raw_prev);

				Session::put_key_owner(mock::KEY_ID_A, keys.get_raw(mock::KEY_ID_A), &i);
				Session::put_key_owner(mock::KEY_ID_B, keys.get_raw(mock::KEY_ID_B), &i);
			}
		}

		// Do the upgrade and check sanity.
		let mock_keys_for = |val| mock::MockSessionKeys { dummy: UintAuthorityId(val) };
		Session::upgrade_keys::<PreUpgradeMockSessionKeys, _>(|val, _old_keys| mock_keys_for(val));

		// Check key ownership.
		for (i, ref keys) in val_keys.iter() {
			assert!(Session::key_owner(mock::KEY_ID_A, keys.get_raw(mock::KEY_ID_A)).is_none());
			assert!(Session::key_owner(mock::KEY_ID_B, keys.get_raw(mock::KEY_ID_B)).is_none());

			let migrated_key = UintAuthorityId(*i);
			assert_eq!(Session::key_owner(DUMMY, migrated_key.as_ref()), Some(*i));
		}

		// Check queued keys.
		assert_eq!(
			QueuedKeys::<Test>::get(),
			vec![(1, mock_keys_for(1)), (2, mock_keys_for(2)), (3, mock_keys_for(3)),],
		);

		for i in 1u64..4 {
			assert_eq!(super::NextKeys::<Test>::get(&i), Some(mock_keys_for(i)));
		}
	})
}

#[cfg(feature = "historical")]
#[test]
fn test_migration_v1() {
	use crate::{
		historical::{HistoricalSessions, StoredRange},
		mock::Historical,
	};
	use frame_support::traits::{PalletInfoAccess, StorageVersion};

	new_test_ext().execute_with(|| {
		assert!(HistoricalSessions::<Test>::iter_values().count() > 0);
		assert!(StoredRange::<Test>::exists());

		let old_pallet = "Session";
		let new_pallet = <Historical as PalletInfoAccess>::name();
		frame_support::storage::migration::move_pallet(
			new_pallet.as_bytes(),
			old_pallet.as_bytes(),
		);
		StorageVersion::new(0).put::<Historical>();

		crate::migrations::historical::pre_migrate::<Test, Historical>();
		crate::migrations::historical::migrate::<Test, Historical>();
		crate::migrations::historical::post_migrate::<Test, Historical>();
	});
}

#[test]
fn set_keys_should_fail_with_insufficient_funds() {
	new_test_ext().execute_with(|| {
		// Account 999 is mocked to have KeyDeposit -1
		let account_id = 999;
		let keys = MockSessionKeys { dummy: UintAuthorityId(account_id).into() };
		frame_system::Pallet::<Test>::inc_providers(&account_id);
		// Make sure we have a validator ID
		ValidatorAccounts::mutate(|m| {
			m.insert(account_id, account_id);
		});

		// Attempt to set keys with an account that has insufficient funds
		// Should fail with Err(Token(FundsUnavailable)) from `pallet-balances`
		assert_err!(
			Session::set_keys(
				RuntimeOrigin::signed(account_id),
				keys,
				create_set_keys_proof(account_id, &UintAuthorityId(account_id)),
			),
			sp_runtime::TokenError::FundsUnavailable
		);
	});
}

#[test]
fn set_keys_should_hold_funds() {
	new_test_ext().execute_with(|| {
		// Account 1000 is mocked to have sufficient funds
		let account_id = 1000;
		let keys = MockSessionKeys { dummy: UintAuthorityId(account_id).into() };
		let deposit = KeyDeposit::get();

		// Make sure we have a validator ID
		ValidatorAccounts::mutate(|m| {
			m.insert(account_id, account_id);
		});

		// Set keys and check the operation succeeds
		let res = Session::set_keys(
			RuntimeOrigin::signed(account_id),
			keys,
			create_set_keys_proof(account_id, &UintAuthorityId(account_id)),
		);
		assert_ok!(res);

		// Check that the funds are held
		assert_eq!(session_hold(account_id), deposit);
	});
}

#[test]
fn purge_keys_should_unhold_funds() {
	new_test_ext().execute_with(|| {
		// Account 1000 is mocked to have sufficient funds
		let account_id = 1000;
		let keys = MockSessionKeys { dummy: UintAuthorityId(account_id).into() };
		let deposit = KeyDeposit::get();

		// Make sure we have a validator ID
		ValidatorAccounts::mutate(|m| {
			m.insert(account_id, account_id);
		});

		// Ensure system providers are properly set for the test account
		frame_system::Pallet::<Test>::inc_providers(&account_id);

		// First set the keys to reserve the deposit
		let res = Session::set_keys(
			RuntimeOrigin::signed(account_id),
			keys,
			create_set_keys_proof(account_id, &UintAuthorityId(account_id)),
		);
		assert_ok!(res);

		// Check the reserved balance after setting keys
		let reserved_balance_before_purge = Balances::reserved_balance(&account_id);
		assert!(
			reserved_balance_before_purge >= deposit,
			"Deposit should be reserved after setting keys"
		);

		// Now purge the keys
		let res = Session::purge_keys(RuntimeOrigin::signed(account_id));
		assert_ok!(res);

		// Check that the funds were unreserved
		let reserved_balance_after_purge = Balances::reserved_balance(&account_id);
		assert_eq!(reserved_balance_after_purge, reserved_balance_before_purge - deposit);
	});
}

#[test]
fn existing_validators_without_hold_are_except() {
	// upon addition of `SessionDeposit`, a runtime may have some old validators without any held
	// amount. They can freely still update their session keys. They can also purge them.

	// disable key deposit for initial validators
	KeyDeposit::set(0);
	new_test_ext().execute_with(|| {
		// reset back to the first value.
		KeyDeposit::set(10);
		// 1 is an initial validator
		assert_eq!(session_hold(1), 0);

		// upgrade 1's keys
		assert_ok!(Session::set_keys(
			RuntimeOrigin::signed(1),
			UintAuthorityId(7).into(),
			create_set_keys_proof(1, &UintAuthorityId(7))
		));
		assert_eq!(session_hold(1), 0);

		// purge 1's keys
		assert_ok!(Session::purge_keys(RuntimeOrigin::signed(1)));
		assert_eq!(session_hold(1), 0);
	});
}

#[cfg(feature = "historical")]
mod externally_set_keys_tracking {
	use super::*;

	const ACCOUNT: u64 = 1000;

	fn setup_account() {
		frame_system::Pallet::<Test>::inc_providers(&ACCOUNT);
		ValidatorAccounts::mutate(|m| {
			m.insert(ACCOUNT, ACCOUNT);
		});
	}

	fn set_local(key: u64) {
		let keys = UintAuthorityId(key).into();
		let proof = create_set_keys_proof(ACCOUNT, &UintAuthorityId(key));
		assert_ok!(Session::set_keys(RuntimeOrigin::signed(ACCOUNT), keys, proof));
	}

	fn set_remote(key: u64) {
		<Session as SessionInterface>::set_keys(&ACCOUNT, UintAuthorityId(key).into()).unwrap();
	}

	fn purge_local() {
		assert_ok!(Session::purge_keys(RuntimeOrigin::signed(ACCOUNT)));
	}

	fn purge_remote() {
		<Session as SessionInterface>::purge_keys(&ACCOUNT).unwrap();
	}

	fn assert_local_state(consumers_before: u32) {
		assert!(!ExternallySetKeys::<Test>::contains_key(&ACCOUNT));
		// +1 from session's inc_consumers, +1 from pallet-balances hold.
		assert_eq!(System::consumers(&ACCOUNT), consumers_before + 2);
		assert_eq!(session_hold(ACCOUNT), KeyDeposit::get());
	}

	fn assert_remote_state(consumers_before: u32) {
		assert!(ExternallySetKeys::<Test>::contains_key(&ACCOUNT));
		assert_eq!(System::consumers(&ACCOUNT), consumers_before);
		assert_eq!(session_hold(ACCOUNT), 0);
	}

	fn assert_clean_state(consumers_before: u32) {
		assert!(!ExternallySetKeys::<Test>::contains_key(&ACCOUNT));
		assert_eq!(System::consumers(&ACCOUNT), consumers_before);
		assert_eq!(session_hold(ACCOUNT), 0);
	}

	#[test]
	fn set_local_purge_local() {
		new_test_ext().execute_with(|| {
			setup_account();
			let consumers_before = System::consumers(&ACCOUNT);

			set_local(ACCOUNT);
			assert_local_state(consumers_before);

			purge_local();
			assert_clean_state(consumers_before);
		});
	}

	#[test]
	fn set_local_purge_remote() {
		new_test_ext().execute_with(|| {
			setup_account();
			let consumers_before = System::consumers(&ACCOUNT);

			set_local(ACCOUNT);
			assert_local_state(consumers_before);

			purge_remote();
			assert_clean_state(consumers_before);
		});
	}

	#[test]
	fn set_remote_purge_local() {
		new_test_ext().execute_with(|| {
			setup_account();
			let consumers_before = System::consumers(&ACCOUNT);

			set_remote(ACCOUNT);
			assert_remote_state(consumers_before);

			purge_local();
			assert_clean_state(consumers_before);
		});
	}

	#[test]
	fn set_remote_purge_remote() {
		new_test_ext().execute_with(|| {
			setup_account();
			let consumers_before = System::consumers(&ACCOUNT);

			set_remote(ACCOUNT);
			assert_remote_state(consumers_before);

			purge_remote();
			assert_clean_state(consumers_before);
		});
	}

	#[test]
	fn set_local_to_remote() {
		new_test_ext().execute_with(|| {
			setup_account();
			let consumers_before = System::consumers(&ACCOUNT);

			set_local(ACCOUNT);
			assert_local_state(consumers_before);

			// Transition to remote: deposit released, consumer decremented.
			set_remote(70);
			assert_remote_state(consumers_before);
		});
	}

	#[test]
	fn set_remote_to_local() {
		new_test_ext().execute_with(|| {
			setup_account();
			let consumers_before = System::consumers(&ACCOUNT);

			set_remote(ACCOUNT);
			assert_remote_state(consumers_before);

			// Transition to local: deposit placed, consumer incremented.
			set_local(70);
			assert_local_state(consumers_before);
		});
	}

	#[test]
	fn key_rotation_succeeds_at_max_consumers() {
		new_test_ext().execute_with(|| {
			// Given: an account with existing locally-managed session keys.
			setup_account();
			set_local(ACCOUNT);

			// Saturate the consumer count so `can_inc_consumer` returns false.
			while frame_system::Pallet::<Test>::can_inc_consumer(&ACCOUNT) {
				frame_system::Pallet::<Test>::inc_consumers(&ACCOUNT).unwrap();
			}
			assert!(
				!frame_system::Pallet::<Test>::can_inc_consumer(&ACCOUNT),
				"pre-condition: consumer slots exhausted"
			);

			// When: the validator rotates keys (not a first-time registration).
			let new_key = ACCOUNT + 1;
			let keys = UintAuthorityId(new_key).into();
			let proof = create_set_keys_proof(ACCOUNT, &UintAuthorityId(new_key));

			// Then: the rotation succeeds because it does not need a new consumer ref.
			assert_ok!(Session::set_keys(RuntimeOrigin::signed(ACCOUNT), keys, proof));
		});
	}

	#[test]
	fn first_registration_fails_at_max_consumers() {
		new_test_ext().execute_with(|| {
			// Given: a fresh account with no session keys and exhausted consumer slots.
			const FRESH_ACCOUNT: u64 = 2000;
			frame_system::Pallet::<Test>::inc_providers(&FRESH_ACCOUNT);
			ValidatorAccounts::mutate(|m| {
				m.insert(FRESH_ACCOUNT, FRESH_ACCOUNT);
			});
			while frame_system::Pallet::<Test>::can_inc_consumer(&FRESH_ACCOUNT) {
				frame_system::Pallet::<Test>::inc_consumers(&FRESH_ACCOUNT).unwrap();
			}

			// When/Then: first-time registration is rejected.
			let keys = UintAuthorityId(FRESH_ACCOUNT).into();
			let proof = create_set_keys_proof(FRESH_ACCOUNT, &UintAuthorityId(FRESH_ACCOUNT));
			assert_noop!(
				Session::set_keys(RuntimeOrigin::signed(FRESH_ACCOUNT), keys, proof),
				Error::<Test>::NoAccount,
			);
		});
	}

	#[test]
	fn external_to_local_transition_fails_at_max_consumers() {
		new_test_ext().execute_with(|| {
			// Given: an account with externally-set keys and exhausted consumer slots.
			setup_account();
			set_remote(ACCOUNT);
			while frame_system::Pallet::<Test>::can_inc_consumer(&ACCOUNT) {
				frame_system::Pallet::<Test>::inc_consumers(&ACCOUNT).unwrap();
			}

			// When/Then: transitioning from external to local is rejected because it
			// needs a new consumer reference.
			let keys = UintAuthorityId(ACCOUNT + 1).into();
			let proof = create_set_keys_proof(ACCOUNT, &UintAuthorityId(ACCOUNT + 1));
			assert_noop!(
				Session::set_keys(RuntimeOrigin::signed(ACCOUNT), keys, proof),
				Error::<Test>::NoAccount,
			);
		});
	}
}

mod disabling_byzantine_threshold {
	use super::*;
	use crate::disabling::{DisablingStrategy, UpToLimitDisablingStrategy};
	use sp_staking::offence::OffenceSeverity;

	// Common test data - the stash of the offending validator, the era of the offence and the
	// active set
	const OFFENDER_ID: <Test as frame_system::Config>::AccountId = 7;
	const MAX_OFFENDER_SEVERITY: OffenceSeverity = OffenceSeverity(Perbill::from_percent(100));
	const MIN_OFFENDER_SEVERITY: OffenceSeverity = OffenceSeverity(Perbill::from_percent(0));
	const ACTIVE_SET: [<Test as Config>::ValidatorId; 7] = [1, 2, 3, 4, 5, 6, 7];
	const OFFENDER_VALIDATOR_IDX: u32 = 6;

	#[test]
	fn disable_when_below_byzantine_threshold() {
		sp_io::TestExternalities::default().execute_with(|| {
			let initially_disabled = vec![(1, MAX_OFFENDER_SEVERITY)];
			Validators::<Test>::put(ACTIVE_SET.to_vec());

			let disabling_decision =
				<UpToLimitDisablingStrategy as DisablingStrategy<Test>>::decision(
					&OFFENDER_ID,
					MAX_OFFENDER_SEVERITY,
					&initially_disabled,
				);

			assert_eq!(disabling_decision.disable, Some(OFFENDER_VALIDATOR_IDX));
		});
	}

	#[test]
	fn disable_when_below_custom_byzantine_threshold() {
		sp_io::TestExternalities::default().execute_with(|| {
			let initially_disabled = vec![(1, MAX_OFFENDER_SEVERITY), (2, MAX_OFFENDER_SEVERITY)];
			Validators::<Test>::put(ACTIVE_SET.to_vec());

			let disabling_decision =
				<UpToLimitDisablingStrategy<2> as DisablingStrategy<Test>>::decision(
					&OFFENDER_ID,
					MAX_OFFENDER_SEVERITY,
					&initially_disabled,
				);

			assert_eq!(disabling_decision.disable, Some(OFFENDER_VALIDATOR_IDX));
		});
	}

	#[test]
	fn non_slashable_offences_still_disable() {
		sp_io::TestExternalities::default().execute_with(|| {
			let initially_disabled = vec![(1, MAX_OFFENDER_SEVERITY)];
			Validators::<Test>::put(ACTIVE_SET.to_vec());

			let disabling_decision =
				<UpToLimitDisablingStrategy as DisablingStrategy<Test>>::decision(
					&OFFENDER_ID,
					OffenceSeverity(Perbill::from_percent(0)),
					&initially_disabled,
				);

			assert_eq!(disabling_decision.disable, Some(OFFENDER_VALIDATOR_IDX));
		});
	}

	#[test]
	fn dont_disable_beyond_byzantine_threshold() {
		sp_io::TestExternalities::default().execute_with(|| {
			let initially_disabled = vec![(1, MIN_OFFENDER_SEVERITY), (2, MAX_OFFENDER_SEVERITY)];
			Validators::<Test>::put(ACTIVE_SET.to_vec());
			let disabling_decision =
				<UpToLimitDisablingStrategy as DisablingStrategy<Test>>::decision(
					&OFFENDER_ID,
					MAX_OFFENDER_SEVERITY,
					&initially_disabled,
				);

			assert!(disabling_decision.disable.is_none() && disabling_decision.reenable.is_none());
		});
	}
}

mod disabling_with_reenabling {
	use super::*;
	use crate::disabling::{DisablingStrategy, UpToLimitWithReEnablingDisablingStrategy};
	use sp_staking::offence::OffenceSeverity;

	// Common test data - the stash of the offending validator, the era of the offence and the
	// active set
	const OFFENDER_ID: <Test as frame_system::Config>::AccountId = 7;
	const MAX_OFFENDER_SEVERITY: OffenceSeverity = OffenceSeverity(Perbill::from_percent(100));
	const LOW_OFFENDER_SEVERITY: OffenceSeverity = OffenceSeverity(Perbill::from_percent(0));
	const ACTIVE_SET: [<Test as Config>::ValidatorId; 7] = [1, 2, 3, 4, 5, 6, 7];
	const OFFENDER_VALIDATOR_IDX: u32 = 6; // the offender is with index 6 in the active set

	#[test]
	fn disable_when_below_byzantine_threshold() {
		sp_io::TestExternalities::default().execute_with(|| {
			let initially_disabled = vec![(0, MAX_OFFENDER_SEVERITY)];
			Validators::<Test>::put(ACTIVE_SET.to_vec());

			let disabling_decision =
				<UpToLimitWithReEnablingDisablingStrategy as DisablingStrategy<Test>>::decision(
					&OFFENDER_ID,
					MAX_OFFENDER_SEVERITY,
					&initially_disabled,
				);

			// Disable Offender and do not re-enable anyone
			assert_eq!(disabling_decision.disable, Some(OFFENDER_VALIDATOR_IDX));
			assert_eq!(disabling_decision.reenable, None);
		});
	}

	#[test]
	fn reenable_arbitrary_on_equal_severity() {
		sp_io::TestExternalities::default().execute_with(|| {
			let initially_disabled = vec![(0, MAX_OFFENDER_SEVERITY), (1, MAX_OFFENDER_SEVERITY)];
			Validators::<Test>::put(ACTIVE_SET.to_vec());

			let disabling_decision =
				<UpToLimitWithReEnablingDisablingStrategy as DisablingStrategy<Test>>::decision(
					&OFFENDER_ID,
					MAX_OFFENDER_SEVERITY,
					&initially_disabled,
				);

			assert!(disabling_decision.disable.is_some() && disabling_decision.reenable.is_some());
			// Disable 7 and enable 1
			assert_eq!(disabling_decision.disable.unwrap(), OFFENDER_VALIDATOR_IDX);
			assert_eq!(disabling_decision.reenable.unwrap(), 0);
		});
	}

	#[test]
	fn do_not_reenable_higher_offenders() {
		sp_io::TestExternalities::default().execute_with(|| {
			let initially_disabled = vec![(0, MAX_OFFENDER_SEVERITY), (1, MAX_OFFENDER_SEVERITY)];
			Validators::<Test>::put(ACTIVE_SET.to_vec());

			let disabling_decision =
				<UpToLimitWithReEnablingDisablingStrategy as DisablingStrategy<Test>>::decision(
					&OFFENDER_ID,
					LOW_OFFENDER_SEVERITY,
					&initially_disabled,
				);

			assert!(disabling_decision.disable.is_none() && disabling_decision.reenable.is_none());

			assert_ok!(Session::do_try_state());
		});
	}

	#[test]
	fn reenable_lower_offenders() {
		sp_io::TestExternalities::default().execute_with(|| {
			let initially_disabled = vec![(0, LOW_OFFENDER_SEVERITY), (1, LOW_OFFENDER_SEVERITY)];
			Validators::<Test>::put(ACTIVE_SET.to_vec());

			let disabling_decision =
				<UpToLimitWithReEnablingDisablingStrategy as DisablingStrategy<Test>>::decision(
					&OFFENDER_ID,
					MAX_OFFENDER_SEVERITY,
					&initially_disabled,
				);

			assert!(disabling_decision.disable.is_some() && disabling_decision.reenable.is_some());
			// Disable 7 and enable 1
			assert_eq!(disabling_decision.disable.unwrap(), OFFENDER_VALIDATOR_IDX);
			assert_eq!(disabling_decision.reenable.unwrap(), 0);

			assert_ok!(Session::do_try_state());
		});
	}

	#[test]
	fn reenable_lower_offenders_unordered() {
		sp_io::TestExternalities::default().execute_with(|| {
			let initially_disabled = vec![(0, MAX_OFFENDER_SEVERITY), (1, LOW_OFFENDER_SEVERITY)];
			Validators::<Test>::put(ACTIVE_SET.to_vec());

			let disabling_decision =
				<UpToLimitWithReEnablingDisablingStrategy as DisablingStrategy<Test>>::decision(
					&OFFENDER_ID,
					MAX_OFFENDER_SEVERITY,
					&initially_disabled,
				);

			assert!(disabling_decision.disable.is_some() && disabling_decision.reenable.is_some());
			// Disable 7 and enable 1
			assert_eq!(disabling_decision.disable.unwrap(), OFFENDER_VALIDATOR_IDX);
			assert_eq!(disabling_decision.reenable.unwrap(), 1);
		});
	}

	#[test]
	fn update_severity() {
		sp_io::TestExternalities::default().execute_with(|| {
			let initially_disabled =
				vec![(OFFENDER_VALIDATOR_IDX, LOW_OFFENDER_SEVERITY), (0, MAX_OFFENDER_SEVERITY)];
			Validators::<Test>::put(ACTIVE_SET.to_vec());

			let disabling_decision =
				<UpToLimitWithReEnablingDisablingStrategy as DisablingStrategy<Test>>::decision(
					&OFFENDER_ID,
					MAX_OFFENDER_SEVERITY,
					&initially_disabled,
				);

			assert!(disabling_decision.disable.is_some() && disabling_decision.reenable.is_none());
			// Disable 7 "again" AKA update their severity
			assert_eq!(disabling_decision.disable.unwrap(), OFFENDER_VALIDATOR_IDX);
		});
	}

	#[test]
	fn update_cannot_lower_severity() {
		sp_io::TestExternalities::default().execute_with(|| {
			let initially_disabled =
				vec![(OFFENDER_VALIDATOR_IDX, MAX_OFFENDER_SEVERITY), (0, MAX_OFFENDER_SEVERITY)];
			Validators::<Test>::put(ACTIVE_SET.to_vec());

			let disabling_decision =
				<UpToLimitWithReEnablingDisablingStrategy as DisablingStrategy<Test>>::decision(
					&OFFENDER_ID,
					LOW_OFFENDER_SEVERITY,
					&initially_disabled,
				);

			assert!(disabling_decision.disable.is_none() && disabling_decision.reenable.is_none());
		});
	}

	#[test]
	fn no_accidental_reenablement_on_repeated_offence() {
		sp_io::TestExternalities::default().execute_with(|| {
			let initially_disabled =
				vec![(OFFENDER_VALIDATOR_IDX, MAX_OFFENDER_SEVERITY), (0, LOW_OFFENDER_SEVERITY)];
			Validators::<Test>::put(ACTIVE_SET.to_vec());

			let disabling_decision =
				<UpToLimitWithReEnablingDisablingStrategy as DisablingStrategy<Test>>::decision(
					&OFFENDER_ID,
					MAX_OFFENDER_SEVERITY,
					&initially_disabled,
				);

			assert!(disabling_decision.disable.is_none() && disabling_decision.reenable.is_none());
		});
	}
}
