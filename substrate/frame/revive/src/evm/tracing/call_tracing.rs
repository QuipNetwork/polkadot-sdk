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
	Code, DispatchError, Weight,
	evm::{CallLog, CallTrace, CallTracerConfig, CallType, decode_revert_reason},
	primitives::ExecReturnValue,
	tracing::Tracing,
};
use alloc::{format, string::ToString, vec::Vec};
use sp_core::{H160, H256, U256};

/// A Tracer that reports logs and nested call traces transactions.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct CallTracer {
	/// Store all in-progress CallTrace instances.
	traces: Vec<CallTrace>,
	/// Stack of indices to the current active traces.
	current_stack: Vec<usize>,
	/// The code and salt used to instantiate the next contract.
	code_with_salt: Option<(Code, bool)>,
	/// The tracer configuration.
	config: CallTracerConfig,
}

impl CallTracer {
	/// Create a new [`CallTracer`] instance.
	pub fn new(config: CallTracerConfig) -> Self {
		Self { traces: Vec::new(), code_with_salt: None, current_stack: Vec::new(), config }
	}

	/// Collect the traces and return them.
	pub fn collect_trace(mut self) -> Option<CallTrace> {
		self.traces.pop()
	}
}

impl Tracing for CallTracer {
	fn instantiate_code(&mut self, code: &Code, salt: Option<&[u8; 32]>) {
		self.code_with_salt = Some((code.clone(), salt.is_some()));
	}

	fn terminate(
		&mut self,
		contract_address: H160,
		beneficiary_address: H160,
		gas_left: u64,
		value: U256,
	) {
		self.traces.last_mut().unwrap().calls.push(CallTrace {
			from: contract_address,
			to: beneficiary_address,
			call_type: CallType::Selfdestruct,
			gas: gas_left,
			value: Some(value),
			..Default::default()
		});
	}

	fn enter_child_span(
		&mut self,
		from: H160,
		to: H160,
		delegate_call: Option<H160>,
		is_read_only: bool,
		value: U256,
		input: &[u8],
		gas_limit: u64,
	) {
		// Increment parent's child call count.
		if let Some(&index) = self.current_stack.last() &&
			let Some(trace) = self.traces.get_mut(index)
		{
			trace.child_call_count += 1;
		}

		if self.traces.is_empty() || !self.config.only_top_call {
			let (call_type, input) = match self.code_with_salt.take() {
				Some((Code::Upload(code), salt)) => (
					if salt { CallType::Create2 } else { CallType::Create },
					code.into_iter().chain(input.to_vec().into_iter()).collect::<Vec<_>>(),
				),
				Some((Code::Existing(code_hash), salt)) => (
					if salt { CallType::Create2 } else { CallType::Create },
					code_hash
						.to_fixed_bytes()
						.into_iter()
						.chain(input.to_vec().into_iter())
						.collect::<Vec<_>>(),
				),
				None => {
					let call_type = if is_read_only {
						CallType::StaticCall
					} else if delegate_call.is_some() {
						CallType::DelegateCall
					} else {
						CallType::Call
					};
					(call_type, input.to_vec())
				},
			};

			self.traces.push(CallTrace {
				from,
				to,
				value: if is_read_only { None } else { Some(value) },
				call_type,
				input: input.into(),
				gas: gas_limit,
				..Default::default()
			});

			// Push the index onto the stack of the current active trace
			self.current_stack.push(self.traces.len() - 1);

		// We only track the top call, we just push a dummy index
		} else {
			self.current_stack.push(2);
		}
	}

	fn log_event(&mut self, address: H160, topics: &[H256], data: &[u8], log_index: u32) {
		if !self.config.with_logs {
			return;
		}

		let current_index = self.current_stack.last().unwrap();

		if let Some(trace) = self.traces.get_mut(*current_index) {
			let log = CallLog {
				address,
				topics: topics.to_vec(),
				data: data.to_vec().into(),
				position: trace.child_call_count,
				index: log_index,
			};

			trace.logs.push(log);
		}
	}

	fn exit_child_span(
		&mut self,
		output: &ExecReturnValue,
		gas_used: u64,
		_weight_consumed: Weight,
	) {
		self.code_with_salt = None;

		// Set the output of the current trace
		let current_index = self.current_stack.pop().unwrap();

		if let Some(trace) = self.traces.get_mut(current_index) {
			trace.output = output.data.clone().into();
			trace.gas_used = gas_used;

			if output.did_revert() {
				trace.revert_reason = decode_revert_reason(&output.data);
				trace.error = Some("execution reverted".to_string());
			}

			if self.config.only_top_call {
				return;
			}

			//  Move the current trace into its parent
			if let Some(parent_index) = self.current_stack.last() {
				let child_trace = self.traces.remove(current_index);
				self.traces[*parent_index].calls.push(child_trace);
			}
		}
	}

	fn exit_child_span_with_error(
		&mut self,
		error: DispatchError,
		gas_used: u64,
		_weight_consumed: Weight,
	) {
		self.code_with_salt = None;

		// Set the output of the current trace
		let current_index = self.current_stack.pop().unwrap();

		if let Some(trace) = self.traces.get_mut(current_index) {
			trace.gas_used = gas_used;

			trace.error = match error {
				DispatchError::Module(sp_runtime::ModuleError { message, .. }) => {
					Some(message.unwrap_or_default().to_string())
				},
				_ => Some(format!("{:?}", error)),
			};

			if self.config.only_top_call {
				return;
			}

			//  Move the current trace into its parent
			if let Some(parent_index) = self.current_stack.last() {
				let child_trace = self.traces.remove(current_index);
				self.traces[*parent_index].calls.push(child_trace);
			}
		}
	}
}
