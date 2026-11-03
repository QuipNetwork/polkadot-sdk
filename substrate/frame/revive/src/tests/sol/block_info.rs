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
	Code, Config, ExecConfig, Pallet,
	test_utils::{ALICE, builder::Contract},
	tests::{Contracts, ExtBuilder, System, Test, Timestamp, builder},
	vm::evm::DIFFICULTY,
};

use alloy_core::sol_types::{SolCall, SolInterface};
use frame_support::traits::fungible::Mutate;
use pallet_revive_fixtures::{BlockInfo, FixtureType, compile_module_with_type};
use pretty_assertions::assert_eq;
use sp_core::H160;
use test_case::test_case;

/// Tests that the blocknumber opcode works as expected.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn block_number_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("BlockInfo", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		System::set_block_number(42);

		let result = builder::bare_call(addr)
			.data(
				BlockInfo::BlockInfoCalls::blockNumber(BlockInfo::blockNumberCall {}).abi_encode(),
			)
			.build_and_unwrap_result();
		let decoded = BlockInfo::blockNumberCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(42u64, decoded);
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn block_number_dry_run_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("BlockInfo", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		System::set_block_number(42);
		let timestamp_override = Some(Timestamp::get() + 10_000);

		let result = builder::bare_call(addr)
			.data(
				BlockInfo::BlockInfoCalls::blockNumber(BlockInfo::blockNumberCall {}).abi_encode(),
			)
			.exec_config(ExecConfig::new_substrate_tx().with_dry_run(timestamp_override))
			.build_and_unwrap_result();
		let decoded = BlockInfo::blockNumberCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(43u64, decoded);
	});
}

/// Tests that the blockauthor opcode works as expected.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn block_author_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("BlockInfo", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		let result = builder::bare_call(addr)
			.data(BlockInfo::BlockInfoCalls::coinbase(BlockInfo::coinbaseCall {}).abi_encode())
			.build_and_unwrap_result();
		let decoded = BlockInfo::coinbaseCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(Contracts::block_author(), H160::from_slice(decoded.as_slice()));
	});
}

/// Tests that the chainid opcode works as expected.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn chainid_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("BlockInfo", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		let result = builder::bare_call(addr)
			.data(BlockInfo::BlockInfoCalls::chainid(BlockInfo::chainidCall {}).abi_encode())
			.build_and_unwrap_result();
		let decoded = BlockInfo::chainidCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(<Test as Config>::ChainId::get() as u64, decoded);
	});
}

/// Tests that the timestamp opcode works as expected.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn timestamp_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("BlockInfo", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		Timestamp::set_timestamp(2000);
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		let result = builder::bare_call(addr)
			.data(BlockInfo::BlockInfoCalls::timestamp(BlockInfo::timestampCall {}).abi_encode())
			.build_and_unwrap_result();
		let decoded = BlockInfo::timestampCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(
			// Solidity expects timestamps in seconds, whereas pallet_timestamp uses
			// milliseconds.
			(Timestamp::get() / 1000) as u64,
			decoded
		);
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn timestamp_dry_run_override_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("BlockInfo", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		Timestamp::set_timestamp(2000);
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();
		let timestamp_override = Timestamp::get() + 10_000;
		let result: crate::ExecReturnValue = builder::bare_call(addr)
			.data(BlockInfo::BlockInfoCalls::timestamp(BlockInfo::timestampCall {}).abi_encode())
			.exec_config(ExecConfig::new_substrate_tx().with_dry_run(timestamp_override))
			.build_and_unwrap_result();
		let decoded = BlockInfo::timestampCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(
			// Solidity expects timestamps in seconds, whereas pallet_timestamp uses
			// milliseconds.
			(timestamp_override / 1000) as u64,
			decoded
		);
	});
}

/// Tests that the gaslimit opcode works as expected.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn gaslimit_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("BlockInfo", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		let result = builder::bare_call(addr)
			.data(BlockInfo::BlockInfoCalls::gaslimit(BlockInfo::gaslimitCall {}).abi_encode())
			.build_and_unwrap_result();
		let decoded = BlockInfo::gaslimitCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(<Pallet<Test>>::evm_block_gas_limit(), decoded.into());
	});
}

/// Tests that the basefee opcode works as expected.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn base_fee_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("BlockInfo", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		let result = builder::bare_call(addr)
			.data(BlockInfo::BlockInfoCalls::basefee(BlockInfo::basefeeCall {}).abi_encode())
			.build_and_unwrap_result();
		let decoded = BlockInfo::basefeeCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(<crate::Pallet<Test>>::evm_base_fee().as_u64(), decoded);
	});
}

/// Tests that the difficulty opcode works as expected.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn difficulty_works(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("BlockInfo", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		let result = builder::bare_call(addr)
			.data(BlockInfo::BlockInfoCalls::difficulty(BlockInfo::difficultyCall {}).abi_encode())
			.build_and_unwrap_result();
		let decoded = BlockInfo::difficultyCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(
			// Aligned with the value set for PVM (truncated to u64)
			DIFFICULTY as u64,
			decoded
		);
	});
}
