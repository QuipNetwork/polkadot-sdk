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
	AccountInfo, AccountInfoOf, BalanceOf, BalanceWithDust, Code, CodeInfoOf, Config, H160, H256,
	HoldReason, Pallet, StorageDeposit, U256,
	address::{AddressMapper, create1},
	metering::TransactionLimits,
	storage::AccountType,
	test_utils::{ALICE, BOB, BOB_ADDR, DJANGO, DJANGO_ADDR, WEIGHT_LIMIT, builder::Contract},
	tests::{
		Balances, BurnDestination, Contracts, ExtBuilder, RuntimeFreezeReason, RuntimeHoldReason,
		RuntimeOrigin, System, Test, builder, pallet_dummy,
		test_utils::{
			get_balance, get_balance_on_hold, get_code_deposit, get_contract, get_contract_checked,
			set_balance_with_dust,
		},
	},
};
use alloy_core::sol_types::{SolCall, SolConstructor};
use frame_support::{
	assert_ok,
	traits::{
		LockableCurrency, OnIdle, WithdrawReasons,
		fungible::{Inspect, InspectFreeze, Mutate, MutateFreeze, MutateHold},
	},
	weights::Weight,
};
use pallet_revive_fixtures::{
	FixtureType, Terminate, TerminateCaller, TerminateCombined, TerminateCombinedCaller,
	TerminateDelegator, compile_module_with_type,
};
use pretty_assertions::assert_eq;
use test_case::{test_case, test_matrix};

/// Decode a contract return value into an error string.
fn decode_error(output: &[u8]) -> String {
	use alloy_core::sol_types::SolError;
	alloy_core::sol! { error Error(string); }
	Error::abi_decode_validate(output).unwrap().0
}

const METHOD_PRECOMPILE: u8 = 0;
const METHOD_DELEGATE_CALL: u8 = 1;
const METHOD_SYSCALL: u8 = 2;

#[test_matrix(
	[FixtureType::Solc, FixtureType::Resolc],
	[METHOD_PRECOMPILE, METHOD_SYSCALL]
)]
fn base_case(fixture_type: FixtureType, method: u8) {
	let (code, _) = compile_module_with_type("Terminate", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(
				Terminate::constructorCall {
					skip: true,
					method,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_contract();

		let result = builder::bare_call(addr)
			.data(
				Terminate::terminateCall { method, beneficiary: DJANGO_ADDR.0.into() }.abi_encode(),
			)
			.build_and_unwrap_result();

		assert!(result.data.is_empty());
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn precompile_fails_in_constructor(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("Terminate", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let result = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(
				Terminate::constructorCall {
					skip: false,
					method: METHOD_PRECOMPILE,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		assert!(result.result.did_revert());
		assert_eq!(
			decode_error(result.result.data.as_ref()),
			"terminate pre-compile cannot be called from the constructor"
		);
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn syscall_passes_in_constructor(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("Terminate", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let result = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(
				Terminate::constructorCall {
					skip: false,
					method: METHOD_SYSCALL,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		assert!(!result.result.did_revert());
		assert!(result.result.data.is_empty());
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn precompile_fails_for_direct_delegate(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("Terminate", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(
				Terminate::constructorCall {
					skip: true,
					method: METHOD_PRECOMPILE,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_contract();

		let result = builder::bare_call(addr)
			.data(
				Terminate::terminateCall {
					method: METHOD_DELEGATE_CALL,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		assert!(result.did_revert());
		assert_eq!(
			decode_error(result.data.as_ref()),
			"illegal to call this pre-compile via delegate call",
		);
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn precompile_fails_for_indirect_delegate(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("Terminate", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(
				Terminate::constructorCall {
					skip: true,
					method: METHOD_PRECOMPILE,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_contract();

		let result = builder::bare_call(addr)
			.data(
				Terminate::indirectDelegateTerminateCall { beneficiary: DJANGO_ADDR.0.into() }
					.abi_encode(),
			)
			.build_and_unwrap_result();

		assert!(result.did_revert());
		assert_eq!(
			decode_error(result.data.as_ref()),
			"illegal to call this pre-compile via delegate call",
		);
	});
}

/// In this test TerminateDelegator terminates itself by making a delegatecall to Terminate.
/// The SYSCALL terminate method shall work in this case because TerminateDelegator is created and
/// terminated in the same tx.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn syscall_passes_for_direct_delegate_same_tx(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("Terminate", fixture_type).unwrap();
	let (caller_code, _) = compile_module_with_type("TerminateCaller", fixture_type).unwrap();
	let (delegator_code, _) = compile_module_with_type("TerminateDelegator", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		if fixture_type == FixtureType::Resolc {
			// Need to pre-upload code for PVM
			let _ = <Pallet<Test>>::upload_code(
				RuntimeOrigin::signed(ALICE.clone()),
				code.clone(),
				<BalanceOf<Test>>::MAX,
			);
			let _ = <Pallet<Test>>::upload_code(
				RuntimeOrigin::signed(ALICE.clone()),
				delegator_code.clone(),
				<BalanceOf<Test>>::MAX,
			);
		}

		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code))
				.native_value(125)
				.build_and_unwrap_contract();

		let result = builder::bare_call(caller_addr)
			.data(
				TerminateCaller::delegateCallTerminateCall {
					value: alloy_core::primitives::U256::from(123_000_000u64),
					method: METHOD_SYSCALL,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();
		assert!(!result.did_revert());

		let decoded =
			TerminateCaller::delegateCallTerminateCall::abi_decode_returns(&result.data).unwrap();
		let addr = H160::from_slice(decoded._1.as_slice());
		let delegator_addr = H160::from_slice(decoded._0.as_slice());

		assert_eq!(
			get_balance(&DJANGO),
			123 + min_balance,
			"unexpected django balance after terminate"
		);
		assert!(
			get_contract_checked(&addr).is_some(),
			"Terminate contract should still exist after terminate"
		);
		assert!(
			get_contract_checked(&delegator_addr).is_none(),
			"TerminateDelegator contract should not exist after terminate"
		);
	});
}

/// In this test TerminateDelegator terminates itself by making a delegatecall to Terminate.
/// The SYSCALL shall send funds from TerminateDelegator to the beneficiary but TerminateDelegator
/// shall not be truly terminated.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn syscall_passes_for_direct_delegate(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("Terminate", fixture_type).unwrap();
	let (delegator_code, _) = compile_module_with_type("TerminateDelegator", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.constructor_data(
				Terminate::constructorCall {
					skip: true,
					method: METHOD_SYSCALL,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_contract();
		let Contract { addr: delegator_addr, .. } =
			builder::bare_instantiate(Code::Upload(delegator_code.clone()))
				.native_value(123)
				.build_and_unwrap_contract();
		let account_delegator = <Test as Config>::AddressMapper::to_account_id(&delegator_addr);

		let result = builder::bare_call(delegator_addr)
			.data(
				TerminateDelegator::delegateCallTerminateCall {
					terminate_addr: addr.0.into(),
					method: METHOD_SYSCALL,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();
		assert!(!result.did_revert());

		assert_eq!(
			get_balance(&DJANGO),
			123 + min_balance,
			"unexpected django balance after terminate"
		);
		assert_eq!(
			get_balance(&account_delegator),
			min_balance,
			"unexpected delegator balance after terminate"
		);
		assert!(
			get_contract_checked(&addr).is_some(),
			"Terminate contract should still exist after terminate"
		);
		assert!(
			get_contract_checked(&delegator_addr).is_some(),
			"TerminateDelegator contract should still exist after terminate"
		);
	});
}

#[test_matrix(
	[FixtureType::Solc, FixtureType::Resolc],
	[FixtureType::Solc, FixtureType::Resolc]
)]
fn terminate_shall_rollback_if_subsequent_frame_fails(
	caller_type: FixtureType,
	callee_type: FixtureType,
) {
	let (code, _) = compile_module_with_type("Terminate", callee_type).unwrap();
	let (caller_code, _) = compile_module_with_type("TerminateCaller", caller_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(
				Terminate::constructorCall {
					skip: true,
					method: METHOD_PRECOMPILE,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_contract();
		let account = <Test as Config>::AddressMapper::to_account_id(&addr);

		assert!(get_contract_checked(&addr).is_some(), "contract does not exist after create");
		assert_eq!(get_balance(&account), min_balance, "unexpected contract balance after create");

		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code))
				.native_value(125)
				.build_and_unwrap_contract();
		let caller_account = <Test as Config>::AddressMapper::to_account_id(&caller_addr);

		let result = builder::bare_call(caller_addr)
			.data(
				TerminateCaller::revertAfterTerminateCall {
					terminate_addr: addr.0.into(),
					method: METHOD_PRECOMPILE,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		assert!(result.did_revert(), "revertAfterTerminateCall did not revert");
		assert!(
			get_contract_checked(&addr).is_some(),
			"contract does not exist after reverted terminate"
		);
		assert_eq!(
			get_balance(&account),
			min_balance,
			"unexpected contract balance after reverted terminate"
		);

		assert_eq!(get_balance(&DJANGO), 0, "unexpected DJANGO balance after reverted terminate");

		assert_eq!(
			get_balance(&caller_account),
			125 + min_balance,
			"unexpected caller balance after reverted terminate"
		);
	});
}

/// This test does the following in the same transaction:
/// 1. deploy Terminate contract
/// 2. terminate the Terminate contract
/// 3. send funds to the Terminate contract
/// The funds that were sent after termination shall be credited to the beneficiary.
/// Deploying an EVM contract from a PVM contract (or vice versa) is not supported.
#[test_matrix(
	[FixtureType::Solc, FixtureType::Resolc],
	[METHOD_PRECOMPILE, METHOD_SYSCALL]
)]
fn sent_funds_after_terminate_shall_be_credited_to_beneficiary_base_case(
	fixture_type: FixtureType,
	method: u8,
) {
	let (code, _) = compile_module_with_type("Terminate", fixture_type).unwrap();
	let (caller_code, _) = compile_module_with_type("TerminateCaller", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		if fixture_type == FixtureType::Resolc {
			// Need to pre-upload code for PVM
			let _ = <Pallet<Test>>::upload_code(
				RuntimeOrigin::signed(ALICE.clone()),
				code.clone(),
				<BalanceOf<Test>>::MAX,
			);
		}
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code))
				.native_value(125)
				.build_and_unwrap_contract();
		let caller_account = <Test as Config>::AddressMapper::to_account_id(&caller_addr);

		assert_eq!(
			get_balance(&caller_account),
			125 + min_balance,
			"unexpected caller balance before terminate"
		);

		let result = builder::bare_call(caller_addr)
			.data(
				TerminateCaller::sendFundsAfterTerminateAndCreateCall {
					value: alloy_core::primitives::U256::from(123_000_000u64),
					method,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		assert!(
			!result.did_revert(),
			"sendFundsAfterTerminateAndCreateCall reverted: {}",
			decode_error(&result.data)
		);
		let decoded =
			TerminateCaller::sendFundsAfterTerminateAndCreateCall::abi_decode_returns(&result.data)
				.unwrap();
		let addr = H160::from_slice(decoded.0.as_slice());
		assert!(get_contract_checked(&addr).is_none(), "contract still exists after terminate");
		assert_eq!(
			get_balance(&DJANGO),
			123 + min_balance,
			"unexpected DJANGO balance after terminate"
		);
		let account = <Test as Config>::AddressMapper::to_account_id(&addr);
		assert_eq!(get_balance(&account), 0, "ucontract has balance after terminate");
	});
}

/// This test does *not* create and terminate the Terminate contract in the same transaction.
/// Therefore, the SYSCALL terminate method does not be transferred to beneficiary.
#[test_matrix(
	[FixtureType::Solc, FixtureType::Resolc],
	[FixtureType::Solc, FixtureType::Resolc]
)]
fn sent_funds_after_terminate_shall_be_credited_to_beneficiary_precompile(
	caller_type: FixtureType,
	callee_type: FixtureType,
) {
	let (code, _) = compile_module_with_type("Terminate", callee_type).unwrap();
	let (caller_code, _) = compile_module_with_type("TerminateCaller", caller_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(
				Terminate::constructorCall {
					skip: true,
					method: METHOD_PRECOMPILE,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_contract();
		let account = <Test as Config>::AddressMapper::to_account_id(&addr);

		assert!(get_contract_checked(&addr).is_some(), "contract does not exist after create");
		assert_eq!(get_balance(&account), min_balance, "unexpected contract balance after create");

		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code))
				.native_value(125)
				.build_and_unwrap_contract();
		let caller_account = <Test as Config>::AddressMapper::to_account_id(&caller_addr);

		assert_eq!(
			get_balance(&caller_account),
			125 + min_balance,
			"unexpected caller balance before terminate"
		);

		let result = builder::bare_call(caller_addr)
			.data(
				TerminateCaller::sendFundsAfterTerminateCall {
					terminate_addr: addr.0.into(),
					value: alloy_core::primitives::U256::from(123_000_000u64),
					method: METHOD_PRECOMPILE,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		assert!(
			!result.did_revert(),
			"sendFundsAfterTerminateCall reverted: {}",
			decode_error(&result.data)
		);
		assert!(
			result.data.is_empty(),
			"sendFundsAfterTerminateCall returned unexpected data: {:?}",
			result.data
		);
		assert!(get_contract_checked(&addr).is_none(), "contract still exists after terminate");
		assert_eq!(
			get_balance(&DJANGO),
			123 + min_balance,
			"unexpected DJANGO balance after terminate"
		);
		assert_eq!(get_balance(&account), 0, "contract has balance after terminate");
	});
}

/// This test does *not* create and terminate the Terminate contract in the same transaction.
/// Therefore, the SYSCALL terminate method does not be transferred to beneficiary.
#[test_matrix(
	[FixtureType::Solc, FixtureType::Resolc],
	[FixtureType::Solc, FixtureType::Resolc]
)]
fn sent_funds_after_terminate_shall_not_be_credited_to_beneficiary_syscall(
	caller_type: FixtureType,
	callee_type: FixtureType,
) {
	let (code, _) = compile_module_with_type("Terminate", callee_type).unwrap();
	let (caller_code, _) = compile_module_with_type("TerminateCaller", caller_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(
				Terminate::constructorCall {
					skip: true,
					method: METHOD_PRECOMPILE,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_contract();
		let account = <Test as Config>::AddressMapper::to_account_id(&addr);

		assert!(get_contract_checked(&addr).is_some(), "contract does not exist after create");
		assert_eq!(get_balance(&account), min_balance, "unexpected contract balance after create");

		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code))
				.native_value(125)
				.build_and_unwrap_contract();
		let caller_account = <Test as Config>::AddressMapper::to_account_id(&caller_addr);

		assert_eq!(
			get_balance(&caller_account),
			125 + min_balance,
			"unexpected caller balance before terminate"
		);

		let result = builder::bare_call(caller_addr)
			.data(
				TerminateCaller::sendFundsAfterTerminateCall {
					terminate_addr: addr.0.into(),
					value: alloy_core::primitives::U256::from(123_000_000u64),
					method: METHOD_SYSCALL,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		assert!(
			!result.did_revert(),
			"sendFundsAfterTerminateCall reverted: {}",
			decode_error(&result.data)
		);
		assert!(
			result.data.is_empty(),
			"sendFundsAfterTerminateCall returned unexpected data: {:?}",
			result.data
		);
		assert!(get_contract_checked(&addr).is_some(), "contract does not exist after terminate");
		assert_eq!(get_balance(&DJANGO), 0, "unexpected DJANGO balance after terminate");
		assert_eq!(
			get_balance(&account),
			123 + min_balance,
			"unexpected contract balance after terminate"
		);
	});
}

#[test_matrix(
	[FixtureType::Solc, FixtureType::Resolc],
	[METHOD_SYSCALL, METHOD_PRECOMPILE],
	[METHOD_SYSCALL, METHOD_PRECOMPILE]
)]
fn terminate_twice(fixture_type: FixtureType, method1: u8, method2: u8) {
	let (code, _) = compile_module_with_type("Terminate", fixture_type).unwrap();
	let (caller_code, _) = compile_module_with_type("TerminateCaller", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		if fixture_type == FixtureType::Resolc {
			// Need to pre-upload code for PVM
			let _ = <Pallet<Test>>::upload_code(
				RuntimeOrigin::signed(ALICE.clone()),
				code.clone(),
				<BalanceOf<Test>>::MAX,
			);
		}
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code))
				.native_value(125)
				.build_and_unwrap_contract();
		let result = builder::bare_call(caller_addr)
			.data(
				TerminateCaller::createAndTerminateTwiceCall {
					value: alloy_core::primitives::U256::from(123_000_000u64),
					method1,
					method2,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();
		assert!(
			!result.did_revert(),
			"createAndTerminateTwiceCall reverted: {}",
			decode_error(&result.data)
		);

		let decoded =
			TerminateCaller::createAndTerminateTwiceCall::abi_decode_returns(&result.data).unwrap();
		let addr = H160::from_slice(decoded.0.as_slice());
		let account = <Test as Config>::AddressMapper::to_account_id(&addr);
		assert!(get_contract_checked(&addr).is_none(), "contract still exists after terminate");
		assert_eq!(get_balance(&account), 0, "unexpected contract balance after terminate");
		assert_eq!(
			get_balance(&DJANGO),
			123 + min_balance,
			"unexpected DJANGO balance after terminate"
		);
	});
}

#[test_matrix(
	[FixtureType::Solc, FixtureType::Resolc],
	[METHOD_SYSCALL, METHOD_PRECOMPILE]
)]
fn call_after_terminate_works(fixture_type: FixtureType, method: u8) {
	let (code, _) = compile_module_with_type("Terminate", fixture_type).unwrap();
	let (caller_code, _) = compile_module_with_type("TerminateCaller", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let expected_value = alloy_core::primitives::U256::from(0xDEADBEEFu64);

		if fixture_type == FixtureType::Resolc {
			// Need to pre-upload code for PVM
			let _ = <Pallet<Test>>::upload_code(
				RuntimeOrigin::signed(ALICE.clone()),
				code.clone(),
				<BalanceOf<Test>>::MAX,
			);
		}
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();
		let result = builder::bare_call(caller_addr)
			.data(
				TerminateCaller::callAfterTerminateCall { value: expected_value, method }
					.abi_encode(),
			)
			.build_and_unwrap_result();
		assert!(
			!result.did_revert(),
			"callAfterTerminateCall reverted: {}",
			decode_error(&result.data)
		);

		let decoded =
			TerminateCaller::callAfterTerminateCall::abi_decode_returns(&result.data).unwrap();
		let addr = H160::from_slice(decoded._0.as_slice());
		let account = <Test as Config>::AddressMapper::to_account_id(&addr);
		let value = decoded._1;
		assert_eq!(value, expected_value, "unexpected return value from callAfterTerminateCall");
		assert_eq!(get_balance(&account), 0, "unexpected contract balance after terminate");
		assert_eq!(get_balance(&DJANGO), 0, "unexpected DJANGO balance after terminate");
	});
}

/// Something on the contract account that the contract did not put there itself.
#[derive(Clone, Copy, Debug)]
enum Encumbrance {
	/// Nothing: the control case.
	None,
	/// A lock of the given amount placed by another pallet, as a vested transfer does.
	Lock(u128),
	/// A hold of the given amount under a reason other than the storage deposit.
	Hold(u128),
	/// A freeze of the given amount under a reason pallet-revive does not own.
	Freeze(u128),
	/// A consumer placed by another pallet without any lock, hold or freeze.
	Consumer,
}

/// A lock identifier pallet-revive does not own.
const FOREIGN_LOCK: [u8; 8] = *b"foreign ";

/// The balance a `TerminateCombined` contract from [`encumbered_contract`] holds on top of the ED.
const SPENDABLE: u128 = 1_000;

/// An amount below one native unit, which only the dust of an account can hold.
const DUST: u32 = 123_456;

/// The EVM value of `value` in the native currency plus `dust`.
fn evm_value(value: u128, dust: u32) -> U256 {
	Pallet::<Test>::convert_native_to_evm(BalanceWithDust::new_unchecked::<Test>(value, dust))
}

/// The account entry a terminated contract leaves behind: a plain account if `dust` is left on
/// it, otherwise none.
fn leftover_account_info(dust: u32) -> Option<AccountInfo<Test>> {
	(dust != 0).then(|| AccountInfo { account_type: AccountType::EOA, dust })
}

fn storage_hold() -> RuntimeHoldReason {
	HoldReason::StorageDepositReserve.into()
}

fn foreign_hold() -> RuntimeHoldReason {
	pallet_dummy::HoldReason::Foreign.into()
}

fn foreign_freeze() -> RuntimeFreezeReason {
	pallet_dummy::FreezeReason::Foreign.into()
}

fn terminate_constructor(skip: bool, method: u8) -> Vec<u8> {
	TerminateCombined::constructorCall { skip, method, beneficiary: DJANGO_ADDR.0.into() }
		.abi_encode()
}

/// Deploy a `TerminateCombined` contract with [`SPENDABLE`] on top of the ED and apply
/// `encumbrance`.
///
/// The beneficiary and the payout recipient exist already, so that transfers to them do not
/// charge their ED to the origin.
fn encumbered_contract(fixture_type: FixtureType, encumbrance: Encumbrance) -> Contract<Test> {
	let (code, _) = compile_module_with_type("TerminateCombined", fixture_type).unwrap();
	let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
	let _ = <Test as Config>::Currency::set_balance(&DJANGO, 1_000_000);
	let _ = <Test as Config>::Currency::set_balance(&BOB, 1_000_000);

	let contract = builder::bare_instantiate(Code::Upload(code))
		.constructor_data(terminate_constructor(true, METHOD_PRECOMPILE))
		.build_and_unwrap_contract();

	let _ = <Test as Config>::Currency::set_balance(
		&contract.account_id,
		Contracts::min_balance() + SPENDABLE,
	);
	match encumbrance {
		Encumbrance::None => {},
		Encumbrance::Lock(amount) => {
			Balances::set_lock(FOREIGN_LOCK, &contract.account_id, amount, WithdrawReasons::all())
		},
		Encumbrance::Hold(amount) => {
			assert_ok!(Balances::hold(&foreign_hold(), &contract.account_id, amount));
		},
		Encumbrance::Freeze(amount) => {
			assert_ok!(Balances::set_freeze(&foreign_freeze(), &contract.account_id, amount));
		},
		Encumbrance::Consumer => {
			assert_ok!(System::inc_consumers(&contract.account_id));
		},
	}
	contract
}

fn call_terminate(addr: H160, method: u8) -> crate::ExecReturnValue {
	builder::bare_call(addr)
		.data(
			TerminateCombined::terminateCall { method, beneficiary: DJANGO_ADDR.0.into() }
				.abi_encode(),
		)
		.build_and_unwrap_result()
}

/// `System.terminate` succeeds and deletes the contract despite encumbrances the contract did
/// not create. Only what the encumbrance pins stays on the account.
///
/// The storage deposit is refunded to the origin before the payout, as far as the freeze
/// allows. A lock the free balance covers leaves the whole deposit to the origin: a lock of 1
/// only keeps the ED on the account, a lock of 500 also comes out of the payout. Refunding after
/// the payout would instead leave the hold to cover the lock of 500 and take it from the origin.
/// A lock of 1,000,000 pins the whole account: nothing is refunded or paid out. A freeze behaves
/// like a lock of the same amount. A foreign hold stays in place and only the free balance is paid
/// out.
///
/// See <https://github.com/paritytech/polkadot-sdk/issues/13017>.
#[test_matrix(
	[FixtureType::Solc, FixtureType::Resolc],
	[
		Encumbrance::None,
		Encumbrance::Lock(1),
		Encumbrance::Lock(500),
		Encumbrance::Lock(1_000_000),
		Encumbrance::Freeze(1),
		Encumbrance::Freeze(500),
		Encumbrance::Freeze(1_000_000),
		Encumbrance::Hold(1),
		Encumbrance::Consumer,
	]
)]
fn precompile_terminate_with_encumbered_balance(
	fixture_type: FixtureType,
	encumbrance: Encumbrance,
) {
	ExtBuilder::default().build().execute_with(|| {
		let Contract { addr, account_id } = encumbered_contract(fixture_type, encumbrance);
		let ed = Contracts::min_balance();
		let deposit = get_balance_on_hold(&storage_hold(), &account_id);
		let code_deposit = get_code_deposit(&get_contract(&addr).code_hash);
		let alice_before = get_balance(&ALICE);
		let django_before = get_balance(&DJANGO);

		let result = call_terminate(addr, METHOD_PRECOMPILE);

		let (refunded, paid_out, left) = match encumbrance {
			Encumbrance::None => (deposit, SPENDABLE, 0),
			Encumbrance::Lock(frozen) | Encumbrance::Freeze(frozen) if frozen <= ed + SPENDABLE => {
				let pinned = frozen.max(ed);
				(deposit, ed + SPENDABLE - pinned, pinned)
			},
			Encumbrance::Lock(_) | Encumbrance::Freeze(_) => (0, 0, ed + SPENDABLE + deposit),
			Encumbrance::Hold(amount) => (deposit, SPENDABLE - amount, ed + amount),
			Encumbrance::Consumer => (deposit, SPENDABLE, ed),
		};
		assert!(!result.did_revert(), "terminate must succeed with {encumbrance:?}");
		assert!(get_contract_checked(&addr).is_none(), "contract must be deleted");
		assert_eq!(get_balance(&DJANGO) - django_before, paid_out, "beneficiary payout");
		assert_eq!(
			get_balance(&ALICE) - alice_before,
			refunded + code_deposit,
			"origin must get the storage deposit back",
		);
		assert_eq!(
			get_balance_on_hold(&storage_hold(), &account_id),
			deposit - refunded,
			"only the part the freeze needs stays on hold",
		);
		assert_eq!(Balances::total_balance(&account_id), left, "balance left on the account");
		match encumbrance {
			Encumbrance::Hold(amount) => assert_eq!(
				get_balance_on_hold(&foreign_hold(), &account_id),
				amount,
				"the foreign hold must stay in place",
			),
			Encumbrance::Freeze(amount) => assert_eq!(
				Balances::balance_frozen(&foreign_freeze(), &account_id),
				amount,
				"the foreign freeze must stay in place",
			),
			Encumbrance::Consumer => {
				assert_eq!(System::consumers(&account_id), 1, "the foreign consumer must stay")
			},
			Encumbrance::None | Encumbrance::Lock(_) => {},
		}
	});
}

/// Funds that arrive after `System.terminate` go to the beneficiary as long as the balance covers
/// the lock or freeze.
///
/// The ED is 50 and the late funds are either below it or above it. The sweep sends everything
/// above the ED and what an encumbrance pins, before the ED is burned. Without an encumbrance the
/// burn then reaps the account, so nothing is left. An encumbrance keeps the account alive, so the
/// ED is kept, and the sweep still sent the whole late amount. A lock or freeze of 1,000,000 is
/// more than the whole balance: the sweep sends nothing and the late funds stay on the account.
/// Late funds that are not a multiple of the native-to-EVM ratio carry dust. No encumbrance
/// applies to the dust, so the sweep always sends it and the account keeps no entry for it.
#[test_matrix(
	[FixtureType::Solc, FixtureType::Resolc],
	[
		Encumbrance::None,
		Encumbrance::Lock(500),
		Encumbrance::Lock(1_000_000),
		Encumbrance::Freeze(500),
		Encumbrance::Freeze(1_000_000),
		Encumbrance::Hold(1),
		Encumbrance::Consumer,
	],
	[(43, 0), (57, 0), (43, DUST)]
)]
fn precompile_terminate_with_encumbered_balance_and_late_funds(
	fixture_type: FixtureType,
	encumbrance: Encumbrance,
	(late, late_dust): (u128, u32),
) {
	let (caller_code, _) = compile_module_with_type("TerminateCaller", fixture_type).unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let Contract { addr, account_id } = encumbered_contract(fixture_type, encumbrance);
		let ed = Contracts::min_balance();
		let deposit = get_balance_on_hold(&storage_hold(), &account_id);
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code))
				// Sending dust burns one more unit.
				.native_value(late + 1)
				.build_and_unwrap_contract();
		let django_before = get_balance(&DJANGO);
		let django_evm_before = Pallet::<Test>::evm_balance(&DJANGO_ADDR);

		let result = builder::bare_call(caller_addr)
			.data(
				TerminateCaller::sendFundsAfterTerminateCall {
					terminate_addr: addr.0.into(),
					value: alloy_core::primitives::U256::from_limbs(evm_value(late, late_dust).0),
					method: METHOD_PRECOMPILE,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		let (paid_out, swept, left) = match encumbrance {
			Encumbrance::None => (SPENDABLE, late, 0),
			Encumbrance::Lock(frozen) | Encumbrance::Freeze(frozen) if frozen <= ed + SPENDABLE => {
				let pinned = frozen.max(ed);
				(ed + SPENDABLE - pinned, late, pinned)
			},
			Encumbrance::Lock(_) | Encumbrance::Freeze(_) => {
				(0, 0, ed + SPENDABLE + deposit + late)
			},
			Encumbrance::Hold(amount) => (SPENDABLE - amount, late, ed + amount),
			Encumbrance::Consumer => (SPENDABLE, late, ed),
		};
		assert!(!result.did_revert(), "call must succeed: {}", decode_error(&result.data));
		assert!(get_contract_checked(&addr).is_none(), "contract must be deleted");
		assert_eq!(
			get_balance(&DJANGO) - django_before,
			paid_out + swept,
			"beneficiary gets the payout and the late funds the encumbrance does not pin",
		);
		assert_eq!(
			Pallet::<Test>::evm_balance(&DJANGO_ADDR) - django_evm_before,
			evm_value(paid_out + swept, late_dust),
			"beneficiary gets the dust of the late funds as well",
		);
		assert_eq!(Balances::total_balance(&account_id), left, "balance left on the account");
		assert_eq!(AccountInfoOf::<Test>::get(addr), None, "no dust is left on the account");
		match encumbrance {
			Encumbrance::None => {},
			Encumbrance::Lock(amount) => assert!(
				Balances::locks(&account_id)
					.iter()
					.any(|lock| lock.id == FOREIGN_LOCK && lock.amount == amount),
				"the foreign lock must stay in place",
			),
			Encumbrance::Hold(amount) => assert_eq!(
				get_balance_on_hold(&foreign_hold(), &account_id),
				amount,
				"the foreign hold must stay in place",
			),
			Encumbrance::Freeze(amount) => assert_eq!(
				Balances::balance_frozen(&foreign_freeze(), &account_id),
				amount,
				"the foreign freeze must stay in place",
			),
			Encumbrance::Consumer => {
				assert_eq!(System::consumers(&account_id), 1, "the foreign consumer must stay")
			},
		}
	});
}

/// Funds that arrive after `System.terminate` stay on the account if sending them fails. Here the
/// beneficiary does not exist and the deposit limit does not cover the ED that creating it
/// charges, as in #13039.
///
/// The ED is 50. The ED stays with the late funds, with or without a lock or freeze of 1, so the
/// balance of the address is exactly the late funds. This includes late funds that are not a
/// multiple of the native-to-EVM ratio: their dust stays on the account as well, also when it is
/// all that is left above the ED.
#[test_matrix(
	[FixtureType::Solc, FixtureType::Resolc],
	[Encumbrance::None, Encumbrance::Lock(1), Encumbrance::Freeze(1)],
	[(43, 0), (57, 0), (43, DUST), (0, DUST)]
)]
fn precompile_terminate_when_late_funds_cannot_be_sent(
	fixture_type: FixtureType,
	encumbrance: Encumbrance,
	(late, late_dust): (u128, u32),
) {
	let (caller_code, _) = compile_module_with_type("TerminateCaller", fixture_type).unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let Contract { addr, account_id } = encumbered_contract(fixture_type, encumbrance);
		let ed = Contracts::min_balance();
		// Nothing to pay out when `System.terminate` is called. The payout would create the
		// beneficiary already.
		let _ = <Test as Config>::Currency::set_balance(&account_id, ed);
		let deposit = get_balance_on_hold(&storage_hold(), &account_id);
		let code_deposit = get_code_deposit(&get_contract(&addr).code_hash);
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code))
				// Sending dust burns one more unit.
				.native_value(late + 1)
				.build_and_unwrap_contract();
		let beneficiary = H160::from([0x42u8; 20]);
		let beneficiary_account = <Test as Config>::AddressMapper::to_account_id(&beneficiary);
		let alice_before = get_balance(&ALICE);

		let result = builder::bare_call(caller_addr)
			.data(
				TerminateCaller::sendFundsAfterTerminateCall {
					terminate_addr: addr.0.into(),
					value: alloy_core::primitives::U256::from_limbs(evm_value(late, late_dust).0),
					method: METHOD_PRECOMPILE,
					beneficiary: beneficiary.0.into(),
				}
				.abi_encode(),
			)
			.transaction_limits(TransactionLimits::WeightAndDeposit {
				weight_limit: WEIGHT_LIMIT,
				deposit_limit: 0,
			})
			.build_and_unwrap_result();

		assert!(!result.did_revert(), "call must succeed: {}", decode_error(&result.data));
		assert!(get_contract_checked(&addr).is_none(), "contract must be deleted");
		assert_eq!(get_balance(&beneficiary_account), 0, "beneficiary must not be created");
		assert_eq!(
			get_balance(&ALICE) - alice_before,
			deposit + code_deposit,
			"origin must get the storage deposit back",
		);
		assert_eq!(Balances::total_balance(&account_id), ed + late, "the ED stays");
		assert_eq!(Pallet::<Test>::evm_balance(&addr), evm_value(late, late_dust), "late funds");
		assert_eq!(AccountInfoOf::<Test>::get(addr), leftover_account_info(late_dust));
	});
}

/// The contract pays out `address(this).balance` before it calls `System.terminate` under a
/// lock. The free balance then no longer covers the lock, so the part of the storage deposit
/// that the lock needs stays on hold and only the rest is refunded. What stays behind is exactly
/// the locked amount, so only the one who placed the lock loses anything.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn precompile_terminate_after_payout_refunds_partially(fixture_type: FixtureType) {
	ExtBuilder::default().build().execute_with(|| {
		let Contract { addr, account_id } = encumbered_contract(fixture_type, Encumbrance::None);
		let ed = Contracts::min_balance();
		let deposit = get_balance_on_hold(&storage_hold(), &account_id);
		let code_deposit = get_code_deposit(&get_contract(&addr).code_hash);
		// Covered by the ED and half of the storage deposit once the balance is paid out.
		let lock = ed + deposit / 2;
		Balances::set_lock(FOREIGN_LOCK, &account_id, lock, WithdrawReasons::all());
		let alice_before = get_balance(&ALICE);
		let django_before = get_balance(&DJANGO);

		let result = builder::bare_call(addr)
			.data(
				TerminateCombined::payoutAndTerminateCall {
					to: BOB_ADDR.0.into(),
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		let refunded = deposit - deposit / 2;
		assert!(!result.did_revert(), "terminate must succeed: {}", decode_error(&result.data));
		assert!(get_contract_checked(&addr).is_none(), "contract must be deleted");
		assert_eq!(get_balance(&DJANGO), django_before, "nothing is left to pay out");
		assert_eq!(get_balance(&ALICE) - alice_before, refunded + code_deposit);
		assert_eq!(get_balance_on_hold(&storage_hold(), &account_id), deposit / 2);
		assert_eq!(Balances::total_balance(&account_id), lock, "only the locked amount stays");
	});
}

/// `SELFDESTRUCT` of a contract created in the same transaction deletes it even under a lock.
/// The balance moves to the beneficiary when the opcode executes, and the lock keeps the ED on
/// the account.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn syscall_same_tx_with_lock(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("TerminateCombined", fixture_type).unwrap();
	let (caller_code, _) =
		compile_module_with_type("TerminateCombinedCaller", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let ed = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let _ = <Test as Config>::Currency::set_balance(&DJANGO, 1_000_000);
		if fixture_type == FixtureType::Resolc {
			// Need to pre-upload code for PVM
			assert_ok!(<Pallet<Test>>::upload_code(
				RuntimeOrigin::signed(ALICE.clone()),
				code,
				<BalanceOf<Test>>::MAX,
			));
		}
		let Contract { addr: caller_addr, account_id: caller_account } =
			builder::bare_instantiate(Code::Upload(caller_code))
				.native_value(SPENDABLE)
				.build_and_unwrap_contract();

		// Lock the account the caller is about to create. A lock needs an existing account, and
		// instantiation takes it over.
		let addr = create1(&caller_addr, System::account_nonce(&caller_account).into());
		let account_id = <Test as Config>::AddressMapper::to_account_id(&addr);
		let _ = <Test as Config>::Currency::set_balance(&account_id, ed);
		Balances::set_lock(FOREIGN_LOCK, &account_id, 1, WithdrawReasons::all());
		let django_before = get_balance(&DJANGO);

		let result = builder::bare_call(caller_addr)
			.data(
				TerminateCombinedCaller::createAndTerminateCall {
					value: alloy_core::primitives::U256::from_limbs(
						Pallet::<Test>::convert_native_to_evm(SPENDABLE).0,
					),
					method: METHOD_SYSCALL,
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		assert!(
			!result.did_revert(),
			"createAndTerminate reverted: {}",
			decode_error(&result.data)
		);
		let created =
			TerminateCombinedCaller::createAndTerminateCall::abi_decode_returns(&result.data)
				.unwrap();
		assert_eq!(H160::from_slice(created.as_slice()), addr);
		assert!(get_contract_checked(&addr).is_none(), "contract must be deleted");
		assert_eq!(get_balance(&DJANGO) - django_before, SPENDABLE);
		assert_eq!(Balances::total_balance(&account_id), ed, "the lock keeps the ED");
	});
}

/// `SELFDESTRUCT` of a pre-existing contract under a lock keeps EIP-6780 semantics: the balance
/// moves to the beneficiary, and the contract keeps its code and its storage deposit.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn syscall_pre_existing_with_lock(fixture_type: FixtureType) {
	ExtBuilder::default().build().execute_with(|| {
		let Contract { addr, account_id } = encumbered_contract(fixture_type, Encumbrance::Lock(1));
		let deposit = get_balance_on_hold(&storage_hold(), &account_id);
		let code_hash = get_contract(&addr).code_hash;
		let refcount = CodeInfoOf::<Test>::get(code_hash).unwrap().refcount();
		let django_before = get_balance(&DJANGO);

		let result = call_terminate(addr, METHOD_SYSCALL);

		assert!(!result.did_revert());
		assert!(get_contract_checked(&addr).is_some(), "contract must stay");
		assert_eq!(get_balance(&DJANGO) - django_before, SPENDABLE);
		assert_eq!(get_balance_on_hold(&storage_hold(), &account_id), deposit);

		// The code is kept: the contract still runs it and holds its reference.
		let result = builder::bare_call(addr)
			.data(TerminateCombined::echoCall { value: 7.try_into().unwrap() }.abi_encode())
			.build_and_unwrap_result();
		assert!(!result.did_revert());
		let echoed = TerminateCombined::echoCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(echoed, alloy_core::primitives::U256::from(7));
		assert_eq!(get_contract(&addr).code_hash, code_hash);
		assert_eq!(CodeInfoOf::<Test>::get(code_hash).unwrap().refcount(), refcount);
	});
}

/// A lock can keep the account of a terminated contract alive as a plain account. A contract
/// deployed to the same address later takes it over.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn redeploy_onto_leftover_account(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("TerminateCombined", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let Contract { addr, account_id } = encumbered_contract(fixture_type, Encumbrance::Lock(1));
		assert!(!call_terminate(addr, METHOD_PRECOMPILE).did_revert());
		assert!(get_contract_checked(&addr).is_none(), "contract must be deleted");
		assert_eq!(Balances::total_balance(&account_id), Contracts::min_balance());

		// The deletion queue has to clear the old contract's deposit bookkeeping first.
		Contracts::on_idle(System::block_number(), Weight::MAX);

		let redeployed = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(terminate_constructor(true, METHOD_PRECOMPILE))
			.build_and_unwrap_contract();

		assert_eq!(redeployed.addr, addr);
		let result = builder::bare_call(addr)
			.data(TerminateCombined::echoCall { value: 7.try_into().unwrap() }.abi_encode())
			.build_and_unwrap_result();
		assert!(!result.did_revert());
		let echoed = TerminateCombined::echoCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(echoed, alloy_core::primitives::U256::from(7));
	});
}

/// A contract that terminates to itself with nothing but dust above the ED leaves the ED and the
/// dust on the account. A contract deployed to the same address later takes over both, without
/// minting another ED.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn redeploy_onto_leftover_dust(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("TerminateCombined", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let Contract { addr, account_id } = encumbered_contract(fixture_type, Encumbrance::None);
		set_balance_with_dust(&addr, BalanceWithDust::new_unchecked::<Test>(0, DUST));
		let result = builder::bare_call(addr)
			.data(
				TerminateCombined::terminateCall {
					method: METHOD_PRECOMPILE,
					beneficiary: addr.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();
		assert!(!result.did_revert(), "call must succeed: {}", decode_error(&result.data));
		assert!(get_contract_checked(&addr).is_none(), "contract must be deleted");
		assert_eq!(Balances::total_balance(&account_id), Contracts::min_balance());
		assert_eq!(AccountInfoOf::<Test>::get(addr), leftover_account_info(DUST));

		// The deletion queue has to clear the old contract's deposit bookkeeping first.
		Contracts::on_idle(System::block_number(), Weight::MAX);

		let redeployed = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(terminate_constructor(true, METHOD_PRECOMPILE))
			.build_and_unwrap_contract();

		assert_eq!(redeployed.addr, addr);
		assert!(get_contract_checked(&addr).is_some(), "contract must be deployed");
		assert_eq!(
			Pallet::<Test>::evm_balance(&addr),
			evm_value(0, DUST),
			"the contract has the dust and no second ED",
		);
	});
}

/// Fund the account `OnBurn` resolves to, so that burned amounts below the ED reach it as well,
/// and return its balance.
fn fund_burn_destination() -> u128 {
	let _ = <Test as Config>::Currency::set_balance(&BurnDestination::get(), 1_000_000);
	get_balance(&BurnDestination::get())
}

/// The amount `OnBurn` received since [`fund_burn_destination`] returned `before`.
fn burned(before: u128) -> u128 {
	get_balance(&BurnDestination::get()) - before
}

/// `System.terminate` with the contract itself as the beneficiary deletes the contract and leaves
/// its balance on the account, including funds that arrive after the call, as in EIP-8246.
/// Nothing is burned through `OnBurn`. The ED is 50. With a zero balance the ED is burned and the
/// account is reaped. With any other balance the ED stays with it, so the balance of the address
/// does not change apart from the late funds. This includes a balance that is not a multiple of
/// the native-to-EVM ratio, whose dust stays as well, also when it is all there is above the ED.
/// Under a lock of 500 the ED stays as well.
#[test_matrix(
	[FixtureType::Solc, FixtureType::Resolc],
	[
		(Encumbrance::None, 0, 0, 0, 0),
		(Encumbrance::None, 43, 0, 0, 50 + 43),
		(Encumbrance::None, 43, 7, 0, 50 + 50),
		(Encumbrance::None, 57, 0, 0, 50 + 57),
		(Encumbrance::None, 43, 0, DUST, 50 + 43),
		(Encumbrance::None, 0, 0, DUST, 50),
		(Encumbrance::Lock(500), SPENDABLE, 7, 0, 50 + SPENDABLE + 7),
		(Encumbrance::Lock(500), SPENDABLE, 7, DUST, 50 + SPENDABLE + 7),
	]
)]
fn precompile_terminate_to_itself_keeps_balance(
	fixture_type: FixtureType,
	(encumbrance, spendable, late, dust, left): (Encumbrance, u128, u128, u32, u128),
) {
	let (caller_code, _) = compile_module_with_type("TerminateCaller", fixture_type).unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let Contract { addr, account_id } = encumbered_contract(fixture_type, encumbrance);
		set_balance_with_dust(&addr, BalanceWithDust::new_unchecked::<Test>(spendable, dust));
		let deposit = get_balance_on_hold(&storage_hold(), &account_id);
		let code_deposit = get_code_deposit(&get_contract(&addr).code_hash);
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code))
				.native_value(late)
				.build_and_unwrap_contract();
		let burned_before = fund_burn_destination();
		let alice_before = get_balance(&ALICE);

		let result = builder::bare_call(caller_addr)
			.data(
				TerminateCaller::sendFundsAfterTerminateCall {
					terminate_addr: addr.0.into(),
					value: alloy_core::primitives::U256::from_limbs(
						Pallet::<Test>::convert_native_to_evm(late).0,
					),
					method: METHOD_PRECOMPILE,
					beneficiary: addr.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		// The lock pins more than the ED, so the contract sees only what is left above the lock.
		let seen = match encumbrance {
			Encumbrance::Lock(lock) => left - lock,
			_ => spendable + late,
		};
		assert!(!result.did_revert(), "call must succeed: {}", decode_error(&result.data));
		assert!(get_contract_checked(&addr).is_none(), "contract must be deleted");
		assert_eq!(burned(burned_before), 0, "nothing is burned");
		assert_eq!(Balances::total_balance(&account_id), left, "balance left on the account");
		assert_eq!(System::account_exists(&account_id), left != 0, "reaped only when empty");
		assert_eq!(Pallet::<Test>::evm_balance(&addr), evm_value(seen, dust), "balance of addr");
		assert_eq!(AccountInfoOf::<Test>::get(addr), leftover_account_info(dust));
		assert_eq!(
			get_balance(&ALICE) - alice_before,
			deposit + code_deposit,
			"origin must get the storage deposit back",
		);
	});
}

/// A contract created in the same transaction that terminates with itself as the beneficiary,
/// through `System.terminate` or `SELFDESTRUCT`, is deleted and its balance stays on the account
/// together with the ED, as in EIP-8246. This includes the dust of a balance that is not a
/// multiple of the native-to-EVM ratio. With a zero balance the account is reaped.
#[test_matrix(
	[FixtureType::Solc, FixtureType::Resolc],
	[METHOD_PRECOMPILE, METHOD_SYSCALL],
	[(0, 0), (SPENDABLE, 0), (SPENDABLE, DUST), (0, DUST)]
)]
fn same_tx_terminate_to_itself_keeps_balance(
	fixture_type: FixtureType,
	method: u8,
	(value, dust): (u128, u32),
) {
	let (code, _) = compile_module_with_type("TerminateCombined", fixture_type).unwrap();
	let (caller_code, _) =
		compile_module_with_type("TerminateCombinedCaller", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		if fixture_type == FixtureType::Resolc {
			// Need to pre-upload code for PVM
			assert_ok!(<Pallet<Test>>::upload_code(
				RuntimeOrigin::signed(ALICE.clone()),
				code,
				<BalanceOf<Test>>::MAX,
			));
		}
		let Contract { addr: caller_addr, account_id: caller_account } =
			builder::bare_instantiate(Code::Upload(caller_code))
				// Sending dust burns one more unit.
				.native_value(SPENDABLE + 1)
				.build_and_unwrap_contract();
		let addr = create1(&caller_addr, System::account_nonce(&caller_account).into());
		let account_id = <Test as Config>::AddressMapper::to_account_id(&addr);
		let burned_before = fund_burn_destination();

		let result = builder::bare_call(caller_addr)
			.data(
				TerminateCombinedCaller::createAndTerminateCall {
					value: alloy_core::primitives::U256::from_limbs(evm_value(value, dust).0),
					method,
					beneficiary: addr.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		let left = if value == 0 && dust == 0 { 0 } else { Contracts::min_balance() + value };
		assert!(!result.did_revert(), "call must succeed: {}", decode_error(&result.data));
		let created =
			TerminateCombinedCaller::createAndTerminateCall::abi_decode_returns(&result.data)
				.unwrap();
		assert_eq!(H160::from_slice(created.as_slice()), addr);
		assert!(get_contract_checked(&addr).is_none(), "contract must be deleted");
		assert_eq!(burned(burned_before), 0, "nothing is burned");
		assert_eq!(Balances::total_balance(&account_id), left, "the balance stays with the ED");
		assert_eq!(System::account_exists(&account_id), left != 0, "reaped only when empty");
		assert_eq!(Pallet::<Test>::evm_balance(&addr), evm_value(value, dust), "balance of addr");
		assert_eq!(AccountInfoOf::<Test>::get(addr), leftover_account_info(dust));
	});
}

/// `SELFDESTRUCT` of a pre-existing contract with itself as the beneficiary changes nothing, as
/// in EIP-6780: the contract stays, nothing is burned and the balance and deposit stay.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn syscall_pre_existing_to_itself_changes_nothing(fixture_type: FixtureType) {
	ExtBuilder::default().build().execute_with(|| {
		let Contract { addr, account_id } = encumbered_contract(fixture_type, Encumbrance::None);
		let deposit = get_balance_on_hold(&storage_hold(), &account_id);
		let total = Balances::total_balance(&account_id);
		let burned_before = fund_burn_destination();

		let result = builder::bare_call(addr)
			.data(
				TerminateCombined::terminateCall {
					method: METHOD_SYSCALL,
					beneficiary: addr.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		assert!(!result.did_revert(), "call must succeed: {}", decode_error(&result.data));
		assert!(get_contract_checked(&addr).is_some(), "contract must stay");
		assert_eq!(burned(burned_before), 0, "nothing is burned");
		assert_eq!(Balances::total_balance(&account_id), total, "the balance stays");
		assert_eq!(get_balance_on_hold(&storage_hold(), &account_id), deposit);
	});
}

/// A contract terminated twice in one transaction sends the funds that arrive afterwards to the
/// beneficiary it named last. A pre-existing contract calls `System.terminate` with `DJANGO` and
/// runs `SELFDESTRUCT` with itself as the beneficiary, in either order, and then receives late
/// funds. They stay on the account with the ED if it named itself last, as in EIP-8246, and go to
/// `DJANGO` otherwise. Either way the contract is deleted and its deposit is refunded once.
#[test_matrix([FixtureType::Solc, FixtureType::Resolc], [false, true])]
fn terminate_twice_late_funds_stay_or_go_to_last_beneficiary(
	fixture_type: FixtureType,
	itself_last: bool,
) {
	let (caller_code, _) =
		compile_module_with_type("TerminateCombinedCaller", fixture_type).unwrap();
	let late = 7;
	ExtBuilder::default().build().execute_with(|| {
		let Contract { addr, account_id } = encumbered_contract(fixture_type, Encumbrance::None);
		let deposit = get_balance_on_hold(&storage_hold(), &account_id);
		let code_deposit = get_code_deposit(&get_contract(&addr).code_hash);
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code))
				.native_value(late)
				.build_and_unwrap_contract();
		let burned_before = fund_burn_destination();
		let alice_before = get_balance(&ALICE);
		let django_before = get_balance(&DJANGO);

		let to_django = (METHOD_PRECOMPILE, DJANGO_ADDR.0.into());
		let to_itself = (METHOD_SYSCALL, addr.0.into());
		let ((method1, beneficiary1), (method2, beneficiary2)) =
			if itself_last { (to_django, to_itself) } else { (to_itself, to_django) };
		let result = builder::bare_call(caller_addr)
			.data(
				TerminateCombinedCaller::terminateTwiceAndSendFundsCall {
					terminate_addr: addr.0.into(),
					value: alloy_core::primitives::U256::from_limbs(
						Pallet::<Test>::convert_native_to_evm(late).0,
					),
					method1,
					beneficiary1,
					method2,
					beneficiary2,
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		let ed = Contracts::min_balance();
		let (to_beneficiary, left, seen) =
			if itself_last { (SPENDABLE, ed + late, late) } else { (SPENDABLE + late, 0, 0) };
		assert!(!result.did_revert(), "call must succeed: {}", decode_error(&result.data));
		assert!(get_contract_checked(&addr).is_none(), "contract must be deleted");
		assert_eq!(get_balance(&DJANGO) - django_before, to_beneficiary, "beneficiary payout");
		assert_eq!(burned(burned_before), 0, "nothing is burned");
		assert_eq!(Balances::total_balance(&account_id), left, "balance left on the account");
		assert_eq!(Pallet::<Test>::evm_balance(&addr), evm_value(seen, 0), "balance of addr");
		assert_eq!(
			get_balance(&ALICE) - alice_before,
			deposit + code_deposit,
			"origin must get the storage deposit back once",
		);
	});
}

/// Only the first `System.terminate` of a contract refunds its storage deposit. Terminating it
/// twice in one call must still report that refund once.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn precompile_terminate_twice_reports_refund_once(fixture_type: FixtureType) {
	let storage_deposit = |data: Vec<u8>| {
		ExtBuilder::default().build().execute_with(|| {
			let Contract { addr, .. } = encumbered_contract(fixture_type, Encumbrance::None);
			let result = builder::bare_call(addr).data(data).build();
			assert!(!result.result.unwrap().did_revert());
			result.storage_deposit
		})
	};

	let once = storage_deposit(
		TerminateCombined::terminateCall {
			method: METHOD_PRECOMPILE,
			beneficiary: DJANGO_ADDR.0.into(),
		}
		.abi_encode(),
	);
	let twice = storage_deposit(
		TerminateCombined::terminateTwiceCall { beneficiary: DJANGO_ADDR.0.into() }.abi_encode(),
	);

	assert!(matches!(once, StorageDeposit::Refund(amount) if amount > 0), "{once:?}");
	assert_eq!(twice, once);
}

fn terminate_then_selfdestruct(via_delegate_call: bool) -> Vec<u8> {
	TerminateCombined::terminateThenSelfdestructCall {
		beneficiary: DJANGO_ADDR.0.into(),
		viaDelegateCall: via_delegate_call,
	}
	.abi_encode()
}

/// Call the pre-existing contract from [`encumbered_contract`] with `data`, which terminates it,
/// and check that it is deleted and its storage deposit is refunded exactly once.
fn terminate_pre_existing(fixture_type: FixtureType, data: Vec<u8>) {
	ExtBuilder::default().build().execute_with(|| {
		let Contract { addr, account_id } = encumbered_contract(fixture_type, Encumbrance::None);
		let deposit = get_balance_on_hold(&storage_hold(), &account_id);
		let code_deposit = get_code_deposit(&get_contract(&addr).code_hash);
		let alice_before = get_balance(&ALICE);
		let django_before = get_balance(&DJANGO);

		let result = builder::bare_call(addr).data(data).build();
		let output = result.result.unwrap();

		assert!(!output.did_revert(), "terminate must succeed: {}", decode_error(&output.data));
		assert!(get_contract_checked(&addr).is_none(), "contract must be deleted");
		assert_eq!(get_balance(&DJANGO) - django_before, SPENDABLE, "beneficiary payout");
		assert_eq!(
			get_balance(&ALICE) - alice_before,
			deposit + code_deposit,
			"origin must get the storage deposit back once",
		);
		assert_eq!(get_balance_on_hold(&storage_hold(), &account_id), 0);
		assert_eq!(Balances::total_balance(&account_id), 0, "nothing is left on the account");
		assert_eq!(result.storage_deposit, StorageDeposit::Refund(deposit));
	})
}

/// `System.terminate` followed by `SELFDESTRUCT` deletes a pre-existing contract, whether the
/// `SELFDESTRUCT` runs in the same frame or in a delegate call. The `SELFDESTRUCT` must not turn
/// the termination into one that only applies to a contract created in the same transaction: the
/// deposit was already refunded when `System.terminate` was called.
#[test_matrix([FixtureType::Solc, FixtureType::Resolc], [false, true])]
fn precompile_terminate_then_syscall_pre_existing(
	fixture_type: FixtureType,
	via_delegate_call: bool,
) {
	terminate_pre_existing(fixture_type, terminate_then_selfdestruct(via_delegate_call));
}

/// `SELFDESTRUCT` in a delegate call followed by `System.terminate` deletes a pre-existing
/// contract. The `SELFDESTRUCT` schedules a termination that only applies to a contract created in
/// the same transaction. The later `System.terminate` makes it unconditional and refunds the
/// deposit once.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn syscall_then_precompile_terminate_pre_existing(fixture_type: FixtureType) {
	terminate_pre_existing(
		fixture_type,
		TerminateCombined::selfdestructThenTerminateCall { beneficiary: DJANGO_ADDR.0.into() }
			.abi_encode(),
	);
}

/// `System.terminate` followed by `SELFDESTRUCT` deletes a contract created in the same
/// transaction, with the same balances and storage deposit as `System.terminate` alone.
#[test_matrix([FixtureType::Solc, FixtureType::Resolc], [false, true])]
fn precompile_terminate_then_syscall_same_tx(fixture_type: FixtureType, via_delegate_call: bool) {
	let (code, _) = compile_module_with_type("TerminateCombined", fixture_type).unwrap();
	let (caller_code, _) =
		compile_module_with_type("TerminateCombinedCaller", fixture_type).unwrap();
	let value = alloy_core::primitives::U256::from_limbs(
		Pallet::<Test>::convert_native_to_evm(SPENDABLE).0,
	);
	let terminate = |data: Vec<u8>| {
		ExtBuilder::default().build().execute_with(|| {
			let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
			let _ = <Test as Config>::Currency::set_balance(&DJANGO, 1_000_000);
			if fixture_type == FixtureType::Resolc {
				// Need to pre-upload code for PVM
				assert_ok!(<Pallet<Test>>::upload_code(
					RuntimeOrigin::signed(ALICE.clone()),
					code.clone(),
					<BalanceOf<Test>>::MAX,
				));
			}
			let Contract { addr: caller_addr, .. } =
				builder::bare_instantiate(Code::Upload(caller_code.clone()))
					.native_value(SPENDABLE)
					.build_and_unwrap_contract();
			let alice_before = get_balance(&ALICE);
			let django_before = get_balance(&DJANGO);

			let result = builder::bare_call(caller_addr).data(data).build();
			let output = result.result.unwrap();

			assert!(!output.did_revert(), "terminate must succeed: {}", decode_error(&output.data));
			let created =
				TerminateCombinedCaller::createAndTerminateCall::abi_decode_returns(&output.data)
					.unwrap();
			let addr = H160::from_slice(created.as_slice());
			let account_id = <Test as Config>::AddressMapper::to_account_id(&addr);
			assert!(get_contract_checked(&addr).is_none(), "contract must be deleted");
			assert_eq!(get_balance(&DJANGO) - django_before, SPENDABLE, "beneficiary payout");
			assert_eq!(Balances::total_balance(&account_id), 0, "nothing is left on the account");
			// Signed: with PVM the pre-uploaded code is removed and its deposit refunded.
			(result.storage_deposit, get_balance(&ALICE) as i128 - alice_before as i128)
		})
	};

	let once = terminate(
		TerminateCombinedCaller::createAndTerminateCall {
			value,
			method: METHOD_PRECOMPILE,
			beneficiary: DJANGO_ADDR.0.into(),
		}
		.abi_encode(),
	);
	let then_syscall = terminate(
		TerminateCombinedCaller::createAndTerminateThenSelfdestructCall {
			value,
			beneficiary: DJANGO_ADDR.0.into(),
			viaDelegateCall: via_delegate_call,
		}
		.abi_encode(),
	);

	assert_eq!(then_syscall, once);
}

/// The refcount of every stored code.
fn code_infos() -> alloc::collections::BTreeMap<H256, u64> {
	CodeInfoOf::<Test>::iter().map(|(hash, info)| (hash, info.refcount())).collect()
}

/// `SELFDESTRUCT` in the constructor of a top-level instantiation deletes the contract it
/// creates. The value goes to the beneficiary and every deposit goes back to the origin.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn syscall_in_top_level_constructor_deletes_contract(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("TerminateCombined", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let _ = <Test as Config>::Currency::set_balance(&DJANGO, 1_000_000);
		let code_infos_before = code_infos();
		let alice_before = get_balance(&ALICE);
		let django_before = get_balance(&DJANGO);

		let result = builder::bare_instantiate(Code::Upload(code))
			.native_value(SPENDABLE)
			.constructor_data(terminate_constructor(false, METHOD_SYSCALL))
			.build();
		let output = result.result.unwrap();

		assert!(!output.result.did_revert(), "instantiate must succeed");
		let account_id = <Test as Config>::AddressMapper::to_account_id(&output.addr);
		assert!(get_contract_checked(&output.addr).is_none(), "contract must be deleted");
		assert_eq!(get_balance(&DJANGO) - django_before, SPENDABLE, "beneficiary payout");
		assert_eq!(Balances::total_balance(&account_id), 0, "nothing is left on the account");
		assert_eq!(
			alice_before - get_balance(&ALICE),
			SPENDABLE,
			"origin must only pay the value it sent",
		);
		assert_eq!(code_infos(), code_infos_before, "no code reference stays behind");
	});
}

/// `SELFDESTRUCT` in the constructor of a contract created by another contract deletes it. With
/// EVM the termination is scheduled while the new contract still points at the init code, so the
/// teardown has to release the runtime code the constructor returns. No code and no deposit stay
/// behind.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn syscall_in_nested_constructor_deletes_contract(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("TerminateCombined", fixture_type).unwrap();
	let (caller_code, _) =
		compile_module_with_type("TerminateCombinedCaller", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let _ = <Test as Config>::Currency::set_balance(&DJANGO, 1_000_000);
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code))
				.native_value(SPENDABLE)
				.build_and_unwrap_contract();
		let code_infos_before = code_infos();
		let alice_before = get_balance(&ALICE);
		let django_before = get_balance(&DJANGO);
		if fixture_type == FixtureType::Resolc {
			// Need to pre-upload code for PVM. Nothing else references it, so the teardown
			// removes it again and refunds its deposit.
			assert_ok!(<Pallet<Test>>::upload_code(
				RuntimeOrigin::signed(ALICE.clone()),
				code,
				<BalanceOf<Test>>::MAX,
			));
		}

		let result = builder::bare_call(caller_addr)
			.data(
				TerminateCombinedCaller::createAndSelfdestructInConstructorCall {
					value: alloy_core::primitives::U256::from_limbs(
						Pallet::<Test>::convert_native_to_evm(SPENDABLE).0,
					),
					beneficiary: DJANGO_ADDR.0.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		assert!(!result.did_revert(), "call must succeed: {}", decode_error(&result.data));
		let created =
			TerminateCombinedCaller::createAndSelfdestructInConstructorCall::abi_decode_returns(
				&result.data,
			)
			.unwrap();
		let addr = H160::from_slice(created.as_slice());
		let account_id = <Test as Config>::AddressMapper::to_account_id(&addr);
		assert!(get_contract_checked(&addr).is_none(), "contract must be deleted");
		assert_eq!(get_balance(&DJANGO) - django_before, SPENDABLE, "beneficiary payout");
		assert_eq!(Balances::total_balance(&account_id), 0, "nothing is left on the account");
		assert_eq!(code_infos(), code_infos_before, "no code reference stays behind");
		assert_eq!(get_balance(&ALICE), alice_before, "origin must get every deposit back");
	});
}
