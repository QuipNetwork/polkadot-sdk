// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//  http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::*;
use serde::{Deserialize, Serialize};

/// Tracer configuration used to trace calls.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TracerConfig {
	/// The tracer type.
	#[serde(flatten, default)]
	pub config: TracerTypeV1,

	/// Timeout for the tracer.
	#[serde(with = "humantime_serde", default)]
	pub timeout: Option<core::time::Duration>,
}

impl<'de> Deserialize<'de> for TracerConfig {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::de::Deserializer<'de>,
	{
		#[derive(Deserialize)]
		#[serde(rename_all = "camelCase")]
		struct TracerConfigWithType {
			#[serde(flatten)]
			config: TracerTypeV1,
			#[serde(with = "humantime_serde", default)]
			timeout: Option<core::time::Duration>,
		}

		#[derive(Deserialize)]
		#[serde(rename_all = "camelCase")]
		struct TracerConfigInline {
			#[serde(flatten, default)]
			execution_tracer_config: ExecutionTracerConfigV1,
			#[serde(with = "humantime_serde", default)]
			timeout: Option<core::time::Duration>,
		}

		#[derive(Deserialize)]
		#[serde(untagged)]
		enum TracerConfigHelper {
			WithType(TracerConfigWithType),
			Inline(TracerConfigInline),
		}

		match TracerConfigHelper::deserialize(deserializer)? {
			TracerConfigHelper::WithType(cfg) => {
				Ok(TracerConfig { config: cfg.config, timeout: cfg.timeout })
			},
			TracerConfigHelper::Inline(cfg) => Ok(TracerConfig {
				config: TracerTypeV1::ExecutionTracer(Some(cfg.execution_tracer_config)),
				timeout: cfg.timeout,
			}),
		}
	}
}

/// Configuration for `debug_traceCall`, extending [`TracerConfig`] with state overrides.
///
/// Per the [Geth specification](https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-debug#debugtracecall),
/// `debug_traceCall` accepts a config object that is a superset of the base tracer config,
/// adding `stateOverrides` (and optionally `blockOverrides` and `txIndex`, which are not yet
/// supported).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceCallConfig {
	/// The base tracer configuration (tracer type, timeout, etc.).
	#[serde(flatten)]
	pub tracer_config: TracerConfig,

	/// Optional state overrides to apply before executing the traced call.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub state_overrides: Option<StateOverrideSetV1>,
}

/// A transaction trace
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TransactionTrace {
	/// The transaction hash.
	pub tx_hash: H256,
	/// The trace, or the reason it is unavailable.
	#[serde(flatten)]
	pub outcome: TraceOutcome,
}

/// The outcome of tracing a single transaction, geth style: `result` or `error`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum TraceOutcome {
	/// The transaction trace.
	#[serde(rename = "result")]
	Trace(TraceV1),
	/// Why no trace could be produced.
	#[serde(rename = "error")]
	Error(String),
}

#[cfg(test)]
mod tests {
	use super::*;
	use pallet_revive_types::runtime_api::*;

	#[test]
	fn transaction_trace_serializes_geth_style() {
		let traced = TransactionTrace {
			tx_hash: H256::zero(),
			outcome: TraceOutcome::Trace(TraceV1::Call(CallTraceV1::default())),
		};
		let json = serde_json::to_value(&traced).unwrap();
		assert!(json.get("result").is_some());
		assert!(json.get("error").is_none());
		// `flatten` deserializes through serde's content buffer, which is where untagged enums
		// like `TraceV1` tend to break: pin the round-trip, not just the serialization.
		let round: TransactionTrace = serde_json::from_value(json).unwrap();
		assert!(matches!(round.outcome, TraceOutcome::Trace(TraceV1::Call(_))));

		let dropped = TransactionTrace {
			tx_hash: H256::zero(),
			outcome: TraceOutcome::Error("trace unavailable".into()),
		};
		let json = serde_json::to_value(&dropped).unwrap();
		assert_eq!(json["error"], "trace unavailable");
		assert!(json.get("result").is_none());

		let round: TransactionTrace = serde_json::from_value(json).unwrap();
		assert!(matches!(round.outcome, TraceOutcome::Error(_)));
	}

	/// A geth `callTracer` block-trace entry (`{ txHash, result: <call frame> }`) — the exact wire
	/// shape `debug_traceBlock*` returns. Deserialized into our types below to prove compatibility.
	const GETH_CALL_TRACE: &str = r#"{
		"txHash": "0xabababababababababababababababababababababababababababababababab",
		"result": {
			"from": "0x1111111111111111111111111111111111111111",
			"gas": "0x5208",
			"gasUsed": "0x5000",
			"to": "0x2222222222222222222222222222222222222222",
			"input": "0xdeadbeef",
			"output": "0x0102",
			"value": "0x3e8",
			"type": "CALL",
			"calls": [
				{
					"from": "0x2222222222222222222222222222222222222222",
					"gas": "0x1000",
					"gasUsed": "0x800",
					"to": "0x3333333333333333333333333333333333333333",
					"input": "0xaa",
					"type": "STATICCALL"
				},
				{
					"from": "0x2222222222222222222222222222222222222222",
					"gas": "0x1000",
					"gasUsed": "0x1000",
					"to": "0x5555555555555555555555555555555555555555",
					"input": "0xbb",
					"output": "0x08c379a0",
					"error": "execution reverted",
					"revertReason": "nope",
					"type": "CALL"
				}
			],
			"logs": [
				{
					"address": "0x2222222222222222222222222222222222222222",
					"topics": [
						"0x4444444444444444444444444444444444444444444444444444444444444444"
					],
					"data": "0x99",
					"position": "0x0"
				}
			]
		}
	}"#;

	// The JSON we emit for a call trace must match what geth/alloy produce; deserialize a geth
	// callTracer entry and round-trip it through the alloy types to pin that.
	#[test]
	fn transaction_trace_roundtrips_through_alloy() {
		use alloy_rpc_types::trace::geth::{GethTrace, TraceResult};
		let original: TransactionTrace = serde_json::from_str(GETH_CALL_TRACE).unwrap();
		let json = serde_json::to_value(&original).unwrap();
		let alloy: TraceResult = serde_json::from_value(json).unwrap();
		// `GethTrace` is untagged with a `JS(Value)` catch-all; pin that our JSON parsed as a geth
		// call frame, else a future field mismatch would round-trip vacuously.
		assert!(matches!(alloy, TraceResult::Success { result: GethTrace::CallTracer(_), .. }));
		let back = serde_json::to_value(&alloy).unwrap();
		let round: TransactionTrace = serde_json::from_value(back).unwrap();
		assert_eq!(round, original);
	}

	#[test]
	fn call_trace_roundtrips_through_alloy_call_frame() {
		use alloy_rpc_types::trace::geth::CallFrame;
		// The `result` object of a callTracer entry is a bare call frame.
		let entry: serde_json::Value = serde_json::from_str(GETH_CALL_TRACE).unwrap();
		let original: CallTraceV1 = serde_json::from_value(entry["result"].clone()).unwrap();
		let json = serde_json::to_value(&original).unwrap();
		let frame: CallFrame = serde_json::from_value(json).unwrap();
		let back = serde_json::to_value(&frame).unwrap();
		let round: CallTraceV1 = serde_json::from_value(back).unwrap();
		assert_eq!(round, original);
	}

	/// Serialization should support the following JSON format:
	///
	/// ```json
	/// { "tracer": "callTracer", "tracerConfig": { "withLogs": false } }
	/// ```
	///
	/// ```json
	/// { "tracer": "callTracer" }
	/// ```
	///
	/// By default if not specified the tracer is an ExecutionTracer, and it's config is passed
	/// inline
	///
	/// ```json
	/// { "tracer": null,  "enableMemory": true, "disableStack": false, "disableStorage": false, "enableReturnData": true  }
	/// ```
	#[test]
	fn test_tracer_config_serialization() {
		let tracers = vec![
			(
				r#"{ "enableMemory": true, "disableStack": false, "disableStorage": false,
		"enableReturnData": true }"#,
				TracerConfig {
					config: TracerTypeV1::ExecutionTracer(Some(ExecutionTracerConfigV1 {
						enable_memory: true,
						disable_stack: false,
						disable_storage: false,
						enable_return_data: true,
						disable_syscall_details: false,
						limit: None,
						memory_word_limit: 16,
					})),
					timeout: None,
				},
			),
			(
				r#"{  }"#,
				TracerConfig {
					config: TracerTypeV1::ExecutionTracer(Some(ExecutionTracerConfigV1::default())),
					timeout: None,
				},
			),
			(
				r#"{"tracer": "callTracer"}"#,
				TracerConfig { config: TracerTypeV1::CallTracer(None), timeout: None },
			),
			(
				r#"{"tracer": "callTracer", "tracerConfig": { "withLogs": false }}"#,
				TracerConfig {
					config: Some(CallTracerConfigV1 { with_logs: false, only_top_call: false })
						.into(),
					timeout: None,
				},
			),
			(
				r#"{"tracer": "callTracer", "tracerConfig": { "onlyTopCall": true }}"#,
				TracerConfig {
					config: Some(CallTracerConfigV1 { with_logs: true, only_top_call: true })
						.into(),
					timeout: None,
				},
			),
			(
				r#"{"tracer": "callTracer", "tracerConfig": { "onlyTopCall": true }, "timeout":
		"10ms"}"#,
				TracerConfig {
					config: Some(CallTracerConfigV1 { with_logs: true, only_top_call: true })
						.into(),
					timeout: Some(core::time::Duration::from_millis(10)),
				},
			),
			(
				r#"{"tracer": "executionTracer"}"#,
				TracerConfig { config: TracerTypeV1::ExecutionTracer(None), timeout: None },
			),
			(
				r#"{"tracer": "executionTracer", "tracerConfig": { "enableMemory": true }}"#,
				TracerConfig {
					config: Some(ExecutionTracerConfigV1 {
						enable_memory: true,
						..Default::default()
					})
					.into(),
					timeout: None,
				},
			),
			(
				r#"{ "enableMemory": true }"#,
				TracerConfig {
					config: Some(ExecutionTracerConfigV1 {
						enable_memory: true,
						..Default::default()
					})
					.into(),
					timeout: None,
				},
			),
		];

		for (json_data, expected) in tracers {
			let result: TracerConfig =
				serde_json::from_str(json_data).expect("Deserialization should succeed");
			assert_eq!(result, expected, "invalid serialization for {json_data}");
		}
	}
}
