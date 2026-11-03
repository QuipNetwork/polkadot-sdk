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

//! Test to check the migration of the voter bag.

use crate::{RuntimeT, LOG_TARGET};
use frame_support::traits::PalletInfoAccess;
use pallet_staking_async::Nominators;
use remote_externalities::{Builder, Mode, OnlineConfig};
use sp_runtime::{traits::Block as BlockT, DeserializeOwned};

/// Test voter bags migration. `currency_unit` is the number of planks per the the runtimes `UNITS`
/// (i.e. number of decimal places per DOT, KSM etc)
pub async fn execute<Runtime, Block>(
	currency_unit: u64,
	currency_name: &'static str,
	ws_url: String,
) where
	Runtime: RuntimeT<pallet_bags_list::Instance1>,
	Block: BlockT + DeserializeOwned,
	Block::Header: DeserializeOwned,
{
	let mut ext = Builder::<Block>::new()
		.mode(Mode::Online(OnlineConfig {
			transport_uris: vec![ws_url.to_string()],
			pallets: vec![pallet_staking_async::Pallet::<Runtime>::name().to_string()],
			..Default::default()
		}))
		.build()
		.await
		.unwrap();

	ext.execute_with(|| {
		// get the nominator & validator count prior to migrating; these should be invariant.
		let pre_migrate_nominator_count = <Nominators<Runtime>>::iter().count() as u32;
		log::info!(target: LOG_TARGET, "Nominator count: {}", pre_migrate_nominator_count);

		use frame_election_provider_support::SortedListProvider;
		// run the actual migration
		let moved = <Runtime as pallet_staking_async::Config>::VoterList::unsafe_regenerate(
			pallet_staking_async::Nominators::<Runtime>::iter().map(|(n, _)| n),
			Box::new(|x| Some(pallet_staking_async::Pallet::<Runtime>::weight_of(x))),
		);
		log::info!(target: LOG_TARGET, "Moved {} nominators", moved);

		let voter_list_len =
			<Runtime as pallet_staking_async::Config>::VoterList::iter().count() as u32;
		let voter_list_count = <Runtime as pallet_staking_async::Config>::VoterList::count();
		// and confirm it is equal to the length of the `VoterList`.
		assert_eq!(pre_migrate_nominator_count, voter_list_len);
		assert_eq!(pre_migrate_nominator_count, voter_list_count);

		crate::display_and_check_bags::<Runtime>(currency_unit, currency_name);
	});
}
