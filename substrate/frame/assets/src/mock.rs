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

//! Test environment for Assets pallet.

use super::*;
use crate as pallet_assets;

use codec::Encode;
use frame_support::{
	assert_ok, construct_runtime, derive_impl, parameter_types,
	traits::{AsEnsureOriginWithArg, ConstU32},
};
use sp_io::storage;
use sp_runtime::BuildStorage;

type Block = frame_system::mocking::MockBlock<Test>;

construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		Balances: pallet_balances,
		Assets: pallet_assets,
	}
);

type AccountId = u64;
type AssetId = u32;

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u64>;
	type MaxConsumers = ConstU32<3>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type AccountStore = System;
}

pub struct AssetsCallbackHandle;
impl AssetsCallback<AssetId, AccountId> for AssetsCallbackHandle {
	fn created(_id: &AssetId, _owner: &AccountId) -> Result<(), ()> {
		if Self::should_err() {
			Err(())
		} else {
			storage::set(Self::CREATED.as_bytes(), &().encode());
			Ok(())
		}
	}

	fn destroyed(_id: &AssetId) -> Result<(), ()> {
		if Self::should_err() {
			Err(())
		} else {
			storage::set(Self::DESTROYED.as_bytes(), &().encode());
			Ok(())
		}
	}
}

impl AssetsCallbackHandle {
	pub const CREATED: &'static str = "asset_created";
	pub const DESTROYED: &'static str = "asset_destroyed";

	const RETURN_ERROR: &'static str = "return_error";

	// Configures `Self` to return `Ok` when callbacks are invoked
	pub fn set_return_ok() {
		storage::clear(Self::RETURN_ERROR.as_bytes());
	}

	// Configures `Self` to return `Err` when callbacks are invoked
	pub fn set_return_error() {
		storage::set(Self::RETURN_ERROR.as_bytes(), &().encode());
	}

	// If `true`, callback should return `Err`, `Ok` otherwise.
	fn should_err() -> bool {
		storage::exists(Self::RETURN_ERROR.as_bytes())
	}
}

#[derive_impl(crate::config_preludes::TestDefaultConfig)]
impl Config for Test {
	type Currency = Balances;
	type CreateOrigin = AsEnsureOriginWithArg<frame_system::EnsureSigned<u64>>;
	type ForceOrigin = frame_system::EnsureRoot<u64>;
	type Freezer = TestFreezer;
	type Holder = TestHolder;
	type CallbackHandle = AssetsCallbackHandle;
	type AssetIdAllocator = AutoIncAssetId<Test>;
	type ReserveData = u128;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = AssetsBenchmarkHelper;
}

#[cfg(feature = "runtime-benchmarks")]
pub struct AssetsBenchmarkHelper;
#[cfg(feature = "runtime-benchmarks")]
impl<AssetIdParameter: From<u32>, ReserveIdParameter: From<u32>>
	BenchmarkHelper<AssetIdParameter, ReserveIdParameter> for AssetsBenchmarkHelper
{
	fn create_asset_id_parameter(id: u32) -> AssetIdParameter {
		id.into()
	}
	fn create_reserve_id_parameter(id: u32) -> ReserveIdParameter {
		id.into()
	}
}

use std::collections::HashMap;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Hook {
	Died(u32, u64),
}
parameter_types! {
	static Frozen: HashMap<(u32, u64), u64> = Default::default();
	static OnHold: HashMap<(u32, u64), u64> = Default::default();
	static Hooks: Vec<Hook> = Default::default();
}

pub struct TestHolder;
impl BalanceOnHold<u32, u64, u64> for TestHolder {
	fn balance_on_hold(asset: u32, who: &u64) -> Option<u64> {
		OnHold::get().get(&(asset, *who)).cloned()
	}

	fn died(asset: u32, who: &u64) {
		Hooks::mutate(|v| v.push(Hook::Died(asset, *who)))
	}

	fn contains_holds(asset: AssetId) -> bool {
		OnHold::get().iter().any(|((k, _), _)| &asset == k)
	}
}

pub(crate) fn set_balance_on_hold(asset: u32, who: u64, amount: u64) {
	OnHold::mutate(|v| {
		let amount_on_hold = v.get(&(asset, who)).unwrap_or(&0);

		if &amount > amount_on_hold {
			// Hold more funds
			let amount = amount - amount_on_hold;
			let f = DebitFlags { keep_alive: true, best_effort: false };
			assert_ok!(Assets::decrease_balance(asset, &who, amount, f, |_, _| Ok(())));
		} else {
			// Release funds on hold
			let amount = amount_on_hold - amount;
			assert_ok!(Assets::increase_balance(asset, &who, amount, |_| Ok(())));
		}

		// Asset amount still "exists", we just store it here
		v.insert((asset, who), amount);
	});
}

pub(crate) fn clear_balance_on_hold(asset: u32, who: u64) {
	OnHold::mutate(|v| {
		if let Some(amount) = v.remove(&(asset, who)) {
			if amount > 0 {
				assert_ok!(Assets::increase_balance(asset, &who, amount, |_| Ok(())));
			}
		}
	});
}
pub struct TestFreezer;
impl FrozenBalance<u32, u64, u64> for TestFreezer {
	fn frozen_balance(asset: u32, who: &u64) -> Option<u64> {
		Frozen::get().get(&(asset, *who)).cloned()
	}

	fn died(asset: u32, who: &u64) {
		Hooks::mutate(|v| v.push(Hook::Died(asset, *who)));

		// Sanity check: dead accounts have no balance.
		assert!(Assets::balance(asset, *who).is_zero());
	}

	/// Return a value that indicates if there are registered freezes for a given asset.
	fn contains_freezes(asset: AssetId) -> bool {
		Frozen::get().iter().any(|((k, _), _)| &asset == k)
	}
}

pub(crate) fn set_frozen_balance(asset: u32, who: u64, amount: u64) {
	Frozen::mutate(|v| {
		v.insert((asset, who), amount);
	});
}

pub(crate) fn clear_frozen_balance(asset: u32, who: u64) {
	Frozen::mutate(|v| {
		v.remove(&(asset, who));
	});
}

pub(crate) fn hooks() -> Vec<Hook> {
	Hooks::get().clone()
}

pub(crate) fn take_hooks() -> Vec<Hook> {
	Hooks::take()
}

pub(crate) fn new_test_ext() -> sp_io::TestExternalities {
	let mut storage = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();

	let config: pallet_assets::GenesisConfig<Test> = pallet_assets::GenesisConfig {
		assets: vec![
			// id, owner, is_sufficient, min_balance
			(999, 0, true, 1),
		],
		metadata: vec![
			// id, name, symbol, decimals
			(999, "Token Name".into(), "TOKEN".into(), 10),
		],
		accounts: vec![
			// id, account_id, balance
			(999, 1, 100),
		],
		next_asset_id: None,
		reserves: vec![],
	};

	config.assimilate_storage(&mut storage).unwrap();

	let mut ext: sp_io::TestExternalities = storage.into();
	// Clear thread local vars for https://github.com/paritytech/substrate/issues/10479.
	ext.execute_with(|| take_hooks());
	ext.execute_with(|| System::set_block_number(1));
	ext
}

pub fn build_and_execute(test: impl FnOnce()) {
	new_test_ext().execute_with(|| {
		test();
		Assets::do_try_state().expect("All invariants must hold after a test");
	})
}
