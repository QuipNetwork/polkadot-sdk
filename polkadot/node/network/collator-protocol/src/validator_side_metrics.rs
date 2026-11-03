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

use polkadot_node_subsystem_util::metrics::{self, prometheus};
use polkadot_primitives::Id as ParaId;
use std::collections::BTreeMap;

/// A reputation score band — the `score_range` label of the `connected_collators` gauge.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScoreBand {
	/// Score 0 — below `INSTANT_FETCH_REP_THRESHOLD`, so this collator cannot instant-fetch.
	Zero,
	/// Above the instant-fetch threshold but still low score (1-99).
	Low,
	/// Medium score (100-999).
	Medium,
	/// High score (1000+, up to `MAX_SCORE`).
	High,
}

impl ScoreBand {
	/// All bands, so callers can emit an explicit value (including `0`) for every band.
	const ALL: [ScoreBand; 4] = [Self::Zero, Self::Low, Self::Medium, Self::High];

	/// Classify a reputation score into a band.
	pub fn classify(score: u16) -> Self {
		match score {
			0 => Self::Zero,
			1..=99 => Self::Low,
			100..=999 => Self::Medium,
			_ => Self::High,
		}
	}

	/// The `score_range` metric label for this band.
	fn label(&self) -> &'static str {
		match self {
			Self::Zero => "0",
			Self::Low => "1-99",
			Self::Medium => "100-999",
			Self::High => "1000+",
		}
	}
}

/// The `handler` metric label of the `process_msg_duration` histogram.
#[derive(Clone, Copy)]
pub enum TimedHandler {
	/// Handling a new peer connection.
	PeerConnected,
	/// Handling a peer disconnection.
	PeerDisconnected,
	/// Handling a peer's declaration of the para it collates for.
	Declare,
	/// Handling a collation advertisement.
	Advertisement,
	/// Handling a change of our own view.
	OurViewChange,
	/// Handling a block finality notification.
	FinalizedBlock,
	/// Handling the result of a collation fetch request.
	FetchedCollation,
	/// Handling a candidate seconded by the backing subsystem.
	Seconded,
	/// Handling a collation reported as invalid by the backing subsystem.
	InvalidCollation,
	/// Launching new collation fetch requests. Runs on every subsystem loop iteration.
	LaunchFetchRequests,
}

impl TimedHandler {
	fn label(&self) -> &'static str {
		match self {
			Self::PeerConnected => "peer_connected",
			Self::PeerDisconnected => "peer_disconnected",
			Self::Declare => "declare",
			Self::Advertisement => "advertisement",
			Self::OurViewChange => "our_view_change",
			Self::FinalizedBlock => "finalized_block",
			Self::FetchedCollation => "fetched_collation",
			Self::Seconded => "seconded",
			Self::InvalidCollation => "invalid_collation",
			Self::LaunchFetchRequests => "launch_fetch_requests",
		}
	}
}

#[derive(Clone, Default)]
pub struct Metrics(Option<MetricsInner>);

impl Metrics {
	pub fn on_request(&self, succeeded: std::result::Result<(), ()>) {
		if let Some(metrics) = &self.0 {
			match succeeded {
				Ok(()) => metrics.collation_requests.with_label_values(&["succeeded"]).inc(),
				Err(()) => metrics.collation_requests.with_label_values(&["failed"]).inc(),
			}
		}
	}

	/// Provide a timer for `process_msg` which observes on drop.
	pub fn time_process_msg(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.process_msg.start_timer())
	}

	/// Provide a timer for one of the subsystem's handlers, which observes on drop.
	pub fn time_handler(
		&self,
		handler: TimedHandler,
	) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| {
			metrics.process_msg_duration.with_label_values(&[handler.label()]).start_timer()
		})
	}

	/// Provide a timer for `handle_collation_request_result` which observes on drop.
	pub fn time_handle_collation_request_result(
		&self,
	) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0
			.as_ref()
			.map(|metrics| metrics.handle_collation_request_result.start_timer())
	}

	/// Note the current number of collator peers.
	pub fn note_collator_peer_count(&self, collator_peers: usize) {
		self.0
			.as_ref()
			.map(|metrics| metrics.collator_peer_count.set(collator_peers as u64));
	}

	pub fn note_in_memory_connected_peers(&self, connected_peers: usize) {
		if let Some(metrics) = &self.0 {
			metrics.in_memory_connected_peers.set(connected_peers as u64);
		}
	}

	fn note_advertisement(&self, para_id: Option<&ParaId>, outcome: &'static str) {
		if let Some(metrics) = &self.0 {
			let para_id: String = match para_id {
				Some(id) => u32::from(*id).to_string(),
				None => "unknown".into(),
			};
			metrics.advertisements.with_label_values(&[para_id.as_str(), outcome]).inc();
		}
	}

	/// Note an advertisement that passed triage and was accepted.
	pub fn on_advertisement_accepted(&self, para_id: &ParaId) {
		self.note_advertisement(Some(para_id), "accepted");
	}

	/// Rejected: advertisement from a peer we are not connected to. The peer is unknown so the
	/// `para_id` label is set to `"unknown"`.
	pub fn on_advertisement_rejected_unconnected_peer(&self) {
		self.note_advertisement(None, "unconnected_peer");
	}

	/// Rejected: peer advertised before declaring which para it collates for. No `para_id` is
	/// known at this point, so the label is set to `"unknown"`.
	pub fn on_advertisement_rejected_undeclared_peer(&self) {
		self.note_advertisement(None, "undeclared_peer");
	}

	/// Rejected: duplicate advertisement.
	pub fn on_advertisement_rejected_duplicate(&self, para_id: &ParaId) {
		self.note_advertisement(Some(para_id), "duplicate");
	}

	/// Rejected: advertised scheduling parent is out of our view.
	pub fn on_advertisement_rejected_out_of_view(&self, para_id: &ParaId) {
		self.note_advertisement(Some(para_id), "out_of_view");
	}

	/// Rejected: peer reached its candidate limit (or para not schedulable from this SP).
	pub fn on_advertisement_rejected_peer_limit_reached(&self, para_id: &ParaId) {
		self.note_advertisement(Some(para_id), "peer_limit_reached");
	}

	/// Rejected: seconding not allowed by the backing subsystem.
	pub fn on_advertisement_rejected_blocked_by_backing(&self, para_id: &ParaId) {
		self.note_advertisement(Some(para_id), "blocked_by_backing");
	}

	/// Rejected: V1 advertisement for an implicit (non-leaf) relay parent.
	pub fn on_advertisement_rejected_v1_for_implicit_parent(&self, para_id: &ParaId) {
		self.note_advertisement(Some(para_id), "v1_for_implicit_parent");
	}

	/// Rejected: V3 descriptor's scheduling parent matches no expected scheduling parent.
	pub fn on_advertisement_rejected_scheduling_parent_invalid(&self, para_id: &ParaId) {
		self.note_advertisement(Some(para_id), "scheduling_parent_invalid");
	}

	/// Rejected: V4 segment that is empty or carries an entry whose parent and output heads
	/// are equal. Peer-triggerable, so rare but non-zero in the wild is expected.
	pub fn on_advertisement_rejected_malformed_segment(&self, para_id: &ParaId) {
		self.note_advertisement(Some(para_id), "malformed_segment");
	}

	/// Rejected: segment mixes by-hash and by-output-head entries. Only a caller bug can
	/// produce this, so a non-zero count is an alert, not a peer signal.
	pub fn on_advertisement_rejected_mixed_claim_shapes(&self, para_id: &ParaId) {
		self.note_advertisement(Some(para_id), "mixed_claim_shapes");
	}

	/// Note that a collation was sent to the backing subsystem to be seconded.
	pub fn on_collation_seconded(&self, para_id: &ParaId) {
		if let Some(metrics) = &self.0 {
			let para_id = u32::from(*para_id).to_string();
			metrics.collations_seconded.with_label_values(&[para_id.as_str()]).inc();
		}
	}

	/// Note a fetched collation that had to be parked because its parent is not yet known to
	/// prospective-parachains and the collator didn't send the parent head data along with the
	/// collation.
	pub fn on_collation_blocked_on_parent(&self, para_id: &ParaId) {
		if let Some(metrics) = &self.0 {
			let para_id = u32::from(*para_id).to_string();
			metrics
				.collations_blocked_on_parent
				.with_label_values(&[para_id.as_str()])
				.inc();
		}
	}

	/// Note a reputation slash for an invalid collation reported by the backing subsystem.
	pub fn on_slash_invalid_collation(&self, para_id: &ParaId) {
		if let Some(metrics) = &self.0 {
			let para_id = u32::from(*para_id).to_string();
			metrics
				.slashes
				.with_label_values(&[para_id.as_str(), "invalid_collation"])
				.inc();
		}
	}

	/// Note a reputation slash for a fetched collation that failed its sanity / validation checks.
	pub fn on_slash_failed_fetch(&self, para_id: &ParaId) {
		if let Some(metrics) = &self.0 {
			let para_id = u32::from(*para_id).to_string();
			metrics.slashes.with_label_values(&[para_id.as_str(), "failed_fetch"]).inc();
		}
	}

	fn note_approved_peer_signal(&self, para_id: &ParaId, outcome: &'static str) {
		if let Some(metrics) = &self.0 {
			let para_id = u32::from(*para_id).to_string();
			metrics
				.approved_peer_signals
				.with_label_values(&[para_id.as_str(), outcome])
				.inc();
		}
	}

	/// Included candidate carried a valid `ApprovedPeer` signal — the collator earned a bump.
	pub fn on_approved_peer_present(&self, para_id: &ParaId) {
		self.note_approved_peer_signal(para_id, "present");
	}

	/// Included v2+ candidate carried no `ApprovedPeer` signal.
	pub fn on_approved_peer_absent(&self, para_id: &ParaId) {
		self.note_approved_peer_signal(para_id, "absent");
	}

	/// Included candidate carried an `ApprovedPeer` signal with an unparseable peer id.
	pub fn on_approved_peer_invalid(&self, para_id: &ParaId) {
		self.note_approved_peer_signal(para_id, "invalid_peer_id");
	}

	/// Failed to parse the UMP signals of an included candidate (should not happen — they are
	/// checked during on-chain backing).
	pub fn on_approved_peer_parse_error(&self, para_id: &ParaId) {
		self.note_approved_peer_signal(para_id, "parse_error");
	}

	/// Note the number of paras this validator is currently assigned to back.
	pub fn note_assigned_paras(&self, assigned: usize) {
		if let Some(metrics) = &self.0 {
			metrics.assigned_paras.set(assigned as u64);
		}
	}

	pub fn note_score_distribution(
		&self,
		distribution: &BTreeMap<ParaId, BTreeMap<ScoreBand, u64>>,
	) {
		let Some(metrics) = &self.0 else { return };

		metrics.connected_collators.reset();
		for (para_id, counts) in distribution {
			let para_id = u32::from(*para_id).to_string();
			for band in ScoreBand::ALL {
				let count = counts.get(&band).copied().unwrap_or(0);
				metrics
					.connected_collators
					.with_label_values(&[para_id.as_str(), band.label()])
					.set(count);
			}
		}
	}

	/// Provide a timer for `CollationFetchRequest` structure which observes on drop.
	pub fn time_collation_request_duration(
		&self,
	) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.collation_request_duration.start_timer())
	}

	/// Provide a timer for `request_unblocked_collations` which observes on drop.
	pub fn time_request_unblocked_collations(
		&self,
	) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0
			.as_ref()
			.map(|metrics| metrics.request_unblocked_collations.start_timer())
	}
}

#[derive(Clone)]
struct MetricsInner {
	collation_requests: prometheus::CounterVec<prometheus::U64>,
	// Used in the legacy implementation to time message handling. Kept in the revamp for backward
	// compatibility.
	process_msg: prometheus::Histogram,
	// Improved version of `process_msg` used in the revamped collator protocol.
	process_msg_duration: prometheus::HistogramVec,
	handle_collation_request_result: prometheus::Histogram,
	collator_peer_count: prometheus::Gauge<prometheus::U64>,
	// Similar to `collator_peer_count`, but represents the in-memory peer count from
	// `PeerManager`.
	in_memory_connected_peers: prometheus::Gauge<prometheus::U64>,
	advertisements: prometheus::CounterVec<prometheus::U64>,
	collations_seconded: prometheus::CounterVec<prometheus::U64>,
	collations_blocked_on_parent: prometheus::CounterVec<prometheus::U64>,
	slashes: prometheus::CounterVec<prometheus::U64>,
	approved_peer_signals: prometheus::CounterVec<prometheus::U64>,
	assigned_paras: prometheus::Gauge<prometheus::U64>,
	connected_collators: prometheus::GaugeVec<prometheus::U64>,
	collation_request_duration: prometheus::Histogram,
	// TODO: Not available for the new implementation. Remove with the old implementation.
	request_unblocked_collations: prometheus::Histogram,
}

impl metrics::Metrics for Metrics {
	fn try_register(
		registry: &prometheus::Registry,
	) -> std::result::Result<Self, prometheus::PrometheusError> {
		let metrics = MetricsInner {
			collation_requests: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"polkadot_parachain_collation_requests_total",
						"Number of collations requested from Collators.",
					),
					&["success"],
				)?,
				registry,
			)?,
			process_msg: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_collator_protocol_validator_process_msg",
						"Time spent within `collator_protocol_validator::process_msg`",
					)
				)?,
				registry,
			)?,
			process_msg_duration: prometheus::register(
				prometheus::HistogramVec::new(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_collator_protocol_validator_process_msg_duration",
						"Time spent handling a single event, by handler.",
					)
					// The default buckets start at 5ms, which is far too coarse here: most of these
					// handlers are expected to complete in tens of microseconds. These span 100us
					// to ~2s.
					.buckets(prometheus::exponential_buckets(0.0001, 3.0, 10)?),
					&["handler"],
				)?,
				registry,
			)?,
			handle_collation_request_result: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_collator_protocol_validator_handle_collation_request_result",
						"Time spent within `collator_protocol_validator::handle_collation_request_result`",
					)
				)?,
				registry,
			)?,
			collator_peer_count: prometheus::register(
				prometheus::Gauge::new(
					"polkadot_parachain_collator_peer_count",
					"Amount of collator peers connected",
				)?,
				registry,
			)?,
			in_memory_connected_peers: prometheus::register(
				prometheus::Gauge::new(
					"polkadot_parachain_collator_protocol_validator_in_memory_connected_peers",
					"Number of collator peers in the collator protocol's own in-memory store",
				)?,
				registry,
			)?,
			advertisements: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"polkadot_parachain_collator_protocol_validator_advertisements_total",
						"Number of triaged collation advertisements, by outcome: \"accepted\" or the \
						 rejection reason.",
					),
					&["para_id", "outcome"],
				)?,
				registry,
			)?,
			collations_seconded: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"polkadot_parachain_collator_protocol_validator_collations_seconded_total",
						"Number of collations sent to the backing subsystem to be seconded, by para.",
					),
					&["para_id"],
				)?,
				registry,
			)?,
			collations_blocked_on_parent: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"polkadot_parachain_collator_protocol_validator_collations_blocked_on_parent_total",
						"Number of fetched collations that could not be seconded because their parent is unknown",
					),
					&["para_id"],
				)?,
				registry,
			)?,
			slashes: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"polkadot_parachain_collator_protocol_validator_slashes_total",
						"Number of collator reputation slashes, by para and reason.",
					),
					&["para_id", "reason"],
				)?,
				registry,
			)?,
			approved_peer_signals: prometheus::register(
				prometheus::CounterVec::new(
					prometheus::Opts::new(
						"polkadot_parachain_collator_protocol_validator_approved_peer_signals_total",
						"Outcome of the `ApprovedPeer` UMP signal on included v2+ candidates, by para.",
					),
					&["para_id", "outcome"],
				)?,
				registry,
			)?,
			assigned_paras: prometheus::register(
				prometheus::Gauge::new(
					"polkadot_parachain_collator_protocol_validator_assigned_paras",
					"Number of paras this validator is currently assigned to back (0 = idle).",
				)?,
				registry,
			)?,
			connected_collators: prometheus::register(
				prometheus::GaugeVec::new(
					prometheus::Opts::new(
						"polkadot_parachain_collator_protocol_validator_connected_collators",
						"Number of connected declared collators per para, by reputation score band \
					 (`score_range` label). The `0` band is below the instant-fetch threshold.",
					),
					&["para_id", "score_range"],
				)?,
				registry,
			)?,
			collation_request_duration: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_collator_protocol_validator_collation_request_duration",
						"Lifetime of the `CollationFetchRequest` structure",
					).buckets(vec![0.05, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.75, 0.9, 1.0, 1.2, 1.5, 1.75]),
				)?,
				registry,
			)?,
			request_unblocked_collations: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_collator_protocol_validator_request_unblocked_collations",
						"Time spent within `collator_protocol_validator::request_unblocked_collations`",
					)
				)?,
				registry,
			)?,
		};

		Ok(Metrics(Some(metrics)))
	}
}
