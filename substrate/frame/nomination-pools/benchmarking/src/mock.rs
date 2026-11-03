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

use crate::VoterBagsListInstance;
use frame_election_provider_support::VoteWeight;
use frame_support::{
	derive_impl,
	pallet_prelude::*,
	parameter_types,
	traits::{ConstBool, Nothing, VariantCountOf},
	PalletId,
};
use sp_runtime::{
	traits::{BlockNumberProvider, Convert, IdentityLookup},
	BuildStorage, FixedU128, Perbill,
};

type AccountId = u128;
type BlockNumber = u64;
type Balance = u128;

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Runtime {
	type AccountId = AccountId;
	type Lookup = IdentityLookup<Self::AccountId>;
	type Block = Block;
	type AccountData = pallet_balances::AccountData<Balance>;
}

parameter_types! {
	pub const ExistentialDeposit: Balance = 10;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Runtime {
	type Balance = Balance;
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
	type FreezeIdentifier = RuntimeFreezeReason;
	type MaxFreezes = VariantCountOf<RuntimeFreezeReason>;
	type RuntimeHoldReason = RuntimeHoldReason;
	type RuntimeFreezeReason = RuntimeFreezeReason;
}

parameter_types! {
	pub static BondingDuration: u32 = 3;
}

/// A mock `RcClientInterface` for benchmarks that don't need session/validator-set management.
pub struct MockRcClient;
impl pallet_staking_async_rc_client::RcClientInterface for MockRcClient {
	type AccountId = AccountId;

	fn validator_set(
		_new_validator_set: Vec<Self::AccountId>,
		_id: u32,
		_prune_up_to: Option<u32>,
	) {
	}
}

#[derive_impl(pallet_staking_async::config_preludes::TestDefaultConfig)]
impl pallet_staking_async::Config for Runtime {
	type OldCurrency = Balances;
	type Currency = Balances;
	type AdminOrigin = frame_system::EnsureRoot<Self::AccountId>;
	type EraPayout = ();
	type DisableMinting = ConstBool<true>;
	type BondingDuration = BondingDuration;
	type RewardPots = pallet_staking_async::SequentialTest;
	type ElectionProvider =
		frame_election_provider_support::NoElection<(AccountId, BlockNumber, Staking, (), ())>;
	type VoterList = VoterList;
	type TargetList = pallet_staking_async::UseValidatorsMap<Self>;
	type EventListeners = (Pools, DelegatedStaking);
	type RcClientInterface = MockRcClient;
}

parameter_types! {
	pub static BagThresholds: &'static [VoteWeight] = &[10, 20, 30, 40, 50, 60, 1_000, 2_000, 10_000];
}

impl pallet_bags_list::Config<VoterBagsListInstance> for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = ();
	type BagThresholds = BagThresholds;
	type ScoreProvider = Staking;
	type Score = VoteWeight;
	type MaxAutoRebagPerBlock = ();
}

pub struct BalanceToU256;
impl Convert<Balance, sp_core::U256> for BalanceToU256 {
	fn convert(n: Balance) -> sp_core::U256 {
		n.into()
	}
}

pub struct U256ToBalance;
impl Convert<sp_core::U256, Balance> for U256ToBalance {
	fn convert(n: sp_core::U256) -> Balance {
		n.try_into().unwrap()
	}
}

/// Always reports block 0 so commission `throttle_from` is deterministic.
/// While benchmarking on AH, nom-pools `BlockNumberProvider` will be `RelaychainDataProvider`.
pub struct BenchmarkBlockNumberProvider;
impl BlockNumberProvider for BenchmarkBlockNumberProvider {
	type BlockNumber = BlockNumber;
	fn current_block_number() -> Self::BlockNumber {
		0
	}
}

parameter_types! {
	pub static MaxUnbondingPools: u32 = 13;
	pub const PoolsPalletId: PalletId = PalletId(*b"py/nopls");
	pub const MaxPointsToBalance: u8 = 10;
}

impl pallet_nomination_pools::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = ();
	type Currency = Balances;
	type RuntimeFreezeReason = RuntimeFreezeReason;
	type RewardCounter = FixedU128;
	type BalanceToU256 = BalanceToU256;
	type U256ToBalance = U256ToBalance;
	type StakeAdapter =
		pallet_nomination_pools::adapter::DelegateStake<Self, Staking, DelegatedStaking>;
	type MaxUnbondingPools = MaxUnbondingPools;
	type MaxMetadataLen = ConstU32<256>;
	type MaxUnbonding = ConstU32<8>;
	type PalletId = PoolsPalletId;
	type MaxPointsToBalance = MaxPointsToBalance;
	type AdminOrigin = frame_system::EnsureRoot<Self::AccountId>;
	type BlockNumberProvider = BenchmarkBlockNumberProvider;
	type Filter = Nothing;
}

parameter_types! {
	pub const DelegatedStakingPalletId: PalletId = PalletId(*b"py/dlstk");
	pub const SlashRewardFraction: Perbill = Perbill::from_percent(1);
}
impl pallet_delegated_staking::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type PalletId = DelegatedStakingPalletId;
	type Currency = Balances;
	type OnSlash = ();
	type SlashRewardFraction = SlashRewardFraction;
	type RuntimeHoldReason = RuntimeHoldReason;
	type CoreStaking = Staking;
}

impl crate::Config for Runtime {}

type Block = frame_system::mocking::MockBlock<Runtime>;

frame_support::construct_runtime!(
	pub enum Runtime {
		System: frame_system,
		Balances: pallet_balances,
		Staking: pallet_staking_async,
		VoterList: pallet_bags_list::<Instance1>,
		Pools: pallet_nomination_pools,
		DelegatedStaking: pallet_delegated_staking,
	}
);

pub fn new_test_ext() -> sp_io::TestExternalities {
	let mut storage = frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
	let _ = pallet_nomination_pools::GenesisConfig::<Runtime> {
		min_join_bond: 2,
		min_create_bond: 2,
		max_pools: Some(3),
		max_members_per_pool: Some(3),
		max_members: Some(3 * 3),
		global_max_commission: Some(Perbill::from_percent(50)),
	}
	.assimilate_storage(&mut storage);
	sp_io::TestExternalities::from(storage)
}
