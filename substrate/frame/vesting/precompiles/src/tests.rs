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

//! Unit tests for the vesting precompile.

use crate::{IVesting, Vesting, VestingBalance, mock::*};
use alloy_core::sol_types::{SolInterface, SolValue};
use frame_support::traits::{Currency, VestingSchedule};
use pallet_revive::{
	AddressMapper, ExecReturnValue,
	precompiles::{Precompile, U256},
	test_utils::builder::BareCallBuilder,
};
use sp_core::H160;

type CurrencyOf<T> = <T as pallet_vesting::Config>::Currency;

const ALICE: u64 = 1;
const TARGET: u64 = 0xBEEF;

fn precompile_address() -> H160 {
	H160(Vesting::<Test>::MATCHER.base_address())
}

fn target_h160() -> H160 {
	<Test as pallet_revive::Config>::AddressMapper::to_address(&TARGET)
}

fn target_alloy() -> alloy_core::primitives::Address {
	alloy_core::primitives::Address::from_slice(target_h160().as_bytes())
}

fn add_vesting_schedule(who: &u64, locked: u64) {
	let per_block = locked / 20;
	<pallet_vesting::Pallet<Test> as VestingSchedule<u64>>::add_vesting_schedule(
		who, locked, per_block, 0,
	)
	.expect("adding vesting schedule should succeed");
}

fn bare_call(input: &IVesting::IVestingCalls) -> BareCallBuilder<Test> {
	BareCallBuilder::<Test>::bare_call(RuntimeOrigin::signed(ALICE), precompile_address())
		.data(input.abi_encode())
}

fn decode_balance(result: &ExecReturnValue) -> U256 {
	assert!(!result.did_revert(), "unexpected revert: {:?}", result.data);
	U256::from_big_endian(&<[u8; 32]>::abi_decode(&result.data).unwrap())
}

// ---------------------------------------------------------------------------
// vest()
// ---------------------------------------------------------------------------

#[test]
fn vest_succeeds_with_active_schedule() {
	new_test_ext().execute_with(|| {
		let locked: VestingBalance<Test> = 10_000;
		CurrencyOf::<Test>::make_free_balance_be(&ALICE, locked * 10);
		add_vesting_schedule(&ALICE, locked);

		let input = IVesting::IVestingCalls::vest(IVesting::vestCall {});
		let result = bare_call(&input).build_and_unwrap_result();
		assert!(!result.did_revert());
		assert!(result.data.is_empty());
	});
}

#[test]
fn vest_actually_unlocks_funds() {
	new_test_ext().execute_with(|| {
		let locked: VestingBalance<Test> = 10_000;
		CurrencyOf::<Test>::make_free_balance_be(&ALICE, locked * 10);
		add_vesting_schedule(&ALICE, locked);

		// Advance past the full schedule so vest() has something to unlock.
		frame_system::Pallet::<Test>::set_block_number(21);

		let input = IVesting::IVestingCalls::vest(IVesting::vestCall {});
		let result = bare_call(&input).build_and_unwrap_result();
		assert!(!result.did_revert());

		// After vest(), the vesting schedule should be removed entirely.
		let schedule =
			<pallet_vesting::Pallet<Test> as VestingSchedule<u64>>::vesting_balance(&ALICE);
		assert_eq!(schedule, None, "vesting schedule should be removed after full vest");
	});
}

// ---------------------------------------------------------------------------
// vestOther()
// ---------------------------------------------------------------------------

#[test]
fn vest_other_succeeds_with_active_schedule() {
	new_test_ext().execute_with(|| {
		let locked: VestingBalance<Test> = 10_000;
		CurrencyOf::<Test>::make_free_balance_be(&TARGET, locked * 10);
		add_vesting_schedule(&TARGET, locked);

		let input =
			IVesting::IVestingCalls::vestOther(IVesting::vestOtherCall { target: target_alloy() });
		let result = bare_call(&input).build_and_unwrap_result();
		assert!(!result.did_revert());
		assert!(result.data.is_empty());
	});
}

#[test]
fn vest_other_actually_unlocks_funds_for_target() {
	new_test_ext().execute_with(|| {
		let locked: VestingBalance<Test> = 10_000;
		CurrencyOf::<Test>::make_free_balance_be(&TARGET, locked * 10);
		add_vesting_schedule(&TARGET, locked);

		frame_system::Pallet::<Test>::set_block_number(21);

		let input =
			IVesting::IVestingCalls::vestOther(IVesting::vestOtherCall { target: target_alloy() });
		let result = bare_call(&input).build_and_unwrap_result();
		assert!(!result.did_revert());

		let schedule =
			<pallet_vesting::Pallet<Test> as VestingSchedule<u64>>::vesting_balance(&TARGET);
		assert_eq!(schedule, None, "target's vesting schedule should be removed after full vest");
	});
}

// ---------------------------------------------------------------------------
// vestingBalance() & vestingBalanceOf()
// ---------------------------------------------------------------------------

#[test]
fn vesting_balance_returns_correct_locked_amount() {
	new_test_ext().execute_with(|| {
		let locked: VestingBalance<Test> = 10_000;
		CurrencyOf::<Test>::make_free_balance_be(&ALICE, locked * 10);
		add_vesting_schedule(&ALICE, locked);

		let input = IVesting::IVestingCalls::vestingBalance(IVesting::vestingBalanceCall {});

		// At block 1 with per_block=500 (10_000/20), one block has vested: 10_000 - 500 = 9_500.
		let result = bare_call(&input).build_and_unwrap_result();
		assert_eq!(decode_balance(&result), U256::from(9_500u64));

		// Advance to block 10: half the 20-block schedule has elapsed.
		frame_system::Pallet::<Test>::set_block_number(10);
		let result = bare_call(&input).build_and_unwrap_result();
		assert_eq!(decode_balance(&result), U256::from(5_000u64));
	});
}

#[test]
fn vesting_balance_returns_zero_when_no_schedule() {
	new_test_ext().execute_with(|| {
		let input = IVesting::IVestingCalls::vestingBalance(IVesting::vestingBalanceCall {});
		let result = bare_call(&input).build_and_unwrap_result();
		assert_eq!(decode_balance(&result), U256::zero());
	});
}

#[test]
fn vesting_balance_returns_zero_when_fully_vested() {
	new_test_ext().execute_with(|| {
		let locked: VestingBalance<Test> = 10_000;
		CurrencyOf::<Test>::make_free_balance_be(&ALICE, locked * 10);
		add_vesting_schedule(&ALICE, locked);

		frame_system::Pallet::<Test>::set_block_number(21);

		let input = IVesting::IVestingCalls::vestingBalance(IVesting::vestingBalanceCall {});
		let result = bare_call(&input).build_and_unwrap_result();
		assert_eq!(decode_balance(&result), U256::zero());
	});
}

#[test]
fn vesting_balance_of_returns_correct_locked_amount() {
	new_test_ext().execute_with(|| {
		let locked: VestingBalance<Test> = 10_000;
		CurrencyOf::<Test>::make_free_balance_be(&TARGET, locked * 10);
		add_vesting_schedule(&TARGET, locked);

		let input = IVesting::IVestingCalls::vestingBalanceOf(IVesting::vestingBalanceOfCall {
			target: target_alloy(),
		});
		let result = bare_call(&input).build_and_unwrap_result();
		assert_eq!(decode_balance(&result), U256::from(9_500u64));
	});
}

#[test]
fn vesting_balance_of_returns_zero_when_no_schedule() {
	new_test_ext().execute_with(|| {
		let input = IVesting::IVestingCalls::vestingBalanceOf(IVesting::vestingBalanceOfCall {
			target: target_alloy(),
		});
		let result = bare_call(&input).build_and_unwrap_result();
		assert_eq!(decode_balance(&result), U256::zero());
	});
}

#[test]
fn vesting_balance_aggregates_multiple_schedules() {
	new_test_ext().execute_with(|| {
		CurrencyOf::<Test>::make_free_balance_be(&ALICE, 500_000);
		add_vesting_schedule(&ALICE, 10_000);
		add_vesting_schedule(&ALICE, 20_000);

		let input = IVesting::IVestingCalls::vestingBalance(IVesting::vestingBalanceCall {});
		let result = bare_call(&input).build_and_unwrap_result();
		// At block 1: schedule 1 locked = 10_000 - 500 = 9_500,
		//             schedule 2 locked = 20_000 - 1_000 = 19_000.
		// Total = 28_500.
		assert_eq!(decode_balance(&result), U256::from(28_500u64));
	});
}

// ---------------------------------------------------------------------------
// vestedTransfer()
// ---------------------------------------------------------------------------

#[test]
fn vested_transfer_succeeds() {
	new_test_ext().execute_with(|| {
		let locked: VestingBalance<Test> = 10_000;
		CurrencyOf::<Test>::make_free_balance_be(&ALICE, locked * 10);

		let input = IVesting::IVestingCalls::vestedTransfer(IVesting::vestedTransferCall {
			target: target_alloy(),
			locked: alloy_core::primitives::U256::from(locked),
			perBlock: alloy_core::primitives::U256::from(500u64),
			startingBlock: alloy_core::primitives::U256::from(0u64),
		});
		let result = bare_call(&input).build_and_unwrap_result();
		assert!(!result.did_revert());
		assert!(result.data.is_empty());

		// Verify the schedule was created with the correct locked amount.
		// At block 1 with per_block=500, one block vested: 10_000 - 500 = 9_500.
		let balance =
			<pallet_vesting::Pallet<Test> as VestingSchedule<u64>>::vesting_balance(&TARGET);
		assert_eq!(balance, Some(9_500), "locked amount should match the schedule");
	});
}

// ---------------------------------------------------------------------------
// Read-only & delegate-call guards (test vectors)
// ---------------------------------------------------------------------------

struct GuardTestCase {
	name: &'static str,
	input: IVesting::IVestingCalls,
	reject_read_only: bool,
	reject_delegate: bool,
}

fn guard_test_cases() -> Vec<GuardTestCase> {
	vec![
		GuardTestCase {
			name: "vest",
			input: IVesting::IVestingCalls::vest(IVesting::vestCall {}),
			reject_read_only: true,
			reject_delegate: true,
		},
		GuardTestCase {
			name: "vestOther",
			input: IVesting::IVestingCalls::vestOther(IVesting::vestOtherCall {
				target: target_alloy(),
			}),
			reject_read_only: true,
			reject_delegate: true,
		},
		GuardTestCase {
			name: "vestedTransfer",
			input: IVesting::IVestingCalls::vestedTransfer(IVesting::vestedTransferCall {
				target: target_alloy(),
				locked: alloy_core::primitives::U256::from(10_000u64),
				perBlock: alloy_core::primitives::U256::from(500u64),
				startingBlock: alloy_core::primitives::U256::from(0u64),
			}),
			reject_read_only: true,
			reject_delegate: true,
		},
		GuardTestCase {
			name: "vestingBalance",
			input: IVesting::IVestingCalls::vestingBalance(IVesting::vestingBalanceCall {}),
			reject_read_only: false,
			reject_delegate: false,
		},
		GuardTestCase {
			name: "vestingBalanceOf",
			input: IVesting::IVestingCalls::vestingBalanceOf(IVesting::vestingBalanceOfCall {
				target: target_alloy(),
			}),
			reject_read_only: false,
			reject_delegate: false,
		},
	]
}

fn call_with_flags(
	input: &IVesting::IVestingCalls,
	read_only: bool,
	delegate: bool,
) -> Result<alloc::vec::Vec<u8>, pallet_revive::precompiles::Error> {
	let mut call_setup = pallet_revive::call_builder::CallSetup::<Test>::default();
	call_setup.set_read_only(read_only);
	call_setup.set_delegate_call(delegate);
	let (mut ext, _) = call_setup.ext();
	Vesting::<Test>::call(&Vesting::<Test>::MATCHER.base_address(), input, &mut ext)
}

fn assert_guard(
	case: &GuardTestCase,
	read_only: bool,
	delegate: bool,
	should_reject: bool,
	expected_error: sp_runtime::DispatchError,
) {
	let context = if read_only { "read-only" } else { "delegate call" };
	let result = call_with_flags(&case.input, read_only, delegate);
	if should_reject {
		match result {
			Err(pallet_revive::precompiles::Error::Error(err)) => {
				assert_eq!(err.error, expected_error, "{}: wrong error in {context}", case.name);
			},
			Err(other) => panic!("{}: unexpected error type in {context}: {other:?}", case.name),
			Ok(_) => panic!("{}: should be rejected in {context}", case.name),
		}
	} else {
		assert!(result.is_ok(), "{}: should succeed in {context}, got: {:?}", case.name, result);
	}
}

#[test]
fn read_only_guards() {
	new_test_ext().execute_with(|| {
		let error = pallet_revive::Error::<Test>::StateChangeDenied.into();
		for case in guard_test_cases() {
			assert_guard(&case, true, false, case.reject_read_only, error);
		}
	});
}

#[test]
fn delegate_call_guards() {
	new_test_ext().execute_with(|| {
		let error = pallet_revive::Error::<Test>::PrecompileDelegateDenied.into();
		for case in guard_test_cases() {
			assert_guard(&case, false, true, case.reject_delegate, error);
		}
	});
}
