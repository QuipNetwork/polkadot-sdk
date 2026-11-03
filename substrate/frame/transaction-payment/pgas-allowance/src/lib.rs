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

//! # Gas Allowance Pallet
//!
//! Provides the [`ChargePGAS`] transaction extension. When a signed transaction dispatching a
//! call that passes [`Config::CallFilter`] is submitted by an account holding at least the
//! required fee in the PGAS asset, the fee is withdrawn as a [`fungibles::Credit`] held in the
//! extension's `Pre`. Any unused portion is refunded from that credit in `post_dispatch`; the
//! remainder is dropped, which burns the consumed fee via `OnDropCredit`. A
//! [`Event::PGASFeePaid`] event is emitted mirroring
//! [`pallet_transaction_payment::Event::TransactionFeePaid`] so PGAS fee payments are observable.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

const LOG_TARGET: &str = "runtime::pgas-allowance";

use codec::{Decode, DecodeWithMemTracking, Encode};
use frame_support::{
	dispatch::{DispatchInfo, DispatchResult, PostDispatchInfo},
	pallet_prelude::TransactionSource,
	traits::{
		Contains, Get,
		tokens::{
			Fortitude, Precision, Preservation,
			fungibles::{self, Credit},
		},
	},
	weights::Weight,
};
use frame_system::pallet_prelude::OriginFor;
use pallet_transaction_payment::ChargeTransactionPayment;
use scale_info::{StaticTypeInfo, TypeInfo};
use sp_runtime::{
	traits::{
		AsSystemOriginSigner, DispatchInfoOf, Dispatchable, Implication, PostDispatchInfoOf,
		RefundWeight, TransactionExtension, ValidateResult, Zero,
	},
	transaction_validity::{InvalidTransaction, TransactionValidityError, ValidTransaction},
};

pub use pallet::*;
pub use weights::WeightInfo;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
pub mod weights;

type BalanceOf<T> = <<T as pallet_transaction_payment::Config>::OnChargeTransaction as
	pallet_transaction_payment::OnChargeTransaction<T>>::Balance;

type AssetIdOf<T> =
	<<T as Config>::Assets as fungibles::Inspect<<T as frame_system::Config>::AccountId>>::AssetId;

/// Trait used by runtimes to mint PGAS to the benchmark caller.
#[cfg(feature = "runtime-benchmarks")]
pub trait BenchmarkHelperTrait<AccountId, AssetId, Balance> {
	/// Mint `amount` of PGAS to `who`.
	fn mint_pgas(who: &AccountId, asset_id: AssetId, amount: Balance);
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::config]
	pub trait Config:
		frame_system::Config<RuntimeEvent: From<Event<Self>>> + pallet_transaction_payment::Config
	{
		/// Access to the PGAS asset.
		type Assets: fungibles::Balanced<Self::AccountId, Balance = BalanceOf<Self>>;

		/// The PGAS asset id.
		type PGASAssetId: frame_support::traits::Get<AssetIdOf<Self>>;

		/// Filter deciding which calls are eligible to be paid with PGAS.
		type CallFilter: Contains<<Self as frame_system::Config>::RuntimeCall>;

		/// Weight information for the extension.
		type WeightInfo: WeightInfo;

		/// Helper used by the extension benchmarks to endow the caller with enough PGAS to cover
		/// the fee.
		#[cfg(feature = "runtime-benchmarks")]
		type BenchmarkHelper: crate::BenchmarkHelperTrait<Self::AccountId, AssetIdOf<Self>, BalanceOf<Self>>;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A transaction fee `actual_fee` has been paid by `who` in PGAS and burned. Mirrors
		/// [`pallet_transaction_payment::Event::TransactionFeePaid`].
		PGASFeePaid { who: T::AccountId, actual_fee: BalanceOf<T> },
	}
}

/// Transaction extension that charges transaction fees in PGAS when the caller holds enough and
/// the dispatched call passes [`Config::CallFilter`]. Otherwise it delegates to the wrapped
/// extension `S`.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, Eq, PartialEq)]
pub struct ChargePGAS<T, S> {
	inner: S,
	/// When set, the PGAS path is unconditionally skipped and the extension behaves as a pure
	/// delegate to `inner`. Skipped in the codec because it can only be set by runtime code
	#[codec(skip)]
	skip_pgas: bool,
	_phantom: core::marker::PhantomData<T>,
}

impl<T, S: StaticTypeInfo> TypeInfo for ChargePGAS<T, S> {
	type Identity = S;
	fn type_info() -> scale_info::Type {
		S::type_info()
	}
}

impl<T, S: Default> Default for ChargePGAS<T, S> {
	fn default() -> Self {
		Self { inner: S::default(), skip_pgas: false, _phantom: core::marker::PhantomData }
	}
}

impl<T, S> ChargePGAS<T, S> {
	/// Create a new `ChargePGAS` that unconditionally delegates to `inner`, skipping the PGAS
	/// path entirely.
	pub fn new_skip_pgas(inner: S) -> Self {
		Self { inner, skip_pgas: true, _phantom: core::marker::PhantomData }
	}
}

impl<T, S> From<S> for ChargePGAS<T, S> {
	fn from(inner: S) -> Self {
		Self { inner, skip_pgas: false, _phantom: core::marker::PhantomData }
	}
}

impl<T, S: core::fmt::Debug> core::fmt::Debug for ChargePGAS<T, S> {
	#[cfg(feature = "std")]
	fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
		write!(f, "ChargePGAS({:?})", self.inner)
	}
	#[cfg(not(feature = "std"))]
	fn fmt(&self, _: &mut core::fmt::Formatter) -> core::fmt::Result {
		Ok(())
	}
}

/// Info passed from `validate` to `prepare`.
pub enum Val<InnerVal, T: Config> {
	/// Caller pays with PGAS: `fee` units will be withdrawn in `prepare`.
	PGAS { who: T::AccountId, fee: BalanceOf<T> },
	/// Delegate to the inner extension.
	Inner(InnerVal),
}

/// Info passed from `prepare` to `post_dispatch`.
pub enum Pre<InnerPre, T: Config> {
	/// Fee withdrawn as a credit against the PGAS asset.
	PGAS {
		/// Account the fee was withdrawn from.
		who: T::AccountId,
		/// Credit holding the full reserved fee.
		credit: Credit<T::AccountId, T::Assets>,
		/// Weight difference between what [`ChargePGAS::weight`] reserved and the full PGAS path
		/// (`charge_pgas`), returned to the caller in `post_dispatch`.
		weight_refund: Weight,
	},
	/// Inner extension was used (filter miss, unsigned, or caller lacked PGAS).
	Inner {
		/// `Pre` produced by the inner extension, forwarded to its `post_dispatch`.
		inner: InnerPre,
		/// Weight to refund on top of whatever the inner extension refunds
		extra_refund: Weight,
	},
}

impl<T: Config + Send + Sync, S: TransactionExtension<T::RuntimeCall>>
	TransactionExtension<T::RuntimeCall> for ChargePGAS<T, S>
where
	T::RuntimeCall: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
	BalanceOf<T>: Send + Sync,
	AssetIdOf<T>: Send + Sync,
	<T::RuntimeCall as Dispatchable>::RuntimeOrigin: AsSystemOriginSigner<T::AccountId> + Clone,
{
	const IDENTIFIER: &'static str = S::IDENTIFIER;
	type Implicit = S::Implicit;
	type Val = Val<S::Val, T>;
	type Pre = Pre<S::Pre, T>;

	fn implicit(&self) -> Result<Self::Implicit, TransactionValidityError> {
		self.inner.implicit()
	}

	fn metadata() -> alloc::vec::Vec<sp_runtime::traits::TransactionExtensionMetadata> {
		S::metadata()
	}

	fn weight(&self, call: &T::RuntimeCall) -> Weight {
		let inner = self.inner.weight(call);
		if self.skip_pgas {
			return inner;
		}
		if T::CallFilter::contains(call) {
			<T as Config>::WeightInfo::charge_pgas()
				.max(inner.saturating_add(<T as Config>::WeightInfo::charge_pgas_skip()))
		} else {
			inner
		}
	}

	fn validate(
		&self,
		origin: OriginFor<T>,
		call: &T::RuntimeCall,
		info: &DispatchInfoOf<T::RuntimeCall>,
		len: usize,
		self_implicit: S::Implicit,
		inherited_implication: &impl Implication,
		source: TransactionSource,
	) -> ValidateResult<Self::Val, T::RuntimeCall> {
		// PGAS path: signed origin, call passes the filter, and caller holds at least `fee`.
		// Skipped entirely when the extension was constructed with `new_skip_pgas`.
		if !self.skip_pgas &&
			let Some(who) = origin.as_system_origin_signer().cloned() &&
			T::CallFilter::contains(call)
		{
			let fee = pallet_transaction_payment::Pallet::<T>::compute_fee(
				len as u32,
				info,
				Zero::zero(),
			);
			// `Expendable`: PGAS is meant to be minted across many accounts per user, so
			// allow fee withdrawal to dust the account once it drops below ED.
			let pgas = <T::Assets as fungibles::Inspect<T::AccountId>>::reducible_balance(
				T::PGASAssetId::get(),
				&who,
				Preservation::Expendable,
				Fortitude::Polite,
			);
			if pgas >= fee {
				let priority =
					ChargeTransactionPayment::<T>::get_priority(info, len, Zero::zero(), fee);
				return Ok((
					ValidTransaction { priority, ..Default::default() },
					Val::PGAS { who, fee },
					origin,
				));
			}
		}

		// Fall through to the inner extension.
		let (validity, val, origin) = self.inner.validate(
			origin,
			call,
			info,
			len,
			self_implicit,
			inherited_implication,
			source,
		)?;
		Ok((validity, Val::Inner(val), origin))
	}

	fn prepare(
		self,
		val: Self::Val,
		origin: &OriginFor<T>,
		call: &T::RuntimeCall,
		info: &DispatchInfoOf<T::RuntimeCall>,
		len: usize,
	) -> Result<Self::Pre, TransactionValidityError> {
		let inner_weight = self.inner.weight(call);
		let charge_pgas = <T as Config>::WeightInfo::charge_pgas();
		let charge_pgas_skip = <T as Config>::WeightInfo::charge_pgas_skip();
		match val {
			Val::PGAS { who, fee } => {
				// PGAS is committed at `validate`; if the balance dropped since, the tx is
				// rejected rather than falling back to the inner extension.
				let credit = <T::Assets as fungibles::Balanced<T::AccountId>>::withdraw(
					T::PGASAssetId::get(),
					&who,
					fee,
					Precision::Exact,
					Preservation::Expendable,
					Fortitude::Polite,
				)
				.map_err(|_| InvalidTransaction::Payment)?;

				// `weight()` reserved `charge_pgas.max(inner + charge_pgas_skip)`; the PGAS path
				// only consumes `charge_pgas`, so the excess is refunded.
				let reserved = charge_pgas.max(inner_weight.saturating_add(charge_pgas_skip));
				let weight_refund = reserved.saturating_sub(charge_pgas);
				Ok(Pre::PGAS { who, credit, weight_refund })
			},
			Val::Inner(val) => {
				let extra_refund = if !self.skip_pgas && T::CallFilter::contains(call) {
					// Filter matched, but likely the caller didn't hold enough PGAS, so we fell
					// back to `S`.
					let reserved = charge_pgas.max(inner_weight.saturating_add(charge_pgas_skip));
					let consumed = if origin.as_system_origin_signer().is_some() {
						inner_weight.saturating_add(charge_pgas_skip)
					} else {
						inner_weight
					};
					reserved.saturating_sub(consumed)
				} else {
					// `skip_pgas` reserved only `inner_weight` in `weight()`, so no extra refund.
					Weight::zero()
				};
				let inner = self.inner.prepare(val, origin, call, info, len)?;
				Ok(Pre::Inner { inner, extra_refund })
			},
		}
	}

	fn post_dispatch_details(
		pre: Self::Pre,
		info: &DispatchInfoOf<T::RuntimeCall>,
		post_info: &PostDispatchInfoOf<T::RuntimeCall>,
		len: usize,
		result: &DispatchResult,
	) -> Result<Weight, TransactionValidityError> {
		match pre {
			Pre::PGAS { who, credit, weight_refund } => {
				let mut actual_post_info = *post_info;
				actual_post_info.refund(weight_refund);
				let actual_fee = pallet_transaction_payment::Pallet::<T>::compute_actual_fee(
					len as u32,
					info,
					&actual_post_info,
					Zero::zero(),
				);

				// Split the reserved credit into the consumed portion (dropped below to burn)
				// and the refund owed back to `who`.
				let reserved = credit.peek();
				let (consumed, fee_refund) = credit.split(actual_fee);
				// Equals `actual_fee` on the happy path; if the refund cannot be returned to
				// `who` we burn the full reserved amount and report it.
				let burned = if fee_refund.peek().is_zero() {
					actual_fee
				} else {
					match <T::Assets as fungibles::Balanced<T::AccountId>>::resolve(
						&who, fee_refund,
					) {
						Ok(()) => actual_fee,
						Err(fee_refund) => {
							log::debug!(target: LOG_TARGET, "PGAS fee refund to {who:?} failed; burning full reserved fee {reserved:?}");
							let _ = consumed.merge(fee_refund);
							reserved
						},
					}
				};
				Pallet::<T>::deposit_event(Event::PGASFeePaid { who, actual_fee: burned });
				Ok(weight_refund)
			},
			Pre::Inner { inner, extra_refund } => {
				let inner_refund = S::post_dispatch_details(inner, info, post_info, len, result)?;
				Ok(inner_refund.saturating_add(extra_refund))
			},
		}
	}
}
