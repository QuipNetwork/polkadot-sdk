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

use crate::evm::{TraceEntry, TracerType};

pub struct TraceBlockInputPayload<Block> {
	pub block: Block,
	pub config: TracerType,
}

impl<Block> From<TraceBlockVersionedInputPayload<Block>> for TraceBlockInputPayload<Block> {
	fn from(value: TraceBlockVersionedInputPayload<Block>) -> Self {
		match value {
			TraceBlockVersionedInputPayload::V1(payload) => payload.into(),
			TraceBlockVersionedInputPayload::V2(payload) => payload.into(),
		}
	}
}

impl<Block> From<TraceBlockInputPayloadV1<Block>> for TraceBlockInputPayload<Block> {
	fn from(value: TraceBlockInputPayloadV1<Block>) -> Self {
		Self { block: value.block, config: value.config.into() }
	}
}

impl<Block> From<TraceBlockInputPayloadV2<Block>> for TraceBlockInputPayload<Block> {
	fn from(value: TraceBlockInputPayloadV2<Block>) -> Self {
		Self { block: value.block, config: value.config.into() }
	}
}

#[derive(Default)]
pub struct TraceBlockOutputPayload {
	pub entries: Vec<(u32, TraceEntry)>,
}

impl From<TraceBlockOutputPayload> for TraceBlockOutputPayloadV1 {
	fn from(value: TraceBlockOutputPayload) -> Self {
		Self {
			traces: value
				.entries
				.into_iter()
				.filter_map(|(index, entry)| match entry {
					TraceEntry::Traced(trace) => Some((index, trace.into())),
					TraceEntry::NotTraced => None,
				})
				.collect(),
		}
	}
}

impl From<TraceBlockOutputPayload> for TraceBlockOutputPayloadV2 {
	fn from(value: TraceBlockOutputPayload) -> Self {
		Self {
			entries: value
				.entries
				.into_iter()
				.map(|(index, entry)| (index, entry.into()))
				.collect(),
		}
	}
}
