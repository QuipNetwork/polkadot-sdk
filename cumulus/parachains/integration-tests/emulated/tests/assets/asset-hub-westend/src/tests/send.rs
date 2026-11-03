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

use crate::imports::*;

/// Relay Chain should be able to execute `Transact` instructions in System Parachain
/// when `OriginKind::Superuser`.
#[test]
fn send_transact_as_superuser_from_relay_to_asset_hub_works() {
	AssetHubWestend::force_create_asset_from_relay_as_root(
		ASSET_ID,
		ASSET_MIN_BALANCE,
		true,
		AssetHubWestendSender::get().into(),
		Some(Weight::from_parts(78_628_000, 3675)),
	)
}

pub fn penpal_register_foreign_asset_on_asset_hub(asset_location_on_penpal: Location) {
	let penpal_sovereign_account = AssetHubWestend::sovereign_account_id_of(
		AssetHubWestend::sibling_location_of(PenpalA::para_id()),
	);
	let foreign_asset_at_asset_hub =
		Location::new(1, [Junction::Parachain(PenpalA::para_id().into())])
			.appended_with(asset_location_on_penpal)
			.unwrap();

	// Encoded `create_asset` call to be executed in AssetHub
	let call = AssetHubWestend::create_foreign_asset_call(
		foreign_asset_at_asset_hub.clone(),
		ASSET_MIN_BALANCE,
		penpal_sovereign_account.clone(),
	);

	let origin_kind = OriginKind::Xcm;
	let fee_amount = ASSET_HUB_WESTEND_ED * 1000000;
	let system_asset = (Parent, fee_amount).into();

	let root_origin = <PenpalA as Chain>::RuntimeOrigin::root();
	let system_para_destination = PenpalA::sibling_location_of(AssetHubWestend::para_id()).into();
	let xcm = xcm_transact_paid_execution(
		call,
		origin_kind,
		system_asset,
		penpal_sovereign_account.clone(),
	);

	// SA-of-Penpal-on-AHR needs to have balance to pay for fees and asset creation deposit
	AssetHubWestend::fund_accounts(vec![(
		penpal_sovereign_account.clone().into(),
		ASSET_HUB_WESTEND_ED * 10000000000,
	)]);

	PenpalA::execute_with(|| {
		assert_ok!(<PenpalA as PenpalAPallet>::PolkadotXcm::send(
			root_origin,
			bx!(system_para_destination),
			bx!(xcm),
		));

		PenpalA::assert_xcm_pallet_sent();
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		AssetHubWestend::assert_xcmp_queue_success(None);
		assert_expected_events!(
			AssetHubWestend,
			vec![
				// Withdraw the fee
				RuntimeEvent::Balances(pallet_balances::Event::Withdraw { who, amount }) => {
					who: *who == penpal_sovereign_account,
					amount: *amount == fee_amount,
				},
				// Foreign Asset created
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Created { asset_id, creator, owner }) => {
					asset_id: *asset_id == foreign_asset_at_asset_hub,
					creator: *creator == penpal_sovereign_account.clone(),
					owner: *owner == penpal_sovereign_account,
				},
			]
		);

		type ForeignAssets = <AssetHubWestend as AssetHubWestendPallet>::ForeignAssets;
		assert!(ForeignAssets::asset_exists(foreign_asset_at_asset_hub));
	});
}

/// We tests two things here:
/// - Parachain should be able to send XCM paying its fee at Asset Hub using system asset
/// - Parachain should be able to create a new Foreign Asset at Asset Hub
#[test]
fn send_xcm_from_para_to_asset_hub_paying_fee_with_system_asset() {
	let asset_location_on_penpal = Location::new(
		0,
		[Junction::PalletInstance(ASSETS_PALLET_ID), Junction::GeneralIndex(ASSET_ID.into())],
	);
	penpal_register_foreign_asset_on_asset_hub(asset_location_on_penpal);
}

/// We tests two things here:
/// - Parachain should be able to send XCM paying its fee at Asset Hub using sufficient asset
/// - Parachain should be able to create a new Asset at Asset Hub
#[test]
fn send_xcm_from_para_to_asset_hub_paying_fee_with_sufficient_asset() {
	let para_sovereign_account = AssetHubWestend::sovereign_account_id_of(
		AssetHubWestend::sibling_location_of(PenpalA::para_id()),
	);

	// Force create and mint sufficient assets for Parachain's sovereign account
	AssetHubWestend::force_create_and_mint_asset(
		ASSET_ID,
		ASSET_MIN_BALANCE,
		true,
		para_sovereign_account.clone(),
		Some(Weight::from_parts(78_628_000, 3675)),
		ASSET_MIN_BALANCE * 1000000000,
	);

	// Just a different `asset_id`` that does not exist yet
	let new_asset_id = ASSET_ID + 1;

	// Encoded `create_asset` call to be executed in AssetHub
	let call = AssetHubWestend::create_asset_call(
		new_asset_id,
		ASSET_MIN_BALANCE,
		para_sovereign_account.clone(),
	);

	let origin_kind = OriginKind::SovereignAccount;
	let fee_amount = ASSET_MIN_BALANCE * 1000000;
	let asset =
		([PalletInstance(ASSETS_PALLET_ID), GeneralIndex(ASSET_ID.into())], fee_amount).into();
	let asset_location =
		Location::new(0, [PalletInstance(ASSETS_PALLET_ID), GeneralIndex(ASSET_ID.into())]);

	let root_origin = <PenpalA as Chain>::RuntimeOrigin::root();
	let system_para_destination = PenpalA::sibling_location_of(AssetHubWestend::para_id()).into();
	let xcm = xcm_transact_paid_execution(call, origin_kind, asset, para_sovereign_account.clone());

	// SA-of-Penpal-on-AHR needs to have balance to pay for asset creation deposit
	AssetHubWestend::fund_accounts(vec![(
		para_sovereign_account.clone().into(),
		ASSET_HUB_WESTEND_ED * 10000000000,
	)]);

	create_pool_with_relay_native_on!(
		AssetHubWestend,
		asset_location,
		para_sovereign_account.clone(),
		9_000_000_000_000_000,
		9_000_000_000_000
	);

	PenpalA::execute_with(|| {
		assert_ok!(<PenpalA as PenpalAPallet>::PolkadotXcm::send(
			root_origin,
			bx!(system_para_destination),
			bx!(xcm),
		));

		PenpalA::assert_xcm_pallet_sent();
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		AssetHubWestend::assert_xcmp_queue_success(None);
		assert_expected_events!(
			AssetHubWestend,
			vec![
				// Withdrawn the fee
				RuntimeEvent::Assets(pallet_assets::Event::Withdrawn { asset_id, who, amount }) => {
					asset_id: *asset_id == ASSET_ID,
					who: *who == para_sovereign_account,
					amount: *amount == fee_amount,
				},
				// Asset created
				RuntimeEvent::Assets(pallet_assets::Event::Created { asset_id, creator, owner }) => {
					asset_id: *asset_id == new_asset_id,
					creator: *creator == para_sovereign_account.clone(),
					owner: *owner == para_sovereign_account,
				},
			]
		);
	});
}
