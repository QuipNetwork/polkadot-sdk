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

pub struct BlockAuthorInputPayload;

impl From<BlockAuthorVersionedInputPayload> for BlockAuthorInputPayload {
	fn from(value: BlockAuthorVersionedInputPayload) -> Self {
		match value {
			BlockAuthorVersionedInputPayload::V1(payload) => payload.into(),
		}
	}
}

impl From<BlockAuthorInputPayloadV1> for BlockAuthorInputPayload {
	fn from(_value: BlockAuthorInputPayloadV1) -> Self {
		Self
	}
}

pub struct BlockAuthorOutputPayload {
	pub block_author: H160,
}

impl From<BlockAuthorOutputPayload> for BlockAuthorOutputPayloadV1 {
	fn from(value: BlockAuthorOutputPayload) -> Self {
		Self { block_author: value.block_author }
	}
}
