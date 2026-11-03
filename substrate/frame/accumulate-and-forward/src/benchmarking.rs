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

//! Benchmarks for pallet-accumulate-and-forward.

use super::*;
use frame_benchmarking::v2::*;
use frame_support::traits::fungible::Unbalanced;

#[benchmarks]
mod benchmarks {
	use super::*;

	/// Benchmark for [`Forwarder::forward`].
	///
	/// This measures the full cost of an accumulation-account-to-destination transfer.
	#[benchmark]
	fn send_native() {
		let accumulation_account = Pallet::<T>::accumulation_account();
		let ed = T::Currency::minimum_balance();
		let amount = T::MinTransferAmount::get();

		// Fund with ED (to keep account alive) plus the amount to be sent.
		T::Currency::write_balance(&accumulation_account, ed + amount)
			.expect("benchmark setup should succeed");

		#[block]
		{
			let _ = T::Forwarder::forward(accumulation_account, amount);
		}
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(true), crate::mock::Test);
}
