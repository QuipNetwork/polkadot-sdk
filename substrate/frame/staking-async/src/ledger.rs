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

//! A Ledger implementation for stakers.
//!
//! A [`StakingLedger`] encapsulates all the state and logic related to the stake of bonded
//! stakers, namely, it handles the following storage items:
//! * [`Bonded`]: mutates and reads the state of the controller <> stash bond map (to be deprecated
//! soon);
//! * [`Ledger`]: mutates and reads the state of all the stakers. The [`Ledger`] storage item stores
//!   instances of [`StakingLedger`] keyed by the staker's controller account and should be mutated
//!   and read through the [`StakingLedger`] API;
//! * [`Payee`]: mutates and reads the reward destination preferences for a bonded stash.
//! * Staking locks: mutates the locks for staking.
//!
//! NOTE: All the storage operations related to the staking ledger (both reads and writes) *MUST* be
//! performed through the methods exposed by the [`StakingLedger`] implementation in order to ensure
//! state consistency.

use crate::{
	asset, log, BalanceOf, Bonded, Config, DecodeWithMemTracking, Error, Ledger, Pallet, Payee,
	RewardDestination, Vec, VirtualStakers,
};
use alloc::{collections::BTreeMap, fmt::Debug};
use codec::{Decode, Encode, HasCompact, MaxEncodedLen};
use frame_support::{
	defensive, ensure,
	traits::{Defensive, DefensiveSaturating, Get},
	BoundedVec, CloneNoBound, DebugNoBound, EqNoBound, PartialEqNoBound,
};
use scale_info::TypeInfo;
use sp_runtime::{traits::Zero, DispatchResult, Perquintill, Rounding, Saturating};
use sp_staking::{EraIndex, OnStakingUpdate, StakingAccount, StakingInterface};

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
	DebugNoBound,
	TypeInfo,
	MaxEncodedLen,
	DecodeWithMemTracking,
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

	/// The controller associated with this ledger's stash.
	///
	/// This is not stored on-chain, and is only bundled when the ledger is read from storage.
	/// Use [`Self::controller()`] function to get the controller associated with the ledger.
	#[codec(skip)]
	pub controller: Option<T::AccountId>,
}

impl<T: Config> StakingLedger<T> {
	#[cfg(any(feature = "runtime-benchmarks", test))]
	pub fn default_from(stash: T::AccountId) -> Self {
		Self {
			stash: stash.clone(),
			total: Zero::zero(),
			active: Zero::zero(),
			unlocking: Default::default(),
			controller: Some(stash),
		}
	}

	/// Returns a new instance of a staking ledger.
	///
	/// The [`Ledger`] storage is not mutated. In order to store, `StakingLedger::update` must be
	/// called on the returned staking ledger.
	///
	/// Note: as the controller accounts are being deprecated, the stash account is the same as the
	/// controller account.
	pub fn new(stash: T::AccountId, stake: BalanceOf<T>) -> Self {
		Self {
			stash: stash.clone(),
			active: stake,
			total: stake,
			unlocking: Default::default(),
			// controllers are deprecated and mapped 1-1 to stashes.
			controller: Some(stash),
		}
	}

	/// Returns the paired account, if any.
	///
	/// A "pair" refers to the tuple (stash, controller). If the input is a
	/// [`StakingAccount::Stash`] variant, its pair account will be of type
	/// [`StakingAccount::Controller`] and vice-versa.
	///
	/// This method is meant to abstract from the runtime development the difference between stash
	/// and controller. This will be deprecated once the controller is fully deprecated as well.
	pub(crate) fn paired_account(account: StakingAccount<T::AccountId>) -> Option<T::AccountId> {
		match account {
			StakingAccount::Stash(stash) => <Bonded<T>>::get(stash),
			StakingAccount::Controller(controller) => {
				<Ledger<T>>::get(&controller).map(|ledger| ledger.stash)
			},
		}
	}

	/// Returns whether a given account is bonded.
	pub(crate) fn is_bonded(account: StakingAccount<T::AccountId>) -> bool {
		match account {
			StakingAccount::Stash(stash) => <Bonded<T>>::contains_key(stash),
			StakingAccount::Controller(controller) => <Ledger<T>>::contains_key(controller),
		}
	}

	/// Returns a staking ledger, if it is bonded and it exists in storage.
	///
	/// This getter can be called with either a controller or stash account, provided that the
	/// account is properly wrapped in the respective [`StakingAccount`] variant. This is meant to
	/// abstract the concept of controller/stash accounts from the caller.
	///
	/// Returns [`Error::BadState`] when a bond is in "bad state". A bond is in a bad state when a
	/// stash has a controller which is bonding a ledger associated with another stash.
	pub(crate) fn get(account: StakingAccount<T::AccountId>) -> Result<StakingLedger<T>, Error<T>> {
		let (stash, controller) = match account {
			StakingAccount::Stash(stash) => {
				(stash.clone(), <Bonded<T>>::get(&stash).ok_or(Error::<T>::NotStash)?)
			},
			StakingAccount::Controller(controller) => (
				Ledger::<T>::get(&controller)
					.map(|l| l.stash)
					.ok_or(Error::<T>::NotController)?,
				controller,
			),
		};

		let ledger = <Ledger<T>>::get(&controller)
			.map(|mut ledger| {
				ledger.controller = Some(controller.clone());
				ledger
			})
			.ok_or(Error::<T>::NotController)?;

		// if ledger bond is in a bad state, return error to prevent applying operations that may
		// further spoil the ledger's state. A bond is in bad state when the bonded controller is
		// associated with a different ledger (i.e. a ledger with a different stash).
		//
		// See <https://github.com/paritytech/polkadot-sdk/issues/3245> for more details.
		ensure!(
			Bonded::<T>::get(&stash) == Some(controller) && ledger.stash == stash,
			Error::<T>::BadState
		);

		Ok(ledger)
	}

	/// Returns the reward destination of a staking ledger, stored in [`Payee`].
	///
	/// Note: if the stash is not bonded and/or does not have an entry in [`Payee`], it returns the
	/// default reward destination.
	pub(crate) fn reward_destination(
		account: StakingAccount<T::AccountId>,
	) -> Option<RewardDestination<T::AccountId>> {
		let stash = match account {
			StakingAccount::Stash(stash) => Some(stash),
			StakingAccount::Controller(controller) => {
				Self::paired_account(StakingAccount::Controller(controller))
			},
		};

		if let Some(stash) = stash {
			<Payee<T>>::get(stash)
		} else {
			defensive!("fetched reward destination from unbonded stash {}", stash);
			None
		}
	}

	/// Returns the controller account of a staking ledger.
	///
	/// Note: it will fallback into querying the [`Bonded`] storage with the ledger stash if the
	/// controller is not set in `self`, which most likely means that self was fetched directly from
	/// [`Ledger`] instead of through the methods exposed in [`StakingLedger`]. If the ledger does
	/// not exist in storage, it returns `None`.
	pub fn controller(&self) -> Option<T::AccountId> {
		self.controller.clone().or_else(|| {
			defensive!("fetched a controller on a ledger instance without it.");
			Self::paired_account(StakingAccount::Stash(self.stash.clone()))
		})
	}

	/// Inserts/updates a staking ledger account.
	///
	/// Bonds the ledger if it is not bonded yet, signalling that this is a new ledger. The staking
	/// lock/hold of the stash account are updated accordingly.
	///
	/// Note: To ensure lock consistency, all the [`Ledger`] storage updates should be made through
	/// this helper function.
	pub(crate) fn update(self) -> Result<(), Error<T>> {
		if !<Bonded<T>>::contains_key(&self.stash) {
			return Err(Error::<T>::NotStash);
		}

		// We skip locking virtual stakers.
		if !Pallet::<T>::is_virtual_staker(&self.stash) {
			// for direct stakers, update lock on stash based on ledger.
			asset::update_stake::<T>(&self.stash, self.total)
				.map_err(|_| Error::<T>::NotEnoughFunds)?;
		}

		Ledger::<T>::insert(
			&self.controller().ok_or_else(|| {
				defensive!("update called on a ledger that is not bonded.");
				Error::<T>::NotController
			})?,
			&self,
		);

		Ok(())
	}

	/// Bonds a ledger.
	///
	/// It sets the reward preferences for the bonded stash.
	pub(crate) fn bond(self, payee: RewardDestination<T::AccountId>) -> Result<(), Error<T>> {
		if <Bonded<T>>::contains_key(&self.stash) {
			return Err(Error::<T>::AlreadyBonded);
		}

		<Payee<T>>::insert(&self.stash, payee);
		<Bonded<T>>::insert(&self.stash, &self.stash);
		self.update()
	}

	/// Sets the ledger Payee.
	pub(crate) fn set_payee(self, payee: RewardDestination<T::AccountId>) -> Result<(), Error<T>> {
		if !<Bonded<T>>::contains_key(&self.stash) {
			return Err(Error::<T>::NotStash);
		}

		<Payee<T>>::insert(&self.stash, payee);
		Ok(())
	}

	/// Sets the ledger controller to its stash.
	pub(crate) fn set_controller_to_stash(self) -> Result<(), Error<T>> {
		let controller = self.controller.as_ref()
            .defensive_proof("Ledger's controller field didn't exist. The controller should have been fetched using StakingLedger.")
            .ok_or(Error::<T>::NotController)?;

		ensure!(self.stash != *controller, Error::<T>::AlreadyPaired);

		// check if the ledger's stash is a controller of another ledger.
		if let Some(bonded_ledger) = Ledger::<T>::get(&self.stash) {
			// there is a ledger bonded by the stash. In this case, the stash of the bonded ledger
			// should be the same as the ledger's stash. Otherwise fail to prevent data
			// inconsistencies. See <https://github.com/paritytech/polkadot-sdk/pull/3639> for more
			// details.
			ensure!(bonded_ledger.stash == self.stash, Error::<T>::BadState);
		}

		<Ledger<T>>::remove(&controller);
		<Ledger<T>>::insert(&self.stash, &self);
		<Bonded<T>>::insert(&self.stash, &self.stash);

		Ok(())
	}

	/// Clears all data related to a staking ledger and its bond in both [`Ledger`] and [`Bonded`]
	/// storage items and updates the stash staking lock.
	pub(crate) fn kill(stash: &T::AccountId) -> DispatchResult {
		let controller = <Bonded<T>>::get(stash).ok_or(Error::<T>::NotStash)?;

		<Ledger<T>>::get(&controller).ok_or(Error::<T>::NotController).map(|ledger| {
			Ledger::<T>::remove(controller);
			<Bonded<T>>::remove(&stash);
			<Payee<T>>::remove(&stash);

			// kill virtual staker if it exists.
			if <VirtualStakers<T>>::take(&ledger.stash).is_none() {
				// if not virtual staker, clear locks.
				asset::kill_stake::<T>(&ledger.stash)?;
			}
			Pallet::<T>::deposit_event(crate::Event::<T>::StakerRemoved {
				stash: ledger.stash.clone(),
			});
			Ok(())
		})?
	}

	#[cfg(test)]
	pub(crate) fn assert_stash_killed(stash: T::AccountId) {
		assert!(!Ledger::<T>::contains_key(&stash));
		assert!(!Bonded::<T>::contains_key(&stash));
		assert!(!Payee::<T>::contains_key(&stash));
		assert!(!VirtualStakers::<T>::contains_key(&stash));
	}

	/// Remove entries from `unlocking` that are sufficiently old and reduce the
	/// total by the sum of their balances.
	pub(crate) fn consolidate_unlocked(self, current_era: EraIndex) -> Self {
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
			controller: self.controller,
		}
	}

	/// Re-bond funds that were scheduled for unlocking.
	///
	/// Returns the updated ledger, and the amount actually rebonded.
	pub(crate) fn rebond(mut self, value: BalanceOf<T>) -> (Self, BalanceOf<T>) {
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
	/// - If any unlocking chunks exist that are scheduled to be unlocked at `offence_era +
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
	/// `offence_era` is the era in which the offence occurred (not the era it is enacted in).
	///
	/// This calls `Config::OnStakingUpdate::on_slash` with information as to how the slash was
	/// applied.
	pub(crate) fn slash(
		&mut self,
		slash_amount: BalanceOf<T>,
		minimum_balance: BalanceOf<T>,
		offence_era: EraIndex,
	) -> BalanceOf<T> {
		if slash_amount.is_zero() {
			return Zero::zero();
		}

		use sp_runtime::PerThing as _;
		let mut remaining_slash = slash_amount;
		let pre_slash_total = self.total;

		// for an `offence_era = x`, any chunk that is scheduled to be unlocked at era `x + 28`
		// (assuming 28 is the bonding duration) onwards should be slashed.
		let slashable_chunks_start = offence_era.saturating_add(T::BondingDuration::get());

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
			trace,
			"slashing {:?} for offence era {:?} out of {:?}, priority: {:?}, proportional = {:?}",
			slash_amount,
			offence_era,
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

/// State of a ledger with regards with its data and metadata integrity.
#[derive(PartialEq, Debug)]
pub(crate) enum LedgerIntegrityState {
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

// This structs makes it easy to write tests to compare staking ledgers fetched from storage. This
// is required because the controller field is not stored in storage and it is private.
#[cfg(test)]
#[derive(frame_support::DebugNoBound, Clone, Encode, Decode, TypeInfo, MaxEncodedLen)]
pub struct StakingLedgerInspect<T: Config> {
	pub stash: T::AccountId,
	#[codec(compact)]
	pub total: BalanceOf<T>,
	#[codec(compact)]
	pub active: BalanceOf<T>,
	pub unlocking:
		frame_support::BoundedVec<crate::UnlockChunk<BalanceOf<T>>, T::MaxUnlockingChunks>,
}

#[cfg(test)]
impl<T: Config> PartialEq<StakingLedgerInspect<T>> for StakingLedger<T> {
	fn eq(&self, other: &StakingLedgerInspect<T>) -> bool {
		self.stash == other.stash &&
			self.total == other.total &&
			self.active == other.active &&
			self.unlocking == other.unlocking
	}
}

#[cfg(test)]
impl<T: Config> codec::EncodeLike<StakingLedger<T>> for StakingLedgerInspect<T> {}
