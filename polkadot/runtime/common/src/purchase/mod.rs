// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Pallet to process purchase of DOTs.

use alloc::vec::Vec;
use codec::{Decode, Encode};
use frame_support::{
	pallet_prelude::*,
	traits::{Currency, EnsureOrigin, ExistenceRequirement, Get, VestingSchedule},
};
use frame_system::pallet_prelude::*;
pub use pallet::*;
use scale_info::TypeInfo;
use sp_core::sr25519;
use sp_runtime::{
	traits::{CheckedAdd, Saturating, Verify, Zero},
	AnySignature, Debug, DispatchError, DispatchResult, Permill,
};

type BalanceOf<T> =
	<<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

/// The kind of statement an account needs to make for a claim to be valid.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, Copy, Eq, PartialEq, Debug, TypeInfo)]
pub enum AccountValidity {
	/// Account is not valid.
	Invalid,
	/// Account has initiated the account creation process.
	Initiated,
	/// Account is pending validation.
	Pending,
	/// Account is valid with a low contribution amount.
	ValidLow,
	/// Account is valid with a high contribution amount.
	ValidHigh,
	/// Account has completed the purchase process.
	Completed,
}

impl Default for AccountValidity {
	fn default() -> Self {
		AccountValidity::Invalid
	}
}

impl AccountValidity {
	fn is_valid(&self) -> bool {
		match self {
			Self::Invalid => false,
			Self::Initiated => false,
			Self::Pending => false,
			Self::ValidLow => true,
			Self::ValidHigh => true,
			Self::Completed => false,
		}
	}
}

/// All information about an account regarding the purchase of DOTs.
#[derive(Encode, Decode, Default, Clone, Eq, PartialEq, Debug, TypeInfo)]
pub struct AccountStatus<Balance> {
	/// The current validity status of the user. Will denote if the user has passed KYC,
	/// how much they are able to purchase, and when their purchase process has completed.
	validity: AccountValidity,
	/// The amount of free DOTs they have purchased.
	free_balance: Balance,
	/// The amount of locked DOTs they have purchased.
	locked_balance: Balance,
	/// Their sr25519/ed25519 signature verifying they have signed our required statement.
	signature: Vec<u8>,
	/// The percentage of VAT the purchaser is responsible for. This is already factored into
	/// account balance.
	vat: Permill,
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	#[pallet::without_storage_info]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Balances Pallet
		type Currency: Currency<Self::AccountId>;

		/// Vesting Pallet
		type VestingSchedule: VestingSchedule<
			Self::AccountId,
			Moment = BlockNumberFor<Self>,
			Currency = Self::Currency,
		>;

		/// The origin allowed to set account status.
		type ValidityOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// The origin allowed to make configurations to the pallet.
		type ConfigurationOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// The maximum statement length for the statement users to sign when creating an account.
		#[pallet::constant]
		type MaxStatementLength: Get<u32>;

		/// The amount of purchased locked DOTs that we will unlock for basic actions on the chain.
		#[pallet::constant]
		type UnlockedProportion: Get<Permill>;

		/// The maximum amount of locked DOTs that we will unlock.
		#[pallet::constant]
		type MaxUnlocked: Get<BalanceOf<Self>>;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A new account was created.
		AccountCreated { who: T::AccountId },
		/// Someone's account validity was updated.
		ValidityUpdated { who: T::AccountId, validity: AccountValidity },
		/// Someone's purchase balance was updated.
		BalanceUpdated { who: T::AccountId, free: BalanceOf<T>, locked: BalanceOf<T> },
		/// A payout was made to a purchaser.
		PaymentComplete { who: T::AccountId, free: BalanceOf<T>, locked: BalanceOf<T> },
		/// A new payment account was set.
		PaymentAccountSet { who: T::AccountId },
		/// A new statement was set.
		StatementUpdated,
		/// A new statement was set. `[block_number]`
		UnlockBlockUpdated { block_number: BlockNumberFor<T> },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Account is not currently valid to use.
		InvalidAccount,
		/// Account used in the purchase already exists.
		ExistingAccount,
		/// Provided signature is invalid
		InvalidSignature,
		/// Account has already completed the purchase process.
		AlreadyCompleted,
		/// An overflow occurred when doing calculations.
		Overflow,
		/// The statement is too long to be stored on chain.
		InvalidStatement,
		/// The unlock block is in the past!
		InvalidUnlockBlock,
		/// Vesting schedule already exists for this account.
		VestingScheduleExists,
	}

	// A map of all participants in the DOT purchase process.
	#[pallet::storage]
	pub(super) type Accounts<T: Config> =
		StorageMap<_, Blake2_128Concat, T::AccountId, AccountStatus<BalanceOf<T>>, ValueQuery>;

	// The account that will be used to payout participants of the DOT purchase process.
	#[pallet::storage]
	pub(super) type PaymentAccount<T: Config> = StorageValue<_, T::AccountId, OptionQuery>;

	// The statement purchasers will need to sign to participate.
	#[pallet::storage]
	pub(super) type Statement<T> = StorageValue<_, Vec<u8>, ValueQuery>;

	// The block where all locked dots will unlock.
	#[pallet::storage]
	pub(super) type UnlockBlock<T: Config> = StorageValue<_, BlockNumberFor<T>, ValueQuery>;

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Create a new account. Proof of existence through a valid signed message.
		///
		/// We check that the account does not exist at this stage.
		///
		/// Origin must match the `ValidityOrigin`.
		#[pallet::call_index(0)]
		#[pallet::weight(Weight::from_parts(200_000_000, 0) + T::DbWeight::get().reads_writes(4, 1))]
		pub fn create_account(
			origin: OriginFor<T>,
			who: T::AccountId,
			signature: Vec<u8>,
		) -> DispatchResult {
			T::ValidityOrigin::ensure_origin(origin)?;
			// Account is already being tracked by the pallet.
			ensure!(!Accounts::<T>::contains_key(&who), Error::<T>::ExistingAccount);
			// Account should not have a vesting schedule.
			ensure!(
				T::VestingSchedule::vesting_balance(&who).is_none(),
				Error::<T>::VestingScheduleExists
			);

			// Verify the signature provided is valid for the statement.
			Self::verify_signature(&who, &signature)?;

			// Create a new pending account.
			let status = AccountStatus {
				validity: AccountValidity::Initiated,
				signature,
				free_balance: Zero::zero(),
				locked_balance: Zero::zero(),
				vat: Permill::zero(),
			};
			Accounts::<T>::insert(&who, status);
			Self::deposit_event(Event::<T>::AccountCreated { who });
			Ok(())
		}

		/// Update the validity status of an existing account. If set to completed, the account
		/// will no longer be able to continue through the crowdfund process.
		///
		/// We check that the account exists at this stage, but has not completed the process.
		///
		/// Origin must match the `ValidityOrigin`.
		#[pallet::call_index(1)]
		#[pallet::weight(T::DbWeight::get().reads_writes(1, 1))]
		pub fn update_validity_status(
			origin: OriginFor<T>,
			who: T::AccountId,
			validity: AccountValidity,
		) -> DispatchResult {
			T::ValidityOrigin::ensure_origin(origin)?;
			ensure!(Accounts::<T>::contains_key(&who), Error::<T>::InvalidAccount);
			Accounts::<T>::try_mutate(
				&who,
				|status: &mut AccountStatus<BalanceOf<T>>| -> DispatchResult {
					ensure!(
						status.validity != AccountValidity::Completed,
						Error::<T>::AlreadyCompleted
					);
					status.validity = validity;
					Ok(())
				},
			)?;
			Self::deposit_event(Event::<T>::ValidityUpdated { who, validity });
			Ok(())
		}

		/// Update the balance of a valid account.
		///
		/// We check that the account is valid for a balance transfer at this point.
		///
		/// Origin must match the `ValidityOrigin`.
		#[pallet::call_index(2)]
		#[pallet::weight(T::DbWeight::get().reads_writes(2, 1))]
		pub fn update_balance(
			origin: OriginFor<T>,
			who: T::AccountId,
			free_balance: BalanceOf<T>,
			locked_balance: BalanceOf<T>,
			vat: Permill,
		) -> DispatchResult {
			T::ValidityOrigin::ensure_origin(origin)?;

			Accounts::<T>::try_mutate(
				&who,
				|status: &mut AccountStatus<BalanceOf<T>>| -> DispatchResult {
					// Account has a valid status (not Invalid, Pending, or Completed)...
					ensure!(status.validity.is_valid(), Error::<T>::InvalidAccount);

					free_balance.checked_add(&locked_balance).ok_or(Error::<T>::Overflow)?;
					status.free_balance = free_balance;
					status.locked_balance = locked_balance;
					status.vat = vat;
					Ok(())
				},
			)?;
			Self::deposit_event(Event::<T>::BalanceUpdated {
				who,
				free: free_balance,
				locked: locked_balance,
			});
			Ok(())
		}

		/// Pay the user and complete the purchase process.
		///
		/// We reverify all assumptions about the state of an account, and complete the process.
		///
		/// Origin must match the configured `PaymentAccount` (if it is not configured then this
		/// will always fail with `BadOrigin`).
		#[pallet::call_index(3)]
		#[pallet::weight(T::DbWeight::get().reads_writes(4, 2))]
		pub fn payout(origin: OriginFor<T>, who: T::AccountId) -> DispatchResult {
			// Payments must be made directly by the `PaymentAccount`.
			let payment_account = ensure_signed(origin)?;
			let test_against = PaymentAccount::<T>::get().ok_or(DispatchError::BadOrigin)?;
			ensure!(payment_account == test_against, DispatchError::BadOrigin);

			// Account should not have a vesting schedule.
			ensure!(
				T::VestingSchedule::vesting_balance(&who).is_none(),
				Error::<T>::VestingScheduleExists
			);

			Accounts::<T>::try_mutate(
				&who,
				|status: &mut AccountStatus<BalanceOf<T>>| -> DispatchResult {
					// Account has a valid status (not Invalid, Pending, or Completed)...
					ensure!(status.validity.is_valid(), Error::<T>::InvalidAccount);

					// Transfer funds from the payment account into the purchasing user.
					let total_balance = status
						.free_balance
						.checked_add(&status.locked_balance)
						.ok_or(Error::<T>::Overflow)?;
					T::Currency::transfer(
						&payment_account,
						&who,
						total_balance,
						ExistenceRequirement::AllowDeath,
					)?;

					if !status.locked_balance.is_zero() {
						let unlock_block = UnlockBlock::<T>::get();
						// We allow some configurable portion of the purchased locked DOTs to be
						// unlocked for basic usage.
						let unlocked = (T::UnlockedProportion::get() * status.locked_balance)
							.min(T::MaxUnlocked::get());
						let locked = status.locked_balance.saturating_sub(unlocked);
						// We checked that this account has no existing vesting schedule. So this
						// function should never fail, however if it does, not much we can do about
						// it at this point.
						let _ = T::VestingSchedule::add_vesting_schedule(
							// Apply vesting schedule to this user
							&who,
							// For this much amount
							locked,
							// Unlocking the full amount after one block
							locked,
							// When everything unlocks
							unlock_block,
						);
					}

					// Setting the user account to `Completed` ends the purchase process for this
					// user.
					status.validity = AccountValidity::Completed;
					Self::deposit_event(Event::<T>::PaymentComplete {
						who: who.clone(),
						free: status.free_balance,
						locked: status.locked_balance,
					});
					Ok(())
				},
			)?;
			Ok(())
		}

		// Configuration Operations

		/// Set the account that will be used to payout users in the DOT purchase process.
		///
		/// Origin must match the `ConfigurationOrigin`
		#[pallet::call_index(4)]
		#[pallet::weight(T::DbWeight::get().writes(1))]
		pub fn set_payment_account(origin: OriginFor<T>, who: T::AccountId) -> DispatchResult {
			T::ConfigurationOrigin::ensure_origin(origin)?;
			// Possibly this is worse than having the caller account be the payment account?
			PaymentAccount::<T>::put(who.clone());
			Self::deposit_event(Event::<T>::PaymentAccountSet { who });
			Ok(())
		}

		/// Set the statement that must be signed for a user to participate on the DOT sale.
		///
		/// Origin must match the `ConfigurationOrigin`
		#[pallet::call_index(5)]
		#[pallet::weight(T::DbWeight::get().writes(1))]
		pub fn set_statement(origin: OriginFor<T>, statement: Vec<u8>) -> DispatchResult {
			T::ConfigurationOrigin::ensure_origin(origin)?;
			ensure!(
				(statement.len() as u32) < T::MaxStatementLength::get(),
				Error::<T>::InvalidStatement
			);
			// Possibly this is worse than having the caller account be the payment account?
			Statement::<T>::set(statement);
			Self::deposit_event(Event::<T>::StatementUpdated);
			Ok(())
		}

		/// Set the block where locked DOTs will become unlocked.
		///
		/// Origin must match the `ConfigurationOrigin`
		#[pallet::call_index(6)]
		#[pallet::weight(T::DbWeight::get().writes(1))]
		pub fn set_unlock_block(
			origin: OriginFor<T>,
			unlock_block: BlockNumberFor<T>,
		) -> DispatchResult {
			T::ConfigurationOrigin::ensure_origin(origin)?;
			ensure!(
				unlock_block > frame_system::Pallet::<T>::block_number(),
				Error::<T>::InvalidUnlockBlock
			);
			// Possibly this is worse than having the caller account be the payment account?
			UnlockBlock::<T>::set(unlock_block);
			Self::deposit_event(Event::<T>::UnlockBlockUpdated { block_number: unlock_block });
			Ok(())
		}
	}
}

impl<T: Config> Pallet<T> {
	fn verify_signature(who: &T::AccountId, signature: &[u8]) -> Result<(), DispatchError> {
		// sr25519 always expects a 64 byte signature.
		let signature: AnySignature = sr25519::Signature::try_from(signature)
			.map_err(|_| Error::<T>::InvalidSignature)?
			.into();

		// In Polkadot, the AccountId is always the same as the 32 byte public key.
		let account_bytes: [u8; 32] = account_to_bytes(who)?;
		let public_key = sr25519::Public::from_raw(account_bytes);

		let message = Statement::<T>::get();

		// Check if everything is good or not.
		match signature.verify(message.as_slice(), &public_key) {
			true => Ok(()),
			false => Err(Error::<T>::InvalidSignature)?,
		}
	}
}

// This function converts a 32 byte AccountId to its byte-array equivalent form.
fn account_to_bytes<AccountId>(account: &AccountId) -> Result<[u8; 32], DispatchError>
where
	AccountId: Encode,
{
	let account_vec = account.encode();
	ensure!(account_vec.len() == 32, "AccountId must be 32 bytes.");
	let mut bytes = [0u8; 32];
	bytes.copy_from_slice(&account_vec);
	Ok(bytes)
}

/// WARNING: Executing this function will clear all storage used by this pallet.
/// Be sure this is what you want...
pub fn remove_pallet<T>() -> frame_support::weights::Weight
where
	T: frame_system::Config,
{
	use frame_support::migration::clear_storage_prefix;
	let _ = clear_storage_prefix(b"Purchase", b"Accounts", b"", None, None);
	let _ = clear_storage_prefix(b"Purchase", b"PaymentAccount", b"", None, None);
	let _ = clear_storage_prefix(b"Purchase", b"Statement", b"", None, None);
	let _ = clear_storage_prefix(b"Purchase", b"UnlockBlock", b"", None, None);

	<T as frame_system::Config>::BlockWeights::get().max_block
}

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;
