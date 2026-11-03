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

pub struct NewBalanceWithDustInputPayload {
	pub balance: U256,
}

impl From<NewBalanceWithDustVersionedInputPayload> for NewBalanceWithDustInputPayload {
	fn from(value: NewBalanceWithDustVersionedInputPayload) -> Self {
		match value {
			NewBalanceWithDustVersionedInputPayload::V1(payload) => payload.into(),
		}
	}
}

impl From<NewBalanceWithDustInputPayloadV1> for NewBalanceWithDustInputPayload {
	fn from(value: NewBalanceWithDustInputPayloadV1) -> Self {
		Self { balance: value.balance }
	}
}

pub struct NewBalanceWithDustOutputPayload<Balance> {
	pub new_balance: Balance,
	pub dust: u32,
}

impl<Balance> From<NewBalanceWithDustOutputPayload<Balance>>
	for NewBalanceWithDustOutputPayloadV1<Balance>
{
	fn from(value: NewBalanceWithDustOutputPayload<Balance>) -> Self {
		Self { new_balance: value.new_balance, dust: value.dust }
	}
}
