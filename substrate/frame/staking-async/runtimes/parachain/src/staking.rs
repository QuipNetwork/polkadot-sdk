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

/// ! Staking, and election related pallet configurations.
use super::*;
use cumulus_primitives_core::relay_chain::SessionIndex;
use frame_election_provider_support::{ElectionDataProvider, SequentialPhragmen};
use frame_support::traits::{ConstU128, EitherOf};
use pallet_election_provider_multi_block::{self as multi_block, SolutionAccuracyOf};
use pallet_staking_async::UseValidatorsMap;
use pallet_staking_async_rc_client as rc_client;
use polkadot_runtime_common::{prod_or_fast, BalanceToU256, U256ToBalance};
use sp_core::Get;
use sp_npos_elections::BalancingConfig;
use sp_runtime::{
	traits::Convert, transaction_validity::TransactionPriority, FixedPointNumber, FixedU128,
	SaturatedConversion,
};
use xcm::latest::prelude::*;
use xcm_executor::XcmExecutor as XcmExec;

pub(crate) fn enable_dot_preset(fast: bool) {
	Pages::set(&32);
	MinerPages::set(&4);
	MaxElectingVoters::set(&22_500);
	TargetSnapshotPerBlock::set(&2000);
	if !fast {
		SignedValidationPhase::set(&(8 * Pages::get()));
		SignedPhase::set(&(20 * MINUTES));
	}
}

pub(crate) fn enable_ksm_preset(fast: bool) {
	Pages::set(&16);
	MinerPages::set(&4);
	MaxElectingVoters::set(&12_500);
	TargetSnapshotPerBlock::set(&2500);
	if !fast {
		SignedValidationPhase::set(&(4 * Pages::get()));
		SignedPhase::set(&(20 * MINUTES));
	}
}

// This macro contains all of the variable parameters that we intend to use for Polkadot and
// Kusama.
//
// Note that this runtime has 3 broad presets:
//
// 1. dev: fast development preset.
// 2. dot-size: as close to Polkadot as possible.
// 3. ksm-size: as close to Kusama as possible.
//
// The default values here are related to `dev`. The above helper functions are used at launch (see
// `build_state` runtime-api) to enable dot/ksm presets.
parameter_types! {
	/// Number of election pages that we operate upon.
	///
	/// * Polkadot: 32 (3.2m snapshot)
	/// * Kusama: 16 (1.6m snapshot)
	///
	/// Reasoning: Both leads to around 700 nominators per-page, yielding the weights in
	/// https://github.com/paritytech/polkadot-sdk/pull/8704, the maximum of which being around 1mb
	/// compressed PoV and 2mb uncompressed.
	///
	/// NOTE: in principle, there is nothing preventing us from stretching these values further, it
	/// will only reduce the per-page POVs. Although, some operations like the first snapshot, and
	/// the last page of export (where we operate on `MaxValidatorSet` validators) will not get any
	/// better.
	pub storage Pages: u32 = 4;

	/// * Polkadot: 16 * 32 (512 blocks, 51.2m).
	/// * Kusama: 8 * 16 (12 blocks, 12.8m).
	///
	/// (MaxSubmissions * Pages) for both, enough to verify all solutions.
	///
	/// Reasoning: Less security needed in Kusama, to compensate for the shorter session duration.
	pub storage SignedValidationPhase: u32 = Pages::get() * 2;

	/// * Polkadot: 200 blocks, 20m.
	/// * Kusama: 100 blocks, 10m.
	///
	/// Reasoning:
	///
	/// * Polkadot wishes at least 8 submitters to be able to submit. That is  8 * 32 = 256 pages
	///   for all submitters. Weight of each submission page is roughly 0.0007 of block weight. 200
	///   blocks is more than enough.
	/// * Kusama wishes at least 4 submitters to be able to submit. That is 4 * 16 = 64 pages for
	///   all submitters. Weight of each submission page is roughly 0.0007 of block weight. 100
	///   blocks is more than enough.
	///
	/// See `signed_weight_ratios` test below for more info.
	pub storage SignedPhase: u32 = 4 * MINUTES;

	/// * Polkadot: 4
	/// * Kusama: 4
	///
	/// Reasoning: with 4 pages, the `ElectionScore` computed in both Kusama and Polkadot is pretty
	/// good. See and run `run_election_with_pages` below to see. With 4 pages, roughly 2800
	/// nominators will be elected. This is not great for staking reward, but is good enough for
	/// chain's economic security.
	pub storage MinerPages: u32 = 4;

	/// * Polkadot: 300 blocks, 30m
	/// * Kusama: 150 blocks, 15m
	///
	/// Reasoning: The only criteria is for the phase to be long enough such that the OCW miner is
	/// able to run the mining code at least twice. Note that `OffchainRepeat` limits execution of
	/// the OCW to at most 4 times per round, for faster collators.
	///
	/// Benchmarks logs from tests below are:
	///
	/// * exec_time of polkadot miner in WASM with 4 pages is 27369ms
	/// * exec_time of kusama miner in WASM with 4 pages is 23848ms
	///
	/// See `max_ocw_miner_pages_as_per_weights` test below.
	pub storage UnsignedPhase: u32 = MINUTES;

	/// * Polkadot: 22_500
	/// * Kusama: 12_500
	///
	/// Reasoning: Yielding  703 nominators per page in both. See [`Pages`] for more info. Path to
	/// Upgrade: We may wish to increase the number of "active nominators" in both networks by 1)
	/// increasing the `Pages` and `MaxElectingVoters` in sync. This update needs to happen while an
	/// election is NOT ongoing.
	pub storage MaxElectingVoters: u32 = 1000;

	/// * Polkadot: 2000 (always equal to `staking.maxValidatorCount`)
	/// * Kusama: 4000 (always equal to `staking.maxValidatorCount`)
	///
	/// Reasoning: As of now, we don't have a way to sort validators, so we wish to select all of
	/// them. In case this limit is reached, governance should introduce `minValidatorBond`, and
	/// validators would have to compete with their self-stake to force-chill one another. More
	/// info: SRL-417
	pub storage TargetSnapshotPerBlock: u32 = 4000;

	// NOTE: rest of the parameters are computed identically in both Kusama and Polkadot.

	/// Allow OCW miner to at most run 4 times in the entirety of the 10m Unsigned Phase.
	pub OffchainRepeat: u32 = UnsignedPhase::get() / 4;

	/// Upper bound of `Staking.ValidatorCount`, which translates to
	/// `ElectionProvider::DesiredTargets`. 1000 is the end-game for both Kusama and Polkadot for
	/// the foreseeable future.
	pub const MaxValidatorSet: u32 = 1000;

	/// Number of nominators per page of the snapshot, and consequently number of backers in the
	/// solution.
	///
	/// 704 in Polkadot (32 pages), 782 in Kusama (16 pages). Uses ceiling division so that
	/// `VoterSnapshotPerBlock * Pages >= MaxElectingVoters` holds for any configured values.
	pub VoterSnapshotPerBlock: u32 = MaxElectingVoters::get().div_ceil(Pages::get());

	/// In each page, we may observe up to all of the validators.
	pub const MaxWinnersPerPage: u32 = MaxValidatorSet::get();

	/// In each page of the election, we allow up to all of the nominators of that page to be
	/// present.
	///
	/// This in essence translates to "no limit on this as of now".
	pub MaxBackersPerWinner: u32 = VoterSnapshotPerBlock::get();

	/// Total number of backers per winner across all pages.
	///
	/// This in essence translates to "no limit on this as of now".
	pub MaxBackersPerWinnerFinal: u32 = MaxElectingVoters::get();

	/// Size of the exposures. This should be small enough to make the reward payouts cheap and
	/// lightweight per-page.
	// TODO: this is currently 512 in all networks, but 64 might yield better PoV, need to check logs.
	pub const MaxExposurePageSize: u32 = 512;
}

// Signed phase parameters.
parameter_types! {
	/// * Polkadot: 16
	/// * Kusama: 8
	///
	/// Reasoning: This is double the capacity of verification. There is no point for someone to be
	/// a submitter if they cannot be verified, yet, it is beneficial to act as a "reserve", in case
	/// someone bails out last minute.
	pub MaxSubmissions: u32 = 8;

	/// * Polkadot: Geometric progression with starting value 4 DOT, common factor 2. For 16
	///   submissions, it will be [4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384,
	///   32768, 65536, 131072]. Sum is `262140 DOT` for all 16 submissions.
	/// * Kusama: Geometric progression with with starting value 0.1 KSM, common factor 4. For 8
	///   submissions, values will be: `[0.1, 0.4, 1.6, 6.4, 25.6, 102.4, 409.6, 1638.4]`. Sum is
	///   `2184.5 KSM` for all 8 submissions.
	pub DepositBase: Balance = 5 * UNITS;

	/// * Polkadot: standard byte deposit configured in PAH.
	/// * Kusama: standard byte deposit configured in KAH.
	///
	/// TODO: need a maximum solution length for each runtime.
	pub DepositPerPage: Balance = 1 * UNITS;

	/// * Polkadot: 20 DOT
	/// * Kusama: 1 KSM
	///
	///
	/// Fixed deposit for invulnerable accounts.
	pub InvulnerableDeposit: Balance = UNITS;

	/// * Polkadot: 10% (more restrictive, don't bail!)
	/// * Kusama: 25%
	///
	/// Reasoning: The weight/fee of the `bail` transaction is already assuming you delete all pages
	/// of your solution while bailing, and charges you accordingly. So the chain is being
	/// compensated. The risk would be for an attacker to submit a lot of high score pages, and bail
	/// at the end to avoid getting slashed.
	pub BailoutGraceRatio: Perbill = Perbill::from_percent(5);

	/// * Polkadot: 100%
	/// * Kusama: 100%
	///
	/// The transaction fee of `register` takes into account the cost of possibly ejecting another
	/// submission into account. In the scenario that the honest submitter is being ejected by an
	/// attacker, the cost is on the attacker, and having 100% grace ratio here is only to the
	/// benefit of the honest submitter.
	pub EjectGraceRatio: Perbill = Perbill::from_percent(50);

	/// * Polkadot: 5 DOTs per era/day
	/// * Kusama: 1 KSM per era/6h
	pub RewardBase: Balance = 10 * UNITS;
}

// * Polkadot: as seen here.
// * Kusama, we will use a similar type, but with 24 as the maximum filed length.
//
// Reasoning: using u16, we can have up to 65,536 nominators and validators represented in the
// snapshot. If we every go beyond this, we have to first adjust this type.
frame_election_provider_support::generate_solution_type!(
	#[compact]
	pub struct NposCompactSolution16::<
		VoterIndex = u16,
		TargetIndex = u16,
		Accuracy = sp_runtime::PerU16,
		MaxVoters = VoterSnapshotPerBlock,
	>(16)
);

#[cfg(feature = "runtime-benchmarks")]
parameter_types! {
	pub BenchElectionBounds: frame_election_provider_support::bounds::ElectionBounds =
		frame_election_provider_support::bounds::ElectionBoundsBuilder::default().build();
}

#[cfg(feature = "runtime-benchmarks")]
pub struct OnChainConfig;

#[cfg(feature = "runtime-benchmarks")]
impl frame_election_provider_support::onchain::Config for OnChainConfig {
	// unbounded
	type Bounds = BenchElectionBounds;
	// We should not need sorting, as our bounds are large enough for the number of
	// nominators/validators in this test setup.
	type Sort = ConstBool<false>;
	type DataProvider = Staking;
	type MaxBackersPerWinner = MaxBackersPerWinner;
	type MaxWinnersPerPage = MaxWinnersPerPage;
	type Solver = frame_election_provider_support::SequentialPhragmen<AccountId, Perbill>;
	type System = Runtime;
	type WeightInfo = ();
}

impl multi_block::Config for Runtime {
	type AreWeDone = multi_block::RevertToSignedIfNotQueuedOf<Self>;
	type Pages = Pages;
	type UnsignedPhase = UnsignedPhase;
	type SignedPhase = SignedPhase;
	type SignedValidationPhase = SignedValidationPhase;
	type VoterSnapshotPerBlock = VoterSnapshotPerBlock;
	type TargetSnapshotPerBlock = TargetSnapshotPerBlock;
	type AdminOrigin = EnsureRoot<AccountId>;
	type ManagerOrigin = EnsureRoot<AccountId>;
	type DataProvider = Staking;
	#[cfg(not(feature = "runtime-benchmarks"))]
	type Fallback = multi_block::Continue<Self>;
	#[cfg(feature = "runtime-benchmarks")]
	type Fallback = frame_election_provider_support::onchain::OnChainExecution<OnChainConfig>;
	type MinerConfig = Self;
	type Signed = MultiBlockElectionSigned;
	type Verifier = MultiBlockElectionVerifier;
	type OnRoundRotation = multi_block::CleanRound<Self>;
	type WeightInfo = multi_block::weights::polkadot::MultiBlockWeightInfo<Self>;
}

impl multi_block::verifier::Config for Runtime {
	type MaxWinnersPerPage = MaxWinnersPerPage;
	type MaxBackersPerWinner = MaxBackersPerWinner;
	type MaxBackersPerWinnerFinal = MaxBackersPerWinnerFinal;
	type SolutionDataProvider = MultiBlockElectionSigned;
	type WeightInfo = multi_block::weights::polkadot::MultiBlockVerifierWeightInfo<Self>;
}

parameter_types! {
	/// The DAP buffer account, used as the signed-phase reward pot.
	pub SignedRewardPot: Option<AccountId> = Some(Dap::buffer_account());
}

impl multi_block::signed::Config for Runtime {
	type Currency = Balances;
	type BailoutGraceRatio = BailoutGraceRatio;
	type EjectGraceRatio = EjectGraceRatio;
	type DepositBase = DepositBase;
	type DepositPerPage = DepositPerPage;
	type InvulnerableDeposit = InvulnerableDeposit;
	type RewardBase = RewardBase;
	type MaxFeeRefund = multi_block::signed::FullSubmissionFee<Runtime, TransactionPayment>;
	type MaxSubmissions = MaxSubmissions;
	type EstimateCallFee = TransactionPayment;
	type Slash = Dap;
	type RewardSource = multi_block::signed::ReactivatingPot<SignedRewardPot, Balances>;
	type WeightInfo = multi_block::weights::polkadot::MultiBlockSignedWeightInfo<Self>;
}

parameter_types! {
	/// Priority of the offchain miner transactions.
	pub MinerTxPriority: TransactionPriority = TransactionPriority::max_value() / 2;
}

pub struct Balancing;
impl Get<Option<BalancingConfig>> for Balancing {
	fn get() -> Option<BalancingConfig> {
		Some(BalancingConfig { iterations: 10, tolerance: 0 })
	}
}

impl multi_block::unsigned::Config for Runtime {
	type MinerPages = MinerPages;
	type OffchainSolver = SequentialPhragmen<AccountId, SolutionAccuracyOf<Runtime>, Balancing>;
	type MinerTxPriority = MinerTxPriority;
	type OffchainRepeat = OffchainRepeat;
	type OffchainStorage = ConstBool<true>;
	type WeightInfo = multi_block::weights::polkadot::MultiBlockUnsignedWeightInfo<Self>;
}

parameter_types! {
	/// Miner transaction can fill up to 75% of the block size.
	pub MinerMaxLength: u32 = Perbill::from_rational(75u32, 100) *
		*RuntimeBlockLength::get()
		.max
		.get(DispatchClass::Normal);
}

impl multi_block::unsigned::miner::MinerConfig for Runtime {
	type AccountId = AccountId;
	type Hash = Hash;
	type MaxBackersPerWinner = <Self as multi_block::verifier::Config>::MaxBackersPerWinner;
	type MaxBackersPerWinnerFinal =
		<Self as multi_block::verifier::Config>::MaxBackersPerWinnerFinal;
	type MaxWinnersPerPage = <Self as multi_block::verifier::Config>::MaxWinnersPerPage;
	type MaxVotesPerVoter =
		<<Self as multi_block::Config>::DataProvider as ElectionDataProvider>::MaxVotesPerVoter;
	type MaxLength = MinerMaxLength;
	#[cfg(feature = "runtime-benchmarks")]
	type Solver = frame_election_provider_support::QuickDirtySolver<AccountId, Perbill>;
	#[cfg(not(feature = "runtime-benchmarks"))]
	type Solver = <Runtime as multi_block::unsigned::Config>::OffchainSolver;
	type Pages = Pages;
	type Solution = NposCompactSolution16;
	type VoterSnapshotPerBlock = <Runtime as multi_block::Config>::VoterSnapshotPerBlock;
	type TargetSnapshotPerBlock = <Runtime as multi_block::Config>::TargetSnapshotPerBlock;
}

parameter_types! {
	pub const BagThresholds: &'static [u64] = &bag_thresholds::THRESHOLDS;
}

type VoterBagsListInstance = pallet_bags_list::Instance1;
impl pallet_bags_list::Config<VoterBagsListInstance> for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type ScoreProvider = Staking;
	type WeightInfo = weights::pallet_bags_list::WeightInfo<Runtime>;
	type BagThresholds = BagThresholds;
	type Score = sp_npos_elections::VoteWeight;
	type MaxAutoRebagPerBlock = ();
}

parameter_types! {
	pub const StakingPotsPalletId: frame_support::PalletId = frame_support::PalletId(*b"py/stkng");
}

/// Polkadot inflation curve for DAP.
///
/// Same computation as the previous `EraPayout` but returns total emission
/// (the staker/treasury split is now handled by DAP budget allocation).
pub struct PolkadotIssuanceCurve;
impl sp_staking::budget::IssuanceCurve<Balance> for PolkadotIssuanceCurve {
	fn issue(_total_issuance: Balance, elapsed_millis: u64) -> Balance {
		const MILLISECONDS_PER_YEAR: u64 = (1000 * 3600 * 24 * 36525) / 100;
		let relative_period =
			FixedU128::from_rational(elapsed_millis.into(), MILLISECONDS_PER_YEAR.into());

		let fixed_total_issuance: i128 = 5_216_342_402_773_185_773;
		let fixed_inflation_rate = FixedU128::from_rational(8, 100);
		let yearly_emission = fixed_inflation_rate.saturating_mul_int(fixed_total_issuance);

		let emission = relative_period.saturating_mul_int(yearly_emission);
		emission.saturated_into()
	}
}

parameter_types! {
	// Six sessions in an era (6 hours).
	pub const SessionsPerEra: SessionIndex = prod_or_fast!(6, 1);
	/// Duration of a relay session in our blocks. Needs to be hardcoded per-runtime.
	pub const RelaySessionDuration: BlockNumber = 10;
	// 2 eras for unbonding (12 hours).
	pub const BondingDuration: sp_staking::EraIndex = 2;
	// 1 era in which slashes can be cancelled (6 hours).
	pub const SlashDeferDuration: sp_staking::EraIndex = 1;
	// Nominators can unbond faster (2 eras) when not slashable.
	pub const NominatorFastUnbondDuration: sp_staking::EraIndex = 2;
	// Note: this is not really correct as Max Nominators is (MaxExposurePageSize * page_count) but
	// this is an unbounded number. We just set it to a reasonably high value, 1 full page
	// of nominators.
	pub const MaxControllersInDeprecationBatch: u32 = 751;
	pub const MaxNominations: u32 = <NposCompactSolution16 as frame_election_provider_support::NposSolution>::LIMIT as u32;
	pub const MaxEraDuration: u64 = RelaySessionDuration::get() as u64 * RELAY_CHAIN_SLOT_DURATION_MILLIS as u64 * SessionsPerEra::get() as u64;
	pub MaxPruningItems: u32 = 100;
}

impl pallet_staking_async::Config for Runtime {
	type Filter = ();
	type OldCurrency = Balances;
	type Currency = Balances;
	type CurrencyBalance = Balance;
	type RuntimeHoldReason = RuntimeHoldReason;
	type CurrencyToVote = sp_staking::currency_to_vote::SaturatingCurrencyToVote;
	type RewardRemainder = ();
	type Slash = Dap;
	type Reward = ();
	type SessionsPerEra = SessionsPerEra;
	type BondingDuration = BondingDuration;
	type SlashDeferDuration = SlashDeferDuration;
	type NominatorFastUnbondDuration = NominatorFastUnbondDuration;
	type AdminOrigin = EitherOf<EnsureRoot<AccountId>, StakingAdmin>;
	type EraPayout = ();
	type MaxExposurePageSize = MaxExposurePageSize;
	type ElectionProvider = MultiBlockElection;
	type VoterList = VoterList;
	type TargetList = UseValidatorsMap<Self>;
	type MaxValidatorSet = MaxValidatorSet;
	type NominationsQuota = pallet_staking_async::FixedNominationsQuota<{ MaxNominations::get() }>;
	type MaxUnlockingChunks = frame_support::traits::ConstU32<32>;
	type HistoryDepth = ConstU32<1>;
	type MaxControllersInDeprecationBatch = MaxControllersInDeprecationBatch;
	type EventListeners = (NominationPools, DelegatedStaking);
	type WeightInfo = pallet_staking_async::weights::SubstrateWeight<Runtime>;
	type MaxEraDuration = MaxEraDuration;
	type DisableMinting = ConstBool<true>;
	type UnclaimedRewardHandler = Dap;
	type RewardPots = pallet_staking_async::Seed<StakingPotsPalletId>;
	type StakerRewardCalculator =
		pallet_staking_async::reward::DefaultStakerRewardCalculator<Runtime>;
	type MaxPruningItems = MaxPruningItems;
	type PlanningEraOffset =
		pallet_staking_async::PlanningEraOffsetOf<Self, RelaySessionDuration, ConstU32<10>>;
	type RcClientInterface = StakingRcClient;
	type IsValidatorInactive = ();
}

// Relay chain session keys matching Westend configuration.
sp_runtime::impl_opaque_keys! {
	pub struct RelayChainSessionKeys {
		pub grandpa: sp_consensus_grandpa::AuthorityId,
		pub babe: sp_consensus_babe::AuthorityId,
		pub para_validator: polkadot_primitives::ValidatorId,
		pub para_assignment: polkadot_primitives::AssignmentId,
		pub authority_discovery: sp_authority_discovery::AuthorityId,
		pub beefy: sp_consensus_beefy::ecdsa_crypto::AuthorityId,
	}
}

// Relay chain session keys type for rc-client.
impl pallet_staking_async_rc_client::Config for Runtime {
	type RelayChainOrigin = EnsureRoot<AccountId>;
	type AHStakingInterface = Staking;
	type SendToRelayChain = StakingXcmToRelayChain;
	type MaxValidatorSetRetries = ConstU32<5>;
	// export validator session at end of session 4 within an era.
	type ValidatorSetExportSession = ConstU32<4>;
	type RelayChainSessionKeys = RelayChainSessionKeys;
	type Currency = Balances;
	type KeyDeposit = ConstU128<{ 10_000 * UNITS }>;
	type WeightInfo = ();
}

parameter_types! {
	pub const DapPalletId: frame_support::PalletId = pallet_dap::DAP_PALLET_ID;
	pub const DapIssuanceCadence: u64 = 60_000;
	pub const DapMaxElapsedPerDrip: u64 = 600_000;
}

impl pallet_dap::Config for Runtime {
	type Currency = Balances;
	type PalletId = DapPalletId;
	type IssuanceCurve = PolkadotIssuanceCurve;
	type BudgetRecipients = (
		pallet_dap::Pallet<Runtime>,
		pallet_staking_async::StakerRewardRecipient<
			pallet_staking_async::Seed<StakingPotsPalletId>,
		>,
		pallet_staking_async::ValidatorIncentiveRecipient<
			pallet_staking_async::Seed<StakingPotsPalletId>,
		>,
	);
	type Time = pallet_timestamp::Pallet<Runtime>;
	type IssuanceCadence = DapIssuanceCadence;
	type MaxElapsedPerDrip = DapMaxElapsedPerDrip;
	type BudgetOrigin = frame_system::EnsureRoot<AccountId>;
	type WeightInfo = ();
}

parameter_types! {
	pub StakingXcmDestination: Location = Location::parent();
}

#[derive(Encode, Decode)]
// Call indices taken from westend-next runtime.
pub enum RelayChainRuntimePallets {
	#[codec(index = 67)]
	AhClient(AhClientCalls),
}

#[derive(Encode, Decode)]
pub enum AhClientCalls {
	#[codec(index = 0)]
	ValidatorSet(rc_client::ValidatorSetReport<AccountId>),
	#[codec(index = 3)]
	SetKeysFromAh { stash: AccountId, keys: Vec<u8> },
	#[codec(index = 4)]
	PurgeKeysFromAh { stash: AccountId },
}

pub struct ValidatorSetToXcm;
impl Convert<rc_client::ValidatorSetReport<AccountId>, Xcm<()>> for ValidatorSetToXcm {
	fn convert(report: rc_client::ValidatorSetReport<AccountId>) -> Xcm<()> {
		rc_client::build_transact_xcm(
			RelayChainRuntimePallets::AhClient(AhClientCalls::ValidatorSet(report)).encode(),
		)
	}
}

pub struct KeysMessageToXcm;
impl Convert<rc_client::KeysMessage<AccountId>, Xcm<()>> for KeysMessageToXcm {
	fn convert(msg: rc_client::KeysMessage<AccountId>) -> Xcm<()> {
		let encoded_call = match msg {
			rc_client::KeysMessage::SetKeys { stash, keys } => {
				RelayChainRuntimePallets::AhClient(AhClientCalls::SetKeysFromAh { stash, keys })
					.encode()
			},
			rc_client::KeysMessage::PurgeKeys { stash } => {
				RelayChainRuntimePallets::AhClient(AhClientCalls::PurgeKeysFromAh { stash })
					.encode()
			},
		};
		rc_client::build_transact_xcm(encoded_call)
	}
}

pub struct StakingXcmToRelayChain;

impl rc_client::SendToRelayChain for StakingXcmToRelayChain {
	type AccountId = AccountId;
	type Balance = Balance;

	fn validator_set(report: rc_client::ValidatorSetReport<Self::AccountId>) -> Result<(), ()> {
		rc_client::XCMSender::<
			xcm_config::XcmRouter,
			StakingXcmDestination,
			rc_client::ValidatorSetReport<Self::AccountId>,
			ValidatorSetToXcm,
		>::send(report)
	}

	fn set_keys(
		stash: Self::AccountId,
		keys: Vec<u8>,
		max_fee: Option<Self::Balance>,
	) -> Result<Self::Balance, rc_client::SendKeysError<Self::Balance>> {
		rc_client::XCMSender::<
			xcm_config::XcmRouter,
			StakingXcmDestination,
			rc_client::KeysMessage<Self::AccountId>,
			KeysMessageToXcm,
		>::send_with_fees::<
			XcmExec<xcm_config::XcmConfig>,
			RuntimeCall,
			AccountId,
			rc_client::AccountId32ToLocation,
			Self::Balance,
		>(rc_client::KeysMessage::set_keys(stash.clone(), keys), stash, max_fee, 0)
	}

	fn purge_keys(
		stash: Self::AccountId,
		max_fee: Option<Self::Balance>,
	) -> Result<Self::Balance, rc_client::SendKeysError<Self::Balance>> {
		rc_client::XCMSender::<
			xcm_config::XcmRouter,
			StakingXcmDestination,
			rc_client::KeysMessage<Self::AccountId>,
			KeysMessageToXcm,
		>::send_with_fees::<
			XcmExec<xcm_config::XcmConfig>,
			RuntimeCall,
			AccountId,
			rc_client::AccountId32ToLocation,
			Self::Balance,
		>(rc_client::KeysMessage::purge_keys(stash.clone()), stash, max_fee, 0)
	}
}

impl pallet_fast_unstake::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type BatchSize = ConstU32<64>;
	type Deposit = ConstU128<{ UNITS }>;
	type ControlOrigin = EnsureRoot<AccountId>;
	type Staking = Staking;
	type MaxErasToCheckPerBlock = ConstU32<1>;
	type WeightInfo = weights::pallet_fast_unstake::WeightInfo<Runtime>;
}

parameter_types! {
	pub const PoolsPalletId: PalletId = PalletId(*b"py/nopls");
	pub const MaxPointsToBalance: u8 = 10;
}

impl pallet_nomination_pools::Config for Runtime {
	type Filter = ();
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = weights::pallet_nomination_pools::WeightInfo<Self>;
	type Currency = Balances;
	type RuntimeFreezeReason = RuntimeFreezeReason;
	type RewardCounter = FixedU128;
	type BalanceToU256 = BalanceToU256;
	type U256ToBalance = U256ToBalance;
	type StakeAdapter =
		pallet_nomination_pools::adapter::DelegateStake<Self, Staking, DelegatedStaking>;
	// Buffer (30) + bonding duration (2).
	type MaxUnbondingPools = ConstU32<32>;
	type MaxMetadataLen = ConstU32<256>;
	// we use the same number of allowed unlocking chunks as with staking.
	type MaxUnbonding = <Self as pallet_staking_async::Config>::MaxUnlockingChunks;
	type PalletId = PoolsPalletId;
	type MaxPointsToBalance = MaxPointsToBalance;
	type AdminOrigin = EitherOf<EnsureRoot<AccountId>, StakingAdmin>;
	type BlockNumberProvider = RelaychainDataProvider<Runtime>;
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

/// The payload being signed in transactions.
pub type SignedPayload = generic::SignedPayload<RuntimeCall, TxExtension>;
/// Unchecked extrinsic type as expected by this runtime.
pub type UncheckedExtrinsic =
	generic::UncheckedExtrinsic<Address, RuntimeCall, Signature, TxExtension>;

impl frame_system::offchain::SigningTypes for Runtime {
	type Public = <Signature as Verify>::Signer;
	type Signature = Signature;
}

impl<C> frame_system::offchain::CreateTransactionBase<C> for Runtime
where
	RuntimeCall: From<C>,
{
	type RuntimeCall = RuntimeCall;
	type Extrinsic = UncheckedExtrinsic;
}

impl<LocalCall> frame_system::offchain::CreateTransaction<LocalCall> for Runtime
where
	RuntimeCall: From<LocalCall>,
{
	type Extension = TxExtension;

	fn create_transaction(call: RuntimeCall, extension: TxExtension) -> UncheckedExtrinsic {
		UncheckedExtrinsic::new_transaction(call, extension)
	}
}

/// Submits a transaction with the node's public and signature type. Adheres to the signed extension
/// format of the chain.
impl<LocalCall> frame_system::offchain::CreateSignedTransaction<LocalCall> for Runtime
where
	RuntimeCall: From<LocalCall>,
{
	fn create_signed_transaction<
		C: frame_system::offchain::AppCrypto<Self::Public, Self::Signature>,
	>(
		call: RuntimeCall,
		public: <Signature as Verify>::Signer,
		account: AccountId,
		nonce: <Runtime as frame_system::Config>::Nonce,
	) -> Option<UncheckedExtrinsic> {
		use sp_runtime::traits::StaticLookup;
		// take the biggest period possible.
		let period =
			BlockHashCount::get().checked_next_power_of_two().map(|c| c / 2).unwrap_or(2) as u64;

		let current_block = System::block_number()
			.saturated_into::<u64>()
			// The `System::block_number` is initialized with `n+1`,
			// so the actual block number is `n`.
			.saturating_sub(1);
		let tip = 0;
		let tx_ext = TxExtension::from((
			frame_system::CheckNonZeroSender::<Runtime>::new(),
			frame_system::CheckSpecVersion::<Runtime>::new(),
			frame_system::CheckTxVersion::<Runtime>::new(),
			frame_system::CheckGenesis::<Runtime>::new(),
			frame_system::CheckEra::<Runtime>::from(generic::Era::mortal(period, current_block)),
			frame_system::CheckNonce::<Runtime>::from(nonce),
			frame_system::CheckWeight::<Runtime>::new(),
			pallet_asset_conversion_tx_payment::ChargeAssetTxPayment::<Runtime>::from(tip, None),
			frame_metadata_hash_extension::CheckMetadataHash::<Runtime>::new(true),
		));
		let raw_payload = SignedPayload::new(call, tx_ext)
			.map_err(|e| {
				log::warn!("Unable to create signed payload: {:?}", e);
			})
			.ok()?;
		let signature = raw_payload.using_encoded(|payload| C::sign(payload, public))?;
		let (call, tx_ext, _) = raw_payload.deconstruct();
		let address = <Runtime as frame_system::Config>::Lookup::unlookup(account);
		let transaction = UncheckedExtrinsic::new_signed(call, address, signature, tx_ext);
		Some(transaction)
	}
}

impl<LocalCall> frame_system::offchain::CreateInherent<LocalCall> for Runtime
where
	RuntimeCall: From<LocalCall>,
{
	fn create_bare(call: RuntimeCall) -> UncheckedExtrinsic {
		UncheckedExtrinsic::new_bare(call)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame_election_provider_support::ElectionProvider;
	use frame_support::weights::constants::{WEIGHT_PROOF_SIZE_PER_KB, WEIGHT_REF_TIME_PER_MILLIS};
	use pallet_election_provider_multi_block::{
		self as mb, signed::WeightInfo as _, unsigned::WeightInfo as _,
	};
	use remote_externalities::{Builder, Mode, OfflineConfig, OnlineConfig, SnapshotConfig};
	use std::env::var;

	fn weight_diff(block: Weight, op: Weight) {
		log::info!(
			target: "runtime",
			"ref_time: {:?}ms {:.4} of total",
			op.ref_time() / WEIGHT_REF_TIME_PER_MILLIS,
			op.ref_time() as f64 / block.ref_time() as f64
		);
		log::info!(
			target: "runtime",
			"proof_size: {:?}kb {:.4} of total",
			op.proof_size() / WEIGHT_PROOF_SIZE_PER_KB,
			op.proof_size() as f64 / block.proof_size() as f64
		);
	}

	#[test]
	fn fake_dot_preset_snapshot_capacity_covers_max_electing_voters() {
		sp_io::TestExternalities::default().execute_with(|| {
			super::enable_dot_preset(false);
			assert!(
				VoterSnapshotPerBlock::get() * Pages::get() >= MaxElectingVoters::get(),
				"paged snapshot capacity {} < MaxElectingVoters {}",
				VoterSnapshotPerBlock::get() * Pages::get(),
				MaxElectingVoters::get(),
			);
		});
	}

	#[test]
	fn fake_ksm_preset_snapshot_capacity_covers_max_electing_voters() {
		sp_io::TestExternalities::default().execute_with(|| {
			super::enable_ksm_preset(false);
			assert!(
				VoterSnapshotPerBlock::get() * Pages::get() >= MaxElectingVoters::get(),
				"paged snapshot capacity {} < MaxElectingVoters {}",
				VoterSnapshotPerBlock::get() * Pages::get(),
				MaxElectingVoters::get(),
			);
		});
	}

	#[test]
	fn ensure_epmb_weights_sane_polkadot() {
		use sp_io::TestExternalities;
		use sp_runtime::Percent;
		sp_tracing::try_init_simple();
		TestExternalities::default().execute_with(|| {
			super::enable_dot_preset(false);
			pallet_election_provider_multi_block::Pallet::<Runtime>::check_all_weights(
				<Runtime as frame_system::Config>::BlockWeights::get().max_block,
				Some(Percent::from_percent(75)),
				Some(Percent::from_percent(50)),
			)
		});
	}

	#[test]
	fn ensure_epmb_weights_sane_kusama() {
		use sp_io::TestExternalities;
		use sp_runtime::Percent;
		sp_tracing::try_init_simple();
		TestExternalities::default().execute_with(|| {
			super::enable_ksm_preset(false);
			pallet_election_provider_multi_block::Pallet::<Runtime>::check_all_weights(
				<Runtime as frame_system::Config>::BlockWeights::get().max_block,
				Some(Percent::from_percent(75)),
				Some(Percent::from_percent(50)),
			)
		});
	}

	#[test]
	fn signed_weight_ratios() {
		sp_tracing::try_init_simple();
		let block_weight = <Runtime as frame_system::Config>::BlockWeights::get().max_block;
		let polkadot_signed_submission =
			mb::weights::polkadot::MultiBlockSignedWeightInfo::<Runtime>::submit_page();
		let kusama_signed_submission =
			mb::weights::kusama::MultiBlockSignedWeightInfo::<Runtime>::submit_page();

		log::info!(target: "runtime", "Polkadot:");
		weight_diff(block_weight, polkadot_signed_submission);
		log::info!(target: "runtime", "Kusama:");
		weight_diff(block_weight, kusama_signed_submission);
	}

	#[test]
	fn election_duration() {
		sp_tracing::try_init_simple();
		sp_io::TestExternalities::default().execute_with(|| {
			super::enable_dot_preset(false);
			let duration = mb::Pallet::<Runtime>::average_election_duration();
			let polkadot_session = 6 * HOURS;
			log::info!(
				target: "runtime",
				"Polkadot election duration: {:?}, session: {:?} ({} sessions)",
				duration,
				polkadot_session,
				duration / polkadot_session
			);
		});

		sp_io::TestExternalities::default().execute_with(|| {
			super::enable_ksm_preset(false);
			let duration = mb::Pallet::<Runtime>::average_election_duration();
			let kusama_session = 1 * HOURS;
			log::info!(
				target: "runtime",
				"Kusama election duration: {:?}, session: {:?} ({} sessions)",
				duration,
				kusama_session,
				duration / kusama_session
			);
		});
	}

	#[test]
	fn max_ocw_miner_pages_as_per_weights() {
		sp_tracing::try_init_simple();
		for p in 1..=32 {
			log::info!(
				target: "runtime",
				"exec_time of polkadot miner in WASM with {} pages is {:?}ms",
				p,
				mb::weights::polkadot::MultiBlockUnsignedWeightInfo::<Runtime>::mine_solution(p).ref_time() / WEIGHT_REF_TIME_PER_MILLIS
			);
		}
		for p in 1..=16 {
			log::info!(
				target: "runtime",
				"exec_time of kusama miner in WASM with {} pages is {:?}ms",
				p,
				mb::weights::kusama::MultiBlockUnsignedWeightInfo::<Runtime>::mine_solution(p).ref_time() / WEIGHT_REF_TIME_PER_MILLIS
			);
		}
	}

	/// Run it like:
	///
	/// ```text
	/// RUST_BACKTRACE=full \
	/// 	RUST_LOG=remote-ext=info,runtime::staking-async=debug \
	/// 	REMOTE_TESTS=1 \
	/// 	WS=ws://127.0.0.1:9999 \
	/// 	cargo test --release -p pallet-staking-async-parachain-runtime \
	/// 	--features try-runtime run_try
	/// ```
	///
	/// Just replace the node with your local node.
	///
	/// Pass `SNAP=polkadot` or similar to store and reuse a snapshot.
	#[tokio::test]
	async fn run_election_with_pages() {
		if var("REMOTE_TESTS").is_err() {
			return;
		}
		sp_tracing::try_init_simple();

		let transport_uri = var("WS").unwrap_or("wss://westend-rpc.polkadot.io:443".to_string());
		let maybe_state_snapshot: Option<SnapshotConfig> = var("SNAP").map(|s| s.into()).ok();

		let mut ext = Builder::<Block>::default()
			.mode(if let Some(state_snapshot) = maybe_state_snapshot {
				Mode::OfflineOrElseOnline(
					OfflineConfig { state_snapshot: state_snapshot.clone() },
					OnlineConfig {
						transport_uris: vec![transport_uri.clone()],
						hashed_prefixes: vec![vec![]],
						state_snapshot: Some(state_snapshot),
						..Default::default()
					},
				)
			} else {
				Mode::Online(OnlineConfig {
					hashed_prefixes: vec![vec![]],
					transport_uris: vec![transport_uri],
					..Default::default()
				})
			})
			.build()
			.await
			.unwrap();
		ext.execute_with(|| {
			sp_core::crypto::set_default_ss58_version(1u8.into());
			super::enable_dot_preset(true);

			// prepare all snapshot in EPMB pallet.
			mb::Pallet::<Runtime>::asap();
			for page in 1..=32 {
				mb::unsigned::miner::OffchainWorkerMiner::<Runtime>::mine_solution(page, true)
					.inspect(|p| log::info!(target: "runtime", "{:?}", p.score.pretty("DOT", 10)))
					.unwrap();
			}
		});
	}
}
