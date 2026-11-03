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

//! Substrate RPC servers.

#![warn(missing_docs)]

pub mod middleware;
pub mod utils;

use std::{error::Error as StdError, net::SocketAddr, sync::Arc, time::Duration};

use futures::future::BoxFuture;
use jsonrpsee::{
	core::BoxError,
	server::{
		serve_with_graceful_shutdown, stop_channel, ws, PingConfig, ServerHandle, StopHandle,
	},
	Methods, RpcModule,
};
use tower::Service;
use utils::{
	build_rpc_api, deny_unsafe, format_listen_addrs, get_proxy_ip, ListenAddrError, RpcSettings,
};

pub use ip_network::IpNetwork;
pub use jsonrpsee::{
	core::id_providers::{RandomIntegerIdProvider, RandomStringIdProvider},
	server::{middleware::rpc::RpcServiceBuilder, BatchRequestConfig},
};
pub use middleware::{Metrics, MiddlewareLayer, NodeHealthProxyLayer, RpcMetrics};
pub use utils::{RpcEndpoint, RpcMethods};

const MEGABYTE: u32 = 1024 * 1024;

/// Creates a dedicated tokio runtime for RPC operations.
///
/// This runtime isolates RPC blocking operations from the rest of the node
/// by limiting the number of blocking threads to `max_connections`.
pub fn create_rpc_runtime(max_connections: u32) -> std::io::Result<tokio::runtime::Runtime> {
	tokio::runtime::Builder::new_multi_thread()
		.thread_name("rpc")
		.enable_all()
		.max_blocking_threads((max_connections as usize).max(1))
		.on_thread_start(|| {
			sc_utils::metrics::TOKIO_THREADS_ALIVE.inc();
			sc_utils::metrics::TOKIO_THREADS_TOTAL.inc();
			middleware::RPC_THREADS_ALIVE.inc();
			middleware::RPC_THREADS_TOTAL.inc();
		})
		.on_thread_stop(|| {
			sc_utils::metrics::TOKIO_THREADS_ALIVE.dec();
			middleware::RPC_THREADS_ALIVE.dec();
		})
		.build()
}

/// Spawn handle for RPC tasks that uses the dedicated RPC runtime.
///
/// This ensures all RPC-related task spawning (including rpc-spec-v2 APIs like
/// chainHead and transactionWatch) runs on the isolated RPC runtime rather than
/// the main node runtime.
#[derive(Clone)]
pub struct RpcSpawnHandle {
	handle: tokio::runtime::Handle,
}

impl RpcSpawnHandle {
	/// Create a new RpcSpawnHandle from a tokio runtime handle.
	pub fn new(handle: tokio::runtime::Handle) -> Self {
		Self { handle }
	}
}

impl sp_core::traits::SpawnNamed for RpcSpawnHandle {
	fn spawn_blocking(
		&self,
		_name: &'static str,
		_group: Option<&'static str>,
		future: BoxFuture<'static, ()>,
	) {
		let handle = self.handle.clone();
		self.handle.spawn_blocking(move || {
			handle.block_on(future);
		});
	}

	fn spawn(
		&self,
		_name: &'static str,
		_group: Option<&'static str>,
		future: BoxFuture<'static, ()>,
	) {
		self.handle.spawn(future);
	}
}

/// Type to encapsulate the server handle and listening address.
pub struct Server {
	/// Handle to the rpc server
	handle: ServerHandle,
	/// Listening address of the server
	listen_addrs: Vec<SocketAddr>,
	/// Dedicated RPC runtime (kept alive for the lifetime of the server)
	rpc_runtime: Option<tokio::runtime::Runtime>,
}

impl Server {
	/// Creates a new Server.
	pub fn new(
		handle: ServerHandle,
		listen_addrs: Vec<SocketAddr>,
		rpc_runtime: tokio::runtime::Runtime,
	) -> Server {
		Server { handle, listen_addrs, rpc_runtime: Some(rpc_runtime) }
	}

	/// Returns the `jsonrpsee::server::ServerHandle` for this Server. Can be used to stop the
	/// server.
	pub fn handle(&self) -> &ServerHandle {
		&self.handle
	}

	/// The listen address for the running RPC service.
	pub fn listen_addrs(&self) -> &[SocketAddr] {
		&self.listen_addrs
	}

	/// Returns the spawn handle for tasks on the dedicated RPC runtime.
	pub fn spawn_handle(&self) -> Arc<dyn sp_core::traits::SpawnNamed> {
		Arc::new(RpcSpawnHandle::new(
			self.rpc_runtime
				.as_ref()
				.expect("rpc_runtime is only taken in Drop; qed")
				.handle()
				.clone(),
		))
	}
}

impl Drop for Server {
	fn drop(&mut self) {
		// This doesn't not wait for the server to be stopped but fires the signal.
		let _ = self.handle.stop();

		// Use `shutdown_background()` to avoid blocking, which would panic if
		// we are being dropped from within an async context.
		if let Some(runtime) = self.rpc_runtime.take() {
			runtime.shutdown_background();
		}
	}
}

/// Trait for providing subscription IDs that can be cloned.
pub trait SubscriptionIdProvider:
	jsonrpsee::core::traits::IdProvider + dyn_clone::DynClone
{
}

dyn_clone::clone_trait_object!(SubscriptionIdProvider);

/// RPC server configuration.
pub struct Config<M: Send + Sync + 'static> {
	/// RPC interfaces to start.
	pub endpoints: Vec<RpcEndpoint>,
	/// Metrics.
	pub metrics: Option<RpcMetrics>,
	/// RPC API module.
	pub rpc_api: RpcModule<M>,
	/// Subscription ID provider.
	pub id_provider: Option<Box<dyn SubscriptionIdProvider>>,
	/// RPC logger capacity (default: 1024).
	pub request_logger_limit: u32,
	/// Dedicated RPC runtime.
	pub rpc_runtime: tokio::runtime::Runtime,
}

/// Guards the dedicated RPC runtime while [`start_server`] is setting the server up.
///
/// `start_server` is awaited from within a runtime context, so dropping the RPC runtime on an
/// early error return would panic: `Runtime::drop` blocks until the blocking pool has shut down,
/// which tokio forbids on a runtime thread. Routing every exit path through this guard tears the
/// runtime down with `shutdown_background()` instead, just like [`Server::drop`] does.
struct RuntimeGuard(Option<tokio::runtime::Runtime>);

impl RuntimeGuard {
	fn new(runtime: tokio::runtime::Runtime) -> Self {
		Self(Some(runtime))
	}

	fn handle(&self) -> tokio::runtime::Handle {
		self.0
			.as_ref()
			.expect("runtime is only taken in `into_inner` and `Drop`; qed")
			.handle()
			.clone()
	}

	/// Hands the runtime over to the [`Server`], which owns it from then on.
	fn into_inner(mut self) -> tokio::runtime::Runtime {
		self.0.take().expect("runtime is only taken here and in `Drop`; qed")
	}
}

impl Drop for RuntimeGuard {
	fn drop(&mut self) {
		if let Some(runtime) = self.0.take() {
			// Same trade-off `Server::drop` accepts
			runtime.shutdown_background();
		}
	}
}

#[derive(Debug, Clone)]
struct PerConnection {
	methods: Methods,
	stop_handle: StopHandle,
	metrics: Option<RpcMetrics>,
	tokio_handle: tokio::runtime::Handle,
}

/// Start RPC server listening on given address.
pub async fn start_server<M>(config: Config<M>) -> Result<Server, Box<dyn StdError + Send + Sync>>
where
	M: Send + Sync,
{
	let Config { endpoints, metrics, rpc_api, id_provider, request_logger_limit, rpc_runtime } =
		config;

	// Held as a guard so that every early error return below shuts the runtime down
	let rpc_runtime = RuntimeGuard::new(rpc_runtime);
	let rpc_handle = rpc_runtime.handle();

	let (stop_handle, server_handle) = stop_channel();
	let rpc_api = build_rpc_api(rpc_api);
	// Bound the metrics `method` label to the registered methods so its
	// cardinality stays finite (unknown names collapse to `"unknown"`).
	let metrics = metrics.map(|m| {
		let known: Vec<&'static str> = rpc_api.method_names().collect();
		m.with_known_methods(known)
	});
	let cfg = PerConnection {
		methods: rpc_api.into(),
		metrics,
		tokio_handle: rpc_handle.clone(),
		stop_handle,
	};

	let mut local_addrs = Vec::new();

	for endpoint in endpoints {
		let allowed_to_fail = endpoint.is_optional;
		let local_addr = endpoint.listen_addr;

		let mut listener = match endpoint.bind().await {
			Ok(l) => l,
			Err(e) if allowed_to_fail => {
				log::debug!(target: "rpc", "JSON-RPC server failed to bind optional address: {:?}, error: {:?}", local_addr, e);
				continue;
			},
			Err(e) => return Err(e),
		};
		let local_addr = listener.local_addr();
		local_addrs.push(local_addr);
		let cfg = cfg.clone();

		let RpcSettings {
			batch_config,
			max_connections,
			max_payload_in_mb,
			max_payload_out_mb,
			max_buffer_capacity_per_connection,
			max_subscriptions_per_connection,
			rpc_methods,
			rate_limit_trust_proxy_headers,
			rate_limit_whitelisted_ips,
			host_filter,
			cors,
			rate_limit,
		} = listener.rpc_settings();

		let http_middleware = tower::ServiceBuilder::new()
			.option_layer(host_filter)
			// Proxy `GET /health, /health/readiness` requests to the internal
			// `system_health` method.
			.layer(NodeHealthProxyLayer::default())
			.layer(cors);

		let mut builder = jsonrpsee::server::Server::builder()
			.max_request_body_size(max_payload_in_mb.saturating_mul(MEGABYTE))
			.max_response_body_size(max_payload_out_mb.saturating_mul(MEGABYTE))
			.max_connections(max_connections)
			.max_subscriptions_per_connection(max_subscriptions_per_connection)
			.enable_ws_ping(
				PingConfig::new()
					.ping_interval(Duration::from_secs(30))
					.inactive_limit(Duration::from_secs(60))
					.max_failures(3),
			)
			.set_http_middleware(http_middleware)
			.set_message_buffer_capacity(max_buffer_capacity_per_connection)
			.set_batch_request_config(batch_config)
			.custom_tokio_runtime(rpc_handle.clone());

		if let Some(provider) = id_provider.clone() {
			builder = builder.set_id_provider(provider);
		} else {
			builder = builder.set_id_provider(RandomStringIdProvider::new(16));
		};

		let service_builder = builder.to_service_builder();
		let deny_unsafe = deny_unsafe(&local_addr, &rpc_methods);

		rpc_handle.spawn(async move {
			loop {
				let (sock, remote_addr) = tokio::select! {
					res = listener.accept() => {
						match res {
							Ok(s) => s,
							Err(e) => {
								log::debug!(target: "rpc", "Failed to accept connection: {:?}", e);
								continue;
							}
						}
					}
					_ = cfg.stop_handle.clone().shutdown() => break,
				};

				let ip = remote_addr.ip();
				let cfg2 = cfg.clone();
				let service_builder2 = service_builder.clone();
				let rate_limit_whitelisted_ips2 = rate_limit_whitelisted_ips.clone();

				let svc =
					tower::service_fn(move |mut req: http::Request<hyper::body::Incoming>| {
						req.extensions_mut().insert(deny_unsafe);

						let PerConnection { methods, metrics, tokio_handle, stop_handle } =
							cfg2.clone();
						let service_builder = service_builder2.clone();

						let proxy_ip =
							if rate_limit_trust_proxy_headers { get_proxy_ip(&req) } else { None };

						let rate_limit_cfg = if rate_limit_whitelisted_ips2
							.iter()
							.any(|ips| ips.contains(proxy_ip.unwrap_or(ip)))
						{
							log::debug!(target: "rpc", "ip={ip}, proxy_ip={:?} is trusted, disabling rate-limit", proxy_ip);
							None
						} else {
							if !rate_limit_whitelisted_ips2.is_empty() {
								log::debug!(target: "rpc", "ip={ip}, proxy_ip={:?} is not trusted, rate-limit enabled", proxy_ip);
							}
							rate_limit
						};

						let is_websocket = ws::is_upgrade_request(&req);
						let transport_label = if is_websocket { "ws" } else { "http" };

						let middleware_layer = match (metrics, rate_limit_cfg) {
							(None, None) => None,
							(Some(metrics), None) => Some(
								MiddlewareLayer::new()
									.with_metrics(Metrics::new(metrics, transport_label)),
							),
							(None, Some(rate_limit)) =>
								Some(MiddlewareLayer::new().with_rate_limit_per_minute(rate_limit)),
							(Some(metrics), Some(rate_limit)) => Some(
								MiddlewareLayer::new()
									.with_metrics(Metrics::new(metrics, transport_label))
									.with_rate_limit_per_minute(rate_limit),
							),
						};

						let rpc_middleware = RpcServiceBuilder::new()
							.rpc_logger(request_logger_limit)
							.option_layer(middleware_layer.clone());
						let mut svc = service_builder
							.set_rpc_middleware(rpc_middleware)
							.build(methods, stop_handle);

						async move {
							if is_websocket {
								let on_disconnect = svc.on_session_closed();

								// Spawn a task to handle when the connection is closed.
								tokio_handle.spawn(async move {
									let now = std::time::Instant::now();
									middleware_layer.as_ref().map(|m| m.ws_connect());
									on_disconnect.await;
									middleware_layer.as_ref().map(|m| m.ws_disconnect(now));
								});
							}

							// https://github.com/rust-lang/rust/issues/102211 the error type can't be inferred
							// to be `Box<dyn std::error::Error + Send + Sync>` so we need to
							// convert it to a concrete type as workaround.
							svc.call(req).await.map_err(|e| BoxError::from(e))
						}
					});

				cfg.tokio_handle.spawn(serve_with_graceful_shutdown(
					sock,
					svc,
					cfg.stop_handle.clone().shutdown(),
				));
			}
		});
	}

	if local_addrs.is_empty() {
		return Err(Box::new(ListenAddrError));
	}

	// The previous logging format was before
	// `Running JSON-RPC server: addr=127.0.0.1:9944, allowed origins=["*"]`
	//
	// The new format is `Running JSON-RPC server: addr=<addr1, addr2, .. addr_n>`
	// with the exception that for a single address it will be `Running JSON-RPC server: addr=addr,`
	// with a trailing comma.
	//
	// This is to make it work with old scripts/utils that parse the logs.
	log::info!("Running JSON-RPC server: addr={}", format_listen_addrs(&local_addrs));

	Ok(Server::new(server_handle, local_addrs, rpc_runtime.into_inner()))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn test_runtime() -> tokio::runtime::Runtime {
		tokio::runtime::Builder::new_multi_thread()
			.worker_threads(1)
			.enable_all()
			.build()
			.expect("test runtime can be built; qed")
	}

	fn config(endpoints: Vec<RpcEndpoint>) -> Config<()> {
		Config {
			endpoints,
			metrics: None,
			rpc_api: RpcModule::new(()),
			id_provider: None,
			request_logger_limit: 1024,
			rpc_runtime: test_runtime(),
		}
	}

	fn endpoint(listen_addr: SocketAddr) -> RpcEndpoint {
		RpcEndpoint {
			listen_addr,
			batch_config: BatchRequestConfig::Disabled,
			max_connections: 1,
			max_payload_in_mb: 1,
			max_payload_out_mb: 1,
			max_subscriptions_per_connection: 1,
			max_buffer_capacity_per_connection: 1,
			rate_limit: None,
			rate_limit_trust_proxy_headers: false,
			rate_limit_whitelisted_ips: Vec::new(),
			cors: None,
			rpc_methods: RpcMethods::Auto,
			is_optional: false,
			retry_random_port: false,
		}
	}

	#[tokio::test]
	async fn start_server_with_no_endpoints_returns_error_without_panicking() {
		let res = start_server(config(Vec::new())).await;
		assert!(res.is_err(), "expected ListenAddrError, got Ok");
	}

	#[tokio::test]
	async fn start_server_bind_failure_returns_error_without_panicking() {
		let blocker = std::net::TcpListener::bind("127.0.0.1:0").expect("bind blocker; qed");
		let taken_addr = blocker.local_addr().expect("blocker has a local addr; qed");

		let res = start_server(config(vec![endpoint(taken_addr)])).await;
		assert!(res.is_err(), "expected a bind error, got Ok");
	}
}
