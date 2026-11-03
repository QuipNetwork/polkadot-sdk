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

//! The pallet-revive shared VM integration test suite.

use core::iter;

use crate::{
	BalanceOf, Code, Config, DelegateInfo, DispatchError, Error, ExecConfig, ExecOrigin,
	ExecReturnValue, Weight,
	address::AddressMapper,
	evm::{decode_revert_reason, fees::InfoT},
	metering::TransactionLimits,
	test_utils::{ALICE, ALICE_ADDR, BOB_ADDR, WEIGHT_LIMIT, builder::Contract, deposit_limit},
	tests::{ExtBuilder, MOCK_CODE, MockHandlerImpl, RuntimeOrigin, Test, builder},
};
use alloy_core::{
	primitives::{Bytes, FixedBytes},
	sol_types::{Revert, SolCall, SolError, SolInterface},
};
use frame_support::{
	assert_err,
	traits::fungible::{Balanced, Inspect, Mutate},
};
use itertools::Itertools;
use pallet_revive_fixtures::{Callee, Caller, FixtureType, Host, compile_module_with_type};
use pallet_revive_uapi::ReturnFlags;
use pretty_assertions::assert_eq;
use sp_core::{H160, H256};
use test_case::test_case;

/// Tests that the `CALL` opcode works as expected by having one contract call another.
#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
#[test_case(FixtureType::Resolc, FixtureType::Solc;   "resolc->solc")]
#[test_case(FixtureType::Resolc, FixtureType::Resolc; "resolc->resolc")]
fn staticcall_works(caller_type: FixtureType, callee_type: FixtureType) {
	let (caller_code, _) = compile_module_with_type("Caller", caller_type).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", callee_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		// Instantiate the callee contract, which can echo a value.
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_code)).build_and_unwrap_contract();

		// Instantiate the caller contract.
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		let magic_number = 42u64;
		let result = builder::bare_call(caller_addr)
			.data(
				Caller::staticCallCall {
					_callee: callee_addr.0.into(),
					_data: Callee::echoCall { _data: magic_number }.abi_encode().into(),
					_gas: u64::MAX,
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		let result = Caller::staticCallCall::abi_decode_returns(&result.data).unwrap();
		assert!(result.success, "the call must succeed");
		let echo_output = Callee::echoCall::abi_decode_returns(&result.output).unwrap();
		assert_eq!(magic_number, echo_output, "the call must reproduce the magic number");

		let result = builder::bare_call(caller_addr)
			.data(
				Caller::staticCallCall {
					_callee: callee_addr.0.into(),
					_data: Callee::storeCall { _data: magic_number }.abi_encode().into(),
					_gas: u64::MAX,
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		let result = Caller::staticCallCall::abi_decode_returns(&result.data).unwrap();
		assert!(!result.success, "Can not store in static call");
	});
}

#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
#[test_case(FixtureType::Resolc, FixtureType::Solc;   "resolc->solc")]
#[test_case(FixtureType::Resolc, FixtureType::Resolc; "resolc->resolc")]
fn call_works(caller_type: FixtureType, callee_type: FixtureType) {
	let (caller_code, _) = compile_module_with_type("Caller", caller_type).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", callee_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		// Instantiate the callee contract, which can echo a value.
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_code)).build_and_unwrap_contract();

		// Instantiate the caller contract.
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		let magic_number = 42u64;
		let result = builder::bare_call(caller_addr)
			.data(
				Caller::normalCall {
					_callee: callee_addr.0.into(),
					_value: 0,
					_data: Callee::echoCall { _data: magic_number }.abi_encode().into(),
					_gas: u64::MAX,
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		let result = Caller::normalCall::abi_decode_returns(&result.data).unwrap();
		assert!(result.success, "the call must succeed");
		let echo_output = Callee::echoCall::abi_decode_returns(&result.output).unwrap();
		assert_eq!(magic_number, echo_output, "the call must reproduce the magic number");

		let result = builder::bare_call(caller_addr)
			.data(
				Caller::normalCall {
					_callee: callee_addr.0.into(),
					_value: 0,
					_data: Callee::storeCall { _data: magic_number }.abi_encode().into(),
					_gas: u64::MAX,
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		let result = Caller::normalCall::abi_decode_returns(&result.data).unwrap();
		assert!(result.success, "the store call must succeed");
	});
}

#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
#[test_case(FixtureType::Resolc, FixtureType::Solc;   "resolc->solc")]
#[test_case(FixtureType::Resolc, FixtureType::Resolc; "resolc->resolc")]
fn call_revert(caller_type: FixtureType, callee_type: FixtureType) {
	let (caller_code, _) = compile_module_with_type("Caller", caller_type).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", callee_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		// Instantiate the callee contract, which can echo a value.
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_code)).build_and_unwrap_contract();

		// Instantiate the caller contract.
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		// Call revert and assert failure
		let result = builder::bare_call(caller_addr)
			.data(
				Caller::normalCall {
					_callee: callee_addr.0.into(),
					_value: 0,
					_data: Callee::revertCall {}.abi_encode().into(),
					_gas: u64::MAX,
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();
		let result = Caller::normalCall::abi_decode_returns(&result.data).unwrap();
		assert!(!result.success, "Call should propagate revert");
		assert!(result.output.len() > 0, "Returned data should contain revert message");

		let data = result.output.as_ref();
		if data.len() >= 4 && &data[..4] == Revert::SELECTOR {
			let reason = decode_revert_reason(data).expect("Failed to decode revert reason");
			assert_eq!(reason, "revert: This is a revert");
		} else {
			panic!("Error selector not found in revert data");
		}
	});
}

#[test]
fn deploy_revert() {
	let (caller_code, _) = compile_module_with_type("Caller", FixtureType::Solc).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		let result = builder::bare_call(caller_addr)
			.data(Caller::createRevertCall {}.abi_encode())
			.build_and_unwrap_result();

		let data: &[u8] = result.data.as_ref();
		if data.len() >= 72 && &data[68..72] == Revert::SELECTOR {
			let reason = decode_revert_reason(&data[68..]).expect("Failed to decode revert reason");
			assert_eq!(reason, "revert: ChildRevert: revert in constructor");
		} else {
			panic!("Error selector not found at expected position 68");
		}
	});
}

// This test has a `caller` contract calling into a `callee` contract which then executes the
// INVALID opcode. INVALID consumes all gas which means that it will error with OutOfGas.
#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
#[test_case(FixtureType::Resolc, FixtureType::Solc;   "resolc->solc")]
#[test_case(FixtureType::Resolc, FixtureType::Resolc; "resolc->resolc")]
fn call_invalid_opcode(caller_type: FixtureType, callee_type: FixtureType) {
	let (caller_code, _) = compile_module_with_type("Caller", caller_type).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", callee_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		// Pass a large gas stipend to the callee
		let gas_limit = 200_000_000_000u64;

		// Instantiate the callee contract, which can echo a value.
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_code)).build_and_unwrap_contract();

		// Instantiate the caller contract.
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		let contract_result = builder::bare_call(caller_addr)
			.data(
				Caller::normalCall {
					_callee: callee_addr.0.into(),
					_value: 0,
					_data: Callee::invalidCall {}.abi_encode().into(),
					_gas: gas_limit,
				}
				.abi_encode(),
			)
			.build();

		let result = contract_result.result.expect("Outer call should succeed");
		assert!(
			contract_result.gas_consumed > gas_limit as u128,
			"Inner call should consume all forwarded gas. Consumed: {}, Limit: {}",
			contract_result.gas_consumed,
			gas_limit
		);
		let decoded = Caller::normalCall::abi_decode_returns(&result.data)
			.expect("Should decode return data");
		assert!(!decoded.success, "INVALID opcode should cause inner call to fail");
		assert!(decoded.output.is_empty(), "Output should be empty on INVALID opcode");
	});
}

#[test]
fn invalid_opcode_evm() {
	let (callee_code, _) = compile_module_with_type("Callee", FixtureType::Solc).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		// Instantiate the callee contract.
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_code)).build_and_unwrap_contract();

		let result = builder::bare_call(callee_addr)
			.data(Callee::invalidCall {}.abi_encode().into())
			.build();
		assert_err!(result.result, Error::<Test>::InvalidInstruction);
	});
}

#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
#[test_case(FixtureType::Resolc, FixtureType::Solc;   "resolc->solc")]
#[test_case(FixtureType::Resolc, FixtureType::Resolc; "resolc->resolc")]
fn call_stop_opcode(caller_type: FixtureType, callee_type: FixtureType) {
	let (caller_code, _) = compile_module_with_type("Caller", caller_type).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", callee_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		// Instantiate the callee contract.
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_code)).build_and_unwrap_contract();

		// Instantiate the caller contract.
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		let result = builder::bare_call(caller_addr)
			.data(
				Caller::normalCall {
					_callee: callee_addr.0.into(),
					_value: 0,
					_data: Callee::stopCall {}.abi_encode().into(),
					_gas: u64::MAX,
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();
		let result = Caller::normalCall::abi_decode_returns(&result.data).unwrap();

		assert!(result.success);

		let data = result.output.as_ref();
		assert!(data.iter().all(|&x| x == 0), "Returned data should be empty")
	});
}

#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
#[test_case(FixtureType::Resolc, FixtureType::Solc;   "resolc->solc")]
#[test_case(FixtureType::Resolc, FixtureType::Resolc; "resolc->resolc")]
fn delegatecall_works(caller_type: FixtureType, callee_type: FixtureType) {
	let (caller_code, _) = compile_module_with_type("Caller", caller_type).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", callee_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		// Instantiate the callee contract, which can echo a value.
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_code)).build_and_unwrap_contract();

		// Instantiate the caller contract.
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		let magic_number = 42u64;
		let result = builder::bare_call(caller_addr)
			.data(
				Caller::delegateCall {
					_callee: callee_addr.0.into(),
					_data: Callee::echoCall { _data: magic_number }.abi_encode().into(),
					_gas: u64::MAX,
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		let result = Caller::delegateCall::abi_decode_returns(&result.data).unwrap();
		assert!(result.success, "the call must succeed");
		let echo_output = Callee::echoCall::abi_decode_returns(&result.output).unwrap();
		assert_eq!(magic_number, echo_output, "the call must reproduce the magic number");

		let result = builder::bare_call(caller_addr)
			.data(
				Caller::delegateCall {
					_callee: callee_addr.0.into(),
					_data: Callee::whoSenderCall {}.abi_encode().into(),
					_gas: u64::MAX,
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		let result = Caller::delegateCall::abi_decode_returns(&result.data).unwrap();
		assert!(result.success, "the whoSender call must succeed");
		let decoded = Callee::whoSenderCall::abi_decode_returns(&result.output).unwrap();
		assert_eq!(ALICE_ADDR, H160::from_slice(decoded.as_slice()));
	});
}

#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
#[test_case(FixtureType::Resolc, FixtureType::Solc;   "resolc->solc")]
#[test_case(FixtureType::Resolc, FixtureType::Resolc; "resolc->resolc")]
fn mock_caller_hook_works(caller_type: FixtureType, callee_type: FixtureType) {
	let (caller_code, _) = compile_module_with_type("Caller", caller_type).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", callee_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		// Instantiate the callee contract, which can echo a value.
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_code)).build_and_unwrap_contract();

		// Instantiate the caller contract.
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		// Set BOB as the mock caller and check whoSender returns BOB's address.
		let result = builder::bare_call(caller_addr)
			.data(
				Caller::normalCall {
					_callee: callee_addr.0.into(),
					_data: Callee::whoSenderCall {}.abi_encode().into(),
					_gas: u64::MAX,
					_value: 0,
				}
				.abi_encode(),
			)
			.exec_config(ExecConfig {
				mock_handler: Some(Box::new(MockHandlerImpl {
					mock_caller: Some(BOB_ADDR),
					..Default::default()
				})),
				..Default::default()
			})
			.build_and_unwrap_result();

		let result = Caller::normalCall::abi_decode_returns(&result.data).unwrap();
		assert!(result.success, "the whoSender call must succeed");
		let decoded = Callee::whoSenderCall::abi_decode_returns(&result.output).unwrap();
		assert_eq!(BOB_ADDR, H160::from_slice(decoded.as_slice()));
	});
}

#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
#[test_case(FixtureType::Resolc, FixtureType::Solc;   "resolc->solc")]
#[test_case(FixtureType::Resolc, FixtureType::Resolc; "resolc->resolc")]
fn mock_call_hook_works(caller_type: FixtureType, callee_type: FixtureType) {
	let (caller_code, _) = compile_module_with_type("Caller", caller_type).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", callee_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		// Instantiate the callee contract, which can echo a value.
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_code)).build_and_unwrap_contract();

		// Instantiate the caller contract.
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		// Set up mocked_magic_number to be returned by the mock call handler and check that is
		// returned instead of magic_number.
		let magic_number = 42u64;
		let mocked_magic_number = 99u64;
		let result = builder::bare_call(caller_addr)
			.data(
				Caller::normalCall {
					_callee: callee_addr.0.into(),
					_value: 0,
					_data: Callee::echoCall { _data: magic_number }.abi_encode().into(),
					_gas: u64::MAX,
				}
				.abi_encode(),
			)
			.exec_config(ExecConfig {
				mock_handler: Some(Box::new(MockHandlerImpl {
					mock_call: iter::once((
						callee_addr,
						ExecReturnValue {
							flags: ReturnFlags::default(),
							data: alloy_core::sol_types::SolValue::abi_encode(&mocked_magic_number)
								.into(),
						},
					))
					.collect(),
					..Default::default()
				})),
				..Default::default()
			})
			.build_and_unwrap_result();

		let result = Caller::normalCall::abi_decode_returns(&result.data).unwrap();
		assert!(result.success, "the call must succeed");
		let echo_output = Callee::echoCall::abi_decode_returns(&result.output).unwrap();
		assert_eq!(mocked_magic_number, echo_output, "the call must reproduce the magic number");
	});
}

#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
#[test_case(FixtureType::Resolc, FixtureType::Solc;   "resolc->solc")]
#[test_case(FixtureType::Resolc, FixtureType::Resolc; "resolc->resolc")]
fn mock_delegatecall_hook_works(caller_type: FixtureType, callee_type: FixtureType) {
	let (caller_code, _) = compile_module_with_type("Caller", caller_type).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", callee_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		// Instantiate the callee contract, which can echo a value.
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_code)).build_and_unwrap_contract();

		// Instantiate the caller contract.
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		// Instantiate the call with an invalid callee and check that the delegatecall uses the
		// mocked callee address.
		let magic_number = 42u64;
		let result = builder::bare_call(caller_addr)
			.data(
				Caller::normalCall {
					_callee: caller_addr.0.into(), // Wrong callee, should be overridden by the mock hook.
					_value: 0,
					_data: Callee::echoCall { _data: magic_number }.abi_encode().into(),
					_gas: u64::MAX,
				}
				.abi_encode(),
			)
			.exec_config(ExecConfig {
				mock_handler: Some(Box::new(MockHandlerImpl {
					mock_delegate_caller: iter::once((
						Callee::echoCall { _data: magic_number }.abi_encode().into(),
						DelegateInfo {
							callee: callee_addr,
							caller: ExecOrigin::<Test>::from_runtime_origin(crate::OriginFor::<Test>::signed(
								<Test as crate::pallet::Config>::AddressMapper::to_fallback_account_id(
									&caller_addr,
								),
							)).expect("Conversion to ExecOrigin must work"),
						},
					))
					.collect(),
					..Default::default()
				})),
				..Default::default()
			})
			.build_and_unwrap_result();

		let result = Caller::normalCall::abi_decode_returns(&result.data).unwrap();
		assert!(result.success, "the call must succeed");
		let echo_output = Callee::echoCall::abi_decode_returns(&result.output).unwrap();
		assert_eq!(magic_number, echo_output, "the call must reproduce the magic number");
	});
}

#[test]
fn mocked_code_works() {
	let (host_code, _) = compile_module_with_type("Host", FixtureType::Solc).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		let Contract { addr: host_addr, .. } =
			builder::bare_instantiate(Code::Upload(host_code)).build_and_unwrap_contract();

		let mocked_addr = H160::from_slice(&[0x42; 20]);

		let expected_size = MOCK_CODE.len() as u64;
		let expected_hash = sp_io::hashing::keccak_256(&MOCK_CODE);

		// Test EXTCODESIZE with mocked address
		let result = builder::bare_call(host_addr)
			.data(Host::extcodesizeOpCall { account: mocked_addr.0.into() }.abi_encode())
			.exec_config(ExecConfig {
				mock_handler: Some(Box::new(MockHandlerImpl {
					mock_call: iter::once((mocked_addr, ExecReturnValue::default())).collect(),
					..Default::default()
				})),
				..Default::default()
			})
			.build_and_unwrap_result();

		let size = Host::extcodesizeOpCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(
			size, expected_size,
			"EXTCODESIZE should return {} for mocked address",
			expected_size
		);

		// Test EXTCODEHASH with mocked address
		let result = builder::bare_call(host_addr)
			.data(Host::extcodehashOpCall { account: mocked_addr.0.into() }.abi_encode())
			.exec_config(ExecConfig {
				mock_handler: Some(Box::new(MockHandlerImpl {
					mock_call: iter::once((mocked_addr, ExecReturnValue::default())).collect(),
					..Default::default()
				})),
				..Default::default()
			})
			.build_and_unwrap_result();

		let hash = Host::extcodehashOpCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(
			H256::from_slice(hash.as_slice()),
			H256::from_slice(&expected_hash),
			"EXTCODEHASH should return keccak256(MOCK_CODE) for mocked address"
		);

		// Verify that without mock handler, the same address returns 0 for code size
		let result = builder::bare_call(host_addr)
			.data(Host::extcodesizeOpCall { account: mocked_addr.0.into() }.abi_encode())
			.build_and_unwrap_result();

		let size = Host::extcodesizeOpCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(size, 0, "EXTCODESIZE should return 0 for unmocked address without code");
	});
}

#[test]
fn create_works() {
	let (caller_code, _) = compile_module_with_type("Caller", FixtureType::Solc).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", FixtureType::Solc).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000_000);

		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		let create_call_data =
			Caller::createCall { initcode: Bytes::from(callee_code.clone()) }.abi_encode();

		let result = builder::bare_call(caller_addr)
			.data(create_call_data)
			.native_value(1_000)
			.build_and_unwrap_result();

		let callee_addr = Caller::createCall::abi_decode_returns(&result.data).unwrap();
		let magic_number = 42u64;

		// Check if the created contract is working
		let echo_result = builder::bare_call(callee_addr.0.0.into())
			.data(Callee::echoCall { _data: magic_number }.abi_encode())
			.build_and_unwrap_result();

		let echo_output = Callee::echoCall::abi_decode_returns(&echo_result.data).unwrap();

		assert_eq!(magic_number, echo_output, "Callee.echo must return 42");
	});
}

#[test]
fn create2_works() {
	let (caller_code, _) = compile_module_with_type("Caller", FixtureType::Solc).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", FixtureType::Solc).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000_000);

		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		let salt = [42u8; 32];

		let initcode = Bytes::from(callee_code);
		// Prepare the CREATE2 call
		let create_call_data =
			Caller::create2Call { initcode: initcode.clone(), salt: FixedBytes(salt) }.abi_encode();

		let result = builder::bare_call(caller_addr)
			.data(create_call_data)
			.native_value(1000)
			.build_and_unwrap_result();

		let callee_addr = Caller::create2Call::abi_decode_returns(&result.data).unwrap();

		// Compute expected CREATE2 address
		let expected_addr = crate::address::create2(&caller_addr, &initcode, &[], &salt);

		let callee_addr: H160 = callee_addr.0.0.into();
		assert_eq!(callee_addr, expected_addr, "CREATE2 address should be deterministic");
		let magic_number = 42u64;

		// Check if the created contract is working
		let echo_result = builder::bare_call(callee_addr)
			.data(Callee::echoCall { _data: magic_number }.abi_encode())
			.build_and_unwrap_result();

		let echo_output = Callee::echoCall::abi_decode_returns(&echo_result.data).unwrap();

		assert_eq!(magic_number, echo_output, "Callee.echo must return 42");
	});
}

#[test]
fn instantiate_from_constructor_works() {
	use pallet_revive_fixtures::CallerWithConstructor::*;

	let (caller_code, _) =
		compile_module_with_type("CallerWithConstructor", FixtureType::Solc).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		let data = CallerWithConstructorCalls::callBar(callBarCall {}).abi_encode();
		let result = builder::bare_call(addr).data(data).build_and_unwrap_result();
		let result = callBarCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(result, 42u64);
	});
}

/// Root creates a contract via nested CREATE in block N and destroys it via the
/// system precompile in block N+1. Exercises the full `do_terminate` path under
/// Root and confirms the deposit waiver holds across both calls.
#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
#[test_case(FixtureType::Resolc, FixtureType::Resolc; "resolc->resolc")]
fn root_call_can_create_and_destroy_in_next_block(
	caller_type: FixtureType,
	callee_type: FixtureType,
) {
	use crate::{
		AccountInfo, HoldReason, Pallet,
		address::AddressMapper,
		test_utils::DJANGO_ADDR,
		tests::{System, initialize_block, test_utils::get_balance_on_hold},
	};
	use alloy_core::primitives::Address;
	use pallet_revive_fixtures::{
		NestedChild::{NestedChildCalls, destroyViaPrecompileCall},
		NestedDeployer::{NestedDeployerCalls, deployChildCall},
	};

	let (code, _) = compile_module_with_type("NestedDeployer", caller_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000_000);

		if caller_type == FixtureType::Resolc {
			let (child_code, _) = compile_module_with_type("NestedChild", callee_type).unwrap();
			Pallet::<Test>::upload_code(
				RuntimeOrigin::signed(ALICE.clone()),
				child_code,
				<BalanceOf<Test>>::MAX,
			)
			.unwrap();
		}

		let Contract { addr, account_id } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();
		let _ = <Test as Config>::Currency::set_balance(&account_id, 100_000_000_000_000);

		// Snapshot balances/holds the two Root calls must not touch.
		let storage_hold = HoldReason::StorageDepositReserve.into();
		let upload_hold = HoldReason::CodeUploadDepositReserve.into();
		let pallet_account = Pallet::<Test>::account_id();
		let deployer_storage_hold_before = get_balance_on_hold(&storage_hold, &account_id);
		let deployer_free_before = <<Test as Config>::Currency as Inspect<_>>::balance(&account_id);
		let pallet_upload_hold_before = get_balance_on_hold(&upload_hold, &pallet_account);

		// Block 1: Root creates the child via nested CREATE.
		let create_result = builder::bare_call(addr)
			.origin(RuntimeOrigin::root())
			.data(NestedDeployerCalls::deployChild(deployChildCall {}).abi_encode())
			.build_and_unwrap_result();
		assert!(!create_result.did_revert());
		let returned: Address = deployChildCall::abi_decode_returns(&create_result.data).unwrap();
		let child_addr = H160::from_slice(returned.as_slice());
		assert!(AccountInfo::<Test>::load_contract(&child_addr).is_some());

		// Deposits stayed waived across the Root create.
		let child_id = <Test as crate::Config>::AddressMapper::to_account_id(&child_addr);
		assert_eq!(get_balance_on_hold(&storage_hold, &child_id), 0);
		assert_eq!(get_balance_on_hold(&storage_hold, &account_id), deployer_storage_hold_before);
		assert_eq!(
			<<Test as Config>::Currency as Inspect<_>>::balance(&account_id),
			deployer_free_before,
		);

		// Block 2: Root tells the child to self-terminate via the system precompile.
		initialize_block(System::block_number() + 1);
		let destroy_result = builder::bare_call(child_addr)
			.origin(RuntimeOrigin::root())
			.data(
				NestedChildCalls::destroyViaPrecompile(destroyViaPrecompileCall {
					beneficiary: DJANGO_ADDR.0.into(),
				})
				.abi_encode(),
			)
			.build_and_unwrap_result();
		assert!(!destroy_result.did_revert(), "Root cross-tx terminate should succeed");

		assert!(
			AccountInfo::<Test>::load_contract(&child_addr).is_none(),
			"child must be destroyed by the cross-block terminate call",
		);

		// Deposits stayed waived across both Root calls.
		assert_eq!(get_balance_on_hold(&storage_hold, &child_id), 0);
		assert_eq!(get_balance_on_hold(&storage_hold, &account_id), deployer_storage_hold_before);
		assert_eq!(
			<<Test as Config>::Currency as Inspect<_>>::balance(&account_id),
			deployer_free_before,
		);
		if caller_type == FixtureType::Solc {
			assert_eq!(
				get_balance_on_hold(&upload_hold, &pallet_account),
				pallet_upload_hold_before
			);
		}
	});
}

/// Sibling of the cross-block test, but using the Solidity `selfdestruct` opcode
/// (`only_if_same_tx: true`). To actually reach `do_terminate` past the EIP-6780
/// gate at [exec.rs] `contracts_to_destroy`, creation and destruction must
/// happen in the same tx — covers the `only_if_same_tx: true` branch.
#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
#[test_case(FixtureType::Resolc, FixtureType::Resolc; "resolc->resolc")]
fn root_call_can_create_and_destroy_in_same_tx(caller_type: FixtureType, callee_type: FixtureType) {
	use crate::{
		AccountInfo, HoldReason, Pallet, address::AddressMapper, test_utils::DJANGO_ADDR,
		tests::test_utils::get_balance_on_hold,
	};
	use alloy_core::primitives::Address;
	use pallet_revive_fixtures::NestedDeployer::{NestedDeployerCalls, deployAndDestroyChildCall};

	let (code, _) = compile_module_with_type("NestedDeployer", caller_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000_000);

		if caller_type == FixtureType::Resolc {
			let (child_code, _) = compile_module_with_type("NestedChild", callee_type).unwrap();
			Pallet::<Test>::upload_code(
				RuntimeOrigin::signed(ALICE.clone()),
				child_code,
				<BalanceOf<Test>>::MAX,
			)
			.unwrap();
		}

		let Contract { addr, account_id } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();
		let _ = <Test as Config>::Currency::set_balance(&account_id, 100_000_000_000_000);

		let storage_hold = HoldReason::StorageDepositReserve.into();
		let upload_hold = HoldReason::CodeUploadDepositReserve.into();
		let pallet_account = Pallet::<Test>::account_id();
		let deployer_storage_hold_before = get_balance_on_hold(&storage_hold, &account_id);
		let deployer_free_before = <<Test as Config>::Currency as Inspect<_>>::balance(&account_id);
		let pallet_upload_hold_before = get_balance_on_hold(&upload_hold, &pallet_account);

		let result = builder::bare_call(addr)
			.origin(RuntimeOrigin::root())
			.data(
				NestedDeployerCalls::deployAndDestroyChild(deployAndDestroyChildCall {
					beneficiary: DJANGO_ADDR.0.into(),
				})
				.abi_encode(),
			)
			.build_and_unwrap_result();
		assert!(!result.did_revert(), "Root nested CREATE + SELFDESTRUCT should succeed");

		let returned: Address =
			deployAndDestroyChildCall::abi_decode_returns(&result.data).unwrap();
		let child_addr = H160::from_slice(returned.as_slice());

		// EIP-6780: created-and-destroyed in the same tx must actually remove the
		// contract. Before the do_terminate fix this silently failed under Root and
		// the ContractInfo stayed put.
		assert!(
			AccountInfo::<Test>::load_contract(&child_addr).is_none(),
			"child contract must have been terminated, not silently left on-chain",
		);

		let child_id = <Test as crate::Config>::AddressMapper::to_account_id(&child_addr);
		assert_eq!(get_balance_on_hold(&storage_hold, &child_id), 0);
		assert_eq!(get_balance_on_hold(&storage_hold, &account_id), deployer_storage_hold_before);
		assert_eq!(
			<<Test as Config>::Currency as Inspect<_>>::balance(&account_id),
			deployer_free_before,
		);
		if caller_type == FixtureType::Solc {
			assert_eq!(
				get_balance_on_hold(&upload_hold, &pallet_account),
				pallet_upload_hold_before
			);
		}
	});
}

/// No resolc caller since the subcall limiting is not implemented on resolc, yet.
#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
fn subcall_effectively_limited_substrate_tx(caller_type: FixtureType, callee_type: FixtureType) {
	let (caller_code, _) = compile_module_with_type("Caller", caller_type).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", callee_type).unwrap();

	let no_collection_config = ExecConfig::new_substrate_tx();
	let mut collection_config = ExecConfig::new_substrate_tx();
	collection_config.collect_deposit_from_hold = Some(Default::default());
	let configs = [no_collection_config, collection_config];

	let call_types =
		[Caller::CallType::Call, Caller::CallType::StaticCall, Caller::CallType::DelegateCall];

	struct Case {
		deposit_limit: BalanceOf<Test>,
		gas_divisor: u64,
		callee_input: Vec<u8>,
		result: Result<bool, DispatchError>,
		is_store_call: bool,
	}

	let test_cases = [
		Case {
			deposit_limit: deposit_limit::<Test>(),
			gas_divisor: 1,
			callee_input: Callee::consumeAllReftimeCall {}.abi_encode(),
			result: Err(<Error<Test>>::OutOfGas.into()),
			is_store_call: false,
		},
		Case {
			deposit_limit: deposit_limit::<Test>(),
			gas_divisor: 2,
			callee_input: Callee::consumeAllReftimeCall {}.abi_encode(),
			result: Ok(false),
			is_store_call: false,
		},
		Case {
			deposit_limit: deposit_limit::<Test>(),
			gas_divisor: u64::MAX,
			callee_input: Callee::consumeAllReftimeCall {}.abi_encode(),
			result: Ok(false),
			is_store_call: false,
		},
		Case {
			deposit_limit: 130,
			gas_divisor: 1,
			callee_input: Callee::storeCall { _data: 42 }.abi_encode(),
			result: Err(<Error<Test>>::StorageDepositLimitExhausted.into()),
			is_store_call: true,
		},
		Case {
			deposit_limit: 130,
			gas_divisor: 2,
			callee_input: Callee::storeCall { _data: 42 }.abi_encode(),
			result: Ok(false),
			is_store_call: true,
		},
		Case {
			deposit_limit: 130,
			gas_divisor: u64::MAX,
			callee_input: Callee::storeCall { _data: 42 }.abi_encode(),
			result: Ok(false),
			is_store_call: true,
		},
		Case {
			deposit_limit: deposit_limit::<Test>(),
			gas_divisor: 2,
			callee_input: Callee::storeCall { _data: 42 }.abi_encode(),
			result: Ok(true),
			is_store_call: true,
		},
	];

	for ((case, config), call_type) in
		test_cases.iter().cartesian_product(&configs).cartesian_product(call_types)
	{
		// the storage stuff won't work on static or delegate call
		if case.is_store_call && !matches!(call_type, Caller::CallType::Call) {
			continue;
		}

		ExtBuilder::default().build().execute_with(|| {
			let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
			let fees = <Test as Config>::FeeInfo::tx_fee_from_weight(0, &WEIGHT_LIMIT) +
				case.deposit_limit;
			<Test as Config>::FeeInfo::deposit_txfee(<Test as Config>::Currency::issue(fees));

			// Instantiate the callee contract, which can echo a value.
			let Contract { addr: callee_addr, .. } =
				builder::bare_instantiate(Code::Upload(callee_code.clone()))
					.build_and_unwrap_contract();

			// Instantiate the caller contract.
			let Contract { addr: caller_addr, .. } =
				builder::bare_instantiate(Code::Upload(caller_code.clone()))
					.build_and_unwrap_contract();

			let output = builder::bare_call(caller_addr)
				.data(
					Caller::callPartialGasCall {
						_callee: callee_addr.0.into(),
						_data: case.callee_input.clone().into(),
						_gasDivisor: case.gas_divisor,
						_callType: call_type,
					}
					.abi_encode(),
				)
				.exec_config(config.clone())
				.transaction_limits(TransactionLimits::WeightAndDeposit {
					weight_limit: Weight::from_parts(50_000_000_000, 10 * 1024 * 1024),
					deposit_limit: case.deposit_limit,
				})
				.build();

			let result = output.result.map(|result| {
				Caller::callPartialGasCall::abi_decode_returns(&result.data).unwrap()
			});
			assert_eq!(case.result, result);
		});
	}
}

#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
fn delegatecall_with_large_deposit_limit_succeeds(
	caller_type: FixtureType,
	callee_type: FixtureType,
) {
	let (caller_code, _) = compile_module_with_type("Caller", caller_type).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", callee_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_code)).build_and_unwrap_contract();

		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		// Use a very large deposit limit to trigger the bug scenario
		let large_deposit_limit: u128 = u64::MAX as _;

		let result = builder::bare_call(caller_addr)
			.data(
				Caller::delegateCall {
					_callee: callee_addr.0.into(),
					_data: Callee::echoCall { _data: 42 }.abi_encode().into(),
					_gas: u64::MAX,
				}
				.abi_encode(),
			)
			.transaction_limits(TransactionLimits::WeightAndDeposit {
				weight_limit: WEIGHT_LIMIT,
				deposit_limit: large_deposit_limit,
			})
			.build();

		// The call must succeed - before the fix, this would fail with OutOfGas
		let exec_result = result.result.expect("call must not fail");
		let decoded = Caller::delegateCall::abi_decode_returns(&exec_result.data).unwrap();
		assert!(decoded.success, "delegatecall must succeed");

		let echo_result = Callee::echoCall::abi_decode_returns(&decoded.output).unwrap();
		assert_eq!(echo_result, 42, "echo must return the magic number");
	});
}
