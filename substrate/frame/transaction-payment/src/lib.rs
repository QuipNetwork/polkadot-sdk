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

//! # Transaction Payment Pallet
//!
//! This pallet provides the basic logic needed to pay the absolute minimum amount needed for a
//! transaction to be included. This includes:
//!   - _base fee_: This is the minimum amount a user pays for a transaction. It is declared
//! 	as a base _weight_ in the runtime and converted to a fee using `WeightToFee`.
//!   - _weight fee_: A fee proportional to amount of weight a transaction consumes.
//!   - _length fee_: A fee proportional to the encoded length of the transaction.
//!   - _tip_: An optional tip. Tip increases the priority of the transaction, giving it a higher
//!     chance to be included by the transaction queue.
//!
//! The base fee and adjusted weight and length fees constitute the _inclusion fee_, which is
//! the minimum fee for a transaction to be included in a block.
//!
//! The formula of final fee:
//!   ```ignore
//!   inclusion_fee = base_fee + length_fee + [targeted_fee_adjustment * weight_fee];
//!   final_fee = inclusion_fee + tip;
//!   ```
//!
//!   - `targeted_fee_adjustment`: This is a multiplier that can tune the final fee based on
//! 	the congestion of the network.
//!
//! Additionally, this pallet allows one to configure:
//!   - The mapping between one unit of weight to one unit of fee via [`Config::WeightToFee`].
//!   - A means of updating the fee for the next block, via defining a multiplier, based on the
//!     final state of the chain at the end of the previous block. This can be configured via
//!     [`Config::FeeMultiplierUpdate`]
//!   - How the fees are paid via [`Config::OnChargeTransaction`].

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use scale_info::TypeInfo;

use frame_support::{
	dispatch::{
		DispatchClass, DispatchInfo, DispatchResult, GetDispatchInfo, Pays, PostDispatchInfo,
	},
	pallet_prelude::TransactionSource,
	traits::{Defensive, EstimateCallFee, EstimateFee, Get, Imbalance, SuppressedDrop},
	weights::{Weight, WeightToFee},
	DebugNoBound,
};
pub use pallet::*;
pub use payment::*;
use sp_runtime::{
	traits::{
		Convert, DispatchInfoOf, Dispatchable, One, PostDispatchInfoOf, SaturatedConversion,
		Saturating, TransactionExtension, Zero,
	},
	transaction_validity::{TransactionPriority, TransactionValidityError, ValidTransaction},
	Debug, FixedPointNumber, FixedU128, Perbill, Perquintill,
};
pub use types::{FeeDetails, InclusionFee, RuntimeDispatchInfo};
pub use weights::WeightInfo;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
#[cfg(feature = "runtime-benchmarks")]
pub use benchmarking::Config as BenchmarkConfig;

mod payment;
mod types;
pub mod weights;

/// Fee multiplier.
pub type Multiplier = FixedU128;

type BalanceOf<T> = <<T as Config>::OnChargeTransaction as OnChargeTransaction<T>>::Balance;
type CreditOf<T> = <StoredCreditOf<T> as SuppressedDrop>::Inner;
type StoredCreditOf<T> = <<T as Config>::OnChargeTransaction as TxCreditHold<T>>::Credit;

const LOG_TARGET: &str = "runtime::txpayment";

/// A struct to update the weight multiplier per block. It implements `Convert<Multiplier,
/// Multiplier>`, meaning that it can convert the previous multiplier to the next one. This should
/// be called on `on_finalize` of a block, prior to potentially cleaning the weight data from the
/// system pallet.
///
/// given:
/// 	s = previous block weight
/// 	s'= ideal block weight
/// 	m = maximum block weight
/// 		diff = (s - s')/m
/// 		v = 0.00001
/// 		t1 = (v * diff)
/// 		t2 = (v * diff)^2 / 2
/// 	then:
/// 	next_multiplier = prev_multiplier * (1 + t1 + t2)
///
/// Where `(s', v)` must be given as the `Get` implementation of the `T` generic type. Moreover, `M`
/// must provide the minimum allowed value for the multiplier. Note that a runtime should ensure
/// with tests that the combination of this `M` and `V` is not such that the multiplier can drop to
/// zero and never recover.
///
/// Note that `s'` is interpreted as a portion in the _normal transaction_ capacity of the block.
/// For example, given `s' == 0.25` and `AvailableBlockRatio = 0.75`, then the target fullness is
/// _0.25 of the normal capacity_ and _0.1875 of the entire block_.
///
/// Since block weight is multi-dimension, we use the scarcer resource, referred as limiting
/// dimension, for calculation of fees. We determine the limiting dimension by comparing the
/// dimensions using the ratio of `dimension_value / max_dimension_value` and selecting the largest
/// ratio. For instance, if a block is 30% full based on `ref_time` and 25% full based on
/// `proof_size`, we identify `ref_time` as the limiting dimension, indicating that the block is 30%
/// full.
///
/// This implementation implies the bound:
/// - `v ≤ p / k * (s − s')`
/// - or, solving for `p`: `p >= v * k * (s - s')`
///
/// where `p` is the amount of change over `k` blocks.
///
/// Hence:
/// - in a fully congested chain: `p >= v * k * (1 - s')`.
/// - in an empty chain: `p >= v * k * (-s')`.
///
/// For example, when all blocks are full and there are 28800 blocks per day (default in
/// `substrate-node`) and v == 0.00001, s' == 0.1875, we'd have:
///
/// p >= 0.00001 * 28800 * 0.8125
/// p >= 0.234
///
/// Meaning that fees can change by around ~23% per day, given extreme congestion.
///
/// More info can be found at:
/// <https://research.web3.foundation/Polkadot/overview/token-economics>
pub struct TargetedFeeAdjustment<T, S, V, M, X>(core::marker::PhantomData<(T, S, V, M, X)>);

/// Something that can convert the current multiplier to the next one.
pub trait MultiplierUpdate: Convert<Multiplier, Multiplier> {
	/// Minimum multiplier. Any outcome of the `convert` function should be at least this.
	fn min() -> Multiplier;
	/// Maximum multiplier. Any outcome of the `convert` function should be less or equal this.
	fn max() -> Multiplier;
	/// Target block saturation level
	fn target() -> Perquintill;
	/// Variability factor
	fn variability() -> Multiplier;
}

impl MultiplierUpdate for () {
	fn min() -> Multiplier {
		Default::default()
	}
	fn max() -> Multiplier {
		<Multiplier as sp_runtime::traits::Bounded>::max_value()
	}
	fn target() -> Perquintill {
		Default::default()
	}
	fn variability() -> Multiplier {
		Default::default()
	}
}

impl<T, S, V, M, X> MultiplierUpdate for TargetedFeeAdjustment<T, S, V, M, X>
where
	T: frame_system::Config,
	S: Get<Perquintill>,
	V: Get<Multiplier>,
	M: Get<Multiplier>,
	X: Get<Multiplier>,
{
	fn min() -> Multiplier {
		M::get()
	}
	fn max() -> Multiplier {
		X::get()
	}
	fn target() -> Perquintill {
		S::get()
	}
	fn variability() -> Multiplier {
		V::get()
	}
}

impl<T, S, V, M, X> Convert<Multiplier, Multiplier> for TargetedFeeAdjustment<T, S, V, M, X>
where
	T: frame_system::Config,
	S: Get<Perquintill>,
	V: Get<Multiplier>,
	M: Get<Multiplier>,
	X: Get<Multiplier>,
{
	fn convert(previous: Multiplier) -> Multiplier {
		// Defensive only. The multiplier in storage should always be at most positive. Nonetheless
		// we recover here in case of errors, because any value below this would be stale and can
		// never change.
		let min_multiplier = M::get();
		let max_multiplier = X::get();
		let previous = previous.max(min_multiplier);

		let weights = T::BlockWeights::get();
		// the computed ratio is only among the normal class.
		let normal_max_weight =
			weights.get(DispatchClass::Normal).max_total.unwrap_or(weights.max_block);
		let current_block_weight = frame_system::Pallet::<T>::block_weight();
		let normal_block_weight =
			current_block_weight.get(DispatchClass::Normal).min(normal_max_weight);

		// Normalize dimensions so they can be compared. Ensure (defensive) max weight is non-zero.
		let normalized_ref_time = Perbill::from_rational(
			normal_block_weight.ref_time(),
			normal_max_weight.ref_time().max(1),
		);
		let normalized_proof_size = Perbill::from_rational(
			normal_block_weight.proof_size(),
			normal_max_weight.proof_size().max(1),
		);

		// Pick the limiting dimension. If the proof size is the limiting dimension, then the
		// multiplier is adjusted by the proof size. Otherwise, it is adjusted by the ref time.
		let (normal_limiting_dimension, max_limiting_dimension) =
			if normalized_ref_time < normalized_proof_size {
				(normal_block_weight.proof_size(), normal_max_weight.proof_size())
			} else {
				(normal_block_weight.ref_time(), normal_max_weight.ref_time())
			};

		let target_block_fullness = S::get();
		let adjustment_variable = V::get();

		let target_weight = (target_block_fullness * max_limiting_dimension) as u128;
		let block_weight = normal_limiting_dimension as u128;

		// determines if the first_term is positive
		let positive = block_weight >= target_weight;
		let diff_abs = block_weight.max(target_weight) - block_weight.min(target_weight);

		// defensive only, a test case assures that the maximum weight diff can fit in Multiplier
		// without any saturation.
		let diff = Multiplier::saturating_from_rational(diff_abs, max_limiting_dimension.max(1));
		let diff_squared = diff.saturating_mul(diff);

		let v_squared_2 = adjustment_variable.saturating_mul(adjustment_variable) /
			Multiplier::saturating_from_integer(2);

		let first_term = adjustment_variable.saturating_mul(diff);
		let second_term = v_squared_2.saturating_mul(diff_squared);

		if positive {
			let excess = first_term.saturating_add(second_term).saturating_mul(previous);
			previous.saturating_add(excess).clamp(min_multiplier, max_multiplier)
		} else {
			// Defensive-only: first_term > second_term. Safe subtraction.
			let negative = first_term.saturating_sub(second_term).saturating_mul(previous);
			previous.saturating_sub(negative).clamp(min_multiplier, max_multiplier)
		}
	}
}

/// A struct to make the fee multiplier a constant
pub struct ConstFeeMultiplier<M: Get<Multiplier>>(core::marker::PhantomData<M>);

impl<M: Get<Multiplier>> MultiplierUpdate for ConstFeeMultiplier<M> {
	fn min() -> Multiplier {
		M::get()
	}
	fn max() -> Multiplier {
		M::get()
	}
	fn target() -> Perquintill {
		Default::default()
	}
	fn variability() -> Multiplier {
		Default::default()
	}
}

impl<M> Convert<Multiplier, Multiplier> for ConstFeeMultiplier<M>
where
	M: Get<Multiplier>,
{
	fn convert(_previous: Multiplier) -> Multiplier {
		Self::min()
	}
}

/// Storage releases of the pallet.
#[derive(Encode, Decode, Clone, Copy, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
pub enum Releases {
	/// Original version of the pallet.
	V1Ancient,
	/// One that bumps the usage to FixedU128 from FixedI128.
	V2,
}

impl Default for Releases {
	fn default() -> Self {
		Releases::V1Ancient
	}
}

/// Default value for NextFeeMultiplier. This is used in genesis and is also used in
/// NextFeeMultiplierOnEmpty() to provide a value when none exists in storage.
const MULTIPLIER_DEFAULT_VALUE: Multiplier = Multiplier::from_u32(1);

#[frame_support::pallet]
pub mod pallet {
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	use super::*;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	pub mod config_preludes {
		use super::*;
		use frame_support::derive_impl;

		/// Default prelude sensible to be used in a testing environment.
		pub struct TestDefaultConfig;

		#[derive_impl(frame_system::config_preludes::TestDefaultConfig, no_aggregated_types)]
		impl frame_system::DefaultConfig for TestDefaultConfig {}

		#[frame_support::register_default_impl(TestDefaultConfig)]
		impl DefaultConfig for TestDefaultConfig {
			#[inject_runtime_type]
			type RuntimeEvent = ();
			type FeeMultiplierUpdate = ();
			type OperationalFeeMultiplier = ();
			type WeightInfo = ();
		}
	}

	#[pallet::config(with_default)]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		#[pallet::no_default_bounds]
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Handler for withdrawing, refunding and depositing the transaction fee.
		/// Transaction fees are withdrawn before the transaction is executed.
		/// After the transaction was executed the transaction weight can be
		/// adjusted, depending on the used resources by the transaction. If the
		/// transaction weight is lower than expected, parts of the transaction fee
		/// might be refunded. In the end the fees can be deposited.
		#[pallet::no_default]
		type OnChargeTransaction: OnChargeTransaction<Self>;

		/// Convert a weight value into a deductible fee based on the currency type.
		#[pallet::no_default]
		type WeightToFee: WeightToFee<Balance = BalanceOf<Self>>;

		/// Convert a length value into a deductible fee based on the currency type.
		#[pallet::no_default]
		type LengthToFee: WeightToFee<Balance = BalanceOf<Self>>;

		/// Update the multiplier of the next block, based on the previous block's weight.
		type FeeMultiplierUpdate: MultiplierUpdate;

		/// A fee multiplier for `Operational` extrinsics to compute "virtual tip" to boost their
		/// `priority`
		///
		/// This value is multiplied by the `final_fee` to obtain a "virtual tip" that is later
		/// added to a tip component in regular `priority` calculations.
		/// It means that a `Normal` transaction can front-run a similarly-sized `Operational`
		/// extrinsic (with no tip), by including a tip value greater than the virtual tip.
		///
		/// ```rust,ignore
		/// // For `Normal`
		/// let priority = priority_calc(tip);
		///
		/// // For `Operational`
		/// let virtual_tip = (inclusion_fee + tip) * OperationalFeeMultiplier;
		/// let priority = priority_calc(tip + virtual_tip);
		/// ```
		///
		/// Note that since we use `final_fee` the multiplier applies also to the regular `tip`
		/// sent with the transaction. So, not only does the transaction get a priority bump based
		/// on the `inclusion_fee`, but we also amplify the impact of tips applied to `Operational`
		/// transactions.
		#[pallet::constant]
		type OperationalFeeMultiplier: Get<u8>;

		/// The weight information of this pallet.
		type WeightInfo: WeightInfo;
	}

	#[pallet::type_value]
	pub fn NextFeeMultiplierOnEmpty() -> Multiplier {
		MULTIPLIER_DEFAULT_VALUE
	}

	#[pallet::storage]
	#[pallet::whitelist_storage]
	pub type NextFeeMultiplier<T: Config> =
		StorageValue<_, Multiplier, ValueQuery, NextFeeMultiplierOnEmpty>;

	#[pallet::storage]
	pub type StorageVersion<T: Config> = StorageValue<_, Releases, ValueQuery>;

	/// The `OnChargeTransaction` stores the withdrawn tx fee here.
	///
	/// Use `withdraw_txfee` and `remaining_txfee` to access from outside the crate.
	#[pallet::storage]
	#[pallet::whitelist_storage]
	pub(crate) type TxPaymentCredit<T: Config> = StorageValue<_, StoredCreditOf<T>>;

	#[pallet::genesis_config]
	pub struct GenesisConfig<T: Config> {
		pub multiplier: Multiplier,
		#[serde(skip)]
		pub _config: core::marker::PhantomData<T>,
	}

	impl<T: Config> Default for GenesisConfig<T> {
		fn default() -> Self {
			Self { multiplier: MULTIPLIER_DEFAULT_VALUE, _config: Default::default() }
		}
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
		fn build(&self) {
			StorageVersion::<T>::put(Releases::V2);
			NextFeeMultiplier::<T>::put(self.multiplier);
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A transaction fee `actual_fee`, of which `tip` was added to the minimum inclusion fee,
		/// has been paid by `who`.
		TransactionFeePaid { who: T::AccountId, actual_fee: BalanceOf<T>, tip: BalanceOf<T> },
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_finalize(_: frame_system::pallet_prelude::BlockNumberFor<T>) {
			NextFeeMultiplier::<T>::mutate(|fm| {
				*fm = T::FeeMultiplierUpdate::convert(*fm);
			});

			// We generally expect the `OnChargeTransaction` implementation to delete this value
			// after each transaction. To make sure it is never stored between blocks we
			// delete the value here just in case.
			TxPaymentCredit::<T>::take().map(|credit| {
				log::error!(target: LOG_TARGET, "The `TxPaymentCredit` was stored between blocks. This is a bug.");
				// Converting to inner makes sure that the drop implementation is called.
				credit.into_inner()
			});
		}

		#[cfg(feature = "std")]
		fn integrity_test() {
			// given weight == u64, we build multipliers from `diff` of two weight values, which can
			// at most be maximum block weight. Make sure that this can fit in a multiplier without
			// loss.
			assert!(
				<Multiplier as sp_runtime::traits::Bounded>::max_value() >=
					Multiplier::checked_from_integer::<u128>(
						T::BlockWeights::get().max_block.ref_time().try_into().unwrap()
					)
					.unwrap(),
			);

			let target = T::FeeMultiplierUpdate::target() *
				T::BlockWeights::get().get(DispatchClass::Normal).max_total.expect(
					"Setting `max_total` for `Normal` dispatch class is not compatible with \
					`transaction-payment` pallet.",
				);
			// add 1 percent;
			let addition = target / 100;
			if addition == Weight::zero() {
				// this is most likely because in a test setup we set everything to ()
				// or to `ConstFeeMultiplier`.
				return;
			}

			// This is the minimum value of the multiplier. Make sure that if we collapse to this
			// value, we can recover with a reasonable amount of traffic. For this test we assert
			// that if we collapse to minimum, the trend will be positive with a weight value which
			// is 1% more than the target.
			let min_value = T::FeeMultiplierUpdate::min();
			let target = target + addition;

			frame_system::Pallet::<T>::set_block_consumed_resources(target, 0);
			let next = T::FeeMultiplierUpdate::convert(min_value);
			assert!(
				next > min_value,
				"The minimum bound of the multiplier is too low. When \
				block saturation is more than target by 1% and multiplier is minimal then \
				the multiplier doesn't increase."
			);
		}
	}
}

impl<T: Config> Pallet<T> {
	/// Public function to access the next fee multiplier.
	pub fn next_fee_multiplier() -> Multiplier {
		NextFeeMultiplier::<T>::get()
	}

	/// Query the data that we know about the fee of a given `call`.
	///
	/// This pallet is not and cannot be aware of the internals of a signed extension, for example
	/// a tip. It only interprets the extrinsic as some encoded value and accounts for its weight
	/// and length, the runtime's extrinsic base weight, and the current fee multiplier.
	///
	/// All dispatchables must be annotated with weight and will have some fee info. This function
	/// always returns.
	pub fn query_info<Extrinsic: sp_runtime::traits::ExtrinsicLike + GetDispatchInfo>(
		unchecked_extrinsic: Extrinsic,
		len: u32,
	) -> RuntimeDispatchInfo<BalanceOf<T>>
	where
		T::RuntimeCall: Dispatchable<Info = DispatchInfo>,
	{
		// NOTE: we can actually make it understand `ChargeTransactionPayment`, but would be some
		// hassle for sure. We have to make it aware of the index of `ChargeTransactionPayment` in
		// `Extra`. Alternatively, we could actually execute the tx's per-dispatch and record the
		// balance of the sender before and after the pipeline.. but this is way too much hassle for
		// a very very little potential gain in the future.
		let dispatch_info = <Extrinsic as GetDispatchInfo>::get_dispatch_info(&unchecked_extrinsic);

		let partial_fee = if unchecked_extrinsic.is_bare() {
			// Bare extrinsics have no partial fee.
			0u32.into()
		} else {
			Self::compute_fee(len, &dispatch_info, 0u32.into())
		};

		let DispatchInfo { class, .. } = dispatch_info;

		RuntimeDispatchInfo { weight: dispatch_info.total_weight(), class, partial_fee }
	}

	/// Query the detailed fee of a given `call`.
	pub fn query_fee_details<Extrinsic: sp_runtime::traits::ExtrinsicLike + GetDispatchInfo>(
		unchecked_extrinsic: Extrinsic,
		len: u32,
	) -> FeeDetails<BalanceOf<T>>
	where
		T::RuntimeCall: Dispatchable<Info = DispatchInfo>,
	{
		let dispatch_info = <Extrinsic as GetDispatchInfo>::get_dispatch_info(&unchecked_extrinsic);

		let tip = 0u32.into();

		if unchecked_extrinsic.is_bare() {
			// Bare extrinsics have no inclusion fee.
			FeeDetails { inclusion_fee: None, tip }
		} else {
			Self::compute_fee_details(len, &dispatch_info, tip)
		}
	}

	/// Query information of a dispatch class, weight, and fee of a given encoded `Call`.
	pub fn query_call_info(call: T::RuntimeCall, len: u32) -> RuntimeDispatchInfo<BalanceOf<T>>
	where
		T::RuntimeCall: Dispatchable<Info = DispatchInfo> + GetDispatchInfo,
	{
		let dispatch_info = <T::RuntimeCall as GetDispatchInfo>::get_dispatch_info(&call);
		let DispatchInfo { class, .. } = dispatch_info;

		RuntimeDispatchInfo {
			weight: dispatch_info.total_weight(),
			class,
			partial_fee: Self::compute_fee(len, &dispatch_info, 0u32.into()),
		}
	}

	/// Query fee details of a given encoded `Call`.
	pub fn query_call_fee_details(call: T::RuntimeCall, len: u32) -> FeeDetails<BalanceOf<T>>
	where
		T::RuntimeCall: Dispatchable<Info = DispatchInfo> + GetDispatchInfo,
	{
		let dispatch_info = <T::RuntimeCall as GetDispatchInfo>::get_dispatch_info(&call);
		let tip = 0u32.into();

		Self::compute_fee_details(len, &dispatch_info, tip)
	}

	/// Compute the final fee value (including tip) for a particular transaction.
	pub fn compute_fee(
		len: u32,
		info: &DispatchInfoOf<T::RuntimeCall>,
		tip: BalanceOf<T>,
	) -> BalanceOf<T>
	where
		T::RuntimeCall: Dispatchable<Info = DispatchInfo>,
	{
		Self::compute_fee_details(len, info, tip).final_fee()
	}

	/// Compute the fee details for a particular transaction.
	pub fn compute_fee_details(
		len: u32,
		info: &DispatchInfoOf<T::RuntimeCall>,
		tip: BalanceOf<T>,
	) -> FeeDetails<BalanceOf<T>>
	where
		T::RuntimeCall: Dispatchable<Info = DispatchInfo>,
	{
		Self::compute_fee_raw(len, info.total_weight(), tip, info.pays_fee, info.class)
	}

	/// Compute the actual post dispatch fee for a particular transaction.
	///
	/// Identical to `compute_fee` with the only difference that the post dispatch corrected
	/// weight is used for the weight fee calculation.
	pub fn compute_actual_fee(
		len: u32,
		info: &DispatchInfoOf<T::RuntimeCall>,
		post_info: &PostDispatchInfoOf<T::RuntimeCall>,
		tip: BalanceOf<T>,
	) -> BalanceOf<T>
	where
		T::RuntimeCall: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
	{
		Self::compute_actual_fee_details(len, info, post_info, tip).final_fee()
	}

	/// Compute the actual post dispatch fee details for a particular transaction.
	pub fn compute_actual_fee_details(
		len: u32,
		info: &DispatchInfoOf<T::RuntimeCall>,
		post_info: &PostDispatchInfoOf<T::RuntimeCall>,
		tip: BalanceOf<T>,
	) -> FeeDetails<BalanceOf<T>>
	where
		T::RuntimeCall: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
	{
		Self::compute_fee_raw(
			len,
			post_info.calc_actual_weight(info),
			tip,
			post_info.pays_fee(info),
			info.class,
		)
	}

	fn compute_fee_raw(
		len: u32,
		weight: Weight,
		tip: BalanceOf<T>,
		pays_fee: Pays,
		class: DispatchClass,
	) -> FeeDetails<BalanceOf<T>> {
		if pays_fee == Pays::Yes {
			// the adjustable part of the fee.
			let unadjusted_weight_fee = Self::weight_to_fee(weight);
			let multiplier = NextFeeMultiplier::<T>::get();
			// final adjusted weight fee.
			let adjusted_weight_fee = multiplier.saturating_mul_int(unadjusted_weight_fee);

			// length fee. this is adjusted via `LengthToFee`.
			let len_fee = Self::length_to_fee(len);

			let base_fee = Self::weight_to_fee(T::BlockWeights::get().get(class).base_extrinsic);
			FeeDetails {
				inclusion_fee: Some(InclusionFee { base_fee, len_fee, adjusted_weight_fee }),
				tip,
			}
		} else {
			FeeDetails { inclusion_fee: None, tip }
		}
	}

	/// Compute the length portion of a fee by invoking the configured `LengthToFee` impl.
	pub fn length_to_fee(length: u32) -> BalanceOf<T> {
		T::LengthToFee::weight_to_fee(&Weight::from_parts(length as u64, 0))
	}

	/// Compute the unadjusted portion of the weight fee by invoking the configured `WeightToFee`
	/// impl. Note that the input `weight` is capped by the maximum block weight before computation.
	pub fn weight_to_fee(weight: Weight) -> BalanceOf<T> {
		// cap the weight to the maximum defined in runtime, otherwise it will be the
		// `Bounded` maximum of its data type, which is not desired.
		let capped_weight = weight.min(T::BlockWeights::get().max_block);
		T::WeightToFee::weight_to_fee(&capped_weight)
	}

	/// Deposit the [`Event::TransactionFeePaid`] event.
	pub fn deposit_fee_paid_event(who: T::AccountId, actual_fee: BalanceOf<T>, tip: BalanceOf<T>) {
		Self::deposit_event(Event::TransactionFeePaid { who, actual_fee, tip });
	}

	/// Withdraw `amount` from the currents transaction's fees.
	///
	/// If enough balance is available a credit of size `amount` is returned.
	///
	/// # Warning
	///
	/// Do **not** use this to pay for Weight fees. Use only to pay for storage fees
	/// that can be rolled back by a storage transaction.
	///
	/// # Note
	///
	/// This is only useful if a pallet knows that the pre-dispatch weight was vastly
	/// overestimated. Pallets need to make sure to leave enough balance to pay for the
	/// transaction fees. They can do that by first drawing as much as they need and then
	/// at the end of the transaction (when they know the post dispatch fee) return an error
	/// in case not enough is left. The error will automatically roll back all the storage
	/// changes done by the pallet including the balance drawn by calling this function.
	pub fn withdraw_txfee<Balance>(amount: Balance) -> Option<CreditOf<T>>
	where
		CreditOf<T>: Imbalance<Balance>,
		Balance: PartialOrd,
	{
		<TxPaymentCredit<T>>::mutate(|credit| {
			let credit = SuppressedDrop::as_mut(credit.as_mut()?);
			if amount > credit.peek() {
				return None;
			}
			Some(credit.extract(amount))
		})
	}

	/// Deposit some additional balance.
	pub fn deposit_txfee<Balance>(deposit: CreditOf<T>)
	where
		CreditOf<T>: Imbalance<Balance>,
	{
		<TxPaymentCredit<T>>::mutate(|credit| {
			if let Some(credit) = credit.as_mut().map(SuppressedDrop::as_mut) {
				credit.subsume(deposit);
			} else {
				*credit = Some(SuppressedDrop::new(deposit))
			}
		});
	}

	/// Return how much balance is currently available to pay for the transaction.
	///
	/// Does **not** include the tip.
	///
	/// If noone calls `charge_from_txfee` it is the same as the pre dispatch fee.
	pub fn remaining_txfee<Balance>() -> Balance
	where
		CreditOf<T>: Imbalance<Balance>,
		Balance: Default,
	{
		<TxPaymentCredit<T>>::get()
			.map(|c| SuppressedDrop::as_ref(&c).peek())
			.unwrap_or_default()
	}
}

impl<T> Convert<Weight, BalanceOf<T>> for Pallet<T>
where
	T: Config,
{
	/// Compute the fee for the specified weight.
	///
	/// This fee is already adjusted by the per block fee adjustment factor and is therefore the
	/// share that the weight contributes to the overall fee of a transaction. It is mainly
	/// for informational purposes and not used in the actual fee calculation.
	fn convert(weight: Weight) -> BalanceOf<T> {
		NextFeeMultiplier::<T>::get().saturating_mul_int(Self::weight_to_fee(weight))
	}
}

/// Require the transactor pay for themselves and maybe include a tip to gain additional priority
/// in the queue.
///
/// # Transaction Validity
///
/// This extension sets the `priority` field of `TransactionValidity` depending on the amount
/// of tip being paid per weight unit.
///
/// Operational transactions will receive an additional priority bump, so that they are normally
/// considered before regular transactions.
#[derive(Encode, Decode, DecodeWithMemTracking, Clone, Eq, PartialEq, TypeInfo)]
#[scale_info(skip_type_params(T))]
pub struct ChargeTransactionPayment<T: Config>(#[codec(compact)] BalanceOf<T>);

impl<T: Config> ChargeTransactionPayment<T>
where
	T::RuntimeCall: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
	BalanceOf<T>: Send + Sync,
{
	/// utility constructor. Used only in client/factory code.
	pub fn from(fee: BalanceOf<T>) -> Self {
		Self(fee)
	}

	/// Returns the tip as being chosen by the transaction sender.
	pub fn tip(&self) -> BalanceOf<T> {
		self.0
	}

	fn withdraw_fee(
		&self,
		who: &T::AccountId,
		call: &T::RuntimeCall,
		info: &DispatchInfoOf<T::RuntimeCall>,
		fee_with_tip: BalanceOf<T>,
	) -> Result<
		(
			BalanceOf<T>,
			<<T as Config>::OnChargeTransaction as OnChargeTransaction<T>>::LiquidityInfo,
		),
		TransactionValidityError,
	> {
		let tip = self.0;

		<<T as Config>::OnChargeTransaction as OnChargeTransaction<T>>::withdraw_fee(
			who,
			call,
			info,
			fee_with_tip,
			tip,
		)
		.map(|liquidity_info| (fee_with_tip, liquidity_info))
	}

	fn can_withdraw_fee(
		&self,
		who: &T::AccountId,
		call: &T::RuntimeCall,
		info: &DispatchInfoOf<T::RuntimeCall>,
		len: usize,
	) -> Result<BalanceOf<T>, TransactionValidityError> {
		let tip = self.0;
		let fee_with_tip = Pallet::<T>::compute_fee(len as u32, info, tip);

		<<T as Config>::OnChargeTransaction as OnChargeTransaction<T>>::can_withdraw_fee(
			who,
			call,
			info,
			fee_with_tip,
			tip,
		)?;
		Ok(fee_with_tip)
	}

	/// Get an appropriate priority for a transaction with the given `DispatchInfo`, encoded length
	/// and user-included tip.
	///
	/// The priority is based on the amount of `tip` the user is willing to pay per unit of either
	/// `weight` or `length`, depending which one is more limiting. For `Operational` extrinsics
	/// we add a "virtual tip" to the calculations.
	///
	/// The formula should simply be `tip / bounded_{weight|length}`, but since we are using
	/// integer division, we have no guarantees it's going to give results in any reasonable
	/// range (might simply end up being zero). Hence we use a scaling factor:
	/// `tip * (max_block_{weight|length} / bounded_{weight|length})`, since given current
	/// state of-the-art blockchains, number of per-block transactions is expected to be in a
	/// range reasonable enough to not saturate the `Balance` type while multiplying by the tip.
	pub fn get_priority(
		info: &DispatchInfoOf<T::RuntimeCall>,
		len: usize,
		tip: BalanceOf<T>,
		final_fee_with_tip: BalanceOf<T>,
	) -> TransactionPriority {
		// Calculate how many such extrinsics we could fit into an empty block and take the
		// limiting factor.
		let max_block_weight = T::BlockWeights::get().max_block;
		let max_block_length = *T::BlockLength::get().max.get(info.class) as u64;

		// bounded_weight is used as a divisor later so we keep it non-zero.
		let bounded_weight =
			info.total_weight().max(Weight::from_parts(1, 1)).min(max_block_weight);
		let bounded_length = (len as u64).clamp(1, max_block_length);

		// returns the scarce resource, i.e. the one that is limiting the number of transactions.
		let max_tx_per_block_weight = max_block_weight
			.checked_div_per_component(&bounded_weight)
			.defensive_proof("bounded_weight is non-zero; qed")
			.unwrap_or(1);
		let max_tx_per_block_length = max_block_length / bounded_length;
		// Given our current knowledge this value is going to be in a reasonable range - i.e.
		// less than 10^9 (2^30), so multiplying by the `tip` value is unlikely to overflow the
		// balance type. We still use saturating ops obviously, but the point is to end up with some
		// `priority` distribution instead of having all transactions saturate the priority.
		let max_tx_per_block = max_tx_per_block_length
			.min(max_tx_per_block_weight)
			.saturated_into::<BalanceOf<T>>();
		let max_reward = |val: BalanceOf<T>| val.saturating_mul(max_tx_per_block);

		// To distribute no-tip transactions a little bit, we increase the tip value by one.
		// This means that given two transactions without a tip, smaller one will be preferred.
		let tip = tip.saturating_add(One::one());
		let scaled_tip = max_reward(tip);

		match info.class {
			DispatchClass::Normal => {
				// For normal class we simply take the `tip_per_weight`.
				scaled_tip
			},
			DispatchClass::Mandatory => {
				// Mandatory extrinsics should be prohibited (e.g. by the [`CheckWeight`]
				// extensions), but just to be safe let's return the same priority as `Normal` here.
				scaled_tip
			},
			DispatchClass::Operational => {
				// A "virtual tip" value added to an `Operational` extrinsic.
				// This value should be kept high enough to allow `Operational` extrinsics
				// to get in even during congestion period, but at the same time low
				// enough to prevent a possible spam attack by sending invalid operational
				// extrinsics which push away regular transactions from the pool.
				let fee_multiplier = T::OperationalFeeMultiplier::get().saturated_into();
				let virtual_tip = final_fee_with_tip.saturating_mul(fee_multiplier);
				let scaled_virtual_tip = max_reward(virtual_tip);

				scaled_tip.saturating_add(scaled_virtual_tip)
			},
		}
		.saturated_into::<TransactionPriority>()
	}
}

impl<T: Config> core::fmt::Debug for ChargeTransactionPayment<T> {
	#[cfg(feature = "std")]
	fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
		write!(f, "ChargeTransactionPayment<{:?}>", self.0)
	}
	#[cfg(not(feature = "std"))]
	fn fmt(&self, _: &mut core::fmt::Formatter) -> core::fmt::Result {
		Ok(())
	}
}

/// The info passed between the validate and prepare steps for the `ChargeAssetTxPayment` extension.
#[derive(DebugNoBound)]
pub enum Val<T: Config> {
	Charge {
		tip: BalanceOf<T>,
		// who paid the fee
		who: T::AccountId,
		// transaction fee
		fee_with_tip: BalanceOf<T>,
	},
	NoCharge,
}

/// The info passed between the prepare and post-dispatch steps for the `ChargeAssetTxPayment`
/// extension.
pub enum Pre<T: Config> {
	Charge {
		tip: BalanceOf<T>,
		// who paid the fee
		who: T::AccountId,
		// implementation defined type that is passed into the post charge function
		liquidity_info:
			<<T as Config>::OnChargeTransaction as OnChargeTransaction<T>>::LiquidityInfo,
	},
	NoCharge {
		// weight initially estimated by the extension, to be refunded
		refund: Weight,
	},
}

impl<T: Config> core::fmt::Debug for Pre<T> {
	#[cfg(feature = "std")]
	fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
		match self {
			Pre::Charge { tip, who, liquidity_info: _ } => {
				write!(f, "Charge {{ tip: {:?}, who: {:?}, imbalance: <stripped> }}", tip, who)
			},
			Pre::NoCharge { refund } => write!(f, "NoCharge {{ refund: {:?} }}", refund),
		}
	}

	#[cfg(not(feature = "std"))]
	fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
		f.write_str("<wasm:stripped>")
	}
}

impl<T: Config> TransactionExtension<T::RuntimeCall> for ChargeTransactionPayment<T>
where
	T::RuntimeCall: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
{
	const IDENTIFIER: &'static str = "ChargeTransactionPayment";
	type Implicit = ();
	type Val = Val<T>;
	type Pre = Pre<T>;

	fn weight(&self, _: &T::RuntimeCall) -> Weight {
		T::WeightInfo::charge_transaction_payment()
	}

	fn validate(
		&self,
		origin: <T::RuntimeCall as Dispatchable>::RuntimeOrigin,
		call: &T::RuntimeCall,
		info: &DispatchInfoOf<T::RuntimeCall>,
		len: usize,
		_: (),
		_implication: &impl Encode,
		_source: TransactionSource,
	) -> Result<
		(ValidTransaction, Self::Val, <T::RuntimeCall as Dispatchable>::RuntimeOrigin),
		TransactionValidityError,
	> {
		let Ok(who) = frame_system::ensure_signed(origin.clone()) else {
			return Ok((ValidTransaction::default(), Val::NoCharge, origin));
		};
		let fee_with_tip = self.can_withdraw_fee(&who, call, info, len)?;
		let tip = self.0;
		Ok((
			ValidTransaction {
				priority: Self::get_priority(info, len, tip, fee_with_tip),
				..Default::default()
			},
			Val::Charge { tip: self.0, who, fee_with_tip },
			origin,
		))
	}

	fn prepare(
		self,
		val: Self::Val,
		_origin: &<T::RuntimeCall as Dispatchable>::RuntimeOrigin,
		call: &T::RuntimeCall,
		info: &DispatchInfoOf<T::RuntimeCall>,
		_len: usize,
	) -> Result<Self::Pre, TransactionValidityError> {
		match val {
			Val::Charge { tip, who, fee_with_tip } => {
				// Mutating call to `withdraw_fee` to actually charge for the transaction.
				let (_fee_with_tip, liquidity_info) =
					self.withdraw_fee(&who, call, info, fee_with_tip)?;
				Ok(Pre::Charge { tip, who, liquidity_info })
			},
			Val::NoCharge => Ok(Pre::NoCharge { refund: self.weight(call) }),
		}
	}

	fn post_dispatch_details(
		pre: Self::Pre,
		info: &DispatchInfoOf<T::RuntimeCall>,
		post_info: &PostDispatchInfoOf<T::RuntimeCall>,
		len: usize,
		_result: &DispatchResult,
	) -> Result<Weight, TransactionValidityError> {
		let (tip, who, liquidity_info) = match pre {
			Pre::Charge { tip, who, liquidity_info } => (tip, who, liquidity_info),
			Pre::NoCharge { refund } => {
				// No-op: Refund everything
				return Ok(refund);
			},
		};
		let actual_fee_with_tip =
			Pallet::<T>::compute_actual_fee(len as u32, info, &post_info, tip);
		T::OnChargeTransaction::correct_and_deposit_fee(
			&who,
			info,
			&post_info,
			actual_fee_with_tip,
			tip,
			liquidity_info,
		)?;
		Pallet::<T>::deposit_event(Event::<T>::TransactionFeePaid {
			who,
			actual_fee: actual_fee_with_tip,
			tip,
		});
		Ok(Weight::zero())
	}
}

impl<T: Config, AnyCall: GetDispatchInfo + Encode> EstimateCallFee<AnyCall, BalanceOf<T>>
	for Pallet<T>
where
	T::RuntimeCall: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
{
	fn estimate_call_fee(call: &AnyCall, post_info: PostDispatchInfo) -> BalanceOf<T> {
		let len = call.encoded_size() as u32;
		let info = call.get_dispatch_info();
		Self::compute_actual_fee(len, &info, &post_info, Zero::zero())
	}
}

impl<T: Config> EstimateFee<BalanceOf<T>> for Pallet<T>
where
	T::RuntimeCall: Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
{
	fn estimate_fee(len: u32, info: &DispatchInfo) -> BalanceOf<T> {
		Self::compute_fee(len, info, Zero::zero())
	}
}
