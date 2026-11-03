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

//! Benchmarks for `pallet-vesting-precompiles`.
//!
//! Only the view functions (`vestingBalance`, `vestingBalanceOf`) are benchmarked here.
//! The mutating calls (`vest`, `vestOther`) delegate to `pallet-vesting` dispatchables
//! and charge the pallet's own benchmarked dispatch weight via
//! `get_dispatch_info().call_weight`, so no separate benchmark is needed.

#![cfg(feature = "runtime-benchmarks")]

use crate::{
	IVesting, Vesting, VestingBalance,
	pallet::{Config, Pallet},
};
use alloy_core::sol_types::SolValue;
use frame_benchmarking::v2::*;
use frame_support::traits::{Get, VestingSchedule, fungible::Mutate};
use pallet_revive::{
	AddressMapper,
	precompiles::{Precompile, U256},
};
use sp_runtime::traits::Zero;

type FungibleOf<T> = <T as pallet_revive::Config>::Currency;

/// Set up a vesting schedule for `who`.
///
/// Uses `MinVestedTransfer` as the locked amount to satisfy runtime constraints,
/// and funds the account with enough balance to cover the lock.
fn setup_vesting<T: Config>(who: &T::AccountId) -> VestingBalance<T>
where
	VestingBalance<T>: Into<U256>,
	VestingBalance<T>: From<<T as pallet_revive::Config>::Balance>,
	<T as pallet_revive::Config>::Balance: From<VestingBalance<T>>,
{
	let locked = T::MinVestedTransfer::get();
	// Fund the account with 10x the locked amount.
	let fund_amount: U256 = (locked * 10u32.into()).into();
	let balance = fund_amount.try_into().ok().expect("balance fits");
	FungibleOf::<T>::set_balance(who, balance);

	let per_block = (locked / 20u32.into()).max(1u32.into());
	let starting_block = Zero::zero();
	<pallet_vesting::Pallet<T> as VestingSchedule<T::AccountId>>::add_vesting_schedule(
		who,
		locked,
		per_block,
		starting_block,
	)
	.expect("adding vesting schedule should succeed");

	locked
}

#[benchmarks(
	where
		VestingBalance<T>: Into<U256>,
		VestingBalance<T>: From<<T as pallet_revive::Config>::Balance>,
		<T as pallet_revive::Config>::Balance: From<VestingBalance<T>>,
)]
mod benchmarks {
	use super::*;
	fn precompile_address<T: Config>() -> [u8; 20]
	where
		VestingBalance<T>: Into<U256>,
		VestingBalance<T>: From<<T as pallet_revive::Config>::Balance>,
		<T as pallet_revive::Config>::Balance: From<VestingBalance<T>>,
	{
		Vesting::<T>::MATCHER.base_address()
	}

	/// Benchmark `vestingBalance()`: query locked balance for the caller (with schedule).
	#[benchmark]
	fn vesting_balance() {
		let mut call_setup = pallet_revive::call_builder::CallSetup::<T>::default();
		let caller_account = call_setup.contract().caller.clone();

		setup_vesting::<T>(&caller_account);

		let input = IVesting::IVestingCalls::vestingBalance(IVesting::vestingBalanceCall {});
		let address = precompile_address::<T>();
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result = Vesting::<T>::call(&address, &input, &mut ext);
		}
		let raw_data = result.unwrap();
		let balance = U256::from_big_endian(&<[u8; 32]>::abi_decode(&raw_data).unwrap());
		assert!(balance > U256::zero(), "locked balance should be non-zero");
	}

	/// Benchmark `vestingBalanceOf(target)`: query locked balance for another account.
	#[benchmark]
	fn vesting_balance_of() {
		let mut call_setup = pallet_revive::call_builder::CallSetup::<T>::default();

		let target_addr = pallet_revive::precompiles::H160::from_low_u64_be(0xBEEF);
		let target_account = T::AddressMapper::to_account_id(&target_addr);
		setup_vesting::<T>(&target_account);

		let input = IVesting::IVestingCalls::vestingBalanceOf(IVesting::vestingBalanceOfCall {
			target: alloy_core::primitives::Address::from_slice(target_addr.as_bytes()),
		});
		let address = precompile_address::<T>();
		let (mut ext, _) = call_setup.ext();

		let result;
		#[block]
		{
			result = Vesting::<T>::call(&address, &input, &mut ext);
		}
		let raw_data = result.unwrap();
		let balance = U256::from_big_endian(&<[u8; 32]>::abi_decode(&raw_data).unwrap());
		assert!(balance > U256::zero(), "locked balance should be non-zero");
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Test);
}
