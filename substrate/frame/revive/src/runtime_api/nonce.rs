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

pub struct NonceInputPayload {
	pub address: H160,
}

impl From<NonceVersionedInputPayload> for NonceInputPayload {
	fn from(value: NonceVersionedInputPayload) -> Self {
		match value {
			NonceVersionedInputPayload::V1(payload) => payload.into(),
		}
	}
}

impl From<NonceInputPayloadV1> for NonceInputPayload {
	fn from(value: NonceInputPayloadV1) -> Self {
		Self { address: value.address }
	}
}

pub struct NonceOutputPayload<Nonce> {
	pub nonce: Nonce,
}

impl<Nonce> From<NonceOutputPayload<Nonce>> for NonceOutputPayloadV1<Nonce> {
	fn from(value: NonceOutputPayload<Nonce>) -> Self {
		Self { nonce: value.nonce }
	}
}
