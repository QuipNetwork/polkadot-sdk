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

use std::{
	collections::HashMap,
	time::{Duration, Instant},
};

use polkadot_node_subsystem::prometheus::prometheus::HistogramTimer;
use polkadot_node_subsystem_util::metrics::{self, prometheus};
use polkadot_primitives::{BlockNumber, CandidateReceiptV2 as CandidateReceipt, Hash};
use sp_core::H256;

use super::collation::CollationStatus;

#[derive(Clone, Default)]
pub struct Metrics(Option<MetricsInner>);

impl Metrics {
	/// Record the time a collation took to be backed.
	pub fn on_collation_backed(&self, latency: f64) {
		if let Some(metrics) = &self.0 {
			metrics.collation_backing_latency.observe(latency);
		}
	}

	/// Record the time a collation took to be included.
	pub fn on_collation_included(&self, latency: f64) {
		if let Some(metrics) = &self.0 {
			metrics.collation_inclusion_latency.observe(latency);
		}
	}

	pub fn on_advertisement_made(&self) {
		if let Some(metrics) = &self.0 {
			metrics.advertisements_made.inc();
		}
	}

	pub fn on_collation_sent_requested(&self) {
		if let Some(metrics) = &self.0 {
			metrics.collations_send_requested.inc();
		}
	}

	pub fn on_collation_sent(&self) {
		if let Some(metrics) = &self.0 {
			metrics.collations_sent.inc();
		}
	}

	/// Provide a timer for `process_msg` which observes on drop.
	pub fn time_process_msg(&self) -> Option<prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.process_msg.start_timer())
	}

	/// Provide a timer for `distribute_collation` which observes on drop.
	pub fn time_collation_distribution(
		&self,
		label: &'static str,
	) -> Option<prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| {
			metrics.collation_distribution_time.with_label_values(&[label]).start_timer()
		})
	}

	/// Create a timer to measure how much time collations spend before being fetched.
	pub fn time_collation_fetch_latency(&self) -> Option<prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.collation_fetch_latency.start_timer())
	}

	/// Create a timer to measure how much time it takes for fetched collations to be backed.
	pub fn time_collation_backing_latency(&self) -> Option<prometheus::prometheus::HistogramTimer> {
		self.0
			.as_ref()
			.map(|metrics| metrics.collation_backing_latency_time.start_timer())
	}

	/// Record the time a collation took before expiring.
	/// Collations can expire in the following states: "advertised, fetched or backed"
	pub fn on_collation_expired(&self, latency: f64, state: &'static str) {
		if let Some(metrics) = &self.0 {
			metrics.collation_expired_total.with_label_values(&[state]).observe(latency);
		}
	}
}

#[derive(Clone)]
struct MetricsInner {
	advertisements_made: prometheus::Counter<prometheus::U64>,
	collations_sent: prometheus::Counter<prometheus::U64>,
	collations_send_requested: prometheus::Counter<prometheus::U64>,
	process_msg: prometheus::Histogram,
	collation_distribution_time: prometheus::HistogramVec,
	collation_fetch_latency: prometheus::Histogram,
	collation_backing_latency_time: prometheus::Histogram,
	collation_backing_latency: prometheus::Histogram,
	collation_inclusion_latency: prometheus::Histogram,
	collation_expired_total: prometheus::HistogramVec,
}

impl metrics::Metrics for Metrics {
	fn try_register(
		registry: &prometheus::Registry,
	) -> std::result::Result<Self, prometheus::PrometheusError> {
		let metrics = MetricsInner {
			advertisements_made: prometheus::register(
				prometheus::Counter::new(
					"polkadot_parachain_collation_advertisements_made_total",
					"A number of collation advertisements sent to validators.",
				)?,
				registry,
			)?,
			collations_send_requested: prometheus::register(
				prometheus::Counter::new(
					"polkadot_parachain_collations_sent_requested_total",
					"A number of collations requested to be sent to validators.",
				)?,
				registry,
			)?,
			collations_sent: prometheus::register(
				prometheus::Counter::new(
					"polkadot_parachain_collations_sent_total",
					"A number of collations sent to validators.",
				)?,
				registry,
			)?,
			process_msg: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_collator_protocol_collator_process_msg",
						"Time spent within `collator_protocol_collator::process_msg`",
					)
					.buckets(vec![
						0.001, 0.002, 0.005, 0.01, 0.025, 0.05, 0.1, 0.15, 0.25, 0.35, 0.5, 0.75,
						1.0,
					]),
				)?,
				registry,
			)?,
			collation_distribution_time: prometheus::register(
				prometheus::HistogramVec::new(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_collator_protocol_collator_distribution_time",
						"Time spent within `collator_protocol_collator::distribute_collation`",
					)
					.buckets(vec![
						0.001, 0.002, 0.005, 0.01, 0.025, 0.05, 0.1, 0.15, 0.25, 0.35, 0.5, 0.75,
						1.0,
					]),
					&["state"],
				)?,
				registry,
			)?,
			collation_fetch_latency: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_collation_fetch_latency",
						"How much time collations spend waiting to be fetched",
					)
					.buckets(vec![
						0.25, 0.35, 0.5, 0.75, 1.0, 2.0, 5.0, 10.0, 15.0, 20.0, 25.0, 30.0, 35.0,
						60.0,
					]),
				)?,
				registry,
			)?,
			collation_backing_latency_time: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_collation_backing_latency_time",
						"How much time it takes for a fetched collation to be backed",
					)
					.buckets(vec![
						1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 12.0, 15.0, 18.0, 24.0, 30.0,
					]),
				)?,
				registry,
			)?,
			collation_backing_latency: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_collation_backing_latency",
						"How many blocks away from the relay parent are collations backed",
					)
					.buckets(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]),
				)?,
				registry,
			)?,
			collation_inclusion_latency: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_collation_inclusion_latency",
						"How many blocks it takes for a backed collation to be included",
					)
					.buckets(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]),
				)?,
				registry,
			)?,
			collation_expired_total: prometheus::register(
				prometheus::HistogramVec::new(
					prometheus::HistogramOpts::new(
						"polkadot_parachain_collation_expired",
						"How many collations expired (not backed or not included)",
					)
					.buckets(vec![
						1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 15.0, 20.0, 25.0, 30.0,
					]),
					&["state"],
				)?,
				registry,
			)?,
		};

		Ok(Metrics(Some(metrics)))
	}
}

// Equal to claim queue length.
pub(crate) const MAX_BACKING_DELAY: BlockNumber = 3;
// Paras availability period. In practice, candidates time out in exceptional situations.
pub(crate) const MAX_AVAILABILITY_DELAY: BlockNumber = 10;

/// Collations are kept in the tracker, until they are included or expired
#[derive(Default)]
pub(crate) struct CollationTracker {
	/// All un-expired collation entries
	entries: HashMap<Hash, CollationStats>,
}

impl CollationTracker {
	/// Mark a tracked collation as backed.
	///
	/// Block built on top of N is earliest backed at N + 1.
	pub fn collation_backed(
		&mut self,
		block_number: BlockNumber,
		leaf: H256,
		receipt: CandidateReceipt,
	) {
		let head = receipt.descriptor.para_head();
		let Some(entry) = self.entries.get_mut(&head) else {
			gum::debug!(
				target: crate::LOG_TARGET_STATS,
				?head,
				"Backed collation not found in tracker",
			);
			return;
		};

		if entry.backed().is_some() {
			gum::debug!(
				target: crate::LOG_TARGET_STATS,
				?head,
				"Collation already backed in a fork, skipping",
			);
			return;
		}

		entry.set_backed_at(block_number);
		if let Some(latency) = entry.backed() {
			// Observe the backing latency since the collation was fetched.
			let maybe_latency =
				entry.backed_latency_metric.take().map(|metric| metric.stop_and_record());
			gum::debug!(
				target: crate::LOG_TARGET_STATS,
				latency_blocks = ?latency,
				latency_time = ?maybe_latency,
				relay_block = ?leaf,
				relay_parent = ?entry.relay_parent,
				para_id = ?receipt.descriptor.para_id(),
				?head,
				"A fetched collation was backed on relay chain",
			);
		}
	}

	/// Mark a previously backed collation as included.
	///
	/// Block built on top of N is earliest included at N + 2.
	pub fn collation_included(
		&mut self,
		block_number: BlockNumber,
		leaf: H256,
		receipt: CandidateReceipt,
	) {
		let head = receipt.descriptor.para_head();
		let para_id = receipt.descriptor.para_id();
		let Some(entry) = self.entries.get_mut(&head) else {
			gum::debug!(
				target: crate::LOG_TARGET_STATS,
				?para_id,
				?head,
				"Included collation not found in tracker",
			);
			return;
		};

		let pov_hash = entry.pov_hash();
		let candidate_hash = entry.candidate_hash();

		if entry.included().is_some() {
			gum::debug!(
				target: crate::LOG_TARGET_STATS,
				?para_id,
				?head,
				?candidate_hash,
				?pov_hash,
				"Collation already included in a fork, skipping",
			);
			return;
		}

		entry.set_included_at(block_number);
		if let Some(latency) = entry.included() {
			gum::debug!(
				target: crate::LOG_TARGET_STATS,
				?latency,
				relay_block = ?leaf,
				relay_parent = ?entry.relay_parent,
				?para_id,
				?head,
				?candidate_hash,
				?pov_hash,
				"Collation included on relay chain",
			);
		}
	}

	/// Returns all the collations that have expired at `block_number`.
	pub fn drain_expired(&mut self, block_number: BlockNumber) -> Vec<CollationStats> {
		let expired = self
			.entries
			.iter()
			.filter_map(|(head, entry)| entry.is_tracking_expired(block_number).then_some(*head))
			.collect::<Vec<_>>();
		expired
			.iter()
			.filter_map(|head| self.entries.remove(head))
			.map(|mut entry| {
				entry.set_expired_at(block_number);
				entry
			})
			.collect::<Vec<_>>()
	}

	/// Drain and return all collations that are possibly finalized at `block_number`.
	///
	/// We only track the inclusion block number, not the inclusion block hash.
	/// There is a small chance that a collation was included in a fork that is not finalized.
	pub fn drain_finalized(&mut self, block_number: BlockNumber) -> Vec<CollationStats> {
		let finalized = self
			.entries
			.iter()
			.filter_map(|(head, entry)| entry.is_possibly_finalized(block_number).then_some(*head))
			.collect::<Vec<_>>();
		finalized
			.iter()
			.filter_map(|head| self.entries.remove(head))
			.collect::<Vec<_>>()
	}

	/// Track a collation for a given period of time (TTL). TTL depends
	/// on the collation state.
	/// Collation is evicted after it expires.
	pub fn track(&mut self, mut stats: CollationStats) {
		// Disable the fetch timer, to prevent bogus observe on drop.
		if let Some(fetch_latency_metric) = stats.fetch_latency_metric.take() {
			fetch_latency_metric.stop_and_discard();
		}

		if let Some(entry) = self
			.entries
			.values()
			.find(|entry| entry.relay_parent_number == stats.relay_parent_number)
		{
			gum::debug!(
				target: crate::LOG_TARGET_STATS,
				?stats.relay_parent_number,
				?stats.relay_parent,
				entry_relay_parent = ?entry.relay_parent,
				"Collation built on a fork",
			);
		}

		self.entries.insert(stats.head, stats);
	}
}

/// Information about how collations live their lives.
pub(crate) struct CollationStats {
	/// The pre-backing collation status information
	pre_backing_status: CollationStatus,
	/// The block header hash.
	head: Hash,
	/// The relay parent on top of which collation was built
	relay_parent_number: BlockNumber,
	/// The relay parent hash.
	relay_parent: Hash,
	/// The expiration block number if expired.
	expired_at: Option<BlockNumber>,
	/// The backed block number.
	backed_at: Option<BlockNumber>,
	/// The included block number if backed.
	included_at: Option<BlockNumber>,
	/// The collation fetch time.
	fetched_at: Option<Instant>,
	/// Advertisement time
	advertised_at: Instant,
	/// The collation fetch latency (seconds).
	fetch_latency_metric: Option<HistogramTimer>,
	/// The collation backing latency (seconds). Duration since collation fetched
	/// until the import of a relay chain block where collation is backed.
	backed_latency_metric: Option<HistogramTimer>,
	/// The Collation candidate hash
	candidate_hash: Hash,
	/// The Collation PoV hash
	pov_hash: Hash,
}

impl CollationStats {
	/// Create new empty instance.
	pub fn new(
		clock: &dyn polkadot_node_clock::Clock,
		head: Hash,
		relay_parent_number: BlockNumber,
		relay_parent: Hash,
		metrics: &Metrics,
		candidate_hash: Hash,
		pov_hash: Hash,
	) -> Self {
		Self {
			pre_backing_status: CollationStatus::Created,
			head,
			relay_parent_number,
			relay_parent,
			advertised_at: clock.now(),
			backed_at: None,
			expired_at: None,
			fetched_at: None,
			included_at: None,
			fetch_latency_metric: metrics.time_collation_fetch_latency(),
			backed_latency_metric: None,
			candidate_hash,
			pov_hash,
		}
	}

	/// Returns the hash and number of the relay parent.
	pub fn relay_parent(&self) -> (Hash, BlockNumber) {
		(self.relay_parent, self.relay_parent_number)
	}

	/// Returns the age at which the collation expired.
	pub fn expired(&self) -> Option<BlockNumber> {
		let expired_at = self.expired_at?;
		Some(expired_at.saturating_sub(self.relay_parent_number))
	}

	/// Returns the age of the collation at the moment of backing.
	pub fn backed(&self) -> Option<BlockNumber> {
		let backed_at = self.backed_at?;
		Some(backed_at.saturating_sub(self.relay_parent_number))
	}

	/// Returns the age of the collation at the moment of inclusion.
	pub fn included(&self) -> Option<BlockNumber> {
		let included_at = self.included_at?;
		let backed_at = self.backed_at?;
		Some(included_at.saturating_sub(backed_at))
	}

	/// Returns time the collation waited to be fetched.
	pub fn fetch_latency(&self) -> Option<Duration> {
		let fetched_at = self.fetched_at?;
		Some(fetched_at - self.advertised_at)
	}

	/// Get parachain block header hash.
	pub fn head(&self) -> H256 {
		self.head
	}

	/// Get candidate hash.
	pub fn candidate_hash(&self) -> H256 {
		self.candidate_hash
	}

	/// Get candidate PoV hash.
	pub fn pov_hash(&self) -> H256 {
		self.pov_hash
	}

	/// Set the timestamp at which collation is fetched.
	pub fn set_fetched_at(&mut self, fetched_at: Instant) {
		self.fetched_at = Some(fetched_at);
	}

	/// Set the timestamp at which collation is backed.
	pub fn set_backed_at(&mut self, backed_at: BlockNumber) {
		self.backed_at = Some(backed_at);
	}

	/// Set the timestamp at which collation is included.
	pub fn set_included_at(&mut self, included_at: BlockNumber) {
		self.included_at = Some(included_at);
	}

	/// Set the timestamp at which collation is expired.
	pub fn set_expired_at(&mut self, expired_at: BlockNumber) {
		self.expired_at = Some(expired_at);
	}

	/// Sets the pre-backing status of the collation.
	pub fn set_pre_backing_status(&mut self, status: CollationStatus) {
		self.pre_backing_status = status;
	}

	/// Returns the pre-backing status of the collation.
	pub fn pre_backing_status(&self) -> &CollationStatus {
		&self.pre_backing_status
	}

	/// Take the fetch latency metric timer.
	pub fn take_fetch_latency_metric(&mut self) -> Option<HistogramTimer> {
		self.fetch_latency_metric.take()
	}

	/// Set the backing latency metric timer.
	pub fn set_backed_latency_metric(&mut self, timer: Option<HistogramTimer>) {
		self.backed_latency_metric = timer;
	}

	/// Returns the time to live for the collation.
	pub fn tracking_ttl(&self) -> BlockNumber {
		if self.fetch_latency().is_none() {
			0 // Collation was never fetched, expires ASAP
		} else if self.backed().is_none() {
			MAX_BACKING_DELAY
		} else if self.included().is_none() {
			self.backed().expect("backed, checked above") + MAX_AVAILABILITY_DELAY
		} else {
			0 // If block included no reason to track it.
		}
	}

	/// Returns the state of the collation at the moment of expiry.
	pub fn expiry_state(&self) -> &'static str {
		if self.fetch_latency().is_none() {
			// If collation was not fetched, we rely on the status provided
			// by the collator protocol.
			self.pre_backing_status().label()
		} else if self.backed().is_none() {
			"fetched"
		} else if self.included().is_none() {
			"backed"
		} else {
			"none"
		}
	}

	/// Returns true if the collation is expired.
	pub fn is_tracking_expired(&self, current_block: BlockNumber) -> bool {
		// Don't expire included collations
		if self.included().is_some() {
			return false;
		}
		let expiry_block = self.relay_parent_number + self.tracking_ttl();
		expiry_block <= current_block
	}

	/// Check if this collation is possibly finalized based on block number.
	///
	/// Returns `true` if the collation was included at or before `last_finalized`.
	///
	/// We only track the inclusion block number, not the inclusion block hash.
	/// There is a small chance that a collation was included in a fork that is not finalized.
	pub fn is_possibly_finalized(&self, last_finalized: BlockNumber) -> bool {
		self.included_at
			.map(|included_at| included_at <= last_finalized)
			.unwrap_or_default()
	}
}

impl Drop for CollationStats {
	fn drop(&mut self) {
		if let Some(fetch_latency_metric) = self.fetch_latency_metric.take() {
			// This metric is only observed when collation was sent fully to the validator.
			//
			// If `fetch_latency_metric` is Some it means that the metrics was observed.
			// We don't want to observe it again and report a higher value at a later point in time.
			fetch_latency_metric.stop_and_discard();
		}
		// If timer still exists, drop it. It is measured in `collation_backed`.
		if let Some(backed_latency_metric) = self.backed_latency_metric.take() {
			backed_latency_metric.stop_and_discard();
		}
	}
}
