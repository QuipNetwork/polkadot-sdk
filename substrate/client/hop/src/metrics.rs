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

//! Prometheus metrics for the HOP pool, RPC, and promotion task.

use crate::types::HopError;
use prometheus_endpoint::{
	register, Counter, CounterVec, Gauge, Opts, PrometheusError, Registry, U64,
};

/// `reason` label values for `substrate_hop_pool_removed_total`.
pub mod removal_reasons {
	/// All recipients acknowledged; the entry served its purpose.
	pub const ACKED: &str = "acked";
	/// Expired after the node observed the entry on-chain.
	pub const EXPIRED_PROMOTED: &str = "expired_promoted";
	/// Expired without the node ever observing the entry on-chain. An upper
	/// bound on loss: re-checking stops at `MAX_PROMOTION_ATTEMPTS`.
	pub const EXPIRED_UNPROMOTED: &str = "expired_unpromoted";
	/// Blob failed its content-hash integrity check.
	pub const CORRUPT: &str = "corrupt";
	/// Lost to startup recovery: `.meta` unreadable, undecodable, stale-version,
	/// or missing its blob.
	pub const STARTUP_DROPPED: &str = "startup_dropped";

	/// Every reason, for pre-creating the series at registration.
	pub const ALL: [&str; 5] =
		[ACKED, EXPIRED_PROMOTED, EXPIRED_UNPROMOTED, CORRUPT, STARTUP_DROPPED];
}

/// `method` label values: the wire names, so they join against the RPC
/// middleware's `substrate_rpc_calls_*`.
pub mod rpc_methods {
	pub const SUBMIT: &str = "hop_submit";
	pub const CLAIM: &str = "hop_claim";
	pub const ACK: &str = "hop_ack";
}

/// Stable low-cardinality label for a [`HopError`] variant.
fn error_label(err: &HopError) -> &'static str {
	match err {
		HopError::DataTooLarge(_, _) => "data_too_large",
		HopError::PoolFull(_, _) => "pool_full",
		HopError::DuplicateEntry => "duplicate_entry",
		HopError::NotFound => "not_found",
		HopError::EmptyData => "empty_data",
		HopError::InvalidSignature => "invalid_signature",
		HopError::NotRecipient => "not_recipient",
		HopError::NoRecipients => "no_recipients",
		HopError::InvalidRecipientKey => "invalid_recipient_key",
		HopError::UserQuotaExceeded { .. } => "user_quota_exceeded",
		HopError::NotAuthorized => "not_authorized",
		HopError::IoError(_) => "io_error",
		HopError::InvalidSigner => "invalid_signer",
		HopError::AlreadyClaimed => "already_claimed",
		HopError::InvalidHashLength(_) => "invalid_hash_length",
		HopError::RuntimeApiError(_) => "runtime_api_error",
		HopError::TooManyRecipients { .. } => "too_many_recipients",
		HopError::DuplicateRecipient => "duplicate_recipient",
		HopError::RateLimited { .. } => "rate_limited",
		HopError::MissingDataDir => "missing_data_dir",
	}
}

struct Inner {
	pool_entries: Gauge<U64>,
	pool_bytes: Gauge<U64>,
	pool_max_bytes: Gauge<U64>,
	pool_inserted_bytes_total: Counter<U64>,
	pool_removed_total: CounterVec<U64>,
	rpc_errors_total: CounterVec<U64>,
	promotions_confirmed_total: Counter<U64>,
	promotion_backlog: Gauge<U64>,
	maintenance_ticks_total: Counter<U64>,
}

impl Inner {
	fn register(registry: &Registry) -> Result<Self, PrometheusError> {
		let pool_removed_total = register(
			CounterVec::new(
				Opts::new(
					"substrate_hop_pool_removed_total",
					"Total number of entries removed from the HOP pool, by reason",
				),
				&["reason"],
			)?,
			registry,
		)?;
		// A CounterVec exports nothing until a series is touched; pre-create
		// them all so the first data-loss event is a visible 0->1 transition.
		for reason in removal_reasons::ALL {
			pool_removed_total.with_label_values(&[reason]);
		}
		Ok(Self {
			pool_entries: register(
				Gauge::new("substrate_hop_pool_entries", "Number of entries in the HOP pool")?,
				registry,
			)?,
			pool_bytes: register(
				Gauge::new(
					"substrate_hop_pool_bytes",
					"Accounted size of the HOP pool in bytes (data plus per-recipient overhead)",
				)?,
				registry,
			)?,
			pool_max_bytes: register(
				Gauge::new(
					"substrate_hop_pool_max_bytes",
					"Configured maximum HOP pool size in bytes",
				)?,
				registry,
			)?,
			pool_inserted_bytes_total: register(
				Counter::new(
					"substrate_hop_pool_inserted_bytes_total",
					"Total accounted bytes successfully inserted into the HOP pool",
				)?,
				registry,
			)?,
			pool_removed_total,
			// Per-method call counts and durations are already covered by the
			// RPC middleware (`substrate_rpc_calls_*`); this only adds the
			// error-variant granularity the middleware cannot see.
			rpc_errors_total: register(
				CounterVec::new(
					Opts::new(
						"substrate_hop_rpc_errors_total",
						"Total number of failed HOP RPC requests, by method and reason",
					),
					&["method", "reason"],
				)?,
				registry,
			)?,
			promotions_confirmed_total: register(
				Counter::new(
					"substrate_hop_promotions_confirmed_total",
					"Total number of HOP entries confirmed as promoted on-chain",
				)?,
				registry,
			)?,
			promotion_backlog: register(
				Gauge::new(
					"substrate_hop_promotion_backlog",
					"Number of unpromoted HOP entries inside the promotion window",
				)?,
				registry,
			)?,
			maintenance_ticks_total: register(
				Counter::new(
					"substrate_hop_maintenance_ticks_total",
					"Total number of completed HOP maintenance cycles (promotion + cleanup)",
				)?,
				registry,
			)?,
		})
	}
}

/// HOP metrics; every recorder is a no-op when built without a `Registry`.
pub struct HopMetrics {
	inner: Option<Inner>,
}

impl HopMetrics {
	/// Register the metrics with the given Prometheus registry, if any.
	pub fn new(registry: Option<&Registry>) -> Result<Self, PrometheusError> {
		Ok(Self { inner: registry.map(Inner::register).transpose()? })
	}

	/// Create no-op metrics.
	pub fn disabled() -> Self {
		Self { inner: None }
	}

	/// Whether metrics were registered.
	pub(crate) fn is_enabled(&self) -> bool {
		self.inner.is_some()
	}

	/// Set the pool gauges to absolute values (used after disk recovery).
	pub(crate) fn set_pool_status(&self, entries: u64, bytes: u64, max_bytes: u64) {
		if let Some(inner) = &self.inner {
			inner.pool_entries.set(entries);
			inner.pool_bytes.set(bytes);
			inner.pool_max_bytes.set(max_bytes);
		}
	}

	/// Snapshot the pool size gauges from the authoritative counts. Snapshots
	/// rather than inc/dec because a `Gauge<U64>` wraps on underflow; callers
	/// publish under the pool's index lock so updates arrive in order.
	pub(crate) fn set_pool_size(&self, entries: u64, bytes: u64) {
		if let Some(inner) = &self.inner {
			inner.pool_entries.set(entries);
			inner.pool_bytes.set(bytes);
		}
	}

	/// Count `accounted` bytes successfully inserted.
	pub(crate) fn record_inserted_bytes(&self, accounted: u64) {
		if let Some(inner) = &self.inner {
			inner.pool_inserted_bytes_total.inc_by(accounted);
		}
	}

	/// Record `entries` removals under `reason`.
	pub(crate) fn record_removed(&self, reason: &str, entries: u64) {
		if let Some(inner) = &self.inner {
			inner.pool_removed_total.with_label_values(&[reason]).inc_by(entries);
		}
	}

	/// Record one failed RPC request.
	pub(crate) fn record_rpc_error(&self, method: &str, err: &HopError) {
		if let Some(inner) = &self.inner {
			inner.rpc_errors_total.with_label_values(&[method, error_label(err)]).inc();
		}
	}

	/// Record one entry confirmed as promoted on-chain.
	pub(crate) fn record_promotion_confirmed(&self) {
		if let Some(inner) = &self.inner {
			inner.promotions_confirmed_total.inc();
		}
	}

	/// Set the promotion backlog gauge.
	pub(crate) fn set_promotion_backlog(&self, backlog: u64) {
		if let Some(inner) = &self.inner {
			inner.promotion_backlog.set(backlog);
		}
	}

	/// Count one completed maintenance cycle (liveness signal).
	pub(crate) fn record_maintenance_tick(&self) {
		if let Some(inner) = &self.inner {
			inner.maintenance_ticks_total.inc();
		}
	}

	/// Counter value of `substrate_hop_pool_removed_total{reason}`.
	#[cfg(test)]
	pub(crate) fn removed_count(&self, reason: &str) -> u64 {
		self.inner
			.as_ref()
			.map(|i| i.pool_removed_total.with_label_values(&[reason]).get())
			.unwrap_or(0)
	}

	/// Current value of the pool entries / bytes gauges.
	#[cfg(test)]
	pub(crate) fn pool_gauges(&self) -> (u64, u64) {
		self.inner
			.as_ref()
			.map(|i| (i.pool_entries.get(), i.pool_bytes.get()))
			.unwrap_or((0, 0))
	}

	/// Counter value of `substrate_hop_promotions_confirmed_total`.
	#[cfg(test)]
	pub(crate) fn promotions_confirmed(&self) -> u64 {
		self.inner.as_ref().map(|i| i.promotions_confirmed_total.get()).unwrap_or(0)
	}

	/// Current value of the `substrate_hop_promotion_backlog` gauge.
	#[cfg(test)]
	pub(crate) fn promotion_backlog(&self) -> u64 {
		self.inner.as_ref().map(|i| i.promotion_backlog.get()).unwrap_or(0)
	}

	/// Counter value of `substrate_hop_rpc_errors_total{method,reason}`.
	#[cfg(test)]
	pub(crate) fn rpc_error_count(&self, method: &str, reason: &str) -> u64 {
		self.inner
			.as_ref()
			.map(|i| i.rpc_errors_total.with_label_values(&[method, reason]).get())
			.unwrap_or(0)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn no_registry_or_disabled_means_no_ops() {
		assert!(!HopMetrics::new(None).unwrap().is_enabled());

		let metrics = HopMetrics::disabled();
		metrics.set_pool_status(1, 2, 3);
		metrics.set_pool_size(1, 2);
		metrics.record_inserted_bytes(42);
		metrics.record_removed(removal_reasons::ACKED, 1);
		metrics.record_rpc_error(rpc_methods::SUBMIT, &HopError::NotFound);
		metrics.record_promotion_confirmed();
		metrics.set_promotion_backlog(7);
		metrics.record_maintenance_tick();
		assert_eq!(metrics.removed_count(removal_reasons::ACKED), 0);
	}

	#[test]
	fn duplicate_registration_fails_and_falls_back_to_disabled() {
		let registry = Registry::new();
		assert!(HopMetrics::new(Some(&registry)).unwrap().is_enabled());

		// `HopParams::build_pool` degrades this error into disabled metrics.
		assert!(matches!(HopMetrics::new(Some(&registry)), Err(PrometheusError::AlreadyReg)));

		let fallback = HopMetrics::new(Some(&registry)).unwrap_or_else(|_| HopMetrics::disabled());
		assert!(!fallback.is_enabled());
	}

	#[test]
	fn removal_series_are_pre_created_at_registration() {
		let registry = Registry::new();
		let _metrics = HopMetrics::new(Some(&registry)).unwrap();

		let family = registry
			.gather()
			.into_iter()
			.find(|f| f.get_name() == "substrate_hop_pool_removed_total")
			.expect("family is registered");
		assert_eq!(family.get_metric().len(), removal_reasons::ALL.len());
	}

	#[test]
	fn enabled_metrics_track_values() {
		let registry = Registry::new();
		let metrics = HopMetrics::new(Some(&registry)).unwrap();

		metrics.set_pool_status(2, 200, 1000);
		metrics.set_pool_size(3, 300);
		metrics.record_inserted_bytes(100);
		metrics.record_removed(removal_reasons::EXPIRED_UNPROMOTED, 2);
		metrics.record_rpc_error(rpc_methods::CLAIM, &HopError::NotFound);
		metrics.record_promotion_confirmed();
		metrics.set_promotion_backlog(4);
		metrics.record_maintenance_tick();

		let inner = metrics.inner.as_ref().unwrap();
		assert_eq!(metrics.pool_gauges(), (3, 300));
		assert_eq!(inner.pool_max_bytes.get(), 1000);
		assert_eq!(inner.pool_inserted_bytes_total.get(), 100);
		assert_eq!(metrics.removed_count(removal_reasons::EXPIRED_UNPROMOTED), 2);
		assert_eq!(metrics.removed_count(removal_reasons::ACKED), 0);
		assert_eq!(metrics.rpc_error_count(rpc_methods::CLAIM, "not_found"), 1);
		assert_eq!(metrics.promotions_confirmed(), 1);
		assert_eq!(metrics.promotion_backlog(), 4);
		assert_eq!(inner.maintenance_ticks_total.get(), 1);
	}
}
