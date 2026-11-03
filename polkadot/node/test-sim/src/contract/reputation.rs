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

//! Bucketing for reputation changes.
//!
//! Tests assert on the *bucket* a reputation change falls into, not its exact magnitude. Mapping
//! is derived from `UnifiedReputationChange` variants: `Malicious(_) -> Malicious`,
//! benefits -> `Benefit`, all `CostMinor`/`CostMajor` flavours -> `Performance`.

use polkadot_node_network_protocol::{ReputationChange, UnifiedReputationChange};

/// Coarse classification of a reputation change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepBucket {
	/// A protocol-defined malicious behavior. `i32::MIN` magnitude.
	Malicious,
	/// A non-malicious cost (slow response, oversized message, ...).
	Performance,
	/// Good behavior reward.
	Benefit,
}

impl RepBucket {
	/// Map a [`UnifiedReputationChange`] (the typed reputation enum used at most call sites in
	/// the subsystem) to a coarse bucket.
	pub const fn from_unified(rep: &UnifiedReputationChange) -> Self {
		match rep {
			UnifiedReputationChange::Malicious(_) => RepBucket::Malicious,
			UnifiedReputationChange::CostMajor(_) |
			UnifiedReputationChange::CostMinor(_) |
			UnifiedReputationChange::CostMajorRepeated(_) |
			UnifiedReputationChange::CostMinorRepeated(_) => RepBucket::Performance,
			UnifiedReputationChange::BenefitMajor(_) |
			UnifiedReputationChange::BenefitMinor(_) |
			UnifiedReputationChange::BenefitMajorFirst(_) |
			UnifiedReputationChange::BenefitMinorFirst(_) => RepBucket::Benefit,
		}
	}

	/// Bucket a raw reputation magnitude. `i32::MIN` is the malicious sentinel; negative is a
	/// cost; positive is a benefit; **zero is a no-op** and returns `None` so callers don't
	/// record a spurious change. (A net-zero magnitude only arises from
	/// [`ReportPeerMessage::Batch`](polkadot_node_subsystem::messages::ReportPeerMessage)
	/// accumulation, where offsetting cost and benefit cancel.)
	pub const fn from_magnitude(magnitude: i32) -> Option<Self> {
		if magnitude == i32::MIN {
			Some(RepBucket::Malicious)
		} else if magnitude < 0 {
			Some(RepBucket::Performance)
		} else if magnitude > 0 {
			Some(RepBucket::Benefit)
		} else {
			None
		}
	}

	/// Map a raw [`ReputationChange`] (i32 magnitude) into a bucket. The single-report path
	/// only ever carries non-zero magnitudes produced by
	/// `UnifiedReputationChange::cost_or_benefit`, so a zero (no-op) is not expected here; it
	/// degenerately buckets as `Benefit`. The shared classification lives in
	/// [`RepBucket::from_magnitude`].
	pub fn from_raw(rep: &ReputationChange) -> Self {
		Self::from_magnitude(rep.value).unwrap_or(RepBucket::Benefit)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn unified_mapping() {
		assert_eq!(
			RepBucket::from_unified(&UnifiedReputationChange::Malicious("bad")),
			RepBucket::Malicious
		);
		assert_eq!(
			RepBucket::from_unified(&UnifiedReputationChange::CostMinor("slow")),
			RepBucket::Performance
		);
		assert_eq!(
			RepBucket::from_unified(&UnifiedReputationChange::BenefitMajor("nice")),
			RepBucket::Benefit
		);
	}

	#[test]
	fn raw_mapping() {
		assert_eq!(
			RepBucket::from_raw(&ReputationChange::new(i32::MIN, "bad")),
			RepBucket::Malicious
		);
		assert_eq!(
			RepBucket::from_raw(&ReputationChange::new(-100, "slow")),
			RepBucket::Performance
		);
		assert_eq!(RepBucket::from_raw(&ReputationChange::new(100, "nice")), RepBucket::Benefit);
	}
}
