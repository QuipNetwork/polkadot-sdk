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

//! Frame System benchmarks.

use alloc::{vec, vec::Vec};
use codec::Encode;
use frame_benchmarking::v2::*;
use frame_support::{dispatch::DispatchClass, storage, traits::Get};
use frame_system::{Call, Pallet as System, RawOrigin};
use sp_core::storage::well_known_keys;
use sp_runtime::traits::Hash;

pub struct Pallet<T: Config>(System<T>);
pub trait Config: frame_system::Config {
	/// Adds ability to the Runtime to test against their sample code.
	///
	/// Default is `../res/kitchensink_runtime.compact.compressed.wasm`.
	fn prepare_set_code_data() -> Vec<u8> {
		include_bytes!("../res/kitchensink_runtime.compact.compressed.wasm").to_vec()
	}

	/// Adds ability to the Runtime to prepare/initialize before running benchmark `set_code`.
	fn setup_set_code_requirements(_code: &Vec<u8>) -> Result<(), BenchmarkError> {
		Ok(())
	}

	/// Adds ability to the Runtime to do custom validation after benchmark.
	///
	/// Default is checking for `CodeUpdated` event .
	fn verify_set_code() {
		let code = Self::prepare_set_code_data();
		let hash = Self::Hashing::hash(&code);
		let want: Self::RuntimeEvent = frame_system::Event::<Self>::CodeUpdated { hash }.into();

		System::<Self>::assert_last_event(want);
	}
}

#[benchmarks]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn remark(
		b: Linear<0, { *T::BlockLength::get().max.get(DispatchClass::Normal) as u32 }>,
	) -> Result<(), BenchmarkError> {
		let remark_message = vec![1; b as usize];
		let caller = whitelisted_caller();

		#[extrinsic_call]
		remark(RawOrigin::Signed(caller), remark_message);

		Ok(())
	}

	#[benchmark]
	fn remark_with_event(
		b: Linear<0, { *T::BlockLength::get().max.get(DispatchClass::Normal) as u32 }>,
	) -> Result<(), BenchmarkError> {
		let remark_message = vec![1; b as usize];
		let caller: T::AccountId = whitelisted_caller();
		let hash = T::Hashing::hash(&remark_message[..]);

		#[extrinsic_call]
		remark_with_event(RawOrigin::Signed(caller.clone()), remark_message);

		System::<T>::assert_last_event(
			frame_system::Event::<T>::Remarked { sender: caller, hash }.into(),
		);
		Ok(())
	}

	#[benchmark]
	fn set_heap_pages() -> Result<(), BenchmarkError> {
		#[extrinsic_call]
		set_heap_pages(RawOrigin::Root, Default::default());

		Ok(())
	}

	#[benchmark]
	fn set_code() -> Result<(), BenchmarkError> {
		let runtime_blob = T::prepare_set_code_data();
		T::setup_set_code_requirements(&runtime_blob)?;

		#[extrinsic_call]
		set_code(RawOrigin::Root, runtime_blob);

		T::verify_set_code();
		Ok(())
	}

	#[benchmark(extra)]
	fn set_code_without_checks() -> Result<(), BenchmarkError> {
		// Assume Wasm ~4MB
		let code = vec![1; 4_000_000 as usize];
		T::setup_set_code_requirements(&code)?;

		#[block]
		{
			System::<T>::set_code_without_checks(RawOrigin::Root.into(), code)?;
		}

		let code = storage::unhashed::get_raw(well_known_keys::CODE).ok_or("Code not stored.")?;
		assert_eq!(code.len(), 4_000_000 as usize);
		Ok(())
	}

	#[benchmark(skip_meta)]
	fn set_storage(i: Linear<0, { 1_000 }>) -> Result<(), BenchmarkError> {
		// Set up i items to add
		let mut items = Vec::new();
		for j in 0..i {
			let hash = (i, j).using_encoded(T::Hashing::hash).as_ref().to_vec();
			items.push((hash.clone(), hash.clone()));
		}

		let items_to_verify = items.clone();

		#[extrinsic_call]
		set_storage(RawOrigin::Root, items);

		// Verify that they're actually in the storage.
		for (item, _) in items_to_verify {
			let value = storage::unhashed::get_raw(&item).ok_or("No value stored")?;
			assert_eq!(value, *item);
		}
		Ok(())
	}

	#[benchmark(skip_meta)]
	fn kill_storage(i: Linear<0, { 1_000 }>) -> Result<(), BenchmarkError> {
		// Add i items to storage
		let mut items = Vec::with_capacity(i as usize);
		for j in 0..i {
			let hash = (i, j).using_encoded(T::Hashing::hash).as_ref().to_vec();
			storage::unhashed::put_raw(&hash, &hash);
			items.push(hash);
		}

		// Verify that they're actually in the storage.
		for item in &items {
			let value = storage::unhashed::get_raw(item).ok_or("No value stored")?;
			assert_eq!(value, *item);
		}

		let items_to_verify = items.clone();

		#[extrinsic_call]
		kill_storage(RawOrigin::Root, items);

		// Verify that they're not in the storage anymore.
		for item in items_to_verify {
			assert!(storage::unhashed::get_raw(&item).is_none());
		}
		Ok(())
	}

	#[benchmark(skip_meta)]
	fn kill_prefix(p: Linear<0, { 1_000 }>) -> Result<(), BenchmarkError> {
		let prefix = p.using_encoded(T::Hashing::hash).as_ref().to_vec();
		let mut items = Vec::with_capacity(p as usize);
		// add p items that share a prefix
		for i in 0..p {
			let hash = (p, i).using_encoded(T::Hashing::hash).as_ref().to_vec();
			let key = [&prefix[..], &hash[..]].concat();
			storage::unhashed::put_raw(&key, &key);
			items.push(key);
		}

		// Verify that they're actually in the storage.
		for item in &items {
			let value = storage::unhashed::get_raw(item).ok_or("No value stored")?;
			assert_eq!(value, *item);
		}

		#[extrinsic_call]
		kill_prefix(RawOrigin::Root, prefix, p);

		// Verify that they're not in the storage anymore.
		for item in items {
			assert!(storage::unhashed::get_raw(&item).is_none());
		}
		Ok(())
	}

	#[benchmark]
	fn authorize_upgrade() -> Result<(), BenchmarkError> {
		let runtime_blob = T::prepare_set_code_data();
		T::setup_set_code_requirements(&runtime_blob)?;
		let hash = T::Hashing::hash(&runtime_blob);

		#[extrinsic_call]
		authorize_upgrade(RawOrigin::Root, hash);

		assert_eq!(System::<T>::authorized_upgrade().unwrap().code_hash(), &hash);
		Ok(())
	}

	#[benchmark]
	fn apply_authorized_upgrade() -> Result<(), BenchmarkError> {
		let runtime_blob = T::prepare_set_code_data();
		T::setup_set_code_requirements(&runtime_blob)?;
		let hash = T::Hashing::hash(&runtime_blob);
		// Will be heavier when it needs to do verification (i.e. don't use `...without_checks`).
		System::<T>::authorize_upgrade(RawOrigin::Root.into(), hash)?;

		#[extrinsic_call]
		apply_authorized_upgrade(RawOrigin::Root, runtime_blob);

		// Can't check for `CodeUpdated` in parachain upgrades. Just check that the authorization is
		// gone.
		assert!(System::<T>::authorized_upgrade().is_none());
		Ok(())
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Test);
}
