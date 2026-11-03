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

//! Tests to ensure correct XCM fee estimation for cross-chain asset transfers.

use crate::{assets_balance_on, imports::*};
use frame_support::{dispatch::RawOrigin, traits::fungible};
use xcm_runtime_apis::{
	dry_run::runtime_decl_for_dry_run_api::DryRunApiV2,
	fees::runtime_decl_for_xcm_payment_api::XcmPaymentApiV2,
};

fn usdt_transfer_call(
	destination: Location,
	beneficiary: Location,
	amount_to_send: u128,
	usdt_location_on_penpal: Location,
	usdt_location_on_ah: Location,
) -> <PenpalA as Chain>::RuntimeCall {
	let asset_hub_location: Location = PenpalA::sibling_location_of(AssetHubWestend::para_id());

	// Create the XCM to transfer USDT to PenpalB via Asset Hub using InitiateTransfer
	let remote_xcm_on_penpal_b =
		Xcm::<()>(vec![DepositAsset { assets: Wild(AllCounted(1)), beneficiary }]);

	let xcm_on_asset_hub = Xcm::<()>(vec![InitiateTransfer {
		destination,
		remote_fees: Some(AssetTransferFilter::ReserveDeposit(
			Definite((usdt_location_on_ah, 1_000_000u128).into()), // 1 USDT for fees
		)),
		preserve_origin: false,
		assets: BoundedVec::truncate_from(vec![AssetTransferFilter::ReserveDeposit(Wild(All))]),
		remote_xcm: remote_xcm_on_penpal_b,
	}]);

	let xcm = Xcm::<<PenpalA as Chain>::RuntimeCall>(vec![
		WithdrawAsset((usdt_location_on_penpal.clone(), amount_to_send).into()),
		PayFees {
			asset: Asset {
				id: AssetId(usdt_location_on_penpal.clone()),
				fun: Fungible(1_000_000u128), // 1 USDT for local fees
			},
		},
		InitiateTransfer {
			destination: asset_hub_location,
			remote_fees: Some(AssetTransferFilter::ReserveWithdraw(
				Definite((usdt_location_on_penpal.clone(), 1_000_000u128).into()), /* 1 USDT for
				                                                                    * Asset Hub fees */
			)),
			preserve_origin: false,
			assets: BoundedVec::truncate_from(vec![AssetTransferFilter::ReserveWithdraw(Wild(
				All,
			))]),
			remote_xcm: xcm_on_asset_hub,
		},
	]);

	<PenpalA as Chain>::RuntimeCall::PolkadotXcm(pallet_xcm::Call::execute {
		message: bx!(VersionedXcm::from(xcm)),
		max_weight: Weight::MAX,
	})
}

fn sender_assertions(test: ParaToParaThroughAHTest) {
	type RuntimeEvent = <PenpalA as Chain>::RuntimeEvent;
	PenpalA::assert_xcm_pallet_attempted_complete(None);

	assert_expected_events!(
		PenpalA,
		vec![
			RuntimeEvent::Assets(
				pallet_assets::Event::Withdrawn { asset_id, who, amount }
			) => {
				asset_id: *asset_id == Location::new(1, []),
				who: *who == test.sender.account_id,
				amount: *amount == test.args.amount,
			},
		]
	);
}

fn hop_assertions(test: ParaToParaThroughAHTest) {
	type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
	AssetHubWestend::assert_xcmp_queue_success(None);

	assert_expected_events!(
		AssetHubWestend,
		vec![
			RuntimeEvent::Balances(
				pallet_balances::Event::Withdraw { amount, .. }
			) => {
				// Depends on: XCM execution fees + delivery fees + liquidity pool
				// swap overhead at the destination.
				amount: *amount >= test.args.amount * 80/100,
			},
		]
	);
}

fn receiver_assertions(test: ParaToParaThroughAHTest) {
	type RuntimeEvent = <PenpalB as Chain>::RuntimeEvent;
	PenpalB::assert_xcmp_queue_success(None);

	assert_expected_events!(
		PenpalB,
		vec![
			RuntimeEvent::Assets(
				pallet_assets::Event::Deposited { asset_id, who, .. }
			) => {
				asset_id: *asset_id == Location::new(1, []),
				who: *who == test.receiver.account_id,
			},
		]
	);
}

fn transfer_assets_para_to_para_through_ah_call(
	test: ParaToParaThroughAHTest,
) -> <PenpalA as Chain>::RuntimeCall {
	type RuntimeCall = <PenpalA as Chain>::RuntimeCall;

	let asset_hub_location: Location = PenpalA::sibling_location_of(AssetHubWestend::para_id());
	let custom_xcm_on_dest = Xcm::<()>(vec![DepositAsset {
		assets: Wild(AllCounted(test.args.assets.len() as u32)),
		beneficiary: test.args.beneficiary,
	}]);
	let remote_fee_id: AssetId = test
		.args
		.assets
		.clone()
		.into_inner()
		.get(test.args.fee_asset_item as usize)
		.expect("asset in index fee_asset_item should exist")
		.clone()
		.id;
	RuntimeCall::PolkadotXcm(pallet_xcm::Call::transfer_assets_using_type_and_then {
		dest: bx!(test.args.dest.into()),
		assets: bx!(test.args.assets.clone().into()),
		assets_transfer_type: bx!(TransferType::RemoteReserve(asset_hub_location.clone().into())),
		remote_fees_id: bx!(VersionedAssetId::from(remote_fee_id)),
		fees_transfer_type: bx!(TransferType::RemoteReserve(asset_hub_location.into())),
		custom_xcm_on_dest: bx!(VersionedXcm::from(custom_xcm_on_dest)),
		weight_limit: test.args.weight_limit,
	})
}

/// We are able to dry-run and estimate the fees for a multi-hop XCM journey.
/// Scenario: Alice on PenpalA has some WND and wants to send them to PenpalB.
/// We want to know the fees using the `DryRunApi` and `XcmPaymentApi`.
#[test]
fn multi_hop_works() {
	let destination = PenpalA::sibling_location_of(PenpalB::para_id());
	let sender = PenpalASender::get();
	let amount_to_send = 1_000_000_000_000;
	let asset_owner = PenpalAssetOwner::get();
	let relay_native_assets: Assets = (Parent, amount_to_send).into();
	let relay_native_asset_location = Location::parent();
	let sender_as_seen_by_ah = AssetHubWestend::sibling_location_of(PenpalA::para_id());
	let sov_of_sender_on_ah =
		AssetHubWestend::sovereign_account_id_of(sender_as_seen_by_ah.clone());

	// fund Parachain's sender account
	PenpalA::mint_foreign_asset(
		<PenpalA as Chain>::RuntimeOrigin::signed(asset_owner.clone()),
		relay_native_asset_location.clone(),
		sender.clone(),
		amount_to_send * 2,
	);

	// fund the Parachain Origin's SA on AssetHub with the native tokens held in reserve.
	AssetHubWestend::fund_accounts(vec![(sov_of_sender_on_ah.clone(), amount_to_send * 2)]);

	// Init values for Parachain Destination
	let beneficiary_id = PenpalBReceiver::get();

	let test_args = TestContext {
		sender: PenpalASender::get(),     // Bob in PenpalB.
		receiver: PenpalBReceiver::get(), // Alice.
		args: TestArgs::new_para(
			destination,
			beneficiary_id.clone(),
			amount_to_send,
			relay_native_assets,
			None,
			0,
		),
	};
	let mut test = ParaToParaThroughAHTest::new(test_args);

	// We get them from the PenpalA closure.
	let mut delivery_fees_amount = 0;
	let mut remote_message = VersionedXcm::from(Xcm(Vec::new()));
	<PenpalA as TestExt>::execute_with(|| {
		type Runtime = <PenpalA as Chain>::Runtime;
		type OriginCaller = <PenpalA as Chain>::OriginCaller;

		let call = transfer_assets_para_to_para_through_ah_call(test.clone());
		let origin = OriginCaller::system(RawOrigin::Signed(sender.clone()));
		let result = Runtime::dry_run_call(origin, call, xcm::prelude::XCM_VERSION).unwrap();
		// We filter the result to get only the messages we are interested in.
		let (destination_to_query, messages_to_query) = &result
			.forwarded_xcms
			.iter()
			.find(|(destination, _)| {
				*destination == VersionedLocation::from(Location::new(1, [Parachain(1000)]))
			})
			.unwrap();
		assert_eq!(messages_to_query.len(), 1);
		remote_message = messages_to_query[0].clone();
		let asset_id_for_delivery_fees = VersionedAssetId::from(Location::here());
		let delivery_fees = Runtime::query_delivery_fees(
			destination_to_query.clone(),
			remote_message.clone(),
			asset_id_for_delivery_fees,
		)
		.unwrap();
		delivery_fees_amount = get_amount_from_versioned_assets(delivery_fees);
	});

	// These are set in the AssetHub closure.
	let mut intermediate_execution_fees = 0;
	let mut intermediate_delivery_fees_amount = 0;
	let mut intermediate_remote_message = VersionedXcm::from(Xcm::<()>(Vec::new()));
	<AssetHubWestend as TestExt>::execute_with(|| {
		type Runtime = <AssetHubWestend as Chain>::Runtime;
		type RuntimeCall = <AssetHubWestend as Chain>::RuntimeCall;

		// First we get the execution fees.
		let weight = Runtime::query_xcm_weight(remote_message.clone()).unwrap();
		intermediate_execution_fees =
			Runtime::query_weight_to_asset_fee(weight, VersionedAssetId::from(Location::parent()))
				.unwrap();

		// We have to do this to turn `VersionedXcm<()>` into `VersionedXcm<RuntimeCall>`.
		let xcm_program = VersionedXcm::from(Xcm::<RuntimeCall>::from(
			remote_message.clone().try_into().unwrap(),
		));

		// Now we get the delivery fees to the final destination.
		let result =
			Runtime::dry_run_xcm(sender_as_seen_by_ah.clone().into(), xcm_program).unwrap();
		let (destination_to_query, messages_to_query) = &result
			.forwarded_xcms
			.iter()
			.find(|(destination, _)| {
				*destination == VersionedLocation::from(Location::new(1, [Parachain(2001)]))
			})
			.unwrap();
		// There's actually two messages here.
		// One created when the message we sent from PenpalA arrived and was executed.
		// The second one when we dry-run the xcm.
		// We could've gotten the message from the queue without having to dry-run, but
		// offchain applications would have to dry-run, so we do it here as well.
		intermediate_remote_message = messages_to_query[0].clone();
		let asset_id_for_delivery_fees = VersionedAssetId::from(Location::parent());
		let delivery_fees = Runtime::query_delivery_fees(
			destination_to_query.clone(),
			intermediate_remote_message.clone(),
			asset_id_for_delivery_fees,
		)
		.unwrap();
		intermediate_delivery_fees_amount = get_amount_from_versioned_assets(delivery_fees);
	});

	// Dry-running is done.
	PenpalA::reset_ext();
	AssetHubWestend::reset_ext();
	PenpalB::reset_ext();

	// Fund accounts again.
	PenpalA::mint_foreign_asset(
		<PenpalA as Chain>::RuntimeOrigin::signed(asset_owner),
		relay_native_asset_location.clone(),
		sender.clone(),
		amount_to_send * 2,
	);

	// Get the final execution fees at the destination.
	//
	// Note: We need to do this after resetting the externalities to get an accurate value here.
	// This is because the dry-run on Asset Hub does affect the liquidity pool distribution on
	// PenpalB which affects the assets amount we have to pay.
	// See side-effects: https://github.com/paritytech/polkadot-sdk/issues/11486.
	let mut final_execution_fees = 0;
	<PenpalB as TestExt>::execute_with(|| {
		type Runtime = <PenpalB as Chain>::Runtime;

		let weight = Runtime::query_xcm_weight(intermediate_remote_message.clone()).unwrap();
		final_execution_fees =
			Runtime::query_weight_to_asset_fee(weight, VersionedAssetId::from(Location::parent()))
				.unwrap();
	});

	AssetHubWestend::fund_accounts(vec![(sov_of_sender_on_ah, amount_to_send * 2)]);

	// Actually run the extrinsic.
	let sender_assets_before =
		assets_balance_on!(PenpalA, relay_native_asset_location.clone(), &sender);
	let sender_balance_before = PenpalA::execute_with(|| {
		type Balances = <PenpalA as PenpalAPallet>::Balances;
		<Balances as fungible::Inspect<_>>::balance(&sender)
	});
	let receiver_assets_before =
		assets_balance_on!(PenpalB, relay_native_asset_location.clone(), &beneficiary_id);
	let receiver_balance_before = PenpalB::execute_with(|| {
		type Balances = <PenpalB as PenpalBPallet>::Balances;
		<Balances as fungible::Inspect<_>>::balance(&beneficiary_id)
	});

	test.set_assertion::<PenpalA>(sender_assertions);
	test.set_assertion::<AssetHubWestend>(hop_assertions);
	test.set_assertion::<PenpalB>(receiver_assertions);
	let call = transfer_assets_para_to_para_through_ah_call(test.clone());
	test.set_call(call);
	test.assert();

	let sender_assets_after =
		assets_balance_on!(PenpalA, relay_native_asset_location.clone(), &sender);
	let sender_balance_after = PenpalA::execute_with(|| {
		type Balances = <PenpalA as PenpalAPallet>::Balances;
		<Balances as fungible::Inspect<_>>::balance(&sender)
	});
	let receiver_assets_after =
		assets_balance_on!(PenpalB, relay_native_asset_location, &beneficiary_id);
	let receiver_balance_after = PenpalB::execute_with(|| {
		type Balances = <PenpalB as PenpalBPallet>::Balances;
		<Balances as fungible::Inspect<_>>::balance(&beneficiary_id)
	});

	// We know the exact fees on every hop.
	assert_eq!(sender_assets_after, sender_assets_before - amount_to_send);
	assert_eq!(
		sender_balance_after,
		// This is charged directly from the sender's account.
		sender_balance_before - delivery_fees_amount
	);

	// Receiver native balance is unchanged; received funds in relay asset, and paid in relay asset.
	assert_eq!(receiver_balance_after, receiver_balance_before);
	assert_eq!(
		receiver_assets_after,
		receiver_assets_before + amount_to_send -
			intermediate_execution_fees -
			intermediate_delivery_fees_amount -
			final_execution_fees
	);
}

#[test]
fn multi_hop_pay_fees_works() {
	test_can_estimate_and_pay_exact_fees!(
		PenpalA,
		AssetHubWestend,
		PenpalB,
		(Parent, 1_000_000_000_000u128),
		Penpal
	);
}

/// We are able to estimate delivery fees in USDT for a USDT transfer from PenpalA to PenpalB via
/// Asset Hub. Scenario: Alice on PenpalA has some USDT and wants to send them to PenpalB.
/// We want to estimate the delivery fees in USDT using the new `asset_id` parameter in
/// `query_delivery_fees`.
#[test]
fn usdt_fee_estimation_in_usdt_works() {
	use emulated_integration_tests_common::USDT_ID;

	let destination = PenpalA::sibling_location_of(PenpalB::para_id());
	let sender = PenpalASender::get();
	let amount_to_send = 10_000_000; // 10 USDT (6 decimals)

	// USDT location from PenpalA's perspective
	let usdt_location_on_penpal = PenpalUsdtFromAssetHub::get();

	// USDT location from Asset Hub's perspective
	let usdt_location_on_ah =
		Location::new(0, [PalletInstance(ASSETS_PALLET_ID), GeneralIndex(USDT_ID.into())]);

	let penpal_as_seen_by_ah = AssetHubWestend::sibling_location_of(PenpalA::para_id());
	let sov_of_penpal_on_ah =
		AssetHubWestend::sovereign_account_id_of(penpal_as_seen_by_ah.clone());

	// fund PenpalA's sender account with USDT
	PenpalA::mint_foreign_asset(
		<PenpalA as Chain>::RuntimeOrigin::signed(PenpalAssetOwner::get()),
		usdt_location_on_penpal.clone(),
		sender.clone(),
		amount_to_send * 2,
	);

	// fund PenpalA's sovereign account on AssetHub with USDT
	AssetHubWestend::mint_asset(
		<AssetHubWestend as Chain>::RuntimeOrigin::signed(AssetHubWestendAssetOwner::get()),
		USDT_ID,
		sov_of_penpal_on_ah.clone(),
		amount_to_send * 2,
	);

	// Create a liquidity pool between WND (relay token) and USDT on AssetHub
	// This is needed for the asset conversion in fee estimation
	create_pool_with_relay_native_on!(
		AssetHubWestend,
		usdt_location_on_ah.clone(),
		AssetHubWestendSender::get(),
		1_000_000_000_000, // 1 WND
		2_000_000          // 2 USDT (1:2 ratio)
	);

	// Create a liquidity pool between WND and USDT on PenpalA as well
	// This is needed for PenpalA to perform asset conversion for fee estimation
	create_foreign_pool_with_native_on!(
		PenpalA,
		usdt_location_on_penpal.clone(),
		PenpalAssetOwner::get(),
		1_000_000_000_000, // 1 WND
		2_000_000          // 2 USDT (1:2 ratio)
	);

	let beneficiary_id = PenpalBReceiver::get();

	// We get the delivery fees from the PenpalA closure.
	let mut delivery_fees_amount = 0;
	let mut remote_message = VersionedXcm::from(Xcm(Vec::new()));
	<PenpalA as TestExt>::execute_with(|| {
		type Runtime = <PenpalA as Chain>::Runtime;
		type OriginCaller = <PenpalA as Chain>::OriginCaller;

		let call = usdt_transfer_call(
			destination.clone(),
			beneficiary_id.clone().into(),
			amount_to_send,
			usdt_location_on_penpal.clone(),
			usdt_location_on_ah.clone(),
		);

		let asset_hub_location: Location = PenpalA::sibling_location_of(AssetHubWestend::para_id());

		let origin = OriginCaller::system(RawOrigin::Signed(sender.clone()));
		let result = Runtime::dry_run_call(origin, call, xcm::prelude::XCM_VERSION).unwrap();

		// Find the message sent to Asset Hub
		let (destination_to_query, messages_to_query) = &result
			.forwarded_xcms
			.iter()
			.find(|(destination, _)| {
				*destination == VersionedLocation::from(asset_hub_location.clone())
			})
			.unwrap();

		assert_eq!(messages_to_query.len(), 1);
		remote_message = messages_to_query[0].clone();

		// Query delivery fees in USDT using the new asset_id parameter
		let usdt_asset_id = VersionedAssetId::from(AssetId(usdt_location_on_penpal.clone()));
		let delivery_fees = Runtime::query_delivery_fees(
			destination_to_query.clone(),
			remote_message.clone(),
			usdt_asset_id,
		)
		.unwrap();

		delivery_fees_amount = get_amount_from_versioned_assets(delivery_fees.clone());

		// Verify the fees are quoted in USDT (the delivery fees should be converted from native to
		// USDT)
		let fee_assets = match delivery_fees {
			VersionedAssets::V5(assets) => assets,
			_ => panic!("Expected V5 assets"),
		};

		// Should have one asset (USDT)
		assert_eq!(fee_assets.len(), 1);
		let fee_asset = fee_assets.get(0).unwrap();

		// Verify it's USDT
		assert_eq!(fee_asset.id.0, usdt_location_on_penpal);

		// Verify we get a reasonable USDT amount (delivery fees should be > 0)
		if let Fungible(amount) = fee_asset.fun {
			assert!(amount > 0, "Delivery fees should be greater than 0");
		} else {
			panic!("Expected fungible delivery fees");
		}
	});
}
