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
use codec::{Decode, Encode};
use derive_more::{From, TryInto};
use scale_info::TypeInfo;
use sp_core::H160;

use crate::runtime_api::*;

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct GetStorageInputPayloadV1 {
	pub address: H160,
	pub key: StorageKeyV1,
}

/// The input type used when calling the `get_storage_versioned` runtime API function. This function
/// replaces the unversioned `get_storage` and `get_storage_var_key` runtime API functions.
#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto)]
pub enum GetStorageVersionedInputPayload {
	/// The arguments provided when calling the `get_storage_versioned` runtime API function.
	///
	/// This version combines the unversioned `get_storage` and `get_storage_var_key` runtime API
	/// functions. `StorageKeyV1::Fixed` preserves the behavior and output of `get_storage`, while
	/// `StorageKeyV1::Variable` preserves the behavior and output of `get_storage_var_key`.
	V1(GetStorageInputPayloadV1),
}

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct GetStorageOutputPayloadV1 {
	pub storage: Option<Vec<u8>>,
}

/// The output type returned when calling the `get_storage_versioned` runtime API function. This
/// function replaces the unversioned `get_storage` and `get_storage_var_key` runtime API functions.
#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto)]
pub enum GetStorageVersionedOutputPayload {
	/// The output returned when calling the `get_storage_versioned` runtime API function with `V1`
	/// arguments.
	///
	/// This output is identical to the output of the unversioned `get_storage` runtime API function
	/// for `StorageKeyV1::Fixed` and to the output of `get_storage_var_key` for
	/// `StorageKeyV1::Variable`.
	V1(GetStorageOutputPayloadV1),
}
