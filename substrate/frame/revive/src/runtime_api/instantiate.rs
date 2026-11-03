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
use pallet_revive_types::runtime_api::*;

use crate::{Code, ContractResult, InstantiateReturnValue, Weight};

pub struct InstantiateInputPayload<AccountId, Balance> {
	pub origin: AccountId,
	pub value: Balance,
	pub gas_limit: Option<Weight>,
	pub storage_deposit_limit: Option<Balance>,
	pub code: Code,
	pub data: Vec<u8>,
	pub salt: Option<[u8; 32]>,
}

impl<AccountId, Balance> From<InstantiateVersionedInputPayload<AccountId, Balance>>
	for InstantiateInputPayload<AccountId, Balance>
{
	fn from(value: InstantiateVersionedInputPayload<AccountId, Balance>) -> Self {
		match value {
			InstantiateVersionedInputPayload::V1(payload) => payload.into(),
		}
	}
}

impl<AccountId, Balance> From<InstantiateInputPayloadV1<AccountId, Balance>>
	for InstantiateInputPayload<AccountId, Balance>
{
	fn from(value: InstantiateInputPayloadV1<AccountId, Balance>) -> Self {
		Self {
			origin: value.origin,
			value: value.value,
			gas_limit: value.gas_limit,
			storage_deposit_limit: value.storage_deposit_limit,
			code: value.code.into(),
			data: value.data,
			salt: value.salt,
		}
	}
}

pub struct InstantiateOutputPayload<Balance> {
	pub contract_result: ContractResult<InstantiateReturnValue, Balance>,
}

impl<Balance> From<InstantiateOutputPayload<Balance>> for InstantiateOutputPayloadV1<Balance> {
	fn from(value: InstantiateOutputPayload<Balance>) -> Self {
		Self { contract_result: value.contract_result.into() }
	}
}
