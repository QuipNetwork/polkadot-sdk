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

use crate::{
	asset_exists_on, assets_balance_on, assets_issuance_on, foreign_balance_on,
	foreign_issuance_on, imports::*, tests::send::penpal_register_foreign_asset_on_asset_hub,
};

// Registers a new asset on Penpal, then registers it over XCM as foreign asset on Asset Hub.
// The foreign asset is set up either as teleportable between Penpal and AH, by making AH a reserve
// for it too. Or it keeps the asset's reserve solely on Penpal resulting in reserve-based transfers
// between Penpal and AH.
pub fn set_up_foreign_asset(
	sender: sp_runtime::AccountId32,
	asset_location_on_penpal: Location,
	asset_amount_to_send: u128,
	teleportable: bool,
) -> (Location, Location) {
	let asset_owner = PenpalAssetOwner::get();

	// Give the sender enough native
	PenpalA::fund_accounts(vec![(sender.clone(), asset_amount_to_send)]);

	// Create the asset on Penpal
	let to_fund = asset_amount_to_send * 2;
	PenpalA::force_create_foreign_asset(
		asset_location_on_penpal.clone(),
		asset_owner.clone(),
		true,
		ASSET_MIN_BALANCE,
		vec![(sender.clone(), to_fund)],
	);

	asset_exists_on!(PenpalA, asset_location_on_penpal.clone());

	// Setup a pool on Penpal between native asset and newly created asset, so we can pay fees using
	// new asset directly.
	create_foreign_pool_with_native_on!(
		PenpalA,
		asset_location_on_penpal.clone(),
		asset_owner.clone()
	);

	// Register asset on Asset Hub using XCM
	let penpal_sovereign_account = AssetHubWestend::sovereign_account_id_of(
		AssetHubWestend::sibling_location_of(PenpalA::para_id()),
	);
	let penpal_location = Location::new(1, [Junction::Parachain(PenpalA::para_id().into())]);
	let foreign_asset_at_asset_hub =
		penpal_location.clone().appended_with(asset_location_on_penpal.clone()).unwrap();
	// Do remote registration
	penpal_register_foreign_asset_on_asset_hub(asset_location_on_penpal.clone());

	// Setup a pool on Asset Hub between native asset and newly created asset, so we can pay fees
	// using new asset directly.
	create_foreign_pool_with_parent_native_on!(
		AssetHubWestend,
		foreign_asset_at_asset_hub.clone(),
		penpal_sovereign_account.clone()
	);

	if teleportable {
		// Configure Penpal to allow teleports of this asset to AH
		PenpalA::execute_with(|| {
			assert_ok!(<PenpalA as Chain>::System::set_storage(
				<PenpalA as Chain>::RuntimeOrigin::root(),
				vec![(
					PenpalLocalTeleportableToAssetHub::key().to_vec(),
					asset_location_on_penpal.encode(),
				)],
			));
		});
	}
	let reserves_data = vec![(penpal_location, teleportable).into()];
	AssetHubWestend::set_foreign_asset_reserves(
		foreign_asset_at_asset_hub.clone(),
		penpal_sovereign_account.clone(),
		reserves_data,
	);
	(asset_location_on_penpal, foreign_asset_at_asset_hub)
}

// Helper for Penpal root to call ForeignAssets::set_reserves() on Asset Hub.
pub fn penpal_set_foreign_asset_reserves_on_asset_hub(
	asset_id_on_ah: Location,
	reserves: Vec<ForeignAssetReserveData>,
) {
	// Encoded `set_reserves` call to be executed in AssetHub
	let call = <AssetHubWestend as Chain>::RuntimeCall::ForeignAssets(pallet_assets::Call::<
		<AssetHubWestend as Chain>::Runtime,
		pallet_assets::Instance2,
	>::set_reserves {
		id: asset_id_on_ah.into(),
		reserves: reserves.try_into().unwrap(),
	})
	.encode()
	.into();
	let penpal_sovereign = AssetHubWestend::sovereign_account_id_of(
		AssetHubWestend::sibling_location_of(PenpalA::para_id()),
	);
	let origin_kind = OriginKind::Xcm;
	let fee_amount = ASSET_HUB_WESTEND_ED * 1000000;
	let system_asset = (Parent, fee_amount).into();
	let root_origin = <PenpalA as Chain>::RuntimeOrigin::root();
	let asset_hub_location = PenpalA::sibling_location_of(AssetHubWestend::para_id()).into();
	let xcm =
		xcm_transact_paid_execution(call, origin_kind, system_asset, penpal_sovereign.clone());

	PenpalA::execute_with(|| {
		assert_ok!(<PenpalA as PenpalAPallet>::PolkadotXcm::send(
			root_origin,
			bx!(asset_hub_location),
			bx!(xcm),
		));
		PenpalA::assert_xcm_pallet_sent();
	});
}

// ==============================================================================================
// ==== Bidirectional Transfer - Teleportable Foreign Asset - Penpal<->AssetHub ====
// ==============================================================================================
/// Transfers of teleportable foreign asset from Penpal to AssetHub and back.
/// Also verifies that reserve-transferring the asset fails both ways.
#[test]
fn bidirectional_teleport_foreign_asset_between_penpal_and_asset_hub() {
	let sender = PenpalASender::get();
	let receiver = AssetHubWestendReceiver::get();
	let new_asset_id = local_penpal_asset(42);
	let asset_amount_to_send = ASSET_HUB_WESTEND_ED * 10_000;
	let (asset_location_on_penpal, foreign_asset_location_on_ah) =
		set_up_foreign_asset(sender.clone(), new_asset_id.clone(), asset_amount_to_send, true);

	////////////////////////////////
	// Teleport it from Penpal to AH
	////////////////////////////////

	let penpal_sender_balance_before = assets_balance_on!(PenpalA, new_asset_id.clone(), &sender);
	let ah_receiver_balance_before =
		foreign_balance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone(), &receiver);

	let dest = PenpalA::sibling_location_of(AssetHubWestend::para_id());
	let assets: Assets =
		vec![(asset_location_on_penpal.clone(), asset_amount_to_send).into()].into();

	let penpal_issuance_before = assets_issuance_on!(PenpalA, new_asset_id.clone());
	let ah_issuance_before =
		foreign_issuance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone());
	// execute xcm from penpal to asset hub
	PenpalA::execute_with(|| {
		// xcm to be executed at dest
		let xcm_on_dest = Xcm(vec![
			// since this is the last hop, we don't need to further use any assets previously
			// reserved for fees (there are no further hops to cover delivery fees for); we
			// RefundSurplus to get back any unspent fees
			RefundSurplus,
			DepositAsset { assets: Wild(All), beneficiary: receiver.clone().into() },
		]);
		let xcm = Xcm::<()>(vec![
			WithdrawAsset(assets.clone().into()),
			SetFeesMode { jit_withdraw: true },
			InitiateTransfer {
				destination: dest.clone(),
				remote_fees: Some(AssetTransferFilter::Teleport(assets.clone().into())),
				preserve_origin: false,
				assets: BoundedVec::new(),
				remote_xcm: xcm_on_dest.clone(),
			},
		]);
		// teleporting the asset works
		<PenpalA as PenpalAPallet>::PolkadotXcm::execute(
			<PenpalA as Chain>::RuntimeOrigin::signed(sender.clone()),
			bx!(xcm::VersionedXcm::from(xcm.into())),
			Weight::MAX,
		)
		.unwrap();
	});

	let penpal_sender_balance_after = assets_balance_on!(PenpalA, new_asset_id.clone(), &sender);
	let ah_receiver_balance_after =
		foreign_balance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone(), &receiver);

	assert!(penpal_sender_balance_after < penpal_sender_balance_before);
	assert!(ah_receiver_balance_after > ah_receiver_balance_before);

	let penpal_issuance_after = assets_issuance_on!(PenpalA, new_asset_id.clone());
	let ah_issuance_after =
		foreign_issuance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone());
	// Penpal keeps track of its own teleport assets using its checking account.
	assert_eq!(
		penpal_issuance_before, penpal_issuance_after,
		"Unexpected total issuance change on Penpal"
	);
	assert!(penpal_issuance_after > 0);
	// Issuance on AH is expected to increase because of the foreign asset being teleported in.
	assert_eq!(
		ah_issuance_after,
		ah_issuance_before + asset_amount_to_send,
		"Unexpected total issuance on Asset Hub"
	);

	let ah_issuance_before =
		foreign_issuance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone());
	// reserve-transferring the asset fails
	PenpalA::execute_with(|| {
		let xcm = Xcm::<()>(vec![
			WithdrawAsset(assets.clone().into()),
			SetFeesMode { jit_withdraw: true },
			InitiateTransfer {
				destination: dest,
				remote_fees: Some(AssetTransferFilter::ReserveDeposit(assets.clone().into())),
				preserve_origin: false,
				assets: BoundedVec::new(),
				remote_xcm: Default::default(),
			},
		]);
		<PenpalA as PenpalAPallet>::PolkadotXcm::execute(
			<PenpalA as Chain>::RuntimeOrigin::signed(sender.clone()),
			bx!(xcm::VersionedXcm::from(xcm.into())),
			Weight::MAX,
		)
		.unwrap();
	});
	// AH is expected to reject the transfer with `UntrustedReserveLocation`
	let expected_origin = AssetHubWestend::sibling_location_of(PenpalA::para_id());
	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::PolkadotXcm(
					pallet_xcm::Event::ProcessXcmError { origin, error, .. }
				) => {
					origin: *origin == expected_origin,
					error: *error == xcm::latest::Error::UntrustedReserveLocation,
				},
			]
		);
	});
	let ah_issuance_after =
		foreign_issuance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone());
	assert_eq!(ah_issuance_after, ah_issuance_before);

	/////////////////////////////////////
	// Teleport it back from AH to Penpal
	/////////////////////////////////////

	let asset_amount_to_send = ah_receiver_balance_after;
	let ah_sender_balance_before =
		foreign_balance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone(), &receiver);
	let penpal_receiver_balance_before = assets_balance_on!(PenpalA, new_asset_id.clone(), &sender);

	let dest = AssetHubWestend::sibling_location_of(PenpalA::para_id());
	// execute xcm from asset hub to penpal
	AssetHubWestend::execute_with(|| {
		let assets: Assets =
			vec![(foreign_asset_location_on_ah.clone(), asset_amount_to_send).into()].into();
		// xcm to be executed at dest
		let xcm_on_dest = Xcm(vec![
			// since this is the last hop, we don't need to further use any assets previously
			// reserved for fees (there are no further hops to cover delivery fees for); we
			// RefundSurplus to get back any unspent fees
			RefundSurplus,
			DepositAsset { assets: Wild(All), beneficiary: sender.clone().into() },
		]);
		// reserve-transferring the asset back to penpal fails
		let xcm = Xcm::<()>(vec![
			WithdrawAsset(assets.clone().into()),
			SetFeesMode { jit_withdraw: true },
			InitiateTransfer {
				destination: dest.clone(),
				remote_fees: Some(AssetTransferFilter::ReserveWithdraw(assets.clone().into())),
				preserve_origin: false,
				assets: BoundedVec::new(),
				remote_xcm: Default::default(),
			},
		]);
		assert!(matches!(
			<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
				<AssetHubWestend as Chain>::RuntimeOrigin::signed(receiver.clone()),
				bx!(xcm::VersionedXcm::from(xcm.into())),
				Weight::MAX,
			),
			Err(sp_runtime::DispatchErrorWithPostInfo { .. }),
		));
		// teleporting it back works
		let xcm = Xcm::<()>(vec![
			WithdrawAsset(assets.clone().into()),
			SetFeesMode { jit_withdraw: true },
			InitiateTransfer {
				destination: dest,
				remote_fees: Some(AssetTransferFilter::Teleport(assets.into())),
				preserve_origin: false,
				assets: BoundedVec::new(),
				remote_xcm: xcm_on_dest,
			},
		]);
		<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
			<AssetHubWestend as Chain>::RuntimeOrigin::signed(receiver.clone()),
			bx!(xcm::VersionedXcm::from(xcm.into())),
			Weight::MAX,
		)
		.unwrap();
	});

	let ah_sender_balance_after =
		foreign_balance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone(), &receiver);
	let penpal_receiver_balance_after = assets_balance_on!(PenpalA, new_asset_id.clone(), &sender);

	assert!(ah_sender_balance_after < ah_sender_balance_before);
	assert!(penpal_receiver_balance_after > penpal_receiver_balance_before);
	let ah_issuance_after = foreign_issuance_on!(AssetHubWestend, foreign_asset_location_on_ah);
	assert_eq!(ah_issuance_after, ah_issuance_before - asset_amount_to_send);
	let penpal_issuance_after = assets_issuance_on!(PenpalA, new_asset_id);
	assert_eq!(
		penpal_issuance_before, penpal_issuance_after,
		"Unexpected total issuance change on Penpal"
	);
}

// ==============================================================================================
// ==== Bidirectional Transfer - Reserve-based Foreign Asset - Penpal<->AssetHub ====
// ==============================================================================================
/// Transfers of foreign asset from Penpal to AssetHub and back. Foreign Asset is not registered
/// with Asset Hub as a trusted reserve, ergo teleports are not available and reserve-transfers are
/// to be used. Also verifies that teleporting the asset fails both ways.
#[test]
fn bidirectional_reserve_transfer_foreign_asset_between_penpal_and_asset_hub() {
	let sender = PenpalASender::get();
	let receiver = AssetHubWestendReceiver::get();
	let new_asset_id = local_penpal_asset(42);
	let asset_amount_to_send = ASSET_HUB_WESTEND_ED * 10_000;
	let (asset_location_on_penpal, foreign_asset_location_on_ah) =
		set_up_foreign_asset(sender.clone(), new_asset_id.clone(), asset_amount_to_send, false);

	////////////////////////////////////////
	// Reserve-transfer it from Penpal to AH
	////////////////////////////////////////

	let penpal_sender_balance_before = assets_balance_on!(PenpalA, new_asset_id.clone(), &sender);
	let ah_receiver_balance_before =
		foreign_balance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone(), &receiver);

	let dest = PenpalA::sibling_location_of(AssetHubWestend::para_id());
	let assets: Assets =
		vec![(asset_location_on_penpal.clone(), asset_amount_to_send).into()].into();

	let penpal_issuance_before = assets_issuance_on!(PenpalA, new_asset_id.clone());
	let ah_issuance_before =
		foreign_issuance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone());
	// execute xcm from penpal to asset hub
	PenpalA::execute_with(|| {
		// xcm to be executed at dest
		let xcm_on_dest = Xcm(vec![
			// since this is the last hop, we don't need to further use any assets previously
			// reserved for fees (there are no further hops to cover delivery fees for); we
			// RefundSurplus to get back any unspent fees
			RefundSurplus,
			DepositAsset { assets: Wild(All), beneficiary: receiver.clone().into() },
		]);
		// teleporting the asset fails
		let xcm = Xcm::<()>(vec![
			WithdrawAsset(assets.clone().into()),
			SetFeesMode { jit_withdraw: true },
			InitiateTransfer {
				destination: dest.clone(),
				remote_fees: Some(AssetTransferFilter::Teleport(assets.clone().into())),
				preserve_origin: false,
				assets: BoundedVec::new(),
				remote_xcm: xcm_on_dest.clone(),
			},
		]);
		assert!(matches!(
			<PenpalA as PenpalAPallet>::PolkadotXcm::execute(
				<PenpalA as Chain>::RuntimeOrigin::signed(sender.clone()),
				bx!(xcm::VersionedXcm::from(xcm.into())),
				Weight::MAX,
			),
			Err(sp_runtime::DispatchErrorWithPostInfo { .. }),
		));
		// reserve-transferring the asset works
		let xcm = Xcm::<()>(vec![
			WithdrawAsset(assets.clone().into()),
			SetFeesMode { jit_withdraw: true },
			InitiateTransfer {
				destination: dest,
				remote_fees: Some(AssetTransferFilter::ReserveDeposit(assets.into())),
				preserve_origin: false,
				assets: BoundedVec::new(),
				remote_xcm: xcm_on_dest,
			},
		]);
		assert_ok!(<PenpalA as PenpalAPallet>::PolkadotXcm::execute(
			<PenpalA as Chain>::RuntimeOrigin::signed(sender.clone()),
			bx!(xcm::VersionedXcm::from(xcm.into())),
			Weight::MAX,
		));
	});

	let penpal_sender_balance_after = assets_balance_on!(PenpalA, new_asset_id.clone(), &sender);
	let ah_receiver_balance_after =
		foreign_balance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone(), &receiver);

	assert!(penpal_sender_balance_after < penpal_sender_balance_before);
	assert!(ah_receiver_balance_after > ah_receiver_balance_before);

	let penpal_issuance_after = assets_issuance_on!(PenpalA, new_asset_id.clone());
	let ah_issuance_after =
		foreign_issuance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone());
	assert_eq!(
		penpal_issuance_before, penpal_issuance_after,
		"Unexpected total issuance change on Penpal"
	);
	assert!(penpal_issuance_after > 0);
	// Issuance on AH increases because of the foreign asset being reserve-transferred in.
	assert_eq!(
		ah_issuance_after,
		ah_issuance_before + asset_amount_to_send,
		"Unexpected total issuance on Asset Hub"
	);

	/////////////////////////////////////////////
	// Reserve-transfer it back from AH to Penpal
	/////////////////////////////////////////////

	let asset_amount_to_send = ah_receiver_balance_after;
	let ah_sender_balance_before =
		foreign_balance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone(), &receiver);
	let penpal_receiver_balance_before = assets_balance_on!(PenpalA, new_asset_id.clone(), &sender);
	let ah_issuance_before =
		foreign_issuance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone());

	let dest = AssetHubWestend::sibling_location_of(PenpalA::para_id());
	// execute xcm from asset hub to penpal
	AssetHubWestend::execute_with(|| {
		let assets: Assets =
			vec![(foreign_asset_location_on_ah.clone(), asset_amount_to_send).into()].into();
		// xcm to be executed at dest
		let xcm_on_dest = Xcm(vec![
			// since this is the last hop, we don't need to further use any assets previously
			// reserved for fees (there are no further hops to cover delivery fees for); we
			// RefundSurplus to get back any unspent fees
			RefundSurplus,
			DepositAsset { assets: Wild(All), beneficiary: sender.clone().into() },
		]);
		// teleporting the asset back to penpal fails
		let xcm = Xcm::<()>(vec![
			WithdrawAsset(assets.clone().into()),
			SetFeesMode { jit_withdraw: true },
			InitiateTransfer {
				destination: dest.clone(),
				remote_fees: Some(AssetTransferFilter::Teleport(assets.clone().into())),
				preserve_origin: false,
				assets: BoundedVec::new(),
				remote_xcm: xcm_on_dest.clone(),
			},
		]);
		assert!(matches!(
			<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
				<AssetHubWestend as Chain>::RuntimeOrigin::signed(receiver.clone()),
				bx!(xcm::VersionedXcm::from(xcm.into())),
				Weight::MAX,
			),
			Err(sp_runtime::DispatchErrorWithPostInfo { .. }),
		));
		// but reserve-transferring it back works
		let xcm = Xcm::<()>(vec![
			WithdrawAsset(assets.clone().into()),
			SetFeesMode { jit_withdraw: true },
			InitiateTransfer {
				destination: dest,
				remote_fees: Some(AssetTransferFilter::ReserveWithdraw(assets.into())),
				preserve_origin: false,
				assets: BoundedVec::new(),
				remote_xcm: xcm_on_dest,
			},
		]);
		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
			<AssetHubWestend as Chain>::RuntimeOrigin::signed(receiver.clone()),
			bx!(xcm::VersionedXcm::from(xcm.into())),
			Weight::MAX,
		));
	});

	let ah_sender_balance_after =
		foreign_balance_on!(AssetHubWestend, foreign_asset_location_on_ah.clone(), &receiver);
	let penpal_receiver_balance_after = assets_balance_on!(PenpalA, new_asset_id.clone(), &sender);

	assert!(ah_sender_balance_after < ah_sender_balance_before);
	assert!(penpal_receiver_balance_after > penpal_receiver_balance_before);
	let ah_issuance_after = foreign_issuance_on!(AssetHubWestend, foreign_asset_location_on_ah);
	assert_eq!(ah_issuance_after, ah_issuance_before - asset_amount_to_send);
	let penpal_issuance_after = assets_issuance_on!(PenpalA, new_asset_id);
	assert_eq!(
		penpal_issuance_before, penpal_issuance_after,
		"Unexpected total issuance change on Penpal"
	);
}

/// Verifies that foreign asset reserves can be only set by signed `Owner` account or through XCM
/// using remote `ManagerOrigin`.
#[test]
fn verify_foreign_asset_origin_checks() {
	let sender = PenpalASender::get();
	let new_asset_id = local_penpal_asset(42);
	let asset_amount_to_send = ASSET_HUB_WESTEND_ED * 10_000;
	let (_, foreign_asset_location_on_ah) =
		set_up_foreign_asset(sender.clone(), new_asset_id.clone(), asset_amount_to_send, false);

	let penpal_sovereign = AssetHubWestend::sovereign_account_id_of(
		AssetHubWestend::sibling_location_of(PenpalA::para_id()),
	);
	let reserves_data = ForeignAssetReserveData {
		reserve: AssetHubWestend::sibling_location_of(PenpalA::para_id()),
		teleportable: true,
	};
	// Set asset reserves using signed `owner` account.
	let origin = <AssetHubWestend as Chain>::RuntimeOrigin::signed(penpal_sovereign);
	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::set_reserves(
			origin,
			foreign_asset_location_on_ah.clone(),
			vec![reserves_data.clone()].try_into().unwrap(),
		)
		.unwrap();
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::ForeignAssets(pallet_assets::Event::ReservesUpdated { asset_id, .. }) => {
					asset_id: *asset_id == foreign_asset_location_on_ah,
				},
			]
		);
	});
	// Now set asset reserves using some other signed account. It should fail.
	let origin = <AssetHubWestend as Chain>::RuntimeOrigin::signed(sender.clone());
	AssetHubWestend::execute_with(|| {
		assert!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::set_reserves(
			origin,
			foreign_asset_location_on_ah.clone(),
			vec![reserves_data].try_into().unwrap(),
		)
		.is_err());
	});
	// Now set asset reserves using remote XCM from correct origin chain.
	// Use wrong `{origin, asset}` combination.
	let asset_id_on_ah = PenpalBPen2TeleportableAssetLocation::get();
	penpal_set_foreign_asset_reserves_on_asset_hub(asset_id_on_ah, vec![]);
	// Verify it failed.
	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed { success: false, .. }) => {},
			]
		);
	});
	// Verify it works when using right `{origin, asset}` combination.
	let asset_id_on_ah = foreign_asset_location_on_ah;
	penpal_set_foreign_asset_reserves_on_asset_hub(asset_id_on_ah.clone(), vec![]);
	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		AssetHubWestend::assert_xcmp_queue_success(None);
		assert_expected_events!(
			AssetHubWestend,
			vec![
				// Foreign Asset created
				RuntimeEvent::ForeignAssets(pallet_assets::Event::ReservesRemoved { asset_id }) => {
					asset_id: *asset_id == asset_id_on_ah,
				},
			]
		);
	});
}
