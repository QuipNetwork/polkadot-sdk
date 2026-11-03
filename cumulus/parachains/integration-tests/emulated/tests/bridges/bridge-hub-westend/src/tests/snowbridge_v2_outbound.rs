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
	tests::{snowbridge_common::*, usdt_at_ah_westend},
};
use bridge_hub_westend_runtime::{
	bridge_to_ethereum_config::EthereumGatewayAddress, EthereumOutboundQueueV2,
};
use frame_support::{assert_err_ignore_postinfo, pallet_prelude::TypeInfo};
use snowbridge_core::{reward::MessageId, AssetMetadata, BasicOperatingMode};
use snowbridge_outbound_queue_primitives::v2::{ContractCall, DeliveryReceipt};
use snowbridge_pallet_outbound_queue_v2::Error;
use snowbridge_pallet_system_v2::LostTips;
use sp_core::H256;
use xcm::v5::AssetTransferFilter;

#[derive(Encode, Decode, Debug, PartialEq, Clone, TypeInfo)]
pub enum EthereumSystemFrontendCall {
	#[codec(index = 1)]
	RegisterToken { asset_id: Box<VersionedLocation>, metadata: AssetMetadata, fee_asset: Asset },
}

#[allow(clippy::large_enum_variant)]
#[derive(Encode, Decode, Debug, PartialEq, Clone, TypeInfo)]
pub enum EthereumSystemFrontend {
	#[codec(index = 36)]
	EthereumSystemFrontend(EthereumSystemFrontendCall),
}

#[test]
fn send_weth_from_asset_hub_to_ethereum() {
	fund_on_bh();

	register_assets_on_ah();

	fund_on_ah();

	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		let local_fee_asset =
			Asset { id: AssetId(Location::parent()), fun: Fungible(LOCAL_FEE_AMOUNT_IN_DOT) };

		let remote_fee_asset =
			Asset { id: AssetId(ethereum()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) };

		let reserve_asset = Asset { id: AssetId(weth_location()), fun: Fungible(TOKEN_AMOUNT) };

		let assets = vec![reserve_asset.clone(), remote_fee_asset.clone(), local_fee_asset.clone()];

		let xcm = VersionedXcm::from(Xcm(vec![
			WithdrawAsset(assets.clone().into()),
			PayFees { asset: local_fee_asset.clone() },
			InitiateTransfer {
				destination: ethereum(),
				remote_fees: Some(AssetTransferFilter::ReserveWithdraw(Definite(
					remote_fee_asset.clone().into(),
				))),
				preserve_origin: true,
				assets: BoundedVec::truncate_from(vec![AssetTransferFilter::ReserveWithdraw(
					Definite(reserve_asset.clone().into()),
				)]),
				remote_xcm: Xcm(vec![DepositAsset {
					assets: Wild(AllCounted(2)),
					beneficiary: beneficiary(),
				}]),
			},
		]));

		// Send the Weth back to Ethereum
		<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
			RuntimeOrigin::signed(AssetHubWestendReceiver::get()),
			bx!(xcm),
			Weight::from(EXECUTION_WEIGHT),
		)
		.unwrap();
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		// Check that the Ethereum message was queue in the Outbound Queue
		assert_expected_events!(
			BridgeHubWestend,
			vec![
				RuntimeEvent::EthereumOutboundQueueV2(snowbridge_pallet_outbound_queue_v2::Event::MessageQueued{ .. }) => {},
			]
		);

		let relayer = BridgeHubWestendSender::get();
		let reward_account = AssetHubWestendReceiver::get();
		let receipt = DeliveryReceipt {
			gateway: EthereumGatewayAddress::get(),
			nonce: 1,
			reward_address: reward_account.into(),
			topic: H256::zero(),
			success: true,
		};

		// Submit a delivery receipt
		assert_ok!(EthereumOutboundQueueV2::process_delivery_receipt(relayer, receipt));

		assert_expected_events!(
			BridgeHubWestend,
			vec![
				RuntimeEvent::BridgeRelayers(pallet_bridge_relayers::Event::RewardRegistered { .. }) => {},
			]
		);
	});
}

#[test]
pub fn register_relay_token_from_asset_hub_with_sudo() {
	fund_on_bh();
	register_assets_on_ah();
	fund_on_ah();
	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		let fees_asset = Asset { id: AssetId(ethereum()), fun: Fungible(1) };

		assert_ok!(
			<AssetHubWestend as AssetHubWestendPallet>::SnowbridgeSystemFrontend::register_token(
				RuntimeOrigin::root(),
				bx!(VersionedLocation::from(Location { parents: 1, interior: [].into() })),
				AssetMetadata {
					name: "wnd".as_bytes().to_vec().try_into().unwrap(),
					symbol: "wnd".as_bytes().to_vec().try_into().unwrap(),
					decimals: 12,
				},
				fees_asset
			)
		);
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueueV2(snowbridge_pallet_outbound_queue_v2::Event::MessageQueued{ .. }) => {},]
		);
	});
}

#[test]
pub fn register_usdt_from_owner_on_asset_hub() {
	fund_on_bh();
	register_assets_on_ah();
	fund_on_ah();
	set_up_eth_and_dot_pool();
	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		let fees_asset =
			Asset { id: AssetId(Location::parent()), fun: Fungible(1_000_000_000u128) };

		assert_ok!(
			<AssetHubWestend as AssetHubWestendPallet>::SnowbridgeSystemFrontend::register_token(
				RuntimeOrigin::signed(AssetHubWestendAssetOwner::get()),
				bx!(VersionedLocation::from(usdt_at_ah_westend())),
				AssetMetadata {
					name: "usdt".as_bytes().to_vec().try_into().unwrap(),
					symbol: "usdt".as_bytes().to_vec().try_into().unwrap(),
					decimals: 6,
				},
				fees_asset
			)
		);
		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::AssetConversion(pallet_asset_conversion::Event::SwapExecuted { .. }) => {},]
		);
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueueV2(snowbridge_pallet_outbound_queue_v2::Event::MessageQueued{ .. }) => {},]
		);
	});
}

#[test]
pub fn add_tip_from_asset_hub_user_origin() {
	fund_on_bh();
	register_assets_on_ah();
	fund_on_ah();
	set_up_eth_and_dot_pool();
	let relayer = AssetHubWestendSender::get();

	// Fund the relayer account to pay xcm delivery fees from AH -> BH.
	AssetHubWestend::fund_accounts(vec![(relayer.clone(), INITIAL_FUND)]);

	// Send a message from AH to Ethereum to increase the nonce
	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		let local_fee_asset =
			Asset { id: AssetId(Location::parent()), fun: Fungible(LOCAL_FEE_AMOUNT_IN_DOT) };
		let remote_fee_asset =
			Asset { id: AssetId(ethereum()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) };
		let reserve_asset = Asset { id: AssetId(weth_location()), fun: Fungible(TOKEN_AMOUNT) };
		let assets = vec![reserve_asset.clone(), remote_fee_asset.clone(), local_fee_asset.clone()];

		let xcm = VersionedXcm::from(Xcm(vec![
			WithdrawAsset(assets.clone().into()),
			PayFees { asset: local_fee_asset.clone() },
			InitiateTransfer {
				destination: ethereum(),
				remote_fees: Some(AssetTransferFilter::ReserveWithdraw(Definite(
					remote_fee_asset.clone().into(),
				))),
				preserve_origin: true,
				assets: BoundedVec::truncate_from(vec![AssetTransferFilter::ReserveWithdraw(
					Definite(reserve_asset.clone().into()),
				)]),
				remote_xcm: Xcm(vec![DepositAsset {
					assets: Wild(AllCounted(2)),
					beneficiary: beneficiary(),
				}]),
			},
		]));

		// Send the Weth back to Ethereum
		<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
			RuntimeOrigin::signed(AssetHubWestendReceiver::get()),
			bx!(xcm),
			Weight::from(EXECUTION_WEIGHT),
		)
		.unwrap();
	});

	// Add the tip.
	let tip_message_id = MessageId::Outbound(1);

	let dot = Location::new(1, Here);
	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::SnowbridgeSystemFrontend::add_tip(
			RuntimeOrigin::signed(relayer.clone()),
			tip_message_id.clone(),
			xcm::prelude::Asset::from((dot, 1_000_000_000u128)),
		));
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		let events = BridgeHubWestend::events();
		assert!(
			events.iter().any(|event| matches!(
				event,
				RuntimeEvent::EthereumSystemV2(snowbridge_pallet_system_v2::Event::TipProcessed { sender, message_id, success, ..})
					if *sender == relayer && *message_id == tip_message_id.clone() && *success, // expect success
			)),
			"tip added event found"
		);
	});
}

#[test]
pub fn tip_to_invalid_nonce_is_added_to_lost_tips() {
	fund_on_bh();
	register_assets_on_ah();
	fund_on_ah();
	set_up_eth_and_dot_pool();
	let relayer = AssetHubWestendSender::get();

	AssetHubWestend::fund_accounts(vec![(relayer.clone(), INITIAL_FUND)]);

	// A nonce that does not exist.
	let tip_message_id = MessageId::Outbound(22);

	let dot = Location::new(1, Here);
	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::SnowbridgeSystemFrontend::add_tip(
			RuntimeOrigin::signed(relayer.clone()),
			tip_message_id.clone(),
			xcm::prelude::Asset::from((dot, 1_000_000_000u128)),
		));
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		let events = BridgeHubWestend::events();
		assert!(
			events.iter().any(|event| matches!(
				event,
				RuntimeEvent::EthereumSystemV2(snowbridge_pallet_system_v2::Event::TipProcessed { sender, message_id, success, ..})
					if *sender == relayer && *message_id == tip_message_id.clone() && !(*success), // expect a failure
			)),
			"tip added event found"
		);

		let relayer_lost_tip = LostTips::<bridge_hub_westend_runtime::Runtime>::get::<
			sp_runtime::AccountId32,
		>(relayer.into());
		// Assert a tip was added to storage.
		assert!(relayer_lost_tip > 0);
	});
}

#[test]
fn transfer_relay_token_from_ah() {
	let ethereum_sovereign: AccountId = snowbridge_sovereign();

	fund_on_bh();

	// register token in either of the follow way should work
	// a. register_relay_token_on_bh();
	// b. register_relay_token_from_asset_hub_with_sudo();
	// c. register_relay_token_from_asset_hub_user_origin();
	register_relay_token_on_bh();

	register_assets_on_ah();

	fund_on_ah();

	// Send token to Ethereum
	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		let local_fee_asset =
			Asset { id: AssetId(Location::parent()), fun: Fungible(LOCAL_FEE_AMOUNT_IN_DOT) };
		let remote_fee_asset =
			Asset { id: AssetId(ethereum()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) };

		let assets = vec![
			Asset {
				id: AssetId(Location::parent()),
				fun: Fungible(TOKEN_AMOUNT + LOCAL_FEE_AMOUNT_IN_DOT),
			},
			remote_fee_asset.clone(),
		];

		let xcm = VersionedXcm::from(Xcm(vec![
			WithdrawAsset(assets.clone().into()),
			PayFees { asset: local_fee_asset.clone() },
			InitiateTransfer {
				destination: ethereum(),
				remote_fees: Some(AssetTransferFilter::ReserveWithdraw(Definite(
					remote_fee_asset.clone().into(),
				))),
				preserve_origin: true,
				assets: BoundedVec::truncate_from(vec![AssetTransferFilter::ReserveDeposit(
					Definite(
						Asset { id: AssetId(Location::parent()), fun: Fungible(TOKEN_AMOUNT) }
							.into(),
					),
				)]),
				remote_xcm: Xcm(vec![DepositAsset {
					assets: Wild(AllCounted(2)),
					beneficiary: beneficiary(),
				}]),
			},
		]));

		// Send DOT to Ethereum
		<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
			RuntimeOrigin::signed(AssetHubWestendSender::get()),
			bx!(xcm),
			Weight::from(EXECUTION_WEIGHT),
		)
		.unwrap();

		// Check that the native asset transferred to some reserved account(sovereign of Ethereum)
		let events = AssetHubWestend::events();
		assert!(
			events.iter().any(|event| matches!(
				event,
				RuntimeEvent::Balances(pallet_balances::Event::Deposit { who, amount})
					if *who == ethereum_sovereign.clone() && *amount == TOKEN_AMOUNT,
			)),
			"native token reserved to Ethereum sovereign account."
		);
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		// Check that the Ethereum message was queue in the Outbound Queue
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueueV2(snowbridge_pallet_outbound_queue_v2::Event::MessageQueued{ .. }) => {},]
		);

		let relayer = BridgeHubWestendSender::get();
		let reward_account = AssetHubWestendReceiver::get();
		let receipt = DeliveryReceipt {
			gateway: EthereumGatewayAddress::get(),
			nonce: 1,
			reward_address: reward_account.into(),
			topic: H256::zero(),
			success: true,
		};

		// Submit a delivery receipt
		assert_ok!(EthereumOutboundQueueV2::process_delivery_receipt(relayer, receipt));

		assert_expected_events!(
			BridgeHubWestend,
			vec![
				RuntimeEvent::BridgeRelayers(pallet_bridge_relayers::Event::RewardRegistered { .. }) => {},
			]
		);
	});
}

#[test]
fn send_weth_and_dot_from_asset_hub_to_ethereum() {
	fund_on_bh();

	register_relay_token_on_bh();

	register_assets_on_ah();

	fund_on_ah();

	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		let local_fee_asset =
			Asset { id: AssetId(Location::parent()), fun: Fungible(LOCAL_FEE_AMOUNT_IN_DOT) };
		let remote_fee_asset =
			Asset { id: AssetId(ethereum()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) };

		let weth_asset = Asset { id: weth_location().into(), fun: Fungible(TOKEN_AMOUNT) };

		let dot_asset = Asset { id: AssetId(Location::parent()), fun: Fungible(TOKEN_AMOUNT) };

		let assets = vec![
			weth_asset.clone(),
			dot_asset.clone(),
			local_fee_asset.clone(),
			remote_fee_asset.clone(),
		];

		let xcms = VersionedXcm::from(Xcm(vec![
			WithdrawAsset(assets.clone().into()),
			PayFees { asset: local_fee_asset.clone() },
			InitiateTransfer {
				destination: ethereum(),
				remote_fees: Some(AssetTransferFilter::ReserveWithdraw(Definite(
					remote_fee_asset.clone().into(),
				))),
				preserve_origin: true,
				assets: BoundedVec::truncate_from(vec![
					AssetTransferFilter::ReserveWithdraw(Definite(weth_asset.clone().into())),
					AssetTransferFilter::ReserveDeposit(Definite(dot_asset.into())),
				]),
				remote_xcm: Xcm(vec![DepositAsset {
					assets: Wild(All),
					beneficiary: beneficiary(),
				}]),
			},
		]));

		<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
			RuntimeOrigin::signed(AssetHubWestendReceiver::get()),
			bx!(xcms),
			Weight::from(EXECUTION_WEIGHT),
		)
		.unwrap();
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		// Check that Ethereum message was queue in the Outbound Queue
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueueV2(snowbridge_pallet_outbound_queue_v2::Event::MessageQueued{ .. }) => {},]
		);

		let relayer = BridgeHubWestendSender::get();
		let reward_account = AssetHubWestendReceiver::get();
		let receipt = DeliveryReceipt {
			gateway: EthereumGatewayAddress::get(),
			nonce: 1,
			reward_address: reward_account.into(),
			topic: H256::zero(),
			success: true,
		};

		// Submit a delivery receipt
		assert_ok!(EthereumOutboundQueueV2::process_delivery_receipt(relayer, receipt));

		assert_expected_events!(
			BridgeHubWestend,
			vec![
				RuntimeEvent::BridgeRelayers(pallet_bridge_relayers::Event::RewardRegistered { .. }) => {},
			]
		);
	});
}

#[test]
fn transact_with_agent_from_asset_hub() {
	let weth_asset_location: Location = weth_location();

	fund_on_bh();

	register_assets_on_ah();

	fund_on_ah();

	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		let local_fee_asset =
			Asset { id: AssetId(Location::parent()), fun: Fungible(LOCAL_FEE_AMOUNT_IN_DOT) };

		let remote_fee_asset =
			Asset { id: AssetId(ethereum()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) };

		let reserve_asset =
			Asset { id: AssetId(weth_asset_location.clone()), fun: Fungible(TOKEN_AMOUNT) };

		let assets = vec![reserve_asset.clone(), local_fee_asset.clone(), remote_fee_asset.clone()];

		let beneficiary =
			Location::new(0, [AccountKey20 { network: None, key: AGENT_ADDRESS.into() }]);

		let transact_info = ContractCall::V1 {
			target: Default::default(),
			calldata: vec![],
			gas: 40000,
			// value should be less than the transfer amount, require validation on BH Exporter
			value: 4 * (TOKEN_AMOUNT - REMOTE_FEE_AMOUNT_IN_ETHER) / 5,
		};

		let xcms = VersionedXcm::from(Xcm(vec![
			WithdrawAsset(assets.clone().into()),
			PayFees { asset: local_fee_asset.clone() },
			InitiateTransfer {
				destination: ethereum(),
				remote_fees: Some(AssetTransferFilter::ReserveWithdraw(Definite(
					remote_fee_asset.clone().into(),
				))),
				preserve_origin: true,
				assets: BoundedVec::truncate_from(vec![AssetTransferFilter::ReserveWithdraw(
					Definite(reserve_asset.clone().into()),
				)]),
				remote_xcm: Xcm(vec![
					DepositAsset { assets: Wild(AllCounted(2)), beneficiary },
					Transact {
						origin_kind: OriginKind::SovereignAccount,
						fallback_max_weight: None,
						call: transact_info.encode().into(),
					},
				]),
			},
		]));

		<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
			RuntimeOrigin::signed(AssetHubWestendSender::get()),
			bx!(xcms),
			Weight::from(EXECUTION_WEIGHT),
		)
		.unwrap();
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		// Check that Ethereum message was queue in the Outbound Queue
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueueV2(snowbridge_pallet_outbound_queue_v2::Event::MessageQueued{ .. }) => {},]
		);

		let relayer = BridgeHubWestendSender::get();
		let reward_account = AssetHubWestendReceiver::get();
		let receipt = DeliveryReceipt {
			gateway: EthereumGatewayAddress::get(),
			nonce: 1,
			reward_address: reward_account.into(),
			topic: H256::zero(),
			success: true,
		};

		// Submit a delivery receipt
		assert_ok!(EthereumOutboundQueueV2::process_delivery_receipt(relayer, receipt));

		assert_expected_events!(
			BridgeHubWestend,
			vec![
				RuntimeEvent::BridgeRelayers(pallet_bridge_relayers::Event::RewardRegistered { .. }) => {},
			]
		);
	});
}

#[test]
fn transact_with_agent_from_asset_hub_without_any_asset_transfer() {
	fund_on_bh();

	register_assets_on_ah();

	fund_on_ah();

	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		let local_fee_asset =
			Asset { id: AssetId(Location::parent()), fun: Fungible(LOCAL_FEE_AMOUNT_IN_DOT) };

		let remote_fee_asset =
			Asset { id: AssetId(ethereum()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) };

		let assets = vec![local_fee_asset.clone(), remote_fee_asset.clone()];

		let beneficiary =
			Location::new(0, [AccountKey20 { network: None, key: AGENT_ADDRESS.into() }]);

		let transact_info =
			ContractCall::V1 { target: Default::default(), calldata: vec![], gas: 40000, value: 0 };

		let xcms = VersionedXcm::from(Xcm(vec![
			WithdrawAsset(assets.clone().into()),
			PayFees { asset: local_fee_asset.clone() },
			InitiateTransfer {
				destination: ethereum(),
				remote_fees: Some(AssetTransferFilter::ReserveWithdraw(Definite(
					remote_fee_asset.clone().into(),
				))),
				preserve_origin: true,
				assets: BoundedVec::new(),
				remote_xcm: Xcm(vec![
					DepositAsset { assets: Wild(AllCounted(2)), beneficiary },
					Transact {
						origin_kind: OriginKind::SovereignAccount,
						fallback_max_weight: None,
						call: transact_info.encode().into(),
					},
				]),
			},
		]));

		<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
			RuntimeOrigin::signed(AssetHubWestendSender::get()),
			bx!(xcms),
			Weight::from(EXECUTION_WEIGHT),
		)
		.unwrap();
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		// Check that Ethereum message was queue in the Outbound Queue
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueueV2(snowbridge_pallet_outbound_queue_v2::Event::MessageQueued{ .. }) => {},]
		);

		let relayer = BridgeHubWestendSender::get();
		let reward_account = AssetHubWestendReceiver::get();
		let receipt = DeliveryReceipt {
			gateway: EthereumGatewayAddress::get(),
			nonce: 1,
			reward_address: reward_account.into(),
			success: true,
			topic: Default::default(),
		};

		// Submit a delivery receipt
		assert_ok!(EthereumOutboundQueueV2::process_delivery_receipt(relayer, receipt));

		assert_expected_events!(
			BridgeHubWestend,
			vec![
				RuntimeEvent::BridgeRelayers(pallet_bridge_relayers::Event::RewardRegistered { .. }) => {},
			]
		);
	});
}

#[test]
fn register_token_from_penpal() {
	fund_on_bh();
	register_assets_on_ah();
	fund_on_ah();
	create_pools_on_ah();
	set_trust_reserve_on_penpal();
	register_assets_on_penpal();
	fund_on_penpal();
	let penpal_user_location = Location::new(
		1,
		[
			Parachain(PenpalB::para_id().into()),
			AccountId32 {
				network: Some(ByGenesis(WESTEND_GENESIS_HASH)),
				id: PenpalBSender::get().into(),
			},
		],
	);
	let asset_location_on_penpal = PenpalB::execute_with(|| PenpalLocalPen2Asset::get());
	let foreign_asset_at_asset_hub =
		Location::new(1, [Junction::Parachain(PenpalB::para_id().into())])
			.appended_with(asset_location_on_penpal)
			.unwrap();
	PenpalB::execute_with(|| {
		type RuntimeOrigin = <PenpalB as Chain>::RuntimeOrigin;

		let local_fee_asset_on_penpal =
			Asset { id: AssetId(Location::here()), fun: Fungible(LOCAL_FEE_AMOUNT_IN_PAL) };

		let remote_fee_asset_on_ah =
			Asset { id: AssetId(ethereum()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) };

		let remote_fee_asset_on_ethereum =
			Asset { id: AssetId(ethereum()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) };

		let call = EthereumSystemFrontend::EthereumSystemFrontend(
			EthereumSystemFrontendCall::RegisterToken {
				asset_id: Box::new(VersionedLocation::from(foreign_asset_at_asset_hub)),
				metadata: Default::default(),
				fee_asset: remote_fee_asset_on_ethereum.clone(),
			},
		);

		let assets = vec![
			local_fee_asset_on_penpal.clone(),
			remote_fee_asset_on_ah.clone(),
			remote_fee_asset_on_ethereum.clone(),
		];

		let xcm = VersionedXcm::from(Xcm(vec![
			WithdrawAsset(assets.clone().into()),
			PayFees { asset: local_fee_asset_on_penpal.clone() },
			InitiateTransfer {
				destination: asset_hub(),
				remote_fees: Some(AssetTransferFilter::ReserveWithdraw(Definite(
					remote_fee_asset_on_ah.clone().into(),
				))),
				preserve_origin: true,
				assets: BoundedVec::truncate_from(vec![AssetTransferFilter::ReserveWithdraw(
					Definite(remote_fee_asset_on_ethereum.clone().into()),
				)]),
				remote_xcm: Xcm(vec![
					DepositAsset { assets: Wild(All), beneficiary: penpal_user_location },
					Transact {
						origin_kind: OriginKind::Xcm,
						call: call.encode().into(),
						fallback_max_weight: None,
					},
					ExpectTransactStatus(MaybeErrorCode::Success),
				]),
			},
		]));

		assert_ok!(<PenpalB as PenpalBPallet>::PolkadotXcm::execute(
			RuntimeOrigin::root(),
			bx!(xcm.clone()),
			Weight::from(EXECUTION_WEIGHT),
		));
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Withdrawn { .. }) => {},]
		);
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueueV2(snowbridge_pallet_outbound_queue_v2::Event::MessageQueued{ .. }) => {},]
		);

		let relayer = BridgeHubWestendSender::get();
		let reward_account = AssetHubWestendReceiver::get();
		let receipt = DeliveryReceipt {
			gateway: EthereumGatewayAddress::get(),
			nonce: 1,
			reward_address: reward_account.into(),
			topic: H256::zero(),
			success: true,
		};

		// Submit a delivery receipt
		assert_ok!(EthereumOutboundQueueV2::process_delivery_receipt(relayer, receipt));

		assert_expected_events!(
			BridgeHubWestend,
			vec![
				RuntimeEvent::EthereumOutboundQueueV2(snowbridge_pallet_outbound_queue_v2::Event::MessageDelivered { .. }) => {},
			]
		);
	});
}

fn send_message_from_penpal_to_ethereum(sudo: bool) {
	// bh
	fund_on_bh();
	// ah
	register_assets_on_ah();
	create_pools_on_ah();
	mint_pal_on_ah();
	register_pal_on_bh();
	fund_on_ah();
	// penpal
	set_trust_reserve_on_penpal();
	register_assets_on_penpal();
	fund_on_penpal();

	PenpalB::execute_with(|| {
		type RuntimeOrigin = <PenpalB as Chain>::RuntimeOrigin;

		let local_fee_asset_on_penpal =
			Asset { id: AssetId(Location::here()), fun: Fungible(LOCAL_FEE_AMOUNT_IN_PAL) };

		let remote_fee_asset_on_ah =
			Asset { id: AssetId(ethereum()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) };

		let remote_fee_asset_on_ethereum =
			Asset { id: AssetId(ethereum()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) };

		let pna = Asset { id: AssetId(PenpalNativeCurrency::get()), fun: Fungible(TOKEN_AMOUNT) };

		let ena = Asset { id: AssetId(weth_location()), fun: Fungible(TOKEN_AMOUNT / 2) };

		let transfer_asset_reanchor_on_ah =
			Asset { id: AssetId(PenpalBLocation::get()), fun: Fungible(TOKEN_AMOUNT) };

		let assets = vec![
			local_fee_asset_on_penpal.clone(),
			remote_fee_asset_on_ah.clone(),
			remote_fee_asset_on_ethereum.clone(),
			pna.clone(),
			ena.clone(),
		];

		let transact_info =
			ContractCall::V1 { target: Default::default(), calldata: vec![], gas: 40000, value: 0 };

		let xcm = VersionedXcm::from(Xcm(vec![
			WithdrawAsset(assets.clone().into()),
			PayFees { asset: local_fee_asset_on_penpal.clone() },
			InitiateTransfer {
				destination: asset_hub(),
				remote_fees: Some(AssetTransferFilter::ReserveWithdraw(Definite(
					remote_fee_asset_on_ah.clone().into(),
				))),
				preserve_origin: true,
				assets: BoundedVec::truncate_from(vec![
					AssetTransferFilter::ReserveWithdraw(Definite(
						remote_fee_asset_on_ethereum.clone().into(),
					)),
					AssetTransferFilter::ReserveWithdraw(Definite(ena.clone().into())),
					// Should use Teleport here because:
					// a. Penpal is configured to allow teleport specific asset to AH
					// b. AH is configured to trust asset teleport from sibling chain
					AssetTransferFilter::Teleport(Definite(pna.clone().into())),
				]),
				remote_xcm: Xcm(vec![InitiateTransfer {
					destination: ethereum(),
					remote_fees: Some(AssetTransferFilter::ReserveWithdraw(Definite(
						remote_fee_asset_on_ethereum.clone().into(),
					))),
					preserve_origin: true,
					assets: BoundedVec::truncate_from(vec![
						// should use ReserveDeposit because Ethereum does not trust asset from
						// penpal. transfer_asset should be reachored first on AH
						AssetTransferFilter::ReserveDeposit(Definite(
							transfer_asset_reanchor_on_ah.clone().into(),
						)),
						AssetTransferFilter::ReserveWithdraw(Definite(ena.clone().into())),
					]),
					remote_xcm: Xcm(vec![
						DepositAsset { assets: Wild(All), beneficiary: beneficiary() },
						Transact {
							origin_kind: OriginKind::SovereignAccount,
							fallback_max_weight: None,
							call: transact_info.encode().into(),
						},
					]),
				}]),
			},
		]));

		if sudo {
			assert_ok!(<PenpalB as PenpalBPallet>::PolkadotXcm::execute(
				RuntimeOrigin::root(),
				bx!(xcm.clone()),
				Weight::from(EXECUTION_WEIGHT),
			));
		} else {
			assert_ok!(<PenpalB as PenpalBPallet>::PolkadotXcm::execute(
				RuntimeOrigin::signed(PenpalBSender::get()),
				bx!(xcm.clone()),
				Weight::from(EXECUTION_WEIGHT),
			));
		}
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::AssetConversion(pallet_asset_conversion::Event::SwapCreditExecuted { .. }) => {},]
		);
		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { .. }) => {},]
		);
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueueV2(snowbridge_pallet_outbound_queue_v2::Event::MessageQueued{ .. }) => {},]
		);
	});
}

#[test]
fn send_message_from_penpal_to_ethereum_with_sudo() {
	send_message_from_penpal_to_ethereum(true)
}

#[test]
fn send_message_from_penpal_to_ethereum_with_user_origin() {
	send_message_from_penpal_to_ethereum(false)
}

#[test]
fn invalid_nonce_for_delivery_receipt_fails() {
	BridgeHubWestend::execute_with(|| {
		type Runtime = <BridgeHubWestend as Chain>::Runtime;

		let relayer = BridgeHubWestendSender::get();
		let reward_account = AssetHubWestendReceiver::get();
		let receipt = DeliveryReceipt {
			gateway: EthereumGatewayAddress::get(),
			nonce: 0,
			reward_address: reward_account.into(),
			topic: H256::zero(),
			success: true,
		};

		assert_err!(
			EthereumOutboundQueueV2::process_delivery_receipt(relayer, receipt),
			Error::<Runtime>::InvalidPendingNonce
		);
	});
}

#[test]
fn export_message_from_asset_hub_to_ethereum_is_banned_when_set_operating_mode() {
	fund_on_bh();

	register_assets_on_ah();

	fund_on_ah();

	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;
		assert_ok!(
			<AssetHubWestend as AssetHubWestendPallet>::SnowbridgeSystemFrontend::set_operating_mode(
				RuntimeOrigin::root(),
				BasicOperatingMode::Halted));
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		type Runtime = <AssetHubWestend as Chain>::Runtime;

		let local_fee_asset =
			Asset { id: AssetId(Location::parent()), fun: Fungible(LOCAL_FEE_AMOUNT_IN_DOT) };

		let remote_fee_asset =
			Asset { id: AssetId(ethereum()), fun: Fungible(REMOTE_FEE_AMOUNT_IN_ETHER) };

		let reserve_asset = Asset { id: AssetId(weth_location()), fun: Fungible(TOKEN_AMOUNT) };

		let assets = vec![reserve_asset.clone(), remote_fee_asset.clone(), local_fee_asset.clone()];

		let xcm = VersionedXcm::from(Xcm(vec![
			WithdrawAsset(assets.clone().into()),
			PayFees { asset: local_fee_asset.clone() },
			InitiateTransfer {
				destination: ethereum(),
				remote_fees: Some(AssetTransferFilter::ReserveWithdraw(Definite(
					remote_fee_asset.clone().into(),
				))),
				preserve_origin: true,
				assets: BoundedVec::truncate_from(vec![AssetTransferFilter::ReserveWithdraw(
					Definite(reserve_asset.clone().into()),
				)]),
				remote_xcm: Xcm(vec![DepositAsset {
					assets: Wild(AllCounted(2)),
					beneficiary: beneficiary(),
				}]),
			},
		]));

		// Send the Weth back to Ethereum
		assert_err_ignore_postinfo!(
			<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::execute(
				RuntimeOrigin::signed(AssetHubWestendReceiver::get()),
				bx!(xcm),
				Weight::from(EXECUTION_WEIGHT),
			),
			pallet_xcm::Error::<Runtime>::LocalExecutionIncompleteWithError {
				error: XcmError::Unroutable.into(),
				index: 2
			}
		);
	});
}
