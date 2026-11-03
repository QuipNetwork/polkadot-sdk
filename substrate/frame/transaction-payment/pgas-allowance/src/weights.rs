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

//! Weights for `pallet_pgas_allowance`.

#![allow(unused_parens)]
#![allow(unused_imports)]
#![allow(missing_docs)]

use core::marker::PhantomData;
use frame_support::{
	traits::Get,
	weights::{Weight, constants::RocksDbWeight},
};

/// Weight functions needed for `pallet_pgas_allowance`.
pub trait WeightInfo {
	/// Full PGAS path: validate, withdraw into a credit and resolve refund.
	fn charge_pgas() -> Weight;
	/// PGAS path is skipped (unsigned, filter miss, or insufficient PGAS balance).
	fn charge_pgas_skip() -> Weight;
}

/// Weights for `pallet_pgas_allowance` using the Substrate node and recommended hardware.
pub struct SubstrateWeight<T>(PhantomData<T>);
impl<T: frame_system::Config> WeightInfo for SubstrateWeight<T> {
	fn charge_pgas() -> Weight {
		Weight::from_parts(45_000_000, 3675)
			.saturating_add(T::DbWeight::get().reads(5_u64))
			.saturating_add(T::DbWeight::get().writes(2_u64))
	}
	fn charge_pgas_skip() -> Weight {
		Weight::from_parts(1_000_000, 0).saturating_add(T::DbWeight::get().reads(1_u64))
	}
}

// For backwards compatibility and tests.
impl WeightInfo for () {
	fn charge_pgas() -> Weight {
		Weight::from_parts(45_000_000, 3675)
			.saturating_add(RocksDbWeight::get().reads(5_u64))
			.saturating_add(RocksDbWeight::get().writes(2_u64))
	}
	fn charge_pgas_skip() -> Weight {
		Weight::from_parts(1_000_000, 0).saturating_add(RocksDbWeight::get().reads(1_u64))
	}
}
