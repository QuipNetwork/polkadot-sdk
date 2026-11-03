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

use crate::{assets_balance_on, foreign_balance_on, foreign_issuance_on, imports::*};

fn relay_origin_assertions(t: RelayToSystemParaTest) {
	type RuntimeEvent = <Westend as Chain>::RuntimeEvent;
	Westend::assert_xcm_pallet_attempted_complete(None);
	assert_expected_events!(
		Westend,
		vec![
			// Amount to teleport is withdrawn from Sender
			RuntimeEvent::Balances(pallet_balances::Event::Withdraw { who, amount }) => {
				who: *who == t.sender.account_id,
				amount: *amount == t.args.amount,
			},
		]
	);
}

fn penpal_to_ah_foreign_assets_sender_assertions(t: ParaToSystemParaTest) {
	type RuntimeEvent = <PenpalA as Chain>::RuntimeEvent;
	let system_para_native_asset_location = RelayLocation::get();
	let expected_asset_id = t.args.asset_id.unwrap();
	let (_, expected_asset_amount) =
		non_fee_asset(&t.args.assets, t.args.fee_asset_item as usize).unwrap();

	PenpalA::assert_xcm_pallet_attempted_complete(None);
	assert_expected_events!(
		PenpalA,
		vec![
			RuntimeEvent::Assets(
				pallet_assets::Event::Withdrawn { asset_id, who, .. }
			) => {
				asset_id: *asset_id == system_para_native_asset_location,
				who: *who == t.sender.account_id,
			},
			RuntimeEvent::Assets(pallet_assets::Event::Withdrawn { asset_id, who, amount }) => {
				asset_id: *asset_id == expected_asset_id,
				who: *who == t.sender.account_id,
				amount: *amount == expected_asset_amount,
			},
		]
	);
}

fn penpal_to_ah_foreign_assets_receiver_assertions(t: ParaToSystemParaTest) {
	type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
	let sov_penpal_on_ahr = AssetHubWestend::sovereign_account_id_of(
		AssetHubWestend::sibling_location_of(PenpalA::para_id()),
	);
	let (_, expected_foreign_asset_amount) =
		non_fee_asset(&t.args.assets, t.args.fee_asset_item as usize).unwrap();
	let (_, fee_asset_amount) = fee_asset(&t.args.assets, t.args.fee_asset_item as usize).unwrap();

	AssetHubWestend::assert_xcmp_queue_success(None);

	assert_expected_events!(
		AssetHubWestend,
		vec![
			// native asset reserve transfer for paying fees, withdrawn from Penpal's sov account
			RuntimeEvent::Balances(
				pallet_balances::Event::Withdraw { who, amount }
			) => {
				who: *who == sov_penpal_on_ahr.clone().into(),
				amount: *amount >= fee_asset_amount / 2,
			},
			RuntimeEvent::Balances(pallet_balances::Event::Deposit { who, .. }) => {
				who: *who == t.receiver.account_id,
			},
			RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { asset_id, who, amount }) => {
				asset_id: *asset_id == PenpalAPen2TeleportableAssetLocation::get(),
				who: *who == t.receiver.account_id,
				amount: *amount == expected_foreign_asset_amount,
			},
			RuntimeEvent::Balances(pallet_balances::Event::Deposit { .. }) => {},
		]
	);
}

fn ah_to_penpal_foreign_assets_sender_assertions(t: SystemParaToParaTest) {
	type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
	AssetHubWestend::assert_xcm_pallet_attempted_complete(None);
	let (expected_foreign_asset_id, expected_foreign_asset_amount) =
		non_fee_asset(&t.args.assets, t.args.fee_asset_item as usize).unwrap();
	assert_expected_events!(
		AssetHubWestend,
		vec![
			// foreign asset is withdrawn and burned locally as part of teleportation
			RuntimeEvent::ForeignAssets(pallet_assets::Event::Withdrawn { asset_id, who, amount }) => {
				asset_id: *asset_id == expected_foreign_asset_id,
				who: *who == t.sender.account_id,
				amount: *amount == expected_foreign_asset_amount,
			},
		]
	);
}

fn ah_to_penpal_foreign_assets_receiver_assertions(t: SystemParaToParaTest) {
	type RuntimeEvent = <PenpalA as Chain>::RuntimeEvent;
	let expected_asset_id = t.args.asset_id.unwrap();
	let (_, expected_asset_amount) =
		non_fee_asset(&t.args.assets, t.args.fee_asset_item as usize).unwrap();
	let checking_account = <PenpalA as PenpalAPallet>::PolkadotXcm::check_account();
	let system_para_native_asset_location = RelayLocation::get();

	PenpalA::assert_xcmp_queue_success(None);

	assert_expected_events!(
		PenpalA,
		vec![
			// checking account burns local asset as part of incoming teleport
			RuntimeEvent::Assets(pallet_assets::Event::Burned { asset_id, owner, balance }) => {
				asset_id: *asset_id == expected_asset_id,
				owner: *owner == checking_account,
				balance: *balance == expected_asset_amount,
			},
			// local asset is teleported into account of receiver
			RuntimeEvent::Assets(pallet_assets::Event::Deposited { asset_id, who, amount }) => {
				asset_id: *asset_id == expected_asset_id,
				who: *who == t.receiver.account_id,
				amount: *amount == expected_asset_amount,
			},
			// native asset for fee is deposited to receiver
			RuntimeEvent::Assets(pallet_assets::Event::Deposited { asset_id, who, .. }) => {
				asset_id: *asset_id == system_para_native_asset_location,
				who: *who == t.receiver.account_id,
			},
		]
	);
}

fn relay_to_system_para_limited_teleport_assets(t: RelayToSystemParaTest) -> DispatchResult {
	Dmp::make_parachain_reachable(AssetHubWestend::para_id());

	<Westend as WestendPallet>::XcmPallet::limited_teleport_assets(
		t.signed_origin,
		bx!(t.args.dest.into()),
		bx!(t.args.beneficiary.into()),
		bx!(t.args.assets.into()),
		t.args.fee_asset_item,
		t.args.weight_limit,
	)
}

fn para_to_system_para_transfer_assets(t: ParaToSystemParaTest) -> DispatchResult {
	type Runtime = <PenpalA as Chain>::Runtime;
	let remote_fee_id: AssetId = t
		.args
		.assets
		.clone()
		.into_inner()
		.get(t.args.fee_asset_item as usize)
		.ok_or(pallet_xcm::Error::<Runtime>::Empty)?
		.clone()
		.id;

	<PenpalA as PenpalAPallet>::PolkadotXcm::transfer_assets_using_type_and_then(
		t.signed_origin,
		bx!(t.args.dest.into()),
		bx!(t.args.assets.into()),
		bx!(TransferType::Teleport),
		bx!(remote_fee_id.into()),
		bx!(TransferType::DestinationReserve),
		bx!(VersionedXcm::from(
			Xcm::<()>::builder_unsafe()
				.deposit_asset(AllCounted(2), t.args.beneficiary)
				.build()
		)),
		t.args.weight_limit,
	)
}

fn system_para_to_para_transfer_assets(t: SystemParaToParaTest) -> DispatchResult {
	type Runtime = <AssetHubWestend as Chain>::Runtime;
	let remote_fee_id: AssetId = t
		.args
		.assets
		.clone()
		.into_inner()
		.get(t.args.fee_asset_item as usize)
		.ok_or(pallet_xcm::Error::<Runtime>::Empty)?
		.clone()
		.id;

	<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::transfer_assets_using_type_and_then(
		t.signed_origin,
		bx!(t.args.dest.into()),
		bx!(t.args.assets.into()),
		bx!(TransferType::Teleport),
		bx!(remote_fee_id.into()),
		bx!(TransferType::LocalReserve),
		bx!(VersionedXcm::from(
			Xcm::<()>::builder_unsafe()
				.deposit_asset(AllCounted(2), t.args.beneficiary)
				.build()
		)),
		t.args.weight_limit,
	)
}

#[test]
fn teleport_via_limited_teleport_assets_to_other_system_parachains_works() {
	let amount = ASSET_HUB_WESTEND_ED * 100;
	let native_asset: Assets = (Parent, amount).into();

	test_parachain_is_trusted_teleporter!(
		AssetHubWestend,        // Origin
		vec![BridgeHubWestend], // Destinations
		(native_asset, amount),
		limited_teleport_assets
	);
}

#[test]
fn teleport_via_transfer_assets_to_other_system_parachains_works() {
	let amount = ASSET_HUB_WESTEND_ED * 100;
	let native_asset: Assets = (Parent, amount).into();

	test_parachain_is_trusted_teleporter!(
		AssetHubWestend,        // Origin
		vec![BridgeHubWestend], // Destinations
		(native_asset, amount),
		transfer_assets
	);
}

#[test]
fn teleport_via_limited_teleport_assets_from_and_to_relay() {
	let amount = WESTEND_ED * 100;
	let native_asset: Assets = (Here, amount).into();

	test_relay_is_trusted_teleporter!(
		Westend,
		vec![AssetHubWestend],
		(native_asset, amount),
		limited_teleport_assets
	);

	test_parachain_is_trusted_teleporter_for_relay!(
		AssetHubWestend,
		Westend,
		amount,
		limited_teleport_assets
	);
}

#[test]
fn teleport_via_transfer_assets_from_and_to_relay() {
	let amount = WESTEND_ED * 100;
	let native_asset: Assets = (Here, amount).into();

	test_relay_is_trusted_teleporter!(
		Westend,
		vec![AssetHubWestend],
		(native_asset, amount),
		transfer_assets
	);

	test_parachain_is_trusted_teleporter_for_relay!(
		AssetHubWestend,
		Westend,
		amount,
		transfer_assets
	);
}

/// Limited Teleport of native asset from Relay Chain to Asset Hub
/// shouldn't work when there is not enough balance in Asset Hub's `CheckAccount`
#[test]
fn limited_teleport_native_assets_from_relay_to_asset_hub_checking_acc_fails() {
	let check_account = AssetHubWestend::execute_with(|| {
		<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::check_account()
	});
	let amount_to_send_larger_than_checking_acc: Balance =
		AssetHubWestend::account_data_of(check_account).free + 1;
	let destination = Westend::child_location_of(AssetHubWestend::para_id());
	let beneficiary_id = AssetHubWestendReceiver::get().into();
	let assets = (Here, amount_to_send_larger_than_checking_acc).into();

	let test_args = TestContext {
		sender: WestendSender::get(),
		receiver: AssetHubWestendReceiver::get(),
		args: TestArgs::new_para(
			destination,
			beneficiary_id,
			amount_to_send_larger_than_checking_acc,
			assets,
			None,
			0,
		),
	};

	let mut test = RelayToSystemParaTest::new(test_args);

	let sender_balance_before = test.sender.balance;
	let receiver_balance_before = test.receiver.balance;

	fn para_dest_assertions_fails(_t: RelayToSystemParaTest) {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::MessageQueue(
					pallet_message_queue::Event::Processed { success: false, .. }
				) => {},
			]
		);
	}

	test.set_assertion::<AssetHubWestend>(para_dest_assertions_fails);
	test.set_assertion::<Westend>(relay_origin_assertions);
	test.set_dispatchable::<Westend>(relay_to_system_para_limited_teleport_assets);
	test.assert();

	let sender_balance_after = test.sender.balance;
	let receiver_balance_after = test.receiver.balance;

	let delivery_fees = Westend::execute_with(|| {
		xcm_helpers::teleport_assets_delivery_fees::<
			<WestendXcmConfig as xcm_executor::Config>::XcmSender,
		>(
			test.args.assets.clone(), 0, test.args.weight_limit, test.args.beneficiary, test.args.dest
		)
	});

	// Sender's balance is reduced
	assert_eq!(
		sender_balance_before - amount_to_send_larger_than_checking_acc - delivery_fees,
		sender_balance_after
	);
	// Receiver's balance does not change
	assert_eq!(receiver_balance_after, receiver_balance_before);
}

/// Checking account should correctly account for incoming teleports.
#[test]
fn limited_teleport_native_assets_from_relay_to_asset_hub_checking_acc_burn_works() {
	// Init values for Relay Chain
	let amount_to_send: Balance = ASSET_HUB_WESTEND_ED * 1000;
	let destination = Westend::child_location_of(AssetHubWestend::para_id());
	let beneficiary_id = AssetHubWestendReceiver::get().into();
	let assets = (Here, amount_to_send).into();

	let test_args = TestContext {
		sender: WestendSender::get(),
		receiver: AssetHubWestendReceiver::get(),
		args: TestArgs::new_para(destination, beneficiary_id, amount_to_send, assets, None, 0),
	};

	let mut test = RelayToSystemParaTest::new(test_args);

	let sender_balance_before = test.sender.balance;
	let receiver_balance_before = test.receiver.balance;

	fn para_dest_assertions_works(t: RelayToSystemParaTest) {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![
				// Amount to teleport is burned from Asset Hub's `CheckAccount`
				RuntimeEvent::Balances(pallet_balances::Event::Burned { who, amount }) => {
					who: *who == <AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::check_account(),
					amount:  *amount == t.args.amount,
				},
				RuntimeEvent::Balances(pallet_balances::Event::Deposit { who, .. }) => {
					who: *who == t.receiver.account_id,
				},
				RuntimeEvent::MessageQueue(
					pallet_message_queue::Event::Processed { success: true, .. }
				) => {},
			]
		);
	}

	test.set_assertion::<AssetHubWestend>(para_dest_assertions_works);
	test.set_assertion::<Westend>(relay_origin_assertions);
	test.set_dispatchable::<Westend>(relay_to_system_para_limited_teleport_assets);
	test.assert();

	let sender_balance_after = test.sender.balance;
	let receiver_balance_after = test.receiver.balance;

	let delivery_fees = Westend::execute_with(|| {
		xcm_helpers::teleport_assets_delivery_fees::<
			<WestendXcmConfig as xcm_executor::Config>::XcmSender,
		>(
			test.args.assets.clone(), 0, test.args.weight_limit, test.args.beneficiary, test.args.dest
		)
	});

	// Sender's balance is reduced
	assert_eq!(sender_balance_before - amount_to_send - delivery_fees, sender_balance_after);
	// Receiver's asset balance is increased
	assert!(receiver_balance_after > receiver_balance_before);
	// Receiver's asset balance increased by `amount_to_send - delivery_fees - bought_execution`;
	// `delivery_fees` might be paid from transfer or JIT, also `bought_execution` is unknown but
	// should be non-zero
	assert!(receiver_balance_after < receiver_balance_before + amount_to_send);
}

/// Checking account should correctly account for outgoing teleports.
#[test]
fn limited_teleport_native_assets_from_asset_hub_to_relay_checking_acc_mint_works() {
	// Init values for Relay Chain
	let amount_to_send: Balance = ASSET_HUB_WESTEND_ED * 1000;
	let destination = AssetHubWestend::parent_location().into();
	let beneficiary_id = WestendReceiver::get().into();
	let assets = (Parent, amount_to_send).into();

	let test_args = TestContext {
		sender: AssetHubWestendSender::get(),
		receiver: WestendReceiver::get(),
		args: TestArgs::new_para(destination, beneficiary_id, amount_to_send, assets, None, 0),
	};

	let mut test = SystemParaToRelayTest::new(test_args);

	let sender_balance_before = test.sender.balance;
	let receiver_balance_before = test.receiver.balance;

	fn para_origin_assertions(t: SystemParaToRelayTest) {
		AssetHubWestend::assert_xcm_pallet_attempted_complete(None);

		AssetHubWestend::assert_parachain_system_ump_sent();

		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::Balances(
					pallet_balances::Event::Withdraw { who, amount }
				) => {
					who: *who == t.sender.account_id,
					amount: *amount == t.args.amount,
				},
				RuntimeEvent::Balances(pallet_balances::Event::Minted { who, amount }) => {
					who: *who == <AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::check_account(),
					amount:  *amount == t.args.amount,
				},
			]
		);
	}

	fn relay_dest_assertions(t: SystemParaToRelayTest) {
		type RuntimeEvent = <Westend as Chain>::RuntimeEvent;
		assert_expected_events!(
			Westend,
			vec![
				RuntimeEvent::MessageQueue(
					pallet_message_queue::Event::Processed { success: true, .. }
				) => {},
				RuntimeEvent::Balances(pallet_balances::Event::Deposit { who, .. }) => {
					who: *who == t.receiver.account_id,
				},
			]
		);
	}

	fn system_para_limited_teleport_assets(t: SystemParaToRelayTest) -> DispatchResult {
		<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::limited_teleport_assets(
			t.signed_origin,
			bx!(t.args.dest.into()),
			bx!(t.args.beneficiary.into()),
			bx!(t.args.assets.into()),
			t.args.fee_asset_item,
			t.args.weight_limit,
		)
	}

	test.set_assertion::<AssetHubWestend>(para_origin_assertions);
	test.set_assertion::<Westend>(relay_dest_assertions);
	test.set_dispatchable::<AssetHubWestend>(system_para_limited_teleport_assets);
	test.assert();

	let sender_balance_after = test.sender.balance;
	let receiver_balance_after = test.receiver.balance;

	let delivery_fees = AssetHubWestend::execute_with(|| {
		xcm_helpers::teleport_assets_delivery_fees::<
			<AssetHubWestendXcmConfig as xcm_executor::Config>::XcmSender,
		>(
			test.args.assets.clone(), 0, test.args.weight_limit, test.args.beneficiary, test.args.dest
		)
	});

	// Sender's balance is reduced
	assert_eq!(sender_balance_before - amount_to_send - delivery_fees, sender_balance_after);
	// Receiver's asset balance is increased
	assert!(receiver_balance_after > receiver_balance_before);
	// Receiver's asset balance increased by `amount_to_send - delivery_fees - bought_execution`;
	// `delivery_fees` might be paid from transfer or JIT, also `bought_execution` is unknown but
	// should be non-zero
	assert!(receiver_balance_after < receiver_balance_before + amount_to_send);
}

/// Bidirectional teleports of local Penpal assets to Asset Hub as foreign assets while paying
/// fees using (reserve transferred) native asset.
pub fn do_bidirectional_teleport_foreign_assets_between_para_and_asset_hub_using_xt(
	para_to_ah_dispatchable: fn(ParaToSystemParaTest) -> DispatchResult,
	ah_to_para_dispatchable: fn(SystemParaToParaTest) -> DispatchResult,
) {
	// Init values for Parachain
	let fee_amount_to_send: Balance = ASSET_HUB_WESTEND_ED * 1000;
	let asset_location_on_penpal = PenpalA::execute_with(|| PenpalLocalPen2Asset::get());
	let asset_amount_to_send = ASSET_HUB_WESTEND_ED * 1000;
	let asset_owner = PenpalAssetOwner::get();
	let system_para_native_asset_location = RelayLocation::get();
	let sender = PenpalASender::get();
	let penpal_check_account = <PenpalA as PenpalAPallet>::PolkadotXcm::check_account();
	let ah_as_seen_by_penpal = PenpalA::sibling_location_of(AssetHubWestend::para_id());
	let penpal_assets: Assets = vec![
		(Parent, fee_amount_to_send).into(),
		(asset_location_on_penpal.clone(), asset_amount_to_send).into(),
	]
	.into();
	let fee_asset_index = penpal_assets
		.inner()
		.iter()
		.position(|r| r == &(Parent, fee_amount_to_send).into())
		.unwrap() as u32;

	// fund Parachain's sender account
	PenpalA::mint_foreign_asset(
		<PenpalA as Chain>::RuntimeOrigin::signed(asset_owner.clone()),
		system_para_native_asset_location.clone(),
		sender.clone(),
		fee_amount_to_send * 2,
	);

	// No need to create the asset (only mint) as it exists in genesis.
	PenpalA::mint_foreign_asset(
		<PenpalA as Chain>::RuntimeOrigin::signed(asset_owner.clone()),
		asset_location_on_penpal.clone(),
		sender.clone(),
		asset_amount_to_send * 2,
	);
	// fund Parachain's check account to be able to teleport
	PenpalA::fund_accounts(vec![(
		penpal_check_account.clone().into(),
		ASSET_HUB_WESTEND_ED * 1000,
	)]);

	// prefund SA of Penpal on AssetHub with enough native tokens to pay for fees
	let penpal_as_seen_by_ah = AssetHubWestend::sibling_location_of(PenpalA::para_id());
	let sov_penpal_on_ah = AssetHubWestend::sovereign_account_id_of(penpal_as_seen_by_ah);
	AssetHubWestend::fund_accounts(vec![(
		sov_penpal_on_ah.clone().into(),
		ASSET_HUB_WESTEND_ED * 100_000_000_000,
	)]);

	// Init values for System Parachain
	let foreign_asset_at_asset_hub =
		Location::new(1, [Junction::Parachain(PenpalA::para_id().into())])
			.appended_with(asset_location_on_penpal.clone())
			.unwrap();
	let penpal_to_ah_beneficiary_id = AssetHubWestendReceiver::get();

	// Penpal to AH test args
	let penpal_to_ah_test_args = TestContext {
		sender: PenpalASender::get(),
		receiver: AssetHubWestendReceiver::get(),
		args: TestArgs::new_para(
			ah_as_seen_by_penpal,
			penpal_to_ah_beneficiary_id,
			asset_amount_to_send,
			penpal_assets,
			Some(asset_location_on_penpal.clone()),
			fee_asset_index,
		),
	};
	let mut penpal_to_ah = ParaToSystemParaTest::new(penpal_to_ah_test_args);
	let penpal_sender_balance_before = assets_balance_on!(
		PenpalA,
		system_para_native_asset_location.clone(),
		&PenpalASender::get()
	);

	let ah_receiver_balance_before = penpal_to_ah.receiver.balance;

	let penpal_sender_assets_before =
		assets_balance_on!(PenpalA, asset_location_on_penpal.clone(), &PenpalASender::get());
	let ah_receiver_assets_before = foreign_balance_on!(
		AssetHubWestend,
		foreign_asset_at_asset_hub.clone(),
		&AssetHubWestendReceiver::get()
	);
	let ah_issuance_before =
		foreign_issuance_on!(AssetHubWestend, foreign_asset_at_asset_hub.clone());

	penpal_to_ah.set_assertion::<PenpalA>(penpal_to_ah_foreign_assets_sender_assertions);
	penpal_to_ah.set_assertion::<AssetHubWestend>(penpal_to_ah_foreign_assets_receiver_assertions);
	penpal_to_ah.set_dispatchable::<PenpalA>(para_to_ah_dispatchable);
	penpal_to_ah.assert();

	let penpal_sender_balance_after = assets_balance_on!(
		PenpalA,
		system_para_native_asset_location.clone(),
		&PenpalASender::get()
	);

	let ah_receiver_balance_after = penpal_to_ah.receiver.balance;

	let penpal_sender_assets_after =
		assets_balance_on!(PenpalA, asset_location_on_penpal.clone(), &PenpalASender::get());
	let ah_receiver_assets_after = foreign_balance_on!(
		AssetHubWestend,
		foreign_asset_at_asset_hub.clone(),
		&AssetHubWestendReceiver::get()
	);
	let ah_issuance_after =
		foreign_issuance_on!(AssetHubWestend, foreign_asset_at_asset_hub.clone());

	// Sender's balance is reduced
	assert!(penpal_sender_balance_after < penpal_sender_balance_before);
	// Receiver's balance is increased
	assert!(ah_receiver_balance_after > ah_receiver_balance_before);
	// Receiver's balance increased by `amount_to_send - delivery_fees - bought_execution`;
	// `delivery_fees` might be paid from transfer or JIT, also `bought_execution` is unknown but
	// should be non-zero
	assert!(ah_receiver_balance_after < ah_receiver_balance_before + fee_amount_to_send);

	// Sender's balance is reduced by exact amount
	assert_eq!(penpal_sender_assets_before - asset_amount_to_send, penpal_sender_assets_after);
	// Receiver's balance is increased by exact amount
	assert_eq!(ah_receiver_assets_after, ah_receiver_assets_before + asset_amount_to_send);
	// AH foreign asset total supply is increased by exact amount
	assert_eq!(ah_issuance_after, ah_issuance_before + asset_amount_to_send);

	///////////////////////////////////////////////////////////////////////
	// Now test transferring foreign assets back from AssetHub to Penpal //
	///////////////////////////////////////////////////////////////////////

	// Move funds on AH from AHReceiver to AHSender
	AssetHubWestend::execute_with(|| {
		type ForeignAssets = <AssetHubWestend as AssetHubWestendPallet>::ForeignAssets;
		assert_ok!(ForeignAssets::transfer(
			<AssetHubWestend as Chain>::RuntimeOrigin::signed(AssetHubWestendReceiver::get()),
			foreign_asset_at_asset_hub.clone().try_into().unwrap(),
			AssetHubWestendSender::get().into(),
			asset_amount_to_send,
		));
	});

	// Only send back half the amount.
	let asset_amount_to_send = asset_amount_to_send / 2;
	let fee_amount_to_send = fee_amount_to_send / 2;

	let ah_to_penpal_beneficiary_id = PenpalAReceiver::get();
	let penpal_as_seen_by_ah = AssetHubWestend::sibling_location_of(PenpalA::para_id());
	let ah_assets: Assets = vec![
		(Parent, fee_amount_to_send).into(),
		(foreign_asset_at_asset_hub.clone(), asset_amount_to_send).into(),
	]
	.into();
	let fee_asset_index = ah_assets
		.inner()
		.iter()
		.position(|r| r == &(Parent, fee_amount_to_send).into())
		.unwrap() as u32;

	// AH to Penpal test args
	let ah_to_penpal_test_args = TestContext {
		sender: AssetHubWestendSender::get(),
		receiver: PenpalAReceiver::get(),
		args: TestArgs::new_para(
			penpal_as_seen_by_ah,
			ah_to_penpal_beneficiary_id,
			asset_amount_to_send,
			ah_assets,
			Some(asset_location_on_penpal.clone()),
			fee_asset_index,
		),
	};
	let mut ah_to_penpal = SystemParaToParaTest::new(ah_to_penpal_test_args);

	let ah_sender_balance_before = ah_to_penpal.sender.balance;
	let penpal_receiver_balance_before = assets_balance_on!(
		PenpalA,
		system_para_native_asset_location.clone(),
		&PenpalAReceiver::get()
	);

	let ah_sender_assets_before = foreign_balance_on!(
		AssetHubWestend,
		foreign_asset_at_asset_hub.clone(),
		&AssetHubWestendSender::get()
	);
	let penpal_receiver_assets_before =
		assets_balance_on!(PenpalA, asset_location_on_penpal.clone(), &PenpalAReceiver::get());
	let ah_issuance_before =
		foreign_issuance_on!(AssetHubWestend, foreign_asset_at_asset_hub.clone());

	ah_to_penpal.set_assertion::<AssetHubWestend>(ah_to_penpal_foreign_assets_sender_assertions);
	ah_to_penpal.set_assertion::<PenpalA>(ah_to_penpal_foreign_assets_receiver_assertions);
	ah_to_penpal.set_dispatchable::<AssetHubWestend>(ah_to_para_dispatchable);
	ah_to_penpal.assert();

	let ah_sender_balance_after = ah_to_penpal.sender.balance;
	let penpal_receiver_balance_after =
		assets_balance_on!(PenpalA, system_para_native_asset_location, &PenpalAReceiver::get());

	let ah_sender_assets_after = foreign_balance_on!(
		AssetHubWestend,
		foreign_asset_at_asset_hub.clone(),
		&AssetHubWestendSender::get()
	);
	let penpal_receiver_assets_after =
		assets_balance_on!(PenpalA, asset_location_on_penpal.clone(), &PenpalAReceiver::get());
	let ah_issuance_after = foreign_issuance_on!(AssetHubWestend, foreign_asset_at_asset_hub);

	// Sender's balance is reduced
	assert!(ah_sender_balance_after < ah_sender_balance_before);
	// Receiver's balance is increased
	assert!(penpal_receiver_balance_after > penpal_receiver_balance_before);
	// Receiver's balance increased by `amount_to_send - delivery_fees - bought_execution`;
	// `delivery_fees` might be paid from transfer or JIT, also `bought_execution` is unknown but
	// should be non-zero
	assert!(penpal_receiver_balance_after < penpal_receiver_balance_before + fee_amount_to_send);

	// Sender's balance is reduced by exact amount
	assert_eq!(ah_sender_assets_before - asset_amount_to_send, ah_sender_assets_after);
	// Receiver's balance is increased by exact amount
	assert_eq!(penpal_receiver_assets_after, penpal_receiver_assets_before + asset_amount_to_send);
	// AH foreign asset total supply is decreased by exact amount
	assert_eq!(ah_issuance_after, ah_issuance_before - asset_amount_to_send);
}

/// Bidirectional teleports of local Penpal assets to Asset Hub as foreign assets should work
/// (using native reserve-based transfer for fees)
#[test]
fn bidirectional_teleport_foreign_assets_between_para_and_asset_hub() {
	do_bidirectional_teleport_foreign_assets_between_para_and_asset_hub_using_xt(
		para_to_system_para_transfer_assets,
		system_para_to_para_transfer_assets,
	);
}

/// Teleport Native Asset from AssetHub to Parachain fails.
#[test]
fn teleport_to_untrusted_chain_fails() {
	// Init values for Parachain Origin
	let destination = AssetHubWestend::sibling_location_of(PenpalA::para_id());
	let signed_origin =
		<AssetHubWestend as Chain>::RuntimeOrigin::signed(AssetHubWestendSender::get().into());
	let roc_to_send: Balance = WESTEND_ED * 10000;
	let roc_location = RelayLocation::get();

	// Assets to send
	let assets: Vec<Asset> = vec![(roc_location.clone(), roc_to_send).into()];
	let fee_id: AssetId = roc_location.into();

	// this should fail
	AssetHubWestend::execute_with(|| {
		let result = <AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::transfer_assets_using_type_and_then(
			signed_origin.clone(),
			bx!(destination.clone().into()),
			bx!(assets.clone().into()),
			bx!(TransferType::Teleport),
			bx!(fee_id.into()),
			bx!(TransferType::Teleport),
			bx!(VersionedXcm::from(Xcm::<()>::new())),
			Unlimited,
		);
		assert_err!(
			result,
			DispatchError::Module(sp_runtime::ModuleError {
				index: 31,
				error: [2, 0, 0, 0],
				message: Some("Filtered")
			})
		);
	});

	// this should also fail
	AssetHubWestend::execute_with(|| {
		let xcm: Xcm<asset_hub_westend_runtime::RuntimeCall> = Xcm(vec![
			WithdrawAsset(assets.into()),
			InitiateTeleport { assets: Wild(All), dest: destination, xcm: Xcm::<()>::new() },
		]);
		let result = <AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
			signed_origin,
			bx!(xcm::VersionedXcm::from(xcm)),
			Weight::MAX,
		);
		assert!(result.is_err());
	});
}

#[test]
fn accumulate_forward_relay_transfers_native_to_asset_hub() {
	// Required so the relay's XCM router knows the version for the AssetHub parachain.
	Westend::execute_with(|| {
		Dmp::make_parachain_reachable(AssetHubWestend::para_id());
	});
	emulated_integration_tests_common::dap_helpers::test_accumulate_forward_transfers_to_asset_hub::<
		Westend,
		AssetHubWestend,
	>();
}
