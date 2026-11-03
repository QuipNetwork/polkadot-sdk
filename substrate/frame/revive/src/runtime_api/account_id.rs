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

pub struct AccountIdInputPayload {
	pub address: H160,
}

impl From<AccountIdVersionedInputPayload> for AccountIdInputPayload {
	fn from(value: AccountIdVersionedInputPayload) -> Self {
		match value {
			AccountIdVersionedInputPayload::V1(payload) => payload.into(),
		}
	}
}

impl From<AccountIdInputPayloadV1> for AccountIdInputPayload {
	fn from(value: AccountIdInputPayloadV1) -> Self {
		Self { address: value.address }
	}
}

pub struct AccountIdOutputPayload<AccountId> {
	pub account_id: AccountId,
}

impl<AccountId> From<AccountIdOutputPayload<AccountId>> for AccountIdOutputPayloadV1<AccountId> {
	fn from(value: AccountIdOutputPayload<AccountId>) -> Self {
		Self { account_id: value.account_id }
	}
}
