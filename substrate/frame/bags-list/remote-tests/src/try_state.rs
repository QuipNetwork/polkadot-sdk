// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Substrate.
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

//! Test to execute the sanity-check of the voter bag.

use frame_support::{
	storage::generator::StorageMap,
	traits::{Get, PalletInfoAccess},
};
use remote_externalities::{Builder, Mode, OnlineConfig};
use sp_runtime::{traits::Block as BlockT, DeserializeOwned};

/// Execute the sanity check of the bags-list.
pub async fn execute<Runtime, Block>(
	currency_unit: u64,
	currency_name: &'static str,
	ws_url: String,
) where
	Runtime: crate::RuntimeT<pallet_bags_list::Instance1>,
	Block: BlockT + DeserializeOwned,
	Block::Header: DeserializeOwned,
{
	let mut ext = Builder::<Block>::new()
		.mode(Mode::Online(OnlineConfig {
			transport_uris: vec![ws_url.to_string()],
			pallets: vec![pallet_bags_list::Pallet::<Runtime, pallet_bags_list::Instance1>::name()
				.to_string()],
			hashed_prefixes: vec![
				<pallet_staking_async::Bonded<Runtime>>::prefix_hash().to_vec(),
				<pallet_staking_async::Ledger<Runtime>>::prefix_hash().to_vec(),
			],
			..Default::default()
		}))
		.build()
		.await
		.unwrap();

	ext.execute_with(|| {
		sp_core::crypto::set_default_ss58_version(Runtime::SS58Prefix::get().try_into().unwrap());

		pallet_bags_list::Pallet::<Runtime, pallet_bags_list::Instance1>::do_try_state().unwrap();

		log::info!(target: crate::LOG_TARGET, "executed bags-list sanity check with no errors.");

		crate::display_and_check_bags::<Runtime>(currency_unit, currency_name);
	});
}
