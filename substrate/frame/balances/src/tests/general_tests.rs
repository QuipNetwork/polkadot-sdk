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

#![cfg(test)]

use crate::{
	system::AccountInfo,
	tests::{
		ensure_ti_valid, get_test_account, get_test_account_data, Balances, ExtBuilder, System,
		Test, TestId, UseSystem,
	},
	AccountData, ExtraFlags, TotalIssuance,
};
use frame_support::{
	assert_noop, assert_ok, hypothetically,
	traits::{
		fungible::{self, InspectFreeze, Mutate, MutateFreeze, MutateHold},
		tokens::{Fortitude, Precision, Preservation},
		LockableCurrency, WithdrawReasons,
	},
};
use sp_runtime::DispatchError;

/// There are some accounts that have one consumer ref too few. These accounts are at risk of losing
/// their held (reserved) balance. They do not just lose it - it is also not accounted for in the
/// Total Issuance. Here we test the case that the account does not reap in such a case, but gets
/// one consumer ref for its reserved balance.
#[test]
fn regression_historic_acc_does_not_evaporate_reserve() {
	ExtBuilder::default().build_and_execute_with(|| {
		UseSystem::set(true);
		let (alice, bob) = (0, 1);
		// Alice is in a bad state with consumer == 0 && reserved > 0:
		Balances::set_balance(&alice, 100);
		TotalIssuance::<Test>::put(100);
		ensure_ti_valid();

		assert_ok!(Balances::hold(&TestId::Foo, &alice, 10));
		// This is the issue of the account:
		System::dec_consumers(&alice);

		assert_eq!(
			get_test_account(alice),
			AccountInfo {
				data: AccountData {
					free: 90,
					reserved: 10,
					frozen: 0,
					flags: ExtraFlags(1u128 << 127),
				},
				nonce: 0,
				consumers: 0, // should be 1 on a good acc
				providers: 1,
				sufficients: 0,
			}
		);

		ensure_ti_valid();

		// Reaping the account is prevented by the new logic:
		assert_noop!(
			Balances::transfer_allow_death(Some(alice).into(), bob, 90),
			DispatchError::ConsumerRemaining
		);
		assert_noop!(
			Balances::transfer_all(Some(alice).into(), bob, false),
			DispatchError::ConsumerRemaining
		);

		// normal transfers still work:
		hypothetically!({
			assert_ok!(Balances::transfer_keep_alive(Some(alice).into(), bob, 40));
			// Alice got back her consumer ref:
			assert_eq!(System::consumers(&alice), 1);
			ensure_ti_valid();
		});
		hypothetically!({
			assert_ok!(Balances::transfer_all(Some(alice).into(), bob, true));
			// Alice got back her consumer ref:
			assert_eq!(System::consumers(&alice), 1);
			ensure_ti_valid();
		});

		// un-reserving all does not add a consumer ref:
		hypothetically!({
			assert_ok!(Balances::release(&TestId::Foo, &alice, 10, Precision::Exact));
			assert_eq!(System::consumers(&alice), 0);
			assert_ok!(Balances::transfer_keep_alive(Some(alice).into(), bob, 40));
			assert_eq!(System::consumers(&alice), 0);
			ensure_ti_valid();
		});
		// un-reserving some does add a consumer ref:
		hypothetically!({
			assert_ok!(Balances::release(&TestId::Foo, &alice, 5, Precision::Exact));
			assert_eq!(System::consumers(&alice), 1);
			assert_ok!(Balances::transfer_keep_alive(Some(alice).into(), bob, 40));
			assert_eq!(System::consumers(&alice), 1);
			ensure_ti_valid();
		});
	});
}

/// Releasing the last hold from an account dusted below the ED must not leak the
/// dust from `TotalIssuance` (`set_balance_on_hold` discards `maybe_dust`).
#[test]
fn regression_release_hold_below_ed_leaks_issuance() {
	ExtBuilder::default().existential_deposit(10).build_and_execute_with(|| {
		UseSystem::set(true);

		let (alice, bob) = (0, 1);
		Balances::set_balance(&alice, 100);
		TotalIssuance::<Test>::put(100);

		let _ = System::inc_providers(&alice);
		assert_ok!(Balances::hold(&TestId::Foo, &alice, 50));

		// free -> 5 (< ED), kept alive by the reserve.
		assert_ok!(Balances::transfer_allow_death(Some(alice).into(), bob, 45));
		assert_eq!(Balances::free_balance(alice), 5);

		// The hold is reduced before free balance is credited, so the intermediate state
		// (free = 5, reserved = 0) dusts the 5 free units.
		assert_ok!(Balances::release(&TestId::Foo, &alice, 50, Precision::Exact));
		assert_eq!(Balances::free_balance(alice), 50);

		ensure_ti_valid();
	});
}

/// Dusting a frozen account (via a `Force` slash) must not desync `Freezes` from
/// the account's `frozen` field.
#[test]
fn regression_force_withdraw_dusting_frozen_account_desyncs_freezes() {
	ExtBuilder::default().existential_deposit(10).build_and_execute_with(|| {
		UseSystem::set(true);
		let alice = 0;

		Balances::set_balance(&alice, 100);
		let _ = System::inc_providers(&alice); // survives being dusted below ED
		assert_ok!(Balances::set_freeze(&TestId::Foo, &alice, 30));

		// `Force` ignores the freeze: free drops to 5 (< ED), dusting the account.
		let credit = <Balances as fungible::Balanced<_>>::withdraw(
			&alice,
			95,
			Precision::Exact,
			Preservation::Expendable,
			Fortitude::Force,
		)
		.unwrap();
		drop(credit);

		assert!(
			Balances::balance_frozen(&TestId::Foo, &alice) <= get_test_account_data(alice).frozen
		);
	});
}

/// Same freeze desync as above, but reachable by an unprivileged
/// `transfer_allow_death`: a freeze smaller than the ED lets a plain transfer
/// push free below the ED (but above the freeze) and dust the account.
#[test]
fn regression_transfer_allow_death_dusting_frozen_account_desyncs_freezes() {
	ExtBuilder::default().existential_deposit(10).build_and_execute_with(|| {
		UseSystem::set(true);
		let (alice, bob) = (0, 1);

		Balances::set_balance(&alice, 100);
		let _ = System::inc_providers(&alice); // survives being dusted below ED
		assert_ok!(Balances::set_freeze(&TestId::Foo, &alice, 5));

		// Leaves free = 7: below ED (10) but at/above the freeze (5), so allowed.
		assert_ok!(Balances::transfer_allow_death(Some(alice).into(), bob, 93));

		assert!(
			Balances::balance_frozen(&TestId::Foo, &alice) <= get_test_account_data(alice).frozen
		);
	});
}

/// Thawing the last freeze of an account dusted below the ED must not leak the
/// dust from `TotalIssuance` (`update_freezes` discards `maybe_dust`).
#[test]
fn regression_thaw_dusting_sub_ed_account_leaks_issuance() {
	ExtBuilder::default().existential_deposit(10).build_and_execute_with(|| {
		UseSystem::set(true);
		let alice = 0;

		Balances::set_balance(&alice, 100);
		TotalIssuance::<Test>::put(100);
		// This provider makes the withdraw below work
		let _ = System::inc_providers(&alice);
		assert_ok!(Balances::set_freeze(&TestId::Foo, &alice, 30));

		// `Force` slash ignores the freeze: free -> 5 (< ED), kept alive by the freeze.
		let credit = <Balances as fungible::Balanced<_>>::withdraw(
			&alice,
			95,
			Precision::Exact,
			Preservation::Expendable,
			Fortitude::Force,
		)
		.unwrap();
		drop(credit);

		// Thawing the last freeze dusts the 5 free units; the dust must be burned.
		assert_ok!(Balances::thaw(&TestId::Foo, &alice));
		ensure_ti_valid();
	});
}

/// Removing the last lock from an account dusted below the ED must not leak the
/// dust from `TotalIssuance` (`update_locks` must handle `maybe_dust`).
#[test]
fn regression_remove_lock_dusting_sub_ed_account_leaks_issuance() {
	ExtBuilder::default().existential_deposit(10).build_and_execute_with(|| {
		UseSystem::set(true);
		let (alice, bob) = (0, 1);

		Balances::set_balance(&alice, 100);
		TotalIssuance::<Test>::put(100);
		let _ = System::inc_providers(&alice);
		Balances::set_lock(*b"locktest", &alice, 5, WithdrawReasons::all());

		// Leaves free = 7: below ED, but kept alive by the lock.
		assert_ok!(Balances::transfer_allow_death(Some(alice).into(), bob, 93));

		// Removing the last lock dusts the 7 free units; the dust must be burned.
		Balances::remove_lock(*b"locktest", &alice);
		ensure_ti_valid();
	});
}

#[cfg(feature = "try-runtime")]
#[test]
fn try_state_works() {
	use crate::{Config, Freezes, Holds};
	use frame_support::{
		storage,
		traits::{Get, Hooks, VariantCount},
	};

	ExtBuilder::default().auto_try_state(false).build_and_execute_with(|| {
		storage::unhashed::put(
			&Holds::<Test>::hashed_key_for(1),
			&vec![0u8; <Test as Config>::RuntimeHoldReason::VARIANT_COUNT as usize + 1],
		);

		assert!(format!("{:?}", Balances::try_state(0).unwrap_err())
			.contains("Found `Hold` with too many elements"));
	});

	ExtBuilder::default().auto_try_state(false).build_and_execute_with(|| {
		let max_freezes: u32 = <Test as Config>::MaxFreezes::get();

		storage::unhashed::put(
			&Freezes::<Test>::hashed_key_for(1),
			&vec![0u8; max_freezes as usize + 1],
		);

		assert!(format!("{:?}", Balances::try_state(0).unwrap_err())
			.contains("Found `Freeze` with too many elements"));
	});
}

/// Releasing a hold while the free balance is zero must not reset the frozen balance.
#[test]
fn release_hold_with_zero_free_balance_keeps_frozen() {
	ExtBuilder::default().existential_deposit(10).build_and_execute_with(|| {
		UseSystem::set(true);
		let (alice, bob) = (0, 1);

		Balances::set_balance(&alice, 1010);
		TotalIssuance::<Test>::put(1010);
		let _ = System::inc_providers(&alice);
		Balances::set_lock(*b"locktest", &alice, 1000, WithdrawReasons::all());
		assert_ok!(Balances::hold(&TestId::Foo, &alice, 1000));

		// Leaves free = 0: kept alive by the extra provider.
		assert_ok!(Balances::transfer_allow_death(Some(alice).into(), bob, 10));
		assert_eq!(get_test_account_data(alice).free, 0);

		assert_ok!(Balances::release(&TestId::Foo, &alice, 1000, Precision::Exact));

		let data = get_test_account_data(alice);
		assert_eq!((data.free, data.reserved, data.frozen), (1000, 0, 1000));
		assert_noop!(
			Balances::transfer_allow_death(Some(alice).into(), bob, 999),
			sp_runtime::TokenError::Frozen
		);
		ensure_ti_valid();
	});
}
