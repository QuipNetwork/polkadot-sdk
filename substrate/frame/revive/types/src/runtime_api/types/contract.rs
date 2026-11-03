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
use pallet_revive_uapi::ReturnFlags;
use scale_info::TypeInfo;
use sp_core::{H160, H256};
use sp_runtime::DispatchError;
use sp_weights::Weight;

#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug, TypeInfo)]
pub struct ContractResultV1<R, Balance> {
	pub weight_consumed: Weight,
	pub weight_required: Weight,
	pub storage_deposit: StorageDepositV1<Balance>,
	pub max_storage_deposit: StorageDepositV1<Balance>,
	pub gas_consumed: Balance,
	pub result: Result<R, DispatchError>,
}

#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug, TypeInfo)]
pub enum StorageDepositV1<Balance> {
	Refund(Balance),
	Charge(Balance),
}

#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug, TypeInfo)]
pub struct ExecReturnValueV1 {
	pub flags: ReturnFlags,
	pub data: Vec<u8>,
}

#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug, TypeInfo)]
pub struct InstantiateReturnValueV1 {
	pub result: ExecReturnValueV1,
	pub addr: H160,
}

#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug, TypeInfo)]
pub enum CodeV1 {
	Upload(Vec<u8>),
	Existing(H256),
}
