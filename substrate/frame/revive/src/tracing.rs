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

use crate::{Code, DispatchError, Key, Weight, evm::Bytes, primitives::ExecReturnValue};
use alloc::vec::Vec;
use environmental::environmental;
use sp_core::{H160, H256, U256};

environmental!(tracer: dyn Tracing + 'static);

/// Synthetic syscall name used for tracing PVM interpreter fuel consumption between real syscalls.
pub const PVM_FUEL_NAME: &str = "pvm_fuel";

/// Trace the execution of the given closure.
///
/// # Warning
///
/// Only meant to be called from off-chain code as its additional resource usage is
/// not accounted for in the weights or memory envelope.
pub fn trace<R, F: FnOnce() -> R>(tracer: &mut (dyn Tracing + 'static), f: F) -> R {
	tracer::using_once(tracer, f)
}

/// Run the closure when tracing is enabled.
///
/// This is safe to be called from on-chain code as tracing will never be activated
/// there. Hence the closure is not executed in this case.
pub(crate) fn if_tracing<R, F: FnOnce(&mut (dyn Tracing + 'static)) -> R>(f: F) -> Option<R> {
	tracer::with(f)
}

/// Interface to provide frame trace information for the current execution frame.
pub trait FrameTraceInfo {
	/// Get the amount of gas remaining in the current frame.
	fn gas_left(&self) -> u64;

	/// Returns how much weight was spent
	fn weight_consumed(&self) -> Weight;

	/// Get the output from the last frame.
	fn last_frame_output(&self) -> Bytes;
}

/// Interface to provide EVM-specific trace information for the current execution frame.
pub trait EVMFrameTraceInfo: FrameTraceInfo {
	/// Get a snapshot of the memory at this point in execution.
	///
	/// # Parameters
	/// - `limit`: Maximum number of memory words to capture.
	fn memory_snapshot(&self, limit: usize) -> Vec<Bytes>;

	/// Get a snapshot of the stack at this point in execution.
	fn stack_snapshot(&self) -> Vec<Bytes>;
}

/// Defines methods to trace contract interactions.
///
/// # Contract
///
/// Every [`Tracing::enter_opcode`] and [`Tracing::enter_ecall`] must be followed by exactly one
/// [`Tracing::exit_step`], on every path including reverts and traps. Tracers pair the two to
/// attribute a step's cost, so an unmatched call charges that step to an enclosing one.
pub trait Tracing {
	/// Register an address that should be traced.
	fn watch_address(&mut self, _addr: &H160) {}

	/// Called before a contract call is executed.
	///
	/// For CALL/DELEGATECALL opcodes:
	/// - `gas_limit`: gas forwarded to the child call
	fn enter_child_span(
		&mut self,
		_from: H160,
		_to: H160,
		_delegate_call: Option<H160>,
		_is_read_only: bool,
		_value: U256,
		_input: &[u8],
		_gas_limit: u64,
	) {
	}

	/// Called when a contract calls terminates (selfdestructs)
	fn terminate(
		&mut self,
		_contract_address: H160,
		_beneficiary_address: H160,
		_gas_left: u64,
		_value: U256,
	) {
	}

	/// Record the next code and salt to be instantiated.
	fn instantiate_code(&mut self, _code: &Code, _salt: Option<&[u8; 32]>) {}

	/// Called when a balance is read
	fn balance_read(&mut self, _addr: &H160, _value: U256) {}

	/// Called when storage read is called
	fn storage_read(&mut self, _key: &Key, _value: Option<&[u8]>) {}

	/// Called when storage write is called
	fn storage_write(
		&mut self,
		_key: &Key,
		_old_value: Option<Vec<u8>>,
		_new_value: Option<&[u8]>,
	) {
	}

	/// Record a log event
	fn log_event(&mut self, _event: H160, _topics: &[H256], _data: &[u8], _log_index: u32) {}

	/// Called after a contract call is executed
	fn exit_child_span(
		&mut self,
		_output: &ExecReturnValue,
		_gas_used: u64,
		_weight_consumed: Weight,
	) {
	}

	/// Called when a contract call terminates with an error
	fn exit_child_span_with_error(
		&mut self,
		_error: DispatchError,
		_gas_used: u64,
		_weight_consumed: Weight,
	) {
	}

	/// Check if the tracer is an execution tracer.
	fn is_execution_tracer(&self) -> bool {
		false
	}

	/// Called before an EVM opcode is executed.
	///
	/// # Parameters
	/// - `pc`: The current program counter.
	/// - `opcode`: The opcode being executed.
	/// - `trace_info`: Information about the current execution frame.
	fn enter_opcode(&mut self, _pc: u64, _opcode: u8, _trace_info: &dyn EVMFrameTraceInfo) {}

	/// Called before a PVM syscall is executed.
	///
	/// # Parameters
	/// - `ecall`: The name of the syscall being executed.
	/// - `args`: The syscall arguments (register values).
	/// - `trace_info`: Information about the current execution frame.
	fn enter_ecall(
		&mut self,
		_ecall: &'static str,
		_args: &[u64],
		_trace_info: &dyn FrameTraceInfo,
	) {
	}

	/// Called after an EVM opcode or PVM syscall is executed to record the gas cost.
	///
	/// # Parameters
	/// - `trace_info`: Information about the current execution frame.
	/// - `returned`: The syscall return value (PVM only, `None` for EVM opcodes).
	fn exit_step(&mut self, _trace_info: &dyn FrameTraceInfo, _returned: Option<u64>) {}

	/// Called once the transaction completes to report the gas consumed by the meter.
	///
	/// # Parameters
	/// - `base_call_weight`: Extrinsic base weight that is added on top of `weight_consumed` when
	///   charging.
	/// - `weight_consumed`: Weight used by the transaction logic, excluding the extrinsic base
	///   weight.
	fn dispatch_result(&mut self, _base_call_weight: Weight, _weight_consumed: Weight) {}
}
