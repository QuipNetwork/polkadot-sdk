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

//! Basic types used in delegated staking.

use super::*;
use frame_support::traits::DefensiveSaturating;

/// The type of pot account being created.
#[derive(Encode, Decode)]
pub(crate) enum AccountType {
	/// A proxy delegator account created for a nominator who migrated to an `Agent` account.
	///
	/// Funds for unmigrated `delegator` accounts of the `Agent` are kept here.
	ProxyDelegator,
}

/// Information about delegation of a `delegator`.
#[derive(Default, Encode, Clone, Decode, Debug, TypeInfo, MaxEncodedLen)]
#[scale_info(skip_type_params(T))]
pub struct Delegation<T: Config> {
	/// The target of delegation.
	pub agent: T::AccountId,
	/// The amount delegated.
	pub amount: BalanceOf<T>,
}

impl<T: Config> Delegation<T> {
	/// Get delegation of a `delegator`.
	pub(crate) fn get(delegator: &T::AccountId) -> Option<Self> {
		<Delegators<T>>::get(delegator)
	}

	/// Create and return a new delegation instance.
	pub(crate) fn new(agent: &T::AccountId, amount: BalanceOf<T>) -> Self {
		Delegation { agent: agent.clone(), amount }
	}

	/// Ensure the delegator is either a new delegator or they are adding more delegation to the
	/// existing agent.
	///
	/// Delegators are prevented from delegating to multiple agents at the same time.
	pub(crate) fn can_delegate(delegator: &T::AccountId, agent: &T::AccountId) -> bool {
		Delegation::<T>::get(delegator)
			.map(|delegation| delegation.agent == *agent)
			.unwrap_or(
				// all good if it is a new delegator except it should not be an existing agent.
				!<Agents<T>>::contains_key(delegator),
			)
	}

	/// Save self to storage.
	///
	/// If the delegation amount is zero, no delegation is stored: an existing one is removed and a
	/// new one is not created. Also adds and removes provider reference as needed.
	pub(crate) fn update(self, key: &T::AccountId) {
		let exists = <Delegators<T>>::contains_key(key);

		if self.amount.is_zero() {
			if exists {
				<Delegators<T>>::remove(key);
				// Remove provider if no delegation left.
				let _ = frame_system::Pallet::<T>::dec_providers(key).defensive();
			}
			return;
		}

		if !exists {
			// this is a new delegation. Provide for this account.
			frame_system::Pallet::<T>::inc_providers(key);
		}

		<Delegators<T>>::insert(key, self);
	}
}

/// Ledger of all delegations to an `Agent`.
///
/// This keeps track of the active balance of the `Agent` that is made up from the funds that
/// are currently delegated to this `Agent`. It also tracks the pending slashes yet to be
/// applied among other things.
#[derive(Default, Clone, Encode, Decode, Debug, TypeInfo, MaxEncodedLen)]
#[scale_info(skip_type_params(T))]
pub struct AgentLedger<T: Config> {
	/// Where the reward should be paid out.
	pub payee: T::AccountId,
	/// Sum of all delegated funds to this `Agent`.
	#[codec(compact)]
	pub total_delegated: BalanceOf<T>,
	/// Funds that are withdrawn from core staking but not released to delegator/s. It is a subset
	/// of `total_delegated` and can never be greater than it.
	///
	/// We need this register to ensure that the `Agent` does not bond funds from delegated
	/// funds that are withdrawn and should be claimed by delegators.
	#[codec(compact)]
	pub unclaimed_withdrawals: BalanceOf<T>,
	/// Slashes that are not yet applied. This affects the effective balance of the `Agent`.
	#[codec(compact)]
	pub pending_slash: BalanceOf<T>,
}

impl<T: Config> AgentLedger<T> {
	/// Create a new instance of `AgentLedger`.
	pub(crate) fn new(reward_destination: &T::AccountId) -> Self {
		AgentLedger {
			payee: reward_destination.clone(),
			total_delegated: Zero::zero(),
			unclaimed_withdrawals: Zero::zero(),
			pending_slash: Zero::zero(),
		}
	}

	/// Get `AgentLedger` from storage.
	pub(crate) fn get(key: &T::AccountId) -> Option<Self> {
		<Agents<T>>::get(key)
	}

	/// Save self to storage with the given key.
	///
	/// Increments provider count if this is a new agent.
	pub(crate) fn update(self, key: &T::AccountId) {
		<Agents<T>>::insert(key, self)
	}

	/// Remove self from storage.
	pub(crate) fn remove(key: &T::AccountId) {
		debug_assert!(<Agents<T>>::contains_key(key), "Agent should exist in storage");
		<Agents<T>>::remove(key);
	}

	/// Effective total balance of the `Agent`.
	///
	/// This takes into account any slashes reported to `Agent` but unapplied.
	pub(crate) fn effective_balance(&self) -> BalanceOf<T> {
		defensive_assert!(
			self.total_delegated >= self.pending_slash,
			"slash cannot be higher than actual balance of delegator"
		);

		// pending slash needs to be burned and cannot be used for stake.
		self.total_delegated.saturating_sub(self.pending_slash)
	}

	/// Agent balance that can be staked/bonded in [`T::CoreStaking`].
	pub(crate) fn stakeable_balance(&self) -> BalanceOf<T> {
		self.effective_balance().saturating_sub(self.unclaimed_withdrawals)
	}
}

/// Wrapper around `AgentLedger` to provide some helper functions to mutate the ledger.
#[derive(Clone)]
pub struct AgentLedgerOuter<T: Config> {
	/// storage key
	pub key: T::AccountId,
	/// storage value
	pub ledger: AgentLedger<T>,
}

impl<T: Config> AgentLedgerOuter<T> {
	/// Get `Agent` from storage if it exists or return an error.
	pub(crate) fn get(agent: &T::AccountId) -> Result<AgentLedgerOuter<T>, DispatchError> {
		let ledger = AgentLedger::<T>::get(agent).ok_or(Error::<T>::NotAgent)?;
		Ok(AgentLedgerOuter { key: agent.clone(), ledger })
	}

	/// Remove funds that are withdrawn from [Config::CoreStaking] but not claimed by a delegator.
	///
	/// Checked decrease of delegation amount from `total_delegated` and `unclaimed_withdrawals`
	/// registers. Consumes self and returns a new instance of self if success.
	pub(crate) fn remove_unclaimed_withdraw(
		self,
		amount: BalanceOf<T>,
	) -> Result<Self, DispatchError> {
		let new_total_delegated = self
			.ledger
			.total_delegated
			.checked_sub(&amount)
			.defensive_ok_or(ArithmeticError::Overflow)?;
		let new_unclaimed_withdrawals = self
			.ledger
			.unclaimed_withdrawals
			.checked_sub(&amount)
			.defensive_ok_or(ArithmeticError::Overflow)?;

		Ok(AgentLedgerOuter {
			ledger: AgentLedger {
				total_delegated: new_total_delegated,
				unclaimed_withdrawals: new_unclaimed_withdrawals,
				..self.ledger
			},
			..self
		})
	}

	/// Add funds that are withdrawn from [Config::CoreStaking] to be claimed by delegators later.
	pub(crate) fn add_unclaimed_withdraw(
		self,
		amount: BalanceOf<T>,
	) -> Result<Self, DispatchError> {
		let new_unclaimed_withdrawals = self
			.ledger
			.unclaimed_withdrawals
			.checked_add(&amount)
			.defensive_ok_or(ArithmeticError::Overflow)?;

		Ok(AgentLedgerOuter {
			ledger: AgentLedger { unclaimed_withdrawals: new_unclaimed_withdrawals, ..self.ledger },
			..self
		})
	}

	/// Amount that is delegated but not bonded yet.
	///
	/// This importantly does not include `unclaimed_withdrawals` as those should not be bonded
	/// again unless explicitly requested.
	pub(crate) fn available_to_bond(&self) -> BalanceOf<T> {
		let bonded_stake = self.bonded_stake();
		let stakeable = self.ledger.stakeable_balance();

		defensive_assert!(
			stakeable >= bonded_stake,
			"cannot be bonded with more than total amount delegated to agent"
		);

		stakeable.saturating_sub(bonded_stake)
	}

	/// Remove slashes from the `AgentLedger`.
	pub(crate) fn remove_slash(self, amount: BalanceOf<T>) -> Self {
		let pending_slash = self.ledger.pending_slash.defensive_saturating_sub(amount);
		let total_delegated = self.ledger.total_delegated.defensive_saturating_sub(amount);

		AgentLedgerOuter {
			ledger: AgentLedger { pending_slash, total_delegated, ..self.ledger },
			..self
		}
	}

	/// Get the total stake of agent bonded in [`Config::CoreStaking`].
	pub(crate) fn bonded_stake(&self) -> BalanceOf<T> {
		T::CoreStaking::total_stake(&self.key).unwrap_or(Zero::zero())
	}

	/// Returns true if the agent is bonded in [`Config::CoreStaking`].
	pub(crate) fn is_bonded(&self) -> bool {
		T::CoreStaking::stake(&self.key).is_ok()
	}

	/// Returns the reward account registered by the agent.
	pub(crate) fn reward_account(&self) -> &T::AccountId {
		&self.ledger.payee
	}

	/// Save self to storage.
	pub(crate) fn save(self) {
		let key = self.key;
		self.ledger.update(&key)
	}

	/// Update agent ledger.
	pub(crate) fn update(self) {
		let key = self.key;
		self.ledger.update(&key);
	}

	/// Reloads self from storage.
	pub(crate) fn reload(self) -> Result<AgentLedgerOuter<T>, DispatchError> {
		Self::get(&self.key)
	}

	/// Balance of `Agent` that is not bonded.
	///
	/// This is similar to [Self::available_to_bond] except it also includes `unclaimed_withdrawals`
	/// of `Agent`.
	#[cfg(test)]
	pub(crate) fn total_unbonded(&self) -> BalanceOf<T> {
		let bonded_stake = self.bonded_stake();

		let net_balance = self.ledger.effective_balance();

		assert!(net_balance >= bonded_stake, "cannot be bonded with more than the agent balance");

		net_balance.saturating_sub(bonded_stake)
	}
}
