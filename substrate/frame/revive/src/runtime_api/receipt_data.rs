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

use crate::ReceiptGasInfo;

pub struct ReceiptDataInputPayload;

impl From<ReceiptDataVersionedInputPayload> for ReceiptDataInputPayload {
	fn from(value: ReceiptDataVersionedInputPayload) -> Self {
		match value {
			ReceiptDataVersionedInputPayload::V1(payload) => payload.into(),
		}
	}
}

impl From<ReceiptDataInputPayloadV1> for ReceiptDataInputPayload {
	fn from(_value: ReceiptDataInputPayloadV1) -> Self {
		Self
	}
}

pub struct ReceiptDataOutputPayload {
	pub receipt_data: Vec<ReceiptGasInfo>,
}

impl From<ReceiptDataOutputPayload> for ReceiptDataOutputPayloadV1 {
	fn from(value: ReceiptDataOutputPayload) -> Self {
		Self { receipt_data: value.receipt_data.into_iter().map(Into::into).collect() }
	}
}
