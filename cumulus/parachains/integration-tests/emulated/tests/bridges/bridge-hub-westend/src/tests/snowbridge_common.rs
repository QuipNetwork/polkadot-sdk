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

use crate::{imports::*, tests::bridged_roc_at_ah_westend};
use asset_hub_westend_runtime::xcm_config::LocationToAccountId;
use emulated_integration_tests_common::snowbridge::{SEPOLIA_ID, WETH};
use frame_support::traits::{fungible::Mutate as _, fungibles::Mutate};
use hex_literal::hex;
use snowbridge_core::AssetMetadata;
use sp_core::H160;
use testnet_parachains_constants::westend::snowbridge::EthereumNetwork;
use xcm_builder::ExternalConsensusLocationsConverterFor;
use xcm_executor::traits::ConvertLocation;

pub const INITIAL_FUND: u128 = 50_000_000_000_000;
pub const ETHEREUM_DESTINATION_ADDRESS: [u8; 20] = hex!("44a57ee2f2FCcb85FDa2B0B18EBD0D8D2333700e");
pub const AGENT_ADDRESS: [u8; 20] = hex!("90A987B944Cb1dCcE5564e5FDeCD7a54D3de27Fe");
pub const TOKEN_AMOUNT: u128 = 10_000_000_000_000;
pub const REMOTE_FEE_AMOUNT_IN_ETHER: u128 = 600_000_000_000;
pub const LOCAL_FEE_AMOUNT_IN_DOT: u128 = 800_000_000_000;
pub const LOCAL_FEE_AMOUNT_IN_PAL: u128 = 800_000_000_000;

pub const EXECUTION_WEIGHT: u64 = 8_000_000_000;

pub fn beneficiary() -> Location {
	Location::new(0, [AccountKey20 { network: None, key: ETHEREUM_DESTINATION_ADDRESS.into() }])
}

pub fn asset_hub() -> Location {
	Location::new(1, Parachain(AssetHubWestend::para_id().into()))
}

pub fn bridge_hub() -> Location {
	Location::new(1, Parachain(BridgeHubWestend::para_id().into()))
}

pub fn fund_on_bh() {
	let assethub_sovereign = BridgeHubWestend::sovereign_account_id_of(asset_hub());
	BridgeHubWestend::fund_accounts(vec![(assethub_sovereign.clone(), INITIAL_FUND)]);
}

pub fn register_assets_on_ah() {}
pub fn register_relay_token_on_bh() {
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		type RuntimeOrigin = <BridgeHubWestend as Chain>::RuntimeOrigin;

		// Register WND on BH
		assert_ok!(<BridgeHubWestend as BridgeHubWestendPallet>::EthereumSystem::register_token(
			RuntimeOrigin::root(),
			Box::new(VersionedLocation::from(Location::parent())),
			AssetMetadata {
				name: "wnd".as_bytes().to_vec().try_into().unwrap(),
				symbol: "wnd".as_bytes().to_vec().try_into().unwrap(),
				decimals: 12,
			},
		));
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumSystem(snowbridge_pallet_system::Event::RegisterToken { .. }) => {},]
		);
	});
}

pub fn register_assets_on_penpal() {
	let ethereum_sovereign: AccountId = snowbridge_sovereign();
	PenpalB::execute_with(|| {
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::force_create(
			<PenpalB as Chain>::RuntimeOrigin::root(),
			weth_location().try_into().unwrap(),
			ethereum_sovereign.clone().into(),
			true,
			1,
		));
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::force_create(
			<PenpalB as Chain>::RuntimeOrigin::root(),
			ethereum().try_into().unwrap(),
			ethereum_sovereign.into(),
			true,
			1,
		));
	});
}

pub fn register_foreign_asset(token_location: Location) {
	let bridge_owner = snowbridge_sovereign();
	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::force_create(
			RuntimeOrigin::root(),
			token_location.clone().try_into().unwrap(),
			bridge_owner.clone().into(),
			true,
			1000,
		));
		assert!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::asset_exists(
			token_location.clone().try_into().unwrap(),
		));
	});
	AssetHubWestend::set_foreign_asset_reserves(
		token_location,
		bridge_owner,
		vec![(ethereum(), false).into()],
	);
}

pub fn mint_pal_on_ah() {
	// Create PAL(i.e. native asset for penpal) on AH.
	AssetHubWestend::execute_with(|| {
		let penpal_asset_id = Location::new(1, Parachain(PenpalB::para_id().into()));

		assert!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::asset_exists(
			penpal_asset_id.clone(),
		));

		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::mint_into(
			penpal_asset_id.clone(),
			&AssetHubWestendReceiver::get(),
			TOKEN_AMOUNT,
		));

		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::mint_into(
			penpal_asset_id.clone(),
			&AssetHubWestendSender::get(),
			TOKEN_AMOUNT,
		));
	});
}

pub fn penpal_root_sovereign() -> sp_runtime::AccountId32 {
	let penpal_root_sovereign: AccountId = PenpalB::execute_with(|| {
		use rococo_westend_system_emulated_network::penpal_emulated_chain::penpal_runtime::xcm_config;
		xcm_config::LocationToAccountId::convert_location(&xcm_config::RootLocation::get())
			.unwrap()
			.into()
	});
	penpal_root_sovereign
}

pub fn fund_on_penpal() {
	let sudo_account = penpal_root_sovereign();
	PenpalB::fund_accounts(vec![
		(PenpalBReceiver::get(), INITIAL_FUND),
		(PenpalBSender::get(), INITIAL_FUND),
		(PenpalCheckingAccount::get(), INITIAL_FUND),
		(sudo_account.clone(), INITIAL_FUND),
	]);
	PenpalB::execute_with(|| {
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			Location::parent(),
			&PenpalBReceiver::get(),
			INITIAL_FUND,
		));
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			Location::parent(),
			&PenpalBSender::get(),
			INITIAL_FUND,
		));
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			Location::parent(),
			&sudo_account,
			INITIAL_FUND,
		));
	});
	PenpalB::execute_with(|| {
		assert_ok!(<PenpalB as PenpalBPallet>::Balances::mint_into(
			&PenpalBReceiver::get(),
			INITIAL_FUND,
		));
		assert_ok!(<PenpalB as PenpalBPallet>::Balances::mint_into(
			&PenpalBSender::get(),
			INITIAL_FUND,
		));
		assert_ok!(<PenpalB as PenpalBPallet>::Balances::mint_into(&sudo_account, INITIAL_FUND,));
	});
	PenpalB::execute_with(|| {
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			PenpalLocalPen2Asset::get(),
			&PenpalBReceiver::get(),
			INITIAL_FUND,
		));
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			PenpalLocalPen2Asset::get(),
			&PenpalBSender::get(),
			INITIAL_FUND,
		));
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			PenpalLocalPen2Asset::get(),
			&sudo_account,
			INITIAL_FUND,
		));
	});
	PenpalB::execute_with(|| {
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			weth_location().try_into().unwrap(),
			&PenpalBReceiver::get(),
			INITIAL_FUND,
		));
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			weth_location().try_into().unwrap(),
			&PenpalBSender::get(),
			INITIAL_FUND,
		));
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			weth_location().try_into().unwrap(),
			&sudo_account,
			INITIAL_FUND,
		));
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			ethereum().try_into().unwrap(),
			&PenpalBReceiver::get(),
			INITIAL_FUND,
		));
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			ethereum().try_into().unwrap(),
			&PenpalBSender::get(),
			INITIAL_FUND,
		));
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			ethereum().try_into().unwrap(),
			&sudo_account,
			INITIAL_FUND,
		));
	});
}

pub fn set_trust_reserve_on_penpal() {
	PenpalB::execute_with(|| {
		assert_ok!(<PenpalB as Chain>::System::set_storage(
			<PenpalB as Chain>::RuntimeOrigin::root(),
			vec![(
				PenpalCustomizableAssetFromSystemAssetHub::key().to_vec(),
				Location::new(2, [GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })]).encode(),
			)],
		));
	});
}

pub fn fund_on_ah() {
	AssetHubWestend::fund_accounts(vec![(AssetHubWestendSender::get(), INITIAL_FUND)]);
	AssetHubWestend::fund_accounts(vec![(AssetHubWestendReceiver::get(), INITIAL_FUND)]);

	let penpal_sovereign = AssetHubWestend::sovereign_account_id_of(
		AssetHubWestend::sibling_location_of(PenpalB::para_id()),
	);
	let penpal_user_sovereign = LocationToAccountId::convert_location(&Location::new(
		1,
		[
			Parachain(PenpalB::para_id().into()),
			AccountId32 {
				network: Some(ByGenesis(WESTEND_GENESIS_HASH)),
				id: PenpalBSender::get().into(),
			},
		],
	))
	.unwrap();

	AssetHubWestend::execute_with(|| {
		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::mint_into(
			weth_location().try_into().unwrap(),
			&penpal_sovereign,
			INITIAL_FUND,
		));
		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::mint_into(
			weth_location().try_into().unwrap(),
			&penpal_user_sovereign,
			INITIAL_FUND,
		));
		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::mint_into(
			weth_location().try_into().unwrap(),
			&AssetHubWestendReceiver::get(),
			INITIAL_FUND,
		));
		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::mint_into(
			weth_location().try_into().unwrap(),
			&AssetHubWestendSender::get(),
			INITIAL_FUND,
		));

		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::mint_into(
			ethereum().try_into().unwrap(),
			&penpal_sovereign,
			INITIAL_FUND,
		));
		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::mint_into(
			ethereum().try_into().unwrap(),
			&penpal_user_sovereign,
			INITIAL_FUND,
		));
		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::mint_into(
			ethereum().try_into().unwrap(),
			&AssetHubWestendReceiver::get(),
			INITIAL_FUND,
		));
		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::mint_into(
			ethereum().try_into().unwrap(),
			&AssetHubWestendSender::get(),
			INITIAL_FUND,
		));
	});

	AssetHubWestend::fund_accounts(vec![(snowbridge_sovereign(), INITIAL_FUND)]);
	AssetHubWestend::fund_accounts(vec![(penpal_sovereign.clone(), INITIAL_FUND)]);
	AssetHubWestend::fund_accounts(vec![(penpal_user_sovereign.clone(), INITIAL_FUND)]);
}

pub fn create_pools_on_ah() {
	// We create a pool between WND and WETH in AssetHub to support paying for fees with WETH.
	let ethereum_sovereign = snowbridge_sovereign();
	AssetHubWestend::fund_accounts(vec![(ethereum_sovereign.clone(), INITIAL_FUND)]);
	PenpalB::fund_accounts(vec![(ethereum_sovereign.clone(), INITIAL_FUND)]);
	create_foreign_pool_with_parent_native_on!(
		AssetHubWestend,
		weth_location(),
		ethereum_sovereign.clone(),
		1_000_000_000_000,
		20_000_000_000
	);
	create_foreign_pool_with_parent_native_on!(
		AssetHubWestend,
		ethereum(),
		ethereum_sovereign.clone(),
		1_000_000_000_000,
		20_000_000_000
	);
}

pub(crate) fn set_up_eth_and_dot_pool() {
	// We create a pool between WND and WETH in AssetHub to support paying for fees with WETH.
	let ethereum_sovereign = snowbridge_sovereign();
	AssetHubWestend::fund_accounts(vec![(ethereum_sovereign.clone(), INITIAL_FUND)]);
	PenpalB::fund_accounts(vec![(ethereum_sovereign.clone(), INITIAL_FUND)]);
	create_foreign_pool_with_parent_native_on!(
		AssetHubWestend,
		eth_location(),
		ethereum_sovereign.clone()
	);
}

pub(crate) fn set_up_eth_and_dot_pool_on_penpal() {
	let ethereum_sovereign = snowbridge_sovereign();
	AssetHubWestend::fund_accounts(vec![(ethereum_sovereign.clone(), INITIAL_FUND)]);
	PenpalB::fund_accounts(vec![(ethereum_sovereign.clone(), INITIAL_FUND)]);
	create_foreign_pool_with_native_on!(
		PenpalB,
		Assets,
		eth_location(),
		ethereum_sovereign.clone()
	);
}

pub(crate) fn set_up_eth_and_dot_pool_on_rococo() {
	let sa_of_wah_on_rah = AssetHubRococo::sovereign_account_of_parachain_on_other_global_consensus(
		ByGenesis(WESTEND_GENESIS_HASH),
		AssetHubWestend::para_id(),
	);
	AssetHubRococo::fund_accounts(vec![(sa_of_wah_on_rah.clone(), INITIAL_FUND)]);
	create_foreign_pool_with_parent_native_on!(
		AssetHubRococo,
		eth_location(),
		sa_of_wah_on_rah.clone()
	);
}

pub fn register_pal_on_bh() {
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		type RuntimeOrigin = <BridgeHubWestend as Chain>::RuntimeOrigin;

		assert_ok!(<BridgeHubWestend as BridgeHubWestendPallet>::EthereumSystem::register_token(
			RuntimeOrigin::root(),
			Box::new(VersionedLocation::from(PenpalBLocation::get())),
			AssetMetadata {
				name: "pal".as_bytes().to_vec().try_into().unwrap(),
				symbol: "pal".as_bytes().to_vec().try_into().unwrap(),
				decimals: 12,
			},
		));
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumSystem(snowbridge_pallet_system::Event::RegisterToken { .. }) => {},]
		);
	});
}

pub fn snowbridge_sovereign() -> sp_runtime::AccountId32 {
	use asset_hub_westend_runtime::xcm_config::UniversalLocation as AssetHubWestendUniversalLocation;
	let ethereum_sovereign: AccountId = AssetHubWestend::execute_with(|| {
		ExternalConsensusLocationsConverterFor::<
			AssetHubWestendUniversalLocation,
			[u8; 32],
		>::convert_location(&Location::new(
				2,
				[xcm::v5::Junction::GlobalConsensus(EthereumNetwork::get())],
			))
			.unwrap()
			.into()
	});

	ethereum_sovereign
}

pub fn weth_location() -> Location {
	erc20_token_location(WETH.into())
}

pub fn eth_location() -> Location {
	Location::new(2, [GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })])
}

pub fn ethereum() -> Location {
	eth_location()
}

pub fn erc20_token_location(token_id: H160) -> Location {
	Location::new(
		2,
		[
			GlobalConsensus(EthereumNetwork::get().into()),
			AccountKey20 { network: None, key: token_id.into() },
		],
	)
}

// ROC and wROC
pub(crate) fn roc_at_ah_rococo() -> Location {
	Parent.into()
}

// set up pool
pub(crate) fn set_up_pool_with_wnd_on_ah_westend(
	asset: Location,
	is_foreign: bool,
	initial_fund: u128,
	initial_liquidity: u128,
) {
	let wnd: Location = Parent.into();
	AssetHubWestend::fund_accounts(vec![(AssetHubWestendSender::get(), initial_fund)]);
	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		let owner = AssetHubWestendSender::get();
		let signed_owner = <AssetHubWestend as Chain>::RuntimeOrigin::signed(owner.clone());

		if is_foreign {
			assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::mint(
				signed_owner.clone(),
				asset.clone().into(),
				owner.clone().into(),
				initial_fund,
			));
		} else {
			let asset_id = match asset.interior.last() {
				Some(GeneralIndex(id)) => *id as u32,
				_ => unreachable!(),
			};
			assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::Assets::mint(
				signed_owner.clone(),
				asset_id.into(),
				owner.clone().into(),
				initial_fund,
			));
		}
		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::AssetConversion::create_pool(
			signed_owner.clone(),
			Box::new(wnd.clone()),
			Box::new(asset.clone()),
		));
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::AssetConversion(pallet_asset_conversion::Event::PoolCreated { .. }) => {},
			]
		);
		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::AssetConversion::add_liquidity(
			signed_owner.clone(),
			Box::new(wnd),
			Box::new(asset),
			initial_liquidity,
			initial_liquidity,
			1,
			1,
			owner.into()
		));
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::AssetConversion(pallet_asset_conversion::Event::LiquidityAdded {..}) => {},
			]
		);
	});
}

pub fn register_roc_on_bh() {
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		type RuntimeOrigin = <BridgeHubWestend as Chain>::RuntimeOrigin;

		// Register ROC on BH
		assert_ok!(<BridgeHubWestend as BridgeHubWestendPallet>::EthereumSystem::register_token(
			RuntimeOrigin::root(),
			Box::new(VersionedLocation::from(bridged_roc_at_ah_westend())),
			AssetMetadata {
				name: "roc".as_bytes().to_vec().try_into().unwrap(),
				symbol: "roc".as_bytes().to_vec().try_into().unwrap(),
				decimals: 12,
			},
		));
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumSystem(snowbridge_pallet_system::Event::RegisterToken { .. }) => {},]
		);
	});
}

pub(crate) fn asset_hub_westend_global_location() -> Location {
	Location::new(
		2,
		[
			GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
			Parachain(AssetHubWestend::para_id().into()),
		],
	)
}
pub(crate) fn bridge_hub_westend_location() -> Location {
	Location::new(
		2,
		[
			GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
			Parachain(BridgeHubWestend::para_id().into()),
		],
	)
}
