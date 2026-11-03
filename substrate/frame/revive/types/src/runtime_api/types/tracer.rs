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
use derive_more::From;
use scale_info::TypeInfo;
use serde::{Deserialize, Serialize};

use crate::common::*;

#[derive(TypeInfo, Debug, Clone, Encode, Decode, Serialize, Deserialize, PartialEq, From)]
#[serde(tag = "tracer", content = "tracerConfig", rename_all = "camelCase")]
pub enum TracerTypeV1 {
	CallTracer(Option<CallTracerConfigV1>),
	PrestateTracer(Option<PrestateTracerConfigV1>),
	ExecutionTracer(Option<ExecutionTracerConfigV1>),
}

impl Default for TracerTypeV1 {
	fn default() -> Self {
		TracerTypeV1::ExecutionTracer(Some(ExecutionTracerConfigV1::default()))
	}
}

#[derive(Clone, Debug, Decode, Serialize, Deserialize, Encode, PartialEq, TypeInfo)]
#[serde(default, rename_all = "camelCase")]
pub struct CallTracerConfigV1 {
	pub with_logs: bool,
	pub only_top_call: bool,
}

impl Default for CallTracerConfigV1 {
	fn default() -> Self {
		Self { with_logs: true, only_top_call: false }
	}
}

#[derive(Clone, Debug, Decode, Serialize, Deserialize, Encode, PartialEq, TypeInfo, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct PrestateTracerConfigV1 {
	pub diff_mode: bool,
	pub disable_storage: bool,
	pub disable_code: bool,
}

#[derive(Clone, Debug, Decode, Serialize, Deserialize, Encode, PartialEq, TypeInfo)]
#[serde(default, rename_all = "camelCase")]
pub struct ExecutionTracerConfigV1 {
	pub enable_memory: bool,
	pub disable_stack: bool,
	pub disable_storage: bool,
	pub enable_return_data: bool,
	pub disable_syscall_details: bool,
	#[serde(skip_serializing_if = "Option::is_none", deserialize_with = "zero_to_none")]
	pub limit: Option<u64>,
	pub memory_word_limit: u32,
}

impl Default for ExecutionTracerConfigV1 {
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
