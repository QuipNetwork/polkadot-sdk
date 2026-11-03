// Copyright (C) 2022 Parity Technologies (UK) Ltd.
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

//! The Ambassador Program.
//!
//! The module defines the following on-chain functionality of the Ambassador Program:
//!
//! - Managed set of program members, where every member has a [rank](ranks)
//! (via [AmbassadorCollective](pallet_ranked_collective)).
//! - Referendum functionality for the program members to propose, vote on, and execute
//! proposals on behalf of the members of a certain [rank](Origin)
//! (via [AmbassadorReferenda](pallet_referenda)).
//! - Managed content (charter, announcements) (via [pallet_collective_content]).
//! - Promotion and demotion periods, register of members' activity, and rank based salaries
//! (via [AmbassadorCore](pallet_core_fellowship)).
//! - Members' salaries (via [AmbassadorSalary](pallet_salary), requiring a member to be
//! imported or inducted into [AmbassadorCore](pallet_core_fellowship)).

pub mod origins;
mod tracks;

use super::*;
use crate::xcm_config::{FellowshipAdminBodyId, WndAssetHub};
use frame_support::traits::{EitherOf, MapSuccess, TryMapSuccess};
use frame_system::EnsureRootWithSuccess;
pub use origins::pallet_origins as pallet_ambassador_origins;
use origins::pallet_origins::{
	EnsureAmbassadorsVoice, EnsureAmbassadorsVoiceFrom, EnsureHeadAmbassadorsVoice, Origin,
};
use sp_core::ConstU128;
use sp_runtime::traits::{CheckedReduceBy, ConstU16, ConvertToValue, Replace, ReplaceWithDefault};
use xcm::prelude::*;
use xcm_builder::{AliasesIntoAccountId32, PayOverXcm};

/// The Ambassador Program's member ranks.
pub mod ranks {
	use pallet_ranked_collective::Rank;

	#[allow(dead_code)]
	pub const CANDIDATE: Rank = 0;
	pub const AMBASSADOR_TIER_1: Rank = 1;
	pub const AMBASSADOR_TIER_2: Rank = 2;
	pub const SENIOR_AMBASSADOR_TIER_3: Rank = 3;
	pub const SENIOR_AMBASSADOR_TIER_4: Rank = 4;
	pub const HEAD_AMBASSADOR_TIER_5: Rank = 5;
	pub const HEAD_AMBASSADOR_TIER_6: Rank = 6;
	pub const HEAD_AMBASSADOR_TIER_7: Rank = 7;
	pub const MASTER_AMBASSADOR_TIER_8: Rank = 8;
	pub const MASTER_AMBASSADOR_TIER_9: Rank = 9;
}

impl pallet_ambassador_origins::Config for Runtime {}

pub type AmbassadorCollectiveInstance = pallet_ranked_collective::Instance2;

/// Demotion is by any of:
/// - Root can demote arbitrarily.
/// - the FellowshipAdmin origin (i.e. token holder referendum);
/// - a senior members vote by the rank two above the current rank.
pub type DemoteOrigin = EitherOf<
	frame_system::EnsureRootWithSuccess<AccountId, ConstU16<65535>>,
	EitherOf<
		MapSuccess<
			EnsureXcm<IsVoiceOfBody<GovernanceLocation, FellowshipAdminBodyId>>,
			Replace<ConstU16<{ ranks::MASTER_AMBASSADOR_TIER_9 }>>,
		>,
		TryMapSuccess<
			EnsureAmbassadorsVoiceFrom<ConstU16<{ ranks::SENIOR_AMBASSADOR_TIER_3 }>>,
			CheckedReduceBy<ConstU16<2>>,
		>,
	>,
>;

/// Promotion and approval (rank-retention) is by any of:
/// - Root can promote arbitrarily.
/// - the FellowshipAdmin origin (i.e. token holder referendum);
/// - a senior members vote by the rank two above the new/current rank.
/// - a member of rank `5` or above can add a candidate (rank `0`).
pub type PromoteOrigin = EitherOf<
	DemoteOrigin,
	TryMapSuccess<
		pallet_ranked_collective::EnsureMember<
			Runtime,
			AmbassadorCollectiveInstance,
			{ ranks::HEAD_AMBASSADOR_TIER_5 },
		>,
		Replace<ConstU16<0>>,
	>,
>;

/// Exchange is by any of:
/// - Root can exchange arbitrarily.
/// - the Fellows origin
pub type ExchangeOrigin = EitherOf<EnsureRootWithSuccess<AccountId, ConstU16<65535>>, Fellows>;

impl pallet_ranked_collective::Config<AmbassadorCollectiveInstance> for Runtime {
	type WeightInfo = weights::pallet_ranked_collective_ambassador_collective::WeightInfo<Runtime>;
	type RuntimeEvent = RuntimeEvent;
	type AddOrigin = MapSuccess<Self::PromoteOrigin, ReplaceWithDefault<()>>;
	type PromoteOrigin = PromoteOrigin;
	type DemoteOrigin = DemoteOrigin;
	type RemoveOrigin = Self::DemoteOrigin;
	type ExchangeOrigin = ExchangeOrigin;
	type Polls = AmbassadorReferenda;
	type MinRankOfClass = sp_runtime::traits::Identity;
	type MemberSwappedHandler = (crate::AmbassadorCore, crate::AmbassadorSalary);
	type VoteWeight = pallet_ranked_collective::Linear;
	type MaxMemberCount = ();
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkSetup = (crate::AmbassadorCore, crate::AmbassadorSalary);
}

parameter_types! {
	pub const AlarmInterval: BlockNumber = 1;
	pub const SubmissionDeposit: Balance = 0;
	pub const UndecidingTimeout: BlockNumber = 7 * DAYS;
}

pub type AmbassadorReferendaInstance = pallet_referenda::Instance2;

impl pallet_referenda::Config<AmbassadorReferendaInstance> for Runtime {
	type WeightInfo = weights::pallet_referenda_ambassador_referenda::WeightInfo<Runtime>;
	type RuntimeCall = RuntimeCall;
	type RuntimeEvent = RuntimeEvent;
	type Scheduler = Scheduler;
	type Currency = Balances;
	// A proposal can be submitted by a member of the Ambassador Program of
	// [ranks::SENIOR_AMBASSADOR_TIER_3] rank or higher.
	type SubmitOrigin = pallet_ranked_collective::EnsureMember<
		Runtime,
		AmbassadorCollectiveInstance,
		{ ranks::SENIOR_AMBASSADOR_TIER_3 },
	>;
	type CancelOrigin = EitherOf<EnsureRoot<AccountId>, EnsureHeadAmbassadorsVoice>;
	type KillOrigin = EitherOf<EnsureRoot<AccountId>, EnsureHeadAmbassadorsVoice>;
	type Slash = pallet_accumulate_and_forward::LegacyAdapter<Runtime, Balances>;
	type Votes = pallet_ranked_collective::Votes;
	type Tally = pallet_ranked_collective::TallyOf<Runtime, AmbassadorCollectiveInstance>;
	type SubmissionDeposit = SubmissionDeposit;
	type MaxQueued = ConstU32<20>;
	type UndecidingTimeout = UndecidingTimeout;
	type AlarmInterval = AlarmInterval;
	type Tracks = tracks::TracksInfo;
	type Preimages = Preimage;
	type BlockNumberProvider = System;
}

parameter_types! {
	pub const AnnouncementLifetime: BlockNumber = 180 * DAYS;
	pub const MaxAnnouncements: u32 = 50;
}

pub type AmbassadorContentInstance = pallet_collective_content::Instance1;

impl pallet_collective_content::Config<AmbassadorContentInstance> for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type CharterOrigin = EitherOf<EnsureRoot<AccountId>, EnsureHeadAmbassadorsVoice>;
	type AnnouncementLifetime = AnnouncementLifetime;
	// An announcement can be submitted by a Senior Ambassador member or an ambassador plurality
	// voice taken via referendum.
	type AnnouncementOrigin = EitherOfDiverse<
		pallet_ranked_collective::EnsureMember<
			Runtime,
			AmbassadorCollectiveInstance,
			{ ranks::SENIOR_AMBASSADOR_TIER_3 },
		>,
		EnsureAmbassadorsVoice,
	>;
	type MaxAnnouncements = MaxAnnouncements;
	type WeightInfo = weights::pallet_collective_content::WeightInfo<Runtime>;
}

pub type AmbassadorCoreInstance = pallet_core_fellowship::Instance2;

impl pallet_core_fellowship::Config<AmbassadorCoreInstance> for Runtime {
	type WeightInfo = weights::pallet_core_fellowship_ambassador_core::WeightInfo<Runtime>;
	type RuntimeEvent = RuntimeEvent;
	type Members = pallet_ranked_collective::Pallet<Runtime, AmbassadorCollectiveInstance>;
	type Balance = Balance;
	// Parameters are set by any of:
	// - Root;
	// - the FellowshipAdmin origin (i.e. token holder referendum);
	// - a vote among all Head Ambassadors.
	type ParamsOrigin = EitherOfDiverse<
		EnsureRoot<AccountId>,
		EitherOfDiverse<
			EnsureXcm<IsVoiceOfBody<GovernanceLocation, FellowshipAdminBodyId>>,
			EnsureHeadAmbassadorsVoice,
		>,
	>;
	// Induction (creating a candidate) is by any of:
	// - Root;
	// - the FellowshipAdmin origin (i.e. token holder referendum);
	// - a single Head Ambassador;
	// - a vote among all senior members.
	type InductOrigin = EitherOfDiverse<
		EnsureRoot<AccountId>,
		EitherOfDiverse<
			EnsureXcm<IsVoiceOfBody<GovernanceLocation, FellowshipAdminBodyId>>,
			EitherOfDiverse<
				pallet_ranked_collective::EnsureMember<
					Runtime,
					AmbassadorCollectiveInstance,
					{ ranks::HEAD_AMBASSADOR_TIER_5 },
				>,
				EnsureAmbassadorsVoiceFrom<ConstU16<{ ranks::SENIOR_AMBASSADOR_TIER_3 }>>,
			>,
		>,
	>;
	type ApproveOrigin = PromoteOrigin;
	type PromoteOrigin = PromoteOrigin;
	type FastPromoteOrigin = Self::PromoteOrigin;
	type EvidenceSize = ConstU32<65536>;
	type MaxRank = ConstU16<9>;
	type BlockNumberProvider = cumulus_pallet_parachain_system::RelaychainDataProvider<Runtime>;
}

pub type AmbassadorSalaryInstance = pallet_salary::Instance2;

parameter_types! {
	// The interior location on AssetHub for the paying account. This is the Ambassador Salary
	// pallet instance (which sits at index 74). This sovereign account will need funding.
	pub AmbassadorSalaryLocation: InteriorLocation = PalletInstance(74).into();
}

/// [`PayOverXcm`] setup to pay the Ambassador salary on the AssetHub in WND.
pub type AmbassadorSalaryPaymaster = PayOverXcm<
	AmbassadorSalaryLocation,
	crate::xcm_config::XcmConfig,
	crate::PolkadotXcm,
	ConstU32<{ 6 * HOURS }>,
	AccountId,
	(),
	ConvertToValue<WndAssetHub>,
	AliasesIntoAccountId32<(), AccountId>,
>;

impl pallet_salary::Config<AmbassadorSalaryInstance> for Runtime {
	type WeightInfo = weights::pallet_salary_ambassador_salary::WeightInfo<Runtime>;
	type RuntimeEvent = RuntimeEvent;

	#[cfg(not(feature = "runtime-benchmarks"))]
	type Paymaster = AmbassadorSalaryPaymaster;
	#[cfg(feature = "runtime-benchmarks")]
	type Paymaster = crate::impls::benchmarks::PayWithEnsure<
		AmbassadorSalaryPaymaster,
		crate::impls::benchmarks::OpenHrmpChannel<ConstU32<1000>>,
	>;
	type Members = pallet_ranked_collective::Pallet<Runtime, AmbassadorCollectiveInstance>;

	#[cfg(not(feature = "runtime-benchmarks"))]
	type Salary = pallet_core_fellowship::Pallet<Runtime, AmbassadorCoreInstance>;
	#[cfg(feature = "runtime-benchmarks")]
	type Salary = frame_support::traits::tokens::ConvertRank<
		crate::impls::benchmarks::RankToSalary<Balances>,
	>;
	// 15 days to register for a salary payment.
	type RegistrationPeriod = ConstU32<{ 15 * DAYS }>;
	// 15 days to claim the salary payment.
	type PayoutPeriod = ConstU32<{ 15 * DAYS }>;
	// Total monthly salary budget.
	type Budget = ConstU128<{ 10_000 * DOLLARS }>;
}
