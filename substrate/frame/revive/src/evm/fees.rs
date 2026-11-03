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

//! Contains the fee types that need to be configured for `pallet-transaction-payment`.

use crate::{
	BalanceOf, CallOf, Config, DispatchErrorWithPostInfo, DispatchResultWithPostInfo, Error,
	LOG_TARGET, PostDispatchInfo,
	evm::{
		OnChargeTransactionBalanceOf,
		runtime::{EthExtra, SetWeightLimit},
	},
};
use codec::Encode;
use core::marker::PhantomData;
use frame_support::{
	dispatch::{DispatchClass, DispatchInfo, GetDispatchInfo},
	pallet_prelude::Weight,
	traits::{Get, SuppressedDrop, fungible::Credit, tokens::Balance},
	weights::WeightToFee,
};
use frame_system::Config as SysConfig;
use num_traits::{One, Zero};
use pallet_transaction_payment::{
	Config as TxConfig, MultiplierUpdate, NextFeeMultiplier, Pallet as TxPallet, TxCreditHold,
};
use sp_arithmetic::{FixedPointOperand, SignedRounding};
use sp_runtime::{
	FixedPointNumber, FixedU128, SaturatedConversion, Saturating,
	generic::UncheckedExtrinsic,
	traits::{
		Block as BlockT, Dispatchable, ExtensionPostDispatchWeightHandler, TransactionExtension,
	},
};

type CreditOf<T> = Credit<<T as frame_system::Config>::AccountId, <T as Config>::Currency>;

/// The only [`WeightToFee`] implementation that is supported by this pallet.
///
/// `P,Q`: Rational number that defines the ref_time to fee mapping.
///
/// This enforces a ratio of ref_time and proof_time that is proportional
/// to their distribution in the block limits. We enforce the usage of this fee
/// structure because our gas mapping depends on it.
///
/// # Panics
///
/// If either `P` or `Q` is zero.
pub struct BlockRatioFee<const P: u128, const Q: u128, T, B>(PhantomData<(T, B)>);

/// The only [`InfoT`] implementation valid for [`Config::FeeInfo`].
///
/// The reason for this type is to avoid coupling the rest of pallet_revive to
/// pallet_transaction_payment. This way we bundle all the trait bounds in once place.
pub struct Info<Address, Signature, Extra>(PhantomData<(Address, Signature, Extra)>);

/// A trait that exposes all the transaction payment details to `pallet_revive`.
///
/// This trait is sealed. Use [`Info`].
pub trait InfoT<T: Config>: seal::Sealed {
	/// Check that the fee configuration of the chain is valid.
	///
	/// This is being called by the pallets `integrity_check`.
	fn integrity_test() {}

	/// Exposes the current fee multiplier of the chain.
	fn next_fee_multiplier() -> FixedU128 {
		FixedU128::from_rational(1, 1)
	}

	/// The reciprocal of the next fee multiplier.
	///
	/// Needed when dividing a fee by the multiplier before presenting
	/// it to the eth wallet as gas. Needed because the wallet will multiply
	/// it with the gas_price which includes this multiplicator.
	fn next_fee_multiplier_reciprocal() -> FixedU128 {
		Self::next_fee_multiplier()
			.reciprocal()
			.expect("The minimum multiplier is not 0. We check that in `integrity_test`; qed")
	}

	/// Calculate the fee of a transaction including the next fee multiplier adjustment.
	fn tx_fee(_len: u32, _call: &CallOf<T>) -> BalanceOf<T> {
		Zero::zero()
	}

	/// Calculate the fee using the weight instead of a dispatch info.
	fn tx_fee_from_weight(_encoded_len: u32, _weight: &Weight) -> BalanceOf<T> {
		Zero::zero()
	}

	/// The base extrinsic and len fee.
	fn fixed_fee(_encoded_len: u32) -> BalanceOf<T> {
		Zero::zero()
	}

	/// Makes sure that not too much storage deposit was withdrawn.
	fn ensure_not_overdrawn(
		_fee: BalanceOf<T>,
		result: DispatchResultWithPostInfo,
	) -> DispatchResultWithPostInfo {
		result
	}

	/// Get the dispatch info of a call with the proper extension weight set.
	fn dispatch_info(_call: &CallOf<T>) -> DispatchInfo {
		Default::default()
	}

	/// The dispatch info with the weight argument set to `0`.
	fn base_dispatch_info(_call: &mut CallOf<T>) -> DispatchInfo {
		Default::default()
	}

	/// Calculate the encoded length of a call.
	fn encoded_len(_eth_transact_call: CallOf<T>) -> u32 {
		0
	}

	/// Convert a weight to an unadjusted fee.
	fn weight_to_fee(_weight: &Weight) -> BalanceOf<T> {
		Zero::zero()
	}

	/// Convert a weight to an unadjusted fee using an average instead of maximum.
	fn weight_to_fee_average(_weight: &Weight) -> BalanceOf<T> {
		Zero::zero()
	}

	/// Convert an unadjusted fee back to a weight.
	fn fee_to_weight(_fee: BalanceOf<T>) -> Weight {
		Zero::zero()
	}

	/// Convert the length of a transaction to an unadjusted weight.
	fn length_to_fee(_len: u32) -> BalanceOf<T> {
		Zero::zero()
	}

	/// Add some additional fee to the `pallet_transaction_payment` credit.
	fn deposit_txfee(_credit: CreditOf<T>) {}

	/// Withdraw some fee to pay for storage deposits.
	fn withdraw_txfee(_amount: BalanceOf<T>) -> Option<CreditOf<T>> {
		Default::default()
	}

	/// Return the remaining transaction fee.
	fn remaining_txfee() -> BalanceOf<T> {
		Default::default()
	}

	/// Compute the actual post_dispatch fee
	fn compute_actual_fee(
		_encoded_len: u32,
		_info: &DispatchInfo,
		_result: &DispatchResultWithPostInfo,
	) -> BalanceOf<T> {
		Default::default()
	}
}

impl<const P: u128, const Q: u128, T: SysConfig, B> BlockRatioFee<P, Q, T, B> {
	const REF_TIME_TO_FEE: FixedU128 = {
		assert!(P > 0 && Q > 0);
		FixedU128::from_rational(P, Q)
	};

	/// The proof_size to fee coefficient.
	fn proof_size_to_fee() -> FixedU128 {
		let max_weight = <T as SysConfig>::BlockWeights::get().max_block;
		let ratio =
			FixedU128::from_rational(max_weight.ref_time().into(), max_weight.proof_size().into());
		Self::REF_TIME_TO_FEE.saturating_mul(ratio)
	}
}

impl<const P: u128, const Q: u128, T, B> WeightToFee for BlockRatioFee<P, Q, T, B>
where
	T: SysConfig,
	B: Balance,
{
	type Balance = B;

	fn weight_to_fee(weight: &Weight) -> Self::Balance {
		let ref_time_fee = Self::REF_TIME_TO_FEE
			.saturating_mul_int(Self::Balance::saturated_from(weight.ref_time()));
		let proof_size_fee = Self::proof_size_to_fee()
			.saturating_mul_int(Self::Balance::saturated_from(weight.proof_size()));
		ref_time_fee.max(proof_size_fee)
	}
}

impl<const P: u128, const Q: u128, Address, Signature, E: EthExtra> InfoT<E::Config>
	for Info<Address, Signature, E>
where
	E::Config: TxConfig<WeightToFee = BlockRatioFee<P, Q, E::Config, BalanceOf<E::Config>>>,
	BalanceOf<E::Config>: From<OnChargeTransactionBalanceOf<E::Config>>,
	<E::Config as frame_system::Config>::RuntimeCall:
		Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
	CallOf<E::Config>: SetWeightLimit,
	<<E::Config as SysConfig>::Block as BlockT>::Extrinsic: From<
		UncheckedExtrinsic<
			Address,
			CallOf<E::Config>,
			Signature,
			E::ExtensionV0,
			E::ExtensionOtherVersions,
		>,
	>,
	<<E::Config as TxConfig>::OnChargeTransaction as TxCreditHold<E::Config>>::Credit:
		SuppressedDrop<Inner = CreditOf<E::Config>>,
{
	fn integrity_test() {
		let min_multiplier = <E::Config as TxConfig>::FeeMultiplierUpdate::min();
		assert!(!min_multiplier.is_zero(), "The multiplier is never allowed to be zero.");
		assert!(
			min_multiplier.saturating_mul_int(<E::Config as Config>::NativeToEthRatio::get()) > 0,
			"The gas price needs to be greater zero."
		);
		assert!(
			!<E::Config as TxConfig>::WeightToFee::REF_TIME_TO_FEE.is_zero(),
			"ref_time to fee is not allowed to be zero."
		);
		assert!(
			!<E::Config as TxConfig>::WeightToFee::proof_size_to_fee().is_zero(),
			"proof_size to fee is not allowed to be zero."
		);
	}

	fn next_fee_multiplier() -> FixedU128 {
		<NextFeeMultiplier<E::Config>>::get()
	}

	fn tx_fee(len: u32, call: &CallOf<E::Config>) -> BalanceOf<E::Config> {
		let dispatch_info = Self::dispatch_info(call);
		TxPallet::<E::Config>::compute_fee(len, &dispatch_info, 0u32.into()).into()
	}

	/// Calculate the fee using the weight instead of a dispatch info.
	fn tx_fee_from_weight(encoded_len: u32, weight: &Weight) -> BalanceOf<E::Config> {
		let fixed_fee = Self::fixed_fee(encoded_len);
		let weight_fee =
			Self::next_fee_multiplier().saturating_mul_int(Self::weight_to_fee(weight));
		fixed_fee.saturating_add(weight_fee)
	}

	fn fixed_fee(encoded_len: u32) -> BalanceOf<E::Config> {
		Self::weight_to_fee(
			&<E::Config as frame_system::Config>::BlockWeights::get()
				.get(DispatchClass::Normal)
				.base_extrinsic,
		)
		.saturating_add(Self::length_to_fee(encoded_len))
	}

	fn ensure_not_overdrawn(
		fee: BalanceOf<E::Config>,
		result: DispatchResultWithPostInfo,
	) -> DispatchResultWithPostInfo {
		// if tx is already failing we can ignore
		// as it will be rolled back anyways
		let Ok(post_info) = result else {
			return result;
		};

		let available = Self::remaining_txfee();
		if fee > available {
			log::debug!(target: LOG_TARGET, "Drew too much from the txhold. \
				fee={fee:?} \
				available={available:?} \
				overdrawn_by={:?}",
				fee.saturating_sub(available),
			);
			Err(DispatchErrorWithPostInfo {
				post_info,
				error: <Error<E::Config>>::TxFeeOverdraw.into(),
			})
		} else {
			log::trace!(target: LOG_TARGET, "Enough left in the txhold. \
				fee={fee:?} \
				available={available:?} \
				refund={:?}",
				available.saturating_sub(fee),
			);
			result
		}
	}

	fn dispatch_info(call: &CallOf<E::Config>) -> DispatchInfo {
		let mut dispatch_info = call.get_dispatch_info();
		dispatch_info.extension_weight =
			E::get_eth_extension(0u32.into(), 0u32.into()).weight(call);
		dispatch_info
	}

	fn base_dispatch_info(call: &mut CallOf<E::Config>) -> DispatchInfo {
		let pre_weight = call.set_weight_limit(Zero::zero());
		let info = Self::dispatch_info(call);
		call.set_weight_limit(pre_weight);
		info
	}

	fn encoded_len(eth_transact_call: CallOf<E::Config>) -> u32 {
		let uxt: <<E::Config as SysConfig>::Block as BlockT>::Extrinsic =
			UncheckedExtrinsic::new_bare(eth_transact_call).into();
		uxt.encoded_size() as u32
	}

	fn weight_to_fee(weight: &Weight) -> BalanceOf<E::Config> {
		<E::Config as TxConfig>::WeightToFee::weight_to_fee(weight)
	}

	// Convert a weight to an unadjusted fee using an average instead of maximum.
	fn weight_to_fee_average(weight: &Weight) -> BalanceOf<E::Config> {
		let ref_time_part = <E::Config as TxConfig>::WeightToFee::REF_TIME_TO_FEE
			.saturating_mul_int(weight.ref_time());
		let proof_size_part = <E::Config as TxConfig>::WeightToFee::proof_size_to_fee()
			.saturating_mul_int(weight.proof_size());

		// saturated addition not required here but better to be defensive
		((ref_time_part / 2).saturating_add(proof_size_part / 2)).saturated_into()
	}

	/// Convert an unadjusted fee back to a weight.
	fn fee_to_weight(fee: BalanceOf<E::Config>) -> Weight {
		let ref_time_to_fee = <E::Config as TxConfig>::WeightToFee::REF_TIME_TO_FEE;
		let proof_size_to_fee = <E::Config as TxConfig>::WeightToFee::proof_size_to_fee();

		let (ref_time, proof_size) =
			compute_max_integer_pair_quotient((ref_time_to_fee, proof_size_to_fee), fee);

		Weight::from_parts(ref_time.saturated_into(), proof_size.saturated_into())
	}

	fn length_to_fee(len: u32) -> BalanceOf<E::Config> {
		TxPallet::<E::Config>::length_to_fee(len).into()
	}

	fn deposit_txfee(credit: CreditOf<E::Config>) {
		TxPallet::<E::Config>::deposit_txfee(credit)
	}

	fn withdraw_txfee(amount: BalanceOf<E::Config>) -> Option<CreditOf<E::Config>> {
		TxPallet::<E::Config>::withdraw_txfee(amount)
	}

	fn remaining_txfee() -> BalanceOf<E::Config> {
		TxPallet::<E::Config>::remaining_txfee()
	}

	fn compute_actual_fee(
		encoded_len: u32,
		info: &DispatchInfo,
		result: &DispatchResultWithPostInfo,
	) -> BalanceOf<E::Config> {
		let mut post_info = *match result {
			Ok(post_info) => post_info,
			Err(err) => &err.post_info,
		};

		post_info.set_extension_weight(info);
		<TxPallet<E::Config>>::compute_actual_fee(encoded_len, info, &post_info, Zero::zero())
			.into()
	}
}

impl<T: Config> InfoT<T> for () {}

mod seal {
	pub trait Sealed {}
	impl<Address, Signature, E: super::EthExtra> Sealed for super::Info<Address, Signature, E> {}
	impl Sealed for () {}
}

/// Determine the maximal integer `n` so that `multiplier.saturating_mul_int(n) <= product`
///
/// See the tests `compute_max_quotient_works` below for an example why simple division does not
/// give the correct result. This level of pedantry is required because otherwise we observed actual
/// cases where limits where calculated incorrectly and the transaction ran out of gas although it
/// used the correct gas estimate.
///
/// FixedU128 wraps a 128 bit unsigned integer `self.0` and it is interpreted to represent the real
/// number self.0 / FixedU128::DIV, where FixedU128::DIV is 1_000_000_000_000_000_000.
///
/// Given an integer `n`, the operation `multiplier.saturating_mul_int(n)` is defined as
///      `div_round_down(multiplier.0 * n, FixedU128::DIV)`
/// where `div_round_down` is integer division where the result is rounded down.
///
/// To determine the maximal integer `n` so that `multiplier.saturating_mul_int(n) <= product` is
/// therefore equivalent to determining the maximal `n` such that
///      `div_round_down(multiplier.0 * n, FixedU128::DIV) <= product`
/// This is equivalent to the condition
///      `multiplier.0 * n <= product * FixedU128::DIV + FixedU128::DIV - 1`
/// This is equivalent to
///      `multiplier.0 * n < (product + 1) * FixedU128::DIV`
/// This is equivalent to
///      `n < div_round_up((product + 1) * FixedU128::DIV, multiplier.0)`
/// where `div_round_up` is integer division where the result is rounded up.
/// Since we look for a maximal `n` with this condition, the result is
///      `n = div_round_up((product + 1) * FixedU128::DIV, multiplier.0) - 1`.
///
/// We can take advantage of the function `FixedU128::checked_rounding_div`, which, given two fixed
/// point numbers `a` and `b`, just computes `a.0 * FixedU128::DIV / b.0`. It also allows to specify
/// the rounding mode `SignedRounding::Major`, which means that the result of the division is
/// rounded up.
pub fn compute_max_integer_quotient<F: FixedPointOperand + One>(
	multiplier: FixedU128,
	product: F,
) -> F {
	let one = F::one();
	let product_plus_one = FixedU128::from_inner(product.saturating_add(one).saturated_into());

	product_plus_one
		.checked_rounding_div(multiplier, SignedRounding::Major)
		.map(|f| f.into_inner().saturated_into::<F>().saturating_sub(one))
		.unwrap_or(F::max_value())
}

/// same as compute_max_integer_quotient but applied to a pair
pub fn compute_max_integer_pair_quotient<F: FixedPointOperand + One>(
	multiplier: (FixedU128, FixedU128),
	product: F,
) -> (F, F) {
	let one = F::one();
	let product_plus_one = FixedU128::from_inner(product.saturating_add(one).saturated_into());

	let result1 = product_plus_one
		.checked_rounding_div(multiplier.0, SignedRounding::Major)
		.map(|f| f.into_inner().saturated_into::<F>().saturating_sub(one))
		.unwrap_or(F::max_value());

	let result2 = product_plus_one
		.checked_rounding_div(multiplier.1, SignedRounding::Major)
		.map(|f| f.into_inner().saturated_into::<F>().saturating_sub(one))
		.unwrap_or(F::max_value());

	(result1, result2)
}

#[cfg(test)]
mod tests {
	use super::*;
	use proptest::proptest;

	#[test]
	fn compute_max_quotient_works() {
		let product1 = 8625031518u64;
		let product2 = 2597808837u64;

		let multiplier = FixedU128::from_rational(4_000_000_000_000, 10 * 1024 * 1024);

		assert_eq!(compute_max_integer_quotient(multiplier, product1), 22610);
		assert_eq!(compute_max_integer_quotient(multiplier, product2), 6810);

		// This shows that just dividing by the multiplier does not give the correct result, neither
		// when rounding up, nor when rounding down
		assert_eq!(multiplier.reciprocal().unwrap().saturating_mul_int(product1), 22610);
		assert_eq!(multiplier.reciprocal().unwrap().saturating_mul_int(product2), 6809);
	}

	#[test]
	fn proptest_max_quotient_works() {
		proptest!(|(numerator: u128, denominator: u128, product: u128)| {
			let multiplier = FixedU128::from_rational(numerator.saturating_add(1), denominator.saturating_add(1));
			let max_quotient = compute_max_integer_quotient(multiplier, product);

			assert!(multiplier.saturating_mul_int(max_quotient) <= product);
			if max_quotient < u128::MAX {
				assert!(multiplier.saturating_mul_int(max_quotient + 1) > product);
			}
		});
	}

	#[test]
	fn proptest_max_pair_quotient_works() {
		proptest!(|(numerator1: u128, denominator1: u128, numerator2: u128, denominator2: u128, product: u128)| {
			let multiplier1 = FixedU128::from_rational(numerator1.saturating_add(1), denominator1.saturating_add(1));
			let multiplier2 = FixedU128::from_rational(numerator2.saturating_add(1), denominator2.saturating_add(1));
			let (max_quotient1, max_quotient2) = compute_max_integer_pair_quotient((multiplier1, multiplier2), product);

			assert!(multiplier1.saturating_mul_int(max_quotient1) <= product);
			if max_quotient1 < u128::MAX {
				assert!(multiplier1.saturating_mul_int(max_quotient1 + 1) > product);
			}

			assert!(multiplier2.saturating_mul_int(max_quotient2) <= product);
			if max_quotient2 < u128::MAX {
				assert!(multiplier2.saturating_mul_int(max_quotient2 + 1) > product);
			}
		});
	}
}
