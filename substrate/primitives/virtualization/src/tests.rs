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

//! Shared test driver for the virtualization forwarder API.
//!
//! The cases exercised here only use the public [`Module`] / [`Instance`] / [`Execution`]
//! surface, so they can be invoked from two distinct contexts:
//!
//! - **Runtime-side**, compiled into `sc-runtime-test` and dispatched through the wasm executor —
//!   this exercises the full host-function FFI round-trip.
//! - **Host-side**, invoked as a regular `#[test]` from `sc-virtualization` — this exercises the
//!   native dispatch path with no wasm involved.
//!
//! Entry point: [`run`]. Tests that need pre-populated externalities (e.g. the storage
//! fallback path of `Module::from_storage_key`) live as standalone host-only `#[test]`s
//! in `sc-virtualization` rather than here.

use crate::{ExecError, ExecResult, Execution, Instance, Module, ModuleError};

/// Default gas budget used by every test driver.
pub const GAS_MAX: i64 = i64::MAX;

/// Run every test that uses only the public forwarder API.
///
/// Exported as a regular function so it can be invoked from both:
/// - the runtime (compiled into `sc-runtime-test`) — exercises the wasm/forwarder path end-to-end,
///   including the host-function FFI;
/// - native host tests (in `sc-virtualization`) — exercises the host-side dispatch directly.
///
/// Tests that need pre-populated externalities (storage fallback for `compile_from_storage_key`)
/// can't run from a runtime context and live as standalone `#[test]`s in `sc-virtualization`.
///
/// The `program` needs to be set to `sp_virtualization_test_fixture::binary()`. It can't be
/// hard coded because when this crate is compiled into a runtime the binary is not available.
/// Instead, we pass it as an argument to the runtime exported function.
pub fn run(program: &[u8]) {
	counter_start_at_0(program);
	counter_start_at_7(program);
	counter_multiple_calls(program);
	panic_works(program);
	exit_works(program);
	run_out_of_gas_works(program);
	gas_consumption_works(program);
	memory_reset_on_instantiate(program);
	memory_persistent(program);
	counter_in_subcall(program);
	from_storage_key_not_found(program);
}

/// The result of running a program to completion.
pub enum RunResult {
	/// Execution finished normally. The idle instance is returned for reuse.
	Ok(Instance),
	/// A syscall handler signalled exit.
	Exit,
	/// Execution returned an error.
	Err(ExecError),
}

/// Drives the prepare/run loop calling `handler` for each syscall.
///
/// The closure receives `(execution, syscall_symbol, a0, a1, a2, a3, a4, a5)` and returns
/// `Ok(return_value)` to resume or `Err(())` to signal exit (trap).
pub fn run_loop(
	mut execution: Execution,
	gas_left: &mut i64,
	mut handler: impl FnMut(&mut Execution, &[u8], u64, u64, u64, u64, u64, u64) -> Result<u64, ()>,
) -> RunResult {
	let mut a0 = 0u64;
	loop {
		match execution.run(*gas_left, a0) {
			ExecResult::Finished { instance, gas_left: g } => {
				*gas_left = g;
				return RunResult::Ok(instance);
			},
			ExecResult::Syscall {
				execution: e,
				gas_left: g,
				syscall_symbol,
				a0: sa0,
				a1,
				a2,
				a3,
				a4,
				a5,
			} => {
				execution = e;
				*gas_left = g;
				match handler(&mut execution, syscall_symbol.as_ref(), sa0, a1, a2, a3, a4, a5) {
					Ok(result) => a0 = result,
					Err(()) => return RunResult::Exit,
				}
			},
			ExecResult::Error { instance: _, error: ExecError::OutOfGas } => {
				*gas_left = 0;
				return RunResult::Err(ExecError::OutOfGas);
			},
			ExecResult::Error { instance: _, error } => return RunResult::Err(error),
		}
	}
}

/// The standard syscall handler for the test fixture.
///
/// Captures `counter` from the caller; memory access goes through the `&mut Execution` passed
/// on each invocation.
pub fn make_handler<'a>(
	counter: &'a mut u64,
) -> impl FnMut(&mut Execution, &[u8], u64, u64, u64, u64, u64, u64) -> Result<u64, ()> + 'a {
	move |execution, syscall_symbol, a0, _a1, _a2, _a3, _a4, _a5| match syscall_symbol {
		b"read_counter" => {
			let buf = counter.to_le_bytes();
			execution.write_memory(a0 as u32, buf.as_ref()).unwrap();
			Ok(1)
		},
		b"increment_counter" => {
			let mut buf = [0u8; 8];
			execution.read_memory(a0 as u32, buf.as_mut()).unwrap();
			*counter += u64::from_le_bytes(buf);
			Ok(2u64 << 56)
		},
		b"exit" => Err(()),
		_ => panic!("unknown syscall: {:?}", syscall_symbol),
	}
}

/// Checks memory access and user state functionality.
fn counter_start_at_0(program: &[u8]) {
	let instance = Module::from_bytes(program, None).unwrap().0.instantiate().unwrap();
	let execution = instance.prepare(b"counter").unwrap();
	let mut gas_left = GAS_MAX;
	let mut counter: u64 = 0;
	let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
	assert!(matches!(result, RunResult::Ok(_)));
	assert_eq!(counter, 8);
}

/// Checks memory access and user state functionality.
fn counter_start_at_7(program: &[u8]) {
	let instance = Module::from_bytes(program, None).unwrap().0.instantiate().unwrap();
	let execution = instance.prepare(b"counter").unwrap();
	let mut gas_left = GAS_MAX;
	let mut counter: u64 = 7;
	let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
	assert!(matches!(result, RunResult::Ok(_)));
	assert_eq!(counter, 15);
}

/// Makes sure user state is persistent between calls into the same instance.
fn counter_multiple_calls(program: &[u8]) {
	let instance = Module::from_bytes(program, None).unwrap().0.instantiate().unwrap();
	let execution = instance.prepare(b"counter").unwrap();
	let mut gas_left = GAS_MAX;
	let mut counter: u64 = 7;

	let instance = match run_loop(execution, &mut gas_left, make_handler(&mut counter)) {
		RunResult::Ok(instance) => instance,
		_ => panic!("expected Ok"),
	};
	assert_eq!(counter, 15);

	let execution = instance.prepare(b"counter").unwrap();
	let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
	assert!(matches!(result, RunResult::Ok(_)));
	assert_eq!(counter, 23);
}

/// Check the correct status is returned when hitting an `unimp` instruction.
fn panic_works(program: &[u8]) {
	let instance = Module::from_bytes(program, None).unwrap().0.instantiate().unwrap();
	let execution = instance.prepare(b"do_panic").unwrap();
	let mut gas_left = GAS_MAX;
	let mut counter: u64 = 0;
	let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
	assert!(matches!(result, RunResult::Err(ExecError::Trap)));
	assert_eq!(counter, 0);
}

/// Check that setting exit in a host function aborts the execution.
fn exit_works(program: &[u8]) {
	let instance = Module::from_bytes(program, None).unwrap().0.instantiate().unwrap();
	let execution = instance.prepare(b"do_exit").unwrap();
	let mut gas_left = GAS_MAX;
	let mut counter: u64 = 0;
	let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
	assert!(matches!(result, RunResult::Exit));
	assert_eq!(counter, 0);
}

/// Increment the counter in an endless loop until we run out of gas.
fn run_out_of_gas_works(program: &[u8]) {
	let instance = Module::from_bytes(program, None).unwrap().0.instantiate().unwrap();
	let execution = instance.prepare(b"increment_forever").unwrap();
	let mut gas_left: i64 = 100_000;
	let mut counter: u64 = 0;
	let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
	assert!(matches!(result, RunResult::Err(ExecError::OutOfGas)));
	assert_eq!(counter, 793);
	assert_eq!(gas_left, 0);
}

/// Call same function with different gas limits and make sure they consume the same amount of gas.
fn gas_consumption_works(program: &[u8]) {
	let gas_limit_0 = GAS_MAX;
	let gas_limit_1 = gas_limit_0 / 2;

	let instance = Module::from_bytes(program, None).unwrap().0.instantiate().unwrap();
	let execution = instance.prepare(b"counter").unwrap();
	let mut gas_left = gas_limit_0;
	let mut counter: u64 = 0;
	let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
	assert!(matches!(result, RunResult::Ok(_)));
	let gas_consumed = gas_limit_0 - gas_left;

	let instance = Module::from_bytes(program, None).unwrap().0.instantiate().unwrap();
	let execution = instance.prepare(b"counter").unwrap();
	let mut gas_left = gas_limit_1;
	let mut counter: u64 = 0;
	let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
	assert!(matches!(result, RunResult::Ok(_)));
	assert_eq!(gas_consumed, gas_limit_1 - gas_left);
}

/// Make sure that globals are reset for a new instance.
fn memory_reset_on_instantiate(program: &[u8]) {
	let instance = Module::from_bytes(program, None).unwrap().0.instantiate().unwrap();
	let execution = instance.prepare(b"offset").unwrap();
	let mut gas_left = GAS_MAX;
	let mut counter: u64 = 0;
	let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
	assert!(matches!(result, RunResult::Ok(_)));
	assert_eq!(counter, 3);

	let instance = Module::from_bytes(program, None).unwrap().0.instantiate().unwrap();
	let execution = instance.prepare(b"offset").unwrap();
	let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
	assert!(matches!(result, RunResult::Ok(_)));
	assert_eq!(counter, 6);
}

/// Make sure globals are not reset between multiple calls into the same instance.
fn memory_persistent(program: &[u8]) {
	let instance = Module::from_bytes(program, None).unwrap().0.instantiate().unwrap();
	let execution = instance.prepare(b"offset").unwrap();
	let mut gas_left = GAS_MAX;
	let mut counter: u64 = 0;

	let instance = match run_loop(execution, &mut gas_left, make_handler(&mut counter)) {
		RunResult::Ok(instance) => instance,
		_ => panic!("expected Ok"),
	};
	assert_eq!(counter, 3);

	let execution = instance.prepare(b"offset").unwrap();
	let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
	assert!(matches!(result, RunResult::Ok(_)));
	assert_eq!(counter, 7);
}

/// Calls a function that spawns another instance where it calls the `counter` entry point.
fn counter_in_subcall(program: &[u8]) {
	let instance = Module::from_bytes(program, None).unwrap().0.instantiate().unwrap();
	let execution = instance.prepare(b"do_subcall").unwrap();
	let mut gas_left = GAS_MAX;
	let mut counter: u64 = 0;
	let program = program.to_vec();
	let result =
		run_loop(execution, &mut gas_left, |execution, syscall_symbol, a0, a1, a2, a3, a4, a5| {
			match syscall_symbol {
				b"read_counter" | b"increment_counter" | b"exit" => {
					make_handler(&mut counter)(execution, syscall_symbol, a0, a1, a2, a3, a4, a5)
				},
				// subcall: spawn a new instance and run counter in it
				b"subcall" => {
					let sub_instance = Module::from_bytes(program.as_ref(), None)
						.unwrap()
						.0
						.instantiate()
						.unwrap();
					let sub_execution = sub_instance.prepare(b"counter").unwrap();
					let mut sub_gas = GAS_MAX;
					let mut sub_counter: u64 = 0;
					let result =
						run_loop(sub_execution, &mut sub_gas, make_handler(&mut sub_counter));
					assert!(matches!(result, RunResult::Ok(_)));
					assert_eq!(sub_counter, 8);
					Ok(0)
				},
				_ => panic!("unknown syscall: {:?}", syscall_symbol),
			}
		});
	assert!(matches!(result, RunResult::Ok(_)));
	// sub call should not affect parent state
	assert_eq!(counter, 0);
}

/// Storage key not in cache and no code in storage returns NotFound.
fn from_storage_key_not_found(_program: &[u8]) {
	let storage_key = b"::missing::";
	assert!(matches!(Module::from_storage_key(storage_key, b""), Err(ModuleError::NotFound)));
}
