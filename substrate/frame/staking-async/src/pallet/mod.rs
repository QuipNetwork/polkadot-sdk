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

//! `pallet-staking-async`'s main `pallet` module.

use crate::{
	asset, session_rotation::EraElectionPlanner, slashing, weights::WeightInfo, AccountIdLookupOf,
	ActiveEraInfo, BalanceOf, EraPayout, EraRewardPoints, ExposurePage, Forcing,
	LedgerIntegrityState, MaxNominationsOf, NegativeImbalanceOf, Nominations, NominationsQuota,
	PositiveImbalanceOf, RewardDestination, StakingLedger, UnappliedSlash, UnlockChunk,
	ValidatorPrefs,
};
use alloc::{format, vec::Vec};
use codec::Codec;
use frame_election_provider_support::{ElectionProvider, SortedListProvider, VoteWeight};
use frame_support::{
	assert_ok,
	pallet_prelude::*,
	traits::{
		fungible::{
			hold::{Balanced as FunHoldBalanced, Mutate as FunHoldMutate},
			Mutate, Mutate as FunMutate,
		},
		Contains, Defensive, DefensiveSaturating, EnsureOrigin, Get, InspectLockableCurrency,
		Nothing, OnUnbalanced,
	},
	weights::Weight,
	BoundedBTreeSet, BoundedVec,
};
use frame_system::{ensure_root, ensure_signed, pallet_prelude::*};
pub use impls::*;
use rand::seq::SliceRandom;
use rand_chacha::{
	rand_core::{RngCore, SeedableRng},
	ChaChaRng,
};
use sp_core::{sr25519::Pair as SrPair, Pair};
use sp_runtime::{
	traits::{StaticLookup, Zero},
	ArithmeticError, Perbill, Percent,
};
use sp_staking::{
	EraIndex, Page, SessionIndex,
	StakingAccount::{self, Controller, Stash},
	StakingInterface,
};

mod impls;

#[frame_support::pallet]
pub mod pallet {
	use core::ops::Deref;

	use super::*;
	use crate::{
		session_rotation::{self, Eras, Rotator},
		IsValidatorInactive, PagedExposureMetadata, SnapshotStatus,
	};
	use codec::HasCompact;
	use frame_election_provider_support::{ElectionDataProvider, PageIndex};
	use frame_support::{traits::ConstBool, weights::WeightMeter, DefaultNoBound, PalletError};

	/// Dimensionless weight from the validator self-stake incentive curve. Same underlying type as
	/// `BalanceOf<T>` for arithmetic compatibility, but represents the output of the sqrt weight
	/// function.
	pub(crate) type IncentiveWeight<T> = BalanceOf<T>;

	/// Represents the current step in the era pruning process
	#[derive(Encode, Decode, Clone, Copy, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
	pub enum PruningStep {
		/// Pruning ErasStakersPaged storage
		ErasStakersPaged,
		/// Pruning ErasStakersOverview storage
		ErasStakersOverview,
		/// Pruning ErasValidatorPrefs storage
		ErasValidatorPrefs,
		/// Pruning ClaimedRewards storage
		ClaimedRewards,
		/// Pruning ErasValidatorReward storage
		ErasValidatorReward,
		/// Pruning ErasRewardPoints storage
		ErasRewardPoints,
		/// Pruning single-entry storages
		SingleEntryCleanups,
		/// Pruning ValidatorSlashInEra storage
		ValidatorSlashInEra,
		/// Pruning ErasValidatorIncentiveWeight storage
		ErasValidatorIncentiveWeight,
	}

	/// The in-code storage version.
	const STORAGE_VERSION: StorageVersion = StorageVersion::new(18);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	/// Possible operations on the configuration values of this pallet.
	#[derive(TypeInfo, Debug, Clone, Encode, Decode, DecodeWithMemTracking, PartialEq)]
	pub enum ConfigOp<T: Default + Codec> {
		/// Don't change.
		Noop,
		/// Set the given value.
		Set(T),
		/// Remove from storage.
		Remove,
	}

	#[pallet::config(with_default)]
	pub trait Config: frame_system::Config {
		/// The old trait for staking balance. Deprecated and only used for migrating old ledgers.
		#[pallet::no_default]
		type OldCurrency: InspectLockableCurrency<
			Self::AccountId,
			Moment = BlockNumberFor<Self>,
			Balance = Self::CurrencyBalance,
		>;

		/// The staking balance.
		#[pallet::no_default]
		type Currency: FunHoldMutate<
				Self::AccountId,
				Reason = Self::RuntimeHoldReason,
				Balance = Self::CurrencyBalance,
			> + FunMutate<Self::AccountId, Balance = Self::CurrencyBalance>
			+ FunHoldBalanced<Self::AccountId, Balance = Self::CurrencyBalance>;

		/// Overarching hold reason.
		#[pallet::no_default_bounds]
		type RuntimeHoldReason: From<HoldReason>;

		/// Just the `Currency::Balance` type; we have this item to allow us to constrain it to
		/// `From<u64>`.
		type CurrencyBalance: sp_runtime::traits::AtLeast32BitUnsigned
			+ codec::FullCodec
			+ DecodeWithMemTracking
			+ HasCompact<Type: DecodeWithMemTracking>
			+ Copy
			+ MaybeSerializeDeserialize
			+ core::fmt::Debug
			+ Default
			+ From<u64>
			+ TypeInfo
			+ Send
			+ Sync
			+ MaxEncodedLen;

		/// Convert a balance into a number used for election calculation. This must fit into a
		/// `u64` but is allowed to be sensibly lossy. The `u64` is used to communicate with the
		/// [`frame_election_provider_support`] crate which accepts u64 numbers and does operations
		/// in 128.
		/// Consequently, the backward convert is used convert the u128s from sp-elections back to a
		/// [`BalanceOf`].
		#[pallet::no_default_bounds]
		type CurrencyToVote: sp_staking::currency_to_vote::CurrencyToVote<BalanceOf<Self>>;

		/// Something that provides the election functionality.
		#[pallet::no_default]
		type ElectionProvider: ElectionProvider<
			AccountId = Self::AccountId,
			BlockNumber = BlockNumberFor<Self>,
			// we only accept an election provider that has staking as data provider.
			DataProvider = Pallet<Self>,
		>;

		/// Something that defines the maximum number of nominations per nominator.
		#[pallet::no_default_bounds]
		type NominationsQuota: NominationsQuota<BalanceOf<Self>>;

		/// Number of eras to keep in history.
		///
		/// Following information is kept for eras in `[current_era -
		/// HistoryDepth, current_era]`: `ErasValidatorPrefs`, `ErasValidatorReward`,
		/// `ErasRewardPoints`, `ErasTotalStake`, `ClaimedRewards`,
		/// `ErasStakersPaged`, `ErasStakersOverview`.
		///
		/// Must be more than the number of eras delayed by session.
		/// I.e. active era must always be in history. I.e. `active_era >
		/// current_era - history_depth` must be guaranteed.
		///
		/// If migrating an existing pallet from storage value to config value,
		/// this should be set to same value or greater as in storage.
		#[pallet::constant]
		type HistoryDepth: Get<u32>;

		/// Tokens have been minted and are unused for validator-reward.
		///
		/// Only used in legacy minting mode (`DisableMinting = false`).
		#[pallet::no_default_bounds]
		type RewardRemainder: OnUnbalanced<NegativeImbalanceOf<Self>>;

		/// Handler for the unbalanced reduction when slashing a staker.
		#[pallet::no_default_bounds]
		type Slash: OnUnbalanced<NegativeImbalanceOf<Self>>;

		/// Handler for the unbalanced increment when rewarding a staker.
		/// NOTE: in most cases, the implementation of `OnUnbalanced` should modify the total
		/// issuance.
		///
		/// Only used in legacy minting mode (`DisableMinting = false`).
		#[pallet::no_default_bounds]
		type Reward: OnUnbalanced<PositiveImbalanceOf<Self>>;

		/// Number of sessions per era, as per the preferences of the **relay chain**.
		#[pallet::constant]
		type SessionsPerEra: Get<SessionIndex>;

		/// Number of sessions before the end of an era when the election for the next era will
		/// start.
		///
		/// - This determines how many sessions **before** the last session of the era the staking
		///   election process should begin.
		/// - The value is bounded between **1** (election starts at the beginning of the last
		///   session) and `SessionsPerEra` (election starts at the beginning of the first session
		///   of the era).
		///
		/// ### Example:
		/// - If `SessionsPerEra = 6` and `PlanningEraOffset = 1`, the election starts at the
		///   beginning of session `6 - 1 = 5`.
		/// - If `PlanningEraOffset = 6`, the election starts at the beginning of session `6 - 6 =
		///   0`, meaning it starts at the very beginning of the era.
		#[pallet::constant]
		type PlanningEraOffset: Get<SessionIndex>;

		/// Number of eras that staked funds must remain bonded for.
		///
		/// This is the bonding duration for validators. Nominators may have a shorter bonding
		/// duration when [`AreNominatorsSlashable`] is set to `false` (see
		/// [`StakingInterface::nominator_bonding_duration`]).
		#[pallet::constant]
		type BondingDuration: Get<EraIndex>;

		/// Number of eras nominators must wait to unbond when they are not slashable.
		///
		/// This duration is used for nominators when [`AreNominatorsSlashable`] is `false`.
		/// When nominators are slashable, they use the full [`Config::BondingDuration`] to ensure
		/// slashes can be applied during the unbonding period.
		///
		/// Setting this to a lower value (e.g., 1 era) allows for faster withdrawals when
		/// nominators are not subject to slashing risk.
		#[pallet::constant]
		type NominatorFastUnbondDuration: Get<EraIndex>;

		/// Number of eras that slashes are deferred by, after computation.
		///
		/// This should be less than the bonding duration. Set to 0 if slashes
		/// should be applied immediately, without opportunity for intervention.
		#[pallet::constant]
		type SlashDeferDuration: Get<EraIndex>;

		/// The origin which can manage less critical staking parameters that does not require root.
		///
		/// Supported actions: (1) cancel deferred slash, (2) set minimum commission.
		#[pallet::no_default]
		type AdminOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// The payout for validators and the system for the current era.
		/// See [Era payout](./index.html#era-payout).
		///
		/// Only used in legacy minting mode (`DisableMinting = false`).
		/// Should be set to () in non-minting mode.
		#[pallet::no_default]
		type EraPayout: EraPayout<BalanceOf<Self>>;

		/// When `true`, staking does not mint. It expects an external source to fund
		/// the general reward pot. At era boundary, rewards are snapshotted from
		/// the pot. `EraPayout` is not called.
		///
		/// When `false`, staking uses the legacy path: `EraPayout` computes inflation,
		/// tokens are minted on-the-fly during payout.
		///
		/// **Irreversible**: once set to `true`, must never be switched back. Eras
		/// created in non-minting mode have funded reward pots — switching to legacy
		/// would orphan those pots and cause double-minting.
		#[pallet::constant]
		type DisableMinting: Get<bool>;

		/// Handler for unclaimed era rewards (non-minting mode only).
		///
		/// When era pots are cleaned up past `HistoryDepth`, remaining funds are
		/// withdrawn and passed to this handler.
		#[pallet::no_default_bounds]
		type UnclaimedRewardHandler: OnUnbalanced<NegativeImbalanceOf<Self>>;

		/// Provider for generating reward pot account IDs (non-minting mode only).
		///
		/// Provides both general pots (funded by an external source like pallet-dap)
		/// and era-specific pots (snapshotted at era boundaries).
		#[pallet::no_default]
		type RewardPots: crate::PotAccountProvider<Self::AccountId>;

		/// Calculator for staker rewards.
		///
		/// Determines how staking rewards are distributed between validators and nominators.
		#[pallet::no_default_bounds]
		type StakerRewardCalculator: sp_staking::StakerRewardCalculator<BalanceOf<Self>>;

		/// The maximum size of each `T::ExposurePage`.
		///
		/// An `ExposurePage` is weakly bounded to a maximum of `MaxExposurePageSize`
		/// nominators.
		///
		/// For older non-paged exposure, a reward payout was restricted to the top
		/// `MaxExposurePageSize` nominators. This is to limit the i/o cost for the
		/// nominator payout.
		///
		/// Note: `MaxExposurePageSize` is used to bound `ClaimedRewards` and is unsafe to
		/// reduce without handling it in a migration.
		#[pallet::constant]
		type MaxExposurePageSize: Get<u32>;

		/// The absolute maximum of winner validators this pallet should return.
		///
		/// As this pallet supports multi-block election, the set of winner validators *per
		/// election* is bounded by this type.
		#[pallet::constant]
		type MaxValidatorSet: Get<u32>;

		/// Something that provides a best-effort sorted list of voters aka electing nominators,
		/// used for NPoS election.
		///
		/// The changes to nominators are reported to this. Moreover, each validator's self-vote is
		/// also reported as one independent vote.
		///
		/// To keep the load off the chain as much as possible, changes made to the staked amount
		/// via rewards and slashes are not reported and thus need to be manually fixed by the
		/// staker. In case of `bags-list`, this always means using `rebag` and `putInFrontOf`.
		///
		/// Invariant: what comes out of this list will always be a nominator.
		#[pallet::no_default]
		type VoterList: SortedListProvider<Self::AccountId, Score = VoteWeight>;

		/// WIP: This is a noop as of now, the actual business logic that's described below is going
		/// to be introduced in a follow-up PR.
		///
		/// Something that provides a best-effort sorted list of targets aka electable validators,
		/// used for NPoS election.
		///
		/// The changes to the approval stake of each validator are reported to this. This means any
		/// change to:
		/// 1. The stake of any validator or nominator.
		/// 2. The targets of any nominator
		/// 3. The role of any staker (e.g. validator -> chilled, nominator -> validator, etc)
		///
		/// Unlike `VoterList`, the values in this list are always kept up to date with reward and
		/// slash as well, and thus represent the accurate approval stake of all account being
		/// nominated by nominators.
		///
		/// Note that while at the time of nomination, all targets are checked to be real
		/// validators, they can chill at any point, and their approval stakes will still be
		/// recorded. This implies that what comes out of iterating this list MIGHT NOT BE AN ACTIVE
		/// VALIDATOR.
		#[pallet::no_default]
		type TargetList: SortedListProvider<Self::AccountId, Score = BalanceOf<Self>>;

		/// The maximum number of `unlocking` chunks a [`StakingLedger`] can
		/// have. Effectively determines how many unique eras a staker may be
		/// unbonding in.
		///
		/// Note: `MaxUnlockingChunks` is used as the upper bound for the
		/// `BoundedVec` item `StakingLedger.unlocking`. Setting this value
		/// lower than the existing value can lead to inconsistencies in the
		/// `StakingLedger` and will need to be handled properly in a runtime
		/// migration. The test `reducing_max_unlocking_chunks_abrupt` shows
		/// this effect.
		#[pallet::constant]
		type MaxUnlockingChunks: Get<u32>;

		/// The maximum amount of controller accounts that can be deprecated in one call.
		type MaxControllersInDeprecationBatch: Get<u32>;

		/// Something that listens to staking updates and performs actions based on the data it
		/// receives.
		///
		/// WARNING: this only reports slashing and withdraw events for the time being.
		#[pallet::no_default_bounds]
		type EventListeners: sp_staking::OnStakingUpdate<Self::AccountId, BalanceOf<Self>>;

		/// Maximum allowed era duration in milliseconds.
		///
		/// This provides a defensive upper bound to cap the effective era duration, preventing
		/// excessively long eras from causing runaway inflation (e.g., due to bugs). If the actual
		/// era duration exceeds this value, it will be clamped to this maximum.
		///
		/// Example: For an ideal era duration of 24 hours (86,400,000 ms),
		/// this can be set to 604,800,000 ms (7 days).
		///
		/// Only used in legacy minting mode (`DisableMinting = false`).
		#[pallet::constant]
		type MaxEraDuration: Get<u64>;

		/// Maximum number of storage items that can be pruned in a single call.
		///
		/// This controls how many storage items can be deleted in each call to `prune_era_step`.
		/// This should be set to a conservative value (e.g., 100-500 items) to ensure pruning
		/// doesn't consume too much block space. The actual weight is determined by benchmarks.
		#[pallet::constant]
		type MaxPruningItems: Get<u32>;

		/// Interface to talk to the RC-Client pallet, possibly sending election results to the
		/// relay chain.
		#[pallet::no_default]
		type RcClientInterface: pallet_staking_async_rc_client::RcClientInterface<
			AccountId = Self::AccountId,
		>;

		#[pallet::no_default_bounds]
		/// Filter some accounts from participating in staking.
		///
		/// This is useful for example to blacklist an account that is participating in staking in
		/// another way (such as pools).
		type Filter: Contains<Self::AccountId>;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;

		/// Check if validator was inactive at some era.
		///
		/// This check is used by `chill_inactive` extrinsic to check which
		/// validators can be chilled.
		type IsValidatorInactive: IsValidatorInactive<Self::AccountId>;
	}

	/// A reason for placing a hold on funds.
	#[pallet::composite_enum]
	pub enum HoldReason {
		/// Funds on stake by a nominator or a validator.
		#[codec(index = 0)]
		Staking,
	}

	/// Default implementations of [`DefaultConfig`], which can be used to implement [`Config`].
	pub mod config_preludes {
		use super::*;
		use frame_support::{derive_impl, parameter_types, traits::ConstU32};
		pub struct TestDefaultConfig;

		#[derive_impl(frame_system::config_preludes::TestDefaultConfig, no_aggregated_types)]
		impl frame_system::DefaultConfig for TestDefaultConfig {}

		parameter_types! {
			pub const SessionsPerEra: SessionIndex = 3;
			pub const BondingDuration: EraIndex = 3;
			pub const NominatorFastUnbondDuration: EraIndex = 2;
			pub const MaxPruningItems: u32 = 100;
		}

		#[frame_support::register_default_impl(TestDefaultConfig)]
		impl DefaultConfig for TestDefaultConfig {
			#[inject_runtime_type]
			type RuntimeHoldReason = ();
			type CurrencyBalance = u128;
			type CurrencyToVote = ();
			type NominationsQuota = crate::FixedNominationsQuota<16>;
			type HistoryDepth = ConstU32<84>;
			type RewardRemainder = ();
			type Slash = ();
			type Reward = ();
			type UnclaimedRewardHandler = ();
			type StakerRewardCalculator = ();
			type DisableMinting = ConstBool<false>;
			type SessionsPerEra = SessionsPerEra;
			type BondingDuration = BondingDuration;
			type NominatorFastUnbondDuration = NominatorFastUnbondDuration;
			type PlanningEraOffset = ConstU32<1>;
			type SlashDeferDuration = ();
			type MaxExposurePageSize = ConstU32<64>;
			type MaxUnlockingChunks = ConstU32<32>;
			type MaxValidatorSet = ConstU32<100>;
			type MaxControllersInDeprecationBatch = ConstU32<100>;
			type MaxEraDuration = ();
			type MaxPruningItems = MaxPruningItems;
			type EventListeners = ();
			type Filter = Nothing;
			type WeightInfo = ();
			type IsValidatorInactive = ();
		}
	}

	/// The ideal number of active validators.
	#[pallet::storage]
	pub type ValidatorCount<T> = StorageValue<_, u32, ValueQuery>;

	/// Map from all locked "stash" accounts to the controller account.
	///
	/// TWOX-NOTE: SAFE since `AccountId` is a secure hash.
	#[pallet::storage]
	pub type Bonded<T: Config> = StorageMap<_, Twox64Concat, T::AccountId, T::AccountId>;

	/// The minimum active bond to become and maintain the role of a nominator.
	#[pallet::storage]
	pub type MinNominatorBond<T: Config> = StorageValue<_, BalanceOf<T>, ValueQuery>;

	/// The minimum active bond to become and maintain the role of a validator.
	#[pallet::storage]
	pub type MinValidatorBond<T: Config> = StorageValue<_, BalanceOf<T>, ValueQuery>;

	/// The minimum active nominator stake of the last successful election.
	#[pallet::storage]
	pub type MinimumActiveStake<T> = StorageValue<_, BalanceOf<T>, ValueQuery>;

	/// The minimum amount of commission that validators can set.
	///
	/// If set to `0`, no limit exists.
	#[pallet::storage]
	pub type MinCommission<T: Config> = StorageValue<_, Perbill, ValueQuery>;

	/// The maximum commission that validators can set.
	///
	/// If not set, defaults to `Perbill::one()` (100%), i.e. no upper limit.
	#[pallet::storage]
	pub type MaxCommission<T: Config> = StorageValue<_, Perbill, ValueQuery, MaxCommissionDefault>;

	/// Default for MaxCommission: 100% (no restriction).
	pub struct MaxCommissionDefault;
	impl Get<Perbill> for MaxCommissionDefault {
		fn get() -> Perbill {
			Perbill::one()
		}
	}

	/// Safety guard: the era from which legacy minting is permanently disabled on the
	/// payout side. **Irreversible** — once set, should never be cleared.
	///
	/// Separate from [`Config::DisableMinting`] which controls the `end_era` path.
	/// This storage guards against minting during payout for eras that were created
	/// in DAP mode. Set automatically by `end_era_dap` on first successful pot snapshot.
	/// In legacy mode (Kusama), this is never set and the guard is inactive.
	#[pallet::storage]
	pub type DisableMintingGuard<T: Config> = StorageValue<_, EraIndex>;

	/// Optimum self-stake threshold for validators.
	///
	/// Below this threshold, the incentive weight grows as `sqrt(self_stake)`.
	/// Above it, growth is dampened by [`SelfStakeSlopeFactor`].
	#[pallet::storage]
	pub type OptimumSelfStake<T: Config> = StorageValue<_, BalanceOf<T>, ValueQuery>;

	/// Hard cap on effective validator self-stake.
	///
	/// Self-stake above this value receives no additional reward benefit (plateau).
	#[pallet::storage]
	pub type HardCapSelfStake<T: Config> = StorageValue<_, BalanceOf<T>, ValueQuery>;

	/// Slope factor controlling the discouragement rate for self-stake between optimum and cap.
	///
	/// Value between 0 and 1: k=1 means no discouragement, k=0 means immediate plateau.
	#[pallet::storage]
	pub type SelfStakeSlopeFactor<T: Config> = StorageValue<_, Perbill, ValueQuery>;

	/// The total validator incentive budget for the given era, snapshotted at era end.
	///
	/// This is the similar to [`ErasValidatorReward`] but for the self-stake incentive pot.
	#[pallet::storage]
	pub type ErasValidatorIncentiveBudget<T: Config> =
		StorageMap<_, Twox64Concat, EraIndex, BalanceOf<T>, ValueQuery>;

	/// Sum of all validators' incentive weights for the era.
	///
	/// Directly linked to [`ErasValidatorIncentiveWeight`].
	#[pallet::storage]
	pub type ErasSumValidatorIncentiveWeight<T: Config> =
		StorageMap<_, Twox64Concat, EraIndex, IncentiveWeight<T>, ValueQuery>;

	/// Individual validator incentive weight per era.
	/// Each validator's share of the incentive pot = `their_weight / sum_weight`.
	#[pallet::storage]
	pub type ErasValidatorIncentiveWeight<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		EraIndex,
		Twox64Concat,
		T::AccountId,
		IncentiveWeight<T>,
		OptionQuery,
	>;

	/// Running sum of `validator_incentive_weight × era_points` across all validators
	/// with non-zero era points for the era.
	///
	/// Maintained incrementally inside [`session_rotation::Eras::reward_active_era`] every
	/// time validator points are credited. Used as the denominator of the weighted-points
	/// share that determines each validator's slice of [`ErasValidatorIncentiveBudget`].
	#[pallet::storage]
	pub type ErasSumWeightedPoints<T: Config> =
		StorageMap<_, Twox64Concat, EraIndex, IncentiveWeight<T>, ValueQuery>;

	/// Cutoff era from which the validator self-stake incentive switches to the
	/// weighted-points formula.
	///
	/// `None` is the pre-migration state for chains whose storage predates this item. Until the
	/// migration records a cutoff, [`session_rotation::Eras::uses_weighted_points`] treats all
	/// eras as weighted-points eras. Chains initialized with this storage item set the cutoff to
	/// `0` in `genesis_build`, and the upgrade migration leaves any existing value untouched.
	///
	/// See [`session_rotation::Eras::uses_weighted_points`] for the exact semantics and
	/// the rationale for the cutoff.
	///
	/// TODO(staking-async): remove this storage item, the legacy stake-only branch in
	/// [`crate::Pallet::calculate_validator_incentive_for_page`], the
	/// [`session_rotation::Eras::uses_weighted_points`] cutoff helper, and the
	/// [`crate::migrations::SetWeightedPointsFormulaStartEra`] migration once
	/// [`Config::HistoryDepth`] eras have elapsed since the upgrade — i.e. once the cutoff
	/// satisfies `cutoff <= active_era - HistoryDepth`, at which point no pre-cutoff era
	/// remains claimable and every live era uses the weighted-points formula.
	#[pallet::storage]
	pub type WeightedPointsFormulaStartEra<T: Config> = StorageValue<_, EraIndex, OptionQuery>;

	/// Whether nominators are slashable or not.
	///
	/// - When set to `true` (default), nominators are slashed along with validators and must wait
	///   the full [`Config::BondingDuration`] before withdrawing unbonded funds.
	/// - When set to `false`, nominators are not slashed, and can unbond in
	///   [`Config::NominatorFastUnbondDuration`] eras instead of the full
	///   [`Config::BondingDuration`] (see [`StakingInterface::nominator_bonding_duration`]).
	#[pallet::storage]
	pub type AreNominatorsSlashable<T: Config> = StorageValue<_, bool, ValueQuery, ConstBool<true>>;

	/// Per-era snapshot of whether nominators are slashable.
	///
	/// This is copied from [`AreNominatorsSlashable`] at the start of each era. When processing
	/// offences, we use the value from this storage for the offence era to ensure that the
	/// slashing rules at the time of the offence are applied, not the current rules.
	///
	/// If an entry does not exist for an era, nominators are assumed to be slashable (default).
	#[pallet::storage]
	pub type ErasNominatorsSlashable<T: Config> =
		StorageMap<_, Twox64Concat, EraIndex, bool, OptionQuery>;

	/// Map from all (unlocked) "controller" accounts to the info regarding the staking.
	///
	/// Note: All the reads and mutations to this storage *MUST* be done through the methods exposed
	/// by [`StakingLedger`] to ensure data and lock consistency.
	#[pallet::storage]
	pub type Ledger<T: Config> = StorageMap<_, Blake2_128Concat, T::AccountId, StakingLedger<T>>;

	/// Where the reward payment should be made. Keyed by stash.
	///
	/// TWOX-NOTE: SAFE since `AccountId` is a secure hash.
	#[pallet::storage]
	pub type Payee<T: Config> =
		StorageMap<_, Twox64Concat, T::AccountId, RewardDestination<T::AccountId>, OptionQuery>;

	/// The map from (wannabe) validator stash key to the preferences of that validator.
	///
	/// TWOX-NOTE: SAFE since `AccountId` is a secure hash.
	#[pallet::storage]
	pub type Validators<T: Config> =
		CountedStorageMap<_, Twox64Concat, T::AccountId, ValidatorPrefs, ValueQuery>;

	/// The maximum validator count before we stop allowing new validators to join.
	///
	/// When this value is not set, no limits are enforced.
	#[pallet::storage]
	pub type MaxValidatorsCount<T> = StorageValue<_, u32, OptionQuery>;

	/// Tracks the last era in which an account was active as a validator (included in the era's
	/// exposure/snapshot).
	///
	/// This is used to enforce that accounts who were recently validators must wait the full
	/// [`Config::BondingDuration`] before their funds can be withdrawn, even if they switch to
	/// nominator role. This prevents validators from:
	/// 1. Committing a slashable offence in era N
	/// 2. Switching to nominator role
	/// 3. Using the shorter nominator unbonding duration to withdraw funds before being slashed
	///
	/// Updated when era snapshots are created (in `ErasStakersPaged`/`ErasStakersOverview`).
	/// Cleaned up when the stash is killed (fully withdrawn/reaped).
	#[pallet::storage]
	pub type LastValidatorEra<T: Config> = StorageMap<_, Twox64Concat, T::AccountId, EraIndex>;

	/// The map from nominator stash key to their nomination preferences, namely the validators that
	/// they wish to support.
	///
	/// Note that the keys of this storage map might become non-decodable in case the
	/// account's [`NominationsQuota::MaxNominations`] configuration is decreased.
	/// In this rare case, these nominators
	/// are still existent in storage, their key is correct and retrievable (i.e. `contains_key`
	/// indicates that they exist), but their value cannot be decoded. Therefore, the non-decodable
	/// nominators will effectively not-exist, until they re-submit their preferences such that it
	/// is within the bounds of the newly set `Config::MaxNominations`.
	///
	/// This implies that `::iter_keys().count()` and `::iter().count()` might return different
	/// values for this map. Moreover, the main `::count()` is aligned with the former, namely the
	/// number of keys that exist.
	///
	/// Lastly, if any of the nominators become non-decodable, they can be chilled immediately via
	/// [`Call::chill_other`] dispatchable by anyone.
	///
	/// TWOX-NOTE: SAFE since `AccountId` is a secure hash.
	#[pallet::storage]
	pub type Nominators<T: Config> =
		CountedStorageMap<_, Twox64Concat, T::AccountId, Nominations<T>>;

	/// Stakers whose funds are managed by other pallets.
	///
	/// This pallet does not apply any locks on them, therefore they are only virtually bonded. They
	/// are expected to be keyless accounts and hence should not be allowed to mutate their ledger
	/// directly via this pallet. Instead, these accounts are managed by other pallets and accessed
	/// via low level apis. We keep track of them to do minimal integrity checks.
	#[pallet::storage]
	pub type VirtualStakers<T: Config> = CountedStorageMap<_, Twox64Concat, T::AccountId, ()>;

	/// The maximum nominator count before we stop allowing new validators to join.
	///
	/// When this value is not set, no limits are enforced.
	#[pallet::storage]
	pub type MaxNominatorsCount<T> = StorageValue<_, u32, OptionQuery>;

	// --- AUDIT NOTE: the following storage items should only be controlled by `Rotator`

	/// The current planned era index.
	///
	/// This is the latest planned era, depending on how the Session pallet queues the validator
	/// set, it might be active or not.
	#[pallet::storage]
	pub type CurrentEra<T> = StorageValue<_, EraIndex>;

	/// The active era information, it holds index and start.
	///
	/// The active era is the era being currently rewarded. Validator set of this era must be
	/// equal to what is RC's session pallet.
	#[pallet::storage]
	pub type ActiveEra<T> = StorageValue<_, ActiveEraInfo>;

	/// Custom bound for [`BondedEras`] which is equal to [`Config::BondingDuration`] + 1.
	pub struct BondedErasBound<T>(core::marker::PhantomData<T>);
	impl<T: Config> Get<u32> for BondedErasBound<T> {
		fn get() -> u32 {
			T::BondingDuration::get().saturating_add(1)
		}
	}

	const OFFENCE_QUEUE_ERAS_BOUND: u32 = 10;
	/// Custom bound for [`OffenceQueueEras`] which is equal to `Config::BondingDuration +
	/// OFFENCE_QUEUE_ERAS_BOUND`.
	pub struct OffenceQueueErasBound<T>(core::marker::PhantomData<T>);
	impl<T: Config> Get<u32> for OffenceQueueErasBound<T> {
		fn get() -> u32 {
			let bonding_duration = T::BondingDuration::get();
			bonding_duration.saturating_add(OFFENCE_QUEUE_ERAS_BOUND) // adding OFFENCE_QUEUE_ERAS_BOUND eras
			                                                 // to add headroom to
			                                                 // the bound for runtime upgrades that
			                                                 // lower BondingDuration so we avoid
			                                                 // the try_into trap.
		}
	}

	/// A mapping from still-bonded eras to the first session index of that era.
	///
	/// Must contains information for eras for the range:
	/// `[active_era - bounding_duration; active_era]`
	#[pallet::storage]
	pub type BondedEras<T: Config> =
		StorageValue<_, BoundedVec<(EraIndex, SessionIndex), BondedErasBound<T>>, ValueQuery>;

	// --- AUDIT Note: end of storage items controlled by `Rotator`.

	/// Summary of validator exposure at a given era.
	///
	/// This contains the total stake in support of the validator and their own stake. In addition,
	/// it can also be used to get the number of nominators backing this validator and the number of
	/// exposure pages they are divided into. The page count is useful to determine the number of
	/// pages of rewards that needs to be claimed.
	///
	/// This is keyed first by the era index to allow bulk deletion and then the stash account.
	/// Should only be accessed through `Eras`.
	///
	/// Is it removed after [`Config::HistoryDepth`] eras.
	/// If stakers hasn't been set or has been removed then empty overview is returned.
	#[pallet::storage]
	pub type ErasStakersOverview<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		EraIndex,
		Twox64Concat,
		T::AccountId,
		PagedExposureMetadata<BalanceOf<T>>,
		OptionQuery,
	>;

	/// A bounded wrapper for [`sp_staking::ExposurePage`].
	///
	/// It has `Deref` and `DerefMut` impls that map it back [`sp_staking::ExposurePage`] for all
	/// purposes. This is done in such a way because we prefer to keep the types in [`sp_staking`]
	/// pure, and not polluted by pallet-specific bounding logic.
	///
	/// It encoded and decodes exactly the same as [`sp_staking::ExposurePage`], and provides a
	/// manual `MaxEncodedLen` implementation, to be used in benchmarking
	#[derive(PartialEqNoBound, Encode, Decode, DebugNoBound, TypeInfo, DefaultNoBound)]
	#[scale_info(skip_type_params(T))]
	pub struct BoundedExposurePage<T: Config>(pub ExposurePage<T::AccountId, BalanceOf<T>>);
	impl<T: Config> Deref for BoundedExposurePage<T> {
		type Target = ExposurePage<T::AccountId, BalanceOf<T>>;

		fn deref(&self) -> &Self::Target {
			&self.0
		}
	}

	impl<T: Config> core::ops::DerefMut for BoundedExposurePage<T> {
		fn deref_mut(&mut self) -> &mut Self::Target {
			&mut self.0
		}
	}

	impl<T: Config> codec::MaxEncodedLen for BoundedExposurePage<T> {
		fn max_encoded_len() -> usize {
			let max_exposure_page_size = T::MaxExposurePageSize::get() as usize;
			let individual_size =
				T::AccountId::max_encoded_len() + BalanceOf::<T>::max_encoded_len();

			// 1 balance for `total`
			BalanceOf::<T>::max_encoded_len() +
			// individual_size multiplied by page size
				max_exposure_page_size.saturating_mul(individual_size)
		}
	}

	impl<T: Config> From<ExposurePage<T::AccountId, BalanceOf<T>>> for BoundedExposurePage<T> {
		fn from(value: ExposurePage<T::AccountId, BalanceOf<T>>) -> Self {
			Self(value)
		}
	}

	impl<T: Config> From<BoundedExposurePage<T>> for ExposurePage<T::AccountId, BalanceOf<T>> {
		fn from(value: BoundedExposurePage<T>) -> Self {
			value.0
		}
	}

	impl<T: Config> codec::EncodeLike<BoundedExposurePage<T>>
		for ExposurePage<T::AccountId, BalanceOf<T>>
	{
	}

	/// Paginated exposure of a validator at given era.
	///
	/// This is keyed first by the era index to allow bulk deletion, then stash account and finally
	/// the page. Should only be accessed through `Eras`.
	///
	/// This is cleared after [`Config::HistoryDepth`] eras.
	#[pallet::storage]
	pub type ErasStakersPaged<T: Config> = StorageNMap<
		_,
		(
			NMapKey<Twox64Concat, EraIndex>,
			NMapKey<Twox64Concat, T::AccountId>,
			NMapKey<Twox64Concat, Page>,
		),
		BoundedExposurePage<T>,
		OptionQuery,
	>;

	pub struct ClaimedRewardsBound<T>(core::marker::PhantomData<T>);
	impl<T: Config> Get<u32> for ClaimedRewardsBound<T> {
		fn get() -> u32 {
			let max_total_nominators_per_validator =
				<T::ElectionProvider as ElectionProvider>::MaxBackersPerWinnerFinal::get();
			let exposure_page_size = T::MaxExposurePageSize::get();
			max_total_nominators_per_validator
				.saturating_div(exposure_page_size)
				.saturating_add(1)
		}
	}

	/// History of claimed paged rewards by era and validator.
	///
	/// This is keyed by era and validator stash which maps to the set of page indexes which have
	/// been claimed.
	///
	/// It is removed after [`Config::HistoryDepth`] eras.
	#[pallet::storage]
	pub type ClaimedRewards<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		EraIndex,
		Twox64Concat,
		T::AccountId,
		WeakBoundedVec<Page, ClaimedRewardsBound<T>>,
		ValueQuery,
	>;

	/// Exposure of validator at era with the preferences of validators.
	///
	/// This is keyed first by the era index to allow bulk deletion and then the stash account.
	///
	/// Is it removed after [`Config::HistoryDepth`] eras.
	// If prefs hasn't been set or has been removed then 0 commission is returned.
	#[pallet::storage]
	pub type ErasValidatorPrefs<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		EraIndex,
		Twox64Concat,
		T::AccountId,
		ValidatorPrefs,
		ValueQuery,
	>;

	/// The total staker reward budget for each era within [`Config::HistoryDepth`].
	///
	/// Set at era finalization:
	/// - in non-minting mode this is the snapshot of the era pot balance before any payouts.
	/// - in legacy mode it comes from `EraPayout`, with rewards minted on the fly.
	#[pallet::storage]
	pub type ErasValidatorReward<T: Config> = StorageMap<_, Twox64Concat, EraIndex, BalanceOf<T>>;

	/// Rewards for the last [`Config::HistoryDepth`] eras.
	/// If reward hasn't been set or has been removed then 0 reward is returned.
	#[pallet::storage]
	pub type ErasRewardPoints<T: Config> =
		StorageMap<_, Twox64Concat, EraIndex, EraRewardPoints<T>, ValueQuery>;

	/// The total amount staked for the last [`Config::HistoryDepth`] eras.
	/// If total hasn't been set or has been removed then 0 stake is returned.
	#[pallet::storage]
	pub type ErasTotalStake<T: Config> =
		StorageMap<_, Twox64Concat, EraIndex, BalanceOf<T>, ValueQuery>;

	/// Mode of era forcing.
	#[pallet::storage]
	pub type ForceEra<T> = StorageValue<_, Forcing, ValueQuery>;

	/// Maximum staked rewards, i.e. the percentage of the era inflation that
	/// is used for stake rewards.
	///
	/// Only used in legacy minting mode (`DisableMinting = false`).
	#[pallet::storage]
	pub type MaxStakedRewards<T> = StorageValue<_, Percent, OptionQuery>;

	/// The percentage of the slash that is distributed to reporters.
	///
	/// The rest of the slashed value is handled by the `Slash`.
	#[pallet::storage]
	pub type SlashRewardFraction<T> = StorageValue<_, Perbill, ValueQuery>;

	/// The amount of currency given to reporters of a slash event which was
	/// canceled by extraordinary circumstances (e.g. governance).
	#[pallet::storage]
	pub type CanceledSlashPayout<T: Config> = StorageValue<_, BalanceOf<T>, ValueQuery>;

	/// Stores reported offences in a queue until they are processed in subsequent blocks.
	///
	/// Each offence is recorded under the corresponding era index and the offending validator's
	/// account. If an offence spans multiple pages, only one page is processed at a time. Offences
	/// are handled sequentially, with their associated slashes computed and stored in
	/// `UnappliedSlashes`. These slashes are then applied in a future era as determined by
	/// `SlashDeferDuration`.
	///
	/// Any offences tied to an era older than `BondingDuration` are automatically dropped.
	/// Processing always prioritizes the oldest era first.
	#[pallet::storage]
	pub type OffenceQueue<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		EraIndex,
		Twox64Concat,
		T::AccountId,
		slashing::OffenceRecord<T::AccountId>,
	>;

	/// Tracks the eras that contain offences in `OffenceQueue`, sorted from **earliest to latest**.
	///
	/// - This ensures efficient retrieval of the oldest offence without iterating through
	/// `OffenceQueue`.
	/// - When a new offence is added to `OffenceQueue`, its era is **inserted in sorted order**
	/// if not already present.
	/// - When all offences for an era are processed, it is **removed** from this list.
	/// - The maximum length of this vector is bounded by `BondingDuration +
	///   OFFENCE_QUEUE_ERAS_BOUND`.
	///
	/// This eliminates the need for expensive iteration and sorting when fetching the next offence
	/// to process.
	#[pallet::storage]
	pub type OffenceQueueEras<T: Config> =
		StorageValue<_, WeakBoundedVec<u32, OffenceQueueErasBound<T>>>;

	/// Tracks the currently processed offence record from the `OffenceQueue`.
	///
	/// - When processing offences, an offence record is **popped** from the oldest era in
	///   `OffenceQueue` and stored here.
	/// - The function `process_offence` reads from this storage, processing one page of exposure at
	///   a time.
	/// - After processing a page, the `exposure_page` count is **decremented** until it reaches
	///   zero.
	/// - Once fully processed, the offence record is removed from this storage.
	///
	/// This ensures that offences are processed incrementally, preventing excessive computation
	/// in a single block while maintaining correct slashing behavior.
	#[pallet::storage]
	pub type ProcessingOffence<T: Config> =
		StorageValue<_, (EraIndex, T::AccountId, slashing::OffenceRecord<T::AccountId>)>;

	/// All unapplied slashes that are queued for later.
	#[pallet::storage]
	pub type UnappliedSlashes<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		EraIndex,
		Twox64Concat,
		// Unique key for unapplied slashes: (validator, slash fraction, page index).
		(T::AccountId, Perbill, u32),
		UnappliedSlash<T>,
		OptionQuery,
	>;

	/// Cancelled slashes by era and validator with maximum slash fraction to be cancelled.
	///
	/// When slashes are cancelled by governance, this stores the era and the validators
	/// whose slashes should be cancelled, along with the maximum slash fraction that should
	/// be cancelled for each validator.
	#[pallet::storage]
	pub type CancelledSlashes<T: Config> = StorageMap<
		_,
		Twox64Concat,
		EraIndex,
		BoundedVec<(T::AccountId, Perbill), T::MaxValidatorSet>,
		ValueQuery,
	>;

	/// All slashing events on validators, mapped by era to the highest slash proportion
	/// and slash value of the era.
	#[pallet::storage]
	pub type ValidatorSlashInEra<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		EraIndex,
		Twox64Concat,
		T::AccountId,
		(Perbill, BalanceOf<T>),
	>;

	/// The threshold for when users can start calling `chill_other` for other validators /
	/// nominators. The threshold is compared to the actual number of validators / nominators
	/// (`CountFor*`) in the system compared to the configured max (`Max*Count`).
	#[pallet::storage]
	pub type ChillThreshold<T: Config> = StorageValue<_, Percent, OptionQuery>;

	/// Voter snapshot progress status.
	///
	/// If the status is `Ongoing`, it keeps a cursor of the last voter retrieved to proceed when
	/// creating the next snapshot page.
	#[pallet::storage]
	pub type VoterSnapshotStatus<T: Config> =
		StorageValue<_, SnapshotStatus<T::AccountId>, ValueQuery>;

	/// Keeps track of an ongoing multi-page election solution request.
	///
	/// If `Some(_)``, it is the next page that we intend to elect. If `None`, we are not in the
	/// election process.
	///
	/// This is only set in multi-block elections. Should always be `None` otherwise.
	#[pallet::storage]
	pub type NextElectionPage<T: Config> = StorageValue<_, PageIndex, OptionQuery>;

	/// A bounded list of the "electable" stashes that resulted from a successful election.
	#[pallet::storage]
	pub type ElectableStashes<T: Config> =
		StorageValue<_, BoundedBTreeSet<T::AccountId, T::MaxValidatorSet>, ValueQuery>;

	/// Tracks the current step of era pruning process for each era being lazily pruned.
	#[pallet::storage]
	pub type EraPruningState<T: Config> = StorageMap<_, Twox64Concat, EraIndex, PruningStep>;

	/// The number of eras a validator can remain inactive during the last
	/// [`Config::HistoryDepth`] before being subject to chilling because of inactivity.
	///
	/// This must be less than or equal [`Config::HistoryDepth`] and bigger than 1.
	#[pallet::storage]
	pub type ChillInactiveThreshold<T: Config> = StorageValue<_, u32, ValueQuery, T::HistoryDepth>;

	#[pallet::genesis_config]
	#[derive(frame_support::DefaultNoBound, frame_support::DebugNoBound)]
	pub struct GenesisConfig<T: Config> {
		pub validator_count: u32,
		pub force_era: Forcing,
		pub slash_reward_fraction: Perbill,
		pub canceled_payout: BalanceOf<T>,
		pub stakers: Vec<(T::AccountId, BalanceOf<T>, crate::StakerStatus<T::AccountId>)>,
		pub min_nominator_bond: BalanceOf<T>,
		pub min_validator_bond: BalanceOf<T>,
		pub max_validator_count: Option<u32>,
		pub max_nominator_count: Option<u32>,
		/// Create the given number of validators and nominators.
		///
		/// These account need not be in the endowment list of balances, and are auto-topped up
		/// here.
		///
		/// Useful for testing genesis config.
		pub dev_stakers: Option<(u32, u32)>,
		/// initial active era, corresponding session index and start timestamp.
		pub active_era: (u32, u32, u64),
	}

	impl<T: Config> GenesisConfig<T> {
		fn generate_endowed_bonded_account(derivation: &str, rng: &mut ChaChaRng) -> T::AccountId {
			let pair: SrPair = Pair::from_string(&derivation, None)
				.expect(&format!("Failed to parse derivation string: {derivation}"));
			let who = T::AccountId::decode(&mut &pair.public().encode()[..])
				.expect(&format!("Failed to decode public key from pair: {:?}", pair.public()));

			let (min, max) = T::VoterList::range();
			let stake = BalanceOf::<T>::from(rng.next_u64().min(max).max(min));
			let two: BalanceOf<T> = 2u32.into();

			assert_ok!(T::Currency::mint_into(&who, stake * two));
			assert_ok!(<Pallet<T>>::bond(
				T::RuntimeOrigin::from(Some(who.clone()).into()),
				stake,
				RewardDestination::Staked,
			));
			who
		}
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
		fn build(&self) {
			crate::log!(trace, "initializing with {:?}", self);
			assert!(
				self.validator_count <=
					<T::ElectionProvider as ElectionProvider>::MaxWinnersPerPage::get() *
						<T::ElectionProvider as ElectionProvider>::Pages::get(),
				"validator count is too high, `ElectionProvider` can never fulfill this"
			);
			ValidatorCount::<T>::put(self.validator_count);

			ForceEra::<T>::put(self.force_era);
			CanceledSlashPayout::<T>::put(self.canceled_payout);
			SlashRewardFraction::<T>::put(self.slash_reward_fraction);
			MinNominatorBond::<T>::put(self.min_nominator_bond);
			MinValidatorBond::<T>::put(self.min_validator_bond);
			if let Some(x) = self.max_validator_count {
				MaxValidatorsCount::<T>::put(x);
			}
			if let Some(x) = self.max_nominator_count {
				MaxNominatorsCount::<T>::put(x);
			}

			// First pass: set up all validators and idle stakers
			for &(ref stash, balance, ref status) in &self.stakers {
				match status {
					crate::StakerStatus::Validator => {
						crate::log!(
							trace,
							"inserting genesis validator: {:?} => {:?} => {:?}",
							stash,
							balance,
							status
						);
						assert!(
							asset::free_to_stake::<T>(stash) >= balance,
							"Stash does not have enough balance to bond."
						);
						assert_ok!(<Pallet<T>>::bond(
							T::RuntimeOrigin::from(Some(stash.clone()).into()),
							balance,
							RewardDestination::Staked,
						));
						assert_ok!(<Pallet<T>>::validate(
							T::RuntimeOrigin::from(Some(stash.clone()).into()),
							Default::default(),
						));
					},
					crate::StakerStatus::Idle => {
						crate::log!(
							trace,
							"inserting genesis idle staker: {:?} => {:?} => {:?}",
							stash,
							balance,
							status
						);
						assert!(
							asset::free_to_stake::<T>(stash) >= balance,
							"Stash does not have enough balance to bond."
						);
						assert_ok!(<Pallet<T>>::bond(
							T::RuntimeOrigin::from(Some(stash.clone()).into()),
							balance,
							RewardDestination::Staked,
						));
					},
					_ => {},
				}
			}

			// Second pass: set up all nominators (now that validators exist)
			for &(ref stash, balance, ref status) in &self.stakers {
				match status {
					crate::StakerStatus::Nominator(votes) => {
						crate::log!(
							trace,
							"inserting genesis nominator: {:?} => {:?} => {:?}",
							stash,
							balance,
							status
						);
						assert!(
							asset::free_to_stake::<T>(stash) >= balance,
							"Stash does not have enough balance to bond."
						);
						assert_ok!(<Pallet<T>>::bond(
							T::RuntimeOrigin::from(Some(stash.clone()).into()),
							balance,
							RewardDestination::Staked,
						));
						assert_ok!(<Pallet<T>>::nominate(
							T::RuntimeOrigin::from(Some(stash.clone()).into()),
							votes.iter().map(|l| T::Lookup::unlookup(l.clone())).collect(),
						));
					},
					_ => {},
				}
			}

			// all voters are reported to the `VoterList`.
			assert_eq!(
				T::VoterList::count(),
				Nominators::<T>::count() + Validators::<T>::count(),
				"not all genesis stakers were inserted into sorted list provider, something is wrong."
			);

			// now generate the dev stakers, after all else is setup
			if let Some((validators, nominators)) = self.dev_stakers {
				crate::log!(
					debug,
					"generating dev stakers: validators: {}, nominators: {}",
					validators,
					nominators
				);
				let base_derivation = "//staker//{}";

				// it is okay for the randomness to be the same on every call. If we want different,
				// we can make `base_derivation` configurable.
				let mut rng = ChaChaRng::from_seed(
					base_derivation.using_encoded(sp_crypto_hashing::blake2_256),
				);

				(0..validators).for_each(|index| {
					let derivation = base_derivation.replace("{}", &format!("validator{}", index));
					let who = Self::generate_endowed_bonded_account(&derivation, &mut rng);
					assert_ok!(<Pallet<T>>::validate(
						T::RuntimeOrigin::from(Some(who.clone()).into()),
						Default::default(),
					));
				});

				// This allows us to work with configs like `dev_stakers: (0, 10)`. Don't create new
				// validators, just add a bunch of nominators. Useful for slashing tests.
				let all_validators = Validators::<T>::iter_keys().collect::<Vec<_>>();

				(0..nominators).for_each(|index| {
					let derivation = base_derivation.replace("{}", &format!("nominator{}", index));
					let who = Self::generate_endowed_bonded_account(&derivation, &mut rng);

					let random_nominations = all_validators
						.choose_multiple(&mut rng, MaxNominationsOf::<T>::get() as usize)
						.map(|v| v.clone())
						.collect::<Vec<_>>();

					assert_ok!(<Pallet<T>>::nominate(
						T::RuntimeOrigin::from(Some(who.clone()).into()),
						random_nominations.iter().map(|l| T::Lookup::unlookup(l.clone())).collect(),
					));
				})
			}

			// Chains initialized with this storage item maintain `ErasSumWeightedPoints` from
			// era 0. Pin the cutoff to the first era so every era uses the weighted-points formula
			// and the idempotent migration only acts on chains whose storage predates this item.
			WeightedPointsFormulaStartEra::<T>::put(0);

			let (active_era, session_index, timestamp) = self.active_era;
			ActiveEra::<T>::put(ActiveEraInfo { index: active_era, start: Some(timestamp) });
			// at genesis, we do not have any new planned era.
			CurrentEra::<T>::put(active_era);
			// set the bonded genesis era
			BondedEras::<T>::put(
				BoundedVec::<_, BondedErasBound<T>>::try_from(
					alloc::vec![(active_era, session_index)]
				)
				.expect("bound for BondedEras is BondingDuration + 1; can contain at least one element; qed")
			);
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub fn deposit_event)]
	pub enum Event<T: Config> {
		/// The era payout has been set.
		///
		/// In non-minting mode, `validator_payout` is the staker reward budget
		/// snapshotted from the general pot, and `remainder` is always zero.
		/// In legacy minting mode, both fields reflect the `EraPayout` computation.
		EraPaid {
			era_index: EraIndex,
			validator_payout: BalanceOf<T>,
			remainder: BalanceOf<T>,
		},
		/// The nominator has been rewarded by this amount to this destination.
		Rewarded {
			stash: T::AccountId,
			dest: RewardDestination<T::AccountId>,
			amount: BalanceOf<T>,
		},
		/// A staker (validator or nominator) has been slashed by the given amount.
		Slashed {
			staker: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// An old slashing report from a prior era was discarded because it could
		/// not be processed.
		OldSlashingReportDiscarded {
			session_index: SessionIndex,
		},
		/// An account has bonded this amount. \[stash, amount\]
		///
		/// NOTE: This event is only emitted when funds are bonded via a dispatchable. Notably,
		/// it will not be emitted for staking rewards when they are added to stake.
		Bonded {
			stash: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// An account has unbonded this amount.
		Unbonded {
			stash: T::AccountId,
			amount: BalanceOf<T>,
			/// The era at which `amount` becomes withdrawable.
			era: EraIndex,
		},
		/// An account has called `withdraw_unbonded` and removed unbonding chunks worth `Balance`
		/// from the unlocking queue.
		Withdrawn {
			stash: T::AccountId,
			amount: BalanceOf<T>,
		},
		/// A subsequent event of `Withdrawn`, indicating that `stash` was fully removed from the
		/// system.
		StakerRemoved {
			stash: T::AccountId,
		},
		/// A nominator has been kicked from a validator.
		Kicked {
			nominator: T::AccountId,
			stash: T::AccountId,
		},
		/// An account has stopped participating as either a validator or nominator.
		Chilled {
			stash: T::AccountId,
		},
		/// A Page of stakers rewards are getting paid. `next` is `None` if all pages are claimed.
		PayoutStarted {
			era_index: EraIndex,
			validator_stash: T::AccountId,
			page: Page,
			next: Option<Page>,
		},
		/// A validator has set their preferences.
		ValidatorPrefsSet {
			stash: T::AccountId,
			prefs: ValidatorPrefs,
		},
		/// Voters size limit reached.
		SnapshotVotersSizeExceeded {
			size: u32,
		},
		/// Targets size limit reached.
		SnapshotTargetsSizeExceeded {
			size: u32,
		},
		ForceEra {
			mode: Forcing,
		},
		/// Report of a controller batch deprecation.
		ControllerBatchDeprecated {
			failures: u32,
		},
		/// Staking balance migrated from locks to holds, with any balance that could not be held
		/// is force withdrawn.
		CurrencyMigrated {
			stash: T::AccountId,
			force_withdraw: BalanceOf<T>,
		},
		/// A page from a multi-page election was fetched. A number of these are followed by
		/// `StakersElected`.
		///
		/// `Ok(count)` indicates the give number of stashes were added.
		/// `Err(index)` indicates that the stashes after index were dropped.
		/// `Err(0)` indicates that an error happened but no stashes were dropped nor added.
		///
		/// The error indicates that a number of validators were dropped due to excess size, but
		/// the overall election will continue.
		PagedElectionProceeded {
			page: PageIndex,
			result: Result<u32, u32>,
		},
		/// An offence for the given validator, for the given percentage of their stake, at the
		/// given era as been reported.
		OffenceReported {
			offence_era: EraIndex,
			validator: T::AccountId,
			fraction: Perbill,
		},
		/// An offence has been processed and the corresponding slash has been computed.
		SlashComputed {
			offence_era: EraIndex,
			slash_era: EraIndex,
			offender: T::AccountId,
			page: u32,
		},
		/// An unapplied slash has been cancelled.
		SlashCancelled {
			slash_era: EraIndex,
			validator: T::AccountId,
		},
		/// Session change has been triggered.
		///
		/// If planned_era is one era ahead of active_era, it implies new era is being planned and
		/// election is ongoing.
		SessionRotated {
			starting_session: SessionIndex,
			active_era: EraIndex,
			planned_era: EraIndex,
		},
		/// Something occurred that should never happen under normal operation.
		/// Logged as an event for fail-safe observability.
		Unexpected(UnexpectedKind<T>),
		/// An offence was reported that was too old to be processed, and thus was dropped.
		OffenceTooOld {
			offence_era: EraIndex,
			validator: T::AccountId,
			fraction: Perbill,
		},
		/// An old era with the given index was pruned.
		EraPruned {
			index: EraIndex,
		},
		/// The validator has been paid their self-stake incentive bonus.
		ValidatorIncentivePaid {
			era: EraIndex,
			validator_stash: T::AccountId,
			dest: RewardDestination<T::AccountId>,
			amount: BalanceOf<T>,
		},
		/// Validator self-stake incentive configuration has been updated.
		ValidatorIncentiveConfigSet {
			optimum_self_stake: BalanceOf<T>,
			hard_cap_self_stake: BalanceOf<T>,
			slope_factor: Perbill,
		},
	}

	/// Represents unexpected or invariant-breaking conditions encountered during execution.
	///
	/// These variants are emitted as [`Event::Unexpected`] and indicate a defensive check has
	/// failed. While these should never occur under normal operation, they are useful for
	/// diagnosing issues in production or test environments.
	#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, TypeInfo, DebugNoBound)]
	#[codec(mel_bound())]
	#[scale_info(skip_type_params(T))]
	pub enum UnexpectedKind<T: Config> {
		/// Emitted when calculated era duration exceeds the configured maximum.
		EraDurationBoundExceeded,
		/// Received a validator activation event that is not recognized.
		UnknownValidatorActivation,
		/// Failed to proceed paged election due to weight limits
		PagedElectionOutOfWeight { page: PageIndex, required: Weight, had: Weight },
		/// Payee not set for a staker when paying rewards.
		MissingPayee { era: EraIndex, stash: T::AccountId },
		/// Total validator weight is zero but incentive allocation exists.
		ValidatorIncentiveWeightMismatch { era: EraIndex },
		/// Validator incentive transfer from era pot failed.
		ValidatorIncentiveTransferFailed { era: EraIndex },
	}

	#[pallet::error]
	#[derive(PartialEq)]
	pub enum Error<T> {
		/// Not a controller account.
		NotController,
		/// Not a stash account.
		NotStash,
		/// Stash is already bonded.
		AlreadyBonded,
		/// Controller is already paired.
		AlreadyPaired,
		/// Targets cannot be empty.
		EmptyTargets,
		/// Duplicate index.
		DuplicateIndex,
		/// Slash record not found.
		InvalidSlashRecord,
		/// Cannot bond, nominate or validate with value less than the minimum defined by
		/// governance (see `MinValidatorBond` and `MinNominatorBond`). If unbonding is the
		/// intention, `chill` first to remove one's role as validator/nominator.
		InsufficientBond,
		/// Can not schedule more unlock chunks.
		NoMoreChunks,
		/// Can not rebond without unlocking chunks.
		NoUnlockChunk,
		/// Attempting to target a stash that still has funds.
		FundedTarget,
		/// Invalid era to reward.
		InvalidEraToReward,
		/// Invalid number of nominations.
		InvalidNumberOfNominations,
		/// Rewards for this era have already been claimed for this validator.
		AlreadyClaimed,
		/// No nominators exist on this page.
		InvalidPage,
		/// Incorrect previous history depth input provided.
		IncorrectHistoryDepth,
		/// Internal state has become somehow corrupted and the operation cannot continue.
		BadState,
		/// Too many nomination targets supplied.
		TooManyTargets,
		/// A nomination target was supplied that was blocked or otherwise not a validator.
		BadTarget,
		/// The user has enough bond and thus cannot be chilled forcefully by an external person.
		CannotChillOther,
		/// There are too many nominators in the system. Governance needs to adjust the staking
		/// settings to keep things safe for the runtime.
		TooManyNominators,
		/// There are too many validator candidates in the system. Governance needs to adjust the
		/// staking settings to keep things safe for the runtime.
		TooManyValidators,
		/// Commission is too low. Must be at least `MinCommission`.
		CommissionTooLow,
		/// Some bound is not met.
		BoundNotMet,
		/// Used when attempting to use deprecated controller account logic.
		ControllerDeprecated,
		/// Cannot reset a ledger.
		CannotRestoreLedger,
		/// Provided reward destination is not allowed.
		RewardDestinationRestricted,
		/// Not enough funds available to withdraw.
		NotEnoughFunds,
		/// Operation not allowed for virtual stakers.
		VirtualStakerNotAllowed,
		/// Stash could not be reaped as other pallet might depend on it.
		CannotReapStash,
		/// The stake of this account is already migrated to `Fungible` holds.
		AlreadyMigrated,
		/// Era not yet started.
		EraNotStarted,
		/// Account is restricted from participation in staking. This may happen if the account is
		/// staking in another way already, such as via pool.
		Restricted,
		/// Unapplied slashes in the recently concluded era is blocking this operation.
		/// See `Call::apply_slash` to apply them.
		UnappliedSlashesInPreviousEra,
		/// The era is not eligible for pruning.
		EraNotPrunable,
		/// The slash has been cancelled and cannot be applied.
		CancelledSlash,
		/// Commission is higher than the allowed maximum `MaxCommission`.
		CommissionTooHigh,
		/// Optimum self-stake cannot be greater than hard cap.
		OptimumGreaterThanCap,
		/// Validator inactivity proof is invalid.
		InvalidInactivityProof(InvalidInactivityProofError),
		/// Cannot set [`ChillInactiveThreshold`] to the provided value.
		InvalidChillInactiveThreshold,
	}

	#[derive(Encode, Decode, DecodeWithMemTracking, PartialEq, Eq, TypeInfo, PalletError)]
	pub enum InvalidInactivityProofError {
		/// Invalid proof length.
		InvalidLen,
		/// Eras in proof are not sorted.
		NotSorted,
		/// Validator wasn't expected to validate at some era.
		ValidatorNotExposed,
		/// Validator wasn't inactive at some era.
		ValidatorActive,
		/// An invalid era was provided in the proof.
		InvalidEra,
	}

	impl<T: Config> Pallet<T> {
		/// Apply previously-unapplied slashes on the beginning of a new era, after a delay.
		pub fn apply_unapplied_slashes(active_era: EraIndex) -> Weight {
			let mut slashes = UnappliedSlashes::<T>::iter_prefix(&active_era).take(1);
			if let Some((key, slash)) = slashes.next() {
				crate::log!(
					debug,
					"🦹 found slash {:?} scheduled to be executed in era {:?}",
					slash,
					active_era,
				);

				let nominators_slashed = slash.others.len() as u32;

				// Check if this slash has been cancelled
				if Self::check_slash_cancelled(active_era, &key.0, key.1) {
					crate::log!(
						debug,
						"🦹 slash for {:?} in era {:?} was cancelled, skipping",
						key.0,
						active_era,
					);
				} else {
					slashing::apply_slash::<T>(slash, Self::offence_era_of(active_era));
				}

				// Always remove the slash from UnappliedSlashes
				UnappliedSlashes::<T>::remove(&active_era, &key);

				// Check if there are more slashes for this era
				if UnappliedSlashes::<T>::iter_prefix(&active_era).next().is_none() {
					// No more slashes for this era, clear CancelledSlashes
					CancelledSlashes::<T>::remove(&active_era);
				}

				T::WeightInfo::apply_slash(nominators_slashed)
			} else {
				// No slashes found for this era
				T::DbWeight::get().reads(1)
			}
		}

		/// Execute one step of era pruning and get actual weight used
		fn do_prune_era_step(era: EraIndex) -> Result<Weight, DispatchError> {
			// Get current pruning state. If EraPruningState doesn't exist, it means:
			// - Era was never marked for pruning, OR
			// - Era was already fully pruned (pruning state was removed on final step)
			// In either case, this is an error - user should not call prune on non-prunable eras
			let current_step = EraPruningState::<T>::get(era).ok_or(Error::<T>::EraNotPrunable)?;

			// Limit items to prevent deleting more than we can safely account for in weight
			// calculations
			let items_limit = T::MaxPruningItems::get().min(T::MaxValidatorSet::get());

			let actual_weight = match current_step {
				PruningStep::ErasStakersPaged => {
					let result = ErasStakersPaged::<T>::clear_prefix((era,), items_limit, None);
					let items_deleted = result.backend as u32;
					result.maybe_cursor.is_none().then(|| {
						EraPruningState::<T>::insert(era, PruningStep::ErasStakersOverview)
					});
					T::WeightInfo::prune_era_stakers_paged(items_deleted)
				},
				PruningStep::ErasStakersOverview => {
					let result = ErasStakersOverview::<T>::clear_prefix(era, items_limit, None);
					let items_deleted = result.backend as u32;
					result.maybe_cursor.is_none().then(|| {
						EraPruningState::<T>::insert(era, PruningStep::ErasValidatorPrefs)
					});
					T::WeightInfo::prune_era_stakers_overview(items_deleted)
				},
				PruningStep::ErasValidatorPrefs => {
					let result = ErasValidatorPrefs::<T>::clear_prefix(era, items_limit, None);
					let items_deleted = result.backend as u32;
					result
						.maybe_cursor
						.is_none()
						.then(|| EraPruningState::<T>::insert(era, PruningStep::ClaimedRewards));
					T::WeightInfo::prune_era_validator_prefs(items_deleted)
				},
				PruningStep::ClaimedRewards => {
					let result = ClaimedRewards::<T>::clear_prefix(era, items_limit, None);
					let items_deleted = result.backend as u32;
					result.maybe_cursor.is_none().then(|| {
						EraPruningState::<T>::insert(era, PruningStep::ErasValidatorReward)
					});
					T::WeightInfo::prune_era_claimed_rewards(items_deleted)
				},
				PruningStep::ErasValidatorReward => {
					ErasValidatorReward::<T>::remove(era);
					EraPruningState::<T>::insert(era, PruningStep::ErasRewardPoints);
					T::WeightInfo::prune_era_validator_reward()
				},
				PruningStep::ErasRewardPoints => {
					ErasRewardPoints::<T>::remove(era);
					EraPruningState::<T>::insert(era, PruningStep::SingleEntryCleanups);
					T::WeightInfo::prune_era_reward_points()
				},
				PruningStep::SingleEntryCleanups => {
					ErasTotalStake::<T>::remove(era);
					ErasNominatorsSlashable::<T>::remove(era);
					ErasValidatorIncentiveBudget::<T>::remove(era);
					ErasSumValidatorIncentiveWeight::<T>::remove(era);
					ErasSumWeightedPoints::<T>::remove(era);
					EraPruningState::<T>::insert(era, PruningStep::ValidatorSlashInEra);
					T::WeightInfo::prune_era_single_entry_cleanups()
				},
				PruningStep::ValidatorSlashInEra => {
					let result = ValidatorSlashInEra::<T>::clear_prefix(era, items_limit, None);
					let items_deleted = result.backend as u32;

					if result.maybe_cursor.is_none() {
						EraPruningState::<T>::insert(
							era,
							PruningStep::ErasValidatorIncentiveWeight,
						);
					}

					T::WeightInfo::prune_era_validator_slash_in_era(items_deleted)
				},
				PruningStep::ErasValidatorIncentiveWeight => {
					let result =
						ErasValidatorIncentiveWeight::<T>::clear_prefix(era, items_limit, None);
					if result.maybe_cursor.is_none() {
						// Final step — remove pruning state.
						EraPruningState::<T>::remove(era);
					}
					T::WeightInfo::prune_era_validator_incentive_weight(result.backend as u32)
				},
			};

			// Check if era is fully pruned (pruning state removed) and emit event
			if EraPruningState::<T>::get(era).is_none() {
				Self::deposit_event(Event::<T>::EraPruned { index: era });
			}

			Ok(actual_weight)
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_poll(_now: BlockNumberFor<T>, weight_meter: &mut WeightMeter) {
			let (weight, exec) = EraElectionPlanner::<T>::maybe_fetch_election_results();
			crate::log!(
				trace,
				"weight of fetching next election page is {:?}, have {:?}",
				weight,
				weight_meter.remaining()
			);

			if weight_meter.can_consume(weight) {
				exec(weight_meter);
			} else {
				Self::deposit_event(Event::<T>::Unexpected(
					UnexpectedKind::PagedElectionOutOfWeight {
						page: NextElectionPage::<T>::get().unwrap_or(
							EraElectionPlanner::<T>::election_pages().defensive_saturating_sub(1),
						),
						required: weight,
						had: weight_meter.remaining(),
					},
				));
			}
		}

		fn on_initialize(_now: BlockNumberFor<T>) -> Weight {
			// Process our queue, using the era-specific nominators slashable setting.
			let mut consumed_weight = slashing::process_offence_for_era::<T>();

			// apply any pending slashes after `SlashDeferDuration`.
			consumed_weight.saturating_accrue(T::DbWeight::get().reads(1));
			if let Some(active_era) = ActiveEra::<T>::get() {
				let slash_weight = Self::apply_unapplied_slashes(active_era.index);
				consumed_weight.saturating_accrue(slash_weight);
			}

			consumed_weight
		}

		fn integrity_test() {
			// ensure that we funnel the correct value to the `DataProvider::MaxVotesPerVoter`;
			assert_eq!(
				MaxNominationsOf::<T>::get(),
				<Self as ElectionDataProvider>::MaxVotesPerVoter::get()
			);

			// and that MaxNominations is always greater than 1, since we count on this.
			assert!(!MaxNominationsOf::<T>::get().is_zero());

			assert!(
				T::SlashDeferDuration::get() < T::BondingDuration::get() || T::BondingDuration::get() == 0,
				"As per documentation, slash defer duration ({}) should be less than bonding duration ({}).",
				T::SlashDeferDuration::get(),
				T::BondingDuration::get(),
			);

			// Ensure NominatorFastUnbondDuration is not greater than BondingDuration
			assert!(
				T::NominatorFastUnbondDuration::get() <= T::BondingDuration::get(),
				"NominatorFastUnbondDuration ({}) must not exceed BondingDuration ({}).",
				T::NominatorFastUnbondDuration::get(),
				T::BondingDuration::get(),
			);
			// Ensure MaxPruningItems is reasonable (minimum 100 for efficiency)
			assert!(
				T::MaxPruningItems::get() >= 100,
				"MaxPruningItems must be at least 100 for efficient pruning, got: {}",
				T::MaxPruningItems::get()
			);

			assert!(
				crate::POT_POOL_SIZE > T::HistoryDepth::get(),
				"POT_POOL_SIZE ({}) must be strictly greater than HistoryDepth ({}) \
				 to avoid reusing a pot slot whose era is still in the active history.",
				crate::POT_POOL_SIZE,
				T::HistoryDepth::get(),
			);

			// If minting is disabled, EraPayout must be a noop to prevent double-minting.
			if T::DisableMinting::get() {
				let (v, r) = T::EraPayout::era_payout(
					BalanceOf::<T>::from(1u64),
					BalanceOf::<T>::from(1u64),
					1000u64,
				);
				assert!(
					v.is_zero() && r.is_zero(),
					"DisableMinting is true but EraPayout returns non-zero. \
					 Set EraPayout = () when DisableMinting = true."
				);
			}
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(n: BlockNumberFor<T>) -> Result<(), sp_runtime::TryRuntimeError> {
			Self::do_try_state(n)
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Take the origin account as a stash and lock up `value` of its balance. `controller` will
		/// be the account that controls it.
		///
		/// `value` must be more than the `minimum_balance` specified by `T::Currency`.
		///
		/// The dispatch origin for this call must be _Signed_ by the stash account.
		///
		/// Emits `Bonded`.
		///
		/// NOTE: Two of the storage writes (`Self::bonded`, `Self::payee`) are _never_ cleaned
		/// unless the `origin` falls below _existential deposit_ (or equal to 0) and gets removed
		/// as dust.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::bond())]
		pub fn bond(
			origin: OriginFor<T>,
			#[pallet::compact] value: BalanceOf<T>,
			payee: RewardDestination<T::AccountId>,
		) -> DispatchResult {
			let stash = ensure_signed(origin)?;

			ensure!(!T::Filter::contains(&stash), Error::<T>::Restricted);

			if StakingLedger::<T>::is_bonded(StakingAccount::Stash(stash.clone())) {
				return Err(Error::<T>::AlreadyBonded.into());
			}

			// An existing controller cannot become a stash.
			if StakingLedger::<T>::is_bonded(StakingAccount::Controller(stash.clone())) {
				return Err(Error::<T>::AlreadyPaired.into());
			}

			// Reject a bond which is lower than the minimum bond.
			if value < Self::min_chilled_bond() {
				return Err(Error::<T>::InsufficientBond.into());
			}

			let stash_balance = asset::free_to_stake::<T>(&stash);
			let value = value.min(stash_balance);
			Self::deposit_event(Event::<T>::Bonded { stash: stash.clone(), amount: value });
			let ledger = StakingLedger::<T>::new(stash.clone(), value);

			// You're auto-bonded forever, here. We might improve this by only bonding when
			// you actually validate/nominate and remove once you unbond __everything__.
			ledger.bond(payee)?;

			Ok(())
		}

		/// Add some extra amount that have appeared in the stash `free_balance` into the balance up
		/// for staking.
		///
		/// The dispatch origin for this call must be _Signed_ by the stash, not the controller.
		///
		/// Use this if there are additional funds in your stash account that you wish to bond.
		/// Unlike [`bond`](Self::bond) or [`unbond`](Self::unbond) this function does not impose
		/// any limitation on the amount that can be added.
		///
		/// Emits `Bonded`.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::bond_extra())]
		pub fn bond_extra(
			origin: OriginFor<T>,
			#[pallet::compact] max_additional: BalanceOf<T>,
		) -> DispatchResult {
			let stash = ensure_signed(origin)?;
			ensure!(!T::Filter::contains(&stash), Error::<T>::Restricted);
			Self::do_bond_extra(&stash, max_additional)
		}

		/// Schedule a portion of the stash to be unlocked ready for transfer out after the bond
		/// period ends. If this leaves an amount actively bonded less than
		/// [`asset::existential_deposit`], then it is increased to the full amount.
		///
		/// The dispatch origin for this call must be _Signed_ by the controller, not the stash.
		///
		/// Once the unlock period is done, you can call `withdraw_unbonded` to actually move
		/// the funds out of management ready for transfer.
		///
		/// No more than a limited number of unlocking chunks (see `MaxUnlockingChunks`)
		/// can co-exists at the same time. If there are no unlocking chunks slots available
		/// [`Call::withdraw_unbonded`] is called to remove some of the chunks (if possible).
		///
		/// If a user encounters the `InsufficientBond` error when calling this extrinsic,
		/// they should call `chill` first in order to free up their bonded funds.
		///
		/// Emits `Unbonded`.
		///
		/// See also [`Call::withdraw_unbonded`].
		#[pallet::call_index(2)]
		#[pallet::weight(
            T::WeightInfo::withdraw_unbonded_kill().saturating_add(T::WeightInfo::unbond()))
        ]
		pub fn unbond(
			origin: OriginFor<T>,
			#[pallet::compact] value: BalanceOf<T>,
		) -> DispatchResultWithPostInfo {
			let controller = ensure_signed(origin)?;
			let unlocking =
				Self::ledger(Controller(controller.clone())).map(|l| l.unlocking.len())?;

			// if there are no unlocking chunks available, try to remove any chunks by withdrawing
			// funds that have fully unbonded.
			let maybe_withdraw_weight = {
				if unlocking == T::MaxUnlockingChunks::get() as usize {
					Some(Self::do_withdraw_unbonded(&controller)?)
				} else {
					None
				}
			};

			// we need to fetch the ledger again because it may have been mutated in the call
			// to `Self::do_withdraw_unbonded` above.
			let mut ledger = Self::ledger(Controller(controller))?;
			let mut value = value.min(ledger.active);
			let stash = ledger.stash.clone();

			// If unbonding all active stake, chill the stash first to avoid `InsufficientBond`
			// errors. This matches the behavior of pallet-staking.
			let chill_weight = if value >= ledger.active {
				Self::chill_stash(&stash);
				T::WeightInfo::chill()
			} else {
				Weight::zero()
			};

			ensure!(
				ledger.unlocking.len() < T::MaxUnlockingChunks::get() as usize,
				Error::<T>::NoMoreChunks,
			);

			if !value.is_zero() {
				ledger.active -= value;

				// Avoid there being a dust balance left in the staking system.
				if ledger.active < asset::existential_deposit::<T>() {
					value += ledger.active;
					ledger.active = Zero::zero();
				}

				let is_nominator = Nominators::<T>::contains_key(&stash);

				let min_active_bond = if is_nominator {
					Self::min_nominator_bond()
				} else if Validators::<T>::contains_key(&stash) {
					Self::min_validator_bond()
				} else {
					// staker is chilled, no min bond.
					Zero::zero()
				};

				// Make sure that the user maintains enough active bond for their role.
				// If a user runs into this error, they should chill first.
				ensure!(ledger.active >= min_active_bond, Error::<T>::InsufficientBond);

				// Determine unbonding duration based on validator history.
				// If the account was a validator in recent eras (within BondingDuration), they must
				// wait the full BondingDuration even if they've switched to nominator role.
				// This prevents validators from avoiding slashing by switching roles and using the
				// shorter nominator unbonding period.
				let active_era = session_rotation::Rotator::<T>::active_era();
				let was_recent_validator = LastValidatorEra::<T>::get(&stash)
					.map(|last_era| active_era.saturating_sub(last_era) < T::BondingDuration::get())
					.unwrap_or(false);

				let unbond_duration = if was_recent_validator {
					// Use full bonding duration for recent validators
					T::BondingDuration::get()
				} else {
					// Use nominator bonding duration for pure nominators
					<Self as sp_staking::StakingInterface>::nominator_bonding_duration()
				};

				let era =
					session_rotation::Rotator::<T>::active_era().saturating_add(unbond_duration);
				if let Some(chunk) = ledger.unlocking.last_mut().filter(|chunk| chunk.era == era) {
					// To keep the chunk count down, we only keep one chunk per era. Since
					// `unlocking` is a FiFo queue, if a chunk exists for `era` we know that it will
					// be the last one.
					chunk.value = chunk.value.defensive_saturating_add(value)
				} else {
					ledger
						.unlocking
						.try_push(UnlockChunk { value, era })
						.map_err(|_| Error::<T>::NoMoreChunks)?;
				};
				// NOTE: ledger must be updated prior to calling `Self::weight_of`.
				ledger.update()?;

				// update this staker in the sorted list, if they exist in it.
				if T::VoterList::contains(&stash) {
					let _ = T::VoterList::on_update(&stash, Self::weight_of(&stash));
				}

				Self::deposit_event(Event::<T>::Unbonded { stash, amount: value, era });
			}

			let actual_weight = if let Some(withdraw_weight) = maybe_withdraw_weight {
				Some(
					T::WeightInfo::unbond()
						.saturating_add(withdraw_weight)
						.saturating_add(chill_weight),
				)
			} else {
				Some(T::WeightInfo::unbond().saturating_add(chill_weight))
			};

			Ok(actual_weight.into())
		}

		/// Remove any stake that has been fully unbonded and is ready for withdrawal.
		///
		/// Stake is considered fully unbonded once [`Config::BondingDuration`] has elapsed since
		/// the unbonding was initiated. In rare cases—such as when offences for the unbonded era
		/// have been reported but not yet processed—withdrawal is restricted to eras for which
		/// all offences have been processed.
		///
		/// The unlocked stake will be returned as free balance in the stash account.
		///
		/// The dispatch origin for this call must be _Signed_ by the controller.
		///
		/// Emits `Withdrawn`.
		///
		/// See also [`Call::unbond`].
		///
		/// ## Parameters
		///
		/// - `num_slashing_spans`: **Deprecated**. Retained only for backward compatibility; this
		///   parameter has no effect.
		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::withdraw_unbonded_kill())]
		pub fn withdraw_unbonded(
			origin: OriginFor<T>,
			_num_slashing_spans: u32,
		) -> DispatchResultWithPostInfo {
			let controller = ensure_signed(origin)?;

			let actual_weight = Self::do_withdraw_unbonded(&controller)?;
			Ok(Some(actual_weight).into())
		}

		/// Declare the desire to validate for the origin controller.
		///
		/// Effects will be felt at the beginning of the next era.
		///
		/// The dispatch origin for this call must be _Signed_ by the controller, not the stash.
		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::validate())]
		pub fn validate(origin: OriginFor<T>, prefs: ValidatorPrefs) -> DispatchResult {
			let controller = ensure_signed(origin)?;

			let ledger = Self::ledger(Controller(controller))?;

			ensure!(ledger.active >= Self::min_validator_bond(), Error::<T>::InsufficientBond);
			let stash = &ledger.stash;

			// ensure their commission is correct.
			ensure!(prefs.commission >= MinCommission::<T>::get(), Error::<T>::CommissionTooLow);
			ensure!(prefs.commission <= MaxCommission::<T>::get(), Error::<T>::CommissionTooHigh);

			// Only check limits if they are not already a validator.
			if !Validators::<T>::contains_key(stash) {
				// If this error is reached, we need to adjust the `MinValidatorBond` and start
				// calling `chill_other`. Until then, we explicitly block new validators to protect
				// the runtime.
				if let Some(max_validators) = MaxValidatorsCount::<T>::get() {
					ensure!(
						Validators::<T>::count() < max_validators,
						Error::<T>::TooManyValidators
					);
				}
			}

			Self::do_remove_nominator(stash);
			Self::do_add_validator(stash, prefs.clone());
			Self::deposit_event(Event::<T>::ValidatorPrefsSet { stash: ledger.stash, prefs });

			Ok(())
		}

		/// Declare the desire to nominate `targets` for the origin controller.
		///
		/// Effects will be felt at the beginning of the next era.
		///
		/// The dispatch origin for this call must be _Signed_ by the controller, not the stash.
		#[pallet::call_index(5)]
		#[pallet::weight(T::WeightInfo::nominate(targets.len() as u32))]
		pub fn nominate(
			origin: OriginFor<T>,
			targets: Vec<AccountIdLookupOf<T>>,
		) -> DispatchResult {
			let controller = ensure_signed(origin)?;

			let ledger = Self::ledger(StakingAccount::Controller(controller.clone()))?;

			ensure!(ledger.active >= Self::min_nominator_bond(), Error::<T>::InsufficientBond);
			let stash = &ledger.stash;

			// Only check limits if they are not already a nominator.
			if !Nominators::<T>::contains_key(stash) {
				// If this error is reached, we need to adjust the `MinNominatorBond` and start
				// calling `chill_other`. Until then, we explicitly block new nominators to protect
				// the runtime.
				if let Some(max_nominators) = MaxNominatorsCount::<T>::get() {
					ensure!(
						Nominators::<T>::count() < max_nominators,
						Error::<T>::TooManyNominators
					);
				}
			}

			// dedup targets
			let mut targets = targets
				.into_iter()
				.map(|t| T::Lookup::lookup(t).map_err(DispatchError::from))
				.collect::<Result<Vec<_>, _>>()?;
			targets.sort();
			targets.dedup();

			ensure!(!targets.is_empty(), Error::<T>::EmptyTargets);
			ensure!(
				targets.len() <= T::NominationsQuota::get_quota(ledger.active) as usize,
				Error::<T>::TooManyTargets
			);

			let old = Nominators::<T>::get(stash).map_or_else(Vec::new, |x| x.targets.into_inner());

			let targets: BoundedVec<_, _> = targets
				.into_iter()
				.map(|n| {
					if old.contains(&n) ||
						(Validators::<T>::contains_key(&n) && !Validators::<T>::get(&n).blocked)
					{
						Ok(n)
					} else {
						Err(Error::<T>::BadTarget.into())
					}
				})
				.collect::<Result<Vec<_>, DispatchError>>()?
				.try_into()
				.map_err(|_| Error::<T>::TooManyNominators)?;

			let nominations = Nominations {
				targets,
				// Initial nominations are considered submitted at era 0. See `Nominations` doc.
				submitted_in: CurrentEra::<T>::get().unwrap_or(0),
				suppressed: false,
			};

			Self::do_remove_validator(stash);
			Self::do_add_nominator(stash, nominations);
			Ok(())
		}

		/// Declare no desire to either validate or nominate.
		///
		/// Effects will be felt at the beginning of the next era.
		///
		/// The dispatch origin for this call must be _Signed_ by the controller, not the stash.
		///
		/// ## Complexity
		/// - Independent of the arguments. Insignificant complexity.
		/// - Contains one read.
		/// - Writes are limited to the `origin` account key.
		#[pallet::call_index(6)]
		#[pallet::weight(T::WeightInfo::chill())]
		pub fn chill(origin: OriginFor<T>) -> DispatchResult {
			let controller = ensure_signed(origin)?;

			let ledger = Self::ledger(StakingAccount::Controller(controller))?;

			Self::chill_stash(&ledger.stash);
			Ok(())
		}

		/// (Re-)set the payment target for a controller.
		///
		/// Effects will be felt instantly (as soon as this function is completed successfully).
		///
		/// The dispatch origin for this call must be _Signed_ by the controller, not the stash.
		#[pallet::call_index(7)]
		#[pallet::weight(T::WeightInfo::set_payee())]
		pub fn set_payee(
			origin: OriginFor<T>,
			payee: RewardDestination<T::AccountId>,
		) -> DispatchResult {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(Controller(controller.clone()))?;

			ensure!(
				(payee != {
					#[allow(deprecated)]
					RewardDestination::Controller
				}),
				Error::<T>::ControllerDeprecated
			);

			let _ = ledger
				.set_payee(payee)
				.defensive_proof("ledger was retrieved from storage, thus it's bonded; qed.")?;

			Ok(())
		}

		/// (Re-)sets the controller of a stash to the stash itself. This function previously
		/// accepted a `controller` argument to set the controller to an account other than the
		/// stash itself. This functionality has now been removed, now only setting the controller
		/// to the stash, if it is not already.
		///
		/// Effects will be felt instantly (as soon as this function is completed successfully).
		///
		/// The dispatch origin for this call must be _Signed_ by the stash, not the controller.
		#[pallet::call_index(8)]
		#[pallet::weight(T::WeightInfo::set_controller())]
		pub fn set_controller(origin: OriginFor<T>) -> DispatchResult {
			let stash = ensure_signed(origin)?;

			Self::ledger(StakingAccount::Stash(stash.clone())).map(|ledger| {
				let controller = ledger.controller()
                    .defensive_proof("Ledger's controller field didn't exist. The controller should have been fetched using StakingLedger.")
                    .ok_or(Error::<T>::NotController)?;

				if controller == stash {
					// Stash is already its own controller.
					return Err(Error::<T>::AlreadyPaired.into())
				}

				let _ = ledger.set_controller_to_stash()?;
				Ok(())
			})?
		}

		/// Sets the ideal number of validators.
		///
		/// The dispatch origin must be Root.
		#[pallet::call_index(9)]
		#[pallet::weight(T::WeightInfo::set_validator_count())]
		pub fn set_validator_count(
			origin: OriginFor<T>,
			#[pallet::compact] new: u32,
		) -> DispatchResult {
			ensure_root(origin)?;

			ensure!(new <= T::MaxValidatorSet::get(), Error::<T>::TooManyValidators);

			ValidatorCount::<T>::put(new);
			Ok(())
		}

		/// Increments the ideal number of validators up to maximum of
		/// `T::MaxValidatorSet`.
		///
		/// The dispatch origin must be Root.
		#[pallet::call_index(10)]
		#[pallet::weight(T::WeightInfo::set_validator_count())]
		pub fn increase_validator_count(
			origin: OriginFor<T>,
			#[pallet::compact] additional: u32,
		) -> DispatchResult {
			ensure_root(origin)?;
			let old = ValidatorCount::<T>::get();
			let new = old.checked_add(additional).ok_or(ArithmeticError::Overflow)?;

			ensure!(new <= T::MaxValidatorSet::get(), Error::<T>::TooManyValidators);

			ValidatorCount::<T>::put(new);
			Ok(())
		}

		/// Scale up the ideal number of validators by a factor up to maximum of
		/// `T::MaxValidatorSet`.
		///
		/// The dispatch origin must be Root.
		#[pallet::call_index(11)]
		#[pallet::weight(T::WeightInfo::set_validator_count())]
		pub fn scale_validator_count(origin: OriginFor<T>, factor: Percent) -> DispatchResult {
			ensure_root(origin)?;
			let old = ValidatorCount::<T>::get();
			let new = old.checked_add(factor.mul_floor(old)).ok_or(ArithmeticError::Overflow)?;

			ensure!(new <= T::MaxValidatorSet::get(), Error::<T>::TooManyValidators);

			ValidatorCount::<T>::put(new);
			Ok(())
		}

		/// Force there to be no new eras indefinitely.
		///
		/// The dispatch origin must be Root.
		///
		/// # Warning
		///
		/// The election process starts multiple blocks before the end of the era.
		/// Thus the election process may be ongoing when this is called. In this case the
		/// election will continue until the next era is triggered.
		#[pallet::call_index(12)]
		#[pallet::weight(T::WeightInfo::force_no_eras())]
		pub fn force_no_eras(origin: OriginFor<T>) -> DispatchResult {
			ensure_root(origin)?;
			Self::set_force_era(Forcing::ForceNone);
			Ok(())
		}

		/// Force there to be a new era at the end of the next session. After this, it will be
		/// reset to normal (non-forced) behaviour.
		///
		/// The dispatch origin must be Root.
		///
		/// # Warning
		///
		/// The election process starts multiple blocks before the end of the era.
		/// If this is called just before a new era is triggered, the election process may not
		/// have enough blocks to get a result.
		#[pallet::call_index(13)]
		#[pallet::weight(T::WeightInfo::force_new_era())]
		pub fn force_new_era(origin: OriginFor<T>) -> DispatchResult {
			ensure_root(origin)?;
			Self::set_force_era(Forcing::ForceNew);
			Ok(())
		}

		/// Force a current staker to become completely unstaked, immediately.
		///
		/// The dispatch origin must be Root.
		/// ## Parameters
		///
		/// - `stash`: The stash account to be unstaked.
		/// - `num_slashing_spans`: **Deprecated**. This parameter is retained for backward
		/// compatibility. It no longer has any effect.
		#[pallet::call_index(15)]
		#[pallet::weight(T::WeightInfo::force_unstake())]
		pub fn force_unstake(
			origin: OriginFor<T>,
			stash: T::AccountId,
			_num_slashing_spans: u32,
		) -> DispatchResult {
			ensure_root(origin)?;

			// Remove all staking-related information and lock.
			Self::kill_stash(&stash)?;

			Ok(())
		}

		/// Force there to be a new era at the end of sessions indefinitely.
		///
		/// The dispatch origin must be Root.
		///
		/// # Warning
		///
		/// The election process starts multiple blocks before the end of the era.
		/// If this is called just before a new era is triggered, the election process may not
		/// have enough blocks to get a result.
		#[pallet::call_index(16)]
		#[pallet::weight(T::WeightInfo::force_new_era_always())]
		pub fn force_new_era_always(origin: OriginFor<T>) -> DispatchResult {
			ensure_root(origin)?;
			Self::set_force_era(Forcing::ForceAlways);
			Ok(())
		}

		/// Cancels scheduled slashes for a given era before they are applied.
		///
		/// This function allows `T::AdminOrigin` to cancel pending slashes for specified validators
		/// in a given era. The cancelled slashes are stored and will be checked when applying
		/// slashes.
		///
		/// ## Parameters
		/// - `era`: The staking era for which slashes should be cancelled. This is the era where
		///   the slash would be applied, not the era in which the offence was committed.
		/// - `validator_slashes`: A list of validator stash accounts and their slash fractions to
		///   be cancelled.
		#[pallet::call_index(17)]
		#[pallet::weight(T::WeightInfo::cancel_deferred_slash(validator_slashes.len() as u32))]
		pub fn cancel_deferred_slash(
			origin: OriginFor<T>,
			era: EraIndex,
			validator_slashes: Vec<(T::AccountId, Perbill)>,
		) -> DispatchResult {
			T::AdminOrigin::ensure_origin(origin)?;
			ensure!(!validator_slashes.is_empty(), Error::<T>::EmptyTargets);

			// Get current cancelled slashes for this era
			let mut cancelled_slashes = CancelledSlashes::<T>::get(&era);

			// Process each validator slash
			for (validator, slash_fraction) in validator_slashes {
				// Since this is gated by admin origin, we don't need to check if they are really
				// validators and trust governance to correctly set the parameters.

				// Remove any existing entry for this validator
				cancelled_slashes.retain(|(v, _)| v != &validator);

				// Add the validator with the specified slash fraction
				cancelled_slashes
					.try_push((validator.clone(), slash_fraction))
					.map_err(|_| Error::<T>::BoundNotMet)
					.defensive_proof("cancelled_slashes should have capacity for all validators")?;

				Self::deposit_event(Event::<T>::SlashCancelled { slash_era: era, validator });
			}

			// Update storage
			CancelledSlashes::<T>::insert(&era, cancelled_slashes);

			Ok(())
		}

		/// Pay out next page of the stakers behind a validator for the given era.
		///
		/// - `validator_stash` is the stash account of the validator.
		/// - `era` may be any era between `[current_era - history_depth; current_era]`.
		///
		/// The origin of this call must be _Signed_. Any account can call this function, even if
		/// it is not one of the stakers.
		///
		/// The reward payout could be paged in case there are too many nominators backing the
		/// `validator_stash`. This call will payout unpaid pages in an ascending order. To claim a
		/// specific page, use `payout_stakers_by_page`.`
		///
		/// If all pages are claimed, it returns an error `InvalidPage`.
		#[pallet::call_index(18)]
		#[pallet::weight(T::WeightInfo::payout_stakers_alive_staked(T::MaxExposurePageSize::get()))]
		pub fn payout_stakers(
			origin: OriginFor<T>,
			validator_stash: T::AccountId,
			era: EraIndex,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;

			Self::do_payout_stakers(validator_stash, era)
		}

		/// Rebond a portion of the stash scheduled to be unlocked.
		///
		/// The dispatch origin must be signed by the controller.
		#[pallet::call_index(19)]
		#[pallet::weight(T::WeightInfo::rebond(T::MaxUnlockingChunks::get() as u32))]
		pub fn rebond(
			origin: OriginFor<T>,
			#[pallet::compact] value: BalanceOf<T>,
		) -> DispatchResultWithPostInfo {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(Controller(controller))?;

			ensure!(!T::Filter::contains(&ledger.stash), Error::<T>::Restricted);
			ensure!(!ledger.unlocking.is_empty(), Error::<T>::NoUnlockChunk);

			let initial_unlocking = ledger.unlocking.len() as u32;
			let (ledger, rebonded_value) = ledger.rebond(value);
			// Last check: the new active amount of ledger must be more than min bond.
			ensure!(ledger.active >= Self::min_chilled_bond(), Error::<T>::InsufficientBond);

			Self::deposit_event(Event::<T>::Bonded {
				stash: ledger.stash.clone(),
				amount: rebonded_value,
			});

			let stash = ledger.stash.clone();
			let final_unlocking = ledger.unlocking.len();

			// NOTE: ledger must be updated prior to calling `Self::weight_of`.
			ledger.update()?;
			if T::VoterList::contains(&stash) {
				let _ = T::VoterList::on_update(&stash, Self::weight_of(&stash));
			}

			let removed_chunks = 1u32 // for the case where the last iterated chunk is not removed
				.saturating_add(initial_unlocking)
				.saturating_sub(final_unlocking as u32);
			Ok(Some(T::WeightInfo::rebond(removed_chunks)).into())
		}

		/// Remove all data structures concerning a staker/stash once it is at a state where it can
		/// be considered `dust` in the staking system. The requirements are:
		///
		/// 1. the `total_balance` of the stash is below the existential deposit.
		/// 2. or, the `ledger.total` of the stash is below the existential deposit.
		///
		/// The former can happen in cases like a slash; the latter when a fully unbonded account
		/// is still receiving staking rewards in `RewardDestination::Staked`.
		///
		/// The gate is intentionally the existential deposit and *not* `min_chilled_bond`: a
		/// governance change to `MinValidatorBond` / `MinNominatorBond` must not turn previously
		/// safe stashes into permissionlessly reapable ones. Accounts that fall below the new
		/// minimums after such a change should be `chill_other`-ed (which has a density gate and
		/// does not destroy the ledger), not reaped.
		///
		/// It can be called by anyone, as long as `stash` meets the above requirements.
		///
		/// Refunds the transaction fees upon successful execution.
		///
		/// ## Parameters
		///
		/// - `stash`: The stash account to be reaped.
		/// - `num_slashing_spans`: **Deprecated**. This parameter is retained for backward
		/// compatibility. It no longer has any effect.
		#[pallet::call_index(20)]
		#[pallet::weight(T::WeightInfo::reap_stash())]
		pub fn reap_stash(
			origin: OriginFor<T>,
			stash: T::AccountId,
			_num_slashing_spans: u32,
		) -> DispatchResultWithPostInfo {
			let _ = ensure_signed(origin)?;

			// virtual stakers should not be allowed to be reaped.
			ensure!(!Self::is_virtual_staker(&stash), Error::<T>::VirtualStakerNotAllowed);

			let ed = asset::existential_deposit::<T>();
			let origin_balance = asset::total_balance::<T>(&stash);
			let ledger_total =
				Self::ledger(Stash(stash.clone())).map(|l| l.total).unwrap_or_default();
			let reapable = origin_balance < ed ||
				origin_balance.is_zero() ||
				ledger_total < ed ||
				ledger_total.is_zero();
			ensure!(reapable, Error::<T>::FundedTarget);

			// Remove all staking-related information and lock.
			Self::kill_stash(&stash)?;

			Ok(Pays::No.into())
		}

		/// Remove the given nominations from the calling validator.
		///
		/// Effects will be felt at the beginning of the next era.
		///
		/// The dispatch origin for this call must be _Signed_ by the controller, not the stash.
		///
		/// - `who`: A list of nominator stash accounts who are nominating this validator which
		///   should no longer be nominating this validator.
		///
		/// Note: Making this call only makes sense if you first set the validator preferences to
		/// block any further nominations.
		#[pallet::call_index(21)]
		#[pallet::weight(T::WeightInfo::kick(who.len() as u32))]
		pub fn kick(origin: OriginFor<T>, who: Vec<AccountIdLookupOf<T>>) -> DispatchResult {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(Controller(controller))?;
			let stash = &ledger.stash;

			for nom_stash in who
				.into_iter()
				.map(T::Lookup::lookup)
				.collect::<Result<Vec<T::AccountId>, _>>()?
				.into_iter()
			{
				Nominators::<T>::mutate(&nom_stash, |maybe_nom| {
					if let Some(ref mut nom) = maybe_nom {
						if let Some(pos) = nom.targets.iter().position(|v| v == stash) {
							nom.targets.swap_remove(pos);
							Self::deposit_event(Event::<T>::Kicked {
								nominator: nom_stash.clone(),
								stash: stash.clone(),
							});
						}
					}
				});
			}

			Ok(())
		}

		/// Update the various staking configurations .
		///
		/// * `min_nominator_bond`: The minimum active bond needed to be a nominator.
		/// * `min_validator_bond`: The minimum active bond needed to be a validator.
		/// * `max_nominator_count`: The max number of users who can be a nominator at once. When
		///   set to `None`, no limit is enforced.
		/// * `max_validator_count`: The max number of users who can be a validator at once. When
		///   set to `None`, no limit is enforced.
		/// * `chill_threshold`: The ratio of `max_nominator_count` or `max_validator_count` which
		///   should be filled in order for the `chill_other` transaction to work.
		/// * `min_commission`: The minimum amount of commission that each validators must maintain.
		///   This is checked only upon calling `validate`. Existing validators are not affected.
		/// * `chill_inactive_threshold`: The number of eras a validator can remain inactive during
		///   the last [`Config::HistoryDepth`] eras before being subject to chilling becuase of
		///   inactivity.
		///
		/// RuntimeOrigin must be Root to call this function.
		///
		/// NOTE: Existing nominators and validators will not be affected by this update.
		/// to kick people under the new limits, `chill_other` should be called.
		// We assume the worst case for this call is either: all items are set or all items are
		// removed.
		#[pallet::call_index(22)]
		#[pallet::weight(
			T::WeightInfo::set_staking_configs_all_set()
				.max(T::WeightInfo::set_staking_configs_all_remove())
		)]
		pub fn set_staking_configs(
			origin: OriginFor<T>,
			min_nominator_bond: ConfigOp<BalanceOf<T>>,
			min_validator_bond: ConfigOp<BalanceOf<T>>,
			max_nominator_count: ConfigOp<u32>,
			max_validator_count: ConfigOp<u32>,
			chill_threshold: ConfigOp<Percent>,
			min_commission: ConfigOp<Perbill>,
			max_staked_rewards: ConfigOp<Percent>,
			are_nominators_slashable: ConfigOp<bool>,
			chill_inactive_threshold: ConfigOp<u32>,
		) -> DispatchResult {
			ensure_root(origin)?;

			if let ConfigOp::Set(threshold) = chill_inactive_threshold {
				ensure!(
					threshold > 1 && threshold <= T::HistoryDepth::get(),
					Error::<T>::InvalidChillInactiveThreshold
				);
			}

			macro_rules! config_op_exp {
				($storage:ty, $op:ident) => {
					match $op {
						ConfigOp::Noop => (),
						ConfigOp::Set(v) => <$storage>::put(v),
						ConfigOp::Remove => <$storage>::kill(),
					}
				};
			}

			config_op_exp!(MinNominatorBond<T>, min_nominator_bond);
			config_op_exp!(MinValidatorBond<T>, min_validator_bond);
			config_op_exp!(MaxNominatorsCount<T>, max_nominator_count);
			config_op_exp!(MaxValidatorsCount<T>, max_validator_count);
			config_op_exp!(ChillThreshold<T>, chill_threshold);
			config_op_exp!(MinCommission<T>, min_commission);
			config_op_exp!(MaxStakedRewards<T>, max_staked_rewards);
			config_op_exp!(AreNominatorsSlashable<T>, are_nominators_slashable);
			config_op_exp!(ChillInactiveThreshold<T>, chill_inactive_threshold);

			Ok(())
		}
		/// Declare a `controller` to stop participating as either a validator or nominator.
		///
		/// Effects will be felt at the beginning of the next era.
		///
		/// The dispatch origin for this call must be _Signed_, but can be called by anyone.
		///
		/// If the caller is the same as the controller being targeted, then no further checks are
		/// enforced, and this function behaves just like `chill`.
		///
		/// If the caller is different than the controller being targeted, the following conditions
		/// must be met:
		///
		/// * `controller` must belong to a nominator who has become non-decodable,
		///
		/// Or:
		///
		/// * A `ChillThreshold` must be set and checked which defines how close to the max
		///   nominators or validators we must reach before users can start chilling one-another.
		/// * A `MaxNominatorCount` and `MaxValidatorCount` must be set which is used to determine
		///   how close we are to the threshold.
		/// * A `MinNominatorBond` and `MinValidatorBond` must be set and checked, which determines
		///   if this is a person that should be chilled because they have not met the threshold
		///   bond required.
		///
		/// This can be helpful if bond requirements are updated, and we need to remove old users
		/// who do not satisfy these requirements.
		#[pallet::call_index(23)]
		#[pallet::weight(T::WeightInfo::chill_other())]
		pub fn chill_other(origin: OriginFor<T>, stash: T::AccountId) -> DispatchResult {
			// Anyone can call this function.
			let caller = ensure_signed(origin)?;
			let ledger = Self::ledger(Stash(stash.clone()))?;
			let controller = ledger
				.controller()
				.defensive_proof(
					"Ledger's controller field didn't exist. The controller should have been fetched using StakingLedger.",
				)
				.ok_or(Error::<T>::NotController)?;

			// In order for one user to chill another user, the following conditions must be met:
			//
			// * `controller` belongs to a nominator who has become non-decodable,
			//
			// Or
			//
			// * A `ChillThreshold` is set which defines how close to the max nominators or
			//   validators we must reach before users can start chilling one-another.
			// * A `MaxNominatorCount` and `MaxValidatorCount` which is used to determine how close
			//   we are to the threshold.
			// * A `MinNominatorBond` and `MinValidatorBond` which is the final condition checked to
			//   determine this is a person that should be chilled because they have not met the
			//   threshold bond required.
			//
			// Otherwise, if caller is the same as the controller, this is just like `chill`.

			if Nominators::<T>::contains_key(&stash) && Nominators::<T>::get(&stash).is_none() {
				Self::chill_stash(&stash);
				return Ok(());
			}

			if caller != controller {
				let threshold = ChillThreshold::<T>::get().ok_or(Error::<T>::CannotChillOther)?;
				let min_active_bond = if Nominators::<T>::contains_key(&stash) {
					let max_nominator_count =
						MaxNominatorsCount::<T>::get().ok_or(Error::<T>::CannotChillOther)?;
					let current_nominator_count = Nominators::<T>::count();
					ensure!(
						threshold * max_nominator_count < current_nominator_count,
						Error::<T>::CannotChillOther
					);
					Self::min_nominator_bond()
				} else if Validators::<T>::contains_key(&stash) {
					let max_validator_count =
						MaxValidatorsCount::<T>::get().ok_or(Error::<T>::CannotChillOther)?;
					let current_validator_count = Validators::<T>::count();
					ensure!(
						threshold * max_validator_count < current_validator_count,
						Error::<T>::CannotChillOther
					);
					Self::min_validator_bond()
				} else {
					Zero::zero()
				};

				ensure!(ledger.active < min_active_bond, Error::<T>::CannotChillOther);
			}

			Self::chill_stash(&stash);
			Ok(())
		}

		/// Clamps a validator's commission to the `[MinCommission, MaxCommission]` range.
		///
		/// Named `force_apply_min_commission` for legacy reasons — it also enforces the
		/// maximum. Any account can call this.
		#[pallet::call_index(24)]
		#[pallet::weight(T::WeightInfo::force_apply_min_commission())]
		pub fn force_apply_min_commission(
			origin: OriginFor<T>,
			validator_stash: T::AccountId,
		) -> DispatchResult {
			ensure_signed(origin)?;
			let min_commission = MinCommission::<T>::get();
			let max_commission = MaxCommission::<T>::get();
			Validators::<T>::try_mutate_exists(validator_stash, |maybe_prefs| {
				maybe_prefs
					.as_mut()
					.map(|prefs| {
						if prefs.commission < min_commission {
							prefs.commission = min_commission;
						}
						if prefs.commission > max_commission {
							prefs.commission = max_commission;
						}
					})
					.ok_or(Error::<T>::NotStash)
			})?;
			Ok(())
		}

		/// Sets the minimum amount of commission that each validators must maintain.
		///
		/// This call has lower privilege requirements than `set_staking_config` and can be called
		/// by the `T::AdminOrigin`. Root can always call this.
		#[pallet::call_index(25)]
		#[pallet::weight(T::WeightInfo::set_min_commission())]
		pub fn set_min_commission(origin: OriginFor<T>, new: Perbill) -> DispatchResult {
			T::AdminOrigin::ensure_origin(origin)?;
			ensure!(new <= MaxCommission::<T>::get(), Error::<T>::CommissionTooHigh);
			MinCommission::<T>::put(new);
			Ok(())
		}

		/// Pay out a page of the stakers behind a validator for the given era and page.
		///
		/// - `validator_stash` is the stash account of the validator.
		/// - `era` may be any era between `[current_era - history_depth; current_era]`.
		/// - `page` is the page index of nominators to pay out with value between 0 and
		///   `num_nominators / T::MaxExposurePageSize`.
		///
		/// The origin of this call must be _Signed_. Any account can call this function, even if
		/// it is not one of the stakers.
		///
		/// If a validator has more than [`Config::MaxExposurePageSize`] nominators backing
		/// them, then the list of nominators is paged, with each page being capped at
		/// [`Config::MaxExposurePageSize`]. If a validator has more than one page of nominators,
		/// the call needs to be made for each page separately in order for all the nominators
		/// backing a validator to receive the reward. The nominators are not sorted across pages
		/// and so it should not be assumed the highest staker would be on the topmost page and vice
		/// versa. If rewards are not claimed in [`Config::HistoryDepth`] eras, they are lost.
		///
		/// The validator's own reward (commission + own-stake share) is prorated across pages
		/// proportional to each page's stake. The full validator reward is the sum across all
		/// pages.
		#[pallet::call_index(26)]
		#[pallet::weight(T::WeightInfo::payout_stakers_alive_staked(T::MaxExposurePageSize::get()))]
		pub fn payout_stakers_by_page(
			origin: OriginFor<T>,
			validator_stash: T::AccountId,
			era: EraIndex,
			page: Page,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;
			Self::do_payout_stakers_by_page(validator_stash, era, page)
		}

		/// Migrates an account's `RewardDestination::Controller` to
		/// `RewardDestination::Account(controller)`.
		///
		/// Effects will be felt instantly (as soon as this function is completed successfully).
		///
		/// This will waive the transaction fee if the `payee` is successfully migrated.
		#[pallet::call_index(27)]
		#[pallet::weight(T::WeightInfo::update_payee())]
		pub fn update_payee(
			origin: OriginFor<T>,
			controller: T::AccountId,
		) -> DispatchResultWithPostInfo {
			let _ = ensure_signed(origin)?;
			let ledger = Self::ledger(StakingAccount::Controller(controller.clone()))?;

			ensure!(
				(Payee::<T>::get(&ledger.stash) == {
					#[allow(deprecated)]
					Some(RewardDestination::Controller)
				}),
				Error::<T>::NotController
			);

			let _ = ledger
				.set_payee(RewardDestination::Account(controller))
				.defensive_proof("ledger should have been previously retrieved from storage.")?;

			Ok(Pays::No.into())
		}

		/// Updates a batch of controller accounts to their corresponding stash account if they are
		/// not the same. Ignores any controller accounts that do not exist, and does not operate if
		/// the stash and controller are already the same.
		///
		/// Effects will be felt instantly (as soon as this function is completed successfully).
		///
		/// The dispatch origin must be `T::AdminOrigin`.
		#[pallet::call_index(28)]
		#[pallet::weight(T::WeightInfo::deprecate_controller_batch(controllers.len() as u32))]
		pub fn deprecate_controller_batch(
			origin: OriginFor<T>,
			controllers: BoundedVec<T::AccountId, T::MaxControllersInDeprecationBatch>,
		) -> DispatchResultWithPostInfo {
			T::AdminOrigin::ensure_origin(origin)?;

			// Ignore controllers that do not exist or are already the same as stash.
			let filtered_batch_with_ledger: Vec<_> = controllers
				.iter()
				.filter_map(|controller| {
					let ledger = Self::ledger(StakingAccount::Controller(controller.clone()));
					ledger.ok().map_or(None, |ledger| {
						// If the controller `RewardDestination` is still the deprecated
						// `Controller` variant, skip deprecating this account.
						let payee_deprecated = Payee::<T>::get(&ledger.stash) == {
							#[allow(deprecated)]
							Some(RewardDestination::Controller)
						};

						if ledger.stash != *controller && !payee_deprecated {
							Some(ledger)
						} else {
							None
						}
					})
				})
				.collect();

			// Update unique pairs.
			let mut failures = 0;
			for ledger in filtered_batch_with_ledger {
				let _ = ledger.clone().set_controller_to_stash().map_err(|_| failures += 1);
			}
			Self::deposit_event(Event::<T>::ControllerBatchDeprecated { failures });

			Ok(Some(T::WeightInfo::deprecate_controller_batch(controllers.len() as u32)).into())
		}

		/// Restores the state of a ledger which is in an inconsistent state.
		///
		/// The requirements to restore a ledger are the following:
		/// * The stash is bonded; or
		/// * The stash is not bonded but it has a staking lock left behind; or
		/// * If the stash has an associated ledger and its state is inconsistent; or
		/// * If the ledger is not corrupted *but* its staking lock is out of sync.
		///
		/// The `maybe_*` input parameters will overwrite the corresponding data and metadata of the
		/// ledger associated with the stash. If the input parameters are not set, the ledger will
		/// be reset values from on-chain state.
		#[pallet::call_index(29)]
		#[pallet::weight(T::WeightInfo::restore_ledger())]
		pub fn restore_ledger(
			origin: OriginFor<T>,
			stash: T::AccountId,
			maybe_controller: Option<T::AccountId>,
			maybe_total: Option<BalanceOf<T>>,
			maybe_unlocking: Option<BoundedVec<UnlockChunk<BalanceOf<T>>, T::MaxUnlockingChunks>>,
		) -> DispatchResult {
			T::AdminOrigin::ensure_origin(origin)?;

			// cannot restore ledger for virtual stakers.
			ensure!(!Self::is_virtual_staker(&stash), Error::<T>::VirtualStakerNotAllowed);

			let current_lock = asset::staked::<T>(&stash);
			let stash_balance = asset::stakeable_balance::<T>(&stash);

			let (new_controller, new_total) = match Self::inspect_bond_state(&stash) {
				Ok(LedgerIntegrityState::Corrupted) => {
					let new_controller = maybe_controller.unwrap_or(stash.clone());

					let new_total = if let Some(total) = maybe_total {
						let new_total = total.min(stash_balance);
						// enforce hold == ledger.amount.
						asset::update_stake::<T>(&stash, new_total)?;
						new_total
					} else {
						current_lock
					};

					Ok((new_controller, new_total))
				},
				Ok(LedgerIntegrityState::CorruptedKilled) => {
					if current_lock == Zero::zero() {
						// this case needs to restore both lock and ledger, so the new total needs
						// to be given by the called since there's no way to restore the total
						// on-chain.
						ensure!(maybe_total.is_some(), Error::<T>::CannotRestoreLedger);
						Ok((
							stash.clone(),
							maybe_total.expect("total exists as per the check above; qed."),
						))
					} else {
						Ok((stash.clone(), current_lock))
					}
				},
				Ok(LedgerIntegrityState::LockCorrupted) => {
					// ledger is not corrupted but its locks are out of sync. In this case, we need
					// to enforce a new ledger.total and staking lock for this stash.
					let new_total =
						maybe_total.ok_or(Error::<T>::CannotRestoreLedger)?.min(stash_balance);
					asset::update_stake::<T>(&stash, new_total)?;

					Ok((stash.clone(), new_total))
				},
				Err(Error::<T>::BadState) => {
					// the stash and ledger do not exist but lock is lingering.
					asset::kill_stake::<T>(&stash)?;
					ensure!(
						Self::inspect_bond_state(&stash) == Err(Error::<T>::NotStash),
						Error::<T>::BadState
					);

					return Ok(());
				},
				Ok(LedgerIntegrityState::Ok) | Err(_) => Err(Error::<T>::CannotRestoreLedger),
			}?;

			// re-bond stash and controller tuple.
			Bonded::<T>::insert(&stash, &new_controller);

			// resoter ledger state.
			let mut ledger = StakingLedger::<T>::new(stash.clone(), new_total);
			ledger.controller = Some(new_controller);
			ledger.unlocking = maybe_unlocking.unwrap_or_default();
			ledger.update()?;

			ensure!(
				Self::inspect_bond_state(&stash) == Ok(LedgerIntegrityState::Ok),
				Error::<T>::BadState
			);
			Ok(())
		}

		/// Migrates permissionlessly a stash from locks to holds.
		///
		/// This removes the old lock on the stake and creates a hold on it atomically. If all
		/// stake cannot be held, the best effort is made to hold as much as possible. The remaining
		/// stake is removed from the ledger.
		///
		/// The fee is waived if the migration is successful.
		#[pallet::call_index(30)]
		#[pallet::weight(T::WeightInfo::migrate_currency())]
		pub fn migrate_currency(
			origin: OriginFor<T>,
			stash: T::AccountId,
		) -> DispatchResultWithPostInfo {
			let _ = ensure_signed(origin)?;
			Self::do_migrate_currency(&stash)?;

			// Refund the transaction fee if successful.
			Ok(Pays::No.into())
		}

		/// Manually and permissionlessly applies a deferred slash for a given era.
		///
		/// Normally, slashes are automatically applied shortly after the start of the `slash_era`.
		/// The automatic application of slashes is handled by the pallet's internal logic, and it
		/// tries to apply one slash page per block of the era.
		/// If for some reason, one era is not enough for applying all slash pages, the remaining
		/// slashes need to be manually (permissionlessly) applied.
		///
		/// For a given era x, if at era x+1, slashes are still unapplied, all withdrawals get
		/// blocked, and these need to be manually applied by calling this function.
		/// This function exists as a **fallback mechanism** for this extreme situation, but we
		/// never expect to encounter this in normal scenarios.
		///
		/// The parameters for this call can be queried by looking at the `UnappliedSlashes` storage
		/// for eras older than the active era.
		///
		/// ## Parameters
		/// - `slash_era`: The application era (`offence_era + SlashDeferDuration`), i.e. the key
		///   into [`UnappliedSlashes`].
		/// - `slash_key`: A unique identifier for the slash, represented as a tuple:
		///   - `stash`: The stash account of the validator being slashed.
		///   - `slash_fraction`: The fraction of the stake that was slashed.
		///   - `page_index`: The index of the exposure page being processed.
		///
		/// ## Behavior
		/// - The function is **permissionless**—anyone can call it.
		/// - The `slash_era` **must be the current era or a past era**.
		/// If it is in the future, the
		///   call fails with `EraNotStarted`.
		/// - The fee is waived if the slash is successfully applied.
		///
		/// ## Future Improvement
		/// - Implement an **off-chain worker (OCW) task** to automatically apply slashes when there
		///   is unused block space, improving efficiency.
		#[pallet::call_index(31)]
		#[pallet::weight(T::WeightInfo::apply_slash(T::MaxExposurePageSize::get()))]
		pub fn apply_slash(
			origin: OriginFor<T>,
			slash_era: EraIndex,
			slash_key: (T::AccountId, Perbill, u32),
		) -> DispatchResultWithPostInfo {
			let _ = ensure_signed(origin)?;
			let active_era = ActiveEra::<T>::get().map(|a| a.index).unwrap_or_default();
			ensure!(slash_era <= active_era, Error::<T>::EraNotStarted);

			// Check if this slash has been cancelled
			ensure!(
				!Self::check_slash_cancelled(slash_era, &slash_key.0, slash_key.1),
				Error::<T>::CancelledSlash
			);

			let unapplied_slash = UnappliedSlashes::<T>::take(&slash_era, &slash_key)
				.ok_or(Error::<T>::InvalidSlashRecord)?;
			slashing::apply_slash::<T>(unapplied_slash, Self::offence_era_of(slash_era));

			Ok(Pays::No.into())
		}

		/// Perform one step of era pruning to prevent PoV size exhaustion from unbounded deletions.
		///
		/// This extrinsic enables permissionless lazy pruning of era data by performing
		/// incremental deletion of storage items. Each call processes a limited number
		/// of items based on available block weight to avoid exceeding block limits.
		///
		/// Returns `Pays::No` when work is performed to incentivize regular maintenance.
		/// Anyone can call this to help maintain the chain's storage health.
		///
		/// The era must be eligible for pruning (older than HistoryDepth + 1).
		/// Check `EraPruningState` storage to see if an era needs pruning before calling.
		#[pallet::call_index(32)]
		// NOTE: as pre-dispatch weight, use the maximum of all possible pruning step weights
		#[pallet::weight({
			let v = T::MaxValidatorSet::get();
			T::WeightInfo::prune_era_stakers_paged(v)
				.max(T::WeightInfo::prune_era_stakers_overview(v))
				.max(T::WeightInfo::prune_era_validator_prefs(v))
				.max(T::WeightInfo::prune_era_claimed_rewards(v))
				.max(T::WeightInfo::prune_era_validator_reward())
				.max(T::WeightInfo::prune_era_reward_points())
				.max(T::WeightInfo::prune_era_single_entry_cleanups())
				.max(T::WeightInfo::prune_era_validator_slash_in_era(v))
				.max(T::WeightInfo::prune_era_validator_incentive_weight(v))
		})]
		pub fn prune_era_step(origin: OriginFor<T>, era: EraIndex) -> DispatchResultWithPostInfo {
			let _ = ensure_signed(origin)?;

			// Verify era is eligible for pruning: era <= active_era - history_depth - 1
			let active_era = crate::session_rotation::Rotator::<T>::active_era();
			let history_depth = T::HistoryDepth::get();
			let earliest_prunable_era = active_era.saturating_sub(history_depth).saturating_sub(1);
			ensure!(era <= earliest_prunable_era, Error::<T>::EraNotPrunable);

			let actual_weight = Self::do_prune_era_step(era)?;

			Ok(frame_support::dispatch::PostDispatchInfo {
				actual_weight: Some(actual_weight),
				pays_fee: frame_support::dispatch::Pays::No,
			})
		}

		/// Sets the maximum commission that validators can set.
		///
		/// The dispatch origin must be `T::AdminOrigin`.
		#[pallet::call_index(33)]
		#[pallet::weight(T::WeightInfo::set_max_commission())]
		pub fn set_max_commission(origin: OriginFor<T>, new: Perbill) -> DispatchResult {
			T::AdminOrigin::ensure_origin(origin)?;
			ensure!(new >= MinCommission::<T>::get(), Error::<T>::CommissionTooLow);
			MaxCommission::<T>::put(new);
			Ok(())
		}

		/// Configure the validator self-stake incentive parameters.
		///
		/// The dispatch origin must be `T::AdminOrigin`.
		///
		/// Changes take effect in the next era when rewards are calculated.
		#[pallet::call_index(34)]
		#[pallet::weight(T::WeightInfo::set_validator_self_stake_incentive_config())]
		pub fn set_validator_self_stake_incentive_config(
			origin: OriginFor<T>,
			optimum_self_stake: ConfigOp<BalanceOf<T>>,
			hard_cap_self_stake: ConfigOp<BalanceOf<T>>,
			self_stake_slope_factor: ConfigOp<Perbill>,
		) -> DispatchResult {
			T::AdminOrigin::ensure_origin(origin)?;

			let new_optimum = match optimum_self_stake {
				ConfigOp::Noop => OptimumSelfStake::<T>::get(),
				ConfigOp::Set(v) => v,
				ConfigOp::Remove => BalanceOf::<T>::zero(),
			};

			let new_cap = match hard_cap_self_stake {
				ConfigOp::Noop => HardCapSelfStake::<T>::get(),
				ConfigOp::Set(v) => v,
				ConfigOp::Remove => BalanceOf::<T>::zero(),
			};

			ensure!(new_optimum <= new_cap, Error::<T>::OptimumGreaterThanCap);

			let has_changes = !matches!(
				(&optimum_self_stake, &hard_cap_self_stake, &self_stake_slope_factor),
				(ConfigOp::Noop, ConfigOp::Noop, ConfigOp::Noop)
			);

			macro_rules! config_op_exp {
				($storage:ty, $op:ident) => {
					match $op {
						ConfigOp::Noop => (),
						ConfigOp::Set(v) => <$storage>::put(v),
						ConfigOp::Remove => <$storage>::kill(),
					}
				};
			}

			config_op_exp!(OptimumSelfStake<T>, optimum_self_stake);
			config_op_exp!(HardCapSelfStake<T>, hard_cap_self_stake);
			config_op_exp!(SelfStakeSlopeFactor<T>, self_stake_slope_factor);

			if has_changes {
				Self::deposit_event(Event::<T>::ValidatorIncentiveConfigSet {
					optimum_self_stake: OptimumSelfStake::<T>::get(),
					hard_cap_self_stake: HardCapSelfStake::<T>::get(),
					slope_factor: SelfStakeSlopeFactor::<T>::get(),
				});
			}

			Ok(())
		}

		/// Chill an inactive validator.
		///
		/// This extrinsic can be called by anyone, given that the valid inactivity proof is
		/// provided. Inactivity proof is a vector of eras where the given validator was inactive.
		///
		/// Requirements for the inactivity proof:
		/// - The length must be equal to [`ChillInactiveThreshold`].
		/// - It must be sorted in ascending order.
		/// - It must not contain duplicate entries.
		/// - For every era the `stash` account must be exposed.
		/// - Every item must pass the check provided by [`Config::IsValidatorInactive`].
		/// - Every era must be less than the active era.
		///
		/// On a successfull execution, caller doesn't pay fees.
		#[pallet::call_index(35)]
		#[pallet::weight(T::WeightInfo::chill_inactive(proof.len() as _))]
		pub fn chill_inactive(
			origin: OriginFor<T>,
			stash: T::AccountId,
			proof: BoundedVec<EraIndex, T::HistoryDepth>,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;

			let threshold = ChillInactiveThreshold::<T>::get();
			ensure!(
				proof.len() as EraIndex == threshold,
				Error::<T>::InvalidInactivityProof(InvalidInactivityProofError::InvalidLen)
			);
			ensure!(
				proof.is_sorted_by(|a, b| a < b),
				Error::<T>::InvalidInactivityProof(InvalidInactivityProofError::NotSorted)
			);

			// All proof eras must fall in the retained window `[active_era - HistoryDepth,
			// active_era)`.
			let active_era = Rotator::<T>::active_era();
			let oldest_allowed_era = active_era.saturating_sub(T::HistoryDepth::get());
			let oldest_proof_era = proof.first().copied().unwrap_or(EraIndex::MAX);
			let most_recent_proof_era = proof.last().copied().unwrap_or(EraIndex::MAX);
			ensure!(
				oldest_proof_era >= oldest_allowed_era && most_recent_proof_era < active_era,
				Error::<T>::InvalidInactivityProof(InvalidInactivityProofError::InvalidEra)
			);

			for era in proof {
				ensure!(
					Eras::<T>::was_validator_exposed(era, &stash),
					Error::<T>::InvalidInactivityProof(
						InvalidInactivityProofError::ValidatorNotExposed
					)
				);

				let points = Eras::<T>::get_reward_points_for_validator(era, &stash);

				ensure!(
					T::IsValidatorInactive::is_inactive(era, &stash, points),
					Error::<T>::InvalidInactivityProof(
						InvalidInactivityProofError::ValidatorActive
					)
				);
			}

			if Self::do_remove_validator(&stash) {
				Self::deposit_event(Event::<T>::Chilled { stash });

				Ok(Pays::No.into())
			} else {
				Err(Error::<T>::BadTarget.into())
			}
		}
	}

	#[pallet::view_functions]
	impl<T: Config> Pallet<T> {
		/// Resolve the account ID for a given reward pot.
		pub fn pot_account(pot: crate::RewardPot) -> T::AccountId {
			<T::RewardPots as crate::PotAccountProvider<T::AccountId>>::pot_account(pot)
		}

		/// Current balance held in a given reward pot.
		pub fn pot_balance(pot: crate::RewardPot) -> BalanceOf<T> {
			let account =
				<T::RewardPots as crate::PotAccountProvider<T::AccountId>>::pot_account(pot);
			<T::Currency as frame_support::traits::fungible::Inspect<T::AccountId>>::balance(
				&account,
			)
		}

		/// Per-era reward allocation (staker rewards + validator incentive budget).
		///
		/// Both fields are zero for eras created in legacy minting mode.
		pub fn era_reward_allocation(
			era: EraIndex,
		) -> crate::reward::EraRewardAllocation<BalanceOf<T>> {
			crate::reward::EraRewardAllocation {
				staker_rewards: ErasValidatorReward::<T>::get(era).unwrap_or_else(Zero::zero),
				validator_incentive: ErasValidatorIncentiveBudget::<T>::get(era),
			}
		}
	}
}
