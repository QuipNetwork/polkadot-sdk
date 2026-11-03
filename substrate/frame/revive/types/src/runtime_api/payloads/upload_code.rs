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

use crate::runtime_api::*;

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct UploadCodeInputPayloadV1<AccountId, Balance> {
	pub origin: AccountId,
	pub code: Vec<u8>,
	pub storage_deposit_limit: Option<Balance>,
}

/// The input type used when calling the `upload_code_versioned` runtime API function. This function
/// replaces the unversioned `upload_code` runtime API function.
#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto)]
pub enum UploadCodeVersionedInputPayload<AccountId, Balance> {
	/// The arguments provided when calling the `upload_code_versioned` runtime API function.
	///
	/// When this version is provided, the function behaves identically to and returns the same
	/// output as the unversioned `upload_code` runtime API function.
	V1(UploadCodeInputPayloadV1<AccountId, Balance>),
}

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct UploadCodeOutputPayloadV1<Balance> {
	pub code_upload_return_value: CodeUploadReturnValueV1<Balance>,
}

/// The output type returned when calling the `upload_code_versioned` runtime API function. This
/// function replaces the unversioned `upload_code` runtime API function.
#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto)]
pub enum UploadCodeVersionedOutputPayload<Balance> {
	/// The output returned when calling the `upload_code_versioned` runtime API function with `V1`
	/// arguments.
	///
	/// This output is identical to the output returned by the unversioned `upload_code` runtime API
	/// function.
	V1(UploadCodeOutputPayloadV1<Balance>),
}
