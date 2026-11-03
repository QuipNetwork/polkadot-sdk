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

//! Tests for budget allocation functionality.
use super::{budget_map, key};
use crate::{
	mock::{
		account_id, assert_try_state_invalid, build_and_execute, set_default_budget_allocation,
		Dap, RuntimeOrigin, System, Test,
	},
	BudgetAllocation, Error, Event,
};
use frame_support::{assert_noop, assert_ok};
use sp_runtime::Perbill;

#[test]
fn set_budget_allocation_works_with_root() {
	build_and_execute(true, || {
		System::set_block_number(1);

		let allocs =
			budget_map(&[(b"buffer", 20), (b"staker_rewards", 60), (b"validator_incentive", 20)]);

		assert_ok!(Dap::set_budget_allocation(RuntimeOrigin::root(), allocs.clone()));

		assert_eq!(BudgetAllocation::<Test>::get(), allocs);
		System::assert_has_event(Event::BudgetAllocationUpdated { allocations: allocs }.into());
	});
}

#[test]
fn set_budget_allocation_rejects_unknown_key() {
	build_and_execute(true, || {
		// GIVEN: default budget allocation with known keys (buffer, staker_rewards,
		// validator_incentive).
		set_default_budget_allocation();

		// WHEN: trying to set an allocation with an unknown key.
		let allocs = budget_map(&[(b"unknown_key", 50)]);

		// THEN: rejected.
		assert_noop!(
			Dap::set_budget_allocation(RuntimeOrigin::root(), allocs),
			Error::<Test>::UnknownBudgetKey
		);
	});
}

#[test]
fn set_budget_allocation_rejects_over_100_percent() {
	build_and_execute(true, || {
		set_default_budget_allocation();

		// WHEN: allocations sum to 110%.
		let allocs = budget_map(&[(b"buffer", 50), (b"staker_rewards", 60)]);

		// THEN: rejected.
		assert_noop!(
			Dap::set_budget_allocation(RuntimeOrigin::root(), allocs),
			Error::<Test>::BudgetNotExact
		);
	});
}

#[test]
fn set_budget_allocation_rejects_under_100_percent() {
	build_and_execute(true, || {
		set_default_budget_allocation();

		// WHEN: allocations sum to only 50%.
		let allocs = budget_map(&[(b"staker_rewards", 50)]);

		// THEN: rejected.
		assert_noop!(
			Dap::set_budget_allocation(RuntimeOrigin::root(), allocs),
			Error::<Test>::BudgetNotExact
		);
	});
}

#[test]
fn set_budget_allocation_requires_budget_origin() {
	build_and_execute(true, || {
		set_default_budget_allocation();

		let allocs = budget_map(&[(b"staker_rewards", 80)]);

		assert_noop!(
			Dap::set_budget_allocation(RuntimeOrigin::signed(account_id(1)), allocs),
			sp_runtime::DispatchError::BadOrigin
		);
	});
}

#[test]
fn try_state_detects_unknown_key_in_allocation() {
	build_and_execute(true, || {
		set_default_budget_allocation();

		// Corrupt: inject an unregistered key.
		let mut corrupt = budget_map(&[(b"staker_rewards", 100)]);
		corrupt.try_insert(key(b"rogue_key"), Perbill::from_percent(0)).unwrap();
		BudgetAllocation::<Test>::put(corrupt);

		assert_try_state_invalid();

		// Restore valid state for post-test try_state.
		set_default_budget_allocation();
	});
}

#[test]
fn try_state_detects_allocation_not_summing_to_100() {
	build_and_execute(true, || {
		set_default_budget_allocation();

		// Corrupt: allocations don't sum to 100%.
		let corrupt = budget_map(&[(b"buffer", 10), (b"staker_rewards", 50)]);
		BudgetAllocation::<Test>::put(corrupt);

		assert_try_state_invalid();

		// Restore valid state for post-test try_state.
		set_default_budget_allocation();
	});
}
