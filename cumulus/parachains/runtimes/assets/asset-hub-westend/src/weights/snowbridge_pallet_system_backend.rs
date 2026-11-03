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

#![cfg_attr(rustfmt, rustfmt_skip)]
#![allow(unused_parens)]
#![allow(unused_imports)]
#![allow(missing_docs)]

use frame_support::{traits::Get, weights::Weight};
use core::marker::PhantomData;

/// Weight functions for `snowbridge_pallet_system_frontend::BackendWeightInfo`.
/// Copy the weight generated for `fn register_token() -> Weight` from
/// ../../../../bridge-hubs/bridge-hub-westend/src/weights/snowbridge_pallet_system_v2.rs
pub struct WeightInfo<T>(PhantomData<T>);
impl<T: frame_system::Config> snowbridge_pallet_system_frontend::BackendWeightInfo for WeightInfo<T> {
	fn transact_register_token() -> Weight {
		Weight::from_parts(45_000_000, 6044)
			.saturating_add(T::DbWeight::get().reads(5_u64))
			.saturating_add(T::DbWeight::get().writes(3_u64))
	}

	fn transact_add_tip() -> Weight {
		Weight::from_parts(45_000_000, 6044)
			.saturating_add(T::DbWeight::get().reads(5_u64))
			.saturating_add(T::DbWeight::get().writes(3_u64))
	}

	fn do_process_message() -> Weight {
		Weight::from_parts(23_370_000, 0)
			.saturating_add(Weight::from_parts(0, 1493))
			.saturating_add(T::DbWeight::get().reads(3))
			.saturating_add(T::DbWeight::get().writes(4))
	}

	fn commit_single() -> Weight {
		Weight::from_parts(11_263_000, 0)
			.saturating_add(Weight::from_parts(0, 1582))
			.saturating_add(T::DbWeight::get().reads(1))
			.saturating_add(T::DbWeight::get().writes(1))
	}

	fn submit_delivery_receipt() -> Weight {
		Weight::from_parts(99_542_000, 0)
			.saturating_add(Weight::from_parts(0, 3756))
			.saturating_add(T::DbWeight::get().reads(5))
			.saturating_add(T::DbWeight::get().writes(1))
	}
}
