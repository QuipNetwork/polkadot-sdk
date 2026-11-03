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
use sp_core::U256;

use crate::evm::{GenericTransaction, StateOverrideSet};

pub struct EstimateGasInputPayload<Moment> {
	pub tx: GenericTransaction,
	pub timestamp_override: Option<Moment>,
	pub state_overrides: Option<StateOverrideSet>,
}

impl<Moment> From<EstimateGasVersionedInputPayload<Moment>> for EstimateGasInputPayload<Moment> {
	fn from(value: EstimateGasVersionedInputPayload<Moment>) -> Self {
		match value {
			EstimateGasVersionedInputPayload::V1(payload) => payload.into(),
		}
	}
}

impl<Moment> From<EstimateGasInputPayloadV1<Moment>> for EstimateGasInputPayload<Moment> {
	fn from(value: EstimateGasInputPayloadV1<Moment>) -> Self {
		Self {
			tx: value.tx.into(),
			timestamp_override: value.timestamp_override,
			state_overrides: value.state_overrides.map(Into::into),
		}
	}
}

pub struct EstimateGasOutputPayload {
	pub gas_estimate: U256,
}

impl From<EstimateGasOutputPayload> for EstimateGasOutputPayloadV1 {
	fn from(value: EstimateGasOutputPayload) -> Self {
		Self { gas_estimate: value.gas_estimate }
	}
}
