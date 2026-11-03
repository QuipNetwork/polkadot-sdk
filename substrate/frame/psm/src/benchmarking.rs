// This file is part of Substrate.

// Copyright (C) Amforc AG.
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

//! Benchmarking setup for pallet-psm

use super::*;
use crate::Pallet as Psm;
use alloc::boxed::Box;
use frame_benchmarking::v2::*;
use frame_support::traits::{
	fungibles::{
		metadata::Inspect as FungiblesMetadataInspect, Create as FungiblesCreate,
		Inspect as FungiblesInspect, Mutate as FungiblesMutate,
	},
	Consideration, EnsureOriginWithArg, Get,
};
use frame_system::RawOrigin;
use pallet::BalanceOf;
use sp_runtime::{Permill, Saturating};

/// Asset-ID indices passed to `BenchmarkHelper::get_asset_id`. Chosen to avoid
/// collision with typical genesis assets.
const INTERNAL_ASSET_INDEX: u32 = 50;
const EXTERNAL_ASSET_OFFSET: u32 = 100;
/// Minimum swap amount seeded for the benchmarked PSM, in internal-asset units.
const BENCH_MIN_SWAP: u32 = 1_000;

/// Ensure the benchmarked internal asset exists and a PSM record is installed for
/// it. Returns `(internal_asset_id, internal_decimals)`.
fn ensure_internal_setup<T: Config>() -> (T::AssetId, u8)
where
	T::Fungibles: FungiblesCreate<T::AccountId>,
{
	let admin: T::AccountId = whitelisted_caller();
	let _ = frame_system::Pallet::<T>::inc_providers(&admin);
	let internal_id: T::AssetId = T::BenchmarkHelper::get_asset_id(INTERNAL_ASSET_INDEX);
	if !T::Fungibles::asset_exists(internal_id.clone()) {
		let _ = T::Fungibles::create(internal_id.clone(), admin.clone(), true, 1u32.into());
	}
	let internal_decimals = T::Fungibles::decimals(internal_id.clone());
	if !crate::Psm::<T>::contains_key(&internal_id) {
		let origin = T::CreateOrigin::try_successful_origin(&internal_id)
			.expect("benchmark CreateOrigin is available");
		if let Some(depositor) = T::CreateOrigin::ensure_origin(origin.clone(), &internal_id)
			.expect("benchmark CreateOrigin succeeds")
		{
			T::Consideration::ensure_successful(&depositor, Psm::<T>::psm_creation_footprint());
		}
		let root_origin: T::PalletsOrigin = RawOrigin::Root.into();
		Psm::<T>::create_psm(
			origin,
			internal_id.clone(),
			Box::new(root_origin.clone()),
			Box::new(root_origin),
			admin,
			BalanceOf::<T>::from(u32::MAX).saturating_mul(1_000_000u32.into()),
			BalanceOf::<T>::from(BENCH_MIN_SWAP),
		)
		.expect("benchmark PSM creation succeeds");
	}
	(internal_id, internal_decimals)
}

/// Set up `n` external assets ready for PSM benchmarks. Returns
/// `(internal_asset_id, target_external_id)`.
///
/// Creates the target external asset (`EXTERNAL_ASSET_OFFSET`) and the internal
/// asset, registers `n` external assets, and configures ceiling weights so the
/// target can absorb the full mint amount.
///
/// Assets beyond the target are filler, they only populate PSM storage so the
/// iterators in `total_psm_debt()` and `max_asset_debt()` touch `n` entries
/// during `mint()`.
fn setup_assets<T: Config>(n: u32) -> (T::AssetId, T::AssetId)
where
	T::Fungibles: FungiblesCreate<T::AccountId>,
{
	let admin: T::AccountId = whitelisted_caller();
	let _ = frame_system::Pallet::<T>::inc_providers(&admin);

	let (internal_id, internal_decimals) = ensure_internal_setup::<T>();

	// Target asset: create + set metadata via the runtime-provided benchmark
	// helper. Setting metadata requires reserving a native deposit, which the
	// helper handles by funding `admin` first — something the fungibles traits
	// alone cannot express.
	let target_id: T::AssetId = T::BenchmarkHelper::get_asset_id(EXTERNAL_ASSET_OFFSET);
	if !T::Fungibles::asset_exists(target_id.clone()) {
		T::BenchmarkHelper::create_asset(target_id.clone(), &admin, internal_decimals);
	}

	// Filler assets only populate PSM storage so mint()'s iterators touch `n`
	// entries. They are never swapped against; we still seed `internal_decimals`
	// so the storage shape matches the target row.
	for i in 0..n {
		let id: T::AssetId = T::BenchmarkHelper::get_asset_id(EXTERNAL_ASSET_OFFSET + i);
		crate::ExternalAssets::<T>::insert(
			&internal_id,
			&id,
			crate::ExternalAssetInfo {
				status: CircuitBreakerLevel::AllEnabled,
				decimals: internal_decimals,
			},
		);
		crate::AssetCeilingWeight::<T>::insert(&internal_id, &id, Permill::from_percent(1));
		crate::PsmDebt::<T>::insert(&internal_id, &id, BalanceOf::<T>::from(1u32));
	}
	// Target-specific: dominant weight so it can absorb the full mint amount.
	crate::AssetCeilingWeight::<T>::insert(&internal_id, &target_id, Permill::from_percent(100));

	// Keep `external_count` consistent with the rows we wrote.
	crate::Psm::<T>::mutate(&internal_id, |maybe| {
		if let Some(info) = maybe.as_mut() {
			info.external_count = n.max(1);
		}
	});

	(internal_id, target_id)
}

#[benchmarks(
	where
		T::Fungibles: FungiblesCreate<T::AccountId>,
)]
mod benchmarks {
	use super::*;

	/// Linear in `n`. The number of registered external assets, because
	/// `total_psm_debt()` iterates `PsmDebt` and `max_asset_debt()` iterates
	/// `AssetCeilingWeight`.
	#[benchmark]
	fn mint(n: Linear<1, { T::MaxExternals::get() }>) -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let (internal_id, asset_id) = setup_assets::<T>(n);
		let mint_amount = BalanceOf::<T>::from(BENCH_MIN_SWAP).saturating_mul(10u32.into());

		T::Fungibles::mint_into(asset_id.clone(), &caller, mint_amount.saturating_mul(2u32.into()))
			.map_err(|_| BenchmarkError::Stop("Failed to fund caller"))?;

		let psm_account = Psm::<T>::psm_account(&internal_id);
		let reserve_before = T::Fungibles::balance(asset_id.clone(), &psm_account);

		#[extrinsic_call]
		_(
			RawOrigin::Signed(caller.clone()),
			internal_id.clone(),
			asset_id.clone(),
			mint_amount,
			MintingFee::<T>::get(&internal_id, &asset_id),
		);

		assert!(T::Fungibles::balance(asset_id, &psm_account) > reserve_before);
		Ok(())
	}

	#[benchmark]
	fn redeem() -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let (internal_id, asset_id) = setup_assets::<T>(1);
		let setup_amount = BalanceOf::<T>::from(BENCH_MIN_SWAP).saturating_mul(10u32.into());
		let redeem_amount = BalanceOf::<T>::from(BENCH_MIN_SWAP);

		T::Fungibles::mint_into(
			asset_id.clone(),
			&caller,
			setup_amount.saturating_mul(2u32.into()),
		)
		.map_err(|_| BenchmarkError::Stop("Failed to fund caller"))?;
		Psm::<T>::mint(
			RawOrigin::Signed(caller.clone()).into(),
			internal_id.clone(),
			asset_id.clone(),
			setup_amount,
			MintingFee::<T>::get(&internal_id, &asset_id),
		)
		.map_err(|_| BenchmarkError::Stop("Failed to setup reserve via mint"))?;

		let psm_account = Psm::<T>::psm_account(&internal_id);
		let reserve_before = T::Fungibles::balance(asset_id.clone(), &psm_account);

		#[extrinsic_call]
		_(
			RawOrigin::Signed(caller.clone()),
			internal_id.clone(),
			asset_id.clone(),
			redeem_amount,
			RedemptionFee::<T>::get(&internal_id, &asset_id),
		);

		assert!(T::Fungibles::balance(asset_id, &psm_account) < reserve_before);
		Ok(())
	}

	#[benchmark]
	fn set_minting_fee() -> Result<(), BenchmarkError> {
		let (internal_id, asset_id) = setup_assets::<T>(1);
		let new_fee = Permill::from_percent(2);

		#[extrinsic_call]
		_(RawOrigin::Root, internal_id.clone(), asset_id.clone(), new_fee);

		assert_eq!(crate::MintingFee::<T>::get(&internal_id, &asset_id), new_fee);
		Ok(())
	}

	#[benchmark]
	fn set_redemption_fee() -> Result<(), BenchmarkError> {
		let (internal_id, asset_id) = setup_assets::<T>(1);
		let new_fee = Permill::from_percent(2);

		#[extrinsic_call]
		_(RawOrigin::Root, internal_id.clone(), asset_id.clone(), new_fee);

		assert_eq!(crate::RedemptionFee::<T>::get(&internal_id, &asset_id), new_fee);
		Ok(())
	}

	#[benchmark]
	fn set_max_debt() -> Result<(), BenchmarkError> {
		let (internal_id, _) = ensure_internal_setup::<T>();
		let new_value = BalanceOf::<T>::from(123u32);

		#[extrinsic_call]
		_(RawOrigin::Root, internal_id.clone(), new_value);

		assert_eq!(crate::Psm::<T>::get(&internal_id).unwrap().max_debt, new_value);
		Ok(())
	}

	#[benchmark]
	fn set_asset_status() -> Result<(), BenchmarkError> {
		let (internal_id, asset_id) = setup_assets::<T>(1);
		let new_status = CircuitBreakerLevel::MintingDisabled;

		#[extrinsic_call]
		_(RawOrigin::Root, internal_id.clone(), asset_id.clone(), new_status);

		assert_eq!(
			crate::ExternalAssets::<T>::get(&internal_id, &asset_id).map(|e| e.status),
			Some(new_status),
		);
		Ok(())
	}

	#[benchmark]
	fn set_asset_ceiling_weight() -> Result<(), BenchmarkError> {
		let (internal_id, asset_id) = setup_assets::<T>(1);
		let new_weight = Permill::from_percent(50);

		#[extrinsic_call]
		_(RawOrigin::Root, internal_id.clone(), asset_id.clone(), new_weight);

		assert_eq!(crate::AssetCeilingWeight::<T>::get(&internal_id, &asset_id), new_weight);
		Ok(())
	}
	#[benchmark]
	fn add_external_asset() -> Result<(), BenchmarkError> {
		// Seed PsmInfo and ensure the internal asset exists; the extrinsic
		// reads the snapshot and compares it against live metadata.
		let (internal_id, internal_decimals) = ensure_internal_setup::<T>();
		let caller: T::AccountId = whitelisted_caller();
		let new_asset_id: T::AssetId = T::BenchmarkHelper::get_asset_id(EXTERNAL_ASSET_OFFSET);

		T::BenchmarkHelper::create_asset(new_asset_id.clone(), &caller, internal_decimals);

		#[extrinsic_call]
		_(RawOrigin::Root, internal_id.clone(), new_asset_id.clone());

		assert!(crate::ExternalAssets::<T>::contains_key(&internal_id, &new_asset_id));
		Ok(())
	}

	#[benchmark]
	fn remove_external_asset() -> Result<(), BenchmarkError> {
		let (internal_id, asset_id) = setup_assets::<T>(1);
		crate::PsmDebt::<T>::remove(&internal_id, &asset_id);

		#[extrinsic_call]
		_(RawOrigin::Root, internal_id.clone(), asset_id.clone());

		assert!(!crate::ExternalAssets::<T>::contains_key(&internal_id, &asset_id));
		Ok(())
	}

	#[benchmark]
	fn create_psm() -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let _ = frame_system::Pallet::<T>::inc_providers(&caller);

		// A fresh internal asset.
		let internal_id: T::AssetId = T::BenchmarkHelper::get_asset_id(INTERNAL_ASSET_INDEX + 1);
		if !T::Fungibles::asset_exists(internal_id.clone()) {
			let _ = T::Fungibles::create(internal_id.clone(), caller.clone(), true, 1u32.into());
		}

		// The origin permitted to create, and the optional account it resolves to for deposit
		// payment.
		let origin = T::CreateOrigin::try_successful_origin(&internal_id)
			.map_err(|_| BenchmarkError::Weightless)?;
		let maybe_depositor = T::CreateOrigin::ensure_origin(origin.clone(), &internal_id)
			.map_err(|_| BenchmarkError::Stop("CreateOrigin failed"))?;
		if let Some(who) = &maybe_depositor {
			T::Consideration::ensure_successful(who, Psm::<T>::psm_creation_footprint());
		}

		let admin: T::PalletsOrigin =
			maybe_depositor.map(RawOrigin::Signed).unwrap_or(RawOrigin::Root).into();
		let max_debt = BalanceOf::<T>::from(u32::MAX);
		let min_swap = BalanceOf::<T>::from(BENCH_MIN_SWAP);

		#[extrinsic_call]
		_(
			origin,
			internal_id.clone(),
			Box::new(admin.clone()),
			Box::new(admin),
			caller,
			max_debt,
			min_swap,
		);

		assert!(crate::Psm::<T>::contains_key(&internal_id));
		Ok(())
	}

	#[benchmark]
	fn remove_psm() -> Result<(), BenchmarkError> {
		// `ensure_internal_setup` installs a PSM with `Root` as full admin, no externals and
		// no debt: exactly the preconditions for removal.
		let (internal_id, _) = ensure_internal_setup::<T>();

		#[extrinsic_call]
		_(RawOrigin::Root, internal_id.clone());

		assert!(!crate::Psm::<T>::contains_key(&internal_id));
		Ok(())
	}

	#[benchmark]
	fn set_full_admin() -> Result<(), BenchmarkError> {
		let (internal_id, _) = ensure_internal_setup::<T>();
		let new_admin: T::PalletsOrigin = RawOrigin::Signed(whitelisted_caller()).into();

		#[extrinsic_call]
		_(RawOrigin::Root, internal_id.clone(), Box::new(new_admin.clone()));

		assert_eq!(crate::PsmAdmin::<T>::get(&internal_id).unwrap().full_admin, new_admin);
		Ok(())
	}

	#[benchmark]
	fn set_emergency_admin() -> Result<(), BenchmarkError> {
		let (internal_id, _) = ensure_internal_setup::<T>();
		let new_admin: T::PalletsOrigin = RawOrigin::Signed(whitelisted_caller()).into();

		#[extrinsic_call]
		_(RawOrigin::Root, internal_id.clone(), Box::new(new_admin.clone()));

		assert_eq!(crate::PsmAdmin::<T>::get(&internal_id).unwrap().emergency_admin, new_admin);
		Ok(())
	}

	impl_benchmark_test_suite!(Psm, crate::mock::new_test_ext(), crate::mock::Test);
}
