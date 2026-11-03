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

//! Tests mock for `pallet-assets-precompiles`.

pub use super::*;
use frame_support::{derive_impl, parameter_types, traits::AsEnsureOriginWithArg};
use sp_runtime::BuildStorage;

#[frame_support::runtime]
mod runtime {
	#[runtime::runtime]
	#[runtime::derive(
		RuntimeCall,
		RuntimeEvent,
		RuntimeError,
		RuntimeOrigin,
		RuntimeTask,
		RuntimeHoldReason,
		RuntimeFreezeReason
	)]
	pub struct Test;

	#[runtime::pallet_index(0)]
	pub type System = frame_system;
	#[runtime::pallet_index(10)]
	pub type Balances = pallet_balances;
	#[runtime::pallet_index(11)]
	pub type Timestamp = pallet_timestamp;
	#[runtime::pallet_index(20)]
	pub type Assets = pallet_assets;
	#[runtime::pallet_index(21)]
	pub type Revive = pallet_revive;
	#[runtime::pallet_index(22)]
	pub type ForeignAssets = super::foreign_assets;
	#[runtime::pallet_index(23)]
	pub type Permit = super::permit;
}

type Block = frame_system::mocking::MockBlock<Test>;

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u128>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig as pallet_balances::DefaultConfig)]
impl pallet_balances::Config for Test {
	type Balance = u128;
	type AccountStore = System;
	type RuntimeFreezeReason = RuntimeFreezeReason;
	type FreezeIdentifier = RuntimeFreezeReason;
	type MaxFreezes = frame_support::traits::VariantCountOf<RuntimeFreezeReason>;
}

parameter_types! {
	pub const MinimumPeriod: u64 = 1;
}

impl pallet_timestamp::Config for Test {
	type Moment = u64;
	type OnTimestampSet = ();
	type MinimumPeriod = MinimumPeriod;
	type WeightInfo = ();
}

#[derive_impl(pallet_assets::config_preludes::TestDefaultConfig as pallet_assets::DefaultConfig)]
impl pallet_assets::Config for Test {
	type Balance = u128;
	type CreateOrigin = AsEnsureOriginWithArg<frame_system::EnsureSigned<u64>>;
	type ForceOrigin = frame_system::EnsureRoot<u64>;
	type Currency = Balances;
}

impl foreign_assets::pallet::Config for Test {
	type ForeignAssetId = u32;
	#[cfg(feature = "runtime-benchmarks")]
	type AssetsInstance = ();
}

parameter_types! {
	/// Test chain ID - use a distinct value to avoid masking chain ID handling bugs.
	/// 31337 is commonly used for local development chains (Hardhat default).
	pub const ChainId: u64 = 31337;
}

impl permit::pallet::Config for Test {
	type ChainId = ChainId;
	type WeightInfo = ();
}

#[derive_impl(pallet_revive::config_preludes::TestDefaultConfig)]
impl pallet_revive::Config for Test {
	type AddressMapper = pallet_revive::TestAccountMapper<Self>;
	type Balance = u128;
	type Currency = Balances;
	type Precompiles =
		(ERC20<Self, InlineIdConfig<0x0120>>, ERC20<Self, ForeignIdConfig<0x0220, Self>>);
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let t = RuntimeGenesisConfig {
		assets: pallet_assets::GenesisConfig {
			assets: vec![(1, 0, true, 1)],
			metadata: vec![],
			accounts: vec![(1, 1, 100)],
			next_asset_id: None,
			reserves: vec![],
		},
		system: Default::default(),
		balances: Default::default(),
		revive: Default::default(),
	}
	.build_storage()
	.unwrap();
	let mut ext: sp_io::TestExternalities = t.into();
	ext.register_extension(sp_keystore::KeystoreExt::new(
		sp_keystore::testing::MemoryKeystore::new(),
	));
	ext.execute_with(|| {
		System::set_block_number(1);
		// Set a reasonable timestamp for tests (e.g., 2024-01-01 00:00:00 UTC = 1704067200)
		pallet_timestamp::Pallet::<Test>::set_timestamp(1704067200000);
	});

	ext
}
