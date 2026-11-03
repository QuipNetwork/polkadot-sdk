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

//! Statement store Prometheus metrics.

use std::sync::Arc;

use prometheus_endpoint::{
	prometheus::HistogramTimer, register, Counter, CounterVec, Gauge, Histogram, HistogramOpts,
	Opts, PrometheusError, Registry, U64,
};

#[derive(Clone, Default)]
pub struct MetricsLink(Arc<Option<Metrics>>);

impl MetricsLink {
	pub fn new(registry: Option<&Registry>) -> Self {
		Self(Arc::new(registry.and_then(|registry| {
			Metrics::register(registry)
				.map_err(|err| {
					log::warn!("Failed to register prometheus metrics: {}", err);
				})
				.ok()
		})))
	}

	pub fn report(&self, do_this: impl FnOnce(&Metrics)) {
		if let Some(metrics) = self.0.as_ref() {
			do_this(metrics);
		}
	}

	pub fn start_submit_timer(&self) -> Option<HistogramTimer> {
		self.0.as_ref().as_ref().map(|m| m.submit_duration_seconds.start_timer())
	}

	pub fn start_check_expiration_timer(&self) -> Option<HistogramTimer> {
		self.0
			.as_ref()
			.as_ref()
			.map(|m| m.check_expiration_duration_seconds.start_timer())
	}
}

/// Statement store Prometheus metrics.
pub struct Metrics {
	pub submitted_statements: Counter<U64>,
	pub validations_invalid: CounterVec<U64>,
	pub statements_pruned: Counter<U64>,
	pub statements_total: Gauge<U64>,
	pub bytes_total: Gauge<U64>,
	pub accounts_total: Gauge<U64>,
	pub expired_total: Gauge<U64>,
	pub capacity_statements: Gauge<U64>,
	pub capacity_bytes: Gauge<U64>,
	pub rejections: CounterVec<U64>,
	pub internal_errors: CounterVec<U64>,
	pub known_statements: CounterVec<U64>,
	pub submit_duration_seconds: Histogram,
	pub check_expiration_duration_seconds: Histogram,
	pub statements_expired_total: Counter<U64>,
}

impl Metrics {
	pub fn register(registry: &Registry) -> Result<Self, PrometheusError> {
		Ok(Self {
			submitted_statements: register(
				Counter::new(
					"substrate_sub_statement_store_submitted_statements",
					"Total number of new statements successfully accepted into the store",
				)?,
				registry,
			)?,
			validations_invalid: register(
				CounterVec::new(
					Opts::new(
						"substrate_sub_statement_store_validations_invalid",
						"Total statement validation failures during submission, by reason",
					),
					&["reason"],
				)?,
				registry,
			)?,
			statements_pruned: register(
				Counter::new(
					"substrate_sub_statement_store_statements_pruned_total",
					"Total number of expired-statement hashes pruned after the grace period, leaving no trace of the statement",
				)?,
				registry,
			)?,
			statements_total: register(
				Gauge::new(
					"substrate_sub_statement_store_statements_total",
					"Current number of statements in the store",
				)?,
				registry,
			)?,
			capacity_statements: register(
				Gauge::new(
					"substrate_sub_statement_store_capacity_statements",
					"Maximum number of statements the store can hold",
				)?,
				registry,
			)?,

			capacity_bytes: register(
				Gauge::new(
					"substrate_sub_statement_store_capacity_bytes",
					"Maximum total size of statement data in bytes",
				)?,
				registry,
			)?,
			bytes_total: register(
				Gauge::new(
					"substrate_sub_statement_store_bytes_total",
					"Current total size of all statement data in bytes",
				)?,
				registry,
			)?,
			accounts_total: register(
				Gauge::new(
					"substrate_sub_statement_store_accounts_total",
					"Current number of unique accounts with statements",
				)?,
				registry,
			)?,
			expired_total: register(
				Gauge::new(
					"substrate_sub_statement_store_expired_total",
					"Current number of expired statements awaiting purge",
				)?,
				registry,
			)?,
			rejections: register(
				CounterVec::new(
					Opts::new(
						"substrate_sub_statement_store_rejections_total",
						"Total statement rejections by reason",
					),
					&["reason"],
				)?,
				registry,
			)?,
			internal_errors: register(
				CounterVec::new(
					Opts::new(
						"substrate_sub_statement_store_internal_errors_total",
						"Total internal errors during statement submission by source",
					),
					&["source"],
				)?,
				registry,
			)?,
			known_statements: register(
				CounterVec::new(
					Opts::new(
						"substrate_sub_statement_store_known_statements_total",
						"Total submissions of statements already known to the store, by prior state",
					),
					&["state"],
				)?,
				registry,
			)?,
			submit_duration_seconds: register(
				Histogram::with_opts(
					HistogramOpts::new(
						"substrate_sub_statement_store_submit_duration_seconds",
						"Time to submit a statement",
					)
					.buckets(vec![0.000_001, 0.000_01, 0.000_1, 0.001, 0.01, 0.1, 1.0]),
				)?,
				registry,
			)?,
			check_expiration_duration_seconds: register(
				Histogram::with_opts(
					HistogramOpts::new(
						"substrate_sub_statement_store_check_expiration_duration_seconds",
						"Time to check and process statement expiration",
					)
					.buckets(vec![0.000_001, 0.000_01, 0.000_1, 0.001, 0.01, 0.1, 1.0]),
				)?,
				registry,
			)?,
			statements_expired_total: register(
				Counter::new(
					"substrate_sub_statement_store_statements_expired_total",
					"Total number of statements that expired and were removed; the hash is retained to be pruned after the grace period",
				)?,
				registry,
			)?,
		})
	}
}
