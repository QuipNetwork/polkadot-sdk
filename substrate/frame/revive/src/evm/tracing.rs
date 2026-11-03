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
	Config,
	evm::{CallTrace, ExecutionTrace, Trace},
	tracing::Tracing,
};

mod call_tracing;
pub use call_tracing::*;

mod prestate_tracing;
pub use prestate_tracing::*;

mod execution_tracing;
pub use execution_tracing::*;

/// A composite tracer.
#[derive(derive_more::From, Debug)]
pub enum Tracer<T> {
	/// A tracer that traces calls.
	CallTracer(CallTracer),
	/// A tracer that traces the prestate.
	PrestateTracer(PrestateTracer<T>),
	/// A tracer that traces opcodes and syscalls.
	ExecutionTracer(ExecutionTracer),
}

impl<T: Config> Tracer<T>
where
	T::Nonce: Into<u32>,
{
	/// Returns an empty trace.
	pub fn empty_trace(&self) -> Trace {
		match self {
			Tracer::CallTracer(_) => CallTrace::default().into(),
			Tracer::PrestateTracer(tracer) => tracer.empty_trace().into(),
			Tracer::ExecutionTracer(_) => ExecutionTrace::default().into(),
		}
	}

	/// Get a mutable trait‐object reference to the inner tracer.
	pub fn as_tracing(&mut self) -> &mut (dyn Tracing + 'static) {
		match self {
			Tracer::CallTracer(inner) => inner as &mut dyn Tracing,
			Tracer::PrestateTracer(inner) => inner as &mut dyn Tracing,
			Tracer::ExecutionTracer(inner) => inner as &mut dyn Tracing,
		}
	}

	/// Collect the trace, or `None` when it is empty. A tracer can run and still produce an empty
	/// trace (e.g. a prestate diff with no state changes), so the caller decides what `None` means.
	pub fn collect_trace(self) -> Option<Trace> {
		let empty = self.empty_trace();
		let trace = match self {
			Tracer::CallTracer(inner) => Trace::Call(inner.collect_trace().unwrap_or_default()),
			Tracer::PrestateTracer(inner) => Trace::Prestate(inner.collect_trace()),
			Tracer::ExecutionTracer(inner) => Trace::Execution(inner.collect_trace()),
		};
		(trace != empty).then_some(trace)
	}

	/// Check if this is an execution tracer.
	pub fn is_execution_tracer(&self) -> bool {
		matches!(self, Tracer::ExecutionTracer(_))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::tests::{ExtBuilder, Test};

	// An extrinsic that runs no contract code must produce no trace, so
	// consumers can rely on a trace's presence to mean a contract actually ran.
	#[test]
	fn collect_trace_is_none_when_nothing_executed() {
		ExtBuilder::default().build().execute_with(|| {
			let call = Tracer::<Test>::CallTracer(CallTracer::new(Default::default()));
			let prestate = Tracer::<Test>::PrestateTracer(PrestateTracer::new(Default::default()));
			let execution =
				Tracer::<Test>::ExecutionTracer(ExecutionTracer::new(Default::default()));

			assert_eq!(call.collect_trace(), None);
			assert_eq!(prestate.collect_trace(), None);
			assert_eq!(execution.collect_trace(), None);
		});
	}
}
