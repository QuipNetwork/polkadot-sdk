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

//! Benchmarks for the dmp pallet's internal queue helpers.

#![cfg(feature = "runtime-benchmarks")]

use super::{inbound_downward_queue::LAZY_DELETE_MAX_PAGES, migration, *};
use alloc::{vec, vec::Vec};
use frame_benchmarking::v2::*;
use frame_support::{migrations::SteppedMigration, weights::WeightMeter};
use polkadot_primitives::Id as ParaId;

#[benchmarks]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn lazy_delete_some() {
		let para = ParaId::from(1);
		let pages: u64 = LAZY_DELETE_MAX_PAGES as u64;
		let max_size = configuration::ActiveConfig::<T>::get().max_downward_message_size as usize;
		let payload = vec![0u8; max_size];

		for i in 0..pages {
			DownwardMessageQueuePages::<T>::insert(
				para,
				i,
				InboundDownwardMessage {
					sent_at: frame_system::Pallet::<T>::block_number(),
					msg: payload.clone(),
				},
			);
		}
		DownwardMessageQueueLazyDelete::<T>::insert(para, (0u64, pages));

		let mut meter = WeightMeter::new();

		#[block]
		{
			InboundDownwardQueue::<T>::lazy_delete_some(&mut meter);
		}

		assert!(!DownwardMessageQueueLazyDelete::<T>::contains_key(para));
	}

	/// Base case for [`migration::MigrateV0ToV1::step`]: nothing left in
	/// `v0::DownwardMessageQueues`, so the loop body terminates on the first
	/// iter probe without doing any per-para work.
	#[benchmark]
	fn migrate_v0_to_v1_step_base() {
		// A real runtime writes the current version at genesis; rewind so `step` runs the
		// migration path instead of short-circuiting on the version check.
		StorageVersion::new(0).put::<Pallet<T>>();

		let mut meter = WeightMeter::new();

		#[block]
		{
			migration::MigrateV0ToV1::<T>::step(None, &mut meter).expect("step has full meter");
		}
	}

	#[benchmark]
	fn migrate_v0_to_v1_step_iter() {
		// A real runtime writes the current version at genesis; rewind so `step` runs the
		// migration path instead of short-circuiting on the version check.
		StorageVersion::new(0).put::<Pallet<T>>();

		let para = ParaId::from(1);
		let max_size = configuration::ActiveConfig::<T>::get().max_downward_message_size;
		let payload = vec![0u8; max_size as usize];

		// One step migrates two messages (the per-iter freebie plus one per-msg), so a third
		// forces the write-back of the unmigrated suffix into `v0::DownwardMessageQueues`.
		let messages: Vec<InboundDownwardMessage<BlockNumberFor<T>>> = (0..3)
			.map(|_| InboundDownwardMessage {
				sent_at: frame_system::Pallet::<T>::block_number(),
				msg: payload.clone(),
			})
			.collect();

		migration::v0::DownwardMessageQueues::<T>::insert(para, &messages);

		// `step` and this bound read the same `WeightInfo`, so they agree on where the meter
		// runs out: after one full iteration, mid-para.
		let minimum = <T as Config>::WeightInfo::migrate_v0_to_v1_step_base()
			.saturating_add(<T as Config>::WeightInfo::migrate_v0_to_v1_step_iter())
			.saturating_add(<T as Config>::WeightInfo::migrate_v0_to_v1_step_msg());
		let mut meter = WeightMeter::with_limit(minimum);

		#[block]
		{
			migration::MigrateV0ToV1::<T>::step(None, &mut meter).expect("step has minimum meter");
		}

		let meta =
			DownwardMessageQueueMeta::<T>::get(para).expect("meta written for non-empty queue");
		assert_eq!(meta.first_full, 0);
		assert_eq!(meta.first_free, 2);
		assert_eq!(migration::v0::DownwardMessageQueues::<T>::decode_len(para), Some(1));
	}

	/// One re-enqueue from the inner loop of [`migration::MigrateV0ToV1::step`].
	#[benchmark]
	fn migrate_v0_to_v1_step_msg() {
		let para = ParaId::from(1);
		let max_size = configuration::ActiveConfig::<T>::get().max_downward_message_size as usize;
		let msg = InboundDownwardMessage {
			sent_at: frame_system::Pallet::<T>::block_number(),
			msg: vec![0u8; max_size],
		};

		#[block]
		{
			InboundDownwardQueue::<T>::push_back_inbound(para, &msg)
				.expect("push_back_inbound on empty queue cannot overflow");
		}

		assert!(DownwardMessageQueuePages::<T>::contains_key(para, 0u64));
	}

	impl_benchmark_test_suite!(
		Pallet,
		crate::mock::new_test_ext(Default::default()),
		crate::mock::Test
	);
}
