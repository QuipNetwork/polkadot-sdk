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
use sp_core::H160;

use crate::{ContractResult, ExecReturnValue, Weight};

pub struct CallInputPayload<AccountId, Balance> {
	pub origin: AccountId,
	pub dest: H160,
	pub value: Balance,
	pub gas_limit: Option<Weight>,
	pub storage_deposit_limit: Option<Balance>,
	pub input_data: Vec<u8>,
}

impl<AccountId, Balance> From<CallVersionedInputPayload<AccountId, Balance>>
	for CallInputPayload<AccountId, Balance>
{
	fn from(value: CallVersionedInputPayload<AccountId, Balance>) -> Self {
		match value {
			CallVersionedInputPayload::V1(payload) => payload.into(),
		}
	}
}

impl<AccountId, Balance> From<CallInputPayloadV1<AccountId, Balance>>
	for CallInputPayload<AccountId, Balance>
{
	fn from(value: CallInputPayloadV1<AccountId, Balance>) -> Self {
		Self {
			origin: value.origin,
			dest: value.dest,
			value: value.value,
			gas_limit: value.gas_limit,
			storage_deposit_limit: value.storage_deposit_limit,
			input_data: value.input_data,
		}
	}
}

pub struct CallOutputPayload<Balance> {
	pub contract_result: ContractResult<ExecReturnValue, Balance>,
}

impl<Balance> From<CallOutputPayload<Balance>> for CallOutputPayloadV1<Balance> {
	fn from(value: CallOutputPayload<Balance>) -> Self {
		Self { contract_result: value.contract_result.into() }
	}
}
