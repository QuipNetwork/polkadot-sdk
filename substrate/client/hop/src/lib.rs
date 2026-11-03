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

//! # `sc-hop` — Hand-Off Protocol
//!
//! Node-level ephemeral disk-backed data pool for Substrate collators, with an
//! RPC for submit/claim/ack, best-effort on-chain promotion, per-account rate
//! limiting, and graceful degradation when the runtime lacks `HopRuntimeApi`.
//!
//! See the crate [`README`] for the design overview, integration guide, CLI
//! flags, RPC reference, and error codes.
//!
//! [`README`]: https://github.com/paritytech/polkadot-sdk/blob/master/substrate/client/hop/README.md

pub mod cli;
pub mod metrics;
pub mod pool;
pub mod promotion;
pub mod rate_limit;
pub mod rpc;
pub mod runtime_api;
pub mod types;

// Convenience re-exports for common use cases
pub use cli::HopParams;
pub use metrics::HopMetrics;
pub use pool::HopDataPool;
pub use promotion::{build_maintenance_task, HopMaintenanceTask};
pub use rate_limit::{RateLimitConfig, RateLimiter};
pub use rpc::{HopApiServer, HopRpcServer};
pub use types::{
	HopBlockNumber, HopEntryMeta, HopError, HopHash, PoolStatus, SenderId, SubmitResult,
};
