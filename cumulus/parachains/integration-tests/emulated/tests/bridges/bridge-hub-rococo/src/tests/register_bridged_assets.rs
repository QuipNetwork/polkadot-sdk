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

use crate::{imports::*, tests::*};

const XCM_FEE: u128 = 4_000_000_000_000;

/// Tests the registering of a Rococo Asset as a bridged asset on Westend Asset Hub.
#[test]
fn register_rococo_asset_on_wah_from_rah() {
	let sa_of_rah_on_wah =
		AssetHubWestend::sovereign_account_of_parachain_on_other_global_consensus(
			ByGenesis(ROCOCO_GENESIS_HASH),
			AssetHubRococo::para_id(),
		);

	// Rococo Asset Hub asset when bridged to Westend Asset Hub.
	let bridged_asset_at_wah = Location::new(
		2,
		[
			GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
			Parachain(AssetHubRococo::para_id().into()),
			PalletInstance(ASSETS_PALLET_ID),
			GeneralIndex(ASSET_ID.into()),
		],
	);

	// Encoded `create_asset` call to be executed in Westend Asset Hub ForeignAssets pallet.
	let call = AssetHubWestend::create_foreign_asset_call(
		bridged_asset_at_wah.clone(),
		ASSET_MIN_BALANCE,
		sa_of_rah_on_wah.clone(),
	);

	let origin_kind = OriginKind::Xcm;
	let fee_amount = XCM_FEE;
	let fees = (Parent, fee_amount).into();

	let xcm = xcm_transact_paid_execution(call, origin_kind, fees, sa_of_rah_on_wah.clone());

	// SA-of-RAH-on-WAH needs to have balance to pay for fees and asset creation deposit
	AssetHubWestend::fund_accounts(vec![(
		sa_of_rah_on_wah.clone(),
		ASSET_HUB_WESTEND_ED * 10000000000,
	)]);

	let destination = asset_hub_westend_location();

	// set XCM versions
	AssetHubRococo::force_xcm_version(destination.clone(), XCM_VERSION);
	BridgeHubRococo::force_xcm_version(bridge_hub_westend_location(), XCM_VERSION);

	let root_origin = <AssetHubRococo as Chain>::RuntimeOrigin::root();
	AssetHubRococo::execute_with(|| {
		assert_ok!(<AssetHubRococo as AssetHubRococoPallet>::PolkadotXcm::send(
			root_origin,
			bx!(destination.into()),
			bx!(xcm),
		));

		AssetHubRococo::assert_xcm_pallet_sent();
	});

	assert_bridge_hub_rococo_message_accepted(true);
	assert_bridge_hub_westend_message_received();
	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		AssetHubWestend::assert_xcmp_queue_success(None);
		assert_expected_events!(
			AssetHubWestend,
			vec![
				// Withdrawn the fee
				RuntimeEvent::Balances(pallet_balances::Event::Withdraw { who, amount }) => {
					who: *who == sa_of_rah_on_wah.clone(),
					amount: *amount == fee_amount,
				},
				// Foreign Asset created
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Created { asset_id, creator, owner }) => {
					asset_id: asset_id == &bridged_asset_at_wah,
					creator: *creator == sa_of_rah_on_wah.clone(),
					owner: *owner == sa_of_rah_on_wah,
				},
				// Unspent fee deposited to origin
				RuntimeEvent::Balances(pallet_balances::Event::Deposit { who, .. }) => {
					who: *who == sa_of_rah_on_wah.clone(),
				},
			]
		);
		type ForeignAssets = <AssetHubWestend as AssetHubWestendPallet>::ForeignAssets;
		assert!(ForeignAssets::asset_exists(bridged_asset_at_wah));
	});
}
