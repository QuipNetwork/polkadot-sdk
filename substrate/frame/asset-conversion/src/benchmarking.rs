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

//! Asset Conversion pallet benchmarking.

use super::*;
use crate::Pallet as AssetConversion;
use alloc::vec;
use core::marker::PhantomData;
use frame_benchmarking::{v2::*, whitelisted_caller};
use frame_support::{
	assert_ok,
	traits::{
		fungible::NativeOrWithId,
		fungibles::{Create, Inspect, Mutate, Refund},
		EnsureOrigin,
	},
};
use frame_system::RawOrigin as SystemOrigin;
use sp_core::Get;
use sp_runtime::Permill;

/// Benchmark Helper
pub trait BenchmarkHelper<AssetKind> {
	/// Returns a valid assets pair for the pool creation.
	///
	/// When a specific asset, such as the native asset, is required in every pool, it should be
	/// returned for each odd-numbered seed.
	fn create_pair(seed1: u32, seed2: u32) -> (AssetKind, AssetKind);
}

impl<AssetKind> BenchmarkHelper<AssetKind> for ()
where
	AssetKind: From<u32>,
{
	fn create_pair(seed1: u32, seed2: u32) -> (AssetKind, AssetKind) {
		(seed1.into(), seed2.into())
	}
}

/// Factory for creating a valid asset pairs with [`NativeOrWithId::Native`] always leading in the
/// pair.
pub struct NativeOrWithIdFactory<AssetId>(PhantomData<AssetId>);
impl<AssetId: From<u32> + Ord> BenchmarkHelper<NativeOrWithId<AssetId>>
	for NativeOrWithIdFactory<AssetId>
{
	fn create_pair(seed1: u32, seed2: u32) -> (NativeOrWithId<AssetId>, NativeOrWithId<AssetId>) {
		if seed1.is_multiple_of(2) {
			(NativeOrWithId::WithId(seed2.into()), NativeOrWithId::Native)
		} else {
			(NativeOrWithId::Native, NativeOrWithId::WithId(seed2.into()))
		}
	}
}

/// Provides a pair of amounts expected to serve as sufficient initial liquidity for a pool.
fn valid_liquidity_amount<T: Config>(ed1: T::Balance, ed2: T::Balance) -> (T::Balance, T::Balance)
where
	T::Assets: Inspect<T::AccountId>,
{
	let l =
		ed1.max(ed2) + T::MintMinLiquidity::get() + T::MintMinLiquidity::get() + T::Balance::one();
	(l, l)
}

/// Create the `asset` and mint the `amount` for the `caller`.
fn create_asset<T: Config>(
	caller: &T::AccountId,
	asset: &T::AssetKind,
	amount: T::Balance,
	is_sufficient: bool,
) where
	T::Assets: Create<T::AccountId> + Mutate<T::AccountId>,
{
	if !T::Assets::asset_exists(asset.clone()) {
		assert_ok!(T::Assets::create(
			asset.clone(),
			caller.clone(),
			is_sufficient,
			T::Balance::one()
		));
	}
	assert_ok!(T::Assets::mint_into(
		asset.clone(),
		&caller,
		amount + T::Assets::minimum_balance(asset.clone())
	));
}

/// Create the designated fee asset for pool creation.
fn create_fee_asset<T: Config>(caller: &T::AccountId)
where
	T::Assets: Create<T::AccountId> + Mutate<T::AccountId>,
{
	let fee_asset = T::PoolSetupFeeAsset::get();
	if !T::Assets::asset_exists(fee_asset.clone()) {
		assert_ok!(T::Assets::create(fee_asset.clone(), caller.clone(), true, T::Balance::one()));
	}
	assert_ok!(T::Assets::mint_into(
		fee_asset.clone(),
		&caller,
		T::Assets::minimum_balance(fee_asset)
	));
}

/// Mint the fee asset for the `caller` sufficient to cover the fee for creating a new pool.
fn mint_setup_fee_asset<T: Config>(
	caller: &T::AccountId,
	asset1: &T::AssetKind,
	asset2: &T::AssetKind,
	lp_token: &T::PoolAssetId,
) where
	T::Assets: Create<T::AccountId> + Mutate<T::AccountId>,
{
	assert_ok!(T::Assets::mint_into(
		T::PoolSetupFeeAsset::get(),
		&caller,
		T::PoolSetupFee::get() +
			T::Assets::deposit_required(asset1.clone()) +
			T::Assets::deposit_required(asset2.clone()) +
			T::PoolAssets::deposit_required(lp_token.clone())
	));
}

/// Creates a pool for a given asset pair.
///
/// This action mints the necessary amounts of the given assets for the `caller` to provide initial
/// liquidity. It returns the LP token ID along with a pair of amounts sufficient for the pool's
/// initial liquidity.
fn create_asset_and_pool<T: Config>(
	caller: &T::AccountId,
	asset1: &T::AssetKind,
	asset2: &T::AssetKind,
) -> (T::PoolAssetId, T::Balance, T::Balance)
where
	T::Assets: Create<T::AccountId> + Mutate<T::AccountId>,
{
	let (liquidity1, liquidity2) = valid_liquidity_amount::<T>(
		T::Assets::minimum_balance(asset1.clone()),
		T::Assets::minimum_balance(asset2.clone()),
	);
	create_asset::<T>(caller, asset1, liquidity1, true);
	create_asset::<T>(caller, asset2, liquidity2, true);
	let lp_token = AssetConversion::<T>::get_next_pool_asset_id();

	mint_setup_fee_asset::<T>(caller, asset1, asset2, &lp_token);

	assert_ok!(AssetConversion::<T>::create_pool(
		SystemOrigin::Signed(caller.clone()).into(),
		Box::new(asset1.clone()),
		Box::new(asset2.clone())
	));

	(lp_token, liquidity1, liquidity2)
}

fn assert_last_event<T: Config>(generic_event: <T as Config>::RuntimeEvent) {
	let events = frame_system::Pallet::<T>::events();
	let system_event: <T as frame_system::Config>::RuntimeEvent = generic_event.into();
	// compare to the last event record
	let frame_system::EventRecord { event, .. } = &events[events.len() - 1];
	assert_eq!(event, &system_event);
}

#[benchmarks(where T::Assets: Create<T::AccountId> + Mutate<T::AccountId>, T::PoolAssetId: Into<u32>,)]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn create_pool() {
		let caller: T::AccountId = whitelisted_caller();
		let (asset1, asset2) = T::BenchmarkHelper::create_pair(0, 1);
		create_asset::<T>(&caller, &asset1, T::Assets::minimum_balance(asset1.clone()), true);
		create_asset::<T>(&caller, &asset2, T::Assets::minimum_balance(asset2.clone()), true);

		let lp_token = AssetConversion::<T>::get_next_pool_asset_id();
		create_fee_asset::<T>(&caller);
		mint_setup_fee_asset::<T>(&caller, &asset1, &asset2, &lp_token);

		#[extrinsic_call]
		_(SystemOrigin::Signed(caller.clone()), Box::new(asset1.clone()), Box::new(asset2.clone()));

		let pool_id = T::PoolLocator::pool_id(&asset1, &asset2).unwrap();
		let pool_account = T::PoolLocator::address(&pool_id).unwrap();
		assert_last_event::<T>(
			Event::PoolCreated { creator: caller, pool_account, pool_id, lp_token }.into(),
		);
	}

	#[benchmark]
	fn create_pool_with_fee() -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let (asset1, asset2) = T::BenchmarkHelper::create_pair(0, 1);
		create_asset::<T>(&caller, &asset1, T::Assets::minimum_balance(asset1.clone()), true);
		create_asset::<T>(&caller, &asset2, T::Assets::minimum_balance(asset2.clone()), true);

		let lp_token = AssetConversion::<T>::get_next_pool_asset_id();
		create_fee_asset::<T>(&caller);
		mint_setup_fee_asset::<T>(&caller, &asset1, &asset2, &lp_token);

		let fee = Permill::from_percent(1);
		let origin =
			T::AdminOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;

		#[extrinsic_call]
		_(
			origin as T::RuntimeOrigin,
			caller.clone(),
			Box::new(asset1.clone()),
			Box::new(asset2.clone()),
			fee,
		);

		let pool_id = T::PoolLocator::pool_id(&asset1, &asset2).unwrap();
		assert_eq!(PoolFees::<T>::get(&pool_id), Some(fee));
		assert_last_event::<T>(Event::PoolFeeSet { pool_id, fee }.into());
		Ok(())
	}

	#[benchmark]
	fn set_pool_fee() -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let (asset1, asset2) = T::BenchmarkHelper::create_pair(0, 1);

		create_fee_asset::<T>(&caller);
		let _ = create_asset_and_pool::<T>(&caller, &asset1, &asset2);

		let pool_id = T::PoolLocator::pool_id(&asset1, &asset2).unwrap();
		let fee = Permill::from_percent(1);
		let origin =
			T::AdminOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, pool_id.clone(), fee);

		assert_eq!(PoolFees::<T>::get(&pool_id), Some(fee));
		assert_last_event::<T>(Event::PoolFeeSet { pool_id, fee }.into());
		Ok(())
	}

	#[benchmark]
	fn add_liquidity() {
		let caller: T::AccountId = whitelisted_caller();
		let (asset1, asset2) = T::BenchmarkHelper::create_pair(0, 1);

		create_fee_asset::<T>(&caller);
		let (lp_token, liquidity1, liquidity2) =
			create_asset_and_pool::<T>(&caller, &asset1, &asset2);

		#[extrinsic_call]
		_(
			SystemOrigin::Signed(caller.clone()),
			Box::new(asset1.clone()),
			Box::new(asset2.clone()),
			liquidity1,
			liquidity2,
			T::Balance::one(),
			T::Balance::zero(),
			caller.clone(),
		);

		let pool_account = T::PoolLocator::pool_address(&asset1, &asset2).unwrap();
		let lp_minted =
			AssetConversion::<T>::calc_lp_amount_for_zero_supply(&liquidity1, &liquidity2).unwrap();
		assert_eq!(T::PoolAssets::balance(lp_token, &caller), lp_minted);
		assert_eq!(T::Assets::balance(asset1, &pool_account), liquidity1);
		assert_eq!(T::Assets::balance(asset2, &pool_account), liquidity2);
	}

	#[benchmark]
	fn remove_liquidity() {
		let caller: T::AccountId = whitelisted_caller();
		let (asset1, asset2) = T::BenchmarkHelper::create_pair(0, 1);

		create_fee_asset::<T>(&caller);
		let (lp_token, liquidity1, liquidity2) =
			create_asset_and_pool::<T>(&caller, &asset1, &asset2);

		let remove_lp_amount = T::Balance::one();

		assert_ok!(AssetConversion::<T>::add_liquidity(
			SystemOrigin::Signed(caller.clone()).into(),
			Box::new(asset1.clone()),
			Box::new(asset2.clone()),
			liquidity1,
			liquidity2,
			T::Balance::one(),
			T::Balance::zero(),
			caller.clone(),
		));
		let total_supply =
			<T::PoolAssets as Inspect<T::AccountId>>::total_issuance(lp_token.clone());

		#[extrinsic_call]
		_(
			SystemOrigin::Signed(caller.clone()),
			Box::new(asset1),
			Box::new(asset2),
			remove_lp_amount,
			T::Balance::zero(),
			T::Balance::zero(),
			caller.clone(),
		);

		let new_total_supply = <T::PoolAssets as Inspect<T::AccountId>>::total_issuance(lp_token);
		assert_eq!(new_total_supply, total_supply - remove_lp_amount);
	}

	#[benchmark]
	fn swap_exact_tokens_for_tokens(n: Linear<2, { T::MaxSwapPathLength::get() }>) {
		let mut swap_amount = T::Balance::one();
		let mut path = vec![];

		let caller: T::AccountId = whitelisted_caller();
		create_fee_asset::<T>(&caller);
		for n in 1..n {
			let (asset1, asset2) = T::BenchmarkHelper::create_pair(n - 1, n);
			swap_amount = swap_amount + T::Balance::one();
			if path.len() == 0 {
				path = vec![Box::new(asset1.clone()), Box::new(asset2.clone())];
			} else {
				path.push(Box::new(asset2.clone()));
			}

			let (_, liquidity1, liquidity2) = create_asset_and_pool::<T>(&caller, &asset1, &asset2);

			assert_ok!(AssetConversion::<T>::add_liquidity(
				SystemOrigin::Signed(caller.clone()).into(),
				Box::new(asset1.clone()),
				Box::new(asset2.clone()),
				liquidity1,
				liquidity2,
				T::Balance::one(),
				T::Balance::zero(),
				caller.clone(),
			));
		}

		let asset_in = *path.first().unwrap().clone();
		assert_ok!(T::Assets::mint_into(
			asset_in.clone(),
			&caller,
			swap_amount + T::Balance::one()
		));
		let init_caller_balance = T::Assets::balance(asset_in.clone(), &caller);

		#[extrinsic_call]
		_(
			SystemOrigin::Signed(caller.clone()),
			path,
			swap_amount,
			T::Balance::one(),
			caller.clone(),
			true,
		);

		let actual_balance = T::Assets::balance(asset_in, &caller);
		assert_eq!(actual_balance, init_caller_balance - swap_amount);
	}

	#[benchmark]
	fn swap_tokens_for_exact_tokens(n: Linear<2, { T::MaxSwapPathLength::get() }>) {
		let mut max_swap_amount = T::Balance::one();
		let mut path = vec![];

		let caller: T::AccountId = whitelisted_caller();
		create_fee_asset::<T>(&caller);
		for n in 1..n {
			let (asset1, asset2) = T::BenchmarkHelper::create_pair(n - 1, n);
			max_swap_amount = max_swap_amount + T::Balance::one() + T::Balance::one();
			if path.len() == 0 {
				path = vec![Box::new(asset1.clone()), Box::new(asset2.clone())];
			} else {
				path.push(Box::new(asset2.clone()));
			}

			let (_, liquidity1, liquidity2) = create_asset_and_pool::<T>(&caller, &asset1, &asset2);

			assert_ok!(AssetConversion::<T>::add_liquidity(
				SystemOrigin::Signed(caller.clone()).into(),
				Box::new(asset1.clone()),
				Box::new(asset2.clone()),
				liquidity1,
				liquidity2,
				T::Balance::one(),
				T::Balance::zero(),
				caller.clone(),
			));
		}

		let asset_in = *path.first().unwrap().clone();
		let asset_out = *path.last().unwrap().clone();
		assert_ok!(T::Assets::mint_into(asset_in, &caller, max_swap_amount));
		let init_caller_balance = T::Assets::balance(asset_out.clone(), &caller);

		#[extrinsic_call]
		_(
			SystemOrigin::Signed(caller.clone()),
			path,
			T::Balance::one(),
			max_swap_amount,
			caller.clone(),
			true,
		);

		let actual_balance = T::Assets::balance(asset_out, &caller);
		assert_eq!(actual_balance, init_caller_balance + T::Balance::one());
	}

	#[benchmark]
	fn touch(n: Linear<0, 3>) {
		let caller: T::AccountId = whitelisted_caller();
		let (asset1, asset2) = T::BenchmarkHelper::create_pair(0, 1);
		let pool_id = T::PoolLocator::pool_id(&asset1, &asset2).unwrap();
		let pool_account = T::PoolLocator::address(&pool_id).unwrap();

		create_fee_asset::<T>(&caller);
		create_asset::<T>(&caller, &asset1, <T as Config>::Balance::one(), false);
		create_asset::<T>(&caller, &asset2, <T as Config>::Balance::one(), false);
		let lp_token = AssetConversion::<T>::get_next_pool_asset_id();
		mint_setup_fee_asset::<T>(&caller, &asset1, &asset2, &lp_token);

		assert_ok!(AssetConversion::<T>::create_pool(
			SystemOrigin::Signed(caller.clone()).into(),
			Box::new(asset1.clone()),
			Box::new(asset2.clone())
		));

		if n > 0 &&
			<T as Config>::Assets::deposit_held(asset1.clone(), pool_account.clone()).is_some()
		{
			let _ = <T as Config>::Assets::refund(asset1.clone(), pool_account.clone());
		}
		if n > 1 &&
			<T as Config>::Assets::deposit_held(asset2.clone(), pool_account.clone()).is_some()
		{
			let _ = <T as Config>::Assets::refund(asset2.clone(), pool_account.clone());
		}
		if n > 2 &&
			<T as Config>::PoolAssets::deposit_held(lp_token.clone(), pool_account.clone())
				.is_some()
		{
			let _ = <T as Config>::PoolAssets::refund(lp_token, pool_account);
		}

		#[extrinsic_call]
		_(SystemOrigin::Signed(caller.clone()), Box::new(asset1.clone()), Box::new(asset2.clone()));

		assert_last_event::<T>(Event::Touched { pool_id, who: caller }.into());
	}

	#[benchmark]
	fn get_reserves() {
		let caller: T::AccountId = whitelisted_caller();
		let (asset1, asset2) = T::BenchmarkHelper::create_pair(0, 1);

		create_fee_asset::<T>(&caller);
		let (_, liquidity1, liquidity2) = create_asset_and_pool::<T>(&caller, &asset1, &asset2);

		assert_ok!(AssetConversion::<T>::add_liquidity(
			SystemOrigin::Signed(caller.clone()).into(),
			Box::new(asset1.clone()),
			Box::new(asset2.clone()),
			liquidity1,
			liquidity2,
			T::Balance::one(),
			T::Balance::zero(),
			caller.clone(),
		));

		#[block]
		{
			AssetConversion::<T>::get_reserves(asset1, asset2).expect("pool has liquidity; qed");
		}
	}

	impl_benchmark_test_suite!(AssetConversion, crate::mock::new_test_ext(), crate::mock::Test);
}
