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

use crate::{
	Code, Config, H160, HoldReason, StorageDeposit,
	address::AddressMapper,
	metering::TransactionLimits,
	test_utils::{ALICE, WEIGHT_LIMIT, builder::Contract},
	tests::{
		ExtBuilder, Test, builder,
		test_utils::{get_balance, get_balance_on_hold, get_contract, get_contract_checked},
	},
};
use alloy_core::sol_types::SolCall;
use frame_support::traits::fungible::Mutate;
use pallet_revive_fixtures::{FixtureType, TerminateStorageCaller, compile_module_with_type};
use pretty_assertions::assert_eq;
use test_case::test_case;

/// A call that writes storage and then terminates a contract must still charge and hold
/// the storage deposit for those writes when the deposit limit only covers the writes
/// themselves.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn storage_written_before_terminate_is_charged(fixture_type: FixtureType) {
	let (caller_code, _) =
		compile_module_with_type("TerminateStorageCaller", fixture_type).unwrap();
	let (inner_code, _) = compile_module_with_type("TerminateStorageInner", fixture_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let n: u64 = 4;

		let storage_hold: <Test as Config>::RuntimeHoldReason =
			HoldReason::StorageDepositReserve.into();

		// Cost of the `n` fresh writes on their own, measured with a fresh caller and a
		// generous limit.
		let Contract { addr: measure_caller, .. } =
			builder::bare_instantiate(Code::Upload(caller_code.clone()))
				.build_and_unwrap_contract();
		let measure = builder::bare_call(measure_caller)
			.data(TerminateStorageCaller::writeOnlyCall { n }.abi_encode())
			.build();
		assert!(measure.result.is_ok(), "measurement call failed: {:?}", measure.result);
		let cost = match measure.storage_deposit {
			StorageDeposit::Charge(cost) => cost,
			other => panic!("expected a charge from the writes, got {other:?}"),
		};
		assert!(cost > 0);

		// The inner contract starts with no balance, so its scheduled termination transfers
		// nothing immediately; the caller funds it after scheduling.
		let Contract { addr: inner, account_id: inner_account } =
			builder::bare_instantiate(Code::Upload(inner_code)).build_and_unwrap_contract();
		let inner_deposit = get_balance_on_hold(&storage_hold, &inner_account);

		// A second, identical caller (distinct salt for a distinct address) so its slots
		// are fresh again. Funded so it can transfer to the inner contract.
		let Contract { addr: caller, .. } = builder::bare_instantiate(Code::Upload(caller_code))
			.native_value(1_000_000_000)
			.salt(Some([1u8; 32]))
			.build_and_unwrap_contract();
		let caller_account = <Test as Config>::AddressMapper::to_account_id(&caller);

		let beneficiary = H160::from([0x42u8; 20]);
		let beneficiary_account = <Test as Config>::AddressMapper::to_account_id(&beneficiary);
		assert_eq!(get_balance(&beneficiary_account), 0, "beneficiary must not exist yet");

		// The storage deposit for the writes is held either from the origin or from the
		// contract account, so track both.
		let held_before = get_balance_on_hold(&storage_hold, &ALICE) +
			get_balance_on_hold(&storage_hold, &caller_account);

		let result = builder::bare_call(caller)
			.data(
				TerminateStorageCaller::writeAndTerminateCall {
					inner: inner.0.into(),
					n,
					beneficiary: beneficiary.0.into(),
				}
				.abi_encode(),
			)
			.transaction_limits(TransactionLimits::WeightAndDeposit {
				weight_limit: WEIGHT_LIMIT,
				deposit_limit: cost,
			})
			.build();

		assert!(result.result.is_ok(), "call failed: {:?}", result.result);

		// The writes are charged and backed by a real hold, not wiped to Charge(0). The inner
		// contract's deposit is refunded on top.
		assert_eq!(
			result.storage_deposit,
			StorageDeposit::Charge(cost).saturating_add(&StorageDeposit::Refund(inner_deposit)),
		);
		let held_after = get_balance_on_hold(&storage_hold, &ALICE) +
			get_balance_on_hold(&storage_hold, &caller_account);
		assert_eq!(held_after - held_before, cost, "the writes must be held");
		assert_eq!(
			get_contract(&caller).extra_deposit(),
			cost,
			"the caller's contract info deposit must match the hold",
		);

		// Sending the funds that arrived after scheduling would create the beneficiary, which
		// the limit does not cover. The funds stay on the inner account and the inner contract
		// is deleted anyway.
		assert!(get_contract_checked(&inner).is_none(), "inner contract must be deleted");
		assert_eq!(get_balance(&beneficiary_account), 0, "beneficiary must not be created");
		assert!(get_balance(&inner_account) > 0, "the funds must stay on the inner account");
	});
}
