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

#![cfg_attr(not(feature = "std"), no_std)]

//! A crate which contains primitives that are useful for implementation that uses staking
//! approaches in general. Definitions related to sessions, slashing, etc go here.

extern crate alloc;

use crate::currency_to_vote::CurrencyToVote;
use alloc::{collections::btree_map::BTreeMap, vec, vec::Vec};
use codec::{Decode, DecodeWithMemTracking, Encode, FullCodec, HasCompact, MaxEncodedLen};
use core::ops::{Add, AddAssign, Sub, SubAssign};
use scale_info::TypeInfo;
use sp_runtime::{
	traits::{AtLeast32BitUnsigned, Zero},
	Debug, DispatchError, DispatchResult, Perbill, Saturating,
};

pub mod budget;
pub mod offence;

pub mod currency_to_vote;

/// Simple index type with which we can count sessions.
pub type SessionIndex = u32;

/// Counter for the number of eras that have passed.
pub type EraIndex = u32;

/// Type for identifying a page.
pub type Page = u32;
/// Representation of a staking account, which may be a stash or controller account.
///
/// Note: once the controller is completely deprecated, this enum can also be deprecated in favor of
/// the stash account. Tracking issue: <https://github.com/paritytech/substrate/issues/6927>.
#[derive(Clone, Debug)]
pub enum StakingAccount<AccountId> {
	Stash(AccountId),
	Controller(AccountId),
}

#[cfg(feature = "std")]
impl<AccountId> From<AccountId> for StakingAccount<AccountId> {
	fn from(account: AccountId) -> Self {
		StakingAccount::Stash(account)
	}
}

/// Representation of the status of a staker.
#[derive(Debug, TypeInfo)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone))]
pub enum StakerStatus<AccountId> {
	/// Chilling.
	Idle,
	/// Declaring desire in validate, i.e author blocks.
	Validator,
	/// Declaring desire to nominate, delegate, or generally approve of the given set of others.
	Nominator(Vec<AccountId>),
}

/// A struct that reflects stake that an account has in the staking system. Provides a set of
/// methods to operate on it's properties. Aimed at making `StakingInterface` more concise.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub struct Stake<Balance> {
	/// The total stake that `stash` has in the staking system. This includes the
	/// `active` stake, and any funds currently in the process of unbonding via
	/// [`StakingInterface::unbond`].
	///
	/// # Note
	///
	/// This is only guaranteed to reflect the amount locked by the staking system. If there are
	/// non-staking locks on the bonded pair's balance this amount is going to be larger in
	/// reality.
	pub total: Balance,
	/// The total amount of the stash's balance that will be at stake in any forthcoming
	/// rounds.
	pub active: Balance,
}

/// A generic staking event listener.
///
/// Note that the interface is designed in a way that the events are fired post-action, so any
/// pre-action data that is needed needs to be passed to interface methods. The rest of the data can
/// be retrieved by using `StakingInterface`.
#[impl_trait_for_tuples::impl_for_tuples(10)]
pub trait OnStakingUpdate<AccountId, Balance> {
	/// Fired when the stake amount of someone updates.
	///
	/// This is effectively any changes to the bond amount, such as bonding more funds, and
	/// unbonding.
	fn on_stake_update(_who: &AccountId, _prev_stake: Option<Stake<Balance>>) {}

	/// Fired when someone sets their intention to nominate.
	///
	/// This should never be fired for existing nominators.
	fn on_nominator_add(_who: &AccountId) {}

	/// Fired when an existing nominator updates their nominations.
	///
	/// Note that this is not fired when a nominator changes their stake. For that,
	/// `on_stake_update` should be used, followed by querying whether `who` was a validator or a
	/// nominator.
	fn on_nominator_update(_who: &AccountId, _prev_nominations: Vec<AccountId>) {}

	/// Fired when someone removes their intention to nominate, either due to chill or validating.
	///
	/// The set of nominations at the time of removal is provided as it can no longer be fetched in
	/// any way.
	fn on_nominator_remove(_who: &AccountId, _nominations: Vec<AccountId>) {}

	/// Fired when someone sets their intention to validate.
	///
	/// Note validator preference changes are not communicated, but could be added if needed.
	fn on_validator_add(_who: &AccountId) {}

	/// Fired when an existing validator updates their preferences.
	///
	/// Note validator preference changes are not communicated, but could be added if needed.
	fn on_validator_update(_who: &AccountId) {}

	/// Fired when someone removes their intention to validate, either due to chill or nominating.
	fn on_validator_remove(_who: &AccountId) {}

	/// Fired when someone is fully unstaked.
	fn on_unstake(_who: &AccountId) {}

	/// Fired when a staker is slashed.
	///
	/// * `stash` - The stash of the staker whom the slash was applied to.
	/// * `slashed_active` - The new bonded balance of the staker after the slash was applied.
	/// * `slashed_unlocking` - A map of slashed eras, and the balance of that unlocking chunk after
	///   the slash is applied. Any era not present in the map is not affected at all.
	/// * `slashed_total` - The aggregated balance that was lost due to the slash.
	fn on_slash(
		_stash: &AccountId,
		_slashed_active: Balance,
		_slashed_unlocking: &BTreeMap<EraIndex, Balance>,
		_slashed_total: Balance,
	) {
	}

	/// Fired when a portion of a staker's balance has been withdrawn.
	fn on_withdraw(_stash: &AccountId, _amount: Balance) {}
}

/// A generic representation of a staking implementation.
///
/// This interface uses the terminology of NPoS, but it is aims to be generic enough to cover other
/// implementations as well.
pub trait StakingInterface {
	/// Balance type used by the staking system.
	type Balance: Sub<Output = Self::Balance>
		+ Ord
		+ PartialEq
		+ Default
		+ Copy
		+ MaxEncodedLen
		+ FullCodec
		+ TypeInfo
		+ Saturating;

	/// AccountId type used by the staking system.
	type AccountId: Clone + core::fmt::Debug;

	/// Means of converting Currency to VoteWeight.
	type CurrencyToVote: CurrencyToVote<Self::Balance>;

	/// The minimum amount required to bond in order to set nomination intentions. This does not
	/// necessarily mean the nomination will be counted in an election, but instead just enough to
	/// be stored as a nominator. In other words, this is the minimum amount to register the
	/// intention to nominate.
	fn minimum_nominator_bond() -> Self::Balance;

	/// The minimum amount required to bond in order to set validation intentions.
	fn minimum_validator_bond() -> Self::Balance;

	/// Return a stash account that is controlled by a `controller`.
	///
	/// ## Note
	///
	/// The controller abstraction is not permanent and might go away. Avoid using this as much as
	/// possible.
	fn stash_by_ctrl(controller: &Self::AccountId) -> Result<Self::AccountId, DispatchError>;

	/// Number of eras that staked funds must remain bonded for.
	///
	/// This is the full bonding duration used by validators and recent ex-validators.
	fn bonding_duration() -> EraIndex;

	/// Number of eras that staked funds of a pure nominator must remain bonded for.
	///
	/// Same as [`Self::bonding_duration`] by default, but can be lower for pure nominators
	/// (who have not been validators in recent eras) when nominators are not slashable.
	///
	/// Note: The actual unbonding duration for a specific account may vary:
	/// - Validators always use [`Self::bonding_duration`]
	/// - Nominators who were recently validators use [`Self::bonding_duration`]
	/// - Pure nominators (never validators, or not validators in recent eras) may use a shorter
	///   duration when not slashable
	fn nominator_bonding_duration() -> EraIndex {
		Self::bonding_duration()
	}

	/// The current era index.
	///
	/// This should be the latest planned era that the staking system knows about.
	fn current_era() -> EraIndex;

	/// Returns the [`Stake`] of `who`.
	fn stake(who: &Self::AccountId) -> Result<Stake<Self::Balance>, DispatchError>;

	/// Total stake of a staker, `Err` if not a staker.
	fn total_stake(who: &Self::AccountId) -> Result<Self::Balance, DispatchError> {
		Self::stake(who).map(|s| s.total)
	}

	/// Total active portion of a staker's [`Stake`], `Err` if not a staker.
	fn active_stake(who: &Self::AccountId) -> Result<Self::Balance, DispatchError> {
		Self::stake(who).map(|s| s.active)
	}

	/// Returns whether a staker is unbonding, `Err` if not a staker at all.
	fn is_unbonding(who: &Self::AccountId) -> Result<bool, DispatchError> {
		Self::stake(who).map(|s| s.active != s.total)
	}

	/// Returns whether a staker is FULLY unbonding, `Err` if not a staker at all.
	fn fully_unbond(who: &Self::AccountId) -> DispatchResult {
		Self::unbond(who, Self::stake(who)?.active)
	}

	/// Bond (lock) `value` of `who`'s balance, while forwarding any rewards to `payee`.
	fn bond(who: &Self::AccountId, value: Self::Balance, payee: &Self::AccountId)
		-> DispatchResult;

	/// Have `who` nominate `validators`.
	fn nominate(who: &Self::AccountId, validators: Vec<Self::AccountId>) -> DispatchResult;

	/// Chill `who`.
	fn chill(who: &Self::AccountId) -> DispatchResult;

	/// Bond some extra amount in `who`'s free balance against the active bonded balance of
	/// the account. The amount extra actually bonded will never be more than `who`'s free
	/// balance.
	fn bond_extra(who: &Self::AccountId, extra: Self::Balance) -> DispatchResult;

	/// Schedule a portion of the active bonded balance to be unlocked at era
	/// [Self::current_era] + [`Self::bonding_duration`].
	///
	/// Once the unlock era has been reached, [`Self::withdraw_unbonded`] can be called to unlock
	/// the funds.
	///
	/// The amount of times this can be successfully called is limited based on how many distinct
	/// eras funds are schedule to unlock in. Calling [`Self::withdraw_unbonded`] after some unlock
	/// schedules have reached their unlocking era should allow more calls to this function.
	fn unbond(stash: &Self::AccountId, value: Self::Balance) -> DispatchResult;

	/// Set the reward destination for the ledger associated with the stash.
	fn set_payee(stash: &Self::AccountId, reward_acc: &Self::AccountId) -> DispatchResult;

	/// Unlock any funds schedule to unlock before or at the current era.
	///
	/// Returns whether the stash was killed because of this withdraw or not.
	fn withdraw_unbonded(
		stash: Self::AccountId,
		num_slashing_spans: u32,
	) -> Result<bool, DispatchError>;

	/// The ideal number of active validators.
	fn desired_validator_count() -> u32;

	/// Whether or not there is an ongoing election.
	fn election_ongoing() -> bool;

	/// Force a current staker to become completely unstaked, immediately.
	fn force_unstake(who: Self::AccountId) -> DispatchResult;

	/// Checks whether an account `staker` has been exposed in an era.
	fn is_exposed_in_era(who: &Self::AccountId, era: &EraIndex) -> bool;

	/// Return the status of the given staker, `Err` if not staked at all.
	fn status(who: &Self::AccountId) -> Result<StakerStatus<Self::AccountId>, DispatchError>;

	/// Checks whether or not this is a validator account.
	fn is_validator(who: &Self::AccountId) -> bool {
		Self::status(who).map(|s| matches!(s, StakerStatus::Validator)).unwrap_or(false)
	}

	/// Checks whether the staker is a virtual account.
	///
	/// A virtual staker is an account whose locks are not managed by the [`StakingInterface`]
	/// implementation but by an external pallet. See [`StakingUnchecked::virtual_bond`] for more
	/// details.
	fn is_virtual_staker(who: &Self::AccountId) -> bool;

	/// Get the nominations of a stash, if they are a nominator, `None` otherwise.
	fn nominations(who: &Self::AccountId) -> Option<Vec<Self::AccountId>> {
		match Self::status(who) {
			Ok(StakerStatus::Nominator(t)) => Some(t),
			_ => None,
		}
	}

	/// Returns the fraction of the slash to be rewarded to reporter.
	fn slash_reward_fraction() -> Perbill;

	#[cfg(feature = "runtime-benchmarks")]
	fn max_exposure_page_size() -> Page;

	#[cfg(feature = "runtime-benchmarks")]
	fn add_era_stakers(
		current_era: &EraIndex,
		stash: &Self::AccountId,
		exposures: Vec<(Self::AccountId, Self::Balance)>,
	);

	/// Benchmark and test helper to set both active and current era.
	#[cfg(any(feature = "std", feature = "runtime-benchmarks"))]
	fn set_era(era: EraIndex);
}

/// Set of low level apis to manipulate staking ledger.
///
/// These apis bypass some or all safety checks and should only be used if you know what you are
/// doing.
pub trait StakingUnchecked: StakingInterface {
	/// Migrate an existing staker to a virtual staker.
	///
	/// It would release all funds held by the implementation pallet.
	fn migrate_to_virtual_staker(who: &Self::AccountId) -> DispatchResult;

	/// Book-keep a new bond for `keyless_who` without applying any locks (hence virtual).
	///
	/// It is important that `keyless_who` is a keyless account and therefore cannot interact with
	/// staking pallet directly. Caller is responsible for ensuring the passed amount is locked and
	/// valid.
	fn virtual_bond(
		keyless_who: &Self::AccountId,
		value: Self::Balance,
		payee: &Self::AccountId,
	) -> DispatchResult;

	/// Migrate a virtual staker to a direct staker.
	///
	/// Only used for testing.
	#[cfg(feature = "runtime-benchmarks")]
	fn migrate_to_direct_staker(who: &Self::AccountId);
}

/// The amount of exposure for an era that an individual nominator has (susceptible to slashing).
#[derive(
	PartialEq,
	Eq,
	PartialOrd,
	Ord,
	Clone,
	Encode,
	Decode,
	DecodeWithMemTracking,
	Debug,
	TypeInfo,
	Copy,
)]
pub struct IndividualExposure<AccountId, Balance: HasCompact> {
	/// The stash account of the nominator in question.
	pub who: AccountId,
	/// Amount of funds exposed.
	#[codec(compact)]
	pub value: Balance,
}

/// A snapshot of the stake backing a single validator in the system.
#[derive(
	PartialEq, Eq, PartialOrd, Ord, Clone, Encode, Decode, DecodeWithMemTracking, Debug, TypeInfo,
)]
pub struct Exposure<AccountId, Balance: HasCompact> {
	/// The total balance backing this validator.
	#[codec(compact)]
	pub total: Balance,
	/// The validator's own stash that is exposed.
	#[codec(compact)]
	pub own: Balance,
	/// The portions of nominators stashes that are exposed.
	pub others: Vec<IndividualExposure<AccountId, Balance>>,
}

impl<AccountId, Balance: Default + HasCompact> Default for Exposure<AccountId, Balance> {
	fn default() -> Self {
		Self { total: Default::default(), own: Default::default(), others: vec![] }
	}
}

impl<
		AccountId: Clone,
		Balance: HasCompact + AtLeast32BitUnsigned + Copy + codec::MaxEncodedLen,
	> Exposure<AccountId, Balance>
{
	/// Splits self into two instances of exposures.
	///
	/// `n_others` individual exposures are consumed from self and returned as part of the new
	/// exposure.
	///
	/// Since this method splits `others` of a single exposure, `total.own` will be the same for
	/// both `self` and the returned exposure.
	pub fn split_others(&mut self, n_others: u32) -> Self {
		let head_others: Vec<_> =
			self.others.drain(..(n_others as usize).min(self.others.len())).collect();

		let total_others_head: Balance = head_others
			.iter()
			.fold(Zero::zero(), |acc: Balance, o| acc.saturating_add(o.value));

		self.total = self.total.saturating_sub(total_others_head);

		Self {
			total: total_others_head.saturating_add(self.own),
			own: self.own,
			others: head_others,
		}
	}

	/// Converts an `Exposure` into `PagedExposureMetadata` and multiple chunks of
	/// `IndividualExposure` with each chunk having maximum of `page_size` elements.
	pub fn into_pages(
		self,
		page_size: Page,
	) -> (PagedExposureMetadata<Balance>, Vec<ExposurePage<AccountId, Balance>>) {
		let individual_chunks = self.others.chunks(page_size as usize);
		let mut exposure_pages: Vec<ExposurePage<AccountId, Balance>> =
			Vec::with_capacity(individual_chunks.len());

		for chunk in individual_chunks {
			let mut page_total: Balance = Zero::zero();
			let mut others: Vec<IndividualExposure<AccountId, Balance>> =
				Vec::with_capacity(chunk.len());
			for individual in chunk.iter() {
				page_total.saturating_accrue(individual.value);
				others.push(IndividualExposure {
					who: individual.who.clone(),
					value: individual.value,
				})
			}
			exposure_pages.push(ExposurePage { page_total, others });
		}

		(
			PagedExposureMetadata {
				total: self.total,
				own: self.own,
				nominator_count: self.others.len() as u32,
				page_count: exposure_pages.len() as Page,
			},
			exposure_pages,
		)
	}
}

/// A snapshot of the stake backing a single validator in the system.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Encode, Decode, Debug, TypeInfo)]
pub struct ExposurePage<AccountId, Balance: HasCompact> {
	/// The total balance of this chunk/page.
	#[codec(compact)]
	pub page_total: Balance,
	/// The portions of nominators stashes that are exposed.
	pub others: Vec<IndividualExposure<AccountId, Balance>>,
}

impl<A, B: Default + HasCompact> Default for ExposurePage<A, B> {
	fn default() -> Self {
		ExposurePage { page_total: Default::default(), others: vec![] }
	}
}

/// Returns an exposure page from a set of individual exposures.
impl<A, B: HasCompact + Default + AddAssign + SubAssign + Clone> From<Vec<IndividualExposure<A, B>>>
	for ExposurePage<A, B>
{
	fn from(exposures: Vec<IndividualExposure<A, B>>) -> Self {
		exposures.into_iter().fold(ExposurePage::default(), |mut page, e| {
			page.page_total += e.value.clone();
			page.others.push(e);
			page
		})
	}
}

/// Metadata for Paged Exposure of a validator such as total stake across pages and page count.
///
/// In combination with the associated `ExposurePage`s, it can be used to reconstruct a full
/// `Exposure` set of a validator. This is useful for cases where we want to query full set of
/// `Exposure` as one page (for backward compatibility).
#[derive(
	PartialEq,
	Eq,
	PartialOrd,
	Ord,
	Clone,
	Encode,
	Decode,
	Debug,
	TypeInfo,
	Default,
	MaxEncodedLen,
	Copy,
)]
pub struct PagedExposureMetadata<Balance: HasCompact + codec::MaxEncodedLen> {
	/// The total balance backing this validator.
	#[codec(compact)]
	pub total: Balance,
	/// The validator's own stash that is exposed.
	#[codec(compact)]
	pub own: Balance,
	/// Number of nominators backing this validator.
	pub nominator_count: u32,
	/// Number of pages of nominators.
	pub page_count: Page,
}

impl<Balance> PagedExposureMetadata<Balance>
where
	Balance: HasCompact
		+ codec::MaxEncodedLen
		+ Add<Output = Balance>
		+ Sub<Output = Balance>
		+ sp_runtime::Saturating
		+ PartialEq
		+ Copy
		+ sp_runtime::traits::Debug,
{
	/// Consumes self and returns the result of the metadata updated with `other_balances` and
	/// of adding `other_num` nominators to the metadata.
	///
	/// `Max` is a getter of the maximum number of nominators per page.
	pub fn update_with<Max: sp_core::Get<u32>>(
		self,
		others_balance: Balance,
		others_num: u32,
	) -> Self {
		let page_limit = Max::get().max(1);
		let new_nominator_count = self.nominator_count.saturating_add(others_num);
		let new_page_count = new_nominator_count
			.saturating_add(page_limit)
			.saturating_sub(1)
			.saturating_div(page_limit);

		Self {
			total: self.total.saturating_add(others_balance),
			own: self.own,
			nominator_count: new_nominator_count,
			page_count: new_page_count,
		}
	}
}

/// A type that belongs only in the context of an `Agent`.
///
/// `Agent` is someone that manages delegated funds from [`Delegator`] accounts. It can
/// then use these funds to participate in the staking system. It can never use its own funds to
/// stake. They instead (virtually bond)[`StakingUnchecked::virtual_bond`] into the staking system
/// and are also called `Virtual Stakers`.
///
/// The `Agent` is also responsible for managing rewards and slashing for all the `Delegators` that
/// have delegated funds to it.
#[derive(Clone, Debug)]
pub struct Agent<T>(T);
impl<T> From<T> for Agent<T> {
	fn from(acc: T) -> Self {
		Agent(acc)
	}
}

impl<T> Agent<T> {
	pub fn get(self) -> T {
		self.0
	}
}

/// A type that belongs only in the context of a `Delegator`.
///
/// `Delegator` is someone that delegates funds to an `Agent`, allowing them to pool funds
/// along with other delegators and participate in the staking system.
#[derive(Clone, Debug)]
pub struct Delegator<T>(T);
impl<T> From<T> for Delegator<T> {
	fn from(acc: T) -> Self {
		Delegator(acc)
	}
}

impl<T> Delegator<T> {
	pub fn get(self) -> T {
		self.0
	}
}

/// Trait to provide delegation functionality for stakers.
pub trait DelegationInterface {
	/// Balance type used by the staking system.
	type Balance: Sub<Output = Self::Balance>
		+ Ord
		+ PartialEq
		+ Default
		+ Copy
		+ MaxEncodedLen
		+ FullCodec
		+ TypeInfo
		+ Saturating;

	/// AccountId type used by the staking system.
	type AccountId: Clone + core::fmt::Debug;

	/// Returns effective balance of the `Agent` account. `None` if not an `Agent`.
	///
	/// This takes into account any pending slashes to `Agent` against the delegated balance.
	fn agent_balance(agent: Agent<Self::AccountId>) -> Option<Self::Balance>;

	/// Returns the total amount of funds that is unbonded and can be withdrawn from the `Agent`
	/// account. `None` if not an `Agent`.
	fn agent_transferable_balance(agent: Agent<Self::AccountId>) -> Option<Self::Balance>;

	/// Returns the total amount of funds delegated. `None` if not a `Delegator`.
	fn delegator_balance(delegator: Delegator<Self::AccountId>) -> Option<Self::Balance>;

	/// Register `Agent` such that it can accept delegation.
	fn register_agent(
		agent: Agent<Self::AccountId>,
		reward_account: &Self::AccountId,
	) -> DispatchResult;

	/// Removes `Agent` registration.
	///
	/// This should only be allowed if the agent has no staked funds.
	fn remove_agent(agent: Agent<Self::AccountId>) -> DispatchResult;

	/// Add delegation to the `Agent`.
	fn delegate(
		delegator: Delegator<Self::AccountId>,
		agent: Agent<Self::AccountId>,
		amount: Self::Balance,
	) -> DispatchResult;

	/// Withdraw or revoke delegation to `Agent`.
	///
	/// If there are `Agent` funds upto `amount` available to withdraw, then those funds would
	/// be released to the `delegator`
	fn withdraw_delegation(
		delegator: Delegator<Self::AccountId>,
		agent: Agent<Self::AccountId>,
		amount: Self::Balance,
		num_slashing_spans: u32,
	) -> DispatchResult;

	/// Returns pending slashes posted to the `Agent` account. None if not an `Agent`.
	///
	/// Slashes to `Agent` account are not immediate and are applied lazily. Since `Agent`
	/// has an unbounded number of delegators, immediate slashing is not possible.
	fn pending_slash(agent: Agent<Self::AccountId>) -> Option<Self::Balance>;

	/// Apply a pending slash to an `Agent` by slashing `value` from `delegator`.
	///
	/// A reporter may be provided (if one exists) in order for the implementor to reward them,
	/// if applicable.
	fn delegator_slash(
		agent: Agent<Self::AccountId>,
		delegator: Delegator<Self::AccountId>,
		value: Self::Balance,
		maybe_reporter: Option<Self::AccountId>,
	) -> DispatchResult;
}

/// Trait to provide functionality for direct stakers to migrate to delegation agents.
/// See [`DelegationInterface`] for more details on delegation.
pub trait DelegationMigrator {
	/// Balance type used by the staking system.
	type Balance: Sub<Output = Self::Balance>
		+ Ord
		+ PartialEq
		+ Default
		+ Copy
		+ MaxEncodedLen
		+ FullCodec
		+ TypeInfo
		+ Saturating;

	/// AccountId type used by the staking system.
	type AccountId: Clone + core::fmt::Debug;

	/// Migrate an existing `Nominator` to `Agent` account.
	///
	/// The implementation should ensure the `Nominator` account funds are moved to an escrow
	/// from which `Agents` can later release funds to its `Delegators`.
	fn migrate_nominator_to_agent(
		agent: Agent<Self::AccountId>,
		reward_account: &Self::AccountId,
	) -> DispatchResult;

	/// Migrate `value` of delegation to `delegator` from a migrating agent.
	///
	/// When a direct `Nominator` migrates to `Agent`, the funds are kept in escrow. This function
	/// allows the `Agent` to release the funds to the `delegator`.
	fn migrate_delegation(
		agent: Agent<Self::AccountId>,
		delegator: Delegator<Self::AccountId>,
		value: Self::Balance,
	) -> DispatchResult;

	/// Drop the `Agent` account and its associated delegators.
	///
	/// Also removed from [`StakingUnchecked`] as a Virtual Staker. Useful for testing.
	#[cfg(feature = "runtime-benchmarks")]
	fn force_kill_agent(agent: Agent<Self::AccountId>);
}

/// Handler for determining how much of a balance should be paid out on the current era.
///
/// [`budget::IssuanceCurve`] is the successor to this trait, decoupling issuance computation
/// from staking state.
pub trait EraPayout<Balance> {
	/// Determine the payout for this era.
	///
	/// Returns the amount to be paid to stakers in this era, as well as whatever else should be
	/// paid out ("the rest").
	fn era_payout(
		total_staked: Balance,
		total_issuance: Balance,
		era_duration_millis: u64,
	) -> (Balance, Balance);
}

impl<Balance: Default> EraPayout<Balance> for () {
	fn era_payout(
		_total_staked: Balance,
		_total_issuance: Balance,
		_era_duration_millis: u64,
	) -> (Balance, Balance) {
		(Default::default(), Default::default())
	}
}

/// Result of splitting a validator's staking reward between the validator and their nominators.
///
/// Produced by [`StakerRewardCalculator::calculate_staker_reward`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StakerRewardResult<Balance> {
	/// Total payout for the validator (commission + proportional stake reward).
	pub validator_payout: Balance,
	/// Total payout for all nominators, to be split proportionally by the caller.
	pub nominator_payout: Balance,
}

/// Handles two independent reward calculations:
///
/// 1. **Staker reward split** ([`Self::calculate_staker_reward`]) — determines how a validator's
///    staking reward is divided between the validator and their nominators.
///
/// 2. **Validator incentive weight** ([`Self::calculate_validator_incentive_weight`]) — determines
///    a validator's relative share of a separate validator incentive pot, based on self-stake. This
///    incentive pot is validator-only; nominators do not receive from it.
pub trait StakerRewardCalculator<Balance> {
	/// Compute a weight for this validator's share of the validator incentive pot.
	///
	/// Called once per validator during era planning. All validators' weights are summed, and
	/// each validator's incentive payout is proportional to `their_weight / total_weight`.
	fn calculate_validator_incentive_weight(self_stake: Balance) -> Balance;

	/// Split a validator's staking reward into validator and nominator portions.
	fn calculate_staker_reward(
		validator_total_reward: Balance,
		validator_commission: Perbill,
		validator_own_stake: Balance,
		total_exposure: Balance,
	) -> StakerRewardResult<Balance>;
}

impl<Balance: Default> StakerRewardCalculator<Balance> for () {
	fn calculate_validator_incentive_weight(_self_stake: Balance) -> Balance {
		Default::default()
	}

	fn calculate_staker_reward(
		_validator_total_reward: Balance,
		_validator_commission: Perbill,
		_validator_own_stake: Balance,
		_total_exposure: Balance,
	) -> StakerRewardResult<Balance> {
		StakerRewardResult {
			validator_payout: Default::default(),
			nominator_payout: Default::default(),
		}
	}
}

sp_core::generate_feature_enabled_macro!(runtime_benchmarks_enabled, feature = "runtime-benchmarks", $);
sp_core::generate_feature_enabled_macro!(std_or_benchmarks_enabled, any(feature = "std", feature = "runtime-benchmarks"), $);

#[cfg(test)]
mod tests {
	use sp_core::ConstU32;

	use super::*;

	#[test]
	fn update_with_works() {
		let metadata = PagedExposureMetadata::<u32> {
			total: 1000,
			own: 0, // don't care
			nominator_count: 10,
			page_count: 1,
		};

		assert_eq!(
			metadata.update_with::<ConstU32<10>>(1, 1),
			PagedExposureMetadata { total: 1001, own: 0, nominator_count: 11, page_count: 2 },
		);

		assert_eq!(
			metadata.update_with::<ConstU32<5>>(1, 1),
			PagedExposureMetadata { total: 1001, own: 0, nominator_count: 11, page_count: 3 },
		);

		assert_eq!(
			metadata.update_with::<ConstU32<4>>(1, 1),
			PagedExposureMetadata { total: 1001, own: 0, nominator_count: 11, page_count: 3 },
		);

		assert_eq!(
			metadata.update_with::<ConstU32<1>>(1, 1),
			PagedExposureMetadata { total: 1001, own: 0, nominator_count: 11, page_count: 11 },
		);
	}

	#[test]
	fn individual_exposures_to_exposure_works() {
		let exposure_1 = IndividualExposure { who: 1, value: 10u32 };
		let exposure_2 = IndividualExposure { who: 2, value: 20 };
		let exposure_3 = IndividualExposure { who: 3, value: 30 };

		let exposure_page: ExposurePage<u32, u32> = vec![exposure_1, exposure_2, exposure_3].into();

		assert_eq!(
			exposure_page,
			ExposurePage { page_total: 60, others: vec![exposure_1, exposure_2, exposure_3] },
		);
	}

	#[test]
	fn empty_individual_exposures_to_exposure_works() {
		let empty_exposures: Vec<IndividualExposure<u32, u32>> = vec![];

		let exposure_page: ExposurePage<u32, u32> = empty_exposures.into();
		assert_eq!(exposure_page, ExposurePage { page_total: 0, others: vec![] });
	}

	#[test]
	fn exposure_split_others_works() {
		let exposure = Exposure {
			total: 100,
			own: 20,
			others: vec![
				IndividualExposure { who: 1, value: 20u32 },
				IndividualExposure { who: 2, value: 20 },
				IndividualExposure { who: 3, value: 20 },
				IndividualExposure { who: 4, value: 20 },
			],
		};

		let mut exposure_0 = exposure.clone();
		// split others with with 0 `n_others` is a noop and returns an empty exposure (with `own`
		// only).
		let split_exposure = exposure_0.split_others(0);
		assert_eq!(exposure_0, exposure);
		assert_eq!(split_exposure, Exposure { total: 20, own: 20, others: vec![] });

		let mut exposure_1 = exposure.clone();
		// split individual exposures so that the returned exposure has 1 individual exposure.
		let split_exposure = exposure_1.split_others(1);
		assert_eq!(exposure_1.own, 20);
		assert_eq!(exposure_1.total, 20 + 3 * 20);
		assert_eq!(exposure_1.others.len(), 3);

		assert_eq!(split_exposure.own, 20);
		assert_eq!(split_exposure.total, 20 + 1 * 20);
		assert_eq!(split_exposure.others.len(), 1);

		let mut exposure_3 = exposure.clone();
		// split individual exposures so that the returned exposure has 3 individual exposures,
		// which are consumed from the original exposure.
		let split_exposure = exposure_3.split_others(3);
		assert_eq!(exposure_3.own, 20);
		assert_eq!(exposure_3.total, 20 + 1 * 20);
		assert_eq!(exposure_3.others.len(), 1);

		assert_eq!(split_exposure.own, 20);
		assert_eq!(split_exposure.total, 20 + 3 * 20);
		assert_eq!(split_exposure.others.len(), 3);

		let mut exposure_max = exposure.clone();
		// split others with with more `n_others` than the number of others in the exposure
		// consumes all the individual exposures of the original Exposure and returns them in the
		// new exposure.
		let split_exposure = exposure_max.split_others(u32::MAX);
		assert_eq!(split_exposure, exposure);
		assert_eq!(exposure_max, Exposure { total: 20, own: 20, others: vec![] });
	}
}
