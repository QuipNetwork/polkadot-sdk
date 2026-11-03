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

//! Contains the `WeightMeter` primitive to meter weight usage.

use super::Weight;

use sp_arithmetic::Perbill;

/// Meters consumed weight and a hard limit for the maximal consumable weight.
///
/// Can be used to check if enough weight for an operation is available before committing to it.
///
/// # Example
///
/// ```rust
/// use sp_weights::{Weight, WeightMeter};
///
/// // The weight is limited to (10, 0).
/// let mut meter = WeightMeter::with_limit(Weight::from_parts(10, 0));
/// // There is enough weight remaining for an operation with (6, 0) weight.
/// assert!(meter.try_consume(Weight::from_parts(6, 0)).is_ok());
/// assert_eq!(meter.remaining(), Weight::from_parts(4, 0));
/// // There is not enough weight remaining for an operation with (5, 0) weight.
/// assert!(!meter.try_consume(Weight::from_parts(5, 0)).is_ok());
/// // The total limit is obviously unchanged:
/// assert_eq!(meter.limit(), Weight::from_parts(10, 0));
/// ```
#[derive(Debug, Clone)]
pub struct WeightMeter {
	/// The already consumed weight.
	consumed: Weight,

	/// The maximal consumable weight.
	limit: Weight,
}

impl WeightMeter {
	/// Creates [`Self`] from `consumed` and `limit`.
	pub fn with_consumed_and_limit(consumed: Weight, limit: Weight) -> Self {
		Self { consumed, limit }
	}

	/// Creates [`Self`] from a limit for the maximal consumable weight.
	pub fn with_limit(limit: Weight) -> Self {
		Self { consumed: Weight::zero(), limit }
	}

	/// Creates [`Self`] with the maximal possible limit for the consumable weight.
	pub fn new() -> Self {
		Self::with_limit(Weight::MAX)
	}

	/// Change the limit to the given `weight`.
	///
	/// The actual weight will be determined by `min(weight, self.remaining())`.
	pub fn limit_to(self, weight: Weight) -> Self {
		Self::with_limit(self.remaining().min(weight))
	}

	/// The already consumed weight.
	pub fn consumed(&self) -> Weight {
		self.consumed
	}

	/// The limit can ever be accrued.
	pub fn limit(&self) -> Weight {
		self.limit
	}

	/// The remaining weight that can still be consumed.
	pub fn remaining(&self) -> Weight {
		self.limit.saturating_sub(self.consumed)
	}

	/// The ratio of consumed weight to the limit.
	///
	/// Calculates one ratio per component and returns the largest.
	///
	/// # Example
	/// ```rust
	/// use sp_weights::{Weight, WeightMeter};
	/// use sp_arithmetic::Perbill;
	///
	/// let mut meter = WeightMeter::with_limit(Weight::from_parts(10, 20));
	/// // Nothing consumed so far:
	/// assert_eq!(meter.consumed_ratio(), Perbill::from_percent(0));
	/// meter.consume(Weight::from_parts(5, 5));
	/// // The ref-time is the larger ratio:
	/// assert_eq!(meter.consumed_ratio(), Perbill::from_percent(50));
	/// meter.consume(Weight::from_parts(1, 10));
	/// // Now the larger ratio is proof-size:
	/// assert_eq!(meter.consumed_ratio(), Perbill::from_percent(75));
	/// // Eventually it reaches 100%:
	/// meter.consume(Weight::from_parts(4, 0));
	/// assert_eq!(meter.consumed_ratio(), Perbill::from_percent(100));
	/// // Saturating the second component won't change anything anymore:
	/// meter.consume(Weight::from_parts(0, 5));
	/// assert_eq!(meter.consumed_ratio(), Perbill::from_percent(100));
	/// ```
	pub fn consumed_ratio(&self) -> Perbill {
		let time = Perbill::from_rational(self.consumed.ref_time(), self.limit.ref_time());
		let pov = Perbill::from_rational(self.consumed.proof_size(), self.limit.proof_size());
		time.max(pov)
	}

	/// Consume some weight and defensively fail if it is over the limit. Saturate in any case.
	pub fn consume(&mut self, w: Weight) {
		self.consumed.saturating_accrue(w);
		debug_assert!(self.consumed.all_lte(self.limit), "Weight counter overflow");
	}

	/// Consume the given weight after checking that it can be consumed.
	///
	/// Returns `Ok` if the weight can be consumed or otherwise an `Err`.
	pub fn try_consume(&mut self, w: Weight) -> Result<(), ()> {
		self.consumed.checked_add(&w).map_or(Err(()), |test| {
			if test.any_gt(self.limit) {
				Err(())
			} else {
				self.consumed = test;
				Ok(())
			}
		})
	}

	/// Check if the given weight can be consumed.
	pub fn can_consume(&self, w: Weight) -> bool {
		self.consumed.checked_add(&w).map_or(false, |t| t.all_lte(self.limit))
	}

	/// Reclaim the given weight.
	pub fn reclaim_proof_size(&mut self, s: u64) {
		self.consumed.saturating_reduce(Weight::from_parts(0, s));
	}
}

#[cfg(test)]
mod tests {
	use crate::*;
	use sp_arithmetic::traits::Zero;

	#[test]
	fn weight_meter_remaining_works() {
		let mut meter = WeightMeter::with_limit(Weight::from_parts(10, 20));

		assert_eq!(meter.try_consume(Weight::from_parts(5, 0)), Ok(()));
		assert_eq!(meter.consumed, Weight::from_parts(5, 0));
		assert_eq!(meter.remaining(), Weight::from_parts(5, 20));

		assert_eq!(meter.try_consume(Weight::from_parts(2, 10)), Ok(()));
		assert_eq!(meter.consumed, Weight::from_parts(7, 10));
		assert_eq!(meter.remaining(), Weight::from_parts(3, 10));

		assert_eq!(meter.try_consume(Weight::from_parts(3, 10)), Ok(()));
		assert_eq!(meter.consumed, Weight::from_parts(10, 20));
		assert_eq!(meter.remaining(), Weight::from_parts(0, 0));
	}

	#[test]
	fn weight_meter_can_consume_works() {
		let meter = WeightMeter::with_limit(Weight::from_parts(1, 1));

		assert!(meter.can_consume(Weight::from_parts(0, 0)));
		assert!(meter.can_consume(Weight::from_parts(1, 1)));
		assert!(!meter.can_consume(Weight::from_parts(0, 2)));
		assert!(!meter.can_consume(Weight::from_parts(2, 0)));
		assert!(!meter.can_consume(Weight::from_parts(2, 2)));
	}

	#[test]
	fn weight_meter_try_consume_works() {
		let mut meter = WeightMeter::with_limit(Weight::from_parts(2, 2));

		assert_eq!(meter.try_consume(Weight::from_parts(0, 0)), Ok(()));
		assert_eq!(meter.try_consume(Weight::from_parts(1, 1)), Ok(()));
		assert_eq!(meter.try_consume(Weight::from_parts(0, 2)), Err(()));
		assert_eq!(meter.try_consume(Weight::from_parts(2, 0)), Err(()));
		assert_eq!(meter.try_consume(Weight::from_parts(2, 2)), Err(()));
		assert_eq!(meter.try_consume(Weight::from_parts(0, 1)), Ok(()));
		assert_eq!(meter.try_consume(Weight::from_parts(1, 0)), Ok(()));
	}

	#[test]
	fn weight_meter_check_and_can_consume_works() {
		let mut meter = WeightMeter::new();

		assert!(meter.can_consume(Weight::from_parts(u64::MAX, 0)));
		assert_eq!(meter.try_consume(Weight::from_parts(u64::MAX, 0)), Ok(()));

		assert!(meter.can_consume(Weight::from_parts(0, u64::MAX)));
		assert_eq!(meter.try_consume(Weight::from_parts(0, u64::MAX)), Ok(()));

		assert!(!meter.can_consume(Weight::from_parts(0, 1)));
		assert_eq!(meter.try_consume(Weight::from_parts(0, 1)), Err(()));

		assert!(!meter.can_consume(Weight::from_parts(1, 0)));
		assert_eq!(meter.try_consume(Weight::from_parts(1, 0)), Err(()));

		assert!(meter.can_consume(Weight::zero()));
		assert_eq!(meter.try_consume(Weight::zero()), Ok(()));
	}

	#[test]
	fn consumed_ratio_works() {
		let mut meter = WeightMeter::with_limit(Weight::from_parts(10, 20));

		assert_eq!(meter.try_consume(Weight::from_parts(5, 0)), Ok(()));
		assert_eq!(meter.consumed_ratio(), Perbill::from_percent(50));
		assert_eq!(meter.try_consume(Weight::from_parts(0, 12)), Ok(()));
		assert_eq!(meter.consumed_ratio(), Perbill::from_percent(60));

		assert_eq!(meter.try_consume(Weight::from_parts(2, 0)), Ok(()));
		assert_eq!(meter.consumed_ratio(), Perbill::from_percent(70));
		assert_eq!(meter.try_consume(Weight::from_parts(0, 4)), Ok(()));
		assert_eq!(meter.consumed_ratio(), Perbill::from_percent(80));

		assert_eq!(meter.try_consume(Weight::from_parts(3, 0)), Ok(()));
		assert_eq!(meter.consumed_ratio(), Perbill::from_percent(100));
		assert_eq!(meter.try_consume(Weight::from_parts(0, 4)), Ok(()));
		assert_eq!(meter.consumed_ratio(), Perbill::from_percent(100));
	}

	#[test]
	fn try_consume_works() {
		let mut meter = WeightMeter::with_limit(Weight::from_parts(10, 0));

		assert!(meter.try_consume(Weight::from_parts(11, 0)).is_err());
		assert!(meter.consumed().is_zero(), "No modification");

		assert!(meter.try_consume(Weight::from_parts(9, 0)).is_ok());
		assert!(meter.try_consume(Weight::from_parts(2, 0)).is_err());
		assert!(meter.try_consume(Weight::from_parts(1, 0)).is_ok());
		assert!(meter.remaining().is_zero());
		assert_eq!(meter.consumed(), Weight::from_parts(10, 0));
	}

	#[test]
	fn can_consume_works() {
		let mut meter = WeightMeter::with_limit(Weight::from_parts(10, 0));

		assert!(!meter.can_consume(Weight::from_parts(11, 0)));
		assert!(meter.consumed().is_zero(), "No modification");

		assert!(meter.can_consume(Weight::from_parts(9, 0)));
		meter.consume(Weight::from_parts(9, 0));
		assert!(!meter.can_consume(Weight::from_parts(2, 0)));
		assert!(meter.can_consume(Weight::from_parts(1, 0)));
	}

	#[test]
	#[cfg(debug_assertions)]
	fn consume_works() {
		let mut meter = WeightMeter::with_limit(Weight::from_parts(5, 10));

		meter.consume(Weight::from_parts(4, 0));
		assert_eq!(meter.remaining(), Weight::from_parts(1, 10));
		meter.consume(Weight::from_parts(1, 0));
		assert_eq!(meter.remaining(), Weight::from_parts(0, 10));
		meter.consume(Weight::from_parts(0, 10));
		assert_eq!(meter.consumed(), Weight::from_parts(5, 10));
	}

	#[test]
	#[cfg(debug_assertions)]
	fn reclaim_works() {
		let mut meter = WeightMeter::with_limit(Weight::from_parts(5, 10));

		meter.consume(Weight::from_parts(5, 10));
		assert_eq!(meter.consumed(), Weight::from_parts(5, 10));

		meter.reclaim_proof_size(3);
		assert_eq!(meter.consumed(), Weight::from_parts(5, 7));

		meter.reclaim_proof_size(10);
		assert_eq!(meter.consumed(), Weight::from_parts(5, 0));
	}

	#[test]
	#[cfg(debug_assertions)]
	#[should_panic(expected = "Weight counter overflow")]
	fn consume_defensive_fail() {
		let mut meter = WeightMeter::with_limit(Weight::from_parts(10, 0));
		let _ = meter.consume(Weight::from_parts(11, 0));
	}
}
