// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
use super::*;

use frame_support::{assert_noop, assert_ok, storage::with_storage_layer};
use hex_literal::hex;
use snowbridge_core::ChannelId;
use snowbridge_inbound_queue_primitives::Proof;
use sp_keyring::Sr25519Keyring as Keyring;
use sp_runtime::{DispatchError, TokenError};
use sp_std::convert::From;

use crate::Error;

use crate::mock::*;

fn fee_for(event: &EventProof) -> u128 {
	let envelope = Envelope::try_from(&event.event_log).unwrap();
	let message = VersionedMessage::decode_all(&mut envelope.payload.as_ref()).unwrap();
	let (_, fee) = InboundQueue::do_convert(envelope.message_id, message).unwrap();
	fee
}

#[test]
fn test_submit_happy_path() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let channel_sovereign = sibling_sovereign_account::<Test>(ASSET_HUB_PARAID.into());

		let origin = RuntimeOrigin::signed(relayer.clone());

		// Submit event proof
		let event = EventProof {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};

		let initial_fund = InitialFund::get();
		let fee = fee_for(&event);
		let relayer_initial_fund = fee;
		Balances::mint_into(&relayer, relayer_initial_fund).unwrap();
		assert_eq!(Balances::balance(&relayer), relayer_initial_fund);
		assert_eq!(Balances::balance(&channel_sovereign), initial_fund);

		assert_ok!(InboundQueue::submit(origin.clone(), event.clone()));

		let pallet_events = frame_system::Pallet::<Test>::events();
		assert!(
			pallet_events.iter().any(|event| matches!(
				event.event,
				RuntimeEvent::InboundQueue(Event::MessageReceived { nonce, fee_burned, ..})
					if nonce == 1 && fee_burned == fee
			)),
			"no event emit."
		);

		let delivery_cost = InboundQueue::calculate_delivery_cost(event.encode().len() as u32);
		assert!(
			Parameters::get().rewards.local < delivery_cost,
			"delivery cost exceeds pure reward"
		);

		assert_eq!(
			Balances::balance(&relayer),
			relayer_initial_fund + delivery_cost,
			"relayer was reimbursed for the burned fee and rewarded"
		);
		assert_eq!(
			Balances::balance(&channel_sovereign),
			initial_fund - fee - delivery_cost,
			"sovereign account paid the full fee refund and reward"
		);
	});
}

#[test]
fn test_submit_xcm_invalid_channel() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer);

		// Deposit funds into sovereign account of parachain 1001
		let sovereign_account = sibling_sovereign_account::<Test>(TEMPLATE_PARAID.into());
		println!("account: {}", sovereign_account);
		let _ = Balances::mint_into(&sovereign_account, 10000);

		// Submit event proof
		let event = EventProof {
			event_log: mock_event_log_invalid_channel(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};
		assert_noop!(
			InboundQueue::submit(origin.clone(), event.clone()),
			Error::<Test>::InvalidChannel,
		);
	});
}

#[test]
fn test_submit_with_invalid_gateway() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer);

		// Deposit funds into sovereign account of Asset Hub (Statemint)
		let sovereign_account = sibling_sovereign_account::<Test>(ASSET_HUB_PARAID.into());
		let _ = Balances::mint_into(&sovereign_account, 10000);

		// Submit event proof
		let event = EventProof {
			event_log: mock_event_log_invalid_gateway(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};
		assert_noop!(
			InboundQueue::submit(origin.clone(), event.clone()),
			Error::<Test>::InvalidGateway
		);
	});
}

#[test]
fn test_submit_with_invalid_nonce() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer);

		// Deposit funds into sovereign account of Asset Hub (Statemint)
		let sovereign_account = sibling_sovereign_account::<Test>(ASSET_HUB_PARAID.into());
		let _ = Balances::mint_into(&sovereign_account, 10000);

		// Submit message
		let event = EventProof {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};
		let fee = fee_for(&event);
		Balances::mint_into(&Keyring::Bob.into(), fee).unwrap();
		assert_ok!(InboundQueue::submit(origin.clone(), event.clone()));

		let nonce: u64 = <Nonce<Test>>::get(ChannelId::from(hex!(
			"c173fac324158e77fb5840738a1a541f633cbec8884c6a601c567d2b376a0539"
		)));
		assert_eq!(nonce, 1);

		// Submit the same again
		assert_noop!(
			InboundQueue::submit(origin.clone(), event.clone()),
			Error::<Test>::InvalidNonce
		);
	});
}

#[test]
fn test_submit_no_funds_to_reward_relayers_just_ignore() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer.clone());

		// Reset balance of sovereign_account to zero first
		let sovereign_account = sibling_sovereign_account::<Test>(ASSET_HUB_PARAID.into());
		Balances::set_balance(&sovereign_account, 0);

		// Submit message
		let event = EventProof {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};
		let fee = fee_for(&event);
		Balances::mint_into(&relayer, fee).unwrap();

		// Check submit successfully in case no funds available in the sovereign account.
		assert_ok!(InboundQueue::submit(origin.clone(), event.clone()));
		assert_eq!(Balances::balance(&relayer), 0, "relayer was not reimbursed");
		assert_eq!(Balances::balance(&sovereign_account), 0);
		assert_eq!(
			<Nonce<Test>>::get(ChannelId::from(hex!(
				"c173fac324158e77fb5840738a1a541f633cbec8884c6a601c567d2b376a0539"
			))),
			1,
			"message nonce advanced"
		);
		assert!(
			frame_system::Pallet::<Test>::events().iter().any(|event| matches!(
				event.event,
				RuntimeEvent::InboundQueue(Event::MessageReceived { nonce, fee_burned, .. })
					if nonce == 1 && fee_burned == fee
			)),
			"message was not received"
		);
	});
}

// If the relayer cannot front the teleported fee, the whole extrinsic must revert, including the
// nonce increment, so that a funded relayer can resubmit the same message and the channel is not
// stalled.
#[test]
fn test_submit_fails_and_nonce_reverts_when_relayer_cannot_front_fee() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer.clone());
		let channel_id = ChannelId::from(hex!(
			"c173fac324158e77fb5840738a1a541f633cbec8884c6a601c567d2b376a0539"
		));

		// Fund the sovereign so only the relayer's shortfall can cause a failure.
		let sovereign_account = sibling_sovereign_account::<Test>(ASSET_HUB_PARAID.into());
		Balances::set_balance(&sovereign_account, InitialFund::get());

		let event = EventProof {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};

		// Relayer is deliberately left unfunded, so it cannot front the teleported fee.
		assert_eq!(Balances::balance(&relayer), 0);

		// Dispatch inside a storage layer to mimic the on-chain transactional revert.
		assert_noop!(
			with_storage_layer(|| InboundQueue::submit(origin.clone(), event.clone())),
			TokenError::FundsUnavailable
		);

		assert_eq!(<Nonce<Test>>::get(channel_id), 0, "nonce must not advance on failure");
	});
}

// A reimbursement that would leave the relayer below the existential deposit fails, but that must
// never revert message processing (best-effort). The relayer simply goes unrewarded.
#[test]
fn test_submit_ignores_reimbursement_below_existential_deposit() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer.clone());
		let channel_id = ChannelId::from(hex!(
			"c173fac324158e77fb5840738a1a541f633cbec8884c6a601c567d2b376a0539"
		));

		// Sovereign holds only dust above its ED, so its reducible balance is below the ED. The
		// relayer is reaped to zero by fronting the fee, so reimbursing this dust would try to
		// create the relayer account below the ED and fail.
		let sovereign_account = sibling_sovereign_account::<Test>(ASSET_HUB_PARAID.into());
		let dust = ExistentialDeposit::get() / 2;
		let sovereign_balance = ExistentialDeposit::get() + dust;
		Balances::set_balance(&sovereign_account, sovereign_balance);

		let event = EventProof {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};
		let fee = fee_for(&event);
		Balances::mint_into(&relayer, fee).unwrap();

		// Message still processes even though the reimbursement transfer fails.
		assert_ok!(InboundQueue::submit(origin.clone(), event.clone()));

		assert_eq!(Balances::balance(&relayer), 0, "relayer was not reimbursed");
		assert_eq!(
			Balances::balance(&sovereign_account),
			sovereign_balance,
			"failed reimbursement left the sovereign untouched"
		);
		assert_eq!(<Nonce<Test>>::get(channel_id), 1, "message nonce advanced");
		assert!(
			frame_system::Pallet::<Test>::events().iter().any(|event| matches!(
				event.event,
				RuntimeEvent::InboundQueue(Event::MessageReceived { nonce, fee_burned, .. })
					if nonce == 1 && fee_burned == fee
			)),
			"message was not received"
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

		assert_noop!(InboundQueue::submit(origin, event), Error::<Test>::Halted);
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
fn test_submit_no_funds_to_reward_relayers_and_ed_preserved() {
	new_tester().execute_with(|| {
		let relayer: AccountId = Keyring::Bob.into();
		let origin = RuntimeOrigin::signed(relayer.clone());

		// Reset balance of sovereign account to (ED+1) first
		let sovereign_account = sibling_sovereign_account::<Test>(ASSET_HUB_PARAID.into());
		Balances::set_balance(&sovereign_account, ExistentialDeposit::get() + 1);

		// Submit message successfully
		let event = EventProof {
			event_log: mock_event_log(),
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};
		let fee = fee_for(&event);
		Balances::mint_into(&relayer, fee * 2).unwrap();
		assert_ok!(InboundQueue::submit(origin.clone(), event.clone()));

		// Check balance of sovereign account to ED
		let amount = Balances::balance(&sovereign_account);
		assert_eq!(amount, ExistentialDeposit::get());

		// Submit another message with nonce set as 2
		let mut event_log = mock_event_log();
		event_log.data[31] = 2;
		let event = EventProof {
			event_log,
			proof: Proof {
				receipt_proof: Default::default(),
				execution_proof: mock_execution_proof(),
			},
		};
		assert_ok!(InboundQueue::submit(origin.clone(), event.clone()));
		// Check balance of sovereign account as ED does not change
		let amount = Balances::balance(&sovereign_account);
		assert_eq!(amount, ExistentialDeposit::get());
	});
}
