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

use crate::{Weight, evm::Bytes};
use alloc::{collections::BTreeMap, string::String, vec::Vec};
use derive_more::From;
use pallet_revive_types::runtime_api::*;
use sp_core::{H160, H256, U256};
use sp_runtime::ApplyExtrinsicResult;

/// The type of tracer to use.
#[derive(Debug, Clone, PartialEq, From)]
pub enum TracerType {
	/// A tracer that traces calls.
	CallTracer(Option<CallTracerConfig>),

	/// A tracer that traces the prestate.
	PrestateTracer(Option<PrestateTracerConfig>),

	/// A tracer that traces opcodes and syscalls.
	ExecutionTracer(Option<ExecutionTracerConfig>),
}

impl Default for TracerType {
	fn default() -> Self {
		TracerType::ExecutionTracer(Some(ExecutionTracerConfig::default()))
	}
}

impl From<TracerTypeV1> for TracerType {
	fn from(value: TracerTypeV1) -> Self {
		match value {
			TracerTypeV1::CallTracer(config) => Self::CallTracer(config.map(Into::into)),
			TracerTypeV1::PrestateTracer(config) => Self::PrestateTracer(config.map(Into::into)),
			TracerTypeV1::ExecutionTracer(config) => Self::ExecutionTracer(config.map(Into::into)),
		}
	}
}

/// The configuration for the call tracer.
#[derive(Clone, Debug, PartialEq)]
pub struct CallTracerConfig {
	/// Whether to include logs in the trace.
	pub with_logs: bool,

	/// Whether to only include the top-level calls in the trace.
	pub only_top_call: bool,
}

impl Default for CallTracerConfig {
	fn default() -> Self {
		Self { with_logs: true, only_top_call: false }
	}
}

impl From<CallTracerConfigV1> for CallTracerConfig {
	fn from(value: CallTracerConfigV1) -> Self {
		Self { with_logs: value.with_logs, only_top_call: value.only_top_call }
	}
}

/// The configuration for the prestate tracer.
#[derive(Clone, Debug, PartialEq)]
pub struct PrestateTracerConfig {
	/// Whether to include the diff mode in the trace.
	pub diff_mode: bool,

	/// Whether to include storage in the trace.
	pub disable_storage: bool,

	/// Whether to include code in the trace.
	pub disable_code: bool,
}

impl Default for PrestateTracerConfig {
	fn default() -> Self {
		Self { diff_mode: false, disable_storage: false, disable_code: false }
	}
}

impl From<PrestateTracerConfigV1> for PrestateTracerConfig {
	fn from(value: PrestateTracerConfigV1) -> Self {
		Self {
			diff_mode: value.diff_mode,
			disable_storage: value.disable_storage,
			disable_code: value.disable_code,
		}
	}
}

/// The configuration for the execution tracer.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionTracerConfig {
	/// Whether to enable memory capture
	pub enable_memory: bool,

	/// Whether to disable stack capture
	pub disable_stack: bool,

	/// Whether to disable storage capture
	pub disable_storage: bool,

	/// Whether to enable return data capture
	pub enable_return_data: bool,

	/// Whether to disable syscall details capture, including arguments and return value (PVM only)
	pub disable_syscall_details: bool,

	/// Limit number of steps captured
	pub limit: Option<u64>,

	/// Maximum number of memory words to capture per step (default: 16)
	pub memory_word_limit: u32,
}

impl Default for ExecutionTracerConfig {
	fn default() -> Self {
		Self {
			enable_memory: false,
			disable_stack: false,
			disable_storage: false,
			enable_return_data: false,
			disable_syscall_details: false,
			limit: None,
			memory_word_limit: 16,
		}
	}
}

impl From<ExecutionTracerConfigV1> for ExecutionTracerConfig {
	fn from(value: ExecutionTracerConfigV1) -> Self {
		Self {
			enable_memory: value.enable_memory,
			disable_stack: value.disable_stack,
			disable_storage: value.disable_storage,
			enable_return_data: value.enable_return_data,
			disable_syscall_details: value.disable_syscall_details,
			limit: value.limit,
			memory_word_limit: value.memory_word_limit,
		}
	}
}

/// The type of call that was executed.
#[derive(Default, Eq, PartialEq, Clone, Debug)]
pub enum CallType {
	/// A regular call.
	#[default]
	Call,
	/// A read-only call.
	StaticCall,
	/// A delegate call.
	DelegateCall,
	/// A create call.
	Create,
	/// A create2 call.
	Create2,
	/// A selfdestruct call.
	Selfdestruct,
}

impl From<CallType> for CallTypeV1 {
	fn from(value: CallType) -> Self {
		match value {
			CallType::Call => Self::Call,
			CallType::StaticCall => Self::StaticCall,
			CallType::DelegateCall => Self::DelegateCall,
			CallType::Create => Self::Create,
			CallType::Create2 => Self::Create2,
			CallType::Selfdestruct => Self::Selfdestruct,
		}
	}
}

/// A Trace
#[derive(From, Clone, Debug, Eq, PartialEq)]
pub enum Trace {
	/// A call trace.
	Call(CallTrace),
	/// A prestate trace.
	Prestate(PrestateTrace),
	/// An execution trace (opcodes and syscalls).
	Execution(ExecutionTrace),
}

impl From<Trace> for TraceV1 {
	fn from(value: Trace) -> Self {
		match value {
			Trace::Call(value) => Self::Call(value.into()),
			Trace::Prestate(value) => Self::Prestate(value.into()),
			Trace::Execution(value) => Self::Execution(value.into()),
		}
	}
}

impl From<Trace> for TraceV2 {
	fn from(value: Trace) -> Self {
		match value {
			Trace::Call(value) => Self::Call(value.into()),
			Trace::Prestate(value) => Self::Prestate(value.into()),
			Trace::Execution(value) => Self::Execution(value.into()),
		}
	}
}

/// A single extrinsic's trace, or a signal that it could not be traced.
pub enum TraceEntry {
	/// The extrinsic's trace.
	Traced(Trace),
	/// The extrinsic could not be traced.
	NotTraced,
}

impl TraceEntry {
	/// The entry for an extrinsic that produced no trace: `NotTraced` if the replay dropped it,
	/// `None` if there was nothing to trace. `ExhaustsResources` is the only spurious failure a
	/// faithful replay introduces.
	pub fn for_untraced(result: &ApplyExtrinsicResult) -> Option<Self> {
		matches!(result, Err(err) if err.exhausted_resources()).then_some(Self::NotTraced)
	}
}

impl From<TraceEntry> for TraceEntryV1 {
	fn from(value: TraceEntry) -> Self {
		match value {
			TraceEntry::Traced(trace) => TraceEntryV1::Traced(trace.into()),
			TraceEntry::NotTraced => TraceEntryV1::NotTraced,
		}
	}
}

/// A prestate Trace
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrestateTrace {
	/// The Prestate mode returns the accounts necessary to execute a given transaction
	Prestate(BTreeMap<H160, PrestateTraceInfo>),

	/// The diff mode returns the differences between the transaction's pre and post-state
	/// The result only contains the accounts that were modified by the transaction
	DiffMode {
		/// The state before the call.
		/// The accounts in the `pre` field will contain all of their basic fields, even if those
		/// fields have not been modified. For `storage` however, only non-empty slots that have
		/// been modified will be included
		pre: BTreeMap<H160, PrestateTraceInfo>,
		/// The state after the call.
		/// It only contains the specific fields that were actually modified during the transaction
		post: BTreeMap<H160, PrestateTraceInfo>,
	},
}

impl PrestateTrace {
	/// Returns the pre and post trace info.
	pub fn state_mut(
		&mut self,
	) -> (&mut BTreeMap<H160, PrestateTraceInfo>, Option<&mut BTreeMap<H160, PrestateTraceInfo>>) {
		match self {
			PrestateTrace::Prestate(pre) => (pre, None),
			PrestateTrace::DiffMode { pre, post } => (pre, Some(post)),
		}
	}
}

impl From<PrestateTrace> for PrestateTraceV1 {
	fn from(value: PrestateTrace) -> Self {
		let convert = |v: BTreeMap<H160, PrestateTraceInfo>| {
			v.into_iter().map(|(k, v)| (k, v.into())).collect()
		};

		match value {
			PrestateTrace::Prestate(accounts) => Self::Prestate(convert(accounts)),
			PrestateTrace::DiffMode { pre, post } => {
				Self::DiffMode { pre: convert(pre), post: convert(post) }
			},
		}
	}
}

/// The info of a prestate trace.
#[derive(Default, Clone, Debug, Eq, PartialEq)]
pub struct PrestateTraceInfo {
	/// The balance of the account.
	pub balance: Option<U256>,
	/// The nonce of the account.
	pub nonce: Option<u32>,
	/// The code of the contract account.
	pub code: Option<Bytes>,
	/// The storage of the contract account.
	pub storage: BTreeMap<Bytes, Option<Bytes>>,
}

impl From<PrestateTraceInfo> for PrestateTraceInfoV1 {
	fn from(value: PrestateTraceInfo) -> Self {
		Self {
			balance: value.balance,
			nonce: value.nonce,
			code: value.code,
			storage: value.storage,
		}
	}
}

/// An execution trace containing the step-by-step execution of EVM opcodes and PVM syscalls.
/// This matches Geth's structLogger output format.
#[derive(Default, Clone, Debug, Eq, PartialEq)]
pub struct ExecutionTrace {
	/// Total gas used by the transaction.
	pub gas: u64,
	/// The weight consumed by the transaction meter.
	pub weight_consumed: Weight,
	/// The base call weight of the transaction.
	pub base_call_weight: Weight,
	/// Whether the transaction failed.
	pub failed: bool,
	/// The return value of the transaction.
	pub return_value: Bytes,
	/// The list of execution steps (structLogs in Geth).
	pub struct_logs: Vec<ExecutionStep>,
}

impl From<ExecutionTrace> for ExecutionTraceV1 {
	fn from(value: ExecutionTrace) -> Self {
		Self {
			gas: value.gas,
			weight_consumed: value.weight_consumed,
			base_call_weight: value.base_call_weight,
			failed: value.failed,
			return_value: value.return_value,
			struct_logs: value.struct_logs.into_iter().map(Into::into).collect(),
		}
	}
}

/// An execution step which can be either an EVM opcode or a PVM syscall.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct ExecutionStep {
	/// Remaining gas before executing this step.
	pub gas: u64,
	/// Gas Cost of executing this step.
	pub gas_cost: u64,
	/// Weight cost of executing this step.
	pub weight_cost: Weight,
	/// Current call depth.
	pub depth: u16,
	/// Return data from last frame output.
	pub return_data: Bytes,
	/// Any error that occurred during execution.
	pub error: Option<String>,
	/// The kind of execution step (EVM opcode or PVM syscall).
	pub kind: ExecutionStepKind,
}

impl From<ExecutionStep> for ExecutionStepV1 {
	fn from(value: ExecutionStep) -> Self {
		Self {
			gas: value.gas,
			gas_cost: value.gas_cost,
			weight_cost: value.weight_cost,
			depth: value.depth,
			return_data: value.return_data,
			error: value.error,
			kind: value.kind.into(),
		}
	}
}

/// The kind of execution step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionStepKind {
	/// An EVM opcode execution.
	EVMOpcode {
		/// The program counter.
		pc: u32,
		/// The opcode being executed.
		op: u8,
		/// EVM stack contents.
		stack: Vec<Bytes>,
		/// EVM memory contents.
		memory: Vec<Bytes>,
		/// Contract storage changes.
		storage: Option<alloc::collections::BTreeMap<Bytes, Bytes>>,
	},
	/// A PVM syscall execution.
	PVMSyscall {
		/// The executed syscall.
		op: u8,
		/// The syscall arguments (register values a0-a5).
		/// Omitted when `disable_syscall_details` is true in ExecutionTracerConfig.
		args: Vec<u64>,
		/// The syscall return value.
		/// Omitted when `disable_syscall_details` is true in ExecutionTracerConfig.
		returned: Option<u64>,
	},
}

impl Default for ExecutionStepKind {
	fn default() -> Self {
		Self::EVMOpcode { pc: 0, op: 0, stack: Vec::new(), memory: Vec::new(), storage: None }
	}
}

impl From<ExecutionStepKind> for ExecutionStepKindV1 {
	fn from(value: ExecutionStepKind) -> Self {
		match value {
			ExecutionStepKind::EVMOpcode { pc, op, stack, memory, storage } => {
				Self::EVMOpcode { pc, op: EvmOpcodeV1(op), stack, memory, storage }
			},
			ExecutionStepKind::PVMSyscall { op, args, returned } => Self::PVMSyscall {
				op: op
					.try_into()
					.expect("all sys calls produced by revive are valid. Tested in env.rs; qed"),
				args,
				returned,
			},
		}
	}
}

/// A smart contract execution call trace.
#[derive(Default, Clone, Debug, Eq, PartialEq)]
pub struct CallTrace {
	/// Address of the sender.
	pub from: H160,
	/// Amount of gas provided for the call.
	pub gas: u64,
	/// Amount of gas used.
	pub gas_used: u64,
	/// Address of the receiver.
	pub to: H160,
	/// Call input data.
	pub input: Bytes,
	/// Return data.
	pub output: Bytes,
	/// The error message if the call failed.
	pub error: Option<String>,
	/// The revert reason, if the call reverted.
	pub revert_reason: Option<String>,
	/// List of sub-calls.
	pub calls: Vec<CallTrace>,
	/// List of logs emitted during the call.
	pub logs: Vec<CallLog>,
	/// Amount of value transferred.
	pub value: Option<U256>,
	/// Type of call.
	pub call_type: CallType,
	/// Number of child calls entered (for log position calculation)
	pub child_call_count: u32,
}

impl From<CallTrace> for CallTraceV1 {
	fn from(value: CallTrace) -> Self {
		Self {
			from: value.from,
			gas: value.gas,
			gas_used: value.gas_used,
			to: value.to,
			input: value.input,
			output: value.output,
			error: value.error,
			revert_reason: value.revert_reason,
			calls: value.calls.into_iter().map(Into::into).collect(),
			logs: value.logs.into_iter().map(Into::into).collect(),
			value: value.value,
			call_type: value.call_type.into(),
			child_call_count: value.child_call_count,
		}
	}
}

impl From<CallTrace> for CallTraceV2 {
	fn from(value: CallTrace) -> Self {
		Self {
			from: value.from,
			gas: value.gas,
			gas_used: value.gas_used,
			to: value.to,
			input: value.input,
			output: value.output,
			error: value.error,
			revert_reason: value.revert_reason,
			calls: value.calls.into_iter().map(Into::into).collect(),
			logs: value.logs.into_iter().map(Into::into).collect(),
			value: value.value,
			call_type: value.call_type.into(),
		}
	}
}

/// A log emitted during a call.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct CallLog {
	/// The address of the contract that emitted the log.
	pub address: H160,
	/// The topics used to index the log.
	pub topics: Vec<H256>,
	/// The log's data.
	pub data: Bytes,
	/// Position of the log relative to subcalls within the same trace
	/// See <https://github.com/ethereum/go-ethereum/pull/28389> for details
	pub position: u32,
	/// The block-wide index of the log, matching the `logIndex` in receipts and Geth's call
	/// tracer. Distinct from `position`, which tracks ordering relative to sub-calls within the
	/// same trace frame.
	pub index: u32,
}

impl From<CallLog> for CallLogV1 {
	fn from(value: CallLog) -> Self {
		Self {
			address: value.address,
			topics: value.topics,
			data: value.data,
			position: value.position,
		}
	}
}

impl From<CallLog> for CallLogV2 {
	fn from(value: CallLog) -> Self {
		Self {
			address: value.address,
			topics: value.topics,
			data: value.data,
			position: value.position,
			index: value.index,
		}
	}
}
