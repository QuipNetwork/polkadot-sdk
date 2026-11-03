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

use crate::tests::*;
use emulated_integration_tests_common::assets_balance_on;

fn send_assets_over_bridge<F: FnOnce()>(send_fn: F) {
	// set XCM versions
	let local_asset_hub = PenpalA::sibling_location_of(AssetHubRococo::para_id());
	PenpalA::force_xcm_version(local_asset_hub.clone(), XCM_VERSION);
	AssetHubRococo::force_xcm_version(asset_hub_westend_location(), XCM_VERSION);
	BridgeHubRococo::force_xcm_version(bridge_hub_westend_location(), XCM_VERSION);

	// send message over bridge
	send_fn();

	// process and verify intermediary hops
	assert_bridge_hub_rococo_message_accepted(true);
	assert_bridge_hub_westend_message_received();
}

fn set_up_rocs_for_penpal_rococo_through_ahr_to_ahw(
	sender: &AccountId,
	amount: u128,
) -> (Location, v5::Location) {
	let roc_at_rococo_parachains = roc_at_ah_rococo();
	let roc_at_asset_hub_westend = bridged_roc_at_ah_westend();
	let reserves = vec![(asset_hub_rococo_global_location(), false).into()];
	create_foreign_on_ah_westend(roc_at_asset_hub_westend.clone(), true, reserves);

	let penpal_location = AssetHubRococo::sibling_location_of(PenpalA::para_id());
	let sov_penpal_on_ahr = AssetHubRococo::sovereign_account_id_of(penpal_location);
	// fund Penpal's sovereign account on AssetHub
	AssetHubRococo::fund_accounts(vec![(sov_penpal_on_ahr.into(), amount * 2)]);
	// fund Penpal's sender account
	PenpalA::mint_foreign_asset(
		<PenpalA as Chain>::RuntimeOrigin::signed(PenpalAssetOwner::get()),
		roc_at_rococo_parachains.clone(),
		sender.clone(),
		amount * 2,
	);
	(roc_at_rococo_parachains, roc_at_asset_hub_westend)
}

fn send_assets_from_penpal_rococo_through_rococo_ah_to_westend_ah(
	destination: Location,
	assets: (Assets, TransferType),
	fees: (AssetId, TransferType),
	custom_xcm_on_dest: Xcm<()>,
) {
	send_assets_over_bridge(|| {
		let sov_penpal_on_ahr = AssetHubRococo::sovereign_account_id_of(
			AssetHubRococo::sibling_location_of(PenpalA::para_id()),
		);
		// send message over bridge
		assert_ok!(PenpalA::execute_with(|| {
			let signed_origin = <PenpalA as Chain>::RuntimeOrigin::signed(PenpalASender::get());
			<PenpalA as PenpalAPallet>::PolkadotXcm::transfer_assets_using_type_and_then(
				signed_origin,
				bx!(destination.into()),
				bx!(assets.0.into()),
				bx!(assets.1),
				bx!(fees.0.into()),
				bx!(fees.1),
				bx!(VersionedXcm::from(custom_xcm_on_dest)),
				WeightLimit::Unlimited,
			)
		}));
		// verify intermediary AH Rococo hop
		AssetHubRococo::execute_with(|| {
			type RuntimeEvent = <AssetHubRococo as Chain>::RuntimeEvent;
			assert_expected_events!(
				AssetHubRococo,
				vec![
					// Amount to reserve transfer is withdrawn from Penpal's sovereign account
					RuntimeEvent::Balances(
						pallet_balances::Event::Withdraw { who, .. }
					) => {
						who: *who == sov_penpal_on_ahr.clone().into(),
					},
					// Amount deposited in AHW's sovereign account
					RuntimeEvent::Balances(pallet_balances::Event::Deposit { who, .. }) => {
						who: *who == TreasuryAccount::get(),
					},
					RuntimeEvent::XcmpQueue(
						cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }
					) => {},
				]
			);
		});
	});
}

#[test]
/// Test transfer of ROC from AssetHub Rococo to AssetHub Westend.
fn send_roc_from_asset_hub_rococo_to_asset_hub_westend() {
	let amount = ASSET_HUB_ROCOCO_ED * 1_000_000;
	let sender = AssetHubRococoSender::get();
	let receiver = AssetHubWestendReceiver::get();
	let roc_at_asset_hub_rococo = roc_at_ah_rococo();
	let bridged_roc_at_asset_hub_westend = bridged_roc_at_ah_westend();
	let reserves = vec![(asset_hub_rococo_global_location(), false).into()];
	create_foreign_on_ah_westend(bridged_roc_at_asset_hub_westend.clone(), true, reserves);
	set_up_pool_with_wnd_on_ah_westend(bridged_roc_at_asset_hub_westend.clone(), true);

	let sov_ahw_on_ahr = AssetHubRococo::sovereign_account_of_parachain_on_other_global_consensus(
		ByGenesis(WESTEND_GENESIS_HASH),
		AssetHubWestend::para_id(),
	);
	let rocs_in_reserve_on_ahr_before =
		<AssetHubRococo as Chain>::account_data_of(sov_ahw_on_ahr.clone()).free;
	let sender_rocs_before = <AssetHubRococo as Chain>::account_data_of(sender.clone()).free;
	let receiver_rocs_before =
		foreign_balance_on_ah_westend(bridged_roc_at_asset_hub_westend.clone(), &receiver);

	// send ROCs, use them for fees
	send_assets_over_bridge(|| {
		let destination = asset_hub_westend_location();
		let assets: Assets = (roc_at_asset_hub_rococo.clone(), amount).into();
		let fee_idx = 0;
		let transfer_type = TransferType::LocalReserve;
		assert_ok!(send_assets_from_asset_hub_rococo(destination, assets, fee_idx, transfer_type));
	});

	// verify expected events on final destination
	let roc = Location::new(2, [GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH))]);
	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![
				// issue ROCs on AHW
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { asset_id, who, .. }) => {
					asset_id: *asset_id == roc,
					who: who == &receiver,
				},
				// message processed successfully
				RuntimeEvent::MessageQueue(
					pallet_message_queue::Event::Processed { success: true, .. }
				) => {},
			]
		);
	});

	let sender_rocs_after = <AssetHubRococo as Chain>::account_data_of(sender.clone()).free;
	let receiver_rocs_after =
		foreign_balance_on_ah_westend(bridged_roc_at_asset_hub_westend, &receiver);
	let rocs_in_reserve_on_ahr_after =
		<AssetHubRococo as Chain>::account_data_of(sov_ahw_on_ahr.clone()).free;

	// Sender's ROC balance is reduced
	assert!(sender_rocs_before > sender_rocs_after);
	// Receiver's ROC balance is increased
	assert!(receiver_rocs_after > receiver_rocs_before);
	// Reserve ROC balance is increased by sent amount
	assert_eq!(rocs_in_reserve_on_ahr_after, rocs_in_reserve_on_ahr_before + amount);
}

#[test]
/// Send bridged assets "back" from AssetHub Rococo to AssetHub Westend.
///
/// This mix of assets should cover the whole range:
/// - bridged native assets: ROC,
/// - bridged trust-based assets: USDT (exists only on Westend, Rococo gets it from Westend over
///   bridge),
/// - bridged foreign asset / double-bridged asset (other bridge / Snowfork): wETH (bridged from
///   Ethereum to Westend over Snowbridge, then bridged over to Rococo through this bridge).
fn send_back_wnds_usdt_and_weth_from_asset_hub_rococo_to_asset_hub_westend() {
	let prefund_amount = 10_000_000_000_000u128;
	let amount_to_send = ASSET_HUB_WESTEND_ED * 1_000;
	let sender = AssetHubRococoSender::get();
	let receiver = AssetHubWestendReceiver::get();
	let wnd_at_asset_hub_rococo = bridged_wnd_at_ah_rococo();
	let prefund_accounts = vec![(sender.clone(), prefund_amount)];
	let reserves = vec![(asset_hub_westend_location(), false).into()];
	create_foreign_on_ah_rococo(wnd_at_asset_hub_rococo.clone(), true, reserves, prefund_accounts);

	////////////////////////////////////////////////////////////
	// Let's first send back just some WNDs as a simple example
	////////////////////////////////////////////////////////////

	// fund the AHR's SA on AHW with the WND tokens held in reserve
	let sov_ahr_on_ahw = AssetHubWestend::sovereign_account_of_parachain_on_other_global_consensus(
		ByGenesis(ROCOCO_GENESIS_HASH),
		AssetHubRococo::para_id(),
	);
	AssetHubWestend::fund_accounts(vec![(sov_ahr_on_ahw.clone(), prefund_amount)]);

	let wnds_in_reserve_on_ahw_before =
		<AssetHubWestend as Chain>::account_data_of(sov_ahr_on_ahw.clone()).free;
	assert_eq!(wnds_in_reserve_on_ahw_before, prefund_amount);

	let sender_wnds_before = foreign_balance_on_ah_rococo(wnd_at_asset_hub_rococo.clone(), &sender);
	assert_eq!(sender_wnds_before, prefund_amount);
	let receiver_wnds_before = <AssetHubWestend as Chain>::account_data_of(receiver.clone()).free;

	// send back WNDs, use them for fees
	send_assets_over_bridge(|| {
		let destination = asset_hub_westend_location();
		let assets: Assets = (wnd_at_asset_hub_rococo.clone(), amount_to_send).into();
		let fee_idx = 0;
		let transfer_type = TransferType::DestinationReserve;
		assert_ok!(send_assets_from_asset_hub_rococo(destination, assets, fee_idx, transfer_type));
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![
				// WND is withdrawn from AHR's SA on AHW
				RuntimeEvent::Balances(
					pallet_balances::Event::Withdraw { who, amount }
				) => {
					who: *who == sov_ahr_on_ahw,
					amount: *amount == amount_to_send,
				},
				// WNDs deposited to beneficiary
				RuntimeEvent::Balances(pallet_balances::Event::Deposit { who, .. }) => {
					who: who == &receiver,
				},
				// message processed successfully
				RuntimeEvent::MessageQueue(
					pallet_message_queue::Event::Processed { success: true, .. }
				) => {},
			]
		);
	});

	let sender_wnds_after = foreign_balance_on_ah_rococo(wnd_at_asset_hub_rococo, &sender);
	let receiver_wnds_after = <AssetHubWestend as Chain>::account_data_of(receiver.clone()).free;
	let wnds_in_reserve_on_ahw_after =
		<AssetHubWestend as Chain>::account_data_of(sov_ahr_on_ahw).free;

	// Sender's balance is reduced
	assert!(sender_wnds_before > sender_wnds_after);
	// Receiver's balance is increased
	assert!(receiver_wnds_after > receiver_wnds_before);
	// Reserve balance is reduced by sent amount
	assert_eq!(wnds_in_reserve_on_ahw_after, wnds_in_reserve_on_ahw_before - amount_to_send);

	//////////////////////////////////////////////////////////////////
	// Now let's send back over USDTs + wETH (and pay fees with USDT)
	//////////////////////////////////////////////////////////////////

	// wETH has same relative location on both Westend and Rococo AssetHubs
	let bridged_weth_at_ah = weth_at_asset_hubs();
	let bridged_usdt_at_asset_hub_rococo = bridged_usdt_at_ah_rococo();

	// set up destination chain AH Westend:
	// create a WND/USDT pool to be able to pay fees with USDT (USDT created in genesis)
	set_up_pool_with_wnd_on_ah_westend(usdt_at_ah_westend(), false);
	// prefund AHR's sovereign account on AHW to be able to withdraw USDT and wETH from reserves
	let sov_ahr_on_ahw = AssetHubWestend::sovereign_account_of_parachain_on_other_global_consensus(
		ByGenesis(ROCOCO_GENESIS_HASH),
		AssetHubRococo::para_id(),
	);

	let sov_ahw_on_ahr = AssetHubRococo::sovereign_account_of_parachain_on_other_global_consensus(
		ByGenesis(WESTEND_GENESIS_HASH),
		AssetHubWestend::para_id(),
	);

	AssetHubWestend::mint_asset(
		<AssetHubWestend as Chain>::RuntimeOrigin::signed(AssetHubWestendAssetOwner::get()),
		USDT_ID,
		sov_ahr_on_ahw.clone(),
		amount_to_send * 2,
	);
	AssetHubWestend::mint_foreign_asset(
		<AssetHubWestend as Chain>::RuntimeOrigin::signed(snowbridge_sovereign()),
		bridged_weth_at_ah.clone(),
		sov_ahr_on_ahw.clone(),
		amount_to_send * 2,
	);
	AssetHubRococo::mint_foreign_asset(
		<AssetHubRococo as Chain>::RuntimeOrigin::signed(sov_ahw_on_ahr.clone()),
		bridged_weth_at_ah.clone(),
		sov_ahr_on_ahw,
		prefund_amount,
	);
	AssetHubRococo::mint_foreign_asset(
		<AssetHubRococo as Chain>::RuntimeOrigin::signed(sov_ahw_on_ahr),
		bridged_weth_at_ah.clone(),
		sender.clone(),
		prefund_amount,
	);

	// set up source chain AH Rococo:
	// create wETH and USDT foreign assets on Rococo and prefund sender's account
	let prefund_accounts = vec![(sender.clone(), amount_to_send * 2)];
	let reserves = vec![(asset_hub_westend_location(), false).into()];
	create_foreign_on_ah_rococo(
		bridged_usdt_at_asset_hub_rococo.clone(),
		true,
		reserves,
		prefund_accounts,
	);

	// check balances before
	let receiver_usdts_before = AssetHubWestend::execute_with(|| {
		type Assets = <AssetHubWestend as AssetHubWestendPallet>::Assets;
		<Assets as Inspect<_>>::balance(USDT_ID, &receiver)
	});
	let receiver_weth_before = foreign_balance_on_ah_westend(bridged_weth_at_ah.clone(), &receiver);

	let usdt_id: AssetId = Location::try_from(bridged_usdt_at_asset_hub_rococo).unwrap().into();
	// send USDTs and wETHs
	let assets: Assets = vec![
		(usdt_id.clone(), amount_to_send).into(),
		(Location::try_from(bridged_weth_at_ah.clone()).unwrap(), amount_to_send).into(),
	]
	.into();
	// use USDT for fees
	let fee = usdt_id;

	// process XCMP `ConcatenatedOpaqueVersionedXcm` negotiation message.
	BridgeHubWestend::execute_with(|| {});
	BridgeHubRococo::execute_with(|| {});

	// use the more involved transfer extrinsic
	let custom_xcm_on_dest = Xcm::<()>(vec![DepositAsset {
		assets: Wild(AllCounted(assets.len() as u32)),
		beneficiary: AccountId32Junction { network: None, id: receiver.clone().into() }.into(),
	}]);
	assert_ok!(AssetHubRococo::execute_with(|| {
		<AssetHubRococo as AssetHubRococoPallet>::PolkadotXcm::transfer_assets_using_type_and_then(
			<AssetHubRococo as Chain>::RuntimeOrigin::signed(sender.into()),
			bx!(asset_hub_westend_location().into()),
			bx!(assets.into()),
			bx!(TransferType::DestinationReserve),
			bx!(fee.into()),
			bx!(TransferType::DestinationReserve),
			bx!(VersionedXcm::from(custom_xcm_on_dest)),
			WeightLimit::Unlimited,
		)
	}));
	// verify hops (also advances the message through the hops)
	assert_bridge_hub_rococo_message_accepted(true);
	assert_bridge_hub_westend_message_received();
	AssetHubWestend::execute_with(|| {
		AssetHubWestend::assert_xcmp_queue_success(None);
	});

	let receiver_usdts_after = AssetHubWestend::execute_with(|| {
		type Assets = <AssetHubWestend as AssetHubWestendPallet>::Assets;
		<Assets as Inspect<_>>::balance(USDT_ID, &receiver)
	});
	let receiver_weth_after = foreign_balance_on_ah_westend(bridged_weth_at_ah, &receiver);

	// Receiver's USDT balance is increased by almost `amount_to_send` (minus fees)
	assert!(receiver_usdts_after > receiver_usdts_before);
	assert!(receiver_usdts_after < receiver_usdts_before + amount_to_send);
	// Receiver's wETH balance is increased by `amount_to_send`
	assert_eq!(receiver_weth_after, receiver_weth_before + amount_to_send);
}

#[test]
fn send_rocs_from_penpal_rococo_through_asset_hub_rococo_to_asset_hub_westend() {
	let amount = ASSET_HUB_ROCOCO_ED * 10_000_000;
	let sender = PenpalASender::get();
	let receiver = AssetHubWestendReceiver::get();
	let local_asset_hub = PenpalA::sibling_location_of(AssetHubRococo::para_id());
	let (roc_at_rococo_parachains, roc_at_asset_hub_westend) =
		set_up_rocs_for_penpal_rococo_through_ahr_to_ahw(&sender, amount);
	set_up_pool_with_wnd_on_ah_westend(roc_at_asset_hub_westend.clone(), true);

	let sov_ahw_on_ahr = AssetHubRococo::sovereign_account_of_parachain_on_other_global_consensus(
		ByGenesis(WESTEND_GENESIS_HASH),
		AssetHubWestend::para_id(),
	);
	let rocs_in_reserve_on_ahr_before =
		<AssetHubRococo as Chain>::account_data_of(sov_ahw_on_ahr.clone()).free;
	let sender_rocs_before = assets_balance_on!(PenpalA, roc_at_rococo_parachains.clone(), &sender);
	let receiver_rocs_before =
		foreign_balance_on_ah_westend(roc_at_asset_hub_westend.clone(), &receiver);

	// Send ROCs over bridge
	{
		let destination = asset_hub_westend_location();
		let assets: Assets = (roc_at_rococo_parachains.clone(), amount).into();
		let asset_transfer_type = TransferType::RemoteReserve(local_asset_hub.clone().into());
		let fees_id: AssetId = roc_at_rococo_parachains.clone().into();
		let fees_transfer_type = TransferType::RemoteReserve(local_asset_hub.into());
		let beneficiary: Location =
			AccountId32Junction { network: None, id: receiver.clone().into() }.into();
		let custom_xcm_on_dest = Xcm::<()>(vec![DepositAsset {
			assets: Wild(AllCounted(assets.len() as u32)),
			beneficiary,
		}]);
		send_assets_from_penpal_rococo_through_rococo_ah_to_westend_ah(
			destination,
			(assets, asset_transfer_type),
			(fees_id, fees_transfer_type),
			custom_xcm_on_dest,
		);
	}

	// process AHW incoming message and check events
	let roc = Location::new(2, [GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH))]);
	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![
				// issue ROCs on AHW
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { asset_id, who, .. }) => {
					asset_id: *asset_id == roc,
					who: who == &receiver,
				},
				// message processed successfully
				RuntimeEvent::MessageQueue(
					pallet_message_queue::Event::Processed { success: true, .. }
				) => {},
			]
		);
	});

	let sender_rocs_after = assets_balance_on!(PenpalA, roc_at_rococo_parachains, &sender);
	let receiver_rocs_after = foreign_balance_on_ah_westend(roc_at_asset_hub_westend, &receiver);
	let rocs_in_reserve_on_ahr_after =
		<AssetHubRococo as Chain>::account_data_of(sov_ahw_on_ahr.clone()).free;

	// Sender's balance is reduced
	assert!(sender_rocs_after < sender_rocs_before);
	// Receiver's balance is increased
	assert!(receiver_rocs_after > receiver_rocs_before);
	// Reserve balance is increased by sent amount (less fess)
	assert!(rocs_in_reserve_on_ahr_after > rocs_in_reserve_on_ahr_before);
	assert!(rocs_in_reserve_on_ahr_after <= rocs_in_reserve_on_ahr_before + amount);
}

#[test]
fn send_back_wnds_from_penpal_rococo_through_asset_hub_rococo_to_asset_hub_westend() {
	let wnd_at_rococo_parachains = bridged_wnd_at_ah_rococo();
	let amount = ASSET_HUB_ROCOCO_ED * 10_000_000;
	let sender = PenpalASender::get();
	let receiver = AssetHubWestendReceiver::get();

	// set up ROCs for transfer
	let (roc_at_rococo_parachains, _) =
		set_up_rocs_for_penpal_rococo_through_ahr_to_ahw(&sender, amount);

	// set up WNDs for transfer
	let penpal_location = AssetHubRococo::sibling_location_of(PenpalA::para_id());
	let sov_penpal_on_ahr = AssetHubRococo::sovereign_account_id_of(penpal_location);
	let prefund_accounts = vec![(sov_penpal_on_ahr, amount * 2)];
	let reserves = vec![(asset_hub_westend_location(), false).into()];
	create_foreign_on_ah_rococo(wnd_at_rococo_parachains.clone(), true, reserves, prefund_accounts);
	let asset_owner: AccountId = AssetHubRococo::account_id_of(ALICE);
	PenpalA::force_create_foreign_asset(
		wnd_at_rococo_parachains.clone(),
		asset_owner.clone(),
		true,
		ASSET_MIN_BALANCE,
		vec![(sender.clone(), amount * 2)],
	);
	// Configure source Penpal chain to trust local AH as reserve of bridged WND
	PenpalA::execute_with(|| {
		assert_ok!(<PenpalA as Chain>::System::set_storage(
			<PenpalA as Chain>::RuntimeOrigin::root(),
			vec![(
				PenpalCustomizableAssetFromSystemAssetHub::key().to_vec(),
				wnd_at_rococo_parachains.encode(),
			)],
		));
	});

	// fund the AHR's SA on AHW with the WND tokens held in reserve
	let sov_ahr_on_ahw = AssetHubWestend::sovereign_account_of_parachain_on_other_global_consensus(
		NetworkId::ByGenesis(ROCOCO_GENESIS_HASH),
		AssetHubRococo::para_id(),
	);
	AssetHubWestend::fund_accounts(vec![(sov_ahr_on_ahw.clone(), amount * 2)]);

	// balances before
	let sender_wnds_before =
		assets_balance_on!(PenpalA, wnd_at_rococo_parachains.clone().into(), &sender);
	let receiver_wnds_before = <AssetHubWestend as Chain>::account_data_of(receiver.clone()).free;

	// send WNDs over the bridge, ROCs only used to pay fees on local AH, pay with WND on remote AH
	{
		let final_destination = asset_hub_westend_location();
		let intermediary_hop = PenpalA::sibling_location_of(AssetHubRococo::para_id());
		let context = PenpalA::execute_with(|| PenpalUniversalLocation::get());

		// what happens at final destination
		let beneficiary = AccountId32Junction { network: None, id: receiver.clone().into() }.into();
		// use WND as fees on the final destination (AHW)
		let remote_fees: Asset = (wnd_at_rococo_parachains.clone(), amount).into();
		let remote_fees = remote_fees.reanchored(&final_destination, &context).unwrap();
		// buy execution using WNDs, then deposit all remaining WNDs
		let xcm_on_final_dest = Xcm::<()>(vec![
			BuyExecution { fees: remote_fees, weight_limit: WeightLimit::Unlimited },
			DepositAsset { assets: Wild(AllCounted(1)), beneficiary },
		]);

		// what happens at intermediary hop
		// reanchor final dest (Asset Hub Westend) to the view of hop (Asset Hub Rococo)
		let mut final_destination = final_destination.clone();
		final_destination.reanchor(&intermediary_hop, &context).unwrap();
		// reanchor WNDs to the view of hop (Asset Hub Rococo)
		let asset: Asset = (wnd_at_rococo_parachains.clone(), amount).into();
		let asset = asset.reanchored(&intermediary_hop, &context).unwrap();
		// on Asset Hub Rococo, forward a request to withdraw WNDs from reserve on Asset Hub Westend
		let xcm_on_hop = Xcm::<()>(vec![InitiateReserveWithdraw {
			assets: Definite(asset.into()), // WNDs
			reserve: final_destination,     // AHW
			xcm: xcm_on_final_dest,         // XCM to execute on AHW
		}]);
		// assets to send from Penpal and how they reach the intermediary hop
		let assets: Assets = vec![
			(wnd_at_rococo_parachains.clone(), amount).into(),
			(roc_at_rococo_parachains.clone(), amount).into(),
		]
		.into();
		let asset_transfer_type = TransferType::DestinationReserve;
		let fees_id: AssetId = roc_at_rococo_parachains.into();
		let fees_transfer_type = TransferType::DestinationReserve;

		// initiate the transfer
		send_assets_from_penpal_rococo_through_rococo_ah_to_westend_ah(
			intermediary_hop,
			(assets, asset_transfer_type),
			(fees_id, fees_transfer_type),
			xcm_on_hop,
		);
	}

	// process AHW incoming message and check events
	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![
				// issue WNDs on AHW
				RuntimeEvent::Balances(pallet_balances::Event::Deposit { .. }) => {},
				// message processed successfully
				RuntimeEvent::MessageQueue(
					pallet_message_queue::Event::Processed { success: true, .. }
				) => {},
			]
		);
	});

	let sender_wnds_after = assets_balance_on!(PenpalA, wnd_at_rococo_parachains.into(), &sender);
	let receiver_wnds_after = <AssetHubWestend as Chain>::account_data_of(receiver).free;

	// Sender's balance is reduced by sent "amount"
	assert_eq!(sender_wnds_after, sender_wnds_before - amount);
	// Receiver's balance is increased by no more than "amount"
	assert!(receiver_wnds_after > receiver_wnds_before);
	assert!(receiver_wnds_after <= receiver_wnds_before + amount);
}

#[test]
fn dry_run_transfer_to_westend_sends_xcm_to_bridge_hub() {
	test_dry_run_transfer_across_pk_bridge!(
		AssetHubRococo,
		BridgeHubRococo,
		asset_hub_westend_location()
	);
}
