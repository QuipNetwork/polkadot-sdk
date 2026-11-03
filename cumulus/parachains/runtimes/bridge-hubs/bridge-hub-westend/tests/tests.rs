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

use crate::bridge_common_config::BridgeRewardBeneficiaries;
use bp_messages::LegacyLaneId;
use bp_polkadot_core::Signature;
use bp_relayers::{PayRewardFromAccount, RewardsAccountOwner, RewardsAccountParams};
use bridge_common_config::{BridgeRelayersInstance, BridgeReward, RequiredStakeForStakeAndSlash};
use bridge_hub_test_utils::{
	test_cases::{from_parachain, run_test},
	GovernanceOrigin, SlotDurations,
};
use bridge_hub_westend_runtime::{
	bridge_common_config, bridge_to_rococo_config,
	bridge_to_rococo_config::RococoGlobalConsensusNetwork,
	xcm_config::{
		GovernanceLocation, LocationToAccountId, RelayNetwork, WestendLocation, XcmConfig,
	},
	AllPalletsWithoutSystem, Balances, Block, BridgeRejectObsoleteHeadersAndMessages,
	BridgeRelayers, Executive, ExistentialDeposit, ParachainSystem, PolkadotXcm, Runtime,
	RuntimeCall, RuntimeEvent, RuntimeOrigin, SessionKeys, TransactionPayment, TxExtension,
	UncheckedExtrinsic,
};
use bridge_to_rococo_config::{
	BridgeGrandpaRococoInstance, BridgeHubRococoLocation, BridgeParachainRococoInstance,
	DeliveryRewardInBalance, WithBridgeHubRococoMessagesInstance, XcmOverBridgeHubRococoInstance,
};
use codec::{Decode, Encode};
use cumulus_primitives_core::UpwardMessageSender;
use frame_support::{
	assert_err, assert_ok,
	dispatch::GetDispatchInfo,
	parameter_types,
	traits::{
		fungible::{Inspect, Mutate},
		ConstU8,
	},
};
use hex_literal::hex;
use parachains_common::{AccountId, AuraId, Balance};
use parachains_runtimes_test_utils::ExtBuilder;
use sp_consensus_aura::SlotDuration;
use sp_core::crypto::Ss58Codec;
use sp_keyring::Sr25519Keyring::{Alice, Bob};
use sp_runtime::{
	generic::{Era, SignedPayload},
	AccountId32, Either, Perbill,
};
use testnet_parachains_constants::westend::{consensus::*, fee::WeightToFee};
use xcm::{
	latest::{prelude::*, ROCOCO_GENESIS_HASH, WESTEND_GENESIS_HASH},
	VersionedLocation,
};
use xcm_runtime_apis::conversions::LocationToAccountHelper;

// Random para id of sibling chain used in tests.
pub const SIBLING_PARACHAIN_ID: u32 = 2053;
// Random para id of sibling chain used in tests.
pub const SIBLING_SYSTEM_PARACHAIN_ID: u32 = 1008;
// Random para id of bridged chain from different global consensus used in tests.
pub const BRIDGED_LOCATION_PARACHAIN_ID: u32 = 1075;

parameter_types! {
	pub SiblingParachainLocation: Location = Location::new(1, [Parachain(SIBLING_PARACHAIN_ID)]);
	pub SiblingSystemParachainLocation: Location = Location::new(1, [Parachain(SIBLING_SYSTEM_PARACHAIN_ID)]);
	pub BridgedUniversalLocation: InteriorLocation = [GlobalConsensus(RococoGlobalConsensusNetwork::get()), Parachain(BRIDGED_LOCATION_PARACHAIN_ID)].into();
	pub Governance: GovernanceOrigin<RuntimeOrigin> = GovernanceOrigin::Location(GovernanceLocation::get());
}

// Runtime from tests PoV
type RuntimeTestsAdapter = from_parachain::WithRemoteParachainHelperAdapter<
	Runtime,
	AllPalletsWithoutSystem,
	BridgeGrandpaRococoInstance,
	BridgeParachainRococoInstance,
	WithBridgeHubRococoMessagesInstance,
	BridgeRelayersInstance,
>;

fn construct_extrinsic(
	sender: sp_keyring::Sr25519Keyring,
	call: RuntimeCall,
) -> UncheckedExtrinsic {
	let account_id = AccountId32::from(sender.public());
	let tx_ext: TxExtension = (
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
		frame_metadata_hash_extension::CheckMetadataHash::new(false),
	)
		.into();
	let payload = SignedPayload::new(call.clone(), tx_ext.clone()).unwrap();
	let signature = payload.using_encoded(|e| sender.sign(e));
	UncheckedExtrinsic::new_signed(call, account_id.into(), Signature::Sr25519(signature), tx_ext)
}

fn construct_and_apply_extrinsic(
	relayer_at_target: sp_keyring::Sr25519Keyring,
	call: RuntimeCall,
) -> sp_runtime::DispatchOutcome {
	let xt = construct_extrinsic(relayer_at_target, call);
	let r = Executive::apply_extrinsic(xt);
	r.unwrap()
}

fn construct_and_estimate_extrinsic_fee(call: RuntimeCall) -> Balance {
	let info = call.get_dispatch_info();
	let xt = construct_extrinsic(Alice, call);
	TransactionPayment::compute_fee(xt.encoded_size() as _, &info, 0)
}

fn collator_session_keys() -> bridge_hub_test_utils::CollatorSessionKeys<Runtime> {
	bridge_hub_test_utils::CollatorSessionKeys::new(
		AccountId::from(Alice),
		AccountId::from(Alice),
		SessionKeys { aura: AuraId::from(Alice.public()) },
	)
}

fn slot_durations() -> SlotDurations {
	SlotDurations {
		relay: SlotDuration::from_millis(RELAY_CHAIN_SLOT_DURATION_MILLIS.into()),
		para: SlotDuration::from_millis(SLOT_DURATION),
	}
}

bridge_hub_test_utils::test_cases::include_teleports_for_native_asset_works!(
	Runtime,
	AllPalletsWithoutSystem,
	XcmConfig,
	(),
	WeightToFee,
	ParachainSystem,
	collator_session_keys(),
	slot_durations(),
	ExistentialDeposit::get(),
	Box::new(|runtime_event_encoded: Vec<u8>| {
		match RuntimeEvent::decode(&mut &runtime_event_encoded[..]) {
			Ok(RuntimeEvent::PolkadotXcm(event)) => Some(event),
			_ => None,
		}
	}),
	bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID
);

#[test]
fn initialize_bridge_by_governance_works() {
	bridge_hub_test_utils::test_cases::initialize_bridge_by_governance_works::<
		Runtime,
		BridgeGrandpaRococoInstance,
	>(
		collator_session_keys(),
		bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		Governance::get(),
	)
}

#[test]
fn change_bridge_grandpa_pallet_mode_by_governance_works() {
	bridge_hub_test_utils::test_cases::change_bridge_grandpa_pallet_mode_by_governance_works::<
		Runtime,
		BridgeGrandpaRococoInstance,
	>(
		collator_session_keys(),
		bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		Governance::get(),
	)
}

#[test]
fn change_bridge_parachains_pallet_mode_by_governance_works() {
	bridge_hub_test_utils::test_cases::change_bridge_parachains_pallet_mode_by_governance_works::<
		Runtime,
		BridgeParachainRococoInstance,
	>(
		collator_session_keys(),
		bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		Governance::get(),
	)
}

#[test]
fn change_bridge_messages_pallet_mode_by_governance_works() {
	bridge_hub_test_utils::test_cases::change_bridge_messages_pallet_mode_by_governance_works::<
		Runtime,
		WithBridgeHubRococoMessagesInstance,
	>(
		collator_session_keys(),
		bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		Governance::get(),
	)
}

#[test]
fn change_delivery_reward_by_governance_works() {
	bridge_hub_test_utils::test_cases::change_storage_constant_by_governance_works::<
		Runtime,
		DeliveryRewardInBalance,
		u64,
	>(
		collator_session_keys(),
		bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		Governance::get(),
		|| (DeliveryRewardInBalance::key().to_vec(), DeliveryRewardInBalance::get()),
		|old_value| old_value.checked_mul(2).unwrap(),
	)
}

#[test]
fn change_required_stake_by_governance_works() {
	bridge_hub_test_utils::test_cases::change_storage_constant_by_governance_works::<
		Runtime,
		RequiredStakeForStakeAndSlash,
		Balance,
	>(
		collator_session_keys(),
		bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		Governance::get(),
		|| (RequiredStakeForStakeAndSlash::key().to_vec(), RequiredStakeForStakeAndSlash::get()),
		|old_value| old_value.checked_mul(2).unwrap(),
	)
}

#[test]
fn handle_export_message_from_system_parachain_add_to_outbound_queue_works() {
	bridge_hub_test_utils::test_cases::handle_export_message_from_system_parachain_to_outbound_queue_works::<
			Runtime,
			XcmConfig,
			WithBridgeHubRococoMessagesInstance,
			LocationToAccountId,
		>(
			collator_session_keys(),
			bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID,
			SIBLING_PARACHAIN_ID,
			Box::new(|runtime_event_encoded: Vec<u8>| {
				match RuntimeEvent::decode(&mut &runtime_event_encoded[..]) {
					Ok(RuntimeEvent::BridgeRococoMessages(event)) => Some(event),
					_ => None,
				}
			}),
			|| ExportMessage { network: RococoGlobalConsensusNetwork::get(), destination: [Parachain(BRIDGED_LOCATION_PARACHAIN_ID)].into(), xcm: Xcm(vec![]) },
			Some((WestendLocation::get(), ExistentialDeposit::get()).into()),
			// value should be >= than value generated by `can_calculate_weight_for_paid_export_message_with_reserve_transfer`
			Some((WestendLocation::get(), bp_bridge_hub_westend::BridgeHubWestendBaseXcmFeeInWnds::get()).into()),
			|| {
				PolkadotXcm::force_xcm_version(RuntimeOrigin::root(), Box::new(BridgeHubRococoLocation::get()), XCM_VERSION).expect("version saved!");

				// we need to create lane between sibling parachain and remote destination
				bridge_hub_test_utils::ensure_opened_bridge::<
					Runtime,
					XcmOverBridgeHubRococoInstance,
					LocationToAccountId,
					WestendLocation,
				>(
					SiblingParachainLocation::get(),
					BridgedUniversalLocation::get(),
					false,
					|locations, _fee| {
						bridge_hub_test_utils::open_bridge_with_storage::<
							Runtime, XcmOverBridgeHubRococoInstance
						>(locations, LegacyLaneId([0, 0, 0, 1]))
					}
				).1
			},
		)
}

#[test]
fn message_dispatch_routing_works() {
	bridge_hub_test_utils::test_cases::message_dispatch_routing_works::<
		Runtime,
		AllPalletsWithoutSystem,
		XcmConfig,
		ParachainSystem,
		WithBridgeHubRococoMessagesInstance,
		RelayNetwork,
		bridge_to_rococo_config::RococoGlobalConsensusNetwork,
		ConstU8<2>,
	>(
		collator_session_keys(),
		slot_durations(),
		bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		SIBLING_PARACHAIN_ID,
		Box::new(|runtime_event_encoded: Vec<u8>| {
			match RuntimeEvent::decode(&mut &runtime_event_encoded[..]) {
				Ok(RuntimeEvent::ParachainSystem(event)) => Some(event),
				_ => None,
			}
		}),
		Box::new(|runtime_event_encoded: Vec<u8>| {
			match RuntimeEvent::decode(&mut &runtime_event_encoded[..]) {
				Ok(RuntimeEvent::XcmpQueue(event)) => Some(event),
				_ => None,
			}
		}),
		|| <ParachainSystem as UpwardMessageSender>::ensure_successful_delivery(),
	)
}

#[test]
fn relayed_incoming_message_works() {
	from_parachain::relayed_incoming_message_works::<RuntimeTestsAdapter>(
		collator_session_keys(),
		slot_durations(),
		bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		bp_bridge_hub_rococo::BRIDGE_HUB_ROCOCO_PARACHAIN_ID,
		SIBLING_PARACHAIN_ID,
		ByGenesis(WESTEND_GENESIS_HASH),
		|| {
			// we need to create lane between sibling parachain and remote destination
			bridge_hub_test_utils::ensure_opened_bridge::<
				Runtime,
				XcmOverBridgeHubRococoInstance,
				LocationToAccountId,
				WestendLocation,
			>(
				SiblingParachainLocation::get(),
				BridgedUniversalLocation::get(),
				false,
				|locations, _fee| {
					bridge_hub_test_utils::open_bridge_with_storage::<
						Runtime,
						XcmOverBridgeHubRococoInstance,
					>(locations, LegacyLaneId([0, 0, 0, 1]))
				},
			)
			.1
		},
		construct_and_apply_extrinsic,
		true,
	)
}

#[test]
fn free_relay_extrinsic_works() {
	// from Rococo
	from_parachain::free_relay_extrinsic_works::<RuntimeTestsAdapter>(
		collator_session_keys(),
		slot_durations(),
		bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		bp_bridge_hub_rococo::BRIDGE_HUB_ROCOCO_PARACHAIN_ID,
		SIBLING_PARACHAIN_ID,
		ByGenesis(WESTEND_GENESIS_HASH),
		|| {
			// we need to create lane between sibling parachain and remote destination
			bridge_hub_test_utils::ensure_opened_bridge::<
				Runtime,
				XcmOverBridgeHubRococoInstance,
				LocationToAccountId,
				WestendLocation,
			>(
				SiblingParachainLocation::get(),
				BridgedUniversalLocation::get(),
				false,
				|locations, _fee| {
					bridge_hub_test_utils::open_bridge_with_storage::<
						Runtime,
						XcmOverBridgeHubRococoInstance,
					>(locations, LegacyLaneId([0, 0, 0, 1]))
				},
			)
			.1
		},
		construct_and_apply_extrinsic,
		true,
	)
}

#[test]
pub fn can_calculate_weight_for_paid_export_message_with_reserve_transfer() {
	bridge_hub_test_utils::check_sane_fees_values(
		"bp_bridge_hub_westend::BridgeHubWestendBaseXcmFeeInWnds",
		bp_bridge_hub_westend::BridgeHubWestendBaseXcmFeeInWnds::get(),
		|| {
			bridge_hub_test_utils::test_cases::can_calculate_weight_for_paid_export_message_with_reserve_transfer::<
			Runtime,
			XcmConfig,
			WeightToFee,
		>()
		},
		Perbill::from_percent(33),
		Some(-33),
		&format!(
			"Estimate fee for `ExportMessage` for runtime: {:?}",
			<Runtime as frame_system::Config>::Version::get()
		),
	)
}

#[test]
pub fn can_calculate_fee_for_standalone_message_delivery_transaction() {
	bridge_hub_test_utils::check_sane_fees_values(
		"bp_bridge_hub_westend::BridgeHubWestendBaseDeliveryFeeInWnds",
		bp_bridge_hub_westend::BridgeHubWestendBaseDeliveryFeeInWnds::get(),
		|| {
			from_parachain::can_calculate_fee_for_standalone_message_delivery_transaction::<
				RuntimeTestsAdapter,
			>(collator_session_keys(), construct_and_estimate_extrinsic_fee)
		},
		Perbill::from_percent(25),
		Some(-25),
		&format!(
			"Estimate fee for `single message delivery` for runtime: {:?}",
			<Runtime as frame_system::Config>::Version::get()
		),
	)
}

#[test]
pub fn can_calculate_fee_for_standalone_message_confirmation_transaction() {
	bridge_hub_test_utils::check_sane_fees_values(
		"bp_bridge_hub_westend::BridgeHubWestendBaseConfirmationFeeInWnds",
		bp_bridge_hub_westend::BridgeHubWestendBaseConfirmationFeeInWnds::get(),
		|| {
			from_parachain::can_calculate_fee_for_standalone_message_confirmation_transaction::<
				RuntimeTestsAdapter,
			>(collator_session_keys(), construct_and_estimate_extrinsic_fee)
		},
		Perbill::from_percent(25),
		Some(-25),
		&format!(
			"Estimate fee for `single message confirmation` for runtime: {:?}",
			<Runtime as frame_system::Config>::Version::get()
		),
	)
}

#[test]
fn location_conversion_works() {
	// the purpose of hardcoded values is to catch an unintended location conversion logic change.
	struct TestCase {
		description: &'static str,
		location: Location,
		expected_account_id_str: &'static str,
	}

	let test_cases = vec![
		// DescribeTerminus
		TestCase {
			description: "DescribeTerminus Parent",
			location: Location::new(1, Here),
			expected_account_id_str: "5Dt6dpkWPwLaH4BBCKJwjiWrFVAGyYk3tLUabvyn4v7KtESG",
		},
		TestCase {
			description: "DescribeTerminus Sibling",
			location: Location::new(1, [Parachain(1111)]),
			expected_account_id_str: "5Eg2fnssmmJnF3z1iZ1NouAuzciDaaDQH7qURAy3w15jULDk",
		},
		// DescribePalletTerminal
		TestCase {
			description: "DescribePalletTerminal Parent",
			location: Location::new(1, [PalletInstance(50)]),
			expected_account_id_str: "5CnwemvaAXkWFVwibiCvf2EjqwiqBi29S5cLLydZLEaEw6jZ",
		},
		TestCase {
			description: "DescribePalletTerminal Sibling",
			location: Location::new(1, [Parachain(1111), PalletInstance(50)]),
			expected_account_id_str: "5GFBgPjpEQPdaxEnFirUoa51u5erVx84twYxJVuBRAT2UP2g",
		},
		// DescribeAccountId32Terminal
		TestCase {
			description: "DescribeAccountId32Terminal Parent",
			location: Location::new(
				1,
				[Junction::AccountId32 { network: None, id: AccountId::from(Alice).into() }],
			),
			expected_account_id_str: "5EueAXd4h8u75nSbFdDJbC29cmi4Uo1YJssqEL9idvindxFL",
		},
		TestCase {
			description: "DescribeAccountId32Terminal Sibling",
			location: Location::new(
				1,
				[
					Parachain(1111),
					Junction::AccountId32 { network: None, id: AccountId::from(Alice).into() },
				],
			),
			expected_account_id_str: "5Dmbuiq48fU4iW58FKYqoGbbfxFHjbAeGLMtjFg6NNCw3ssr",
		},
		// DescribeAccountKey20Terminal
		TestCase {
			description: "DescribeAccountKey20Terminal Parent",
			location: Location::new(1, [AccountKey20 { network: None, key: [0u8; 20] }]),
			expected_account_id_str: "5F5Ec11567pa919wJkX6VHtv2ZXS5W698YCW35EdEbrg14cg",
		},
		TestCase {
			description: "DescribeAccountKey20Terminal Sibling",
			location: Location::new(
				1,
				[Parachain(1111), AccountKey20 { network: None, key: [0u8; 20] }],
			),
			expected_account_id_str: "5CB2FbUds2qvcJNhDiTbRZwiS3trAy6ydFGMSVutmYijpPAg",
		},
		// DescribeTreasuryVoiceTerminal
		TestCase {
			description: "DescribeTreasuryVoiceTerminal Parent",
			location: Location::new(1, [Plurality { id: BodyId::Treasury, part: BodyPart::Voice }]),
			expected_account_id_str: "5CUjnE2vgcUCuhxPwFoQ5r7p1DkhujgvMNDHaF2bLqRp4D5F",
		},
		TestCase {
			description: "DescribeTreasuryVoiceTerminal Sibling",
			location: Location::new(
				1,
				[Parachain(1111), Plurality { id: BodyId::Treasury, part: BodyPart::Voice }],
			),
			expected_account_id_str: "5G6TDwaVgbWmhqRUKjBhRRnH4ry9L9cjRymUEmiRsLbSE4gB",
		},
		// DescribeBodyTerminal
		TestCase {
			description: "DescribeBodyTerminal Parent",
			location: Location::new(1, [Plurality { id: BodyId::Unit, part: BodyPart::Voice }]),
			expected_account_id_str: "5EBRMTBkDisEXsaN283SRbzx9Xf2PXwUxxFCJohSGo4jYe6B",
		},
		TestCase {
			description: "DescribeBodyTerminal Sibling",
			location: Location::new(
				1,
				[Parachain(1111), Plurality { id: BodyId::Unit, part: BodyPart::Voice }],
			),
			expected_account_id_str: "5DBoExvojy8tYnHgLL97phNH975CyT45PWTZEeGoBZfAyRMH",
		},
		// ExternalConsensusLocationsConverterFor
		TestCase {
			description: "Describe Ethereum Location",
			location: Location::new(2, [GlobalConsensus(Ethereum { chain_id: 11155111 })]),
			expected_account_id_str: "5GjRnmh5o3usSYzVmsxBWzHEpvJyHK4tKNPhjpUR3ASrruBy",
		},
		TestCase {
			description: "Describe Ethereum AccountKey",
			location: Location::new(
				2,
				[
					GlobalConsensus(Ethereum { chain_id: 11155111 }),
					AccountKey20 {
						network: None,
						key: hex!("87d1f7fdfEe7f651FaBc8bFCB6E086C278b77A7d"),
					},
				],
			),
			expected_account_id_str: "5HV4j4AsqT349oLRZmTjhGKDofPBWmWaPUfWGaRkuvzkjW9i",
		},
		TestCase {
			description: "Describe Rococo Location",
			location: Location::new(2, [GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH))]),
			expected_account_id_str: "5FfpYGrFybJXFsQk7dabr1vEbQ5ycBBu85vrDjPJsF3q4A8P",
		},
		TestCase {
			description: "Describe Rococo AccountID",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
					xcm::prelude::AccountId32 { network: None, id: AccountId::from(Alice).into() },
				],
			),
			expected_account_id_str: "5CYn32qPAc8FpQP55Br6AS2ZKhfCHD8Tt3v4CnCZo1rhDPd4",
		},
		TestCase {
			description: "Describe Rococo AccountKey",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
					AccountKey20 { network: None, key: [0u8; 20] },
				],
			),
			expected_account_id_str: "5GbRhbJWb2hZY7TCeNvTqZXaP3x3UY5xt4ccxpV1ZtJS1gFL",
		},
		TestCase {
			description: "Describe Rococo Treasury Plurality",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
					Plurality { id: BodyId::Treasury, part: BodyPart::Voice },
				],
			),
			expected_account_id_str: "5EGi9NgJNGoMawY8ubnCDLmbdEW6nt2W2U2G3j9E3jXmspT7",
		},
		TestCase {
			description: "Describe Rococo Parachain Location",
			location: Location::new(
				2,
				[GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)), Parachain(1000)],
			),
			expected_account_id_str: "5CQeLKM7XC1xNBiQLp26Wa948cudjYRD5VzvaTG3BjnmUvLL",
		},
		TestCase {
			description: "Describe Rococo Parachain AccountID",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
					Parachain(1000),
					xcm::prelude::AccountId32 { network: None, id: AccountId::from(Alice).into() },
				],
			),
			expected_account_id_str: "5CWnqmyXccGPg27BTxGmycvdEs5HvQq2FQY61xsS8H7uAvmW",
		},
		TestCase {
			description: "Describe Rococo Parachain AccountKey",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
					Parachain(1000),
					AccountKey20 { network: None, key: [0u8; 20] },
				],
			),
			expected_account_id_str: "5G121Rtddxn6zwMD2rZZGXxFHZ2xAgzFUgM9ki4A8wMGo4e2",
		},
		TestCase {
			description: "Describe Rococo Parachain Treasury Plurality",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
					Parachain(1000),
					Plurality { id: BodyId::Treasury, part: BodyPart::Voice },
				],
			),
			expected_account_id_str: "5FNk7za2pQ71NHnN1jA63hJxJwdQywiVGnK6RL3nYjCdkWDF",
		},
		TestCase {
			description: "Describe Rococo USDT Location",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
					Parachain(1000),
					PalletInstance(50),
					GeneralIndex(1984),
				],
			),
			expected_account_id_str: "5HNfT779KHeAL7PaVBTQDVxrT6dfJZJoQMTScxLSahBc9kxF",
		},
	];

	ExtBuilder::<Runtime>::default()
		.with_collators(collator_session_keys().collators())
		.with_session_keys(collator_session_keys().session_keys())
		.with_para_id(1000.into())
		.build()
		.execute_with(|| {
			for tc in test_cases {
				let expected = AccountId::from_string(tc.expected_account_id_str)
					.expect("Invalid AccountId string");

				let got =
					LocationToAccountHelper::<AccountId, LocationToAccountId>::convert_location(
						tc.location.into(),
					)
					.unwrap();

				assert_eq!(got, expected, "{}", tc.description);
			}
		});
}

#[test]
fn xcm_payment_api_works() {
	parachains_runtimes_test_utils::test_cases::xcm_payment_api_with_native_token_works::<
		Runtime,
		RuntimeCall,
		RuntimeOrigin,
		Block,
		WeightToFee,
	>();
}

#[test]
pub fn bridge_rewards_works() {
	run_test::<Runtime, _>(
		collator_session_keys(),
		bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		vec![],
		|| {
			// reward in WNDs
			let reward1: u128 = 2_000_000_000;
			// reward in WETH
			let reward2: u128 = 3_000_000_000;

			// prepare accounts
			let account1 = AccountId32::from(Alice);
			let account2 = AccountId32::from(Bob);
			let reward1_for = RewardsAccountParams::new(
				LegacyLaneId([1; 4]),
				*b"test",
				RewardsAccountOwner::ThisChain,
			);
			let expected_reward1_account =
				PayRewardFromAccount::<(), AccountId, LegacyLaneId, ()>::rewards_account(
					reward1_for,
				);
			assert_ok!(Balances::mint_into(&expected_reward1_account, ExistentialDeposit::get()));
			assert_ok!(Balances::mint_into(&expected_reward1_account, reward1.into()));
			assert_ok!(Balances::mint_into(&account1, ExistentialDeposit::get()));
			// To pay for delivery to AH when claiming the reward on BH
			assert_ok!(Balances::mint_into(&account2, ExistentialDeposit::get() * 10000));

			// register rewards
			use bp_relayers::RewardLedger;
			BridgeRelayers::register_reward(&account1, BridgeReward::from(reward1_for), reward1);
			BridgeRelayers::register_reward(&account2, BridgeReward::Snowbridge, reward2);

			// check stored rewards
			assert_eq!(
				BridgeRelayers::relayer_reward(&account1, BridgeReward::from(reward1_for)),
				Some(reward1)
			);
			assert_eq!(BridgeRelayers::relayer_reward(&account1, BridgeReward::Snowbridge), None,);
			assert_eq!(
				BridgeRelayers::relayer_reward(&account2, BridgeReward::Snowbridge),
				Some(reward2),
			);
			assert_eq!(
				BridgeRelayers::relayer_reward(&account2, BridgeReward::from(reward1_for)),
				None,
			);

			// claim rewards
			assert_ok!(BridgeRelayers::claim_rewards(
				RuntimeOrigin::signed(account1.clone()),
				reward1_for.into()
			));
			assert_eq!(Balances::total_balance(&account1), ExistentialDeposit::get() + reward1);
			assert_eq!(
				BridgeRelayers::relayer_reward(&account1, BridgeReward::from(reward1_for)),
				None,
			);

			// already claimed
			assert_err!(
				BridgeRelayers::claim_rewards(
					RuntimeOrigin::signed(account1.clone()),
					reward1_for.into()
				),
				pallet_bridge_relayers::Error::<Runtime, BridgeRelayersInstance>::NoRewardForRelayer
			);

			// Local account claiming is not supported for Snowbridge
			assert_err!(
				BridgeRelayers::claim_rewards(
					RuntimeOrigin::signed(account2.clone()),
					BridgeReward::Snowbridge
				),
				pallet_bridge_relayers::Error::<Runtime, BridgeRelayersInstance>::FailedToPayReward
			);

			let claim_location = VersionedLocation::V5(Location::new(
				1,
				[
					Parachain(1000),
					xcm::latest::Junction::AccountId32 {
						id: account2.clone().into(),
						network: None,
					},
				],
			));
			// In unit tests without proper HRMP channel setup, the claim will fail at XCM sending.
			assert_err!(
				BridgeRelayers::claim_rewards_to(
					RuntimeOrigin::signed(account2.clone()),
					BridgeReward::Snowbridge,
					BridgeRewardBeneficiaries::AssetHubLocation(claim_location)
				),
				pallet_bridge_relayers::Error::<Runtime, BridgeRelayersInstance>::FailedToPayReward
			);
		},
	);
}

#[test]
fn governance_authorize_upgrade_works() {
	use westend_runtime_constants::system_parachain::{ASSET_HUB_ID, COLLECTIVES_ID};

	// no - random para
	assert_err!(
		parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
			Runtime,
			RuntimeOrigin,
		>(GovernanceOrigin::Location(Location::new(1, Parachain(12334)))),
		Either::Right(InstructionError { index: 0, error: XcmError::Barrier })
	);
	// ok - AssetHub
	assert_ok!(parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
		Runtime,
		RuntimeOrigin,
	>(GovernanceOrigin::Location(Location::new(1, Parachain(ASSET_HUB_ID)))));
	// no - Collectives (passes barrier as system parachain, but not root)
	assert_err!(
		parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
			Runtime,
			RuntimeOrigin,
		>(GovernanceOrigin::Location(Location::new(1, Parachain(COLLECTIVES_ID)))),
		Either::Right(InstructionError { index: 1, error: XcmError::BadOrigin })
	);
	// no - Collectives Voice of Fellows plurality (bridge-hub has no FellowsPlurality
	// in its barrier, so the descended origin is rejected)
	assert_err!(
		parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
			Runtime,
			RuntimeOrigin,
		>(GovernanceOrigin::LocationAndDescendOrigin(
			Location::new(1, Parachain(COLLECTIVES_ID)),
			Plurality { id: BodyId::Technical, part: BodyPart::Voice }.into()
		)),
		Either::Right(InstructionError { index: 0, error: XcmError::Barrier })
	);

	// ok - relaychain
	assert_ok!(parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
		Runtime,
		RuntimeOrigin,
	>(GovernanceOrigin::Location(Location::parent())));

	// ok - governance location
	assert_ok!(parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
		Runtime,
		RuntimeOrigin,
	>(Governance::get()));
}

#[test]
fn tx_fees_go_to_accumulation_account() {
	let alice = AccountId::from(Alice);
	let accumulation_account =
		pallet_accumulate_and_forward::Pallet::<Runtime>::accumulation_account();
	let ed = ExistentialDeposit::get();

	run_test::<Runtime, _>(
		collator_session_keys(),
		bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		vec![(alice.clone(), 100 * ed), (accumulation_account.clone(), ed)],
		|| {
			let alice_before = <Balances as Inspect<AccountId>>::balance(&alice);
			let accumulation_before =
				<Balances as Inspect<AccountId>>::balance(&accumulation_account);
			let issuance_before = <Balances as Inspect<AccountId>>::total_issuance();

			let call = RuntimeCall::System(frame_system::Call::remark { remark: vec![] });
			let xt = construct_extrinsic(Alice, call);
			assert_ok!(Executive::apply_extrinsic(xt).unwrap());

			let alice_after = <Balances as Inspect<AccountId>>::balance(&alice);
			let fee_paid = alice_before - alice_after;
			assert!(fee_paid > 0, "a fee should have been paid");

			let accumulation_after =
				<Balances as Inspect<AccountId>>::balance(&accumulation_account);
			let issuance_after = <Balances as Inspect<AccountId>>::total_issuance();

			assert_eq!(accumulation_after, accumulation_before + fee_paid);
			assert_eq!(issuance_before, issuance_after);
		},
	);
}

#[test]
fn dust_removal_goes_to_accumulation_account() {
	let alice = AccountId::from(Alice);
	let bob = AccountId::from(Bob);
	let accumulation_account =
		pallet_accumulate_and_forward::Pallet::<Runtime>::accumulation_account();
	let ed = ExistentialDeposit::get();
	let dust = ed / 2;

	run_test::<Runtime, _>(
		collator_session_keys(),
		bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID,
		vec![
			(alice.clone(), 100 * ed),
			(bob.clone(), ed + dust),
			(accumulation_account.clone(), ed),
		],
		|| {
			let accumulation_before =
				<Balances as Inspect<AccountId>>::balance(&accumulation_account);

			assert_ok!(Balances::transfer_allow_death(
				RuntimeOrigin::signed(bob.clone()),
				alice.clone().into(),
				ed,
			));

			let accumulation_after =
				<Balances as Inspect<AccountId>>::balance(&accumulation_account);
			assert_eq!(accumulation_after, accumulation_before + dust);
			assert_eq!(<Balances as Inspect<AccountId>>::balance(&bob), 0);
		},
	);
}
