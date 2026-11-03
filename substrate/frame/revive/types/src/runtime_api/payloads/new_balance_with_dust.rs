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
use sp_core::U256;

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct NewBalanceWithDustInputPayloadV1 {
	pub balance: U256,
}

/// The input type used when calling the `new_balance_with_dust_versioned` runtime API function.
/// This function replaces the unversioned `new_balance_with_dust` runtime API function.
#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto)]
pub enum NewBalanceWithDustVersionedInputPayload {
	/// The arguments provided when calling the `new_balance_with_dust_versioned` runtime API
	/// function.
	///
	/// When this version is provided, the function behaves identically to and returns the same
	/// output as the unversioned `new_balance_with_dust` runtime API function.
	V1(NewBalanceWithDustInputPayloadV1),
}

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct NewBalanceWithDustOutputPayloadV1<Balance> {
	pub new_balance: Balance,
	pub dust: u32,
}

/// The output type returned when calling the `new_balance_with_dust_versioned` runtime API
/// function. This function replaces the unversioned `new_balance_with_dust` runtime API function.
#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto)]
pub enum NewBalanceWithDustVersionedOutputPayload<Balance> {
	/// The output returned when calling the `new_balance_with_dust_versioned` runtime API function
	/// with `V1` arguments.
	///
	/// This output is identical to the output returned by the unversioned `new_balance_with_dust`
	/// runtime API function.
	V1(NewBalanceWithDustOutputPayloadV1<Balance>),
}
