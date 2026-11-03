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

//! # Accumulate-and-Forward Pallet
//!
//! Intercepts configurable token inflows (transaction fees, dust removal, coretime revenue) on
//! system parachains and gathers them in a local accumulation account for periodic forwarding
//! to a configurable destination.
//!
//! ## Usage
//!
//! - **Fees**: Use [`DealWithFeesSplit`] to split fees between accumulation and other handlers
//! - **Burns/Revenue**: Use the pallet as `OnUnbalanced<CreditOf>` handler (e.g., dust removal,
//!   coretime revenue)
//! Note: Direct calls to `pallet_balances::Pallet::burn()` extrinsic are not redirected to
//! the accumulation account — they still reduce total issuance directly.
//!
//! ## Setup
//!
//! The accumulation account must be pre-funded with at least the existential deposit.
//! For new chains, include the account in the balances genesis config.
//! For existing chains, fund it via a manual transfer.
//!
//! If the accumulation account is not pre-funded, deposits below ED will be silently burned.
//!
//! ## Forwarding
//!
//! `on_idle` forwards once `TransferPeriod` blocks of the configured `BlockNumberProvider` have
//! elapsed since the last attempt and the account holds at least `MinTransferAmount` above its
//! existential deposit. A chain may not observe every block of the provider, so the period is
//! measured from the recorded last attempt.
//!
//! ## Total Issuance
//!
//! Accumulated funds are burnt upon forwarding (reducing `total_issuance` here) and the same
//! funds are minted at the destination when the sent message is received.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(test)]
pub(crate) mod mock;
#[cfg(test)]
mod tests;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

pub mod weights;
pub use weights::WeightInfo;

use frame_support::{
	pallet_prelude::*,
	sp_runtime::traits::Zero,
	traits::{
		fungible::{Balanced, Credit, Inspect, Unbalanced},
		tokens::{Fortitude, Preservation},
		Currency, Imbalance, OnUnbalanced,
	},
	weights::WeightMeter,
	PalletId,
};
use sp_runtime::{traits::BlockNumberProvider, Percent, Saturating};

pub use pallet::*;

/// Trait for forwarding accumulated funds to a configured destination.
///
/// Implementations carry all message-construction and dispatch logic, keeping this pallet
/// free of transport-specific dependencies.
pub trait Forwarder<AccountId, Balance> {
	/// Forward `amount` from `source` to the configured destination.
	fn forward(source: AccountId, amount: Balance) -> Result<(), ()>;
}

const LOG_TARGET: &str = "runtime::accumulate-forward";

/// Type alias for balance.
pub type BalanceOf<T> =
	<<T as Config>::Currency as Inspect<<T as frame_system::Config>::AccountId>>::Balance;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::sp_runtime::traits::AccountIdConversion;
	use frame_system::pallet_prelude::BlockNumberFor as SystemBlockNumberFor;

	/// The in-code storage version.
	const STORAGE_VERSION: frame_support::traits::StorageVersion =
		frame_support::traits::StorageVersion::new(1);

	/// Block number type derived from the configured [`Config::BlockNumberProvider`].
	pub type BlockNumberFor<T> =
		<<T as Config>::BlockNumberProvider as BlockNumberProvider>::BlockNumber;

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The currency type.
		type Currency: Inspect<Self::AccountId>
			+ Unbalanced<Self::AccountId>
			+ Balanced<Self::AccountId>;

		/// The pallet ID used to derive the accumulation account.
		type PalletId: Get<PalletId>;

		/// The implementation responsible for forwarding accumulated funds to the destination.
		/// Message construction and dispatch logic lives here, keeping this pallet free of
		/// message-related dependencies.
		type Forwarder: super::Forwarder<Self::AccountId, BalanceOf<Self>>;

		/// Minimum number of blocks, as counted by [`Config::BlockNumberProvider`], between
		/// successive forwards. Acts as a rate limiter to avoid sending too many messages.
		#[pallet::constant]
		type TransferPeriod: Get<BlockNumberFor<Self>>;

		/// Minimum transferable balance required to trigger a forward.
		/// This avoids forwarding very small / negligible amounts.
		/// The accumulation account always retains its existential deposit on top of this.
		#[pallet::constant]
		type MinTransferAmount: Get<BalanceOf<Self>>;

		/// Block number provider. Use `RelaychainDataProvider` on parachains so that
		/// `TransferPeriod` is expressed in relay chain blocks, keeping the cadence stable. It
		/// may not be observed at every block.
		type BlockNumberProvider: BlockNumberProvider;

		/// Weight information for the pallet's operations.
		type WeightInfo: weights::WeightInfo;
	}

	/// Block of [`Config::BlockNumberProvider`] at which a forward was last attempted.
	///
	/// `None` means none was attempted yet, so the next one is not rate limited.
	#[pallet::storage]
	pub type LastForwardBlock<T: Config> = StorageValue<_, BlockNumberFor<T>, OptionQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Successfully forwarded accumulated funds to the destination.
		ForwardSucceeded { amount: BalanceOf<T> },
		/// Failed to forward funds. They will remain in the accumulation account
		/// and forwarding will be retried after another `TransferPeriod` blocks.
		ForwardFailed { amount: BalanceOf<T> },
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<SystemBlockNumberFor<T>> for Pallet<T> {
		fn on_idle(_block: SystemBlockNumberFor<T>, remaining_weight: Weight) -> Weight {
			let mut meter = WeightMeter::with_limit(remaining_weight);

			// Need one read for `LastForwardBlock`.
			if meter.try_consume(T::DbWeight::get().reads(1)).is_err() {
				return meter.consumed();
			}

			// A chain may not observe every block of the provider, so the period is measured
			// from the recorded last attempt.
			let block = T::BlockNumberProvider::current_block_number();
			if let Some(last) = LastForwardBlock::<T>::get() {
				if block.saturating_sub(last) < T::TransferPeriod::get() {
					return meter.consumed();
				}
			}

			// Need one read for the balance check.
			if meter.try_consume(T::DbWeight::get().reads(1)).is_err() {
				return meter.consumed();
			}

			let accumulation_account = Self::accumulation_account();
			// We use `reducible_balance` with `Preservation::Preserve` to get the
			// usable balance (excluding the ED).
			let available_funds = T::Currency::reducible_balance(
				&accumulation_account,
				Preservation::Preserve,
				Fortitude::Polite,
			);

			if available_funds < T::MinTransferAmount::get() {
				return meter.consumed();
			}

			// Ensure there is budget for the send plus the write recording it.
			let send_weight =
				T::WeightInfo::send_native().saturating_add(T::DbWeight::get().writes(1));
			if meter.try_consume(send_weight).is_err() {
				return meter.consumed();
			}

			// Record before dispatching: a failing destination must be rate limited too.
			LastForwardBlock::<T>::put(block);

			// Attempt to forward accumulated funds.
			match T::Forwarder::forward(accumulation_account, available_funds) {
				Ok(()) => {
					Self::deposit_event(Event::ForwardSucceeded { amount: available_funds });
				},
				Err(()) => {
					log::debug!(
						target: LOG_TARGET,
						"accumulate-forward transfer of {:?} failed at block {:?}",
						available_funds,
						block,
					);
					Self::deposit_event(Event::ForwardFailed { amount: available_funds });
				},
			}

			meter.consumed()
		}

		fn integrity_test() {
			assert!(
				!T::TransferPeriod::get().is_zero(),
				"TransferPeriod must not be zero (would forward on every block, defeating the \
				 rate limiter)"
			);
		}
	}

	impl<T: Config> Pallet<T> {
		/// Get the accumulation account derived from the pallet ID.
		///
		/// This account accumulates funds locally before they are forwarded to the destination.
		pub fn accumulation_account() -> T::AccountId {
			T::PalletId::get().into_account_truncating()
		}
	}
}

/// Type alias for credit (negative imbalance - funds that were removed).
/// This is for the `fungible::Balanced` trait.
pub type CreditOf<T> = Credit<<T as frame_system::Config>::AccountId, <T as Config>::Currency>;

/// A configurable fee handler that splits fees between the accumulation account and another
/// destination.
///
/// - `AccumulatedPercent`: Percentage of fees to accumulate (e.g., `Percent::from_percent(0)`)
/// - `OtherHandler`: Where to send the remaining fees (e.g., `ToAuthor`, `DealWithFees`)
///
/// Tips always go 100% to `OtherHandler`.
///
/// # Example
///
/// ```ignore
/// parameter_types! {
///     pub const AccumulateForwardFeePercent: Percent = Percent::from_percent(0); // 0% accumulated
/// }
///
/// type DealWithFeesAccumulate = pallet_accumulate_and_forward::DealWithFeesSplit<
///     Runtime,
///     AccumulateForwardFeePercent,
///     DealWithFees<Runtime>, // Or ToAuthor<Runtime> for relay chain
/// >;
///
/// impl pallet_transaction_payment::Config for Runtime {
///     type OnChargeTransaction = FungibleAdapter<Balances, DealWithFeesAccumulate>;
/// }
/// ```
pub struct DealWithFeesSplit<T, AccumulatedPercent, OtherHandler>(
	core::marker::PhantomData<(T, AccumulatedPercent, OtherHandler)>,
);

impl<T, AccumulatedPercent, OtherHandler> OnUnbalanced<CreditOf<T>>
	for DealWithFeesSplit<T, AccumulatedPercent, OtherHandler>
where
	T: Config,
	AccumulatedPercent: Get<Percent>,
	OtherHandler: OnUnbalanced<CreditOf<T>>,
{
	fn on_unbalanceds(mut fees_then_tips: impl Iterator<Item = CreditOf<T>>) {
		if let Some(fees) = fees_then_tips.next() {
			let accumulated_percent = AccumulatedPercent::get();
			let other_percent = Percent::one().saturating_sub(accumulated_percent);
			let mut split = fees.ration(
				accumulated_percent.deconstruct() as u32,
				other_percent.deconstruct() as u32,
			);
			if let Some(tips) = fees_then_tips.next() {
				// Tips go 100% to other handler.
				tips.merge_into(&mut split.1);
			}
			if !accumulated_percent.is_zero() {
				<Pallet<T> as OnUnbalanced<_>>::on_unbalanced(split.0);
			}
			OtherHandler::on_unbalanced(split.1);
		}
	}
}

/// Implementation of `OnUnbalanced` for the `fungible::Balanced` trait.
///
/// Use this on system chains to collect imbalances (e.g. coretime revenue, tx fees, dust removal)
/// that would otherwise be burned, redirecting them to the accumulation account for later
/// forwarding.
///
/// For pallets still using the legacy `Currency` trait (e.g. `pallet_identity`), use
/// [`LegacyAdapter`] instead.
impl<T: Config> OnUnbalanced<CreditOf<T>> for Pallet<T> {
	fn on_nonzero_unbalanced(amount: CreditOf<T>) {
		let accumulation_account = Self::accumulation_account();
		let numeric_amount = amount.peek();

		// Resolve should never fail because:
		// - can_deposit on destination succeeds assuming accumulation account is pre-funded with ED
		// - amount is guaranteed non-zero by the trait method signature
		// The only failure would be overflow on destination or unfunded account.
		let _ = T::Currency::resolve(&accumulation_account, amount).inspect_err(|_| {
			frame_support::defensive!(
				"🚨 Failed to deposit to accumulation account - funds burned, it should never happen!"
			);
		});

		log::debug!(
			target: LOG_TARGET,
			"💸 Deposited {numeric_amount:?} to accumulation account"
		);
	}
}

/// Type alias for legacy `NegativeImbalance` from the `Currency` trait.
type LegacyNegativeImbalance<A, C> = <C as Currency<A>>::NegativeImbalance;

/// Adapter that redirects `NegativeImbalance` from the legacy `Currency` trait to the
/// accumulation account.
///
/// Cannot be implemented directly on `Pallet<T>` because the compiler cannot prove that
/// `<C as Currency>::NegativeImbalance` and `fungible::Credit` are always distinct types,
/// so two `OnUnbalanced` impls on the same struct are rejected.
///
/// Will be removed once all consumer pallets migrate to fungible traits.
///
/// # Example
/// ```ignore
/// type Slashed = pallet_accumulate_and_forward::LegacyAdapter<Runtime, Balances>;
/// ```
pub struct LegacyAdapter<T, C>(core::marker::PhantomData<(T, C)>);

impl<T: Config, C> OnUnbalanced<LegacyNegativeImbalance<T::AccountId, C>> for LegacyAdapter<T, C>
where
	C: Currency<T::AccountId>,
{
	fn on_nonzero_unbalanced(amount: LegacyNegativeImbalance<T::AccountId, C>) {
		let accumulation_account = Pallet::<T>::accumulation_account();
		let numeric_amount = amount.peek();
		// NOTE: `resolve_creating` is "infallible" because it returns `()`, but it silently burns
		// the imbalance if it is less than ED and the destination is empty. We guard against this
		// by making misconfigured runtimes clearly visible. See crate-level docs for the
		// pre-funding requirement.
		if C::total_balance(&accumulation_account).saturating_add(numeric_amount) <
			C::minimum_balance()
		{
			frame_support::defensive!(
				"🚨 LegacyAdapter: deposit to accumulation account will be silently burned — \
				 ensure the accumulation account is pre-funded with at least ED!"
			);
		}
		C::resolve_creating(&accumulation_account, amount);
		log::debug!(
			target: LOG_TARGET,
			"💸 Deposited (legacy) {numeric_amount:?} to accumulation account"
		);
	}
}
