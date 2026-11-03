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

//! Per-account submit rate limiting for HOP.
//!
//! Two token buckets per `SenderId`: one counts requests, the other counts bytes.
//! Both must admit a call for it to proceed. Refill happens lazily on each check
//! using monotonic `Instant`s, so idle users never block a background task.

use crate::types::SenderId;
use parking_lot::{Mutex, RwLock};
use std::{
	collections::HashMap,
	sync::Arc,
	time::{Duration, Instant},
};

/// How long a rate-limit entry can sit untouched before maintenance evicts it.
const STALE_ENTRY_TTL: Duration = Duration::from_secs(3600);

/// A classic token bucket: `tokens` refills at `refill_per_sec` up to `capacity`.
#[derive(Debug, Clone)]
struct TokenBucket {
	tokens: f64,
	capacity: f64,
	refill_per_sec: f64,
	last: Instant,
}

impl TokenBucket {
	fn new(capacity: f64, refill_per_sec: f64) -> Self {
		Self { tokens: capacity, capacity, refill_per_sec, last: Instant::now() }
	}

	/// Refill based on elapsed time and cap at `capacity`.
	fn refill(&mut self, now: Instant) {
		let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
		if elapsed > 0.0 {
			self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
			self.last = now;
		}
	}

	/// Try to consume `n` tokens. On failure returns the `Duration` until enough
	/// tokens will have refilled to satisfy the request.
	fn try_consume(&mut self, n: f64, now: Instant) -> Result<(), Duration> {
		self.refill(now);
		if self.tokens >= n {
			self.tokens -= n;
			Ok(())
		} else {
			let deficit = n - self.tokens;
			let secs =
				if self.refill_per_sec > 0.0 { deficit / self.refill_per_sec } else { f64::MAX };
			Err(Duration::from_secs_f64(secs.clamp(0.0, u64::MAX as f64)))
		}
	}
}

#[derive(Debug)]
struct UserRateState {
	requests: TokenBucket,
	bandwidth: TokenBucket,
	last_touch: Instant,
}

/// Configuration for the per-account submit rate limiter.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
	/// If false, `RateLimiter::check` always admits immediately.
	pub enabled: bool,
	/// Sustained requests per account per minute.
	pub submit_rate_per_min: u32,
	/// Burst size for the request bucket.
	pub submit_burst: u32,
	/// Sustained bytes per account per minute.
	pub bandwidth_per_min: u64,
	/// Burst size for the bandwidth bucket, in bytes.
	pub bandwidth_burst: u64,
}

impl RateLimitConfig {
	/// Create a disabled config (admit everything).
	pub fn disabled() -> Self {
		Self {
			enabled: false,
			submit_rate_per_min: 0,
			submit_burst: 0,
			bandwidth_per_min: 0,
			bandwidth_burst: 0,
		}
	}
}

/// Per-account token-bucket rate limiter for HOP submissions.
pub struct RateLimiter {
	cfg: RateLimitConfig,
	users: RwLock<HashMap<SenderId, Arc<Mutex<UserRateState>>>>,
}

impl RateLimiter {
	/// Build a rate limiter from configuration.
	pub fn new(cfg: RateLimitConfig) -> Self {
		Self { cfg, users: RwLock::new(HashMap::new()) }
	}

	fn new_state(&self, now: Instant) -> UserRateState {
		let requests = TokenBucket::new(
			self.cfg.submit_burst as f64,
			self.cfg.submit_rate_per_min as f64 / 60.0,
		);
		let bandwidth = TokenBucket::new(
			self.cfg.bandwidth_burst as f64,
			self.cfg.bandwidth_per_min as f64 / 60.0,
		);
		UserRateState { requests, bandwidth, last_touch: now }
	}

	fn get_or_create(&self, sender_id: &SenderId, now: Instant) -> Arc<Mutex<UserRateState>> {
		if let Some(state) = self.users.read().get(sender_id).cloned() {
			return state;
		}
		let mut users = self.users.write();
		users
			.entry(*sender_id)
			.or_insert_with(|| Arc::new(Mutex::new(self.new_state(now))))
			.clone()
	}

	/// Check whether this account may submit `data_len` bytes right now.
	///
	/// Returns `Ok(())` on admission (tokens consumed) or `Err(retry_after_secs)`
	/// when either bucket is empty.
	pub fn check(&self, sender_id: &SenderId, data_len: u64) -> Result<(), u64> {
		if !self.cfg.enabled {
			return Ok(());
		}

		let now = Instant::now();
		let state = self.get_or_create(sender_id, now);
		let mut state = state.lock();
		state.last_touch = now;

		let req_wait = state.requests.try_consume(1.0, now).err();
		if let Some(wait) = req_wait {
			return Err(wait.as_secs().max(1));
		}

		// If the bandwidth bucket is exhausted, reject immediately and refund the request
		// token so both buckets stay consistent.
		if let Err(wait) = state.bandwidth.try_consume(data_len as f64, now) {
			// Refund the request token we just took so the two buckets stay consistent.
			state.requests.tokens = (state.requests.tokens + 1.0).min(state.requests.capacity);
			return Err(wait.as_secs().max(1));
		}

		Ok(())
	}

	/// Drop entries that haven't been touched in `STALE_ENTRY_TTL`.
	/// Called from the pool's maintenance loop.
	pub fn evict_stale(&self) {
		if !self.cfg.enabled {
			return;
		}
		let now = Instant::now();
		let mut users = self.users.write();
		users.retain(|_, state| {
			let state = state.lock();
			now.saturating_duration_since(state.last_touch) < STALE_ENTRY_TTL
		});
	}

	/// Number of tracked senders (for tests / metrics).
	pub fn tracked_senders(&self) -> usize {
		self.users.read().len()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const SENDER_A: SenderId = [1u8; 32];
	const SENDER_B: SenderId = [2u8; 32];

	fn test_cfg() -> RateLimitConfig {
		RateLimitConfig {
			enabled: true,
			submit_rate_per_min: 60,
			submit_burst: 3,
			bandwidth_per_min: 6_000,
			bandwidth_burst: 6_000,
		}
	}

	#[test]
	fn disabled_admits_everything() {
		let rl = RateLimiter::new(RateLimitConfig::disabled());
		for _ in 0..100 {
			rl.check(&SENDER_A, 1_000_000_000).unwrap();
		}
	}

	#[test]
	fn burst_then_limited() {
		let rl = RateLimiter::new(test_cfg());
		// Burst of 3.
		rl.check(&SENDER_A, 100).unwrap();
		rl.check(&SENDER_A, 100).unwrap();
		rl.check(&SENDER_A, 100).unwrap();
		// 4th is limited.
		let err = rl.check(&SENDER_A, 100).unwrap_err();
		assert!(err >= 1);
	}

	#[test]
	fn bandwidth_exhaustion_limits() {
		let rl = RateLimiter::new(test_cfg());
		// Consume all 6000 bytes of burst in one call.
		rl.check(&SENDER_A, 6_000).unwrap();
		// Next call, even 1 byte, should be rejected.
		assert!(rl.check(&SENDER_A, 1).is_err());
	}

	#[test]
	fn isolated_per_sender() {
		let rl = RateLimiter::new(test_cfg());
		for _ in 0..3 {
			rl.check(&SENDER_A, 100).unwrap();
		}
		// A is limited, B is fresh.
		assert!(rl.check(&SENDER_A, 100).is_err());
		rl.check(&SENDER_B, 100).unwrap();
	}

	#[test]
	fn refills_over_time() {
		let cfg = RateLimitConfig {
			enabled: true,
			submit_rate_per_min: 60,
			submit_burst: 1,
			bandwidth_per_min: 600_000,
			bandwidth_burst: 600_000,
		};
		let rl = RateLimiter::new(cfg);
		rl.check(&SENDER_A, 100).unwrap();
		assert!(rl.check(&SENDER_A, 100).is_err());

		// Fake a 2-second advance by mutating the bucket's `last`.
		{
			let state = rl.get_or_create(&SENDER_A, Instant::now());
			let mut state = state.lock();
			state.requests.last -= Duration::from_secs(2);
		}
		// Should now succeed (1 request/sec refill, 2 seconds elapsed).
		rl.check(&SENDER_A, 100).unwrap();
	}

	#[test]
	fn evict_stale_removes_untouched() {
		let rl = RateLimiter::new(test_cfg());
		rl.check(&SENDER_A, 100).unwrap();
		assert_eq!(rl.tracked_senders(), 1);

		// Backdate last_touch.
		{
			let state = rl.get_or_create(&SENDER_A, Instant::now());
			let mut state = state.lock();
			state.last_touch -= STALE_ENTRY_TTL + Duration::from_secs(1);
		}
		rl.evict_stale();
		assert_eq!(rl.tracked_senders(), 0);
	}
}
