// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
use crate::{
	mock::{AggregateMessageOrigin::*, *},
	*,
};
use alloy_core::primitives::FixedBytes;
use codec::{Decode, Encode};
use frame_support::{
	assert_err, assert_noop, assert_ok,
	traits::{Hooks, ProcessMessage, ProcessMessageError, QueueFootprintQuery},
	weights::WeightMeter,
	BoundedVec,
};
use hex_literal::hex;
use snowbridge_beacon_primitives::{types::deneb, VersionedExecutionPayloadHeader};
use snowbridge_core::{digest_item::SnowbridgeDigestItem, ChannelId, ParaId};
use snowbridge_outbound_queue_primitives::{
	v2::{abi::OutboundMessageWrapper, Command, Initializer, SendMessage},
	EventProof, Proof, SendError, VerificationError,
};
use sp_core::{hexdisplay::HexDisplay, H256};
use sp_runtime::AccountId32;

#[test]
fn submit_messages_and_commit() {
	new_tester().execute_with(|| {
		for para_id in 1000..1004 {
			let message = mock_message(para_id);
			let ticket = OutboundQueue::validate(&message).unwrap();
			assert_ok!(OutboundQueue::deliver(ticket));
		}

		ServiceWeight::set(Some(Weight::MAX));
		run_to_end_of_next_block();

		assert_eq!(Nonce::<Test>::get(), 4);

		let digest = System::digest();
		let digest_items = digest.logs();
		assert!(digest_items.len() == 1 && digest_items[0].as_other().is_some());
		assert_eq!(Messages::<Test>::decode_len(), Some(4));
	});
}

#[test]
fn prove_message_recomputes_committed_leaves_after_commit() {
	new_tester().execute_with(|| {
		for para_id in 1000..1004 {
			let message = mock_message(para_id);
			let ticket = OutboundQueue::validate(&message).unwrap();
			assert_ok!(OutboundQueue::deliver(ticket));
		}

		ServiceWeight::set(Some(Weight::MAX));
		run_to_end_of_next_block();

		// The messages stay available for relayers, but the transient leaves have been dropped from
		// state by `commit`.
		let messages = Messages::<Test>::get();
		assert_eq!(messages.len(), 4);
		assert_eq!(MessageLeaves::<Test>::decode_len().unwrap_or_default(), 0);

		// Recover the merkle root that was committed on-chain into the header digest.
		let digest = System::digest();
		let committed_root = digest
			.logs()
			.iter()
			.find_map(|item| item.as_other())
			.and_then(|bytes| SnowbridgeDigestItem::decode(&mut &bytes[..]).ok())
			.map(|item| match item {
				SnowbridgeDigestItem::SnowbridgeV2(root) => root,
				other => panic!("unexpected digest item: {:?}", other),
			})
			.expect("commitment digest item should be present");

		// `prove_message` recomputes the leaves from `Messages` (since `MessageLeaves` is gone) and
		// must produce proofs that verify against the very same root committed on-chain.
		for (index, message) in messages.iter().enumerate() {
			let proof = crate::api::prove_message::<Test>(index as u64)
				.expect("a proof should be generated for a committed message");
			assert_eq!(proof.root, committed_root);
			assert_eq!(proof.leaf, OutboundQueue::message_leaf(message));
			assert_eq!(proof.leaf_index, index as u64);
			assert_eq!(proof.number_of_leaves, messages.len() as u64);
		}

		// An out-of-range `leaf_index` returns `None` instead of panicking in `merkle_proof`.
		assert!(crate::api::prove_message::<Test>(messages.len() as u64).is_none());
	});
}

#[test]
fn submit_message_fail_too_large() {
	new_tester().execute_with(|| {
		let message = mock_invalid_governance_message::<Test>();
		assert_err!(OutboundQueue::validate(&message), SendError::MessageTooLarge);
	});
}

#[test]
fn commit_exits_early_if_no_processed_messages() {
	new_tester().execute_with(|| {
		// on_finalize should do nothing, nor should it panic
		OutboundQueue::on_finalize(System::block_number());

		let digest = System::digest();
		let digest_items = digest.logs();
		assert_eq!(digest_items.len(), 0);
	});
}

#[test]
fn process_message_yields_on_max_messages_per_block() {
	new_tester().execute_with(|| {
		for _ in 0..<Test as Config>::MaxMessagesPerBlock::get() {
			MessageLeaves::<Test>::append(H256::zero())
		}

		let _channel_id: ChannelId = ParaId::from(1000).into();
		let origin = AggregateMessageOrigin::SnowbridgeV2(H256::zero());
		let message = Message {
			origin: Default::default(),
			id: Default::default(),
			fee: 0,
			commands: BoundedVec::try_from(vec![Command::Upgrade {
				impl_address: Default::default(),
				impl_code_hash: Default::default(),
				initializer: Initializer {
					params: (0..512).map(|_| 1u8).collect::<Vec<u8>>(),
					maximum_required_gas: 0,
				},
			}])
			.unwrap(),
		};

		let mut meter = WeightMeter::new();

		assert_err!(
			OutboundQueue::process_message(
				message.encode().as_slice(),
				origin,
				&mut meter,
				&mut [0u8; 32]
			),
			ProcessMessageError::Yield
		);
		let events = System::events();
		let last_event = events.last().expect("Expected at least one event").event.clone();

		match last_event {
			mock::RuntimeEvent::OutboundQueue(Event::MessagePostponed {
				payload: _,
				reason: ProcessMessageError::Yield,
			}) => {},
			_ => {
				panic!("Expected Event::MessagePostponed(Yield) but got {:?}", last_event);
			},
		}
	})
}

#[test]
fn process_message_fails_on_max_nonce_reached() {
	new_tester().execute_with(|| {
		let sibling_id = 1000;
		let _channel_id: ChannelId = ParaId::from(sibling_id).into();
		let origin = AggregateMessageOrigin::SnowbridgeV2(H256::zero());
		let message: Message = mock_message(sibling_id);

		let mut meter = WeightMeter::with_limit(Weight::MAX);

		Nonce::<Test>::set(u64::MAX);

		let result = OutboundQueue::process_message(
			message.encode().as_slice(),
			origin,
			&mut meter,
			&mut [0u8; 32],
		);
		assert_err!(result, ProcessMessageError::Unsupported);

		let events = System::events();
		let last_event = events.last().expect("Expected at least one event").event.clone();

		match last_event {
			mock::RuntimeEvent::OutboundQueue(Event::MessageRejected {
				id: None,
				payload: _,
				error: ProcessMessageError::Unsupported,
			}) => {},
			_ => {
				panic!("Expected Event::MessageRejected(Unsupported) but got {:?}", last_event);
			},
		}
	})
}

#[test]
fn process_message_fails_on_overweight_message() {
	new_tester().execute_with(|| {
		let sibling_id = 1000;
		let _channel_id: ChannelId = ParaId::from(sibling_id).into();
		let origin = AggregateMessageOrigin::SnowbridgeV2(H256::zero());
		let message: Message = mock_message(sibling_id);
		let mut meter = WeightMeter::with_limit(Weight::from_parts(1, 1));
		assert_err!(
			OutboundQueue::process_message(
				message.encode().as_slice(),
				origin,
				&mut meter,
				&mut [0u8; 32]
			),
			ProcessMessageError::Overweight(<Test as Config>::WeightInfo::do_process_message())
		);
		let events = System::events();
		let last_event = events.last().expect("Expected at least one event").event.clone();

		match last_event {
			mock::RuntimeEvent::OutboundQueue(Event::MessagePostponed {
				payload: _,
				reason: ProcessMessageError::Overweight(_),
			}) => {},
			_ => {
				panic!("Expected Event::MessagePostponed(Overweight(_)) but got {:?}", last_event);
			},
		}
	})
}

#[test]
fn governance_message_not_processed_in_same_block_when_queue_congested_with_low_priority_messages()
{
	let sibling_id: u32 = 1000;

	new_tester().execute_with(|| {
		// submit a lot of low priority messages from asset_hub which will need multiple blocks to
		// execute(20 messages for each block so 40 required at least 2 blocks)
		let max_messages = 40;
		for _ in 0..max_messages {
			// submit low priority message
			let message = mock_message(sibling_id);
			let ticket = OutboundQueue::validate(&message).unwrap();
			OutboundQueue::deliver(ticket).unwrap();
		}

		let footprint =
			MessageQueue::footprint(SnowbridgeV2(H256::from_low_u64_be(sibling_id as u64)));
		assert_eq!(footprint.storage.count, (max_messages) as u64);

		let message = mock_governance_message::<Test>();
		let ticket = OutboundQueue::validate(&message).unwrap();
		OutboundQueue::deliver(ticket).unwrap();

		// move to next block
		ServiceWeight::set(Some(Weight::MAX));
		run_to_end_of_next_block();

		// first process 20 messages from sibling channel
		let footprint =
			MessageQueue::footprint(SnowbridgeV2(H256::from_low_u64_be(sibling_id as u64)));
		assert_eq!(footprint.storage.count, 40 - 20);

		// and governance message does not have the chance to execute in same block
		let footprint = MessageQueue::footprint(SnowbridgeV2(bridge_hub_root_origin()));
		assert_eq!(footprint.storage.count, 1);

		// move to next block
		ServiceWeight::set(Some(Weight::MAX));
		run_to_end_of_next_block();

		// now governance message get executed in this block
		let footprint = MessageQueue::footprint(SnowbridgeV2(bridge_hub_root_origin()));
		assert_eq!(footprint.storage.count, 0);

		// and this time process 19 messages from sibling channel so we have 1 message left
		let footprint =
			MessageQueue::footprint(SnowbridgeV2(H256::from_low_u64_be(sibling_id as u64)));
		assert_eq!(footprint.storage.count, 1);

		// move to the next block, the last 1 message from sibling channel get executed
		ServiceWeight::set(Some(Weight::MAX));
		run_to_end_of_next_block();
		let footprint =
			MessageQueue::footprint(SnowbridgeV2(H256::from_low_u64_be(sibling_id as u64)));
		assert_eq!(footprint.storage.count, 0);
	});
}

#[test]
fn encode_digest_item_with_correct_index() {
	new_tester().execute_with(|| {
		let digest_item: DigestItem = SnowbridgeDigestItem::Snowbridge(H256::default()).into();
		let enum_prefix = match digest_item {
			DigestItem::Other(data) => data[0],
			_ => u8::MAX,
		};
		assert_eq!(enum_prefix, 0);
	});
}

#[test]
fn encode_digest_item() {
	new_tester().execute_with(|| {
		let digest_item: DigestItem = SnowbridgeDigestItem::Snowbridge([5u8; 32].into()).into();
		let digest_item_raw = digest_item.encode();
		assert_eq!(digest_item_raw[0], 0); // DigestItem::Other
		assert_eq!(digest_item_raw[2], 0); // SnowbridgeDigestItem::Snowbridge
		assert_eq!(
			digest_item_raw,
			[
				0, 132, 0, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
				5, 5, 5, 5, 5, 5, 5, 5
			]
		);
	});
}

fn encode_mock_message(message: Message) -> Vec<u8> {
	let commands: Vec<CommandWrapper> = message
		.commands
		.into_iter()
		.map(|command| CommandWrapper {
			kind: command.index(),
			gas: <Test as Config>::GasMeter::maximum_dispatch_gas_used_at_most(&command),
			payload: Bytes::from(command.abi_encode()),
		})
		.collect();

	// print the abi-encoded message and decode with solidity test
	let committed_message = OutboundMessageWrapper {
		origin: FixedBytes::from(message.origin.as_fixed_bytes()),
		nonce: 1,
		topic: FixedBytes::from(message.id.as_fixed_bytes()),
		commands,
	};
	let message_abi_encoded = committed_message.abi_encode();
	message_abi_encoded
}

#[test]
fn encode_unlock_message() {
	let message: Message = mock_message(1000);
	let message_abi_encoded = encode_mock_message(message);
	println!("{}", HexDisplay::from(&message_abi_encoded));
	assert_eq!(hex!("000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000003e80000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000030d4000000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000060000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b1185ede04202fe62d38f5db72f71e38ff3e830500000000000000000000000000000000000000000000000000000000000f4240").to_vec(), message_abi_encoded)
}

#[test]
fn encode_register_pna() {
	let message: Message = mock_register_token_message(1000);
	let message_abi_encoded = encode_mock_message(message);
	println!("{}", HexDisplay::from(&message_abi_encoded));
	assert_eq!(hex!("000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000003e80000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000030000000000000000000000000000000000000000000000000000000000124f80000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000e000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000").to_vec(), message_abi_encoded)
}

#[test]
fn test_add_tip_cumulative() {
	new_tester().execute_with(|| {
		let nonce = 1;
		let initial_fee = 1000;
		let additional_fee = 500;
		let current_block = System::block_number();
		let order = PendingOrder { nonce, fee: initial_fee, block_number: current_block };
		PendingOrders::<Test>::insert(nonce, order);
		assert_ok!(OutboundQueue::add_tip(nonce, additional_fee));
		let order_after = PendingOrders::<Test>::get(nonce).unwrap();
		assert_eq!(order_after.fee, initial_fee + additional_fee);
	});
}

#[test]
fn test_add_tip_fails_no_pending_order() {
	new_tester().execute_with(|| {
		let nonce = 42;
		let amount = 1000;
		assert_noop!(OutboundQueue::add_tip(nonce, amount), AddTipError::UnknownMessage);
	});
}

fn mock_event_proof() -> EventProof {
	EventProof {
		event_log: snowbridge_outbound_queue_primitives::Log {
			address: Default::default(),
			topics: vec![],
			data: vec![],
			tx_index: 0,
		},
		proof: Proof {
			receipt_proof: Default::default(),
			execution_proof: snowbridge_beacon_primitives::ExecutionProof {
				header: Default::default(),
				ancestry_proof: None,
				execution_header: VersionedExecutionPayloadHeader::Deneb(
					deneb::ExecutionPayloadHeader {
						parent_hash: Default::default(),
						fee_recipient: Default::default(),
						state_root: Default::default(),
						receipts_root: Default::default(),
						logs_bloom: vec![],
						prev_randao: Default::default(),
						block_number: 0,
						gas_limit: 0,
						gas_used: 0,
						timestamp: 0,
						extra_data: vec![],
						base_fee_per_gas: Default::default(),
						block_hash: Default::default(),
						transactions_root: Default::default(),
						withdrawals_root: Default::default(),
						blob_gas_used: 0,
						excess_blob_gas: 0,
					},
				),
				execution_branch: vec![],
			},
		},
	}
}

// A valid, decodable `InboundMessageDispatched` event log emitted by the mock Gateway.
// Nonce (indexed topic) is 0, matching a `PendingOrder` inserted with nonce=0 in tests.
fn mock_valid_event_proof() -> EventProof {
	let mut event = mock_event_proof();
	event.event_log = snowbridge_outbound_queue_primitives::Log {
		address: hex!("b1185ede04202fe62d38f5db72f71e38ff3e8305").into(),
		topics: vec![
			hex!("8856ab63954e6c2938803a4654fb704c8779757e7bfdbe94a578e341ec637a95").into(),
			hex!("0000000000000000000000000000000000000000000000000000000000000000").into(),
		],
		data: hex!("907b6ec7bf3f2496ef79238e0fb19e032bfe444c7ffe906bd340c6c4ffe8511f0000000000000000000000000000000000000000000000000000000000000001d43593c715fdd31c61141abd04a99fd6822c8558854ccde39a5684e7a56da27d").into(),
		tx_index: 0,
	};
	event
}

// Reward processing must be blocked while the bridge is halted: `submit_delivery_receipt`
// should not pay out `PendingOrder` fees if the verifier reports the bridge as halted.
#[test]
fn poc_m1() {
	new_tester().execute_with(|| {
		let nonce = 1;
		let fee: u128 = 1_000_000;
		let order = PendingOrder { nonce, fee, block_number: System::block_number() };
		PendingOrders::<Test>::insert(nonce, order);

		let relayer: AccountId32 = [7u8; 32].into();
		let origin = RuntimeOrigin::signed(relayer);
		let event = Box::new(mock_event_proof());

		set_verifier_halted(true);

		assert_noop!(
			OutboundQueue::submit_delivery_receipt(origin.clone(), event.clone()),
			Error::<Test>::Verification(VerificationError::Halted)
		);

		let order_after = PendingOrders::<Test>::get(nonce).expect("order still present");
		assert_eq!(order_after.fee, fee);

		set_verifier_halted(false);
	});
}

// After governance resumes the bridge, legitimate delivery receipts flow through again:
// the order is paid out and removed from storage.
#[test]
fn submit_delivery_receipt_succeeds_after_unhalt() {
	new_tester().execute_with(|| {
		let nonce = 0;
		let fee: u128 = 1_000_000;
		let order = PendingOrder { nonce, fee, block_number: System::block_number() };
		PendingOrders::<Test>::insert(nonce, order);

		let relayer: AccountId32 = [7u8; 32].into();
		let origin = RuntimeOrigin::signed(relayer);
		let event = Box::new(mock_valid_event_proof());

		// Bridge halted — receipt rejected, order untouched.
		set_verifier_halted(true);
		assert_noop!(
			OutboundQueue::submit_delivery_receipt(origin.clone(), event.clone()),
			Error::<Test>::Verification(VerificationError::Halted)
		);
		assert!(PendingOrders::<Test>::get(nonce).is_some());

		// Bridge resumed — same receipt succeeds and the order is settled.
		set_verifier_halted(false);
		assert_ok!(OutboundQueue::submit_delivery_receipt(origin, event));
		assert!(PendingOrders::<Test>::get(nonce).is_none());

		System::assert_has_event(mock::RuntimeEvent::OutboundQueue(Event::MessageDelivered {
			nonce,
		}));
	});
}

#[test]
fn test_add_tip_fails_amount_zero() {
	new_tester().execute_with(|| {
		let nonce = 1;
		let initial_fee = 1000;
		let zero_amount = 0;
		let current_block = System::block_number();
		let order = PendingOrder { nonce, fee: initial_fee, block_number: current_block };
		PendingOrders::<Test>::insert(nonce, order);

		assert_noop!(OutboundQueue::add_tip(nonce, zero_amount), AddTipError::AmountZero);

		// Verify the original fee is unchanged
		let order_after = PendingOrders::<Test>::get(nonce).unwrap();
		assert_eq!(order_after.fee, initial_fee);
	});
}
