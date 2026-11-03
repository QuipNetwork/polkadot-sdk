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

use pallet_revive_types::runtime_api::*;

use crate::U256;

pub struct MaxExtrinsicWeightInGasInputPayload;

impl From<MaxExtrinsicWeightInGasVersionedInputPayload> for MaxExtrinsicWeightInGasInputPayload {
	fn from(value: MaxExtrinsicWeightInGasVersionedInputPayload) -> Self {
		match value {
			MaxExtrinsicWeightInGasVersionedInputPayload::V1(payload) => payload.into(),
		}
	}
}

impl From<MaxExtrinsicWeightInGasInputPayloadV1> for MaxExtrinsicWeightInGasInputPayload {
	fn from(_value: MaxExtrinsicWeightInGasInputPayloadV1) -> Self {
		Self
	}
}

pub struct MaxExtrinsicWeightInGasOutputPayload {
	pub max_extrinsic_weight_in_gas: U256,
}

impl From<MaxExtrinsicWeightInGasOutputPayload> for MaxExtrinsicWeightInGasOutputPayloadV1 {
	fn from(value: MaxExtrinsicWeightInGasOutputPayload) -> Self {
		Self { max_extrinsic_weight_in_gas: value.max_extrinsic_weight_in_gas }
	}
}
