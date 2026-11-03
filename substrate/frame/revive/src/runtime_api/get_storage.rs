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

pub struct GetStorageInputPayload {
	pub address: H160,
	pub key: StorageKey,
}

impl From<GetStorageVersionedInputPayload> for GetStorageInputPayload {
	fn from(value: GetStorageVersionedInputPayload) -> Self {
		match value {
			GetStorageVersionedInputPayload::V1(payload) => payload.into(),
		}
	}
}

impl From<GetStorageInputPayloadV1> for GetStorageInputPayload {
	fn from(value: GetStorageInputPayloadV1) -> Self {
		Self { address: value.address, key: value.key.into() }
	}
}

pub enum StorageKey {
	Fixed([u8; 32]),
	Variable(Vec<u8>),
}

impl From<StorageKeyV1> for StorageKey {
	fn from(value: StorageKeyV1) -> Self {
		match value {
			StorageKeyV1::Fixed(key) => Self::Fixed(key),
			StorageKeyV1::Variable(key) => Self::Variable(key),
		}
	}
}

pub struct GetStorageOutputPayload {
	pub storage: Option<Vec<u8>>,
}

impl From<GetStorageOutputPayload> for GetStorageOutputPayloadV1 {
	fn from(value: GetStorageOutputPayload) -> Self {
		Self { storage: value.storage }
	}
}
