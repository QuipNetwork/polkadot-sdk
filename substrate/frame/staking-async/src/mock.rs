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

//! Test utilities

use crate::{
	self as pallet_staking_async,
	session_rotation::{Eras, Rotator},
	*,
};
use frame_election_provider_support::{
	bounds::{ElectionBounds, ElectionBoundsBuilder},
	onchain, BoundedSupports, BoundedSupportsOf, ElectionProvider, PageIndex, SequentialPhragmen,
	Support, VoteWeight,
};
use frame_support::{
	assert_ok, derive_impl, ord_parameter_types, parameter_types,
	traits::{EitherOfDiverse, Get, Imbalance, OnUnbalanced},
	weights::constants::RocksDbWeight,
};
use frame_system::{pallet_prelude::BlockNumberFor, EnsureRoot, EnsureSignedBy};
use pallet_staking_async_rc_client as rc_client;
use sp_core::{ConstBool, ConstU64};
use sp_io;
use sp_npos_elections::BalancingConfig;
use sp_runtime::{traits::Zero, BuildStorage, Weight};
use sp_staking::{
	budget::{BudgetRecipient, IssuanceCurve},
	currency_to_vote::SaturatingCurrencyToVote,
	OnStakingUpdate, SessionIndex, StakingAccount,
};
use std::collections::BTreeMap;

pub(crate) const INIT_TIMESTAMP: u64 = 30_000;
pub(crate) const BLOCK_TIME: u64 = 1000;

frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		Dap: pallet_dap,
		Staking: pallet_staking_async,
		VoterBagsList: pallet_bags_list::<Instance1>,
	}
);

pub(crate) type T = Test;
pub(crate) type Runtime = Test;
pub(crate) type AccountId = <Runtime as frame_system::Config>::AccountId;
pub(crate) type BlockNumber = BlockNumberFor<Runtime>;
pub(crate) type Balance = <Runtime as pallet_balances::Config>::Balance;

#[derive(Clone, Copy)]
pub enum PlanningEraMode {
	Fixed(SessionIndex),
	Smart,
}

pub struct PlanningEraOffset;
impl Get<SessionIndex> for PlanningEraOffset {
	fn get() -> SessionIndex {
		match PlanningEraModeVal::get() {
			PlanningEraMode::Fixed(value) => value,
			PlanningEraMode::Smart => crate::PlanningEraOffsetOf::<T, Period, ConstU64<0>>::get(),
		}
	}
}

parameter_types! {
	pub static ExistentialDeposit: Balance = 1;
	pub static SlashDeferDuration: EraIndex = 0;
	pub static MaxControllersInDeprecationBatch: u32 = 5900;
	pub static BondingDuration: EraIndex = 3;
	pub static NominatorFastUnbondDuration: EraIndex = 2;
	pub static HistoryDepth: u32 = 80;
	pub static MaxExposurePageSize: u32 = 64;
	pub static MaxUnlockingChunks: u32 = 32;
	pub static RewardOnUnbalanceWasCalled: bool = false;
	pub static MaxValidatorSet: u32 = 100;
	pub static ElectionsBounds: ElectionBounds = ElectionBoundsBuilder::default().build();
	pub static AbsoluteMaxNominations: u32 = 16;
	pub static PlanningEraModeVal: PlanningEraMode = PlanningEraMode::Fixed(2);
	// Session configs
	pub static SessionsPerEra: SessionIndex = 3;
	pub static Period: BlockNumber = 5;
	pub static Offset: BlockNumber = 0;
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type DbWeight = RocksDbWeight;
	type Block = frame_system::mocking::MockBlock<Test>;
	type AccountData = pallet_balances::AccountData<Balance>;
}
#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type MaxLocks = frame_support::traits::ConstU32<1024>;
	type Balance = u128;
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
}

parameter_types! {
	pub static RewardRemainderUnbalanced: u128 = 0;
}
pub struct RewardRemainderMock;
impl OnUnbalanced<NegativeImbalanceOf<Test>> for RewardRemainderMock {
	fn on_nonzero_unbalanced(amount: NegativeImbalanceOf<Test>) {
		RewardRemainderUnbalanced::mutate(|v| {
			*v += amount.peek();
		});
		drop(amount);
	}
}

pub(crate) const THRESHOLDS: [sp_npos_elections::VoteWeight; 9] =
	[10, 20, 30, 40, 50, 60, 1_000, 2_000, 10_000];

parameter_types! {
	pub static BagThresholds: &'static [sp_npos_elections::VoteWeight] = &THRESHOLDS;
}

pub type VoterBagsListInstance = pallet_bags_list::Instance1;
impl pallet_bags_list::Config<VoterBagsListInstance> for Test {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = ();
	// Staking is the source of truth for voter bags list, since they are not kept up to date.
	type ScoreProvider = Staking;
	type BagThresholds = BagThresholds;
	type MaxAutoRebagPerBlock = ();
	type Score = VoteWeight;
}

// multi-page types and controller.
parameter_types! {
	pub static Pages: PageIndex = 1;
	pub static MaxBackersPerWinner: u32 = 256;
	pub static MaxWinnersPerPage: u32 = MaxValidatorSet::get();
	pub static StartReceived: Option<BlockNumber> = None;
	pub static ElectionDelay: BlockNumber = 0;
}

pub type InnerElection = onchain::OnChainExecution<OnChainSeqPhragmen>;
pub struct Balancing;
impl Get<Option<BalancingConfig>> for Balancing {
	fn get() -> Option<BalancingConfig> {
		Some(BalancingConfig { iterations: 5, tolerance: 0 })
	}
}

pub struct OnChainSeqPhragmen;
impl onchain::Config for OnChainSeqPhragmen {
	type System = Test;
	type Solver = SequentialPhragmen<AccountId, Perbill, Balancing>;
	type DataProvider = Staking;
	type WeightInfo = ();
	type Bounds = ElectionsBounds;
	type Sort = ConstBool<true>;
	type MaxBackersPerWinner = MaxBackersPerWinner;
	type MaxWinnersPerPage = MaxWinnersPerPage;
}

pub struct TestElectionProvider;
impl ElectionProvider for TestElectionProvider {
	type AccountId = AccountId;
	type BlockNumber = BlockNumber;
	type MaxWinnersPerPage = MaxWinnersPerPage;
	type MaxBackersPerWinner = MaxBackersPerWinner;
	type MaxBackersPerWinnerFinal = MaxBackersPerWinner;
	type Pages = Pages;
	type DataProvider = Staking;
	type Error = onchain::Error;

	fn elect(page: PageIndex) -> Result<BoundedSupportsOf<Self>, Self::Error> {
		if page == 0 {
			StartReceived::set(None);
		}
		InnerElection::elect(page)
	}
	fn start() -> Result<(), Self::Error> {
		StartReceived::set(Some(System::block_number()));
		Ok(())
	}
	fn duration() -> Self::BlockNumber {
		InnerElection::duration() + ElectionDelay::get()
	}
	fn status() -> Result<Option<Weight>, ()> {
		let now = System::block_number();
		match StartReceived::get() {
			Some(at) if now - at >= ElectionDelay::get() => Ok(Some(Default::default())),
			Some(_) => Ok(None),
			None => Err(()),
		}
	}
}
pub struct MockReward {}
impl OnUnbalanced<PositiveImbalanceOf<Test>> for MockReward {
	fn on_unbalanced(_: PositiveImbalanceOf<Test>) {
		RewardOnUnbalanceWasCalled::set(true);
	}
}

parameter_types! {
	pub static LedgerSlashPerEra:
		(BalanceOf<Test>, BTreeMap<EraIndex, BalanceOf<Test>>) =
		(Zero::zero(), BTreeMap::new());
	pub static SlashObserver: BTreeMap<AccountId, BalanceOf<Test>> = BTreeMap::new();
	pub static RestrictedAccounts: Vec<AccountId> = Vec::new();
}

pub struct EventListenerMock;
impl OnStakingUpdate<AccountId, Balance> for EventListenerMock {
	fn on_slash(
		pool_account: &AccountId,
		slashed_bonded: Balance,
		slashed_chunks: &BTreeMap<EraIndex, Balance>,
		total_slashed: Balance,
	) {
		LedgerSlashPerEra::set((slashed_bonded, slashed_chunks.clone()));
		SlashObserver::mutate(|map| {
			map.insert(*pool_account, map.get(pool_account).unwrap_or(&0) + total_slashed)
		});
	}
}

pub struct MockedRestrictList;
impl Contains<AccountId> for MockedRestrictList {
	fn contains(who: &AccountId) -> bool {
		RestrictedAccounts::get().contains(who)
	}
}

/// A representation of the session pallet that lives on the relay chain.
pub mod session_mock {
	use super::*;
	use frame_support::{
		traits::{OnInitialize, OnPoll},
		weights::WeightMeter,
	};
	use pallet_staking_async_rc_client::ValidatorSetReport;

	pub struct Session;

	impl Session {
		pub fn queued_validators() -> Option<Vec<AccountId>> {
			Queued::get()
		}

		pub fn validators() -> Vec<AccountId> {
			Active::get()
		}

		pub fn current_index() -> SessionIndex {
			CurrentIndex::get()
		}

		pub fn roll_until(block: BlockNumber) {
			while System::block_number() < block {
				Self::roll_next();
			}
		}

		pub fn roll_next() {
			let now = System::block_number();
			Timestamp::mutate(|ts| *ts += BLOCK_TIME);
			System::set_block_number(now + 1);
			<AllPalletsWithSystem as OnInitialize<BlockNumber>>::on_initialize(now + 1);
			<AllPalletsWithSystem as OnPoll<BlockNumber>>::on_poll(
				now + 1,
				&mut WeightMeter::new(),
			);
			Self::maybe_rotate_session_now();
		}

		pub fn roll_to_next_session() {
			let current = Self::current_index();
			while Self::current_index() != (current + 1) {
				Self::roll_next();
			}
		}

		pub fn roll_until_session(session: SessionIndex) {
			while Self::current_index() != session {
				Self::roll_next();
			}
		}

		pub fn roll_until_active_era(era: EraIndex) {
			while active_era() != era {
				Self::roll_next();
			}
		}

		pub fn roll_until_active_era_with(era: EraIndex, op: impl Fn() -> ()) {
			while active_era() != era {
				Self::roll_next();
				op()
			}
		}

		fn maybe_rotate_session_now() {
			let now = System::block_number();
			let period = Period::get();
			if now.is_multiple_of(period) {
				Self::advance_session();
			}
		}

		fn advance_session() {
			let ending = Self::current_index();
			if let Some((q, id)) = Queued::get().zip(QueuedId::get()) {
				// we have something queued
				if QueuedBufferSessions::get() == 0 {
					// buffer time has passed
					Active::set(q);
					<Staking as rc_client::AHStakingInterface>::on_relay_session_report(
						rc_client::SessionReport::new_terminal(
							ending,
							// TODO: currently we use `Eras::reward_active_era()` to set validator
							// points in our tests. We should improve this and find a good way to
							// set this value instead.
							vec![],
							Some((Timestamp::get(), id)),
						),
					);
					Queued::reset();
					QueuedId::reset();
				} else {
					QueuedBufferSessions::mutate(|s| *s -= 1);
					<Staking as rc_client::AHStakingInterface>::on_relay_session_report(
						rc_client::SessionReport::new_terminal(ending, vec![], None),
					);
				}
			} else {
				// just end the session.
				<Staking as rc_client::AHStakingInterface>::on_relay_session_report(
					rc_client::SessionReport::new_terminal(ending, vec![], None),
				);
			}
			CurrentIndex::set(ending + 1);
		}
	}

	parameter_types! {
		pub static ReceivedValidatorSets
			: BTreeMap<BlockNumber, ValidatorSetReport<AccountId>>
			= BTreeMap::new();
		pub static Queued: Option<Vec<AccountId>> = None;
		pub static QueuedId: Option<u32> = None;
		pub static QueuedBufferSessions: BlockNumber = 1;
		pub static Active: Vec<AccountId> = Vec::new();
		pub static CurrentIndex: u32 = 0;
		pub static Timestamp: u64 = INIT_TIMESTAMP;
	}

	impl ReceivedValidatorSets {
		pub fn get_last() -> ValidatorSetReport<AccountId> {
			let mut data = Self::get();
			data.pop_last().unwrap().1
		}
	}

	impl pallet_staking_async_rc_client::RcClientInterface for Session {
		type AccountId = AccountId;

		fn validator_set(
			new_validator_set: Vec<Self::AccountId>,
			id: u32,
			prune_up_to: Option<u32>,
		) {
			log::debug!(target: "runtime::staking-async::session_mock", "Received validator set: {:?}", new_validator_set);
			let now = System::block_number();
			// store the report for further inspection.
			ReceivedValidatorSets::mutate(|reports| {
				reports.insert(
					now,
					ValidatorSetReport {
						id,
						new_validator_set: new_validator_set.clone(),
						prune_up_to,
						leftover: false,
					},
				);
			});

			// queue the validator set.
			Queued::set(Some(new_validator_set));
			QueuedId::set(Some(id));
			QueuedBufferSessions::set(1);
		}
	}
}

pub use session_mock::Session;

ord_parameter_types! {
	pub const One: u64 = 1;
}

parameter_types! {
	pub static UseLegacyEraPayout: bool = false;
	pub static RemainderRatio: Perbill = Perbill::from_percent(50);
	pub static MaxEraDuration: u64 = time_per_era() * 7;
	pub const MaxPruningItems: u32 = 100;
	pub const DapPalletId: frame_support::PalletId = pallet_dap::DAP_PALLET_ID;
	pub const TestIssuanceCadence: u64 = 0; // drip every block
	pub const TestMaxElapsedPerDrip: u64 = 600_000; // 10 minutes
}

/// Mock time provider backed by session_mock::Timestamp.
pub struct MockTime;
impl frame_support::traits::Time for MockTime {
	type Moment = u64;
	fn now() -> u64 {
		session_mock::Timestamp::get()
	}
}

/// Switchable EraPayout: returns (0,0) in DAP mode, real values in legacy mode.
pub struct TestEraPayout;
impl EraPayout<Balance> for TestEraPayout {
	fn era_payout(
		_total_staked: Balance,
		_total_issuance: Balance,
		era_duration_millis: u64,
	) -> (Balance, Balance) {
		if !UseLegacyEraPayout::get() {
			// DAP mode: return (0, 0)
			(0, 0)
		} else {
			// Legacy mode: same as old OneTokenPerMillisecond + RemainderRatio
			let total = era_duration_millis as Balance;
			let remainder = RemainderRatio::get() * total;
			let stakers = total - remainder;
			(stakers, remainder)
		}
	}
}

/// DisableMinting follows UseLegacyEraPayout: when legacy is on, minting is NOT disabled.
pub struct DisableMintingMode;
impl Get<bool> for DisableMintingMode {
	fn get() -> bool {
		!UseLegacyEraPayout::get()
	}
}

pub fn general_staker_pot() -> AccountId {
	SequentialTest::pot_account(RewardPot::General(RewardKind::StakerRewards))
}

impl pallet_dap::Config for Test {
	type Currency = Balances;
	type PalletId = DapPalletId;
	type IssuanceCurve = OneTokenPerMillisecond;
	type BudgetRecipients =
		(Dap, StakerRewardRecipient<SequentialTest>, ValidatorIncentiveRecipient<SequentialTest>);
	type Time = MockTime;
	type IssuanceCadence = TestIssuanceCadence;
	type MaxElapsedPerDrip = TestMaxElapsedPerDrip;
	type BudgetOrigin = EnsureRoot<AccountId>;
	type WeightInfo = ();
}

pub struct OneTokenPerMillisecond;
impl IssuanceCurve<Balance> for OneTokenPerMillisecond {
	fn issue(_total_issuance: Balance, elapsed_millis: u64) -> Balance {
		elapsed_millis as Balance
	}
}

pub(crate) fn staker_reward_key() -> sp_staking::budget::BudgetKey {
	<StakerRewardRecipient<SequentialTest> as BudgetRecipient<AccountId>>::budget_key()
}

pub(crate) fn validator_incentive_key() -> sp_staking::budget::BudgetKey {
	<ValidatorIncentiveRecipient<SequentialTest> as BudgetRecipient<AccountId>>::budget_key()
}

pub(crate) fn general_incentive_pot() -> AccountId {
	SequentialTest::pot_account(RewardPot::General(RewardKind::ValidatorSelfStake))
}

pub(crate) fn buffer_key() -> sp_staking::budget::BudgetKey {
	<Dap as BudgetRecipient<AccountId>>::budget_key()
}

pub(crate) fn build_budget(
	entries: &[(sp_staking::budget::BudgetKey, u32)],
) -> pallet_dap::BudgetAllocationMap {
	let mut budget = BoundedBTreeMap::new();
	for (key, pct) in entries {
		budget.try_insert(key.clone(), Perbill::from_percent(*pct)).unwrap();
	}
	budget
}

pub(crate) fn default_budget() -> pallet_dap::BudgetAllocationMap {
	build_budget(&[(staker_reward_key(), 50), (buffer_key(), 50)])
}

/// Set up DAP infrastructure: budget allocation, timestamp, fund pots with ED.
pub(crate) fn setup_dap() {
	pallet_dap::BudgetAllocation::<Test>::put(default_budget());
	pallet_dap::LastIssuanceTimestamp::<Test>::put(session_mock::Timestamp::get());
	let ed = ExistentialDeposit::get();
	<Balances as frame_support::traits::fungible::Mutate<_>>::mint_into(&general_staker_pot(), ed)
		.expect("mint general staker pot");
	let dap_buffer =
		<pallet_dap::Pallet<Test> as sp_staking::budget::BudgetRecipient<AccountId>>::pot_account();
	<Balances as frame_support::traits::fungible::Mutate<_>>::mint_into(&dap_buffer, ed)
		.expect("mint dap buffer");
}

impl Config for Test {
	type OldCurrency = Balances;
	type Currency = Balances;
	type RewardRemainder = RewardRemainderMock;
	type Reward = MockReward;
	type SessionsPerEra = SessionsPerEra;
	type SlashDeferDuration = SlashDeferDuration;
	type AdminOrigin = EitherOfDiverse<EnsureRoot<AccountId>, EnsureSignedBy<One, AccountId>>;
	type EraPayout = TestEraPayout;
	type MaxExposurePageSize = MaxExposurePageSize;
	type MaxValidatorSet = MaxValidatorSet;
	type ElectionProvider = TestElectionProvider;
	type VoterList = VoterBagsList;
	type TargetList = UseValidatorsMap<Self>;
	type NominationsQuota = WeightedNominationsQuota<16>;
	type MaxUnlockingChunks = MaxUnlockingChunks;
	type HistoryDepth = HistoryDepth;
	type BondingDuration = BondingDuration;
	type NominatorFastUnbondDuration = NominatorFastUnbondDuration;
	type MaxControllersInDeprecationBatch = MaxControllersInDeprecationBatch;
	type EventListeners = EventListenerMock;
	type MaxEraDuration = MaxEraDuration;
	type DisableMinting = DisableMintingMode;
	type UnclaimedRewardHandler = Dap;
	type RewardPots = SequentialTest;
	type StakerRewardCalculator = reward::DefaultStakerRewardCalculator<Test>;
	type MaxPruningItems = MaxPruningItems;
	type PlanningEraOffset = PlanningEraOffset;
	type Filter = MockedRestrictList;
	type RcClientInterface = session_mock::Session;
	type CurrencyBalance = Balance;
	type CurrencyToVote = SaturatingCurrencyToVote;
	type Slash = Dap;
	type RuntimeHoldReason = RuntimeHoldReason;
	type WeightInfo = ();
	type IsValidatorInactive = ();
}

pub struct WeightedNominationsQuota<const MAX: u32>;
impl<Balance, const MAX: u32> NominationsQuota<Balance> for WeightedNominationsQuota<MAX>
where
	u128: From<Balance>,
{
	type MaxNominations = AbsoluteMaxNominations;

	fn curve(balance: Balance) -> u32 {
		match balance.into() {
			// random curve for testing.
			0..=110 => MAX,
			111 => 0,
			222 => 2,
			333 => MAX + 10,
			_ => MAX,
		}
	}
}

parameter_types! {
	// if true, skips the try-state for the test running.
	pub static SkipTryStateCheck: bool = false;
}

pub struct ExtBuilder {
	nominate: bool,
	validator_count: u32,
	has_stakers: bool,
	pub min_nominator_bond: Balance,
	min_validator_bond: Balance,
	balance_factor: Balance,
	status: BTreeMap<AccountId, StakerStatus<AccountId>>,
	stakes: BTreeMap<AccountId, Balance>,
	stakers: Vec<(AccountId, Balance, StakerStatus<AccountId>)>,
	flush_events: bool,
	nominators_slashable: bool,
}

impl Default for ExtBuilder {
	fn default() -> Self {
		// Default both min bonds strictly above ED — and to different values — to
		// 1. mimic the situation we live in (ED < MinNominatorBond < MinValidatorBond on Polkadot /
		//    Kusama / Westend AH).
		// 2. avoid tests from accidentally passing because `min_chilled_bond` happens to
		// collapse to ED when all three are equal.
		Self {
			nominate: true,
			validator_count: 2,
			balance_factor: 1,
			has_stakers: true,
			min_nominator_bond: ExistentialDeposit::get() + 1,
			min_validator_bond: ExistentialDeposit::get() + 2,
			status: Default::default(),
			stakes: Default::default(),
			stakers: Default::default(),
			flush_events: true,
			nominators_slashable: true,
		}
	}
}

#[allow(unused)]
impl ExtBuilder {
	pub(crate) fn existential_deposit(self, existential_deposit: Balance) -> Self {
		EXISTENTIAL_DEPOSIT.with(|v| *v.borrow_mut() = existential_deposit);
		self
	}
	pub(crate) fn max_unlock_chunks(self, max: u32) -> Self {
		MaxUnlockingChunks::set(max);
		self
	}
	pub(crate) fn bonding_duration(self, bonding_duration: EraIndex) -> Self {
		BondingDuration::set(bonding_duration);
		self
	}
	pub(crate) fn planning_era_offset(self, offset: SessionIndex) -> Self {
		PlanningEraModeVal::set(PlanningEraMode::Fixed(offset));
		self
	}
	pub fn smart_era_planner(self) -> Self {
		PlanningEraModeVal::set(PlanningEraMode::Smart);
		self
	}
	pub fn election_delay(self, delay: BlockNumber) -> Self {
		ElectionDelay::set(delay);
		self
	}
	/// Switch to legacy reward mode (EraPayout-based minting instead of DAP pots).
	pub(crate) fn legacy_reward_mode(self) -> Self {
		UseLegacyEraPayout::set(true);
		self
	}
	pub(crate) fn nominate(mut self, nominate: bool) -> Self {
		self.nominate = nominate;
		self
	}
	pub(crate) fn no_flush_events(mut self) -> Self {
		self.flush_events = false;
		self
	}
	pub(crate) fn validator_count(mut self, count: u32) -> Self {
		self.validator_count = count;
		self
	}
	pub(crate) fn slash_defer_duration(self, eras: EraIndex) -> Self {
		SlashDeferDuration::set(eras);
		self
	}
	pub(crate) fn set_nominators_slashable(mut self, slashable: bool) -> Self {
		self.nominators_slashable = slashable;
		self
	}
	pub(crate) fn session_per_era(self, length: SessionIndex) -> Self {
		SessionsPerEra::set(length);
		self
	}
	pub(crate) fn period(self, length: BlockNumber) -> Self {
		Period::set(length);
		self
	}
	pub(crate) fn has_stakers(mut self, has: bool) -> Self {
		self.has_stakers = has;
		self
	}
	pub(crate) fn offset(self, offset: BlockNumber) -> Self {
		OFFSET.with(|v| *v.borrow_mut() = offset);
		self
	}
	pub(crate) fn min_nominator_bond(mut self, amount: Balance) -> Self {
		self.min_nominator_bond = amount;
		self
	}
	pub(crate) fn min_validator_bond(mut self, amount: Balance) -> Self {
		self.min_validator_bond = amount;
		self
	}
	pub(crate) fn set_status(mut self, who: AccountId, status: StakerStatus<AccountId>) -> Self {
		self.status.insert(who, status);
		self
	}
	pub(crate) fn set_stake(mut self, who: AccountId, stake: Balance) -> Self {
		self.stakes.insert(who, stake);
		self
	}
	pub(crate) fn add_staker(
		mut self,
		stash: AccountId,
		stake: Balance,
		status: StakerStatus<AccountId>,
	) -> Self {
		self.stakers.push((stash, stake, status));
		self
	}
	pub(crate) fn exposures_page_size(self, max: u32) -> Self {
		MaxExposurePageSize::set(max);
		self
	}
	pub(crate) fn balance_factor(mut self, factor: Balance) -> Self {
		self.balance_factor = factor;
		self
	}
	pub(crate) fn multi_page_election_provider(self, pages: PageIndex) -> Self {
		Pages::set(pages);
		self
	}
	pub(crate) fn election_bounds(self, voter_count: u32, target_count: u32) -> Self {
		let bounds = ElectionBoundsBuilder::default()
			.voters_count(voter_count.into())
			.targets_count(target_count.into())
			.build();
		ElectionsBounds::set(bounds);
		self
	}
	pub(crate) fn max_winners_per_page(self, max: u32) -> Self {
		MaxWinnersPerPage::set(max);
		self
	}
	/// Do NOT use this. Disabling try-state means the test stops proving that storage
	/// invariants hold at the end, which hides real corruption. A test that deliberately
	/// corrupts storage to exercise a code path must restore valid state before its closure
	/// returns so this hook still runs and passes. Existing `false` call sites are bugs to
	/// be fixed.
	pub(crate) fn try_state(self, enable: bool) -> Self {
		SkipTryStateCheck::set(!enable);
		self
	}

	fn build(self) -> sp_io::TestExternalities {
		sp_tracing::try_init_simple();
		let mut storage = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
		let ed = ExistentialDeposit::get();

		let mut maybe_stakers = vec![];
		if self.has_stakers {
			maybe_stakers = vec![
				// (stash, stake, status)
				// these two will be elected in the default test where we elect 2.
				(11, self.balance_factor * 1000, StakerStatus::<AccountId>::Validator),
				(21, self.balance_factor * 1000, StakerStatus::<AccountId>::Validator),
				// a loser validator
				(31, self.balance_factor * 500, StakerStatus::<AccountId>::Validator),
				// idle stakers
				(41, self.balance_factor * 4000, StakerStatus::<AccountId>::Idle),
				(51, self.balance_factor * 5000, StakerStatus::<AccountId>::Idle),
			]; // optionally add a nominator
			if self.nominate {
				maybe_stakers.push((
					101,
					self.balance_factor * 500,
					StakerStatus::<AccountId>::Nominator(vec![11, 21]),
				))
			}
			// replace any of the status if needed.
			self.status.into_iter().for_each(|(stash, status)| {
				let (_, _, ref mut prev_status) = maybe_stakers
					.iter_mut()
					.find(|s| s.0 == stash)
					.expect("set_status staker should exist; qed");
				*prev_status = status;
			});
			// replaced any of the stakes if needed.
			self.stakes.into_iter().for_each(|(stash, stake)| {
				let (_, ref mut prev_stake, _) = maybe_stakers
					.iter_mut()
					.find(|s| s.0 == stash)
					.expect("set_stake staker should exits; qed.");
				*prev_stake = stake;
			});
			// extend stakers if needed.
			maybe_stakers.extend(self.stakers)
		}

		let aux_balances = vec![
			// aux accounts
			(1, ed + 10 * self.balance_factor),
			(2, ed + 20 * self.balance_factor),
			(3, ed + 300 * self.balance_factor),
			(4, ed + 400 * self.balance_factor),
			// This allows us to have a total_payout different from 0.
			(999, 1_000_000_000_000),
		];
		// given each stakers their stake + ed as balance.
		let stakers_balances =
			maybe_stakers.clone().into_iter().map(|(who, stake, _)| (who, stake + ed));
		let balances = aux_balances.into_iter().chain(stakers_balances).collect::<Vec<_>>();

		let _ = pallet_balances::GenesisConfig::<Test> { balances, ..Default::default() }
			.assimilate_storage(&mut storage);

		let _ = pallet_staking_async::GenesisConfig::<Test> {
			stakers: maybe_stakers,
			validator_count: self.validator_count,
			active_era: (0, 0, INIT_TIMESTAMP),
			slash_reward_fraction: Perbill::from_percent(10),
			min_nominator_bond: self.min_nominator_bond,
			min_validator_bond: self.min_validator_bond,
			..Default::default()
		}
		.assimilate_storage(&mut storage);

		let mut ext = sp_io::TestExternalities::from(storage);
		let nominators_slashable = self.nominators_slashable;

		ext.execute_with(|| {
			crate::AreNominatorsSlashable::<Test>::put(nominators_slashable);

			if !UseLegacyEraPayout::get() {
				crate::DisableMintingGuard::<Test>::put(0);
				setup_dap();
			}
			// Legacy mode: no DAP setup needed. EraPayout computes inflation, staking mints.
			session_mock::Session::roll_until_active_era(1);
			if self.flush_events {
				let _ = staking_events_since_last_call();
			}
		});

		ext
	}
	pub fn build_and_execute(self, test: impl FnOnce() -> ()) {
		sp_tracing::try_init_simple();
		let mut ext = self.build();
		ext.execute_with(test);
		ext.execute_with(|| {
			if !SkipTryStateCheck::get() {
				Staking::do_try_state(System::block_number()).unwrap();
			}
		});
	}
}

pub(crate) fn active_era() -> EraIndex {
	pallet_staking_async::ActiveEra::<Test>::get().unwrap().index
}

pub(crate) fn current_era() -> EraIndex {
	pallet_staking_async::CurrentEra::<Test>::get().unwrap()
}

pub(crate) fn bond(who: AccountId, val: Balance) {
	let _ = asset::set_stakeable_balance::<Test>(&who, val);
	assert_ok!(Staking::bond(RuntimeOrigin::signed(who), val, RewardDestination::Stash));
}

pub(crate) fn bond_validator(who: AccountId, val: Balance) {
	bond(who, val);
	assert_ok!(Staking::validate(RuntimeOrigin::signed(who), ValidatorPrefs::default()));
}

pub(crate) fn bond_nominator(who: AccountId, val: Balance, target: Vec<AccountId>) {
	bond(who, val);
	assert_ok!(Staking::nominate(RuntimeOrigin::signed(who), target));
}

pub(crate) fn bond_virtual_nominator(
	who: AccountId,
	payee: AccountId,
	val: Balance,
	target: Vec<AccountId>,
) {
	// Bond who virtually.
	assert_ok!(<Staking as sp_staking::StakingUnchecked>::virtual_bond(&who, val, &payee));
	assert_ok!(Staking::nominate(RuntimeOrigin::signed(who), target));
}

pub(crate) fn validator_payout_for(duration: u64) -> Balance {
	let total_inflation =
		OneTokenPerMillisecond::issue(pallet_balances::TotalIssuance::<Test>::get(), duration);
	// Apply budget allocation to get staker portion
	let budget = pallet_dap::BudgetAllocation::<Test>::get();
	let staker_pct = budget.get(&staker_reward_key()).copied().unwrap_or(Perbill::zero());
	let payout = staker_pct.mul_floor(total_inflation);
	assert!(payout > 0);
	payout
}

pub(crate) fn total_payout_for(duration: u64) -> Balance {
	OneTokenPerMillisecond::issue(pallet_balances::TotalIssuance::<Test>::get(), duration)
}

/// Time it takes to finish a session.
///
/// Note, if you see `time_per_session() - BLOCK_TIME`, it is fine. This is because we set the
/// timestamp after on_initialize, so the timestamp is always one block old.
pub(crate) fn time_per_session() -> u64 {
	Period::get() * BLOCK_TIME
}

/// Time it takes to finish an era.
pub(crate) fn time_per_era() -> u64 {
	time_per_session() * SessionsPerEra::get() as u64
}

pub(crate) fn reward_all_elected() {
	let rewards = session_mock::Session::validators().into_iter().map(|v| (v, 1));
	<Pallet<Test>>::reward_by_ids(rewards)
}

pub(crate) fn era_exposures(era: u32) -> Vec<(AccountId, Exposure<AccountId, Balance>)> {
	ErasStakersOverview::<T>::iter_prefix(era)
		.map(|(v, _overview)| (v, Staking::eras_stakers(era, &v)))
		.collect::<Vec<_>>()
}

pub(crate) fn session_validators() -> Vec<AccountId> {
	Session::validators()
}

/// Add a slash for who
pub(crate) fn add_slash(who: AccountId) {
	let _ = <Staking as rc_client::AHStakingInterface>::on_new_offences(
		session_mock::Session::current_index(),
		vec![rc_client::Offence {
			offender: who,
			reporters: vec![],
			slash_fraction: Perbill::from_percent(10),
		}],
	);
}

pub(crate) fn add_slash_in_era(who: AccountId, era: EraIndex, p: Perbill) {
	let _ = <Staking as rc_client::AHStakingInterface>::on_new_offences(
		Rotator::<T>::era_start_session_index(era).unwrap(),
		vec![rc_client::Offence { offender: who, reporters: vec![], slash_fraction: p }],
	);
}

pub(crate) fn add_slash_with_percent(who: AccountId, percent: u32) {
	let _ = <Staking as rc_client::AHStakingInterface>::on_new_offences(
		session_mock::Session::current_index(),
		vec![rc_client::Offence {
			offender: who,
			reporters: vec![],
			slash_fraction: Perbill::from_percent(percent),
		}],
	);
}

/// Make all validator and nominator request their payment
pub(crate) fn make_all_reward_payment(era: EraIndex) {
	let validators_with_reward = ErasRewardPoints::<Test>::get(era)
		.individual
		.keys()
		.cloned()
		.collect::<Vec<_>>();

	// reward validators
	for validator_controller in validators_with_reward.iter().filter_map(Staking::bonded) {
		let ledger = <Ledger<Test>>::get(&validator_controller).unwrap();
		for page in 0..Eras::<Test>::exposure_page_count(era, &ledger.stash) {
			assert_ok!(Staking::payout_stakers_by_page(
				RuntimeOrigin::signed(1337),
				ledger.stash,
				era,
				page
			));
		}
	}
}

pub(crate) fn bond_controller_stash(controller: AccountId, stash: AccountId) -> Result<(), String> {
	<Bonded<Test>>::get(&stash).map_or(Ok(()), |_| Err("stash already bonded"))?;
	<Ledger<Test>>::get(&controller).map_or(Ok(()), |_| Err("controller already bonded"))?;

	<Bonded<Test>>::insert(stash, controller);
	<Ledger<Test>>::insert(controller, StakingLedger::<Test>::default_from(stash));
	<Payee<Test>>::insert(stash, RewardDestination::Staked);

	Ok(())
}

// simulates `set_controller` without corrupted ledger checks for testing purposes.
pub(crate) fn set_controller_no_checks(stash: &AccountId) {
	let controller = Bonded::<Test>::get(stash).expect("testing stash should be bonded");
	let ledger = Ledger::<Test>::get(&controller).expect("testing ledger should exist");

	Ledger::<Test>::remove(&controller);
	Ledger::<Test>::insert(stash, ledger);
	Bonded::<Test>::insert(stash, stash);
}

// simulates `bond_extra` without corrupted ledger checks for testing purposes.
pub(crate) fn bond_extra_no_checks(stash: &AccountId, amount: Balance) {
	let controller = Bonded::<Test>::get(stash).expect("bond must exist to bond_extra");
	let mut ledger = Ledger::<Test>::get(&controller).expect("ledger must exist to bond_extra");

	let new_total = ledger.total + amount;
	let _ = asset::update_stake::<Test>(stash, new_total);
	ledger.total = new_total;
	ledger.active = new_total;
	Ledger::<Test>::insert(controller, ledger);
}

pub(crate) fn setup_double_bonded_ledgers() {
	let init_ledgers = Ledger::<Test>::iter().count();

	let _ = asset::set_stakeable_balance::<Test>(&333, 2000);
	let _ = asset::set_stakeable_balance::<Test>(&444, 2000);
	let _ = asset::set_stakeable_balance::<Test>(&555, 2000);
	let _ = asset::set_stakeable_balance::<Test>(&777, 2000);

	assert_ok!(Staking::bond(RuntimeOrigin::signed(333), 10, RewardDestination::Staked));
	assert_ok!(Staking::bond(RuntimeOrigin::signed(444), 20, RewardDestination::Staked));
	assert_ok!(Staking::bond(RuntimeOrigin::signed(555), 20, RewardDestination::Staked));
	// not relevant to the test case, but ensures try-runtime checks pass.
	[333, 444, 555]
		.iter()
		.for_each(|s| Payee::<Test>::insert(s, RewardDestination::Staked));

	// we want to test the case where a controller can also be a stash of another ledger.
	// for that, we change the controller/stash bonding so that:
	// * 444 becomes controller of 333.
	// * 555 becomes controller of 444.
	// * 777 becomes controller of 555.
	let ledger_333 = Ledger::<Test>::get(333).unwrap();
	let ledger_444 = Ledger::<Test>::get(444).unwrap();
	let ledger_555 = Ledger::<Test>::get(555).unwrap();

	// 777 becomes controller of 555.
	Bonded::<Test>::mutate(555, |controller| *controller = Some(777));
	Ledger::<Test>::insert(777, ledger_555);

	// 555 becomes controller of 444.
	Bonded::<Test>::mutate(444, |controller| *controller = Some(555));
	Ledger::<Test>::insert(555, ledger_444);

	// 444 becomes controller of 333.
	Bonded::<Test>::mutate(333, |controller| *controller = Some(444));
	Ledger::<Test>::insert(444, ledger_333);

	// 333 is not controller anymore.
	Ledger::<Test>::remove(333);

	// checks. now we have:
	// * +3 ledgers
	assert_eq!(Ledger::<Test>::iter().count(), 3 + init_ledgers);

	// * stash 333 has controller 444.
	assert_eq!(Bonded::<Test>::get(333), Some(444));
	assert_eq!(StakingLedger::<Test>::paired_account(StakingAccount::Stash(333)), Some(444));
	assert_eq!(Ledger::<Test>::get(444).unwrap().stash, 333);

	// * stash 444 has controller 555.
	assert_eq!(Bonded::<Test>::get(444), Some(555));
	assert_eq!(StakingLedger::<Test>::paired_account(StakingAccount::Stash(444)), Some(555));
	assert_eq!(Ledger::<Test>::get(555).unwrap().stash, 444);

	// * stash 555 has controller 777.
	assert_eq!(Bonded::<Test>::get(555), Some(777));
	assert_eq!(StakingLedger::<Test>::paired_account(StakingAccount::Stash(555)), Some(777));
	assert_eq!(Ledger::<Test>::get(777).unwrap().stash, 555);
}

#[macro_export]
macro_rules! assert_session_era {
	($session:expr, $era:expr) => {
		assert_eq!(
			session_mock::Session::current_index(),
			$session,
			"wrong session {} != {}",
			session_mock::Session::current_index(),
			$session,
		);
		assert_eq!(
			CurrentEra::<T>::get().unwrap(),
			$era,
			"wrong current era {} != {}",
			CurrentEra::<T>::get().unwrap(),
			$era,
		);
	};
}

pub(crate) fn staking_events() -> Vec<crate::Event<Test>> {
	System::events()
		.into_iter()
		.map(|r| r.event)
		.filter_map(|e| if let RuntimeEvent::Staking(inner) = e { Some(inner) } else { None })
		.collect()
}

parameter_types! {
	static StakingEventsIndex: usize = 0;
}

pub(crate) fn staking_events_since_last_call() -> Vec<crate::Event<Test>> {
	let all: Vec<_> = System::events()
		.into_iter()
		.filter_map(|r| if let RuntimeEvent::Staking(inner) = r.event { Some(inner) } else { None })
		.collect();
	let seen = StakingEventsIndex::get();
	StakingEventsIndex::set(all.len());
	all.into_iter().skip(seen).collect()
}

pub(crate) fn to_bounded_supports(
	supports: Vec<(AccountId, Support<AccountId>)>,
) -> BoundedSupports<
	AccountId,
	<<Test as Config>::ElectionProvider as ElectionProvider>::MaxWinnersPerPage,
	<<Test as Config>::ElectionProvider as ElectionProvider>::MaxBackersPerWinner,
> {
	supports.try_into().unwrap()
}

pub(crate) fn restrict(who: &AccountId) {
	if !RestrictedAccounts::get().contains(who) {
		RestrictedAccounts::mutate(|l| l.push(*who));
	}
}

pub(crate) fn remove_from_restrict_list(who: &AccountId) {
	RestrictedAccounts::mutate(|l| l.retain(|x| x != who));
}

pub(crate) fn era_unprocessed_offence_count(era: EraIndex) -> u32 {
	OffenceQueue::<T>::iter_prefix_values(era).count() as u32
}

pub(crate) fn era_unapplied_slash_count(era: EraIndex) -> u32 {
	UnappliedSlashes::<T>::iter_prefix_values(era).count() as u32
}

/// A pending slash from the previous era blocks withdrawal. Use this to apply them.
pub(crate) fn apply_pending_slashes_from_previous_era() {
	apply_pending_slashes_from_era(active_era() - 1);
}

pub(crate) fn apply_pending_slashes_from_era(era: EraIndex) {
	for (key, _) in UnappliedSlashes::<T>::iter_prefix(era) {
		assert_ok!(Staking::apply_slash(RuntimeOrigin::signed(1), era, key));
	}
}

pub(crate) fn setup_incentive_config() {
	assert_ok!(Staking::set_validator_self_stake_incentive_config(
		RuntimeOrigin::root(),
		ConfigOp::Set(30_000),
		ConfigOp::Set(100_000),
		ConfigOp::Set(Perbill::from_rational(1u32, 2u32)),
	));
}

/// Sets up incentive config and DAP budget. Remainder goes to buffer.
/// E.g. `(45, 5)` → 45% staker, 5% incentive, 50% buffer.
pub(crate) fn setup_incentive_with_budget(staker_pct: u32, incentive_pct: u32) {
	setup_incentive_config();
	let buffer_pct = 100u32.saturating_sub(staker_pct).saturating_sub(incentive_pct);
	let mut entries = vec![(staker_reward_key(), staker_pct)];
	if incentive_pct > 0 {
		entries.push((validator_incentive_key(), incentive_pct));
	}
	if buffer_pct > 0 {
		entries.push((buffer_key(), buffer_pct));
	}
	pallet_dap::BudgetAllocation::<Test>::put(build_budget(&entries));
}

pub(crate) fn staker_reward_for(stash: AccountId, events: &[Event<Test>]) -> Option<Balance> {
	events.iter().find_map(|e| match e {
		Event::Rewarded { stash: s, amount, .. } if *s == stash => Some(*amount),
		_ => None,
	})
}

pub(crate) fn incentive_paid_for(stash: AccountId, events: &[Event<Test>]) -> Option<Balance> {
	incentive_paid_details(stash, events).map(|(amount, _)| amount)
}

pub(crate) fn incentive_paid_details(
	stash: AccountId,
	events: &[Event<Test>],
) -> Option<(Balance, RewardDestination<AccountId>)> {
	events.iter().find_map(|e| match e {
		Event::ValidatorIncentivePaid { validator_stash, amount, dest, .. }
			if *validator_stash == stash =>
		{
			Some((*amount, *dest))
		},
		_ => None,
	})
}
