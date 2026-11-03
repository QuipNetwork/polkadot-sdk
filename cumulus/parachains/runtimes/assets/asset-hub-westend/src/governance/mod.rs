// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

//! Governance configurations for the Asset Hub runtime.

use super::*;
use crate::xcm_config::Collectives;
use frame_support::{
	parameter_types,
	traits::{tokens::UnityOrOuterConversion, EitherOf, EitherOfDiverse, FromContains},
};
use frame_system::EnsureRootWithSuccess;
use pallet_xcm::{EnsureXcm, IsVoiceOfBody};
use parachains_common::pay::{AccountIdToLocalLocation, LocalPay, VersionedLocatableAccount};
use polkadot_runtime_common::{
	impls::{ContainsParts, VersionedLocatableAsset},
	prod_or_fast,
};
use sp_runtime::{traits::IdentityLookup, Percent};
use xcm::latest::BodyId;

mod origins;
pub use origins::{
	pallet_custom_origins, AuctionAdmin, FellowshipAdmin, GeneralAdmin, LeaseAdmin,
	ReferendumCanceller, ReferendumKiller, Spender, StakingAdmin, Treasurer, WhitelistedCaller,
};
mod tracks;
pub use tracks::TracksInfo;
parameter_types! {
	pub const VoteLockingPeriod: BlockNumber = prod_or_fast!(7 * RC_DAYS, 1);
}

impl pallet_conviction_voting::Config for Runtime {
	type WeightInfo = weights::pallet_conviction_voting::WeightInfo<Self>;
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type VoteLockingPeriod = VoteLockingPeriod;
	type MaxVotes = ConstU32<512>;
	type MaxTurnout =
		frame_support::traits::tokens::currency::ActiveIssuanceOf<Balances, Self::AccountId>;
	type Polls = Referenda;
	type BlockNumberProvider = RelaychainDataProvider<Runtime>;
	type VotingHooks = ();
}

parameter_types! {
	pub const AlarmInterval: BlockNumber = 1;
	pub const SubmissionDeposit: Balance = 1 * 3 * CENTS;
	pub const UndecidingTimeout: BlockNumber = 14 * RC_DAYS;
}

impl origins::pallet_custom_origins::Config for Runtime {}

parameter_types! {
	// Fellows pluralistic body.
	pub const FellowsBodyId: BodyId = BodyId::Technical;
}

impl pallet_whitelist::Config for Runtime {
	type WeightInfo = weights::pallet_whitelist::WeightInfo<Self>;
	type RuntimeCall = RuntimeCall;
	type RuntimeEvent = RuntimeEvent;
	type WhitelistOrigin = EitherOfDiverse<
		EnsureRoot<Self::AccountId>,
		EnsureXcm<IsVoiceOfBody<Collectives, FellowsBodyId>>,
	>;
	type DispatchWhitelistedOrigin = EitherOf<EnsureRoot<Self::AccountId>, WhitelistedCaller>;
	type DeferredDispatchExpiration = ConstU32<{ 28 * RC_DAYS }>;
	type BlockNumberProvider = RelaychainDataProvider<Runtime>;
	type Preimages = Preimage;
}

impl pallet_referenda::Config for Runtime {
	type WeightInfo = weights::pallet_referenda::WeightInfo<Self>;
	type RuntimeCall = RuntimeCall;
	type RuntimeEvent = RuntimeEvent;
	type Scheduler = Scheduler;
	type Currency = Balances;
	type SubmitOrigin = frame_system::EnsureSigned<AccountId>;
	type CancelOrigin = EitherOf<EnsureRoot<AccountId>, ReferendumCanceller>;
	type KillOrigin = EitherOf<EnsureRoot<AccountId>, ReferendumKiller>;
	type Slash = pallet_dap::DapLegacyAdapter<Runtime, Balances>;
	type Votes = pallet_conviction_voting::VotesOf<Runtime>;
	type Tally = pallet_conviction_voting::TallyOf<Runtime>;
	type SubmissionDeposit = SubmissionDeposit;
	type MaxQueued = ConstU32<100>;
	type UndecidingTimeout = UndecidingTimeout;
	type AlarmInterval = AlarmInterval;
	type Tracks = TracksInfo;
	type Preimages = Preimage;
	type BlockNumberProvider = RelaychainDataProvider<Runtime>;
}

parameter_types! {
	pub const SpendPeriod: BlockNumber = 6 * DAYS;
	pub const TreasuryPalletId: PalletId = PalletId(*b"py/trsry");
	pub const PayoutSpendPeriod: BlockNumber = 30 * DAYS;

	pub const TipCountdown: BlockNumber = 1 * DAYS;
	pub const TipFindersFee: Percent = Percent::from_percent(20);
	pub const TipReportDepositBase: Balance = 100 * CENTS;
	pub const DataDepositPerByte: Balance = 1 * CENTS;
	pub const MaxApprovals: u32 = 100;
	pub const MaxAuthorities: u32 = 100_000;
	pub const MaxKeys: u32 = 10_000;
	pub const MaxPeerInHeartbeats: u32 = 10_000;
	pub const MaxBalance: Balance = Balance::max_value();
	pub TreasuryAccount: AccountId = Treasury::account_id();
}

pub type TreasurySpender = EitherOf<EnsureRootWithSuccess<AccountId, MaxBalance>, Spender>;
pub type TreasuryBalanceConverter = UnityOrOuterConversion<
	ContainsParts<
		FromContains<
			xcm_builder::IsChildSystemParachain<ParaId>,
			xcm_builder::IsParentsOnly<ConstU8<1>>,
		>,
	>,
	AssetRate,
>;

impl pallet_treasury::Config for Runtime {
	type PalletId = TreasuryPalletId;
	type Currency = Balances;
	type RejectOrigin = EitherOfDiverse<EnsureRoot<AccountId>, Treasurer>;
	type RuntimeEvent = RuntimeEvent;
	type SpendPeriod = SpendPeriod;
	// NOTE: Treasury burn is currently disabled. If ever enabled (`Burn > 0`), wire
	// `BurnDestination = Dap` so burned funds flow to the DAP buffer instead of being destroyed.
	type Burn = ();
	type BurnDestination = ();
	type MaxApprovals = MaxApprovals;
	type WeightInfo = weights::pallet_treasury::WeightInfo<Runtime>;
	type SpendFunds = ();
	type SpendOrigin = TreasurySpender;
	type AssetKind = VersionedLocatableAsset;
	type Beneficiary = VersionedLocatableAccount;
	type BeneficiaryLookup = IdentityLookup<Self::Beneficiary>;
	type Paymaster = LocalPay<NativeAndAllAssets, TreasuryAccount, xcm_config::LocationToAccountId>;
	type BalanceConverter = TreasuryBalanceConverter;
	type PayoutPeriod = PayoutSpendPeriod;
	type BlockNumberProvider = RelaychainDataProvider<Runtime>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = parachains_common::pay::benchmarks::LocalPayArguments<
		xcm_config::TrustBackedAssetsPalletIndex,
	>;
}

impl pallet_asset_rate::Config for Runtime {
	type WeightInfo = weights::pallet_asset_rate::WeightInfo<Runtime>;
	type RuntimeEvent = RuntimeEvent;
	type CreateOrigin = EnsureRoot<AccountId>;
	type RemoveOrigin = EnsureRoot<AccountId>;
	type UpdateOrigin = EnsureRoot<AccountId>;
	type Currency = Balances;
	type AssetKind = <Runtime as pallet_treasury::Config>::AssetKind;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = polkadot_runtime_common::impls::benchmarks::AssetRateArguments;
}

parameter_types! {
	pub const BountyValueMinimum: Balance = 200 * CENTS;
	pub const ChildBountyValueMinimum: Balance = BountyValueMinimum::get() / 10;
	pub const MaxActiveChildBountyCount: u32 = 100;
	pub const CuratorHoldReason: RuntimeHoldReason =
		RuntimeHoldReason::MultiAssetBounties(pallet_multi_asset_bounties::HoldReason::CuratorDeposit);
	pub const CuratorDepositFromValueMultiplier: Permill = Permill::from_percent(10);
	pub const CuratorDepositMin: Balance = 10 * CENTS;
	pub const CuratorDepositMax: Balance = 500 * CENTS;
}

impl pallet_multi_asset_bounties::Config for Runtime {
	type Balance = Balance;
	type RejectOrigin = EitherOfDiverse<EnsureRoot<AccountId>, Treasurer>;
	type SpendOrigin = TreasurySpender;
	type AssetKind = VersionedLocatableAsset;
	type Beneficiary = VersionedLocatableAccount;
	type BeneficiaryLookup = IdentityLookup<Self::Beneficiary>;
	type BountyValueMinimum = BountyValueMinimum;
	type ChildBountyValueMinimum = ChildBountyValueMinimum;
	type MaxActiveChildBountyCount = MaxActiveChildBountyCount;
	type WeightInfo = pallet_multi_asset_bounties::weights::SubstrateWeight<Runtime>;
	type FundingSource = pallet_multi_asset_bounties::PalletIdAsFundingSource<
		TreasuryPalletId,
		Runtime,
		AccountIdToLocalLocation,
	>;
	type BountySource = pallet_multi_asset_bounties::BountySourceFromPalletId<
		TreasuryPalletId,
		pallet_multi_asset_bounties::BountyAccountPrefix,
		Runtime,
		AccountIdToLocalLocation,
	>;
	type ChildBountySource = pallet_multi_asset_bounties::ChildBountySourceFromPalletId<
		TreasuryPalletId,
		pallet_multi_asset_bounties::ChildBountyAccountPrefix,
		Runtime,
		AccountIdToLocalLocation,
	>;
	type Paymaster = LocalPay<NativeAndAllAssets, AccountId, xcm_config::LocationToAccountId>;
	type BalanceConverter = TreasuryBalanceConverter;
	type Preimages = Preimage;
	type Consideration = HoldConsideration<
		AccountId,
		Balances,
		CuratorHoldReason,
		pallet_multi_asset_bounties::CuratorDepositAmount<
			CuratorDepositFromValueMultiplier,
			CuratorDepositMin,
			CuratorDepositMax,
			Balance,
		>,
		Balance,
	>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = parachains_common::pay::benchmarks::LocalPayWithSourceArguments<
		xcm_config::TrustBackedAssetsPalletIndex,
	>;
}
