// This file is part of Substrate.

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

//! All benchmarks in this file are just for debugging the PoV calculation logic, they are unused.

#![cfg(feature = "runtime-benchmarks")]

use super::*;

use frame_benchmarking::v2::*;
use frame_support::traits::UnfilteredDispatchable;
use frame_system::{Pallet as System, RawOrigin};
use sp_runtime::traits::Hash;

#[benchmarks]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn storage_single_value_read() {
		Value::<T>::put(123);

		#[block]
		{
			assert_eq!(Value::<T>::get(), Some(123));
		}
	}

	#[benchmark(pov_mode = Ignored)]
	fn storage_single_value_ignored_read() {
		Value::<T>::put(123);
		#[block]
		{
			assert_eq!(Value::<T>::get(), Some(123));
		}
	}

	#[benchmark(pov_mode = MaxEncodedLen {
		Pov::Value2: Ignored
	})]
	fn storage_single_value_ignored_some_read() {
		Value::<T>::put(123);
		Value2::<T>::put(123);

		#[block]
		{
			assert_eq!(Value::<T>::get(), Some(123));
			assert_eq!(Value2::<T>::get(), Some(123));
		}
	}

	#[benchmark]
	fn storage_single_value_read_twice() {
		Value::<T>::put(123);

		#[block]
		{
			assert_eq!(Value::<T>::get(), Some(123));
			assert_eq!(Value::<T>::get(), Some(123));
		}
	}

	#[benchmark]
	fn storage_single_value_write() {
		#[block]
		{
			Value::<T>::put(123);
		}

		assert_eq!(Value::<T>::get(), Some(123));
	}

	#[benchmark]
	fn storage_single_value_kill() {
		Value::<T>::put(123);

		#[block]
		{
			Value::<T>::kill();
		}

		assert!(!Value::<T>::exists());
	}

	// This benchmark and the following are testing a storage map with adjacent storage items.
	//
	// First a storage map is filled and a specific number of other storage items is
	// created. Then the one value is read from the map. This demonstrates that the number of other
	// nodes in the Trie influences the proof size. The number of inserted nodes can be interpreted
	// as the number of `StorageMap`/`StorageValue` in the whole runtime.
	#[benchmark(pov_mode = Measured)]
	fn storage_1m_map_read_one_value_two_additional_layers() {
		(0..(1 << 10)).for_each(|i| Map1M::<T>::insert(i, i));
		// Assume there are 16-256 other storage items.
		(0..(1u32 << 4)).for_each(|i| {
			let k = T::Hashing::hash(&i.to_be_bytes());
			frame_support::storage::unhashed::put(k.as_ref(), &i);
		});

		#[block]
		{
			assert_eq!(Map1M::<T>::get(1 << 9), Some(1 << 9));
		}
	}

	#[benchmark(pov_mode = Measured)]
	fn storage_1m_map_read_one_value_three_additional_layers() {
		(0..(1 << 10)).for_each(|i| Map1M::<T>::insert(i, i));
		// Assume there are 256-4096 other storage items.
		(0..(1u32 << 8)).for_each(|i| {
			let k = T::Hashing::hash(&i.to_be_bytes());
			frame_support::storage::unhashed::put(k.as_ref(), &i);
		});

		#[block]
		{
			assert_eq!(Map1M::<T>::get(1 << 9), Some(1 << 9));
		}
	}

	#[benchmark(pov_mode = Measured)]
	fn storage_1m_map_read_one_value_four_additional_layers() {
		(0..(1 << 10)).for_each(|i| Map1M::<T>::insert(i, i));
		// Assume there are 4096-65536 other storage items.
		(0..(1u32 << 12)).for_each(|i| {
			let k = T::Hashing::hash(&i.to_be_bytes());
			frame_support::storage::unhashed::put(k.as_ref(), &i);
		});

		#[block]
		{
			assert_eq!(Map1M::<T>::get(1 << 9), Some(1 << 9));
		}
	}

	// Reads from both storage maps each `n` and `m` times. Should result in two linear components.
	#[benchmark]
	fn storage_map_read_per_component(n: Linear<0, 100>, m: Linear<0, 100>) {
		(0..m * 10).for_each(|i| Map1M::<T>::insert(i, i));
		(0..n * 10).for_each(|i| Map16M::<T>::insert(i, i));

		#[block]
		{
			(0..m).for_each(|i| assert_eq!(Map1M::<T>::get(i * 10), Some(i * 10)));
			(0..n).for_each(|i| assert_eq!(Map16M::<T>::get(i * 10), Some(i * 10)));
		}
	}

	#[benchmark(pov_mode = MaxEncodedLen {
		Pov::Map1M: Ignored
	})]
	fn storage_map_read_per_component_one_ignored(n: Linear<0, 100>, m: Linear<0, 100>) {
		(0..m * 10).for_each(|i| Map1M::<T>::insert(i, i));
		(0..n * 10).for_each(|i| Map16M::<T>::insert(i, i));

		#[block]
		{
			(0..m).for_each(|i| assert_eq!(Map1M::<T>::get(i * 10), Some(i * 10)));
			(0..n).for_each(|i| assert_eq!(Map16M::<T>::get(i * 10), Some(i * 10)));
		}
	}

	// Reads the same value from a storage map. Should not result in a component.
	#[benchmark]
	fn storage_1m_map_one_entry_repeated_read(n: Linear<0, 100>) {
		Map1M::<T>::insert(0, 0);

		#[block]
		{
			(0..n).for_each(|_| assert_eq!(Map1M::<T>::get(0), Some(0)));
		}
	}

	// Reads the same values from a storage map. Should result in a `1x` linear component.
	#[benchmark]
	fn storage_1m_map_multiple_entry_repeated_read(n: Linear<0, 100>) {
		(0..n).for_each(|i| Map1M::<T>::insert(i, i));

		#[block]
		{
			(0..n).for_each(|i| {
				// Reading the same value 10 times does nothing.
				(0..10).for_each(|_| assert_eq!(Map1M::<T>::get(i), Some(i)));
			});
		}
	}

	#[benchmark]
	fn storage_1m_double_map_read_per_component(n: Linear<0, 1024>) {
		(0..(1 << 10)).for_each(|i| DoubleMap1M::<T>::insert(i, i, i));

		#[block]
		{
			(0..n).for_each(|i| assert_eq!(DoubleMap1M::<T>::get(i, i), Some(i)));
		}
	}

	#[benchmark]
	fn storage_value_bounded_read() {
		#[block]
		{
			assert!(BoundedValue::<T>::get().is_none());
		}
	}

	// Reading unbounded values will produce no mathematical worst case PoV size for this component.
	#[benchmark]
	fn storage_value_unbounded_read() {
		#[block]
		{
			assert!(UnboundedValue::<T>::get().is_none());
		}
	}

	#[benchmark(pov_mode = Ignored)]
	fn storage_value_unbounded_ignored_read() {
		#[block]
		{
			assert!(UnboundedValue::<T>::get().is_none());
		}
	}

	// Same as above, but we still expect a mathematical worst case PoV size for the bounded one.
	#[benchmark]
	fn storage_value_bounded_and_unbounded_read() {
		(0..1024).for_each(|i| Map1M::<T>::insert(i, i));
		#[block]
		{
			assert!(UnboundedValue::<T>::get().is_none());
			assert!(BoundedValue::<T>::get().is_none());
		}
	}

	#[benchmark(pov_mode = Measured)]
	fn measured_storage_value_read_linear_size(l: Linear<0, { 1 << 22 }>) {
		let v: sp_runtime::BoundedVec<u8, _> = alloc::vec![0u8; l as usize].try_into().unwrap();
		LargeValue::<T>::put(&v);
		#[block]
		{
			assert!(LargeValue::<T>::get().is_some());
		}
	}

	#[benchmark(pov_mode = MaxEncodedLen)]
	fn mel_storage_value_read_linear_size(l: Linear<0, { 1 << 22 }>) {
		let v: sp_runtime::BoundedVec<u8, _> = alloc::vec![0u8; l as usize].try_into().unwrap();
		LargeValue::<T>::put(&v);
		#[block]
		{
			assert!(LargeValue::<T>::get().is_some());
		}
	}

	#[benchmark(pov_mode = Measured)]
	fn measured_storage_double_value_read_linear_size(l: Linear<0, { 1 << 22 }>) {
		let v: sp_runtime::BoundedVec<u8, _> = alloc::vec![0u8; l as usize].try_into().unwrap();
		LargeValue::<T>::put(&v);
		LargeValue2::<T>::put(&v);
		#[block]
		{
			assert!(LargeValue::<T>::get().is_some());
			assert!(LargeValue2::<T>::get().is_some());
		}
	}

	#[benchmark(pov_mode = MaxEncodedLen)]
	fn mel_storage_double_value_read_linear_size(l: Linear<0, { 1 << 22 }>) {
		let v: sp_runtime::BoundedVec<u8, _> = alloc::vec![0u8; l as usize].try_into().unwrap();
		LargeValue::<T>::put(&v);
		LargeValue2::<T>::put(&v);
		#[block]
		{
			assert!(LargeValue::<T>::get().is_some());
			assert!(LargeValue2::<T>::get().is_some());
		}
	}

	#[benchmark(pov_mode = MaxEncodedLen {
		Pov::LargeValue2: Measured
	})]
	fn mel_mixed_storage_double_value_read_linear_size(l: Linear<0, { 1 << 22 }>) {
		let v: sp_runtime::BoundedVec<u8, _> = alloc::vec![0u8; l as usize].try_into().unwrap();
		LargeValue::<T>::put(&v);
		LargeValue2::<T>::put(&v);
		#[block]
		{
			assert!(LargeValue::<T>::get().is_some());
			assert!(LargeValue2::<T>::get().is_some());
		}
	}

	#[benchmark(pov_mode = Measured {
		Pov::LargeValue2: MaxEncodedLen
	})]
	fn measured_mixed_storage_double_value_read_linear_size(l: Linear<0, { 1 << 22 }>) {
		let v: sp_runtime::BoundedVec<u8, _> = alloc::vec![0u8; l as usize].try_into().unwrap();
		LargeValue::<T>::put(&v);
		LargeValue2::<T>::put(&v);
		#[block]
		{
			assert!(LargeValue::<T>::get().is_some());
			assert!(LargeValue2::<T>::get().is_some());
		}
	}

	#[benchmark(pov_mode = Measured)]
	fn storage_map_unbounded_both_measured_read(i: Linear<0, 1000>) {
		UnboundedMap::<T>::insert(i, alloc::vec![0; i as usize]);
		UnboundedMap2::<T>::insert(i, alloc::vec![0; i as usize]);
		#[block]
		{
			assert!(UnboundedMap::<T>::get(i).is_some());
			assert!(UnboundedMap2::<T>::get(i).is_some());
		}
	}

	#[benchmark(pov_mode = MaxEncodedLen {
		Pov::UnboundedMap: Measured
	})]
	fn storage_map_partial_unbounded_read(i: Linear<0, 1000>) {
		Map1M::<T>::insert(i, 0);
		UnboundedMap::<T>::insert(i, alloc::vec![0; i as usize]);
		#[block]
		{
			assert!(Map1M::<T>::get(i).is_some());
			assert!(UnboundedMap::<T>::get(i).is_some());
		}
	}

	#[benchmark(pov_mode = MaxEncodedLen {
		Pov::UnboundedMap: Ignored
	})]
	fn storage_map_partial_unbounded_ignored_read(i: Linear<0, 1000>) {
		Map1M::<T>::insert(i, 0);
		UnboundedMap::<T>::insert(i, alloc::vec![0; i as usize]);
		#[block]
		{
			assert!(Map1M::<T>::get(i).is_some());
			assert!(UnboundedMap::<T>::get(i).is_some());
		}
	}

	// Emitting an event will not incur any PoV.
	#[benchmark]
	fn emit_event() {
		// Emit a single event.
		let call = Call::<T>::emit_event {};
		#[block]
		{
			call.dispatch_bypass_filter(RawOrigin::Root.into()).unwrap();
		}
		assert_eq!(System::<T>::events().len(), 1);
	}

	// A No-OP will not incur any PoV.
	#[benchmark]
	fn noop() {
		let call = Call::<T>::noop {};
		#[block]
		{
			call.dispatch_bypass_filter(RawOrigin::Root.into()).unwrap();
		}
	}

	#[benchmark]
	fn storage_iteration() {
		for i in 0..65000 {
			UnboundedMapTwox::<T>::insert(i, alloc::vec![0; 64]);
		}
		#[block]
		{
			for (key, value) in UnboundedMapTwox::<T>::iter() {
				unsafe {
					core::ptr::read_volatile(&key);
					core::ptr::read_volatile(value.as_ptr());
				}
			}
		}
	}

	#[benchmark(pov_mode = Measured)]
	fn storage_whitelisted_read() {
		let key = b"key1".to_vec();
		frame_support::storage::unhashed::put_raw(&key, b"value");
		frame_benchmarking::benchmarking::add_to_whitelist(key.clone().into());

		#[block]
		{
			assert!(frame_support::storage::unhashed::get_raw(&key).is_some());
		}
	}

	#[benchmark(pov_mode = Measured)]
	fn child_storage_whitelisted_read() {
		let child_trie_key = b":child_storage:default:my_trie".to_vec();
		let key = b"my_key".to_vec();
		sp_io::default_child_storage::set(&child_trie_key, &key, b"value");
		frame_benchmarking::add_to_whitelist_child(child_trie_key.clone(), key.clone());

		#[block]
		{
			assert!(sp_io::default_child_storage::get(&child_trie_key, &key).is_some());
		}
	}

	impl_benchmark_test_suite!(Pallet, super::mock::new_test_ext(), super::mock::Test,);
}

#[cfg(test)]
mod mock {
	use frame_support::derive_impl;
	use sp_runtime::{testing::H256, BuildStorage};

	type AccountId = u64;
	type Nonce = u32;

	type Block = frame_system::mocking::MockBlock<Test>;

	frame_support::construct_runtime!(
		pub enum Test
		{
			System: frame_system,
			Baseline: crate,
		}
	);

	#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
	impl frame_system::Config for Test {
		type BaseCallFilter = frame_support::traits::Everything;
		type BlockWeights = ();
		type BlockLength = ();
		type DbWeight = ();
		type RuntimeOrigin = RuntimeOrigin;
		type Nonce = Nonce;
		type RuntimeCall = RuntimeCall;
		type Hash = H256;
		type Hashing = ::sp_runtime::traits::BlakeTwo256;
		type AccountId = AccountId;
		type Lookup = sp_runtime::traits::IdentityLookup<Self::AccountId>;
		type Block = Block;
		type RuntimeEvent = RuntimeEvent;
		type BlockHashCount = ();
		type Version = ();
		type PalletInfo = PalletInfo;
		type AccountData = ();
		type OnNewAccount = ();
		type OnKilledAccount = ();
		type SystemWeightInfo = ();
		type SS58Prefix = ();
		type OnSetCode = ();
		type MaxConsumers = frame_support::traits::ConstU32<16>;
	}

	impl crate::Config for Test {
		type RuntimeEvent = RuntimeEvent;
	}

	pub fn new_test_ext() -> sp_io::TestExternalities {
		frame_system::GenesisConfig::<Test>::default().build_storage().unwrap().into()
	}
}
