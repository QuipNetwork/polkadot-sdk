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
	imports::*,
	tests::{
		asset_hub_rococo_location, bridged_roc_at_ah_westend, create_foreign_on_ah_westend,
		snowbridge_common::*,
		snowbridge_v2_outbound::{EthereumSystemFrontend, EthereumSystemFrontendCall},
	},
};
use frame_support::traits::fungibles::Mutate;
use xcm::latest::AssetTransferFilter;

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

pub(crate) fn assert_bridge_hub_rococo_message_accepted(expected_processed: bool) {
	BridgeHubRococo::execute_with(|| {
		type RuntimeEvent = <BridgeHubRococo as Chain>::RuntimeEvent;

		if expected_processed {
			assert_expected_events!(
				BridgeHubRococo,
				vec![
					// message exported
					RuntimeEvent::BridgeWestendMessages(
						pallet_bridge_messages::Event::MessageAccepted { .. }
					) => {},
					// message processed successfully
					RuntimeEvent::MessageQueue(
						pallet_message_queue::Event::Processed { success: true, .. }
					) => {},
				]
			);
		} else {
			assert_expected_events!(
				BridgeHubRococo,
				vec![
					RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed {
						success: false,
						..
					}) => {},
				]
			);
		}
	});
}

pub(crate) fn assert_bridge_hub_westend_message_received() {
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			BridgeHubWestend,
			vec![
				// message sent to destination
				RuntimeEvent::XcmpQueue(
					cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }
				) => {},
			]
		);
	})
}

#[test]
fn send_roc_from_asset_hub_rococo_to_ethereum() {
	let initial_fund: u128 = 200_000_000_000_000;
	let initial_liquidity: u128 = initial_fund / 2;
	let amount: u128 = initial_fund;
	let roc_fee_amount: u128 = initial_liquidity / 2;
	let wnd_amount_to_swap: u128 = initial_liquidity / 10;
	let wnd_fee_amount: u128 = wnd_amount_to_swap / 10;

	let ether_fee_amount: u128 = 4_000_000;

	let sender = AssetHubRococoSender::get();
	let roc_at_asset_hub_rococo = roc_at_ah_rococo();
	let bridged_roc_at_asset_hub_westend = bridged_roc_at_ah_westend();

	create_foreign_on_ah_westend(
		bridged_roc_at_asset_hub_westend.clone(),
		true,
		vec![(asset_hub_rococo_location(), false).into()],
		vec![],
	);
	set_up_pool_with_wnd_on_ah_westend(
		bridged_roc_at_asset_hub_westend.clone(),
		true,
		initial_fund,
		initial_liquidity,
	);
	let previous_owner = snowbridge_sovereign();
	AssetHubWestend::execute_with(|| {
		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::start_destroy(
			<AssetHubWestend as Chain>::RuntimeOrigin::signed(previous_owner),
			ethereum()
		));
		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::finish_destroy(
			<AssetHubWestend as Chain>::RuntimeOrigin::signed(AssetHubWestend::account_id_of(
				ALICE
			)),
			ethereum()
		));
	});
	create_foreign_on_ah_westend(ethereum(), true, vec![(ethereum(), false).into()], vec![]);
	set_up_pool_with_wnd_on_ah_westend(ethereum(), true, initial_fund, initial_liquidity);
	BridgeHubRococo::fund_para_sovereign(AssetHubRococo::para_id(), initial_fund);
	AssetHubRococo::fund_accounts(vec![(AssetHubRococoSender::get(), initial_fund)]);
	fund_on_bh();
	register_roc_on_bh();

	// set XCM versions
	AssetHubRococo::force_xcm_version(asset_hub_westend_global_location(), XCM_VERSION);
	BridgeHubRococo::force_xcm_version(bridge_hub_westend_location(), XCM_VERSION);

	// send ROCs, use them for fees
	let local_fee_asset: Asset = (roc_at_asset_hub_rococo.clone(), roc_fee_amount).into();
	let remote_fee_on_westend: Asset = (roc_at_asset_hub_rococo.clone(), roc_fee_amount).into();
	let assets: Assets = (roc_at_asset_hub_rococo.clone(), amount).into();
	let reserved_asset_on_westend: Asset =
		(roc_at_asset_hub_rococo.clone(), amount - roc_fee_amount * 2).into();
	let reserved_asset_on_westend_reanchored: Asset =
		(bridged_roc_at_asset_hub_westend.clone(), (amount - roc_fee_amount * 2) / 2).into();

	let xcm = VersionedXcm::from(Xcm(vec![
		WithdrawAsset(assets.clone().into()),
		PayFees { asset: local_fee_asset.clone() },
		InitiateTransfer {
			destination: asset_hub_westend_global_location(),
			remote_fees: Some(AssetTransferFilter::ReserveDeposit(Definite(
				remote_fee_on_westend.clone().into(),
			))),
			preserve_origin: true,
			assets: BoundedVec::truncate_from(vec![AssetTransferFilter::ReserveDeposit(Definite(
				reserved_asset_on_westend.clone().into(),
			))]),
			remote_xcm: Xcm(vec![
				// swap from roc to wnd
				ExchangeAsset {
					give: Definite(reserved_asset_on_westend_reanchored.clone().into()),
					want: (Parent, wnd_amount_to_swap).into(),
					maximal: true,
				},
				// swap some wnd to ether
				ExchangeAsset {
					give: Definite((Parent, ether_fee_amount * 2).into()),
					want: (ethereum(), ether_fee_amount).into(),
					maximal: true,
				},
				PayFees { asset: (Parent, wnd_fee_amount).into() },
				InitiateTransfer {
					destination: ethereum(),
					remote_fees: Some(AssetTransferFilter::ReserveWithdraw(Definite(
						Asset { id: AssetId(ethereum()), fun: Fungible(ether_fee_amount) }.into(),
					))),
					preserve_origin: true,
					assets: BoundedVec::truncate_from(vec![AssetTransferFilter::ReserveDeposit(
						Definite(reserved_asset_on_westend_reanchored.clone().into()),
					)]),
					remote_xcm: Xcm(vec![DepositAsset {
						assets: Wild(All),
						beneficiary: beneficiary(),
					}]),
				},
			]),
		},
	]));

	let _ = AssetHubRococo::execute_with(|| {
		<AssetHubRococo as AssetHubRococoPallet>::PolkadotXcm::execute(
			<AssetHubRococo as Chain>::RuntimeOrigin::signed(sender),
			bx!(xcm),
			Weight::from(EXECUTION_WEIGHT),
		)
	});

	assert_bridge_hub_rococo_message_accepted(true);
	assert_bridge_hub_westend_message_received();

	// verify expected events on final destination
	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![
				// message processed successfully
				RuntimeEvent::MessageQueue(
					pallet_message_queue::Event::Processed { success: true, .. }
				) => {},
			]
		);
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		// Check that the Ethereum message was queue in the Outbound Queue
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueueV2(snowbridge_pallet_outbound_queue_v2::Event::MessageQueued{ .. }) => {},]
		);
	});
}

#[test]
fn register_rococo_asset_on_ethereum_from_rah() {
	const XCM_FEE: u128 = 4_000_000_000_000;
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

	AssetHubWestend::force_create_foreign_asset(
		bridged_asset_at_wah.clone(),
		sa_of_rah_on_wah.clone(),
		true,
		ASSET_MIN_BALANCE,
		vec![],
	);

	let fee_asset = Asset { id: AssetId(ethereum()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) };

	let call =
		EthereumSystemFrontend::EthereumSystemFrontend(EthereumSystemFrontendCall::RegisterToken {
			asset_id: Box::new(VersionedLocation::from(bridged_asset_at_wah.clone())),
			metadata: Default::default(),
			fee_asset,
		})
		.encode();

	let origin_kind = OriginKind::Xcm;
	let fee_amount = XCM_FEE;
	let fees = (Parent, fee_amount).into();

	let xcm = xcm_transact_paid_execution(call.into(), origin_kind, fees, sa_of_rah_on_wah.clone());

	// SA-of-RAH-on-WAH needs to have balance to pay for fees and asset creation deposit
	AssetHubWestend::execute_with(|| {
		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::ForeignAssets::mint_into(
			ethereum().try_into().unwrap(),
			&sa_of_rah_on_wah,
			INITIAL_FUND,
		));
		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::Balances::force_set_balance(
			<AssetHubWestend as Chain>::RuntimeOrigin::root(),
			sa_of_rah_on_wah.into(),
			INITIAL_FUND
		));
	});

	let destination = asset_hub_westend_global_location();

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
		AssetHubWestend::assert_xcmp_queue_success(None);
	});
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		// Check that the Ethereum message was queue in the Outbound Queue
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueueV2(snowbridge_pallet_outbound_queue_v2::Event::MessageQueued{ .. }) => {},]
		);
	});
}
