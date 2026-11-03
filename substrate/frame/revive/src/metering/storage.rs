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

//! This module contains functions to meter the storage deposit.

#[cfg(test)]
mod tests;

use super::{Nested, Root, State};
use crate::{
	BalanceOf, Config, ExecConfig, ExecOrigin as Origin, HoldReason, Pallet,
	StorageDeposit as Deposit, storage::ContractInfo,
};
use alloc::vec::Vec;
use core::{marker::PhantomData, mem};
use frame_support::{DebugNoBound, DefaultNoBound, traits::Get};
use sp_runtime::{
	DispatchError, FixedPointNumber, FixedU128,
	traits::{Saturating, Zero},
};

#[cfg(test)]
use num_traits::Bounded;

/// Deposit that uses the native fungible's balance type.
pub type DepositOf<T> = Deposit<BalanceOf<T>>;

/// A production root storage meter that actually charges from its origin.
pub type Meter<T> = RawMeter<T, ReservingExt, Root>;

/// A production storage meter that actually charges from its origin.
///
/// This can be used where we want to be generic over the state (Root vs. Nested).
pub type GenericMeter<T, S> = RawMeter<T, ReservingExt, S>;

/// A trait that allows to decouple the metering from the charging of balance.
///
/// This mostly exists for testing so that the charging can be mocked.
pub trait Ext<T: Config> {
	/// This is called to inform the implementer that some balance should be charged due to
	/// some interaction of the `origin` with a `contract`.
	///
	/// The balance transfer can either flow from `origin` to `contract` or the other way
	/// around depending on whether `amount` constitutes a `Charge` or a `Refund`.
	/// It will fail in case the `origin` has not enough balance to cover all storage deposits.
	fn charge(
		origin: &T::AccountId,
		contract: &T::AccountId,
		amount: &DepositOf<T>,
		exec_config: &ExecConfig<T>,
	) -> Result<(), DispatchError>;
}

/// This [`Ext`] is used for actual on-chain execution when balance needs to be charged.
///
/// It uses [`frame_support::traits::fungible::Mutate`] in order to do accomplish the reserves.
pub enum ReservingExt {}

/// A type that allows the metering of consumed or freed storage of a single contract call stack.
#[derive(DefaultNoBound, DebugNoBound)]
pub struct RawMeter<T: Config, E, S: State> {
	/// The limit of how much balance this meter is allowed to consume.
	pub(crate) limit: Option<BalanceOf<T>>,
	/// The amount of balance that was used in this meter and all of its already absorbed children.
	total_deposit: DepositOf<T>,
	/// The amount of storage changes that were recorded in this meter alone.
	/// This has no meaning for Root meters and will always be Contribution::Checked(0)
	own_contribution: Contribution<T>,
	/// List of charges that should be applied at the end of a contract stack execution.
	///
	/// We only have one charge per contract hence the size of this vector is
	/// limited by the maximum call depth.
	charges: Vec<Charge<T>>,
	/// The maximal consumed deposit that occurred at any point during the execution of this
	/// storage deposit meter
	max_charged: BalanceOf<T>,
	/// True if this is the root meter.
	///
	/// Sometimes we cannot know at compile time.
	pub(crate) is_root: bool,
	/// Type parameter only used in impls.
	_phantom: PhantomData<(E, S)>,
}

/// This type is used to describe a storage change when charging from the meter.
#[derive(Default, DebugNoBound)]
pub struct Diff {
	/// How many bytes were added to storage.
	pub bytes_added: u32,
	/// How many bytes were removed from storage.
	pub bytes_removed: u32,
	/// How many storage items were added to storage.
	pub items_added: u32,
	/// How many storage items were removed from storage.
	pub items_removed: u32,
}

impl Diff {
	/// Calculate how much of a charge or refund results from applying the diff and store it
	/// in the passed `info` if any.
	///
	/// # Note
	///
	/// In case `None` is passed for `info` only charges are calculated. This is because refunds
	/// are calculated pro rata of the existing storage within a contract and hence need extract
	/// this information from the passed `info`.
	pub fn update_contract<T: Config>(&self, info: Option<&mut ContractInfo<T>>) -> DepositOf<T> {
		let per_byte = T::DepositPerByte::get();
		let per_item = T::DepositPerChildTrieItem::get();
		let bytes_added = self.bytes_added.saturating_sub(self.bytes_removed);
		let items_added = self.items_added.saturating_sub(self.items_removed);
		let mut bytes_deposit = Deposit::Charge(per_byte.saturating_mul((bytes_added).into()));
		let mut items_deposit = Deposit::Charge(per_item.saturating_mul((items_added).into()));

		// Without any contract info we can only calculate diffs which add storage
		let info = if let Some(info) = info {
			info
		} else {
			return bytes_deposit.saturating_add(&items_deposit);
		};

		// Refunds are calculated pro rata based on the accumulated storage within the contract
		let bytes_removed = self.bytes_removed.saturating_sub(self.bytes_added);
		let items_removed = self.items_removed.saturating_sub(self.items_added);
		let ratio = FixedU128::checked_from_rational(bytes_removed, info.storage_bytes)
			.unwrap_or_default()
			.min(FixedU128::from_u32(1));
		bytes_deposit = bytes_deposit
			.saturating_add(&Deposit::Refund(ratio.saturating_mul_int(info.storage_byte_deposit)));
		let ratio = FixedU128::checked_from_rational(items_removed, info.storage_items)
			.unwrap_or_default()
			.min(FixedU128::from_u32(1));
		items_deposit = items_deposit
			.saturating_add(&Deposit::Refund(ratio.saturating_mul_int(info.storage_item_deposit)));

		// We need to update the contract info structure with the new deposits
		info.storage_bytes =
			info.storage_bytes.saturating_add(bytes_added).saturating_sub(bytes_removed);
		info.storage_items =
			info.storage_items.saturating_add(items_added).saturating_sub(items_removed);
		match &bytes_deposit {
			Deposit::Charge(amount) => {
				info.storage_byte_deposit = info.storage_byte_deposit.saturating_add(*amount)
			},
			Deposit::Refund(amount) => {
				info.storage_byte_deposit = info.storage_byte_deposit.saturating_sub(*amount)
			},
		}
		match &items_deposit {
			Deposit::Charge(amount) => {
				info.storage_item_deposit = info.storage_item_deposit.saturating_add(*amount)
			},
			Deposit::Refund(amount) => {
				info.storage_item_deposit = info.storage_item_deposit.saturating_sub(*amount)
			},
		}

		bytes_deposit.saturating_add(&items_deposit)
	}
}

impl Diff {
	fn saturating_add(&self, rhs: &Self) -> Self {
		Self {
			bytes_added: self.bytes_added.saturating_add(rhs.bytes_added),
			bytes_removed: self.bytes_removed.saturating_add(rhs.bytes_removed),
			items_added: self.items_added.saturating_add(rhs.items_added),
			items_removed: self.items_removed.saturating_add(rhs.items_removed),
		}
	}
}

/// The state of a contract.
#[derive(DebugNoBound, Clone, PartialEq, Eq)]
pub enum ContractState<T: Config> {
	Alive { amount: DepositOf<T> },
	Terminated,
}

/// Records information to charge or refund a plain account.
///
/// All the charges are deferred to the end of a whole call stack. Reason is that by doing
/// this we can do all the refunds before doing any charge. This way a plain account can use
/// more deposit than it has balance as along as it is covered by a refund. This
/// essentially makes the order of storage changes irrelevant with regard to the deposit system.
/// The only exception is when a special (tougher) deposit limit is specified for a cross-contract
/// call. In that case the limit is enforced once the call is returned, rolling it back if
/// exhausted.
#[derive(DebugNoBound, Clone)]
struct Charge<T: Config> {
	contract: T::AccountId,
	state: ContractState<T>,
}

/// Records the storage changes of a storage meter.
#[derive(DebugNoBound)]
enum Contribution<T: Config> {
	/// The contract the meter belongs to is alive and accumulates changes using a [`Diff`].
	Alive(Diff),
	/// The meter was checked against its limit using [`RawMeter::enforce_limit`] at the end of
	/// its execution. In this process the [`Diff`] was converted into a [`Deposit`].
	Checked(DepositOf<T>),
}

impl<T: Config> Contribution<T> {
	/// See [`Diff::update_contract`].
	fn update_contract(&self, info: Option<&mut ContractInfo<T>>) -> DepositOf<T> {
		match self {
			Self::Alive(diff) => diff.update_contract::<T>(info),
			Self::Checked(deposit) => deposit.clone(),
		}
	}
}

impl<T: Config> Default for Contribution<T> {
	fn default() -> Self {
		Self::Alive(Default::default())
	}
}

/// Functions that apply to all states.
impl<T, E, S> RawMeter<T, E, S>
where
	T: Config,
	E: Ext<T>,
	S: State,
{
	/// Create a new child that has its `limit`.
	///
	/// This is called whenever a new subcall is initiated in order to track the storage
	/// usage for this sub call separately. This is necessary because we want to exchange balance
	/// with the current contract we are interacting with.
	pub fn nested(&self, mut limit: Option<BalanceOf<T>>) -> RawMeter<T, E, Nested> {
		if let (Some(new_limit), Some(old_limit)) = (limit, self.limit) {
			limit = Some(new_limit.min(old_limit));
		}

		RawMeter { limit, ..Default::default() }
	}

	/// Absorb a child that was spawned to handle a sub call.
	///
	/// This should be called whenever a sub call comes to its end and it is **not** reverted.
	/// This does the actual balance transfer from/to `origin` and `contract` based on the
	/// overall storage consumption of the call. It also updates the supplied contract info.
	///
	/// In case a contract reverted the child meter should just be dropped in order to revert
	/// any changes it recorded.
	///
	/// # Parameters
	///
	/// - `absorbed`: The child storage meter that should be absorbed.
	/// - `origin`: The origin that spawned the original root meter.
	/// - `contract`: The contract's account that this sub call belongs to.
	/// - `info`: The info of the contract in question. `None` if the contract was terminated.
	pub fn absorb(
		&mut self,
		absorbed: RawMeter<T, E, Nested>,
		contract: &T::AccountId,
		info: Option<&mut ContractInfo<T>>,
	) {
		// We are now at the position to calculate the actual final net charge of `absorbed` as we
		// now have the contract information `info`. Before that we only took net charges related to
		// the contract storage into account but ignored net refunds.
		// However, with this complete information there is no need to recalculate `max_charged` for
		// `absorbed` here before we absorb it because the actual final net charge will not be more
		// than the net charge we observed before (as we only ignored net refunds but not net
		// charges).
		self.max_charged = self
			.max_charged
			.max(self.consumed().saturating_add(&absorbed.max_charged()).charge_or_zero());

		let own_deposit = absorbed.own_contribution.update_contract(info);
		self.total_deposit = self
			.total_deposit
			.saturating_add(&absorbed.total_deposit)
			.saturating_add(&own_deposit);
		self.charges.extend_from_slice(&absorbed.charges);

		self.recalulculate_max_charged();

		if !own_deposit.is_zero() {
			self.charges.push(Charge {
				contract: contract.clone(),
				state: ContractState::Alive { amount: own_deposit },
			});
		}
	}

	/// Absorb only the maximum charge of the child meter.
	///
	/// This should be called whenever a sub call ends and reverts.
	///
	/// # Parameters
	///
	/// - `absorbed`: The child storage meter
	pub fn absorb_only_max_charged(&mut self, absorbed: RawMeter<T, E, Nested>) {
		self.max_charged = self
			.max_charged
			.max(self.consumed().saturating_add(&absorbed.max_charged()).charge_or_zero());
	}

	/// Record a charge that has taken place externally.
	///
	/// This will not perform a charge. It just records it to reflect it in the
	/// total amount of storage required for a transaction.
	pub fn record_charge(&mut self, amount: &DepositOf<T>) {
		self.total_deposit = self.total_deposit.saturating_add(amount);
		self.recalulculate_max_charged();
	}

	/// The amount of balance that this meter has consumed.
	///
	/// This disregards any refunds pending in the current frame. This
	/// is because we can calculate refunds only at the end of each frame.
	pub fn consumed(&self) -> DepositOf<T> {
		self.total_deposit.saturating_add(&self.own_contribution.update_contract(None))
	}

	/// Return the maximum consumed deposit at any point in the previous execution
	pub fn max_charged(&self) -> DepositOf<T> {
		Deposit::Charge(self.max_charged)
	}

	/// Recaluclate the max deposit value
	fn recalulculate_max_charged(&mut self) {
		self.max_charged = self.max_charged.max(self.consumed().charge_or_zero());
	}

	/// The amount of balance still available from the current meter.
	///
	/// This includes charges from the current frame but no refunds.
	#[cfg(test)]
	pub fn available(&self) -> BalanceOf<T> {
		self.consumed()
			.available(&self.limit.unwrap_or(BalanceOf::<T>::max_value()))
			.unwrap_or_default()
	}
}

/// Functions that only apply to the root state.
impl<T, E> RawMeter<T, E, Root>
where
	T: Config,
	E: Ext<T>,
{
	/// Create new storage limiting storage deposits to the passed `limit`.
	///
	/// If the limit is larger than what the origin can afford we will just fail
	/// when collecting the deposits in `execute_postponed_deposits`.
	pub fn new(limit: Option<BalanceOf<T>>) -> Self {
		Self {
			limit,
			is_root: true,
			own_contribution: Contribution::Checked(Default::default()),
			..Default::default()
		}
	}

	/// The total amount of deposit that should change hands as result of the execution
	/// that this meter was passed into. This will also perform all the charges accumulated
	/// in the whole contract stack.
	pub fn execute_postponed_deposits(
		&mut self,
		origin: &Origin<T>,
		exec_config: &ExecConfig<T>,
	) -> Result<DepositOf<T>, DispatchError> {
		// Only refund or charge deposit if the origin is not root.
		let origin = match origin {
			Origin::Root => return Ok(Deposit::Charge(Zero::zero())),
			Origin::Signed(o) => o,
		};

		// Coalesce charges of the same contract
		self.charges.sort_by(|a, b| a.contract.cmp(&b.contract));
		self.charges = {
			let mut coalesced: Vec<Charge<T>> = Vec::with_capacity(self.charges.len());
			for mut ch in mem::take(&mut self.charges) {
				if let Some(last) = coalesced.last_mut() {
					if last.contract == ch.contract {
						match (&mut last.state, &mut ch.state) {
							(
								ContractState::Alive { amount: last_amount },
								ContractState::Alive { amount: ch_amount },
							) => {
								*last_amount = last_amount.saturating_add(&ch_amount);
							},
							(ContractState::Alive { amount }, ContractState::Terminated) |
							(ContractState::Terminated, ContractState::Alive { amount }) => {
								// undo all deposits made by a terminated contract
								self.total_deposit = self.total_deposit.saturating_sub(&amount);
								last.state = ContractState::Terminated;
							},
							(ContractState::Terminated, ContractState::Terminated) => {
								debug_assert!(
									false,
									"We never emit two terminates for the same contract."
								)
							},
						}
						continue;
					}
				}
				coalesced.push(ch);
			}
			coalesced
		};

		// refunds first so origin is able to pay for the charges using the refunds
		for charge in self.charges.iter() {
			if let ContractState::Alive { amount: amount @ Deposit::Refund(_) } = &charge.state {
				E::charge(origin, &charge.contract, amount, exec_config)?;
			}
		}
		for charge in self.charges.iter() {
			if let ContractState::Alive { amount: amount @ Deposit::Charge(_) } = &charge.state {
				E::charge(origin, &charge.contract, amount, exec_config)?;
			}
		}

		Ok(self.total_deposit.clone())
	}

	/// Flag a `contract` as terminated.
	///
	/// This will signal to the meter to discard all charged and refunds incured by this
	/// contract.
	pub fn terminate(&mut self, contract: T::AccountId, refunded: BalanceOf<T>) {
		self.total_deposit = self.total_deposit.saturating_add(&Deposit::Refund(refunded));
		self.charges.push(Charge { contract, state: ContractState::Terminated });

		// no need to recalculate max_charged here as the total consumed amount will just decrease
		// with this extra refund
	}
}

/// Functions that only apply to the nested state.
impl<T: Config, E: Ext<T>> RawMeter<T, E, Nested> {
	/// Charges `diff` from the meter.
	pub fn charge(&mut self, diff: &Diff) {
		match &mut self.own_contribution {
			Contribution::Alive(own) => {
				*own = own.saturating_add(diff);
				self.recalulculate_max_charged();
			},
			_ => panic!("Charge is never called after termination; qed"),
		};
	}

	/// Adds a charge without recording it in the contract info.
	///
	/// Use this method instead of [`Self::charge`] when the charge is not the result of a storage
	/// change within the contract's child trie. This is the case when when the `code_hash` is
	/// updated. [`Self::charge`] cannot be used here because we keep track of the deposit charge
	/// separately from the storage charge.
	///
	/// If this functions is used the amount of the charge has to be stored by the caller somewhere
	/// alese in order to be able to refund it.
	pub fn charge_deposit(&mut self, contract: T::AccountId, amount: DepositOf<T>) {
		// will not fail in a nested meter
		self.record_charge(&amount);
		self.charges.push(Charge { contract, state: ContractState::Alive { amount } });
	}

	/// Determine the actual final charge from the own contributions
	pub fn finalize_own_contributions(&mut self, info: Option<&mut ContractInfo<T>>) {
		let deposit = self.own_contribution.update_contract(info);
		self.own_contribution = Contribution::Checked(deposit);

		// no need to recalculate max_charged here as the consumed amount cannot increase
		// when taking removed bytes/items into account
	}

	/// Apply pending storage changes to a ContractInfo without finalizing the meter.
	///
	/// This is used before creating a nested frame to ensure the child frame can see
	/// the parent's pending storage changes when calculating refunds.
	///
	/// Unlike [`Self::finalize_own_contributions`], this does not consume the pending diff,
	/// allowing the meter to continue tracking changes after the nested call returns.
	pub fn apply_pending_changes_to_contract(&self, info: &mut ContractInfo<T>) {
		if let Contribution::Alive(diff) = &self.own_contribution {
			// Apply the diff to update the ContractInfo's storage deposit fields.
			// We don't care about the return value (the deposit amount) here,
			// we just want to update the ContractInfo so child frames can see it.
			let _ = diff.update_contract::<T>(Some(info));
		}
	}

	/// Apply the pending diff to `info` and push its deposit as a final charge, then reset
	/// `own_contribution` so finalize does not apply it a second time.
	pub fn bank_pending_changes(&mut self, contract: T::AccountId, info: &mut ContractInfo<T>) {
		if let Contribution::Alive(_) = &self.own_contribution {
			let deposit = self.own_contribution.update_contract(Some(info));
			self.own_contribution = Contribution::Alive(Default::default());
			if !deposit.is_zero() {
				self.charge_deposit(contract, deposit);
			}
		} else {
			debug_assert!(
				false,
				"on-stack ancestor frames have not finalized yet, so own_contribution \
				 should be Alive when banked; qed",
			);
		}
	}
}

impl<T: Config> Ext<T> for ReservingExt {
	fn charge(
		origin: &T::AccountId,
		contract: &T::AccountId,
		amount: &DepositOf<T>,
		exec_config: &ExecConfig<T>,
	) -> Result<(), DispatchError> {
		match amount {
			Deposit::Charge(amount) | Deposit::Refund(amount) if amount.is_zero() => (),
			Deposit::Charge(amount) => {
				<Pallet<T>>::charge_deposit(
					HoldReason::StorageDepositReserve,
					origin,
					contract,
					*amount,
					exec_config,
				)?;
			},
			Deposit::Refund(amount) => {
				<Pallet<T>>::refund_deposit(
					HoldReason::StorageDepositReserve,
					contract,
					exec_config.funds(origin),
					*amount,
				)?;
			},
		}
		Ok(())
	}
}
