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

//! # Staking Pallet
//!
//! NOTE: This pallet is deprecated and no longer maintained in favour of `pallet-staking-async`.
//!
//! The Staking pallet is used to manage funds at stake by network maintainers.
//!
//! - [`Config`]
//! - [`Call`]
//! - [`Pallet`]
//!
//! ## Overview
//!
//! The Staking pallet is the means by which a set of network maintainers (known as _authorities_ in
//! some contexts and _validators_ in others) are chosen based upon those who voluntarily place
//! funds under deposit. Under deposit, those funds are rewarded under normal operation but are held
//! at pain of _slash_ (expropriation) should the staked maintainer be found not to be discharging
//! its duties properly.
//!
//! ### Terminology
//! <!-- Original author of paragraph: @gavofyork -->
//!
//! - Staking: The process of locking up funds for some time, placing them at risk of slashing
//!   (loss) in order to become a rewarded maintainer of the network.
//! - Validating: The process of running a node to actively maintain the network, either by
//!   producing blocks or guaranteeing finality of the chain.
//! - Nominating: The process of placing staked funds behind one or more validators in order to
//!   share in any reward, and punishment, they take.
//! - Stash account: The account holding an owner's funds used for staking.
//! - Controller account (being deprecated): The account that controls an owner's funds for staking.
//! - Era: A (whole) number of sessions, which is the period that the validator set (and each
//!   validator's active nominator set) is recalculated and where rewards are paid out.
//! - Slash: The punishment of a staker by reducing its funds.
//!
//! ### Goals
//! <!-- Original author of paragraph: @gavofyork -->
//!
//! The staking system in Substrate NPoS is designed to make the following possible:
//!
//! - Stake funds that are controlled by a cold wallet.
//! - Withdraw some, or deposit more, funds without interrupting the role of an entity.
//! - Switch between roles (nominator, validator, idle) with minimal overhead.
//!
//! ### Scenarios
//!
//! #### Staking
//!
//! Almost any interaction with the Staking pallet requires a process of _**bonding**_ (also known
//! as being a _staker_). To become *bonded*, a fund-holding register known as the _stash account_,
//! which holds some or all of the funds that become frozen in place as part of the staking process.
//! The controller account, which this pallet now assigns the stash account to, issues instructions
//! on how funds shall be used.
//!
//! An account can become a bonded stash account using the [`bond`](Call::bond) call.
//!
//! In the event stash accounts registered a unique controller account before the controller account
//! deprecation, they can update their associated controller back to the stash account using the
//! [`set_controller`](Call::set_controller) call.
//!
//! There are three possible roles that any staked account pair can be in: `Validator`, `Nominator`
//! and `Idle` (defined in [`StakerStatus`]). There are three corresponding instructions to change
//! between roles, namely: [`validate`](Call::validate), [`nominate`](Call::nominate), and
//! [`chill`](Call::chill).
//!
//! #### Validating
//!
//! A **validator** takes the role of either validating blocks or ensuring their finality,
//! maintaining the veracity of the network. A validator should avoid both any sort of malicious
//! misbehavior and going offline. Bonded accounts that state interest in being a validator do NOT
//! get immediately chosen as a validator. Instead, they are declared as a _candidate_ and they
//! _might_ get elected at the _next era_ as a validator. The result of the election is determined
//! by nominators and their votes.
//!
//! An account can become a validator candidate via the [`validate`](Call::validate) call.
//!
//! #### Nomination
//!
//! A **nominator** does not take any _direct_ role in maintaining the network, instead, it votes on
//! a set of validators to be elected. Once interest in nomination is stated by an account, it takes
//! effect at the next election round. The funds in the nominator's stash account indicate the
//! _weight_ of its vote. Both the rewards and any punishment that a validator earns are shared
//! between the validator and its nominators. This rule incentivizes the nominators to NOT vote for
//! the misbehaving/offline validators as much as possible, simply because the nominators will also
//! lose funds if they vote poorly.
//!
//! An account can become a nominator via the [`nominate`](Call::nominate) call.
//!
//! #### Voting
//!
//! Staking is closely related to elections; actual validators are chosen from among all potential
//! validators via election by the potential validators and nominators. To reduce use of the phrase
//! "potential validators and nominators", we often use the term **voters**, who are simply the
//! union of potential validators and nominators.
//!
//! #### Rewards and Slash
//!
//! The **reward and slashing** procedure is the core of the Staking pallet, attempting to _embrace
//! valid behavior_ while _punishing any misbehavior or lack of availability_.
//!
//! Rewards must be claimed for each era before it gets too old by
//! [`HistoryDepth`](`Config::HistoryDepth`) using the `payout_stakers` call. Any account can call
//! `payout_stakers`, which pays the reward to the validator as well as its nominators. Only
//! [`Config::MaxExposurePageSize`] nominator rewards can be claimed in a single call. When the
//! number of nominators exceeds [`Config::MaxExposurePageSize`], then the exposed nominators are
//! stored in multiple pages, with each page containing up to [`Config::MaxExposurePageSize`]
//! nominators. To pay out all nominators, `payout_stakers` must be called once for each available
//! page. Paging exists to limit the i/o cost to mutate storage for each nominator's account.
//!
//! Slashing can occur at any point in time, once misbehavior is reported. Once slashing is
//! determined, a value is deducted from the balance of the validator and all the nominators who
//! voted for this validator (values are deducted from the _stash_ account of the slashed entity).
//!
//! Slashing logic is further described in the documentation of the `slashing` pallet.
//!
//! Similar to slashing, rewards are also shared among a validator and its associated nominators.
//! Yet, the reward funds are not always transferred to the stash account and can be configured. See
//! [Reward Calculation](#reward-calculation) for more details.
//!
//! #### Chilling
//!
//! Finally, any of the roles above can choose to step back temporarily and just chill for a while.
//! This means that if they are a nominator, they will not be considered as voters anymore and if
//! they are validators, they will no longer be a candidate for the next election.
//!
//! An account can step back via the [`chill`](Call::chill) call.
//!
//! ### Session managing
//!
//! The pallet implement the trait `SessionManager`. Which is the only API to query new validator
//! set and allowing these validator set to be rewarded once their era is ended.
//!
//! ## Interface
//!
//! ### Dispatchable Functions
//!
//! The dispatchable functions of the Staking pallet enable the steps needed for entities to accept
//! and change their role, alongside some helper functions to get/set the metadata of the pallet.
//!
//! ### Public Functions
//!
//! The Staking pallet contains many public storage items and (im)mutable functions.
//!
//! ## Usage
//!
//! ### Example: Rewarding a validator by id.
//!
//! ```
//! use pallet_staking::{self as staking};
//! use frame_support::traits::RewardsReporter;
//!
//! #[frame_support::pallet(dev_mode)]
//! pub mod pallet {
//!   use super::*;
//!   use frame_support::pallet_prelude::*;
//!   use frame_system::pallet_prelude::*;
//!   # use frame_support::traits::RewardsReporter;
//!
//!   #[pallet::pallet]
//!   pub struct Pallet<T>(_);
//!
//!   #[pallet::config]
//!   pub trait Config: frame_system::Config + staking::Config {}
//!
//!   #[pallet::call]
//!   impl<T: Config> Pallet<T> {
//!         /// Reward a validator.
//!         #[pallet::weight(0)]
//!         pub fn reward_myself(origin: OriginFor<T>) -> DispatchResult {
//!             let reported = ensure_signed(origin)?;
//!             <staking::Pallet<T>>::reward_by_ids(vec![(reported, 10)]);
//!             Ok(())
//!         }
//!     }
//! }
//! # fn main() { }
//! ```
//!
//! ## Implementation Details
//!
//! ### Era payout
//!
//! The era payout is computed using yearly inflation curve defined at [`Config::EraPayout`] as
//! such:
//!
//! ```nocompile
//! staker_payout = yearly_inflation(npos_token_staked / total_tokens) * total_tokens / era_per_year
//! ```
//! This payout is used to reward stakers as defined in next section
//!
//! ```nocompile
//! remaining_payout = max_yearly_inflation * total_tokens / era_per_year - staker_payout
//! ```
//!
//! Note, however, that it is possible to set a cap on the total `staker_payout` for the era through
//! the `MaxStakersRewards` storage type. The `era_payout` implementor must ensure that the
//! `max_payout = remaining_payout + (staker_payout * max_stakers_rewards)`. The excess payout that
//! is not allocated for stakers is the era remaining reward.
//!
//! The remaining reward is send to the configurable end-point [`Config::RewardRemainder`].
//!
//! ### Reward Calculation
//!
//! Validators and nominators are rewarded at the end of each era. The total reward of an era is
//! calculated using the era duration and the staking rate (the total amount of tokens staked by
//! nominators and validators, divided by the total token supply). It aims to incentivize toward a
//! defined staking rate. The full specification can be found
//! [here](https://research.web3.foundation/en/latest/polkadot/Token%20Economics.html#inflation-model).
//!
//! Total reward is split among validators and their nominators depending on the number of points
//! they received during the era. Points are added to a validator using the method
//! [`frame_support::traits::RewardsReporter::reward_by_ids`] implemented by the [`Pallet`].
//!
//! [`Pallet`] implements [`pallet_authorship::EventHandler`] to add reward points to block producer
//! and block producer of referenced uncles.
//!
//! The validator and its nominator split their reward as following:
//!
//! The validator can declare an amount, named [`commission`](ValidatorPrefs::commission), that does
//! not get shared with the nominators at each reward payout through its [`ValidatorPrefs`]. This
//! value gets deducted from the total reward that is paid to the validator and its nominators. The
//! remaining portion is split pro rata among the validator and the nominators that nominated the
//! validator, proportional to the value staked behind the validator (_i.e._ dividing the
//! [`own`](Exposure::own) or [`others`](Exposure::others) by [`total`](Exposure::total) in
//! [`Exposure`]). Note that payouts are made in pages with each page capped at
//! [`Config::MaxExposurePageSize`] nominators. The distribution of nominators across pages may be
//! unsorted. The total commission is paid out proportionally across pages based on the total stake
//! of the page.
//!
//! All entities who receive a reward have the option to choose their reward destination through the
//! [`Payee`] storage item (see [`set_payee`](Call::set_payee)), to be one of the following:
//!
//! - Stash account, not increasing the staked value.
//! - Stash account, also increasing the staked value.
//! - Any other account, sent as free balance.
//!
//! ### Additional Fund Management Operations
//!
//! Any funds already placed into stash can be the target of the following operations:
//!
//! The controller account can free a portion (or all) of the funds using the
//! [`unbond`](Call::unbond) call. Note that the funds are not immediately accessible. Instead, a
//! duration denoted by [`Config::BondingDuration`] (in number of eras) must pass until the funds
//! can actually be removed. Once the `BondingDuration` is over, the
//! [`withdraw_unbonded`](Call::withdraw_unbonded) call can be used to actually withdraw the funds.
//!
//! Note that there is a limitation to the number of fund-chunks that can be scheduled to be
//! unlocked in the future via [`unbond`](Call::unbond). In case this maximum
//! (`MAX_UNLOCKING_CHUNKS`) is reached, the bonded account _must_ first wait until a successful
//! call to `withdraw_unbonded` to remove some of the chunks.
//!
//! ### Election Algorithm
//!
//! The current election algorithm is implemented based on Phragmén. The reference implementation
//! can be found [here](https://github.com/w3f/consensus/tree/master/NPoS).
//!
//! The election algorithm, aside from electing the validators with the most stake value and votes,
//! tries to divide the nominator votes among candidates in an equal manner. To further assure this,
//! an optional post-processing can be applied that iteratively normalizes the nominator staked
//! values until the total difference among votes of a particular nominator are less than a
//! threshold.
//!
//! ## GenesisConfig
//!
//! The Staking pallet depends on the [`GenesisConfig`]. The `GenesisConfig` is optional and allow
//! to set some initial stakers.
//!
//! ## Related Modules
//!
//! - [Balances](../pallet_balances/index.html): Used to manage values at stake.
//! - [Session](../pallet_session/index.html): Used to manage sessions. Also, a list of new
//!   validators is stored in the Session pallet's `Validators` at the end of each era.

#![cfg_attr(not(feature = "std"), no_std)]
#![recursion_limit = "256"]

#[cfg(feature = "runtime-benchmarks")]
pub mod benchmarking;
#[cfg(any(feature = "runtime-benchmarks", test))]
pub mod testing_utils;

#[cfg(test)]
pub(crate) mod mock;
#[cfg(test)]
mod tests;

pub mod asset;
pub mod election_size_tracker;
pub mod inflation;
pub mod ledger;
pub mod migrations;
pub mod slashing;
pub mod weights;

mod pallet;

extern crate alloc;

use alloc::{collections::btree_map::BTreeMap, vec, vec::Vec};
use codec::{Decode, DecodeWithMemTracking, Encode, HasCompact, MaxEncodedLen};
use frame_election_provider_support::ElectionProvider;
use frame_support::{
	defensive, defensive_assert,
	traits::{
		tokens::fungible::{Credit, Debt},
		ConstU32, Contains, Defensive, DefensiveMax, DefensiveSaturating, Get, LockIdentifier,
	},
	weights::Weight,
	BoundedVec, CloneNoBound, DebugNoBound, EqNoBound, PartialEqNoBound,
};
use scale_info::TypeInfo;
use sp_runtime::{
	curve::PiecewiseLinear,
	traits::{AtLeast32BitUnsigned, Convert, StaticLookup, Zero},
	Debug, Perbill, Perquintill, Rounding, Saturating,
};
use sp_staking::{
	offence::{Offence, OffenceError, OffenceSeverity, ReportOffence},
	EraIndex, ExposurePage, OnStakingUpdate, Page, PagedExposureMetadata, SessionIndex,
};
pub use sp_staking::{EraPayout, Exposure, IndividualExposure, StakerStatus};
pub use weights::WeightInfo;

pub use pallet::{pallet::*, UseNominatorsAndValidatorsMap, UseValidatorsMap};

pub(crate) const STAKING_ID: LockIdentifier = *b"staking ";
pub(crate) const LOG_TARGET: &str = "runtime::staking";

// syntactic sugar for logging.
#[macro_export]
macro_rules! log {
	($level:tt, $patter:expr $(, $values:expr)* $(,)?) => {
		log::$level!(
			target: crate::LOG_TARGET,
			concat!("[{:?}] 💸 ", $patter), <frame_system::Pallet<T>>::block_number() $(, $values)*
		)
	};
}

/// Alias for the maximum number of winners (aka. active validators), as defined in by this pallet's
/// config.
pub type MaxWinnersOf<T> = <T as Config>::MaxValidatorSet;

/// Alias for the maximum number of winners per page, as expected by the election provider.
pub type MaxWinnersPerPageOf<P> = <P as ElectionProvider>::MaxWinnersPerPage;

/// Maximum number of nominations per nominator.
pub type MaxNominationsOf<T> =
	<<T as Config>::NominationsQuota as NominationsQuota<BalanceOf<T>>>::MaxNominations;

/// Counter for the number of "reward" points earned by a given validator.
pub type RewardPoint = u32;

/// The balance type of this pallet.
pub type BalanceOf<T> = <T as Config>::CurrencyBalance;

type PositiveImbalanceOf<T> = Debt<<T as frame_system::Config>::AccountId, <T as Config>::Currency>;
pub type NegativeImbalanceOf<T> =
	Credit<<T as frame_system::Config>::AccountId, <T as Config>::Currency>;

type AccountIdLookupOf<T> = <<T as frame_system::Config>::Lookup as StaticLookup>::Source;

/// Information regarding the active era (era in used in session).
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, Debug, TypeInfo, MaxEncodedLen, PartialEq, Eq,
)]
pub struct ActiveEraInfo {
	/// Index of era.
	pub index: EraIndex,
	/// Moment of start expressed as millisecond from `$UNIX_EPOCH`.
	///
	/// Start can be none if start hasn't been set for the era yet,
	/// Start is set on the first on_finalize of the era to guarantee usage of `Time`.
	pub start: Option<u64>,
}

/// Reward points of an era. Used to split era total payout between validators.
///
/// This points will be used to reward validators and their respective nominators.
#[derive(Encode, Decode, DecodeWithMemTracking, Debug, TypeInfo, Clone, PartialEq, Eq)]
pub struct EraRewardPoints<AccountId: Ord> {
	/// Total number of points. Equals the sum of reward points for each validator.
	pub total: RewardPoint,
	/// The reward points earned by a given validator.
	pub individual: BTreeMap<AccountId, RewardPoint>,
}

impl<AccountId: Ord> Default for EraRewardPoints<AccountId> {
	fn default() -> Self {
		EraRewardPoints { total: Default::default(), individual: BTreeMap::new() }
	}
}

/// A destination account for payment.
#[derive(
	PartialEq,
	Eq,
	Copy,
	Clone,
	Encode,
	Decode,
	DecodeWithMemTracking,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub enum RewardDestination<AccountId> {
	/// Pay into the stash account, increasing the amount at stake accordingly.
	Staked,
	/// Pay into the stash account, not increasing the amount at stake.
	Stash,
	#[deprecated(
		note = "`Controller` will be removed after January 2024. Use `Account(controller)` instead."
	)]
	Controller,
	/// Pay into a specified account.
	Account(AccountId),
	/// Receive no reward.
	None,
}

/// Preference of what happens regarding validation.
#[derive(
	PartialEq,
	Eq,
	Clone,
	Encode,
	Decode,
	DecodeWithMemTracking,
	Debug,
	TypeInfo,
	Default,
	MaxEncodedLen,
)]
pub struct ValidatorPrefs {
	/// Reward that validator takes up-front; only the rest is split between themselves and
	/// nominators.
	#[codec(compact)]
	pub commission: Perbill,
	/// Whether or not this validator is accepting more nominations. If `true`, then no nominator
	/// who is not already nominating this validator may nominate them. By default, validators
	/// are accepting nominations.
	pub blocked: bool,
}

/// Just a Balance/BlockNumber tuple to encode when a chunk of funds will be unlocked.
#[derive(
	PartialEq, Eq, Clone, Encode, Decode, DecodeWithMemTracking, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct UnlockChunk<Balance: HasCompact + MaxEncodedLen> {
	/// Amount of funds to be unlocked.
	#[codec(compact)]
	pub value: Balance,
	/// Era number at which point it'll be unlocked.
	#[codec(compact)]
	pub era: EraIndex,
}

/// The ledger of a (bonded) stash.
///
/// Note: All the reads and mutations to the [`Ledger`], [`Bonded`] and [`Payee`] storage items
/// *MUST* be performed through the methods exposed by this struct, to ensure the consistency of
/// ledger's data and corresponding staking lock
///
/// TODO: move struct definition and full implementation into `/src/ledger.rs`. Currently
/// leaving here to enforce a clean PR diff, given how critical this logic is. Tracking issue
/// <https://github.com/paritytech/substrate/issues/14749>.
#[derive(
	PartialEqNoBound,
	EqNoBound,
	CloneNoBound,
	Encode,
	Decode,
	DecodeWithMemTracking,
	DebugNoBound,
	TypeInfo,
	MaxEncodedLen,
)]
#[scale_info(skip_type_params(T))]
pub struct StakingLedger<T: Config> {
	/// The stash account whose balance is actually locked and at stake.
	pub stash: T::AccountId,

	/// The total amount of the stash's balance that we are currently accounting for.
	/// It's just `active` plus all the `unlocking` balances.
	#[codec(compact)]
	pub total: BalanceOf<T>,

	/// The total amount of the stash's balance that will be at stake in any forthcoming
	/// rounds.
	#[codec(compact)]
	pub active: BalanceOf<T>,

	/// Any balance that is becoming free, which may eventually be transferred out of the stash
	/// (assuming it doesn't get slashed first). It is assumed that this will be treated as a first
	/// in, first out queue where the new (higher value) eras get pushed on the back.
	pub unlocking: BoundedVec<UnlockChunk<BalanceOf<T>>, T::MaxUnlockingChunks>,

	/// List of eras for which the stakers behind a validator have claimed rewards. Only updated
	/// for validators.
	///
	/// This is deprecated as of V14 in favor of `T::ClaimedRewards` and will be removed in future.
	/// Refer to issue <https://github.com/paritytech/polkadot-sdk/issues/433>
	pub legacy_claimed_rewards: BoundedVec<EraIndex, T::HistoryDepth>,

	/// The controller associated with this ledger's stash.
	///
	/// This is not stored on-chain, and is only bundled when the ledger is read from storage.
	/// Use [`Self::controller()`] function to get the controller associated with the ledger.
	#[codec(skip)]
	pub controller: Option<T::AccountId>,
}

/// State of a ledger with regards with its data and metadata integrity.
#[derive(PartialEq, Debug)]
enum LedgerIntegrityState {
	/// Ledger, bond and corresponding staking lock is OK.
	Ok,
	/// Ledger and/or bond is corrupted. This means that the bond has a ledger with a different
	/// stash than the bonded stash.
	Corrupted,
	/// Ledger was corrupted and it has been killed.
	CorruptedKilled,
	/// Ledger and bond are OK, however the ledger's stash lock is out of sync.
	LockCorrupted,
}

impl<T: Config> StakingLedger<T> {
	/// Remove entries from `unlocking` that are sufficiently old and reduce the
	/// total by the sum of their balances.
	fn consolidate_unlocked(self, current_era: EraIndex) -> Self {
		let mut total = self.total;
		let unlocking: BoundedVec<_, _> = self
			.unlocking
			.into_iter()
			.filter(|chunk| {
				if chunk.era > current_era {
					true
				} else {
					total = total.saturating_sub(chunk.value);
					false
				}
			})
			.collect::<Vec<_>>()
			.try_into()
			.expect(
				"filtering items from a bounded vec always leaves length less than bounds. qed",
			);

		Self {
			stash: self.stash,
			total,
			active: self.active,
			unlocking,
			legacy_claimed_rewards: self.legacy_claimed_rewards,
			controller: self.controller,
		}
	}

	/// Sets ledger total to the `new_total`.
	///
	/// Removes entries from `unlocking` upto `amount` starting from the oldest first.
	fn update_total_stake(mut self, new_total: BalanceOf<T>) -> Self {
		let old_total = self.total;
		self.total = new_total;
		debug_assert!(
			new_total <= old_total,
			"new_total {:?} must be <= old_total {:?}",
			new_total,
			old_total
		);

		let to_withdraw = old_total.defensive_saturating_sub(new_total);
		// accumulator to keep track of how much is withdrawn.
		// First we take out from active.
		let mut withdrawn = BalanceOf::<T>::zero();

		// first we try to remove stake from active
		if self.active >= to_withdraw {
			self.active -= to_withdraw;
			return self;
		} else {
			withdrawn += self.active;
			self.active = BalanceOf::<T>::zero();
		}

		// start removing from the oldest chunk.
		while let Some(last) = self.unlocking.last_mut() {
			if withdrawn.defensive_saturating_add(last.value) <= to_withdraw {
				withdrawn += last.value;
				self.unlocking.pop();
			} else {
				let diff = to_withdraw.defensive_saturating_sub(withdrawn);
				withdrawn += diff;
				last.value -= diff;
			}

			if withdrawn >= to_withdraw {
				break;
			}
		}

		self
	}

	/// Re-bond funds that were scheduled for unlocking.
	///
	/// Returns the updated ledger, and the amount actually rebonded.
	fn rebond(mut self, value: BalanceOf<T>) -> (Self, BalanceOf<T>) {
		let mut unlocking_balance = BalanceOf::<T>::zero();

		while let Some(last) = self.unlocking.last_mut() {
			if unlocking_balance.defensive_saturating_add(last.value) <= value {
				unlocking_balance += last.value;
				self.active += last.value;
				self.unlocking.pop();
			} else {
				let diff = value.defensive_saturating_sub(unlocking_balance);

				unlocking_balance += diff;
				self.active += diff;
				last.value -= diff;
			}

			if unlocking_balance >= value {
				break;
			}
		}

		(self, unlocking_balance)
	}

	/// Slash the staker for a given amount of balance.
	///
	/// This implements a proportional slashing system, whereby we set our preference to slash as
	/// such:
	///
	/// - If any unlocking chunks exist that are scheduled to be unlocked at `slash_era +
	///   bonding_duration` and onwards, the slash is divided equally between the active ledger and
	///   the unlocking chunks.
	/// - If no such chunks exist, then only the active balance is slashed.
	///
	/// Note that the above is only a *preference*. If for any reason the active ledger, with or
	/// without some portion of the unlocking chunks that are more justified to be slashed are not
	/// enough, then the slashing will continue and will consume as much of the active and unlocking
	/// chunks as needed.
	///
	/// This will never slash more than the given amount. If any of the chunks become dusted, the
	/// last chunk is slashed slightly less to compensate. Returns the amount of funds actually
	/// slashed.
	///
	/// `slash_era` is the era in which the slash (which is being enacted now) actually happened.
	///
	/// This calls `Config::OnStakingUpdate::on_slash` with information as to how the slash was
	/// applied.
	pub fn slash(
		&mut self,
		slash_amount: BalanceOf<T>,
		minimum_balance: BalanceOf<T>,
		slash_era: EraIndex,
	) -> BalanceOf<T> {
		if slash_amount.is_zero() {
			return Zero::zero();
		}

		use sp_runtime::PerThing as _;
		let mut remaining_slash = slash_amount;
		let pre_slash_total = self.total;

		// for a `slash_era = x`, any chunk that is scheduled to be unlocked at era `x + 28`
		// (assuming 28 is the bonding duration) onwards should be slashed.
		let slashable_chunks_start = slash_era.saturating_add(T::BondingDuration::get());

		// `Some(ratio)` if this is proportional, with `ratio`, `None` otherwise. In both cases, we
		// slash first the active chunk, and then `slash_chunks_priority`.
		let (maybe_proportional, slash_chunks_priority) = {
			if let Some(first_slashable_index) =
				self.unlocking.iter().position(|c| c.era >= slashable_chunks_start)
			{
				// If there exists a chunk who's after the first_slashable_start, then this is a
				// proportional slash, because we want to slash active and these chunks
				// proportionally.

				// The indices of the first chunk after the slash up through the most recent chunk.
				// (The most recent chunk is at greatest from this era)
				let affected_indices = first_slashable_index..self.unlocking.len();
				let unbonding_affected_balance =
					affected_indices.clone().fold(BalanceOf::<T>::zero(), |sum, i| {
						if let Some(chunk) = self.unlocking.get(i).defensive() {
							sum.saturating_add(chunk.value)
						} else {
							sum
						}
					});
				let affected_balance = self.active.saturating_add(unbonding_affected_balance);
				let ratio = Perquintill::from_rational_with_rounding(
					slash_amount,
					affected_balance,
					Rounding::Up,
				)
				.unwrap_or_else(|_| Perquintill::one());
				(
					Some(ratio),
					affected_indices.chain((0..first_slashable_index).rev()).collect::<Vec<_>>(),
				)
			} else {
				// We just slash from the last chunk to the most recent one, if need be.
				(None, (0..self.unlocking.len()).rev().collect::<Vec<_>>())
			}
		};

		// Helper to update `target` and the ledgers total after accounting for slashing `target`.
		log!(
			debug,
			"slashing {:?} for era {:?} out of {:?}, priority: {:?}, proportional = {:?}",
			slash_amount,
			slash_era,
			self,
			slash_chunks_priority,
			maybe_proportional,
		);

		let mut slash_out_of = |target: &mut BalanceOf<T>, slash_remaining: &mut BalanceOf<T>| {
			let mut slash_from_target = if let Some(ratio) = maybe_proportional {
				ratio.mul_ceil(*target)
			} else {
				*slash_remaining
			}
			// this is the total that that the slash target has. We can't slash more than
			// this anyhow!
			.min(*target)
			// this is the total amount that we would have wanted to slash
			// non-proportionally, a proportional slash should never exceed this either!
			.min(*slash_remaining);

			// slash out from *target exactly `slash_from_target`.
			*target = *target - slash_from_target;
			if *target < minimum_balance {
				// Slash the rest of the target if it's dust. This might cause the last chunk to be
				// slightly under-slashed, by at most `MaxUnlockingChunks * ED`, which is not a big
				// deal.
				slash_from_target =
					core::mem::replace(target, Zero::zero()).saturating_add(slash_from_target)
			}

			self.total = self.total.saturating_sub(slash_from_target);
			*slash_remaining = slash_remaining.saturating_sub(slash_from_target);
		};

		// If this is *not* a proportional slash, the active will always wiped to 0.
		slash_out_of(&mut self.active, &mut remaining_slash);

		let mut slashed_unlocking = BTreeMap::<_, _>::new();
		for i in slash_chunks_priority {
			if remaining_slash.is_zero() {
				break;
			}

			if let Some(chunk) = self.unlocking.get_mut(i).defensive() {
				slash_out_of(&mut chunk.value, &mut remaining_slash);
				// write the new slashed value of this chunk to the map.
				slashed_unlocking.insert(chunk.era, chunk.value);
			} else {
				break;
			}
		}

		// clean unlocking chunks that are set to zero.
		self.unlocking.retain(|c| !c.value.is_zero());

		let final_slashed_amount = pre_slash_total.saturating_sub(self.total);
		T::EventListeners::on_slash(
			&self.stash,
			self.active,
			&slashed_unlocking,
			final_slashed_amount,
		);
		final_slashed_amount
	}
}

/// A record of the nominations made by a specific account.
#[derive(
	PartialEqNoBound,
	EqNoBound,
	Clone,
	Encode,
	Decode,
	DecodeWithMemTracking,
	DebugNoBound,
	TypeInfo,
	MaxEncodedLen,
)]
#[codec(mel_bound())]
#[scale_info(skip_type_params(T))]
pub struct Nominations<T: Config> {
	/// The targets of nomination.
	pub targets: BoundedVec<T::AccountId, MaxNominationsOf<T>>,
	/// The era the nominations were submitted.
	///
	/// Except for initial nominations which are considered submitted at era 0.
	pub submitted_in: EraIndex,
	/// Whether the nominations have been suppressed. This can happen due to slashing of the
	/// validators, or other events that might invalidate the nomination.
	///
	/// NOTE: this for future proofing and is thus far not used.
	pub suppressed: bool,
}

/// Facade struct to encapsulate `PagedExposureMetadata` and a single page of `ExposurePage`.
///
/// This is useful where we need to take into account the validator's own stake and total exposure
/// in consideration, in addition to the individual nominators backing them.
#[derive(Encode, Decode, Debug, TypeInfo, PartialEq, Eq)]
pub struct PagedExposure<AccountId, Balance: HasCompact + codec::MaxEncodedLen> {
	exposure_metadata: PagedExposureMetadata<Balance>,
	exposure_page: ExposurePage<AccountId, Balance>,
}

impl<AccountId, Balance: HasCompact + Copy + AtLeast32BitUnsigned + codec::MaxEncodedLen>
	PagedExposure<AccountId, Balance>
{
	/// Create a new instance of `PagedExposure` from legacy clipped exposures.
	pub fn from_clipped(exposure: Exposure<AccountId, Balance>) -> Self {
		Self {
			exposure_metadata: PagedExposureMetadata {
				total: exposure.total,
				own: exposure.own,
				nominator_count: exposure.others.len() as u32,
				page_count: 1,
			},
			exposure_page: ExposurePage { page_total: exposure.total, others: exposure.others },
		}
	}

	/// Returns total exposure of this validator across pages
	pub fn total(&self) -> Balance {
		self.exposure_metadata.total
	}

	/// Returns total exposure of this validator for the current page
	pub fn page_total(&self) -> Balance {
		self.exposure_page.page_total + self.exposure_metadata.own
	}

	/// Returns validator's own stake that is exposed
	pub fn own(&self) -> Balance {
		self.exposure_metadata.own
	}

	/// Returns the portions of nominators stashes that are exposed in this page.
	pub fn others(&self) -> &Vec<IndividualExposure<AccountId, Balance>> {
		&self.exposure_page.others
	}
}

/// A pending slash record. The value of the slash has been computed but not applied yet,
/// rather deferred for several eras.
#[derive(Encode, Decode, Debug, TypeInfo, PartialEq, Eq, Clone, DecodeWithMemTracking)]
pub struct UnappliedSlash<AccountId, Balance: HasCompact> {
	/// The stash ID of the offending validator.
	pub validator: AccountId,
	/// The validator's own slash.
	pub own: Balance,
	/// All other slashed stakers and amounts.
	pub others: Vec<(AccountId, Balance)>,
	/// Reporters of the offence; bounty payout recipients.
	pub reporters: Vec<AccountId>,
	/// The amount of payout.
	pub payout: Balance,
}

impl<AccountId, Balance: HasCompact + Zero> UnappliedSlash<AccountId, Balance> {
	/// Initializes the default object using the given `validator`.
	pub fn default_from(validator: AccountId) -> Self {
		Self {
			validator,
			own: Zero::zero(),
			others: vec![],
			reporters: vec![],
			payout: Zero::zero(),
		}
	}
}

/// Something that defines the maximum number of nominations per nominator based on a curve.
///
/// The method `curve` implements the nomination quota curve and should not be used directly.
/// However, `get_quota` returns the bounded maximum number of nominations based on `fn curve` and
/// the nominator's balance.
pub trait NominationsQuota<Balance> {
	/// Strict maximum number of nominations that caps the nominations curve. This value can be
	/// used as the upper bound of the number of votes per nominator.
	type MaxNominations: Get<u32>;

	/// Returns the voter's nomination quota within reasonable bounds [`min`, `max`], where `min`
	/// is 1 and `max` is `Self::MaxNominations`.
	fn get_quota(balance: Balance) -> u32 {
		Self::curve(balance).clamp(1, Self::MaxNominations::get())
	}

	/// Returns the voter's nomination quota based on its balance and a curve.
	fn curve(balance: Balance) -> u32;
}

/// A nomination quota that allows up to MAX nominations for all validators.
pub struct FixedNominationsQuota<const MAX: u32>;
impl<Balance, const MAX: u32> NominationsQuota<Balance> for FixedNominationsQuota<MAX> {
	type MaxNominations = ConstU32<MAX>;

	fn curve(_: Balance) -> u32 {
		MAX
	}
}

/// Means for interacting with a specialized version of the `session` trait.
///
/// This is needed because `Staking` sets the `ValidatorIdOf` of the `pallet_session::Config`
pub trait SessionInterface<AccountId> {
	/// Report an offending validator.
	fn report_offence(validator: AccountId, severity: OffenceSeverity);
	/// Get the validators from session.
	fn validators() -> Vec<AccountId>;
	/// Prune historical session tries up to but not including the given index.
	fn prune_historical_up_to(up_to: SessionIndex);
}

impl<T: Config> SessionInterface<<T as frame_system::Config>::AccountId> for T
where
	T: pallet_session::Config<ValidatorId = <T as frame_system::Config>::AccountId>,
	T: pallet_session::historical::Config,
	T::SessionHandler: pallet_session::SessionHandler<<T as frame_system::Config>::AccountId>,
	T::SessionManager: pallet_session::SessionManager<<T as frame_system::Config>::AccountId>,
	T::ValidatorIdOf: Convert<
		<T as frame_system::Config>::AccountId,
		Option<<T as frame_system::Config>::AccountId>,
	>,
{
	fn report_offence(
		validator: <T as frame_system::Config>::AccountId,
		severity: OffenceSeverity,
	) {
		<pallet_session::Pallet<T>>::report_offence(validator, severity)
	}

	fn validators() -> Vec<<T as frame_system::Config>::AccountId> {
		<pallet_session::Pallet<T>>::validators()
	}

	fn prune_historical_up_to(up_to: SessionIndex) {
		<pallet_session::historical::Pallet<T>>::prune_up_to(up_to);
	}
}

impl<AccountId> SessionInterface<AccountId> for () {
	fn report_offence(_validator: AccountId, _severity: OffenceSeverity) {
		()
	}
	fn validators() -> Vec<AccountId> {
		Vec::new()
	}
	fn prune_historical_up_to(_: SessionIndex) {
		()
	}
}

/// Adaptor to turn a `PiecewiseLinear` curve definition into an `EraPayout` impl, used for
/// backwards compatibility.
pub struct ConvertCurve<T>(core::marker::PhantomData<T>);
impl<Balance, T> sp_staking::EraPayout<Balance> for ConvertCurve<T>
where
	Balance: AtLeast32BitUnsigned + Clone + Copy,
	T: Get<&'static PiecewiseLinear<'static>>,
{
	fn era_payout(
		total_staked: Balance,
		total_issuance: Balance,
		era_duration_millis: u64,
	) -> (Balance, Balance) {
		let (validator_payout, max_payout) = inflation::compute_total_payout(
			T::get(),
			total_staked,
			total_issuance,
			// Duration of era; more than u64::MAX is rewarded as u64::MAX.
			era_duration_millis,
		);
		let rest = max_payout.saturating_sub(validator_payout);
		(validator_payout, rest)
	}
}

/// Mode of era-forcing.
#[derive(
	Copy,
	Clone,
	PartialEq,
	Eq,
	Encode,
	Decode,
	DecodeWithMemTracking,
	Debug,
	TypeInfo,
	MaxEncodedLen,
	serde::Serialize,
	serde::Deserialize,
)]
pub enum Forcing {
	/// Not forcing anything - just let whatever happen.
	NotForcing,
	/// Force a new era, then reset to `NotForcing` as soon as it is done.
	/// Note that this will force to trigger an election until a new era is triggered, if the
	/// election failed, the next session end will trigger a new election again, until success.
	ForceNew,
	/// Avoid a new era indefinitely.
	ForceNone,
	/// Force a new era at the end of all sessions indefinitely.
	ForceAlways,
}

impl Default for Forcing {
	fn default() -> Self {
		Forcing::NotForcing
	}
}

/// Identify a current validator with a default [`Exposure`].
///
/// Returns `Some(Exposure::default())` when the account is in the active validator set, and `None`
/// otherwise. This keeps `FullIdentification = Exposure<..>` for runtimes that still need to decode
/// historical session identification data of that shape, without constructing a full exposure for
/// new identification requests.
///
/// Prefer [`UnitIdentificationOf`] for fresh runtimes that do not need exposure-shaped
/// identification.
///
/// A typical usage of this type is:
///
/// ```ignore
/// impl pallet_session::historical::Config for Runtime {
///     type FullIdentification = sp_staking::Exposure<AccountId, Balance>;
///     type IdentificationOf = pallet_staking::DefaultExposureOf<Self>
/// }
/// ```
pub struct DefaultExposureOf<T>(core::marker::PhantomData<T>);

impl<T: Config> Convert<T::AccountId, Option<Exposure<T::AccountId, BalanceOf<T>>>>
	for DefaultExposureOf<T>
{
	fn convert(validator: T::AccountId) -> Option<Exposure<T::AccountId, BalanceOf<T>>> {
		T::SessionInterface::validators()
			.contains(&validator)
			.then_some(Default::default())
	}
}

/// An identification type that signifies the existence of a validator by returning `Some(())`, and
/// `None` otherwise. Also see the documentation of [`DefaultExposureOf`] for more info.
///
/// ```ignore
/// impl pallet_session::historical::Config for Runtime {
///     type FullIdentification = ();
///     type IdentificationOf = pallet_staking::UnitIdentificationOf<Self>
/// }
/// ```
pub struct UnitIdentificationOf<T>(core::marker::PhantomData<T>);
impl<T: Config> Convert<T::AccountId, Option<()>> for UnitIdentificationOf<T> {
	fn convert(validator: T::AccountId) -> Option<()> {
		DefaultExposureOf::<T>::convert(validator).map(|_default_exposure| ())
	}
}

/// Filter historical offences out and only allow those from the bonding period.
pub struct FilterHistoricalOffences<T, R> {
	_inner: core::marker::PhantomData<(T, R)>,
}

impl<T, Reporter, Offender, R, O> ReportOffence<Reporter, Offender, O>
	for FilterHistoricalOffences<Pallet<T>, R>
where
	T: Config,
	R: ReportOffence<Reporter, Offender, O>,
	O: Offence<Offender>,
{
	fn report_offence(reporters: Vec<Reporter>, offence: O) -> Result<(), OffenceError> {
		// Disallow any slashing from before the current bonding period.
		let offence_session = offence.session_index();
		let bonded_eras = BondedEras::<T>::get();

		if bonded_eras.first().filter(|(_, start)| offence_session >= *start).is_some() {
			R::report_offence(reporters, offence)
		} else {
			<Pallet<T>>::deposit_event(Event::<T>::OldSlashingReportDiscarded {
				session_index: offence_session,
			});
			Ok(())
		}
	}

	fn is_known_offence(offenders: &[Offender], time_slot: &O::Slot) -> bool {
		R::is_known_offence(offenders, time_slot)
	}
}

/// Wrapper struct for Era-related information. It is not a pure encapsulation as these storage
/// items can be accessed directly but nevertheless, its recommended to use `EraInfo` where we
/// can and add more functions to it as needed.
pub struct EraInfo<T>(core::marker::PhantomData<T>);
impl<T: Config> EraInfo<T> {
	/// Returns true if validator has one or more page of era rewards not claimed yet.
	// Also looks at legacy storage that can be cleaned up after #433.
	pub fn pending_rewards(era: EraIndex, validator: &T::AccountId) -> bool {
		let page_count = if let Some(overview) = <ErasStakersOverview<T>>::get(&era, validator) {
			overview.page_count
		} else {
			if <ErasStakers<T>>::contains_key(era, validator) {
				// this means non paged exposure, and we treat them as single paged.
				1
			} else {
				// if no exposure, then no rewards to claim.
				return false;
			}
		};

		// check if era is marked claimed in legacy storage.
		if <Ledger<T>>::get(validator)
			.map(|l| l.legacy_claimed_rewards.contains(&era))
			.unwrap_or_default()
		{
			return false;
		}

		ClaimedRewards::<T>::get(era, validator).len() < page_count as usize
	}

	/// Temporary function which looks at both (1) passed param `T::StakingLedger` for legacy
	/// non-paged rewards, and (2) `T::ClaimedRewards` for paged rewards. This function can be
	/// removed once `T::HistoryDepth` eras have passed and none of the older non-paged rewards
	/// are relevant/claimable.
	// Refer tracker issue for cleanup: https://github.com/paritytech/polkadot-sdk/issues/433
	pub(crate) fn is_rewards_claimed_with_legacy_fallback(
		era: EraIndex,
		ledger: &StakingLedger<T>,
		validator: &T::AccountId,
		page: Page,
	) -> bool {
		ledger.legacy_claimed_rewards.binary_search(&era).is_ok() ||
			Self::is_rewards_claimed(era, validator, page)
	}

	/// Check if the rewards for the given era and page index have been claimed.
	///
	/// This is only used for paged rewards. Once older non-paged rewards are no longer
	/// relevant, `is_rewards_claimed_with_legacy_fallback` can be removed and this function can
	/// be made public.
	fn is_rewards_claimed(era: EraIndex, validator: &T::AccountId, page: Page) -> bool {
		ClaimedRewards::<T>::get(era, validator).contains(&page)
	}

	/// Get exposure for a validator at a given era and page.
	///
	/// This builds a paged exposure from `PagedExposureMetadata` and `ExposurePage` of the
	/// validator. For older non-paged exposure, it returns the clipped exposure directly.
	pub fn get_paged_exposure(
		era: EraIndex,
		validator: &T::AccountId,
		page: Page,
	) -> Option<PagedExposure<T::AccountId, BalanceOf<T>>> {
		let overview = <ErasStakersOverview<T>>::get(&era, validator);

		// return clipped exposure if page zero and paged exposure does not exist
		// exists for backward compatibility and can be removed as part of #13034
		if overview.is_none() && page == 0 {
			return Some(PagedExposure::from_clipped(<ErasStakersClipped<T>>::get(era, validator)));
		}

		// no exposure for this validator
		if overview.is_none() {
			return None;
		}

		let overview = overview.expect("checked above; qed");

		// validator stake is added only in page zero
		let validator_stake = if page == 0 { overview.own } else { Zero::zero() };

		// since overview is present, paged exposure will always be present except when a
		// validator only has its own stake and no nominator stake.
		let exposure_page = <ErasStakersPaged<T>>::get((era, validator, page)).unwrap_or_default();

		// build the exposure
		Some(PagedExposure {
			exposure_metadata: PagedExposureMetadata { own: validator_stake, ..overview },
			exposure_page,
		})
	}

	/// Get full exposure of the validator at a given era.
	pub fn get_full_exposure(
		era: EraIndex,
		validator: &T::AccountId,
	) -> Exposure<T::AccountId, BalanceOf<T>> {
		let overview = <ErasStakersOverview<T>>::get(&era, validator);

		if overview.is_none() {
			return ErasStakers::<T>::get(era, validator);
		}

		let overview = overview.expect("checked above; qed");

		let mut others = Vec::with_capacity(overview.nominator_count as usize);
		for page in 0..overview.page_count {
			let nominators = <ErasStakersPaged<T>>::get((era, validator, page));
			others.append(&mut nominators.map(|n| n.others).defensive_unwrap_or_default());
		}

		Exposure { total: overview.total, own: overview.own, others }
	}

	/// Returns the number of pages of exposure a validator has for the given era.
	///
	/// For eras where paged exposure does not exist, this returns 1 to keep backward compatibility.
	pub(crate) fn get_page_count(era: EraIndex, validator: &T::AccountId) -> Page {
		<ErasStakersOverview<T>>::get(&era, validator)
			.map(|overview| {
				if overview.page_count == 0 && overview.own > Zero::zero() {
					// Even though there are no nominator pages, there is still validator's own
					// stake exposed which needs to be paid out in a page.
					1
				} else {
					overview.page_count
				}
			})
			// Always returns 1 page for older non-paged exposure.
			// FIXME: Can be cleaned up with issue #13034.
			.unwrap_or(1)
	}

	/// Returns the next page that can be claimed or `None` if nothing to claim.
	pub(crate) fn get_next_claimable_page(
		era: EraIndex,
		validator: &T::AccountId,
		ledger: &StakingLedger<T>,
	) -> Option<Page> {
		if Self::is_non_paged_exposure(era, validator) {
			return match ledger.legacy_claimed_rewards.binary_search(&era) {
				// already claimed
				Ok(_) => None,
				// Non-paged exposure is considered as a single page
				Err(_) => Some(0),
			};
		}

		// Find next claimable page of paged exposure.
		let page_count = Self::get_page_count(era, validator);
		let all_claimable_pages: Vec<Page> = (0..page_count).collect();
		let claimed_pages = ClaimedRewards::<T>::get(era, validator);

		all_claimable_pages.into_iter().find(|p| !claimed_pages.contains(p))
	}

	/// Checks if exposure is paged or not.
	fn is_non_paged_exposure(era: EraIndex, validator: &T::AccountId) -> bool {
		<ErasStakersClipped<T>>::contains_key(&era, validator)
	}

	/// Returns validator commission for this era and page.
	pub(crate) fn get_validator_commission(
		era: EraIndex,
		validator_stash: &T::AccountId,
	) -> Perbill {
		<ErasValidatorPrefs<T>>::get(&era, validator_stash).commission
	}

	/// Creates an entry to track whether validator reward has been claimed for a given era and
	/// page. Noop if already claimed.
	pub(crate) fn set_rewards_as_claimed(era: EraIndex, validator: &T::AccountId, page: Page) {
		let mut claimed_pages = ClaimedRewards::<T>::get(era, validator);

		// this should never be called if the reward has already been claimed
		if claimed_pages.contains(&page) {
			defensive!("Trying to set an already claimed reward");
			// nevertheless don't do anything since the page already exists in claimed rewards.
			return;
		}

		// add page to claimed entries
		claimed_pages.push(page);
		ClaimedRewards::<T>::insert(era, validator, claimed_pages);
	}

	/// Store exposure for elected validators at start of an era.
	pub fn set_exposure(
		era: EraIndex,
		validator: &T::AccountId,
		exposure: Exposure<T::AccountId, BalanceOf<T>>,
	) {
		let page_size = T::MaxExposurePageSize::get().defensive_max(1);

		let nominator_count = exposure.others.len();
		// expected page count is the number of nominators divided by the page size, rounded up.
		let expected_page_count = nominator_count
			.defensive_saturating_add((page_size as usize).defensive_saturating_sub(1))
			.saturating_div(page_size as usize);

		let (exposure_metadata, exposure_pages) = exposure.into_pages(page_size);
		defensive_assert!(exposure_pages.len() == expected_page_count, "unexpected page count");

		<ErasStakersOverview<T>>::insert(era, &validator, &exposure_metadata);
		exposure_pages.iter().enumerate().for_each(|(page, paged_exposure)| {
			<ErasStakersPaged<T>>::insert((era, &validator, page as Page), &paged_exposure);
		});
	}

	/// Store total exposure for all the elected validators in the era.
	pub(crate) fn set_total_stake(era: EraIndex, total_stake: BalanceOf<T>) {
		<ErasTotalStake<T>>::insert(era, total_stake);
	}
}

/// A utility struct that provides a way to check if a given account is a staker.
///
/// This struct implements the `Contains` trait, allowing it to determine whether
/// a particular account is currently staking by checking if the account exists in
/// the staking ledger.
pub struct AllStakers<T: Config>(core::marker::PhantomData<T>);

impl<T: Config> Contains<T::AccountId> for AllStakers<T> {
	/// Checks if the given account ID corresponds to a staker.
	///
	/// # Returns
	/// - `true` if the account has an entry in the staking ledger (indicating it is staking).
	/// - `false` otherwise.
	fn contains(account: &T::AccountId) -> bool {
		Ledger::<T>::contains_key(account)
	}
}

/// Configurations of the benchmarking of the pallet.
pub trait BenchmarkingConfig {
	/// The maximum number of validators to use.
	type MaxValidators: Get<u32>;
	/// The maximum number of nominators to use.
	type MaxNominators: Get<u32>;
}

/// A mock benchmarking config for pallet-staking.
///
/// Should only be used for testing.
#[cfg(feature = "std")]
pub struct TestBenchmarkingConfig;

#[cfg(feature = "std")]
impl BenchmarkingConfig for TestBenchmarkingConfig {
	type MaxValidators = frame_support::traits::ConstU32<100>;
	type MaxNominators = frame_support::traits::ConstU32<100>;
}
