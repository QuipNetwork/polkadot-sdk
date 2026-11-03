// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
use super::*;

use crate::{mock::*, Error};
use codec::Encode;
use frame_support::{assert_noop, assert_ok};
use snowbridge_inbound_queue_primitives::{v2::Payload, EventProof, Proof};
use snowbridge_test_utils::{
	mock_rewards::{RegisteredRewardAmount, RegisteredRewardsCount},
	mock_xcm::{set_charge_fees_override, set_sender_override},
};
use sp_keyring::sr25519::Keyring;
use sp_runtime::DispatchError;
use xcm::prelude::*;

#[test]
fn test_submit_happy_path() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();

		let origin = RuntimeOrigin::signed(relayer.clone());

		// Submit message
		let event = EventProof {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};

		assert_ok!(InboundQueue::submit(origin.clone(), Box::new(event.clone())));

		let events = frame_system::Pallet::<Test>::events();
		assert!(
			events.iter().any(|event| matches!(
				event.event,
				RuntimeEvent::InboundQueue(Event::MessageReceived { nonce, ..})
					if nonce == 1
			)),
			"no message received event emitted."
		);

		assert_eq!(RegisteredRewardsCount::get(), 1, "Relayer reward should have been registered");
	});
}

#[test]
fn test_submit_with_invalid_gateway() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer);

		// Submit message
		let event = EventProof {
			event_log: mock_event_log_invalid_gateway(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};
		assert_noop!(
			InboundQueue::submit(origin.clone(), Box::new(event.clone())),
			Error::<Test>::InvalidGateway
		);
	});
}

#[test]
fn test_submit_verification_fails_with_invalid_proof() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();

		let origin = RuntimeOrigin::signed(relayer.clone());

		// Submit message
		let mut event = EventProof {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};
		// The mock verifier will error once it matches this address.
		event.event_log.address = ERROR_ADDRESS.into();

		assert_noop!(
			InboundQueue::submit(origin.clone(), Box::new(event.clone())),
			Error::<Test>::Verification(VerificationError::InvalidProof)
		);
	});
}

#[test]
fn test_submit_fails_with_malformed_message() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();

		let origin = RuntimeOrigin::signed(relayer.clone());

		// Submit message
		let event = EventProof {
			event_log: mock_event_log_invalid_message(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};

		assert_noop!(
			InboundQueue::submit(origin.clone(), Box::new(event.clone())),
			Error::<Test>::InvalidMessage
		);
	});
}

#[test]
fn test_using_same_nonce_fails() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();

		let origin = RuntimeOrigin::signed(relayer.clone());

		// Submit message
		let event = EventProof {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};

		assert_ok!(InboundQueue::submit(origin.clone(), Box::new(event.clone())));

		let events = frame_system::Pallet::<Test>::events();
		assert!(
			events.iter().any(|event| matches!(
				event.event,
				RuntimeEvent::InboundQueue(Event::MessageReceived { nonce, ..})
					if nonce == 1
			)),
			"no event emitted."
		);

		assert_noop!(
			InboundQueue::submit(origin.clone(), Box::new(event.clone())),
			Error::<Test>::InvalidNonce
		);
	});
}

#[test]
fn test_set_operating_mode() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer);
		let event = EventProof {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};

		assert_ok!(InboundQueue::set_operating_mode(
			RuntimeOrigin::root(),
			snowbridge_core::BasicOperatingMode::Halted
		));

		assert_noop!(InboundQueue::submit(origin, Box::new(event)), Error::<Test>::Halted);
	});
}

#[test]
fn test_set_operating_mode_root_only() {
	new_tester().execute_with(|| {
		assert_noop!(
			InboundQueue::set_operating_mode(
				RuntimeOrigin::signed(Keyring::Bob.into()),
				snowbridge_core::BasicOperatingMode::Halted
			),
			DispatchError::BadOrigin
		);
	});
}

#[test]
fn test_xcm_send_failure() {
	crate::test::new_tester().execute_with(|| {
		set_sender_override(
			|dest: &mut Option<Location>, xcm: &mut Option<Xcm<()>>| {
				if let Some(location) = dest {
					match location.unpack() {
						(_, [Parachain(1001)]) => return Err(SendError::NotApplicable),
						_ => Ok((xcm.clone().unwrap(), Assets::default())),
					}
				} else {
					Ok((xcm.clone().unwrap(), Assets::default()))
				}
			},
			|_| Err(SendError::DestinationUnsupported),
		);
		let relayer: AccountId = Keyring::Bob.into();

		let origin = mock::RuntimeOrigin::signed(relayer.clone());

		// Submit message
		let event = EventProof {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};

		assert_noop!(
			crate::test::InboundQueue::submit(origin.clone(), Box::new(event.clone())),
			Error::<Test>::SendFailure
		);
	});
}

#[test]
fn test_xcm_send_validate_failure() {
	crate::test::new_tester().execute_with(|| {
		set_sender_override(
			|_, _| return Err(SendError::NotApplicable),
			|xcm| {
				let hash = xcm.using_encoded(sp_io::hashing::blake2_256);
				Ok(hash)
			},
		);
		let relayer: AccountId = Keyring::Bob.into();

		let origin = mock::RuntimeOrigin::signed(relayer.clone());

		// Submit message
		let event = EventProof {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};

		assert_noop!(
			crate::test::InboundQueue::submit(origin.clone(), Box::new(event.clone())),
			Error::<Test>::Unreachable
		);
	});
}

#[test]
fn test_xcm_charge_fees_failure() {
	crate::test::new_tester().execute_with(|| {
		set_charge_fees_override(|_, _| Err(XcmError::FeesNotMet));

		let relayer: AccountId = Keyring::Bob.into();

		let origin = mock::RuntimeOrigin::signed(relayer.clone());

		// Submit message
		let event = EventProof {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};

		assert_noop!(
			crate::test::InboundQueue::submit(origin.clone(), Box::new(event.clone())),
			Error::<Test>::FeesNotMet
		);
	});
}

#[test]
fn test_register_token() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer);
		let event = EventProof {
			event_log: mock_event_log_v2(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};

		assert_ok!(InboundQueue::submit(origin, Box::new(event)));
	});
}

#[test]
fn test_switch_operating_mode() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer);
		let event = EventProof {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};

		assert_ok!(InboundQueue::set_operating_mode(
			RuntimeOrigin::root(),
			snowbridge_core::BasicOperatingMode::Halted
		));

		assert_noop!(
			InboundQueue::submit(origin.clone(), Box::new(event.clone())),
			Error::<Test>::Halted
		);

		assert_ok!(InboundQueue::set_operating_mode(
			RuntimeOrigin::root(),
			snowbridge_core::BasicOperatingMode::Normal
		));

		assert_ok!(InboundQueue::submit(origin, Box::new(event)));
	});
}

#[test]
fn zero_reward_does_not_register_reward() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = H160::random();
		assert_ok!(InboundQueue::process_message(
			relayer,
			Message {
				nonce: 0,
				assets: vec![],
				payload: Payload::Raw(vec![]),
				claimer: None,
				execution_fee: 1_000_000_000,
				relayer_fee: 0,
				gateway: GatewayAddress::get(),
				origin,
				value: 3_000_000_000,
			}
		));

		assert_eq!(
			RegisteredRewardsCount::get(),
			0,
			"Zero relayer reward should not be registered"
		);
	});
}

#[test]
fn test_add_tip_cumulative() {
	new_tester().execute_with(|| {
		let nonce: u64 = 10;
		let amount1: u128 = 500;
		let amount2: u128 = 300;

		assert_eq!(Tips::<Test>::get(nonce), None);
		assert_ok!(InboundQueue::add_tip(nonce, amount1));
		assert_eq!(Tips::<Test>::get(nonce), Some(amount1));
		assert_ok!(InboundQueue::add_tip(nonce, amount2));
		assert_eq!(Tips::<Test>::get(nonce), Some(amount1 + amount2));
	});
}

#[test]
fn test_add_tip_nonce_consumed() {
	new_tester().execute_with(|| {
		let nonce: u64 = 20;
		let amount: u128 = 400;
		Nonce::<Test>::set(nonce.into());

		assert_noop!(InboundQueue::add_tip(nonce, amount), AddTipError::NonceConsumed);
		assert_eq!(Tips::<Test>::get(nonce), None);
	});
}

#[test]
fn test_add_tip_amount_zero() {
	new_tester().execute_with(|| {
		let nonce: u64 = 30;
		let amount: u128 = 0;

		assert_noop!(InboundQueue::add_tip(nonce, amount), AddTipError::AmountZero);
		assert_eq!(Tips::<Test>::get(nonce), None);
	});
}

#[test]
fn inbound_tip_is_paid_out_to_relayer() {
	new_tester().execute_with(|| {
		let nonce: u64 = 77;
		let tip: u128 = 12_345;
		let relayer_fee: u128 = 2_000;

		// Add tip for nonce before message is processed
		assert_ok!(InboundQueue::add_tip(nonce, tip));
		assert_eq!(Tips::<Test>::get(nonce), Some(tip));

		// Process inbound message with relayer_fee
		let relayer: AccountId = Keyring::Bob.into();
		assert_ok!(InboundQueue::process_message(
			relayer,
			Message {
				nonce,
				assets: vec![],
				payload: Payload::Raw(vec![]),
				claimer: None,
				execution_fee: 1_000_000_000,
				relayer_fee,
				gateway: mock::GatewayAddress::get(),
				origin: H160::random(),
				value: 3_000_000_000,
			},
		));

		// Reward should be registered from relayer_fee + tip
		assert_eq!(
			RegisteredRewardsCount::get(),
			1,
			"Reward should be registered from relayer_fee + tip"
		);

		// Check the actual reward amount paid out (should be relayer_fee + tip)
		assert_eq!(
			RegisteredRewardAmount::get(),
			relayer_fee + tip,
			"Reward amount should equal relayer_fee + tip"
		);

		// Tip should be consumed from storage
		assert_eq!(Tips::<Test>::get(nonce), None);
	});
}

#[test]
fn relayer_fee_paid_out_when_no_tip_exists() {
	new_tester().execute_with(|| {
		let nonce: u64 = 88;
		let relayer_fee: u128 = 5_000;

		// Ensure no tip exists for this nonce
		assert_eq!(Tips::<Test>::get(nonce), None);

		// Process inbound message with relayer_fee but no tip
		let relayer: AccountId = Keyring::Bob.into();
		assert_ok!(InboundQueue::process_message(
			relayer,
			Message {
				nonce,
				assets: vec![],
				payload: Payload::Raw(vec![]),
				claimer: None,
				execution_fee: 1_000_000_000,
				relayer_fee,
				gateway: mock::GatewayAddress::get(),
				origin: H160::random(),
				value: 3_000_000_000,
			},
		));

		// Relayer fee should be paid out even without tip
		assert_eq!(
			RegisteredRewardsCount::get(),
			1,
			"Relayer fee should be paid out even when no tip exists"
		);

		// Check the actual reward amount paid out
		assert_eq!(
			RegisteredRewardAmount::get(),
			relayer_fee,
			"Reward amount should equal relayer_fee when no tip exists"
		);

		// Confirm no tip storage was affected
		assert_eq!(Tips::<Test>::get(nonce), None);
	});
}

#[test]
fn tip_paid_out_when_no_relayer_fee() {
	new_tester().execute_with(|| {
		let nonce: u64 = 99;
		let tip: u128 = 8_500;

		// Add tip for nonce before message is processed
		assert_ok!(InboundQueue::add_tip(nonce, tip));
		assert_eq!(Tips::<Test>::get(nonce), Some(tip));

		// Process inbound message with zero relayer_fee but with tip
		let relayer: AccountId = Keyring::Bob.into();
		assert_ok!(InboundQueue::process_message(
			relayer,
			Message {
				nonce,
				assets: vec![],
				payload: Payload::Raw(vec![]),
				claimer: None,
				execution_fee: 1_000_000_000,
				relayer_fee: 0,
				gateway: mock::GatewayAddress::get(),
				origin: H160::random(),
				value: 3_000_000_000,
			},
		));

		// Tip should be paid out even without relayer fee
		assert_eq!(
			RegisteredRewardsCount::get(),
			1,
			"Tip should be paid out even when relayer_fee is 0"
		);

		// Check the actual reward amount paid out (should be just the tip)
		assert_eq!(
			RegisteredRewardAmount::get(),
			tip,
			"Reward amount should equal tip when relayer_fee is 0"
		);

		// Tip should be consumed from storage
		assert_eq!(Tips::<Test>::get(nonce), None);
	});
}

#[test]
fn poc_permissionless_forged_receipt_bypasses_verifier_and_injects_xcm() {
	use crate::mock::exploit;
	use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope};
	use alloy_core::sol_types::{SolEvent, SolValue};
	use alloy_primitives::{Address, Bytes, Log as AlloyLog, B256};
	use frame_support::{assert_noop, assert_ok};
	use snowbridge_inbound_queue_primitives::{receipt::verify_receipt_proof, v2::IGatewayV2};
	use snowbridge_pallet_ethereum_client_fixtures::make_inbound_fixture;

	exploit::new_tester().execute_with(|| {
		let fixture = make_inbound_fixture();

		let attacker: AccountId = Keyring::Eve.into();
		let origin = exploit::RuntimeOrigin::signed(attacker.clone());

		// Initialize the Ethereum light client storage with a real finalized header.
		assert_ok!(exploit::EthereumBeaconClient::store_finalized_header(
			fixture.finalized_header,
			fixture.block_roots_root
		));

		// Unlimited mint ...
		let token_key: [u8; 20] = [0x42; 20]; // the "token address" that the XCM will try to mint to the attacker
		let token_value: u128 = 1_000_000_000_000_000_000u128;

		// Minting to the attacker itself
		let beneficiary: Location =
			Location::new(0, [AccountId32 { network: None, id: attacker.clone().into() }]);

		// Craft a valid raw XCM payload and ABI-encode a Gateway log carrying it
		let xcm: Xcm<()> = vec![DepositAsset {
			assets: Wild(AllCounted(1).into()),
			beneficiary: beneficiary.clone(),
		}]
		.into();
		let raw_xcm: Vec<u8> = VersionedXcm::V5(xcm).encode();

		let gateway_h160 = crate::mock::GatewayAddress::get();
		let sig = IGatewayV2::OutboundMessageAccepted::SIGNATURE_HASH;
		let topics = vec![sp_core::H256::from_slice(sig.as_slice())];
		let log_data = IGatewayV2::OutboundMessageAccepted {
			nonce: 1337u64,
			payload: IGatewayV2::Payload {
				origin: Address::from_slice(&[0x11; 20]),
				assets: vec![IGatewayV2::EthereumAsset {
					kind: 0,
					data: IGatewayV2::AsNativeTokenERC20 {
						token_id: Address::from_slice(&token_key),
						value: token_value,
					}
					.abi_encode()
					.into(),
				}],
				xcm: IGatewayV2::Xcm { kind: 0, data: raw_xcm.clone().into() },
				claimer: Bytes::new(),
				value: 0,
				executionFee: 1,
				relayerFee: 0,
			},
		}
		.encode_data();

		let forged_event_log = snowbridge_inbound_queue_primitives::Log {
			address: gateway_h160,
			topics,
			data: log_data.clone(),
			tx_index: 0,
		};

		// Forge an Ethereum receipt containing that exact log.
		let forged_receipt_log = AlloyLog::new_unchecked(
			Address::from_slice(forged_event_log.address.as_bytes()),
			vec![B256::from_slice(sig.as_slice())],
			Bytes::copy_from_slice(&log_data),
		);
		let forged_receipt = ReceiptEnvelope::Legacy(
			Receipt {
				status: Eip658Value::success(),
				cumulative_gas_used: 0,
				logs: vec![forged_receipt_log],
			}
			.with_bloom(),
		);
		let forged_receipt_bytes = alloy_rlp::encode(&forged_receipt);

		// Build a malicious receipt proof: a real receipts-trie root node + an extra "proof node"
		// that is just the forged receipt RLP bytes.
		let receipts_root = fixture.event.proof.execution_proof.execution_header.receipts_root();
		let root_node = fixture.event.proof.receipt_proof[0].clone();
		let exploit_proof_nodes = vec![root_node, forged_receipt_bytes.clone()];

		// With the path check fix, this forged proof must never verify for any tx index.
		for tx_index in 0u64..10_000u64 {
			assert!(
				verify_receipt_proof(receipts_root, tx_index, &exploit_proof_nodes).is_none(),
				"forged proof unexpectedly verified at tx_index={tx_index}"
			);
		}

		let mut forged_proof = fixture.event.proof;
		forged_proof.receipt_proof = exploit_proof_nodes;

		let forged_event = EventProof { event_log: forged_event_log, proof: forged_proof };
		assert_noop!(
			exploit::InboundQueue::submit(origin, Box::new(forged_event)),
			Error::<exploit::ExploitTest>::Verification(VerificationError::InvalidProof)
		);
	});
}
