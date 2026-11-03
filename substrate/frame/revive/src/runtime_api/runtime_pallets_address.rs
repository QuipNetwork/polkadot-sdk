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

use crate::H160;

pub struct RuntimePalletsAddressInputPayload;

impl From<RuntimePalletsAddressVersionedInputPayload> for RuntimePalletsAddressInputPayload {
	fn from(value: RuntimePalletsAddressVersionedInputPayload) -> Self {
		match value {
			RuntimePalletsAddressVersionedInputPayload::V1(payload) => payload.into(),
		}
	}
}

impl From<RuntimePalletsAddressInputPayloadV1> for RuntimePalletsAddressInputPayload {
	fn from(_value: RuntimePalletsAddressInputPayloadV1) -> Self {
		Self
	}
}

pub struct RuntimePalletsAddressOutputPayload {
	pub runtime_pallets_address: H160,
}

impl From<RuntimePalletsAddressOutputPayload> for RuntimePalletsAddressOutputPayloadV1 {
	fn from(value: RuntimePalletsAddressOutputPayload) -> Self {
		Self { runtime_pallets_address: value.runtime_pallets_address }
	}
}
