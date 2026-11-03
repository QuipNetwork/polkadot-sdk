// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Recorder: append-only ordered log of observations.

use crate::{
	contract::Effect,
	harness::observation::{Observation, Stamped},
};
use std::time::Duration;

/// An append-only observation log. Used by the dispatcher to record effects and by tests to
/// query / assert against the resulting log.
#[derive(Debug, Clone, Default)]
pub struct Recorder {
	entries: Vec<Observation>,
}

impl Recorder {
	/// Create a fresh recorder.
	pub fn new() -> Self {
		Self::default()
	}

	/// Record an effect stamped with the simulated time elapsed since the start of the
	/// scenario.
	///
	/// The caller supplies `sim_t` directly (from the clock's `duration_since_epoch`) rather
	/// than the recorder deriving it from a first-observation epoch. This keeps a recorded
	/// effect's `sim_t` on the same time origin (sim construction) as everything else the
	/// harness measures with `Sim::now_sim_t`, so windowed assertions compare like with like.
	pub fn record_effect(&mut self, sim_t: Duration, effect: Effect) {
		self.entries.push(Observation::Effect(Stamped { sim_t, value: effect }));
	}

	/// All recorded observations, in order.
	pub fn entries(&self) -> &[Observation] {
		&self.entries
	}

	/// Total observation count.
	pub fn len(&self) -> usize {
		self.entries.len()
	}

	/// Whether the recorder has any observations.
	pub fn is_empty(&self) -> bool {
		self.entries.is_empty()
	}

	/// All effects in the log, in order. Convenience for tests that don't need timestamps.
	pub fn effects(&self) -> impl Iterator<Item = &Effect> {
		self.entries.iter().map(|o| match o {
			Observation::Effect(s) => &s.value,
		})
	}

	/// Effects observed within the last `window`, in order. Useful for failure messages.
	pub fn effects_within(&self, window: Duration) -> impl Iterator<Item = &Stamped<Effect>> {
		let cutoff = self.entries.last().map(|o| match o {
			Observation::Effect(s) => s.sim_t,
		});
		self.entries.iter().filter_map(move |o| match o {
			Observation::Effect(s) => {
				if cutoff.map_or(false, |c| c.saturating_sub(s.sim_t) <= window) {
					Some(s)
				} else {
					None
				}
			},
		})
	}

	/// Find the first effect matching `predicate`. Returns its index in the log.
	pub fn find<F: Fn(&Effect) -> bool>(&self, predicate: F) -> Option<usize> {
		self.entries.iter().position(|o| match o {
			Observation::Effect(s) => predicate(&s.value),
		})
	}

	/// Find the first effect at index `>= from` matching `predicate`.
	///
	/// Callers snapshot [`Recorder::len`] before a stimulus and pass it here to match only
	/// effects recorded *after* that point. This is sharper than filtering on `sim_t`: two
	/// effects emitted in the same simulated instant are distinguished by their position in
	/// the log, so a fresh effect is never confused with an unrelated earlier one recorded at
	/// the same `sim_t`.
	pub fn find_effect_from<F: Fn(&Effect) -> bool>(
		&self,
		from: usize,
		predicate: F,
	) -> Option<&Effect> {
		self.entries.get(from..)?.iter().find_map(|o| match o {
			Observation::Effect(s) => {
				if predicate(&s.value) {
					Some(&s.value)
				} else {
					None
				}
			},
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::contract::{Effect, RepBucket};
	use sc_network_types::PeerId;

	#[test]
	fn records_effects_with_supplied_timestamps() {
		let mut rec = Recorder::new();
		let p1 = PeerId::random();
		rec.record_effect(
			Duration::ZERO,
			Effect::Reputation { peer: p1, bucket: RepBucket::Performance },
		);
		rec.record_effect(
			Duration::from_millis(50),
			Effect::Reputation { peer: p1, bucket: RepBucket::Malicious },
		);

		assert_eq!(rec.len(), 2);
		match &rec.entries()[0] {
			Observation::Effect(s) => assert_eq!(s.sim_t, Duration::ZERO),
		}
		match &rec.entries()[1] {
			Observation::Effect(s) => assert_eq!(s.sim_t, Duration::from_millis(50)),
		}
	}

	#[test]
	fn find_returns_first_match_index() {
		let mut rec = Recorder::new();
		let p1 = PeerId::random();
		rec.record_effect(
			Duration::ZERO,
			Effect::Reputation { peer: p1, bucket: RepBucket::Performance },
		);
		rec.record_effect(
			Duration::ZERO,
			Effect::Reputation { peer: p1, bucket: RepBucket::Malicious },
		);
		rec.record_effect(
			Duration::ZERO,
			Effect::Reputation { peer: p1, bucket: RepBucket::Performance },
		);

		let idx = rec
			.find(|e| matches!(e, Effect::Reputation { bucket: RepBucket::Malicious, .. }))
			.expect("malicious entry");
		assert_eq!(idx, 1);
	}
}
