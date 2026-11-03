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

use crate::tests::{snowbridge_common::snowbridge_sovereign, *};
use emulated_integration_tests_common::assets_balance_on;
use sp_core::Get;
use xcm::latest::AssetTransferFilter;

const ETHEREUM_BOB: [u8; 20] = hex_literal::hex!("11b0b11000011b0b11000011b0b11000011b0b11");

/// Bob on Ethereum transacts on PenpalB, paying fees using WETH. XCM has to go through Asset Hub
/// as the reserve location of WETH. The original origin `Ethereum/Bob` is proxied by Asset Hub.
///
/// This particular test is not testing snowbridge, but only Bridge Hub, so the tested XCM flow from
/// Ethereum starts from Bridge Hub.
// TODO(https://github.com/paritytech/polkadot-sdk/issues/6243): Once Snowbridge supports Transact, start the flow from Ethereum and test completely e2e.
fn transfer_and_transact_in_same_xcm(
	sender: Location,
	weth: Asset,
	destination: Location,
	beneficiary: Location,
	call: xcm::DoubleEncoded<()>,
) {
	let signed_origin = <BridgeHubWestend as Chain>::RuntimeOrigin::root();
	let context: InteriorLocation = [
		GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
		Parachain(<BridgeHubWestend as Para>::ParachainInfo::get().into()),
	]
	.into();
	let asset_hub_location = BridgeHubWestend::sibling_location_of(AssetHubWestend::para_id());

	// TODO(https://github.com/paritytech/polkadot-sdk/issues/6197): dry-run to get local fees, for now use hardcoded value.
	let ah_fees_amount = 90_000_000_000u128; // current exact value 79_948_099_299
	let fees_for_ah: Asset = (weth.id.clone(), ah_fees_amount).into();

	// xcm to be executed at dest
	let xcm_on_dest = Xcm(vec![
		Transact { origin_kind: OriginKind::Xcm, call, fallback_max_weight: None },
		ExpectTransactStatus(MaybeErrorCode::Success),
		// since this is the last hop, we don't need to further use any assets previously
		// reserved for fees (there are no further hops to cover delivery fees for); we
		// RefundSurplus to get back any unspent fees
		RefundSurplus,
		DepositAsset { assets: Wild(All), beneficiary },
	]);
	let destination = destination.reanchored(&asset_hub_location, &context).unwrap();
	let xcm_to_ah = Xcm::<()>(vec![
		UnpaidExecution { check_origin: None, weight_limit: Unlimited },
		DescendOrigin([PalletInstance(80)].into()), // snowbridge pallet
		UniversalOrigin(GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })),
		ReserveAssetDeposited(weth.clone().into()),
		AliasOrigin(sender),
		PayFees { asset: fees_for_ah },
		InitiateTransfer {
			destination,
			// on the last hop we can just put everything in fees and `RefundSurplus` to get any
			// unused back
			remote_fees: Some(AssetTransferFilter::ReserveDeposit(Wild(All))),
			preserve_origin: true,
			assets: BoundedVec::new(),
			remote_xcm: xcm_on_dest,
		},
	]);
	<BridgeHubWestend as BridgeHubWestendPallet>::PolkadotXcm::send(
		signed_origin,
		bx!(asset_hub_location.into()),
		bx!(xcm::VersionedXcm::from(xcm_to_ah.into())),
	)
	.unwrap();
}

/// Bob on Ethereum transacts on PenpalB, paying fees using WETH. XCM has to go through Asset Hub
/// as the reserve location of WETH. The original origin `Ethereum/Bob` is proxied by Asset Hub.
///
/// This particular test is not testing snowbridge, but only Bridge Hub, so the tested XCM flow from
/// Ethereum starts from Bridge Hub.
// TODO(https://github.com/paritytech/polkadot-sdk/issues/6243): Once Snowbridge supports Transact, start the flow from Ethereum and test completely e2e.
#[test]
fn transact_from_ethereum_to_penpalb_through_asset_hub() {
	// Snowbridge doesn't support transact yet, we are emulating it by sending one from Bridge Hub
	// as if it comes from Snowbridge.
	let destination = BridgeHubWestend::sibling_location_of(PenpalB::para_id());
	let sender = Location::new(
		2,
		[
			GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID }),
			AccountKey20 { network: None, key: ETHEREUM_BOB },
		],
	);

	let bridged_weth = weth_at_asset_hubs();
	PenpalB::force_create_foreign_asset(
		bridged_weth.clone(),
		PenpalAssetOwner::get(),
		true,
		ASSET_MIN_BALANCE,
		vec![],
	);
	// Configure source Penpal chain to trust local AH as reserve of bridged WETH
	PenpalB::execute_with(|| {
		assert_ok!(<PenpalB as Chain>::System::set_storage(
			<PenpalB as Chain>::RuntimeOrigin::root(),
			vec![(
				PenpalCustomizableAssetFromSystemAssetHub::key().to_vec(),
				bridged_weth.encode(),
			)],
		));
	});

	let fee_amount_to_send: parachains_common::Balance = ASSET_HUB_WESTEND_ED * 10000;
	let sender_chain_as_seen_by_asset_hub =
		Location::new(2, [GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })]);

	let sov_of_sender_on_asset_hub = AssetHubWestend::execute_with(|| {
		AssetHubWestend::sovereign_account_id_of(sender_chain_as_seen_by_asset_hub)
	});
	let receiver_as_seen_by_asset_hub = AssetHubWestend::sibling_location_of(PenpalB::para_id());
	let sov_of_receiver_on_asset_hub = AssetHubWestend::execute_with(|| {
		AssetHubWestend::sovereign_account_id_of(receiver_as_seen_by_asset_hub)
	});
	// Create SAs of sender and receiver on AHW with ED.
	AssetHubWestend::fund_accounts(vec![
		(sov_of_sender_on_asset_hub.clone().into(), ASSET_HUB_WESTEND_ED),
		(sov_of_receiver_on_asset_hub.clone().into(), ASSET_HUB_WESTEND_ED),
		(snowbridge_sovereign().into(), 10_000_000_000_00),
	]);

	// We create a pool between WND and WETH in AssetHub to support paying for fees with WETH.
	let snowbridge_sovereign = snowbridge_sovereign();
	create_foreign_pool_with_parent_native_on!(
		AssetHubWestend,
		bridged_weth.clone(),
		snowbridge_sovereign.clone(),
		1_000_000_000_000,
		20_000_000_000
	);
	// We also need a pool between PEN and WETH on PenpalB to support paying for fees with WETH.
	create_foreign_pool_with_native_on!(
		PenpalB,
		bridged_weth.clone(),
		PenpalAssetOwner::get(),
		1_000_000_000_000,
		20_000_000_000
	);

	// Init values for Parachain Destination
	let receiver = PenpalBReceiver::get();

	// Query initial balances
	let receiver_assets_before = assets_balance_on!(PenpalB, bridged_weth.clone(), &receiver);

	// Now register a new asset on PenpalB from Ethereum/Bob account while paying fees using WETH
	// (going through Asset Hub)
	let weth_to_send: Asset = (bridged_weth.clone(), fee_amount_to_send).into();
	// Silly example of a Transact: Bob creates his own foreign assset on PenpalB based on his
	// Ethereum address
	let foreign_asset_at_penpal_b = Location::new(
		2,
		[
			GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID }),
			AccountKey20 { network: None, key: ETHEREUM_BOB },
		],
	);
	// Encoded `create_asset` call to be executed in PenpalB
	let call = PenpalB::create_foreign_asset_call(
		foreign_asset_at_penpal_b.clone(),
		ASSET_MIN_BALANCE,
		receiver.clone(),
	);
	BridgeHubWestend::execute_with(|| {
		// initiate transaction
		transfer_and_transact_in_same_xcm(
			sender.clone(),
			weth_to_send,
			destination,
			receiver.clone().into(),
			call,
		);
	});
	AssetHubWestend::execute_with(|| {
		asset_hub_hop_assertions();
	});
	PenpalB::execute_with(|| {
		let expected_creator = PenpalB::sovereign_account_id_of(sender);
		penpal_b_assertions(foreign_asset_at_penpal_b, expected_creator, receiver.clone());
	});

	// Query final balances
	let receiver_assets_after = assets_balance_on!(PenpalB, bridged_weth, &receiver);
	// Receiver's balance is increased
	assert!(receiver_assets_after > receiver_assets_before);
}

fn asset_hub_hop_assertions() {
	type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
	assert_expected_events!(
		AssetHubWestend,
		vec![
			// Deposited to receiver parachain SA
			RuntimeEvent::ForeignAssets(
				pallet_assets::Event::Deposited { .. }
			) => {},
			RuntimeEvent::MessageQueue(
				pallet_message_queue::Event::Processed { success: true, .. }
			) => {},
		]
	);
}

fn penpal_b_assertions(
	expected_asset: Location,
	expected_creator: AccountId,
	expected_owner: AccountId,
) {
	type RuntimeEvent = <PenpalB as Chain>::RuntimeEvent;
	PenpalB::assert_xcmp_queue_success(None);
	assert_expected_events!(
		PenpalB,
		vec![
			RuntimeEvent::Assets(
				pallet_assets::Event::Created { asset_id, creator, owner }
			) => {
				asset_id: *asset_id == expected_asset,
				creator: *creator == expected_creator,
				owner: *owner == expected_owner,
			},
		]
	);
}
