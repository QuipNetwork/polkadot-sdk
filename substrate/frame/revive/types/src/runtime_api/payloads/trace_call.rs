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

use codec::{Decode, Encode};
use derive_more::{From, TryInto};
use scale_info::TypeInfo;

use crate::runtime_api::*;

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct TraceCallInputPayloadV1 {
	pub tx: GenericTransactionV1,
	pub config: TracerTypeV1,
	pub state_overrides: Option<StateOverrideSetV1>,
}

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct TraceCallInputPayloadV2 {
	pub tx: GenericTransactionV1,
	pub config: TracerTypeV1,
	pub state_overrides: Option<StateOverrideSetV1>,
}

/// The input type used when calling the `trace_call_versioned` runtime API function. This function
/// replaces the unversioned `trace_call` and `trace_call_with_config` runtime API functions.
#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto)]
pub enum TraceCallVersionedInputPayload {
	/// The arguments provided when calling the `trace_call_versioned` runtime API function.
	///
	/// This version combines the unversioned `trace_call` and `trace_call_with_config` runtime API
	/// functions. Setting `state_overrides` to `None` preserves the behavior and output of
	/// `trace_call`, while passing the `state_overrides` field from `TracingConfigV1` preserves the
	/// behavior and output of `trace_call_with_config`.
	V1(TraceCallInputPayloadV1),
	/// This version accepts the same arguments as `V1` and selects `TraceV2` rather than `TraceV1`
	/// for the returned trace data.
	V2(TraceCallInputPayloadV2),
}

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct TraceCallOutputPayloadV1 {
	pub trace: TraceV1,
}

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct TraceCallOutputPayloadV2 {
	pub trace: TraceV2,
}

/// The output type returned when calling the `trace_call_versioned` runtime API function. This
/// function replaces the unversioned `trace_call` and `trace_call_with_config` runtime API
/// functions.
#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto)]
pub enum TraceCallVersionedOutputPayload {
	/// The output returned when calling the `trace_call_versioned` runtime API function with `V1`
	/// arguments.
	///
	/// This output is identical to the output returned by the corresponding unversioned
	/// `trace_call` or `trace_call_with_config` runtime API function.
	V1(TraceCallOutputPayloadV1),
	/// This version uses `TraceV2` for trace output instead of `TraceV1`.
	///
	/// `TraceV2::Call` uses `CallTraceV2`, which replaces `CallLogV1` with `CallLogV2`.
	/// `CallLogV2` adds `index`, the block-wide log index matching the receipt `logIndex`, while
	/// `position` continues to describe log ordering relative to child calls within the same trace
	/// frame. `CallTraceV2` also removes `child_call_count`, which was only used to calculate
	/// `position`. Prestate and execution traces are unchanged.
	V2(TraceCallOutputPayloadV2),
}
