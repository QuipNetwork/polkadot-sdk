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

use alloc::vec::Vec;
use codec::{Decode, Encode};
use derive_more::{From, TryInto};
use scale_info::TypeInfo;

use crate::runtime_api::*;

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct TraceBlockInputPayloadV1<Block> {
	pub block: Block,
	pub config: TracerTypeV1,
}

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct TraceBlockInputPayloadV2<Block> {
	pub block: Block,
	pub config: TracerTypeV1,
}

/// The input type used when calling the `trace_block_versioned` runtime API function. This function
/// replaces the unversioned `trace_block` runtime API function.
#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto)]
pub enum TraceBlockVersionedInputPayload<Block> {
	/// The arguments provided when calling the `trace_block_versioned` runtime API function.
	///
	/// When this version is provided, the function behaves identically to and returns the same
	/// output as the unversioned `trace_block` runtime API function.
	V1(TraceBlockInputPayloadV1<Block>),
	/// This version accepts the same arguments as `V1` and selects `TraceV2` rather than `TraceV1`
	/// for the returned trace data.
	V2(TraceBlockInputPayloadV2<Block>),
}

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct TraceBlockOutputPayloadV1 {
	pub traces: Vec<(u32, TraceV1)>,
}

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct TraceBlockOutputPayloadV2 {
	pub entries: Vec<(u32, TraceEntryV1)>,
}

/// The output type returned when calling the `trace_block_versioned` runtime API function. This
/// function replaces the unversioned `trace_block` runtime API function.
#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto)]
pub enum TraceBlockVersionedOutputPayload {
	/// The output returned when calling the `trace_block_versioned` runtime API function with `V1`
	/// arguments.
	///
	/// This output is identical to the output returned by the unversioned `trace_block` runtime API
	/// function.
	V1(TraceBlockOutputPayloadV1),
	/// This version uses `TraceV2` for trace output instead of `TraceV1`.
	///
	/// `TraceV2::Call` uses `CallTraceV2`, which replaces `CallLogV1` with `CallLogV2`.
	/// `CallLogV2` adds `index`, the block-wide log index matching the receipt `logIndex`, while
	/// `position` continues to describe log ordering relative to child calls within the same trace
	/// frame. `CallTraceV2` also removes `child_call_count`, which was only used to calculate
	/// `position`. Prestate and execution traces are unchanged.
	V2(TraceBlockOutputPayloadV2),
}
