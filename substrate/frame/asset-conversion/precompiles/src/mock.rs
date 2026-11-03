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

//! Test mock for `pallet-asset-conversion-precompiles`.

pub use super::*;
use frame_support::{
	construct_runtime, derive_impl,
	instances::{Instance1, Instance2},
	parameter_types,
	traits::{
		tokens::fungible::{NativeFromLeft, NativeOrWithId, UnionOf},
		AsEnsureOriginWithArg, ConstU32, ConstU64,
	},
	PalletId,
};
use frame_system::EnsureSignedBy;
use pallet_asset_conversion::{AccountIdConverter, Ascending, Chain, WithFirstAsset};
use pallet_revive::precompiles::H160;
use sp_runtime::{traits::AccountIdConversion, BuildStorage, Permill};

type Block = frame_system::mocking::MockBlock<Test>;

construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		Balances: pallet_balances,
		Assets: pallet_assets::<Instance1>,
		PoolAssets: pallet_assets::<Instance2>,
		AssetConversion: pallet_asset_conversion,
		Revive: pallet_revive,
	}
);

parameter_types! {
	pub const ExistentialDeposit: u64 = 100;
	pub const AssetConversionPalletId: PalletId = PalletId(*b"py/ascon");
	pub const Native: NativeOrWithId<u32> = NativeOrWithId::Native;
	pub LpFee: Permill = Permill::from_rational(3u32, 1_000u32); // 0.3%
	pub MaxSwapFee: Permill = Permill::from_percent(10);
	pub storage LiquidityWithdrawalFee: Permill = Permill::from_percent(0);
}

frame_support::ord_parameter_types! {
	pub const AssetConversionOrigin: u64 = AccountIdConversion::<u64>::into_account_truncating(&AssetConversionPalletId::get());
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u64>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig as pallet_balances::DefaultConfig)]
impl pallet_balances::Config for Test {
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
	type RuntimeFreezeReason = RuntimeFreezeReason;
	type FreezeIdentifier = RuntimeFreezeReason;
	type MaxFreezes = frame_support::traits::VariantCountOf<RuntimeFreezeReason>;
}

#[derive_impl(pallet_assets::config_preludes::TestDefaultConfig as pallet_assets::DefaultConfig)]
impl pallet_assets::Config<Instance1> for Test {
	type Currency = Balances;
	type CreateOrigin = AsEnsureOriginWithArg<frame_system::EnsureSigned<u64>>;
	type ForceOrigin = frame_system::EnsureRoot<u64>;
}

#[derive_impl(pallet_assets::config_preludes::TestDefaultConfig as pallet_assets::DefaultConfig)]
impl pallet_assets::Config<Instance2> for Test {
	type Currency = Balances;
	type CreateOrigin = AsEnsureOriginWithArg<EnsureSignedBy<AssetConversionOrigin, u64>>;
	type ForceOrigin = frame_system::EnsureRoot<u64>;
	type AssetDeposit = ConstU64<0>;
	type AssetAccountDeposit = ConstU64<0>;
	type MetadataDepositBase = ConstU64<0>;
	type MetadataDepositPerByte = ConstU64<0>;
	type ApprovalDeposit = ConstU64<0>;
}

pub type NativeAndAssets = UnionOf<Balances, Assets, NativeFromLeft, NativeOrWithId<u32>, u64>;
pub type PoolIdToAccountId =
	AccountIdConverter<AssetConversionPalletId, (NativeOrWithId<u32>, NativeOrWithId<u32>)>;
pub type AscendingLocator = Ascending<u64, NativeOrWithId<u32>, PoolIdToAccountId>;
pub type WithFirstAssetLocator =
	WithFirstAsset<Native, u64, NativeOrWithId<u32>, PoolIdToAccountId>;

impl pallet_asset_conversion::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type Balance = <Self as pallet_balances::Config>::Balance;
	type HigherPrecisionBalance = u128;
	type AssetKind = NativeOrWithId<u32>;
	type Assets = NativeAndAssets;
	type PoolId = (Self::AssetKind, Self::AssetKind);
	type PoolLocator = Chain<WithFirstAssetLocator, AscendingLocator>;
	type PoolAssetId = u32;
	type PoolAssets = PoolAssets;
	type PoolSetupFee = ConstU64<100>;
	type PoolSetupFeeAsset = Native;
	type PoolSetupFeeTarget = frame_support::traits::tokens::imbalance::ResolveAssetTo<
		AssetConversionOrigin,
		Self::Assets,
	>;
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

/// Precompile address for asset conversion: 0x00...04200000
pub const PRECOMPILE_ADDRESS: u16 = 0x0420;

#[derive_impl(pallet_revive::config_preludes::TestDefaultConfig)]
impl pallet_revive::Config for Test {
	type AddressMapper = pallet_revive::TestAccountMapper<Self>;
	type Balance = u64;
	type Currency = Balances;
	type Precompiles = (super::AssetConversion<PRECOMPILE_ADDRESS, Self>,);
}

/// Helper: create the precompile's fixed address.
pub fn precompile_address() -> H160 {
	let mut addr = [0u8; 20];
	addr[16..18].copy_from_slice(&PRECOMPILE_ADDRESS.to_be_bytes());
	H160(addr)
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();

	pallet_balances::GenesisConfig::<Test> {
		balances: vec![
			(1, 1_000_000),
			(2, 1_000_000),
			(3, 1_000_000),
			(555, 1_000_000_000_000_000),
		],
		..Default::default()
	}
	.assimilate_storage(&mut t)
	.unwrap();

	pallet_revive::GenesisConfig::<Test> { ..Default::default() }
		.assimilate_storage(&mut t)
		.unwrap();

	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| System::set_block_number(1));
	ext
}
