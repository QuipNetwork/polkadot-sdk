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

//! # Delegated Staking Pallet
//!
//! This pallet implements [`sp_staking::DelegationInterface`] that provides delegation
//! functionality to `delegators` and `agents`. It is designed to be used in conjunction with
//! [`StakingInterface`] and relies on [`Config::CoreStaking`] to provide primitive staking
//! functions.
//!
//! Currently, it does not expose any dispatchable calls but is written with a vision to expose them
//! in the future such that it can be utilised by any external account, off-chain entity or xcm
//! `MultiLocation` such as a parachain or a smart contract.
//!
//! ## Key Terminologies
//! - **Agent**: An account who accepts delegations from other accounts and act as an agent on their
//!   behalf for staking these delegated funds. Also, sometimes referred as `Delegatee`.
//! - **Delegator**: An account who delegates their funds to an `agent` and authorises them to use
//!   it for staking.
//! - **AgentLedger**: A data structure that holds important information about the `agent` such as
//!   total delegations they have received, any slashes posted to them, etc.
//! - **Delegation**: A data structure that stores the amount of funds delegated to an `agent` by a
//!   `delegator`.
//!
//! ## Goals
//!
//! Direct nomination on the Staking pallet does not scale well. Nominations pools were created to
//! address this by pooling delegator funds into one account and then staking it. This though had
//! a very critical limitation that the funds were moved from delegator account to pool account
//! and hence the delegator lost control over their funds for using it for other purposes such as
//! governance. This pallet aims to solve this by extending the staking pallet to support a new
//! primitive function: delegation of funds to an `agent` with the intent of staking. The agent can
//! then stake the delegated funds to [`Config::CoreStaking`] on behalf of the delegators.
//!
//! ### Withdrawal Management
//! Agent unbonding does not regulate ordering of consequent withdrawal for delegators. This is upto
//! the consumer of this pallet to implement in what order unbondable funds from
//! [`Config::CoreStaking`] can be withdrawn by the delegators.
//!
//! ### Reward and Slashing
//! This pallet does not enforce any specific strategy for how rewards or slashes are applied. It
//! is upto the `agent` account to decide how to apply the rewards and slashes.
//!
//! This importantly allows clients of this pallet to build their own strategies for reward/slashes.
//! For example, an `agent` account can choose to first slash the reward pot before slashing the
//! delegators. Or part of the reward can go to an insurance fund that can be used to cover any
//! potential future slashes. The goal is to eventually allow foreign MultiLocations
//! (smart contracts or pallets on another chain) to build their own pooled staking solutions
//! similar to `NominationPools`.

//! ## Core functions
//!
//! - Allow an account to receive delegations. See [`Pallet::register_agent`].
//! - Delegate funds to an `agent` account. See [`Pallet::delegate_to_agent`].
//! - Release delegated funds from an `agent` account to the `delegator`. See
//!   [`Pallet::release_delegation`].
//! - Migrate a `Nominator` account to an `agent` account. See [`Pallet::migrate_to_agent`].
//!   Explained in more detail in the `Migration` section.
//! - Migrate unclaimed delegated funds from `agent` to delegator. When a nominator migrates to an
//!   agent, the funds are held in a proxy account. This function allows the delegator to claim
//!   their share of the funds from the proxy account. See [`Pallet::migrate_delegation`].
//!
//! ## Lazy Slashing
//! One of the reasons why direct nominators on staking pallet cannot scale well is because all
//! nominators are slashed at the same time. This is expensive and needs to be bounded operation.
//!
//! This pallet implements a lazy slashing mechanism. Any slashes to the `agent` are posted in its
//! `AgentLedger` as a pending slash. Since the actual amount is held in the multiple
//! `delegator` accounts, this pallet has no way to know how to apply slash. It is the `agent`'s
//! responsibility to apply slashes for each delegator, one at a time. Staking pallet ensures the
//! pending slash never exceeds staked amount and would freeze further withdraws until all pending
//! slashes are cleared.
//!
//! The user of this pallet can apply slash using
//! [DelegationInterface::delegator_slash](sp_staking::DelegationInterface::delegator_slash).
//!
//! ## Migration from Nominator to Agent
//! More details [here](https://hackmd.io/@ak0n/454-np-governance).
//!
//! ## Nomination Pool vs Delegation Staking
//! This pallet is not a replacement for Nomination Pool but adds a new primitive in addition to
//! staking pallet that can be used by Nomination Pool to support delegation based staking. It can
//! be thought of as an extension to the Staking Pallet in relation to Nomination Pools.
//! Technically, these changes could be made in one of those pallets as well but that would have
//! meant significant refactoring and high chances of introducing a regression. With this approach,
//! we can keep the existing pallets with minimal changes and introduce a new pallet that can be
//! optionally used by Nomination Pool. The vision is to build this in a configurable way such that
//! runtime can choose whether to use this pallet or not.
//!
//! With that said, following is the main difference between
//! #### Nomination Pool without delegation support
//!  1) transfer fund from delegator to pool account, and
//!  2) stake from pool account as a direct nominator.
//!
//! #### Nomination Pool with delegation support
//!  1) delegate fund from delegator to pool account, and
//!  2) stake from pool account as an `Agent` account on the staking pallet.
//!
//! The difference being, in the second approach, the delegated funds will be locked in-place in
//! user's account enabling them to participate in use cases that allows use of `held` funds such
//! as participation in governance voting.
//!
//! Nomination pool still does all the heavy lifting around pool administration, reward
//! distribution, lazy slashing and as such, is not meant to be replaced with this pallet.
//!
//! ## Limitations
//! - Rewards can not be auto-compounded.
//! - Slashes are lazy and hence there could be a period of time when an account can use funds for
//!   operations such as voting in governance even though they should be slashed.

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(rustdoc::broken_intra_doc_links)]

mod impls;
pub mod migration;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
pub mod types;

extern crate alloc;

pub use pallet::*;

use types::*;

use core::convert::TryInto;
use frame_support::{
	pallet_prelude::*,
	traits::{
		fungible::{
			hold::{
				Balanced as FunHoldBalanced, Inspect as FunHoldInspect, Mutate as FunHoldMutate,
			},
			Balanced, Inspect as FunInspect, Mutate as FunMutate,
		},
		tokens::{fungible::Credit, Fortitude, Precision, Preservation, Restriction},
		Defensive, DefensiveOption, Imbalance, OnUnbalanced,
	},
};
use sp_io::hashing::blake2_256;
use sp_runtime::{
	traits::{CheckedAdd, CheckedSub, TrailingZeroInput, Zero},
	ArithmeticError, Debug, DispatchResult, Perbill, Saturating,
};
use sp_staking::{Agent, Delegator, EraIndex, StakingInterface, StakingUnchecked};

/// The log target of this pallet.
pub const LOG_TARGET: &str = "runtime::delegated-staking";
// syntactic sugar for logging.
#[macro_export]
macro_rules! log {
	($level:tt, $patter:expr $(, $values:expr)* $(,)?) => {
		log::$level!(
			target: $crate::LOG_TARGET,
			concat!("[{:?}] 🏊‍♂️ ", $patter), <frame_system::Pallet<T>>::block_number() $(, $values)*
		)
	};
}
pub type BalanceOf<T> =
	<<T as Config>::Currency as FunInspect<<T as frame_system::Config>::AccountId>>::Balance;

use frame_system::{ensure_signed, pallet_prelude::*, RawOrigin};

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	/// The in-code storage version.
	const STORAGE_VERSION: StorageVersion = StorageVersion::new(0);
	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(PhantomData<T>);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Injected identifier for the pallet.
		#[pallet::constant]
		type PalletId: Get<frame_support::PalletId>;

		/// Currency type.
		type Currency: FunHoldMutate<Self::AccountId, Reason = Self::RuntimeHoldReason>
			+ FunMutate<Self::AccountId>
			+ FunHoldBalanced<Self::AccountId>;

		/// Handler for the unbalanced reduction when slashing a delegator.
		type OnSlash: OnUnbalanced<Credit<Self::AccountId, Self::Currency>>;

		/// Fraction of the slash that is rewarded to the caller of pending slash to the agent.
		#[pallet::constant]
		type SlashRewardFraction: Get<Perbill>;

		/// Overarching hold reason.
		type RuntimeHoldReason: From<HoldReason>;

		/// Core staking implementation.
		type CoreStaking: StakingUnchecked<Balance = BalanceOf<Self>, AccountId = Self::AccountId>;
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The account cannot perform this operation.
		NotAllowed,
		/// An existing staker cannot perform this action.
		AlreadyStaking,
		/// Reward Destination cannot be same as `Agent` account.
		InvalidRewardDestination,
		/// Delegation conditions are not met.
		///
		/// Possible issues are
		/// 1) Cannot delegate to self,
		/// 2) Cannot delegate to multiple delegates.
		InvalidDelegation,
		/// The account does not have enough funds to perform the operation.
		NotEnoughFunds,
		/// Not an existing `Agent` account.
		NotAgent,
		/// Not a Delegator account.
		NotDelegator,
		/// Some corruption in internal state.
		BadState,
		/// Unapplied pending slash restricts operation on `Agent`.
		UnappliedSlash,
		/// `Agent` has no pending slash to be applied.
		NothingToSlash,
		/// Failed to withdraw amount from Core Staking.
		WithdrawFailed,
		/// Operation not supported by this pallet.
		NotSupported,
	}

	/// A reason for placing a hold on funds.
	#[pallet::composite_enum]
	pub enum HoldReason {
		/// Funds held for stake delegation to another account.
		#[codec(index = 0)]
		StakingDelegation,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub (super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Funds delegated by a delegator.
		Delegated { agent: T::AccountId, delegator: T::AccountId, amount: BalanceOf<T> },
		/// Funds released to a delegator.
		Released { agent: T::AccountId, delegator: T::AccountId, amount: BalanceOf<T> },
		/// Funds slashed from a delegator.
		Slashed { agent: T::AccountId, delegator: T::AccountId, amount: BalanceOf<T> },
		/// Unclaimed delegation funds migrated to delegator.
		MigratedDelegation { agent: T::AccountId, delegator: T::AccountId, amount: BalanceOf<T> },
	}

	/// Map of Delegators to their `Delegation`.
	///
	/// Implementation note: We are not using a double map with `delegator` and `agent` account
	/// as keys since we want to restrict delegators to delegate only to one account at a time.
	#[pallet::storage]
	pub type Delegators<T: Config> =
		CountedStorageMap<_, Twox64Concat, T::AccountId, Delegation<T>, OptionQuery>;

	/// Map of `Agent` to their `Ledger`.
	#[pallet::storage]
	pub type Agents<T: Config> =
		CountedStorageMap<_, Twox64Concat, T::AccountId, AgentLedger<T>, OptionQuery>;

	// This pallet is not currently written with the intention of exposing any calls. But the
	// functions defined in the following impl block should act as a good reference for how the
	// exposed calls would look like when exposed.
	impl<T: Config> Pallet<T> {
		/// Register an account to become a stake `Agent`. Sometimes also called a `Delegatee`.
		///
		/// Delegators can authorize `Agent`s to stake on their behalf by delegating their funds to
		/// them. The `Agent` can then use the delegated funds to stake to [`Config::CoreStaking`].
		///
		/// An account that is directly staked to [`Config::CoreStaking`] cannot become an `Agent`.
		/// However, they can migrate to become an agent using [`Self::migrate_to_agent`].
		///
		/// Implementation note: This function allows any account to become an agent. It is
		/// important though that accounts that call [`StakingUnchecked::virtual_bond`] are keyless
		/// accounts. This is not a problem for now since this is only used by other pallets in the
		/// runtime which use keyless account as agents. If we later want to expose this as a
		/// dispatchable call, we should derive a sub-account from the caller and use that as the
		/// agent account.
		pub fn register_agent(
			origin: OriginFor<T>,
			reward_account: T::AccountId,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;

			// Existing `agent` cannot register again and a delegator cannot become an `agent`.
			ensure!(!Self::is_agent(&who) && !Self::is_delegator(&who), Error::<T>::NotAllowed);

			// Reward account cannot be same as `agent` account.
			ensure!(reward_account != who, Error::<T>::InvalidRewardDestination);

			Self::do_register_agent(&who, &reward_account);
			Ok(())
		}

		/// Remove an account from being an `Agent`.
		///
		/// This can only be called if the agent has no delegated funds, no pending slashes and no
		/// unclaimed withdrawals.
		pub fn remove_agent(origin: OriginFor<T>) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let ledger = AgentLedger::<T>::get(&who).ok_or(Error::<T>::NotAgent)?;

			ensure!(
				ledger.total_delegated == Zero::zero() &&
					ledger.pending_slash == Zero::zero() &&
					ledger.unclaimed_withdrawals == Zero::zero(),
				Error::<T>::NotAllowed
			);

			AgentLedger::<T>::remove(&who);
			Ok(())
		}

		/// Migrate from a `Nominator` account to `Agent` account.
		///
		/// The origin needs to
		/// - be a `Nominator` with [`Config::CoreStaking`],
		/// - not already an `Agent`,
		///
		/// This function will create a proxy account to the agent called `proxy_delegator` and
		/// transfer the directly staked amount by the agent to it. The `proxy_delegator` delegates
		/// the funds to the origin making origin an `Agent` account. The real `delegator`
		/// accounts of the origin can later migrate their funds using [Self::migrate_delegation] to
		/// claim back their share of delegated funds from `proxy_delegator` to self.
		///
		/// Any free fund in the agent's account will be marked as unclaimed withdrawal.
		pub fn migrate_to_agent(
			origin: OriginFor<T>,
			reward_account: T::AccountId,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			// ensure who is a staker in `CoreStaking` but not already an agent or a delegator.
			ensure!(
				Self::is_direct_staker(&who) && !Self::is_agent(&who) && !Self::is_delegator(&who),
				Error::<T>::NotAllowed
			);

			// Reward account cannot be same as `agent` account.
			ensure!(reward_account != who, Error::<T>::InvalidRewardDestination);

			Self::do_migrate_to_agent(&who, &reward_account)
		}

		/// Release previously delegated funds by delegator to origin.
		///
		/// Only agents can call this.
		///
		/// Tries to withdraw unbonded funds from `CoreStaking` if needed and release amount to
		/// `delegator`.
		pub fn release_delegation(
			origin: OriginFor<T>,
			delegator: T::AccountId,
			amount: BalanceOf<T>,
			num_slashing_spans: u32,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::do_release(
				Agent::from(who),
				Delegator::from(delegator),
				amount,
				num_slashing_spans,
			)
		}

		/// Migrate delegated funds that are held in `proxy_delegator` to the claiming `delegator`'s
		/// account. If successful, the specified funds will be moved and delegated from `delegator`
		/// account to the agent.
		///
		/// This can be called by `agent` accounts that were previously a direct `Nominator` with
		/// [`Config::CoreStaking`] and has some remaining unclaimed delegations.
		///
		/// Internally, it moves some delegations from `proxy_delegator` account to `delegator`
		/// account and reapplying the holds.
		pub fn migrate_delegation(
			origin: OriginFor<T>,
			delegator: T::AccountId,
			amount: BalanceOf<T>,
		) -> DispatchResult {
			let agent = ensure_signed(origin)?;

			// Ensure delegator is sane.
			ensure!(!Self::is_agent(&delegator), Error::<T>::NotAllowed);
			ensure!(!Self::is_delegator(&delegator), Error::<T>::NotAllowed);

			// ensure agent is sane.
			ensure!(Self::is_agent(&agent), Error::<T>::NotAgent);

			// nothing to delegate.
			if amount.is_zero() {
				return Ok(());
			}

			// and has enough delegated balance to migrate.
			let proxy_delegator = Self::generate_proxy_delegator(Agent::from(agent));
			let balance_remaining = Self::held_balance_of(proxy_delegator.clone());
			ensure!(balance_remaining >= amount, Error::<T>::NotEnoughFunds);

			Self::do_migrate_delegation(proxy_delegator, Delegator::from(delegator), amount)
		}

		/// Delegate given `amount` of tokens to an `Agent` account.
		///
		/// If `origin` is the first time delegator, we add them to state. If they are already
		/// delegating, we increase the delegation.
		///
		/// Conditions:
		/// - Delegators cannot delegate to more than one agent.
		/// - The `agent` account should already be registered as such. See
		///   [`Self::register_agent`].
		pub fn delegate_to_agent(
			origin: OriginFor<T>,
			agent: T::AccountId,
			amount: BalanceOf<T>,
		) -> DispatchResult {
			let delegator = ensure_signed(origin)?;

			// ensure delegator is sane.
			ensure!(
				Delegation::<T>::can_delegate(&delegator, &agent),
				Error::<T>::InvalidDelegation
			);

			// ensure agent is sane.
			ensure!(Self::is_agent(&agent), Error::<T>::NotAgent);

			// nothing to delegate, and nothing to bond.
			if amount.is_zero() {
				return Ok(());
			}

			// add to delegation.
			Self::do_delegate(Delegator::from(delegator), Agent::from(agent.clone()), amount)?;

			// bond the newly delegated amount to `CoreStaking`.
			Self::do_bond(Agent::from(agent), amount)
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		#[cfg(feature = "try-runtime")]
		fn try_state(_n: BlockNumberFor<T>) -> Result<(), sp_runtime::TryRuntimeError> {
			Self::do_try_state()
		}
	}
}

impl<T: Config> Pallet<T> {
	/// Derive an account from the migrating agent account where the unclaimed delegation funds
	/// are held.
	pub fn generate_proxy_delegator(agent: Agent<T::AccountId>) -> Delegator<T::AccountId> {
		Delegator::from(Self::sub_account(AccountType::ProxyDelegator, agent.get()))
	}

	/// Derive a (keyless) pot account from the given agent account and account type.
	fn sub_account(account_type: AccountType, acc: T::AccountId) -> T::AccountId {
		let entropy = (T::PalletId::get(), acc, account_type).using_encoded(blake2_256);
		Decode::decode(&mut TrailingZeroInput::new(entropy.as_ref()))
			.expect("infinite length input; no invalid inputs for type; qed")
	}

	/// Held balance of a delegator.
	pub(crate) fn held_balance_of(who: Delegator<T::AccountId>) -> BalanceOf<T> {
		T::Currency::balance_on_hold(&HoldReason::StakingDelegation.into(), &who.get())
	}

	/// Returns true if who is registered as an `Agent`.
	fn is_agent(who: &T::AccountId) -> bool {
		<Agents<T>>::contains_key(who)
	}

	/// Returns true if who is delegating to an `Agent` account.
	fn is_delegator(who: &T::AccountId) -> bool {
		<Delegators<T>>::contains_key(who)
	}

	/// Returns true if who is already staking on [`Config::CoreStaking`].
	fn is_direct_staker(who: &T::AccountId) -> bool {
		T::CoreStaking::status(who).is_ok()
	}

	/// Registers a new agent in the system.
	fn do_register_agent(who: &T::AccountId, reward_account: &T::AccountId) {
		// TODO: Consider taking a deposit for being an agent.
		AgentLedger::<T>::new(reward_account).update(who);
	}

	/// Migrate existing staker account `who` to an `Agent` account.
	fn do_migrate_to_agent(who: &T::AccountId, reward_account: &T::AccountId) -> DispatchResult {
		Self::do_register_agent(who, reward_account);

		// We create a proxy delegator that will keep all the delegation funds until funds are
		// transferred to actual delegator.
		let proxy_delegator = Self::generate_proxy_delegator(Agent::from(who.clone()));

		// Get current stake
		let stake = T::CoreStaking::stake(who)?;

		// release funds from core staking.
		T::CoreStaking::migrate_to_virtual_staker(who)?;

		// transfer just released staked amount plus any free amount.
		let amount_to_transfer =
			T::Currency::reducible_balance(who, Preservation::Expendable, Fortitude::Polite);

		// This should never fail but if it does, it indicates bad state and we abort.
		T::Currency::transfer(
			who,
			&proxy_delegator.clone().get(),
			amount_to_transfer,
			Preservation::Expendable,
		)?;

		T::CoreStaking::set_payee(who, reward_account)?;
		// delegate all transferred funds back to agent.
		Self::do_delegate(proxy_delegator, Agent::from(who.clone()), amount_to_transfer)?;
		// if the transferred/delegated amount was greater than the stake, mark the extra as
		// unclaimed withdrawal.
		let unclaimed_withdraws = amount_to_transfer
			.checked_sub(&stake.total)
			.defensive_ok_or(ArithmeticError::Underflow)?;

		if !unclaimed_withdraws.is_zero() {
			let mut ledger = AgentLedger::<T>::get(who).ok_or(Error::<T>::NotAgent)?;
			ledger.unclaimed_withdrawals = ledger
				.unclaimed_withdrawals
				.checked_add(&unclaimed_withdraws)
				.defensive_ok_or(ArithmeticError::Overflow)?;
			ledger.update(who);
		}

		Ok(())
	}

	/// Bond `amount` to `agent_acc` in [`Config::CoreStaking`].
	fn do_bond(agent_acc: Agent<T::AccountId>, amount: BalanceOf<T>) -> DispatchResult {
		let agent_ledger = AgentLedgerOuter::<T>::get(&agent_acc.get())?;

		let available_to_bond = agent_ledger.available_to_bond();
		defensive_assert!(amount == available_to_bond, "not expected value to bond");

		if agent_ledger.is_bonded() {
			T::CoreStaking::bond_extra(&agent_ledger.key, amount)
		} else {
			T::CoreStaking::virtual_bond(&agent_ledger.key, amount, agent_ledger.reward_account())
		}
	}

	/// Delegate `amount` from `delegator` to `agent`.
	fn do_delegate(
		delegator: Delegator<T::AccountId>,
		agent: Agent<T::AccountId>,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		// get inner type
		let agent = agent.get();
		let delegator = delegator.get();

		let mut ledger = AgentLedger::<T>::get(&agent).ok_or(Error::<T>::NotAgent)?;

		if let Some(mut existing_delegation) = Delegation::<T>::get(&delegator) {
			ensure!(existing_delegation.agent == agent, Error::<T>::InvalidDelegation);
			// update amount and return the updated delegation.
			existing_delegation.amount = existing_delegation
				.amount
				.checked_add(&amount)
				.ok_or(ArithmeticError::Overflow)?;
			existing_delegation
		} else {
			Delegation::<T>::new(&agent, amount)
		}
		.update(&delegator);

		// try to hold the funds.
		T::Currency::hold(&HoldReason::StakingDelegation.into(), &delegator, amount)?;

		ledger.total_delegated =
			ledger.total_delegated.checked_add(&amount).ok_or(ArithmeticError::Overflow)?;
		ledger.update(&agent);

		Self::deposit_event(Event::<T>::Delegated { agent, delegator, amount });

		Ok(())
	}

	/// Release `amount` of delegated funds from `agent` to `delegator`.
	fn do_release(
		who: Agent<T::AccountId>,
		delegator: Delegator<T::AccountId>,
		amount: BalanceOf<T>,
		num_slashing_spans: u32,
	) -> DispatchResult {
		// get inner type
		let agent = who.get();
		let delegator = delegator.get();

		let mut agent_ledger = AgentLedgerOuter::<T>::get(&agent)?;
		let mut delegation = Delegation::<T>::get(&delegator).ok_or(Error::<T>::NotDelegator)?;

		// make sure delegation to be released is sound.
		ensure!(delegation.agent == agent, Error::<T>::NotAgent);
		ensure!(delegation.amount >= amount, Error::<T>::NotEnoughFunds);

		// if we do not already have enough funds to be claimed, try to withdraw some more.
		if agent_ledger.ledger.unclaimed_withdrawals < amount {
			// withdraw account.
			T::CoreStaking::withdraw_unbonded(agent.clone(), num_slashing_spans)
				.map_err(|_| Error::<T>::WithdrawFailed)?;
			// reload agent from storage since withdrawal might have changed the state.
			agent_ledger = agent_ledger.reload()?;
		}

		// if we still do not have enough funds to release, abort.
		ensure!(agent_ledger.ledger.unclaimed_withdrawals >= amount, Error::<T>::NotEnoughFunds);
		agent_ledger.remove_unclaimed_withdraw(amount)?.update();

		delegation.amount = delegation
			.amount
			.checked_sub(&amount)
			.defensive_ok_or(ArithmeticError::Overflow)?;

		let released = T::Currency::release(
			&HoldReason::StakingDelegation.into(),
			&delegator,
			amount,
			Precision::BestEffort,
		)?;

		defensive_assert!(released == amount, "hold should have been released fully");

		// update delegation.
		delegation.update(&delegator);

		Self::deposit_event(Event::<T>::Released { agent, delegator, amount });

		Ok(())
	}

	/// Migrates delegation of `amount` from `source` account to `destination` account.
	fn do_migrate_delegation(
		source_delegator: Delegator<T::AccountId>,
		destination_delegator: Delegator<T::AccountId>,
		amount: BalanceOf<T>,
	) -> DispatchResult {
		// get inner type
		let source_delegator = source_delegator.get();
		let destination_delegator = destination_delegator.get();

		let mut source_delegation =
			Delegators::<T>::get(&source_delegator).defensive_ok_or(Error::<T>::BadState)?;

		// ensure source has enough funds to migrate.
		ensure!(source_delegation.amount >= amount, Error::<T>::NotEnoughFunds);
		debug_assert!(
			!Self::is_delegator(&destination_delegator) && !Self::is_agent(&destination_delegator)
		);

		let agent = source_delegation.agent.clone();
		// create a new delegation for destination delegator.
		Delegation::<T>::new(&agent, amount).update(&destination_delegator);

		source_delegation.amount = source_delegation
			.amount
			.checked_sub(&amount)
			.defensive_ok_or(Error::<T>::BadState)?;

		// transfer the held amount in `source_delegator` to `destination_delegator`.
		T::Currency::transfer_on_hold(
			&HoldReason::StakingDelegation.into(),
			&source_delegator,
			&destination_delegator,
			amount,
			Precision::Exact,
			Restriction::OnHold,
			Fortitude::Polite,
		)?;

		// update source delegation.
		source_delegation.update(&source_delegator);

		Self::deposit_event(Event::<T>::MigratedDelegation {
			agent,
			delegator: destination_delegator,
			amount,
		});

		Ok(())
	}

	/// Take slash `amount` from agent's `pending_slash`counter and apply it to `delegator` account.
	pub fn do_slash(
		agent: Agent<T::AccountId>,
		delegator: Delegator<T::AccountId>,
		amount: BalanceOf<T>,
		maybe_reporter: Option<T::AccountId>,
	) -> DispatchResult {
		// get inner type
		let agent = agent.get();
		let delegator = delegator.get();

		let agent_ledger = AgentLedgerOuter::<T>::get(&agent)?;
		// ensure there is something to slash
		ensure!(agent_ledger.ledger.pending_slash > Zero::zero(), Error::<T>::NothingToSlash);

		let mut delegation = <Delegators<T>>::get(&delegator).ok_or(Error::<T>::NotDelegator)?;
		ensure!(delegation.agent == agent.clone(), Error::<T>::NotAgent);
		ensure!(delegation.amount >= amount, Error::<T>::NotEnoughFunds);

		// slash delegator
		let (mut credit, missing) =
			T::Currency::slash(&HoldReason::StakingDelegation.into(), &delegator, amount);

		defensive_assert!(missing.is_zero(), "slash should have been fully applied");

		let actual_slash = credit.peek();

		// remove the applied slashed amount from agent.
		agent_ledger.remove_slash(actual_slash).save();
		delegation.amount =
			delegation.amount.checked_sub(&actual_slash).ok_or(ArithmeticError::Overflow)?;
		delegation.update(&delegator);

		if let Some(reporter) = maybe_reporter {
			let reward_payout: BalanceOf<T> = T::SlashRewardFraction::get() * actual_slash;
			let (reporter_reward, rest) = credit.split(reward_payout);

			// credit is the amount that we provide to `T::OnSlash`.
			credit = rest;

			// reward reporter or drop it.
			let _ = T::Currency::resolve(&reporter, reporter_reward);
		}

		T::OnSlash::on_unbalanced(credit);

		Self::deposit_event(Event::<T>::Slashed { agent, delegator, amount });

		Ok(())
	}

	/// Total balance that is available for stake. Includes already staked amount.
	#[cfg(test)]
	pub(crate) fn stakeable_balance(who: Agent<T::AccountId>) -> BalanceOf<T> {
		AgentLedgerOuter::<T>::get(&who.get())
			.map(|agent| agent.ledger.stakeable_balance())
			.unwrap_or_default()
	}
}

#[cfg(any(test, feature = "try-runtime"))]
use alloc::collections::btree_map::BTreeMap;

#[cfg(any(test, feature = "try-runtime"))]
impl<T: Config> Pallet<T> {
	pub(crate) fn do_try_state() -> Result<(), sp_runtime::TryRuntimeError> {
		// build map to avoid reading storage multiple times.
		let delegation_map = Delegators::<T>::iter().collect::<BTreeMap<_, _>>();
		let ledger_map = Agents::<T>::iter().collect::<BTreeMap<_, _>>();

		Self::check_delegates(ledger_map.clone())?;
		Self::check_delegators(delegation_map, ledger_map)?;

		Ok(())
	}

	fn check_delegates(
		ledgers: BTreeMap<T::AccountId, AgentLedger<T>>,
	) -> Result<(), sp_runtime::TryRuntimeError> {
		for (agent, ledger) in ledgers {
			let staked_value = ledger.stakeable_balance();

			if !staked_value.is_zero() {
				ensure!(
					matches!(
						T::CoreStaking::status(&agent).expect("agent should be bonded"),
						sp_staking::StakerStatus::Nominator(_) | sp_staking::StakerStatus::Idle
					),
					"agent should be bonded and not validator"
				);
			}

			ensure!(
				ledger.stakeable_balance() >=
					T::CoreStaking::total_stake(&agent).unwrap_or_default(),
				"Cannot stake more than balance"
			);
		}

		Ok(())
	}

	fn check_delegators(
		delegations: BTreeMap<T::AccountId, Delegation<T>>,
		ledger: BTreeMap<T::AccountId, AgentLedger<T>>,
	) -> Result<(), sp_runtime::TryRuntimeError> {
		let mut delegation_aggregation = BTreeMap::<T::AccountId, BalanceOf<T>>::new();
		for (delegator, delegation) in delegations.iter() {
			ensure!(!Self::is_agent(delegator), "delegator cannot be an agent");
			ensure!(!delegation.amount.is_zero(), "delegation amount must be non-zero");

			delegation_aggregation
				.entry(delegation.agent.clone())
				.and_modify(|e| *e += delegation.amount)
				.or_insert(delegation.amount);
		}

		for (agent, total_delegated) in delegation_aggregation {
			ensure!(!Self::is_delegator(&agent), "agent cannot be delegator");

			let ledger = ledger.get(&agent).expect("ledger should exist");
			ensure!(
				ledger.total_delegated == total_delegated,
				"ledger total delegated should match delegations"
			);
		}

		Ok(())
	}
}
