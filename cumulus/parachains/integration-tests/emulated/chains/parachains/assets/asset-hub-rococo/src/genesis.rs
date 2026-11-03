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

// Substrate
use frame_support::parameter_types;
use sp_core::storage::Storage;
use sp_keyring::Sr25519Keyring as Keyring;

// Cumulus
use emulated_integration_tests_common::{
	accounts, build_genesis_storage, collators,
	snowbridge::{ETHER_MIN_BALANCE, WETH},
	xcm_emulator::ConvertLocation,
	PenpalALocation, PenpalAPen2TeleportableAssetLocation, PenpalASiblingSovereignAccount,
	PenpalBLocation, PenpalBPen2TeleportableAssetLocation, PenpalBSiblingSovereignAccount,
	RESERVABLE_ASSET_ID, SAFE_XCM_VERSION, USDT_ID,
};
use parachains_common::{AccountId, Balance};
use testnet_parachains_constants::rococo::snowbridge::EthereumNetwork;
use xcm::{
	latest::prelude::*,
	opaque::latest::{ROCOCO_GENESIS_HASH, WESTEND_GENESIS_HASH},
};
use xcm_builder::ExternalConsensusLocationsConverterFor;

pub const PARA_ID: u32 = 1000;
pub const ED: Balance = testnet_parachains_constants::rococo::currency::EXISTENTIAL_DEPOSIT;

parameter_types! {
	pub AssetHubRococoAssetOwner: AccountId = Keyring::Alice.to_account_id();
	pub RococoGlobalConsensusNetwork: NetworkId = NetworkId::ByGenesis(ROCOCO_GENESIS_HASH);
	pub AssetHubRococoUniversalLocation: InteriorLocation = [GlobalConsensus(RococoGlobalConsensusNetwork::get()), Parachain(PARA_ID)].into();
	pub AssetHubWestendLocation: Location = Location::new(
			2,
			[GlobalConsensus( NetworkId::ByGenesis(WESTEND_GENESIS_HASH)), Parachain(PARA_ID)],
		);
	pub AssetHubWestendSovereignAccount: AccountId = ExternalConsensusLocationsConverterFor::<
			AssetHubRococoUniversalLocation,
			AccountId,
		>::convert_location(&AssetHubWestendLocation::get()).unwrap();
	pub EthereumLocation: Location = Location::new(2, [GlobalConsensus(EthereumNetwork::get())]);
}

pub fn genesis() -> Storage {
	let genesis_config = asset_hub_rococo_runtime::RuntimeGenesisConfig {
		system: asset_hub_rococo_runtime::SystemConfig::default(),
		balances: asset_hub_rococo_runtime::BalancesConfig {
			balances: accounts::init_balances()
				.iter()
				.cloned()
				.map(|k| (k, ED * 4096 * 4096))
				.collect(),
			..Default::default()
		},
		parachain_info: asset_hub_rococo_runtime::ParachainInfoConfig {
			parachain_id: PARA_ID.into(),
			..Default::default()
		},
		collator_selection: asset_hub_rococo_runtime::CollatorSelectionConfig {
			invulnerables: collators::invulnerables().iter().cloned().map(|(acc, _)| acc).collect(),
			candidacy_bond: ED * 16,
			..Default::default()
		},
		session: asset_hub_rococo_runtime::SessionConfig {
			keys: collators::invulnerables()
				.into_iter()
				.map(|(acc, aura)| {
					(
						acc.clone(),                                    // account id
						acc,                                            // validator id
						asset_hub_rococo_runtime::SessionKeys { aura }, // session keys
					)
				})
				.collect(),
			..Default::default()
		},
		polkadot_xcm: asset_hub_rococo_runtime::PolkadotXcmConfig {
			safe_xcm_version: Some(SAFE_XCM_VERSION),
			..Default::default()
		},
		assets: asset_hub_rococo_runtime::AssetsConfig {
			assets: vec![
				(RESERVABLE_ASSET_ID, AssetHubRococoAssetOwner::get(), false, ED),
				(USDT_ID, AssetHubRococoAssetOwner::get(), true, ED),
			],
			..Default::default()
		},
		foreign_assets: asset_hub_rococo_runtime::ForeignAssetsConfig {
			assets: vec![
				// PenpalA's native asset representation
				(PenpalALocation::get(), PenpalASiblingSovereignAccount::get(), true, ED),
				// PenpalB's native asset representation
				(PenpalBLocation::get(), PenpalBSiblingSovereignAccount::get(), true, ED),
				(
					PenpalAPen2TeleportableAssetLocation::get(),
					PenpalASiblingSovereignAccount::get(),
					false,
					ED,
				),
				// PenpalB's teleportable asset representation
				(
					PenpalBPen2TeleportableAssetLocation::get(),
					PenpalBSiblingSovereignAccount::get(),
					false,
					ED,
				),
				// Ether
				(
					EthereumLocation::get(),
					// Emulate double bridging; WAH is the owner of assets from Ethereum on RAH.
					AssetHubWestendSovereignAccount::get(),
					true,
					ETHER_MIN_BALANCE,
				),
				// Weth
				(
					Location::new(
						2,
						[
							GlobalConsensus(EthereumNetwork::get()),
							AccountKey20 { network: None, key: WETH.into() },
						],
					),
					AssetHubWestendSovereignAccount::get(),
					true,
					ETHER_MIN_BALANCE,
				),
			],
			reserves: vec![
				(PenpalALocation::get(), vec![(PenpalALocation::get(), true).into()]),
				(PenpalBLocation::get(), vec![(PenpalBLocation::get(), true).into()]),
				(
					PenpalAPen2TeleportableAssetLocation::get(),
					vec![(PenpalALocation::get(), true).into()],
				),
				(
					PenpalBPen2TeleportableAssetLocation::get(),
					vec![(PenpalBLocation::get(), true).into()],
				),
				(EthereumLocation::get(), vec![(AssetHubWestendLocation::get(), false).into()]),
				(
					Location::new(
						2,
						[
							GlobalConsensus(EthereumNetwork::get()),
							AccountKey20 { network: None, key: WETH.into() },
						],
					),
					vec![(AssetHubWestendLocation::get(), false).into()],
				),
			],
			..Default::default()
		},
		..Default::default()
	};

	build_genesis_storage(
		&genesis_config,
		asset_hub_rococo_runtime::WASM_BINARY.expect("WASM binary was not built, please build it!"),
	)
}
