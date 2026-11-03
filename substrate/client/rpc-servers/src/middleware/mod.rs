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

//! JSON-RPC specific middleware.

use std::{
	num::NonZeroU32,
	time::{Duration, Instant},
};

use futures::future::{BoxFuture, FutureExt};
use governor::{clock::Clock, Jitter};
use jsonrpsee::{
	server::middleware::rpc::RpcServiceT,
	types::{ErrorObject, Id, Request},
	MethodResponse,
};

mod metrics;
mod node_health;
mod rate_limit;

pub use metrics::*;
pub use node_health::*;
pub use rate_limit::*;

const MAX_JITTER: Duration = Duration::from_millis(50);
const MAX_RETRIES: usize = 10;

/// JSON-RPC middleware layer.
#[derive(Debug, Clone, Default)]
pub struct MiddlewareLayer {
	rate_limit: Option<RateLimit>,
	metrics: Option<Metrics>,
}

impl MiddlewareLayer {
	/// Create an empty MiddlewareLayer.
	pub fn new() -> Self {
		Self::default()
	}

	/// Enable new rate limit middleware enforced per minute.
	pub fn with_rate_limit_per_minute(self, n: NonZeroU32) -> Self {
		Self { rate_limit: Some(RateLimit::per_minute(n)), metrics: self.metrics }
	}

	/// Enable metrics middleware.
	pub fn with_metrics(self, metrics: Metrics) -> Self {
		Self { rate_limit: self.rate_limit, metrics: Some(metrics) }
	}

	/// Register a new websocket connection.
	pub fn ws_connect(&self) {
		self.metrics.as_ref().map(|m| m.ws_connect());
	}

	/// Register that a websocket connection was closed.
	pub fn ws_disconnect(&self, now: Instant) {
		self.metrics.as_ref().map(|m| m.ws_disconnect(now));
	}
}

impl<S> tower::Layer<S> for MiddlewareLayer {
	type Service = Middleware<S>;

	fn layer(&self, service: S) -> Self::Service {
		Middleware { service, rate_limit: self.rate_limit.clone(), metrics: self.metrics.clone() }
	}
}

/// JSON-RPC middleware that handles metrics
/// and rate-limiting.
///
/// These are part of the same middleware
/// because the metrics needs to know whether
/// a call was rate-limited or not because
/// it will impact the roundtrip for a call.
pub struct Middleware<S> {
	service: S,
	rate_limit: Option<RateLimit>,
	metrics: Option<Metrics>,
}

impl<'a, S> RpcServiceT<'a> for Middleware<S>
where
	S: Send + Sync + RpcServiceT<'a> + Clone + 'static,
{
	type Future = BoxFuture<'a, MethodResponse>;

	fn call(&self, req: Request<'a>) -> Self::Future {
		let now = Instant::now();

		self.metrics.as_ref().map(|m| m.on_call(&req));

		let service = self.service.clone();
		let rate_limit = self.rate_limit.clone();
		let metrics = self.metrics.clone();

		async move {
			let mut is_rate_limited = false;

			if let Some(limit) = rate_limit.as_ref() {
				let mut attempts = 0;
				let jitter = Jitter::up_to(MAX_JITTER);

				loop {
					if attempts >= MAX_RETRIES {
						metrics.as_ref().map(|m| m.on_rejected(&req));
						return reject_too_many_calls(req.id);
					}

					if let Err(rejected) = limit.inner.check() {
						metrics.as_ref().map(|m| m.on_retry(&req));
						tokio::time::sleep(jitter + rejected.wait_time_from(limit.clock.now()))
							.await;
					} else {
						break;
					}

					is_rate_limited = true;
					attempts += 1;
				}
			}

			let rp = service.call(req.clone()).await;
			metrics.as_ref().map(|m| m.on_response(&req, &rp, is_rate_limited, now));

			rp
		}
		.boxed()
	}
}

fn reject_too_many_calls(id: Id) -> MethodResponse {
	MethodResponse::error(id, ErrorObject::owned(-32999, "RPC rate limit exceeded", None::<()>))
}
