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

#![cfg(feature = "runtime-benchmarks")]

use super::*;

use crate::{Pallet as OnDemand, DEFAULT_BASE_FEE, DEFAULT_PRICE_STEP};
use frame_benchmarking::v2::*;
use frame_support::{
	pallet_prelude::*,
	traits::{
		fungible::{Inspect, Mutate},
		Hooks,
	},
};
use frame_system::{pallet_prelude::*, Pallet as System, RawOrigin};
use sp_runtime::{Perbill, Saturating};

#[benchmarks]
mod benches {
	use super::*;
	#[cfg(not(feature = "std"))]
	use num_traits::float::FloatCore;

	#[benchmark]
	fn configure() -> Result<(), BenchmarkError> {
		let config = PriceParameters {
			order_cap: 80,
			drain_rate_per_block: 2,
			price_step: Perbill::from_percent(2),
			base_fee: BalanceOf::<T>::from(1_000_000u32),
		};

		let origin =
			T::AdminOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, config.clone());

		assert_eq!(PriceConfig::<T>::get(), config);

		Ok(())
	}

	#[benchmark]
	fn place_order() -> Result<(), BenchmarkError> {
		let current_block = BlockNumberFor::<T>::from(1u32);
		System::<T>::set_block_number(current_block);

		let base_fee = BalanceOf::<T>::from(DEFAULT_BASE_FEE);

		let caller: T::AccountId = whitelisted_caller();
		T::Currency::set_balance(
			&caller.clone(),
			T::Currency::minimum_balance().saturating_add(base_fee),
		);
		// Store minimum balance in the pallet's account, so that small transfers to it don't fail
		T::Currency::set_balance(&OnDemand::<T>::account_id(), T::Currency::minimum_balance());

		let _ = OnDemand::<T>::on_initialize(current_block);

		#[block]
		{
			OnDemand::<T>::place_order(
				T::RuntimeOrigin::from(RawOrigin::Signed(caller.clone())),
				2000,
				base_fee,
			)
			.map_err(|_| BenchmarkError::Stop("place_order failed"))?;
		}

		Ok(())
	}

	/// Benchmark the `on_finalize` hook scaling with number of orders.
	///
	/// This benchmark measures the marginal computational cost of adding orders
	/// to a block during finalization.
	///
	/// We do not benchmark with zero orders, since that would skew the results by not generating an
	/// outgoing message. We want the linear part to reflect the additional overhead of processing
	/// more orders, and the fixed part to reflect sending a message - so we need all cases being
	/// benchmarked to send exactly one message.
	///
	/// ## Parameters:
	/// - `n`: Number of transactions in the block (1-100)
	///
	/// ## Test Setup:
	/// - Initializes the account balance with enough funds to cover all orders.
	/// - Places `n` orders.
	#[benchmark(pov_mode = Measured)]
	fn on_finalize_with_orders(n: Linear<1, 100>) -> Result<(), BenchmarkError> {
		let current_block = BlockNumberFor::<T>::from(1u32);
		System::<T>::set_block_number(current_block);

		let base_fee = BalanceOf::<T>::from(DEFAULT_BASE_FEE);
		let step = DEFAULT_PRICE_STEP as f32 / 100.0;

		// k-th order's price will be base_fee * (1 + step)^k
		// Thus, the cost of n orders will be the base_fee times 1 + x + x^2 + ... + x^(n-1),
		// where x = 1 + step. Such a sum equals (x^n - 1) / (x - 1), which in our case gives
		// ((1 + step)^n - 1) / step.
		// We add n to account for possible rounding errors at each order.
		let multiplier = ((1.0 + step).powi(n as i32) - 1.0) / step + n as f32;

		let required_amount = base_fee
			.checked_mul(&BalanceOf::<T>::from(multiplier.ceil() as u32))
			.expect("the price of n orders should fit within the balance type");

		let max_price = base_fee
			.checked_mul(&BalanceOf::<T>::from((1.0 + step).powi(n as i32).ceil() as u32))
			.expect("maximum price should fit within the balance type");

		let caller: T::AccountId = whitelisted_caller();
		T::Currency::set_balance(
			&caller.clone(),
			T::Currency::minimum_balance().saturating_add(required_amount),
		);
		// Store minimum balance in the pallet's account, so that small transfers to it don't fail
		T::Currency::set_balance(&OnDemand::<T>::account_id(), T::Currency::minimum_balance());

		// Pre-populate InflightTransactions with n transactions of fixed size
		if n > 0 {
			// Initialize block
			let _ = OnDemand::<T>::on_initialize(current_block);

			for _ in 0..n {
				OnDemand::<T>::place_order(
					T::RuntimeOrigin::from(RawOrigin::Signed(caller.clone())),
					From::from(2000 + n),
					max_price,
				)
				.map_err(|_| BenchmarkError::Stop("place_order failed"))?;
			}
		}

		#[block]
		{
			// Measure only the finalization cost with n transactions of fixed size
			let _ = OnDemand::<T>::on_finalize(current_block);
		}

		Ok(())
	}

	// Implements a test for each benchmark. Execute with:
	// `cargo test -p pallet-on-demand-para --features runtime-benchmarks`.
	impl_benchmark_test_suite!(OnDemand, crate::mock::new_test_ext(), crate::mock::Test);
}
