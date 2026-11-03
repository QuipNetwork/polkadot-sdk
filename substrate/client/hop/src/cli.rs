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

//! HOP CLI parameters.
//!
//! ## Usage
//!
//! To integrate HOP into your Substrate node CLI, flatten these parameters:
//!
//! ```rust,ignore
//! use sc_hop::HopParams;
//!
//! #[derive(Debug, clap::Parser)]
//! pub struct Cli {
//!     // ... your other CLI fields ...
//!
//!     #[clap(flatten)]
//!     pub hop: HopParams,
//! }
//! ```

use crate::{
	metrics::HopMetrics,
	pool::HopDataPool,
	rate_limit::RateLimitConfig,
	types::{
		HopError, DEFAULT_BANDWIDTH_BURST_MIB, DEFAULT_BANDWIDTH_PER_MIN_MIB,
		DEFAULT_CHECK_INTERVAL_SECS, DEFAULT_MAX_POOL_SIZE_MIB, DEFAULT_MAX_USER_SIZE_MIB,
		DEFAULT_PROMOTION_BUFFER_SECS, DEFAULT_RETENTION_SECS, DEFAULT_SUBMIT_BURST,
		DEFAULT_SUBMIT_RATE_PER_MIN,
	},
};
use clap::Parser;
use prometheus_endpoint::Registry;
use std::{path::PathBuf, sync::Arc};

/// HOP (Hand-Off Protocol) configuration parameters
#[derive(Debug, Clone, Parser)]
pub struct HopParams {
	/// Enable HOP
	#[arg(id = "enable-hop", long = "enable-hop", default_value_t = false)]
	pub enabled: bool,

	/// HOP maximum data pool size in MiB. Must be at least 1.
	#[arg(
		long = "hop-max-pool-size",
		default_value_t = DEFAULT_MAX_POOL_SIZE_MIB,
		value_parser = clap::value_parser!(u64).range(1..),
	)]
	pub max_pool_size: u64,

	/// HOP maximum per-user pool size in MiB (hard cap, not scaled by active users). Must be at
	/// least 1.
	#[arg(
		long = "hop-max-user-size",
		default_value_t = DEFAULT_MAX_USER_SIZE_MIB,
		value_parser = clap::value_parser!(u64).range(1..),
	)]
	pub max_user_size: u64,

	/// HOP data retention period in seconds (24h = 86400s). Must be at least 1.
	#[arg(
		long = "hop-retention-secs",
		default_value_t = DEFAULT_RETENTION_SECS,
		value_parser = clap::value_parser!(u64).range(1..),
	)]
	pub retention_secs: u64,

	/// HOP expiry cleanup interval in seconds. Must be at least 1 (a value of 0
	/// would turn the maintenance loop into a CPU-burning busy loop).
	#[arg(
		long = "hop-check-interval",
		default_value_t = DEFAULT_CHECK_INTERVAL_SECS,
		value_parser = clap::value_parser!(u64).range(1..),
	)]
	pub check_interval: u64,

	/// Seconds before expiry at which to start promoting entries on-chain. Must be at least 1.
	#[arg(
		long = "hop-promotion-buffer-secs",
		default_value_t = DEFAULT_PROMOTION_BUFFER_SECS,
		value_parser = clap::value_parser!(u64).range(1..),
	)]
	pub promotion_buffer_secs: u64,

	/// Sustained per-account submit rate (requests per minute). Must be at least 1
	/// when rate limiting is enabled — use `--hop-disable-rate-limit` to turn it off.
	#[arg(
		long = "hop-submit-rate-per-min",
		default_value_t = DEFAULT_SUBMIT_RATE_PER_MIN,
		value_parser = clap::value_parser!(u32).range(1..),
	)]
	pub submit_rate_per_min: u32,

	/// Per-account submit burst size (requests). Must be at least 1.
	#[arg(
		long = "hop-submit-burst",
		default_value_t = DEFAULT_SUBMIT_BURST,
		value_parser = clap::value_parser!(u32).range(1..),
	)]
	pub submit_burst: u32,

	/// Sustained per-account bandwidth (MiB per minute). Must be at least 1
	/// when rate limiting is enabled — use `--hop-disable-rate-limit` to turn it off.
	#[arg(
		long = "hop-bandwidth-per-min-mib",
		default_value_t = DEFAULT_BANDWIDTH_PER_MIN_MIB,
		value_parser = clap::value_parser!(u64).range(1..),
	)]
	pub bandwidth_per_min_mib: u64,

	/// Per-account bandwidth burst size (MiB). Must be at least 1.
	#[arg(
		long = "hop-bandwidth-burst-mib",
		default_value_t = DEFAULT_BANDWIDTH_BURST_MIB,
		value_parser = clap::value_parser!(u64).range(1..),
	)]
	pub bandwidth_burst_mib: u64,

	/// Disable per-account submit rate limiting (intended for tests and dev nodes).
	#[arg(long = "hop-disable-rate-limit")]
	pub disable_rate_limit: bool,

	/// Directory for HOP persistent data storage.
	///
	/// If not specified, defaults to `<chain-data-dir>/hop`.
	#[arg(long = "hop-data-dir")]
	pub data_dir: Option<std::path::PathBuf>,
}

impl Default for HopParams {
	fn default() -> Self {
		Self {
			enabled: false,
			max_pool_size: DEFAULT_MAX_POOL_SIZE_MIB,
			max_user_size: DEFAULT_MAX_USER_SIZE_MIB,
			retention_secs: DEFAULT_RETENTION_SECS,
			check_interval: DEFAULT_CHECK_INTERVAL_SECS,
			promotion_buffer_secs: DEFAULT_PROMOTION_BUFFER_SECS,
			submit_rate_per_min: DEFAULT_SUBMIT_RATE_PER_MIN,
			submit_burst: DEFAULT_SUBMIT_BURST,
			bandwidth_per_min_mib: DEFAULT_BANDWIDTH_PER_MIN_MIB,
			bandwidth_burst_mib: DEFAULT_BANDWIDTH_BURST_MIB,
			disable_rate_limit: false,
			data_dir: None,
		}
	}
}

impl HopParams {
	/// Derive a [`RateLimitConfig`] from these CLI parameters.
	pub fn rate_limit_config(&self) -> RateLimitConfig {
		if self.disable_rate_limit {
			return RateLimitConfig::disabled();
		}
		RateLimitConfig {
			enabled: true,
			submit_rate_per_min: self.submit_rate_per_min,
			submit_burst: self.submit_burst,
			bandwidth_per_min: self.bandwidth_per_min_mib.saturating_mul(1024 * 1024),
			bandwidth_burst: self.bandwidth_burst_mib.saturating_mul(1024 * 1024),
		}
	}

	/// Build a HOP data pool from these CLI parameters, resolving the data directory.
	///
	/// The resolved data directory is [`Self::data_dir`] if set, otherwise
	/// `<database_path>/hop`; if neither is available, returns [`HopError::MissingDataDir`].
	/// Callers gate on whether HOP is enabled (e.g. via `--enable-hop`) before calling this.
	///
	/// Metrics are registered with `registry` when given; a registration failure
	/// only disables metrics, it never fails pool construction.
	pub fn build_pool(
		&self,
		database_path: Option<PathBuf>,
		registry: Option<&Registry>,
	) -> Result<Arc<HopDataPool>, HopError> {
		let data_dir = match &self.data_dir {
			Some(dir) => dir.clone(),
			None => database_path.ok_or(HopError::MissingDataDir)?.join("hop"),
		};

		tracing::info!(
			target: "hop",
			params = ?self,
			data_dir = %data_dir.display(),
			"Initializing HOP data pool",
		);

		let metrics = HopMetrics::new(registry).unwrap_or_else(|e| {
			tracing::warn!(
				target: "hop",
				error = %e,
				"Failed to register HOP metrics; continuing without metrics"
			);
			HopMetrics::disabled()
		});

		let pool = HopDataPool::new(
			self.max_pool_size.saturating_mul(1024 * 1024),
			self.max_user_size.saturating_mul(1024 * 1024),
			self.retention_secs,
			data_dir,
			self.rate_limit_config(),
			metrics,
		)?;

		tracing::info!(
			target: "hop",
			status = ?pool.status(),
			"HOP data pool initialized, RPC methods will be registered",
		);

		Ok(Arc::new(pool))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use clap::Parser;

	/// Wrap `HopParams` so we can drive `clap`'s parser with a synthetic argv.
	#[derive(Parser)]
	struct TestCli {
		#[clap(flatten)]
		hop: HopParams,
	}

	#[test]
	fn build_pool_without_any_dir_returns_missing_data_dir() {
		match HopParams::default().build_pool(None, None) {
			Err(HopError::MissingDataDir) => (),
			Err(other) => panic!("expected MissingDataDir, got: {other:?}"),
			Ok(_) => panic!("expected MissingDataDir, got Ok"),
		}
	}

	#[test]
	fn cli_rejects_zero_for_critical_numeric_parameters() {
		// Each of these parameters would, at zero, either lock the maintenance
		// loop into a busy spin, expire entries the same block they're created,
		// or break rate-limit math. clap must reject them at parse time.
		let zero_flags = [
			"--hop-max-pool-size",
			"--hop-max-user-size",
			"--hop-retention-secs",
			"--hop-check-interval",
			"--hop-promotion-buffer-secs",
			"--hop-submit-rate-per-min",
			"--hop-submit-burst",
			"--hop-bandwidth-per-min-mib",
			"--hop-bandwidth-burst-mib",
		];
		for flag in zero_flags {
			let argv = ["test-bin", flag, "0"];
			let result = TestCli::try_parse_from(argv);
			assert!(
				result.is_err(),
				"clap accepted zero for {flag} but it should have been rejected",
			);
		}
	}

	#[test]
	fn cli_accepts_one_for_critical_numeric_parameters() {
		let one_flags = ["--hop-max-pool-size", "--hop-retention-secs", "--hop-check-interval"];
		for flag in one_flags {
			let argv = ["test-bin", flag, "1"];
			TestCli::try_parse_from(argv).expect("parse should succeed");
		}
	}
}
