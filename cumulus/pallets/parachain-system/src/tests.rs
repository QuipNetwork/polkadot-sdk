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

use super::*;
use crate::mock::*;

use alloc::collections::BTreeMap;
use core::num::NonZeroU32;
use cumulus_primitives_core::{
	relay_chain::ApprovedPeerId, AbridgedHrmpChannel, ClaimQueueOffset, CoreInfo, CoreSelector,
	InboundDownwardMessage, InboundHrmpMessage, CUMULUS_CONSENSUS_ID,
};
use cumulus_primitives_parachain_inherent::{
	v0, INHERENT_IDENTIFIER, PARACHAIN_INHERENT_IDENTIFIER_V0,
};
use frame_support::{assert_ok, parameter_types, weights::Weight};
use frame_system::RawOrigin;
use hex_literal::hex;
use rand::Rng;
use relay_chain::HrmpChannelId;
use sp_core::H256;
use sp_inherents::InherentDataProvider;
use sp_runtime::DigestItem;
use sp_trie::StorageProof;

#[test]
#[should_panic]
fn block_tests_run_on_drop() {
	BlockTests::new().add(123, || panic!("if this test passes, block tests run properly"));
}

/// Test that ensures that the parachain-system pallet accepts both the legacy
/// and versioned inherent format.
#[test]
fn test_inherent_compatibility() {
	sp_tracing::init_for_tests();
	let mut valid_inherent_data_v1 = sp_inherents::InherentData::new();
	valid_inherent_data_v1
		.put_data(
			INHERENT_IDENTIFIER,
			&ParachainInherentData {
				validation_data: Default::default(),
				relay_chain_state: StorageProof::empty(),
				downward_messages: Default::default(),
				horizontal_messages: Default::default(),
				relay_parent_descendants: Default::default(),
				collator_peer_id: None,
			},
		)
		.expect("Put validation function params failed");

	let mut valid_inherent_data_legacy = sp_inherents::InherentData::new();
	valid_inherent_data_legacy
		.put_data(
			PARACHAIN_INHERENT_IDENTIFIER_V0,
			&v0::ParachainInherentData {
				validation_data: Default::default(),
				relay_chain_state: StorageProof::empty(),
				downward_messages: Default::default(),
				horizontal_messages: Default::default(),
			},
		)
		.expect("Put validation function params failed");

	let mut valid_inherent_data_full_compatibility = sp_inherents::InherentData::new();
	let data = ParachainInherentData {
		validation_data: Default::default(),
		relay_chain_state: StorageProof::empty(),
		downward_messages: Default::default(),
		horizontal_messages: Default::default(),
		relay_parent_descendants: Default::default(),
		collator_peer_id: None,
	};
	let _ = futures::executor::block_on(
		data.provide_inherent_data(&mut valid_inherent_data_full_compatibility),
	);

	wasm_ext().execute_with(|| {
		assert!(
			ParachainSystem::create_inherent(&valid_inherent_data_v1).is_some(),
			"V1 inherent was not accepted"
		);
		assert!(
			ParachainSystem::create_inherent(&valid_inherent_data_legacy).is_some(),
			"Legacy inherent was not accepted"
		);

		assert!(
			ParachainSystem::create_inherent(&valid_inherent_data_full_compatibility).is_some(),
			"Inherent on multiple keys was not accepted."
		);
	})
}

#[test]
fn test_xcmp_source_keeps_messages() {
	let recipient = ParaId::from(400);

	CONSENSUS_HOOK.with(|c| {
		*c.borrow_mut() = Box::new(|_| (Weight::zero(), NonZeroU32::new(3).unwrap().into()))
	});

	BlockTests::new()
		.with_inclusion_delay(2)
		.with_relay_sproof_builder(move |_, block_number, sproof| {
			sproof.host_config.hrmp_max_message_num_per_candidate = 10;
			let channel = sproof.upsert_outbound_channel(recipient);
			channel.max_total_size = 10;
			channel.max_message_size = 10;

			// Only fit messages starting from 3rd block.
			channel.max_capacity = if block_number < 3 { 0 } else { 1 };
		})
		.add(1, || {})
		.add_with_post_test(
			2,
			move || {
				send_message(recipient, b"22".to_vec());
			},
			move || {
				let v = HrmpOutboundMessages::<Test>::get();
				assert!(v.is_empty());
			},
		)
		.add_with_post_test(
			3,
			move || {},
			move || {
				// Not discarded.
				let v = HrmpOutboundMessages::<Test>::get();
				assert_eq!(v, vec![OutboundHrmpMessage { recipient, data: b"22".to_vec() }]);
			},
		);
}

#[test]
fn unincluded_segment_works() {
	CONSENSUS_HOOK.with(|c| {
		*c.borrow_mut() = Box::new(|_| (Weight::zero(), NonZeroU32::new(10).unwrap().into()))
	});

	BlockTests::new()
		.with_inclusion_delay(1)
		.add_with_post_test(
			1,
			|| {},
			|| {
				let segment = <UnincludedSegment<Test>>::get();
				assert_eq!(segment.len(), 1);
				assert!(<AggregatedUnincludedSegment<Test>>::get().is_some());
			},
		)
		.add_with_post_test(
			2,
			|| {},
			|| {
				let segment = <UnincludedSegment<Test>>::get();
				assert_eq!(segment.len(), 2);
			},
		)
		.add_with_post_test(
			3,
			|| {},
			|| {
				let segment = <UnincludedSegment<Test>>::get();
				// Block 1 was popped from the segment, the len is still 2.
				assert_eq!(segment.len(), 2);
			},
		);
}

#[test]
#[should_panic = "No space left for the block in the unincluded segment: new_len(1) < capacity(1)"]
fn unincluded_segment_is_limited() {
	CONSENSUS_HOOK.with(|c| {
		*c.borrow_mut() = Box::new(|_| (Weight::zero(), NonZeroU32::new(1).unwrap().into()))
	});

	BlockTests::new()
		.with_inclusion_delay(2)
		.add_with_post_test(
			1,
			|| {},
			|| {
				let segment = <UnincludedSegment<Test>>::get();
				assert_eq!(segment.len(), 1);
				assert!(<AggregatedUnincludedSegment<Test>>::get().is_some());
			},
		)
		.add(2, || {}); // The previous block wasn't included yet, should panic in `create_inherent`.
}

#[test]
fn unincluded_code_upgrade_handles_signal() {
	CONSENSUS_HOOK.with(|c| {
		*c.borrow_mut() = Box::new(|_| (Weight::zero(), NonZeroU32::new(2).unwrap().into()))
	});

	BlockTests::new()
		.with_inclusion_delay(1)
		.with_relay_sproof_builder(|_, block_number, builder| {
			if block_number > 1 && block_number <= 3 {
				builder.upgrade_go_ahead = Some(relay_chain::UpgradeGoAhead::GoAhead);
			}
		})
		.add(1, || {
			assert_ok!(System::set_code(RawOrigin::Root.into(), Default::default()));
		})
		.add_with_post_test(
			2,
			|| {},
			|| {
				assert!(
					!<PendingValidationCode<Test>>::exists(),
					"validation function must have been unset"
				);
			},
		)
		.add_with_post_test(
			3,
			|| {
				// The signal is present in relay state proof and ignored.
				// Block that processed the signal is still not included.
			},
			|| {
				let segment = <UnincludedSegment<Test>>::get();
				assert_eq!(segment.len(), 2);
				let aggregated_segment =
					<AggregatedUnincludedSegment<Test>>::get().expect("segment is non-empty");
				assert_eq!(
					aggregated_segment.consumed_go_ahead_signal(),
					Some(relay_chain::UpgradeGoAhead::GoAhead)
				);
			},
		)
		.add_with_post_test(
			4,
			|| {},
			|| {
				let aggregated_segment =
					<AggregatedUnincludedSegment<Test>>::get().expect("segment is non-empty");
				// Block that processed the signal is included.
				assert!(aggregated_segment.consumed_go_ahead_signal().is_none());
			},
		);
}

#[test]
fn unincluded_code_upgrade_scheduled_after_go_ahead() {
	CONSENSUS_HOOK.with(|c| {
		*c.borrow_mut() = Box::new(|_| (Weight::zero(), NonZeroU32::new(2).unwrap().into()))
	});

	BlockTests::new()
		.with_inclusion_delay(1)
		.with_relay_sproof_builder(|_, block_number, builder| {
			if block_number > 1 && block_number <= 3 {
				builder.upgrade_go_ahead = Some(relay_chain::UpgradeGoAhead::GoAhead);
			}
		})
		.add(1, || {
			assert_ok!(System::set_code(RawOrigin::Root.into(), Default::default()));
		})
		.add_with_post_test(
			2,
			|| {},
			|| {
				assert!(
					!<PendingValidationCode<Test>>::exists(),
					"validation function must have been unset"
				);
				// The previous go-ahead signal was processed, schedule another upgrade.
				assert_ok!(System::set_code(RawOrigin::Root.into(), Default::default()));
			},
		)
		.add_with_post_test(
			3,
			|| {
				// The signal is present in relay state proof and ignored.
				// Block that processed the signal is still not included.
			},
			|| {
				let segment = <UnincludedSegment<Test>>::get();
				assert_eq!(segment.len(), 2);
				let aggregated_segment =
					<AggregatedUnincludedSegment<Test>>::get().expect("segment is non-empty");
				assert_eq!(
					aggregated_segment.consumed_go_ahead_signal(),
					Some(relay_chain::UpgradeGoAhead::GoAhead)
				);
			},
		)
		.add_with_post_test(
			4,
			|| {},
			|| {
				assert!(<PendingValidationCode<Test>>::exists(), "upgrade is pending");
			},
		);
}

#[test]
fn inherent_processed_messages_are_ignored() {
	CONSENSUS_HOOK.with(|c| {
		*c.borrow_mut() = Box::new(|_| (Weight::zero(), NonZeroU32::new(2).unwrap().into()))
	});

	BlockTests::new()
		.with_inclusion_delay(1)
		.with_relay_block_number(|block_number| 3.max(*block_number as RelayChainBlockNumber))
		.with_relay_sproof_builder(|_, relay_block_num, sproof| match relay_block_num {
			3 => {
				sproof.dmq_mqc_head =
					Some(MessageQueueChain::default().extend_downward(&mk_dmp(3, 0)).head());
				sproof.upsert_inbound_channel(ParaId::from(200)).mqc_head = Some(
					MessageQueueChain::default()
						.extend_hrmp(&mk_hrmp(2, 1))
						.extend_hrmp(&mk_hrmp(3, 1))
						.head(),
				);
			},
			_ => unreachable!(),
		})
		.with_inherent_data(|_, relay_block_num, data| match relay_block_num {
			3 => {
				data.downward_messages.push(mk_dmp(3, 0));
				data.horizontal_messages
					.insert(ParaId::from(200), vec![mk_hrmp(2, 1), mk_hrmp(3, 1)]);
			},
			_ => unreachable!(),
		})
		.add(1, || {
			// Don't drop processed messages for this test.
			HANDLED_DMP_MESSAGES.with(|m| {
				let m = m.borrow();
				// NOTE: if this fails, then run the test without benchmark features.
				assert_eq!(&*m, &[mk_dmp(3, 0).msg]);
			});
			HANDLED_XCMP_MESSAGES.with(|m| {
				let m = m.borrow_mut();
				assert_eq!(
					&*m,
					&[(ParaId::from(200), 2, vec![2]), (ParaId::from(200), 3, vec![3]),]
				);
			});
		})
		.add(2, || {})
		.add(3, || {
			HANDLED_DMP_MESSAGES.with(|m| {
				let m = m.borrow();
				assert_eq!(&*m, &[mk_dmp(3, 0).msg]);
			});
			HANDLED_XCMP_MESSAGES.with(|m| {
				let m = m.borrow_mut();
				assert_eq!(
					&*m,
					&[(ParaId::from(200), 2, vec![2]), (ParaId::from(200), 3, vec![3]),]
				);
			});
		});
}

#[test]
fn inherent_messages_are_compressed() {
	CONSENSUS_HOOK.with(|c| {
		*c.borrow_mut() = Box::new(|_| (Weight::zero(), NonZeroU32::new(2).unwrap().into()))
	});

	let mut dmp_msgs = vec![];
	let mut hrmp_msgs = vec![];

	// Batch 1
	dmp_msgs.extend(vec![mk_dmp(1, 1024 * 100); 10]);
	hrmp_msgs.push((ParaId::new(100), mk_hrmp(1, 24576)));
	hrmp_msgs.extend(vec![(ParaId::new(100), mk_hrmp(1, 1024 * 100)); 9]);
	hrmp_msgs.push((ParaId::new(200), mk_hrmp(1, 1024 * 100)));

	// Batch 2
	dmp_msgs.extend(vec![mk_dmp(2, 1024 * 100); 10]);
	hrmp_msgs.extend(vec![(ParaId::new(200), mk_hrmp(1, 1024 * 100)); 10]);

	// Batch 3
	dmp_msgs.extend(vec![mk_dmp(2, 1024 * 100); 5]);
	hrmp_msgs.extend(vec![(ParaId::new(100), mk_hrmp(2, 1024 * 100)); 15]);

	// Batch 4
	hrmp_msgs.extend(vec![(ParaId::new(200), mk_hrmp(2, 1024 * 100)); 1]);

	let dmp_msgs_clone = dmp_msgs.clone();
	let hrmp_msgs_clone = hrmp_msgs.clone();
	let mut test = BlockTests::new()
		.with_inclusion_delay(1)
		.with_relay_block_number(|block_number| 4.max(*block_number as RelayChainBlockNumber))
		.with_relay_sproof_builder(move |_, relay_block_num, sproof| match relay_block_num {
			4 => {
				let mut dmp_mqc = MessageQueueChain::default();
				for msg in &dmp_msgs_clone {
					dmp_mqc.extend_downward(msg);
				}
				sproof.dmq_mqc_head = Some(dmp_mqc.head());

				for (sender, msg) in &hrmp_msgs_clone {
					let channel = sproof.upsert_inbound_channel(*sender);
					channel.max_message_size = 100 * 1024;
					let mqc_head = channel.mqc_head.get_or_insert_default();
					let mut mqc = MessageQueueChain::new(*mqc_head);
					mqc.extend_hrmp(msg);
					*mqc_head = mqc.head();
				}
			},
			_ => unreachable!(),
		});

	let dmp_msgs_clone = dmp_msgs.clone();
	let hrmp_msgs_clone = hrmp_msgs.clone();
	test = test.with_inherent_data(move |_, relay_block_num, data| match relay_block_num {
		4 => {
			data.downward_messages.extend(dmp_msgs_clone.iter().cloned());

			for (sender, msg) in &hrmp_msgs_clone {
				let entry = data.horizontal_messages.entry(*sender).or_default();
				entry.push(msg.clone())
			}
		},
		_ => unreachable!(),
	});

	let dmp_msgs_clone = dmp_msgs.clone();
	let hrmp_msgs_clone = hrmp_msgs.clone();
	test = test.add(1, move || {
		HANDLED_DMP_MESSAGES.with(|m| {
			let m = m.borrow();
			assert_eq!(
				&*m,
				&dmp_msgs_clone[..10].into_iter().map(|msg| msg.msg.clone()).collect::<Vec<_>>()
			);
		});
		assert_eq!(
			LastProcessedDownwardMessage::<Test>::get(),
			Some(InboundMessageId { sent_at: 1, reverse_idx: 0 })
		);

		HANDLED_XCMP_MESSAGES.with(|m| {
			let m = m.borrow_mut();
			assert_eq!(
				&*m,
				&hrmp_msgs_clone[..11]
					.iter()
					.map(|(sender, msg)| (*sender, msg.sent_at, msg.data.clone()))
					.collect::<Vec<_>>()
			);
		});
		assert_eq!(
			LastProcessedHrmpMessage::<Test>::get(),
			Some(InboundMessageId { sent_at: 1, reverse_idx: 10 })
		);
		assert_eq!(HrmpWatermark::<Test>::get(), 0);
	});

	let dmp_msgs_clone = dmp_msgs.clone();
	let hrmp_msgs_clone = hrmp_msgs.clone();
	test = test.add(2, move || {
		HANDLED_DMP_MESSAGES.with(|m| {
			let m = m.borrow();
			assert_eq!(
				&*m,
				&dmp_msgs_clone[..20].iter().map(|msg| msg.msg.clone()).collect::<Vec<_>>()
			);
		});
		assert_eq!(
			LastProcessedDownwardMessage::<Test>::get(),
			Some(InboundMessageId { sent_at: 2, reverse_idx: 5 })
		);

		HANDLED_XCMP_MESSAGES.with(|m| {
			let m = m.borrow_mut();
			assert_eq!(
				&*m,
				&hrmp_msgs_clone[..21]
					.iter()
					.map(|(sender, msg)| (*sender, msg.sent_at, msg.data.clone()))
					.collect::<Vec<_>>()
			);
		});
		assert_eq!(
			LastProcessedHrmpMessage::<Test>::get(),
			Some(InboundMessageId { sent_at: 1, reverse_idx: 0 })
		);
		assert_eq!(HrmpWatermark::<Test>::get(), 1);
	});

	let dmp_msgs_clone = dmp_msgs.clone();
	let hrmp_msgs_clone = hrmp_msgs.clone();
	test = test.add(3, move || {
		HANDLED_DMP_MESSAGES.with(|m| {
			let m = m.borrow();
			assert_eq!(
				&*m,
				&dmp_msgs_clone[..25].iter().map(|msg| msg.msg.clone()).collect::<Vec<_>>()
			);
		});
		assert_eq!(
			LastProcessedDownwardMessage::<Test>::get(),
			Some(InboundMessageId { sent_at: 2, reverse_idx: 0 })
		);

		HANDLED_XCMP_MESSAGES.with(|m| {
			let m = m.borrow_mut();
			assert_eq!(
				&*m,
				&hrmp_msgs_clone[..36]
					.iter()
					.map(|(sender, msg)| (*sender, msg.sent_at, msg.data.clone()))
					.collect::<Vec<_>>()
			);
		});
		assert_eq!(
			LastProcessedHrmpMessage::<Test>::get(),
			Some(InboundMessageId { sent_at: 2, reverse_idx: 1 })
		);
		assert_eq!(HrmpWatermark::<Test>::get(), 1);
	});

	test.add(4, move || {
		HANDLED_DMP_MESSAGES.with(|m| {
			let m = m.borrow();
			assert_eq!(&*m, &dmp_msgs[..25].iter().map(|msg| msg.msg.clone()).collect::<Vec<_>>());
		});
		assert_eq!(
			LastProcessedDownwardMessage::<Test>::get(),
			Some(InboundMessageId { sent_at: 2, reverse_idx: 0 })
		);

		HANDLED_XCMP_MESSAGES.with(|m| {
			let m = m.borrow_mut();
			assert_eq!(
				&*m,
				&hrmp_msgs[..37]
					.iter()
					.map(|(sender, msg)| (*sender, msg.sent_at, msg.data.clone()))
					.collect::<Vec<_>>()
			);
		});
		assert_eq!(
			LastProcessedHrmpMessage::<Test>::get(),
			Some(InboundMessageId { sent_at: 2, reverse_idx: 0 })
		);
		assert_eq!(HrmpWatermark::<Test>::get(), 2);
	});
}

#[test]
fn check_hrmp_message_metadata_works_with_known_channel() {
	Pallet::<Test>::check_hrmp_message_metadata(
		&[(1000.into(), Default::default())],
		&mut None,
		(1, 1000.into()),
	);
}

#[test]
#[should_panic(
	expected = "One of the messages submitted by the collator was sent from a sender (2000) that \
	doesn't have a channel opened to this parachain"
)]
fn check_hrmp_message_metadata_panics_on_unknown_channel() {
	Pallet::<Test>::check_hrmp_message_metadata(
		&[(1000.into(), Default::default())],
		&mut None,
		(1, 2000.into()),
	);
}

#[test]
fn check_hrmp_message_metadata_works_when_correctly_ordered() {
	Pallet::<Test>::check_hrmp_message_metadata(
		&[(1000.into(), Default::default())],
		&mut None,
		(1, 1000.into()),
	);

	Pallet::<Test>::check_hrmp_message_metadata(
		&[(1000.into(), Default::default())],
		&mut Some((0, 1000.into())),
		(1, 1000.into()),
	);

	// Test chained checks
	let mut prev = None;
	Pallet::<Test>::check_hrmp_message_metadata(
		&[(1000.into(), Default::default())],
		&mut prev,
		(0, 1000.into()),
	);
	Pallet::<Test>::check_hrmp_message_metadata(
		&[(1000.into(), Default::default())],
		&mut prev,
		(1, 1000.into()),
	);
	Pallet::<Test>::check_hrmp_message_metadata(
		&[(1000.into(), Default::default())],
		&mut prev,
		(1, 1000.into()),
	);
	Pallet::<Test>::check_hrmp_message_metadata(
		&[(1000.into(), Default::default())],
		&mut prev,
		(2, 1000.into()),
	);
}

#[test]
#[should_panic(expected = "[HRMP] Messages order violation")]
fn check_hrmp_message_metadata_panics_on_unordered_sent_at() {
	Pallet::<Test>::check_hrmp_message_metadata(
		&[(1000.into(), Default::default())],
		&mut Some((1, 1000.into())),
		(0, 1000.into()),
	);
}

#[test]
#[should_panic(expected = "[HRMP] Messages order violation")]
fn chained_check_hrmp_message_metadata_panics_on_unordered_sent_at() {
	// Test chained checks
	let mut prev = None;
	Pallet::<Test>::check_hrmp_message_metadata(
		&[(1000.into(), Default::default())],
		&mut prev,
		(1, 1000.into()),
	);
	Pallet::<Test>::check_hrmp_message_metadata(
		&[(1000.into(), Default::default())],
		&mut prev,
		(0, 1000.into()),
	);
}

#[test]
#[should_panic(expected = "[HRMP] Messages order violation")]
fn check_hrmp_message_metadata_panics_on_unordered_para_id() {
	Pallet::<Test>::check_hrmp_message_metadata(
		&[(1000.into(), Default::default())],
		&mut Some((1, 2000.into())),
		(1, 1000.into()),
	);
}

#[test]
#[should_panic(expected = "[HRMP] Messages order violation")]
fn chained_check_hrmp_message_metadata_panics_on_unordered_para_id() {
	// Test chained checks
	let mut prev = None;
	Pallet::<Test>::check_hrmp_message_metadata(
		&[(1000.into(), Default::default()), (2000.into(), Default::default())],
		&mut prev,
		(1, 2000.into()),
	);
	Pallet::<Test>::check_hrmp_message_metadata(
		&[(1000.into(), Default::default())],
		&mut prev,
		(1, 1000.into()),
	);
}

#[test]
#[should_panic(
	expected = "One of the messages submitted by the collator was sent from a sender (2000) that \
	doesn't have a channel opened to this parachain"
)]
fn hrmp_ingress_channels_are_checked() {
	CONSENSUS_HOOK.with(|c| {
		*c.borrow_mut() = Box::new(|_| (Weight::zero(), NonZeroU32::new(2).unwrap().into()))
	});

	let mut test = BlockTests::new()
		.with_inclusion_delay(1)
		.with_relay_block_number(|block_number| 1.max(*block_number as RelayChainBlockNumber))
		.with_relay_sproof_builder(move |_, relay_block_num, sproof| match relay_block_num {
			// Let's open a channel only with parachain 1000.
			1 => {
				let mqc_head =
					sproof.upsert_inbound_channel(1000.into()).mqc_head.get_or_insert_default();
				let mut mqc = MessageQueueChain::new(*mqc_head);
				mqc.extend_hrmp(&mk_hrmp(1, 100));
				*mqc_head = mqc.head();
			},
			_ => {},
		})
		.with_inherent_data(move |_, relay_block_num, data| match relay_block_num {
			// Simulate receiving a message from parachain 1000 at block 1. This should work.
			1 => {
				let entry = data.horizontal_messages.entry(1000.into()).or_default();
				entry.push(mk_hrmp(1, 100))
			},
			_ => {},
		})
		.add(1, move || {
			HANDLED_XCMP_MESSAGES.with(|m| {
				let m = m.borrow_mut();
				assert_eq!(&*m, &vec![(1000.into(), 1, vec![1; 100])]);
			});
		});
	test.run();

	let mut test = test
		.with_relay_block_number(|block_number| 2.max(*block_number as RelayChainBlockNumber))
		.with_inherent_data(move |_, relay_block_num, data| match relay_block_num {
			// Simulate receiving a message from parachain 2000 at block 2. This should lead to a
			// panic.
			2 => {
				let entry = data.horizontal_messages.entry(2000.into()).or_default();
				entry.push(mk_hrmp(1, 100))
			},
			_ => {},
		});
	test.run();
}

#[test]
fn hrmp_outbound_respects_used_bandwidth() {
	let recipient = ParaId::from(400);

	CONSENSUS_HOOK.with(|c| {
		*c.borrow_mut() = Box::new(|_| (Weight::zero(), NonZeroU32::new(3).unwrap().into()))
	});

	BlockTests::new()
		.with_inclusion_delay(2)
		.with_relay_sproof_builder(move |_, block_number, sproof| {
			sproof.host_config.hrmp_max_message_num_per_candidate = 10;
			let channel = sproof.upsert_outbound_channel(recipient);
			channel.max_capacity = 2;
			channel.max_total_size = 4;

			channel.max_message_size = 10;

			// states:
			// [relay_chain][unincluded_segment] + [message_queue]
			// 2: []["2"] + ["2222"]
			// 3: []["2", "3"] + ["2222"]
			// 4: []["2", "3"] + ["2222", "444", "4"]
			// 5: ["2"]["3"] + ["2222", "444", "4"]
			// 6: ["2", "3"][] + ["2222", "444", "4"]
			// 7: ["3"]["444"] + ["2222", "4"]
			// 8: []["444", "4"] + ["2222"]
			//
			// 2 tests max bytes - there is message space but no byte space.
			// 4 tests max capacity - there is byte space but no message space

			match block_number {
				5 => {
					// 2 included.
					// one message added
					channel.msg_count = 1;
					channel.total_size = 1;
				},
				6 => {
					// 3 included.
					// one message added
					channel.msg_count = 2;
					channel.total_size = 2;
				},
				7 => {
					// 4 included.
					// one message drained.
					channel.msg_count = 1;
					channel.total_size = 1;
				},
				8 => {
					// 5 included. no messages added, one drained.
					channel.msg_count = 0;
					channel.total_size = 0;
				},
				_ => {
					channel.msg_count = 0;
					channel.total_size = 0;
				},
			}
		})
		.add(1, || {})
		.add_with_post_test(
			2,
			move || {
				send_message(recipient, b"2".to_vec());
				send_message(recipient, b"2222".to_vec());
			},
			move || {
				let v = HrmpOutboundMessages::<Test>::get();
				assert_eq!(v, vec![OutboundHrmpMessage { recipient, data: b"2".to_vec() }]);
			},
		)
		.add_with_post_test(
			3,
			move || {
				send_message(recipient, b"3".to_vec());
			},
			move || {
				let v = HrmpOutboundMessages::<Test>::get();
				assert_eq!(v, vec![OutboundHrmpMessage { recipient, data: b"3".to_vec() }]);
			},
		)
		.add_with_post_test(
			4,
			move || {
				send_message(recipient, b"444".to_vec());
				send_message(recipient, b"4".to_vec());
			},
			move || {
				// Queue has byte capacity but not message capacity.
				let v = HrmpOutboundMessages::<Test>::get();
				assert!(v.is_empty());
			},
		)
		.add_with_post_test(
			5,
			|| {},
			move || {
				// 1 is included here, channel not drained yet. nothing fits.
				let v = HrmpOutboundMessages::<Test>::get();
				assert!(v.is_empty());
			},
		)
		.add_with_post_test(
			6,
			|| {},
			move || {
				// 2 is included here. channel is totally full.
				let v = HrmpOutboundMessages::<Test>::get();
				assert!(v.is_empty());
			},
		)
		.add_with_post_test(
			7,
			|| {},
			move || {
				// 3 is included here. One message was drained out. The 3-byte message
				// finally fits
				let v = HrmpOutboundMessages::<Test>::get();
				// This line relies on test implementation of [`XcmpMessageSource`].
				assert_eq!(v, vec![OutboundHrmpMessage { recipient, data: b"444".to_vec() }]);
			},
		)
		.add_with_post_test(
			8,
			|| {},
			move || {
				// 4 is included here. Relay-chain side of the queue is empty,
				let v = HrmpOutboundMessages::<Test>::get();
				// This line relies on test implementation of [`XcmpMessageSource`].
				assert_eq!(v, vec![OutboundHrmpMessage { recipient, data: b"4".to_vec() }]);
			},
		);
}

#[test]
fn runtime_upgrade_events() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, block_number, builder| {
			if block_number == 2 {
				builder.upgrade_go_ahead = Some(relay_chain::UpgradeGoAhead::GoAhead);
			}
		})
		.add_with_post_test(
			1,
			|| {
				assert_ok!(System::set_code(RawOrigin::Root.into(), Default::default()));
			},
			|| {
				let events = System::events();
				assert_eq!(
					events[0].event,
					RuntimeEvent::ParachainSystem(crate::Event::ValidationFunctionStored)
				);
			},
		)
		.add_with_post_test(
			2,
			|| {},
			|| {
				let events = System::events();

				// system_version 1: update_code_in_storage writes :code directly,
				// emitting both the digest and CodeUpdated event in the same block.
				assert!(matches!(
					events[0].event,
					RuntimeEvent::System(frame_system::Event::CodeUpdated { .. })
				));
				assert_eq!(
					events[1].event,
					RuntimeEvent::ParachainSystem(crate::Event::ValidationFunctionApplied {
						relay_chain_block_num: 2
					})
				);

				assert!(System::digest()
					.logs()
					.iter()
					.any(|d| *d == sp_runtime::generic::DigestItem::RuntimeEnvironmentUpdated));
			},
		);
}

#[test]
fn non_overlapping() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, _, builder| {
			builder.host_config.validation_upgrade_delay = 1000;
		})
		.add(1, || {
			assert_ok!(System::set_code(RawOrigin::Root.into(), Default::default()));
		})
		.add(2, || {
			assert_eq!(
				System::set_code(RawOrigin::Root.into(), Default::default()),
				Err(Error::<Test>::OverlappingUpgrades.into()),
			)
		});
}

#[test]
fn manipulates_storage() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, block_number, builder| {
			if block_number > 1 {
				builder.upgrade_go_ahead = Some(relay_chain::UpgradeGoAhead::GoAhead);
			}
		})
		.add(1, || {
			assert!(
				!<PendingValidationCode<Test>>::exists(),
				"validation function must not exist yet"
			);
			assert_ok!(System::set_code(RawOrigin::Root.into(), Default::default()));
			assert!(<PendingValidationCode<Test>>::exists(), "validation function must now exist");
		})
		.add_with_post_test(
			2,
			|| {},
			|| {
				assert!(
					!<PendingValidationCode<Test>>::exists(),
					"validation function must have been unset"
				);
			},
		);
}

#[test]
fn aborted_upgrade() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, block_number, builder| {
			if block_number > 1 {
				builder.upgrade_go_ahead = Some(relay_chain::UpgradeGoAhead::Abort);
			}
		})
		.add(1, || {
			assert_ok!(System::set_code(RawOrigin::Root.into(), Default::default()));
		})
		.add_with_post_test(
			2,
			|| {},
			|| {
				assert!(
					!<PendingValidationCode<Test>>::exists(),
					"validation function must have been unset"
				);
				let events = System::events();
				assert_eq!(
					events[0].event,
					RuntimeEvent::ParachainSystem(crate::Event::ValidationFunctionDiscarded)
				);
			},
		);
}

#[test]
fn checks_code_size() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, _, builder| {
			builder.host_config.max_code_size = 8;
		})
		.add(1, || {
			assert_eq!(
				System::set_code(RawOrigin::Root.into(), vec![0; 64]),
				Err(Error::<Test>::TooBig.into()),
			);
		});
}

#[test]
fn send_upward_message_num_per_candidate() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, _, sproof| {
			sproof.host_config.max_upward_message_num_per_candidate = 1;
			sproof.relay_dispatch_queue_remaining_capacity = None;
		})
		.add_with_post_test(
			1,
			|| {
				ParachainSystem::send_upward_message(b"Mr F was here".to_vec()).unwrap();
				ParachainSystem::send_upward_message(b"message 2".to_vec()).unwrap();
			},
			|| {
				let v = UpwardMessages::<Test>::get();
				assert_eq!(v, vec![b"Mr F was here".to_vec()]);
			},
		)
		.add_with_post_test(
			2,
			|| {
				assert_eq!(UnincludedSegment::<Test>::get().len(), 0);
				// do nothing within block
			},
			|| {
				let v = UpwardMessages::<Test>::get();
				assert_eq!(v, vec![b"message 2".to_vec()]);
			},
		);
}

#[test]
fn send_upward_message_relay_bottleneck() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, relay_block_num, sproof| {
			sproof.host_config.max_upward_message_num_per_candidate = 2;
			sproof.host_config.max_upward_queue_count = 5;

			match relay_block_num {
				1 => sproof.relay_dispatch_queue_remaining_capacity = Some((0, 2048)),
				2 => sproof.relay_dispatch_queue_remaining_capacity = Some((1, 2048)),
				_ => unreachable!(),
			}
		})
		.add_with_post_test(
			1,
			|| {
				ParachainSystem::send_upward_message(vec![0u8; 8]).unwrap();
			},
			|| {
				// The message won't be sent because there is already one message in queue.
				let v = UpwardMessages::<Test>::get();
				assert!(v.is_empty());
			},
		)
		.add_with_post_test(
			2,
			|| { /* do nothing within block */ },
			|| {
				let v = UpwardMessages::<Test>::get();
				assert_eq!(v, vec![vec![0u8; 8]]);
			},
		);
}

#[test]
fn send_upwards_message_checks_size_on_validate() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, _, sproof| {
			sproof.host_config.max_upward_message_size = 128;
		})
		.add(1, || {
			assert_eq!(
				ParachainSystem::can_send_upward_message(vec![0u8; 129].as_ref()),
				Err(MessageSendError::TooBig)
			);
		});
}

#[test]
fn send_upward_message_check_size() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, _, sproof| {
			sproof.host_config.max_upward_message_size = 128;
		})
		.add(1, || {
			assert_eq!(
				ParachainSystem::send_upward_message(vec![0u8; 129]),
				Err(MessageSendError::TooBig)
			);
		});
}

#[test]
fn send_hrmp_message_buffer_channel_close() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, relay_block_num, sproof| {
			// Base case setup
			//
			sproof.para_id = ParaId::from(200);
			sproof.hrmp_egress_channel_index = Some(vec![ParaId::from(300), ParaId::from(400)]);
			sproof.hrmp_channels.insert(
				HrmpChannelId { sender: ParaId::from(200), recipient: ParaId::from(300) },
				AbridgedHrmpChannel {
					max_capacity: 1,
					msg_count: 1, // <- 1/1 means the channel is full
					max_total_size: 1024,
					max_message_size: 8,
					total_size: 0,
					mqc_head: Default::default(),
				},
			);
			sproof.hrmp_channels.insert(
				HrmpChannelId { sender: ParaId::from(200), recipient: ParaId::from(400) },
				AbridgedHrmpChannel {
					max_capacity: 1,
					msg_count: 1,
					max_total_size: 1024,
					max_message_size: 8,
					total_size: 0,
					mqc_head: Default::default(),
				},
			);

			// Adjustment according to block
			//
			match relay_block_num {
				1 => {},
				2 => {},
				3 => {
					// The channel 200->400 ceases to exist at the relay chain block 3
					sproof
						.hrmp_egress_channel_index
						.as_mut()
						.unwrap()
						.retain(|n| n != &ParaId::from(400));
					sproof.hrmp_channels.remove(&HrmpChannelId {
						sender: ParaId::from(200),
						recipient: ParaId::from(400),
					});

					// We also free up space for a message in the 200->300 channel.
					sproof
						.hrmp_channels
						.get_mut(&HrmpChannelId {
							sender: ParaId::from(200),
							recipient: ParaId::from(300),
						})
						.unwrap()
						.msg_count = 0;
				},
				_ => unreachable!(),
			}
		})
		.add_with_post_test(
			1,
			|| {
				send_message(ParaId::from(300), b"1".to_vec());
				send_message(ParaId::from(400), b"2".to_vec());
			},
			|| {},
		)
		.add_with_post_test(
			2,
			|| {},
			|| {
				// Both channels are at capacity so we do not expect any messages.
				let v = HrmpOutboundMessages::<Test>::get();
				assert!(v.is_empty());
			},
		)
		.add_with_post_test(
			3,
			|| {},
			|| {
				let v = HrmpOutboundMessages::<Test>::get();
				assert_eq!(
					v,
					vec![OutboundHrmpMessage { recipient: ParaId::from(300), data: b"1".to_vec() }]
				);
			},
		);
}

#[test]
fn message_queue_chain() {
	assert_eq!(MessageQueueChain::default().head(), H256::zero());

	// Note that the resulting hashes are the same for HRMP and DMP. That's because even though
	// the types are nominally different, they have the same structure and computation of the
	// new head doesn't differ.
	//
	// These cases are taken from https://github.com/paritytech/polkadot/pull/2351
	assert_eq!(
		MessageQueueChain::default()
			.extend_downward(&InboundDownwardMessage { sent_at: 2, msg: vec![1, 2, 3] })
			.extend_downward(&InboundDownwardMessage { sent_at: 3, msg: vec![4, 5, 6] })
			.head(),
		hex!["88dc00db8cc9d22aa62b87807705831f164387dfa49f80a8600ed1cbe1704b6b"].into(),
	);
	assert_eq!(
		MessageQueueChain::default()
			.extend_hrmp(&InboundHrmpMessage { sent_at: 2, data: vec![1, 2, 3] })
			.extend_hrmp(&InboundHrmpMessage { sent_at: 3, data: vec![4, 5, 6] })
			.head(),
		hex!["88dc00db8cc9d22aa62b87807705831f164387dfa49f80a8600ed1cbe1704b6b"].into(),
	);
}

#[test]
#[cfg(not(feature = "runtime-benchmarks"))]
fn receive_dmp() {
	static MSG: std::sync::LazyLock<InboundDownwardMessage> =
		std::sync::LazyLock::new(|| InboundDownwardMessage { sent_at: 1, msg: b"down".to_vec() });

	BlockTests::new()
		.with_relay_sproof_builder(|_, relay_block_num, sproof| match relay_block_num {
			1 => {
				sproof.dmq_mqc_head =
					Some(MessageQueueChain::default().extend_downward(&MSG).head());
			},
			_ => unreachable!(),
		})
		.with_inherent_data(|_, relay_block_num, data| match relay_block_num {
			1 => {
				data.downward_messages.push((*MSG).clone());
			},
			_ => unreachable!(),
		})
		.add(1, || {
			HANDLED_DMP_MESSAGES.with(|m| {
				let mut m = m.borrow_mut();
				assert_eq!(&*m, &[MSG.msg.clone()]);
				m.clear();
			});
		});
}

#[test]
#[cfg(not(feature = "runtime-benchmarks"))]
fn receive_dmp_after_pause() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, relay_block_num, sproof| match relay_block_num {
			1 => {
				sproof.dmq_mqc_head =
					Some(MessageQueueChain::default().extend_downward(&mk_dmp(1, 0)).head());
			},
			2 => {
				// no new messages, mqc stayed the same.
				sproof.dmq_mqc_head =
					Some(MessageQueueChain::default().extend_downward(&mk_dmp(1, 0)).head());
			},
			3 => {
				sproof.dmq_mqc_head = Some(
					MessageQueueChain::default()
						.extend_downward(&mk_dmp(1, 0))
						.extend_downward(&mk_dmp(3, 0))
						.head(),
				);
			},
			_ => unreachable!(),
		})
		.with_inherent_data(|_, relay_block_num, data| match relay_block_num {
			1 => {
				data.downward_messages.push(mk_dmp(1, 0));
			},
			2 => {
				// no new messages
			},
			3 => {
				data.downward_messages.push(mk_dmp(3, 0));
			},
			_ => unreachable!(),
		})
		.add(1, || {
			HANDLED_DMP_MESSAGES.with(|m| {
				let mut m = m.borrow_mut();
				assert_eq!(&*m, &[(mk_dmp(1, 0).msg.clone())]);
				m.clear();
			});
		})
		.add(2, || {})
		.add(3, || {
			HANDLED_DMP_MESSAGES.with(|m| {
				let mut m = m.borrow_mut();
				assert_eq!(&*m, &[(mk_dmp(3, 0).msg.clone())]);
				m.clear();
			});
		});
}

// Sent up to 100 DMP messages per block over a period of 100 blocks.
#[test]
#[cfg(not(feature = "runtime-benchmarks"))]
fn receive_dmp_many() {
	wasm_ext().execute_with(|| {
		parameter_types! {
			pub storage MqcHead: MessageQueueChain = Default::default();
			pub storage SentInBlock: Vec<Vec<InboundDownwardMessage>> = Default::default();
		}

		let mut sent_in_block = vec![vec![]];
		let mut rng = rand::thread_rng();

		for block in 1..100 {
			let mut msgs = vec![];
			for _ in 1..=rng.gen_range(1..=100) {
				// Just use the same message multiple times per block.
				msgs.push(mk_dmp(block, 0));
			}
			sent_in_block.push(msgs);
		}
		SentInBlock::set(&sent_in_block);

		let mut tester = BlockTests::new_without_externalities()
			.with_relay_sproof_builder(|_, relay_block_num, sproof| {
				let mut new_hash = MqcHead::get();

				for msg in SentInBlock::get()[relay_block_num as usize].iter() {
					new_hash.extend_downward(&msg);
				}

				sproof.dmq_mqc_head = Some(new_hash.head());
				MqcHead::set(&new_hash);
			})
			.with_inherent_data(|_, relay_block_num, data| {
				for msg in SentInBlock::get()[relay_block_num as usize].iter() {
					data.downward_messages.push(msg.clone());
				}
			});

		for block in 1..100 {
			tester = tester.add(block, move || {
				HANDLED_DMP_MESSAGES.with(|m| {
					let mut m = m.borrow_mut();
					let msgs = SentInBlock::get()[block as usize]
						.iter()
						.map(|m| m.msg.clone())
						.collect::<Vec<_>>();
					assert_eq!(&*m, &msgs);
					m.clear();
				});
			});
		}
	});
}

#[test]
fn receive_hrmp() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, relay_block_num, sproof| match relay_block_num {
			1 => {
				// 200 - doesn't exist yet
				// 300 - one new message
				sproof.upsert_inbound_channel(ParaId::from(300)).mqc_head =
					Some(MessageQueueChain::default().extend_hrmp(&mk_hrmp(1, 1)).head());
			},
			2 => {
				// 200 - now present with one message
				// 300 - two new messages
				sproof.upsert_inbound_channel(ParaId::from(200)).mqc_head =
					Some(MessageQueueChain::default().extend_hrmp(&mk_hrmp(4, 1)).head());
				sproof.upsert_inbound_channel(ParaId::from(300)).mqc_head = Some(
					MessageQueueChain::default()
						.extend_hrmp(&mk_hrmp(1, 1))
						.extend_hrmp(&mk_hrmp(2, 1))
						.extend_hrmp(&mk_hrmp(3, 1))
						.head(),
				);
			},
			3 => {
				// 200 - no new messages
				// 300 - is gone
				sproof.upsert_inbound_channel(ParaId::from(200)).mqc_head =
					Some(MessageQueueChain::default().extend_hrmp(&mk_hrmp(4, 1)).head());
			},
			_ => unreachable!(),
		})
		.with_inherent_data(|_, relay_block_num, data| match relay_block_num {
			1 => {
				data.horizontal_messages.insert(ParaId::from(300), vec![mk_hrmp(1, 1)]);
			},
			2 => {
				data.horizontal_messages.insert(
					ParaId::from(300),
					vec![
						// Can't be sent at the block 1 actually. However, we cheat here
						// because we want to test the case where there are multiple messages
						// but the harness at the moment doesn't support block skipping.
						mk_hrmp(2, 1).clone(),
						mk_hrmp(3, 1).clone(),
					],
				);
				data.horizontal_messages.insert(ParaId::from(200), vec![mk_hrmp(4, 1)]);
			},
			3 => {},
			_ => unreachable!(),
		})
		.add(1, || {
			HANDLED_XCMP_MESSAGES.with(|m| {
				let mut m = m.borrow_mut();
				assert_eq!(&*m, &[(ParaId::from(300), 1, vec![1])]);
				m.clear();
			});
		})
		.add(2, || {
			HANDLED_XCMP_MESSAGES.with(|m| {
				let mut m = m.borrow_mut();
				assert_eq!(
					&*m,
					&[
						(ParaId::from(300), 2, vec![2]),
						(ParaId::from(300), 3, vec![3]),
						(ParaId::from(200), 4, vec![4]),
					]
				);
				m.clear();
			});
		})
		.add(3, || {});
}

// A channel that was force removed from RC state will clean up any remaining state.
#[test]
fn receive_hrmp_channel_suddenly_removed_from_relay_state() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, relay_block_num, sproof| match relay_block_num {
			1 => {
				// 300 - one new message
				sproof.upsert_inbound_channel(ParaId::from(300)).mqc_head =
					Some(MessageQueueChain::default().extend_hrmp(&mk_hrmp(1, 1)).head());
			},
			2 => {
				// 300 - is gone, this should trigger the cleanup
			},
			_ => unreachable!(),
		})
		.with_inherent_data(|_, relay_block_num, data| match relay_block_num {
			1 => {
				data.horizontal_messages.insert(ParaId::from(300), vec![mk_hrmp(1, 1)]);
			},
			2 => {},
			_ => unreachable!(),
		})
		.add(1, || {
			HANDLED_XCMP_MESSAGES.with(|m| {
				let mut m = m.borrow_mut();
				assert_eq!(&*m, &[(ParaId::from(300), 1, vec![1])], "Received on channel 300");
				m.clear();
			});
			assert!(
				LastHrmpMqcHeads::<Test>::get().contains_key(&ParaId::from(300)),
				"Channel 300 should be present"
			);
		})
		.add(2, || {
			assert_eq!(
				LastHrmpMqcHeads::<Test>::get().into_keys().collect::<Vec<_>>(),
				vec![],
				"Channel 300 should be removed"
			);
		});
}

// Same as above but other code path since another channel contains a message.
#[test]
fn receive_hrmp_channel_suddenly_removed_from_relay_state2() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, relay_block_num, sproof| match relay_block_num {
			1 => {
				// 200 - one new message
				sproof.upsert_inbound_channel(ParaId::from(200)).mqc_head =
					Some(MessageQueueChain::default().extend_hrmp(&mk_hrmp(1, 1)).head());
				// 300 - one new message
				sproof.upsert_inbound_channel(ParaId::from(300)).mqc_head =
					Some(MessageQueueChain::default().extend_hrmp(&mk_hrmp(1, 1)).head());
			},
			2 => {
				// 200 - no new messages, mqc stayed the same.
				sproof.upsert_inbound_channel(ParaId::from(200)).mqc_head =
					Some(MessageQueueChain::default().extend_hrmp(&mk_hrmp(1, 1)).head());
				// 300 - is gone, this should trigger the cleanup
			},
			_ => unreachable!(),
		})
		.with_inherent_data(|_, relay_block_num, data| match relay_block_num {
			1 => {
				data.horizontal_messages.insert(ParaId::from(200), vec![mk_hrmp(1, 1)]);
				data.horizontal_messages.insert(ParaId::from(300), vec![mk_hrmp(1, 1)]);
			},
			2 => {},
			_ => unreachable!(),
		})
		.add(1, || {
			HANDLED_XCMP_MESSAGES.with(|m| {
				let mut m = m.borrow_mut();
				assert_eq!(
					&*m,
					&[(ParaId::from(200), 1, vec![1]), (ParaId::from(300), 1, vec![1])]
				);
				m.clear();
			});
			assert!(
				LastHrmpMqcHeads::<Test>::get().contains_key(&ParaId::from(300)),
				"Channel 300 should be present"
			);
		})
		.add(2, || {
			assert_eq!(
				LastHrmpMqcHeads::<Test>::get().into_keys().collect::<Vec<_>>(),
				vec![ParaId::from(200)],
				"Channel 300 should be removed but 200 should be present",
			);
		});
}

#[test]
fn receive_hrmp_empty_channel() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, relay_block_num, sproof| match relay_block_num {
			1 => {
				// no channels
			},
			2 => {
				// one new channel
				sproof.upsert_inbound_channel(ParaId::from(300)).mqc_head =
					Some(MessageQueueChain::default().head());
			},
			_ => unreachable!(),
		})
		.add(1, || {})
		.add(2, || {});
}

#[test]
fn receive_hrmp_after_pause() {
	const ALICE: ParaId = ParaId::new(300);

	BlockTests::new()
		.with_relay_sproof_builder(|_, relay_block_num, sproof| match relay_block_num {
			1 => {
				sproof.upsert_inbound_channel(ALICE).mqc_head =
					Some(MessageQueueChain::default().extend_hrmp(&mk_hrmp(1, 1)).head());
			},
			2 => {
				// 300 - no new messages, mqc stayed the same.
				sproof.upsert_inbound_channel(ALICE).mqc_head =
					Some(MessageQueueChain::default().extend_hrmp(&mk_hrmp(1, 1)).head());
			},
			3 => {
				// 300 - new message.
				sproof.upsert_inbound_channel(ALICE).mqc_head = Some(
					MessageQueueChain::default()
						.extend_hrmp(&mk_hrmp(1, 1))
						.extend_hrmp(&mk_hrmp(3, 1))
						.head(),
				);
			},
			_ => unreachable!(),
		})
		.with_inherent_data(|_, relay_block_num, data| match relay_block_num {
			1 => {
				data.horizontal_messages.insert(ALICE, vec![mk_hrmp(1, 1)]);
			},
			2 => {
				// no new messages
			},
			3 => {
				data.horizontal_messages.insert(ALICE, vec![mk_hrmp(3, 1)]);
			},
			_ => unreachable!(),
		})
		.add(1, || {
			HANDLED_XCMP_MESSAGES.with(|m| {
				let mut m = m.borrow_mut();
				assert_eq!(&*m, &[(ALICE, 1, vec![1])]);
				m.clear();
			});
		})
		.add(2, || {})
		.add(3, || {
			HANDLED_XCMP_MESSAGES.with(|m| {
				let mut m = m.borrow_mut();
				assert_eq!(&*m, &[(ALICE, 3, vec![3])]);
				m.clear();
			});
		});
}

// Sent up to 100 HRMP messages per block over a period of 100 blocks.
#[test]
fn receive_hrmp_many() {
	const ALICE: ParaId = ParaId::new(300);

	wasm_ext().execute_with(|| {
		parameter_types! {
			pub storage MqcHead: MessageQueueChain = Default::default();
			pub storage SentInBlock: Vec<Vec<InboundHrmpMessage>> = Default::default();
		}

		let mut sent_in_block = vec![vec![]];
		let mut rng = rand::thread_rng();

		for block in 1..100 {
			let mut msgs = vec![];
			for _ in 1..=rng.gen_range(1..=100) {
				// Just use the same message multiple times per block.
				msgs.push(mk_hrmp(block, 0));
			}
			sent_in_block.push(msgs);
		}
		SentInBlock::set(&sent_in_block);

		let mut tester = BlockTests::new_without_externalities()
			.with_relay_sproof_builder(|_, relay_block_num, sproof| {
				let mut new_hash = MqcHead::get();

				for msg in SentInBlock::get()[relay_block_num as usize].iter() {
					new_hash.extend_hrmp(&msg);
				}

				sproof.upsert_inbound_channel(ALICE).mqc_head = Some(new_hash.head());
				MqcHead::set(&new_hash);
			})
			.with_inherent_data(|_, relay_block_num, data| {
				// TODO use vector for dmp as well
				data.horizontal_messages
					.insert(ALICE, SentInBlock::get()[relay_block_num as usize].clone());
			});

		for block in 1..100 {
			tester = tester.add(block, move || {
				HANDLED_XCMP_MESSAGES.with(|m| {
					let mut m = m.borrow_mut();
					let msgs = SentInBlock::get()[block as usize]
						.iter()
						.map(|m| (ALICE, m.sent_at, m.data.clone()))
						.collect::<Vec<_>>();
					assert_eq!(&*m, &msgs);
					m.clear();
				});
			});
		}
	});
}

#[test]
#[cfg(not(feature = "runtime-benchmarks"))]
fn upgrade_version_checks_should_work() {
	use codec::Encode;
	use sp_version::RuntimeVersion;

	let test_data = vec![
		("test", 0, 1, frame_system::Error::<Test>::SpecVersionNeedsToIncrease),
		("test", 1, 0, frame_system::Error::<Test>::SpecVersionNeedsToIncrease),
		("test", 1, 1, frame_system::Error::<Test>::SpecVersionNeedsToIncrease),
		("test", 1, 2, frame_system::Error::<Test>::SpecVersionNeedsToIncrease),
		("test2", 1, 1, frame_system::Error::<Test>::InvalidSpecName),
	];

	for (spec_name, spec_version, impl_version, expected) in test_data.into_iter() {
		let version = RuntimeVersion {
			spec_name: spec_name.into(),
			spec_version,
			impl_version,
			..Default::default()
		};
		let read_runtime_version = ReadRuntimeVersion(version.encode());

		let mut ext = new_test_ext();
		ext.register_extension(sp_core::traits::ReadRuntimeVersionExt::new(read_runtime_version));
		ext.execute_with(|| {
			System::set_block_number(1);

			let new_code = vec![1, 2, 3, 4];
			let new_code_hash = H256(sp_crypto_hashing::blake2_256(&new_code));

			let _authorize = System::authorize_upgrade(RawOrigin::Root.into(), new_code_hash);
			assert_ok!(System::apply_authorized_upgrade(RawOrigin::None.into(), new_code));

			System::assert_last_event(
				frame_system::Event::RejectedInvalidAuthorizedUpgrade {
					code_hash: new_code_hash,
					error: expected.into(),
				}
				.into(),
			);
		});
	}
}

#[test]
fn deposits_relay_parent_storage_root() {
	BlockTests::new().add_with_post_test(
		1,
		|| {},
		|| {
			let digest = System::digest();
			assert!(cumulus_primitives_core::rpsr_digest::extract_relay_parent_storage_root(
				&digest
			)
			.is_some());
		},
	);
}

#[test]
fn ump_fee_factor_increases_and_decreases() {
	BlockTests::new()
		.with_relay_sproof_builder(|_, _, sproof| {
			sproof.host_config.max_upward_queue_size = 100;
			sproof.host_config.max_upward_message_num_per_candidate = 1;
		})
		.add_with_post_test(
			1,
			|| {
				// Fee factor increases in `send_upward_message`
				ParachainSystem::send_upward_message(b"Test".to_vec()).unwrap();
				assert_eq!(UpwardDeliveryFeeFactor::<Test>::get(), FixedU128::from_u32(1));

				ParachainSystem::send_upward_message(
					b"This message will be enough to increase the fee factor".to_vec(),
				)
				.unwrap();
				assert_eq!(
					UpwardDeliveryFeeFactor::<Test>::get(),
					FixedU128::from_rational(105, 100)
				);
			},
			|| {
				// Factor decreases in `on_finalize`, but only if we are below the threshold
				let messages = UpwardMessages::<Test>::get();
				assert_eq!(messages, vec![b"Test".to_vec()]);
				assert_eq!(
					UpwardDeliveryFeeFactor::<Test>::get(),
					FixedU128::from_rational(105, 100)
				);
			},
		)
		.add_with_post_test(
			2,
			|| {
				// We do nothing here
			},
			|| {
				let messages = UpwardMessages::<Test>::get();
				assert_eq!(
					messages,
					vec![b"This message will be enough to increase the fee factor".to_vec()]
				);
				// Now the delivery fee factor is decreased, since we are below the threshold
				assert_eq!(UpwardDeliveryFeeFactor::<Test>::get(), FixedU128::from_u32(1));
			},
		);
}

#[test]
fn claim_queue_offset_at_the_bound_is_accepted() {
	let core_info = CoreInfo {
		selector: CoreSelector(0),
		claim_queue_offset: ClaimQueueOffset(1),
		number_of_cores: codec::Compact(1),
	};

	BlockTests::new()
		.with_pre_inherent_digests(vec![DigestItem::PreRuntime(
			CUMULUS_CONSENSUS_ID,
			CumulusDigestItem::CoreInfo(core_info.clone()).encode(),
		)])
		.add_with_post_test(
			1,
			|| {},
			move || {
				// Forwarded to the relay untouched; the relay resolves the core from it.
				assert_eq!(
					UpwardMessages::<Test>::get(),
					vec![
						UMP_SEPARATOR,
						UMPSignal::SelectCore(core_info.selector, core_info.claim_queue_offset)
							.encode(),
					]
				);
			},
		);
}

/// One past the bound is still rejected, so the check has not been widened into a no-op.
#[test]
#[should_panic(expected = "claim_queue_offset 2 exceeds maximum allowed 1")]
fn claim_queue_offset_beyond_the_bound_is_rejected() {
	BlockTests::new()
		.with_pre_inherent_digests(vec![DigestItem::PreRuntime(
			CUMULUS_CONSENSUS_ID,
			CumulusDigestItem::CoreInfo(CoreInfo {
				selector: CoreSelector(0),
				claim_queue_offset: ClaimQueueOffset(2),
				number_of_cores: codec::Compact(1),
			})
			.encode(),
		)])
		.add(1, || {});
}

#[test]
fn max_allowed_claim_queue_offset_matrix() {
	assert_eq!(max_allowed_claim_queue_offset(false, 0), 1);
	assert_eq!(max_allowed_claim_queue_offset(false, 2), 3);
	assert_eq!(max_allowed_claim_queue_offset(true, 0), 2);
	assert_eq!(max_allowed_claim_queue_offset(true, 2), 2);
}

/// The bound in the panic message is the computed one, so this also proves the configured
/// `RelayParentOffset` reaches the assert: "maximum allowed 3" is only reachable via 1 + 2.
#[test]
#[should_panic(expected = "claim_queue_offset 4 exceeds maximum allowed 3")]
fn claim_queue_offset_beyond_the_relay_parent_offset_bound_is_rejected() {
	BlockTests::new()
		.with_relay_parent_offset(2)
		.with_pre_inherent_digests(vec![DigestItem::PreRuntime(
			CUMULUS_CONSENSUS_ID,
			CumulusDigestItem::CoreInfo(CoreInfo {
				selector: CoreSelector(0),
				claim_queue_offset: ClaimQueueOffset(4),
				number_of_cores: codec::Compact(1),
			})
			.encode(),
		)])
		.add(1, || {});
}

#[test]
fn ump_signals_are_sent_correctly() {
	let core_info = CoreInfo {
		selector: CoreSelector(1),
		claim_queue_offset: ClaimQueueOffset(1),
		number_of_cores: codec::Compact(1),
	};

	// Test cases list with the following format:
	// `((expect_approved_peer, expect_select_core), expected_upward_messages)`
	let test_cases = BTreeMap::from([
		((false, false), vec![b"Test".to_vec()]),
		(
			(true, false),
			vec![
				b"Test".to_vec(),
				UMP_SEPARATOR,
				UMPSignal::ApprovedPeer(ApprovedPeerId::try_from(b"12345".to_vec()).unwrap())
					.encode(),
			],
		),
		(
			(false, true),
			vec![
				b"Test".to_vec(),
				UMP_SEPARATOR,
				UMPSignal::SelectCore(core_info.selector, core_info.claim_queue_offset).encode(),
			],
		),
		(
			(true, true),
			vec![
				b"Test".to_vec(),
				UMP_SEPARATOR,
				UMPSignal::SelectCore(core_info.selector, core_info.claim_queue_offset).encode(),
				UMPSignal::ApprovedPeer(ApprovedPeerId::try_from(b"12345".to_vec()).unwrap())
					.encode(),
			],
		),
	]);

	for ((expect_approved_peer, expect_select_core), expected_upward_messages) in test_cases {
		let core_info_digest = CumulusDigestItem::CoreInfo(core_info.clone()).encode();

		BlockTests::new()
			.with_inherent_data(move |_, _, data| {
				if expect_approved_peer {
					data.collator_peer_id =
						Some(ApprovedPeerId::try_from(b"12345".to_vec()).unwrap());
				}
			})
			.add_with_post_test(
				1,
				move || {
					ParachainSystem::send_upward_message(b"Test".to_vec()).unwrap();

					if expect_select_core {
						System::deposit_log(DigestItem::PreRuntime(
							CUMULUS_CONSENSUS_ID,
							core_info_digest.clone(),
						));
					}
				},
				move || {
					assert_eq!(PendingUpwardSignals::<Test>::get(), Vec::<Vec<u8>>::new());
					assert_eq!(UpwardMessages::<Test>::get(), expected_upward_messages);
				},
			);
	}
}
