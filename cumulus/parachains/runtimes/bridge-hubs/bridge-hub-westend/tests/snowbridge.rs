// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

#![cfg(test)]

use bp_asset_hub_westend::ASSET_HUB_WESTEND_PARACHAIN_ID;
use bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID;
use bp_polkadot_core::Signature;
use bridge_hub_test_utils::XcmReceivedFrom;
use bridge_hub_westend_runtime::{
	bridge_to_rococo_config, xcm_config::XcmConfig, AllPalletsWithoutSystem,
	BridgeRejectObsoleteHeadersAndMessages, Executive, MessageQueueServiceWeight, Runtime,
	RuntimeCall, RuntimeEvent, SessionKeys, TxExtension, UncheckedExtrinsic,
};
use codec::{Decode, Encode};
use cumulus_primitives_core::XcmError::FailedToTransactAsset;
use frame_support::parameter_types;
use parachains_common::{AccountId, AuraId, Balance};
use parachains_runtimes_test_utils::RuntimeHelper;
use snowbridge_pallet_ethereum_client::WeightInfo;
use sp_core::H160;
use sp_runtime::{
	generic::{Era, SignedPayload},
	AccountId32,
};
use xcm::latest::prelude::*;

parameter_types! {
		pub const DefaultBridgeHubEthereumBaseFee: Balance = 3_833_568_200_000;
}

fn collator_session_keys() -> bridge_hub_test_utils::CollatorSessionKeys<Runtime> {
	use sp_keyring::Sr25519Keyring::Alice;
	bridge_hub_test_utils::CollatorSessionKeys::new(
		AccountId::from(Alice),
		AccountId::from(Alice),
		SessionKeys { aura: AuraId::from(Alice.public()) },
	)
}

#[test]
pub fn transfer_token_to_ethereum_works() {
	snowbridge_runtime_test_common::send_transfer_token_message_success::<Runtime, XcmConfig>(
		11155111,
		collator_session_keys(),
		BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		ASSET_HUB_WESTEND_PARACHAIN_ID,
		H160::random(),
		H160::random(),
		DefaultBridgeHubEthereumBaseFee::get(),
		Box::new(|runtime_event_encoded: Vec<u8>| {
			match RuntimeEvent::decode(&mut &runtime_event_encoded[..]) {
				Ok(RuntimeEvent::EthereumOutboundQueue(event)) => Some(event),
				_ => None,
			}
		}),
	)
}

#[test]
pub fn unpaid_transfer_token_to_ethereum_should_work() {
	snowbridge_runtime_test_common::send_unpaid_transfer_token_message::<Runtime, XcmConfig>(
		11155111,
		collator_session_keys(),
		BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		ASSET_HUB_WESTEND_PARACHAIN_ID,
		H160::random(),
		H160::random(),
	)
}

#[test]
pub fn transfer_token_to_ethereum_insufficient_fund() {
	snowbridge_runtime_test_common::send_transfer_token_message_failure::<Runtime, XcmConfig>(
		11155111,
		collator_session_keys(),
		BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		ASSET_HUB_WESTEND_PARACHAIN_ID,
		1_000_000_000,
		H160::random(),
		H160::random(),
		DefaultBridgeHubEthereumBaseFee::get(),
		FailedToTransactAsset("Funds are unavailable"),
	)
}

#[test]
fn max_message_queue_service_weight_is_more_than_beacon_extrinsic_weights() {
	let max_message_queue_weight = MessageQueueServiceWeight::get();
	let force_checkpoint =
		<Runtime as snowbridge_pallet_ethereum_client::Config>::WeightInfo::force_checkpoint();
	let submit_checkpoint =
		<Runtime as snowbridge_pallet_ethereum_client::Config>::WeightInfo::submit();
	max_message_queue_weight.all_gt(force_checkpoint);
	max_message_queue_weight.all_gt(submit_checkpoint);
}

#[test]
fn ethereum_client_consensus_extrinsics_work() {
	snowbridge_runtime_test_common::ethereum_extrinsic(
		collator_session_keys(),
		BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		construct_and_apply_extrinsic,
	);
}

#[test]
fn ethereum_to_polkadot_message_extrinsics_work() {
	snowbridge_runtime_test_common::ethereum_to_polkadot_message_extrinsics_work(
		collator_session_keys(),
		BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		construct_and_apply_extrinsic,
	);
}

/// Tests that the digest items are as expected when a Ethereum Outbound message is received.
/// If the MessageQueue pallet is configured before (i.e. the MessageQueue pallet is listed before
/// the EthereumOutboundQueue in the construct_runtime macro) the EthereumOutboundQueue, this test
/// will fail.
#[test]
pub fn ethereum_outbound_queue_processes_messages_before_message_queue_works() {
	snowbridge_runtime_test_common::ethereum_outbound_queue_processes_messages_before_message_queue_works::<
		Runtime,
		XcmConfig,
		AllPalletsWithoutSystem,
	>(
		11155111,
		collator_session_keys(),
		BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		ASSET_HUB_WESTEND_PARACHAIN_ID,
		H160::random(),
		H160::random(),
		DefaultBridgeHubEthereumBaseFee::get(),
		Box::new(|runtime_event_encoded: Vec<u8>| {
			match RuntimeEvent::decode(&mut &runtime_event_encoded[..]) {
				Ok(RuntimeEvent::EthereumOutboundQueue(event)) => Some(event),
				_ => None,
			}
		}),
	)
}

fn construct_extrinsic(
	sender: sp_keyring::Sr25519Keyring,
	call: RuntimeCall,
) -> UncheckedExtrinsic {
	let account_id = AccountId32::from(sender.public());
	let extra: TxExtension = (
		(
			frame_system::AuthorizeCall::<Runtime>::new(),
			frame_system::CheckNonZeroSender::<Runtime>::new(),
			frame_system::CheckSpecVersion::<Runtime>::new(),
			frame_system::CheckTxVersion::<Runtime>::new(),
			frame_system::CheckGenesis::<Runtime>::new(),
			frame_system::CheckEra::<Runtime>::from(Era::immortal()),
			frame_system::CheckNonce::<Runtime>::from(
				frame_system::Pallet::<Runtime>::account(&account_id).nonce,
			),
			frame_system::CheckWeight::<Runtime>::new(),
		),
		pallet_transaction_payment::ChargeTransactionPayment::<Runtime>::from(0),
		BridgeRejectObsoleteHeadersAndMessages::default(),
		(bridge_to_rococo_config::OnBridgeHubWestendRefundBridgeHubRococoMessages::default(),),
		frame_metadata_hash_extension::CheckMetadataHash::<Runtime>::new(false),
	)
		.into();
	let payload = SignedPayload::new(call.clone(), extra.clone()).unwrap();
	let signature = payload.using_encoded(|e| sender.sign(e));
	UncheckedExtrinsic::new_signed(call, account_id.into(), Signature::Sr25519(signature), extra)
}

fn construct_and_apply_extrinsic(
	origin: sp_keyring::Sr25519Keyring,
	call: RuntimeCall,
) -> sp_runtime::DispatchOutcome {
	let xt = construct_extrinsic(origin, call);
	let r = Executive::apply_extrinsic(xt);
	r.unwrap()
}

#[test]
pub fn signed_assethub_user_cannot_forge_assethub_agent_origin() {
	let assethub_parachain_id = ASSET_HUB_WESTEND_PARACHAIN_ID;
	let weth_contract_address = H160::random();
	let destination_address = H160::random();
	let fee_amount = DefaultBridgeHubEthereumBaseFee::get();
	let ethereum_chain_id = 11155111;

	let collator_session_key = collator_session_keys();

	bridge_hub_test_utils::ExtBuilder::<Runtime>::default()
		.with_collators(collator_session_key.collators())
		.with_session_keys(collator_session_key.session_keys())
		.with_para_id(BRIDGE_HUB_WESTEND_PARACHAIN_ID.into())
		.with_tracing()
		.build()
		.execute_with(|| {
			<snowbridge_pallet_system::Pallet<Runtime>>::initialize(
				BRIDGE_HUB_WESTEND_PARACHAIN_ID.into(),
				assethub_parachain_id.into(),
			)
			.unwrap();

			// fund asset hub sovereign account enough so it can pay fees
			snowbridge_runtime_test_common::initial_fund::<Runtime>(
				assethub_parachain_id,
				5_000_000_000_000,
			);

			let fee_asset = Asset { id: AssetId(Here.into()), fun: Fungible(fee_amount) };

			let transfer_asset = Asset {
				id: AssetId(Location::new(
					0,
					[AccountKey20 { network: None, key: weth_contract_address.into() }],
				)),
				fun: Fungible(1000000000),
			};

			// Construct a forged V2 XCM that attempts to use AliasOrigin(AssetHubLocation)
			let forged_assethub_origin = Location::new(1, Parachain(assethub_parachain_id));
			let forged_xcm = Xcm(vec![
				WithdrawAsset(Assets::from(vec![fee_asset.clone()])),
				PayFees { asset: fee_asset },
				WithdrawAsset(Assets::from(vec![transfer_asset.clone()])),
				AliasOrigin(forged_assethub_origin),
				DepositAsset {
					assets: Wild(All),
					beneficiary: Location::new(
						0,
						[AccountKey20 { network: None, key: destination_address.into() }],
					),
				},
				SetTopic([9; 32]),
			]);

			let export_xcm = Xcm(vec![
				WithdrawAsset(Assets::from(vec![Asset {
					id: AssetId(Location::new(1, Here)),
					fun: Fungible(fee_amount),
				}])),
				BuyExecution {
					fees: Asset { id: AssetId(Location::new(1, Here)), fun: Fungible(fee_amount) },
					weight_limit: Unlimited,
				},
				ExportMessage {
					network: Ethereum { chain_id: ethereum_chain_id },
					destination: Here,
					xcm: forged_xcm,
				},
			]);

			let assethub_parachain_location = Location::new(1, Parachain(assethub_parachain_id));
			let mut hash = export_xcm.using_encoded(sp_io::hashing::blake2_256);
			let outcome = xcm_executor::XcmExecutor::<XcmConfig>::prepare_and_execute(
				assethub_parachain_location,
				export_xcm,
				&mut hash,
				RuntimeHelper::<Runtime, AllPalletsWithoutSystem>::xcm_max_weight(
					XcmReceivedFrom::Sibling,
				),
				Weight::zero(),
			);

			// Assert that the message failed to execute due to "Unroutable" error inside the
			// exporter
			assert!(matches!(
				outcome,
				Outcome::Incomplete {
					error: InstructionError { error: XcmError::Unroutable, .. },
					..
				}
			));

			// Check that no messages were queued in the outbound queue
			let committed_messages =
				snowbridge_pallet_outbound_queue_v2::Messages::<Runtime>::get();
			assert_eq!(committed_messages.len(), 0);
		});
}
