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

//! Build human-readable timeline reports from a [`Recorder`] and a failed assertion.

use crate::{
	contract::Effect,
	harness::{
		observation::{Observation, Stamped},
		Recorder,
	},
};
use std::{fmt, time::Duration};

/// One slice of the test history from `start` to `start + window`, with a textual description
/// of what was expected. Calling [`fmt::Display`] yields the canonical failure message.
pub struct TimelineReport<'a> {
	/// Description of the assertion that failed (e.g. "Effect::Second{ candidate: 0xab.. }
	/// within 200ms").
	pub expected: String,
	/// Description of the observed outcome (e.g. "timed out at sim_t = 200ms").
	pub actual: String,
	/// Sim-time of the start of the window.
	pub window_start: Duration,
	/// Length of the assertion window.
	pub window: Duration,
	/// Recorder containing the observations.
	pub recorder: &'a Recorder,
	/// Optional replay seed for reproducing the failure.
	pub replay_seed: Option<u64>,
	/// Optional source location of the assertion (filename:line).
	pub at: Option<&'a str>,
	/// Optional hint sentence (e.g. "closest match was Reputation(...) at +2ms").
	pub hint: Option<String>,
}

impl fmt::Display for TimelineReport<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		if let Some(at) = self.at {
			writeln!(f, "  at {}", at)?;
			writeln!(f)?;
		}
		writeln!(f, "  expected:  {}", self.expected)?;
		writeln!(f, "  actual:    {}", self.actual)?;

		let start = self.window_start;
		let end = self.window_start + self.window;
		writeln!(
			f,
			"             observed effects since the When step (sim_t = {}ms..{}ms):",
			start.as_millis(),
			end.as_millis()
		)?;

		let entries: Vec<&Stamped<Effect>> = self
			.recorder
			.entries()
			.iter()
			.filter_map(|o| match o {
				Observation::Effect(s) => {
					if s.sim_t >= start && s.sim_t <= end {
						Some(s)
					} else {
						None
					}
				},
			})
			.collect();

		if entries.is_empty() {
			writeln!(f, "               <no effects observed in this window>")?;
		} else {
			for e in &entries {
				writeln!(
					f,
					"               [{:>5}ms]  {}",
					e.sim_t.as_millis(),
					format_effect(&e.value)
				)?;
			}
		}

		if let Some(hint) = &self.hint {
			writeln!(f)?;
			writeln!(f, "  hint: {}", hint)?;
		}

		if let Some(seed) = self.replay_seed {
			writeln!(f)?;
			writeln!(f, "  replay:")?;
			writeln!(
				f,
				"    REPLAY_SEED=0x{:x} cargo test -p polkadot-collator-protocol-test-sim",
				seed
			)?;
		}

		Ok(())
	}
}

/// Compact human-readable rendering of an [`Effect`] for the timeline. Exposed so test
/// code can pretty-print recorder entries when an assertion fires.
pub fn format_effect(effect: &Effect) -> String {
	match effect {
		Effect::SecondCandidate { para, candidate_hash, .. } => {
			format!("SecondCandidate(para={}, cand={:?})", u32::from(*para), candidate_hash)
		},
		Effect::SendAdvertisement { peers, .. } => {
			format!("SendAdvertisement(peers={})", peers.len())
		},
		Effect::SendCollation { peers, kind } => {
			format!("SendCollation(peers={}, kind={:?})", peers.len(), kind)
		},
		Effect::SendRequest { request_id, to, kind, candidate_hash } => format!(
			"SendRequest(req={:?}, to={}, kind={:?}, cand={:?})",
			request_id,
			short_peer(to),
			kind,
			candidate_hash
		),
		Effect::SendRequestV3 { request_id, to, kind, output_head_data_hash } => format!(
			"SendRequest(req={:?}, to={}, kind={:?}, cand={:?})",
			request_id,
			short_peer(to),
			kind,
			output_head_data_hash
		),
		Effect::Reputation { peer, bucket } => {
			format!("Reputation(peer={}, bucket={:?})", short_peer(peer), bucket)
		},
		Effect::ConnectValidators { validator_ids, peer_set } => {
			format!("ConnectValidators(n={}, peer_set={:?})", validator_ids.len(), peer_set)
		},
		Effect::DisconnectPeers { peers, peer_set } => {
			format!("DisconnectPeers(n={}, peer_set={:?})", peers.len(), peer_set)
		},
		Effect::RequestResponseSent { request_id, kind } => {
			format!("RequestResponseSent(req={}, kind={:?})", request_id, kind)
		},
	}
}

/// Pretty-print every entry in the recorder as one line per effect, prefixed with its
/// `sim_t` in milliseconds. Tests use this when an assertion fails to dump a readable
/// timeline rather than `Debug` output.
pub fn format_timeline(recorder: &Recorder) -> String {
	let mut out = String::new();
	out.push_str("Effect timeline (relative to first observation):\n");
	let entries: Vec<&Stamped<Effect>> = recorder
		.entries()
		.iter()
		.map(|o| match o {
			Observation::Effect(s) => s,
		})
		.collect();
	if entries.is_empty() {
		out.push_str("  <no effects observed>\n");
		return out;
	}
	for e in entries {
		out.push_str(&format!("  [{:>6}ms] {}\n", e.sim_t.as_millis(), format_effect(&e.value),));
	}
	out
}

fn short_peer(p: &sc_network_types::PeerId) -> String {
	let s = p.to_base58();
	if s.len() <= 10 {
		s
	} else {
		format!("{}..{}", &s[..6], &s[s.len() - 4..])
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::contract::{Effect, RepBucket};
	use sc_network_types::PeerId;

	#[test]
	fn report_renders_observed_window() {
		let mut rec = Recorder::new();
		let p1 = PeerId::random();
		rec.record_effect(
			Duration::ZERO,
			Effect::Reputation { peer: p1, bucket: RepBucket::Performance },
		);
		rec.record_effect(
			Duration::from_millis(2),
			Effect::Reputation { peer: p1, bucket: RepBucket::Malicious },
		);

		let report = TimelineReport {
			expected: "Effect::SecondCandidate within 200ms".into(),
			actual: "timed out at sim_t = 200ms".into(),
			window_start: Duration::ZERO,
			window: Duration::from_millis(200),
			recorder: &rec,
			replay_seed: Some(0xdeadbeef),
			at: Some("tests/foo.rs:42"),
			hint: Some("closest match was Reputation(.., Malicious) at +2ms".into()),
		};

		let s = format!("{}", report);
		assert!(s.contains("at tests/foo.rs:42"));
		assert!(s.contains("expected:  Effect::SecondCandidate"));
		assert!(s.contains("actual:    timed out"));
		assert!(s.contains("[    0ms]  Reputation"));
		assert!(s.contains("[    2ms]  Reputation"));
		assert!(s.contains("hint: closest match"));
		assert!(s.contains("REPLAY_SEED=0xdeadbeef"));
	}

	#[test]
	fn report_handles_empty_window() {
		let rec = Recorder::new();
		let report = TimelineReport {
			expected: "Effect::Foo".into(),
			actual: "timed out".into(),
			window_start: Duration::ZERO,
			window: Duration::from_millis(100),
			recorder: &rec,
			replay_seed: None,
			at: None,
			hint: None,
		};
		let s = format!("{}", report);
		assert!(s.contains("<no effects observed in this window>"));
	}
}
