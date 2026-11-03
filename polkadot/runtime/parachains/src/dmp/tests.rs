// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use super::{
	migration::{self, MigrateV0ToV1, MigrationCursor},
	mock::{
		default_genesis_config, execute_with_try_state, new_test_ext_integrity, pages_in_storage,
		queue_downward_message, register_paras, run_to_block, EXPECTED_LAZY_DELETE_PAGES,
	},
	*,
};
use crate::{
	configuration::ActiveConfig,
	mock::{new_test_ext, Dmp, System, Test},
};
use alloc::collections::BTreeMap;
use codec::Encode;
use frame_support::{
	assert_ok,
	migrations::{SteppedMigration, SteppedMigrationError},
	weights::WeightMeter,
};
use hex_literal::hex;
use sp_arithmetic::traits::Saturating;

#[test]
fn clean_dmp_works() {
	let a = ParaId::from(1312);
	let b = ParaId::from(228);
	let c = ParaId::from(123);

	new_test_ext_integrity(default_genesis_config(), || {
		register_paras(&[a, b, c]);

		// enqueue downward messages to A, B and C.
		queue_downward_message(a, vec![1, 2, 3]).unwrap();
		queue_downward_message(b, vec![4, 5, 6]).unwrap();
		queue_downward_message(c, vec![7, 8, 9]).unwrap();

		let notification = crate::initializer::SessionChangeNotification::default();
		let outgoing_paras = vec![a, b];
		Dmp::initializer_on_new_session(&notification, &outgoing_paras);

		assert!(InboundDownwardQueue::<Test>::len(a).is_none());
		assert!(InboundDownwardQueue::<Test>::len(b).is_none());
		assert!(!InboundDownwardQueue::<Test>::len(c).is_none());
	});
}

#[test]
fn dmq_length_and_head_updated_properly() {
	let a = ParaId::from(1312);
	let b = ParaId::from(228);

	new_test_ext_integrity(default_genesis_config(), || {
		register_paras(&[a, b]);

		assert_eq!(Dmp::dmq_length(a), 0);
		assert_eq!(Dmp::dmq_length(b), 0);

		queue_downward_message(a, vec![1, 2, 3]).unwrap();

		assert_eq!(Dmp::dmq_length(a), 1);
		assert_eq!(Dmp::dmq_length(b), 0);
		assert!(!Dmp::dmq_mqc_head(a).is_zero());
		assert!(Dmp::dmq_mqc_head(b).is_zero());
	});
}

#[test]
fn dmq_fail_if_para_does_not_exist() {
	let a = ParaId::from(1312);

	new_test_ext_integrity(default_genesis_config(), || {
		assert_eq!(Dmp::dmq_length(a), 0);

		assert!(matches!(
			queue_downward_message(a, vec![1, 2, 3]),
			Err(QueueDownwardMessageError::Unroutable)
		));

		assert_eq!(Dmp::dmq_length(a), 0);
		assert!(Dmp::dmq_mqc_head(a).is_zero());
	});
}

#[test]
fn dmp_mqc_head_fixture() {
	let a = ParaId::from(2000);

	new_test_ext_integrity(default_genesis_config(), || {
		register_paras(&[a]);

		run_to_block(2, None);
		assert!(Dmp::dmq_mqc_head(a).is_zero());
		queue_downward_message(a, vec![1, 2, 3]).unwrap();

		run_to_block(3, None);
		queue_downward_message(a, vec![4, 5, 6]).unwrap();

		assert_eq!(
			Dmp::dmq_mqc_head(a),
			hex!["88dc00db8cc9d22aa62b87807705831f164387dfa49f80a8600ed1cbe1704b6b"].into(),
		);
	});
}

#[test]
fn check_processed_downward_messages() {
	let a = ParaId::from(1312);

	new_test_ext_integrity(default_genesis_config(), || {
		register_paras(&[a]);

		let block_number = System::block_number();

		// processed_downward_messages=0 is allowed when the DMQ is empty.
		assert!(Dmp::check_processed_downward_messages(a, block_number, 0).is_ok());

		queue_downward_message(a, vec![1, 2, 3]).unwrap();
		queue_downward_message(a, vec![4, 5, 6]).unwrap();
		queue_downward_message(a, vec![7, 8, 9]).unwrap();

		// 0 doesn't pass if the DMQ has msgs.
		assert!(Dmp::check_processed_downward_messages(a, block_number, 0).is_err());
		// a candidate can consume up to 3 messages
		assert!(Dmp::check_processed_downward_messages(a, block_number, 1).is_ok());
		assert!(Dmp::check_processed_downward_messages(a, block_number, 2).is_ok());
		assert!(Dmp::check_processed_downward_messages(a, block_number, 3).is_ok());
		// there is no 4 messages in the queue
		assert!(Dmp::check_processed_downward_messages(a, block_number, 4).is_err());
	});
}

#[test]
fn check_processed_downward_messages_advancement_rule() {
	let a = ParaId::from(1312);

	new_test_ext_integrity(default_genesis_config(), || {
		register_paras(&[a]);

		let block_number = System::block_number();

		run_to_block(block_number + 1, None);
		let advanced_block_number = System::block_number();

		queue_downward_message(a, vec![1, 2, 3]).unwrap();
		queue_downward_message(a, vec![4, 5, 6]).unwrap();

		// The queue was empty at genesis, 0 is OK despite it being non-empty in the further block.
		assert!(Dmp::check_processed_downward_messages(a, block_number, 0).is_ok());
		// For the advanced block number, however, the rule is broken in case of 0.
		assert!(Dmp::check_processed_downward_messages(a, advanced_block_number, 0).is_err());
	});
}

#[test]
fn dmq_pruning() {
	let a = ParaId::from(1312);

	new_test_ext_integrity(default_genesis_config(), || {
		register_paras(&[a]);

		assert_eq!(Dmp::dmq_length(a), 0);

		queue_downward_message(a, vec![1, 2, 3]).unwrap();
		queue_downward_message(a, vec![4, 5, 6]).unwrap();
		queue_downward_message(a, vec![7, 8, 9]).unwrap();
		assert_eq!(Dmp::dmq_length(a), 3);

		// pruning 0 elements shouldn't change anything.
		Dmp::prune_dmq(a, 0);
		assert_eq!(Dmp::dmq_length(a), 3);

		Dmp::prune_dmq(a, 2);
		assert_eq!(Dmp::dmq_length(a), 1);
	});
}

#[test]
fn queue_downward_message_critical() {
	let a = ParaId::from(1312);

	let mut genesis = default_genesis_config();
	genesis.configuration.config.max_downward_message_size = 7;

	new_test_ext_integrity(genesis, || {
		register_paras(&[a]);

		let smol = [0; 3].to_vec();
		let big = [0; 8].to_vec();

		// still within limits
		assert_eq!(smol.encode().len(), 4);
		assert!(queue_downward_message(a, smol).is_ok());

		// that's too big
		assert_eq!(big.encode().len(), 9);
		assert!(queue_downward_message(a, big).is_err());
	});
}

#[test]
fn verify_dmq_mqc_head_is_externally_accessible() {
	use hex_literal::hex;
	use polkadot_primitives::well_known_keys;

	let a = ParaId::from(2020);

	new_test_ext_integrity(default_genesis_config(), || {
		register_paras(&[a]);

		let head = sp_io::storage::get(&well_known_keys::dmq_mqc_head(a));
		assert_eq!(head, None);

		queue_downward_message(a, vec![1, 2, 3]).unwrap();

		let head = sp_io::storage::get(&well_known_keys::dmq_mqc_head(a));
		assert_eq!(
			head,
			Some(
				hex!["434f8579a2297dfea851bf6be33093c83a78b655a53ae141a7894494c0010589"]
					.to_vec()
					.into()
			)
		);
	});
}

#[test]
fn verify_fee_increase_and_decrease() {
	let a = ParaId::from(123);

	let mut genesis = default_genesis_config();
	genesis.configuration.config.max_downward_message_size = 16777216;
	new_test_ext_integrity(genesis, || {
		register_paras(&[a]);

		let initial = Pallet::<Test>::MIN_FEE_FACTOR;
		assert_eq!(DeliveryFeeFactor::<Test>::get(a), initial);

		// Under fee limit
		queue_downward_message(a, vec![1]).unwrap();
		assert_eq!(DeliveryFeeFactor::<Test>::get(a), initial);

		// Limit reached so fee is increased
		queue_downward_message(a, vec![1]).unwrap();
		let result = Pallet::<Test>::MIN_FEE_FACTOR.saturating_mul(Dmp::EXPONENTIAL_FEE_BASE);
		assert_eq!(DeliveryFeeFactor::<Test>::get(a), result);

		Dmp::prune_dmq(a, 1);
		assert_eq!(DeliveryFeeFactor::<Test>::get(a), initial);

		// 10 Kb message adds additional 0.001 per KB fee factor
		let big_message = [0; 10240].to_vec();
		let msg_len_in_kb = big_message.len().saturating_div(1024) as u32;
		let result = initial.saturating_mul(
			Dmp::EXPONENTIAL_FEE_BASE +
				Dmp::MESSAGE_SIZE_FEE_BASE.saturating_mul(FixedU128::from_u32(msg_len_in_kb)),
		);
		queue_downward_message(a, big_message).unwrap();
		assert_eq!(DeliveryFeeFactor::<Test>::get(a), result);

		queue_downward_message(a, vec![1]).unwrap();
		let result = result.saturating_mul(Dmp::EXPONENTIAL_FEE_BASE);
		assert_eq!(DeliveryFeeFactor::<Test>::get(a), result);

		Dmp::prune_dmq(a, 3);
		let result = result / Dmp::EXPONENTIAL_FEE_BASE;
		assert_eq!(DeliveryFeeFactor::<Test>::get(a), result);
		assert_eq!(Dmp::dmq_length(a), 0);

		// Messages under limit will keep decreasing fee factor until base fee factor is reached
		queue_downward_message(a, vec![1]).unwrap();
		Dmp::prune_dmq(a, 1);
		queue_downward_message(a, vec![1]).unwrap();
		Dmp::prune_dmq(a, 1);
		assert_eq!(DeliveryFeeFactor::<Test>::get(a), initial);
	});
}

#[test]
fn verify_fee_factor_reaches_high_value() {
	let a = ParaId::from(123);
	let mut genesis = default_genesis_config();
	genesis.configuration.config.max_downward_message_size = 51200;
	new_test_ext_integrity(genesis, || {
		register_paras(&[a]);

		let max_messages =
			Dmp::dmq_max_length(ActiveConfig::<Test>::get().max_downward_message_size);
		let mut total_fee_factor = FixedU128::from_float(1.0);
		for _ in 1..max_messages {
			assert_ok!(queue_downward_message(a, vec![]));
			total_fee_factor = total_fee_factor + (DeliveryFeeFactor::<Test>::get(a));
		}
		assert!(total_fee_factor > FixedU128::from_u32(100_000_000));
	});
}

#[test]
fn iq_meta_returns_none_for_unknown_para() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(1);
		assert!(InboundDownwardQueue::<Test>::meta(a).is_none());
	});
}

#[test]
fn iq_len_is_none_for_unknown_para() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(1);
		assert_eq!(InboundDownwardQueue::<Test>::len(a), None);
	});
}

#[test]
fn iq_len_tracks_pushes() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(7);
		for i in 1u64..=5 {
			InboundDownwardQueue::<Test>::push_back(a, vec![i as u8]).unwrap();
			assert_eq!(InboundDownwardQueue::<Test>::len(a), Some(i));
		}
	});
}

#[test]
fn iq_push_back_returns_inbound_with_current_block_and_msg() {
	let mut ext = new_test_ext(default_genesis_config());
	execute_with_try_state(&mut ext, || {
		let a = ParaId::from(8);
		System::set_block_number(42);
		let msg: DownwardMessage = vec![9, 8, 7];

		let inbound = InboundDownwardQueue::<Test>::push_back(a, msg.clone()).unwrap();
		assert_eq!(inbound.sent_at, 42);
		assert_eq!(inbound.msg, msg);
	});
}

#[test]
fn iq_push_back_writes_page_at_first_free_then_advances() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(9);

		// Initially nothing.
		assert_eq!(pages_in_storage(a), Vec::<PageIndex>::new());

		InboundDownwardQueue::<Test>::push_back(a, vec![1]).unwrap();
		let meta = InboundDownwardQueue::<Test>::meta(a).unwrap();
		assert_eq!(meta.first_full, 0);
		assert_eq!(meta.first_free, 1);
		assert_eq!(pages_in_storage(a), vec![0]);

		InboundDownwardQueue::<Test>::push_back(a, vec![2]).unwrap();
		let meta = InboundDownwardQueue::<Test>::meta(a).unwrap();
		assert_eq!(meta.first_full, 0);
		assert_eq!(meta.first_free, 2);
		assert_eq!(pages_in_storage(a), vec![0, 1]);
	});
}

#[test]
fn iq_pop_front_returns_none_for_unknown_para() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(11);
		assert!(InboundDownwardQueue::<Test>::pop_front(a).is_none());
		// No state should be created by a pop on an unknown queue.
		assert!(InboundDownwardQueue::<Test>::meta(a).is_none());
	});
}

#[test]
fn iq_pop_front_is_fifo() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(12);
		System::set_block_number(1);

		InboundDownwardQueue::<Test>::push_back(a, vec![1]).unwrap();
		InboundDownwardQueue::<Test>::push_back(a, vec![2]).unwrap();
		InboundDownwardQueue::<Test>::push_back(a, vec![3]).unwrap();

		let first = InboundDownwardQueue::<Test>::pop_front(a).unwrap();
		assert_eq!(first.msg, vec![1]);
		let second = InboundDownwardQueue::<Test>::pop_front(a).unwrap();
		assert_eq!(second.msg, vec![2]);
		let third = InboundDownwardQueue::<Test>::pop_front(a).unwrap();
		assert_eq!(third.msg, vec![3]);

		assert_eq!(InboundDownwardQueue::<Test>::len(a), Some(0));
	});
}

#[test]
fn iq_pop_front_removes_storage_page_and_advances_first_full() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(13);
		InboundDownwardQueue::<Test>::push_back(a, vec![1]).unwrap();
		InboundDownwardQueue::<Test>::push_back(a, vec![2]).unwrap();

		assert_eq!(pages_in_storage(a), vec![0, 1]);

		let _ = InboundDownwardQueue::<Test>::pop_front(a).unwrap();
		let meta = InboundDownwardQueue::<Test>::meta(a).unwrap();
		assert_eq!(meta.first_full, 1);
		assert_eq!(meta.first_free, 2);
		// Page 0 must no longer be in storage.
		assert_eq!(pages_in_storage(a), vec![1]);

		let _ = InboundDownwardQueue::<Test>::pop_front(a).unwrap();
		let meta = InboundDownwardQueue::<Test>::meta(a).unwrap();
		assert_eq!(meta.first_full, 2);
		assert_eq!(meta.first_free, 2);
		assert_eq!(pages_in_storage(a), Vec::<PageIndex>::new());
	});
}

#[test]
fn iq_pop_front_after_drain_returns_none() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(14);
		InboundDownwardQueue::<Test>::push_back(a, vec![1]).unwrap();
		assert!(InboundDownwardQueue::<Test>::pop_front(a).is_some());
		assert!(InboundDownwardQueue::<Test>::pop_front(a).is_none());
	});
}

#[test]
fn iq_peek_front_returns_none_for_unknown_para() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(15);
		assert!(InboundDownwardQueue::<Test>::peek_front(a).is_none());
	});
}

#[test]
fn iq_peek_front_does_not_modify_state() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(16);
		InboundDownwardQueue::<Test>::push_back(a, vec![1]).unwrap();
		InboundDownwardQueue::<Test>::push_back(a, vec![2]).unwrap();

		let meta_before = InboundDownwardQueue::<Test>::meta(a).unwrap();
		let pages_before = pages_in_storage(a);

		let peek1 = InboundDownwardQueue::<Test>::peek_front(a).unwrap();
		let peek2 = InboundDownwardQueue::<Test>::peek_front(a).unwrap();

		assert_eq!(peek1, peek2);
		assert_eq!(peek1.msg, vec![1]);

		assert_eq!(InboundDownwardQueue::<Test>::meta(a).unwrap(), meta_before);
		assert_eq!(pages_in_storage(a), pages_before);
	});
}

#[test]
fn iq_peek_front_matches_pop_front() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(17);
		InboundDownwardQueue::<Test>::push_back(a, vec![10]).unwrap();
		InboundDownwardQueue::<Test>::push_back(a, vec![20]).unwrap();

		let peeked = InboundDownwardQueue::<Test>::peek_front(a).unwrap();
		let popped = InboundDownwardQueue::<Test>::pop_front(a).unwrap();
		assert_eq!(peeked, popped);
	});
}

#[test]
fn iq_peek_front_after_drain_is_none() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(18);
		InboundDownwardQueue::<Test>::push_back(a, vec![1]).unwrap();
		let _ = InboundDownwardQueue::<Test>::pop_front(a);
		// Meta still exists but no pages — peek_front must return None, not panic.
		assert!(InboundDownwardQueue::<Test>::peek_front(a).is_none());
	});
}

#[test]
fn iq_drop_front_n_returns_none_for_unknown_para() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(19);
		assert_eq!(InboundDownwardQueue::<Test>::drop_front_n(a, 3), None);
	});
}

#[test]
fn iq_drop_front_n_zero_is_noop() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(20);
		InboundDownwardQueue::<Test>::push_back(a, vec![1]).unwrap();
		InboundDownwardQueue::<Test>::push_back(a, vec![2]).unwrap();

		let meta_before = InboundDownwardQueue::<Test>::meta(a).unwrap();
		let pages_before = pages_in_storage(a);

		assert_eq!(InboundDownwardQueue::<Test>::drop_front_n(a, 0), Some(0));

		assert_eq!(InboundDownwardQueue::<Test>::meta(a).unwrap(), meta_before);
		assert_eq!(pages_in_storage(a), pages_before);
	});
}

#[test]
fn iq_drop_front_n_drops_correct_pages_and_advances_meta() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(21);
		InboundDownwardQueue::<Test>::push_back(a, vec![1]).unwrap();
		InboundDownwardQueue::<Test>::push_back(a, vec![2]).unwrap();
		InboundDownwardQueue::<Test>::push_back(a, vec![3]).unwrap();
		assert_eq!(pages_in_storage(a), vec![0, 1, 2]);

		// Drop the first two: pages 0 and 1 should be removed, page 2 must remain.
		let dropped = InboundDownwardQueue::<Test>::drop_front_n(a, 2);
		assert_eq!(dropped, Some(2));

		let meta = InboundDownwardQueue::<Test>::meta(a).unwrap();
		assert_eq!(meta.first_full, 2);
		assert_eq!(meta.first_free, 3);

		// The surviving message must still be reachable: bug in `drop_front_n`
		// that removes the wrong index would manifest here.
		assert_eq!(pages_in_storage(a), vec![2]);
		let front = InboundDownwardQueue::<Test>::peek_front(a).unwrap();
		assert_eq!(front.msg, vec![3]);
	});
}

#[test]
fn iq_drop_front_n_clamps_to_len() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(22);
		InboundDownwardQueue::<Test>::push_back(a, vec![1]).unwrap();
		InboundDownwardQueue::<Test>::push_back(a, vec![2]).unwrap();

		// Asking for more than available drops only what's there and returns the
		// real number of dropped messages.
		let dropped = InboundDownwardQueue::<Test>::drop_front_n(a, 100);
		assert_eq!(dropped, Some(2));

		let meta = InboundDownwardQueue::<Test>::meta(a).unwrap();
		assert_eq!(meta.first_full, 2);
		assert_eq!(meta.first_free, 2);
		assert_eq!(InboundDownwardQueue::<Test>::len(a), Some(0));
		assert_eq!(pages_in_storage(a), Vec::<PageIndex>::new());
	});
}

#[test]
fn iq_drop_front_n_does_not_underflow_first_full() {
	// Specifically guards against any logic that could push `first_full` past
	// `first_free`.
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(23);
		InboundDownwardQueue::<Test>::push_back(a, vec![1]).unwrap();

		let _ = InboundDownwardQueue::<Test>::drop_front_n(a, u64::MAX);

		let meta = InboundDownwardQueue::<Test>::meta(a).unwrap();
		assert!(meta.first_full <= meta.first_free);
		assert_eq!(meta.first_full, meta.first_free);
	});
}

#[test]
fn iq_drop_front_n_then_push_continues_at_first_free() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(24);
		for i in 0..3 {
			InboundDownwardQueue::<Test>::push_back(a, vec![i]).unwrap();
		}
		InboundDownwardQueue::<Test>::drop_front_n(a, 2).unwrap();

		// New pushes must continue at the existing first_free, not reset.
		let inbound = InboundDownwardQueue::<Test>::push_back(a, vec![99]).unwrap();
		assert_eq!(inbound.msg, vec![99]);
		let meta = InboundDownwardQueue::<Test>::meta(a).unwrap();
		assert_eq!(meta.first_full, 2);
		assert_eq!(meta.first_free, 4);

		// FIFO order is preserved across drop+push.
		let m1 = InboundDownwardQueue::<Test>::pop_front(a).unwrap();
		let m2 = InboundDownwardQueue::<Test>::pop_front(a).unwrap();
		assert_eq!(m1.msg, vec![2]);
		assert_eq!(m2.msg, vec![99]);
	});
}

#[test]
fn iq_delete_all_clears_meta_and_pages_for_small_queue() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(30);
		for i in 0..5u8 {
			InboundDownwardQueue::<Test>::push_back(a, vec![i]).unwrap();
		}

		InboundDownwardQueue::<Test>::delete_all(a);

		assert!(InboundDownwardQueue::<Test>::meta(a).is_none());
		assert_eq!(InboundDownwardQueue::<Test>::len(a), None);
		assert_eq!(pages_in_storage(a), Vec::<PageIndex>::new());
		assert!(!DownwardMessageQueueLazyDelete::<Test>::contains_key(a));
	});
}

#[test]
fn iq_delete_all_uses_lazy_delete_for_large_queue() {
	let a = ParaId::from(31);
	let total = EXPECTED_LAZY_DELETE_PAGES + 5;

	let mut ext = new_test_ext(default_genesis_config());
	execute_with_try_state(&mut ext, || {
		for i in 0..total {
			InboundDownwardQueue::<Test>::push_back(a, vec![i as u8]).unwrap();
		}
		assert_eq!(pages_in_storage(a).len() as u64, total);
	});
	// Push pages into the backend so `clear_prefix` respects its limit.
	ext.commit_all().unwrap();

	execute_with_try_state(&mut ext, || {
		InboundDownwardQueue::<Test>::delete_all(a);

		// Meta is gone immediately.
		assert!(InboundDownwardQueue::<Test>::meta(a).is_none());
		// Not all pages were removed in this single call.
		assert!(!pages_in_storage(a).is_empty());
		// And the para is queued for lazy deletion.
		assert!(DownwardMessageQueueLazyDelete::<Test>::contains_key(a));
	});
}

#[test]
fn iq_delete_all_clears_immediately_when_under_limit() {
	let a = ParaId::from(36);
	let total = EXPECTED_LAZY_DELETE_PAGES - 1;

	let mut ext = new_test_ext(default_genesis_config());
	execute_with_try_state(&mut ext, || {
		for i in 0..total {
			InboundDownwardQueue::<Test>::push_back(a, vec![i as u8]).unwrap();
		}
	});
	ext.commit_all().unwrap();

	execute_with_try_state(&mut ext, || {
		InboundDownwardQueue::<Test>::delete_all(a);
		assert!(InboundDownwardQueue::<Test>::meta(a).is_none());
		assert_eq!(pages_in_storage(a), Vec::<PageIndex>::new());
		assert!(!DownwardMessageQueueLazyDelete::<Test>::contains_key(a));
	});
}

#[test]
fn iq_delete_all_on_unknown_para_is_noop() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(32);
		InboundDownwardQueue::<Test>::delete_all(a);
		assert!(InboundDownwardQueue::<Test>::meta(a).is_none());
		assert_eq!(pages_in_storage(a), Vec::<PageIndex>::new());
		assert!(!DownwardMessageQueueLazyDelete::<Test>::contains_key(a));
	});
}

#[test]
fn iq_lazy_delete_some_clears_remaining_pages_eventually() {
	let a = ParaId::from(33);
	let total = EXPECTED_LAZY_DELETE_PAGES * 2 + 5;

	let mut ext = new_test_ext(default_genesis_config());
	execute_with_try_state(&mut ext, || {
		for i in 0..total {
			InboundDownwardQueue::<Test>::push_back(a, vec![i as u8]).unwrap();
		}
	});
	ext.commit_all().unwrap();

	execute_with_try_state(&mut ext, || {
		InboundDownwardQueue::<Test>::delete_all(a);
		assert!(DownwardMessageQueueLazyDelete::<Test>::contains_key(a));
	});
	ext.commit_all().unwrap();

	let mut iterations = 0u32;
	loop {
		let still_pending = execute_with_try_state(&mut ext, || {
			DownwardMessageQueueLazyDelete::<Test>::contains_key(a)
		});
		if !still_pending {
			break;
		}
		execute_with_try_state(&mut ext, || {
			let mut wm = WeightMeter::new();
			InboundDownwardQueue::<Test>::lazy_delete_some(&mut wm);
		});
		ext.commit_all().unwrap();
		iterations += 1;
		assert!(
			(iterations as u64) < total + 10,
			"lazy_delete_some should make progress (iter {} > pages {})",
			iterations,
			total,
		);
	}

	execute_with_try_state(&mut ext, || {
		assert_eq!(pages_in_storage(a), Vec::<PageIndex>::new());
		assert!(InboundDownwardQueue::<Test>::meta(a).is_none());
	});
}

#[test]
fn iq_lazy_delete_some_no_op_when_nothing_pending() {
	new_test_ext_integrity(default_genesis_config(), || {
		let mut wm = WeightMeter::new();
		InboundDownwardQueue::<Test>::lazy_delete_some(&mut wm);
		assert_eq!(DownwardMessageQueueLazyDelete::<Test>::iter_keys().count(), 0);
	});
}

#[test]
fn iq_peek_all_empty_for_unknown_para() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(40);
		assert_eq!(InboundDownwardQueue::<Test>::peek_all_do_not_call_in_consensus(a), Vec::new());
	});
}

#[test]
fn iq_peek_all_returns_messages_in_fifo_order() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(41);
		System::set_block_number(7);
		let msgs: Vec<DownwardMessage> = vec![vec![1], vec![2], vec![3], vec![4]];
		for m in &msgs {
			InboundDownwardQueue::<Test>::push_back(a, m.clone()).unwrap();
		}
		let all = InboundDownwardQueue::<Test>::peek_all_do_not_call_in_consensus(a);
		let got_msgs: Vec<_> = all.iter().map(|m| m.msg.clone()).collect();
		assert_eq!(got_msgs, msgs);
		for m in &all {
			assert_eq!(m.sent_at, 7);
		}
	});
}

#[test]
fn iq_peek_all_after_partial_drop() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(42);
		for i in 0..4u8 {
			InboundDownwardQueue::<Test>::push_back(a, vec![i]).unwrap();
		}
		InboundDownwardQueue::<Test>::drop_front_n(a, 2).unwrap();

		let all = InboundDownwardQueue::<Test>::peek_all_do_not_call_in_consensus(a);
		let got: Vec<_> = all.into_iter().map(|m| m.msg).collect();
		assert_eq!(got, vec![vec![2], vec![3]]);
	});
}

#[test]
fn iq_paras_are_isolated() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(50);
		let b = ParaId::from(51);

		InboundDownwardQueue::<Test>::push_back(a, vec![1]).unwrap();
		InboundDownwardQueue::<Test>::push_back(a, vec![2]).unwrap();
		InboundDownwardQueue::<Test>::push_back(b, vec![100]).unwrap();

		assert_eq!(InboundDownwardQueue::<Test>::len(a), Some(2));
		assert_eq!(InboundDownwardQueue::<Test>::len(b), Some(1));

		// Operating on `a` does not touch `b`.
		InboundDownwardQueue::<Test>::drop_front_n(a, 2).unwrap();
		assert_eq!(InboundDownwardQueue::<Test>::len(a), Some(0));
		assert_eq!(InboundDownwardQueue::<Test>::len(b), Some(1));
		assert_eq!(InboundDownwardQueue::<Test>::peek_front(b).unwrap().msg, vec![100]);

		// `delete_all(a)` must not affect `b`.
		InboundDownwardQueue::<Test>::delete_all(a);
		assert_eq!(InboundDownwardQueue::<Test>::len(b), Some(1));
		assert_eq!(pages_in_storage(b), vec![0]);
	});
}

#[test]
fn iq_full_lifecycle_push_pop_redrain() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(60);

		// Round 1.
		for i in 0..3u8 {
			InboundDownwardQueue::<Test>::push_back(a, vec![i]).unwrap();
		}
		for _ in 0..3 {
			assert!(InboundDownwardQueue::<Test>::pop_front(a).is_some());
		}
		assert_eq!(InboundDownwardQueue::<Test>::len(a), Some(0));
		assert_eq!(pages_in_storage(a), Vec::<PageIndex>::new());

		// Round 2: continues from existing meta — first_free does NOT reset.
		InboundDownwardQueue::<Test>::push_back(a, vec![42]).unwrap();
		let meta = InboundDownwardQueue::<Test>::meta(a).unwrap();
		assert_eq!(meta.first_full, 3);
		assert_eq!(meta.first_free, 4);
		assert_eq!(pages_in_storage(a), vec![3]);
	});
}

#[test]
fn iq_push_back_at_first_free_max_returns_err_without_orphaning_page() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(61);

		// Plant a meta whose `first_free` is already at the maximum so the next
		// `checked_add` overflows.
		DownwardMessageQueueMeta::<Test>::insert(
			a,
			InboundDownwardQueueMeta { first_full: 0, first_free: u64::MAX },
		);

		let res = InboundDownwardQueue::<Test>::push_back(a, vec![1]);
		assert!(res.is_err(), "push_back must error on first_free overflow");

		// Meta must remain unchanged (so callers can rely on idempotency).
		let meta = InboundDownwardQueue::<Test>::meta(a).unwrap();
		assert_eq!(meta.first_free, u64::MAX);
		assert_eq!(meta.first_full, 0);

		// No orphan page may be left in storage at the rejected slot.
		assert!(
			!DownwardMessageQueuePages::<Test>::contains_key(a, u64::MAX),
			"failed push must not leave a page in storage",
		);
	});
}

#[test]
fn iq_pending_lazy_delete_does_not_wipe_new_messages_on_reuse() {
	let a = ParaId::from(62);
	let total = EXPECTED_LAZY_DELETE_PAGES + 5;

	let mut ext = new_test_ext(default_genesis_config());
	execute_with_try_state(&mut ext, || {
		for i in 0..total {
			InboundDownwardQueue::<Test>::push_back(a, vec![i as u8]).unwrap();
		}
	});
	ext.commit_all().unwrap();

	execute_with_try_state(&mut ext, || {
		InboundDownwardQueue::<Test>::delete_all(a);
		assert!(DownwardMessageQueueLazyDelete::<Test>::contains_key(a));
	});
	ext.commit_all().unwrap();

	// Re-use the same ParaId: push a fresh message.
	execute_with_try_state(&mut ext, || {
		InboundDownwardQueue::<Test>::push_back(a, vec![0xAA]).unwrap();
		assert_eq!(InboundDownwardQueue::<Test>::len(a), Some(1));
	});
	ext.commit_all().unwrap();

	// Now drain the lazy-delete queue to completion.
	let mut iterations = 0u32;
	loop {
		let still_pending = execute_with_try_state(&mut ext, || {
			DownwardMessageQueueLazyDelete::<Test>::contains_key(a)
		});
		if !still_pending {
			break;
		}
		execute_with_try_state(&mut ext, || {
			let mut wm = WeightMeter::new();
			InboundDownwardQueue::<Test>::lazy_delete_some(&mut wm);
		});
		ext.commit_all().unwrap();
		iterations += 1;
		assert!((iterations as u64) < total + 10, "lazy_delete_some should make progress",);
	}

	// The freshly-pushed message must still be readable.
	execute_with_try_state(&mut ext, || {
		let front = InboundDownwardQueue::<Test>::peek_front(a);
		assert_eq!(
			front.map(|m| m.msg),
			Some(vec![0xAA]),
			"new message must survive a pending lazy-delete cycle",
		);
		assert_eq!(InboundDownwardQueue::<Test>::len(a), Some(1));
	});
}

#[test]
fn iq_lazy_delete_finishes_cleaning_old_pages_after_reuse() {
	let a = ParaId::from(63);
	let total: u64 = EXPECTED_LAZY_DELETE_PAGES + 20;

	let mut ext = new_test_ext(default_genesis_config());
	execute_with_try_state(&mut ext, || {
		for i in 0..total {
			InboundDownwardQueue::<Test>::push_back(a, vec![i as u8]).unwrap();
		}
	});
	ext.commit_all().unwrap();

	execute_with_try_state(&mut ext, || {
		InboundDownwardQueue::<Test>::delete_all(a);
		assert!(DownwardMessageQueueLazyDelete::<Test>::contains_key(a));
		// At this point some old pages (in [0, total)) survived clear_prefix.
		assert!(!pages_in_storage(a).is_empty());
	});
	ext.commit_all().unwrap();

	// Re-onboard: push several new messages. Their indices start at `total`
	// (from `new_meta` consulting the lazy-delete range) — outside the old
	// `[0, total)` range.
	let new_msgs: u64 = 5;
	execute_with_try_state(&mut ext, || {
		for _ in 0..new_msgs {
			InboundDownwardQueue::<Test>::push_back(a, vec![0xAA]).unwrap();
		}
		assert_eq!(InboundDownwardQueue::<Test>::len(a), Some(new_msgs));
	});
	ext.commit_all().unwrap();

	// Run lazy delete to completion.
	let mut iterations = 0u32;
	loop {
		let still_pending = execute_with_try_state(&mut ext, || {
			DownwardMessageQueueLazyDelete::<Test>::contains_key(a)
		});
		if !still_pending {
			break;
		}
		execute_with_try_state(&mut ext, || {
			let mut wm = WeightMeter::new();
			InboundDownwardQueue::<Test>::lazy_delete_some(&mut wm);
		});
		ext.commit_all().unwrap();
		iterations += 1;
		assert!((iterations as u64) < total + 100, "lazy_delete_some should make progress",);
	}

	// Check that no old pages remain. New pages must still be there.
	execute_with_try_state(&mut ext, || {
		let pages = pages_in_storage(a);
		for &p in &pages {
			assert!(
				p >= total,
				"page {} is in the OLD range [0, {}), should have been cleaned",
				p,
				total,
			);
		}
		assert_eq!(InboundDownwardQueue::<Test>::len(a), Some(new_msgs));
	});
}

#[test]
fn iq_second_delete_all_does_not_drop_first_lazy_delete_range() {
	let a = ParaId::from(64);

	let first_batch: u64 = EXPECTED_LAZY_DELETE_PAGES * 3;
	let second_batch: u64 = EXPECTED_LAZY_DELETE_PAGES * 2;

	let mut ext = new_test_ext(default_genesis_config());

	// Round 1: fill, delete_all (spill).
	execute_with_try_state(&mut ext, || {
		for i in 0..first_batch {
			InboundDownwardQueue::<Test>::push_back(a, vec![i as u8]).unwrap();
		}
	});
	ext.commit_all().unwrap();

	execute_with_try_state(&mut ext, || {
		InboundDownwardQueue::<Test>::delete_all(a);
		assert!(DownwardMessageQueueLazyDelete::<Test>::contains_key(a));
		// Many old pages survived the first spill.
		assert!(pages_in_storage(a).len() as u64 >= first_batch - EXPECTED_LAZY_DELETE_PAGES,);
	});
	ext.commit_all().unwrap();

	// Round 2: re-onboard the same para and queue more.
	execute_with_try_state(&mut ext, || {
		for _ in 0..second_batch {
			InboundDownwardQueue::<Test>::push_back(a, vec![0xAA]).unwrap();
		}
	});
	ext.commit_all().unwrap();

	execute_with_try_state(&mut ext, || {
		InboundDownwardQueue::<Test>::delete_all(a);
		assert!(DownwardMessageQueueLazyDelete::<Test>::contains_key(a));
	});
	ext.commit_all().unwrap();

	// Drain lazy delete to completion (generous bound: it only removes
	// LAZY_DELETE_MAX_PAGES per call and we may have a couple of full ranges).
	let bound = (first_batch + second_batch) * 2;
	let mut iterations = 0u64;
	loop {
		let pending = execute_with_try_state(&mut ext, || {
			DownwardMessageQueueLazyDelete::<Test>::contains_key(a)
		});
		if !pending {
			break;
		}
		execute_with_try_state(&mut ext, || {
			let mut wm = WeightMeter::new();
			InboundDownwardQueue::<Test>::lazy_delete_some(&mut wm);
		});
		ext.commit_all().unwrap();
		iterations += 1;
		assert!(iterations < bound, "lazy_delete_some should make progress");
	}

	// No pages — old or new — must remain. Anything left here is orphaned
	// because the second delete_all's range overwrote the first one's.
	execute_with_try_state(&mut ext, || {
		let pages = pages_in_storage(a);
		assert!(pages.is_empty(), "orphan pages remain after both delete_all cycles: {:?}", pages,);
	});
}

#[test]
fn iq_delete_all_lazy_range_starts_at_meta_first_full_when_no_prior_entry() {
	let a = ParaId::from(65);
	// Need `total - popped > LAZY_DELETE_MAX_PAGES` so the spill branch fires
	// and `popped > 0` so `meta.first_full` differs from 0.
	let total: u64 = EXPECTED_LAZY_DELETE_PAGES * 3;
	let popped: u64 = 2;

	let mut ext = new_test_ext(default_genesis_config());
	execute_with_try_state(&mut ext, || {
		for i in 0..total {
			InboundDownwardQueue::<Test>::push_back(a, vec![i as u8]).unwrap();
		}
		assert_eq!(InboundDownwardQueue::<Test>::drop_front_n(a, popped), Some(popped));
		// Sanity: meta now starts above 0.
		let meta = InboundDownwardQueue::<Test>::meta(a).unwrap();
		assert_eq!(meta.first_full, popped);
		assert_eq!(meta.first_free, total);
		// Sanity: no prior LazyDelete entry.
		assert!(!DownwardMessageQueueLazyDelete::<Test>::contains_key(a));
	});
	ext.commit_all().unwrap();

	execute_with_try_state(&mut ext, || {
		InboundDownwardQueue::<Test>::delete_all(a);

		let (first, last) = DownwardMessageQueueLazyDelete::<Test>::get(a)
			.expect("delete_all must spill — we have more pages than the chunk size");
		assert_eq!(
			first, popped,
			"LazyDelete lower bound should be meta.first_full ({}), not 0",
			popped,
		);
		assert_eq!(last, total);
	});
}

#[test]
fn iq_integrity_test_passes_after_full_drain() {
	let a = ParaId::from(66);
	new_test_ext_integrity(default_genesis_config(), || {
		InboundDownwardQueue::<Test>::push_back(a, vec![1]).unwrap();
		assert!(InboundDownwardQueue::<Test>::pop_front(a).is_some());

		// meta is still present, no pages exist.
		assert!(InboundDownwardQueue::<Test>::meta(a).is_some());
		assert_eq!(pages_in_storage(a), Vec::<PageIndex>::new());
	});
}

#[test]
fn iq_integrity_test_passes_during_reonboard_with_pending_lazy_delete() {
	let a = ParaId::from(67);
	let total: u64 = EXPECTED_LAZY_DELETE_PAGES + 5;

	let mut ext = new_test_ext(default_genesis_config());
	execute_with_try_state(&mut ext, || {
		for i in 0..total {
			InboundDownwardQueue::<Test>::push_back(a, vec![i as u8]).unwrap();
		}
	});
	ext.commit_all().unwrap();

	execute_with_try_state(&mut ext, || {
		InboundDownwardQueue::<Test>::delete_all(a);
		// At this point: lazy delete pending, no meta. integrity_test must hold.
	});
	ext.commit_all().unwrap();

	execute_with_try_state(&mut ext, || {
		// Re-onboard: now meta exists AND lazy delete still pending AND pages
		// exist for the para.
		InboundDownwardQueue::<Test>::push_back(a, vec![0xAA]).unwrap();
		assert!(DownwardMessageQueueLazyDelete::<Test>::contains_key(a));
		assert!(InboundDownwardQueue::<Test>::meta(a).is_some());
		assert!(!pages_in_storage(a).is_empty());
	});
}

#[test]
fn dmp_dmq_contents_returns_in_fifo_order() {
	let a = ParaId::from(70);
	new_test_ext_integrity(default_genesis_config(), || {
		register_paras(&[a]);
		System::set_block_number(5);
		queue_downward_message(a, vec![1]).unwrap();
		queue_downward_message(a, vec![2]).unwrap();
		queue_downward_message(a, vec![3]).unwrap();

		let contents = Dmp::dmq_contents_do_not_call_in_consensus(a);
		let msgs: Vec<_> = contents.iter().map(|m| m.msg.clone()).collect();
		assert_eq!(msgs, vec![vec![1], vec![2], vec![3]]);
		for m in &contents {
			assert_eq!(m.sent_at, 5);
		}
	});
}

#[test]
fn dmp_prune_dmq_keeps_correct_messages() {
	let a = ParaId::from(71);
	new_test_ext_integrity(default_genesis_config(), || {
		register_paras(&[a]);
		queue_downward_message(a, vec![1]).unwrap();
		queue_downward_message(a, vec![2]).unwrap();
		queue_downward_message(a, vec![3]).unwrap();

		Dmp::prune_dmq(a, 2);
		assert_eq!(Dmp::dmq_length(a), 1);

		let contents = Dmp::dmq_contents_do_not_call_in_consensus(a);
		let msgs: Vec<_> = contents.iter().map(|m| m.msg.clone()).collect();
		assert_eq!(msgs, vec![vec![3]]);
	});
}

#[test]
fn dmp_prune_dmq_more_than_available_does_not_underflow() {
	let a = ParaId::from(72);
	new_test_ext_integrity(default_genesis_config(), || {
		register_paras(&[a]);
		queue_downward_message(a, vec![1]).unwrap();
		queue_downward_message(a, vec![2]).unwrap();

		// More than queued: all should be pruned and length should clamp at 0.
		Dmp::prune_dmq(a, 99);
		assert_eq!(Dmp::dmq_length(a), 0);
		assert_eq!(Dmp::dmq_contents_do_not_call_in_consensus(a), Vec::new());
	});
}

#[test]
fn dmp_prune_zero_keeps_all_messages() {
	let a = ParaId::from(73);
	new_test_ext_integrity(default_genesis_config(), || {
		register_paras(&[a]);
		queue_downward_message(a, vec![1]).unwrap();
		queue_downward_message(a, vec![2]).unwrap();

		Dmp::prune_dmq(a, 0);
		assert_eq!(Dmp::dmq_length(a), 2);
		let msgs: Vec<_> = Dmp::dmq_contents_do_not_call_in_consensus(a)
			.into_iter()
			.map(|m| m.msg)
			.collect();
		assert_eq!(msgs, vec![vec![1], vec![2]]);
	});
}

#[test]
fn dmp_dmq_length_zero_when_unknown() {
	let a = ParaId::from(74);
	new_test_ext_integrity(default_genesis_config(), || {
		assert_eq!(Dmp::dmq_length(a), 0);
		assert_eq!(Dmp::dmq_contents_do_not_call_in_consensus(a), Vec::new());
	});
}

#[test]
fn dmp_check_processed_downward_messages_uses_first_message_block() {
	let a = ParaId::from(75);
	new_test_ext_integrity(default_genesis_config(), || {
		register_paras(&[a]);

		System::set_block_number(1);
		queue_downward_message(a, vec![1]).unwrap();
		System::set_block_number(2);
		queue_downward_message(a, vec![2]).unwrap();

		// Relay parent at block 0 — front msg was sent at block 1 > 0 — OK with 0
		// processed messages (advancement rule passes vacuously).
		assert!(Dmp::check_processed_downward_messages(a, 0, 0).is_ok());

		// Relay parent at block 1 — front message is `sent_at == 1 <= 1`, so the
		// advancement rule must require at least one message to be processed.
		assert!(Dmp::check_processed_downward_messages(a, 1, 0).is_err());
		assert!(Dmp::check_processed_downward_messages(a, 1, 1).is_ok());
	});
}

#[test]
fn dmp_clean_dmp_after_outgoing_clears_queue() {
	let a = ParaId::from(80);
	let b = ParaId::from(81);
	new_test_ext_integrity(default_genesis_config(), || {
		register_paras(&[a, b]);
		queue_downward_message(a, vec![1]).unwrap();
		queue_downward_message(a, vec![2]).unwrap();
		queue_downward_message(b, vec![100]).unwrap();

		// Trigger clean via session change with `a` outgoing.
		let notification = crate::initializer::SessionChangeNotification::default();
		Dmp::initializer_on_new_session(&notification, &vec![a]);

		assert_eq!(Dmp::dmq_length(a), 0);
		assert!(InboundDownwardQueue::<Test>::meta(a).is_none());
		assert_eq!(pages_in_storage(a), Vec::<PageIndex>::new());
		assert!(DownwardMessageQueueHeads::<Test>::get(a).is_zero());

		// `b` is unaffected.
		assert_eq!(Dmp::dmq_length(b), 1);
	});
}

#[test]
fn dmp_clean_dmp_large_queue_uses_lazy_delete() {
	let a = ParaId::from(82);
	let total = EXPECTED_LAZY_DELETE_PAGES + 5;

	let mut ext = new_test_ext(default_genesis_config());
	execute_with_try_state(&mut ext, || {
		register_paras(&[a]);
		for i in 0..total {
			queue_downward_message(a, vec![i as u8]).unwrap();
		}
	});
	ext.commit_all().unwrap();

	execute_with_try_state(&mut ext, || {
		let notification = crate::initializer::SessionChangeNotification::default();
		Dmp::initializer_on_new_session(&notification, &vec![a]);

		// dmq_length is meta-driven so it is 0 even with pages still in storage.
		assert_eq!(Dmp::dmq_length(a), 0);
		// Some pages still in storage, scheduled for lazy delete.
		assert!(DownwardMessageQueueLazyDelete::<Test>::contains_key(a));
		assert!(!pages_in_storage(a).is_empty());
	});
}

#[test]
fn dmp_queue_full_returns_exceeds_max_message_size_send_error() {
	// `ExceedsMaxQueueSize` maps to `SendError::ExceedsMaxMessageSize`.
	let err: xcm::latest::SendError = QueueDownwardMessageError::ExceedsMaxQueueSize.into();
	match err {
		xcm::latest::SendError::ExceedsMaxMessageSize => {},
		other => panic!("unexpected mapping: {:?}", other),
	}
}

#[test]
fn dmp_queue_message_returns_exceeds_max_queue_when_hard_limit_hit() {
	// Reach the queue length cap and ensure a further message is rejected.
	let a = ParaId::from(90);

	let mut genesis = default_genesis_config();
	// `dmq_max_length = MAX_POSSIBLE_ALLOCATION / max_downward_message_size`.
	// Setting this to MAX_POSSIBLE_ALLOCATION yields a max length of 1, so we
	// can hit the cap with two messages.
	genesis.configuration.config.max_downward_message_size = MAX_POSSIBLE_ALLOCATION;

	new_test_ext_integrity(genesis, || {
		register_paras(&[a]);
		// First message fills the queue up to the cap.
		queue_downward_message(a, vec![1]).unwrap();
		queue_downward_message(a, vec![2]).unwrap();
		// Now `dmq_length > dmq_max_length`, so the next must be rejected by
		// `can_queue_downward_message` with `ExceedsMaxMessageSize` (the cap is
		// reported through that variant in this branch).
		assert!(matches!(
			queue_downward_message(a, vec![3]),
			Err(QueueDownwardMessageError::ExceedsMaxMessageSize)
		));
	});
}

#[test]
fn dmp_on_poll_drives_lazy_delete() {
	let a = ParaId::from(91);
	let total = EXPECTED_LAZY_DELETE_PAGES + 3;

	let mut ext = new_test_ext(default_genesis_config());
	execute_with_try_state(&mut ext, || {
		register_paras(&[a]);
		for i in 0..total {
			queue_downward_message(a, vec![i as u8]).unwrap();
		}
	});
	ext.commit_all().unwrap();

	execute_with_try_state(&mut ext, || {
		let notification = crate::initializer::SessionChangeNotification::default();
		Dmp::initializer_on_new_session(&notification, &vec![a]);
		assert!(DownwardMessageQueueLazyDelete::<Test>::contains_key(a));
	});
	ext.commit_all().unwrap();

	let mut iterations = 0u32;
	loop {
		let still_pending = execute_with_try_state(&mut ext, || {
			DownwardMessageQueueLazyDelete::<Test>::contains_key(a)
		});
		if !still_pending {
			break;
		}
		execute_with_try_state(&mut ext, || {
			let used = <Dmp as frame_support::traits::Hooks<BlockNumberFor<Test>>>::on_idle(
				System::block_number(),
				Weight::MAX,
			);

			assert!(used.all_lte(Weight::MAX) && !used.is_zero(), "on_idle weight insane");
		});
		ext.commit_all().unwrap();
		iterations += 1;
		assert!((iterations as u64) < total + 10, "on_idle lazy delete should make progress",);
	}
	execute_with_try_state(&mut ext, || {
		assert_eq!(pages_in_storage(a), Vec::<PageIndex>::new());
	});
}

#[test]
fn inbound_downward_queue_meta_codec_roundtrip() {
	use codec::{Decode, Encode};

	// Default round-trips.
	let default = InboundDownwardQueueMeta { first_full: 0, first_free: 0 };
	let bytes = default.encode();
	let decoded: InboundDownwardQueueMeta = Decode::decode(&mut &bytes[..]).unwrap();
	assert_eq!(decoded, default);

	// Non-default round-trips.
	let original = InboundDownwardQueueMeta { first_full: 17, first_free: 42 };
	let bytes = original.encode();
	let decoded: InboundDownwardQueueMeta = Decode::decode(&mut &bytes[..]).unwrap();
	assert_eq!(decoded, original);
}

#[test]
fn migrate_v0_to_v1_step_drains_multiple_paras_across_many_steps() {
	// Tight meter, 4 paras × 7 msgs: forces partial steps within and across paras.
	new_test_ext_integrity(default_genesis_config(), || {
		let paras: Vec<ParaId> = (1..=4).map(ParaId::from).collect();
		let msgs_per_para: u8 = 7;

		let mut expected: BTreeMap<
			ParaId,
			Vec<InboundDownwardMessage<polkadot_primitives::BlockNumber>>,
		> = BTreeMap::new();
		for &p in &paras {
			let v: Vec<_> = (0..msgs_per_para)
				.map(|i| InboundDownwardMessage {
					sent_at: u32::from(p) as polkadot_primitives::BlockNumber,
					msg: vec![u32::from(p) as u8, i],
				})
				.collect();
			migration::v0::DownwardMessageQueues::<Test>::insert(p, &v);
			expected.insert(p, v);
		}

		let base = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_base();
		let per_iter = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_iter();
		let per_msg = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_msg();

		let budget = base.saturating_add(per_iter).saturating_add(per_msg.saturating_mul(2));

		let mut cursor: Option<<MigrateV0ToV1<Test> as SteppedMigration>::Cursor> = None;
		let mut steps = 0u32;
		loop {
			let mut meter = WeightMeter::with_limit(budget);
			let ret = MigrateV0ToV1::<Test>::step(cursor.clone(), &mut meter).unwrap();
			steps += 1;
			match ret {
				None => break,
				Some(c) => cursor = Some(c),
			}
			assert!(steps < 1000, "migration not making progress");
		}
		assert!(steps > 1, "expected multiple steps, got {}", steps);

		assert_eq!(migration::v0::DownwardMessageQueues::<Test>::iter().count(), 0);

		for (para, msgs) in expected {
			let total = msgs.len() as u64;
			let meta = DownwardMessageQueueMeta::<Test>::get(para).unwrap();
			assert_eq!(meta.first_full, 0);
			assert_eq!(meta.first_free, total);
			for (i, msg) in msgs.into_iter().enumerate() {
				let page = DownwardMessageQueuePages::<Test>::get(para, i as PageIndex).unwrap();
				assert_eq!(page, msg);
			}
		}
	});
}

#[test]
fn migrate_v0_to_v1_step_returns_some_cursor_on_partial_first_step() {
	new_test_ext_integrity(default_genesis_config(), || {
		let a = ParaId::from(1234);

		let msgs: Vec<InboundDownwardMessage<polkadot_primitives::BlockNumber>> =
			(0..10u8).map(|i| InboundDownwardMessage { sent_at: 1, msg: vec![i] }).collect();
		migration::v0::DownwardMessageQueues::<Test>::insert(a, &msgs);

		let base = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_base();
		let per_iter = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_iter();
		let per_msg = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_msg();

		let budget = base.saturating_add(per_iter).saturating_add(per_msg.saturating_mul(2));
		let mut meter = WeightMeter::with_limit(budget);

		let result = MigrateV0ToV1::<Test>::step(None, &mut meter);
		assert!(matches!(result, Ok(Some(_))), "got {:?}", result);
	});
}

#[test]
fn migrate_v0_to_v1_step_returns_err_on_insufficient_weight() {
	new_test_ext_integrity(default_genesis_config(), || {
		let para = ParaId::from(1);
		migration::v0::DownwardMessageQueues::<Test>::insert(
			para,
			&vec![InboundDownwardMessage { sent_at: 1, msg: vec![0] }],
		);

		let base = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_base();
		let per_iter = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_iter();
		let per_msg = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_msg();
		let required = base.saturating_add(per_iter).saturating_add(per_msg);

		let mut meter = WeightMeter::with_limit(required.saturating_sub(per_msg));
		match MigrateV0ToV1::<Test>::step(None, &mut meter) {
			Err(SteppedMigrationError::InsufficientWeight { required: r }) => {
				assert_eq!(r, required)
			},
			other => panic!("expected InsufficientWeight, got {:?}", other),
		}
		// Untouched: no base consumed, no pages written, v0 entry intact.
		assert!(migration::v0::DownwardMessageQueues::<Test>::contains_key(para));
		assert!(DownwardMessageQueueMeta::<Test>::get(para).is_none());
	});
}

#[test]
fn migrate_v0_to_v1_step_short_circuits_when_storage_version_already_bumped() {
	new_test_ext_integrity(default_genesis_config(), || {
		// Simulate "already migrated" by bumping the on-chain version.
		frame_support::traits::StorageVersion::new(1).put::<crate::dmp::Pallet<Test>>();

		// Stale v0 data must remain untouched.
		let para = ParaId::from(1);
		migration::v0::DownwardMessageQueues::<Test>::insert(
			para,
			&vec![InboundDownwardMessage { sent_at: 1, msg: vec![0] }],
		);

		let mut meter = WeightMeter::new();
		assert_eq!(MigrateV0ToV1::<Test>::step(None, &mut meter), Ok(None));
		assert!(migration::v0::DownwardMessageQueues::<Test>::contains_key(para));
	});
}

#[test]
fn migrate_v0_to_v1_storage_version_not_bumped_on_partial_step() {
	new_test_ext_integrity(default_genesis_config(), || {
		let para = ParaId::from(1);
		let msgs: Vec<_> =
			(0..10u8).map(|i| InboundDownwardMessage { sent_at: 1, msg: vec![i] }).collect();
		migration::v0::DownwardMessageQueues::<Test>::insert(para, &msgs);

		let base = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_base();
		let per_iter = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_iter();
		let per_msg = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_msg();
		let budget = base.saturating_add(per_iter).saturating_add(per_msg.saturating_mul(2));

		let mut meter = WeightMeter::with_limit(budget);
		assert!(matches!(
			MigrateV0ToV1::<Test>::step(None, &mut meter),
			Ok(Some(MigrationCursor::InProgress { .. })),
		));

		// If the version got bumped here, the next step would short-circuit.
		assert_eq!(
			crate::dmp::Pallet::<Test>::on_chain_storage_version(),
			frame_support::traits::StorageVersion::new(0),
		);
	});
}

#[test]
fn migrate_v0_to_v1_step_skips_in_progress_cursor_with_vanished_v0_entry() {
	new_test_ext_integrity(default_genesis_config(), || {
		let live = ParaId::from(1);
		let vanished = ParaId::from(999);
		let msgs: Vec<_> =
			(0..3u8).map(|i| InboundDownwardMessage { sent_at: 1, msg: vec![i] }).collect();
		migration::v0::DownwardMessageQueues::<Test>::insert(live, &msgs);

		let cursor = Some(MigrationCursor::InProgress { para: vanished });
		let mut meter = WeightMeter::new();
		assert_eq!(MigrateV0ToV1::<Test>::step(cursor, &mut meter), Ok(None));

		// `live` was migrated; the vanished cursor left no trace.
		let meta = DownwardMessageQueueMeta::<Test>::get(live).unwrap();
		assert_eq!(meta.first_full, 0);
		assert_eq!(meta.first_free, msgs.len() as u64);
		assert!(!migration::v0::DownwardMessageQueues::<Test>::contains_key(live));
		assert!(DownwardMessageQueueMeta::<Test>::get(vanished).is_none());
	});
}

#[test]
fn migrate_v0_to_v1_step_empty_v0_entry_produces_no_meta() {
	new_test_ext_integrity(default_genesis_config(), || {
		let empty = ParaId::from(1000);
		let full = ParaId::from(2);
		migration::v0::DownwardMessageQueues::<Test>::insert(
			empty,
			Vec::<InboundDownwardMessage<_>>::new(),
		);
		let msgs: Vec<_> =
			(0..3u8).map(|i| InboundDownwardMessage { sent_at: 1, msg: vec![i] }).collect();
		migration::v0::DownwardMessageQueues::<Test>::insert(full, &msgs);

		let mut cursor = None;
		loop {
			let mut meter = WeightMeter::new();
			match MigrateV0ToV1::<Test>::step(cursor, &mut meter).unwrap() {
				None => break,
				Some(c) => cursor = Some(c),
			}
		}

		assert_eq!(migration::v0::DownwardMessageQueues::<Test>::iter().count(), 0);
		assert!(
			DownwardMessageQueueMeta::<Test>::get(empty).is_none(),
			"empty v0 entry must not produce a meta entry",
		);
		let meta = DownwardMessageQueueMeta::<Test>::get(full).unwrap();
		assert_eq!(meta.first_full, 0);
		assert_eq!(meta.first_free, msgs.len() as u64);
	});
}

#[cfg(feature = "try-runtime")]
#[test]
fn migrate_v0_to_v1_post_upgrade_accepts_empty_v0_entry() {
	new_test_ext_integrity(default_genesis_config(), || {
		let empty = ParaId::from(1000);
		migration::v0::DownwardMessageQueues::<Test>::insert(
			empty,
			Vec::<InboundDownwardMessage<_>>::new(),
		);

		let snapshot = MigrateV0ToV1::<Test>::pre_upgrade().unwrap();

		let mut cursor = None;
		loop {
			let mut meter = WeightMeter::new();
			match MigrateV0ToV1::<Test>::step(cursor, &mut meter).unwrap() {
				None => break,
				Some(c) => cursor = Some(c),
			}
		}

		MigrateV0ToV1::<Test>::post_upgrade(snapshot).unwrap();
	});
}

#[cfg(feature = "try-runtime")]
#[test]
fn migrate_v0_to_v1_post_upgrade_accepts_concurrent_v1_writes_for_empty_v0() {
	new_test_ext_integrity(default_genesis_config(), || {
		let para = ParaId::from(1000);
		migration::v0::DownwardMessageQueues::<Test>::insert(
			para,
			Vec::<InboundDownwardMessage<_>>::new(),
		);

		let snapshot = MigrateV0ToV1::<Test>::pre_upgrade().unwrap();

		InboundDownwardQueue::<Test>::push_back_inbound(
			para,
			&InboundDownwardMessage { sent_at: 7, msg: vec![42] },
		)
		.unwrap();

		let mut cursor = None;
		while let Some(c) = MigrateV0ToV1::<Test>::step(cursor, &mut WeightMeter::new()).unwrap() {
			cursor = Some(c);
		}

		MigrateV0ToV1::<Test>::post_upgrade(snapshot).unwrap();
	});
}

#[cfg(feature = "try-runtime")]
#[test]
fn migrate_v0_to_v1_concurrent_v1_pushes_lose_no_messages() {
	new_test_ext_integrity(default_genesis_config(), || {
		let para = ParaId::from(1);
		register_paras(&[para]);

		// `sent_at = 0` marks v0 messages; live pushes get the current block number.
		let v0_msgs: Vec<_> = (0..5u8)
			.map(|i| InboundDownwardMessage { sent_at: 0, msg: vec![0xA0, i] })
			.collect();
		migration::v0::DownwardMessageQueues::<Test>::insert(para, &v0_msgs);

		let snapshot = MigrateV0ToV1::<Test>::pre_upgrade().unwrap();

		let base = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_base();
		let per_iter = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_iter();
		let per_msg = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_msg();
		let budget = base.saturating_add(per_iter).saturating_add(per_msg.saturating_mul(2));

		let mut cursor = None;
		let mut live_count = 0u32;
		let mut steps = 0u32;
		while let Some(c) =
			MigrateV0ToV1::<Test>::step(cursor, &mut WeightMeter::with_limit(budget)).unwrap()
		{
			cursor = Some(c);
			steps += 1;
			assert!(steps < 100);

			run_to_block(System::block_number() + 1, None);
			assert_ok!(queue_downward_message(para, vec![0xB0, live_count as u8]));
			live_count += 1;
		}
		assert!(steps > 1);

		assert_eq!(migration::v0::DownwardMessageQueues::<Test>::iter().count(), 0);

		let pages = InboundDownwardQueue::<Test>::peek_all_do_not_call_in_consensus(para);
		assert_eq!(pages.len() as u64, v0_msgs.len() as u64 + live_count as u64);
		assert_eq!(pages.iter().filter(|m| m.sent_at == 0).count(), v0_msgs.len());

		MigrateV0ToV1::<Test>::post_upgrade(snapshot).unwrap();
	});
}

#[cfg(feature = "try-runtime")]
#[test]
fn migrate_v0_to_v1_pre_post_upgrade_idempotent() {
	new_test_ext_integrity(default_genesis_config(), || {
		let para = ParaId::from(1);
		let msgs: Vec<_> =
			(0..3u8).map(|i| InboundDownwardMessage { sent_at: 1, msg: vec![i] }).collect();
		migration::v0::DownwardMessageQueues::<Test>::insert(para, &msgs);

		let snapshot = MigrateV0ToV1::<Test>::pre_upgrade().unwrap();

		let mut cursor = None;
		loop {
			let mut meter = WeightMeter::new();
			match MigrateV0ToV1::<Test>::step(cursor, &mut meter).unwrap() {
				None => break,
				Some(c) => cursor = Some(c),
			}
		}
		MigrateV0ToV1::<Test>::post_upgrade(snapshot).unwrap();

		// Re-running the hooks against an already-migrated state must not panic.
		let snapshot_after = MigrateV0ToV1::<Test>::pre_upgrade().unwrap();
		MigrateV0ToV1::<Test>::post_upgrade(snapshot_after).unwrap();
	});
}

/// Re-derive the MQC head by folding over `msgs` in delivery (FIFO) order, mirroring
/// `queue_downward_message`. Reimplemented here so the test is an independent oracle for the
/// on-chain head: the outer hash is `BlakeTwo256` (hard-coded in production) and the inner
/// message hash is `T::Hashing`, which is `BlakeTwo256` in this mock.
fn mqc_head_over(
	msgs: &[InboundDownwardMessage<polkadot_primitives::BlockNumber>],
) -> polkadot_primitives::Hash {
	use sp_runtime::traits::{BlakeTwo256, Hash as HashT};

	let mut head = polkadot_primitives::Hash::zero();
	for m in msgs {
		head = BlakeTwo256::hash_of(&(head, m.sent_at, BlakeTwo256::hash_of(&m.msg)));
	}
	head
}

/// Seed `para` with an `n`-message queue built through the real `queue_downward_message` path —
/// so the MQC head is genuinely chained over them — then relocate the messages into the
/// pre-upgrade v0 `Vec` layout. Returns the head that was built.
fn seed_v0_with_built_mqc_head(para: ParaId, n: u8) -> polkadot_primitives::Hash {
	register_paras(&[para]);
	for i in 0..n {
		// Spread across blocks so `sent_at` varies and actually feeds into the chain.
		run_to_block(System::block_number() + 1, None);
		queue_downward_message(para, vec![0xA0, i]).unwrap();
	}
	let head = DownwardMessageQueueHeads::<Test>::get(para);

	// Guard against an oracle bug masking (or faking) a regression: the re-derivation must
	// already match the production head for the freshly-built queue.
	let built = InboundDownwardQueue::<Test>::peek_all_do_not_call_in_consensus(para);
	assert_eq!(mqc_head_over(&built), head, "oracle disagrees with production head");

	// Move the v1 pages into the v0 layout to mimic the pre-upgrade on-chain state: all
	// messages in `v0::DownwardMessageQueues`, head already reflecting them, version still 0.
	DownwardMessageQueueMeta::<Test>::remove(para);
	for idx in pages_in_storage(para) {
		DownwardMessageQueuePages::<Test>::remove(para, idx);
	}
	migration::v0::DownwardMessageQueues::<Test>::insert(para, &built);

	head
}

#[test]
fn migrate_v0_to_v1_is_mqc_head_transparent() {
	// The migration only relocates messages between storage layouts: it must neither touch the
	// stored MQC head nor reorder the queue, even when drained over several metered steps.
	new_test_ext_integrity(default_genesis_config(), || {
		let para = ParaId::from(1);
		let head = seed_v0_with_built_mqc_head(para, 5);
		assert!(!head.is_zero(), "precondition: head must have been built");

		// Tight meter (~2 msgs/step) so the para migrates over several steps, exercising the
		// suffix write-back rather than a single take-all drain.
		let base = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_base();
		let per_iter = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_iter();
		let per_msg = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_msg();
		let budget = base.saturating_add(per_iter).saturating_add(per_msg.saturating_mul(2));

		let mut cursor = None;
		let mut steps = 0u32;
		while let Some(c) =
			MigrateV0ToV1::<Test>::step(cursor, &mut WeightMeter::with_limit(budget)).unwrap()
		{
			cursor = Some(c);
			steps += 1;
			assert!(steps < 100, "migration not making progress");
		}
		assert!(steps > 1, "expected a multi-step migration, got {}", steps);
		assert_eq!(migration::v0::DownwardMessageQueues::<Test>::iter().count(), 0);

		// Migration does not chain new links, so the head must be bit-identical, and the
		// delivered order must still hash to it.
		assert_eq!(DownwardMessageQueueHeads::<Test>::get(para), head, "migration moved the head");
		let delivered = InboundDownwardQueue::<Test>::peek_all_do_not_call_in_consensus(para);
		assert_eq!(mqc_head_over(&delivered), head, "migration reordered the queue");
	});
}

#[test]
fn migrate_v0_to_v1_preserves_mqc_head_with_interleaved_pushes() {
	// Regression test for the MQC inconsistency. A DMP message that arrives while v0 is only
	// half-migrated must still be *delivered* after the un-migrated v0 messages. If it jumped
	// ahead (straight into v1, which `peek_all` reads first), the relay's send-order head would
	// no longer match the parachain's receive-order head and the parachain would stall.
	new_test_ext_integrity(default_genesis_config(), || {
		let para = ParaId::from(1);
		let pre_msgs = 5u8;
		seed_v0_with_built_mqc_head(para, pre_msgs);

		let base = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_base();
		let per_iter = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_iter();
		let per_msg = <Test as crate::dmp::Config>::WeightInfo::migrate_v0_to_v1_step_msg();
		let budget = base.saturating_add(per_iter).saturating_add(per_msg.saturating_mul(2));

		// Drive the MBM, queuing a fresh message between every step at an advancing block.
		let mut cursor = None;
		let mut live = 0u8;
		let mut steps = 0u32;
		let mut pushed_into_half_migrated = false;
		while let Some(c) =
			MigrateV0ToV1::<Test>::step(cursor, &mut WeightMeter::with_limit(budget)).unwrap()
		{
			cursor = Some(c);
			steps += 1;
			assert!(steps < 100, "migration not making progress");

			run_to_block(System::block_number() + 1, None);
			// Record whether this push lands in the reordering window: v0 still non-empty means
			// some old messages are not yet migrated, so the new one must queue behind them.
			if migration::v0::DownwardMessageQueues::<Test>::decode_len(para)
				.map_or(false, |l| l > 0)
			{
				pushed_into_half_migrated = true;
			}
			queue_downward_message(para, vec![0xB0, live]).unwrap();
			live += 1;
		}

		assert!(steps > 1, "expected a multi-step migration, got {}", steps);
		assert!(
			pushed_into_half_migrated,
			"test ineffective: no message was queued while v0 was half-migrated",
		);
		assert_eq!(migration::v0::DownwardMessageQueues::<Test>::iter().count(), 0);

		// The invariant: the head (chained at send time, never recomputed) must equal the chain
		// folded over the delivered order. Any reordering diverges here with overwhelming odds.
		let delivered = InboundDownwardQueue::<Test>::peek_all_do_not_call_in_consensus(para);
		assert_eq!(
			delivered.len(),
			pre_msgs as usize + live as usize,
			"messages lost or duplicated"
		);
		assert_eq!(
			mqc_head_over(&delivered),
			DownwardMessageQueueHeads::<Test>::get(para),
			"MQC head diverged from delivery order — parachains would stall",
		);
	});
}

#[test]
fn peek_front_falls_through_to_v0_when_v1_range_empty() {
	new_test_ext_integrity(default_genesis_config(), || {
		let para = ParaId::from(1);
		let v0_msg = InboundDownwardMessage { sent_at: 1, msg: vec![42] };

		DownwardMessageQueueMeta::<Test>::insert(
			para,
			InboundDownwardQueueMeta { first_full: 2, first_free: 2 },
		);
		migration::v0::DownwardMessageQueues::<Test>::insert(para, &vec![v0_msg.clone()]);

		assert_eq!(InboundDownwardQueue::<Test>::peek_front(para), Some(v0_msg));
	});
}

#[test]
fn pop_front_v0_returns_oldest_not_newest() {
	new_test_ext_integrity(default_genesis_config(), || {
		let para = ParaId::from(1);
		let msgs: Vec<InboundDownwardMessage<polkadot_primitives::BlockNumber>> =
			(0..3u8).map(|i| InboundDownwardMessage { sent_at: 1, msg: vec![i] }).collect();
		migration::v0::DownwardMessageQueues::<Test>::insert(para, &msgs);

		assert_eq!(InboundDownwardQueue::<Test>::pop_front(para), Some(msgs[0].clone()));
	});
}

#[test]
fn drop_front_n_removes_v0_entry_when_drained() {
	new_test_ext_integrity(default_genesis_config(), || {
		let para = ParaId::from(1);
		let msg = InboundDownwardMessage { sent_at: 1, msg: vec![42] };
		migration::v0::DownwardMessageQueues::<Test>::insert(para, &vec![msg]);

		InboundDownwardQueue::<Test>::drop_front_n(para, 1);

		assert!(!migration::v0::DownwardMessageQueues::<Test>::contains_key(para));
	});
}

#[test]
fn pop_front_does_not_leave_empty_v0_entry() {
	new_test_ext_integrity(default_genesis_config(), || {
		let para = ParaId::from(1);
		let msg = InboundDownwardMessage { sent_at: 1, msg: vec![1] };
		migration::v0::DownwardMessageQueues::<Test>::insert(para, &vec![msg]);

		InboundDownwardQueue::<Test>::pop_front(para);

		assert!(!migration::v0::DownwardMessageQueues::<Test>::contains_key(para));
	});
}

#[test]
fn push_back_routes_to_v1_when_v0_entry_is_empty() {
	// Deliberately injects an empty v0 entry (a state `try_state` forbids) to check
	// `push_back` dispatches on content, not row presence — so it runs without the
	// integrity wrapper.
	new_test_ext(default_genesis_config()).execute_with(|| {
		let para = ParaId::from(1);
		let msg = InboundDownwardMessage { sent_at: 1, msg: vec![0] };
		migration::v0::DownwardMessageQueues::<Test>::insert(para, &vec![msg]);
		migration::v0::DownwardMessageQueues::<Test>::mutate(para, |v| v.clear());
		assert!(migration::v0::DownwardMessageQueues::<Test>::contains_key(para));

		InboundDownwardQueue::<Test>::push_back(para, vec![99]).unwrap();

		let meta =
			InboundDownwardQueue::<Test>::meta(para).expect("push must have written meta in v1");
		assert_eq!(meta.first_free, 1);
	});
}
