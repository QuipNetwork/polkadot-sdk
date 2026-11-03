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

//! Clock abstraction shared by Polkadot node subsystems.
//!
//! Production code uses [`SystemClock`]. Tests inject a deterministic mock so subsystems'
//! time-dependent behavior can be driven and observed without wall-clock dependence.
//!
//! Subsystems that need time should accept `Arc<dyn Clock>` rather than reading
//! `Instant::now()`, `SystemTime::now()`, `futures_timer::Delay::new(..)`, or
//! `tokio::time::sleep(..)` directly. Consider enforcing this via a crate-level
//! `clippy.toml` (see `polkadot-collator-protocol` for an example).
//!
//! Note: not every subsystem currently routes all of its time reads through this
//! trait. In particular, `polkadot-node-core-chain-selection`,
//! `polkadot-node-core-av-store`, and `polkadot-node-core-dispute-coordinator`
//! presently use this trait only for their wall-clock reads and continue to call
//! `futures_timer::Delay::new` directly for their internal timers. Those call sites
//! need to be migrated before those subsystems can run under a deterministic test
//! harness.

#![deny(missing_docs)]
#![deny(unused_crate_dependencies)]

use std::{
	future::Future,
	pin::Pin,
	sync::Arc,
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Boxed future returned by [`Clock::delay`].
pub type BoxedDelay = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Abstraction over wall-clock time. See module-level docs.
pub trait Clock: Send + Sync {
	/// Monotonic timestamp suitable for measuring durations between two reads.
	fn now(&self) -> Instant;

	/// Future that resolves after `dur` has elapsed in this clock's frame.
	fn delay(&self, dur: Duration) -> BoxedDelay;

	/// Wall-clock duration since the UNIX epoch. Used for slot math and persistence
	/// timestamps; not monotonic. Callers pick a granularity (`as_secs`, `as_millis`)
	/// at the call site.
	fn duration_since_epoch(&self) -> Duration;
}

/// Production clock backed by `std::time` and `futures_timer`.
pub struct SystemClock;

impl Clock for SystemClock {
	fn now(&self) -> Instant {
		Instant::now()
	}

	fn delay(&self, dur: Duration) -> BoxedDelay {
		Box::pin(futures_timer::Delay::new(dur))
	}

	fn duration_since_epoch(&self) -> Duration {
		SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.expect("system time is before the UNIX epoch; check the system clock;")
	}
}

/// Convenience constructor returning a thread-safe handle to a [`SystemClock`].
pub fn system_clock() -> Arc<dyn Clock> {
	Arc::new(SystemClock)
}

#[cfg(feature = "test")]
pub mod mock;
#[cfg(feature = "test")]
pub use mock::MockClock;
