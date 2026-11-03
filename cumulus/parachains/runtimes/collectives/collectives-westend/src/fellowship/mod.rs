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

//! The Westend Technical Fellowship.

mod origins;
mod tracks;
use crate::{
	weights,
	xcm_config::{FellowshipAdminBodyId, TreasurerBodyId, UsdtAssetHub},
	AccountId, AssetRate, Balance, Balances, FellowshipReferenda, GovernanceLocation,
	ParachainInfo, Preimage, Runtime, RuntimeCall, RuntimeEvent, RuntimeOrigin, Scheduler, System,
	DAYS,
};
use cumulus_primitives_core::ParaId;
use frame_support::{
	parameter_types,
	traits::{
		tokens::UnityOrOuterConversion, EitherOf, EitherOfDiverse, FromContains, MapSuccess,
		OriginTrait, TryWithMorphedArg,
	},
	PalletId,
};
use frame_system::{EnsureRoot, EnsureRootWithSuccess};
pub use origins::{
	pallet_origins as pallet_fellowship_origins, Architects, EnsureCanPromoteTo, EnsureCanRetainAt,
	EnsureFellowship, Fellows, Masters, Members, ToVoice,
};
use pallet_ranked_collective::EnsureOfRank;
use pallet_xcm::{EnsureXcm, IsVoiceOfBody};
use polkadot_runtime_common::impls::{
	ContainsParts, LocatableAssetConverter, VersionedLocatableAsset, VersionedLocationConverter,
};
use sp_arithmetic::Permill;
use sp_core::{ConstU128, ConstU32, ConstU8};
use sp_runtime::traits::{ConstU16, ConvertToValue, IdentityLookup, Replace, TakeFirst};
use testnet_parachains_constants::westend::currency::GRAND;
use westend_runtime_constants::time::HOURS;
use xcm::prelude::*;
use xcm_builder::{AliasesIntoAccountId32, PayOverXcm};

#[cfg(feature = "runtime-benchmarks")]
use crate::impls::benchmarks::{OpenHrmpChannel, PayWithEnsure};

/// The Fellowship members' ranks.
pub mod ranks {
	use pallet_ranked_collective::Rank;

	pub const DAN_1: Rank = 1; // aka Members.
	pub const DAN_2: Rank = 2;
	pub const DAN_3: Rank = 3; // aka Fellows.
	pub const DAN_4: Rank = 4; // aka Architects.
	pub const DAN_5: Rank = 5;
	pub const DAN_6: Rank = 6;
	pub const DAN_7: Rank = 7; // aka Masters.
	pub const DAN_8: Rank = 8;
	pub const DAN_9: Rank = 9;
}

impl pallet_fellowship_origins::Config for Runtime {}

pub type FellowshipReferendaInstance = pallet_referenda::Instance1;

impl pallet_referenda::Config<FellowshipReferendaInstance> for Runtime {
	type WeightInfo = weights::pallet_referenda_fellowship_referenda::WeightInfo<Runtime>;
	type RuntimeCall = RuntimeCall;
	type RuntimeEvent = RuntimeEvent;
	type Scheduler = Scheduler;
	type Currency = Balances;
	// Fellows can submit proposals.
	type SubmitOrigin = EitherOf<
		pallet_ranked_collective::EnsureMember<Runtime, FellowshipCollectiveInstance, 3>,
		MapSuccess<
			TryWithMorphedArg<
				RuntimeOrigin,
				<RuntimeOrigin as OriginTrait>::PalletsOrigin,
				ToVoice,
				EnsureOfRank<Runtime, FellowshipCollectiveInstance>,
				(AccountId, u16),
			>,
			TakeFirst,
		>,
	>;
	type CancelOrigin = Architects;
	type KillOrigin = Masters;
	type Slash = pallet_accumulate_and_forward::LegacyAdapter<Runtime, Balances>;
	type Votes = pallet_ranked_collective::Votes;
	type Tally = pallet_ranked_collective::TallyOf<Runtime, FellowshipCollectiveInstance>;
	type SubmissionDeposit = ConstU128<0>;
	type MaxQueued = ConstU32<100>;
	type UndecidingTimeout = ConstU32<{ 7 * DAYS }>;
	type AlarmInterval = ConstU32<1>;
	type Tracks = tracks::TracksInfo;
	type Preimages = Preimage;
	type BlockNumberProvider = crate::System;
}

pub type FellowshipCollectiveInstance = pallet_ranked_collective::Instance1;

impl pallet_ranked_collective::Config<FellowshipCollectiveInstance> for Runtime {
	type WeightInfo = weights::pallet_ranked_collective_fellowship_collective::WeightInfo<Runtime>;
	type RuntimeEvent = RuntimeEvent;

	// Promotions and the induction of new members are serviced by `FellowshipCore` pallet instance.
	#[cfg(not(feature = "runtime-benchmarks"))]
	type AddOrigin = frame_system::EnsureNever<()>;
	#[cfg(feature = "runtime-benchmarks")]
	type AddOrigin = frame_system::EnsureRoot<Self::AccountId>;

	// The maximum value of `u16` set as a success value for the root to ensure the benchmarks will
	// pass.
	#[cfg(not(feature = "runtime-benchmarks"))]
	type PromoteOrigin = frame_system::EnsureNever<pallet_ranked_collective::Rank>;
	#[cfg(feature = "runtime-benchmarks")]
	type PromoteOrigin = EnsureRootWithSuccess<Self::AccountId, ConstU16<65535>>;

	// Demotion is by any of:
	// - Root can demote arbitrarily.
	// - the FellowshipAdmin origin (i.e. token holder referendum);
	//
	// The maximum value of `u16` set as a success value for the root to ensure the benchmarks will
	// pass.
	type RemoveOrigin = Self::DemoteOrigin;
	type DemoteOrigin = EitherOf<
		EnsureRootWithSuccess<Self::AccountId, ConstU16<65535>>,
		MapSuccess<
			EnsureXcm<IsVoiceOfBody<GovernanceLocation, FellowshipAdminBodyId>>,
			Replace<ConstU16<{ ranks::DAN_9 }>>,
		>,
	>;
	// Exchange is by any of:
	// - Root can exchange arbitrarily.
	// - the Fellows origin
	type ExchangeOrigin =
		EitherOf<EnsureRootWithSuccess<Self::AccountId, ConstU16<65535>>, Fellows>;
	type Polls = FellowshipReferenda;
	type MinRankOfClass = tracks::MinRankOfClass;
	type MemberSwappedHandler = (crate::FellowshipCore, crate::FellowshipSalary);
	type VoteWeight = pallet_ranked_collective::Geometric;
	type MaxMemberCount = ();
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkSetup = (crate::FellowshipCore, crate::FellowshipSalary);
}

pub type FellowshipCoreInstance = pallet_core_fellowship::Instance1;

impl pallet_core_fellowship::Config<FellowshipCoreInstance> for Runtime {
	type WeightInfo = weights::pallet_core_fellowship_fellowship_core::WeightInfo<Runtime>;
	type RuntimeEvent = RuntimeEvent;
	type Members = pallet_ranked_collective::Pallet<Runtime, FellowshipCollectiveInstance>;
	type Balance = Balance;
	// Parameters are set by any of:
	// - Root;
	// - the FellowshipAdmin origin (i.e. token holder referendum);
	// - a vote among all Fellows.
	type ParamsOrigin = EitherOfDiverse<
		EnsureXcm<IsVoiceOfBody<GovernanceLocation, FellowshipAdminBodyId>>,
		Fellows,
	>;
	// Induction (creating a candidate) is by any of:
	// - Root;
	// - the FellowshipAdmin origin (i.e. token holder referendum);
	// - a single Fellow;
	// - a vote among all Members.
	type InductOrigin = EitherOfDiverse<
		EnsureXcm<IsVoiceOfBody<GovernanceLocation, FellowshipAdminBodyId>>,
		EitherOfDiverse<
			pallet_ranked_collective::EnsureMember<
				Runtime,
				FellowshipCollectiveInstance,
				{ ranks::DAN_3 },
			>,
			Members,
		>,
	>;
	// Approval (rank-retention) of a Member's current rank is by any of:
	// - Root;
	// - the FellowshipAdmin origin (i.e. token holder referendum);
	// - a vote by the rank two above the current rank for all retention up to the Master rank.
	type ApproveOrigin = EitherOf<
		MapSuccess<
			EnsureXcm<IsVoiceOfBody<GovernanceLocation, FellowshipAdminBodyId>>,
			Replace<ConstU16<{ ranks::DAN_9 }>>,
		>,
		EnsureCanRetainAt,
	>;
	// Promotion is by any of:
	// - Root can promote arbitrarily.
	// - the FellowshipAdmin origin (i.e. token holder referendum);
	// - a vote by the rank two above the new rank for all promotions up to the Master rank.
	type PromoteOrigin = EitherOf<
		MapSuccess<
			EnsureXcm<IsVoiceOfBody<GovernanceLocation, FellowshipAdminBodyId>>,
			Replace<ConstU16<{ ranks::DAN_9 }>>,
		>,
		EnsureCanPromoteTo,
	>;
	type FastPromoteOrigin = Self::PromoteOrigin;
	type EvidenceSize = ConstU32<65536>;
	type MaxRank = ConstU16<9>;
	type BlockNumberProvider = cumulus_pallet_parachain_system::RelaychainDataProvider<Runtime>;
}

pub type FellowshipSalaryInstance = pallet_salary::Instance1;

parameter_types! {
	// The interior location on AssetHub for the paying account. This is the Fellowship Salary
	// pallet instance (which sits at index 64). This sovereign account will need funding.
	pub Interior: InteriorLocation = PalletInstance(64).into();
}

const USDT_UNITS: u128 = 1_000_000;

/// [`PayOverXcm`] setup to pay the Fellowship salary on the AssetHub in USDT.
pub type FellowshipSalaryPaymaster = PayOverXcm<
	Interior,
	crate::xcm_config::XcmConfig,
	crate::PolkadotXcm,
	ConstU32<{ 6 * HOURS }>,
	AccountId,
	(),
	ConvertToValue<UsdtAssetHub>,
	AliasesIntoAccountId32<(), AccountId>,
>;

impl pallet_salary::Config<FellowshipSalaryInstance> for Runtime {
	type WeightInfo = weights::pallet_salary_fellowship_salary::WeightInfo<Runtime>;
	type RuntimeEvent = RuntimeEvent;

	#[cfg(not(feature = "runtime-benchmarks"))]
	type Paymaster = FellowshipSalaryPaymaster;
	#[cfg(feature = "runtime-benchmarks")]
	type Paymaster = PayWithEnsure<FellowshipSalaryPaymaster, OpenHrmpChannel<ConstU32<1000>>>;
	type Members = pallet_ranked_collective::Pallet<Runtime, FellowshipCollectiveInstance>;

	#[cfg(not(feature = "runtime-benchmarks"))]
	type Salary = pallet_core_fellowship::Pallet<Runtime, FellowshipCoreInstance>;
	#[cfg(feature = "runtime-benchmarks")]
	type Salary = frame_support::traits::tokens::ConvertRank<
		crate::impls::benchmarks::RankToSalary<Balances>,
	>;
	// 15 days to register for a salary payment.
	type RegistrationPeriod = ConstU32<{ 15 * DAYS }>;
	// 15 days to claim the salary payment.
	type PayoutPeriod = ConstU32<{ 15 * DAYS }>;
	// Total monthly salary budget.
	type Budget = ConstU128<{ 100_000 * USDT_UNITS }>;
}

parameter_types! {
	pub const FellowshipTreasuryPalletId: PalletId =
		testnet_parachains_constants::westend::account::FELLOWSHIP_TREASURY_PALLET_ID;
	pub const HundredPercent: Permill = Permill::from_percent(100);
	pub const Burn: Permill = Permill::from_percent(0);
	pub const MaxBalance: Balance = Balance::max_value();
	// The asset's interior location for the paying account. This is the Fellowship Treasury
	// pallet instance (which sits at index 65).
	pub FellowshipTreasuryInteriorLocation: InteriorLocation = PalletInstance(65).into();
	pub SelfParaId: ParaId = ParachainInfo::parachain_id();
}

/// [`PayOverXcm`] setup to pay the Fellowship Treasury.
pub type FellowshipTreasuryPaymaster = PayOverXcm<
	FellowshipTreasuryInteriorLocation,
	crate::xcm_config::XcmConfig,
	crate::PolkadotXcm,
	ConstU32<{ 6 * HOURS }>,
	VersionedLocation,
	VersionedLocatableAsset,
	LocatableAssetConverter,
	VersionedLocationConverter,
>;

pub type FellowshipTreasuryInstance = pallet_treasury::Instance1;

impl pallet_treasury::Config<FellowshipTreasuryInstance> for Runtime {
	type WeightInfo = weights::pallet_treasury::WeightInfo<Runtime>;
	type PalletId = FellowshipTreasuryPalletId;
	type Currency = Balances;
	type RejectOrigin = EitherOfDiverse<
		EnsureRoot<AccountId>,
		EitherOfDiverse<EnsureXcm<IsVoiceOfBody<GovernanceLocation, TreasurerBodyId>>, Fellows>,
	>;
	type RuntimeEvent = RuntimeEvent;
	type SpendPeriod = ConstU32<{ 7 * DAYS }>;
	type Burn = Burn;
	// NOTE: Treasury burn is currently disabled (`Burn = 0`). If ever enabled, wire
	// `BurnDestination` to `pallet_accumulate_and_forward::LegacyAdapter` so burned funds
	// flow to the accumulation account instead of being destroyed. Note: `Pallet<T>` only
	// implements `OnUnbalanced<Credit>`; use `LegacyAdapter` for the legacy `NegativeImbalance`
	// path.
	type BurnDestination = ();
	type SpendFunds = ();
	type MaxApprovals = ConstU32<100>;
	type SpendOrigin = EitherOf<
		EitherOf<
			EnsureRootWithSuccess<AccountId, MaxBalance>,
			MapSuccess<
				EnsureXcm<IsVoiceOfBody<GovernanceLocation, TreasurerBodyId>>,
				Replace<ConstU128<{ 10_000 * GRAND }>>,
			>,
		>,
		EitherOf<
			MapSuccess<Architects, Replace<ConstU128<{ 10_000 * GRAND }>>>,
			MapSuccess<Fellows, Replace<ConstU128<{ 10 * GRAND }>>>,
		>,
	>;
	type AssetKind = VersionedLocatableAsset;
	type Beneficiary = VersionedLocation;
	type BeneficiaryLookup = IdentityLookup<Self::Beneficiary>;
	#[cfg(not(feature = "runtime-benchmarks"))]
	type Paymaster = FellowshipTreasuryPaymaster;
	#[cfg(feature = "runtime-benchmarks")]
	type Paymaster = PayWithEnsure<FellowshipTreasuryPaymaster, OpenHrmpChannel<ConstU32<1000>>>;
	type BalanceConverter = UnityOrOuterConversion<
		ContainsParts<
			FromContains<
				xcm_builder::IsSiblingSystemParachain<ParaId, SelfParaId>,
				xcm_builder::IsParentsOnly<ConstU8<1>>,
			>,
		>,
		AssetRate,
	>;
	type PayoutPeriod = ConstU32<{ 30 * DAYS }>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = polkadot_runtime_common::impls::benchmarks::TreasuryArguments<
		sp_core::ConstU8<1>,
		ConstU32<1000>,
	>;
	type BlockNumberProvider = System;
}
