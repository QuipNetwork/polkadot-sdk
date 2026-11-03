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

use crate::{H256, U256};

pub struct BlockHashInputPayload {
	pub block_number: U256,
}

impl From<BlockHashVersionedInputPayload> for BlockHashInputPayload {
	fn from(value: BlockHashVersionedInputPayload) -> Self {
		match value {
			BlockHashVersionedInputPayload::V1(payload) => payload.into(),
		}
	}
}

impl From<BlockHashInputPayloadV1> for BlockHashInputPayload {
	fn from(value: BlockHashInputPayloadV1) -> Self {
		Self { block_number: value.block_number }
	}
}

pub struct BlockHashOutputPayload {
	pub block_hash: Option<H256>,
}

impl From<BlockHashOutputPayload> for BlockHashOutputPayloadV1 {
	fn from(value: BlockHashOutputPayload) -> Self {
		Self { block_hash: value.block_hash }
	}
}
