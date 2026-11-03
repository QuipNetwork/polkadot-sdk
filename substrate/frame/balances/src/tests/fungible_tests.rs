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

//! Tests regarding the functionality of the `fungible` trait set implementations.

use super::*;
use frame_support::traits::{
	tokens::{
		Fortitude::{Force, Polite},
		Precision::{BestEffort, Exact},
		Preservation::{Expendable, Preserve, Protect},
		Restriction::Free,
	},
	Consideration, Footprint, LinearStoragePrice, MaybeConsideration,
};
use fungible::{
	FreezeConsideration, HoldConsideration, Inspect, InspectFreeze, InspectHold,
	LoneFreezeConsideration, LoneHoldConsideration, Mutate, MutateFreeze, MutateHold, Unbalanced,
};
use sp_core::ConstU64;

#[test]
fn inspect_trait_reducible_balance_basic_works() {
	ExtBuilder::default().existential_deposit(10).build_and_execute_with(|| {
		Balances::set_balance(&1, 100);
		assert_eq!(Balances::reducible_balance(&1, Expendable, Polite), 100);
		assert_eq!(Balances::reducible_balance(&1, Protect, Polite), 90);
		assert_eq!(Balances::reducible_balance(&1, Preserve, Polite), 90);
		assert_eq!(Balances::reducible_balance(&1, Expendable, Force), 100);
		assert_eq!(Balances::reducible_balance(&1, Protect, Force), 90);
		assert_eq!(Balances::reducible_balance(&1, Preserve, Force), 90);
	});
}

#[test]
fn inspect_trait_reducible_balance_other_provide_works() {
	ExtBuilder::default().existential_deposit(10).build_and_execute_with(|| {
		Balances::set_balance(&1, 100);
		System::inc_providers(&1);
		assert_eq!(Balances::reducible_balance(&1, Expendable, Polite), 100);
		assert_eq!(Balances::reducible_balance(&1, Protect, Polite), 100);
		assert_eq!(Balances::reducible_balance(&1, Preserve, Polite), 90);
		assert_eq!(Balances::reducible_balance(&1, Expendable, Force), 100);
		assert_eq!(Balances::reducible_balance(&1, Protect, Force), 100);
		assert_eq!(Balances::reducible_balance(&1, Preserve, Force), 90);
	});
}

#[test]
fn inspect_trait_reducible_balance_frozen_works() {
	ExtBuilder::default().existential_deposit(10).build_and_execute_with(|| {
		Balances::set_balance(&1, 100);
		assert_ok!(Balances::set_freeze(&TestId::Foo, &1, 50));
		assert_eq!(Balances::reducible_balance(&1, Expendable, Polite), 50);
		assert_eq!(Balances::reducible_balance(&1, Protect, Polite), 50);
		assert_eq!(Balances::reducible_balance(&1, Preserve, Polite), 50);
		assert_eq!(Balances::reducible_balance(&1, Expendable, Force), 90);
		assert_eq!(Balances::reducible_balance(&1, Protect, Force), 90);
		assert_eq!(Balances::reducible_balance(&1, Preserve, Force), 90);
	});
}

#[test]
fn unbalanced_trait_set_balance_works() {
	ExtBuilder::default().build_and_execute_with(|| {
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 0);
		assert_ok!(Balances::write_balance(&1337, 100));
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 100);

		assert_ok!(<Balances as fungible::MutateHold<_>>::hold(&TestId::Foo, &1337, 60));
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 40);
		assert_eq!(<Balances as fungible::InspectHold<_>>::total_balance_on_hold(&1337), 60);
		assert_eq!(
			<Balances as fungible::InspectHold<_>>::balance_on_hold(&TestId::Foo, &1337),
			60
		);

		assert_noop!(Balances::write_balance(&1337, 0), Error::<Test>::InsufficientBalance);

		assert_ok!(Balances::write_balance(&1337, 1));
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 1);
		assert_eq!(
			<Balances as fungible::InspectHold<_>>::balance_on_hold(&TestId::Foo, &1337),
			60
		);

		assert_ok!(<Balances as fungible::MutateHold<_>>::release(&TestId::Foo, &1337, 60, Exact));
		System::assert_last_event(RuntimeEvent::Balances(crate::Event::Released {
			reason: TestId::Foo,
			who: 1337,
			amount: 60,
		}));
		assert_eq!(<Balances as fungible::InspectHold<_>>::balance_on_hold(&TestId::Foo, &1337), 0);
		assert_eq!(<Balances as fungible::InspectHold<_>>::total_balance_on_hold(&1337), 0);
	});
}

#[test]
fn unbalanced_trait_set_total_issuance_works() {
	ExtBuilder::default().build_and_execute_with(|| {
		assert_eq!(<Balances as fungible::Inspect<_>>::total_issuance(), 0);
		Balances::set_total_issuance(100);
		assert_eq!(<Balances as fungible::Inspect<_>>::total_issuance(), 100);
	});
}

#[test]
fn inactive_issuance_survives_reverted_rescind() {
	ExtBuilder::default().build_and_execute_with(|| {
		assert_ok!(Balances::mint_into(&1, 100));
		Balances::deactivate(80);
		assert_eq!(Balances::active_issuance(), 20);

		let debt = <Balances as fungible::Balanced<_>>::rescind(30);
		assert_eq!(Balances::total_issuance(), 70);
		assert_eq!(crate::InactiveIssuance::<Test>::get(), 80);

		drop(debt);
		assert_eq!(Balances::total_issuance(), 100);
		assert_eq!(crate::InactiveIssuance::<Test>::get(), 80);
		assert_eq!(Balances::active_issuance(), 20);
	});
}

#[test]
fn deactivate_zero_does_not_reduce_inactive_issuance() {
	ExtBuilder::default().build_and_execute_with(|| {
		assert_ok!(Balances::mint_into(&1, 100));
		Balances::deactivate(80);

		let debt = <Balances as fungible::Balanced<_>>::rescind(30);
		assert_eq!(Balances::total_issuance(), 70);
		assert_eq!(crate::InactiveIssuance::<Test>::get(), 80);

		assert_storage_noop!(Balances::deactivate(0));

		drop(debt);
	});
}

#[test]
fn unbalanced_trait_decrease_balance_simple_works() {
	ExtBuilder::default().build_and_execute_with(|| {
		// An Account that starts at 100
		assert_ok!(Balances::write_balance(&1337, 100));
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 100);
		// and reserves 50
		assert_ok!(<Balances as fungible::MutateHold<_>>::hold(&TestId::Foo, &1337, 50));
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 50);
		// and is decreased by 20
		assert_ok!(Balances::decrease_balance(&1337, 20, Exact, Expendable, Polite));
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 30);
	});
}

#[test]
fn unbalanced_trait_decrease_balance_works_1() {
	ExtBuilder::default().build_and_execute_with(|| {
		assert_ok!(Balances::write_balance(&1337, 100));
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 100);

		assert_noop!(
			Balances::decrease_balance(&1337, 101, Exact, Expendable, Polite),
			TokenError::FundsUnavailable
		);
		assert_eq!(Balances::decrease_balance(&1337, 100, Exact, Expendable, Polite), Ok(100));
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 0);
	});
}

#[test]
fn unbalanced_trait_decrease_balance_works_2() {
	ExtBuilder::default().build_and_execute_with(|| {
		// free: 40, reserved: 60
		assert_ok!(Balances::write_balance(&1337, 100));
		assert_ok!(Balances::hold(&TestId::Foo, &1337, 60));
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 40);
		assert_eq!(Balances::total_balance_on_hold(&1337), 60);
		assert_noop!(
			Balances::decrease_balance(&1337, 40, Exact, Expendable, Polite),
			TokenError::FundsUnavailable
		);
		assert_eq!(Balances::decrease_balance(&1337, 39, Exact, Expendable, Polite), Ok(39));
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 1);
		assert_eq!(Balances::total_balance_on_hold(&1337), 60);
	});
}

#[test]
fn unbalanced_trait_decrease_balance_at_most_works_1() {
	ExtBuilder::default().build_and_execute_with(|| {
		assert_ok!(Balances::write_balance(&1337, 100));
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 100);

		assert_eq!(Balances::decrease_balance(&1337, 101, BestEffort, Expendable, Polite), Ok(100));
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 0);
	});
}

#[test]
fn unbalanced_trait_decrease_balance_at_most_works_2() {
	ExtBuilder::default().build_and_execute_with(|| {
		assert_ok!(Balances::write_balance(&1337, 99));
		assert_eq!(Balances::decrease_balance(&1337, 99, BestEffort, Expendable, Polite), Ok(99));
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 0);
	});
}

#[test]
fn unbalanced_trait_decrease_balance_at_most_works_3() {
	ExtBuilder::default().build_and_execute_with(|| {
		// free: 40, reserved: 60
		assert_ok!(Balances::write_balance(&1337, 100));
		assert_ok!(Balances::hold(&TestId::Foo, &1337, 60));
		assert_eq!(Balances::free_balance(1337), 40);
		assert_eq!(Balances::total_balance_on_hold(&1337), 60);
		assert_eq!(Balances::decrease_balance(&1337, 0, BestEffort, Expendable, Polite), Ok(0));
		assert_eq!(Balances::free_balance(1337), 40);
		assert_eq!(Balances::total_balance_on_hold(&1337), 60);
		assert_eq!(Balances::decrease_balance(&1337, 10, BestEffort, Expendable, Polite), Ok(10));
		assert_eq!(Balances::free_balance(1337), 30);
		assert_eq!(Balances::decrease_balance(&1337, 200, BestEffort, Expendable, Polite), Ok(29));
		assert_eq!(<Balances as fungible::Inspect<_>>::balance(&1337), 1);
		assert_eq!(Balances::free_balance(1337), 1);
		assert_eq!(Balances::total_balance_on_hold(&1337), 60);
	});
}

#[test]
fn unbalanced_trait_increase_balance_works() {
	ExtBuilder::default().build_and_execute_with(|| {
		assert_noop!(Balances::increase_balance(&1337, 0, Exact), TokenError::BelowMinimum);
		assert_eq!(Balances::increase_balance(&1337, 1, Exact), Ok(1));
		assert_noop!(Balances::increase_balance(&1337, u64::MAX, Exact), ArithmeticError::Overflow);
	});
}

#[test]
fn increase_balance_rejects_total_balance_overflow() {
	ExtBuilder::default().build_and_execute_with(|| {
		// The free-balance addition is checked, but free and reserved are then combined with
		// saturating arithmetic. That masks the aggregate overflow and accepts an account whose
		// mathematical total cannot be represented by `Balance`.
		let account = 1;

		Balances::set_balance(&account, u64::MAX - 1);
		assert_ok!(Balances::hold(&TestId::Foo, &account, 1));
		assert_eq!(get_test_account_data(account).free, u64::MAX - 2);
		assert_eq!(get_test_account_data(account).reserved, 1);

		// Exact cannot credit the requested amount because it would make the aggregate overflow.
		assert_noop!(Balances::increase_balance(&account, 2, Exact), ArithmeticError::Overflow);

		// BestEffort must credit only the remaining aggregate headroom, not hide the overflow by
		// saturating `total_balance` after writing both components.
		assert_eq!(Balances::increase_balance(&account, 2, BestEffort), Ok(1));
		assert_eq!(get_test_account_data(account).free, u64::MAX - 1);
		assert_eq!(get_test_account_data(account).reserved, 1);
		assert_eq!(Balances::total_balance(&account), u64::MAX);
	});
}

#[test]
fn unbalanced_trait_increase_balance_at_most_works() {
	ExtBuilder::default().build_and_execute_with(|| {
		assert_eq!(Balances::increase_balance(&1337, 0, BestEffort), Ok(0));
		assert_eq!(Balances::increase_balance(&1337, 1, BestEffort), Ok(1));
		assert_eq!(Balances::increase_balance(&1337, u64::MAX, BestEffort), Ok(u64::MAX - 1));
	});
}

#[test]
fn freezing_and_holds_should_overlap() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			assert_ok!(Balances::set_freeze(&TestId::Foo, &1, 10));
			assert_ok!(Balances::hold(&TestId::Foo, &1, 9));
			assert_eq!(get_test_account_data(1).free, 1);
			assert_eq!(System::consumers(&1), 1);
			assert_eq!(get_test_account_data(1).free, 1);
			assert_eq!(get_test_account_data(1).frozen, 10);
			assert_eq!(get_test_account_data(1).reserved, 9);
			assert_eq!(Balances::total_balance_on_hold(&1), 9);
		});
}

#[test]
fn frozen_hold_balance_cannot_be_moved_without_force() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			assert_ok!(Balances::set_freeze(&TestId::Foo, &1, 10));
			assert_ok!(Balances::hold(&TestId::Foo, &1, 9));
			assert_eq!(Balances::reducible_total_balance_on_hold(&1, Force), 9);
			assert_eq!(Balances::reducible_total_balance_on_hold(&1, Polite), 0);
			let e = TokenError::Frozen;
			assert_noop!(
				Balances::transfer_on_hold(&TestId::Foo, &1, &2, 1, Exact, Free, Polite),
				e
			);
			assert_ok!(Balances::transfer_on_hold(&TestId::Foo, &1, &2, 1, Exact, Free, Force));

			assert_eq!(
				events(),
				[
					RuntimeEvent::Balances(crate::Event::Frozen { who: 1, amount: 10 }),
					RuntimeEvent::Balances(crate::Event::Held {
						reason: TestId::Foo,
						who: 1,
						amount: 9
					}),
					RuntimeEvent::Balances(crate::Event::TransferOnHold {
						reason: TestId::Foo,
						source: 1,
						dest: 2,
						amount: 1
					})
				]
			);

			assert_eq!(Balances::total_balance(&2), 21);
		});
}

#[test]
fn transfer_and_hold() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			// Freeze 7 units in source account (Account 1)
			assert_ok!(Balances::hold(&TestId::Foo, &1, 7));
			assert_ok!(Balances::hold(&TestId::Foo, &2, 2));

			// Verify reducible balance
			assert_eq!(Balances::reducible_total_balance_on_hold(&1, Force), 7);

			// Force transfer_and_hold should succeed
			assert_ok!(Balances::transfer_and_hold(
				&TestId::Foo,
				&1,
				&2,
				1,
				Exact,
				Preserve,
				Polite
			));

			// Verify state changes
			assert_eq!(Balances::free_balance(1), 2);
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &2), 3);
			assert_eq!(Balances::total_balance(&2), 21);

			assert_eq!(
				events(),
				[
					RuntimeEvent::Balances(crate::Event::Held {
						reason: TestId::Foo,
						who: 1,
						amount: 7
					}),
					RuntimeEvent::Balances(crate::Event::Held {
						reason: TestId::Foo,
						who: 2,
						amount: 2
					}),
					RuntimeEvent::Balances(crate::Event::TransferAndHold {
						reason: TestId::Foo,
						source: 1,
						dest: 2,
						transferred: 1
					})
				]
			);
		});
}

#[test]
fn burn_held() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			let account = 1;

			assert_ok!(Balances::hold(&TestId::Foo, &account, 5));

			// Burn the held funds
			assert_ok!(Balances::burn_held(&TestId::Foo, &account, 4, Exact, Polite));

			// Check that the BurnedHeld event is emitted with correct parameters
			System::assert_last_event(RuntimeEvent::Balances(crate::Event::BurnedHeld {
				reason: TestId::Foo,
				who: account,
				amount: 4,
			}));

			// Verify the held balance is removed and total balance is updated
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &account), 1);
			assert_eq!(Balances::total_balance(&account), 6);
			assert_eq!(Balances::total_issuance(), 106);
		});
}

#[test]
fn negative_imbalance_drop_handled_correctly() {
	ExtBuilder::default().build_and_execute_with(|| {
		let account = 1;
		let initial_balance = 100;
		let withdraw_amount = 50;

		// Set initial balance and total issuance
		Balances::set_balance(&account, initial_balance);
		assert_eq!(Balances::total_issuance(), initial_balance);

		// Withdraw using fungible::Balanced to create a NegativeImbalance
		let negative_imb = <Balances as fungible::Balanced<_>>::withdraw(
			&account,
			withdraw_amount,
			Exact,
			Expendable,
			Polite,
		)
		.expect("Withdraw failed");

		// Verify balance decreased but total issuance remains unchanged
		assert_eq!(Balances::free_balance(&account), initial_balance - withdraw_amount);
		assert_eq!(Balances::total_issuance(), initial_balance);

		// Drop the NegativeImbalance, triggering HandleImbalanceDrop
		drop(negative_imb);

		// Check total issuance decreased and event emitted
		assert_eq!(Balances::total_issuance(), initial_balance - withdraw_amount);
		System::assert_last_event(RuntimeEvent::Balances(crate::Event::BurnedDebt {
			amount: withdraw_amount,
		}));
	});
}

#[test]
fn positive_imbalance_drop_handled_correctly() {
	ExtBuilder::default().build_and_execute_with(|| {
		let account = 1;
		let deposit_amount = 50;
		let initial_issuance = Balances::total_issuance();

		// Deposit using fungible::Balanced to create a PositiveImbalance
		let positive_imb =
			<Balances as fungible::Balanced<_>>::deposit(&account, deposit_amount, Exact)
				.expect("Deposit failed");

		// Verify balance increased but total issuance remains unchanged
		assert_eq!(Balances::free_balance(&account), deposit_amount);
		assert_eq!(Balances::total_issuance(), initial_issuance);

		// Drop the PositiveImbalance, triggering HandleImbalanceDrop
		drop(positive_imb);

		// Check total issuance increased and event emitted
		assert_eq!(Balances::total_issuance(), initial_issuance + deposit_amount);
		System::assert_last_event(RuntimeEvent::Balances(crate::Event::MintedCredit {
			amount: deposit_amount,
		}));
	});
}

#[test]
fn frozen_hold_balance_best_effort_transfer_works() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			assert_ok!(Balances::set_freeze(&TestId::Foo, &1, 5));
			assert_ok!(Balances::hold(&TestId::Foo, &1, 9));
			assert_eq!(Balances::reducible_total_balance_on_hold(&1, Force), 9);
			assert_eq!(Balances::reducible_total_balance_on_hold(&1, Polite), 5);
			assert_ok!(Balances::transfer_on_hold(
				&TestId::Foo,
				&1,
				&2,
				10,
				BestEffort,
				Free,
				Polite
			));
			assert_eq!(Balances::total_balance(&1), 5);
			assert_eq!(Balances::total_balance(&2), 25);
		});
}

#[test]
fn partial_freezing_should_work() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			assert_ok!(Balances::set_freeze(&TestId::Foo, &1, 5));
			assert_eq!(System::consumers(&1), 1);
			assert_ok!(<Balances as fungible::Mutate<_>>::transfer(&1, &2, 5, Expendable));
			assert_noop!(
				<Balances as fungible::Mutate<_>>::transfer(&1, &2, 1, Expendable),
				TokenError::Frozen
			);
		});
}

#[test]
fn thaw_should_work() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			assert_ok!(Balances::set_freeze(&TestId::Foo, &1, u64::MAX));
			assert_ok!(Balances::thaw(&TestId::Foo, &1));
			assert_eq!(System::consumers(&1), 0);
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &1), 0);
			assert_eq!(get_test_account_data(1).frozen, 0);
			assert_ok!(<Balances as fungible::Mutate<_>>::transfer(&1, &2, 10, Expendable));
		});
}

#[test]
fn set_freeze_zero_should_work() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			assert_ok!(Balances::set_freeze(&TestId::Foo, &1, u64::MAX));
			assert_ok!(Balances::set_freeze(&TestId::Foo, &1, 0));
			assert_eq!(System::consumers(&1), 0);
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &1), 0);
			assert_eq!(get_test_account_data(1).frozen, 0);
			assert_ok!(<Balances as fungible::Mutate<_>>::transfer(&1, &2, 10, Expendable));
		});
}

#[test]
fn set_freeze_should_work() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			assert_ok!(Balances::set_freeze(&TestId::Foo, &1, u64::MAX));
			assert_ok!(Balances::set_freeze(&TestId::Foo, &1, 5));
			assert_ok!(<Balances as fungible::Mutate<_>>::transfer(&1, &2, 5, Expendable));
			assert_noop!(
				<Balances as fungible::Mutate<_>>::transfer(&1, &2, 1, Expendable),
				TokenError::Frozen
			);
		});
}

#[test]
fn extend_freeze_should_work() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			assert_ok!(Balances::set_freeze(&TestId::Foo, &1, 5));
			assert_ok!(Balances::extend_freeze(&TestId::Foo, &1, 10));
			assert_eq!(get_test_account_data(1).frozen, 10);
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &1), 10);
			assert_noop!(
				<Balances as fungible::Mutate<_>>::transfer(&1, &2, 1, Expendable),
				TokenError::Frozen
			);
		});
}

#[test]
fn double_freezing_should_work() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			assert_ok!(Balances::set_freeze(&TestId::Foo, &1, 5));
			assert_ok!(Balances::set_freeze(&TestId::Bar, &1, 5));
			assert_eq!(System::consumers(&1), 1);
			assert_ok!(<Balances as fungible::Mutate<_>>::transfer(&1, &2, 5, Expendable));
			assert_noop!(
				<Balances as fungible::Mutate<_>>::transfer(&1, &2, 1, Expendable),
				TokenError::Frozen
			);
		});
}

#[test]
fn can_hold_entire_balance_when_second_provider() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(false)
		.build_and_execute_with(|| {
			<Balances as fungible::Mutate<_>>::set_balance(&1, 100);
			assert_noop!(Balances::hold(&TestId::Foo, &1, 100), TokenError::FundsUnavailable);
			System::inc_providers(&1);
			assert_eq!(System::providers(&1), 2);
			assert_ok!(Balances::hold(&TestId::Foo, &1, 100));
			assert_eq!(System::providers(&1), 1);
			assert_noop!(System::dec_providers(&1), DispatchError::ConsumerRemaining);
		});
}

#[test]
fn unholding_frees_hold_slot() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(false)
		.build_and_execute_with(|| {
			<Balances as fungible::Mutate<_>>::set_balance(&1, 100);
			assert_ok!(Balances::hold(&TestId::Foo, &1, 10));
			assert_ok!(Balances::hold(&TestId::Bar, &1, 10));
			assert_ok!(Balances::release(&TestId::Foo, &1, 10, Exact));
			assert_ok!(Balances::hold(&TestId::Baz, &1, 10));
		});
}

#[test]
fn sufficients_work_properly_with_reference_counting() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			// Only run PoC when the system pallet is enabled, since the underlying bug is in the
			// system pallet it won't work with BalancesAccountStore
			if UseSystem::get() {
				// Start with a balance of 100
				<Balances as fungible::Mutate<_>>::set_balance(&1, 100);
				// Emulate a sufficient, in reality this could be reached by transferring a
				// sufficient asset to the account
				System::inc_sufficients(&1);
				// Spend the same balance multiple times
				assert_ok!(<Balances as fungible::Mutate<_>>::transfer(&1, &1337, 100, Expendable));
				assert_eq!(Balances::free_balance(&1), 0);
				assert_noop!(
					<Balances as fungible::Mutate<_>>::transfer(&1, &1337, 100, Expendable),
					TokenError::FundsUnavailable
				);
			}
		});
}

#[test]
fn emit_events_with_changing_freezes() {
	ExtBuilder::default().build_and_execute_with(|| {
		let _ = Balances::set_balance(&1, 100);
		System::reset_events();

		// Freeze = [] --> [10]
		assert_ok!(Balances::set_freeze(&TestId::Foo, &1, 10));
		assert_eq!(events(), [RuntimeEvent::Balances(crate::Event::Frozen { who: 1, amount: 10 })]);

		// Freeze = [10] --> [15]
		assert_ok!(Balances::set_freeze(&TestId::Foo, &1, 15));
		assert_eq!(events(), [RuntimeEvent::Balances(crate::Event::Frozen { who: 1, amount: 5 })]);

		// Freeze = [15] --> [15, 20]
		assert_ok!(Balances::set_freeze(&TestId::Bar, &1, 20));
		assert_eq!(events(), [RuntimeEvent::Balances(crate::Event::Frozen { who: 1, amount: 5 })]);

		// Freeze = [15, 20] --> [17, 20]
		assert_ok!(Balances::set_freeze(&TestId::Foo, &1, 17));
		for event in events() {
			match event {
				RuntimeEvent::Balances(crate::Event::Frozen { .. }) => {
					assert!(false, "unexpected freeze event")
				},
				RuntimeEvent::Balances(crate::Event::Thawed { .. }) => {
					assert!(false, "unexpected thaw event")
				},
				_ => continue,
			}
		}

		// Freeze = [17, 20] --> [17, 15]
		assert_ok!(Balances::set_freeze(&TestId::Bar, &1, 15));
		assert_eq!(events(), [RuntimeEvent::Balances(crate::Event::Thawed { who: 1, amount: 3 })]);

		// Freeze = [17, 15] --> [15]
		assert_ok!(Balances::thaw(&TestId::Foo, &1));
		assert_eq!(events(), [RuntimeEvent::Balances(crate::Event::Thawed { who: 1, amount: 2 })]);

		// Freeze = [15] --> []
		assert_ok!(Balances::thaw(&TestId::Bar, &1));
		assert_eq!(events(), [RuntimeEvent::Balances(crate::Event::Thawed { who: 1, amount: 15 })]);
	});
}

#[test]
fn withdraw_precision_exact_works() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			assert_ok!(Balances::set_freeze(&TestId::Foo, &1, 10));
			assert_eq!(get_test_account_data(1).free, 10);
			assert_eq!(get_test_account_data(1).frozen, 10);

			// `BestEffort` will not reduce anything
			assert_ok!(<Balances as fungible::Balanced<_>>::withdraw(
				&1, 5, BestEffort, Preserve, Polite
			));

			assert_eq!(get_test_account_data(1).free, 10);
			assert_eq!(get_test_account_data(1).frozen, 10);

			assert_noop!(
				<Balances as fungible::Balanced<_>>::withdraw(&1, 5, Exact, Preserve, Polite),
				TokenError::FundsUnavailable
			);
		});
}

#[test]
fn freeze_consideration_works() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			type Consideration = FreezeConsideration<
				u64,
				Balances,
				FooReason,
				LinearStoragePrice<ConstU64<0>, ConstU64<1>, u64>,
				Footprint,
			>;

			let who = 4;
			// freeze amount taken somewhere outside of our (Consideration) scope.
			let extend_freeze = 15;

			let ticket = Consideration::new(&who, Footprint::from_parts(0, 0)).unwrap();
			assert!(ticket.is_none());
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 0);

			let ticket = Consideration::new(&who, Footprint::from_parts(10, 1)).unwrap();
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 10);

			let ticket = ticket.update(&who, Footprint::from_parts(4, 1)).unwrap();
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 4);

			assert_ok!(Balances::increase_frozen(&TestId::Foo, &who, extend_freeze));
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 4 + extend_freeze);

			let ticket = ticket.update(&who, Footprint::from_parts(8, 1)).unwrap();
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 8 + extend_freeze);

			let ticket = ticket.update(&who, Footprint::from_parts(0, 0)).unwrap();
			assert!(ticket.is_none());
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 0 + extend_freeze);

			let ticket = Consideration::new(&who, Footprint::from_parts(10, 1)).unwrap();
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 10 + extend_freeze);

			let _ = ticket.drop(&who).unwrap();
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 0 + extend_freeze);
		});
}

#[test]
fn hold_consideration_works() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			type Consideration = HoldConsideration<
				u64,
				Balances,
				FooReason,
				LinearStoragePrice<ConstU64<0>, ConstU64<1>, u64>,
				Footprint,
			>;

			let who = 4;
			// hold amount taken somewhere outside of our (Consideration) scope.
			let extend_hold = 15;

			let ticket = Consideration::new(&who, Footprint::from_parts(0, 0)).unwrap();
			assert!(ticket.is_none());
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 0);

			let ticket = ticket.update(&who, Footprint::from_parts(10, 1)).unwrap();
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 10);

			let ticket = ticket.update(&who, Footprint::from_parts(4, 1)).unwrap();
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 4);

			assert_ok!(Balances::hold(&TestId::Foo, &who, extend_hold));
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 4 + extend_hold);

			let ticket = ticket.update(&who, Footprint::from_parts(8, 1)).unwrap();
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 8 + extend_hold);

			let ticket = ticket.update(&who, Footprint::from_parts(0, 0)).unwrap();
			assert!(ticket.is_none());
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 0 + extend_hold);

			let ticket = Consideration::new(&who, Footprint::from_parts(10, 1)).unwrap();
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 10 + extend_hold);

			let _ = ticket.drop(&who).unwrap();
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 0 + extend_hold);
		});
}

#[test]
fn lone_freeze_consideration_works() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			type Consideration = LoneFreezeConsideration<
				u64,
				Balances,
				FooReason,
				LinearStoragePrice<ConstU64<0>, ConstU64<1>, u64>,
				Footprint,
			>;

			let who = 4;
			let zero_ticket = Consideration::new(&who, Footprint::from_parts(0, 0)).unwrap();
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 0);

			let ticket = Consideration::new(&who, Footprint::from_parts(10, 1)).unwrap();
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 10);

			assert_ok!(Balances::increase_frozen(&TestId::Foo, &who, 5));
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 15);

			let ticket = ticket.update(&who, Footprint::from_parts(4, 1)).unwrap();
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 4);

			assert_eq!(ticket.update(&who, Footprint::from_parts(0, 0)).unwrap(), zero_ticket);
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 0);

			let ticket = Consideration::new(&who, Footprint::from_parts(10, 1)).unwrap();
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 10);

			let _ = ticket.drop(&who).unwrap();
			assert_eq!(Balances::balance_frozen(&TestId::Foo, &who), 0);
		});
}

#[test]
fn lone_hold_consideration_works() {
	ExtBuilder::default()
		.existential_deposit(1)
		.monied(true)
		.build_and_execute_with(|| {
			type Consideration = LoneHoldConsideration<
				u64,
				Balances,
				FooReason,
				LinearStoragePrice<ConstU64<0>, ConstU64<1>, u64>,
				Footprint,
			>;

			let who = 4;
			let zero_ticket = Consideration::new(&who, Footprint::from_parts(0, 0)).unwrap();
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 0);

			let ticket = Consideration::new(&who, Footprint::from_parts(10, 1)).unwrap();
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 10);

			assert_ok!(Balances::hold(&TestId::Foo, &who, 5));
			assert_eq!(
				events(),
				[
					RuntimeEvent::Balances(crate::Event::Held {
						reason: TestId::Foo,
						who,
						amount: 10
					}),
					RuntimeEvent::Balances(crate::Event::Held {
						reason: TestId::Foo,
						who,
						amount: 5
					})
				]
			);
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 15);

			let ticket = ticket.update(&who, Footprint::from_parts(4, 1)).unwrap();
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 4);

			assert_eq!(ticket.update(&who, Footprint::from_parts(0, 0)).unwrap(), zero_ticket);
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 0);

			let ticket = Consideration::new(&who, Footprint::from_parts(10, 1)).unwrap();
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 10);

			let _ = ticket.drop(&who).unwrap();
			assert_eq!(Balances::balance_on_hold(&TestId::Foo, &who), 0);
		});
}
