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

//! # Balances Pallet
//!
//! The Balances pallet provides functionality for handling accounts and balances for a single
//! token.
//!
//! It makes heavy use of concepts such as Holds and Freezes from the
//! [`frame_support::traits::fungible`] traits, therefore you should read and understand those docs
//! as a prerequisite to understanding this pallet.
//!
//! Also see the [`frame_tokens`] reference docs for higher level information regarding the
//! place of this palet in FRAME.
//!
//! ## Overview
//!
//! The Balances pallet provides functions for:
//!
//! - Getting and setting free balances.
//! - Retrieving total, reserved and unreserved balances.
//! - Repatriating a reserved balance to a beneficiary account that exists.
//! - Transferring a balance between accounts (when not reserved).
//! - Slashing an account balance.
//! - Account creation and removal.
//! - Managing total issuance.
//! - Setting and managing locks.
//!
//! ### Terminology
//!
//! - **Reaping an account:** The act of removing an account by resetting its nonce. Happens after
//!   its total balance has become less than the Existential Deposit.
//!
//! ### Implementations
//!
//! The Balances pallet provides implementations for the following [`fungible`] traits. If these
//! traits provide the functionality that you need, then you should avoid tight coupling with the
//! Balances pallet.
//!
//! - [`fungible::Inspect`]
//! - [`fungible::Mutate`]
//! - [`fungible::Unbalanced`]
//! - [`fungible::Balanced`]
//! - [`fungible::BalancedHold`]
//! - [`fungible::InspectHold`]
//! - [`fungible::MutateHold`]
//! - [`fungible::InspectFreeze`]
//! - [`fungible::MutateFreeze`]
//! - [`fungible::Imbalance`]
//!
//! It also implements the following [`Currency`] related traits, however they are deprecated and
//! will eventually be removed.
//!
//! - [`Currency`]: Functions for dealing with a fungible assets system.
//! - [`ReservableCurrency`]
//! - [`NamedReservableCurrency`](frame_support::traits::NamedReservableCurrency):
//! Functions for dealing with assets that can be reserved from an account.
//! - [`LockableCurrency`](frame_support::traits::LockableCurrency): Functions for
//! dealing with accounts that allow liquidity restrictions.
//! - [`Imbalance`](frame_support::traits::Imbalance): Functions for handling
//! imbalances between total issuance in the system and account balances. Must be used when a
//! function creates new funds (e.g. a reward) or destroys some funds (e.g. a system fee).
//!
//! ## Usage
//!
//! The following examples show how to use the Balances pallet in your custom pallet.
//!
//! ### Examples from the FRAME
//!
//! The Contract pallet uses the `Currency` trait to handle gas payment, and its types inherit from
//! `Currency`:
//!
//! ```
//! use frame_support::traits::Currency;
//! # pub trait Config: frame_system::Config {
//! #   type Currency: Currency<Self::AccountId>;
//! # }
//!
//! pub type BalanceOf<T> = <<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;
//! pub type NegativeImbalanceOf<T> = <<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::NegativeImbalance;
//!
//! # fn main() {}
//! ```
//!
//! The Staking pallet uses the `LockableCurrency` trait to lock a stash account's funds:
//!
//! ```
//! use frame_support::traits::{WithdrawReasons, LockableCurrency};
//! use sp_runtime::traits::Bounded;
//! pub trait Config: frame_system::Config {
//!     type Currency: LockableCurrency<Self::AccountId, Moment=frame_system::pallet_prelude::BlockNumberFor<Self>>;
//! }
//! # struct StakingLedger<T: Config> {
//! #   stash: <T as frame_system::Config>::AccountId,
//! #   total: <<T as Config>::Currency as frame_support::traits::Currency<<T as frame_system::Config>::AccountId>>::Balance,
//! #   phantom: std::marker::PhantomData<T>,
//! # }
//! # const STAKING_ID: [u8; 8] = *b"staking ";
//!
//! fn update_ledger<T: Config>(
//!     controller: &T::AccountId,
//!     ledger: &StakingLedger<T>
//! ) {
//!     T::Currency::set_lock(
//!         STAKING_ID,
//!         &ledger.stash,
//!         ledger.total,
//!         WithdrawReasons::all()
//!     );
//!     // <Ledger<T>>::insert(controller, ledger); // Commented out as we don't have access to Staking's storage here.
//! }
//! # fn main() {}
//! ```
//!
//! ## Genesis config
//!
//! The Balances pallet depends on the [`GenesisConfig`].
//!
//! ## Assumptions
//!
//! * Total issued balanced of all accounts should be less than `Config::Balance::max_value()`.
//! * Existential Deposit is set to a value greater than zero.
//!
//! Note, you may find the Balances pallet still functions with an ED of zero when the
//! `insecure_zero_ed` cargo feature is enabled. However this is not a configuration which is
//! generally supported, nor will it be.
//!
//! [`frame_tokens`]: ../polkadot_sdk_docs/reference_docs/frame_tokens/index.html

#![cfg_attr(not(feature = "std"), no_std)]
mod benchmarking;
mod impl_currency;
mod impl_fungible;
pub mod migration;
mod tests;
mod types;
pub mod weights;

extern crate alloc;

use alloc::{
	format,
	string::{String, ToString},
	vec::Vec,
};
use codec::{Codec, MaxEncodedLen};
use core::{cmp, fmt::Debug, mem, result};
use frame_support::{
	ensure,
	pallet_prelude::DispatchResult,
	traits::{
		tokens::{
			fungible, BalanceStatus as Status, DepositConsequence,
			Fortitude::{self, Force, Polite},
			IdAmount,
			Preservation::{Expendable, Preserve, Protect},
			WithdrawConsequence,
		},
		Currency, Defensive, Get, OnUnbalanced, ReservableCurrency, StoredMap,
	},
	BoundedSlice, WeakBoundedVec,
};
use frame_system as system;
pub use impl_currency::{NegativeImbalance, PositiveImbalance};
use scale_info::TypeInfo;
use sp_core::{sr25519::Pair as SrPair, Pair};
use sp_runtime::{
	traits::{
		AtLeast32BitUnsigned, CheckedAdd, CheckedSub, MaybeSerializeDeserialize, Saturating,
		StaticLookup, Zero,
	},
	ArithmeticError, DispatchError, FixedPointOperand, Perbill, TokenError,
};

pub use types::{
	AccountData, AdjustmentDirection, BalanceLock, DustCleaner, ExtraFlags, Reasons, ReserveData,
};
pub use weights::WeightInfo;

pub use pallet::*;

const LOG_TARGET: &str = "runtime::balances";

// Default derivation(hard) for development accounts.
const DEFAULT_ADDRESS_URI: &str = "//Sender//{}";

type AccountIdLookupOf<T> = <<T as frame_system::Config>::Lookup as StaticLookup>::Source;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use codec::HasCompact;
	use frame_support::{
		pallet_prelude::*,
		traits::{fungible::Credit, tokens::Precision, VariantCount, VariantCountOf},
	};
	use frame_system::pallet_prelude::*;

	pub type CreditOf<T, I> = Credit<<T as frame_system::Config>::AccountId, Pallet<T, I>>;

	/// Default implementations of [`DefaultConfig`], which can be used to implement [`Config`].
	pub mod config_preludes {
		use super::*;
		use frame_support::derive_impl;

		pub struct TestDefaultConfig;

		#[derive_impl(frame_system::config_preludes::TestDefaultConfig, no_aggregated_types)]
		impl frame_system::DefaultConfig for TestDefaultConfig {}

		#[frame_support::register_default_impl(TestDefaultConfig)]
		impl DefaultConfig for TestDefaultConfig {
			#[inject_runtime_type]
			type RuntimeEvent = ();
			#[inject_runtime_type]
			type RuntimeHoldReason = ();
			#[inject_runtime_type]
			type RuntimeFreezeReason = ();

			type Balance = u64;
			type ExistentialDeposit = ConstUint<1>;

			type ReserveIdentifier = ();
			type FreezeIdentifier = Self::RuntimeFreezeReason;

			type DustRemoval = ();

			type MaxLocks = ConstU32<100>;
			type MaxReserves = ConstU32<100>;
			type MaxFreezes = VariantCountOf<Self::RuntimeFreezeReason>;

			type WeightInfo = ();
			type DoneSlashHandler = ();
		}
	}

	#[pallet::config(with_default)]
	pub trait Config<I: 'static = ()>: frame_system::Config {
		/// The overarching event type.
		#[pallet::no_default_bounds]
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self, I>>
			+ IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The overarching hold reason.
		#[pallet::no_default_bounds]
		type RuntimeHoldReason: Parameter + Member + MaxEncodedLen + Copy + VariantCount;

		/// The overarching freeze reason.
		#[pallet::no_default_bounds]
		type RuntimeFreezeReason: VariantCount;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;

		/// The balance of an account.
		type Balance: Parameter
			+ Member
			+ AtLeast32BitUnsigned
			+ Codec
			+ HasCompact<Type: DecodeWithMemTracking>
			+ Default
			+ Copy
			+ MaybeSerializeDeserialize
			+ Debug
			+ MaxEncodedLen
			+ TypeInfo
			+ FixedPointOperand;

		/// Handler for the unbalanced reduction when removing a dust account.
		#[pallet::no_default_bounds]
		type DustRemoval: OnUnbalanced<CreditOf<Self, I>>;

		/// The minimum amount required to keep an account open. MUST BE GREATER THAN ZERO!
		///
		/// If you *really* need it to be zero, you can enable the feature `insecure_zero_ed` for
		/// this pallet. However, you do so at your own risk: this will open up a major DoS vector.
		/// In case you have multiple sources of provider references, you may also get unexpected
		/// behaviour if you set this to zero.
		///
		/// Bottom line: Do yourself a favour and make it at least one!
		#[pallet::constant]
		#[pallet::no_default_bounds]
		type ExistentialDeposit: Get<Self::Balance>;

		/// The means of storing the balances of an account.
		#[pallet::no_default]
		type AccountStore: StoredMap<Self::AccountId, AccountData<Self::Balance>>;

		/// The ID type for reserves.
		///
		/// Use of reserves is deprecated in favour of holds. See `https://github.com/paritytech/substrate/pull/12951/`
		type ReserveIdentifier: Parameter + Member + MaxEncodedLen + Ord + Copy;

		/// The ID type for freezes.
		type FreezeIdentifier: Parameter + Member + MaxEncodedLen + Copy;

		/// The maximum number of locks that should exist on an account.
		/// Not strictly enforced, but used for weight estimation.
		///
		/// Use of locks is deprecated in favour of freezes. See `https://github.com/paritytech/substrate/pull/12951/`
		#[pallet::constant]
		type MaxLocks: Get<u32>;

		/// The maximum number of named reserves that can exist on an account.
		///
		/// Use of reserves is deprecated in favour of holds. See `https://github.com/paritytech/substrate/pull/12951/`
		#[pallet::constant]
		type MaxReserves: Get<u32>;

		/// The maximum number of individual freeze locks that can exist on an account at any time.
		#[pallet::constant]
		type MaxFreezes: Get<u32>;

		/// Allows callbacks to other pallets so they can update their bookkeeping when a slash
		/// occurs.
		type DoneSlashHandler: fungible::hold::DoneSlash<
			Self::RuntimeHoldReason,
			Self::AccountId,
			Self::Balance,
		>;
	}

	/// The in-code storage version.
	const STORAGE_VERSION: frame_support::traits::StorageVersion =
		frame_support::traits::StorageVersion::new(1);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T, I = ()>(PhantomData<(T, I)>);

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config<I>, I: 'static = ()> {
		/// An account was created with some free balance.
		Endowed { account: T::AccountId, free_balance: T::Balance },
		/// An account was removed whose balance was non-zero but below ExistentialDeposit,
		/// resulting in an outright loss.
		DustLost { account: T::AccountId, amount: T::Balance },
		/// Transfer succeeded.
		Transfer { from: T::AccountId, to: T::AccountId, amount: T::Balance },
		/// A balance was set by root.
		BalanceSet { who: T::AccountId, free: T::Balance },
		/// Some balance was reserved (moved from free to reserved).
		Reserved { who: T::AccountId, amount: T::Balance },
		/// Some balance was unreserved (moved from reserved to free).
		Unreserved { who: T::AccountId, amount: T::Balance },
		/// Some balance was moved from the reserve of the first account to the second account.
		/// Final argument indicates the destination balance type.
		ReserveRepatriated {
			from: T::AccountId,
			to: T::AccountId,
			amount: T::Balance,
			destination_status: Status,
		},
		/// Some amount was deposited (e.g. for transaction fees).
		Deposit { who: T::AccountId, amount: T::Balance },
		/// Some amount was withdrawn from the account (e.g. for transaction fees).
		Withdraw { who: T::AccountId, amount: T::Balance },
		/// Some amount was removed from the account (e.g. for misbehavior).
		Slashed { who: T::AccountId, amount: T::Balance },
		/// Some amount was minted into an account.
		Minted { who: T::AccountId, amount: T::Balance },
		/// Some credit was balanced and added to the TotalIssuance.
		MintedCredit { amount: T::Balance },
		/// Some amount was burned from an account.
		Burned { who: T::AccountId, amount: T::Balance },
		/// Some debt has been dropped from the Total Issuance.
		BurnedDebt { amount: T::Balance },
		/// Some amount was suspended from an account (it can be restored later).
		Suspended { who: T::AccountId, amount: T::Balance },
		/// Some amount was restored into an account.
		Restored { who: T::AccountId, amount: T::Balance },
		/// An account was upgraded.
		Upgraded { who: T::AccountId },
		/// Total issuance was increased by `amount`, creating a credit to be balanced.
		Issued { amount: T::Balance },
		/// Total issuance was decreased by `amount`, creating a debt to be balanced.
		Rescinded { amount: T::Balance },
		/// Some balance was locked.
		Locked { who: T::AccountId, amount: T::Balance },
		/// Some balance was unlocked.
		Unlocked { who: T::AccountId, amount: T::Balance },
		/// Some balance was frozen.
		Frozen { who: T::AccountId, amount: T::Balance },
		/// Some balance was thawed.
		Thawed { who: T::AccountId, amount: T::Balance },
		/// The `TotalIssuance` was forcefully changed.
		TotalIssuanceForced { old: T::Balance, new: T::Balance },
		/// Some balance was placed on hold.
		Held { reason: T::RuntimeHoldReason, who: T::AccountId, amount: T::Balance },
		/// Held balance was burned from an account.
		BurnedHeld { reason: T::RuntimeHoldReason, who: T::AccountId, amount: T::Balance },
		/// A transfer of `amount` on hold from `source` to `dest` was initiated.
		TransferOnHold {
			reason: T::RuntimeHoldReason,
			source: T::AccountId,
			dest: T::AccountId,
			amount: T::Balance,
		},
		/// The `transferred` balance is placed on hold at the `dest` account.
		TransferAndHold {
			reason: T::RuntimeHoldReason,
			source: T::AccountId,
			dest: T::AccountId,
			transferred: T::Balance,
		},
		/// Some balance was released from hold.
		Released { reason: T::RuntimeHoldReason, who: T::AccountId, amount: T::Balance },
		/// An unexpected/defensive event was triggered.
		Unexpected(UnexpectedKind),
	}

	/// Defensive/unexpected errors/events.
	///
	/// In case of observation in explorers, report it as an issue in polkadot-sdk.
	#[derive(Clone, Encode, Decode, DecodeWithMemTracking, PartialEq, TypeInfo, Debug)]
	pub enum UnexpectedKind {
		/// Balance was altered/dusted during an operation that should have NOT done so.
		BalanceUpdated,
		/// Mutating the account failed unexpectedly. This might lead to storage items in
		/// `Balances` and the underlying account in `System` to be out of sync.
		FailedToMutateAccount,
	}

	#[pallet::error]
	pub enum Error<T, I = ()> {
		/// Vesting balance too high to send value.
		VestingBalance,
		/// Account liquidity restrictions prevent withdrawal.
		LiquidityRestrictions,
		/// Balance too low to send value.
		InsufficientBalance,
		/// Value too low to create account due to existential deposit.
		ExistentialDeposit,
		/// Transfer/payment would kill account.
		Expendability,
		/// A vesting schedule already exists for this account.
		ExistingVestingSchedule,
		/// Beneficiary account must pre-exist.
		DeadAccount,
		/// Number of named reserves exceed `MaxReserves`.
		TooManyReserves,
		/// Number of holds exceed `VariantCountOf<T::RuntimeHoldReason>`.
		TooManyHolds,
		/// Number of freezes exceed `MaxFreezes`.
		TooManyFreezes,
		/// The issuance cannot be modified since it is already deactivated.
		IssuanceDeactivated,
		/// The delta cannot be zero.
		DeltaZero,
	}

	/// The total units issued in the system.
	#[pallet::storage]
	#[pallet::whitelist_storage]
	pub type TotalIssuance<T: Config<I>, I: 'static = ()> = StorageValue<_, T::Balance, ValueQuery>;

	/// The total units of outstanding deactivated balance in the system.
	///
	/// This is not guaranteed to be less than or equal to [`TotalIssuance`]. Total issuance may be
	/// reduced independently of this value, including while a reversible imbalance is outstanding.
	/// Active issuance is therefore derived using saturating subtraction.
	#[pallet::storage]
	#[pallet::whitelist_storage]
	pub type InactiveIssuance<T: Config<I>, I: 'static = ()> =
		StorageValue<_, T::Balance, ValueQuery>;

	/// The Balances pallet example of storing the balance of an account.
	///
	/// # Example
	///
	/// ```nocompile
	///  impl pallet_balances::Config for Runtime {
	///    type AccountStore = StorageMapShim<Self::Account<Runtime>, frame_system::Provider<Runtime>, AccountId, Self::AccountData<Balance>>
	///  }
	/// ```
	///
	/// You can also store the balance of an account in the `System` pallet.
	///
	/// # Example
	///
	/// ```nocompile
	///  impl pallet_balances::Config for Runtime {
	///   type AccountStore = System
	///  }
	/// ```
	///
	/// But this comes with tradeoffs, storing account balances in the system pallet stores
	/// `frame_system` data alongside the account data contrary to storing account balances in the
	/// `Balances` pallet, which uses a `StorageMap` to store balances data only.
	/// NOTE: This is only used in the case that this pallet is used to store balances.
	#[pallet::storage]
	pub type Account<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Blake2_128Concat, T::AccountId, AccountData<T::Balance>, ValueQuery>;

	/// Any liquidity locks on some account balances.
	/// NOTE: Should only be accessed when setting, changing and freeing a lock.
	///
	/// Use of locks is deprecated in favour of freezes. See `https://github.com/paritytech/substrate/pull/12951/`
	#[pallet::storage]
	pub type Locks<T: Config<I>, I: 'static = ()> = StorageMap<
		_,
		Blake2_128Concat,
		T::AccountId,
		WeakBoundedVec<BalanceLock<T::Balance>, T::MaxLocks>,
		ValueQuery,
	>;

	/// Named reserves on some account balances.
	///
	/// Use of reserves is deprecated in favour of holds. See `https://github.com/paritytech/substrate/pull/12951/`
	#[pallet::storage]
	pub type Reserves<T: Config<I>, I: 'static = ()> = StorageMap<
		_,
		Blake2_128Concat,
		T::AccountId,
		BoundedVec<ReserveData<T::ReserveIdentifier, T::Balance>, T::MaxReserves>,
		ValueQuery,
	>;

	/// Holds on account balances.
	#[pallet::storage]
	pub type Holds<T: Config<I>, I: 'static = ()> = StorageMap<
		_,
		Blake2_128Concat,
		T::AccountId,
		BoundedVec<
			IdAmount<T::RuntimeHoldReason, T::Balance>,
			VariantCountOf<T::RuntimeHoldReason>,
		>,
		ValueQuery,
	>;

	/// Freeze locks on account balances.
	#[pallet::storage]
	pub type Freezes<T: Config<I>, I: 'static = ()> = StorageMap<
		_,
		Blake2_128Concat,
		T::AccountId,
		BoundedVec<IdAmount<T::FreezeIdentifier, T::Balance>, T::MaxFreezes>,
		ValueQuery,
	>;

	#[pallet::genesis_config]
	pub struct GenesisConfig<T: Config<I>, I: 'static = ()> {
		pub balances: Vec<(T::AccountId, T::Balance)>,
		/// Derived development accounts(Optional):
		/// - `u32`: The number of development accounts to generate.
		/// - `T::Balance`: The initial balance assigned to each development account.
		/// - `String`: An optional derivation(hard) string template.
		/// - Must include `{}` as a placeholder for account indices.
		/// - Defaults to `"//Sender//{}`" if `None`.
		pub dev_accounts: Option<(u32, T::Balance, Option<String>)>,
	}

	impl<T: Config<I>, I: 'static> Default for GenesisConfig<T, I> {
		fn default() -> Self {
			Self { balances: Default::default(), dev_accounts: None }
		}
	}

	#[pallet::genesis_build]
	impl<T: Config<I>, I: 'static> BuildGenesisConfig for GenesisConfig<T, I> {
		fn build(&self) {
			let total = self.balances.iter().fold(Zero::zero(), |acc: T::Balance, &(_, n)| acc + n);

			<TotalIssuance<T, I>>::put(total);

			for (_, balance) in &self.balances {
				assert!(
					*balance >= <T as Config<I>>::ExistentialDeposit::get(),
					"the balance of any account should always be at least the existential deposit.",
				)
			}

			// ensure no duplicates exist.
			let endowed_accounts = self
				.balances
				.iter()
				.map(|(x, _)| x)
				.cloned()
				.collect::<alloc::collections::btree_set::BTreeSet<_>>();

			assert!(
				endowed_accounts.len() == self.balances.len(),
				"duplicate balances in genesis."
			);

			// Generate additional dev accounts.
			if let Some((num_accounts, balance, ref derivation)) = self.dev_accounts {
				// Using the provided derivation string or default to `"//Sender//{}`".
				Pallet::<T, I>::derive_dev_account(
					num_accounts,
					balance,
					derivation.as_deref().unwrap_or(DEFAULT_ADDRESS_URI),
				);
			}
			for &(ref who, free) in self.balances.iter() {
				frame_system::Pallet::<T>::inc_providers(who);
				assert!(T::AccountStore::insert(who, AccountData { free, ..Default::default() })
					.is_ok());
			}
		}
	}

	#[pallet::hooks]
	impl<T: Config<I>, I: 'static> Hooks<BlockNumberFor<T>> for Pallet<T, I> {
		fn integrity_test() {
			#[cfg(not(feature = "insecure_zero_ed"))]
			assert!(
				!<T as Config<I>>::ExistentialDeposit::get().is_zero(),
				"The existential deposit must be greater than zero!"
			);

			assert!(
				T::MaxFreezes::get() >= <T::RuntimeFreezeReason as VariantCount>::VARIANT_COUNT,
				"MaxFreezes should be greater than or equal to the number of freeze reasons: {} < {}",
				T::MaxFreezes::get(), <T::RuntimeFreezeReason as VariantCount>::VARIANT_COUNT,
			);
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(n: BlockNumberFor<T>) -> Result<(), sp_runtime::TryRuntimeError> {
			Self::do_try_state(n)
		}
	}

	#[pallet::call(weight(<T as Config<I>>::WeightInfo))]
	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Transfer some liquid free balance to another account.
		///
		/// `transfer_allow_death` will set the `FreeBalance` of the sender and receiver.
		/// If the sender's account is below the existential deposit as a result
		/// of the transfer, the account will be reaped.
		///
		/// The dispatch origin for this call must be `Signed` by the transactor.
		#[pallet::call_index(0)]
		pub fn transfer_allow_death(
			origin: OriginFor<T>,
			dest: AccountIdLookupOf<T>,
			#[pallet::compact] value: T::Balance,
		) -> DispatchResult {
			let source = ensure_signed(origin)?;
			let dest = T::Lookup::lookup(dest)?;
			<Self as fungible::Mutate<_>>::transfer(&source, &dest, value, Expendable)?;
			Ok(())
		}

		/// Exactly as `transfer_allow_death`, except the origin must be root and the source account
		/// may be specified.
		#[pallet::call_index(2)]
		pub fn force_transfer(
			origin: OriginFor<T>,
			source: AccountIdLookupOf<T>,
			dest: AccountIdLookupOf<T>,
			#[pallet::compact] value: T::Balance,
		) -> DispatchResult {
			ensure_root(origin)?;
			let source = T::Lookup::lookup(source)?;
			let dest = T::Lookup::lookup(dest)?;
			<Self as fungible::Mutate<_>>::transfer(&source, &dest, value, Expendable)?;
			Ok(())
		}

		/// Same as the [`transfer_allow_death`] call, but with a check that the transfer will not
		/// kill the origin account.
		///
		/// 99% of the time you want [`transfer_allow_death`] instead.
		///
		/// [`transfer_allow_death`]: struct.Pallet.html#method.transfer
		#[pallet::call_index(3)]
		pub fn transfer_keep_alive(
			origin: OriginFor<T>,
			dest: AccountIdLookupOf<T>,
			#[pallet::compact] value: T::Balance,
		) -> DispatchResult {
			let source = ensure_signed(origin)?;
			let dest = T::Lookup::lookup(dest)?;
			<Self as fungible::Mutate<_>>::transfer(&source, &dest, value, Preserve)?;
			Ok(())
		}

		/// Transfer the entire transferable balance from the caller account.
		///
		/// NOTE: This function only attempts to transfer _transferable_ balances. This means that
		/// any locked, reserved, or existential deposits (when `keep_alive` is `true`), will not be
		/// transferred by this function. To ensure that this function results in a killed account,
		/// you might need to prepare the account by removing any reference counters, storage
		/// deposits, etc...
		///
		/// The dispatch origin of this call must be Signed.
		///
		/// - `dest`: The recipient of the transfer.
		/// - `keep_alive`: A boolean to determine if the `transfer_all` operation should send all
		///   of the funds the account has, causing the sender account to be killed (false), or
		///   transfer everything except at least the existential deposit, which will guarantee to
		///   keep the sender account alive (true).
		#[pallet::call_index(4)]
		pub fn transfer_all(
			origin: OriginFor<T>,
			dest: AccountIdLookupOf<T>,
			keep_alive: bool,
		) -> DispatchResult {
			let transactor = ensure_signed(origin)?;
			let keep_alive = if keep_alive { Preserve } else { Expendable };
			let reducible_balance = <Self as fungible::Inspect<_>>::reducible_balance(
				&transactor,
				keep_alive,
				Fortitude::Polite,
			);
			let dest = T::Lookup::lookup(dest)?;
			<Self as fungible::Mutate<_>>::transfer(
				&transactor,
				&dest,
				reducible_balance,
				keep_alive,
			)?;
			Ok(())
		}

		/// Unreserve some balance from a user by force.
		///
		/// Can only be called by ROOT.
		#[pallet::call_index(5)]
		pub fn force_unreserve(
			origin: OriginFor<T>,
			who: AccountIdLookupOf<T>,
			amount: T::Balance,
		) -> DispatchResult {
			ensure_root(origin)?;
			let who = T::Lookup::lookup(who)?;
			let _leftover = <Self as ReservableCurrency<_>>::unreserve(&who, amount);
			Ok(())
		}

		/// Upgrade a specified account.
		///
		/// - `origin`: Must be `Signed`.
		/// - `who`: The account to be upgraded.
		///
		/// This will waive the transaction fee if at least all but 10% of the accounts needed to
		/// be upgraded. (We let some not have to be upgraded just in order to allow for the
		/// possibility of churn).
		#[pallet::call_index(6)]
		#[pallet::weight(T::WeightInfo::upgrade_accounts(who.len() as u32))]
		pub fn upgrade_accounts(
			origin: OriginFor<T>,
			who: Vec<T::AccountId>,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;
			if who.is_empty() {
				return Ok(Pays::Yes.into());
			}
			let mut upgrade_count = 0;
			for i in &who {
				let upgraded = Self::ensure_upgraded(i);
				if upgraded {
					upgrade_count.saturating_inc();
				}
			}
			let proportion_upgraded = Perbill::from_rational(upgrade_count, who.len() as u32);
			if proportion_upgraded >= Perbill::from_percent(90) {
				Ok(Pays::No.into())
			} else {
				Ok(Pays::Yes.into())
			}
		}

		/// Set the regular balance of a given account.
		///
		/// The dispatch origin for this call is `root`.
		#[pallet::call_index(8)]
		#[pallet::weight(
			T::WeightInfo::force_set_balance_creating() // Creates a new account.
				.max(T::WeightInfo::force_set_balance_killing()) // Kills an existing account.
		)]
		pub fn force_set_balance(
			origin: OriginFor<T>,
			who: AccountIdLookupOf<T>,
			#[pallet::compact] new_free: T::Balance,
		) -> DispatchResult {
			ensure_root(origin)?;
			let who = T::Lookup::lookup(who)?;
			let existential_deposit = Self::ed();

			let wipeout = new_free < existential_deposit;
			let new_free = if wipeout { Zero::zero() } else { new_free };

			// First we try to modify the account's balance to the forced balance.
			let old_free = Self::mutate_account_handling_dust(&who, false, |account| {
				let old_free = account.free;
				account.free = new_free;
				old_free
			})?;

			// This will adjust the total issuance, which was not done by the `mutate_account`
			// above.
			if new_free > old_free {
				mem::drop(PositiveImbalance::<T, I>::new(new_free - old_free));
			} else if new_free < old_free {
				mem::drop(NegativeImbalance::<T, I>::new(old_free - new_free));
			}

			Self::deposit_event(Event::BalanceSet { who, free: new_free });
			Ok(())
		}

		/// Adjust the total issuance in a saturating way.
		///
		/// Can only be called by root and always needs a positive `delta`.
		///
		/// # Example
		#[doc = docify::embed!("./src/tests/dispatchable_tests.rs", force_adjust_total_issuance_example)]
		#[pallet::call_index(9)]
		#[pallet::weight(T::WeightInfo::force_adjust_total_issuance())]
		pub fn force_adjust_total_issuance(
			origin: OriginFor<T>,
			direction: AdjustmentDirection,
			#[pallet::compact] delta: T::Balance,
		) -> DispatchResult {
			ensure_root(origin)?;

			ensure!(delta > Zero::zero(), Error::<T, I>::DeltaZero);

			let old = TotalIssuance::<T, I>::get();
			let new = match direction {
				AdjustmentDirection::Increase => old.saturating_add(delta),
				AdjustmentDirection::Decrease => old.saturating_sub(delta),
			};

			ensure!(InactiveIssuance::<T, I>::get() <= new, Error::<T, I>::IssuanceDeactivated);
			TotalIssuance::<T, I>::set(new);

			Self::deposit_event(Event::<T, I>::TotalIssuanceForced { old, new });

			Ok(())
		}

		/// Burn the specified liquid free balance from the origin account.
		///
		/// If the origin's account ends up below the existential deposit as a result
		/// of the burn and `keep_alive` is false, the account will be reaped.
		///
		/// Unlike sending funds to a _burn_ address, which merely makes the funds inaccessible,
		/// this `burn` operation will reduce total issuance by the amount _burned_.
		#[pallet::call_index(10)]
		#[pallet::weight(if *keep_alive {T::WeightInfo::burn_keep_alive()} else {T::WeightInfo::burn_allow_death()})]
		pub fn burn(
			origin: OriginFor<T>,
			#[pallet::compact] value: T::Balance,
			keep_alive: bool,
		) -> DispatchResult {
			let source = ensure_signed(origin)?;
			let preservation = if keep_alive { Preserve } else { Expendable };
			<Self as fungible::Mutate<_>>::burn_from(
				&source,
				value,
				preservation,
				Precision::Exact,
				Polite,
			)?;
			Ok(())
		}
	}

	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Public function to get the total issuance.
		pub fn total_issuance() -> T::Balance {
			TotalIssuance::<T, I>::get()
		}

		/// Public function to get the inactive issuance.
		pub fn inactive_issuance() -> T::Balance {
			InactiveIssuance::<T, I>::get()
		}

		/// Public function to access the Locks storage.
		pub fn locks(who: &T::AccountId) -> WeakBoundedVec<BalanceLock<T::Balance>, T::MaxLocks> {
			Locks::<T, I>::get(who)
		}

		/// Public function to access the reserves storage.
		pub fn reserves(
			who: &T::AccountId,
		) -> BoundedVec<ReserveData<T::ReserveIdentifier, T::Balance>, T::MaxReserves> {
			Reserves::<T, I>::get(who)
		}

		fn ed() -> T::Balance {
			T::ExistentialDeposit::get()
		}
		/// Ensure the account `who` is using the new logic.
		///
		/// Returns `true` if the account did get upgraded, `false` if it didn't need upgrading.
		pub fn ensure_upgraded(who: &T::AccountId) -> bool {
			let mut a = T::AccountStore::get(who);
			if a.flags.is_new_logic() {
				return false;
			}
			a.flags.set_new_logic();
			if !a.reserved.is_zero() && a.frozen.is_zero() {
				if system::Pallet::<T>::providers(who) == 0 {
					// Gah!! We have no provider refs :(
					// This shouldn't practically happen, but we need a failsafe anyway: let's give
					// them enough for an ED.
					log::warn!(
						target: LOG_TARGET,
						"account with a non-zero reserve balance has no provider refs, account_id: '{:?}'.",
						who
					);
					a.free = a.free.max(Self::ed());
					system::Pallet::<T>::inc_providers(who);
				}
				let _ = system::Pallet::<T>::inc_consumers_without_limit(who).defensive();
			}
			// Should never fail - we're only setting a bit.
			let _ = T::AccountStore::try_mutate_exists(who, |account| -> DispatchResult {
				*account = Some(a);
				Ok(())
			});
			Self::deposit_event(Event::Upgraded { who: who.clone() });
			return true;
		}

		/// Get the free balance of an account.
		pub fn free_balance(who: impl core::borrow::Borrow<T::AccountId>) -> T::Balance {
			Self::account(who.borrow()).free
		}

		/// Get the balance of an account that can be used for transfers, reservations, or any other
		/// non-locking, non-transaction-fee activity. Will be at most `free_balance`.
		pub fn usable_balance(who: impl core::borrow::Borrow<T::AccountId>) -> T::Balance {
			<Self as fungible::Inspect<_>>::reducible_balance(who.borrow(), Expendable, Polite)
		}

		/// Get the balance of an account that can be used for paying transaction fees (not tipping,
		/// or any other kind of fees, though). Will be at most `free_balance`.
		///
		/// This requires that the account stays alive.
		pub fn usable_balance_for_fees(who: impl core::borrow::Borrow<T::AccountId>) -> T::Balance {
			<Self as fungible::Inspect<_>>::reducible_balance(who.borrow(), Protect, Polite)
		}

		/// Get the reserved balance of an account.
		pub fn reserved_balance(who: impl core::borrow::Borrow<T::AccountId>) -> T::Balance {
			Self::account(who.borrow()).reserved
		}

		/// Get both the free and reserved balances of an account.
		pub(crate) fn account(who: &T::AccountId) -> AccountData<T::Balance> {
			T::AccountStore::get(who)
		}

		/// Mutate an account to some new value, or delete it entirely with `None`. Will enforce
		/// `ExistentialDeposit` law, annulling the account as needed.
		///
		/// It returns the result from the closure. Any dust is handled through the low-level
		/// `fungible::Unbalanced` trap-door for legacy dust management.
		///
		/// NOTE: Doesn't do any preparatory work for creating a new account, so should only be used
		/// when it is known that the account already exists.
		///
		/// NOTE: LOW-LEVEL: This will not attempt to maintain total issuance. It is expected that
		/// the caller will do this.
		pub(crate) fn mutate_account_handling_dust<R>(
			who: &T::AccountId,
			force_consumer_bump: bool,
			f: impl FnOnce(&mut AccountData<T::Balance>) -> R,
		) -> Result<R, DispatchError> {
			let (r, maybe_dust) = Self::mutate_account(who, force_consumer_bump, f)?;
			if let Some(dust) = maybe_dust {
				<Self as fungible::Unbalanced<_>>::handle_raw_dust(dust);
			}
			Ok(r)
		}

		/// Mutate an account to some new value, or delete it entirely with `None`. Will enforce
		/// `ExistentialDeposit` law, annulling the account as needed.
		///
		/// It returns the result from the closure. Any dust is handled through the low-level
		/// `fungible::Unbalanced` trap-door for legacy dust management.
		///
		/// NOTE: Doesn't do any preparatory work for creating a new account, so should only be used
		/// when it is known that the account already exists.
		///
		/// NOTE: LOW-LEVEL: This will not attempt to maintain total issuance. It is expected that
		/// the caller will do this.
		pub(crate) fn try_mutate_account_handling_dust<R, E: From<DispatchError>>(
			who: &T::AccountId,
			force_consumer_bump: bool,
			f: impl FnOnce(&mut AccountData<T::Balance>, bool) -> Result<R, E>,
		) -> Result<R, E> {
			let (r, maybe_dust) = Self::try_mutate_account(who, force_consumer_bump, f)?;
			if let Some(dust) = maybe_dust {
				<Self as fungible::Unbalanced<_>>::handle_raw_dust(dust);
			}
			Ok(r)
		}

		/// Mutate an account to some new value, or delete it entirely with `None`. Will enforce
		/// `ExistentialDeposit` law, annulling the account as needed.
		///
		/// It returns both the result from the closure, and an optional amount of dust
		/// which should be handled once it is known that all nested mutates that could affect
		/// storage items what the dust handler touches have completed.
		///
		/// NOTE: Doesn't do any preparatory work for creating a new account, so should only be used
		/// when it is known that the account already exists.
		///
		/// NOTE: LOW-LEVEL: This will not attempt to maintain total issuance. It is expected that
		/// the caller will do this.
		///
		/// NOTE: LOW-LEVEL: `force_consumer_bump` is mainly there to accomodate for locks, which
		/// have no ability in their API to return an error, and therefore better force increment
		/// the consumer, or else the system will be inconsistent. See `consumer_limits_tests`.
		pub(crate) fn mutate_account<R>(
			who: &T::AccountId,
			force_consumer_bump: bool,
			f: impl FnOnce(&mut AccountData<T::Balance>) -> R,
		) -> Result<(R, Option<T::Balance>), DispatchError> {
			Self::try_mutate_account(who, force_consumer_bump, |a, _| -> Result<R, DispatchError> {
				Ok(f(a))
			})
		}

		/// Returns `true` when `who` has some providers or `insecure_zero_ed` feature is disabled.
		/// Returns `false` otherwise.
		#[cfg(not(feature = "insecure_zero_ed"))]
		fn have_providers_or_no_zero_ed(_: &T::AccountId) -> bool {
			true
		}

		/// Returns `true` when `who` has some providers or `insecure_zero_ed` feature is disabled.
		/// Returns `false` otherwise.
		#[cfg(feature = "insecure_zero_ed")]
		fn have_providers_or_no_zero_ed(who: &T::AccountId) -> bool {
			frame_system::Pallet::<T>::providers(who) > 0
		}

		/// Mutate an account to some new value, or delete it entirely with `None`. Will enforce
		/// `ExistentialDeposit` law, annulling the account as needed. This will do nothing if the
		/// result of `f` is an `Err`.
		///
		/// It returns both the result from the closure, and an optional amount of dust
		/// which should be handled once it is known that all nested mutates that could affect
		/// storage items what the dust handler touches have completed.
		///
		/// NOTE: Doesn't do any preparatory work for creating a new account, so should only be used
		/// when it is known that the account already exists.
		///
		/// NOTE: LOW-LEVEL: This will not attempt to maintain total issuance. It is expected that
		/// the caller will do this.
		pub(crate) fn try_mutate_account<R, E: From<DispatchError>>(
			who: &T::AccountId,
			force_consumer_bump: bool,
			f: impl FnOnce(&mut AccountData<T::Balance>, bool) -> Result<R, E>,
		) -> Result<(R, Option<T::Balance>), E> {
			Self::ensure_upgraded(who);
			let result = T::AccountStore::try_mutate_exists(who, |maybe_account| {
				let is_new = maybe_account.is_none();
				let mut account = maybe_account.take().unwrap_or_default();
				let did_provide =
					account.free >= Self::ed() && Self::have_providers_or_no_zero_ed(who);
				let did_consume =
					!is_new && (!account.reserved.is_zero() || !account.frozen.is_zero());

				let result = f(&mut account, is_new)?;

				let does_provide = account.free >= Self::ed();
				let does_consume = !account.reserved.is_zero() || !account.frozen.is_zero();

				if !did_provide && does_provide {
					frame_system::Pallet::<T>::inc_providers(who);
				}
				if did_consume && !does_consume {
					frame_system::Pallet::<T>::dec_consumers(who);
				}
				if !did_consume && does_consume {
					if force_consumer_bump {
						// If we are forcing a consumer bump, we do it without limit.
						frame_system::Pallet::<T>::inc_consumers_without_limit(who)?;
					} else {
						frame_system::Pallet::<T>::inc_consumers(who)?;
					}
				}
				if does_consume && frame_system::Pallet::<T>::consumers(who) == 0 {
					// NOTE: This is a failsafe and should not happen for normal accounts. A normal
					// account should have gotten a consumer ref in `!did_consume && does_consume`
					// at some point.
					log::error!(target: LOG_TARGET, "Defensively bumping a consumer ref.");
					frame_system::Pallet::<T>::inc_consumers(who)?;
				}
				if did_provide && !does_provide {
					// This could reap the account so must go last.
					frame_system::Pallet::<T>::dec_providers(who).inspect_err(|_| {
						// best-effort revert consumer change.
						if did_consume && !does_consume {
							let _ = frame_system::Pallet::<T>::inc_consumers(who).defensive();
						}
						if !did_consume && does_consume {
							let _ = frame_system::Pallet::<T>::dec_consumers(who);
						}
					})?;
				}

				let maybe_endowed = if is_new { Some(account.free) } else { None };

				// Handle any steps needed after mutating an account.
				//
				// This includes DustRemoval unbalancing, in the case than the `new` account's total
				// balance is non-zero but below ED.
				//
				// Updates `maybe_account` to `Some` iff the account has sufficient balance.
				// Evaluates `maybe_dust`, which is `Some` containing the dust to be dropped, iff
				// some dust should be dropped.
				//
				// We should never be dropping if there are either reserves or freezes since that
				// implies a consumer ref.
				let ed = Self::ed();
				let maybe_dust = if account.free < ed && !does_consume {
					if account.free.is_zero() {
						None
					} else {
						Some(account.free)
					}
				} else {
					*maybe_account = Some(account);
					None
				};
				Ok((maybe_endowed, maybe_dust, result))
			});
			result.map(|(maybe_endowed, maybe_dust, result)| {
				if let Some(endowed) = maybe_endowed {
					Self::deposit_event(Event::Endowed {
						account: who.clone(),
						free_balance: endowed,
					});
				}
				if let Some(amount) = maybe_dust {
					Pallet::<T, I>::deposit_event(Event::DustLost { account: who.clone(), amount });
				}
				(result, maybe_dust)
			})
		}

		/// Update the account entry for `who`, given the locks.
		pub(crate) fn update_locks(who: &T::AccountId, locks: &[BalanceLock<T::Balance>]) {
			let bounded_locks = WeakBoundedVec::<_, T::MaxLocks>::force_from(
				locks.to_vec(),
				Some("Balances Update Locks"),
			);

			if locks.len() as u32 > T::MaxLocks::get() {
				log::warn!(
					target: LOG_TARGET,
					"Warning: A user has more currency locks than expected. \
					A runtime configuration adjustment may be needed."
				);
			}
			let freezes = Freezes::<T, I>::get(who);
			let mut prev_frozen = Zero::zero();
			let mut after_frozen = Zero::zero();
			// The consumer limit might be full, therefore we pass `true` into `mutate_account` to
			// make sure this cannot fail.
			let res = Self::mutate_account(who, true, |b| {
				prev_frozen = b.frozen;
				b.frozen = Zero::zero();
				for l in locks.iter() {
					b.frozen = b.frozen.max(l.amount);
				}
				for l in freezes.iter() {
					b.frozen = b.frozen.max(l.amount);
				}
				after_frozen = b.frozen;
			});
			let maybe_dust = match res {
				Ok((_, maybe_dust)) => maybe_dust,
				Err(_) => {
					Self::deposit_event(Event::Unexpected(UnexpectedKind::FailedToMutateAccount));
					defensive!("errored in mutate_account");
					None
				},
			};

			match locks.is_empty() {
				true => Locks::<T, I>::remove(who),
				false => Locks::<T, I>::insert(who, bounded_locks),
			}

			if let Some(dust) = maybe_dust {
				<Self as fungible::Unbalanced<_>>::handle_raw_dust(dust);
			}

			if prev_frozen > after_frozen {
				let amount = prev_frozen.saturating_sub(after_frozen);
				Self::deposit_event(Event::Unlocked { who: who.clone(), amount });
			} else if after_frozen > prev_frozen {
				let amount = after_frozen.saturating_sub(prev_frozen);
				Self::deposit_event(Event::Locked { who: who.clone(), amount });
			}
		}

		/// Update the account entry for `who`, given the locks.
		pub(crate) fn update_freezes(
			who: &T::AccountId,
			freezes: BoundedSlice<IdAmount<T::FreezeIdentifier, T::Balance>, T::MaxFreezes>,
		) -> DispatchResult {
			let mut prev_frozen = Zero::zero();
			let mut after_frozen = Zero::zero();
			let (_, maybe_dust) = Self::mutate_account(who, false, |b| {
				prev_frozen = b.frozen;
				b.frozen = Zero::zero();
				for l in Locks::<T, I>::get(who).iter() {
					b.frozen = b.frozen.max(l.amount);
				}
				for l in freezes.iter() {
					b.frozen = b.frozen.max(l.amount);
				}
				after_frozen = b.frozen;
			})?;

			if freezes.is_empty() {
				Freezes::<T, I>::remove(who);
			} else {
				Freezes::<T, I>::insert(who, freezes);
			}

			if let Some(dust) = maybe_dust {
				<Self as fungible::Unbalanced<_>>::handle_raw_dust(dust);
			}

			if prev_frozen > after_frozen {
				let amount = prev_frozen.saturating_sub(after_frozen);
				Self::deposit_event(Event::Thawed { who: who.clone(), amount });
			} else if after_frozen > prev_frozen {
				let amount = after_frozen.saturating_sub(prev_frozen);
				Self::deposit_event(Event::Frozen { who: who.clone(), amount });
			}
			Ok(())
		}

		/// Move the reserved balance of one account into the balance of another, according to
		/// `status`. This will respect freezes/locks only if `fortitude` is `Polite`.
		///
		/// Is a no-op if the value to be moved is zero.
		///
		/// NOTE: returns actual amount of transferred value in `Ok` case.
		pub(crate) fn do_transfer_reserved(
			slashed: &T::AccountId,
			beneficiary: &T::AccountId,
			value: T::Balance,
			precision: Precision,
			fortitude: Fortitude,
			status: Status,
		) -> Result<T::Balance, DispatchError> {
			if value.is_zero() {
				return Ok(Zero::zero());
			}

			let max = <Self as fungible::InspectHold<_>>::reducible_total_balance_on_hold(
				slashed, fortitude,
			);
			let actual = match precision {
				Precision::BestEffort => value.min(max),
				Precision::Exact => value,
			};
			ensure!(actual <= max, TokenError::FundsUnavailable);
			if slashed == beneficiary {
				return match status {
					Status::Free => Ok(actual.saturating_sub(Self::unreserve(slashed, actual))),
					Status::Reserved => Ok(actual),
				};
			}

			let ((_, maybe_dust_1), maybe_dust_2) = Self::try_mutate_account(
				beneficiary,
				false,
				|to_account, is_new| -> Result<((), Option<T::Balance>), DispatchError> {
					ensure!(!is_new, Error::<T, I>::DeadAccount);
					Self::try_mutate_account(slashed, false, |from_account, _| -> DispatchResult {
						match status {
							Status::Free => {
								to_account.free = to_account
									.free
									.checked_add(&actual)
									.ok_or(ArithmeticError::Overflow)?
							},
							Status::Reserved => {
								to_account.reserved = to_account
									.reserved
									.checked_add(&actual)
									.ok_or(ArithmeticError::Overflow)?
							},
						}
						from_account.reserved.saturating_reduce(actual);
						Ok(())
					})
				},
			)?;

			if let Some(dust) = maybe_dust_1 {
				<Self as fungible::Unbalanced<_>>::handle_raw_dust(dust);
			}
			if let Some(dust) = maybe_dust_2 {
				<Self as fungible::Unbalanced<_>>::handle_raw_dust(dust);
			}

			Self::deposit_event(Event::ReserveRepatriated {
				from: slashed.clone(),
				to: beneficiary.clone(),
				amount: actual,
				destination_status: status,
			});
			Ok(actual)
		}

		/// Generate dev account from derivation(hard) string.
		pub fn derive_dev_account(num_accounts: u32, balance: T::Balance, derivation: &str) {
			// Ensure that the number of accounts is not zero.
			assert!(num_accounts > 0, "num_accounts must be greater than zero");

			assert!(
				balance >= <T as Config<I>>::ExistentialDeposit::get(),
				"the balance of any account should always be at least the existential deposit.",
			);

			assert!(
				derivation.contains("{}"),
				"Invalid derivation, expected `{{}}` as part of the derivation"
			);

			for index in 0..num_accounts {
				// Replace "{}" in the derivation string with the index.
				let derivation_string = derivation.replace("{}", &index.to_string());

				// Generate the key pair from the derivation string using sr25519.
				let pair: SrPair = Pair::from_string(&derivation_string, None)
					.expect(&format!("Failed to parse derivation string: {derivation_string}"));

				// Convert the public key to AccountId.
				let who = T::AccountId::decode(&mut &pair.public().encode()[..])
					.expect(&format!("Failed to decode public key from pair: {:?}", pair.public()));

				// Set the balance for the generated account.
				Self::mutate_account_handling_dust(&who, false, |account| {
					account.free = balance;
				})
				.expect(&format!("Failed to add account to keystore: {:?}", who));
			}
		}
	}

	#[cfg(any(test, feature = "try-runtime"))]
	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		pub(crate) fn do_try_state(
			_n: BlockNumberFor<T>,
		) -> Result<(), sp_runtime::TryRuntimeError> {
			Self::hold_and_freeze_count()?;
			Self::account_frozen_greater_than_locks()?;
			Self::account_frozen_greater_than_freezes()?;
			Ok(())
		}

		fn hold_and_freeze_count() -> Result<(), sp_runtime::TryRuntimeError> {
			Holds::<T, I>::iter_keys().try_for_each(|k| {
				if Holds::<T, I>::decode_len(k).unwrap_or(0) >
					T::RuntimeHoldReason::VARIANT_COUNT as usize
				{
					Err("Found `Hold` with too many elements")
				} else {
					Ok(())
				}
			})?;

			Freezes::<T, I>::iter_keys().try_for_each(|k| {
				if Freezes::<T, I>::decode_len(k).unwrap_or(0) > T::MaxFreezes::get() as usize {
					Err("Found `Freeze` with too many elements")
				} else {
					Ok(())
				}
			})?;

			Ok(())
		}

		fn account_frozen_greater_than_locks() -> Result<(), sp_runtime::TryRuntimeError> {
			Locks::<T, I>::iter().try_for_each(|(who, locks)| {
				let max_locks = locks.iter().map(|l| l.amount).max().unwrap_or_default();
				let frozen = T::AccountStore::get(&who).frozen;
				if max_locks > frozen {
					log::warn!(
						target: crate::LOG_TARGET,
						"Maximum lock of {:?} ({:?}) is greater than the frozen balance {:?}",
						who,
						max_locks,
						frozen
					);
					Err("bad locks".into())
				} else {
					Ok(())
				}
			})
		}

		fn account_frozen_greater_than_freezes() -> Result<(), sp_runtime::TryRuntimeError> {
			Freezes::<T, I>::iter().try_for_each(|(who, freezes)| {
				let max_locks = freezes.iter().map(|l| l.amount).max().unwrap_or_default();
				let frozen = T::AccountStore::get(&who).frozen;
				if max_locks > frozen {
					log::warn!(
						target: crate::LOG_TARGET,
						"Maximum freeze of {:?} ({:?}) is greater than the frozen balance {:?}",
						who,
						max_locks,
						frozen
					);
					Err("bad freezes".into())
				} else {
					Ok(())
				}
			})
		}
	}
}
