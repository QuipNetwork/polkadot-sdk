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

//! Benchmarks for the `ChargePGAS` transaction extension.
//!
//! The inner extension is pinned to `()` so the benchmarks measure only the wrapper's own
//! cost; runtimes add the inner extension's weight via its own `weight()` fn.

extern crate alloc;

use super::*;
use crate::{BenchmarkHelperTrait, Pallet};
use frame_benchmarking::v2::*;
use frame_support::{
	dispatch::{DispatchClass, DispatchInfo, PostDispatchInfo},
	pallet_prelude::Weight,
	traits::tokens::fungibles,
};
use frame_system::RawOrigin;
use sp_runtime::traits::{
	AsSystemOriginSigner, AsTransactionAuthorizedOrigin, DispatchTransaction, Dispatchable,
};

#[benchmarks(where
	T: Send + Sync,
	T::RuntimeOrigin: AsTransactionAuthorizedOrigin,
	T::RuntimeCall: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>
		+ From<frame_system::Call<T>>,
	BalanceOf<T>: Send + Sync + From<u64>,
	AssetIdOf<T>: Send + Sync,
	<T::RuntimeCall as Dispatchable>::RuntimeOrigin: AsSystemOriginSigner<T::AccountId> + Clone,
)]
mod benchmarks {
	use super::*;

	/// PGAS path: caller holds enough PGAS and the call matches the filter, so the fee is
	/// withdrawn into a credit and the unused portion is resolved back to the caller.
	#[benchmark]
	fn charge_pgas() {
		let caller: T::AccountId = account("caller", 0, 0);
		let initial: BalanceOf<T> = u64::MAX.into();
		<T as Config>::BenchmarkHelper::mint_pgas(&caller, T::PGASAssetId::get(), initial);

		let ext: ChargePGAS<T, ()> = ChargePGAS::<T, ()>::default();
		let call: T::RuntimeCall = frame_system::Call::<T>::remark { remark: alloc::vec![] }.into();
		let info = DispatchInfo {
			call_weight: Weight::from_parts(100, 0),
			class: DispatchClass::Normal,
			..Default::default()
		};
		let post_info = PostDispatchInfo {
			actual_weight: Some(Weight::from_parts(10, 0)),
			pays_fee: Default::default(),
		};

		let result;
		#[block]
		{
			result =
				ext.test_run(RawOrigin::Signed(caller.clone()).into(), &call, &info, 0, 0, |_| {
					Ok(post_info)
				});
		}
		assert!(result.unwrap().is_ok());
		let remaining = <T::Assets as fungibles::Inspect<T::AccountId>>::balance(
			T::PGASAssetId::get(),
			&caller,
		);
		assert!(remaining < initial, "PGAS should be charged on the PGAS path");
	}

	/// Skip path: caller holds some PGAS but not enough to cover the fee, so the extension falls
	/// through to the inner extension. Measures the overhead of the PGAS preamble (origin, filter,
	/// balance read) when the path is ultimately skipped.
	#[benchmark]
	fn charge_pgas_skip() {
		let caller: T::AccountId = account("caller", 0, 0);
		// Mint the asset's minimum balance so the caller has an `Assets::Account` entry.
		// Without one, `reducible_balance` returns early before reading the freezer storage,
		// so the worst-case storage path on the skip branch wouldn't be captured.
		let min_balance =
			<T::Assets as fungibles::Inspect<T::AccountId>>::minimum_balance(T::PGASAssetId::get());
		<T as Config>::BenchmarkHelper::mint_pgas(&caller, T::PGASAssetId::get(), min_balance);

		let ext: ChargePGAS<T, ()> = ChargePGAS::<T, ()>::default();
		let call: T::RuntimeCall = frame_system::Call::<T>::remark { remark: alloc::vec![] }.into();
		let info = DispatchInfo {
			call_weight: Weight::from_parts(10, 0),
			class: DispatchClass::Normal,
			..Default::default()
		};
		let post_info = PostDispatchInfo {
			actual_weight: Some(Weight::from_parts(10, 0)),
			pays_fee: Default::default(),
		};

		let fee = pallet_transaction_payment::Pallet::<T>::compute_fee(0, &info, Zero::zero());
		assert!(!fee.is_zero(), "skip path requires fee > 0 to exercise `pgas < fee`");
		let before = <T::Assets as fungibles::Inspect<T::AccountId>>::balance(
			T::PGASAssetId::get(),
			&caller,
		);
		assert!(before < fee, "caller must not hold enough PGAS to take the PGAS branch");
		let result;
		#[block]
		{
			result =
				ext.test_run(RawOrigin::Signed(caller.clone()).into(), &call, &info, 0, 0, |_| {
					Ok(post_info)
				});
		}
		assert!(result.unwrap().is_ok());
		let after = <T::Assets as fungibles::Inspect<T::AccountId>>::balance(
			T::PGASAssetId::get(),
			&caller,
		);
		assert_eq!(before, after, "PGAS must not be charged on the skip path");
	}

	impl_benchmark_test_suite!(
		Pallet,
		crate::mock::ExtBuilder::default().build(),
		crate::mock::Runtime
	);
}
