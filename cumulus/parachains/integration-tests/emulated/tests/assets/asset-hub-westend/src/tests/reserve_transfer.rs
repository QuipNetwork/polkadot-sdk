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

use crate::{assets_balance_on, assets_issuance_on, balances_issuance_on, imports::*};
use emulated_integration_tests_common::xcm_helpers::{
	find_mq_processed_id, find_xcm_sent_message_id,
};
use sp_core::{crypto::get_public_from_string_or_panic, sr25519};
use westend_system_emulated_network::westend_emulated_chain::westend_runtime::Dmp;

fn relay_to_para_sender_assertions(t: RelayToParaTest) {
	type RuntimeEvent = <Westend as Chain>::RuntimeEvent;

	Westend::assert_xcm_pallet_attempted_complete(Some(Weight::from_parts(350_000_000, 7000)));

	assert_expected_events!(
		Westend,
		vec![
			// Amount to reserve transfer is transferred to Parachain's Sovereign account
			RuntimeEvent::Balances(
				pallet_balances::Event::Transfer { from, to, amount }
			) => {
				from: *from == t.sender.account_id,
				to: *to == Westend::sovereign_account_id_of(
					t.args.dest.clone()
				),
				amount: *amount == t.args.amount,
			},
		]
	);
}

fn para_to_relay_sender_assertions(t: ParaToRelayTest) {
	type RuntimeEvent = <PenpalA as Chain>::RuntimeEvent;
	PenpalA::assert_xcm_pallet_attempted_complete(Some(Weight::from_parts(2_000_000_000, 140_000)));
	assert_expected_events!(
		PenpalA,
		vec![
			// Amount to reserve transfer is transferred to Parachain's Sovereign account
			RuntimeEvent::Assets(
				pallet_assets::Event::Withdrawn { asset_id, who, amount }
			) => {
				asset_id: *asset_id == RelayLocation::get(),
				who: *who == t.sender.account_id,
				amount: *amount == t.args.amount,
			},
		]
	);
}

pub fn system_para_to_para_sender_assertions(t: SystemParaToParaTest) {
	type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
	AssetHubWestend::assert_xcm_pallet_attempted_complete(None);

	let sov_acc_of_dest = AssetHubWestend::sovereign_account_id_of(t.args.dest.clone());
	for asset in t.args.assets.into_inner().into_iter() {
		let expected_id = asset.id.0.clone().try_into().unwrap();
		let asset_amount = if let Fungible(a) = asset.fun { Some(a) } else { None }.unwrap();
		if asset.id == AssetId(Location::new(1, [])) {
			assert_expected_events!(
				AssetHubWestend,
				vec![
					// Amount of native asset is transferred to Parachain's Sovereign account
					RuntimeEvent::Balances(
						pallet_balances::Event::Transfer { from, to, amount }
					) => {
						from: *from == t.sender.account_id,
						to: *to == sov_acc_of_dest,
						amount: *amount == asset_amount,
					},
				]
			);
		} else if matches!(
			asset.id.0.unpack(),
			(0, [PalletInstance(ASSETS_PALLET_ID), GeneralIndex(_)])
		) {
			assert_expected_events!(
				AssetHubWestend,
				vec![
					// Amount of trust-backed asset is transferred to Parachain's Sovereign account
					RuntimeEvent::Assets(
						pallet_assets::Event::Transferred { from, to, amount, .. },
					) => {
						from: *from == t.sender.account_id,
						to: *to == sov_acc_of_dest,
						amount: *amount == asset_amount,
					},
				]
			);
		} else {
			assert_expected_events!(
				AssetHubWestend,
				vec![
					// Amount of foreign asset is transferred to Parachain's Sovereign account
					RuntimeEvent::ForeignAssets(
						pallet_assets::Event::Transferred { asset_id, from, to, amount },
					) => {
						asset_id: *asset_id == expected_id,
						from: *from == t.sender.account_id,
						to: *to == sov_acc_of_dest,
						amount: *amount == asset_amount,
					},
				]
			);
		}
	}
	assert_expected_events!(
		AssetHubWestend,
		vec![
			// Delivery fees are paid
			RuntimeEvent::PolkadotXcm(pallet_xcm::Event::FeesPaid { .. }) => {},
		]
	);
	AssetHubWestend::assert_xcm_pallet_sent();
}

pub fn system_para_to_para_receiver_assertions(t: SystemParaToParaTest) {
	type RuntimeEvent = <PenpalA as Chain>::RuntimeEvent;

	PenpalA::assert_xcmp_queue_success(None);
	for asset in t.args.assets.into_inner().into_iter() {
		let expected_id = asset.id.0.try_into().unwrap();
		assert_expected_events!(
			PenpalA,
			vec![
				RuntimeEvent::Assets(pallet_assets::Event::Deposited { asset_id, who, .. }) => {
					asset_id: *asset_id == expected_id,
					who: *who == t.receiver.account_id,
				},
			]
		);
	}
}

pub fn system_para_to_penpal_receiver_assertions(t: SystemParaToParaTest) {
	type RuntimeEvent = <PenpalA as Chain>::RuntimeEvent;

	PenpalA::assert_xcmp_queue_success(None);
	for asset in t.args.assets.into_inner().into_iter() {
		let mut expected_id: Location = asset.id.0.try_into().unwrap();
		let relative_id = match expected_id {
			Location { parents: 1, interior: Here } => expected_id,
			_ => {
				expected_id
					.push_front_interior(Parachain(AssetHubWestend::para_id().into()))
					.unwrap();
				Location::new(1, expected_id.interior().clone())
			},
		};

		assert_expected_events!(
			PenpalA,
			vec![
				RuntimeEvent::Assets(pallet_assets::Event::Deposited { asset_id, who, .. }) => {
					asset_id: *asset_id == relative_id,
					who: *who == t.receiver.account_id,
				},
			]
		);
	}
}

pub fn para_to_system_para_sender_assertions(t: ParaToSystemParaTest) {
	type RuntimeEvent = <PenpalA as Chain>::RuntimeEvent;
	PenpalA::assert_xcm_pallet_attempted_complete(None);
	for asset in t.args.assets.into_inner().into_iter() {
		let expected_id = asset.id.0;
		let asset_amount = if let Fungible(a) = asset.fun { Some(a) } else { None }.unwrap();
		assert_expected_events!(
			PenpalA,
			vec![
				RuntimeEvent::Assets(
					pallet_assets::Event::Withdrawn { asset_id, who, amount }
				) => {
					asset_id: *asset_id == expected_id,
					who: *who == t.sender.account_id,
					amount: *amount == asset_amount,
				},
			]
		);
	}
}

fn para_to_relay_receiver_assertions(t: ParaToRelayTest) {
	type RuntimeEvent = <Westend as Chain>::RuntimeEvent;
	let sov_penpal_on_relay =
		Westend::sovereign_account_id_of(Westend::child_location_of(PenpalA::para_id()));

	Westend::assert_ump_queue_processed(
		true,
		Some(PenpalA::para_id()),
		Some(Weight::from_parts(306305000, 7_186)),
	);

	assert_expected_events!(
		Westend,
		vec![
			// Amount to reserve transfer is withdrawn from Parachain's Sovereign account
			RuntimeEvent::Balances(
				pallet_balances::Event::Withdraw { who, amount }
			) => {
				who: *who == sov_penpal_on_relay.clone().into(),
				amount: *amount == t.args.amount,
			},
			RuntimeEvent::Balances(pallet_balances::Event::Deposit { .. }) => {},
			RuntimeEvent::MessageQueue(
				pallet_message_queue::Event::Processed { success: true, .. }
			) => {},
		]
	);
}

pub fn para_to_system_para_receiver_assertions(t: ParaToSystemParaTest) {
	type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
	AssetHubWestend::assert_xcmp_queue_success(None);

	let sov_acc_of_penpal = AssetHubWestend::sovereign_account_id_of(Location::new(
		1,
		Parachain(PenpalA::para_id().into()),
	));
	for (idx, asset) in t.args.assets.into_inner().into_iter().enumerate() {
		let expected_id = asset.id.0.clone().try_into().unwrap();
		let asset_amount = if let Fungible(a) = asset.fun { Some(a) } else { None }.unwrap();
		if idx == t.args.fee_asset_item as usize {
			assert_expected_events!(
				AssetHubWestend,
				vec![
					// Amount of native is withdrawn from Parachain's Sovereign account
					RuntimeEvent::Balances(
						pallet_balances::Event::Withdraw { who, amount }
					) => {
						who: *who == sov_acc_of_penpal.clone().into(),
						amount: *amount == asset_amount,
					},
					RuntimeEvent::Balances(pallet_balances::Event::Deposit { who, .. }) => {
						who: *who == t.receiver.account_id,
					},
				]
			);
		} else {
			assert_expected_events!(
				AssetHubWestend,
				vec![
					// Amount of foreign asset is transferred from Parachain's Sovereign account
					// to Receiver's account
					RuntimeEvent::ForeignAssets(
						pallet_assets::Event::Withdrawn { asset_id, who, amount },
					) => {
						asset_id: *asset_id == expected_id,
						who: *who == sov_acc_of_penpal,
						amount: *amount == asset_amount,
					},
					RuntimeEvent::ForeignAssets(
						pallet_assets::Event::Deposited { asset_id, who, amount },
					) => {
						asset_id: *asset_id == expected_id,
						who: *who == t.receiver.account_id,
						amount: *amount == asset_amount,
					},
				]
			);
		}
	}
	assert_expected_events!(
		AssetHubWestend,
		vec![
			RuntimeEvent::MessageQueue(
				pallet_message_queue::Event::Processed { success: true, .. }
			) => {},
		]
	);
}

fn system_para_to_para_assets_sender_assertions(t: SystemParaToParaTest) {
	type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
	AssetHubWestend::assert_xcm_pallet_attempted_complete(Some(Weight::from_parts(
		487_426_000,
		8799,
	)));
	assert_expected_events!(
		AssetHubWestend,
		vec![
			// Amount to reserve transfer is transferred to Parachain's Sovereign account
			RuntimeEvent::Assets(
				pallet_assets::Event::Transferred { asset_id, from, to, amount }
			) => {
				asset_id: *asset_id == RESERVABLE_ASSET_ID,
				from: *from == t.sender.account_id,
				to: *to == AssetHubWestend::sovereign_account_id_of(
					t.args.dest.clone()
				),
				amount: *amount == t.args.amount,
			},
			// Delivery fees are deposited to the DAP buffer account.
			RuntimeEvent::Balances(pallet_balances::Event::Deposit { who, .. }) => {
				who: *who == DapBufferAccount::get(),
			},
			// Delivery fees are paid
			RuntimeEvent::PolkadotXcm(
				pallet_xcm::Event::FeesPaid { .. }
			) => {},
		]
	);
}

fn para_to_system_para_assets_sender_assertions(t: ParaToSystemParaTest) {
	type RuntimeEvent = <PenpalA as Chain>::RuntimeEvent;
	let system_para_native_asset_location = RelayLocation::get();
	let reservable_asset_location = PenpalLocalReservableFromAssetHub::get();
	PenpalA::assert_xcm_pallet_attempted_complete(Some(Weight::from_parts(2_000_000_000, 140000)));
	assert_expected_events!(
		PenpalA,
		vec![
			// Fees amount to reserve transfer is withdrawn from Parachains's sender account
			RuntimeEvent::Assets(
				pallet_assets::Event::Withdrawn { asset_id, who, .. }
			) => {
				asset_id: *asset_id == system_para_native_asset_location,
				who: *who == t.sender.account_id,
			},
			// Amount to reserve transfer is withdrawn from Parachains's sender account
			RuntimeEvent::Assets(
				pallet_assets::Event::Withdrawn { asset_id, who, amount }
			) => {
				asset_id: *asset_id == reservable_asset_location,
				who: *who == t.sender.account_id,
				amount: *amount == t.args.amount,
			},
			// Delivery fees are paid
			RuntimeEvent::PolkadotXcm(
				pallet_xcm::Event::FeesPaid { .. }
			) => {},
		]
	);
}

fn system_para_to_para_assets_receiver_assertions(t: SystemParaToParaTest) {
	type RuntimeEvent = <PenpalA as Chain>::RuntimeEvent;
	let system_para_asset_location = PenpalLocalReservableFromAssetHub::get();
	PenpalA::assert_xcmp_queue_success(None);
	assert_expected_events!(
		PenpalA,
		vec![
			RuntimeEvent::Assets(pallet_assets::Event::Deposited { asset_id, who, .. }) => {
				asset_id: *asset_id == RelayLocation::get(),
				who: *who == t.receiver.account_id,
			},
			RuntimeEvent::Assets(pallet_assets::Event::Deposited { asset_id, who, amount }) => {
				asset_id: *asset_id == system_para_asset_location,
				who: *who == t.receiver.account_id,
				amount: *amount == t.args.amount,
			},
		]
	);
}

fn para_to_system_para_assets_receiver_assertions(t: ParaToSystemParaTest) {
	type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
	let sov_penpal_on_ahr = AssetHubWestend::sovereign_account_id_of(
		AssetHubWestend::sibling_location_of(PenpalA::para_id()),
	);
	AssetHubWestend::assert_xcmp_queue_success(None);
	assert_expected_events!(
		AssetHubWestend,
		vec![
			// Amount to reserve transfer is withdrawn from Parachain's Sovereign account
			RuntimeEvent::Assets(pallet_assets::Event::Withdrawn { asset_id, who, amount }) => {
				asset_id: *asset_id == RESERVABLE_ASSET_ID,
				who: *who == sov_penpal_on_ahr,
				amount: *amount == t.args.amount,
			},
			// Fee amount is withdrawn from Parachain's Sovereign account
			RuntimeEvent::Balances(pallet_balances::Event::Withdraw { who, .. }) => {
				who: *who == sov_penpal_on_ahr,
			},
			// Amount to reserve transfer is deposited to beneficiary
			RuntimeEvent::Assets(pallet_assets::Event::Deposited { asset_id, who, amount }) => {
				asset_id: *asset_id == RESERVABLE_ASSET_ID,
				who: *who == t.receiver.account_id,
				amount: *amount == t.args.amount,
			},
			// Remaining fee amount is deposited to beneficiary
			RuntimeEvent::Balances(pallet_balances::Event::Deposit { who, .. }) => {
				who: *who == t.receiver.account_id,
			},
		]
	);
}

fn relay_to_para_assets_receiver_assertions(t: RelayToParaTest) {
	type RuntimeEvent = <PenpalA as Chain>::RuntimeEvent;

	assert_expected_events!(
		PenpalA,
		vec![
			RuntimeEvent::Assets(pallet_assets::Event::Deposited { asset_id, who, .. }) => {
				asset_id: *asset_id == RelayLocation::get(),
				who: *who == t.receiver.account_id,
			},
			RuntimeEvent::MessageQueue(
				pallet_message_queue::Event::Processed { success: true, .. }
			) => {},
		]
	);
}

pub fn para_to_para_through_hop_sender_assertions<Hop: Clone>(
	mut t: Test<PenpalA, PenpalB, Hop, TestArgs<Location>>,
) {
	type RuntimeEvent = <PenpalA as Chain>::RuntimeEvent;
	PenpalA::assert_xcm_pallet_attempted_complete(None);

	let msg_sent_id = find_xcm_sent_message_id::<PenpalA>().expect("Missing Sent Event");
	t.insert_unique_topic_id("PenpalA", msg_sent_id.into());

	for asset in t.args.assets.into_inner() {
		let expected_id = asset.id.0.clone().try_into().unwrap();
		let expected_amount = if let Fungible(a) = asset.fun { Some(a) } else { None }.unwrap();
		assert_expected_events!(
			PenpalA,
			vec![
				// Amount to reserve transfer is transferred to Parachain's Sovereign account
				RuntimeEvent::Assets(
					pallet_assets::Event::Withdrawn { asset_id, who, amount },
				) => {
					asset_id: *asset_id == expected_id,
					who: *who == t.sender.account_id,
					amount: *amount == expected_amount,
				},
			]
		);
	}
}

fn para_to_para_relay_hop_assertions(t: ParaToParaThroughRelayTest) {
	type RuntimeEvent = <Westend as Chain>::RuntimeEvent;
	let sov_penpal_a_on_westend =
		Westend::sovereign_account_id_of(Westend::child_location_of(PenpalA::para_id()));
	let sov_penpal_b_on_westend =
		Westend::sovereign_account_id_of(Westend::child_location_of(PenpalB::para_id()));

	assert_expected_events!(
		Westend,
		vec![
			// Withdrawn from sender parachain SA
			RuntimeEvent::Balances(
				pallet_balances::Event::Withdraw { who, amount }
			) => {
				who: *who == sov_penpal_a_on_westend,
				amount: *amount == t.args.amount,
			},
			// Deposited to receiver parachain SA
			RuntimeEvent::Balances(
				pallet_balances::Event::Deposit { who, .. }
			) => {
				who: *who == sov_penpal_b_on_westend,
			},
			RuntimeEvent::MessageQueue(
				pallet_message_queue::Event::Processed { success: true, .. }
			) => {},
		]
	);
}

fn para_to_para_asset_hub_hop_assertions(t: ParaToParaThroughAHTest) {
	type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
	let sov_penpal_a_on_ah = AssetHubWestend::sovereign_account_id_of(
		AssetHubWestend::sibling_location_of(PenpalA::para_id()),
	);

	let (_, asset_amount) = fee_asset(&t.args.assets, t.args.fee_asset_item as usize).unwrap();

	assert_expected_events!(
		AssetHubWestend,
		vec![
			// Withdrawn from sender parachain SA
			RuntimeEvent::Assets(
				pallet_assets::Event::Withdrawn { who, amount, .. }
			) => {
				who: *who == sov_penpal_a_on_ah,
				amount: *amount == asset_amount,
			},
			RuntimeEvent::MessageQueue(
				pallet_message_queue::Event::Processed { success: true, .. }
			) => {},
		]
	);
}

pub fn para_to_para_through_hop_receiver_assertions<Hop: Clone>(
	mut t: Test<PenpalA, PenpalB, Hop, TestArgs<Location>>,
) {
	type RuntimeEvent = <PenpalB as Chain>::RuntimeEvent;

	PenpalB::assert_xcmp_queue_success(None);

	let mq_prc_id = find_mq_processed_id::<PenpalB>().expect("Missing Processed Event");
	t.insert_unique_topic_id("PenpalB", mq_prc_id);

	for asset in t.args.assets.into_inner().into_iter() {
		let expected_id = asset.id.0.try_into().unwrap();
		assert_expected_events!(
			PenpalB,
			vec![
				RuntimeEvent::Assets(pallet_assets::Event::Deposited { asset_id, who, .. }) => {
					asset_id: *asset_id == expected_id,
					who: *who == t.receiver.account_id,
				},
			]
		);
	}
}

fn relay_to_para_reserve_transfer_assets(t: RelayToParaTest) -> DispatchResult {
	let Junction::Parachain(para_id) = *t.args.dest.chain_location().last().unwrap() else {
		unimplemented!("Destination is not a parachain?")
	};

	type Runtime = <Westend as Chain>::Runtime;
	let remote_fee_id: AssetId = t
		.args
		.assets
		.clone()
		.into_inner()
		.get(t.args.fee_asset_item as usize)
		.ok_or(pallet_xcm::Error::<Runtime>::Empty)?
		.clone()
		.id;

	Dmp::make_parachain_reachable(para_id);
	<Westend as WestendPallet>::XcmPallet::transfer_assets_using_type_and_then(
		t.signed_origin,
		bx!(t.args.dest.into()),
		bx!(t.args.assets.into()),
		bx!(TransferType::LocalReserve),
		bx!(remote_fee_id.into()),
		bx!(TransferType::LocalReserve),
		bx!(VersionedXcm::from(
			Xcm::<()>::builder_unsafe()
				.deposit_asset(AllCounted(1), t.args.beneficiary)
				.build()
		)),
		t.args.weight_limit,
	)
}

fn para_to_relay_reserve_transfer_assets(t: ParaToRelayTest) -> DispatchResult {
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
		bx!(TransferType::DestinationReserve),
		bx!(remote_fee_id.into()),
		bx!(TransferType::DestinationReserve),
		bx!(VersionedXcm::from(
			Xcm::<()>::builder_unsafe()
				.deposit_asset(AllCounted(1), t.args.beneficiary)
				.build()
		)),
		t.args.weight_limit,
	)
}

fn system_para_to_para_reserve_transfer_assets(t: SystemParaToParaTest) -> DispatchResult {
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
		bx!(TransferType::LocalReserve),
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

fn para_to_system_para_reserve_transfer_assets(t: ParaToSystemParaTest) -> DispatchResult {
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
		bx!(TransferType::DestinationReserve),
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

fn para_to_para_through_relay_limited_reserve_transfer_assets(
	t: ParaToParaThroughRelayTest,
) -> DispatchResult {
	let Junction::Parachain(para_id) = *t.args.dest.chain_location().last().unwrap() else {
		unimplemented!("Destination is not a parachain?")
	};

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

	let relay_location = VersionedLocation::from(Location::parent());

	Westend::ext_wrapper(|| {
		Dmp::make_parachain_reachable(para_id);
	});
	<PenpalA as PenpalAPallet>::PolkadotXcm::transfer_assets_using_type_and_then(
		t.signed_origin,
		bx!(t.args.dest.into()),
		bx!(t.args.assets.into()),
		bx!(TransferType::RemoteReserve(relay_location.clone())),
		bx!(remote_fee_id.into()),
		bx!(TransferType::RemoteReserve(relay_location)),
		bx!(VersionedXcm::from(
			Xcm::<()>::builder_unsafe()
				.deposit_asset(AllCounted(1), t.args.beneficiary)
				.build()
		)),
		t.args.weight_limit,
	)
}

fn para_to_para_through_asset_hub_limited_reserve_transfer_assets(
	t: ParaToParaThroughAHTest,
) -> DispatchResult {
	<PenpalA as PenpalAPallet>::PolkadotXcm::limited_reserve_transfer_assets(
		t.signed_origin,
		bx!(t.args.dest.into()),
		bx!(t.args.beneficiary.into()),
		bx!(t.args.assets.into()),
		t.args.fee_asset_item,
		t.args.weight_limit,
	)
}

/// Reserve Transfers of native asset from Relay Chain to the Asset Hub shouldn't work
#[test]
fn reserve_transfer_native_asset_from_relay_to_asset_hub_fails() {
	// Init values for Relay Chain
	let signed_origin = <Westend as Chain>::RuntimeOrigin::signed(WestendSender::get().into());
	let destination = Westend::child_location_of(AssetHubWestend::para_id());
	let beneficiary: Location =
		AccountId32Junction { network: None, id: AssetHubWestendReceiver::get().into() }.into();
	let amount_to_send: Balance = WESTEND_ED * 1000;
	let assets: Assets = (Here, amount_to_send).into();
	let fee_asset_item = 0;

	// this should fail
	Westend::execute_with(|| {
		let result = <Westend as WestendPallet>::XcmPallet::limited_reserve_transfer_assets(
			signed_origin,
			bx!(destination.into()),
			bx!(beneficiary.into()),
			bx!(assets.into()),
			fee_asset_item,
			WeightLimit::Unlimited,
		);
		assert_err!(
			result,
			DispatchError::Module(sp_runtime::ModuleError {
				index: 99,
				error: [2, 0, 0, 0],
				message: Some("Filtered")
			})
		);
	});
}

/// Reserve Transfers of native asset from Asset Hub to Relay Chain shouldn't work
#[test]
fn reserve_transfer_native_asset_from_asset_hub_to_relay_fails() {
	// Init values for Asset Hub
	let signed_origin =
		<AssetHubWestend as Chain>::RuntimeOrigin::signed(AssetHubWestendSender::get().into());
	let destination = AssetHubWestend::parent_location();
	let beneficiary_id = WestendReceiver::get();
	let beneficiary: Location =
		AccountId32Junction { network: None, id: beneficiary_id.into() }.into();
	let amount_to_send: Balance = ASSET_HUB_WESTEND_ED * 1000;

	let assets: Assets = (Parent, amount_to_send).into();
	let fee_asset_item = 0;

	// this should fail
	AssetHubWestend::execute_with(|| {
		let result =
			<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::limited_reserve_transfer_assets(
				signed_origin,
				bx!(destination.into()),
				bx!(beneficiary.into()),
				bx!(assets.into()),
				fee_asset_item,
				WeightLimit::Unlimited,
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
}

// =========================================================================
// ========= Reserve Transfers - Native Asset - Relay<>Parachain ===========
// =========================================================================
/// Reserve Transfers of native asset from Relay to Parachain should work
#[test]
fn reserve_transfer_native_asset_from_relay_to_para() {
	// Init values for Relay
	let destination = Westend::child_location_of(PenpalA::para_id());
	let sender = WestendSender::get();
	let amount_to_send: Balance = WESTEND_ED * 1000;

	// Init values for Parachain
	let relay_native_asset_location = RelayLocation::get();
	let receiver = PenpalAReceiver::get();

	// Init Test
	let test_args = TestContext {
		sender,
		receiver: receiver.clone(),
		args: TestArgs::new_relay(destination.clone(), receiver.clone(), amount_to_send),
	};
	let mut test = RelayToParaTest::new(test_args);

	// Query initial balances
	let sender_balance_before = test.sender.balance;
	let receiver_assets_before =
		assets_balance_on!(PenpalA, relay_native_asset_location.clone(), &receiver);
	let penpal_issuance_before = assets_issuance_on!(PenpalA, relay_native_asset_location.clone());
	let relay_issuance_before = balances_issuance_on!(Westend);

	// Set assertions and dispatchables
	test.set_assertion::<Westend>(relay_to_para_sender_assertions);
	test.set_assertion::<PenpalA>(relay_to_para_assets_receiver_assertions);
	test.set_dispatchable::<Westend>(relay_to_para_reserve_transfer_assets);
	test.assert();

	// Query final balances
	let sender_balance_after = test.sender.balance;
	let receiver_assets_after =
		assets_balance_on!(PenpalA, relay_native_asset_location.clone(), &receiver);
	let penpal_issuance_after = assets_issuance_on!(PenpalA, relay_native_asset_location);
	let relay_issuance_after = balances_issuance_on!(Westend);

	// Sender's balance is reduced by amount sent plus delivery fees
	assert!(sender_balance_after < sender_balance_before - amount_to_send);
	// Receiver's asset balance is increased
	assert!(receiver_assets_after > receiver_assets_before);
	// Receiver's asset balance increased by `amount_to_send - delivery_fees - bought_execution`;
	// `delivery_fees` might be paid from transfer or JIT, also `bought_execution` is unknown but
	// should be non-zero
	assert!(receiver_assets_after < receiver_assets_before + amount_to_send);
	// Penpal mints native asset transferred in
	assert_eq!(penpal_issuance_after, penpal_issuance_before + amount_to_send);
	// Relay supply doesn't change (assets move to sovereign account)
	assert_eq!(relay_issuance_after, relay_issuance_before);
}

/// Reserve Transfers of native asset from Parachain to Relay should work
#[test]
fn reserve_transfer_native_asset_from_para_to_relay() {
	// Init values for Parachain
	let destination = PenpalA::parent_location();
	let sender = PenpalASender::get();
	let amount_to_send: Balance = WESTEND_ED * 1000;
	let assets: Assets = (Parent, amount_to_send).into();
	let asset_owner = PenpalAssetOwner::get();
	let relay_native_asset_location = RelayLocation::get();

	// fund Parachain's sender account
	PenpalA::mint_foreign_asset(
		<PenpalA as Chain>::RuntimeOrigin::signed(asset_owner),
		relay_native_asset_location.clone(),
		sender.clone(),
		amount_to_send * 2,
	);

	// Init values for Relay
	let receiver = WestendReceiver::get();
	let penpal_location_as_seen_by_relay = Westend::child_location_of(PenpalA::para_id());
	let sov_penpal_on_relay = Westend::sovereign_account_id_of(penpal_location_as_seen_by_relay);

	// fund Parachain's SA on Relay with the native tokens held in reserve
	Westend::fund_accounts(vec![(sov_penpal_on_relay.into(), amount_to_send * 2)]);

	// Init Test
	let test_args = TestContext {
		sender: sender.clone(),
		receiver: receiver.clone(),
		args: TestArgs::new_para(
			destination.clone(),
			receiver,
			amount_to_send,
			assets.clone(),
			None,
			0,
		),
	};
	let mut test = ParaToRelayTest::new(test_args);

	// Query initial balances
	let sender_assets_before =
		assets_balance_on!(PenpalA, relay_native_asset_location.clone(), &sender);
	let receiver_balance_before = test.receiver.balance;
	let penpal_issuance_before = assets_issuance_on!(PenpalA, relay_native_asset_location.clone());
	let relay_issuance_before = balances_issuance_on!(Westend);

	// Set assertions and dispatchables
	test.set_assertion::<PenpalA>(para_to_relay_sender_assertions);
	test.set_assertion::<Westend>(para_to_relay_receiver_assertions);
	test.set_dispatchable::<PenpalA>(para_to_relay_reserve_transfer_assets);
	test.assert();

	// Query final balances
	let sender_assets_after =
		assets_balance_on!(PenpalA, relay_native_asset_location.clone(), &sender);
	let receiver_balance_after = test.receiver.balance;
	let penpal_issuance_after = assets_issuance_on!(PenpalA, relay_native_asset_location);
	let relay_issuance_after = balances_issuance_on!(Westend);

	// Sender's balance is reduced by amount sent (delivery fees are charged in native tokens).
	assert_eq!(sender_assets_after, sender_assets_before - amount_to_send);
	// Receiver's asset balance is increased
	assert!(receiver_balance_after > receiver_balance_before);
	// Receiver's asset balance increased by `amount_to_send - delivery_fees - bought_execution`;
	// `delivery_fees` might be paid from transfer or JIT, also `bought_execution` is unknown but
	// should be non-zero
	assert!(receiver_balance_after < receiver_balance_before + amount_to_send);
	// Penpal burns native asset transferred out
	assert_eq!(penpal_issuance_after, penpal_issuance_before - amount_to_send);
	// Relay supply is reduced only by burnt fees
	assert!(relay_issuance_after < relay_issuance_before);
	assert!(relay_issuance_after > relay_issuance_before - amount_to_send);
}

// =========================================================================
// ======= Reserve Transfers - Native Asset - AssetHub<>Parachain ==========
// =========================================================================
/// Reserve Transfers of native asset from Asset Hub to Parachain should work
#[test]
fn reserve_transfer_native_asset_from_asset_hub_to_para() {
	// Init values for Asset Hub
	let destination = AssetHubWestend::sibling_location_of(PenpalA::para_id());
	let sender = AssetHubWestendSender::get();
	let amount_to_send: Balance = ASSET_HUB_WESTEND_ED * 2000;
	let assets: Assets = (Parent, amount_to_send).into();

	// Init values for Parachain
	let system_para_native_asset_location = RelayLocation::get();
	let receiver = PenpalAReceiver::get();

	// Init Test
	let test_args = TestContext {
		sender,
		receiver: receiver.clone(),
		args: TestArgs::new_para(
			destination.clone(),
			receiver.clone(),
			amount_to_send,
			assets.clone(),
			None,
			0,
		),
	};
	let mut test = SystemParaToParaTest::new(test_args);

	// Query initial balances
	let sender_balance_before = test.sender.balance;
	let receiver_assets_before =
		assets_balance_on!(PenpalA, system_para_native_asset_location.clone(), &receiver);
	let penpal_issuance_before =
		assets_issuance_on!(PenpalA, system_para_native_asset_location.clone());
	let ah_issuance_before = balances_issuance_on!(AssetHubWestend);

	// Set assertions and dispatchables
	test.set_assertion::<AssetHubWestend>(system_para_to_para_sender_assertions);
	test.set_assertion::<PenpalA>(system_para_to_para_receiver_assertions);
	test.set_dispatchable::<AssetHubWestend>(system_para_to_para_reserve_transfer_assets);
	test.assert();

	// Query final balances
	let sender_balance_after = test.sender.balance;
	let receiver_assets_after =
		assets_balance_on!(PenpalA, system_para_native_asset_location.clone(), &receiver);
	let penpal_issuance_after = assets_issuance_on!(PenpalA, system_para_native_asset_location);
	let ah_issuance_after = balances_issuance_on!(AssetHubWestend);

	// Sender's balance is reduced by amount sent plus delivery fees
	assert!(sender_balance_after < sender_balance_before - amount_to_send);
	// Receiver's assets is increased
	assert!(receiver_assets_after > receiver_assets_before);
	// Receiver's assets increased by `amount_to_send - delivery_fees - bought_execution`;
	// `delivery_fees` might be paid from transfer or JIT, also `bought_execution` is unknown but
	// should be non-zero
	assert!(receiver_assets_after < receiver_assets_before + amount_to_send);
	// Penpal mints native asset transferred in
	assert_eq!(penpal_issuance_after, penpal_issuance_before + amount_to_send);
	// Asset Hub supply doesn't change (assets move to sovereign account)
	assert_eq!(ah_issuance_after, ah_issuance_before);
}

/// Reserve Transfers of native asset from Parachain to Asset Hub should work
#[test]
fn reserve_transfer_native_asset_from_para_to_asset_hub() {
	// Init values for Parachain
	let destination = PenpalA::sibling_location_of(AssetHubWestend::para_id());
	let sender = PenpalASender::get();
	let amount_to_send: Balance = ASSET_HUB_WESTEND_ED * 1000;
	let assets: Assets = (Parent, amount_to_send).into();
	let system_para_native_asset_location = RelayLocation::get();
	let asset_owner = PenpalAssetOwner::get();

	// fund Parachain's sender account
	PenpalA::mint_foreign_asset(
		<PenpalA as Chain>::RuntimeOrigin::signed(asset_owner),
		system_para_native_asset_location.clone(),
		sender.clone(),
		amount_to_send * 2,
	);

	// Init values for Asset Hub
	let receiver = AssetHubWestendReceiver::get();
	let penpal_location_as_seen_by_ahr = AssetHubWestend::sibling_location_of(PenpalA::para_id());
	let sov_penpal_on_ahr =
		AssetHubWestend::sovereign_account_id_of(penpal_location_as_seen_by_ahr);

	// fund Parachain's SA on Asset Hub with the native tokens held in reserve
	AssetHubWestend::fund_accounts(vec![(sov_penpal_on_ahr.into(), amount_to_send * 2)]);

	// Init Test
	let test_args = TestContext {
		sender: sender.clone(),
		receiver: receiver.clone(),
		args: TestArgs::new_para(
			destination.clone(),
			receiver.clone(),
			amount_to_send,
			assets.clone(),
			None,
			0,
		),
	};
	let mut test = ParaToSystemParaTest::new(test_args);

	// Query initial balances
	let sender_assets_before =
		assets_balance_on!(PenpalA, system_para_native_asset_location.clone(), &sender);
	let receiver_balance_before = test.receiver.balance;
	let penpal_issuance_before =
		assets_issuance_on!(PenpalA, system_para_native_asset_location.clone());
	let ah_issuance_before = balances_issuance_on!(AssetHubWestend);

	// Set assertions and dispatchables
	test.set_assertion::<PenpalA>(para_to_system_para_sender_assertions);
	test.set_assertion::<AssetHubWestend>(para_to_system_para_receiver_assertions);
	test.set_dispatchable::<PenpalA>(para_to_system_para_reserve_transfer_assets);
	test.assert();

	// Query final balances
	let sender_assets_after =
		assets_balance_on!(PenpalA, system_para_native_asset_location.clone(), &sender);
	let receiver_balance_after = test.receiver.balance;
	let penpal_issuance_after = assets_issuance_on!(PenpalA, system_para_native_asset_location);
	let ah_issuance_after = balances_issuance_on!(AssetHubWestend);

	// Sender's balance is reduced by amount sent (delivery fees are charged in native)
	assert_eq!(sender_assets_after, sender_assets_before - amount_to_send);
	// Receiver's balance is increased
	assert!(receiver_balance_after > receiver_balance_before);
	// Receiver's balance increased by `amount_to_send - delivery_fees - bought_execution`;
	// `delivery_fees` might be paid from transfer or JIT, also `bought_execution` is unknown but
	// should be non-zero
	assert!(receiver_balance_after < receiver_balance_before + amount_to_send);
	// Penpal burns native asset transferred out
	assert_eq!(penpal_issuance_after, penpal_issuance_before - amount_to_send);
	// Asset Hub supply doesn't change (assets move from sovereign account)
	assert_eq!(ah_issuance_after, ah_issuance_before);
}

// =========================================================================
// ======= Reserve Transfers - Non-system Asset - AssetHub<>Parachain ======
// =========================================================================
/// Reserve Transfers of a local asset and native asset from Asset Hub to Parachain should
/// work
#[test]
fn reserve_transfer_multiple_assets_from_asset_hub_to_para() {
	// Init values for Asset Hub
	let destination = AssetHubWestend::sibling_location_of(PenpalA::para_id());
	let sov_penpal_on_ahr = AssetHubWestend::sovereign_account_id_of(destination.clone());
	let sender = AssetHubWestendSender::get();
	let fee_amount_to_send = ASSET_HUB_WESTEND_ED * 100;
	let asset_amount_to_send = ASSET_HUB_WESTEND_ED * 100;
	let asset_owner = AssetHubWestendAssetOwner::get();
	let asset_owner_signer = <AssetHubWestend as Chain>::RuntimeOrigin::signed(asset_owner.clone());
	let assets: Assets = vec![
		(Parent, fee_amount_to_send).into(),
		(
			[PalletInstance(ASSETS_PALLET_ID), GeneralIndex(RESERVABLE_ASSET_ID.into())],
			asset_amount_to_send,
		)
			.into(),
	]
	.into();
	let fee_asset_index = assets
		.inner()
		.iter()
		.position(|r| r == &(Parent, fee_amount_to_send).into())
		.unwrap() as u32;
	AssetHubWestend::mint_asset(
		asset_owner_signer,
		RESERVABLE_ASSET_ID,
		asset_owner,
		asset_amount_to_send * 2,
	);

	// Create SA-of-Penpal-on-AHR with ED.
	AssetHubWestend::fund_accounts(vec![(sov_penpal_on_ahr.into(), ASSET_HUB_WESTEND_ED)]);

	// Init values for Parachain
	let receiver = PenpalAReceiver::get();
	let system_para_native_asset_location = RelayLocation::get();
	let system_para_foreign_asset_location = PenpalLocalReservableFromAssetHub::get();

	// Init Test
	let para_test_args = TestContext {
		sender: sender.clone(),
		receiver: receiver.clone(),
		args: TestArgs::new_para(
			destination,
			receiver.clone(),
			asset_amount_to_send,
			assets,
			None,
			fee_asset_index,
		),
	};
	let mut test = SystemParaToParaTest::new(para_test_args);

	// Query initial balances
	let sender_balance_before = test.sender.balance;
	let sender_assets_before = AssetHubWestend::execute_with(|| {
		type Assets = <AssetHubWestend as AssetHubWestendPallet>::Assets;
		<Assets as Inspect<_>>::balance(RESERVABLE_ASSET_ID, &sender)
	});
	let receiver_system_native_assets_before =
		assets_balance_on!(PenpalA, system_para_native_asset_location.clone(), &receiver);
	let receiver_foreign_assets_before =
		assets_balance_on!(PenpalA, system_para_foreign_asset_location.clone(), &receiver);

	// Set assertions and dispatchables
	test.set_assertion::<AssetHubWestend>(system_para_to_para_assets_sender_assertions);
	test.set_assertion::<PenpalA>(system_para_to_para_assets_receiver_assertions);
	test.set_dispatchable::<AssetHubWestend>(system_para_to_para_reserve_transfer_assets);
	test.assert();

	// Query final balances
	let sender_balance_after = test.sender.balance;
	let sender_assets_after = AssetHubWestend::execute_with(|| {
		type Assets = <AssetHubWestend as AssetHubWestendPallet>::Assets;
		<Assets as Inspect<_>>::balance(RESERVABLE_ASSET_ID, &sender)
	});
	let receiver_system_native_assets_after =
		assets_balance_on!(PenpalA, system_para_native_asset_location, &receiver);
	let receiver_foreign_assets_after =
		assets_balance_on!(PenpalA, system_para_foreign_asset_location.clone(), &receiver);
	// Sender's balance is reduced
	assert!(sender_balance_after < sender_balance_before);
	// Receiver's foreign asset balance is increased
	assert!(receiver_foreign_assets_after > receiver_foreign_assets_before);
	// Receiver's system asset balance increased by `amount_to_send - delivery_fees -
	// bought_execution`; `delivery_fees` might be paid from transfer or JIT, also
	// `bought_execution` is unknown but should be non-zero
	assert!(
		receiver_system_native_assets_after <
			receiver_system_native_assets_before + fee_amount_to_send
	);

	// Sender's asset balance is reduced by exact amount
	assert_eq!(sender_assets_before - asset_amount_to_send, sender_assets_after);
	// Receiver's foreign asset balance is increased by exact amount
	assert_eq!(
		receiver_foreign_assets_after,
		receiver_foreign_assets_before + asset_amount_to_send
	);
}

/// Reserve Transfers of a random asset and native asset from Parachain to Asset Hub should work
/// Receiver is empty account to show deposit works as long as transfer includes enough DOT for ED.
/// Once we have https://github.com/paritytech/polkadot-sdk/issues/5298,
/// we should do equivalent test with USDT instead of DOT.
#[test]
fn reserve_transfer_multiple_assets_from_para_to_asset_hub() {
	// Init values for Parachain
	let destination = PenpalA::sibling_location_of(AssetHubWestend::para_id());
	let sender = PenpalASender::get();
	let fee_amount_to_send = ASSET_HUB_WESTEND_ED * 100;
	let asset_amount_to_send = ASSET_HUB_WESTEND_ED * 100;
	let penpal_asset_owner = PenpalAssetOwner::get();
	let penpal_asset_owner_signer = <PenpalA as Chain>::RuntimeOrigin::signed(penpal_asset_owner);
	let asset_location_on_penpal = PenpalLocalReservableFromAssetHub::get();
	let system_asset_location_on_penpal = RelayLocation::get();
	let assets: Assets = vec![
		(Parent, fee_amount_to_send).into(),
		(asset_location_on_penpal.clone(), asset_amount_to_send).into(),
	]
	.into();
	let fee_asset_index = assets
		.inner()
		.iter()
		.position(|r| r == &(Parent, fee_amount_to_send).into())
		.unwrap() as u32;
	// Fund Parachain's sender account with some foreign assets
	PenpalA::mint_foreign_asset(
		penpal_asset_owner_signer.clone(),
		asset_location_on_penpal.clone(),
		sender.clone(),
		asset_amount_to_send * 2,
	);
	// Fund Parachain's sender account with some system assets
	PenpalA::mint_foreign_asset(
		penpal_asset_owner_signer,
		system_asset_location_on_penpal.clone(),
		sender.clone(),
		fee_amount_to_send * 2,
	);

	// Beneficiary is a new (empty) account
	let receiver: sp_runtime::AccountId32 =
		get_public_from_string_or_panic::<sr25519::Public>(DUMMY_EMPTY).into();
	// Init values for Asset Hub
	let penpal_location_as_seen_by_ahr = AssetHubWestend::sibling_location_of(PenpalA::para_id());
	let sov_penpal_on_ahr =
		AssetHubWestend::sovereign_account_id_of(penpal_location_as_seen_by_ahr);
	let ah_asset_owner = AssetHubWestendAssetOwner::get();
	let ah_asset_owner_signer = <AssetHubWestend as Chain>::RuntimeOrigin::signed(ah_asset_owner);

	// Fund SA-of-Penpal-on-AHR to be able to pay for the fees.
	AssetHubWestend::fund_accounts(vec![(
		sov_penpal_on_ahr.clone().into(),
		ASSET_HUB_WESTEND_ED * 1000,
	)]);
	// Fund SA-of-Penpal-on-AHR to be able to pay for the sent amount.
	AssetHubWestend::mint_asset(
		ah_asset_owner_signer,
		RESERVABLE_ASSET_ID,
		sov_penpal_on_ahr,
		asset_amount_to_send * 2,
	);

	// Init Test
	let para_test_args = TestContext {
		sender: sender.clone(),
		receiver: receiver.clone(),
		args: TestArgs::new_para(
			destination,
			receiver.clone(),
			asset_amount_to_send,
			assets,
			None,
			fee_asset_index,
		),
	};
	let mut test = ParaToSystemParaTest::new(para_test_args);

	// Query initial balances
	let sender_system_assets_before =
		assets_balance_on!(PenpalA, system_asset_location_on_penpal.clone(), &sender);
	let sender_foreign_assets_before =
		assets_balance_on!(PenpalA, asset_location_on_penpal.clone(), &sender);
	let receiver_balance_before = test.receiver.balance;
	let receiver_assets_before = AssetHubWestend::execute_with(|| {
		type Assets = <AssetHubWestend as AssetHubWestendPallet>::Assets;
		<Assets as Inspect<_>>::balance(RESERVABLE_ASSET_ID, &receiver)
	});

	// Set assertions and dispatchables
	test.set_assertion::<PenpalA>(para_to_system_para_assets_sender_assertions);
	test.set_assertion::<AssetHubWestend>(para_to_system_para_assets_receiver_assertions);
	test.set_dispatchable::<PenpalA>(para_to_system_para_reserve_transfer_assets);
	test.assert();

	// Query final balances
	let sender_system_assets_after =
		assets_balance_on!(PenpalA, system_asset_location_on_penpal, &sender);
	let sender_foreign_assets_after =
		assets_balance_on!(PenpalA, asset_location_on_penpal, &sender);
	let receiver_balance_after = test.receiver.balance;
	let receiver_assets_after = AssetHubWestend::execute_with(|| {
		type Assets = <AssetHubWestend as AssetHubWestendPallet>::Assets;
		<Assets as Inspect<_>>::balance(RESERVABLE_ASSET_ID, &receiver)
	});
	// Sender's system asset balance is reduced
	assert!(sender_system_assets_after < sender_system_assets_before);
	// Receiver's balance is increased
	assert!(receiver_balance_after > receiver_balance_before);
	// Receiver's balance increased by `amount_to_send - delivery_fees - bought_execution`;
	// `delivery_fees` might be paid from transfer or JIT, also `bought_execution` is unknown but
	// should be non-zero
	assert!(receiver_balance_after < receiver_balance_before + fee_amount_to_send);

	// Sender's asset balance is reduced by exact amount
	assert_eq!(sender_foreign_assets_before - asset_amount_to_send, sender_foreign_assets_after);
	// Receiver's foreign asset balance is increased by exact amount
	assert_eq!(receiver_assets_after, receiver_assets_before + asset_amount_to_send);
}

// =========================================================================
// ===== Reserve Transfers - Native Asset - Parachain<>Relay<>Parachain ====
// =========================================================================
/// Reserve Transfers of native asset from Parachain to Parachain (through Relay reserve) should
/// work
#[test]
fn reserve_transfer_native_asset_from_para_to_para_through_relay() {
	// Init values for Parachain Origin
	let destination = PenpalA::sibling_location_of(PenpalB::para_id());
	let sender = PenpalASender::get();
	let amount_to_send: Balance = WESTEND_ED * 10000;
	let asset_owner = PenpalAssetOwner::get();
	let assets = (Parent, amount_to_send).into();
	let relay_native_asset_location = RelayLocation::get();
	let sender_as_seen_by_relay = Westend::child_location_of(PenpalA::para_id());
	let sov_of_sender_on_relay = Westend::sovereign_account_id_of(sender_as_seen_by_relay);

	// fund Parachain's sender account
	PenpalA::mint_foreign_asset(
		<PenpalA as Chain>::RuntimeOrigin::signed(asset_owner),
		relay_native_asset_location.clone(),
		sender.clone(),
		amount_to_send * 2,
	);

	// fund the Parachain Origin's SA on Relay Chain with the native tokens held in reserve
	Westend::fund_accounts(vec![(sov_of_sender_on_relay.into(), amount_to_send * 2)]);

	// Init values for Parachain Destination
	let receiver = PenpalBReceiver::get();

	// Init Test
	let test_args = TestContext {
		sender: sender.clone(),
		receiver: receiver.clone(),
		args: TestArgs::new_para(destination, receiver.clone(), amount_to_send, assets, None, 0),
	};
	let mut test = ParaToParaThroughRelayTest::new(test_args);

	// Query initial balances
	let sender_assets_before =
		assets_balance_on!(PenpalA, relay_native_asset_location.clone(), &sender);
	let receiver_assets_before =
		assets_balance_on!(PenpalB, relay_native_asset_location.clone(), &receiver);

	// Set assertions and dispatchables
	test.set_assertion::<PenpalA>(para_to_para_through_hop_sender_assertions);
	test.set_assertion::<Westend>(para_to_para_relay_hop_assertions);
	test.set_assertion::<PenpalB>(para_to_para_through_hop_receiver_assertions);
	test.set_dispatchable::<PenpalA>(para_to_para_through_relay_limited_reserve_transfer_assets);
	test.assert();

	// Query final balances
	let sender_assets_after =
		assets_balance_on!(PenpalA, relay_native_asset_location.clone(), &sender);
	let receiver_assets_after = assets_balance_on!(PenpalB, relay_native_asset_location, &receiver);

	// Sender's balance is reduced by amount sent (delivery fees are charged in native).
	assert_eq!(sender_assets_after, sender_assets_before - amount_to_send);
	// Receiver's balance is increased by `amount_to_send` minus delivery fees.
	assert!(receiver_assets_after > receiver_assets_before);
	assert!(receiver_assets_after < receiver_assets_before + amount_to_send);
}

// ============================================================================
// ==== Reserve Transfers USDT - AssetHub->Parachain - pay fees using pool ====
// ============================================================================
#[test]
fn reserve_transfer_usdt_from_asset_hub_to_para() {
	let usdt_id = 1984u32;
	let penpal_location = AssetHubWestend::sibling_location_of(PenpalA::para_id());
	let penpal_sov_account = AssetHubWestend::sovereign_account_id_of(penpal_location.clone());

	// Create SA-of-Penpal-on-AHW with ED.
	// This ED isn't reflected in any derivative in a PenpalA account.
	AssetHubWestend::fund_accounts(vec![(penpal_sov_account.clone().into(), ASSET_HUB_WESTEND_ED)]);

	let sender = AssetHubWestendSender::get();
	let receiver = PenpalAReceiver::get();
	let asset_amount_to_send = 1_000_000_000_000;

	AssetHubWestend::execute_with(|| {
		use frame_support::traits::tokens::fungibles::Mutate;
		type Assets = <AssetHubWestend as AssetHubWestendPallet>::Assets;
		assert_ok!(<Assets as Mutate<_>>::mint_into(
			usdt_id.into(),
			&AssetHubWestendSender::get(),
			asset_amount_to_send + 10_000_000_000_000, // Make sure it has enough.
		));
	});

	let usdt_from_asset_hub = PenpalUsdtFromAssetHub::get();
	// Setup the pool between `relay_asset_penpal_pov` and `usdt_from_asset_hub` on PenpalA.
	// So we can swap the custom asset that comes from AssetHubWestend for native asset to pay for
	// fees.
	create_foreign_pool_with_native_on!(
		PenpalA,
		PenpalUsdtFromAssetHub::get(),
		PenpalAssetOwner::get()
	);

	let assets: Assets = vec![(
		[PalletInstance(ASSETS_PALLET_ID), GeneralIndex(usdt_id.into())],
		asset_amount_to_send,
	)
		.into()]
	.into();

	let test_args = TestContext {
		sender: sender.clone(),
		receiver: receiver.clone(),
		args: TestArgs::new_para(
			penpal_location,
			receiver.clone(),
			asset_amount_to_send,
			assets,
			None,
			0,
		),
	};
	let mut test = SystemParaToParaTest::new(test_args);

	let sender_initial_balance = AssetHubWestend::execute_with(|| {
		type Assets = <AssetHubWestend as AssetHubWestendPallet>::Assets;
		<Assets as Inspect<_>>::balance(usdt_id, &sender)
	});
	let sender_initial_native_balance = AssetHubWestend::execute_with(|| {
		type Balances = <AssetHubWestend as AssetHubWestendPallet>::Balances;
		Balances::free_balance(&sender)
	});
	let receiver_initial_balance =
		assets_balance_on!(PenpalA, usdt_from_asset_hub.clone(), &receiver);
	let penpal_usdt_issuance_before = assets_issuance_on!(PenpalA, usdt_from_asset_hub.clone());
	let ah_usdt_issuance_before = assets_issuance_on!(AssetHubWestend, usdt_id);

	test.set_assertion::<AssetHubWestend>(system_para_to_para_sender_assertions);
	test.set_assertion::<PenpalA>(system_para_to_penpal_receiver_assertions);
	test.set_dispatchable::<AssetHubWestend>(system_para_to_para_reserve_transfer_assets);
	test.assert();

	let sender_after_balance = AssetHubWestend::execute_with(|| {
		type Assets = <AssetHubWestend as AssetHubWestendPallet>::Assets;
		<Assets as Inspect<_>>::balance(usdt_id, &sender)
	});
	let sender_after_native_balance = AssetHubWestend::execute_with(|| {
		type Balances = <AssetHubWestend as AssetHubWestendPallet>::Balances;
		Balances::free_balance(&sender)
	});
	let receiver_after_balance =
		assets_balance_on!(PenpalA, usdt_from_asset_hub.clone(), &receiver);
	let penpal_usdt_issuance_after = assets_issuance_on!(PenpalA, usdt_from_asset_hub);
	let ah_usdt_issuance_after = assets_issuance_on!(AssetHubWestend, usdt_id);

	// TODO(https://github.com/paritytech/polkadot-sdk/issues/5160): When we allow payment with
	// different assets locally, this should be the same, since they aren't used for fees.
	assert!(sender_after_native_balance < sender_initial_native_balance);
	// Sender account's balance decreases.
	assert_eq!(sender_after_balance, sender_initial_balance - asset_amount_to_send);
	// Receiver account's balance increases.
	assert!(receiver_after_balance > receiver_initial_balance);
	assert!(receiver_after_balance < receiver_initial_balance + asset_amount_to_send);
	// Penpal mints USDT asset transferred in
	assert_eq!(penpal_usdt_issuance_after, penpal_usdt_issuance_before + asset_amount_to_send);
	// Asset Hub supply doesn't change (assets move to sovereign account)
	assert_eq!(ah_usdt_issuance_after, ah_usdt_issuance_before);
}

// ===================================================================================
// == Reserve Transfers USDT - Parachain->AssetHub->Parachain - pay fees using pool ==
// ===================================================================================
//
// Transfer USDT From Penpal A to Penpal B with AssetHub as the reserve, while paying fees using
// USDT by making use of existing USDT pools on AssetHub and destination.
#[test]
fn reserve_transfer_usdt_from_para_to_para_through_asset_hub() {
	let destination = PenpalA::sibling_location_of(PenpalB::para_id());
	let sender = PenpalASender::get();
	let asset_amount_to_send: Balance = WESTEND_ED * 10000;
	let fee_amount_to_send: Balance = WESTEND_ED * 10000;
	let sender_chain_as_seen_by_asset_hub =
		AssetHubWestend::sibling_location_of(PenpalA::para_id());
	let sov_of_sender_on_asset_hub =
		AssetHubWestend::sovereign_account_id_of(sender_chain_as_seen_by_asset_hub);
	let receiver_as_seen_by_asset_hub = AssetHubWestend::sibling_location_of(PenpalB::para_id());
	let sov_of_receiver_on_asset_hub =
		AssetHubWestend::sovereign_account_id_of(receiver_as_seen_by_asset_hub);

	// Create SA-of-Penpal-on-AHW with ED.
	// This ED isn't reflected in any derivative in a PenpalA account.
	AssetHubWestend::fund_accounts(vec![
		(sov_of_sender_on_asset_hub.clone().into(), ASSET_HUB_WESTEND_ED),
		(sov_of_receiver_on_asset_hub.clone().into(), ASSET_HUB_WESTEND_ED),
	]);

	// Give USDT to sov account of sender.
	let usdt_id: u32 = 1984;
	AssetHubWestend::execute_with(|| {
		use frame_support::traits::tokens::fungibles::Mutate;
		type Assets = <AssetHubWestend as AssetHubWestendPallet>::Assets;
		assert_ok!(<Assets as Mutate<_>>::mint_into(
			usdt_id.into(),
			&sov_of_sender_on_asset_hub.clone().into(),
			asset_amount_to_send + fee_amount_to_send,
		));
	});

	// We create a pool between WND and USDT in AssetHub.
	let usdt = Location::new(
		0,
		[Junction::PalletInstance(ASSETS_PALLET_ID), Junction::GeneralIndex(usdt_id.into())],
	);
	create_pool_with_relay_native_on!(AssetHubWestend, usdt, AssetHubWestendSender::get());
	// We also need a pool between WND and USDT on PenpalB.
	create_foreign_pool_with_native_on!(
		PenpalB,
		PenpalUsdtFromAssetHub::get(),
		PenpalAssetOwner::get()
	);

	let usdt_from_asset_hub = PenpalUsdtFromAssetHub::get();
	PenpalA::mint_foreign_asset(
		<PenpalA as Chain>::RuntimeOrigin::signed(PenpalAssetOwner::get()),
		usdt_from_asset_hub.clone(),
		sender.clone(),
		asset_amount_to_send + fee_amount_to_send,
	);

	// Prepare assets to transfer.
	let assets: Assets =
		(usdt_from_asset_hub.clone(), asset_amount_to_send + fee_amount_to_send).into();
	// Just to be very specific we're not including anything other than USDT.
	assert_eq!(assets.len(), 1);

	// Give the sender enough Relay tokens to pay for local delivery fees.
	// TODO(https://github.com/paritytech/polkadot-sdk/issues/5160): When we support local delivery fee payment in other assets, we don't need this.
	PenpalA::mint_foreign_asset(
		<PenpalA as Chain>::RuntimeOrigin::signed(PenpalAssetOwner::get()),
		RelayLocation::get(),
		sender.clone(),
		10_000_000_000_000, // Large estimate to make sure it works.
	);

	// Init values for Parachain Destination
	let receiver = PenpalBReceiver::get();

	// Init Test
	let fee_asset_index = 0;
	let test_args = TestContext {
		sender: sender.clone(),
		receiver: receiver.clone(),
		args: TestArgs::new_para(
			destination,
			receiver.clone(),
			asset_amount_to_send,
			assets,
			None,
			fee_asset_index,
		),
	};
	let mut test = ParaToParaThroughAHTest::new(test_args);

	// Query initial balances
	let sender_assets_before = assets_balance_on!(PenpalA, usdt_from_asset_hub.clone(), &sender);
	let receiver_assets_before =
		assets_balance_on!(PenpalB, usdt_from_asset_hub.clone(), &receiver);
	let penpal_1_usdt_issuance_before = assets_issuance_on!(PenpalA, usdt_from_asset_hub.clone());
	let ah_usdt_issuance_before = assets_issuance_on!(AssetHubWestend, usdt_id);
	let penpal_2_usdt_issuance_before = assets_issuance_on!(PenpalB, usdt_from_asset_hub.clone());
	test.set_assertion::<PenpalA>(para_to_para_through_hop_sender_assertions);
	test.set_assertion::<AssetHubWestend>(para_to_para_asset_hub_hop_assertions);
	test.set_assertion::<PenpalB>(para_to_para_through_hop_receiver_assertions);
	test.set_dispatchable::<PenpalA>(
		para_to_para_through_asset_hub_limited_reserve_transfer_assets,
	);
	test.assert();

	// Query final balances
	let sender_assets_after = assets_balance_on!(PenpalA, usdt_from_asset_hub.clone(), &sender);
	let receiver_assets_after = assets_balance_on!(PenpalB, usdt_from_asset_hub.clone(), &receiver);
	let penpal_1_usdt_issuance_after = assets_issuance_on!(PenpalA, usdt_from_asset_hub.clone());
	let ah_usdt_issuance_after = assets_issuance_on!(AssetHubWestend, usdt_id);
	let penpal_2_usdt_issuance_after = assets_issuance_on!(PenpalB, usdt_from_asset_hub);

	// Sender's balance is reduced by amount
	assert!(sender_assets_after < sender_assets_before - asset_amount_to_send);
	// Receiver's balance is increased
	assert!(receiver_assets_after > receiver_assets_before);
	// PenpalA burns USDT transferred out
	assert_eq!(
		penpal_1_usdt_issuance_after,
		penpal_1_usdt_issuance_before - asset_amount_to_send - fee_amount_to_send
	);
	// Asset Hub supply doesn't change (assets move between sovereign accounts)
	assert_eq!(ah_usdt_issuance_after, ah_usdt_issuance_before);
	// PenpalB mints USDT asset transferred in (amount plus unspent fees)
	assert!(penpal_2_usdt_issuance_after > penpal_2_usdt_issuance_before + asset_amount_to_send);
	assert!(
		penpal_2_usdt_issuance_after <
			penpal_2_usdt_issuance_before + asset_amount_to_send + fee_amount_to_send
	);
}

/// Reserve Withdraw Native Asset from AssetHub to Parachain fails.
#[test]
fn reserve_withdraw_from_untrusted_reserve_fails() {
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
			bx!(TransferType::DestinationReserve),
			bx!(fee_id.into()),
			bx!(TransferType::DestinationReserve),
			bx!(VersionedXcm::from(Xcm::<()>::new())),
			Unlimited,
		);
		assert_err!(
			result,
			DispatchError::Module(sp_runtime::ModuleError {
				index: 31,
				error: [22, 0, 0, 0],
				message: Some("InvalidAssetUnsupportedReserve")
			})
		);
	});

	// this should also fail
	AssetHubWestend::execute_with(|| {
		let xcm: Xcm<asset_hub_westend_runtime::RuntimeCall> = Xcm(vec![
			WithdrawAsset(assets.into()),
			InitiateReserveWithdraw {
				assets: Wild(All),
				reserve: destination,
				xcm: Xcm::<()>::new(),
			},
		]);
		let result = <AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
			signed_origin,
			bx!(xcm::VersionedXcm::from(xcm)),
			Weight::MAX,
		);
		assert!(result.is_err());
	});
}
