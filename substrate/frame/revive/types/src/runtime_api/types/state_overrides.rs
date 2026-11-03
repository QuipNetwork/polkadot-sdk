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

use alloc::collections::BTreeMap;
use codec::{Decode, Encode};
use scale_info::TypeInfo;
use serde::{Deserialize, Serialize};
use sp_core::{H160, H256, U256};

use crate::common::*;

#[derive(
	Debug, Default, Clone, Encode, Decode, TypeInfo, Serialize, Deserialize, Eq, PartialEq,
)]
pub struct StateOverrideSetV1(pub BTreeMap<H160, StateOverrideV1>);

#[derive(Debug, Clone, Encode, Decode, TypeInfo, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum StorageOverrideV1 {
	State(BTreeMap<H256, H256>),
	StateDiff(BTreeMap<H256, H256>),
}

#[derive(
	Debug, Default, Clone, Encode, Decode, TypeInfo, Serialize, Deserialize, Eq, PartialEq,
)]
#[serde(rename_all = "camelCase")]
pub struct StateOverrideV1 {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub balance: Option<U256>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub nonce: Option<U256>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub code: Option<Bytes>,
	#[serde(flatten)]
	pub storage: Option<StorageOverrideV1>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub move_precompile_to_address: Option<H160>,
}

/// Exists for legacy reasons only: this is the configuration type of the unversioned
/// `trace_call_with_config` runtime API function and is not used anywhere in the versioned runtime
/// API.
#[derive(Debug, Default, Encode, TypeInfo, Clone)]
pub struct TracingConfigV1 {
	pub state_overrides: Option<StateOverrideSetV1>,
}

impl Decode for TracingConfigV1 {
	fn decode<I: codec::Input>(input: &mut I) -> Result<Self, codec::Error> {
		let state_overrides = Option::<StateOverrideSetV1>::decode(input).unwrap_or_default();
		Ok(Self { state_overrides })
	}
}
