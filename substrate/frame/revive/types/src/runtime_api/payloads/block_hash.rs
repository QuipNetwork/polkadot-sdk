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
use sp_core::{H256, U256};

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct BlockHashInputPayloadV1 {
	pub block_number: U256,
}

/// The input type used when calling the `eth_block_hash_versioned` runtime API function. This
/// function replaces the unversioned `eth_block_hash` runtime API function.
#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto)]
pub enum BlockHashVersionedInputPayload {
	/// The arguments provided when calling the `eth_block_hash_versioned` runtime API function.
	///
	/// When this version is provided, the function behaves identically to and returns the same
	/// output as the unversioned `eth_block_hash` runtime API function.
	V1(BlockHashInputPayloadV1),
}

#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq)]
pub struct BlockHashOutputPayloadV1 {
	pub block_hash: Option<H256>,
}

/// The output type returned when calling the `eth_block_hash_versioned` runtime API function. This
/// function replaces the unversioned `eth_block_hash` runtime API function.
#[derive(TypeInfo, Debug, Clone, Encode, Decode, PartialEq, From, TryInto)]
pub enum BlockHashVersionedOutputPayload {
	/// The output returned when calling the `eth_block_hash_versioned` runtime API function with
	/// `V1` arguments.
	///
	/// This output is identical to the output returned by the unversioned `eth_block_hash` runtime
	/// API function.
	V1(BlockHashOutputPayloadV1),
}
