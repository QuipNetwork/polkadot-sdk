// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Shared helpers for the crate's test modules.

use crate::{
	alloy::hex,
	mock::{RuntimeEvent, System, Test},
	pallet,
	IERC20::IERC20Events,
};
use pallet_revive::precompiles::alloy::{self, primitives::IntoLogData};
use sp_core::{H160, H256};

alloy::sol! {
	/// Solidity interface for the `Caller` fixture contract. Shared between
	/// `tests.rs` and `permit_tests.rs` so the two suites drive STATICCALL /
	/// DELEGATECALL through one canonical declaration.
	interface ICaller {
		function staticCall(address callee, bytes data, uint64 gas) external view returns (bool success, bytes output);
		function delegate(address callee, bytes data, uint64 gas) external returns (bool success, bytes output);
	}
}

pub(crate) const PRECOMPILE_ADDRESS_PREFIX: u16 = 0x0120;
pub(crate) const PRECOMPILE_ADDRESS_PREFIX_FOREIGN: u16 = 0x0220;

pub(crate) fn set_prefix_in_address(prefix: u16) -> [u8; 20] {
	let mut addr = hex::const_decode_to_array(b"0000000000000000000000000000000000000000").unwrap();
	addr[16..18].copy_from_slice(&prefix.to_be_bytes());
	addr
}

pub(crate) fn assert_contract_event(contract: H160, event: IERC20Events) {
	let (topics, data) = event.into_log_data().split();
	let topics = topics.into_iter().map(|v| H256(v.0)).collect::<Vec<_>>();
	System::assert_has_event(RuntimeEvent::Revive(pallet_revive::Event::ContractEmitted {
		contract,
		data: data.to_vec(),
		topics,
	}));
}

pub(crate) fn setup_asset_for_prefix(asset_id: u32, prefix: u16) {
	if prefix == PRECOMPILE_ADDRESS_PREFIX_FOREIGN {
		pallet::Pallet::<Test>::insert_asset_mapping(&asset_id)
			.expect("Failed to insert asset mapping");
	}
}
