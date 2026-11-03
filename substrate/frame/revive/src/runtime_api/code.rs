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

use crate::H160;

pub struct CodeInputPayload {
	pub address: H160,
}

impl From<CodeVersionedInputPayload> for CodeInputPayload {
	fn from(value: CodeVersionedInputPayload) -> Self {
		match value {
			CodeVersionedInputPayload::V1(payload) => payload.into(),
		}
	}
}

impl From<CodeInputPayloadV1> for CodeInputPayload {
	fn from(value: CodeInputPayloadV1) -> Self {
		Self { address: value.address }
	}
}

pub struct CodeOutputPayload {
	pub code: Vec<u8>,
}

impl From<CodeOutputPayload> for CodeOutputPayloadV1 {
	fn from(value: CodeOutputPayload) -> Self {
		Self { code: value.code }
	}
}
