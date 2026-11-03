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

use crate::WeightInfo;
use frame_support::weights::Weight;

pub trait WeightInfoExt {
	/// Returns the fixed base cost of finalize_block operations.
	///
	/// This represents the constant overhead incurred during `on_finalize()` regardless
	/// of order count. Includes setup costs, storage reads/writes, and other fixed operations.
	fn on_finalize_block_fixed() -> Weight;

	/// Returns the additional cost of finalize_block per placed order.
	fn on_finalize_block_per_order() -> Weight;
}

/// Implementation of `WeightInfoExt` that derives high-level weights from `WeightInfo`
/// benchmarks.
///
/// This implementation solves the linear dependency problem by splitting the finalization weight
/// into a fixed part and a part linearly growing with the number of orders.
///
/// **Weight Formula:**
/// ```text
/// Total weight = fixed_part + n_orders * per_order_part(payload_i)
/// ```
///
/// **Benchmark Sources:**
/// - Fixed cost: `on_finalize_with_orders(0)`
/// - Per-order cost: `on_finalize_with_orders(1) - on_finalize_per_transaction(0)`
///
/// Uses differential calculation to isolate marginal costs from benchmark measurements.
impl<W: WeightInfo> WeightInfoExt for W {
	fn on_finalize_block_fixed() -> Weight {
		// The constant factor in linearly growing weight of finalization of the block.
		W::on_finalize_with_orders(0)
	}

	fn on_finalize_block_per_order() -> Weight {
		W::on_finalize_with_orders(1).saturating_sub(W::on_finalize_with_orders(0))
	}
}
