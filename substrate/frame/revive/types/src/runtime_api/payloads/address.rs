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

use codec::{Decode, Encode};
use derive_more::{From, TryInto};
use scale_info::TypeInfo;
use sp_core::H160;

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct AddressInputPayloadV1<AccountId> {
	pub account_id: AccountId,
}

/// The input type used when calling the `address_versioned` runtime API function. This function
/// replaces the unversioned `address` runtime API function.
#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto)]
pub enum AddressVersionedInputPayload<AccountId> {
	/// The arguments provided when calling the `address_versioned` runtime API function.
	///
	/// When this version is provided, the function behaves identically to and returns the same
	/// output as the unversioned `address` runtime API function.
	V1(AddressInputPayloadV1<AccountId>),
}

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct AddressOutputPayloadV1 {
	pub address: H160,
}

/// The output type returned when calling the `address_versioned` runtime API function. This
/// function replaces the unversioned `address` runtime API function.
#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto)]
pub enum AddressVersionedOutputPayload {
	/// The output returned when calling the `address_versioned` runtime API function with `V1`
	/// arguments.
	///
	/// This output is identical to the output returned by the unversioned `address` runtime API
	/// function.
	V1(AddressOutputPayloadV1),
}
