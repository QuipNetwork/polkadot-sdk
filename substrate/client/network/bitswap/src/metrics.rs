// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use litep2p::protocol::libp2p::bitswap::{BlockPresenceType, ResponseType};
use prometheus_endpoint::{
	exponential_buckets, register, Counter, CounterVec, Gauge, Histogram, HistogramOpts, Opts,
	PrometheusError, Registry, U64,
};
use std::{sync::Arc, time::Duration};

pub(crate) mod outcomes {
	pub(crate) const BLOCK_SERVED: &str = "block_served";
	pub(crate) const HAVE: &str = "have";
	pub(crate) const DONT_HAVE: &str = "dont_have";
	pub(crate) const UNSUPPORTED_CID: &str = "unsupported_cid";
}

pub(crate) mod errors {
	pub(crate) const CLIENT: &str = "client";
}

pub(crate) mod outbound_events {
	pub(crate) const REQUESTED: &str = "requested";
	pub(crate) const DELIVERED: &str = "delivered";
	pub(crate) const TIMED_OUT: &str = "timed_out";
	pub(crate) const ROUND_RESTARTED: &str = "round_restarted";
	pub(crate) const ABANDONED: &str = "abandoned";
}

struct Inner {
	entries_total: CounterVec<U64>,
	request_errors_total: CounterVec<U64>,
	inbound_queue_overflow_total: Counter<U64>,
	inbound_request_duration_seconds: Histogram,
	response_bytes_total: Counter<U64>,
	outbound_events_total: CounterVec<U64>,
	live_cids: Gauge<U64>,
	queued_cids: Gauge<U64>,
	user_requests: Gauge<U64>,
}

impl Inner {
	fn register(registry: &Registry) -> Result<Self, PrometheusError> {
		Ok(Self {
			entries_total: register(
				CounterVec::new(
					Opts::new(
						"substrate_sub_libp2p_bitswap_entries_total",
						"Total number of bitswap wantlist entries processed, by outcome",
					),
					&["outcome"],
				)?,
				registry,
			)?,
			request_errors_total: register(
				CounterVec::new(
					Opts::new(
						"substrate_sub_libp2p_bitswap_request_errors_total",
						"Total number of bitswap inbound requests rejected, by reason",
					),
					&["reason"],
				)?,
				registry,
			)?,
			inbound_queue_overflow_total: register(
				Counter::new(
					"substrate_sub_libp2p_bitswap_inbound_queue_overflow_total",
					"Total number of inbound bitswap wantlist entries dropped because a peer queue was full",
				)?,
				registry,
			)?,
			inbound_request_duration_seconds: register(
				Histogram::with_opts(HistogramOpts {
					common_opts: Opts::new(
						"substrate_sub_libp2p_bitswap_inbound_request_duration_seconds",
						"Duration of handling an inbound bitswap wantlist, in seconds",
					),
					buckets: exponential_buckets(0.001, 2.0, 16)
						.expect("valid histogram parameters"),
				})?,
				registry,
			)?,
			response_bytes_total: register(
				Counter::new(
					"substrate_sub_libp2p_bitswap_response_bytes_total",
					"Total payload bytes sent in bitswap responses to inbound wantlists",
				)?,
				registry,
			)?,
			outbound_events_total: register(
				CounterVec::new(
					Opts::new(
						"substrate_sub_libp2p_bitswap_outbound_events_total",
						"Total number of outbound bitswap events, by event",
					),
					&["event"],
				)?,
				registry,
			)?,
			live_cids: register(
				Gauge::new(
					"substrate_sub_libp2p_bitswap_live_cids",
					"Number of CIDs with an in-flight peer request",
				)?,
				registry,
			)?,
			queued_cids: register(
				Gauge::new(
					"substrate_sub_libp2p_bitswap_queued_cids",
					"Number of CIDs waiting for a dispatch-window slot",
				)?,
				registry,
			)?,
			user_requests: register(
				Gauge::new(
					"substrate_sub_libp2p_bitswap_user_requests",
					"Number of active outbound bitswap requests",
				)?,
				registry,
			)?,
		})
	}
}

#[derive(Clone, Default)]
pub(crate) struct BitswapMetrics {
	inner: Option<Arc<Inner>>,
}

impl BitswapMetrics {
	pub(crate) fn new(registry: Option<&Registry>) -> Result<Self, PrometheusError> {
		Ok(Self { inner: registry.map(Inner::register).transpose()?.map(Arc::new) })
	}

	pub(crate) fn record_entry(&self, outcome: &str) {
		self.record_entries(outcome, 1);
	}

	pub(crate) fn record_entries(&self, outcome: &str, count: usize) {
		if let Some(inner) = &self.inner {
			inner.entries_total.with_label_values(&[outcome]).inc_by(count as u64);
		}
	}

	pub(crate) fn record_response(&self, response: &ResponseType) {
		let outcome = match response {
			ResponseType::Block { .. } => outcomes::BLOCK_SERVED,
			ResponseType::Presence { presence: BlockPresenceType::Have, .. } => outcomes::HAVE,
			ResponseType::Presence { presence: BlockPresenceType::DontHave, .. } => {
				outcomes::DONT_HAVE
			},
		};
		self.record_entry(outcome);
	}

	pub(crate) fn record_error(&self, reason: &str) {
		if let Some(inner) = &self.inner {
			inner.request_errors_total.with_label_values(&[reason]).inc();
		}
	}

	pub(crate) fn record_inbound_queue_overflow(&self, count: usize) {
		if let Some(inner) = &self.inner {
			inner.inbound_queue_overflow_total.inc_by(count as u64);
		}
	}

	pub(crate) fn record_duration(&self, duration: Duration) {
		if let Some(inner) = &self.inner {
			inner.inbound_request_duration_seconds.observe(duration.as_secs_f64());
		}
	}

	pub(crate) fn record_response_bytes(&self, responses: &[ResponseType]) {
		self.add_response_bytes(response_payload_bytes(responses));
	}

	pub(crate) fn record_outbound(&self, event: &str, count: usize) {
		if let Some(inner) = &self.inner {
			inner.outbound_events_total.with_label_values(&[event]).inc_by(count as u64);
		}
	}

	pub(crate) fn set_state(&self, live_cids: usize, queued_cids: usize, user_requests: usize) {
		if let Some(inner) = &self.inner {
			inner.live_cids.set(live_cids as u64);
			inner.queued_cids.set(queued_cids as u64);
			inner.user_requests.set(user_requests as u64);
		}
	}

	fn add_response_bytes(&self, bytes: u64) {
		if let Some(inner) = &self.inner {
			inner.response_bytes_total.inc_by(bytes);
		}
	}
}

fn response_payload_bytes(responses: &[ResponseType]) -> u64 {
	responses
		.iter()
		.map(|response| match response {
			ResponseType::Block { cid, block } => cid.to_bytes().len() + block.len(),
			ResponseType::Presence { cid, .. } => cid.to_bytes().len(),
		})
		.sum::<usize>() as u64
}

#[cfg(test)]
mod tests {
	use super::*;
	use cid::{multihash::Multihash as CidMultihash, Cid};

	fn make_cid() -> Cid {
		let mh = CidMultihash::<64>::wrap(0xb220, &[0u8; 32]).unwrap();
		Cid::new_v1(0x55, mh)
	}

	#[test]
	fn disabled_metrics_are_no_ops() {
		let metrics = BitswapMetrics::default();
		metrics.record_entry(outcomes::BLOCK_SERVED);
		metrics.record_error(errors::CLIENT);
		metrics.record_inbound_queue_overflow(1);
		metrics.record_duration(Duration::from_millis(1));
		metrics.record_outbound(outbound_events::REQUESTED, 1);
		metrics.set_state(1, 2, 3);
	}

	#[test]
	fn enabled_metrics_record_inbound_and_outbound_activity() {
		let registry = Registry::new();
		let metrics = BitswapMetrics::new(Some(&registry)).unwrap();
		let cid = make_cid();
		let responses = [
			ResponseType::Block { cid, block: vec![1, 2, 3] },
			ResponseType::Presence { cid, presence: BlockPresenceType::DontHave },
		];

		for response in &responses {
			metrics.record_response(response);
		}
		metrics.record_response_bytes(&responses);
		metrics.record_error(errors::CLIENT);
		metrics.record_inbound_queue_overflow(7);
		metrics.record_duration(Duration::from_millis(5));
		metrics.record_outbound(outbound_events::REQUESTED, 2);
		metrics.set_state(1, 2, 3);

		let inner = metrics.inner.as_ref().unwrap();
		assert_eq!(inner.entries_total.with_label_values(&[outcomes::BLOCK_SERVED]).get(), 1);
		assert_eq!(inner.entries_total.with_label_values(&[outcomes::HAVE]).get(), 0);
		assert_eq!(inner.entries_total.with_label_values(&[outcomes::DONT_HAVE]).get(), 1);
		assert_eq!(inner.entries_total.with_label_values(&[outcomes::UNSUPPORTED_CID]).get(), 0);
		assert_eq!(inner.inbound_queue_overflow_total.get(), 7);
		assert_eq!(inner.request_errors_total.with_label_values(&[errors::CLIENT]).get(), 1);
		assert_eq!(
			inner
				.outbound_events_total
				.with_label_values(&[outbound_events::REQUESTED])
				.get(),
			2
		);
		assert_eq!(inner.live_cids.get(), 1);
		assert_eq!(inner.queued_cids.get(), 2);
		assert_eq!(inner.user_requests.get(), 3);
		assert_eq!(inner.inbound_request_duration_seconds.get_sample_count(), 1);
		assert_eq!(inner.response_bytes_total.get(), response_payload_bytes(&responses));
	}
}
