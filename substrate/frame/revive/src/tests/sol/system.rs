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

use crate::{
	Code, Config, ExecConfig, TransactionLimits, TransactionMeter, U256,
	evm::fees::InfoT,
	test_utils::{ALICE, ALICE_ADDR, WEIGHT_LIMIT, builder::Contract, deposit_limit},
	tests::{Contracts, ExtBuilder, RuntimeOrigin, Test, builder},
};

use alloy_core::sol_types::{Revert, SolCall, SolConstructor, SolError};
use frame_support::traits::fungible::{Balanced, Mutate};
use pallet_revive_fixtures::{
	Callee, FixtureType, OriginIsRoot as OriginIsRootFixture, System as SystemFixture,
	compile_module_with_type,
};
use pretty_assertions::assert_eq;
use revm::primitives::Bytes;
use sp_core::H160;
use sp_io::hashing::keccak_256;
use test_case::test_case;

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn keccak_256_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("System", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(SystemFixture::constructorCall { panic: false }.abi_encode())
			.build_and_unwrap_contract();

		let pre = b"revive";
		let expected = keccak_256(pre);

		let result = builder::bare_call(addr)
			.data(SystemFixture::keccak256FuncCall { data: Bytes::from(pre) }.abi_encode())
			.build_and_unwrap_result();

		assert_eq!(&expected, result.data.as_slice());
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn address_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("System", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(SystemFixture::constructorCall { panic: false }.abi_encode())
			.build_and_unwrap_contract();

		let result = builder::bare_call(addr)
			.data(SystemFixture::addressFuncCall {}.abi_encode())
			.build_and_unwrap_result();

		let decoded = SystemFixture::addressFuncCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(addr, H160::from_slice(decoded.as_slice()));
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn caller_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("System", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(SystemFixture::constructorCall { panic: false }.abi_encode())
			.build_and_unwrap_contract();

		let result = builder::bare_call(addr)
			.data(SystemFixture::callerCall {}.abi_encode())
			.build_and_unwrap_result();

		let decoded = SystemFixture::callerCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(ALICE_ADDR, H160::from_slice(decoded.as_slice()));
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn callvalue_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("System", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(SystemFixture::constructorCall { panic: false }.abi_encode())
			.build_and_unwrap_contract();

		let value = 1337u64;

		let result = builder::bare_call(addr)
			.evm_value(value.into())
			.data(SystemFixture::callvalueCall {}.abi_encode())
			.build_and_unwrap_result();

		let decoded = SystemFixture::callvalueCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(value, decoded);
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn calldataload_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("System", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(SystemFixture::constructorCall { panic: false }.abi_encode())
			.build_and_unwrap_contract();

		let result = builder::bare_call(addr)
			.data(SystemFixture::calldataloadCall { offset: 4u64 }.abi_encode())
			.build_and_unwrap_result();

		// Call calldataload(offset=4) → returns the argument "4"
		let decoded = SystemFixture::calldataloadCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(U256::from(4u32), U256::from_big_endian(decoded.as_slice()));
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn calldatasize_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("System", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(SystemFixture::constructorCall { panic: false }.abi_encode())
			.build_and_unwrap_contract();

		// calldata = selector + encoded argument
		let result = builder::bare_call(addr)
			.data(SystemFixture::calldatasizeCall {}.abi_encode())
			.build_and_unwrap_result();

		// ABI encodes: 4 (selector) + 0 (no args) = 4
		let decoded = SystemFixture::calldatasizeCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(4u64, decoded);
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn calldatacopy_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("System", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(SystemFixture::constructorCall { panic: false }.abi_encode())
			.build_and_unwrap_contract();

		let call_data = SystemFixture::calldatacopyCall {
			destOffset: 0u64, // unused
			offset: 4u64,     // skip selector
			size: 64u64,      // copy destOffset + offset
		}
		.abi_encode();

		let result = builder::bare_call(addr).data(call_data.clone()).build_and_unwrap_result();

		let returned_data =
			SystemFixture::calldatacopyCall::abi_decode_returns(&result.data).unwrap();

		let returned_data = returned_data.as_ref();
		assert_eq!(returned_data.len(), 64);

		// The expected data is the slice of the original calldata that was copied.
		let expected_data = &call_data.as_slice()[4..(4 + 64) as usize];
		assert_eq!(expected_data, returned_data);
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn codesize_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("System", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.constructor_data(SystemFixture::constructorCall { panic: false }.abi_encode())
			.build_and_unwrap_contract();

		let result = builder::bare_call(addr)
			.data(SystemFixture::codesizeCall {}.abi_encode())
			.build_and_unwrap_result();

		// Now fetch the actual *runtime* code size from storage
		let code = Contracts::code(&addr);

		let decoded = SystemFixture::codesizeCall::abi_decode_returns(&result.data).unwrap();
		let expected_size = code.len() as u64;

		assert_eq!(expected_size, decoded);
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn gas_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("System", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.constructor_data(SystemFixture::constructorCall { panic: false }.abi_encode())
			.build_and_unwrap_contract();

		// enable txhold collection which we expect to be on when using the evm backend
		let limits = TransactionLimits::WeightAndDeposit {
			weight_limit: WEIGHT_LIMIT,
			deposit_limit: deposit_limit::<Test>(),
		};
		let hold_initial =
			TransactionMeter::<Test>::new(limits.clone()).unwrap().eth_gas_left().unwrap();

		<Test as Config>::FeeInfo::deposit_txfee(<Test as Config>::Currency::issue(hold_initial));
		let mut exec_config = ExecConfig::new_substrate_tx();
		exec_config.collect_deposit_from_hold = Some((0u32.into(), Default::default()));

		let result = builder::bare_call(addr)
			.data(SystemFixture::gasCall {}.abi_encode())
			.exec_config(exec_config)
			.transaction_limits(limits)
			.build_and_unwrap_result();

		let gas_left: u64 = SystemFixture::gasCall::abi_decode_returns(&result.data)
			.unwrap()
			.try_into()
			.unwrap();

		assert!(gas_left > 0);
		assert!((gas_left as u128) < hold_initial);
	});
}

#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
#[test_case(FixtureType::Resolc, FixtureType::Solc;   "resolc->solc")]
#[test_case(FixtureType::Resolc, FixtureType::Resolc; "resolc->resolc")]
fn returndatasize_works(caller_type: FixtureType, callee_type: FixtureType) {
	let (code, _) = compile_module_with_type("System", caller_type).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", callee_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		// Instantiate the callee contract, which can echo a value.
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_code)).build_and_unwrap_contract();

		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(SystemFixture::constructorCall { panic: false }.abi_encode())
			.build_and_unwrap_contract();

		let magic_number = 42u64;
		let result = builder::bare_call(addr)
			.data(
				SystemFixture::returndatasizeCall {
					_callee: callee_addr.0.into(),
					_data: Callee::echoCall { _data: magic_number }.abi_encode().into(),
					_gas: u64::MAX,
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		let decoded = SystemFixture::returndatasizeCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(decoded, 32);
	});
}

#[test_case(FixtureType::Solc,   FixtureType::Solc;   "solc->solc")]
#[test_case(FixtureType::Solc,   FixtureType::Resolc; "solc->resolc")]
#[test_case(FixtureType::Resolc, FixtureType::Solc;   "resolc->solc")]
#[test_case(FixtureType::Resolc, FixtureType::Resolc; "resolc->resolc")]
fn returndatacopy_works(caller_type: FixtureType, callee_type: FixtureType) {
	let (code, _) = compile_module_with_type("System", caller_type).unwrap();
	let (callee_code, _) = compile_module_with_type("Callee", callee_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		// Instantiate the callee contract, which can echo a value.
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_code)).build_and_unwrap_contract();

		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(SystemFixture::constructorCall { panic: false }.abi_encode())
			.build_and_unwrap_contract();

		let magic_number = U256::from(42);
		let result = builder::bare_call(addr)
			.data(
				SystemFixture::returndatacopyCall {
					_callee: callee_addr.0.into(),
					_data: Callee::echoCall { _data: 42u64 }.abi_encode().into(),
					_gas: u64::MAX,
					destOffset: 0u64,
					offset: 0u64,
					size: 32u64,
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();

		let decoded = SystemFixture::returndatacopyCall::abi_decode_returns(&result.data).unwrap();
		let decoded_value = U256::from_big_endian(decoded.as_ref());
		assert_eq!(magic_number, decoded_value)
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn constructor_with_argument_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("System", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let result = builder::bare_instantiate(Code::Upload(code))
			.constructor_data(SystemFixture::constructorCall { panic: true }.abi_encode())
			.build()
			.result
			.unwrap()
			.result;
		assert!(result.did_revert());

		let expected_message = "Reverted because revert=true was set as constructor argument";
		assert_eq!(result.data, Revert::from(expected_message).abi_encode());
	});
}

/// Direct: a root-origin call into a contract that invokes `originIsRoot` returns `true`,
/// and a signed-origin call returns `false`.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn origin_is_root_direct(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("OriginIsRoot", fixture_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		let call_data = OriginIsRootFixture::originIsRootCall {}.abi_encode();

		let root_result = builder::bare_call(addr)
			.origin(RuntimeOrigin::root())
			.data(call_data.clone())
			.build_and_unwrap_result();
		assert!(
			OriginIsRootFixture::originIsRootCall::abi_decode_returns(&root_result.data).unwrap(),
		);

		let signed_result = builder::bare_call(addr).data(call_data).build_and_unwrap_result();
		assert!(
			!OriginIsRootFixture::originIsRootCall::abi_decode_returns(&signed_result.data)
				.unwrap(),
		);
	});
}

/// Through a regular contract call: `originIsRoot` reports the origin of the whole call stack,
/// so a root dispatch into a contract that then calls another contract still observes `true`.
/// This is the key behavioural difference from `callerIsRoot`.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn origin_is_root_through_regular_call(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("OriginIsRoot", fixture_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr: caller, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.salt(Some([1u8; 32]))
			.build_and_unwrap_contract();
		let Contract { addr: callee, .. } = builder::bare_instantiate(Code::Upload(code))
			.salt(Some([2u8; 32]))
			.build_and_unwrap_contract();

		let call_data =
			OriginIsRootFixture::callOriginIsRootCall { target: callee.0.into() }.abi_encode();

		let root_result = builder::bare_call(caller)
			.origin(RuntimeOrigin::root())
			.data(call_data.clone())
			.build_and_unwrap_result();
		assert!(
			OriginIsRootFixture::callOriginIsRootCall::abi_decode_returns(&root_result.data)
				.unwrap(),
		);

		let signed_result = builder::bare_call(caller).data(call_data).build_and_unwrap_result();
		assert!(
			!OriginIsRootFixture::callOriginIsRootCall::abi_decode_returns(&signed_result.data)
				.unwrap(),
		);
	});
}

/// Through a delegate call: mirrors the upgradeable-proxy pattern (root → proxy →
/// delegatecall → implementation). `originIsRoot` must propagate root authority across the
/// delegate frame.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn origin_is_root_through_delegate_call(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("OriginIsRoot", fixture_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr: proxy, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.salt(Some([1u8; 32]))
			.build_and_unwrap_contract();
		let Contract { addr: implementation, .. } = builder::bare_instantiate(Code::Upload(code))
			.salt(Some([2u8; 32]))
			.build_and_unwrap_contract();

		let call_data =
			OriginIsRootFixture::delegateOriginIsRootCall { _impl: implementation.0.into() }
				.abi_encode();

		let root_result = builder::bare_call(proxy)
			.origin(RuntimeOrigin::root())
			.data(call_data.clone())
			.build_and_unwrap_result();
		assert!(
			OriginIsRootFixture::delegateOriginIsRootCall::abi_decode_returns(&root_result.data)
				.unwrap(),
		);

		let signed_result = builder::bare_call(proxy).data(call_data).build_and_unwrap_result();
		assert!(
			!OriginIsRootFixture::delegateOriginIsRootCall::abi_decode_returns(&signed_result.data)
				.unwrap(),
		);
	});
}

/// Regression coverage: `callerIsRoot` keeps its strict semantics. The direct caller of the
/// contract calling the precompile must be the root origin; an intermediate contract call
/// from a root-originated dispatch is not enough.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn caller_is_root_does_not_cross_regular_call(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("OriginIsRoot", fixture_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr: caller, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.salt(Some([1u8; 32]))
			.build_and_unwrap_contract();
		let Contract { addr: callee, .. } = builder::bare_instantiate(Code::Upload(code))
			.salt(Some([2u8; 32]))
			.build_and_unwrap_contract();

		// Direct root call -> the precompile sees Root as its immediate caller.
		let direct = builder::bare_call(callee)
			.origin(RuntimeOrigin::root())
			.data(OriginIsRootFixture::callerIsRootCall {}.abi_encode())
			.build_and_unwrap_result();
		assert!(OriginIsRootFixture::callerIsRootCall::abi_decode_returns(&direct.data).unwrap());

		// Root dispatch through one regular contract call -> caller is the intermediate
		// contract, not Root.
		let indirect = builder::bare_call(caller)
			.origin(RuntimeOrigin::root())
			.data(
				OriginIsRootFixture::callCallerIsRootCall { target: callee.0.into() }.abi_encode(),
			)
			.build_and_unwrap_result();
		assert!(
			!OriginIsRootFixture::callCallerIsRootCall::abi_decode_returns(&indirect.data).unwrap(),
		);
	});
}

/// Through a delegate call: root -> proxy -> delegatecall -> implementation. The caller of the
/// delegated code is the caller of the proxy, which is Root, so `callerIsRoot` holds.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn caller_is_root_through_delegate_call(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("OriginIsRoot", fixture_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr: proxy, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.salt(Some([1u8; 32]))
			.build_and_unwrap_contract();
		let Contract { addr: implementation, .. } = builder::bare_instantiate(Code::Upload(code))
			.salt(Some([2u8; 32]))
			.build_and_unwrap_contract();

		let call_data =
			OriginIsRootFixture::delegateCallerIsRootCall { _impl: implementation.0.into() }
				.abi_encode();

		let root_result = builder::bare_call(proxy)
			.origin(RuntimeOrigin::root())
			.data(call_data.clone())
			.build_and_unwrap_result();
		assert!(
			OriginIsRootFixture::delegateCallerIsRootCall::abi_decode_returns(&root_result.data)
				.unwrap(),
		);

		let signed_result = builder::bare_call(proxy).data(call_data).build_and_unwrap_result();
		assert!(
			!OriginIsRootFixture::delegateCallerIsRootCall::abi_decode_returns(&signed_result.data)
				.unwrap(),
		);
	});
}

/// Through a delegate call followed by a regular call: root -> contract -> delegatecall ->
/// regular call -> target. The caller of `target` is the contract, so `callerIsRoot` is false,
/// even though root called the contract.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn caller_is_root_does_not_cross_regular_call_after_delegate_call(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("OriginIsRoot", fixture_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr: caller, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.salt(Some([1u8; 32]))
			.build_and_unwrap_contract();
		let Contract { addr: lib, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.salt(Some([2u8; 32]))
			.build_and_unwrap_contract();
		let Contract { addr: target, .. } = builder::bare_instantiate(Code::Upload(code))
			.salt(Some([3u8; 32]))
			.build_and_unwrap_contract();

		let result = builder::bare_call(caller)
			.origin(RuntimeOrigin::root())
			.data(
				OriginIsRootFixture::forwardDelegateCall {
					_impl: lib.0.into(),
					data: OriginIsRootFixture::callCallerIsRootCall { target: target.0.into() }
						.abi_encode()
						.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();
		assert!(
			!OriginIsRootFixture::forwardDelegateCall::abi_decode_returns(&result.data).unwrap()
		);
	});
}

/// Through a regular call followed by a delegate call: root -> contract -> regular call -> proxy
/// -> delegatecall -> implementation. The caller of the delegated code is the caller of the proxy,
/// which is the contract, so `callerIsRoot` is false, even though root called the contract.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn caller_is_root_does_not_cross_regular_call_before_delegate_call(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("OriginIsRoot", fixture_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr: caller, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.salt(Some([1u8; 32]))
			.build_and_unwrap_contract();
		let Contract { addr: proxy, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.salt(Some([2u8; 32]))
			.build_and_unwrap_contract();
		let Contract { addr: implementation, .. } = builder::bare_instantiate(Code::Upload(code))
			.salt(Some([3u8; 32]))
			.build_and_unwrap_contract();

		let result = builder::bare_call(caller)
			.origin(RuntimeOrigin::root())
			.data(
				OriginIsRootFixture::forwardCall {
					target: proxy.0.into(),
					data: OriginIsRootFixture::delegateCallerIsRootCall {
						_impl: implementation.0.into(),
					}
					.abi_encode()
					.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();
		assert!(!OriginIsRootFixture::forwardCall::abi_decode_returns(&result.data).unwrap());
	});
}

/// Through two chained delegate calls: root -> proxy -> delegatecall -> implementation ->
/// delegatecall -> library. Each delegate frame keeps the caller of the contract it runs as, so
/// `callerIsRoot` holds in the library. A contract the library calls with a regular call sees
/// the proxy as its caller, so `callerIsRoot` is false there.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn caller_is_root_through_chained_delegate_calls(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("OriginIsRoot", fixture_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr: proxy, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.salt(Some([1u8; 32]))
			.build_and_unwrap_contract();
		let Contract { addr: implementation, .. } =
			builder::bare_instantiate(Code::Upload(code.clone()))
				.salt(Some([2u8; 32]))
				.build_and_unwrap_contract();
		let Contract { addr: library, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.salt(Some([3u8; 32]))
			.build_and_unwrap_contract();
		let Contract { addr: target, .. } = builder::bare_instantiate(Code::Upload(code))
			.salt(Some([4u8; 32]))
			.build_and_unwrap_contract();

		// proxy -> delegatecall -> implementation -> delegatecall -> library.callerIsRoot()
		let chained = OriginIsRootFixture::forwardDelegateCall {
			_impl: implementation.0.into(),
			data: OriginIsRootFixture::delegateCallerIsRootCall { _impl: library.0.into() }
				.abi_encode()
				.into(),
		}
		.abi_encode();

		let root_result = builder::bare_call(proxy)
			.origin(RuntimeOrigin::root())
			.data(chained.clone())
			.build_and_unwrap_result();
		assert!(
			OriginIsRootFixture::forwardDelegateCall::abi_decode_returns(&root_result.data)
				.unwrap(),
		);

		let signed_result = builder::bare_call(proxy).data(chained).build_and_unwrap_result();
		assert!(
			!OriginIsRootFixture::forwardDelegateCall::abi_decode_returns(&signed_result.data)
				.unwrap(),
		);

		// proxy -> delegatecall -> implementation -> delegatecall -> library -> regular call ->
		// target.callerIsRoot()
		let chained_then_call = OriginIsRootFixture::forwardDelegateCall {
			_impl: implementation.0.into(),
			data: OriginIsRootFixture::forwardDelegateCall {
				_impl: library.0.into(),
				data: OriginIsRootFixture::callCallerIsRootCall { target: target.0.into() }
					.abi_encode()
					.into(),
			}
			.abi_encode()
			.into(),
		}
		.abi_encode();

		let result = builder::bare_call(proxy)
			.origin(RuntimeOrigin::root())
			.data(chained_then_call)
			.build_and_unwrap_result();
		assert!(
			!OriginIsRootFixture::forwardDelegateCall::abi_decode_returns(&result.data).unwrap()
		);
	});
}

/// Through a delegate call: origin -> proxy -> delegatecall -> implementation. The caller of the
/// delegated code is the caller of the proxy, which is the origin, so `callerIsOrigin` holds.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn caller_is_origin_through_delegate_call(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("OriginIsRoot", fixture_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr: proxy, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.salt(Some([1u8; 32]))
			.build_and_unwrap_contract();
		let Contract { addr: implementation, .. } = builder::bare_instantiate(Code::Upload(code))
			.salt(Some([2u8; 32]))
			.build_and_unwrap_contract();

		let result = builder::bare_call(proxy)
			.data(
				OriginIsRootFixture::delegateCallerIsOriginCall { _impl: implementation.0.into() }
					.abi_encode(),
			)
			.build_and_unwrap_result();
		assert!(
			OriginIsRootFixture::delegateCallerIsOriginCall::abi_decode_returns(&result.data)
				.unwrap(),
		);
	});
}

/// Through a delegate call followed by a regular call: origin -> contract -> delegatecall ->
/// regular call -> target. The caller of `target` is the contract, so `callerIsOrigin` is false,
/// even though the origin called the contract.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn caller_is_origin_does_not_cross_regular_call_after_delegate_call(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("OriginIsRoot", fixture_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr: caller, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.salt(Some([1u8; 32]))
			.build_and_unwrap_contract();
		let Contract { addr: lib, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.salt(Some([2u8; 32]))
			.build_and_unwrap_contract();
		let Contract { addr: target, .. } = builder::bare_instantiate(Code::Upload(code))
			.salt(Some([3u8; 32]))
			.build_and_unwrap_contract();

		let result = builder::bare_call(caller)
			.data(
				OriginIsRootFixture::forwardDelegateCall {
					_impl: lib.0.into(),
					data: OriginIsRootFixture::callCallerIsOriginCall { target: target.0.into() }
						.abi_encode()
						.into(),
				}
				.abi_encode(),
			)
			.build_and_unwrap_result();
		assert!(
			!OriginIsRootFixture::forwardDelegateCall::abi_decode_returns(&result.data).unwrap()
		);
	});
}
