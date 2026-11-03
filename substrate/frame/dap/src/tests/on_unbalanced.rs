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

//! OnUnbalanced tests for the DAP pallet.

use crate::mock::{account_id, build_and_execute, set_default_budget_allocation, Balances, Test};
use frame_support::{
	pallet_prelude::Weight,
	traits::{
		fungible::{Balanced, Inspect},
		tokens::{Fortitude, Precision, Preservation},
		Currency, Hooks, OnUnbalanced,
	},
};

type DapPallet = crate::Pallet<Test>;
type DapLegacy = crate::DapLegacyAdapter<Test, Balances>;

#[test]
#[should_panic(expected = "Failed to deposit slash to DAP staging account")]
fn on_unbalanced_panics_when_staging_not_funded_and_deposit_below_ed() {
	build_and_execute(false, || {
		set_default_budget_allocation();

		let staging = DapPallet::staging_account();
		let ed = <Balances as Inspect<_>>::minimum_balance();

		// Given: staging is not funded
		assert_eq!(Balances::free_balance(&staging), 0);

		// When: deposit < ED -> triggers defensive panic
		let credit = <Balances as Balanced<_>>::withdraw(
			&account_id(1),
			ed - 1,
			Precision::Exact,
			Preservation::Preserve,
			Fortitude::Force,
		)
		.unwrap();
		DapPallet::on_unbalanced(credit);
	});
}

#[test]
fn on_unbalanced_creates_staging_when_not_funded_and_deposit_at_least_ed() {
	build_and_execute(false, || {
		set_default_budget_allocation();

		let staging = DapPallet::staging_account();
		let ed = <Balances as Inspect<_>>::minimum_balance();

		// Given: staging is not funded
		assert_eq!(Balances::free_balance(&staging), 0);

		// When: deposit >= ED
		let credit = <Balances as Balanced<_>>::withdraw(
			&account_id(1),
			ed,
			Precision::Exact,
			Preservation::Preserve,
			Fortitude::Force,
		)
		.unwrap();
		DapPallet::on_unbalanced(credit);

		// Then: staging is created and funded
		assert_eq!(Balances::free_balance(&staging), ed);
	});
}

#[test]
fn slash_to_dap_accumulates_to_staging_then_deactivates_on_idle() {
	build_and_execute(true, || {
		set_default_budget_allocation();

		let buffer = DapPallet::buffer_account();
		let staging = DapPallet::staging_account();
		let ed = <Balances as Inspect<_>>::minimum_balance();

		let alice = account_id(1);
		let bob = account_id(2);
		let charlie = account_id(3);

		// Given: buffer and staging each have ED; users have balances.
		assert_eq!(Balances::free_balance(&buffer), ed);
		assert_eq!(Balances::free_balance(&staging), ed);
		let initial_active = <Balances as Inspect<_>>::active_issuance();
		let initial_total = <Balances as Inspect<_>>::total_issuance();

		// When: multiple slashes occur via OnUnbalanced (simulating staking slashes).
		for (who, amount) in [(&alice, 30u64), (&bob, 20), (&charlie, 50)] {
			let credit = <Balances as Balanced<_>>::withdraw(
				who,
				amount,
				Precision::Exact,
				Preservation::Preserve,
				Fortitude::Force,
			)
			.unwrap();
			DapPallet::on_unbalanced(credit);
		}

		// Then: funds land in staging, not buffer.
		assert_eq!(Balances::free_balance(&staging), ed + 100);
		assert_eq!(Balances::free_balance(&buffer), ed);

		// And: users lost their slashed amounts.
		assert_eq!(Balances::free_balance(&alice), 100 - 30);
		assert_eq!(Balances::free_balance(&bob), 200 - 20);
		assert_eq!(Balances::free_balance(&charlie), 300 - 50);

		// And: active issuance is NOT yet decreased (deactivation is deferred to on_idle).
		assert_eq!(<Balances as Inspect<_>>::active_issuance(), initial_active);

		// And: total issuance unchanged (funds moved, not destroyed).
		assert_eq!(<Balances as Inspect<_>>::total_issuance(), initial_total);

		// When: on_idle drains staging into buffer and deactivates.
		DapPallet::on_idle(1, Weight::MAX);

		// Then: staging retains only ED; buffer gained all slashed funds.
		assert_eq!(Balances::free_balance(&staging), ed);
		assert_eq!(Balances::free_balance(&buffer), ed + 100);

		// And: active issuance decreased by 100 (funds deactivated in DAP buffer).
		assert_eq!(<Balances as Inspect<_>>::active_issuance(), initial_active - 100);

		// And: total issuance still unchanged.
		assert_eq!(<Balances as Inspect<_>>::total_issuance(), initial_total);
	});
}

#[test]
fn legacy_adapter_redirects_slash_to_staging_then_deactivates_on_idle() {
	build_and_execute(true, || {
		set_default_budget_allocation();

		let buffer = DapPallet::buffer_account();
		let staging = DapPallet::staging_account();
		let ed = <Balances as Inspect<_>>::minimum_balance();

		let alice = account_id(1);

		// Given: buffer and staging each have ED, alice has 100.
		assert_eq!(Balances::free_balance(&buffer), ed);
		assert_eq!(Balances::free_balance(&staging), ed);
		let initial_active = <Balances as Inspect<_>>::active_issuance();
		let initial_total = <Balances as Inspect<_>>::total_issuance();

		// When: legacy slash via Currency::slash -> DapLegacyAdapter.
		let (imbalance, _) = <Balances as Currency<_>>::slash(&alice, 30);
		DapLegacy::on_unbalanced(imbalance);

		// Then: funds land in staging, not buffer.
		assert_eq!(Balances::free_balance(&staging), ed + 30);
		assert_eq!(Balances::free_balance(&buffer), ed);

		// And: alice lost the slashed amount.
		assert_eq!(Balances::free_balance(&alice), 100 - 30);

		// And: total issuance unchanged (funds moved, not destroyed).
		assert_eq!(<Balances as Inspect<_>>::total_issuance(), initial_total);

		// And: active issuance is NOT yet decreased (deactivation is deferred to on_idle).
		assert_eq!(<Balances as Inspect<_>>::active_issuance(), initial_active);

		// When: on_idle drains staging into buffer and deactivates.
		DapPallet::on_idle(1, Weight::MAX);

		// Then: staging retains only ED; buffer gained the slashed funds.
		assert_eq!(Balances::free_balance(&staging), ed);
		assert_eq!(Balances::free_balance(&buffer), ed + 30);

		// And: active issuance decreased by 30 (funds deactivated in DAP buffer).
		assert_eq!(<Balances as Inspect<_>>::active_issuance(), initial_active - 30);

		// And: total issuance still unchanged.
		assert_eq!(<Balances as Inspect<_>>::total_issuance(), initial_total);
	});
}
