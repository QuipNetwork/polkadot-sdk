// This file is part of Cumulus.

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

//! Benchmarking for the parachain-system pallet.

#![cfg(feature = "runtime-benchmarks")]

use super::*;
use crate::{
	block_weight::{
		BlockWeightMode, DynamicMaxBlockWeight, MaxParachainBlockWeight, FULL_CORE_WEIGHT,
	},
	parachain_inherent::InboundDownwardMessages,
};
use cumulus_primitives_core::{
	relay_chain::Hash as RelayHash, BlockBundleInfo, CoreInfo, InboundDownwardMessage,
};
use frame_benchmarking::v2::*;
use frame_support::{
	dispatch::{DispatchInfo, PostDispatchInfo},
	weights::constants::WEIGHT_REF_TIME_PER_SECOND,
};
use frame_system::RawOrigin;
use sp_core::ConstU32;
use sp_runtime::traits::{BlakeTwo256, DispatchTransaction, Dispatchable};

fn has_use_full_core_digest<T: Config>() -> bool {
	let digest = frame_system::Pallet::<T>::digest();
	CumulusDigestItem::contains_use_full_core(&digest)
}

#[benchmarks(where
	T: Send + Sync,
    T::RuntimeCall: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
)]
mod benchmarks {
	use super::*;

	/// Enqueue `n` messages via `enqueue_inbound_downward_messages`.
	///
	/// The limit is set to `1000` for benchmarking purposes as the actual limit is only known at
	/// runtime. However, the limit (and default) for Dotsama are magnitudes smaller.
	#[benchmark]
	fn enqueue_inbound_downward_messages(n: Linear<0, 1000>) {
		let msg = InboundDownwardMessage {
			sent_at: n, // The block number does not matter.
			msg: vec![0u8; MaxDmpMessageLenOf::<T>::get() as usize],
		};
		let msgs = vec![msg; n as usize];
		let head = mqp_head(&msgs);

		#[block]
		{
			Pallet::<T>::enqueue_inbound_downward_messages(
				head,
				InboundDownwardMessages::new(msgs).into_abridged(&mut usize::MAX.clone()),
			);
		}

		assert_eq!(ProcessedDownwardMessages::<T>::get(), n);
		assert_eq!(LastDmqMqcHead::<T>::get().head(), head);
	}

	/// Re-implements an easy version of the `MessageQueueChain` for testing purposes.
	fn mqp_head(msgs: &Vec<InboundDownwardMessage>) -> RelayHash {
		let mut head = Default::default();
		for msg in msgs.iter() {
			let msg_hash = BlakeTwo256::hash_of(&msg.msg);
			head = BlakeTwo256::hash_of(&(head, msg.sent_at, msg_hash));
		}
		head
	}

	/// The worst-case scenario for the block weight transaction extension.
	///
	/// Before executing an extrinsic `FractionOfCore` is set, changed to `PotentialFullCore` and
	/// post dispatch switches to `FullCore`.
	#[benchmark]
	fn block_weight_tx_extension_max_weight() -> Result<(), BenchmarkError> {
		let caller = account("caller", 0, 0);

		frame_system::Pallet::<T>::note_inherents_applied();

		frame_system::Pallet::<T>::set_extrinsic_index(1);

		frame_system::Pallet::<T>::deposit_log(
			BlockBundleInfo { index: 0, is_last: false }.to_digest_item(),
		);
		frame_system::Pallet::<T>::deposit_log(
			CoreInfo {
				selector: 0.into(),
				claim_queue_offset: 0.into(),
				number_of_cores: 1.into(),
			}
			.to_digest_item(),
		);
		let target_weight = MaxParachainBlockWeight::<T, ConstU32<4>>::target_block_weight();

		let info = DispatchInfo {
			// The weight needs to be more than the target weight.
			call_weight: target_weight
				.saturating_add(Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 0)),
			extension_weight: Weight::zero(),
			class: DispatchClass::Normal,
			..Default::default()
		};
		let call: T::RuntimeCall = frame_system::Call::remark { remark: vec![] }.into();
		let post_info = PostDispatchInfo { actual_weight: None, pays_fee: Default::default() };
		let len = 0_usize;

		crate::BlockWeightMode::<T>::put(BlockWeightMode::fraction_of_core(None));

		let ext = DynamicMaxBlockWeight::<T, (), ConstU32<4>>::new(());

		#[block]
		{
			ext.test_run(RawOrigin::Signed(caller).into(), &call, &info, len, 0, |_| {
				// Normally this is done by `CheckWeight`
				frame_system::Pallet::<T>::register_extra_weight_unchecked(
					info.call_weight,
					DispatchClass::Normal,
				);
				Ok(post_info)
			})
			.unwrap()
			.unwrap();
		}

		assert_eq!(crate::BlockWeightMode::<T>::get().unwrap(), BlockWeightMode::full_core());
		assert!(has_use_full_core_digest::<T>());
		assert_eq!(MaxParachainBlockWeight::<T, ConstU32<4>>::get(), FULL_CORE_WEIGHT);

		Ok(())
	}

	/// A benchmark that assumes that an extrinsic was executed with `FractionOfCore` set.
	#[benchmark]
	fn block_weight_tx_extension_stays_fraction_of_core() -> Result<(), BenchmarkError> {
		let caller = account("caller", 0, 0);

		frame_system::Pallet::<T>::note_inherents_applied();

		frame_system::Pallet::<T>::set_extrinsic_index(1);

		frame_system::Pallet::<T>::deposit_log(
			BlockBundleInfo { index: 0, is_last: false }.to_digest_item(),
		);
		frame_system::Pallet::<T>::deposit_log(
			CoreInfo {
				selector: 0.into(),
				claim_queue_offset: 0.into(),
				number_of_cores: 1.into(),
			}
			.to_digest_item(),
		);
		let target_weight = MaxParachainBlockWeight::<T, ConstU32<4>>::target_block_weight();

		let info = DispatchInfo {
			call_weight: Weight::from_parts(1024, 1024),
			extension_weight: Weight::zero(),
			class: DispatchClass::Normal,
			..Default::default()
		};
		let call: T::RuntimeCall = frame_system::Call::remark { remark: vec![] }.into();
		let post_info = PostDispatchInfo { actual_weight: None, pays_fee: Default::default() };
		let len = 0_usize;

		crate::BlockWeightMode::<T>::put(BlockWeightMode::fraction_of_core(None));

		let ext = DynamicMaxBlockWeight::<T, (), ConstU32<4>>::new(());

		#[block]
		{
			ext.test_run(RawOrigin::Signed(caller).into(), &call, &info, len, 0, |_| {
				// Normally this is done by `CheckWeight`
				frame_system::Pallet::<T>::register_extra_weight_unchecked(
					info.call_weight,
					DispatchClass::Normal,
				);
				Ok(post_info)
			})
			.unwrap()
			.unwrap();
		}

		assert_eq!(
			crate::BlockWeightMode::<T>::get().unwrap(),
			BlockWeightMode::fraction_of_core(Some(1))
		);
		assert!(!has_use_full_core_digest::<T>());
		assert_eq!(MaxParachainBlockWeight::<T, ConstU32<4>>::get(), target_weight);

		Ok(())
	}

	/// A benchmark that assumes that `FullCore` was set already before executing an extrinsic.
	#[benchmark]
	fn block_weight_tx_extension_full_core() -> Result<(), BenchmarkError> {
		let caller = account("caller", 0, 0);

		frame_system::Pallet::<T>::set_block_number(1u32.into());

		frame_system::Pallet::<T>::note_inherents_applied();

		frame_system::Pallet::<T>::set_extrinsic_index(1);

		frame_system::Pallet::<T>::deposit_log(
			BlockBundleInfo { index: 0, is_last: false }.to_digest_item(),
		);
		frame_system::Pallet::<T>::deposit_log(
			CoreInfo {
				selector: 0.into(),
				claim_queue_offset: 0.into(),
				number_of_cores: 1.into(),
			}
			.to_digest_item(),
		);

		let info = DispatchInfo {
			call_weight: Weight::from_parts(1024, 1024),
			extension_weight: Weight::zero(),
			class: DispatchClass::Normal,
			..Default::default()
		};
		let call: T::RuntimeCall = frame_system::Call::remark { remark: vec![] }.into();
		let post_info = PostDispatchInfo { actual_weight: None, pays_fee: Default::default() };
		let len = 0_usize;

		crate::BlockWeightMode::<T>::put(BlockWeightMode::full_core());

		let ext = DynamicMaxBlockWeight::<T, (), ConstU32<4>>::new(());

		#[block]
		{
			ext.test_run(RawOrigin::Signed(caller).into(), &call, &info, len, 0, |_| {
				// Normally this is done by `CheckWeight`
				frame_system::Pallet::<T>::register_extra_weight_unchecked(
					info.call_weight,
					DispatchClass::Normal,
				);
				Ok(post_info)
			})
			.unwrap()
			.unwrap();
		}

		assert_eq!(crate::BlockWeightMode::<T>::get().unwrap(), BlockWeightMode::full_core());

		Ok(())
	}

	impl_benchmark_test_suite! {
		Pallet,
		crate::mock::new_test_ext(),
		crate::mock::Test
	}
}
