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
	BalanceOf, Code, Config, Error, EthBlockBuilderFirstValues, GenesisConfig, Origin, Pallet,
	PristineCode, assert_refcount,
	call_builder::VmBinaryModule,
	debug::DebugSettings,
	evm::{PrestateTracer, PrestateTracerConfig},
	test_utils::{ALICE, ALICE_ADDR, BOB, builder::Contract},
	tests::{
		AllowEvmBytecode, DebugFlag, ExtBuilder, RuntimeOrigin, Test, builder,
		test_utils::{contract_base_deposit, ensure_stored, get_contract},
	},
	tracing::trace,
	weightinfo_extension::OnFinalizeBlockParts,
};
use alloy_core::sol_types::{SolCall, SolInterface};
use frame_support::{
	assert_err, assert_noop, assert_ok, dispatch::GetDispatchInfo, traits::fungible::Mutate,
};
use pallet_revive_fixtures::{Fibonacci, FixtureType, NestedCounter, compile_module_with_type};
use pallet_revive_types::runtime_api::*;
use pretty_assertions::assert_eq;
use sp_runtime::Weight;
use test_case::test_case;

use revm::bytecode::opcode::*;

mod arithmetic;
mod bitwise;
mod block_info;
mod contract;
mod control;
mod host;
mod memory;
mod stack;
mod system;
mod terminate;
mod terminate_storage;
mod tx_info;

fn make_initcode_from_runtime_code(runtime_code: &Vec<u8>) -> Vec<u8> {
	let runtime_code_len = runtime_code.len();
	assert!(runtime_code_len < 256, "runtime code length must be less than 256 bytes");
	let mut init_code: Vec<u8> = vec![
		vec![PUSH1, 0x80_u8],
		vec![PUSH1, 0x40_u8],
		vec![MSTORE],
		vec![PUSH1, 0x40_u8],
		vec![MLOAD],
		vec![PUSH1, runtime_code_len as u8],
		vec![PUSH1, 0x13_u8],
		vec![DUP3],
		vec![CODECOPY],
		vec![PUSH1, runtime_code_len as u8],
		vec![SWAP1],
		vec![RETURN],
		vec![INVALID],
	]
	.into_iter()
	.flatten()
	.collect();
	init_code.extend(runtime_code);
	init_code
}

/// Init code for a contract that `EXTCODECOPY`s `copy_len` bytes from the address in calldata.
fn make_extcodecopy_reader(copy_len: u32) -> Vec<u8> {
	let [b0, b1, b2, b3] = copy_len.to_be_bytes();
	make_initcode_from_runtime_code(&vec![
		PUSH4,
		b0,
		b1,
		b2,
		b3,           // size: bytes to copy
		PUSH0,        // code offset
		PUSH0,        // destination memory offset
		PUSH0,        // calldata offset of the target address
		CALLDATALOAD, // load the target address
		EXTCODECOPY,
		STOP,
	])
}

#[test]
fn basic_evm_flow_works() {
	let (code, init_hash) = compile_module_with_type("Fibonacci", FixtureType::Solc).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		for i in 1u8..=2 {
			let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
			let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
				.salt(Some([i; 32]))
				.build_and_unwrap_contract();

			// check the code exists
			let contract = get_contract(&addr);
			ensure_stored(contract.code_hash);
			let deposit = contract_base_deposit(&addr);
			assert_eq!(contract.total_deposit(), deposit);
			assert_refcount!(contract.code_hash, i as u64);

			let result = builder::bare_call(addr)
				.data(Fibonacci::FibonacciCalls::fib(Fibonacci::fibCall { n: 10u64 }).abi_encode())
				.build_and_unwrap_result();
			let decoded = Fibonacci::fibCall::abi_decode_returns(&result.data).unwrap();
			assert_eq!(55u64, decoded);
		}

		// init code is not stored
		assert!(!PristineCode::<Test>::contains_key(init_hash));
	});
}

#[test]
fn basic_evm_flow_tracing_works() {
	use crate::{
		evm::{CallTrace, CallTracer, CallType},
		tracing::trace,
	};
	let (code, _) = compile_module_with_type("Fibonacci", FixtureType::Solc).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let mut tracer = CallTracer::new(Default::default());
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		let Contract { addr, .. } = trace(&mut tracer, || {
			builder::bare_instantiate(Code::Upload(code.clone()))
				.salt(None)
				.build_and_unwrap_contract()
		});

		let contract = get_contract(&addr);
		let runtime_code = PristineCode::<Test>::get(contract.code_hash).unwrap();

		let call_trace = tracer.collect_trace().unwrap();
		assert_eq!(
			call_trace,
			CallTrace {
				from: ALICE_ADDR,
				call_type: CallType::Create,
				to: addr,
				input: code.into(),
				output: runtime_code.into(),
				value: Some(crate::U256::zero()),
				gas: call_trace.gas,
				gas_used: call_trace.gas_used,
				..Default::default()
			}
		);

		let mut call_tracer = CallTracer::new(Default::default());
		let result = trace(&mut call_tracer, || {
			builder::bare_call(addr)
				.data(Fibonacci::FibonacciCalls::fib(Fibonacci::fibCall { n: 10u64 }).abi_encode())
				.build_and_unwrap_result()
		});

		let decoded = Fibonacci::fibCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(55u64, decoded);

		let call_trace = call_tracer.collect_trace().unwrap();
		assert_eq!(
			call_trace,
			CallTrace {
				call_type: CallType::Call,
				from: ALICE_ADDR,
				to: addr,
				input: Fibonacci::FibonacciCalls::fib(Fibonacci::fibCall { n: 10u64 })
					.abi_encode()
					.into(),
				output: result.data.into(),
				value: Some(crate::U256::zero()),
				gas: call_trace.gas,
				gas_used: call_trace.gas_used,
				..Default::default()
			},
		);
	});
}

/// EVM `sload` must charge proportionally to the actual byte size of the storage
/// value, not just the EVM word size of 32. The trie's storage values can exceed
/// 32 bytes when a PVM contract sharing the same namespace (via delegatecall) wrote
/// them — a fixed 32-byte charge would undercharge the proof space consumed by the
/// read. The fix charges `STORAGE_BYTES` upfront and refunds the unused portion
/// based on the actual length read (mirroring `get_storage` in PVM).
///
/// Setup: deploy Solc-compiled `Counter`, then directly inject values of different
/// sizes into its storage at slot 0 (simulating a PVM contract writing there via
/// shared namespace). Calling `number()` compiles down to `SLOAD(0)`; the 256-byte
/// read traps (length mismatch) but the gas consumed up to that point reflects the
/// actual read size.
///
/// Without the fix, both 32-byte and 256-byte cases would consume the same gas.
/// With the fix, the 256-byte case consumes strictly more.
#[test]
fn sload_charges_for_actual_storage_value_size() {
	use crate::exec::Key;
	use pallet_revive_fixtures::Counter;

	let (counter_code, _) = compile_module_with_type("Counter", FixtureType::Solc).unwrap();

	let measure_with_value_len = |len: usize| -> u128 {
		ExtBuilder::default().build().execute_with(|| {
			let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

			let Contract { addr, .. } =
				builder::bare_instantiate(Code::Upload(counter_code.clone()))
					.build_and_unwrap_contract();

			// Inject a value of the requested length directly into the contract's
			// storage trie at slot 0 — simulates a PVM contract writing there via
			// shared namespace (delegatecall).
			let info = get_contract(&addr);
			info.write(&Key::Fix([0u8; 32]), Some(vec![0xAAu8; len]), None, false).unwrap();

			// Counter::number() compiles to SLOAD(0). For len != 32 it traps with
			// ContractTrapped after the read; we still observe the gas consumed.
			let result = builder::bare_call(addr).data(Counter::numberCall {}.abi_encode()).build();
			result.gas_consumed
		})
	};

	let gas_32: u128 = measure_with_value_len(32);
	let gas_256: u128 = measure_with_value_len(256);

	// With the fix, the 256-byte read costs strictly more than the 32-byte read.
	// Without the fix (legacy `GetStorage(32)`), the two would be equal.
	assert!(
		gas_256 > gas_32,
		"sload must charge more for larger storage values: gas_256={gas_256}, gas_32={gas_32}",
	);
}

/// Regression: EVM TLOAD must charge gas proportional to the actual length of the transient
/// value it reads. We inject a value at the EVM-visible `Key::Fix([0; 32])` slot via
/// `ExecConfig::test_env_transient_storage` — simulating a PVM contract writing a non-32-byte
/// value there through the shared namespace (delegatecall) — then call a contract whose runtime
/// is `TLOAD(0)`.
///
/// Comparing a `None` read (slot empty → zero) against a 32-byte read keeps both runs on the
/// same, non-trapping control-flow path, so the only difference in gas is TLOAD's `adjust_weight`
/// to the actual length read: ~974 gas with the fix, ~1 without. Mirrors
/// `sload_charges_for_actual_storage_value_size`.
#[test]
fn tload_charges_for_actual_transient_value_size() {
	use crate::{ExecConfig, exec::Key, limits, transient_storage::TransientStorage};
	use core::cell::RefCell;

	// EVM runtime that reads transient slot 0 and returns it as a 32-byte response:
	//   PUSH0 TLOAD PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN
	let tload_runtime: Vec<u8> = vec![PUSH0, TLOAD, PUSH0, MSTORE, PUSH1, 0x20, PUSH0, RETURN];
	let tload_code = make_initcode_from_runtime_code(&tload_runtime);

	let measure = |inject: Option<usize>| -> u128 {
		ExtBuilder::default().build().execute_with(|| {
			let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

			let Contract { addr, account_id } =
				builder::bare_instantiate(Code::Upload(tload_code.clone()))
					.build_and_unwrap_contract();

			// Pre-populate the EVM-visible transient slot under the callee's namespace. The write
			// updates the backing store immediately, and the frame's `start_transaction`
			// checkpoints the journal *after* this entry, so it survives any in-call rollback.
			let mut transient = TransientStorage::<Test>::new(limits::TRANSIENT_STORAGE_BYTES);
			if let Some(len) = inject {
				transient
					.write(&account_id, &Key::Fix([0u8; 32]), Some(vec![0xAAu8; len]), false)
					.unwrap();
			}
			let mut exec_config = ExecConfig::new_substrate_tx();
			exec_config.test_env_transient_storage = Some(RefCell::new(transient));

			builder::bare_call(addr).exec_config(exec_config).build().gas_consumed
		})
	};

	let delta = measure(Some(32)).saturating_sub(measure(None));

	assert!(
		delta >= 100,
		"TLOAD must charge more for a 32-byte read than for a None read: delta={delta} \
		 (expected ~974 with fix, ~1 without)",
	);
}

#[test]
fn extcodecopy_charges_for_actual_code_size() {
	// Copy a single byte, so the charge is driven by the target's code size, not the copy length.
	let reader_code = make_extcodecopy_reader(1);

	let measure = |code_size: u32| -> u128 {
		ExtBuilder::default().build().execute_with(|| {
			let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

			let target_code = VmBinaryModule::evm_init_code_for_runtime_size(code_size).code;
			let Contract { addr: target_addr, .. } =
				builder::bare_instantiate(Code::Upload(target_code)).build_and_unwrap_contract();

			let Contract { addr: reader_addr, .. } =
				builder::bare_instantiate(Code::Upload(reader_code.clone()))
					.build_and_unwrap_contract();

			let mut input = vec![0u8; 12];
			input.extend_from_slice(target_addr.as_bytes());

			let result = builder::bare_call(reader_addr).data(input).build();
			result.result.unwrap();
			result.gas_consumed
		})
	};

	let gas_small = measure(64);
	let gas_large = measure(20_000);

	assert!(
		gas_large > gas_small,
		"extcodecopy must charge more for a larger target contract: \
		 gas_large={gas_large}, gas_small={gas_small}",
	);
}

#[test]
fn extcodecopy_charges_for_copy_length_beyond_code_size() {
	const TARGET_CODE_SIZE: u32 = 64;

	let measure = |copy_len: u32| -> u128 {
		ExtBuilder::default().build().execute_with(|| {
			let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

			let target_code = VmBinaryModule::evm_init_code_for_runtime_size(TARGET_CODE_SIZE).code;
			let Contract { addr: target_addr, .. } =
				builder::bare_instantiate(Code::Upload(target_code)).build_and_unwrap_contract();

			let Contract { addr: reader_addr, .. } =
				builder::bare_instantiate(Code::Upload(make_extcodecopy_reader(copy_len)))
					.build_and_unwrap_contract();

			let mut input = vec![0u8; 12];
			input.extend_from_slice(target_addr.as_bytes());

			let result = builder::bare_call(reader_addr).data(input).build();
			result.result.unwrap();
			result.gas_consumed
		})
	};

	let gas_at_code_size = measure(TARGET_CODE_SIZE);
	let gas_max = measure(crate::limits::EVM_MEMORY_BYTES);

	assert!(
		gas_max > gas_at_code_size,
		"extcodecopy must charge for the copy length when it exceeds the code size: \
		 gas_max={gas_max}, gas_at_code_size={gas_at_code_size}",
	);
}

/// Regression test for paritytech/contract-issues#278 — nested-call variant.
///
/// `Stack::call`'s no-code branch (the path taken when a running contract
/// makes an external call into an account with no code, e.g.
/// `payable(addr).transfer(...)` or `addr.call{value: ...}("")` to an EOA)
/// invokes `exit_child_span` with `Default::default()` for both `gas_used`
/// and `weight_consumed`. The frame meter does charge an existential
/// deposit when the destination is fresh, so the inner `CallTrace` should
/// report non-zero `gas_used`, but today it reports zero. The top-level
/// `Stack::run_call` no-code branch has the same shape and is fixed
/// separately; this test pins down the nested case.
#[test]
fn call_tracing_records_consumption_for_nested_transfer_to_eoa() {
	use crate::evm::{CallTracer, CallType};
	use pallet_revive_fixtures::Caller;
	use sp_core::H160;

	let (caller_code, _) = compile_module_with_type("Caller", FixtureType::Solc).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		let Contract { addr: caller, .. } =
			builder::bare_instantiate(Code::Upload(caller_code)).build_and_unwrap_contract();

		// Pre-fund the caller contract so it has enough balance for the
		// inner value transfer. Pre-funding directly (rather than via
		// `evm_value` on `bare_call`) avoids dust/conversion complications.
		let _ = Pallet::<Test>::set_evm_balance(&caller, 100_000_000_000u128.into());

		// A fresh EOA with no code. The contract's sub-call into this address
		// hits the no-code branch in `Stack::call`, which charges an
		// existential deposit through the frame meter.
		let eoa = H160::from([0xfe; 20]);

		let mut tracer = CallTracer::new(Default::default());
		trace(&mut tracer, || {
			builder::bare_call(caller)
				.data(
					Caller::normalCall {
						_callee: eoa.0.into(),
						_value: 1_000_000,
						_data: Vec::<u8>::new().into(),
						_gas: u64::MAX,
					}
					.abi_encode(),
				)
				.build_and_unwrap_result();
		});

		// Sanity: the value actually arrived.
		assert!(Pallet::<Test>::evm_balance(&eoa) >= 1_000_000.into());

		let trace = tracer.collect_trace().unwrap();
		let inner =
			trace.calls.first().expect("CallTrace must contain the contract → EOA sub-call");
		assert_eq!(inner.to, eoa, "sub-call destination must be the EOA");
		assert_eq!(inner.call_type, CallType::Call, "sub-call must be a regular CALL");
		assert!(
			inner.gas_used > 0,
			"inner call to a fresh EOA must report non-zero gas_used; got {} — see issue #278",
			inner.gas_used,
		);
	});
}

#[test]
fn eth_contract_too_large() {
	// Create EVM init code that is one byte larger than the EIP-3860 limit.
	// We take valid init code and pad it with STOP opcodes after the RETURN instruction
	// (unreachable but makes the init code blob itself exceed MAX_INITCODE_SIZE).
	let mut code = VmBinaryModule::evm_init_code_for_runtime_size(0).code;
	code.resize(revm::primitives::eip3860::MAX_INITCODE_SIZE + 1, revm::bytecode::opcode::STOP);

	for (allow_unlimited_contract_size, debug_flag) in
		[(true, false), (true, true), (false, false), (false, true)]
	{
		// Set the DebugEnabled flag to the desired value for this iteration of the test.
		DebugFlag::set(debug_flag);

		// Initialize genesis config with allow_unlimited_contract_size
		let genesis_config = GenesisConfig::<Test> {
			debug_settings: Some(
				DebugSettings::default()
					.set_allow_unlimited_contract_size(allow_unlimited_contract_size),
			),
			..Default::default()
		};

		ExtBuilder::default()
			.genesis_config(Some(genesis_config))
			.build()
			.execute_with(|| {
				let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

				let result = builder::bare_instantiate(Code::Upload(code.clone())).build();

				if allow_unlimited_contract_size && debug_flag {
					// The contract is too large, but the DebugEnabled flag is set and
					// allow_unlimited_contract_size is true.
					assert_ok!(result.result);
				} else {
					// The contract is too large and either the DebugEnabled flag is not set or
					// allow_unlimited_contract_size is false.
					assert_err!(result.result, <Error<Test>>::BlobTooLarge);
				}
			});
	}
}

#[test]
fn upload_evm_runtime_code_works() {
	use crate::{
		Pallet, TransactionMeter,
		exec::Executable,
		primitives::ExecConfig,
		storage::{AccountInfo, ContractInfo},
	};

	let (runtime_code, _runtime_hash) =
		compile_module_with_type("Fibonacci", FixtureType::SolcRuntime).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let deployer = ALICE;
		let deployer_addr = ALICE_ADDR;
		let _ = Pallet::<Test>::set_evm_balance(&deployer_addr, 1_000_000_000.into());

		let uploaded_blob = Pallet::<Test>::try_upload_code(
			deployer,
			runtime_code.clone(),
			crate::vm::BytecodeType::Evm,
			&mut TransactionMeter::new_from_limits(Weight::MAX, BalanceOf::<Test>::MAX).unwrap(),
			&ExecConfig::new_substrate_tx(),
		)
		.unwrap();

		let contract_address = crate::address::create1(&deployer_addr, 0u32.into());

		let contract_info =
			ContractInfo::<Test>::new(&contract_address, 0u32.into(), *uploaded_blob.code_hash())
				.unwrap();
		AccountInfo::<Test>::insert_contract(&contract_address, contract_info);

		// Call the contract and verify it works
		let result = builder::bare_call(contract_address)
			.data(Fibonacci::FibonacciCalls::fib(Fibonacci::fibCall { n: 10u64 }).abi_encode())
			.build_and_unwrap_result();
		let decoded = Fibonacci::fibCall::abi_decode_returns(&result.data).unwrap();
		assert_eq!(55u64, decoded, "Contract should correctly compute fibonacci(10)");
	});
}

#[test]
fn extcodecopy_out_of_range_offset_zero_fills() {
	// Regression test for #12643: a `code_offset` of 2^64 overflows `usize` and used to halt.
	// Copy 32 bytes of our own code from that offset and return them; expect all zeroes.
	#[rustfmt::skip]
	let runtime_code = vec![
		PUSH1, 0x20,                                          // size = 32
		PUSH9, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // code offset = 2^64
		PUSH1, 0x00,                                          // dest offset = 0
		ADDRESS,                                              // copy from self
		EXTCODECOPY,
		PUSH1, 0x20,                                          // return size = 32
		PUSH1, 0x00,                                          // return offset = 0
		RETURN,
	];

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let init_code = make_initcode_from_runtime_code(&runtime_code);
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(init_code)).build_and_unwrap_contract();

		let result = builder::bare_call(addr).build_and_unwrap_result();
		assert!(!result.did_revert());
		assert_eq!(result.data, vec![0u8; 32]);
	});
}

#[test]
fn upload_and_remove_code_works_for_evm() {
	let (code, code_hash) = compile_module_with_type("Dummy", FixtureType::SolcRuntime).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = Pallet::<Test>::set_evm_balance(&ALICE_ADDR, 5_000_000_000u64.into());

		// Ensure the code is not already stored.
		assert!(!PristineCode::<Test>::contains_key(&code_hash));

		// Upload the code.
		assert_ok!(Pallet::<Test>::upload_code(RuntimeOrigin::signed(ALICE), code, 1000u128));

		// Ensure the contract was stored.
		ensure_stored(code_hash);

		// Remove the code.
		assert_ok!(Pallet::<Test>::remove_code(RuntimeOrigin::signed(ALICE), code_hash));

		// Ensure the code is no longer stored.
		assert!(!PristineCode::<Test>::contains_key(&code_hash));
	});
}

#[test]
fn upload_fails_if_evm_bytecode_disabled() {
	let (code, _) = compile_module_with_type("Dummy", FixtureType::SolcRuntime).unwrap();

	AllowEvmBytecode::set(false); // Disable support for EVM bytecode.
	ExtBuilder::default().build().execute_with(|| {
		// Upload should fail since support for EVM bytecode is disabled.
		assert_err!(
			Pallet::<Test>::upload_code(RuntimeOrigin::signed(ALICE), code, 1000u128),
			<Error<Test>>::CodeRejected
		);
	});
}

#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn dust_work_with_child_calls(fixture_type: FixtureType) {
	use pallet_revive_fixtures::CallSelfWithDust;
	let (code, _) = compile_module_with_type("CallSelfWithDust", fixture_type).unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code.clone())).build_and_unwrap_contract();

		let value = 1_000_000_000.into();
		builder::bare_call(addr)
			.data(
				CallSelfWithDust::CallSelfWithDustCalls::call(CallSelfWithDust::callCall {})
					.abi_encode(),
			)
			.evm_value(value)
			.build_and_unwrap_result();

		assert_eq!(crate::Pallet::<Test>::evm_balance(&addr), value);
	});
}

#[test]
fn prestate_diff_mode_tracing_works() {
	use alloy_core::hex;

	struct TestCase {
		config: PrestateTracerConfig,
		expected_instantiate_trace_json: &'static str,
		expected_call_trace_json: &'static str,
	}

	let (counter_code, _) = compile_module_with_type("NestedCounter", FixtureType::Solc).unwrap();
	let (contract_runtime_code, _) =
		compile_module_with_type("NestedCounter", FixtureType::SolcRuntime).unwrap();
	let (child_runtime_code, _) =
		compile_module_with_type("Counter", FixtureType::SolcRuntime).unwrap();

	let test_cases = [
		TestCase {
			config: PrestateTracerConfig {
				diff_mode: false,
				disable_storage: false,
				disable_code: false,
			},
			expected_instantiate_trace_json: r#"{
					"{{ALICE_ADDR}}": {
						"balance": "{{ALICE_BALANCE_PRE}}"
					}
				}"#,
			expected_call_trace_json: r#"{
					"{{ALICE_ADDR}}": {
						"balance": "{{ALICE_BALANCE_POST}}",
						"nonce": 1
					},
					"{{CONTRACT_ADDR}}": {
						"balance": "0x0",
						"nonce": 2,
						"code": "{{CONTRACT_CODE}}",
						"storage": {
							"0x0000000000000000000000000000000000000000000000000000000000000000": "{{SLOT0_PACKED_7}}"
						}
					},
					"{{CHILD_ADDR}}": {
						"balance": "0x0",
						"nonce": 1,
						"code": "{{CHILD_CODE}}",
						"storage": {
							"0x0000000000000000000000000000000000000000000000000000000000000000": "0x000000000000000000000000000000000000000000000000000000000000000a"
						}
					}
				}"#,
		},
		TestCase {
			config: PrestateTracerConfig {
				diff_mode: true,
				disable_storage: false,
				disable_code: false,
			},
			expected_instantiate_trace_json: r#"{
					"pre": {
						"{{ALICE_ADDR}}": {
							"balance": "{{ALICE_BALANCE_PRE}}"
						}
					},
					"post": {
						"{{ALICE_ADDR}}": {
							"balance": "{{ALICE_BALANCE_POST}}",
							"nonce": 1
						},
						"{{CONTRACT_ADDR}}": {
							"balance": "0x0",
							"nonce": 2,
							"code": "{{CONTRACT_CODE}}",
							"storage": {
								"0x0000000000000000000000000000000000000000000000000000000000000000": "{{SLOT0_PACKED_7}}"
							}
						},
						"{{CHILD_ADDR}}": {
							"balance": "0x0",
							"nonce": 1,
							"code": "{{CHILD_CODE}}",
							"storage": {
								"0x0000000000000000000000000000000000000000000000000000000000000000": "0x000000000000000000000000000000000000000000000000000000000000000a"
							}
						}
					}
				}"#,
			expected_call_trace_json: r#"{
					"pre": {
						"{{CONTRACT_ADDR}}": {
							"balance": "0x0",
							"nonce": 2,
							"code": "{{CONTRACT_CODE}}",
							"storage": {
								"0x0000000000000000000000000000000000000000000000000000000000000000": "{{SLOT0_PACKED_7}}"
							}
						},
						"{{CHILD_ADDR}}": {
							"balance": "0x0",
							"nonce": 1,
							"code": "{{CHILD_CODE}}",
							"storage": {
								"0x0000000000000000000000000000000000000000000000000000000000000000": "0x000000000000000000000000000000000000000000000000000000000000000a"
							}
						}
					},
					"post": {
						"{{CONTRACT_ADDR}}": {
							"storage": {
								"0x0000000000000000000000000000000000000000000000000000000000000000": "{{SLOT0_PACKED_8}}"
							}
						},
						"{{CHILD_ADDR}}": {
							"storage": {
								"0x0000000000000000000000000000000000000000000000000000000000000000": "0x0000000000000000000000000000000000000000000000000000000000000007"
							}
						}
					}
				}"#,
		},
	];

	for test_case in test_cases {
		ExtBuilder::default().build().execute_with(|| {
			let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000_000_000);

			let contract_addr = crate::address::create1(&ALICE_ADDR, 0u64);
			let child_addr = crate::address::create1(&contract_addr, 1u64);

			// Compute balances
			let alice_balance_pre = Pallet::<Test>::convert_native_to_evm(
				1_000_000_000_000 - Pallet::<Test>::min_balance(),
			);

			let replace_placeholders = |json: &str| -> String {
				let alice_balance_post = Pallet::<Test>::evm_balance(&ALICE_ADDR);

				// Packed slot 0: [4 zero bytes][uint64 number BE][20-byte address]
				let slot0_packed = |number: u64| -> String {
					let mut slot = [0u8; 32];
					slot[4..12].copy_from_slice(&number.to_be_bytes());
					slot[12..32].copy_from_slice(child_addr.as_bytes());
					format!("0x{}", hex::encode(slot))
				};

				json.replace("{{ALICE_ADDR}}", &format!("{:#x}", ALICE_ADDR))
					.replace("{{CONTRACT_ADDR}}", &format!("{:#x}", contract_addr))
					.replace("{{CHILD_ADDR}}", &format!("{:#x}", child_addr))
					.replace("{{ALICE_BALANCE_PRE}}", &format!("{:#x}", alice_balance_pre))
					.replace("{{ALICE_BALANCE_POST}}", &format!("{:#x}", alice_balance_post))
					.replace(
						"{{CONTRACT_CODE}}",
						&format!("0x{}", hex::encode(&contract_runtime_code)),
					)
					.replace("{{CHILD_CODE}}", &format!("0x{}", hex::encode(&child_runtime_code)))
					.replace("{{SLOT0_PACKED_7}}", &slot0_packed(7))
					.replace("{{SLOT0_PACKED_8}}", &slot0_packed(8))
			};

			let mut tracer = PrestateTracer::<Test>::new(test_case.config.clone());
			let Contract { addr: contract_addr_actual, .. } = trace(&mut tracer, || {
				builder::bare_instantiate(Code::Upload(counter_code.clone()))
					.salt(None)
					.build_and_unwrap_contract()
			});
			assert_eq!(contract_addr, contract_addr_actual, "contract address mismatch");

			let instantiate_trace = tracer.collect_trace();

			let expected_json = replace_placeholders(test_case.expected_instantiate_trace_json);
			let expected_trace: PrestateTraceV1 = serde_json::from_str(&expected_json).unwrap();
			assert_eq!(
				PrestateTraceV1::from(instantiate_trace),
				expected_trace,
				"unexpected instantiate trace for {:?}",
				test_case.config
			);

			let mut tracer = PrestateTracer::<Test>::new(test_case.config.clone());
			trace(&mut tracer, || {
				builder::bare_call(contract_addr)
					.data(
						NestedCounter::NestedCounterCalls::nestedNumber(
							NestedCounter::nestedNumberCall {},
						)
						.abi_encode(),
					)
					.build_and_unwrap_result();
			});

			let call_trace = tracer.collect_trace();
			let expected_json = replace_placeholders(test_case.expected_call_trace_json);
			let expected_trace: PrestateTraceV1 = serde_json::from_str(&expected_json).unwrap();
			assert_eq!(
				PrestateTraceV1::from(call_trace),
				expected_trace,
				"unexpected call trace for {:?}",
				test_case.config
			);
		});
	}
}

#[test]
fn eth_substrate_call_dispatches_successfully() {
	use frame_support::traits::fungible::Inspect;
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1000);
		let _ = <Test as Config>::Currency::set_balance(&BOB, 100);

		let transfer_call =
			crate::tests::RuntimeCall::Balances(pallet_balances::Call::transfer_allow_death {
				dest: BOB,
				value: 50,
			});

		assert!(EthBlockBuilderFirstValues::<Test>::get().is_none());

		assert_ok!(Pallet::<Test>::eth_substrate_call(
			Origin::EthTransaction(ALICE).into(),
			Box::new(transfer_call),
			vec![]
		));

		// Verify balance changed
		assert_eq!(<Test as Config>::Currency::balance(&ALICE), 950);
		assert_eq!(<Test as Config>::Currency::balance(&BOB), 150);

		assert!(EthBlockBuilderFirstValues::<Test>::get().is_some());
	});
}

#[test]
fn eth_substrate_call_requires_eth_origin() {
	ExtBuilder::default().build().execute_with(|| {
		let inner_call = frame_system::Call::remark { remark: vec![] };

		// Should fail with non-EthTransaction origin
		assert_noop!(
			Pallet::<Test>::eth_substrate_call(
				RuntimeOrigin::signed(ALICE),
				Box::new(inner_call.into()),
				vec![]
			),
			sp_runtime::traits::BadOrigin
		);
	});
}

#[test]
fn eth_substrate_call_tracks_weight_correctly() {
	use crate::weights::WeightInfo;
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1000);

		let inner_call = frame_system::Call::remark { remark: vec![0u8; 100] };
		let transaction_encoded = vec![0u8; 200];
		let transaction_encoded_len = transaction_encoded.len() as u32;

		let result = Pallet::<Test>::eth_substrate_call(
			Origin::EthTransaction(ALICE).into(),
			Box::new(inner_call.clone().into()),
			transaction_encoded,
		);

		assert_ok!(result);
		let post_info = result.unwrap();

		let overhead = <Test as Config>::WeightInfo::eth_substrate_call(transaction_encoded_len)
			.saturating_add(<Test as Config>::WeightInfo::on_finalize_block_per_tx(
				transaction_encoded_len,
			));
		let expected_weight = overhead.saturating_add(inner_call.get_dispatch_info().call_weight);
		assert!(
			expected_weight == post_info.actual_weight.unwrap(),
			"expected_weight ({}) should be == actual_weight ({})",
			expected_weight,
			post_info.actual_weight.unwrap(),
		);
	});
}

/// Tests execution tracing for both EVM and PVM.
///
/// Each test case runs for both Solc (EVM) and Resolc (PVM) with separate expected traces.
/// Expected traces are stored in `src/tests/json_trace/` directory.
///
/// Gas consistency is verified for consecutive steps:
/// - For EVM: gas - gas_cost == next_step.gas (exact, every opcode is traced)
/// - For PVM: gas - gas_cost >= next_step.gas (inequality, PVM instructions between syscalls
///   consume additional gas that isn't individually traced)
#[test]
fn execution_tracing_works() {
	use crate::{
		evm::{Bytes, ExecutionTrace, ExecutionTracer, ExecutionTracerConfig},
		tracing::trace,
	};
	use pallet_revive_fixtures::{Callee, Caller};

	struct TestCase {
		name: &'static str,
		setup: Box<dyn Fn(FixtureType) -> ExecutionTrace>,
		expected_evm_trace: &'static str,
		expected_pvm_trace: &'static str,
	}

	let test_cases: Vec<TestCase> = vec![
		TestCase {
			name: "Fibonacci",
			setup: Box::new(|fixture_type| {
				let (code, _) = compile_module_with_type("Fibonacci", fixture_type).unwrap();
				let Contract { addr, .. } =
					builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

				let config = ExecutionTracerConfig {
					enable_return_data: true,
					limit: Some(5),
					..Default::default()
				};
				let mut tracer = ExecutionTracer::new(config);
				trace(&mut tracer, || {
					builder::bare_call(addr)
						.data(Fibonacci::fibCall { n: 3 }.abi_encode())
						.build_and_unwrap_result()
				});
				tracer.collect_trace()
			}),
			expected_evm_trace: include_str!("json_trace/fibonacci_evm.json"),
			expected_pvm_trace: include_str!("json_trace/fibonacci_pvm.json"),
		},
		TestCase {
			name: "CALL",
			setup: Box::new(|fixture_type| {
				let (callee_code, _) = compile_module_with_type("Callee", fixture_type).unwrap();
				let Contract { addr: callee, .. } =
					builder::bare_instantiate(Code::Upload(callee_code))
						.build_and_unwrap_contract();

				let (caller_code, _) = compile_module_with_type("Caller", fixture_type).unwrap();
				let Contract { addr: caller, .. } =
					builder::bare_instantiate(Code::Upload(caller_code))
						.build_and_unwrap_contract();

				let config =
					ExecutionTracerConfig { enable_return_data: true, ..Default::default() };
				let mut tracer = ExecutionTracer::new(config);
				trace(&mut tracer, || {
					builder::bare_call(caller)
						.data(
							Caller::normalCall {
								_callee: callee.0.into(),
								_value: 0,
								_data: Callee::echoCall { _data: 42u64 }.abi_encode().into(),
								_gas: u64::MAX,
							}
							.abi_encode(),
						)
						.build_and_unwrap_result()
				});
				tracer.collect_trace()
			}),
			expected_evm_trace: include_str!("json_trace/call_evm.json"),
			expected_pvm_trace: include_str!("json_trace/call_pvm.json"),
		},
		TestCase {
			name: "DELEGATECALL",
			setup: Box::new(|fixture_type| {
				let (callee_code, _) = compile_module_with_type("Callee", fixture_type).unwrap();
				let Contract { addr: callee, .. } =
					builder::bare_instantiate(Code::Upload(callee_code))
						.build_and_unwrap_contract();

				let (caller_code, _) = compile_module_with_type("Caller", fixture_type).unwrap();
				let Contract { addr: caller, .. } =
					builder::bare_instantiate(Code::Upload(caller_code))
						.build_and_unwrap_contract();

				let config =
					ExecutionTracerConfig { enable_return_data: true, ..Default::default() };
				let mut tracer = ExecutionTracer::new(config);
				trace(&mut tracer, || {
					builder::bare_call(caller)
						.data(
							Caller::delegateCall {
								_callee: callee.0.into(),
								_data: Callee::echoCall { _data: 42u64 }.abi_encode().into(),
								_gas: u64::MAX,
							}
							.abi_encode(),
						)
						.build_and_unwrap_result()
				});
				tracer.collect_trace()
			}),
			expected_evm_trace: include_str!("json_trace/delegatecall_evm.json"),
			expected_pvm_trace: include_str!("json_trace/delegatecall_pvm.json"),
		},
	];

	/// Normalizes trace by zeroing out all dynamic values for stable comparisons.
	fn normalize_trace(trace: &ExecutionTraceV1) -> ExecutionTraceV1 {
		use frame_support::weights::Weight;

		let mut normalized = trace.clone();
		normalized.gas = 0;
		normalized.weight_consumed = Weight::zero();
		normalized.base_call_weight = Weight::zero();

		for step in &mut normalized.struct_logs {
			step.gas = 0;
			step.gas_cost = 0;
			step.weight_cost = Weight::zero();

			match &mut step.kind {
				ExecutionStepKindV1::EVMOpcode { stack, .. } => {
					for val in stack.iter_mut() {
						*val = Bytes::from(vec![0u8]);
					}
				},
				ExecutionStepKindV1::PVMSyscall { op, args, returned, .. } => {
					// Normalize call/delegate_call to their _evm variants so
					// the test passes regardless of which resolc version
					// compiled the fixtures (older emits call/delegate_call,
					// newer emits call_evm/delegate_call_evm).
					match op {
						PolkavmSyscallV1::Call | PolkavmSyscallV1::CallEvm => {
							*op = PolkavmSyscallV1::CallEvm;
							// Clear args since the two variants have compatible behavior but
							// different argument layouts.
							args.clear();
						},
						PolkavmSyscallV1::DelegateCall | PolkavmSyscallV1::DelegateCallEvm => {
							*op = PolkavmSyscallV1::DelegateCallEvm;
							args.clear();
						},
						_ => {
							for val in args.iter_mut() {
								*val = 0;
							}
						},
					}
					if returned.is_some() {
						*returned = Some(0);
					}
				},
			}
		}
		normalized
	}

	/// Verifies gas consistency for execution traces.
	///
	/// Gas equations:
	/// - gasCost = opcode_cost only (excluding forwarded gas for call/create opcodes)
	/// - For consecutive steps at the same depth:
	///   - EVM: gas - gas_cost == next_step.gas (exact)
	///   - PVM: gas - gas_cost >= next_step.gas (inequality, PVM instructions consume extra gas)
	fn verify_gas_consistency(trace: &ExecutionTrace, is_evm: bool, name: &str) {
		// Verify consecutive steps at the same depth
		let same_depth_violations: Vec<_> = trace
			.struct_logs
			.iter()
			.zip(trace.struct_logs.iter().skip(1))
			.enumerate()
			.filter(|(_, (curr, next))| curr.depth == next.depth)
			.filter_map(|(i, (curr, next))| {
				let expected = curr.gas.saturating_sub(curr.gas_cost);
				let valid = if is_evm { expected == next.gas } else { next.gas <= expected };
				(!valid).then_some((i, curr.depth, curr.gas, curr.gas_cost, expected, next.gas))
			})
			.collect();

		assert!(
			same_depth_violations.is_empty(),
			"{name}: same-depth gas violations (step, depth, gas, gas_cost, expected, actual): {same_depth_violations:?}",
		);
	}

	for test_case in test_cases {
		for fixture_type in [FixtureType::Solc, FixtureType::Resolc] {
			ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
				let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

				let actual_trace = (test_case.setup)(fixture_type);
				let is_evm = matches!(fixture_type, FixtureType::Solc | FixtureType::SolcRuntime);
				let name = test_case.name;
				let vm_type = if is_evm { "EVM" } else { "PVM" };

				let expected_json_str = if is_evm {
					test_case.expected_evm_trace
				} else {
					test_case.expected_pvm_trace
				};
				let expected: ExecutionTraceV1 = serde_json::from_str(expected_json_str)
					.unwrap_or_else(|e| {
						panic!("{name} ({vm_type}): failed to parse expected JSON: {e}")
					});
				// Normalize both traces for comparison (zeroes out dynamic values)
				let normalized_actual = normalize_trace(&actual_trace.clone().into());
				let normalized_expected = normalize_trace(&expected);
				assert_eq!(
					normalized_actual, normalized_expected,
					"{name} ({vm_type}): trace mismatch"
				);

				verify_gas_consistency(&actual_trace, is_evm, &format!("{name} ({vm_type})"));
			});
		}
	}
}

/// Storage opcodes charge a worst-case amount upfront and refund the remainder via
/// `adjust_weight`, so `weight_consumed` is not monotonic within a step. A step's cost is
/// `(exit - entry) - child`, which is only exact if every refund lands inside the window that
/// charged it; one escaping would show up as a gap between consecutive steps.
#[test]
fn storage_refunds_stay_inside_the_step_that_charged_them() {
	use crate::evm::{ExecutionStepKind, ExecutionTracer, ExecutionTracerConfig};
	use pallet_revive_fixtures::Counter;

	let (code, _) = compile_module_with_type("Counter", FixtureType::Solc).unwrap();

	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// `increment` is SLOAD then SSTORE, so both refunding paths run.
		let mut tracer = ExecutionTracer::new(ExecutionTracerConfig::default());
		trace(&mut tracer, || {
			builder::bare_call(addr)
				.data(Counter::incrementCall {}.abi_encode())
				.build_and_unwrap_result()
		});
		let trace = tracer.collect_trace();

		assert!(
			trace.struct_logs.iter().any(|step| matches!(
				&step.kind,
				ExecutionStepKind::EVMOpcode { op, .. } if *op == SSTORE || *op == SLOAD
			)),
			"the fixture must actually touch storage for this to test anything",
		);

		// Consecutive EVM steps at the same depth must leave the meter continuous.
		let breaks = trace
			.struct_logs
			.iter()
			.zip(trace.struct_logs.iter().skip(1))
			.filter(|(step, next)| step.depth == next.depth)
			.filter(|(step, next)| step.gas.saturating_sub(step.gas_cost) != next.gas)
			.map(|(step, next)| (step.gas, step.gas_cost, next.gas))
			.collect::<Vec<_>>();
		assert!(breaks.is_empty(), "gas discontinuity (gas, gas_cost, next.gas): {breaks:?}");

		let summed: u64 = trace.struct_logs.iter().map(|step| step.gas_cost).sum();
		assert!(
			summed <= trace.gas,
			"steps account for {summed} gas but the transaction used {}",
			trace.gas,
		);
	});
}

/// Truncation drops steps; it must not change the ones it keeps, and what it keeps must never
/// sum past what the transaction spent.
///
/// Runs the real interpreter: the unit tests drive `Tracing` directly and so assert the
/// enter/exit pairing rather than exercise it. Truncation must land inside the nested call,
/// so several limits are swept.
#[test]
fn truncation_does_not_alter_the_steps_it_keeps() {
	use crate::evm::{ExecutionTracer, ExecutionTracerConfig};
	use pallet_revive_fixtures::{Callee, Caller};

	for fixture_type in [FixtureType::Solc, FixtureType::Resolc] {
		let trace_with_limit = |limit: Option<u64>| {
			ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
				let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);

				let (callee_code, _) = compile_module_with_type("Callee", fixture_type).unwrap();
				let Contract { addr: callee, .. } =
					builder::bare_instantiate(Code::Upload(callee_code))
						.build_and_unwrap_contract();
				let (caller_code, _) = compile_module_with_type("Caller", fixture_type).unwrap();
				let Contract { addr: caller, .. } =
					builder::bare_instantiate(Code::Upload(caller_code))
						.build_and_unwrap_contract();

				let mut tracer =
					ExecutionTracer::new(ExecutionTracerConfig { limit, ..Default::default() });
				trace(&mut tracer, || {
					builder::bare_call(caller)
						.data(
							Caller::normalCall {
								_callee: callee.0.into(),
								_value: 0,
								_data: Callee::echoCall { _data: 42u64 }.abi_encode().into(),
								_gas: u64::MAX,
							}
							.abi_encode(),
						)
						.build_and_unwrap_result()
				});
				tracer.collect_trace()
			})
		};

		// Sweep limits relative to the untruncated length rather than hard-coding step counts.
		let full = trace_with_limit(None);
		let steps = full.struct_logs.len() as u64;
		assert!(steps > 8, "{fixture_type:?}: expected a multi-step trace, got {steps}");
		assert!(
			full.struct_logs.iter().any(|step| step.depth > 0),
			"{fixture_type:?}: the fixture must make a nested call for this to be a real test",
		);

		for limit in [steps / 4, steps / 2, steps * 3 / 4] {
			let trace = trace_with_limit(Some(limit));
			assert_eq!(trace.struct_logs.len() as u64, limit, "{fixture_type:?}: truncated");

			for (index, (truncated, full)) in
				trace.struct_logs.iter().zip(full.struct_logs.iter()).enumerate()
			{
				assert_eq!(
					(truncated.gas_cost, truncated.weight_cost),
					(full.gas_cost, full.weight_cost),
					"{fixture_type:?} limit {limit}: step {index} changed under truncation",
				);
			}

			let summed: u64 = trace.struct_logs.iter().map(|step| step.gas_cost).sum();
			assert!(
				summed <= trace.gas,
				"{fixture_type:?} limit {limit}: steps account for {summed} gas \
				 but the transaction used {}",
				trace.gas,
			);
		}
	}
}
