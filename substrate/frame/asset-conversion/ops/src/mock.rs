// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//  http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Test environment for Asset Conversion Ops pallet.

use crate as pallet_asset_conversion_ops;
use core::default::Default;
use frame_support::{
	construct_runtime, derive_impl,
	instances::{Instance1, Instance2},
	ord_parameter_types, parameter_types,
	traits::{
		tokens::{
			fungible::{NativeFromLeft, NativeOrWithId, UnionOf},
			imbalance::ResolveAssetTo,
		},
		AsEnsureOriginWithArg, ConstU32, ConstU64,
	},
	PalletId,
};
use frame_system::{EnsureSigned, EnsureSignedBy};
use pallet_asset_conversion::{self, AccountIdConverter, AccountIdConverterNoSeed, Ascending};
use sp_arithmetic::Permill;
use sp_runtime::{traits::AccountIdConversion, BuildStorage};

type Block = frame_system::mocking::MockBlock<Test>;

construct_runtime!(
  pub enum Test
  {
	System: frame_system,
	Balances: pallet_balances,
	Assets: pallet_assets::<Instance1>,
	PoolAssets: pallet_assets::<Instance2>,
	AssetConversion: pallet_asset_conversion,
	AssetConversionOps: pallet_asset_conversion_ops,
  }
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u64>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type AccountStore = System;
}

#[derive_impl(pallet_assets::config_preludes::TestDefaultConfig)]
impl pallet_assets::Config<Instance1> for Test {
	type Currency = Balances;
	type CreateOrigin = AsEnsureOriginWithArg<EnsureSigned<Self::AccountId>>;
	type ForceOrigin = frame_system::EnsureRoot<Self::AccountId>;
	type Holder = ();
	type Freezer = ();
}

#[derive_impl(pallet_assets::config_preludes::TestDefaultConfig)]
impl pallet_assets::Config<Instance2> for Test {
	type Currency = Balances;
	type CreateOrigin =
		AsEnsureOriginWithArg<EnsureSignedBy<AssetConversionOrigin, Self::AccountId>>;
	type ForceOrigin = frame_system::EnsureRoot<Self::AccountId>;
	type Holder = ();
	type Freezer = ();
}

parameter_types! {
  pub const AssetConversionPalletId: PalletId = PalletId(*b"py/ascon");
  pub const Native: NativeOrWithId<u32> = NativeOrWithId::Native;
  pub LpFee: Permill = Permill::from_rational(3u32, 1_000u32); // 0.3%
  pub MaxSwapFee: Permill = Permill::from_percent(10);
  pub storage LiquidityWithdrawalFee: Permill = Permill::from_percent(0);
}

ord_parameter_types! {
  pub const AssetConversionOrigin: u64 = AccountIdConversion::<u64>::into_account_truncating(&AssetConversionPalletId::get());
}

pub type NativeAndAssets = UnionOf<Balances, Assets, NativeFromLeft, NativeOrWithId<u32>, u64>;
pub type PoolIdToAccountId =
	AccountIdConverter<AssetConversionPalletId, (NativeOrWithId<u32>, NativeOrWithId<u32>)>;
pub type AscendingLocator = Ascending<u64, NativeOrWithId<u32>, PoolIdToAccountId>;

impl pallet_asset_conversion::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type Balance = <Self as pallet_balances::Config>::Balance;
	type HigherPrecisionBalance = sp_core::U256;
	type AssetKind = NativeOrWithId<u32>;
	type Assets = NativeAndAssets;
	type PoolId = (Self::AssetKind, Self::AssetKind);
	type PoolLocator = AscendingLocator;
	type PoolAssetId = u32;
	type PoolAssets = PoolAssets;
	type PoolSetupFee = ConstU64<100>;
	type PoolSetupFeeAsset = Native;
	type PoolSetupFeeTarget = ResolveAssetTo<AssetConversionOrigin, Self::Assets>;
	type PalletId = AssetConversionPalletId;
	type WeightInfo = ();
	type LPFee = LpFee;
	type AdminOrigin = frame_system::EnsureRoot<Self::AccountId>;
	type MaxSwapFee = MaxSwapFee;
	type LiquidityWithdrawalFee = LiquidityWithdrawalFee;
	type MaxSwapPathLength = ConstU32<4>;
	type MintMinLiquidity = ConstU64<100>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = ();
}

pub type OldPoolIdToAccountId =
	AccountIdConverterNoSeed<(NativeOrWithId<u32>, NativeOrWithId<u32>)>;

impl pallet_asset_conversion_ops::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type PriorAccountIdConverter = OldPoolIdToAccountId;
	type AssetsRefund = NativeAndAssets;
	type PoolAssetsRefund = PoolAssets;
	type PoolAssetsTeam = PoolAssets;
	type DepositAsset = Balances;
	type WeightInfo = ();
}

pub(crate) fn new_test_ext() -> sp_io::TestExternalities {
	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();

	pallet_balances::GenesisConfig::<Test> {
		balances: vec![(1, 10000), (2, 20000), (3, 30000), (4, 40000)],
		..Default::default()
	}
	.assimilate_storage(&mut t)
	.unwrap();

	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| System::set_block_number(1));
	ext
}
