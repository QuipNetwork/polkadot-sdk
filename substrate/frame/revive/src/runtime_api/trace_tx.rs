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

use crate::evm::{TraceEntry, TracerType};

pub struct TraceTxInputPayload<Block> {
	pub block: Block,
	pub tx_index: u32,
	pub config: TracerType,
}

impl<Block> From<TraceTxVersionedInputPayload<Block>> for TraceTxInputPayload<Block> {
	fn from(value: TraceTxVersionedInputPayload<Block>) -> Self {
		match value {
			TraceTxVersionedInputPayload::V1(payload) => payload.into(),
			TraceTxVersionedInputPayload::V2(payload) => payload.into(),
		}
	}
}

impl<Block> From<TraceTxInputPayloadV1<Block>> for TraceTxInputPayload<Block> {
	fn from(value: TraceTxInputPayloadV1<Block>) -> Self {
		Self { block: value.block, tx_index: value.tx_index, config: value.config.into() }
	}
}

impl<Block> From<TraceTxInputPayloadV2<Block>> for TraceTxInputPayload<Block> {
	fn from(value: TraceTxInputPayloadV2<Block>) -> Self {
		Self { block: value.block, tx_index: value.tx_index, config: value.config.into() }
	}
}

pub struct TraceTxOutputPayload {
	pub entry: Option<TraceEntry>,
}

impl From<TraceTxOutputPayload> for TraceTxOutputPayloadV1 {
	fn from(value: TraceTxOutputPayload) -> Self {
		// V1 has no way to report an untraced extrinsic, so it keeps omitting it.
		let trace = match value.entry {
			Some(TraceEntry::Traced(trace)) => Some(trace.into()),
			Some(TraceEntry::NotTraced) | None => None,
		};
		Self { trace }
	}
}

impl From<TraceTxOutputPayload> for TraceTxOutputPayloadV2 {
	fn from(value: TraceTxOutputPayload) -> Self {
		Self { entry: value.entry.map(Into::into) }
	}
}
