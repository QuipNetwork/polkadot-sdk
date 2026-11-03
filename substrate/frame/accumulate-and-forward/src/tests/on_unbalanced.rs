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

//! OnUnbalanced tests for the accumulate-and-forward pallet.

use crate::mock::*;
use frame_support::{
	assert_ok,
	traits::{
		fungible::{Balanced, Inspect, Mutate},
		tokens::{Fortitude, Precision, Preservation},
		Currency, OnUnbalanced,
	},
};

type AccumulateForwardPallet = crate::Pallet<Test>;
type LegacyAdapterPallet = crate::LegacyAdapter<Test, Balances>;

#[test]
fn on_unbalanced_deposits_to_accumulation_account() {
	new_test_ext(true).execute_with(|| {
		let accumulation_account = AccumulateForwardPallet::accumulation_account();
		let ed = <Balances as Inspect<_>>::minimum_balance();

		// Given: accumulation account has ED, users have balances (1: 100, 2: 200, 3: 300)
		assert_eq!(Balances::free_balance(accumulation_account), ed);
		let initial_total = <Balances as Inspect<_>>::total_issuance();
		let initial_active = <Balances as Inspect<_>>::active_issuance();

		// When: multiple imbalances are deposited (e.g., coretime revenue from user payments)
		// withdraw() takes funds from an account and returns a Credit
		let credit1 = <Balances as Balanced<u64>>::withdraw(
			&1,
			30,
			Precision::Exact,
			Preservation::Preserve,
			Fortitude::Force,
		)
		.unwrap();
		AccumulateForwardPallet::on_unbalanced(credit1);

		let credit2 = <Balances as Balanced<u64>>::withdraw(
			&2,
			20,
			Precision::Exact,
			Preservation::Preserve,
			Fortitude::Force,
		)
		.unwrap();
		AccumulateForwardPallet::on_unbalanced(credit2);

		let credit3 = <Balances as Balanced<u64>>::withdraw(
			&3,
			50,
			Precision::Exact,
			Preservation::Preserve,
			Fortitude::Force,
		)
		.unwrap();
		AccumulateForwardPallet::on_unbalanced(credit3);

		// Then: accumulation account has accumulated all credits
		assert_eq!(Balances::free_balance(accumulation_account), ed + 100);

		// And: users lost their amounts
		assert_eq!(Balances::free_balance(1), 100 - 30);
		assert_eq!(Balances::free_balance(2), 200 - 20);
		assert_eq!(Balances::free_balance(3), 300 - 50);

		// And: total issuance unchanged (funds moved, not created/destroyed)
		assert_eq!(<Balances as Inspect<_>>::total_issuance(), initial_total);

		// And: active issuance unchanged (accumulate-and-forward chains don't deactivate)
		assert_eq!(<Balances as Inspect<_>>::active_issuance(), initial_active);
	});
}

#[test]
fn on_unbalanced_handles_zero_amount() {
	new_test_ext(true).execute_with(|| {
		let accumulation_account = AccumulateForwardPallet::accumulation_account();
		let ed = <Balances as Inspect<_>>::minimum_balance();
		let initial_active = <Balances as Inspect<_>>::active_issuance();

		// Given: accumulation account has ED
		assert_eq!(Balances::free_balance(accumulation_account), ed);

		// When: imbalance with zero amount
		let credit = <Balances as Balanced<u64>>::issue(0);
		AccumulateForwardPallet::on_unbalanced(credit);

		// Then: accumulation account still has just ED (no-op)
		assert_eq!(Balances::free_balance(accumulation_account), ed);
		// And: active issuance unchanged
		assert_eq!(<Balances as Inspect<_>>::active_issuance(), initial_active);
	});
}

#[test]
#[should_panic(expected = "Failed to deposit to accumulation account")]
fn on_unbalanced_panics_when_accumulation_account_not_funded_and_deposit_below_ed() {
	new_test_ext(false).execute_with(|| {
		let accumulation_account = AccumulateForwardPallet::accumulation_account();
		let ed = <Balances as Inspect<_>>::minimum_balance();

		// Given: accumulation account is not funded
		assert_eq!(Balances::free_balance(accumulation_account), 0);

		// When: deposit < ED -> triggers defensive panic
		let credit = <Balances as Balanced<u64>>::withdraw(
			&1,
			ed - 1,
			Precision::Exact,
			Preservation::Preserve,
			Fortitude::Force,
		)
		.unwrap();
		AccumulateForwardPallet::on_unbalanced(credit);
	});
}

#[test]
fn on_unbalanced_creates_accumulation_account_when_not_funded_and_deposit_at_least_ed() {
	new_test_ext(false).execute_with(|| {
		let accumulation_account = AccumulateForwardPallet::accumulation_account();
		let ed = <Balances as Inspect<_>>::minimum_balance();

		// Given: accumulation account is not funded
		assert_eq!(Balances::free_balance(accumulation_account), 0);

		// When: deposit >= ED
		let credit = <Balances as Balanced<u64>>::withdraw(
			&1,
			ed,
			Precision::Exact,
			Preservation::Preserve,
			Fortitude::Force,
		)
		.unwrap();
		AccumulateForwardPallet::on_unbalanced(credit);

		// Then: accumulation account is created and funded
		assert_eq!(Balances::free_balance(accumulation_account), ed);
	});
}

#[test]
fn on_unbalanced_multiple_dust_removals_accumulate() {
	new_test_ext(true).execute_with(|| {
		let accumulation_account = AccumulateForwardPallet::accumulation_account();
		let ed = <Balances as Inspect<_>>::minimum_balance();
		let dust = ed / 2;

		// Given: accumulation account has ED. Create 3 accounts with ED + dust each.
		for acct in 10..=12u64 {
			assert_ok!(<Balances as Mutate<_>>::mint_into(&acct, ed + dust));
		}
		let account_before = Balances::free_balance(accumulation_account);
		let issuance_before = <Balances as Inspect<_>>::total_issuance();

		// When: each account transfers ED away, leaving dust < ED → reaped.
		// DustRemoval = AccumulateForward → dust goes to accumulation account.
		for acct in 10..=12u64 {
			assert_ok!(Balances::transfer_allow_death(
				frame_system::RawOrigin::Signed(acct).into(),
				1,
				ed,
			));
			assert_eq!(Balances::free_balance(acct), 0);
		}

		// Then: accumulation account collected dust from all 3 reaps.
		assert_eq!(Balances::free_balance(accumulation_account), account_before + 3 * dust);

		// And: total issuance unchanged (dust moved, not destroyed).
		assert_eq!(<Balances as Inspect<_>>::total_issuance(), issuance_before);
	});
}

#[test]
fn legacy_adapter_redirects_slash_to_accumulation_account() {
	new_test_ext(true).execute_with(|| {
		let accumulation_account = AccumulateForwardPallet::accumulation_account();
		let ed = <Balances as Inspect<_>>::minimum_balance();

		// Given: accumulation account has ED, user 1 has 100
		assert_eq!(Balances::free_balance(accumulation_account), ed);
		let initial_total = <Balances as Inspect<_>>::total_issuance();

		// When: legacy slash via Currency::slash -> LegacyAdapter
		let (imbalance, _) = <Balances as Currency<_>>::slash(&1, 30);
		LegacyAdapterPallet::on_unbalanced(imbalance);

		// Then: accumulation account accumulated the slash
		assert_eq!(Balances::free_balance(accumulation_account), ed + 30);

		// And: user lost the slashed amount
		assert_eq!(Balances::free_balance(1), 100 - 30);

		// And: total issuance unchanged (funds moved, not destroyed)
		assert_eq!(<Balances as Inspect<_>>::total_issuance(), initial_total);
	});
}
