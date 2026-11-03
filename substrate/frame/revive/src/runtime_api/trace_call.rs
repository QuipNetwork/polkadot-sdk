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

use crate::evm::{GenericTransaction, StateOverrideSet, Trace, TracerType};

pub struct TraceCallInputPayload {
	pub tx: GenericTransaction,
	pub config: TracerType,
	pub state_overrides: Option<StateOverrideSet>,
}

impl From<TraceCallVersionedInputPayload> for TraceCallInputPayload {
	fn from(value: TraceCallVersionedInputPayload) -> Self {
		match value {
			TraceCallVersionedInputPayload::V1(payload) => payload.into(),
			TraceCallVersionedInputPayload::V2(payload) => payload.into(),
		}
	}
}

impl From<TraceCallInputPayloadV1> for TraceCallInputPayload {
	fn from(value: TraceCallInputPayloadV1) -> Self {
		Self {
			tx: value.tx.into(),
			config: value.config.into(),
			state_overrides: value.state_overrides.map(Into::into),
		}
	}
}

impl From<TraceCallInputPayloadV2> for TraceCallInputPayload {
	fn from(value: TraceCallInputPayloadV2) -> Self {
		Self {
			tx: value.tx.into(),
			config: value.config.into(),
			state_overrides: value.state_overrides.map(Into::into),
		}
	}
}

pub struct TraceCallOutputPayload {
	pub trace: Trace,
}

impl From<TraceCallOutputPayload> for TraceCallOutputPayloadV1 {
	fn from(value: TraceCallOutputPayload) -> Self {
		Self { trace: value.trace.into() }
	}
}

impl From<TraceCallOutputPayload> for TraceCallOutputPayloadV2 {
	fn from(value: TraceCallOutputPayload) -> Self {
		Self { trace: value.trace.into() }
	}
}
