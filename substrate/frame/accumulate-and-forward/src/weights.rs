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

//! Placeholder weights for `pallet_accumulate_and_forward`.
//!
//! These weights are not benchmarked. Replace with actual benchmarked weights
//! via `frame-omni-bencher` before deploying to production.

#![cfg_attr(rustfmt, rustfmt_skip)]
#![allow(unused_parens)]
#![allow(unused_imports)]
#![allow(missing_docs)]

use frame_support::weights::Weight;

/// Weight functions needed for `pallet_accumulate_and_forward`.
pub trait WeightInfo {
	fn send_native() -> Weight;
}

/// Default weights (not benchmarked).
impl WeightInfo for () {
	fn send_native() -> Weight {
		Weight::zero()
	}
}
