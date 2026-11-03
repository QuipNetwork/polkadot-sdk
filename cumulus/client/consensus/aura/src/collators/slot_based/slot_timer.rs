// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus. If not, see <https://www.gnu.org/licenses/>.

use crate::LOG_TARGET;
use cumulus_primitives_aura::Slot;
use sc_consensus_aura::SlotDuration;

use sp_timestamp::Timestamp;
use std::time::Duration;

#[derive(Debug)]
pub(crate) struct SlotInfo {
	pub timestamp: Timestamp,
	pub slot: Slot,
}

/// Information about a slot timing, including the relay chain slot duration and exact start
/// timestamp.
#[derive(Debug, Clone)]
pub(crate) struct SlotTime {
	/// The relay chain slot duration used for this timing
	relay_slot_duration: Duration,
	/// The exact timestamp when this relay chain slot started
	slot_start_timestamp: Timestamp,
	/// Time offset to apply when calculating time remaining
	time_offset: Duration,
}

impl SlotTime {
	/// Create a new SlotTime
	pub fn new(
		relay_slot_duration: Duration,
		slot_start_timestamp: Timestamp,
		time_offset: Duration,
	) -> Self {
		Self { relay_slot_duration, slot_start_timestamp, time_offset }
	}

	/// Get the time remaining in this slot
	pub fn time_left(&self) -> Duration {
		self.time_left_internal(Timestamp::current().as_duration())
	}

	/// Internal implementation of [`Self::time_left`] that takes `now` as parameter.
	fn time_left_internal(&self, now: Duration) -> Duration {
		let now = now.saturating_sub(self.time_offset);
		let slot_end_time_millis =
			self.slot_start_timestamp.as_millis() + self.relay_slot_duration.as_millis() as u64;
		let slot_end_time = Duration::from_millis(slot_end_time_millis);

		slot_end_time.saturating_sub(now)
	}

	/// Check if the next relay chain slot would be in a different parachain slot.
	pub fn is_parachain_slot_ending(&self, parachain_slot_duration: Duration) -> bool {
		let now = Timestamp::current().as_duration().saturating_sub(self.time_offset);
		let next_relay_slot_start_time =
			self.slot_start_timestamp.as_duration() + self.relay_slot_duration;

		// Calculate current parachain slot
		let current_parachain_slot = now.as_millis() / parachain_slot_duration.as_millis();

		// Calculate parachain slot for next relay slot
		let next_parachain_slot =
			next_relay_slot_start_time.as_millis() / parachain_slot_duration.as_millis() as u128;

		current_parachain_slot != next_parachain_slot
	}
}

/// Manages block-production slots based on the relay chain slot duration.
#[derive(Debug)]
pub(crate) struct SlotTimer {
	/// Offset the current time by this duration.
	time_offset: Duration,
	/// Slot duration of the relay chain. This is used to compute when to wake up for
	/// block production attempts.
	relay_slot_duration: Duration,
	/// Stores the latest slot that was reported by [`Self::wait_until_next_slot`].
	last_reported_slot: Option<Slot>,
}

/// Returns the duration until the next block production slot and the timestamp at this slot.
fn time_until_next_slot(
	now: Duration,
	block_production_interval: Duration,
	offset: Duration,
) -> (Duration, Timestamp) {
	let now = now.saturating_sub(offset).as_millis();

	let next_slot_time = ((now + block_production_interval.as_millis()) /
		block_production_interval.as_millis()) *
		block_production_interval.as_millis();
	let remaining_millis = next_slot_time - now;
	(Duration::from_millis(remaining_millis as u64), Timestamp::from(next_slot_time as u64))
}

impl SlotTimer {
	/// Create a new slot timer.
	pub fn new_with_offset(time_offset: Duration, relay_slot_duration: Duration) -> Self {
		Self { time_offset, relay_slot_duration, last_reported_slot: None }
	}

	/// Set the time offset depending on the scheduling version.
	pub fn set_offset_by_scheduling_version(&mut self, v3_enabled: bool, offset: Duration) {
		if v3_enabled {
			// Ignore the time offset when V3 scheduling is enabled,
			// since `descendants_start` already handles relay-chain slot alignment.
			self.time_offset = Duration::ZERO;
		} else {
			self.time_offset = offset;
		}
	}

	/// Returns a future that resolves when the next block production should be attempted.
	pub async fn wait_until_next_slot(&mut self) -> Result<SlotTime, ()> {
		let (time_until_next_attempt, timestamp) = time_until_next_slot(
			Timestamp::current().as_duration(),
			self.relay_slot_duration,
			self.time_offset,
		);

		// Calculate the current slot using the relay chain slot duration
		let relay_slot_duration_for_slot = SlotDuration::from(self.relay_slot_duration);
		let mut next_slot = Slot::from_timestamp(timestamp, relay_slot_duration_for_slot);

		// Calculate the actual slot start timestamp (may be different if we're catching up)
		let mut slot_start_timestamp = timestamp;

		match self.last_reported_slot {
			// If we already reported a slot, we don't want to skip a slot. But we also don't want
			// to go through all the slots if a node was halted for some reason.
			Some(ls) if ls + 1 < next_slot && next_slot <= ls + 3 => {
				next_slot = ls + 1u64;
				// Calculate the timestamp for the adjusted slot
				slot_start_timestamp =
					next_slot.timestamp(relay_slot_duration_for_slot).ok_or(())?;
				// Don't sleep since we're catching up
				tracing::debug!(
					target: LOG_TARGET,
					last_slot = ?ls,
					next_slot = ?next_slot,
					"Catching up on skipped slot."
				);
			},
			None | Some(_) => {
				tracing::trace!(
					target: LOG_TARGET,
					time_to_sleep = ?time_until_next_attempt,
					"Feeling sleepy 😴"
				);

				// Wake up slightly before the next slot to avoid noisy "catching up" logs caused by
				// scheduler jitter right at the slot boundary.
				tokio::time::sleep(
					time_until_next_attempt.saturating_sub(Duration::from_millis(2)),
				)
				.await;
			},
		}

		tracing::debug!(
			target: LOG_TARGET,
			relay_slot_duration = ?self.relay_slot_duration,
			?next_slot,
			?slot_start_timestamp,
			"New block production slot."
		);

		// Update internal slot tracking
		self.last_reported_slot = Some(next_slot);

		Ok(SlotTime::new(self.relay_slot_duration, slot_start_timestamp, self.time_offset))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use rstest::rstest;
	const RELAY_CHAIN_SLOT_DURATION: u64 = 6000;

	#[rstest]
	// Test that different now timestamps have correct impact
	#[case(1000, 0, 5000)]
	#[case(0, 0, 6000)]
	#[case(6000, 0, 6000)]
	// Test that offset affects the current time correctly
	#[case(1000, 1000, 6000)]
	#[case(12000, 2000, 2000)]
	#[case(12000, 6000, 6000)]
	#[case(12000, 7000, 1000)]
	// Test basic timing with relay slot duration
	#[case(11999, 0, 1)]
	fn test_get_next_slot(
		#[case] time_now: u64,
		#[case] offset_millis: u64,
		#[case] expected_wait_duration: u128,
	) {
		let relay_slot_duration = Duration::from_millis(RELAY_CHAIN_SLOT_DURATION);
		let time_now = Duration::from_millis(time_now);
		let offset = Duration::from_millis(offset_millis);

		let (wait_duration, _) = time_until_next_slot(time_now, relay_slot_duration, offset);

		assert_eq!(wait_duration.as_millis(), expected_wait_duration, "Wait time mismatch.");
	}

	#[rstest]
	// Basic slot change scenarios
	#[case(6000, 0, 0, Slot::from(0), 6000)]
	#[case(6000, 1000, 0, Slot::from(0), 5000)]
	#[case(6000, 6000, 0, Slot::from(1), 6000)]
	#[case(6000, 12000, 0, Slot::from(2), 6000)]
	// Test with offset
	#[case(6000, 1000, 1000, Slot::from(0), 6000)]
	#[case(6000, 2000, 1000, Slot::from(0), 5000)]
	#[case(6000, 6000, 3000, Slot::from(0), 3000)]
	// Different slot durations
	#[case(3000, 1000, 0, Slot::from(0), 2000)]
	#[case(3000, 3000, 0, Slot::from(1), 3000)]
	#[case(12000, 6000, 0, Slot::from(0), 6000)]
	#[case(12000, 12000, 0, Slot::from(1), 12000)]
	// Edge cases - at slot boundary
	#[case(6000, 5999, 0, Slot::from(0), 1)]
	#[case(6000, 11999, 0, Slot::from(1), 1)]
	fn test_compute_time_until_next_slot_change(
		#[case] para_slot_millis: u64,
		#[case] time_now: u64,
		#[case] offset_millis: u64,
		#[case] last_reported_slot: Slot,
		#[case] expected_duration: u128,
	) {
		let slot_time = SlotTime {
			relay_slot_duration: Duration::from_millis(para_slot_millis),
			time_offset: Duration::from_millis(offset_millis),
			slot_start_timestamp: Timestamp::new(
				Duration::from_millis(para_slot_millis).as_millis() as u64 * *last_reported_slot,
			),
		};

		let time_left = slot_time.time_left_internal(Duration::from_millis(time_now));

		assert_eq!(time_left.as_millis(), expected_duration, "Duration mismatch");
	}
}
