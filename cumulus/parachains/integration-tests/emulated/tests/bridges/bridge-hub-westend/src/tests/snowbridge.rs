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
		assert_bridge_hub_rococo_message_received, assert_bridge_hub_westend_message_accepted,
		asset_hub_rococo_location, asset_hub_westend_global_location, bridged_roc_at_ah_westend,
		bridged_wnd_at_ah_rococo, create_foreign_on_ah_rococo, create_foreign_on_ah_westend,
		penpal_emulated_chain::penpal_runtime,
		snowbridge_common::{bridge_hub, ethereum, register_roc_on_bh, snowbridge_sovereign},
	},
};
use asset_hub_westend_runtime::xcm_config::{
	bridging::to_ethereum::DefaultBridgeHubEthereumBaseFee,
	UniversalLocation as AssetHubWestendUniversalLocation,
};
use bridge_hub_westend_runtime::{
	bridge_to_ethereum_config::EthereumGatewayAddress, EthereumBeaconClient, EthereumInboundQueue,
};
use codec::Encode;
use emulated_integration_tests_common::{
	snowbridge::{SEPOLIA_ID, WETH},
	PENPAL_B_ID, RESERVABLE_ASSET_ID,
};
use frame_support::traits::fungibles::Mutate;
use hex_literal::hex;
use rococo_westend_system_emulated_network::{
	asset_hub_westend_emulated_chain::genesis::AssetHubWestendAssetOwner,
	penpal_emulated_chain::PARA_ID_B, westend_emulated_chain::westend_runtime::Dmp,
};
use snowbridge_core::{AssetMetadata, TokenIdOf};
use snowbridge_inbound_queue_primitives::{
	v1::{Command, Destination, MessageV1, VersionedMessage},
	EventFixture,
};
use snowbridge_pallet_inbound_queue_fixtures::send_native_eth::make_send_native_eth_message;
use sp_core::{H160, H256};
use testnet_parachains_constants::westend::snowbridge::EthereumNetwork;
use xcm_builder::ExternalConsensusLocationsConverterFor;
use xcm_executor::traits::ConvertLocation;

const INITIAL_FUND: u128 = 6_000_000_000_000;
const ETHEREUM_DESTINATION_ADDRESS: [u8; 20] = hex!("44a57ee2f2FCcb85FDa2B0B18EBD0D8D2333700e");
const XCM_FEE: u128 = 100_000_000_000;
const INSUFFICIENT_XCM_FEE: u128 = 1000;
const TOKEN_AMOUNT: u128 = 100_000_000_000;
const BRIDGE_FEE: u128 = 4_000_000_000_000;

pub fn send_inbound_message(fixture: EventFixture) -> DispatchResult {
	EthereumBeaconClient::store_finalized_header(
		fixture.finalized_header,
		fixture.block_roots_root,
	)
	.unwrap();
	EthereumInboundQueue::submit(
		BridgeHubWestendRuntimeOrigin::signed(BridgeHubWestendSender::get()),
		fixture.event,
	)
}

/// Tests the registering of a token as an asset on AssetHub.
#[test]
fn register_token_from_ethereum_to_asset_hub() {
	// Fund AssetHub sovereign account so that it can pay execution fees.
	BridgeHubWestend::fund_para_sovereign(AssetHubWestend::para_id().into(), INITIAL_FUND);
	// Fund Snowbridge Sovereign to satisfy ED.
	AssetHubWestend::fund_accounts(vec![(snowbridge_sovereign(), INITIAL_FUND)]);

	let token = H160::random();

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		let message = VersionedMessage::V1(MessageV1 {
			chain_id: SEPOLIA_ID,
			command: Command::RegisterToken { token: token.into(), fee: XCM_FEE },
		});
		let (xcm, _) = EthereumInboundQueue::do_convert([0; 32].into(), message).unwrap();
		let _ = EthereumInboundQueue::send_xcm(xcm, AssetHubWestend::para_id().into()).unwrap();

		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Created { .. }) => {},]
		);
	});
}

/// Tests the registering of a token as an asset on AssetHub, and then subsequently sending
/// a token from Ethereum to AssetHub.
#[test]
fn send_weth_token_from_ethereum_to_asset_hub() {
	let ethereum_sovereign: AccountId = snowbridge_sovereign();

	BridgeHubWestend::fund_para_sovereign(AssetHubWestend::para_id().into(), INITIAL_FUND);

	// Fund ethereum sovereign on AssetHub
	AssetHubWestend::fund_accounts(vec![
		(AssetHubWestendReceiver::get(), INITIAL_FUND),
		(ethereum_sovereign, INITIAL_FUND),
	]);

	// Send the token
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		type EthereumInboundQueue =
			<BridgeHubWestend as BridgeHubWestendPallet>::EthereumInboundQueue;
		let message_id: H256 = [0; 32].into();
		let message = VersionedMessage::V1(MessageV1 {
			chain_id: SEPOLIA_ID,
			command: Command::SendToken {
				token: WETH.into(),
				destination: Destination::AccountId32 { id: AssetHubWestendSender::get().into() },
				amount: TOKEN_AMOUNT,
				fee: XCM_FEE,
			},
		});
		let (xcm, _) = EthereumInboundQueue::do_convert(message_id, message).unwrap();
		assert_ok!(EthereumInboundQueue::send_xcm(xcm, AssetHubWestend::para_id().into()));

		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		// Check that the token was received and issued as a foreign asset on AssetHub
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { .. }) => {},
			]
		);
	});
}

/// Tests sending a token to a 3rd party parachain, called PenPal. The token reserve is
/// still located on AssetHub.
#[test]
fn send_weth_from_ethereum_to_penpal() {
	let asset_hub_sovereign = BridgeHubWestend::sovereign_account_id_of(Location::new(
		1,
		[Parachain(AssetHubWestend::para_id().into())],
	));
	// Fund AssetHub sovereign account so it can pay execution fees for the asset transfer
	BridgeHubWestend::fund_accounts(vec![(asset_hub_sovereign.clone(), INITIAL_FUND)]);

	// Fund PenPal receiver (covering ED)
	let native_id: Location = Parent.into();
	let receiver: AccountId = [
		28, 189, 45, 67, 83, 10, 68, 112, 90, 208, 136, 175, 49, 62, 24, 248, 11, 83, 239, 22, 179,
		97, 119, 205, 75, 119, 184, 70, 242, 165, 240, 124,
	]
	.into();
	PenpalB::mint_foreign_asset(
		<PenpalB as Chain>::RuntimeOrigin::signed(PenpalAssetOwner::get()),
		native_id,
		receiver,
		penpal_runtime::EXISTENTIAL_DEPOSIT,
	);

	PenpalB::execute_with(|| {
		let key = PenpalCustomizableAssetFromSystemAssetHub::key().to_vec();
		let value = Location::new(2, [GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })]).encode();
		assert_eq!(key, hex!("770800eb78be69c327d8334d09276072"));
		assert_eq!(value, hex!("020109079edaa802"));
		assert_ok!(<PenpalB as Chain>::System::set_storage(
			<PenpalB as Chain>::RuntimeOrigin::root(),
			vec![(
				PenpalCustomizableAssetFromSystemAssetHub::key().to_vec(),
				Location::new(2, [GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })]).encode(),
			)],
		));
	});

	let ethereum_network_v5: NetworkId = EthereumNetwork::get().into();

	// The Weth asset location, identified by the contract address on Ethereum
	let weth_asset_location: Location =
		(Parent, Parent, ethereum_network_v5, AccountKey20 { network: None, key: WETH }).into();

	// Fund ethereum sovereign on AssetHub
	let ethereum_sovereign: AccountId = snowbridge_sovereign();
	AssetHubWestend::fund_accounts(vec![(ethereum_sovereign.clone(), INITIAL_FUND)]);

	// Create asset on the Penpal parachain.
	PenpalB::execute_with(|| {
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::force_create(
			<PenpalB as Chain>::RuntimeOrigin::root(),
			weth_asset_location.clone(),
			asset_hub_sovereign.into(),
			false,
			1000,
		));

		assert!(<PenpalB as PenpalBPallet>::Assets::asset_exists(weth_asset_location));
	});

	// Send the token
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		type EthereumInboundQueue =
			<BridgeHubWestend as BridgeHubWestendPallet>::EthereumInboundQueue;
		let message_id: H256 = [0; 32].into();
		let message = VersionedMessage::V1(MessageV1 {
			chain_id: SEPOLIA_ID,
			command: Command::SendToken {
				token: WETH.into(),
				destination: Destination::ForeignAccountId32 {
					para_id: PENPAL_B_ID,
					id: PenpalBReceiver::get().into(),
					fee: XCM_FEE,
				},
				amount: TOKEN_AMOUNT,
				fee: XCM_FEE,
			},
		});
		let (xcm, _) = EthereumInboundQueue::do_convert(message_id, message).unwrap();
		assert_ok!(EthereumInboundQueue::send_xcm(xcm, AssetHubWestend::para_id().into()));

		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		// Check that the assets were issued on AssetHub
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { .. }) => {},
				RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},
			]
		);
	});

	PenpalB::execute_with(|| {
		type RuntimeEvent = <PenpalB as Chain>::RuntimeEvent;
		// Check that the assets were issued on PenPal
		assert_expected_events!(
			PenpalB,
			vec![
				RuntimeEvent::Assets(pallet_assets::Event::Deposited { .. }) => {},
			]
		);
	});
}

/// Tests the full cycle of eth transfers:
/// - sending a token to AssetHub
/// - returning the token to Ethereum
#[test]
fn send_eth_asset_from_asset_hub_to_ethereum_and_back() {
	let ethereum_network: NetworkId = EthereumNetwork::get().into();
	let origin_location: Location = (Parent, Parent, ethereum_network).into();

	use asset_hub_westend_runtime::xcm_config::bridging::to_ethereum::DefaultBridgeHubEthereumBaseFee;
	let assethub_location = BridgeHubWestend::sibling_location_of(AssetHubWestend::para_id());
	let assethub_sovereign = BridgeHubWestend::sovereign_account_id_of(assethub_location);
	let ethereum_sovereign: AccountId = snowbridge_sovereign();

	AssetHubWestend::force_default_xcm_version(Some(XCM_VERSION));
	BridgeHubWestend::force_default_xcm_version(Some(XCM_VERSION));
	AssetHubWestend::force_xcm_version(origin_location.clone(), XCM_VERSION);

	BridgeHubWestend::fund_accounts(vec![(assethub_sovereign.clone(), INITIAL_FUND)]);
	BridgeHubWestend::fund_accounts(vec![(BridgeHubWestendSender::get(), INITIAL_FUND)]);
	AssetHubWestend::fund_accounts(vec![
		(AssetHubWestendReceiver::get(), INITIAL_FUND),
		(ethereum_sovereign.clone(), INITIAL_FUND),
	]);

	const ETH_AMOUNT: u128 = 1_000_000_000_000_000_000;

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		type RuntimeOrigin = <BridgeHubWestend as Chain>::RuntimeOrigin;

		// Set the gateway. This is needed because new fixtures use a different gateway address.
		assert_ok!(<BridgeHubWestend as Chain>::System::set_storage(
			RuntimeOrigin::root(),
			vec![(
				EthereumGatewayAddress::key().to_vec(),
				sp_core::H160(hex!("87d1f7fdfEe7f651FaBc8bFCB6E086C278b77A7d")).encode(),
			)],
		));

		// Construct SendToken message and sent to inbound queue
		let fixture = make_send_native_eth_message();
		let delivery_cost =
			EthereumInboundQueue::calculate_delivery_cost(fixture.event.encode().len() as u32);
		let relayer_balance_before =
			<BridgeHubWestend as BridgeHubWestendPallet>::Balances::free_balance(
				BridgeHubWestendSender::get(),
			);
		let sovereign_balance_before =
			<BridgeHubWestend as BridgeHubWestendPallet>::Balances::free_balance(
				assethub_sovereign.clone(),
			);

		assert_ok!(send_inbound_message(fixture));

		// Check that the send token message was sent using xcm
		assert_expected_events!(
			BridgeHubWestend,
			vec![
				RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},
			]
		);

		let fee_burned = BridgeHubWestend::events()
			.iter()
			.find_map(|event| match event {
				RuntimeEvent::EthereumInboundQueue(
					snowbridge_pallet_inbound_queue::Event::MessageReceived { fee_burned, .. },
				) => Some(*fee_burned),
				_ => None,
			})
			.expect("inbound message received");
		let relayer_balance_after =
			<BridgeHubWestend as BridgeHubWestendPallet>::Balances::free_balance(
				BridgeHubWestendSender::get(),
			);
		let sovereign_balance_after =
			<BridgeHubWestend as BridgeHubWestendPallet>::Balances::free_balance(
				assethub_sovereign.clone(),
			);

		assert_eq!(relayer_balance_after, relayer_balance_before + delivery_cost);
		assert_eq!(sovereign_balance_after, sovereign_balance_before - fee_burned - delivery_cost);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		let _issued_event = RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited {
			asset_id: origin_location.clone(),
			who: AssetHubWestendReceiver::get().into(),
			amount: ETH_AMOUNT,
		});
		// Check that AssetHub has issued the foreign asset
		assert_expected_events!(
			AssetHubWestend,
			vec![
				_issued_event => {},
			]
		);
		let assets =
			vec![Asset { id: AssetId(origin_location.clone()), fun: Fungible(ETH_AMOUNT) }];
		let multi_assets = VersionedAssets::from(Assets::from(assets));

		let destination = origin_location.clone().into();

		let beneficiary = VersionedLocation::from(Location::new(
			0,
			[AccountKey20 { network: None, key: ETHEREUM_DESTINATION_ADDRESS.into() }],
		));

		let free_balance_before =
			<AssetHubWestend as AssetHubWestendPallet>::Balances::free_balance(
				AssetHubWestendReceiver::get(),
			);
		// Send the Weth back to Ethereum
		<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::limited_reserve_transfer_assets(
			RuntimeOrigin::signed(AssetHubWestendReceiver::get()),
			Box::new(destination),
			Box::new(beneficiary),
			Box::new(multi_assets),
			0,
			Unlimited,
		)
		.unwrap();

		let _burned_event = RuntimeEvent::ForeignAssets(pallet_assets::Event::Withdrawn {
			asset_id: origin_location.clone(),
			who: AssetHubWestendReceiver::get().into(),
			amount: ETH_AMOUNT,
		});
		// Check that AssetHub has issued the foreign asset
		let _destination = origin_location.clone();
		assert_expected_events!(
			AssetHubWestend,
			vec![
				_burned_event => {},
				RuntimeEvent::PolkadotXcm(pallet_xcm::Event::Sent {
					destination: _destination, ..
				}) => {},
			]
		);

		let free_balance_after = <AssetHubWestend as AssetHubWestendPallet>::Balances::free_balance(
			AssetHubWestendReceiver::get(),
		);
		// Assert at least DefaultBridgeHubEthereumBaseFee charged from the sender
		let free_balance_diff = free_balance_before - free_balance_after;
		assert!(free_balance_diff > DefaultBridgeHubEthereumBaseFee::get());
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		// Check that the transfer token back to Ethereum message was queue in the Ethereum
		// Outbound Queue
		assert_expected_events!(
			BridgeHubWestend,
			vec![
				RuntimeEvent::EthereumOutboundQueue(snowbridge_pallet_outbound_queue::Event::MessageAccepted {..}) => {},
				RuntimeEvent::EthereumOutboundQueue(snowbridge_pallet_outbound_queue::Event::MessageQueued {..}) => {},
			]
		);
	});
}

#[test]
fn send_eth_asset_from_ethereum_with_empty_asset_hub_sovereign_preserves_liveness() {
	let ethereum_network: NetworkId = EthereumNetwork::get().into();
	let origin_location: Location = (Parent, Parent, ethereum_network).into();
	let assethub_location = BridgeHubWestend::sibling_location_of(AssetHubWestend::para_id());
	let assethub_sovereign = BridgeHubWestend::sovereign_account_id_of(assethub_location);

	AssetHubWestend::force_default_xcm_version(Some(XCM_VERSION));
	BridgeHubWestend::force_default_xcm_version(Some(XCM_VERSION));
	AssetHubWestend::force_xcm_version(origin_location.clone(), XCM_VERSION);

	BridgeHubWestend::fund_accounts(vec![(BridgeHubWestendSender::get(), INITIAL_FUND)]);
	AssetHubWestend::fund_accounts(vec![
		(AssetHubWestendReceiver::get(), INITIAL_FUND),
		(snowbridge_sovereign(), INITIAL_FUND),
	]);

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		type RuntimeOrigin = <BridgeHubWestend as Chain>::RuntimeOrigin;

		assert_ok!(<BridgeHubWestend as BridgeHubWestendPallet>::Balances::force_set_balance(
			RuntimeOrigin::root(),
			sp_runtime::MultiAddress::Id(assethub_sovereign.clone()),
			0,
		));
		assert_ok!(<BridgeHubWestend as Chain>::System::set_storage(
			RuntimeOrigin::root(),
			vec![(
				EthereumGatewayAddress::key().to_vec(),
				sp_core::H160(hex!("87d1f7fdfEe7f651FaBc8bFCB6E086C278b77A7d")).encode(),
			)],
		));

		let fixture = make_send_native_eth_message();
		let relayer_balance_before =
			<BridgeHubWestend as BridgeHubWestendPallet>::Balances::free_balance(
				BridgeHubWestendSender::get(),
			);

		assert_ok!(send_inbound_message(fixture));

		let fee_burned = BridgeHubWestend::events()
			.iter()
			.find_map(|event| match event {
				RuntimeEvent::EthereumInboundQueue(
					snowbridge_pallet_inbound_queue::Event::MessageReceived {
						nonce,
						fee_burned,
						..
					},
				) if *nonce == 1 => Some(*fee_burned),
				_ => None,
			})
			.expect("inbound message received");
		let relayer_balance_after =
			<BridgeHubWestend as BridgeHubWestendPallet>::Balances::free_balance(
				BridgeHubWestendSender::get(),
			);

		assert_eq!(relayer_balance_after, relayer_balance_before - fee_burned);
		assert_eq!(
			<BridgeHubWestend as BridgeHubWestendPallet>::Balances::free_balance(
				assethub_sovereign.clone()
			),
			0
		);
		assert_expected_events!(
			BridgeHubWestend,
			vec![
				RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},
			]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited {
					asset_id,
					who,
					..
				}) => {
					asset_id: *asset_id == origin_location,
					who: *who == AssetHubWestendReceiver::get().into(),
				},
			]
		);
	});
}

// If the relayer cannot front the teleported Asset Hub execution fee, the inbound message must be
// rejected and the channel nonce must not advance, so that a funded relayer can resubmit the same
// message and the channel is not stalled.
#[test]
fn send_eth_asset_from_ethereum_fails_when_relayer_cannot_front_fee() {
	let assethub_location = BridgeHubWestend::sibling_location_of(AssetHubWestend::para_id());
	let assethub_sovereign = BridgeHubWestend::sovereign_account_id_of(assethub_location);

	// Fund the Asset Hub sovereign so the only possible cause of failure is the relayer's
	// shortfall.
	BridgeHubWestend::fund_accounts(vec![(assethub_sovereign, INITIAL_FUND)]);

	BridgeHubWestend::execute_with(|| {
		type RuntimeOrigin = <BridgeHubWestend as Chain>::RuntimeOrigin;
		type Runtime = <BridgeHubWestend as Chain>::Runtime;

		// Set the Ethereum gateway so the envelope passes the gateway check.
		assert_ok!(<BridgeHubWestend as Chain>::System::set_storage(
			RuntimeOrigin::root(),
			vec![(
				EthereumGatewayAddress::key().to_vec(),
				sp_core::H160(hex!("87d1f7fdfEe7f651FaBc8bFCB6E086C278b77A7d")).encode(),
			)],
		));

		// Drain the relayer so it cannot front the teleported fee.
		assert_ok!(<BridgeHubWestend as BridgeHubWestendPallet>::Balances::force_set_balance(
			RuntimeOrigin::root(),
			sp_runtime::MultiAddress::Id(BridgeHubWestendSender::get()),
			0,
		));

		let channel_id = snowbridge_core::ChannelId::from(hex!(
			"c173fac324158e77fb5840738a1a541f633cbec8884c6a601c567d2b376a0539"
		));
		assert_eq!(snowbridge_pallet_inbound_queue::Nonce::<Runtime>::get(channel_id), 0);

		let fixture = make_send_native_eth_message();

		// Dispatch inside a storage layer to mimic the on-chain transactional revert.
		let result = frame_support::storage::with_storage_layer(|| send_inbound_message(fixture));
		assert_err!(result, sp_runtime::TokenError::FundsUnavailable);

		// The nonce did not advance, so the channel is not stalled and a funded relayer can
		// resubmit the same message.
		assert_eq!(snowbridge_pallet_inbound_queue::Nonce::<Runtime>::get(channel_id), 0);
	});
}

#[test]
fn register_weth_token_in_asset_hub_fail_for_insufficient_fee() {
	BridgeHubWestend::fund_para_sovereign(AssetHubWestend::para_id().into(), INITIAL_FUND);

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		type EthereumInboundQueue =
			<BridgeHubWestend as BridgeHubWestendPallet>::EthereumInboundQueue;
		let message_id: H256 = [0; 32].into();
		let message = VersionedMessage::V1(MessageV1 {
			chain_id: SEPOLIA_ID,
			command: Command::RegisterToken {
				token: WETH.into(),
				// Insufficient fee which should trigger the trap
				fee: INSUFFICIENT_XCM_FEE,
			},
		});
		let (xcm, _) = EthereumInboundQueue::do_convert(message_id, message).unwrap();
		let _ = EthereumInboundQueue::send_xcm(xcm, AssetHubWestend::para_id().into()).unwrap();

		assert_expected_events!(
			BridgeHubWestend,
			vec![
				RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},
			]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed { success:false, .. }) => {},
			]
		);
	});
}

fn send_weth_from_ethereum_to_asset_hub_with_fee(account_id: [u8; 32], fee: u128) {
	// Fund asset hub sovereign on bridge hub
	let asset_hub_sovereign = BridgeHubWestend::sovereign_account_id_of(Location::new(
		1,
		[Parachain(AssetHubWestend::para_id().into())],
	));
	BridgeHubWestend::fund_accounts(vec![(asset_hub_sovereign.clone(), INITIAL_FUND)]);

	// Send WETH to an existent account on asset hub
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		type EthereumInboundQueue =
			<BridgeHubWestend as BridgeHubWestendPallet>::EthereumInboundQueue;
		let message_id: H256 = [0; 32].into();
		let message = VersionedMessage::V1(MessageV1 {
			chain_id: SEPOLIA_ID,
			command: Command::SendToken {
				token: WETH.into(),
				destination: Destination::AccountId32 { id: account_id },
				amount: TOKEN_AMOUNT,
				fee,
			},
		});
		let (xcm, _) = EthereumInboundQueue::do_convert(message_id, message).unwrap();
		assert_ok!(EthereumInboundQueue::send_xcm(xcm, AssetHubWestend::para_id().into()));

		// Check that the message was sent
		assert_expected_events!(
			BridgeHubWestend,
			vec![
				RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},
			]
		);
	});
}

#[test]
fn send_weth_from_ethereum_to_existent_account_on_asset_hub() {
	send_weth_from_ethereum_to_asset_hub_with_fee(AssetHubWestendSender::get().into(), XCM_FEE);

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		// Check that the token was received and issued as a foreign asset on AssetHub
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { .. }) => {},
			]
		);
	});
}

#[test]
fn send_weth_from_ethereum_to_non_existent_account_on_asset_hub() {
	send_weth_from_ethereum_to_asset_hub_with_fee([1; 32], XCM_FEE);

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		// Check that the token was received and issued as a foreign asset on AssetHub
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { .. }) => {},
			]
		);
	});
}

#[test]
fn send_weth_from_ethereum_to_non_existent_account_on_asset_hub_with_insufficient_fee() {
	send_weth_from_ethereum_to_asset_hub_with_fee([1; 32], INSUFFICIENT_XCM_FEE);

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		// Check that the message was not processed successfully due to insufficient fee

		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed { success:false, .. }) => {},
			]
		);
	});
}

#[test]
fn send_weth_from_ethereum_to_non_existent_account_on_asset_hub_with_sufficient_fee_but_do_not_satisfy_ed(
) {
	// On AH the xcm fee is 26_789_690 and the ED is 3_300_000
	send_weth_from_ethereum_to_asset_hub_with_fee([1; 32], 30_000_000);

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		// Check that the message was not processed successfully due to insufficient ED
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed { success:false, .. }) => {},
			]
		);
	});
}

/// Tests the registering of a token as an asset on AssetHub, and then subsequently sending
/// a token from Ethereum to AssetHub.
#[test]
fn send_token_from_ethereum_to_asset_hub() {
	let asset_hub_sovereign = BridgeHubWestend::sovereign_account_id_of(Location::new(
		1,
		[Parachain(AssetHubWestend::para_id().into())],
	));
	// Fund AssetHub sovereign account so it can pay execution fees for the asset transfer
	BridgeHubWestend::fund_accounts(vec![(asset_hub_sovereign.clone(), INITIAL_FUND)]);

	// Fund ethereum sovereign on AssetHub
	AssetHubWestend::fund_accounts(vec![(AssetHubWestendReceiver::get(), INITIAL_FUND)]);

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		let message = VersionedMessage::V1(MessageV1 {
			chain_id: SEPOLIA_ID,
			command: Command::SendToken {
				token: WETH.into(),
				destination: Destination::AccountId32 { id: AssetHubWestendReceiver::get().into() },
				amount: TOKEN_AMOUNT,
				fee: XCM_FEE,
			},
		});
		let (xcm, _) = EthereumInboundQueue::do_convert([0; 32].into(), message).unwrap();
		let _ = EthereumInboundQueue::send_xcm(xcm, AssetHubWestend::para_id().into()).unwrap();

		// Check that the message was sent
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		// Check that the token was received and issued as a foreign asset on AssetHub
		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { .. }) => {},]
		);
	});
}

/// Tests the full cycle of token transfers:
/// - registering a token on AssetHub
/// - sending a token to AssetHub
/// - returning the token to Ethereum
#[test]
fn send_weth_asset_from_asset_hub_to_ethereum() {
	let assethub_location = BridgeHubWestend::sibling_location_of(AssetHubWestend::para_id());
	let assethub_sovereign = BridgeHubWestend::sovereign_account_id_of(assethub_location);

	BridgeHubWestend::fund_accounts(vec![(assethub_sovereign.clone(), INITIAL_FUND)]);

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		let message = VersionedMessage::V1(MessageV1 {
			chain_id: SEPOLIA_ID,
			command: Command::SendToken {
				token: WETH.into(),
				destination: Destination::AccountId32 { id: AssetHubWestendReceiver::get().into() },
				amount: TOKEN_AMOUNT,
				fee: XCM_FEE,
			},
		});
		let (xcm, _) = EthereumInboundQueue::do_convert([0; 32].into(), message).unwrap();
		let _ = EthereumInboundQueue::send_xcm(xcm, AssetHubWestend::para_id().into()).unwrap();

		// Check that the send token message was sent using xcm
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) =>{},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		// Check that AssetHub has issued the foreign asset
		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { .. }) => {},]
		);
		let assets = vec![Asset {
			id: AssetId(Location::new(
				2,
				[
					GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID }),
					AccountKey20 { network: None, key: WETH },
				],
			)),
			fun: Fungible(TOKEN_AMOUNT),
		}];
		let versioned_assets = VersionedAssets::from(Assets::from(assets));

		let destination = VersionedLocation::from(Location::new(
			2,
			[GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })],
		));

		let beneficiary = VersionedLocation::from(Location::new(
			0,
			[AccountKey20 { network: None, key: ETHEREUM_DESTINATION_ADDRESS.into() }],
		));

		let free_balance_before =
			<AssetHubWestend as AssetHubWestendPallet>::Balances::free_balance(
				AssetHubWestendReceiver::get(),
			);
		// Send the Weth back to Ethereum
		<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::limited_reserve_transfer_assets(
			RuntimeOrigin::signed(AssetHubWestendReceiver::get()),
			Box::new(destination),
			Box::new(beneficiary),
			Box::new(versioned_assets),
			0,
			Unlimited,
		)
		.unwrap();
		let free_balance_after = <AssetHubWestend as AssetHubWestendPallet>::Balances::free_balance(
			AssetHubWestendReceiver::get(),
		);
		// Assert at least DefaultBridgeHubEthereumBaseFee charged from the sender
		let free_balance_diff = free_balance_before - free_balance_after;
		assert!(free_balance_diff > DefaultBridgeHubEthereumBaseFee::get());
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		// Check that the transfer token back to Ethereum message was queue in the Ethereum
		// Outbound Queue
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueue(snowbridge_pallet_outbound_queue::Event::MessageQueued{ .. }) => {},]
		);
	});
}

/// Tests sending a token to a 3rd party parachain, called PenPal. The token reserve is
/// still located on AssetHub.
#[test]
fn send_token_from_ethereum_to_penpal() {
	let asset_hub_sovereign = BridgeHubWestend::sovereign_account_id_of(Location::new(
		1,
		[Parachain(AssetHubWestend::para_id().into())],
	));
	// Fund AssetHub sovereign account so it can pay execution fees for the asset transfer
	BridgeHubWestend::fund_accounts(vec![(asset_hub_sovereign.clone(), INITIAL_FUND)]);
	// Fund PenPal receiver (covering ED)
	PenpalB::fund_accounts(vec![(PenpalBReceiver::get(), INITIAL_FUND)]);

	PenpalB::execute_with(|| {
		assert_ok!(<PenpalB as Chain>::System::set_storage(
			<PenpalB as Chain>::RuntimeOrigin::root(),
			vec![(
				PenpalCustomizableAssetFromSystemAssetHub::key().to_vec(),
				Location::new(2, [GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })]).encode(),
			)],
		));
	});

	let ethereum_network_v5: NetworkId = EthereumNetwork::get().into();

	// The Weth asset location, identified by the contract address on Ethereum
	let weth_asset_location: Location =
		(Parent, Parent, ethereum_network_v5, AccountKey20 { network: None, key: WETH }).into();

	// Fund ethereum sovereign on AssetHub
	let ethereum_sovereign: AccountId = snowbridge_sovereign();
	AssetHubWestend::fund_accounts(vec![(ethereum_sovereign.clone(), INITIAL_FUND)]);

	// Create asset on the Penpal parachain.
	PenpalB::execute_with(|| {
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::force_create(
			<PenpalB as Chain>::RuntimeOrigin::root(),
			weth_asset_location.clone(),
			asset_hub_sovereign.clone().into(),
			false,
			1000,
		));

		assert!(<PenpalB as PenpalBPallet>::Assets::asset_exists(weth_asset_location.clone()));
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		let message = VersionedMessage::V1(MessageV1 {
			chain_id: SEPOLIA_ID,
			command: Command::SendToken {
				token: WETH.into(),
				destination: Destination::ForeignAccountId32 {
					para_id: PARA_ID_B,
					id: PenpalBReceiver::get().into(),
					fee: 100_000_000_000u128,
				},
				amount: TOKEN_AMOUNT,
				fee: XCM_FEE,
			},
		});
		let (xcm, _) = EthereumInboundQueue::do_convert([0; 32].into(), message).unwrap();
		let _ = EthereumInboundQueue::send_xcm(xcm, AssetHubWestend::para_id().into()).unwrap();

		// Check that the send token message was sent using xcm
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) =>{},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		// Check that the assets were issued on AssetHub
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { .. }) => {},
				RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},
			]
		);
	});

	PenpalB::execute_with(|| {
		type RuntimeEvent = <PenpalB as Chain>::RuntimeEvent;
		// Check that the assets were issued on PenPal
		assert_expected_events!(
			PenpalB,
			vec![
				RuntimeEvent::Assets(pallet_assets::Event::Deposited { .. }) => {},
			]
		);
	});
}

#[test]
fn transfer_relay_token() {
	let assethub_sovereign = BridgeHubWestend::sovereign_account_id_of(
		BridgeHubWestend::sibling_location_of(AssetHubWestend::para_id()),
	);
	BridgeHubWestend::fund_accounts(vec![(assethub_sovereign.clone(), INITIAL_FUND)]);

	let asset_id: Location = Location { parents: 1, interior: [].into() };
	let expected_asset_id: Location = Location {
		parents: 1,
		interior: [GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH))].into(),
	};

	let expected_token_id = TokenIdOf::convert_location(&expected_asset_id).unwrap();

	let ethereum_sovereign: AccountId = snowbridge_sovereign();

	// Register token
	BridgeHubWestend::execute_with(|| {
		type RuntimeOrigin = <BridgeHubWestend as Chain>::RuntimeOrigin;
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		assert_ok!(<BridgeHubWestend as BridgeHubWestendPallet>::Balances::force_set_balance(
			RuntimeOrigin::root(),
			sp_runtime::MultiAddress::Id(BridgeHubWestendSender::get()),
			INITIAL_FUND * 10,
		));

		assert_ok!(<BridgeHubWestend as BridgeHubWestendPallet>::EthereumSystem::register_token(
			RuntimeOrigin::root(),
			Box::new(VersionedLocation::from(asset_id.clone())),
			AssetMetadata {
				name: "wnd".as_bytes().to_vec().try_into().unwrap(),
				symbol: "wnd".as_bytes().to_vec().try_into().unwrap(),
				decimals: 12,
			},
		));
		// Check that a message was sent to Ethereum to create the agent
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumSystem(snowbridge_pallet_system::Event::RegisterToken { .. }) => {},]
		);
	});

	// Send token to Ethereum
	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		let assets = vec![Asset { id: AssetId(Location::parent()), fun: Fungible(TOKEN_AMOUNT) }];
		let versioned_assets = VersionedAssets::from(Assets::from(assets));

		let destination = VersionedLocation::from(Location::new(
			2,
			[GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })],
		));

		let beneficiary = Location::new(
			0,
			[AccountKey20 { network: None, key: ETHEREUM_DESTINATION_ADDRESS.into() }],
		);

		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::transfer_assets_using_type_and_then(
			RuntimeOrigin::signed(AssetHubWestendSender::get()),
			Box::new(destination),
			Box::new(versioned_assets),
			Box::new(TransferType::LocalReserve),
			Box::new(VersionedAssetId::from(AssetId(Location::parent()))),
			Box::new(TransferType::LocalReserve),
			Box::new(VersionedXcm::from(
				Xcm::<()>::builder_unsafe()
					.deposit_asset(AllCounted(1), beneficiary)
					.build()
			)),
			Unlimited,
		));

		let events = AssetHubWestend::events();
		// Check that the native asset transferred to some reserved account(sovereign of Ethereum)
		assert!(
			events.iter().any(|event| matches!(
				event,
				RuntimeEvent::Balances(pallet_balances::Event::Transfer { amount, to, ..})
					if *amount == TOKEN_AMOUNT && *to == ethereum_sovereign.clone(),
			)),
			"native token reserved to Ethereum sovereign account."
		);
	});

	// Send token back from ethereum
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		// Check that the transfer token back to Ethereum message was queue in the Ethereum
		// Outbound Queue
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueue(snowbridge_pallet_outbound_queue::Event::MessageQueued{ .. }) => {},]
		);

		// Send relay token back to AH
		let message_id: H256 = [0; 32].into();
		let message = VersionedMessage::V1(MessageV1 {
			chain_id: SEPOLIA_ID,
			command: Command::SendNativeToken {
				token_id: expected_token_id,
				destination: Destination::AccountId32 { id: AssetHubWestendReceiver::get().into() },
				amount: TOKEN_AMOUNT,
				fee: XCM_FEE,
			},
		});
		// Convert the message to XCM
		let (xcm, _) = EthereumInboundQueue::do_convert(message_id, message).unwrap();
		// Send the XCM
		let _ = EthereumInboundQueue::send_xcm(xcm, AssetHubWestend::para_id().into()).unwrap();

		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::Balances(pallet_balances::Event::Withdraw{ .. }) => {},]
		);

		let events = AssetHubWestend::events();

		// Check that the native token burnt from some reserved account
		assert!(
			events.iter().any(|event| matches!(
				event,
				RuntimeEvent::Balances(pallet_balances::Event::Withdraw { who, ..})
					if *who == ethereum_sovereign.clone(),
			)),
			"native token burnt from Ethereum sovereign account."
		);

		// Check that the token was minted to beneficiary
		assert!(
			events.iter().any(|event| matches!(
				event,
				RuntimeEvent::Balances(pallet_balances::Event::Deposit { who, amount })
					if *amount >= TOKEN_AMOUNT && *who == AssetHubWestendReceiver::get()
			)),
			"Token minted to beneficiary."
		);
	});
}

#[test]
fn transfer_ah_token() {
	let assethub_sovereign = BridgeHubWestend::sovereign_account_id_of(
		BridgeHubWestend::sibling_location_of(AssetHubWestend::para_id()),
	);
	BridgeHubWestend::fund_accounts(vec![(assethub_sovereign.clone(), INITIAL_FUND)]);

	let ethereum_destination =
		Location::new(2, [GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })]);

	let ethereum_sovereign: AccountId = snowbridge_sovereign();
	AssetHubWestend::fund_accounts(vec![(ethereum_sovereign.clone(), INITIAL_FUND)]);

	let asset_id: Location =
		[PalletInstance(ASSETS_PALLET_ID), GeneralIndex(RESERVABLE_ASSET_ID.into())].into();

	let asset_id_in_bh: Location = Location::new(
		1,
		[
			Parachain(AssetHubWestend::para_id().into()),
			PalletInstance(ASSETS_PALLET_ID),
			GeneralIndex(RESERVABLE_ASSET_ID.into()),
		],
	);

	let asset_id_after_reanchored = Location::new(
		1,
		[
			GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
			Parachain(AssetHubWestend::para_id().into()),
		],
	)
	.appended_with(asset_id.clone().interior)
	.unwrap();

	let token_id = TokenIdOf::convert_location(&asset_id_after_reanchored).unwrap();

	// Register token
	BridgeHubWestend::execute_with(|| {
		type RuntimeOrigin = <BridgeHubWestend as Chain>::RuntimeOrigin;

		assert_ok!(<BridgeHubWestend as BridgeHubWestendPallet>::EthereumSystem::register_token(
			RuntimeOrigin::root(),
			Box::new(VersionedLocation::from(asset_id_in_bh.clone())),
			AssetMetadata {
				name: "ah_asset".as_bytes().to_vec().try_into().unwrap(),
				symbol: "ah_asset".as_bytes().to_vec().try_into().unwrap(),
				decimals: 12,
			},
		));
	});

	// Mint some token
	AssetHubWestend::mint_asset(
		<AssetHubWestend as Chain>::RuntimeOrigin::signed(AssetHubWestendAssetOwner::get()),
		RESERVABLE_ASSET_ID,
		AssetHubWestendSender::get(),
		TOKEN_AMOUNT,
	);

	// Send token to Ethereum
	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		// Send partial of the token, will fail if send all
		let assets =
			vec![Asset { id: AssetId(asset_id.clone()), fun: Fungible(TOKEN_AMOUNT / 10) }];
		let versioned_assets = VersionedAssets::from(Assets::from(assets));

		let beneficiary = VersionedLocation::from(Location::new(
			0,
			[AccountKey20 { network: None, key: ETHEREUM_DESTINATION_ADDRESS.into() }],
		));

		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::transfer_assets(
			RuntimeOrigin::signed(AssetHubWestendSender::get()),
			Box::new(VersionedLocation::from(ethereum_destination)),
			Box::new(beneficiary),
			Box::new(versioned_assets),
			0,
			Unlimited,
		));

		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::Assets(pallet_assets::Event::Transferred{ .. }) => {},]
		);

		let events = AssetHubWestend::events();
		// Check that the native asset transferred to some reserved account(sovereign of Ethereum)
		assert!(
			events.iter().any(|event| matches!(
				event,
				RuntimeEvent::Assets(pallet_assets::Event::Transferred { asset_id, to, ..})
					if *asset_id == RESERVABLE_ASSET_ID && *to == ethereum_sovereign.clone()
			)),
			"native token reserved to Ethereum sovereign account."
		);
	});

	// Send token back from Ethereum
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		// Check that the transfer token back to Ethereum message was queue in the Ethereum
		// Outbound Queue
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueue(snowbridge_pallet_outbound_queue::Event::MessageQueued{ .. }) => {},]
		);

		let message = VersionedMessage::V1(MessageV1 {
			chain_id: SEPOLIA_ID,
			command: Command::SendNativeToken {
				token_id,
				destination: Destination::AccountId32 { id: AssetHubWestendReceiver::get().into() },
				amount: TOKEN_AMOUNT / 10,
				fee: XCM_FEE,
			},
		});
		// Convert the message to XCM
		let (xcm, _) = EthereumInboundQueue::do_convert([0; 32].into(), message).unwrap();
		// Send the XCM
		let _ = EthereumInboundQueue::send_xcm(xcm, AssetHubWestend::para_id().into()).unwrap();

		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::Assets(pallet_assets::Event::Withdrawn{..}) => {},]
		);

		let events = AssetHubWestend::events();

		// Check that the native token burnt from some reserved account
		assert!(
			events.iter().any(|event| matches!(
				event,
				RuntimeEvent::Assets(pallet_assets::Event::Withdrawn { who: owner, .. })
					if *owner == ethereum_sovereign.clone(),
			)),
			"token burnt from Ethereum sovereign account."
		);

		// Check that the token was minted to beneficiary
		assert!(
			events.iter().any(|event| matches!(
				event,
				RuntimeEvent::Assets(pallet_assets::Event::Deposited { who: owner, .. })
					if *owner == AssetHubWestendReceiver::get()
			)),
			"Token minted to beneficiary."
		);
	});
}

// Tests a full cycle of transferring weth from Ethereum to AHW, to AHR using the P<>K bridge,
// and back again to Ethereum. The transaction is done in 2 transaction hops, per direction.
#[test]
fn send_weth_from_ethereum_to_ahw_to_ahr_back_to_ahw_and_ethereum() {
	let sender = AssetHubWestendSender::get();
	BridgeHubWestend::fund_para_sovereign(AssetHubWestend::para_id(), INITIAL_FUND);
	BridgeHubRococo::fund_para_sovereign(AssetHubRococo::para_id(), INITIAL_FUND);
	let ethereum_destination =
		Location::new(2, [GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })]);
	let ethereum_sovereign: AccountId = AssetHubWestend::execute_with(|| {
		ExternalConsensusLocationsConverterFor::<
			AssetHubWestendUniversalLocation,
			[u8; 32],
		>::convert_location(&ethereum_destination.clone())
			.unwrap()
			.into()
	});
	AssetHubWestend::fund_accounts(vec![
		(ethereum_sovereign, INITIAL_FUND),
		// to pay fees to AHR
		(sender.clone(), INITIAL_FUND),
	]);

	let bridged_wnd_at_asset_hub_rococo = bridged_wnd_at_ah_rococo();
	let wnd_reserve = vec![(asset_hub_westend_global_location(), false).into()];
	create_foreign_on_ah_rococo(bridged_wnd_at_asset_hub_rococo.clone(), true, wnd_reserve);
	create_foreign_pool_with_parent_native_on!(
		AssetHubRococo,
		bridged_wnd_at_asset_hub_rococo.clone(),
		AssetHubRococoSender::get()
	);

	// set XCM versions
	BridgeHubWestend::force_xcm_version(asset_hub_westend_global_location(), XCM_VERSION);
	BridgeHubWestend::force_xcm_version(asset_hub_rococo_location(), XCM_VERSION);
	AssetHubWestend::force_xcm_version(asset_hub_rococo_location(), XCM_VERSION);
	AssetHubRococo::force_xcm_version(asset_hub_westend_global_location(), XCM_VERSION);
	BridgeHubRococo::force_xcm_version(asset_hub_westend_global_location(), XCM_VERSION);
	BridgeHubRococo::force_xcm_version(asset_hub_rococo_location(), XCM_VERSION);

	// Bridge token from Ethereum to AHP
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		// Construct SendToken message and sent to inbound queue
		let message = VersionedMessage::V1(MessageV1 {
			chain_id: SEPOLIA_ID,
			command: Command::SendToken {
				token: WETH.into(),
				destination: Destination::AccountId32 { id: sender.clone().into() },
				amount: TOKEN_AMOUNT,
				fee: XCM_FEE,
			},
		});
		// Convert the message to XCM
		let message_id: H256 = [1; 32].into();
		let (xcm, _) = EthereumInboundQueue::do_convert(message_id, message).unwrap();
		// Send the XCM
		let _ = EthereumInboundQueue::send_xcm(xcm, AssetHubWestend::para_id()).unwrap();

		// Check that the message was sent
		assert_expected_events!(
			BridgeHubWestend,
			vec![
				RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},
			]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		// Check that the token was received and issued as a foreign asset on AssetHub
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { .. }) => {},
			]
		);
	});

	let beneficiary =
		Location::new(0, [AccountId32 { network: None, id: AssetHubRococoReceiver::get().into() }]);
	let weth_location = Location::new(
		2,
		[GlobalConsensus(EthereumNetwork::get()), AccountKey20 { network: None, key: WETH }],
	);
	let fee: Location = Parent.into(); // Dot
	let fees_asset: AssetId = fee.clone().into();
	let custom_xcm_on_dest = Xcm::<()>(vec![DepositAsset {
		assets: Wild(AllCounted(2)),
		beneficiary: beneficiary.clone(),
	}]);

	let assets: Assets =
		vec![(weth_location.clone(), TOKEN_AMOUNT).into(), (fee, XCM_FEE * 3).into()].into();

	assert_ok!(AssetHubWestend::execute_with(|| {
		<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::transfer_assets_using_type_and_then(
			<AssetHubWestend as Chain>::RuntimeOrigin::signed(sender.clone()),
			bx!(asset_hub_rococo_location().into()),
			bx!(assets.clone().into()),
			bx!(TransferType::LocalReserve),
			bx!(fees_asset.clone().into()),
			bx!(TransferType::LocalReserve),
			bx!(VersionedXcm::from(custom_xcm_on_dest.clone())),
			WeightLimit::Unlimited,
		)
	}));

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		let events = AssetHubWestend::events();
		// Check that no assets were trapped
		assert!(
			!events.iter().any(|event| matches!(
				event,
				RuntimeEvent::PolkadotXcm(pallet_xcm::Event::AssetsTrapped { .. })
			)),
			"Assets were trapped, should not happen."
		);
	});

	assert_bridge_hub_westend_message_accepted(true);
	assert_bridge_hub_rococo_message_received();

	AssetHubRococo::execute_with(|| {
		type RuntimeEvent = <AssetHubRococo as Chain>::RuntimeEvent;

		// Check that the token was received and issued as a foreign asset on AssetHub
		assert_expected_events!(
			AssetHubRococo,
			vec![
				// Token was issued to beneficiary
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { asset_id, who: owner, .. }) => {
					asset_id: *asset_id == weth_location,
					owner: *owner == AssetHubRococoReceiver::get().into(),
				},
			]
		);

		let events = AssetHubRococo::events();
		// Check that no assets were trapped
		assert!(
			!events.iter().any(|event| matches!(
				event,
				RuntimeEvent::PolkadotXcm(pallet_xcm::Event::AssetsTrapped { .. })
			)),
			"Assets were trapped, should not happen."
		);
	});

	let beneficiary = Location::new(
		0,
		[AccountId32 { network: None, id: AssetHubWestendReceiver::get().into() }],
	);
	let fee = bridged_wnd_at_asset_hub_rococo;
	let fees_asset: AssetId = fee.clone().into();
	let custom_xcm_on_dest =
		Xcm::<()>(vec![DepositAsset { assets: Wild(AllCounted(2)), beneficiary }]);

	let assets: Assets =
		vec![(weth_location.clone(), TOKEN_AMOUNT).into(), (fee, XCM_FEE).into()].into();

	// Transfer the token back to Westend.
	assert_ok!(AssetHubRococo::execute_with(|| {
		<AssetHubRococo as AssetHubRococoPallet>::PolkadotXcm::transfer_assets_using_type_and_then(
			<AssetHubRococo as Chain>::RuntimeOrigin::signed(AssetHubRococoReceiver::get()),
			bx!(asset_hub_westend_global_location().into()),
			bx!(assets.into()),
			bx!(TransferType::DestinationReserve),
			bx!(fees_asset.into()),
			bx!(TransferType::DestinationReserve),
			bx!(VersionedXcm::from(custom_xcm_on_dest)),
			WeightLimit::Unlimited,
		)
	}));

	BridgeHubRococo::execute_with(|| {
		type RuntimeEvent = <BridgeHubRococo as Chain>::RuntimeEvent;
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
	});

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
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		// Check that the token was received and issued as a foreign asset on AssetHub
		assert_expected_events!(
			AssetHubWestend,
			vec![
				// Token was issued to beneficiary
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { asset_id, who: owner, .. }) => {
					asset_id: *asset_id == weth_location,
					owner: *owner == AssetHubWestendReceiver::get().into(),
				},
			]
		);

		let events = AssetHubWestend::events();
		// Check that no assets were trapped
		assert!(
			!events.iter().any(|event| matches!(
				event,
				RuntimeEvent::PolkadotXcm(pallet_xcm::Event::AssetsTrapped { .. })
			)),
			"Assets were trapped, should not happen."
		);
	});

	// Transfer the token back to Ethereum.
	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		let assets = vec![Asset {
			id: AssetId(Location::new(
				2,
				[
					GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID }),
					AccountKey20 { network: None, key: WETH },
				],
			)),
			fun: Fungible(TOKEN_AMOUNT),
		}];

		let versioned_assets = VersionedAssets::from(Assets::from(assets));

		let destination = VersionedLocation::from(Location::new(
			2,
			[GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })],
		));

		let beneficiary = VersionedLocation::from(Location::new(
			0,
			[AccountKey20 { network: None, key: ETHEREUM_DESTINATION_ADDRESS.into() }],
		));

		let free_balance_before =
			<AssetHubWestend as AssetHubWestendPallet>::Balances::free_balance(
				AssetHubWestendReceiver::get(),
			);
		// Send the Weth back to Ethereum
		<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::limited_reserve_transfer_assets(
			RuntimeOrigin::signed(AssetHubWestendReceiver::get()),
			Box::new(destination),
			Box::new(beneficiary),
			Box::new(versioned_assets),
			0,
			Unlimited,
		)
		.unwrap();
		let free_balance_after = <AssetHubWestend as AssetHubWestendPallet>::Balances::free_balance(
			AssetHubWestendReceiver::get(),
		);
		// Assert at least DefaultBridgeHubEthereumBaseFee charged from the sender
		let free_balance_diff = free_balance_before - free_balance_after;
		assert!(free_balance_diff > DefaultBridgeHubEthereumBaseFee::get());
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		// Check that the transfer token back to Ethereum message was queue in the Ethereum
		// Outbound Queue
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueue(snowbridge_pallet_outbound_queue::Event::MessageQueued{ .. }) => {},]
		);
	});
}

#[test]
fn transfer_penpal_native_asset() {
	let assethub_sovereign = BridgeHubWestend::sovereign_account_id_of(
		BridgeHubWestend::sibling_location_of(AssetHubWestend::para_id()),
	);
	BridgeHubWestend::fund_accounts(vec![(assethub_sovereign.clone(), INITIAL_FUND)]);

	let pal_at_asset_hub = Location::new(1, [Parachain(PenpalB::para_id().into())]);

	let pal_after_reanchored = Location::new(
		1,
		[GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)), Parachain(PenpalB::para_id().into())],
	);

	let token_id = TokenIdOf::convert_location(&pal_after_reanchored).unwrap();

	let penpal_sovereign = AssetHubWestend::sovereign_account_id_of(
		AssetHubWestend::sibling_location_of(PenpalB::para_id()),
	);
	AssetHubWestend::fund_accounts(vec![(penpal_sovereign.clone(), INITIAL_FUND)]);

	// Register token
	BridgeHubWestend::execute_with(|| {
		type RuntimeOrigin = <BridgeHubWestend as Chain>::RuntimeOrigin;

		assert_ok!(<BridgeHubWestend as BridgeHubWestendPallet>::EthereumSystem::register_token(
			RuntimeOrigin::root(),
			Box::new(VersionedLocation::from(pal_at_asset_hub.clone())),
			AssetMetadata {
				name: "pal".as_bytes().to_vec().try_into().unwrap(),
				symbol: "pal".as_bytes().to_vec().try_into().unwrap(),
				decimals: 12,
			},
		));
	});

	PenpalB::execute_with(|| {
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			Location::parent(),
			&PenpalBSender::get(),
			INITIAL_FUND,
		));
	});

	// Send PAL to Ethereum
	PenpalB::execute_with(|| {
		type RuntimeOrigin = <PenpalB as Chain>::RuntimeOrigin;
		type RuntimeEvent = <PenpalB as Chain>::RuntimeEvent;

		// DOT as fee
		let assets = vec![
			// Should cover the bridge fee
			Asset { id: AssetId(Location::parent()), fun: Fungible(BRIDGE_FEE) },
			Asset { id: AssetId(Location::here()), fun: Fungible(TOKEN_AMOUNT) },
		];

		let beneficiary = Location::new(
			0,
			[AccountKey20 { network: None, key: ETHEREUM_DESTINATION_ADDRESS.into() }],
		);

		let destination = Location::new(1, [Parachain(AssetHubWestend::para_id().into())]);

		let custom_xcm_on_dest = Xcm::<()>(vec![DepositReserveAsset {
			assets: Wild(AllOf {
				id: AssetId(pal_at_asset_hub.clone()),
				fun: WildFungibility::Fungible,
			}),
			dest: ethereum(),
			xcm: vec![
				BuyExecution {
					fees: Asset {
						id: AssetId(pal_after_reanchored.clone()),
						fun: Fungible(TOKEN_AMOUNT),
					},
					weight_limit: Unlimited,
				},
				DepositAsset { assets: Wild(AllCounted(1)), beneficiary },
			]
			.into(),
		}]);

		assert_ok!(<PenpalB as PenpalBPallet>::PolkadotXcm::transfer_assets_using_type_and_then(
			RuntimeOrigin::signed(PenpalBSender::get()),
			Box::new(VersionedLocation::from(destination)),
			Box::new(VersionedAssets::from(assets)),
			Box::new(TransferType::Teleport),
			Box::new(VersionedAssetId::from(AssetId(Location::parent()))),
			Box::new(TransferType::DestinationReserve),
			Box::new(VersionedXcm::from(custom_xcm_on_dest)),
			Unlimited,
		));

		assert_expected_events!(
			PenpalB,
			vec![RuntimeEvent::Assets(pallet_assets::Event::Withdrawn{ .. }) => {},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { .. }) => {},]
		);
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueue(snowbridge_pallet_outbound_queue::Event::MessageQueued{ .. }) => {},]
		);
	});

	// Send PAL back from Ethereum
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		let message = VersionedMessage::V1(MessageV1 {
			chain_id: SEPOLIA_ID,
			command: Command::SendNativeToken {
				token_id,
				destination: Destination::AccountId32 { id: AssetHubWestendSender::get().into() },
				amount: TOKEN_AMOUNT,
				fee: XCM_FEE,
			},
		});
		// Convert the message to XCM
		let (xcm, _) = EthereumInboundQueue::do_convert([0; 32].into(), message).unwrap();
		// Send the XCM
		let _ = EthereumInboundQueue::send_xcm(xcm, AssetHubWestend::para_id()).unwrap();

		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Withdrawn{..}) => {},]
		);

		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited{..}) => {},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		let destination = AssetHubWestend::sibling_location_of(PenpalB::para_id());

		let beneficiary =
			Location::new(0, [AccountId32 { network: None, id: PenpalBReceiver::get().into() }]);

		// DOT as fee
		let assets =
			vec![Asset { id: AssetId(pal_at_asset_hub.clone()), fun: Fungible(TOKEN_AMOUNT) }];

		assert_ok!(
			<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::limited_teleport_assets(
				RuntimeOrigin::signed(AssetHubWestendSender::get()),
				Box::new(VersionedLocation::from(destination)),
				Box::new(VersionedLocation::from(beneficiary)),
				Box::new(VersionedAssets::from(assets)),
				0,
				Unlimited,
			)
		);

		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Withdrawn{..}) => {},]
		);
	});

	PenpalB::execute_with(|| {
		type RuntimeEvent = <PenpalB as Chain>::RuntimeEvent;

		assert_expected_events!(
			PenpalB,
			vec![RuntimeEvent::Balances(pallet_balances::Event::Deposit{..}) => {},]
		);
	})
}

#[test]
fn transfer_penpal_teleport_enabled_asset() {
	let assethub_sovereign = BridgeHubWestend::sovereign_account_id_of(
		BridgeHubWestend::sibling_location_of(AssetHubWestend::para_id()),
	);
	BridgeHubWestend::fund_accounts(vec![(assethub_sovereign.clone(), INITIAL_FUND)]);

	let asset_location_on_penpal = PenpalB::execute_with(|| PenpalLocalPen2Asset::get());

	let pal_at_asset_hub = Location::new(1, [Junction::Parachain(PenpalB::para_id().into())])
		.appended_with(asset_location_on_penpal.clone())
		.unwrap();

	let pal_after_reanchored = Location::new(
		1,
		[GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)), Parachain(PenpalB::para_id().into())],
	)
	.appended_with(asset_location_on_penpal.clone())
	.unwrap();

	let token_id = TokenIdOf::convert_location(&pal_after_reanchored).unwrap();

	let penpal_sovereign = AssetHubWestend::sovereign_account_id_of(
		AssetHubWestend::sibling_location_of(PenpalB::para_id()),
	);
	AssetHubWestend::fund_accounts(vec![(penpal_sovereign.clone(), INITIAL_FUND)]);
	AssetHubWestend::fund_accounts(vec![(snowbridge_sovereign(), INITIAL_FUND)]);

	// Register token
	BridgeHubWestend::execute_with(|| {
		type RuntimeOrigin = <BridgeHubWestend as Chain>::RuntimeOrigin;

		assert_ok!(<BridgeHubWestend as BridgeHubWestendPallet>::EthereumSystem::register_token(
			RuntimeOrigin::root(),
			Box::new(VersionedLocation::from(pal_at_asset_hub.clone())),
			AssetMetadata {
				name: "pal".as_bytes().to_vec().try_into().unwrap(),
				symbol: "pal".as_bytes().to_vec().try_into().unwrap(),
				decimals: 12,
			},
		));
	});

	// Fund on Penpal
	PenpalB::fund_accounts(vec![(PenpalCheckingAccount::get(), INITIAL_FUND)]);
	PenpalB::execute_with(|| {
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			PenpalLocalPen2Asset::get(),
			&PenpalBSender::get(),
			INITIAL_FUND,
		));
		assert_ok!(<PenpalB as PenpalBPallet>::Assets::mint_into(
			Location::parent(),
			&PenpalBSender::get(),
			INITIAL_FUND,
		));
	});

	// Send PAL to Ethereum
	PenpalB::execute_with(|| {
		type RuntimeOrigin = <PenpalB as Chain>::RuntimeOrigin;
		type RuntimeEvent = <PenpalB as Chain>::RuntimeEvent;

		// DOT as fee
		let assets = vec![
			// Should cover the bridge fee
			Asset { id: AssetId(Location::parent()), fun: Fungible(BRIDGE_FEE) },
			Asset { id: AssetId(asset_location_on_penpal.clone()), fun: Fungible(TOKEN_AMOUNT) },
		];

		let beneficiary = Location::new(
			0,
			[AccountKey20 { network: None, key: ETHEREUM_DESTINATION_ADDRESS.into() }],
		);

		let destination = Location::new(1, [Parachain(AssetHubWestend::para_id().into())]);

		let custom_xcm_on_dest = Xcm::<()>(vec![DepositReserveAsset {
			assets: Wild(AllOf {
				id: AssetId(pal_at_asset_hub.clone()),
				fun: WildFungibility::Fungible,
			}),
			dest: ethereum(),
			xcm: vec![
				BuyExecution {
					fees: Asset {
						id: AssetId(pal_after_reanchored.clone()),
						fun: Fungible(TOKEN_AMOUNT),
					},
					weight_limit: Unlimited,
				},
				DepositAsset { assets: Wild(AllCounted(1)), beneficiary },
			]
			.into(),
		}]);

		assert_ok!(<PenpalB as PenpalBPallet>::PolkadotXcm::transfer_assets_using_type_and_then(
			RuntimeOrigin::signed(PenpalBSender::get()),
			Box::new(VersionedLocation::from(destination)),
			Box::new(VersionedAssets::from(assets)),
			Box::new(TransferType::Teleport),
			Box::new(VersionedAssetId::from(AssetId(Location::parent()))),
			Box::new(TransferType::DestinationReserve),
			Box::new(VersionedXcm::from(custom_xcm_on_dest)),
			Unlimited,
		));

		assert_expected_events!(
			PenpalB,
			vec![RuntimeEvent::Assets(pallet_assets::Event::Withdrawn{ .. }) => {},]
		);

		assert_expected_events!(
			PenpalB,
			vec![RuntimeEvent::Assets(pallet_assets::Event::Withdrawn{ .. }) => {},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { .. }) => {},]
		);
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueue(snowbridge_pallet_outbound_queue::Event::MessageQueued{ .. }) => {},]
		);
	});

	// Send PAL back from Ethereum
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		let message = VersionedMessage::V1(MessageV1 {
			chain_id: SEPOLIA_ID,
			command: Command::SendNativeToken {
				token_id,
				destination: Destination::AccountId32 { id: AssetHubWestendSender::get().into() },
				amount: TOKEN_AMOUNT,
				fee: XCM_FEE,
			},
		});
		// Convert the message to XCM
		let (xcm, _) = EthereumInboundQueue::do_convert([0; 32].into(), message).unwrap();
		// Send the XCM
		let _ = EthereumInboundQueue::send_xcm(xcm, AssetHubWestend::para_id()).unwrap();

		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) =>
	{},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Withdrawn{..}) => {},]
		);

		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited{..}) => {},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		let destination = AssetHubWestend::sibling_location_of(PenpalB::para_id());

		let beneficiary =
			Location::new(0, [AccountId32 { network: None, id: PenpalBReceiver::get().into() }]);

		// DOT as fee
		let assets = vec![
			Asset { id: AssetId(Location::parent()), fun: Fungible(XCM_FEE) },
			Asset { id: AssetId(pal_at_asset_hub.clone()), fun: Fungible(TOKEN_AMOUNT) },
		];

		let custom_xcm_on_dest = Xcm::<()>(vec![
			BuyExecution {
				fees: Asset { id: AssetId(Location::parent()), fun: Fungible(XCM_FEE) },
				weight_limit: Unlimited,
			},
			DepositAsset { assets: Wild(AllCounted(2)), beneficiary },
		]);

		assert_ok!(
			<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::transfer_assets_using_type_and_then(
				RuntimeOrigin::signed(AssetHubWestendSender::get()),
				Box::new(VersionedLocation::from(destination)),
				Box::new(VersionedAssets::from(assets)),
				Box::new(TransferType::Teleport),
				Box::new(VersionedAssetId::from(AssetId(Location::parent()))),
				Box::new(TransferType::LocalReserve),
				Box::new(VersionedXcm::from(custom_xcm_on_dest)),
				Unlimited,
			)
		);

		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Withdrawn{..}) => {},]
		);
	});

	PenpalB::execute_with(|| {
		type RuntimeEvent = <PenpalB as Chain>::RuntimeEvent;

		assert_expected_events!(
			PenpalB,
			vec![RuntimeEvent::Assets(pallet_assets::Event::Deposited{..}) => {},]
		);
	})
}

#[test]
fn mint_native_asset_on_penpal_from_relay_chain() {
	// Send XCM message from Relay Chain to Penpal
	Westend::execute_with(|| {
		Dmp::make_parachain_reachable(PenpalB::para_id());
		// Set balance call
		let mint_token_call = hex!("0a0800d43593c715fdd31c61141abd04a99fd6822c8558854ccde39a5684e7a56da27d0f00406352bfc601");
		let remote_xcm = VersionedXcm::from(Xcm(vec![
			UnpaidExecution { weight_limit: Unlimited, check_origin: None },
			Transact {
				origin_kind: OriginKind::Superuser,
				fallback_max_weight: None,
				call: mint_token_call.to_vec().into(),
			},
		]));
		assert_ok!(<Westend as WestendPallet>::XcmPallet::send(
			<Westend as Chain>::RuntimeOrigin::root(),
			bx!(VersionedLocation::from(Location::new(0, [Parachain(PenpalB::para_id().into())]))),
			bx!(remote_xcm),
		));

		type RuntimeEvent = <Westend as Chain>::RuntimeEvent;
		// Check that the Transact message was sent
		assert_expected_events!(
			Westend,
			vec![
				RuntimeEvent::XcmPallet(pallet_xcm::Event::Sent { .. }) => {},
			]
		);
	});

	PenpalB::execute_with(|| {
		type RuntimeEvent = <PenpalB as Chain>::RuntimeEvent;
		// Check that a message was sent to Ethereum to create the agent
		assert_expected_events!(
			PenpalB,
			vec![
				RuntimeEvent::Balances(pallet_balances::Event::BalanceSet {
					..
				}) => {},
			]
		);
	});
}

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

#[test]
fn transfer_roc_from_ah_with_legacy_api_will_fail() {
	let assethub_sovereign = BridgeHubWestend::sovereign_account_id_of(
		BridgeHubWestend::sibling_location_of(AssetHubWestend::para_id()),
	);
	BridgeHubWestend::fund_accounts(vec![(assethub_sovereign.clone(), INITIAL_FUND)]);

	let ethereum_destination =
		Location::new(2, [GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })]);

	let ethereum_sovereign: AccountId = snowbridge_sovereign();
	AssetHubWestend::fund_accounts(vec![(ethereum_sovereign.clone(), INITIAL_FUND)]);

	let bridged_roc_at_asset_hub_westend = bridged_roc_at_ah_westend();

	create_foreign_on_ah_westend(
		bridged_roc_at_asset_hub_westend.clone(),
		true,
		vec![(asset_hub_rococo_location(), false).into()],
		vec![],
	);

	let asset_id: Location = bridged_roc_at_asset_hub_westend.clone();

	let initial_fund: u128 = 200_000_000_000_000;
	let initial_liquidity: u128 = initial_fund / 2;
	// Setup pool and add liquidity
	set_up_pool_with_wnd_on_ah_westend(
		bridged_roc_at_asset_hub_westend.clone(),
		true,
		initial_fund,
		initial_liquidity,
	);

	register_roc_on_bh();

	// Send token to Ethereum
	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

		// Send partial of the token, will fail if send all
		let assets =
			vec![Asset { id: AssetId(asset_id.clone()), fun: Fungible(initial_fund / 10) }];
		let versioned_assets = VersionedAssets::from(Assets::from(assets));

		let beneficiary = VersionedLocation::from(Location::new(
			0,
			[AccountKey20 { network: None, key: ETHEREUM_DESTINATION_ADDRESS.into() }],
		));

		let result = <AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::transfer_assets(
			RuntimeOrigin::signed(AssetHubWestendSender::get()),
			Box::new(VersionedLocation::from(ethereum_destination)),
			Box::new(beneficiary),
			Box::new(versioned_assets),
			0,
			Unlimited,
		);

		assert_err!(
			result,
			DispatchError::Module(sp_runtime::ModuleError {
				index: 31,
				error: [21, 0, 0, 0],
				message: Some("InvalidAssetUnknownReserve")
			})
		);
	});
}

#[test]
fn transfer_roc_from_ah_with_transfer_and_then() {
	let assethub_sovereign = BridgeHubWestend::sovereign_account_id_of(
		BridgeHubWestend::sibling_location_of(AssetHubWestend::para_id()),
	);
	BridgeHubWestend::fund_accounts(vec![(assethub_sovereign.clone(), INITIAL_FUND)]);

	let ethereum_destination =
		Location::new(2, [GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })]);

	let ethereum_sovereign: AccountId = snowbridge_sovereign();
	AssetHubWestend::fund_accounts(vec![(ethereum_sovereign.clone(), INITIAL_FUND)]);

	let bridged_roc_at_asset_hub_westend = bridged_roc_at_ah_westend();

	create_foreign_on_ah_westend(
		bridged_roc_at_asset_hub_westend.clone(),
		true,
		vec![(asset_hub_rococo_location(), false).into()],
		vec![],
	);

	let asset_id: Location = bridged_roc_at_asset_hub_westend.clone();

	let initial_fund: u128 = 200_000_000_000_000;
	let initial_liquidity: u128 = initial_fund / 2;
	// Setup pool and add liquidity
	set_up_pool_with_wnd_on_ah_westend(
		bridged_roc_at_asset_hub_westend.clone(),
		true,
		initial_fund,
		initial_liquidity,
	);

	register_roc_on_bh();

	// Send token to Ethereum
	AssetHubWestend::execute_with(|| {
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		// Send partial of the token, will fail if send all
		let asset = Asset { id: AssetId(asset_id.clone()), fun: Fungible(initial_fund / 10) };
		let assets = vec![asset.clone()];
		let versioned_assets = VersionedAssets::from(Assets::from(assets.clone()));

		let beneficiary = Location::new(
			0,
			[AccountKey20 { network: None, key: ETHEREUM_DESTINATION_ADDRESS.into() }],
		);

		let custom_xcm = Xcm::<()>(vec![DepositAsset {
			assets: Wild(AllCounted(assets.len() as u32)),
			beneficiary,
		}]);

		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::transfer_assets_using_type_and_then(
			RuntimeOrigin::signed(AssetHubWestendSender::get()),
			Box::new(VersionedLocation::from(ethereum_destination)),
			Box::new(versioned_assets),
			Box::new(TransferType::LocalReserve),
			Box::new(VersionedAssetId::from(asset_id.clone())),
			Box::new(TransferType::LocalReserve),
			Box::new(VersionedXcm::from(custom_xcm)),
			Unlimited,
		));

		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Transferred{ .. }) => {},]
		);
	});

	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		// Check that the transfer token back to Ethereum message was queue in the Ethereum
		// Outbound Queue
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueue(snowbridge_pallet_outbound_queue::Event::MessageQueued{ .. }) => {},]
		);
	});

	// Send token back from Ethereum
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;
		let asset_id_after_reanchor: Location =
			Location::new(1, [GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH))]);
		let token_id = TokenIdOf::convert_location(&asset_id_after_reanchor).unwrap();
		let message = VersionedMessage::V1(MessageV1 {
			chain_id: SEPOLIA_ID,
			command: Command::SendNativeToken {
				token_id,
				destination: Destination::AccountId32 { id: AssetHubWestendReceiver::get().into() },
				amount: initial_fund / 10,
				fee: XCM_FEE,
			},
		});
		let (xcm, _) = EthereumInboundQueue::do_convert([0; 32].into(), message).unwrap();
		let _ = EthereumInboundQueue::send_xcm(xcm, AssetHubWestend::para_id().into()).unwrap();
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		assert_expected_events!(
			AssetHubWestend,
			vec![RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited{..}) => {},]
		);

		let events = AssetHubWestend::events();

		// Check that the native token burnt from reserved account
		assert!(
			events.iter().any(|event| matches!(
				event,
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Withdrawn { who: owner, .. })
					if *owner == ethereum_sovereign.clone(),
			)),
			"token burnt from Ethereum sovereign account."
		);

		// Check that the token was minted to beneficiary
		assert!(
			events.iter().any(|event| matches!(
				event,
				RuntimeEvent::ForeignAssets(pallet_assets::Event::Deposited { who: owner, .. })
					if *owner == AssetHubWestendReceiver::get()
			)),
			"Token minted to beneficiary."
		);
	});
}

#[test]
fn register_pna_in_v5_while_transfer_in_v4_should_work() {
	let assethub_sovereign = BridgeHubWestend::sovereign_account_id_of(
		BridgeHubWestend::sibling_location_of(AssetHubWestend::para_id()),
	);
	BridgeHubWestend::fund_accounts(vec![(assethub_sovereign.clone(), INITIAL_FUND)]);

	let asset_id: Location = Location { parents: 1, interior: [].into() };
	let expected_asset_id: Location = Location {
		parents: 1,
		interior: [GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH))].into(),
	};

	let _expected_token_id = TokenIdOf::convert_location(&expected_asset_id).unwrap();

	let ethereum_sovereign: AccountId = snowbridge_sovereign();

	// Register token in V5
	BridgeHubWestend::execute_with(|| {
		type RuntimeOrigin = <BridgeHubWestend as Chain>::RuntimeOrigin;
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		assert_ok!(<BridgeHubWestend as BridgeHubWestendPallet>::Balances::force_set_balance(
			RuntimeOrigin::root(),
			sp_runtime::MultiAddress::Id(BridgeHubWestendSender::get()),
			INITIAL_FUND * 10,
		));

		assert_ok!(<BridgeHubWestend as BridgeHubWestendPallet>::EthereumSystem::register_token(
			RuntimeOrigin::root(),
			Box::new(VersionedLocation::from(asset_id.clone())),
			AssetMetadata {
				name: "wnd".as_bytes().to_vec().try_into().unwrap(),
				symbol: "wnd".as_bytes().to_vec().try_into().unwrap(),
				decimals: 12,
			},
		));
		// Check that a message was sent to Ethereum to create the agent
		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumSystem(snowbridge_pallet_system::Event::RegisterToken { .. }) => {},]
		);
	});

	AssetHubWestend::force_xcm_version(bridge_hub(), 4);
	AssetHubWestend::force_xcm_version(ethereum(), 4);
	AssetHubWestend::force_default_xcm_version(Some(4));
	BridgeHubWestend::force_default_xcm_version(Some(4));

	// Send token to Ethereum in V4 fomat
	AssetHubWestend::execute_with(|| {
		// LTS is V4
		use xcm::lts::{Junction::*, NetworkId::*, *};
		type RuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;

		let assets = vec![Asset {
			id: AssetId(Location::parent()),
			fun: Fungibility::try_from(Fungible(TOKEN_AMOUNT)).unwrap(),
		}];
		let versioned_assets = VersionedAssets::V4(Assets::from(assets));

		let destination = VersionedLocation::V4(Location::new(
			2,
			[GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID })],
		));

		let beneficiary = Location::new(
			0,
			[AccountKey20 { network: None, key: ETHEREUM_DESTINATION_ADDRESS.into() }],
		);

		assert_ok!(<AssetHubWestend as AssetHubWestendPallet>::PolkadotXcm::transfer_assets_using_type_and_then(
			RuntimeOrigin::signed(AssetHubWestendSender::get()),
			Box::new(destination),
			Box::new(versioned_assets),
			Box::new(TransferType::LocalReserve),
			Box::new(VersionedAssetId::V4(AssetId(Location::parent()))),
			Box::new(TransferType::LocalReserve),
			Box::new(VersionedXcm::V4(
				Xcm::<()>::builder_unsafe()
					.deposit_asset(WildAsset::AllCounted(1), beneficiary)
					.build()
			)),
			Unlimited,
		));

		let events = AssetHubWestend::events();
		// Check that the native asset transferred to some reserved account(sovereign of Ethereum)
		assert!(
			events.iter().any(|event| matches!(
				event,
				RuntimeEvent::Balances(pallet_balances::Event::Transfer { amount, to, ..})
					if *amount == TOKEN_AMOUNT && *to == ethereum_sovereign.clone(),
			)),
			"native token reserved to Ethereum sovereign account."
		);
	});

	// Check that the transfer token back to Ethereum message was queue in the Ethereum
	// Outbound Queue
	BridgeHubWestend::execute_with(|| {
		type RuntimeEvent = <BridgeHubWestend as Chain>::RuntimeEvent;

		assert_expected_events!(
			BridgeHubWestend,
			vec![RuntimeEvent::EthereumOutboundQueue(snowbridge_pallet_outbound_queue::Event::MessageQueued{ .. }) => {},]
		);
	});
}
