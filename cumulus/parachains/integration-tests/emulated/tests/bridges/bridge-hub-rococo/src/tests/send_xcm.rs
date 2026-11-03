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

use rococo_westend_system_emulated_network::rococo_emulated_chain::rococo_runtime::Dmp;

use crate::tests::*;

#[test]
fn send_xcm_from_rococo_relay_to_westend_asset_hub_should_fail_on_not_applicable() {
	// Init tests variables
	// XcmPallet send arguments
	let sudo_origin = <Rococo as Chain>::RuntimeOrigin::root();
	let destination = Rococo::child_location_of(BridgeHubRococo::para_id()).into();
	let weight_limit = WeightLimit::Unlimited;
	let check_origin = None;

	let remote_xcm = Xcm(vec![ClearOrigin]);

	let xcm = VersionedXcm::from(Xcm(vec![
		UnpaidExecution { weight_limit, check_origin },
		ExportMessage {
			network: ByGenesis(WESTEND_GENESIS_HASH),
			destination: [Parachain(AssetHubWestend::para_id().into())].into(),
			xcm: remote_xcm,
		},
	]));

	// Rococo Global Consensus
	// Send XCM message from Relay Chain to Bridge Hub source Parachain
	Rococo::execute_with(|| {
		Dmp::make_parachain_reachable(BridgeHubRococo::para_id());

		assert_ok!(<Rococo as RococoPallet>::XcmPallet::send(
			sudo_origin,
			bx!(destination),
			bx!(xcm),
		));

		type RuntimeEvent = <Rococo as Chain>::RuntimeEvent;

		assert_expected_events!(
			Rococo,
			vec![
				RuntimeEvent::XcmPallet(pallet_xcm::Event::Sent { .. }) => {},
			]
		);
	});
	// Receive XCM message in Bridge Hub source Parachain, it should fail, because we don't have
	// opened bridge/lane.
	assert_bridge_hub_rococo_message_accepted(false);
}

#[test]
fn send_xcm_through_opened_lane_with_different_xcm_version_on_hops_works() {
	// prepare data
	let destination = asset_hub_westend_location();
	let native_token = Location::parent();
	let amount = ASSET_HUB_ROCOCO_ED * 1_000;
	// fund sender
	AssetHubRococo::fund_accounts(vec![(AssetHubRococoSender::get().into(), amount * 10)]);

	// Initially set only default version on all runtimes
	let newer_xcm_version = xcm::prelude::XCM_VERSION;
	let older_xcm_version = newer_xcm_version - 1;
	AssetHubRococo::force_default_xcm_version(Some(older_xcm_version));
	BridgeHubRococo::force_default_xcm_version(Some(older_xcm_version));
	BridgeHubWestend::force_default_xcm_version(Some(older_xcm_version));
	AssetHubWestend::force_default_xcm_version(Some(older_xcm_version));

	// send XCM from AssetHubRococo - fails - destination version not known
	assert_err!(
		send_assets_from_asset_hub_rococo(
			destination.clone(),
			(native_token.clone(), amount).into(),
			0,
			TransferType::LocalReserve
		),
		DispatchError::Module(sp_runtime::ModuleError {
			index: 31,
			error: [1, 0, 0, 0],
			message: Some("SendFailure")
		})
	);

	// set destination version
	AssetHubRococo::force_xcm_version(destination.clone(), newer_xcm_version);

	// set version with `ExportMessage` for BridgeHubRococo
	AssetHubRococo::force_xcm_version(
		ParentThen(Parachain(BridgeHubRococo::para_id().into()).into()).into(),
		newer_xcm_version,
	);
	// send XCM from AssetHubRococo - ok
	assert_ok!(send_assets_from_asset_hub_rococo(
		destination.clone(),
		(native_token.clone(), amount).into(),
		0,
		TransferType::LocalReserve
	));

	// `ExportMessage` on local BridgeHub - fails - remote BridgeHub version not known
	assert_bridge_hub_rococo_message_accepted(false);

	// set version for remote BridgeHub on BridgeHubRococo
	BridgeHubRococo::force_xcm_version(bridge_hub_westend_location(), newer_xcm_version);
	// set version for AssetHubWestend on BridgeHubWestend
	BridgeHubWestend::force_xcm_version(
		ParentThen(Parachain(AssetHubWestend::para_id().into()).into()).into(),
		newer_xcm_version,
	);

	// send XCM from AssetHubRococo - ok
	assert_ok!(send_assets_from_asset_hub_rococo(
		destination.clone(),
		(native_token.clone(), amount).into(),
		0,
		TransferType::LocalReserve
	));
	assert_bridge_hub_rococo_message_accepted(true);
	assert_bridge_hub_westend_message_received();
	// message delivered and processed at destination
	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![
				// message processed with failure, but for this scenario it is ok, important is that was delivered
				RuntimeEvent::MessageQueue(
					pallet_message_queue::Event::Processed { success: false, .. }
				) => {},
			]
		);
	});
}
