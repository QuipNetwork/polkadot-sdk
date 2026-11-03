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

//! Staking FRAME Pallet.

use alloc::vec::Vec;
use codec::Codec;
use frame_election_provider_support::{ElectionProvider, SortedListProvider, VoteWeight};
use frame_support::{
	pallet_prelude::*,
	traits::{
		fungible::{
			hold::{Balanced as FunHoldBalanced, Mutate as FunHoldMutate},
			Mutate as FunMutate,
		},
		Contains, Defensive, EnsureOrigin, EstimateNextNewSession, Get, InspectLockableCurrency,
		Nothing, OnUnbalanced, UnixTime,
	},
	weights::Weight,
	BoundedVec,
};
use frame_system::{ensure_root, ensure_signed, pallet_prelude::*};
use sp_runtime::{
	traits::{SaturatedConversion, StaticLookup, Zero},
	ArithmeticError, Perbill, Percent,
};

use sp_staking::{
	EraIndex, Page, SessionIndex,
	StakingAccount::{self, Controller, Stash},
	StakingInterface,
};

mod impls;

pub use impls::*;

use crate::{
	asset, slashing, weights::WeightInfo, AccountIdLookupOf, ActiveEraInfo, BalanceOf,
	EraRewardPoints, Exposure, ExposurePage, Forcing, LedgerIntegrityState, MaxNominationsOf,
	NegativeImbalanceOf, Nominations, NominationsQuota, PositiveImbalanceOf, RewardDestination,
	SessionInterface, StakingLedger, UnappliedSlash, UnlockChunk, ValidatorPrefs,
};

// The speculative number of spans are used as an input of the weight annotation of
// [`Call::unbond`], as the post dispatch weight may depend on the number of slashing span on the
// account which is not provided as an input. The value set should be conservative but sensible.
pub(crate) const SPECULATIVE_NUM_SPANS: u32 = 32;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use codec::HasCompact;
	use frame_election_provider_support::ElectionDataProvider;

	use crate::{BenchmarkingConfig, PagedExposureMetadata};

	/// The in-code storage version.
	const STORAGE_VERSION: StorageVersion = StorageVersion::new(16);

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
		/// Time used for computing era duration.
		///
		/// It is guaranteed to start being called from the first `on_finalize`. Thus value at
		/// genesis is not used.
		#[pallet::no_default]
		type UnixTime: UnixTime;

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
		/// Something that provides the election functionality at genesis.
		#[pallet::no_default]
		type GenesisElectionProvider: ElectionProvider<
			AccountId = Self::AccountId,
			BlockNumber = BlockNumberFor<Self>,
			DataProvider = Pallet<Self>,
		>;

		/// Something that defines the maximum number of nominations per nominator.
		#[pallet::no_default_bounds]
		type NominationsQuota: NominationsQuota<BalanceOf<Self>>;

		/// Number of eras to keep in history.
		///
		/// Following information is kept for eras in `[current_era -
		/// HistoryDepth, current_era]`: `ErasStakers`, `ErasStakersClipped`,
		/// `ErasValidatorPrefs`, `ErasValidatorReward`, `ErasRewardPoints`,
		/// `ErasTotalStake`, `ErasStartSessionIndex`, `ClaimedRewards`, `ErasStakersPaged`,
		/// `ErasStakersOverview`.
		///
		/// Must be more than the number of eras delayed by session.
		/// I.e. active era must always be in history. I.e. `active_era >
		/// current_era - history_depth` must be guaranteed.
		///
		/// If migrating an existing pallet from storage value to config value,
		/// this should be set to same value or greater as in storage.
		///
		/// Note: `HistoryDepth` is used as the upper bound for the `BoundedVec`
		/// item `StakingLedger.legacy_claimed_rewards`. Setting this value lower than
		/// the existing value can lead to inconsistencies in the
		/// `StakingLedger` and will need to be handled properly in a migration.
		/// The test `reducing_history_depth_abrupt` shows this effect.
		#[pallet::constant]
		type HistoryDepth: Get<u32>;

		/// Tokens have been minted and are unused for validator-reward.
		/// See [Era payout](./index.html#era-payout).
		#[pallet::no_default_bounds]
		type RewardRemainder: OnUnbalanced<NegativeImbalanceOf<Self>>;

		/// The overarching event type.
		#[pallet::no_default_bounds]
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Handler for the unbalanced reduction when slashing a staker.
		#[pallet::no_default_bounds]
		type Slash: OnUnbalanced<NegativeImbalanceOf<Self>>;

		/// Handler for the unbalanced increment when rewarding a staker.
		/// NOTE: in most cases, the implementation of `OnUnbalanced` should modify the total
		/// issuance.
		#[pallet::no_default_bounds]
		type Reward: OnUnbalanced<PositiveImbalanceOf<Self>>;

		/// Number of sessions per era.
		#[pallet::constant]
		type SessionsPerEra: Get<SessionIndex>;

		/// Number of eras that staked funds must remain bonded for.
		#[pallet::constant]
		type BondingDuration: Get<EraIndex>;

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

		/// Interface for interacting with a session pallet.
		type SessionInterface: SessionInterface<Self::AccountId>;

		/// The payout for validators and the system for the current era.
		/// See [Era payout](./index.html#era-payout).
		#[pallet::no_default]
		type EraPayout: sp_staking::EraPayout<BalanceOf<Self>>;

		/// Something that can estimate the next session change, accurately or as a best effort
		/// guess.
		#[pallet::no_default_bounds]
		type NextNewSession: EstimateNextNewSession<BlockNumberFor<Self>>;

		/// The maximum size of each `T::ExposurePage`.
		///
		/// An `ExposurePage` is weakly bounded to a maximum of `MaxExposurePageSize`
		/// nominators.
		///
		/// For older non-paged exposure, a reward payout was restricted to the top
		/// `MaxExposurePageSize` nominators. This is to limit the i/o cost for the
		/// nominator payout.
		///
		/// Note: `MaxExposurePageSize` is used to bound `ClaimedRewards` and is unsafe to reduce
		/// without handling it in a migration.
		#[pallet::constant]
		type MaxExposurePageSize: Get<u32>;

		/// The absolute maximum of winner validators this pallet should return.
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

		#[pallet::no_default_bounds]
		/// Filter some accounts from participating in staking.
		///
		/// This is useful for example to blacklist an account that is participating in staking in
		/// another way (such as pools).
		type Filter: Contains<Self::AccountId>;

		/// Some parameters of the benchmarking.
		#[cfg(feature = "std")]
		type BenchmarkingConfig: BenchmarkingConfig;

		#[cfg(not(feature = "std"))]
		#[pallet::no_default]
		type BenchmarkingConfig: BenchmarkingConfig;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;
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
		}

		#[frame_support::register_default_impl(TestDefaultConfig)]
		impl DefaultConfig for TestDefaultConfig {
			#[inject_runtime_type]
			type RuntimeEvent = ();
			#[inject_runtime_type]
			type RuntimeHoldReason = ();
			type CurrencyBalance = u128;
			type CurrencyToVote = ();
			type NominationsQuota = crate::FixedNominationsQuota<16>;
			type HistoryDepth = ConstU32<84>;
			type RewardRemainder = ();
			type Slash = ();
			type Reward = ();
			type SessionsPerEra = SessionsPerEra;
			type BondingDuration = BondingDuration;
			type SlashDeferDuration = ();
			type SessionInterface = ();
			type NextNewSession = ();
			type MaxExposurePageSize = ConstU32<64>;
			type MaxUnlockingChunks = ConstU32<32>;
			type MaxValidatorSet = ConstU32<100>;
			type MaxControllersInDeprecationBatch = ConstU32<100>;
			type EventListeners = ();
			type Filter = Nothing;
			#[cfg(feature = "std")]
			type BenchmarkingConfig = crate::TestBenchmarkingConfig;
			type WeightInfo = ();
		}
	}

	/// The ideal number of active validators.
	#[pallet::storage]
	pub type ValidatorCount<T> = StorageValue<_, u32, ValueQuery>;

	/// Minimum number of staking participants before emergency conditions are imposed.
	#[pallet::storage]
	pub type MinimumValidatorCount<T> = StorageValue<_, u32, ValueQuery>;

	/// Any validators that may never be slashed or forcibly kicked. It's a Vec since they're
	/// easy to initialize and the performance hit is minimal (we expect no more than four
	/// invulnerables) and restricted to testnets.
	#[pallet::storage]
	#[pallet::unbounded]
	pub type Invulnerables<T: Config> = StorageValue<_, Vec<T::AccountId>, ValueQuery>;

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

	/// The current era index.
	///
	/// This is the latest planned era, depending on how the Session pallet queues the validator
	/// set, it might be active or not.
	#[pallet::storage]
	pub type CurrentEra<T> = StorageValue<_, EraIndex>;

	/// The active era information, it holds index and start.
	///
	/// The active era is the era being currently rewarded. Validator set of this era must be
	/// equal to [`SessionInterface::validators`].
	#[pallet::storage]
	pub type ActiveEra<T> = StorageValue<_, ActiveEraInfo>;

	/// The session index at which the era start for the last [`Config::HistoryDepth`] eras.
	///
	/// Note: This tracks the starting session (i.e. session index when era start being active)
	/// for the eras in `[CurrentEra - HISTORY_DEPTH, CurrentEra]`.
	#[pallet::storage]
	pub type ErasStartSessionIndex<T> = StorageMap<_, Twox64Concat, EraIndex, SessionIndex>;

	/// Exposure of validator at era.
	///
	/// This is keyed first by the era index to allow bulk deletion and then the stash account.
	///
	/// Is it removed after [`Config::HistoryDepth`] eras.
	/// If stakers hasn't been set or has been removed then empty exposure is returned.
	///
	/// Note: Deprecated since v14. Use `EraInfo` instead to work with exposures.
	#[pallet::storage]
	#[pallet::unbounded]
	pub type ErasStakers<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		EraIndex,
		Twox64Concat,
		T::AccountId,
		Exposure<T::AccountId, BalanceOf<T>>,
		ValueQuery,
	>;

	/// Summary of validator exposure at a given era.
	///
	/// This contains the total stake in support of the validator and their own stake. In addition,
	/// it can also be used to get the number of nominators backing this validator and the number of
	/// exposure pages they are divided into. The page count is useful to determine the number of
	/// pages of rewards that needs to be claimed.
	///
	/// This is keyed first by the era index to allow bulk deletion and then the stash account.
	/// Should only be accessed through `EraInfo`.
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

	/// Clipped Exposure of validator at era.
	///
	/// Note: This is deprecated, should be used as read-only and will be removed in the future.
	/// New `Exposure`s are stored in a paged manner in `ErasStakersPaged` instead.
	///
	/// This is similar to [`ErasStakers`] but number of nominators exposed is reduced to the
	/// `T::MaxExposurePageSize` biggest stakers.
	/// (Note: the field `total` and `own` of the exposure remains unchanged).
	/// This is used to limit the i/o cost for the nominator payout.
	///
	/// This is keyed fist by the era index to allow bulk deletion and then the stash account.
	///
	/// It is removed after [`Config::HistoryDepth`] eras.
	/// If stakers hasn't been set or has been removed then empty exposure is returned.
	///
	/// Note: Deprecated since v14. Use `EraInfo` instead to work with exposures.
	#[pallet::storage]
	#[pallet::unbounded]
	pub type ErasStakersClipped<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		EraIndex,
		Twox64Concat,
		T::AccountId,
		Exposure<T::AccountId, BalanceOf<T>>,
		ValueQuery,
	>;

	/// Paginated exposure of a validator at given era.
	///
	/// This is keyed first by the era index to allow bulk deletion, then stash account and finally
	/// the page. Should only be accessed through `EraInfo`.
	///
	/// This is cleared after [`Config::HistoryDepth`] eras.
	#[pallet::storage]
	#[pallet::unbounded]
	pub type ErasStakersPaged<T: Config> = StorageNMap<
		_,
		(
			NMapKey<Twox64Concat, EraIndex>,
			NMapKey<Twox64Concat, T::AccountId>,
			NMapKey<Twox64Concat, Page>,
		),
		ExposurePage<T::AccountId, BalanceOf<T>>,
		OptionQuery,
	>;

	/// History of claimed paged rewards by era and validator.
	///
	/// This is keyed by era and validator stash which maps to the set of page indexes which have
	/// been claimed.
	///
	/// It is removed after [`Config::HistoryDepth`] eras.
	#[pallet::storage]
	#[pallet::unbounded]
	pub type ClaimedRewards<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		EraIndex,
		Twox64Concat,
		T::AccountId,
		Vec<Page>,
		ValueQuery,
	>;

	/// Similar to `ErasStakers`, this holds the preferences of validators.
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

	/// The total validator era payout for the last [`Config::HistoryDepth`] eras.
	///
	/// Eras that haven't finished yet or has been removed doesn't have reward.
	#[pallet::storage]
	pub type ErasValidatorReward<T: Config> = StorageMap<_, Twox64Concat, EraIndex, BalanceOf<T>>;

	/// Rewards for the last [`Config::HistoryDepth`] eras.
	/// If reward hasn't been set or has been removed then 0 reward is returned.
	#[pallet::storage]
	#[pallet::unbounded]
	pub type ErasRewardPoints<T: Config> =
		StorageMap<_, Twox64Concat, EraIndex, EraRewardPoints<T::AccountId>, ValueQuery>;

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
	/// See [Era payout](./index.html#era-payout).
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

	/// All unapplied slashes that are queued for later.
	#[pallet::storage]
	#[pallet::unbounded]
	pub type UnappliedSlashes<T: Config> = StorageMap<
		_,
		Twox64Concat,
		EraIndex,
		Vec<UnappliedSlash<T::AccountId, BalanceOf<T>>>,
		ValueQuery,
	>;

	/// A mapping from still-bonded eras to the first session index of that era.
	///
	/// Must contains information for eras for the range:
	/// `[active_era - bounding_duration; active_era]`
	#[pallet::storage]
	#[pallet::unbounded]
	pub type BondedEras<T: Config> = StorageValue<_, Vec<(EraIndex, SessionIndex)>, ValueQuery>;

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

	/// All slashing events on nominators, mapped by era to the highest slash value of the era.
	#[pallet::storage]
	pub type NominatorSlashInEra<T: Config> =
		StorageDoubleMap<_, Twox64Concat, EraIndex, Twox64Concat, T::AccountId, BalanceOf<T>>;

	/// Slashing spans for stash accounts.
	#[pallet::storage]
	#[pallet::unbounded]
	pub type SlashingSpans<T: Config> =
		StorageMap<_, Twox64Concat, T::AccountId, slashing::SlashingSpans>;

	/// Records information about the maximum slash of a stash within a slashing span,
	/// as well as how much reward has been paid out.
	#[pallet::storage]
	pub type SpanSlash<T: Config> = StorageMap<
		_,
		Twox64Concat,
		(T::AccountId, slashing::SpanIndex),
		slashing::SpanRecord<BalanceOf<T>>,
		ValueQuery,
	>;

	/// The last planned session scheduled by the session pallet.
	///
	/// This is basically in sync with the call to [`pallet_session::SessionManager::new_session`].
	#[pallet::storage]
	pub type CurrentPlannedSession<T> = StorageValue<_, SessionIndex, ValueQuery>;

	/// The threshold for when users can start calling `chill_other` for other validators /
	/// nominators. The threshold is compared to the actual number of validators / nominators
	/// (`CountFor*`) in the system compared to the configured max (`Max*Count`).
	#[pallet::storage]
	pub type ChillThreshold<T: Config> = StorageValue<_, Percent, OptionQuery>;

	#[pallet::genesis_config]
	#[derive(frame_support::DefaultNoBound)]
	pub struct GenesisConfig<T: Config> {
		pub validator_count: u32,
		pub minimum_validator_count: u32,
		pub invulnerables: Vec<T::AccountId>,
		pub force_era: Forcing,
		pub slash_reward_fraction: Perbill,
		pub canceled_payout: BalanceOf<T>,
		pub stakers:
			Vec<(T::AccountId, T::AccountId, BalanceOf<T>, crate::StakerStatus<T::AccountId>)>,
		pub min_nominator_bond: BalanceOf<T>,
		pub min_validator_bond: BalanceOf<T>,
		pub max_validator_count: Option<u32>,
		pub max_nominator_count: Option<u32>,
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
		fn build(&self) {
			ValidatorCount::<T>::put(self.validator_count);
			MinimumValidatorCount::<T>::put(self.minimum_validator_count);
			Invulnerables::<T>::put(&self.invulnerables);
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

			for &(ref stash, _, balance, ref status) in &self.stakers {
				crate::log!(
					trace,
					"inserting genesis staker: {:?} => {:?} => {:?}",
					stash,
					balance,
					status
				);
				assert!(
					asset::free_to_stake::<T>(stash) >= balance,
					"Stash does not have enough balance to bond."
				);
				frame_support::assert_ok!(<Pallet<T>>::bond(
					T::RuntimeOrigin::from(Some(stash.clone()).into()),
					balance,
					RewardDestination::Staked,
				));
				frame_support::assert_ok!(match status {
					crate::StakerStatus::Validator => <Pallet<T>>::validate(
						T::RuntimeOrigin::from(Some(stash.clone()).into()),
						Default::default(),
					),
					crate::StakerStatus::Nominator(votes) => <Pallet<T>>::nominate(
						T::RuntimeOrigin::from(Some(stash.clone()).into()),
						votes.iter().map(|l| T::Lookup::unlookup(l.clone())).collect(),
					),
					_ => Ok(()),
				});
				assert!(
					ValidatorCount::<T>::get() <=
						<T::ElectionProvider as ElectionProvider>::MaxWinnersPerPage::get() *
							<T::ElectionProvider as ElectionProvider>::Pages::get()
				);
			}

			// all voters are reported to the `VoterList`.
			assert_eq!(
				T::VoterList::count(),
				Nominators::<T>::count() + Validators::<T>::count(),
				"not all genesis stakers were inserted into sorted list provider, something is wrong."
			);
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	pub enum Event<T: Config> {
		/// The era payout has been set; the first balance is the validator-payout; the second is
		/// the remainder from the maximum amount of reward.
		EraPaid { era_index: EraIndex, validator_payout: BalanceOf<T>, remainder: BalanceOf<T> },
		/// The nominator has been rewarded by this amount to this destination.
		Rewarded {
			stash: T::AccountId,
			dest: RewardDestination<T::AccountId>,
			amount: BalanceOf<T>,
		},
		/// A staker (validator or nominator) has been slashed by the given amount.
		Slashed { staker: T::AccountId, amount: BalanceOf<T> },
		/// A slash for the given validator, for the given percentage of their stake, at the given
		/// era as been reported.
		SlashReported { validator: T::AccountId, fraction: Perbill, slash_era: EraIndex },
		/// An old slashing report from a prior era was discarded because it could
		/// not be processed.
		OldSlashingReportDiscarded { session_index: SessionIndex },
		/// A new set of stakers was elected.
		StakersElected,
		/// An account has bonded this amount. \[stash, amount\]
		///
		/// NOTE: This event is only emitted when funds are bonded via a dispatchable. Notably,
		/// it will not be emitted for staking rewards when they are added to stake.
		Bonded { stash: T::AccountId, amount: BalanceOf<T> },
		/// An account has unbonded this amount.
		Unbonded { stash: T::AccountId, amount: BalanceOf<T> },
		/// An account has called `withdraw_unbonded` and removed unbonding chunks worth `Balance`
		/// from the unlocking queue.
		Withdrawn { stash: T::AccountId, amount: BalanceOf<T> },
		/// A nominator has been kicked from a validator.
		Kicked { nominator: T::AccountId, stash: T::AccountId },
		/// The election failed. No new era is planned.
		StakingElectionFailed,
		/// An account has stopped participating as either a validator or nominator.
		Chilled { stash: T::AccountId },
		/// A Page of stakers rewards are getting paid. `next` is `None` if all pages are claimed.
		PayoutStarted {
			era_index: EraIndex,
			validator_stash: T::AccountId,
			page: Page,
			next: Option<Page>,
		},
		/// A validator has set their preferences.
		ValidatorPrefsSet { stash: T::AccountId, prefs: ValidatorPrefs },
		/// Voters size limit reached.
		SnapshotVotersSizeExceeded { size: u32 },
		/// Targets size limit reached.
		SnapshotTargetsSizeExceeded { size: u32 },
		/// A new force era mode was set.
		ForceEra { mode: Forcing },
		/// Report of a controller batch deprecation.
		ControllerBatchDeprecated { failures: u32 },
		/// Staking balance migrated from locks to holds, with any balance that could not be held
		/// is force withdrawn.
		CurrencyMigrated { stash: T::AccountId, force_withdraw: BalanceOf<T> },
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
		/// Slash record index out of bounds.
		InvalidSlashIndex,
		/// Cannot have a validator or nominator role, with value less than the minimum defined by
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
		/// Items are not sorted and unique.
		NotSortedAndUnique,
		/// Rewards for this era have already been claimed for this validator.
		AlreadyClaimed,
		/// No nominators exist on this page.
		InvalidPage,
		/// Incorrect previous history depth input provided.
		IncorrectHistoryDepth,
		/// Incorrect number of slashing spans provided.
		IncorrectSlashingSpans,
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
		/// Account is restricted from participation in staking. This may happen if the account is
		/// staking in another way already, such as via pool.
		Restricted,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(_now: BlockNumberFor<T>) -> Weight {
			// just return the weight of the on_finalize.
			T::DbWeight::get().reads(1)
		}

		fn on_finalize(_n: BlockNumberFor<T>) {
			// Set the start of the first era.
			if let Some(mut active_era) = ActiveEra::<T>::get() {
				if active_era.start.is_none() {
					let now_as_millis_u64 = T::UnixTime::now().as_millis().saturated_into::<u64>();
					active_era.start = Some(now_as_millis_u64);
					// This write only ever happens once, we don't include it in the weight in
					// general
					ActiveEra::<T>::put(active_era);
				}
			}
			// `on_finalize` weight is tracked in `on_initialize`
		}

		fn integrity_test() {
			// ensure that we funnel the correct value to the `DataProvider::MaxVotesPerVoter`;
			assert_eq!(
				MaxNominationsOf::<T>::get(),
				<Self as ElectionDataProvider>::MaxVotesPerVoter::get()
			);
			// and that MaxNominations is always greater than 1, since we count on this.
			assert!(!MaxNominationsOf::<T>::get().is_zero());

			// ensure election results are always bounded with the same value
			assert!(
				<T::ElectionProvider as ElectionProvider>::MaxWinnersPerPage::get() ==
					<T::GenesisElectionProvider as ElectionProvider>::MaxWinnersPerPage::get()
			);

			assert!(
				T::SlashDeferDuration::get() < T::BondingDuration::get() || T::BondingDuration::get() == 0,
				"As per documentation, slash defer duration ({}) should be less than bonding duration ({}).",
				T::SlashDeferDuration::get(),
				T::BondingDuration::get(),
			)
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(n: BlockNumberFor<T>) -> Result<(), sp_runtime::TryRuntimeError> {
			Self::do_try_state(n)
		}
	}

	impl<T: Config> Pallet<T> {
		/// Get the ideal number of active validators.
		pub fn validator_count() -> u32 {
			ValidatorCount::<T>::get()
		}

		/// Get the minimum number of staking participants before emergency conditions are imposed.
		pub fn minimum_validator_count() -> u32 {
			MinimumValidatorCount::<T>::get()
		}

		/// Get the validators that may never be slashed or forcibly kicked out.
		pub fn invulnerables() -> Vec<T::AccountId> {
			Invulnerables::<T>::get()
		}

		/// Get the preferences of a given validator.
		pub fn validators<EncodeLikeAccountId>(account_id: EncodeLikeAccountId) -> ValidatorPrefs
		where
			EncodeLikeAccountId: codec::EncodeLike<T::AccountId>,
		{
			Validators::<T>::get(account_id)
		}

		/// Get the nomination preferences of a given nominator.
		pub fn nominators<EncodeLikeAccountId>(
			account_id: EncodeLikeAccountId,
		) -> Option<Nominations<T>>
		where
			EncodeLikeAccountId: codec::EncodeLike<T::AccountId>,
		{
			Nominators::<T>::get(account_id)
		}

		/// Get the current era index.
		pub fn current_era() -> Option<EraIndex> {
			CurrentEra::<T>::get()
		}

		/// Get the active era information.
		pub fn active_era() -> Option<ActiveEraInfo> {
			ActiveEra::<T>::get()
		}

		/// Get the session index at which the era starts for the last [`Config::HistoryDepth`]
		/// eras.
		pub fn eras_start_session_index<EncodeLikeEraIndex>(
			era_index: EncodeLikeEraIndex,
		) -> Option<SessionIndex>
		where
			EncodeLikeEraIndex: codec::EncodeLike<EraIndex>,
		{
			ErasStartSessionIndex::<T>::get(era_index)
		}

		/// Get the clipped exposure of a given validator at an era.
		pub fn eras_stakers_clipped<EncodeLikeEraIndex, EncodeLikeAccountId>(
			era_index: EncodeLikeEraIndex,
			account_id: EncodeLikeAccountId,
		) -> Exposure<T::AccountId, BalanceOf<T>>
		where
			EncodeLikeEraIndex: codec::EncodeLike<EraIndex>,
			EncodeLikeAccountId: codec::EncodeLike<T::AccountId>,
		{
			ErasStakersClipped::<T>::get(era_index, account_id)
		}

		/// Get the paged history of claimed rewards by era for given validator.
		pub fn claimed_rewards<EncodeLikeEraIndex, EncodeLikeAccountId>(
			era_index: EncodeLikeEraIndex,
			account_id: EncodeLikeAccountId,
		) -> Vec<Page>
		where
			EncodeLikeEraIndex: codec::EncodeLike<EraIndex>,
			EncodeLikeAccountId: codec::EncodeLike<T::AccountId>,
		{
			ClaimedRewards::<T>::get(era_index, account_id)
		}

		/// Get the preferences of given validator at given era.
		pub fn eras_validator_prefs<EncodeLikeEraIndex, EncodeLikeAccountId>(
			era_index: EncodeLikeEraIndex,
			account_id: EncodeLikeAccountId,
		) -> ValidatorPrefs
		where
			EncodeLikeEraIndex: codec::EncodeLike<EraIndex>,
			EncodeLikeAccountId: codec::EncodeLike<T::AccountId>,
		{
			ErasValidatorPrefs::<T>::get(era_index, account_id)
		}

		/// Get the total validator era payout for the last [`Config::HistoryDepth`] eras.
		pub fn eras_validator_reward<EncodeLikeEraIndex>(
			era_index: EncodeLikeEraIndex,
		) -> Option<BalanceOf<T>>
		where
			EncodeLikeEraIndex: codec::EncodeLike<EraIndex>,
		{
			ErasValidatorReward::<T>::get(era_index)
		}

		/// Get the rewards for the last [`Config::HistoryDepth`] eras.
		pub fn eras_reward_points<EncodeLikeEraIndex>(
			era_index: EncodeLikeEraIndex,
		) -> EraRewardPoints<T::AccountId>
		where
			EncodeLikeEraIndex: codec::EncodeLike<EraIndex>,
		{
			ErasRewardPoints::<T>::get(era_index)
		}

		/// Get the total amount staked for the last [`Config::HistoryDepth`] eras.
		pub fn eras_total_stake<EncodeLikeEraIndex>(era_index: EncodeLikeEraIndex) -> BalanceOf<T>
		where
			EncodeLikeEraIndex: codec::EncodeLike<EraIndex>,
		{
			ErasTotalStake::<T>::get(era_index)
		}

		/// Get the mode of era forcing.
		pub fn force_era() -> Forcing {
			ForceEra::<T>::get()
		}

		/// Get the percentage of the slash that is distributed to reporters.
		pub fn slash_reward_fraction() -> Perbill {
			SlashRewardFraction::<T>::get()
		}

		/// Get the amount of canceled slash payout.
		pub fn canceled_payout() -> BalanceOf<T> {
			CanceledSlashPayout::<T>::get()
		}

		/// Get the slashing spans for given account.
		pub fn slashing_spans<EncodeLikeAccountId>(
			account_id: EncodeLikeAccountId,
		) -> Option<slashing::SlashingSpans>
		where
			EncodeLikeAccountId: codec::EncodeLike<T::AccountId>,
		{
			SlashingSpans::<T>::get(account_id)
		}

		/// Get the last planned session scheduled by the session pallet.
		pub fn current_planned_session() -> SessionIndex {
			CurrentPlannedSession::<T>::get()
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
		/// ## Complexity
		/// - Independent of the arguments. Moderate complexity.
		/// - O(1).
		/// - Three extra DB entries.
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

			// Reject a bond which is considered to be _dust_.
			if value < asset::existential_deposit::<T>() {
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
		///
		/// ## Complexity
		/// - Independent of the arguments. Insignificant complexity.
		/// - O(1).
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
		/// The stash may be chilled if the ledger total amount falls to 0 after unbonding.
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
            T::WeightInfo::withdraw_unbonded_kill(SPECULATIVE_NUM_SPANS).saturating_add(T::WeightInfo::unbond()).saturating_add(T::WeightInfo::chill()))
        ]
		pub fn unbond(
			origin: OriginFor<T>,
			#[pallet::compact] value: BalanceOf<T>,
		) -> DispatchResultWithPostInfo {
			let controller = ensure_signed(origin)?;

			let ledger = Self::ledger(StakingAccount::Controller(controller.clone()))?;

			let mut total_weight = if value >= ledger.total {
				Self::chill_stash(&ledger.stash);
				T::WeightInfo::chill()
			} else {
				Zero::zero()
			};

			if let Some(withdraw_weight) = Self::do_unbond(controller, value)? {
				total_weight.saturating_accrue(withdraw_weight);
			}

			Ok(Some(total_weight).into())
		}

		/// Remove any unlocked chunks from the `unlocking` queue from our management.
		///
		/// This essentially frees up that balance to be used by the stash account to do whatever
		/// it wants.
		///
		/// The dispatch origin for this call must be _Signed_ by the controller.
		///
		/// Emits `Withdrawn`.
		///
		/// See also [`Call::unbond`].
		///
		/// ## Parameters
		///
		/// - `num_slashing_spans` indicates the number of metadata slashing spans to clear when
		/// this call results in a complete removal of all the data related to the stash account.
		/// In this case, the `num_slashing_spans` must be larger or equal to the number of
		/// slashing spans associated with the stash account in the [`SlashingSpans`] storage type,
		/// otherwise the call will fail. The call weight is directly proportional to
		/// `num_slashing_spans`.
		///
		/// ## Complexity
		/// O(S) where S is the number of slashing spans to remove
		/// NOTE: Weight annotation is the kill scenario, we refund otherwise.
		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::withdraw_unbonded_kill(*num_slashing_spans))]
		pub fn withdraw_unbonded(
			origin: OriginFor<T>,
			num_slashing_spans: u32,
		) -> DispatchResultWithPostInfo {
			let controller = ensure_signed(origin)?;

			let actual_weight = Self::do_withdraw_unbonded(&controller, num_slashing_spans)?;
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

			ensure!(ledger.active >= MinValidatorBond::<T>::get(), Error::<T>::InsufficientBond);
			let stash = &ledger.stash;

			// ensure their commission is correct.
			ensure!(prefs.commission >= MinCommission::<T>::get(), Error::<T>::CommissionTooLow);

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
		///
		/// ## Complexity
		/// - The transaction's complexity is proportional to the size of `targets` (N)
		/// which is capped at CompactAssignments::LIMIT (T::MaxNominations).
		/// - Both the reads and writes follow a similar pattern.
		#[pallet::call_index(5)]
		#[pallet::weight(T::WeightInfo::nominate(targets.len() as u32))]
		pub fn nominate(
			origin: OriginFor<T>,
			targets: Vec<AccountIdLookupOf<T>>,
		) -> DispatchResult {
			let controller = ensure_signed(origin)?;

			let ledger = Self::ledger(StakingAccount::Controller(controller.clone()))?;

			ensure!(ledger.active >= MinNominatorBond::<T>::get(), Error::<T>::InsufficientBond);
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

			ensure!(!targets.is_empty(), Error::<T>::EmptyTargets);
			ensure!(
				targets.len() <= T::NominationsQuota::get_quota(ledger.active) as usize,
				Error::<T>::TooManyTargets
			);

			let old = Nominators::<T>::get(stash).map_or_else(Vec::new, |x| x.targets.into_inner());

			let targets: BoundedVec<_, _> = targets
				.into_iter()
				.map(|t| T::Lookup::lookup(t).map_err(DispatchError::from))
				.map(|n| {
					n.and_then(|n| {
						if old.contains(&n) || !Validators::<T>::get(&n).blocked {
							Ok(n)
						} else {
							Err(Error::<T>::BadTarget.into())
						}
					})
				})
				.collect::<Result<Vec<_>, _>>()?
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
		///
		/// ## Complexity
		/// - O(1)
		/// - Independent of the arguments. Insignificant complexity.
		/// - Contains a limited number of reads.
		/// - Writes are limited to the `origin` account key.
		/// ---------
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

			ledger
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
		///
		/// ## Complexity
		/// O(1)
		/// - Independent of the arguments. Insignificant complexity.
		/// - Contains a limited number of reads.
		/// - Writes are limited to the `origin` account key.
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

				ledger.set_controller_to_stash()?;
				Ok(())
			})?
		}

		/// Sets the ideal number of validators.
		///
		/// The dispatch origin must be Root.
		///
		/// ## Complexity
		/// O(1)
		#[pallet::call_index(9)]
		#[pallet::weight(T::WeightInfo::set_validator_count())]
		pub fn set_validator_count(
			origin: OriginFor<T>,
			#[pallet::compact] new: u32,
		) -> DispatchResult {
			ensure_root(origin)?;
			// ensure new validator count does not exceed maximum winners
			// support by election provider.
			ensure!(new <= T::MaxValidatorSet::get(), Error::<T>::TooManyValidators);

			ValidatorCount::<T>::put(new);
			Ok(())
		}

		/// Increments the ideal number of validators up to maximum of
		/// `ElectionProviderBase::MaxWinners`.
		///
		/// The dispatch origin must be Root.
		///
		/// ## Complexity
		/// Same as [`Self::set_validator_count`].
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
		/// `ElectionProviderBase::MaxWinners`.
		///
		/// The dispatch origin must be Root.
		///
		/// ## Complexity
		/// Same as [`Self::set_validator_count`].
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
		///
		/// ## Complexity
		/// - No arguments.
		/// - Weight: O(1)
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
		///
		/// ## Complexity
		/// - No arguments.
		/// - Weight: O(1)
		#[pallet::call_index(13)]
		#[pallet::weight(T::WeightInfo::force_new_era())]
		pub fn force_new_era(origin: OriginFor<T>) -> DispatchResult {
			ensure_root(origin)?;
			Self::set_force_era(Forcing::ForceNew);
			Ok(())
		}

		/// Set the validators who cannot be slashed (if any).
		///
		/// The dispatch origin must be Root.
		#[pallet::call_index(14)]
		#[pallet::weight(T::WeightInfo::set_invulnerables(invulnerables.len() as u32))]
		pub fn set_invulnerables(
			origin: OriginFor<T>,
			invulnerables: Vec<T::AccountId>,
		) -> DispatchResult {
			ensure_root(origin)?;
			<Invulnerables<T>>::put(invulnerables);
			Ok(())
		}

		/// Force a current staker to become completely unstaked, immediately.
		///
		/// The dispatch origin must be Root.
		///
		/// ## Parameters
		///
		/// - `num_slashing_spans`: Refer to comments on [`Call::withdraw_unbonded`] for more
		/// details.
		#[pallet::call_index(15)]
		#[pallet::weight(T::WeightInfo::force_unstake(*num_slashing_spans))]
		pub fn force_unstake(
			origin: OriginFor<T>,
			stash: T::AccountId,
			num_slashing_spans: u32,
		) -> DispatchResult {
			ensure_root(origin)?;

			// Remove all staking-related information and lock.
			Self::kill_stash(&stash, num_slashing_spans)?;

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

		/// Cancel enactment of a deferred slash.
		///
		/// Can be called by the `T::AdminOrigin`.
		///
		/// Parameters: era and indices of the slashes for that era to kill.
		/// They **must** be sorted in ascending order, *and* unique.
		#[pallet::call_index(17)]
		#[pallet::weight(T::WeightInfo::cancel_deferred_slash(slash_indices.len() as u32))]
		pub fn cancel_deferred_slash(
			origin: OriginFor<T>,
			era: EraIndex,
			slash_indices: Vec<u32>,
		) -> DispatchResult {
			T::AdminOrigin::ensure_origin(origin)?;

			ensure!(!slash_indices.is_empty(), Error::<T>::EmptyTargets);
			ensure!(is_sorted_and_unique(&slash_indices), Error::<T>::NotSortedAndUnique);

			let mut unapplied = UnappliedSlashes::<T>::get(&era);
			let last_item = slash_indices[slash_indices.len() - 1];
			ensure!((last_item as usize) < unapplied.len(), Error::<T>::InvalidSlashIndex);

			for (removed, index) in slash_indices.into_iter().enumerate() {
				let index = (index as usize) - removed;
				unapplied.remove(index);
			}

			UnappliedSlashes::<T>::insert(&era, &unapplied);
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
		///
		/// ## Complexity
		/// - Time complexity: O(L), where L is unlocking chunks
		/// - Bounded by `MaxUnlockingChunks`.
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
			// Last check: the new active amount of ledger must be more than ED.
			ensure!(
				ledger.active >= asset::existential_deposit::<T>(),
				Error::<T>::InsufficientBond
			);

			Self::deposit_event(Event::<T>::Bonded {
				stash: ledger.stash.clone(),
				amount: rebonded_value,
			});

			let stash = ledger.stash.clone();
			let final_unlocking = ledger.unlocking.len();

			// NOTE: ledger must be updated prior to calling `Self::weight_of`.
			ledger.update()?;
			if T::VoterList::contains(&stash) {
				let _ = T::VoterList::on_update(&stash, Self::weight_of(&stash)).defensive();
			}

			let removed_chunks = 1u32 // for the case where the last iterated chunk is not removed
				.saturating_add(initial_unlocking)
				.saturating_sub(final_unlocking as u32);
			Ok(Some(T::WeightInfo::rebond(removed_chunks)).into())
		}

		/// Remove all data structures concerning a staker/stash once it is at a state where it can
		/// be considered `dust` in the staking system. The requirements are:
		///
		/// 1. the `total_balance` of the stash is below existential deposit.
		/// 2. or, the `ledger.total` of the stash is below existential deposit.
		/// 3. or, existential deposit is zero and either `total_balance` or `ledger.total` is zero.
		///
		/// The former can happen in cases like a slash; the latter when a fully unbonded account
		/// is still receiving staking rewards in `RewardDestination::Staked`.
		///
		/// It can be called by anyone, as long as `stash` meets the above requirements.
		///
		/// Refunds the transaction fees upon successful execution.
		///
		/// ## Parameters
		///
		/// - `num_slashing_spans`: Refer to comments on [`Call::withdraw_unbonded`] for more
		/// details.
		#[pallet::call_index(20)]
		#[pallet::weight(T::WeightInfo::reap_stash(*num_slashing_spans))]
		pub fn reap_stash(
			origin: OriginFor<T>,
			stash: T::AccountId,
			num_slashing_spans: u32,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;

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
			Self::kill_stash(&stash, num_slashing_spans)?;

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
		) -> DispatchResult {
			ensure_root(origin)?;

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
					MinNominatorBond::<T>::get()
				} else if Validators::<T>::contains_key(&stash) {
					let max_validator_count =
						MaxValidatorsCount::<T>::get().ok_or(Error::<T>::CannotChillOther)?;
					let current_validator_count = Validators::<T>::count();
					ensure!(
						threshold * max_validator_count < current_validator_count,
						Error::<T>::CannotChillOther
					);
					MinValidatorBond::<T>::get()
				} else {
					Zero::zero()
				};

				ensure!(ledger.active < min_active_bond, Error::<T>::CannotChillOther);
			}

			Self::chill_stash(&stash);
			Ok(())
		}

		/// Force a validator to have at least the minimum commission. This will not affect a
		/// validator who already has a commission greater than or equal to the minimum. Any account
		/// can call this.
		#[pallet::call_index(24)]
		#[pallet::weight(T::WeightInfo::force_apply_min_commission())]
		pub fn force_apply_min_commission(
			origin: OriginFor<T>,
			validator_stash: T::AccountId,
		) -> DispatchResult {
			ensure_signed(origin)?;
			let min_commission = MinCommission::<T>::get();
			Validators::<T>::try_mutate_exists(validator_stash, |maybe_prefs| {
				maybe_prefs
					.as_mut()
					.map(|prefs| {
						(prefs.commission < min_commission)
							.then(|| prefs.commission = min_commission)
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
			ensure_signed(origin)?;
			let ledger = Self::ledger(StakingAccount::Controller(controller.clone()))?;

			ensure!(
				(Payee::<T>::get(&ledger.stash) == {
					#[allow(deprecated)]
					Some(RewardDestination::Controller)
				}),
				Error::<T>::NotController
			);

			ledger
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

		/// Removes the legacy Staking locks if they exist.
		///
		/// This removes the legacy lock on the stake with [`Config::OldCurrency`] and creates a
		/// hold on it if needed. If all stake cannot be held, the best effort is made to hold as
		/// much as possible. The remaining stake is forced withdrawn from the ledger.
		///
		/// The fee is waived if the migration is successful.
		#[pallet::call_index(30)]
		#[pallet::weight(T::WeightInfo::migrate_currency())]
		pub fn migrate_currency(
			origin: OriginFor<T>,
			stash: T::AccountId,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;
			Self::do_migrate_currency(&stash)?;

			// Refund the transaction fee if successful.
			Ok(Pays::No.into())
		}

		/// This function allows governance to manually slash a validator and is a
		/// **fallback mechanism**.
		///
		/// The dispatch origin must be `T::AdminOrigin`.
		///
		/// ## Parameters
		/// - `validator_stash` - The stash account of the validator to slash.
		/// - `era` - The era in which the validator was in the active set.
		/// - `slash_fraction` - The percentage of the stake to slash, expressed as a Perbill.
		///
		/// ## Behavior
		///
		/// The slash will be applied using the standard slashing mechanics, respecting the
		/// configured `SlashDeferDuration`.
		///
		/// This means:
		/// - If the validator was already slashed by a higher percentage for the same era, this
		///   slash will have no additional effect.
		/// - If the validator was previously slashed by a lower percentage, only the difference
		///   will be applied.
		/// - The slash will be deferred by `SlashDeferDuration` eras before being enacted.
		#[pallet::call_index(33)]
		#[pallet::weight(T::WeightInfo::manual_slash())]
		pub fn manual_slash(
			origin: OriginFor<T>,
			validator_stash: T::AccountId,
			era: EraIndex,
			slash_fraction: Perbill,
		) -> DispatchResult {
			T::AdminOrigin::ensure_origin(origin)?;

			// Check era is valid
			let current_era = CurrentEra::<T>::get().ok_or(Error::<T>::InvalidEraToReward)?;
			let history_depth = T::HistoryDepth::get();
			ensure!(
				era <= current_era && era >= current_era.saturating_sub(history_depth),
				Error::<T>::InvalidEraToReward
			);

			let offence_details = sp_staking::offence::OffenceDetails {
				offender: validator_stash.clone(),
				reporters: Vec::new(),
			};

			// Get the session index for the era
			let session_index =
				ErasStartSessionIndex::<T>::get(era).ok_or(Error::<T>::InvalidEraToReward)?;

			// Create the offence and report it through on_offence system
			let _ = Self::on_offence(
				core::iter::once(offence_details),
				&[slash_fraction],
				session_index,
			);

			Ok(())
		}
	}
}

/// Check that list is sorted and has no duplicates.
fn is_sorted_and_unique(list: &[u32]) -> bool {
	list.windows(2).all(|w| w[0] < w[1])
}
