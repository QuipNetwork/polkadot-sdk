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

use crate as pallet_pgas_allowance;

use frame_support::{
	derive_impl,
	dispatch::DispatchClass,
	traits::{AsEnsureOriginWithArg, ConstU32, ConstU64, Contains, Get},
	weights::{IdentityFee, Weight},
};
use frame_system::EnsureRoot;
use pallet_transaction_payment::FungibleAdapter;
use sp_runtime::BuildStorage;

pub type AccountId = <Runtime as frame_system::Config>::AccountId;
pub type Balance = <Runtime as pallet_balances::Config>::Balance;
pub type AssetId = <Runtime as pallet_assets::Config>::AssetId;

type Block = frame_system::mocking::MockBlock<Runtime>;

pub const PGAS_ASSET_ID: AssetId = 42;
pub const ALICE: AccountId = 1;
pub const BOB: AccountId = 2;
pub const CHARLIE: AccountId = 3;

frame_support::construct_runtime!(
	pub enum Runtime {
		System: frame_system,
		Balances: pallet_balances,
		TransactionPayment: pallet_transaction_payment,
		Assets: pallet_assets,
		PgasAllowance: pallet_pgas_allowance,
	}
);

pub struct BlockWeights;
impl Get<frame_system::limits::BlockWeights> for BlockWeights {
	fn get() -> frame_system::limits::BlockWeights {
		frame_system::limits::BlockWeights::builder()
			.base_block(Weight::zero())
			.for_class(DispatchClass::all(), |weights| {
				weights.base_extrinsic = Weight::zero();
			})
			.for_class(DispatchClass::non_mandatory(), |weights| {
				weights.max_total = Weight::from_parts(1024, u64::MAX).into();
			})
			.build_or_panic()
	}
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Runtime {
	type BlockWeights = BlockWeights;
	type Block = Block;
	type AccountData = pallet_balances::AccountData<Balance>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Runtime {
	type ExistentialDeposit = ConstU64<1>;
	type AccountStore = System;
}

#[derive_impl(pallet_transaction_payment::config_preludes::TestDefaultConfig)]
impl pallet_transaction_payment::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type OnChargeTransaction = FungibleAdapter<Balances, ()>;
	type WeightToFee = IdentityFee<Balance>;
	type LengthToFee = IdentityFee<Balance>;
}

#[derive_impl(pallet_assets::config_preludes::TestDefaultConfig)]
impl pallet_assets::Config for Runtime {
	type Currency = Balances;
	type CreateOrigin = AsEnsureOriginWithArg<frame_system::EnsureSigned<AccountId>>;
	type ForceOrigin = EnsureRoot<AccountId>;
}

/// Filter that matches `frame_system` calls (used by tests via `System::remark` and by the
/// benchmarks). `Balances` and other calls fall through so the filter-miss path stays exercised.
pub struct PGASCallFilter;
impl Contains<RuntimeCall> for PGASCallFilter {
	fn contains(call: &RuntimeCall) -> bool {
		matches!(call, RuntimeCall::System(..))
	}
}

impl pallet_pgas_allowance::Config for Runtime {
	type Assets = Assets;
	type PGASAssetId = ConstU32<PGAS_ASSET_ID>;
	type CallFilter = PGASCallFilter;
	type WeightInfo = ();
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = BenchmarkHelper;
}

#[cfg(feature = "runtime-benchmarks")]
pub struct BenchmarkHelper;
#[cfg(feature = "runtime-benchmarks")]
impl pallet_pgas_allowance::BenchmarkHelperTrait<AccountId, AssetId, Balance> for BenchmarkHelper {
	fn mint_pgas(who: &AccountId, asset_id: AssetId, amount: Balance) {
		use frame_support::traits::tokens::fungibles::Mutate;
		<Assets as Mutate<AccountId>>::mint_into(asset_id, who, amount).unwrap();
	}
}

#[derive(Default)]
pub struct ExtBuilder {
	native_balances: Vec<(AccountId, Balance)>,
	pgas_balances: Vec<(AccountId, Balance)>,
}

impl ExtBuilder {
	pub fn with_native(mut self, balances: Vec<(AccountId, Balance)>) -> Self {
		self.native_balances = balances;
		self
	}

	pub fn with_pgas(mut self, balances: Vec<(AccountId, Balance)>) -> Self {
		self.pgas_balances = balances;
		self
	}

	pub fn build(self) -> sp_io::TestExternalities {
		let mut t = frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
		pallet_balances::GenesisConfig::<Runtime> {
			balances: self.native_balances.clone(),
			..Default::default()
		}
		.assimilate_storage(&mut t)
		.unwrap();

		pallet_assets::GenesisConfig::<Runtime> {
			assets: vec![(PGAS_ASSET_ID, ALICE, true, 1)],
			accounts: self
				.pgas_balances
				.iter()
				.map(|(who, bal)| (PGAS_ASSET_ID, *who, *bal))
				.collect(),
			..Default::default()
		}
		.assimilate_storage(&mut t)
		.unwrap();

		let mut ext: sp_io::TestExternalities = t.into();
		ext.execute_with(|| System::set_block_number(1));
		ext
	}
}

/// Build a `DispatchInfo` with the given call weight.
pub fn info_from_weight(w: Weight) -> frame_support::dispatch::DispatchInfo {
	frame_support::dispatch::DispatchInfo { call_weight: w, ..Default::default() }
}

/// Build a `PostDispatchInfo` reporting the given actual weight.
pub fn post_info_from_weight(w: Weight) -> frame_support::dispatch::PostDispatchInfo {
	frame_support::dispatch::PostDispatchInfo {
		actual_weight: Some(w),
		pays_fee: Default::default(),
	}
}

pub fn default_post_info() -> frame_support::dispatch::PostDispatchInfo {
	frame_support::dispatch::PostDispatchInfo { actual_weight: None, pays_fee: Default::default() }
}
