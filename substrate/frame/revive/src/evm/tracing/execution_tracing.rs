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
	DispatchError, ExecReturnValue, Key, Weight,
	evm::{
		Bytes, ExecutionStep, ExecutionStepKind, ExecutionTrace, ExecutionTracerConfig,
		tracing::Tracing,
	},
	tracing::{EVMFrameTraceInfo, FrameTraceInfo},
	vm::pvm::env::lookup_trace_op_index,
};
use alloc::{
	collections::BTreeMap,
	format,
	string::{String, ToString},
	vec::Vec,
};
use sp_core::{H160, U256};

/// Tracks a pending step (opcode/syscall) that hasn't completed yet.
/// Used to accumulate child call consumption for CALL-like opcodes.
#[derive(Default, Debug, Clone, PartialEq)]
struct PendingStep {
	/// Index of this step in `steps`, or `None` when the step was dropped by the limit.
	step_index: Option<usize>,
	/// Accumulated gas consumed by child calls.
	child_gas: u64,
	/// Accumulated weight consumed by child calls.
	child_weight: Weight,
}

/// A tracer that traces opcode and syscall execution step-by-step.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct ExecutionTracer {
	/// The tracer configuration.
	config: ExecutionTracerConfig,

	/// The collected trace steps.
	steps: Vec<ExecutionStep>,

	/// Stack of pending steps awaiting their exit_step call.
	/// When entering an opcode/syscall, we push here.
	/// When exit_step is called, we pop and finalize the step's gas/weight costs.
	pending: Vec<PendingStep>,

	/// Current call depth.
	depth: u16,

	/// Whether any step has been dropped, after which `steps` no longer tracks execution.
	steps_dropped: bool,

	/// Total gas used by the transaction.
	total_gas_used: u64,

	/// The base call weight of the transaction.
	base_call_weight: Weight,

	/// The Weight consumed by the transaction meter.
	weight_consumed: Weight,

	/// Whether the transaction failed.
	failed: bool,

	/// The return value of the transaction.
	return_value: Bytes,

	/// List of storage per call depth.
	storages_per_call: Vec<BTreeMap<Bytes, Bytes>>,
}

impl ExecutionTracer {
	/// Create a new [`ExecutionTracer`] instance.
	pub fn new(config: ExecutionTracerConfig) -> Self {
		Self {
			config,
			steps: Vec::new(),
			pending: Vec::new(),
			depth: 0,
			steps_dropped: false,
			total_gas_used: 0,
			base_call_weight: Default::default(),
			weight_consumed: Default::default(),
			failed: false,
			return_value: Bytes::default(),
			storages_per_call: alloc::vec![Default::default()],
		}
	}

	/// Collect the traces and return them.
	pub fn collect_trace(self) -> ExecutionTrace {
		let Self {
			steps: struct_logs,
			weight_consumed,
			base_call_weight,
			return_value,
			total_gas_used: gas,
			failed,
			..
		} = self;
		ExecutionTrace { gas, weight_consumed, base_call_weight, failed, return_value, struct_logs }
	}

	/// Whether [`ExecutionTracerConfig::limit`] has been reached, so further steps are dropped.
	fn is_truncated(&self) -> bool {
		self.config.limit.is_some_and(|limit| self.steps.len() as u64 >= limit)
	}

	/// Index of the step currently executing, or `None` when it was dropped by the limit.
	fn current_step_index(&self) -> Option<usize> {
		self.pending.last()?.step_index
	}

	/// Open a pending entry for a starting step, capturing it unless the limit was reached.
	///
	/// A dropped step still occupies an entry: [`Tracing`] guarantees an `exit_step` either way,
	/// and that exit pops one.
	fn push_step(&mut self, build: impl FnOnce(&ExecutionTracerConfig, u16) -> ExecutionStep) {
		let step_index = if self.is_truncated() {
			None
		} else {
			let step = build(&self.config, self.depth);
			self.steps.push(step);
			Some(self.steps.len() - 1)
		};
		self.steps_dropped |= step_index.is_none();

		self.pending
			.push(PendingStep { step_index, child_gas: 0, child_weight: Weight::zero() });
	}

	/// Record the transaction-level result. The outermost frame exits at depth 1, since every
	/// frame enters through `enter_child_span` and `depth` is decremented after this runs.
	fn finish_transaction(&mut self, failed: bool, return_value: Bytes, gas_used: u64) {
		if self.depth != 1 {
			return;
		}

		self.failed |= failed;
		self.return_value = return_value;
		self.total_gas_used = gas_used;
	}

	/// The step a storage snapshot belongs to, or `None` when the access can be ignored.
	fn storage_snapshot_target(&self) -> Option<usize> {
		if self.config.disable_storage {
			return None;
		}

		self.current_step_index()
	}

	fn snapshot_storage_into(&mut self, step_index: usize) {
		let Some(storage) = self.storages_per_call.last() else { return };

		if let Some(step) = self.steps.get_mut(step_index) {
			if let ExecutionStepKind::EVMOpcode { storage: ref mut step_storage, .. } = step.kind {
				*step_storage = Some(storage.clone());
			}
		}
	}

	/// Record an error against the step that failed.
	fn record_error(&mut self, error: String) {
		let target = if self.steps_dropped {
			self.current_step_index()
		} else {
			self.steps.len().checked_sub(1)
		};

		if let Some(step) = target.and_then(|index| self.steps.get_mut(index)) {
			step.error = Some(error);
		}
	}
}

impl Tracing for ExecutionTracer {
	fn is_execution_tracer(&self) -> bool {
		true
	}

	fn dispatch_result(&mut self, base_call_weight: Weight, weight_consumed: Weight) {
		self.base_call_weight = base_call_weight;
		self.weight_consumed = weight_consumed;
	}

	fn enter_opcode(&mut self, pc: u64, opcode: u8, trace_info: &dyn EVMFrameTraceInfo) {
		self.push_step(|config, depth| {
			let stack_data =
				if !config.disable_stack { trace_info.stack_snapshot() } else { Vec::new() };

			let memory_data = if config.enable_memory {
				trace_info.memory_snapshot(config.memory_word_limit as usize)
			} else {
				Vec::new()
			};

			let return_data = if config.enable_return_data {
				trace_info.last_frame_output()
			} else {
				Bytes::default()
			};

			ExecutionStep {
				gas: trace_info.gas_left(),
				gas_cost: Default::default(),
				weight_cost: trace_info.weight_consumed(), /* Store initial weight, will be
				                                            * updated later */
				depth,
				return_data,
				error: None,
				kind: ExecutionStepKind::EVMOpcode {
					pc: pc as u32,
					op: opcode,
					stack: stack_data,
					memory: memory_data,
					storage: None,
				},
			}
		});
	}

	fn enter_ecall(&mut self, ecall: &'static str, args: &[u64], trace_info: &dyn FrameTraceInfo) {
		self.push_step(|config, depth| {
			let return_data = if config.enable_return_data {
				trace_info.last_frame_output()
			} else {
				Bytes::default()
			};

			let syscall_args =
				if !config.disable_syscall_details { args.to_vec() } else { Vec::new() };

			ExecutionStep {
				gas: trace_info.gas_left(),
				gas_cost: Default::default(),
				weight_cost: trace_info.weight_consumed(), /* Store initial weight, will be
				                                            * updated later */
				depth,
				return_data,
				error: None,
				kind: ExecutionStepKind::PVMSyscall {
					op: lookup_trace_op_index(ecall).unwrap_or_default(),
					args: syscall_args,
					returned: None,
				},
			}
		});
	}

	fn exit_step(&mut self, trace_info: &dyn FrameTraceInfo, returned: Option<u64>) {
		let Some(pending) = self.pending.pop() else {
			debug_assert!(false, "exit_step without a matching enter_opcode/enter_ecall");
			return;
		};

		// A dropped step has no cost to attribute; its accumulated child credit goes with it.
		let Some(step_index) = pending.step_index else { return };
		let Some(step) = self.steps.get_mut(step_index) else { return };

		let total_gas = step.gas.saturating_sub(trace_info.gas_left());
		step.gas_cost = total_gas.saturating_sub(pending.child_gas);

		// weight_cost currently holds initial weight; calculate total then subtract child
		let total_weight = trace_info.weight_consumed().saturating_sub(step.weight_cost);
		step.weight_cost = total_weight.saturating_sub(pending.child_weight);

		if !self.config.disable_syscall_details &&
			let ExecutionStepKind::PVMSyscall { returned: ref mut ret, .. } = step.kind
		{
			*ret = returned;
		}
	}

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
		// Costs will be calculated in exit_step by subtracting child consumption from total.
		self.storages_per_call.push(Default::default());
		self.depth += 1;
	}

	fn exit_child_span(
		&mut self,
		output: &ExecReturnValue,
		gas_used: u64,
		weight_consumed: Weight,
	) {
		// Accumulate child consumption to the parent step
		if let Some(parent) = self.pending.last_mut() {
			parent.child_gas = parent.child_gas.saturating_add(gas_used);
			parent.child_weight = parent.child_weight.saturating_add(weight_consumed);
		}

		if output.did_revert() {
			self.record_error("execution reverted".to_string());
		}

		self.finish_transaction(output.did_revert(), Bytes(output.data.to_vec()), gas_used);

		self.storages_per_call.pop();

		if self.depth > 0 {
			self.depth -= 1;
		}
	}

	fn exit_child_span_with_error(
		&mut self,
		error: DispatchError,
		gas_used: u64,
		weight_consumed: Weight,
	) {
		// Accumulate child consumption to the parent step
		if let Some(parent) = self.pending.last_mut() {
			parent.child_gas = parent.child_gas.saturating_add(gas_used);
			parent.child_weight = parent.child_weight.saturating_add(weight_consumed);
		}

		self.record_error(format!("{:?}", error));

		self.finish_transaction(true, Bytes::default(), gas_used);

		if self.depth > 0 {
			self.depth -= 1;
		}

		self.storages_per_call.pop();
	}

	fn storage_write(&mut self, key: &Key, _old_value: Option<Vec<u8>>, new_value: Option<&[u8]>) {
		let Some(step_index) = self.storage_snapshot_target() else { return };

		if let Some(storage) = self.storages_per_call.last_mut() {
			let key_bytes = crate::evm::Bytes(key.unhashed().to_vec());
			let value_bytes = crate::evm::Bytes(
				new_value.map(|v| v.to_vec()).unwrap_or_else(|| alloc::vec![0u8; 32]),
			);
			storage.insert(key_bytes, value_bytes);
		}

		self.snapshot_storage_into(step_index);
	}

	fn storage_read(&mut self, key: &Key, value: Option<&[u8]>) {
		let Some(step_index) = self.storage_snapshot_target() else { return };

		if let Some(storage) = self.storages_per_call.last_mut() {
			let key_bytes = crate::evm::Bytes(key.unhashed().to_vec());
			storage.entry(key_bytes).or_insert_with(|| {
				crate::evm::Bytes(value.map(|v| v.to_vec()).unwrap_or_else(|| alloc::vec![0u8; 32]))
			});
		}

		self.snapshot_storage_into(step_index);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::tracing::{EVMFrameTraceInfo, FrameTraceInfo, PVM_FUEL_NAME};
	use core::cell::Cell;
	use pallet_revive_uapi::ReturnFlags;
	use pretty_assertions::assert_eq;
	use revm::bytecode::opcode::{CALL, PUSH1, SSTORE};

	/// A stub execution frame. [`Frame::burn`] advances gas and weight in lockstep, so a step's
	/// expected cost is the same number on both meters.
	struct Frame {
		gas_left: Cell<u64>,
		consumed: Cell<u64>,
	}

	impl Frame {
		fn new(gas: u64) -> Self {
			Self { gas_left: Cell::new(gas), consumed: Cell::new(0) }
		}

		fn burn(&self, amount: u64) {
			self.gas_left.set(self.gas_left.get() - amount);
			self.consumed.set(self.consumed.get() + amount);
		}
	}

	impl FrameTraceInfo for Frame {
		fn gas_left(&self) -> u64 {
			self.gas_left.get()
		}

		fn weight_consumed(&self) -> Weight {
			Weight::from_parts(self.consumed.get(), self.consumed.get())
		}

		fn last_frame_output(&self) -> Bytes {
			Bytes::default()
		}
	}

	impl EVMFrameTraceInfo for Frame {
		fn memory_snapshot(&self, _limit: usize) -> Vec<Bytes> {
			Vec::new()
		}

		fn stack_snapshot(&self) -> Vec<Bytes> {
			Vec::new()
		}
	}

	fn enter_frame(tracer: &mut ExecutionTracer) {
		tracer.enter_child_span(
			H160::zero(),
			H160::zero(),
			None,
			false,
			U256::zero(),
			&[],
			u64::MAX,
		);
	}

	fn reverted() -> ExecReturnValue {
		ExecReturnValue { flags: ReturnFlags::REVERT, data: Vec::new() }
	}

	/// The storage slots a captured step recorded, by their first key byte.
	fn slots_of(step: &ExecutionStep) -> Vec<u8> {
		match &step.kind {
			ExecutionStepKind::EVMOpcode { storage: Some(storage), .. } => {
				storage.keys().map(|key| key.0[0]).collect()
			},
			_ => Vec::new(),
		}
	}

	fn exit_frame(tracer: &mut ExecutionTracer, consumed: u64) {
		tracer.exit_child_span(
			&ExecReturnValue::default(),
			consumed,
			Weight::from_parts(consumed, consumed),
		);
	}

	/// Once truncation begins `steps` stops advancing, so hooks that fire while execution
	/// continues must not pile onto the last captured step.
	#[test]
	fn truncated_steps_do_not_graft_onto_the_last_captured_step() {
		let config = ExecutionTracerConfig { limit: Some(2), ..Default::default() };
		let mut tracer = ExecutionTracer::new(config);
		let frame = Frame::new(1_000);

		enter_frame(&mut tracer);

		for slot in 1u8..=4 {
			tracer.enter_opcode(slot as u64, SSTORE, &frame);
			tracer.storage_write(&Key::from_fixed([slot; 32]), None, Some(&[slot]));
			frame.burn(10);
			tracer.exit_step(&frame, None);
		}

		// A child call reverts, well past the limit.
		enter_frame(&mut tracer);
		tracer.exit_child_span(&reverted(), 5, Weight::from_parts(5, 5));

		exit_frame(&mut tracer, 45);

		let trace = tracer.collect_trace();
		assert_eq!(trace.struct_logs.len(), 2);
		assert_eq!(slots_of(&trace.struct_logs[0]), alloc::vec![1]);
		assert_eq!(
			slots_of(&trace.struct_logs[1]),
			alloc::vec![1, 2],
			"the boundary step keeps its own write and none of the dropped ones",
		);
		assert_eq!(trace.struct_logs[1].error, None, "it did not revert; a later frame did");
	}

	/// Reaching the limit is not the same as dropping a step: until something is actually
	/// entered past it the trace is still complete, so its last step takes the annotation.
	#[test]
	fn a_complete_trace_that_reached_its_limit_still_annotates() {
		let config = ExecutionTracerConfig { limit: Some(1), ..Default::default() };
		let mut tracer = ExecutionTracer::new(config);
		let frame = Frame::new(1_000);

		enter_frame(&mut tracer);
		tracer.enter_opcode(0, PUSH1, &frame);
		frame.burn(10);
		tracer.exit_step(&frame, None);
		tracer.exit_child_span(&reverted(), 10, Weight::from_parts(10, 10));

		let trace = tracer.collect_trace();
		assert_eq!(
			trace.struct_logs[0].error.as_deref(),
			Some("execution reverted"),
			"nothing was dropped, so the last captured step is still the last executed one",
		);
	}

	/// A captured call keeps its error annotation whether or not the callee ran a step before
	/// failing, which the caller cannot observe.
	#[test]
	fn a_captured_call_keeps_its_error_annotation() {
		for callee_steps in 0..2u64 {
			let config = ExecutionTracerConfig { limit: Some(1), ..Default::default() };
			let mut tracer = ExecutionTracer::new(config);
			let frame = Frame::new(1_000);

			enter_frame(&mut tracer);

			// The CALL is step 0, so it is captured and is what reaches the limit.
			tracer.enter_opcode(0, CALL, &frame);
			enter_frame(&mut tracer);

			for pc in 0..callee_steps {
				tracer.enter_opcode(pc, PUSH1, &frame);
				frame.burn(5);
				tracer.exit_step(&frame, None);
			}

			frame.burn(5);
			tracer.exit_child_span(&reverted(), 5, Weight::from_parts(5, 5));
			tracer.exit_step(&frame, None);
			exit_frame(&mut tracer, 20);

			let trace = tracer.collect_trace();
			assert_eq!(trace.struct_logs.len(), 1);
			assert_eq!(
				trace.struct_logs[0].error.as_deref(),
				Some("execution reverted"),
				"{callee_steps} callee steps: the CALL is captured and is what failed",
			);
			assert!(
				!trace.failed,
				"{callee_steps} callee steps: the reverting frame is not the outermost one",
			);
		}
	}

	/// A step's cost is a window on the meters, exclusive of the child frames it contains, so
	/// captured steps describe disjoint windows and can never sum to more than the transaction
	/// consumed. Truncation may only omit cost, never invent it.
	#[test]
	fn truncated_steps_keep_captured_costs_disjoint() {
		let config = ExecutionTracerConfig { limit: Some(3), ..Default::default() };
		let mut tracer = ExecutionTracer::new(config);
		let frame = Frame::new(1_000);

		enter_frame(&mut tracer); // the transaction's own frame

		// Step 0: a plain opcode costing 10.
		tracer.enter_opcode(0, PUSH1, &frame);
		frame.burn(10);
		tracer.exit_step(&frame, None);

		// Step 1: the outer CALL, entered with 990 gas left.
		tracer.enter_opcode(1, CALL, &frame);
		enter_frame(&mut tracer);
		frame.burn(10);

		// Step 2: the inner CALL, entered with 980 gas left. This one reaches the limit.
		tracer.enter_opcode(2, CALL, &frame);
		enter_frame(&mut tracer);

		// Steps in the innermost frame are past the limit and dropped, but the interpreter
		// still reports their exits. Both entry points have to keep the stack aligned, so
		// exercise an EVM opcode and a PVM syscall.
		tracer.enter_opcode(0, PUSH1, &frame);
		frame.burn(10);
		tracer.exit_step(&frame, None);

		tracer.enter_ecall(PVM_FUEL_NAME, &[], &frame);
		frame.burn(10);
		tracer.exit_step(&frame, None);

		// Unwind. Each CALL keeps its own overhead: its window minus what its frame consumed.
		frame.burn(12);
		exit_frame(&mut tracer, 25);
		tracer.exit_step(&frame, None); // inner CALL: (980 - 948) - 25 = 7
		frame.burn(3);
		exit_frame(&mut tracer, 40);
		tracer.exit_step(&frame, None); // outer CALL: (990 - 945) - 40 = 5
		frame.burn(5);
		exit_frame(&mut tracer, 60);

		tracer.dispatch_result(Weight::zero(), Weight::from_parts(60, 60));

		let trace = tracer.collect_trace();
		let costs = trace
			.struct_logs
			.iter()
			.map(|step| (step.gas_cost, step.weight_cost))
			.collect::<Vec<_>>();

		assert_eq!(
			costs,
			alloc::vec![
				(10, Weight::from_parts(10, 10)), // the plain opcode
				(5, Weight::from_parts(5, 5)),    // outer CALL, its frame excluded
				(7, Weight::from_parts(7, 7)),    // inner CALL, its frame excluded
			],
			"the two steps past the limit are dropped and the CALLs keep only their own cost",
		);

		assert_eq!(trace.gas, 60);
		assert_eq!(trace.weight_consumed, Weight::from_parts(60, 60));
	}
}
