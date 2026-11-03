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

use crate::shared;
use frame::testing_prelude::*;
use frame_election_provider_support::{
	bounds::{ElectionBounds, ElectionBoundsBuilder},
	SequentialPhragmen,
};
use frame_support::{
	sp_runtime::testing::TestXt,
	traits::fungible::Mutate,
	weights::{Weight, WeightMeter},
};
use pallet_election_provider_multi_block as multi_block;
use pallet_election_provider_multi_block::{Event as ElectionEvent, Phase};
use pallet_staking_async::{ActiveEra, CurrentEra, Forcing, PotAccountProvider};
use pallet_staking_async_rc_client::{
	OutgoingValidatorSet, SendKeysError, SendOperationError, SessionReport, ValidatorSetReport,
};
use sp_staking::{budget::BudgetRecipient, SessionIndex};
use xcm::latest::{prelude::*, Asset, AssetId, Assets, Fungibility, Junction, Location};
use xcm_builder::{FungibleAdapter, IsConcrete};
use xcm_executor::{
	traits::{ConvertLocation, FeeManager, FeeReason, TransactAsset},
	AssetsInHolding,
};
pub const LOG_TARGET: &str = "ahm-test";

construct_runtime! {
	pub enum Runtime {
		System: frame_system,
		Balances: pallet_balances,
		Proxy: pallet_proxy,

		// NOTE: the validator set is given by pallet-staking to rc-client on-init, and rc-client
		// will not send it immediately, but rather store it and sends it over on its own next
		// on-init call. Yet, because staking comes first here, its on-init is called before
		// rc-client, so under normal conditions, the message is sent immediately.
		Staking: pallet_staking_async,
		RcClient: pallet_staking_async_rc_client,

		MultiBlock: multi_block,
		MultiBlockVerifier: multi_block::verifier,
		MultiBlockSigned: multi_block::signed,
		MultiBlockUnsigned: multi_block::unsigned,

		Dap: pallet_dap,
	}
}

// alias Runtime with T.
pub type T = Runtime;

parameter_types! {
	pub static NextPollWeight: Option<Weight> = None;
}

/// Block time in milliseconds
pub const BLOCK_TIME: u64 = 12_000;

pub fn roll_next() {
	let now = System::block_number();
	let next = now + 1;

	MockTime::set(MockTime::get() + BLOCK_TIME);
	System::set_block_number(next);
	// Re-init frame-system, as execute would do. This resets the block weight usage counter, as we
	// are using a realistic weight meter here.
	frame_system::BlockWeight::<T>::kill();

	System::register_extra_weight_unchecked(Dap::on_initialize(next), DispatchClass::Mandatory);
	System::register_extra_weight_unchecked(Staking::on_initialize(next), DispatchClass::Mandatory);
	System::register_extra_weight_unchecked(
		RcClient::on_initialize(next),
		DispatchClass::Mandatory,
	);

	let mut meter = NextPollWeight::take()
		.map(WeightMeter::with_limit)
		.unwrap_or_else(System::remaining_block_weight);
	Staking::on_poll(next, &mut meter);
	MultiBlock::on_poll(next, &mut meter);
}

pub fn roll_many(blocks: BlockNumber) {
	let current = System::block_number();
	while System::block_number() < current + blocks {
		roll_next();
	}
}

pub fn roll_until_matches(criteria: impl Fn() -> bool, with_rc: bool) {
	let mut iterations = 0;
	const MAX_ITERATIONS: u32 = 1000;

	while !criteria() {
		iterations += 1;
		if iterations > MAX_ITERATIONS {
			panic!(
				"roll_until_matches: exceeded {} iterations without matching criteria. Current block: {}, Current era: {:?}, Active era: {:?}",
				MAX_ITERATIONS,
				frame_system::Pallet::<Runtime>::block_number(),
				CurrentEra::<Runtime>::get(),
				ActiveEra::<Runtime>::get()
			);
		}

		if iterations % 50 == 0 {
			log::debug!(target: LOG_TARGET,
				"roll_until_matches: iteration {}, block: {}, current_era: {:?}, active_era: {:?}",
				iterations,
				frame_system::Pallet::<Runtime>::block_number(),
				CurrentEra::<Runtime>::get(),
				ActiveEra::<Runtime>::get()
			);
		}

		roll_next();
		if with_rc {
			if LocalQueue::get().is_some() {
				panic!("when local queue is set, you cannot roll ah forward as well!")
			}
			shared::in_rc(|| {
				crate::rc::roll_next();
			});
		}
	}
}

/// Use the given `end_index` as the first session report, and increment as per needed.
pub(crate) fn roll_until_next_active(mut end_index: SessionIndex) -> Vec<AccountId> {
	log::debug!(target: LOG_TARGET, "roll_until_next_active: end_index: {:?}", end_index);

	LocalQueue::flush();

	let roll_session = |end_index| {
		log::debug!(target: LOG_TARGET, "Ending session: {}", end_index);
		let report = SessionReport {
			end_index,
			activation_timestamp: None,
			leftover: false,
			validator_points: Default::default(),
		};
		assert_ok!(pallet_staking_async_rc_client::Pallet::<Runtime>::relay_session_report(
			RuntimeOrigin::root(),
			report
		));

		// roll some blocks in the session
		roll_next();
	};

	// receive enough session reports, such that we plan a new era
	let planned_era = pallet_staking_async::session_rotation::Rotator::<Runtime>::planned_era();
	let active_era = pallet_staking_async::session_rotation::Rotator::<Runtime>::active_era();

	let mut session_iterations = 0;
	// iterate until election starts
	while pallet_staking_async::session_rotation::Rotator::<Runtime>::planned_era() == active_era {
		if session_iterations > SessionsPerEra::get() {
			panic!(
				"roll_until_next_active: planning loop exceeded {} iterations. planned_era: {:?}, end_index: {}",
				SessionsPerEra::get(), planned_era, end_index
			);
		}

		roll_session(end_index);
		end_index += 1;
		session_iterations += 1;
	}

	// election is started at this point. Roll until elections are done.
	let mut election_iterations = 0;
	while !pallet_staking_async_rc_client::OutgoingValidatorSet::<T>::exists() {
		roll_next();

		if election_iterations > 500 {
			panic!(
				"roll_until_next_active: election loop exceeded 500 iterations. Block: {}, end_index: {}, messages in queue: {}",
				System::block_number(),
				end_index,
				LocalQueue::get_since_last_call().len()
			);
		}

		if election_iterations % 50 == 0 {
			log::debug!(
				target: LOG_TARGET,
				"roll_until_next_active: waiting for validator set message, iteration: {}, block: {}, end_index: {}",
				election_iterations,
				System::block_number(),
				end_index
			);
		}
		election_iterations += 1;
	}

	// active era is still 0
	assert_eq!(
		pallet_staking_async::session_rotation::Rotator::<Runtime>::active_era(),
		active_era
	);

	// roll sessions until validator set is exported
	let mut session_iterations = 0;
	loop {
		let messages = LocalQueue::get_since_last_call();
		session_iterations += 1;
		if session_iterations > SessionsPerEra::get() * 2 {
			panic!(
				"roll_until_next_active: session loop exceeded {} iterations. messages: {:?}, end_index: {:?}",
				session_iterations, messages.len(), end_index
			);
		}

		match messages.len() {
			0 => {
				roll_session(end_index);
				end_index += 1;
				continue;
			},
			1 => {
				assert_eq!(
					messages[0],
					(
						System::block_number(),
						OutgoingMessages::ValidatorSet(ValidatorSetReport {
							id: active_era + 1,
							leftover: false,
							// arbitrary, feel free to change if test setup updates
							new_validator_set: vec![3, 5, 6, 8],
							prune_up_to: active_era.checked_sub(BondingDuration::get()),
						})
					)
				);
				break;
			},
			_ => panic!("Expected only one message in local queue, but got: {:?}", messages),
		}
	}

	// validator report is queued but not activated in the next session
	roll_session(end_index);
	end_index += 1;

	// in the next session era is activated.
	let report = SessionReport {
		end_index,
		activation_timestamp: Some((MockTime::get(), active_era + 1)),
		leftover: false,
		validator_points: Default::default(),
	};
	assert_ok!(pallet_staking_async_rc_client::Pallet::<Runtime>::relay_session_report(
		RuntimeOrigin::root(),
		report
	));
	log::debug!(target: LOG_TARGET, "Era rotated to {:?} at session ending: {:?}", active_era + 1, end_index);

	// active era is now 1.
	assert_eq!(
		pallet_staking_async::session_rotation::Rotator::<Runtime>::active_era(),
		active_era + 1
	);

	// arbitrary, feel free to change if test setup updates
	vec![3, 5, 6, 8]
}

pub type AccountId = <Runtime as frame_system::Config>::AccountId;
pub type Balance = <Runtime as pallet_balances::Config>::Balance;
pub type Hash = <Runtime as frame_system::Config>::Hash;
pub type BlockNumber = BlockNumberFor<Runtime>;
pub type BlockWeights = <Runtime as frame_system::Config>::BlockWeights;

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Runtime {
	type Block = MockBlock<Self>;
	type AccountData = pallet_balances::AccountData<Balance>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Runtime {
	type Balance = u128;
	type AccountStore = System;
}

frame_election_provider_support::generate_solution_type!(
	pub struct TestNposSolution::<
		VoterIndex = u16,
		TargetIndex = u16,
		Accuracy = PerU16,
		MaxVoters = ConstU32::<1000>
	>(16)
);

type Extrinsic = TestXt<RuntimeCall, ()>;
impl<LocalCall> frame_system::offchain::CreateTransactionBase<LocalCall> for Runtime
where
	RuntimeCall: From<LocalCall>,
{
	type RuntimeCall = RuntimeCall;
	type Extrinsic = Extrinsic;
}

impl<LocalCall> frame_system::offchain::CreateBare<LocalCall> for Runtime
where
	RuntimeCall: From<LocalCall>,
{
	fn create_bare(call: Self::RuntimeCall) -> Self::Extrinsic {
		Extrinsic::new_bare(call)
	}
}

type MaxVotesPerVoter = pallet_staking_async::MaxNominationsOf<Runtime>;
parameter_types! {
	pub static MaxValidators: u32 = 32;
	pub static MaxBackersPerWinner: u32 = 16;
	pub static MaxExposurePageSize: u32 = 8;
	pub static MaxBackersPerWinnerFinal: u32 = 16;
	pub static MaxWinnersPerPage: u32 = 16;
	pub static MaxLength: u32 = 4 * 1024 * 1024;
	pub static Pages: u32 = 3;
	pub static TargetSnapshotPerBlock: u32 = 4;
	pub static VoterSnapshotPerBlock: u32 = 4;

	pub static SignedPhase: BlockNumber = 4;
	pub static UnsignedPhase: BlockNumber = 4;
	pub static SignedValidationPhase: BlockNumber = (2 * Pages::get() as BlockNumber);
}

impl multi_block::unsigned::miner::MinerConfig for Runtime {
	type AccountId = AccountId;
	type Hash = Hash;
	type MaxBackersPerWinner = MaxBackersPerWinner;
	type MaxWinnersPerPage = MaxWinnersPerPage;
	type MaxBackersPerWinnerFinal = MaxBackersPerWinnerFinal;
	type MaxVotesPerVoter = MaxVotesPerVoter;
	type Solution = TestNposSolution;
	type MaxLength = MaxLength;
	type Pages = Pages;
	type Solver = SequentialPhragmen<AccountId, Perbill>;
	type TargetSnapshotPerBlock = TargetSnapshotPerBlock;
	type VoterSnapshotPerBlock = VoterSnapshotPerBlock;
}

parameter_types! {
	pub Bounds: ElectionBounds = ElectionBoundsBuilder::default().build();
}

pub struct OnChainConfig;
impl frame_election_provider_support::onchain::Config for OnChainConfig {
	// unbounded
	type Bounds = Bounds;
	// We should not need sorting, as our bounds are large enough for the number of
	// nominators/validators in this test setup.
	type Sort = ConstBool<false>;
	type DataProvider = Staking;
	type MaxBackersPerWinner = MaxBackersPerWinner;
	type MaxWinnersPerPage = MaxWinnersPerPage;
	type Solver = SequentialPhragmen<AccountId, Perbill>;
	type System = Runtime;
	type WeightInfo = ();
}

impl multi_block::Config for Runtime {
	type AdminOrigin = EnsureRoot<AccountId>;
	type ManagerOrigin = EnsureRoot<AccountId>;
	type DataProvider = Staking;
	type Fallback = frame_election_provider_support::onchain::OnChainExecution<OnChainConfig>;
	type MinerConfig = Self;

	type Pages = Pages;
	type SignedPhase = SignedPhase;
	type UnsignedPhase = UnsignedPhase;
	type SignedValidationPhase = SignedValidationPhase;
	type TargetSnapshotPerBlock = TargetSnapshotPerBlock;
	type VoterSnapshotPerBlock = VoterSnapshotPerBlock;
	type Verifier = MultiBlockVerifier;
	type Signed = MultiBlockSigned;
	type AreWeDone = multi_block::ProceedRegardlessOf<Self>;
	type OnRoundRotation = multi_block::CleanRound<Self>;
	type WeightInfo = super::weights::MultiBlockElectionWeightInfo;
}

impl multi_block::verifier::Config for Runtime {
	type MaxBackersPerWinner = MaxBackersPerWinner;
	type MaxBackersPerWinnerFinal = MaxBackersPerWinnerFinal;
	type MaxWinnersPerPage = MaxWinnersPerPage;

	type SolutionDataProvider = MultiBlockSigned;
	type WeightInfo = super::weights::MultiBlockElectionWeightInfo;
}

impl multi_block::unsigned::Config for Runtime {
	type MinerPages = ConstU32<1>;
	type OffchainStorage = ConstBool<true>;
	type MinerTxPriority = ConstU64<{ u64::MAX }>;
	type OffchainRepeat = ();
	type OffchainSolver = SequentialPhragmen<AccountId, Perbill>;
	type WeightInfo = super::weights::MultiBlockElectionWeightInfo;
}

parameter_types! {
	pub static DepositBase: Balance = 1;
	pub static DepositPerPage: Balance = 1;
	pub static MaxSubmissions: u32 = 2;
	pub static RewardBase: Balance = 5;
	/// The DAP buffer account, used as the signed-phase reward pot.
	pub SignedRewardPot: Option<AccountId> = Some(Dap::buffer_account());
}

impl multi_block::signed::Config for Runtime {
	type Currency = Balances;
	type EjectGraceRatio = ();
	type BailoutGraceRatio = ();
	type InvulnerableDeposit = ();
	type DepositBase = DepositBase;
	type DepositPerPage = DepositPerPage;
	type EstimateCallFee = ConstU32<1>;
	type MaxFeeRefund = multi_block::signed::FullSubmissionFee<Runtime, ConstU32<1>>;
	type MaxSubmissions = MaxSubmissions;
	type RewardBase = RewardBase;
	type Slash = Dap;
	type RewardSource = multi_block::signed::ReactivatingPot<SignedRewardPot, Balances>;
	type WeightInfo = super::weights::MultiBlockElectionWeightInfo;
}

parameter_types! {
	pub static BondingDuration: u32 = 3;
	pub static NominatorFastUnbondDuration: u32 = 2;
	pub static SlashDeferredDuration: u32 = 2;
	pub static SessionsPerEra: u32 = 6;
	// Begin election as soon as a new era starts.
	pub static PlanningEraOffset: u32 = 6;
	pub MaxPruningItems: u32 = 100;
	pub static ValidatorSetExportSession: SessionIndex = 4;
}

impl pallet_staking_async::Config for Runtime {
	type Filter = ();
	type RuntimeHoldReason = RuntimeHoldReason;

	type AdminOrigin = EnsureRoot<AccountId>;
	type BondingDuration = BondingDuration;
	type NominatorFastUnbondDuration = NominatorFastUnbondDuration;
	type SessionsPerEra = SessionsPerEra;
	type PlanningEraOffset = PlanningEraOffset;

	type Currency = Balances;
	type OldCurrency = Balances;
	type CurrencyBalance = Balance;
	type CurrencyToVote = ();

	type ElectionProvider = MultiBlock;

	type RewardRemainder = ();
	type EraPayout = TestEraPayout;
	type MaxEraDuration = ();
	type DisableMinting = DisableMintingMode;
	type UnclaimedRewardHandler = ();
	type RewardPots = pallet_staking_async::SequentialTest;
	type StakerRewardCalculator =
		pallet_staking_async::reward::DefaultStakerRewardCalculator<Runtime>;
	type EventListeners = ();
	type Reward = ();
	type Slash = Dap;
	type SlashDeferDuration = SlashDeferredDuration;
	type MaxPruningItems = MaxPruningItems;

	type HistoryDepth = ConstU32<7>;
	type MaxControllersInDeprecationBatch = ();

	type MaxValidatorSet = MaxValidators;
	type MaxExposurePageSize = MaxExposurePageSize;
	type MaxUnlockingChunks = ConstU32<16>;
	type NominationsQuota = pallet_staking_async::FixedNominationsQuota<16>;

	type VoterList = pallet_staking_async::UseNominatorsAndValidatorsMap<Self>;
	type TargetList = pallet_staking_async::UseValidatorsMap<Self>;

	type RcClientInterface = RcClient;

	type WeightInfo = super::weights::StakingAsyncWeightInfo;

	type IsValidatorInactive = ();
}

// Session keys type that must match RC's SessionKeys.
// This ensures that keys validated on AH can be decoded on RC.
// Uses the same OtherSessionHandler as rc::SessionKeys.
frame::deps::sp_runtime::impl_opaque_keys! {
	pub struct RCSessionKeys {
		pub other: frame::deps::sp_runtime::testing::UintAuthorityId,
	}
}

parameter_types! {
	pub static KeyDeposit: Balance = 10;
}

impl pallet_staking_async_rc_client::Config for Runtime {
	type AHStakingInterface = Staking;
	type SendToRelayChain = DeliverToRelay;
	type RelayChainOrigin = EnsureRoot<AccountId>;
	type MaxValidatorSetRetries = ConstU32<3>;
	type ValidatorSetExportSession = ValidatorSetExportSession;
	type RelayChainSessionKeys = RCSessionKeys;
	type Currency = Balances;
	type KeyDeposit = KeyDeposit;
	type WeightInfo = ();
}

parameter_types! {
	pub const DapPalletId: frame_support::PalletId = pallet_dap::DAP_PALLET_ID;
	pub const DapIssuanceCadence: u64 = 0; // drip every block
	pub const DapMaxElapsedPerDrip: u64 = 600_000;
	pub static MockTime: u64 = 0;
	pub static UseLegacyEraPayout: bool = false;
	/// Staker percentage for legacy EraPayout. Remainder = 100 - this value.
	pub static LegacyStakerPercent: u32 = 50;
}

impl frame_support::traits::Time for MockTime {
	type Moment = u64;
	fn now() -> u64 {
		Self::get()
	}
}

/// Switchable EraPayout: returns (0,0) in DAP mode, real values in legacy mode.
/// In legacy mode, uses `LegacyStakerPercent` for the staker/remainder split.
pub struct TestEraPayout;
impl pallet_staking_async::EraPayout<Balance> for TestEraPayout {
	fn era_payout(
		_total_staked: Balance,
		_total_issuance: Balance,
		era_duration_millis: u64,
	) -> (Balance, Balance) {
		if !UseLegacyEraPayout::get() {
			(0, 0)
		} else {
			// 1 token per millisecond, split per LegacyStakerPercent.
			let total = era_duration_millis as Balance;
			let stakers = Perbill::from_percent(LegacyStakerPercent::get()).mul_floor(total);
			let remainder = total - stakers;
			(stakers, remainder)
		}
	}
}

/// Switchable DisableMinting: follows UseLegacyEraPayout.
pub struct DisableMintingMode;
impl frame_support::traits::Get<bool> for DisableMintingMode {
	fn get() -> bool {
		!UseLegacyEraPayout::get()
	}
}

/// Simple issuance: 1 token per millisecond elapsed.
pub struct OneTokenPerMillisecond;
impl sp_staking::budget::IssuanceCurve<Balance> for OneTokenPerMillisecond {
	fn issue(_total_issuance: Balance, elapsed_millis: u64) -> Balance {
		elapsed_millis as Balance
	}
}

impl pallet_dap::Config for Runtime {
	type Currency = Balances;
	type PalletId = DapPalletId;
	type IssuanceCurve = OneTokenPerMillisecond;
	type BudgetRecipients = (
		pallet_dap::Pallet<Runtime>,
		pallet_staking_async::StakerRewardRecipient<pallet_staking_async::SequentialTest>,
		pallet_staking_async::ValidatorIncentiveRecipient<pallet_staking_async::SequentialTest>,
	);
	type Time = MockTime;
	type IssuanceCadence = DapIssuanceCadence;
	type MaxElapsedPerDrip = DapMaxElapsedPerDrip;
	type BudgetOrigin = frame_system::EnsureRoot<AccountId>;
	type WeightInfo = ();
}

parameter_types! {
	pub static NextRelayDeliveryFails: bool = false;
	/// XCM delivery fees charged for set_keys/purge_keys.
	pub static XcmDeliveryFee: u128 = 10;
	/// Execution cost for set_keys on relay chain.
	/// This is charged on AH even though RC uses UnpaidExecution.
	pub static SetKeysExecutionCost: u128 = 5;
	/// Execution cost for purge_keys on relay chain.
	/// This is charged on AH even though RC uses UnpaidExecution.
	pub static PurgeKeysExecutionCost: u128 = 3;
}

/// Converts an AccountId to an XCM Location.
pub struct AccountIdToLocation;
impl AccountIdToLocation {
	pub fn convert(account: AccountId) -> Location {
		Junction::AccountIndex64 { network: None, index: account }.into()
	}
}

/// Converts an XCM Location back to an AccountId.
pub struct LocationToAccountId;
impl ConvertLocation<AccountId> for LocationToAccountId {
	fn convert_location(location: &Location) -> Option<AccountId> {
		match location.unpack() {
			(0, [Junction::AccountIndex64 { index, .. }]) => Some(*index),
			_ => None,
		}
	}
}

parameter_types! {
	/// The native asset location (here).
	pub Here: Location = Location::here();
}

/// Asset transactor that uses pallet_balances for the native asset.
pub type LocalAssetTransactor =
	FungibleAdapter<Balances, IsConcrete<Here>, LocationToAccountId, AccountId, ()>;

/// Fee manager that never waives fees and burns them.
pub struct BurnFees;
impl FeeManager for BurnFees {
	fn is_waived(_origin: Option<&Location>, _reason: FeeReason) -> bool {
		false
	}
	fn handle_fee(_fee: AssetsInHolding, _context: Option<&XcmContext>, _reason: FeeReason) {
		// Fees are burned (withdrawn but not deposited anywhere)
	}
}

/// Mock XCM executor that performs real fee charging using pallet_balances.
pub struct MockXcmExecutor;

impl MockXcmExecutor {
	/// Charge fees from the given origin location.
	pub fn charge_fees(origin: Location, fees: Assets) -> XcmResult {
		if !BurnFees::is_waived(Some(&origin), FeeReason::ChargeFees) {
			let mut withdrawn = AssetsInHolding::new();
			for asset in fees.inner() {
				withdrawn
					.subsume_assets(LocalAssetTransactor::withdraw_asset(asset, &origin, None)?);
			}
			BurnFees::handle_fee(withdrawn, None, FeeReason::ChargeFees);
		}
		Ok(())
	}
}

pub struct DeliverToRelay;

impl DeliverToRelay {
	fn ensure_delivery_guard() -> Result<(), ()> {
		// `::take` will set it back to the default value, `false`.
		if NextRelayDeliveryFails::take() {
			Err(())
		} else {
			Ok(())
		}
	}

	/// Calculates and charges XCM fees for a keys operation.
	fn charge_keys_fees(
		stash: AccountId,
		execution_cost: Balance,
		max_fee: Option<Balance>,
	) -> Result<Balance, SendKeysError<Balance>> {
		let delivery_fee = XcmDeliveryFee::get();
		let total_fee = delivery_fee.saturating_add(execution_cost);

		if let Some(max) = max_fee {
			if total_fee > max {
				return Err(SendKeysError::FeesExceededMax { required: total_fee, max });
			}
		}

		let fees = Assets::from(Asset {
			id: AssetId(Location::here()),
			fun: Fungibility::Fungible(total_fee),
		});
		let payer_location = AccountIdToLocation::convert(stash);
		MockXcmExecutor::charge_fees(payer_location, fees)
			.map_err(|_| SendKeysError::Send(SendOperationError::ChargeFeesFailed))?;

		Ok(total_fee)
	}
}

impl pallet_staking_async_rc_client::SendToRelayChain for DeliverToRelay {
	type AccountId = AccountId;
	type Balance = Balance;

	fn validator_set(
		report: pallet_staking_async_rc_client::ValidatorSetReport<Self::AccountId>,
	) -> Result<(), ()> {
		Self::ensure_delivery_guard()?;
		if let Some(mut local_queue) = LocalQueue::get() {
			local_queue.push((System::block_number(), OutgoingMessages::ValidatorSet(report)));
			LocalQueue::set(Some(local_queue));
		} else {
			shared::CounterAHRCValidatorSet::mutate(|x| *x += 1);
			shared::in_rc(|| {
				let origin = crate::rc::RuntimeOrigin::root();
				pallet_staking_async_ah_client::Pallet::<crate::rc::Runtime>::validator_set(
					origin,
					report.clone(),
				)
				.unwrap();
			});
		}
		Ok(())
	}

	fn set_keys(
		stash: Self::AccountId,
		keys: Vec<u8>,
		max_delivery_and_remote_execution_fee: Option<Self::Balance>,
	) -> Result<Self::Balance, SendKeysError<Self::Balance>> {
		Self::ensure_delivery_guard()
			.map_err(|()| SendKeysError::Send(SendOperationError::DeliveryFailed))?;

		let total_fee = Self::charge_keys_fees(
			stash,
			SetKeysExecutionCost::get(),
			max_delivery_and_remote_execution_fee,
		)?;

		if let Some(mut local_queue) = LocalQueue::get() {
			local_queue.push((System::block_number(), OutgoingMessages::SetKeys { stash, keys }));
			LocalQueue::set(Some(local_queue));
		} else {
			// Forward to RC (uses UnpaidExecution, so no fees charged there)
			shared::in_rc(|| {
				let origin = crate::rc::RuntimeOrigin::root();
				pallet_staking_async_ah_client::Pallet::<crate::rc::Runtime>::set_keys_from_ah(
					origin,
					stash,
					keys.clone(),
				)
				.unwrap();
			});
		}

		Ok(total_fee)
	}

	fn purge_keys(
		stash: Self::AccountId,
		max_delivery_and_remote_execution_fee: Option<Self::Balance>,
	) -> Result<Self::Balance, SendKeysError<Self::Balance>> {
		Self::ensure_delivery_guard()
			.map_err(|()| SendKeysError::Send(SendOperationError::DeliveryFailed))?;

		let total_fee = Self::charge_keys_fees(
			stash,
			PurgeKeysExecutionCost::get(),
			max_delivery_and_remote_execution_fee,
		)?;

		if let Some(mut local_queue) = LocalQueue::get() {
			local_queue.push((System::block_number(), OutgoingMessages::PurgeKeys { stash }));
			LocalQueue::set(Some(local_queue));
		} else {
			// Forward to RC (uses UnpaidExecution, so no fees charged there)
			shared::in_rc(|| {
				let origin = crate::rc::RuntimeOrigin::root();
				pallet_staking_async_ah_client::Pallet::<crate::rc::Runtime>::purge_keys_from_ah(
					origin, stash,
				)
				.unwrap();
			});
		}

		Ok(total_fee)
	}
}

const INITIAL_BALANCE: Balance = 1000;
const INITIAL_STAKE: Balance = 100;

#[derive(Clone, Debug, PartialEq)]
pub enum OutgoingMessages {
	ValidatorSet(pallet_staking_async_rc_client::ValidatorSetReport<AccountId>),
	SetKeys { stash: AccountId, keys: Vec<u8> },
	PurgeKeys { stash: AccountId },
}

parameter_types! {
	pub static LocalQueue: Option<Vec<(BlockNumber, OutgoingMessages)>> = None;
	pub static LocalQueueLastIndex: usize = 0;
}

impl LocalQueue {
	pub fn get_since_last_call() -> Vec<(BlockNumber, OutgoingMessages)> {
		if let Some(all) = Self::get() {
			let last = LocalQueueLastIndex::get();
			LocalQueueLastIndex::set(all.len());
			all.into_iter().skip(last).collect()
		} else {
			panic!("Must set local_queue()!")
		}
	}

	pub fn flush() {
		let _ = Self::get_since_last_call();
	}
}

pub struct ExtBuilder {
	// if true, emulate pre-ahm-migration state
	pre_migration: bool,
}

impl Default for ExtBuilder {
	fn default() -> Self {
		Self { pre_migration: false }
	}
}

impl ExtBuilder {
	/// Set this if you want to emulate pre-migration state of staking-async.
	pub fn pre_migration(self) -> Self {
		Self { pre_migration: true }
	}

	/// Set this if you want to test the ah-runtime locally. This will push outgoing messages to
	/// `LocalQueue` instead of enacting them on RC.
	pub fn local_queue(self) -> Self {
		LocalQueue::set(Some(Default::default()));
		self
	}

	pub fn slash_defer_duration(self, duration: u32) -> Self {
		SlashDeferredDuration::set(duration);
		self
	}

	pub fn build(self) -> TestState {
		let _ = sp_tracing::try_init_simple();
		let mut t = frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap();

		// Note: The state in pallet-staking-async is retained even when pre-migration is set.
		// This does not impact the tests, but for strict accuracy, be aware that the state isn't
		// fully representative.
		let validators = vec![1, 2, 3, 4, 5, 6, 7, 8]
			.into_iter()
			.map(|x| (x, INITIAL_STAKE, pallet_staking_async::StakerStatus::Validator));

		let nominators = vec![
			(100, vec![1, 2]),
			(101, vec![2, 5]),
			(102, vec![1, 1]),
			(103, vec![3, 3]),
			(104, vec![1, 5]),
			(105, vec![5, 4]),
			(106, vec![6, 2]),
			(107, vec![1, 6]),
			(108, vec![2, 7]),
			(109, vec![4, 8]),
			(110, vec![5, 2]),
			(111, vec![6, 6]),
			(112, vec![8, 1]),
		]
		.into_iter()
		.map(|(x, y)| (x, INITIAL_STAKE, pallet_staking_async::StakerStatus::Nominator(y)));

		let stakers = validators.chain(nominators).collect::<Vec<_>>();
		let mut balances: Vec<_> =
			stakers.clone().into_iter().map(|(x, _, _)| (x, INITIAL_BALANCE)).collect();
		// Fund DAP buffer account with ED so it can receive slashes.
		use frame_support::sp_runtime::traits::AccountIdConversion;
		let dap_buffer: AccountId = DapPalletId::get().into_account_truncating();
		balances.push((dap_buffer, 1));

		pallet_balances::GenesisConfig::<Runtime> { balances, ..Default::default() }
			.assimilate_storage(&mut t)
			.unwrap();

		pallet_staking_async::GenesisConfig::<Runtime> {
			stakers,
			validator_count: 4,
			active_era: (0, 0, 0),
			force_era: if self.pre_migration { Forcing::ForceNone } else { Forcing::default() },
			..Default::default()
		}
		.assimilate_storage(&mut t)
		.unwrap();

		let mut state: TestState = t.into();

		state.execute_with(|| {
			if !UseLegacyEraPayout::get() {
				setup_dap();
			}

			// initialises events
			roll_next();
		});

		state
	}
}

pub(crate) fn staker_reward_key() -> sp_staking::budget::BudgetKey {
	<pallet_staking_async::StakerRewardRecipient<pallet_staking_async::SequentialTest> as
		BudgetRecipient<AccountId>>::budget_key()
}

pub(crate) fn validator_incentive_key() -> sp_staking::budget::BudgetKey {
	<pallet_staking_async::ValidatorIncentiveRecipient<pallet_staking_async::SequentialTest> as
		BudgetRecipient<AccountId>>::budget_key()
}

pub(crate) fn buffer_key() -> sp_staking::budget::BudgetKey {
	<pallet_dap::Pallet<Runtime> as BudgetRecipient<AccountId>>::budget_key()
}

pub(crate) fn build_budget(
	entries: &[(sp_staking::budget::BudgetKey, u32)],
) -> pallet_dap::BudgetAllocationMap {
	let mut budget = pallet_dap::BudgetAllocationMap::new();
	for (key, pct) in entries {
		budget.try_insert(key.clone(), Perbill::from_percent(*pct)).unwrap();
	}
	budget
}

/// Set up DAP infrastructure: budget allocation, timestamp, fund pots with ED.
pub(crate) fn setup_dap() {
	pallet_dap::BudgetAllocation::<Runtime>::put(build_budget(&[
		(staker_reward_key(), 50),
		(buffer_key(), 50),
	]));

	pallet_dap::LastIssuanceTimestamp::<Runtime>::put(MockTime::get());

	// Fund general pots with ED to keep them alive.
	let general_staker = pallet_staking_async::SequentialTest::pot_account(
		pallet_staking_async::RewardPot::General(pallet_staking_async::RewardKind::StakerRewards),
	);
	Balances::mint_into(&general_staker, 1).unwrap();

	let general_incentive = pallet_staking_async::SequentialTest::pot_account(
		pallet_staking_async::RewardPot::General(
			pallet_staking_async::RewardKind::ValidatorSelfStake,
		),
	);
	Balances::mint_into(&general_incentive, 1).unwrap();
}

parameter_types! {
	static StakingEventsIndex: usize = 0;
	static ElectionEventsIndex: usize = 0;
	static VerifierEventsIndex: usize = 0;
	static RcClientEventsIndex: usize = 0;
	static SignedEventsIndex: usize = 0;
}

pub(crate) fn signed_events_since_last_call() -> Vec<multi_block::signed::Event<T>> {
	let all: Vec<_> = System::events()
		.into_iter()
		.filter_map(|r| {
			if let RuntimeEvent::MultiBlockSigned(inner) = r.event {
				Some(inner)
			} else {
				None
			}
		})
		.collect();
	let seen = SignedEventsIndex::get();
	SignedEventsIndex::set(all.len());
	all.into_iter().skip(seen).collect()
}

pub(crate) fn rc_client_events_since_last_call() -> Vec<pallet_staking_async_rc_client::Event<T>> {
	let all: Vec<_> = System::events()
		.into_iter()
		.filter_map(
			|r| if let RuntimeEvent::RcClient(inner) = r.event { Some(inner) } else { None },
		)
		.collect();
	let seen = RcClientEventsIndex::get();
	RcClientEventsIndex::set(all.len());
	all.into_iter().skip(seen).collect()
}

pub(crate) fn staking_events_since_last_call() -> Vec<pallet_staking_async::Event<T>> {
	let all: Vec<_> = System::events()
		.into_iter()
		.filter_map(|r| if let RuntimeEvent::Staking(inner) = r.event { Some(inner) } else { None })
		.collect();
	let seen = StakingEventsIndex::get();
	StakingEventsIndex::set(all.len());
	all.into_iter().skip(seen).collect()
}

pub(crate) fn election_events_since_last_call() -> Vec<multi_block::Event<T>> {
	let all: Vec<_> = System::events()
		.into_iter()
		.filter_map(
			|r| if let RuntimeEvent::MultiBlock(inner) = r.event { Some(inner) } else { None },
		)
		.collect();
	let seen = ElectionEventsIndex::get();
	ElectionEventsIndex::set(all.len());
	all.into_iter().skip(seen).collect()
}

pub(crate) enum AssertSessionType {
	/// A new election is planned in the starting session and result is exported immediately
	ElectionWithImmediateExport,
	/// A new session is planned in the starting session. The result is buffered to be exported
	/// later.
	ElectionWithBufferedExport,
	/// No election happens in the starting session. A previously elected set is exported.
	IdleOnlyExport,
	/// No election and no export in the starting session.
	IdleNoExport,
}

pub(crate) fn end_session_with(activate: bool, assert_starting_session: AssertSessionType) {
	match assert_starting_session {
		AssertSessionType::ElectionWithImmediateExport => {
			end_session_and_assert_election(activate, true)
		},
		AssertSessionType::ElectionWithBufferedExport => {
			end_session_and_assert_election(activate, false)
		},
		AssertSessionType::IdleOnlyExport => end_session_and_assert_idle(activate, true),
		AssertSessionType::IdleNoExport => end_session_and_assert_idle(activate, false),
	}
}

/// End ongoing session.
///
/// - Send activation timestamp if `activate` set.
/// - Expect validator set to be exported in the starting session if `expect_export` set.
fn end_session_and_assert_idle(activate: bool, expect_export: bool) {
	// cache values for state assertion checks later
	let active_era = ActiveEra::<T>::get().unwrap().index;
	let planning_era = CurrentEra::<T>::get().unwrap();
	let old_validator_set_export_count = LocalQueue::get().unwrap().len();
	let was_outgoing_set = OutgoingValidatorSet::<T>::get().is_some();
	let old_era_points = pallet_staking_async::ErasRewardPoints::<T>::get(&active_era);

	let activation_timestamp =
		if activate { Some((planning_era as u64 * 1000, planning_era as u32)) } else { None };

	let last_session_end_index =
		pallet_staking_async_rc_client::LastSessionReportEndingIndex::<T>::get()
			.unwrap_or_default();
	let end_index = last_session_end_index + 1;

	// end the session..
	assert_ok!(pallet_staking_async_rc_client::Pallet::<T>::relay_session_report(
		RuntimeOrigin::root(),
		SessionReport {
			end_index,
			validator_points: vec![(1, 10)],
			activation_timestamp,
			leftover: false,
		}
	));

	let expected_active = if activate { active_era + 1 } else { active_era };
	assert_eq!(
		staking_events_since_last_call().last().unwrap().clone(),
		pallet_staking_async::Event::SessionRotated {
			starting_session: end_index + 1,
			active_era: expected_active,
			// not election session, so this should not change.
			planned_era: planning_era,
		}
	);

	// this ensures any multi-block function is triggered.
	roll_next();

	assert_eq!(ActiveEra::<T>::get().unwrap().index, expected_active);
	// should be no change in planning era
	assert_eq!(CurrentEra::<T>::get().unwrap(), planning_era);

	// ensure era points are updated correctly
	let updated_era_points = pallet_staking_async::ErasRewardPoints::<T>::get(&active_era);
	// era points are updated
	assert_eq!(updated_era_points.total, old_era_points.total + 10);
	assert_eq!(
		updated_era_points.individual.get(&1).unwrap().clone(),
		old_era_points.individual.get(&1).unwrap_or(&0) + 10
	);

	if expect_export {
		// was set before
		assert!(was_outgoing_set);
		// not set anymore
		assert!(OutgoingValidatorSet::<T>::get().is_none());
		// new validator set exported
		assert_eq!(LocalQueue::get().unwrap().len(), old_validator_set_export_count + 1);
	} else {
		// no new messages in the queue
		assert_eq!(LocalQueue::get().unwrap().len(), old_validator_set_export_count);
	}
}

/// End ongoing session.
///
/// Assert this is an election session and roll blocks until election completes. Also:
/// - Send activation timestamp if `activate` set.
/// - Expect validator set to be exported in the starting session if `expect_export` set.
fn end_session_and_assert_election(activate: bool, expect_export: bool) {
	// clear events
	election_events_since_last_call();

	// cache values for state assertion checks later
	let active_era = ActiveEra::<T>::get().unwrap().index;
	let planning_era = CurrentEra::<T>::get().unwrap();
	let old_validator_set_export_count = LocalQueue::get().unwrap().len();
	let was_outgoing_set = OutgoingValidatorSet::<T>::get().is_some();
	let old_era_points = pallet_staking_async::ErasRewardPoints::<T>::get(&active_era);
	assert_eq!(multi_block::CurrentPhase::<T>::get(), Phase::Off);

	let activation_timestamp =
		if activate { Some((planning_era as u64 * 1000, planning_era as u32)) } else { None };

	let last_session_end_index =
		pallet_staking_async_rc_client::LastSessionReportEndingIndex::<T>::get()
			.unwrap_or_default();
	let end_index = last_session_end_index + 1;

	// end the session
	assert_ok!(pallet_staking_async_rc_client::Pallet::<T>::relay_session_report(
		RuntimeOrigin::root(),
		SessionReport {
			end_index: last_session_end_index + 1,
			validator_points: vec![(1, 10)],
			activation_timestamp,
			leftover: false,
		}
	));

	let expected_active = if activate { active_era + 1 } else { active_era };
	assert_eq!(ActiveEra::<T>::get().unwrap().index, expected_active);

	assert_eq!(
		staking_events_since_last_call().last().unwrap().clone(),
		pallet_staking_async::Event::SessionRotated {
			starting_session: end_index + 1,
			active_era: expected_active,
			// since this is an election session, planning era increments.
			planned_era: planning_era + 1,
		}
	);

	// this ensures any multi-block function is triggered.
	roll_next();

	// ensure era points are updated correctly
	let updated_era_points = pallet_staking_async::ErasRewardPoints::<T>::get(&active_era);
	// era points are updated
	assert_eq!(updated_era_points.total, old_era_points.total + 10);
	assert_eq!(
		updated_era_points.individual.get(&1).unwrap().clone(),
		old_era_points.individual.get(&1).unwrap_or(&0) + 10
	);

	// planning era is incremented indicating start of election
	assert_eq!(CurrentEra::<T>::get().unwrap(), planning_era + 1);
	assert_eq!(
		election_events_since_last_call(),
		[ElectionEvent::PhaseTransitioned { from: Phase::Off, to: Phase::Snapshot(Pages::get()) }]
	);

	while multi_block::CurrentPhase::<T>::get() != Phase::Off {
		roll_next();
	}

	// Phase is off now.
	assert_eq!(multi_block::CurrentPhase::<T>::get(), Phase::Off);

	if expect_export {
		// if its immediate export mode, the validator set is exported soon after its received by
		// rc client pallet
		assert_eq!(LocalQueue::get().unwrap().len(), old_validator_set_export_count);
		roll_next();
		assert_eq!(LocalQueue::get().unwrap().len(), old_validator_set_export_count + 1);
	} else {
		// the validator set is buffered
		assert_eq!(LocalQueue::get().unwrap().len(), old_validator_set_export_count);
		// assert outgoing was not set, and now set.
		assert!(!was_outgoing_set);
		assert!(OutgoingValidatorSet::<T>::exists());
		// ensure rolling few blocks still won't export the set
		hypothetically!({
			roll_many(10);
			assert_eq!(LocalQueue::get().unwrap().len(), old_validator_set_export_count);
			assert!(OutgoingValidatorSet::<T>::exists());
		});
	}
}

pub(crate) fn verifier_events_since_last_call() -> Vec<multi_block::verifier::Event<T>> {
	let all: Vec<_> = System::events()
		.into_iter()
		.filter_map(|r| {
			if let RuntimeEvent::MultiBlockVerifier(inner) = r.event {
				Some(inner)
			} else {
				None
			}
		})
		.collect();
	let seen = VerifierEventsIndex::get();
	VerifierEventsIndex::set(all.len());
	all.into_iter().skip(seen).collect()
}

// Proxy
#[derive(
	Copy,
	Clone,
	Eq,
	PartialEq,
	Ord,
	PartialOrd,
	codec::Encode,
	codec::Decode,
	codec::DecodeWithMemTracking,
	Debug,
	codec::MaxEncodedLen,
	scale_info::TypeInfo,
)]
pub enum ProxyType {
	Any,
	Staking,
}

impl Default for ProxyType {
	fn default() -> Self {
		Self::Any
	}
}

impl frame_support::traits::InstanceFilter<RuntimeCall> for ProxyType {
	fn filter(&self, c: &RuntimeCall) -> bool {
		match self {
			ProxyType::Any => true,
			ProxyType::Staking => matches!(
				c,
				RuntimeCall::Staking(..) |
					RuntimeCall::RcClient(
						pallet_staking_async_rc_client::Call::set_keys { .. } |
							pallet_staking_async_rc_client::Call::purge_keys { .. }
					)
			),
		}
	}

	fn is_superset(&self, o: &Self) -> bool {
		match (self, o) {
			(x, y) if x == y => true,
			(ProxyType::Any, _) => true,
			_ => false,
		}
	}
}

impl pallet_proxy::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type Currency = Balances;
	type ProxyType = ProxyType;
	type ProxyDepositBase = ConstU128<1>;
	type ProxyDepositFactor = ConstU128<1>;
	type MaxProxies = ConstU32<32>;
	type MaxPending = ConstU32<32>;
	type CallHasher = frame::deps::sp_runtime::traits::BlakeTwo256;
	type AnnouncementDepositBase = ConstU128<1>;
	type AnnouncementDepositFactor = ConstU128<1>;
	type WeightInfo = ();
	type BlockNumberProvider = System;
}
