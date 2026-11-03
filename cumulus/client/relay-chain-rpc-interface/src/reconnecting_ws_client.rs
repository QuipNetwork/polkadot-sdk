// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus. If not, see <https://www.gnu.org/licenses/>.

use cumulus_primitives_core::relay_chain::{
	Block as RelayBlock, BlockNumber as RelayNumber, Hash as RelayHash, Header as RelayHeader,
};
use futures::{
	channel::{mpsc::Sender, oneshot::Sender as OneshotSender},
	future::{ready, BoxFuture},
	stream::{self, BoxStream, Fuse, FusedStream, FuturesUnordered},
	FutureExt, StreamExt,
};
use jsonrpsee::{
	core::{
		client::{Client as JsonRpcClient, ClientT, Subscription},
		params::ArrayParams,
		ClientError as JsonRpseeError, JsonValue,
	},
	ws_client::WsClientBuilder,
};
use sc_rpc_api::chain::ChainApiClient;
use schnellru::{ByLength, LruMap};
use sp_runtime::generic::SignedBlock;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc::{
	channel as tokio_channel, Receiver as TokioReceiver, Sender as TokioSender,
};
use url::Url;

use crate::rpc_client::{distribute_header, RpcDispatcherMessage};

const LOG_TARGET: &str = "reconnecting-websocket-client";
const DEFAULT_EXTERNAL_RPC_CONN_RETRIES: usize = 5;
const DEFAULT_SLEEP_TIME_MS_BETWEEN_RETRIES: u64 = 1000;
const DEFAULT_SLEEP_EXP_BACKOFF_BETWEEN_RETRIES: i32 = 2;
/// How often to check whether an RPC server listed before the active one can be used again.
const PREFERRED_RPC_SERVER_PROBE_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// How long checking a single preferred RPC server may take.
const PREFERRED_RPC_SERVER_PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Worker that should be used in combination with [`RelayChainRpcClient`].
///
/// Must be polled to distribute header notifications to listeners.
pub struct ReconnectingWebsocketWorker {
	ws_urls: Vec<String>,
	/// Communication channel with the RPC client
	client_receiver: TokioReceiver<RpcDispatcherMessage>,

	/// Senders to distribute incoming header notifications to
	imported_header_listeners: Vec<Sender<RelayHeader>>,
	finalized_header_listeners: Vec<Sender<RelayHeader>>,
	best_header_listeners: Vec<Sender<RelayHeader>>,

	/// How often to check whether an RPC server listed before the active one can be used again.
	preferred_server_probe_interval: Duration,
}

/// Format url and force addition of a port
fn url_to_string_with_port(url: Url) -> Option<String> {
	// This is already validated on CLI side, just defensive here
	if (url.scheme() != "ws" && url.scheme() != "wss") || url.host_str().is_none() {
		tracing::warn!(target: LOG_TARGET, ?url, "Non-WebSocket URL or missing host.");
		return None;
	}

	// Either we have a user-supplied port or use the default for 'ws' or 'wss' here
	Some(format!(
		"{}://{}:{}{}{}",
		url.scheme(),
		url.host_str()?,
		url.port_or_known_default()?,
		url.path(),
		url.query().map(|query| format!("?{}", query)).unwrap_or_default()
	))
}

/// A connection and its header subscriptions, ready to be installed together.
struct RpcConnection {
	index: usize,
	client: Arc<JsonRpcClient>,
	subscriptions: RelayChainSubscriptions,
}

struct RelayChainSubscriptions {
	import_subscription: Subscription<RelayHeader>,
	finalized_subscription: Subscription<RelayHeader>,
	best_subscription: Subscription<RelayHeader>,
}

/// A prepared preferred server awaiting validation against the active server's latest height.
struct PreferredServer {
	connection: RpcConnection,
	/// Latest buffered best head, consumed during probing and published when switching.
	best_header: RelayHeader,
}

/// Try to find a new RPC server to connect to. Uses a naive retry
/// logic that does an exponential backoff in between iterations
/// through all URLs from the list. It uses a constant to tell how
/// many iterations of connection attempts to all URLs we allow. We
/// return early when a connection and its subscriptions are ready.
async fn connect_next_available_rpc_server(
	urls: &[String],
	starting_position: usize,
) -> Result<RpcConnection, ()> {
	tracing::debug!(target: LOG_TARGET, starting_position, "Connecting to RPC server.");

	let mut prev_iteration: u32 = 0;
	for (counter, url) in urls
		.iter()
		.cycle()
		.skip(starting_position)
		.take(urls.len() * DEFAULT_EXTERNAL_RPC_CONN_RETRIES)
		.enumerate()
	{
		// If we reached the end of the urls list, backoff before retrying
		// connections to the entire list once more.
		let Ok(current_iteration) = (counter / urls.len()).try_into() else {
			tracing::error!(target: LOG_TARGET, "Too many connection attempts to the RPC servers, aborting...");
			break;
		};
		if current_iteration > prev_iteration {
			// Safe conversion given we convert positive i32s which are lower than u64::MAX.
			tokio::time::sleep(Duration::from_millis(
				DEFAULT_SLEEP_TIME_MS_BETWEEN_RETRIES *
					DEFAULT_SLEEP_EXP_BACKOFF_BETWEEN_RETRIES.pow(prev_iteration) as u64,
			))
			.await;
			prev_iteration = current_iteration;
		}

		let index = (starting_position + counter) % urls.len();
		tracing::info!(
			target: LOG_TARGET,
			attempt = current_iteration,
			index,
			url,
			"Trying to connect to next external relaychain node.",
		);
		match RpcConnection::connect(index, url).await {
			Ok(connection) => return Ok(connection),
			Err(err) => tracing::debug!(target: LOG_TARGET, url, ?err, "Unable to connect."),
		};
	}

	tracing::error!(target: LOG_TARGET, "Retrying to connect to any external relaychain node failed.");
	Err(())
}

/// Probe earlier servers in preference order without blocking the active connection.
/// Yield each candidate so the worker can compare it with its latest best block number and
/// continue the same round if the candidate is already behind.
fn probe_preferred_rpc_servers(
	candidates: Vec<(usize, String)>,
) -> Fuse<BoxStream<'static, PreferredServer>> {
	stream::iter(candidates)
		.then(|(index, url)| async move {
			match tokio::time::timeout(PREFERRED_RPC_SERVER_PROBE_TIMEOUT, async {
				let mut connection = RpcConnection::connect(index, &url).await?;
				let best = &mut connection.subscriptions.best_subscription;
				// Substrate sends its current best head when the subscription opens. Do not
				// wait for the candidate to catch up; only consume updates already buffered.
				let Some(mut best_header) = best.next().await.transpose()? else {
					return Ok(None);
				};
				while let Some(notification) = best.next().now_or_never() {
					let Some(header) = notification.transpose()? else {
						return Ok(None);
					};
					best_header = header;
				}
				Ok::<_, JsonRpseeError>(Some(PreferredServer { connection, best_header }))
			})
			.await
			{
				Ok(Ok(server)) => server,
				Ok(Err(error)) => {
					tracing::debug!(target: LOG_TARGET, url, ?error, "Unable to use preferred external relaychain node.");
					None
				},
				Err(_) => {
					tracing::debug!(target: LOG_TARGET, url, "Timed out checking preferred external relaychain node.");
					None
				},
			}
		})
		.filter_map(ready)
		.boxed()
		.fuse()
}

impl RpcConnection {
	async fn connect(index: usize, url: &str) -> Result<Self, JsonRpseeError> {
		let client = Arc::new(WsClientBuilder::default().build(url).await?);
		let subscriptions = Self::subscribe_to_heads(&client).await?;
		Ok(Self { index, client, subscriptions })
	}

	async fn subscribe_to_heads(
		client: &JsonRpcClient,
	) -> Result<RelayChainSubscriptions, JsonRpseeError> {
		let import_subscription = <JsonRpcClient as ChainApiClient<
			RelayNumber,
			RelayHash,
			RelayHeader,
			SignedBlock<RelayBlock>,
		>>::subscribe_all_heads(client)
		.await
		.map_err(|e| {
			tracing::error!(
				target: LOG_TARGET,
				?e,
				"Unable to open `chain_subscribeAllHeads` subscription."
			);
			e
		})?;

		let best_subscription = <JsonRpcClient as ChainApiClient<
			RelayNumber,
			RelayHash,
			RelayHeader,
			SignedBlock<RelayBlock>,
		>>::subscribe_new_heads(client)
		.await
		.map_err(|e| {
			tracing::error!(
				target: LOG_TARGET,
				?e,
				"Unable to open `chain_subscribeNewHeads` subscription."
			);
			e
		})?;

		let finalized_subscription = <JsonRpcClient as ChainApiClient<
			RelayNumber,
			RelayHash,
			RelayHeader,
			SignedBlock<RelayBlock>,
		>>::subscribe_finalized_heads(client)
		.await
		.map_err(|e| {
			tracing::error!(
				target: LOG_TARGET,
				?e,
				"Unable to open `chain_subscribeFinalizedHeads` subscription."
			);
			e
		})?;

		Ok(RelayChainSubscriptions {
			import_subscription,
			best_subscription,
			finalized_subscription,
		})
	}

	/// Create a request future that performs an RPC request and sends the results to the caller.
	/// In case of a dead websocket connection, it returns the original request parameters to
	/// enable retries.
	fn create_request(
		&self,
		method: String,
		params: ArrayParams,
		response_sender: OneshotSender<Result<JsonValue, JsonRpseeError>>,
	) -> BoxFuture<'static, Result<(), RpcDispatcherMessage>> {
		let future_client = self.client.clone();
		async move {
			let resp = future_client.request(&method, params.clone()).await;

			// We should only return the original request in case
			// the websocket connection is dead and requires a restart.
			// Other errors should be forwarded to the request caller.
			if let Err(JsonRpseeError::RestartNeeded(_)) = resp {
				return Err(RpcDispatcherMessage::Request(method, params, response_sender));
			}

			if let Err(err) = response_sender.send(resp) {
				tracing::debug!(
					target: LOG_TARGET,
					?err,
					"Recipient no longer interested in request result"
				);
			}
			Ok(())
		}
		.boxed()
	}
}

enum ConnectionStatus {
	Connected,
	ReconnectRequired(Option<RpcDispatcherMessage>),
}

impl ReconnectingWebsocketWorker {
	/// Create new worker. Returns the worker and a channel to register new listeners.
	pub async fn new(
		urls: Vec<Url>,
	) -> (ReconnectingWebsocketWorker, TokioSender<RpcDispatcherMessage>) {
		let urls = urls.into_iter().filter_map(url_to_string_with_port).collect();

		let (tx, rx) = tokio_channel(100);
		let worker = ReconnectingWebsocketWorker {
			ws_urls: urls,
			client_receiver: rx,
			imported_header_listeners: Vec::new(),
			finalized_header_listeners: Vec::new(),
			best_header_listeners: Vec::new(),
			preferred_server_probe_interval: PREFERRED_RPC_SERVER_PROBE_INTERVAL,
		};
		(worker, tx)
	}

	/// Run this worker to drive notification streams.
	/// The worker does the following:
	/// - Listen for [`RpcDispatcherMessage`], perform requests and register new listeners for the
	///   notification streams
	/// - Distribute incoming import, best head and finalization notifications to registered
	///   listeners. If an error occurs during sending, the receiver has been closed and we remove
	///   the sender from the list.
	/// - Find a new valid RPC server to connect to in case the websocket connection is terminated.
	///   If the worker is not able to connect to an RPC server from the list, the worker shuts
	///   down.
	/// - While connected to an RPC server other than the first one in the list, periodically check
	///   the servers listed before it and switch back to the first one that is reachable and has
	///   caught up with the best block seen so far.
	pub async fn run(mut self) {
		let mut pending_requests = FuturesUnordered::new();

		let urls = std::mem::take(&mut self.ws_urls);
		let Ok(mut active) = connect_next_available_rpc_server(&urls, 0).await else {
			tracing::error!(target: LOG_TARGET, "No valid RPC url found. Stopping RPC worker.");
			return;
		};

		let mut imported_blocks_cache = LruMap::new(ByLength::new(40));
		let mut should_reconnect = ConnectionStatus::Connected;
		let mut last_seen_finalized_num: RelayNumber = 0;
		let mut last_seen_best_num: RelayNumber = 0;
		let mut preferred_server_probe_timer = tokio::time::interval_at(
			tokio::time::Instant::now() + self.preferred_server_probe_interval,
			self.preferred_server_probe_interval,
		);
		let mut preferred_server_probe = stream::empty().boxed().fuse();
		loop {
			if let ConnectionStatus::ReconnectRequired(failed_request) = should_reconnect {
				// Requests retain their own clients. Leave them pending while replacing the
				// connection; a slow request on an older server must not delay failover.
				let Ok(connection) =
					connect_next_available_rpc_server(&urls, active.index + 1).await
				else {
					tracing::error!(target: LOG_TARGET, "Unable to reconnect, stopping worker.");
					return;
				};
				active = connection;
				preferred_server_probe = stream::empty().boxed().fuse();
				preferred_server_probe_timer.reset();
				if let Some(RpcDispatcherMessage::Request(method, params, sender)) = failed_request
				{
					pending_requests.push(active.create_request(method, params, sender));
				}
				should_reconnect = ConnectionStatus::Connected;
			}

			tokio::select! {
				evt = self.client_receiver.recv() => match evt {
					Some(RpcDispatcherMessage::RegisterBestHeadListener(tx)) => {
						self.best_header_listeners.push(tx);
					},
					Some(RpcDispatcherMessage::RegisterImportListener(tx)) => {
						self.imported_header_listeners.push(tx)
					},
					Some(RpcDispatcherMessage::RegisterFinalizationListener(tx)) => {
						self.finalized_header_listeners.push(tx)
					},
					Some(RpcDispatcherMessage::Request(method, params, response_sender)) => {
						pending_requests.push(active.create_request(method, params, response_sender));
					},
					None => {
						tracing::error!(target: LOG_TARGET, "RPC client receiver closed. Stopping RPC Worker.");
						return;
					}
				},
				should_retry = pending_requests.next(), if !pending_requests.is_empty() => {
					if let Some(Err(req)) = should_retry {
						// Connection failures are retried on the current server. A failure
						// from an old connection must not displace a healthy active one.
						if !active.client.is_connected() {
							should_reconnect = ConnectionStatus::ReconnectRequired(Some(req));
						} else if let RpcDispatcherMessage::Request(method, params, sender) = req {
							pending_requests.push(active.create_request(method, params, sender));
						}
					}
				},
				_ = preferred_server_probe_timer.tick(),
					if active.index > 0 && preferred_server_probe.is_terminated() =>
				{
					let candidates = urls[..active.index]
						.iter()
						.cloned()
						.enumerate()
						.collect();
					preferred_server_probe = probe_preferred_rpc_servers(candidates);
				},
				preferred_server = preferred_server_probe.next(), if !preferred_server_probe.is_terminated() => {
					let Some(server) = preferred_server else {
						preferred_server_probe_timer.reset();
						continue;
					};
					// Validate against the latest head, not a snapshot from the start of the
					// round. Rejection leaves the remaining candidates available for polling.
					if server.best_header.number < last_seen_best_num {
						continue;
					}
					tracing::info!(
						target: LOG_TARGET,
						index = server.connection.index,
						url = %urls[server.connection.index],
						previous_index = active.index,
						"Switched back to preferred external relay chain node.",
					);
					active = server.connection;
					// Publish the head used for validation, not the older startup notifications
					// consumed by the probe. Subsequent best-head changes may include reorgs.
					last_seen_best_num = server.best_header.number;
					distribute_header(server.best_header, &mut self.best_header_listeners);
					preferred_server_probe = stream::empty().boxed().fuse();
					preferred_server_probe_timer.reset();
				},
				import_event = active.subscriptions.import_subscription.next() => {
					match import_event {
						Some(Ok(header)) => {
							let hash = header.hash();
							if imported_blocks_cache.peek(&hash).is_some() {
								tracing::debug!(
									target: LOG_TARGET,
									number = header.number,
									?hash,
									"Duplicate imported block header. This might happen after switching to a new RPC node. Skipping distribution."
								);
								continue;
							}
							imported_blocks_cache.insert(hash, ());
							distribute_header(header, &mut self.imported_header_listeners);
						},
						None => {
							tracing::error!(target: LOG_TARGET, "Subscription closed.");
							should_reconnect = ConnectionStatus::ReconnectRequired(None);
						},
						Some(Err(error)) => {
							tracing::error!(target: LOG_TARGET, ?error, "Error in RPC subscription.");
							should_reconnect = ConnectionStatus::ReconnectRequired(None);
						},
					}
				},
				best_header_event = active.subscriptions.best_subscription.next() => {
					match best_header_event {
						Some(Ok(header)) => {
							last_seen_best_num = header.number;
							distribute_header(header, &mut self.best_header_listeners)
						},
						None => {
							tracing::error!(target: LOG_TARGET, "Subscription closed.");
							should_reconnect = ConnectionStatus::ReconnectRequired(None);
						},
						Some(Err(error)) => {
							tracing::error!(target: LOG_TARGET, ?error, "Error in RPC subscription.");
							should_reconnect = ConnectionStatus::ReconnectRequired(None);
						},
					}
				}
				finalized_event = active.subscriptions.finalized_subscription.next() => {
					match finalized_event {
						Some(Ok(header)) if header.number > last_seen_finalized_num => {
							last_seen_finalized_num = header.number;
							distribute_header(header, &mut self.finalized_header_listeners);
						},
						Some(Ok(header)) => {
							tracing::debug!(
								target: LOG_TARGET,
								number = header.number,
								last_seen_finalized_num,
								"Duplicate finalized block header. This might happen after switching to a new RPC node. Skipping distribution."
							);
						},
						None => {
							tracing::error!(target: LOG_TARGET, "Subscription closed.");
							should_reconnect = ConnectionStatus::ReconnectRequired(None);
						},
						Some(Err(error)) => {
							tracing::error!(target: LOG_TARGET, ?error, "Error in RPC subscription.");
							should_reconnect = ConnectionStatus::ReconnectRequired(None);
						},
					}
				}
			}
		}
	}
}

#[cfg(test)]
mod test {
	use std::{
		sync::atomic::{AtomicU32, AtomicUsize, Ordering},
		time::Duration,
	};

	use super::{
		connect_next_available_rpc_server, url_to_string_with_port, Arc, ArrayParams, JsonValue,
		ReconnectingWebsocketWorker, RelayHeader, RpcDispatcherMessage, StreamExt, TokioSender,
	};
	use jsonrpsee::{
		core::SubscriptionResult,
		server::{Server, ServerHandle},
		PendingSubscriptionSink, RpcModule, SubscriptionMessage,
	};
	use sp_runtime::traits::Header as _;
	use tokio::sync::Semaphore;
	use url::Url;

	const SERVER_STARTUP_DELAY_SECONDS: u64 = 10;
	/// Upper bound for anything the tests below wait for.
	const TEST_TIMEOUT: Duration = Duration::from_secs(30);

	fn header(number: u32) -> RelayHeader {
		RelayHeader::new(
			number,
			Default::default(),
			Default::default(),
			Default::default(),
			Default::default(),
		)
	}

	/// Send the current best header to the subscriber until it goes away.
	async fn serve_heads(
		pending: PendingSubscriptionSink,
		best: Arc<AtomicU32>,
		subscriptions: Arc<AtomicUsize>,
		gate: Option<Arc<Semaphore>>,
		notification_gate: Option<Arc<Semaphore>>,
		notifications: Arc<AtomicUsize>,
	) -> SubscriptionResult {
		subscriptions.fetch_add(1, Ordering::SeqCst);
		if let Some(gate) = gate {
			gate.acquire().await?.forget();
		}
		let sink = pending.accept().await?;
		// Returning an error would close the subscription on the client side, so only stop once
		// the client is gone.
		loop {
			if let Some(gate) = &notification_gate {
				gate.acquire().await?.forget();
			}
			let head = SubscriptionMessage::from_json(&header(best.load(Ordering::SeqCst)))?;
			if sink.send(head).await.is_err() {
				return Ok(());
			}
			notifications.fetch_add(1, Ordering::SeqCst);
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}

	/// Gates let tests advance the active chain while a candidate's RPC call is in flight.
	#[derive(Default)]
	struct ResponseGates {
		subscriptions: Option<Arc<Semaphore>>,
		best_heads: Option<Arc<Semaphore>>,
	}

	/// Relay chain RPC node serving what the worker needs, plus `test_whoami`, which returns the
	/// node's name, and `test_hang`, which returns it too unless the node is set to hang.
	///
	/// Dropping it stops all its tasks at once, so connections close like for a crashed node.
	struct MockRelayNode {
		/// Number of the best header served.
		best: Arc<AtomicU32>,
		/// Number of best-head notifications sent.
		best_notifications: Arc<AtomicUsize>,
		/// Number of header subscription requests received.
		subscriptions: Arc<AtomicUsize>,
		/// Number of `test_hang` requests received.
		hang_requests: Arc<AtomicUsize>,
		runtime: Option<tokio::runtime::Runtime>,
		_server: ServerHandle,
	}

	impl MockRelayNode {
		fn start(port: u16, name: &'static str, best: u32, hang: bool) -> Self {
			Self::start_with_gates(port, name, best, hang, ResponseGates::default())
		}

		fn start_with_gates(
			port: u16,
			name: &'static str,
			best: u32,
			hang: bool,
			gates: ResponseGates,
		) -> Self {
			let best = Arc::new(AtomicU32::new(best));
			let best_notifications = Arc::new(AtomicUsize::new(0));
			let subscriptions = Arc::new(AtomicUsize::new(0));
			let hang_requests = Arc::new(AtomicUsize::new(0));

			let mut module = RpcModule::new(());
			module.register_method("test_whoami", move |_, _, _| name).unwrap();
			let requests = hang_requests.clone();
			module
				.register_async_method("test_hang", move |_, _, _| {
					let requests = requests.clone();
					async move {
						requests.fetch_add(1, Ordering::SeqCst);
						if hang {
							futures::future::pending::<()>().await;
						}
						name
					}
				})
				.unwrap();

			for (subscribe, notification, unsubscribe) in [
				("chain_subscribeAllHeads", "chain_allHead", "chain_unsubscribeAllHeads"),
				("chain_subscribeNewHeads", "chain_newHead", "chain_unsubscribeNewHeads"),
				(
					"chain_subscribeFinalizedHeads",
					"chain_finalizedHead",
					"chain_unsubscribeFinalizedHeads",
				),
			] {
				let (best, subscriptions) = (best.clone(), subscriptions.clone());
				let gate = gates.subscriptions.clone();
				let (notification_gate, notifications) = if subscribe == "chain_subscribeNewHeads" {
					(gates.best_heads.clone(), best_notifications.clone())
				} else {
					(None, Arc::new(AtomicUsize::new(0)))
				};
				module
					.register_subscription(
						subscribe,
						notification,
						unsubscribe,
						move |_, pending, _, _| {
							serve_heads(
								pending,
								best.clone(),
								subscriptions.clone(),
								gate.clone(),
								notification_gate.clone(),
								notifications.clone(),
							)
						},
					)
					.unwrap();
			}

			// Run on a dedicated runtime, so that dropping it kills the node.
			let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
			listener.set_nonblocking(true).unwrap();
			let runtime = tokio::runtime::Builder::new_multi_thread()
				.worker_threads(1)
				.enable_all()
				.build()
				.unwrap();
			let server = {
				let _guard = runtime.enter();
				Server::builder().build_from_tcp(listener).unwrap().start(module)
			};

			Self {
				best,
				best_notifications,
				subscriptions,
				hang_requests,
				runtime: Some(runtime),
				_server: server,
			}
		}
	}

	impl Drop for MockRelayNode {
		fn drop(&mut self) {
			// Dropping a runtime from within the test's runtime would panic.
			if let Some(runtime) = self.runtime.take() {
				runtime.shutdown_background();
			}
		}
	}

	/// Pick `N` distinct unused ports.
	fn unused_ports<const N: usize>() -> [u16; N] {
		let mut ports = Vec::new();
		while ports.len() < N {
			let port = portpicker::pick_unused_port().unwrap();
			if !ports.contains(&port) {
				ports.push(port);
			}
		}
		ports.try_into().unwrap()
	}

	/// Start a worker for local servers on `ports`, checking preferred servers every 100ms.
	async fn start_worker(ports: &[u16]) -> TokioSender<RpcDispatcherMessage> {
		let urls = ports
			.iter()
			.map(|port| Url::parse(&format!("ws://127.0.0.1:{port}")).unwrap())
			.collect();
		let (mut worker, sender) = ReconnectingWebsocketWorker::new(urls).await;
		worker.preferred_server_probe_interval = Duration::from_millis(100);
		tokio::spawn(worker.run());
		sender
	}

	async fn request(worker: &TokioSender<RpcDispatcherMessage>, method: &str) -> JsonValue {
		let (tx, rx) = futures::channel::oneshot::channel();
		worker
			.send(RpcDispatcherMessage::Request(method.into(), ArrayParams::new(), tx))
			.await
			.unwrap();
		tokio::time::timeout(TEST_TIMEOUT, rx).await.unwrap().unwrap().unwrap()
	}

	async fn wait_until(mut condition: impl FnMut() -> bool) {
		tokio::time::timeout(TEST_TIMEOUT, async {
			while !condition() {
				tokio::time::sleep(Duration::from_millis(10)).await;
			}
		})
		.await
		.unwrap();
	}

	/// Wait until the worker sends requests to the node called `name`.
	async fn wait_until_served_by(worker: &TokioSender<RpcDispatcherMessage>, name: &str) {
		tokio::time::timeout(TEST_TIMEOUT, async {
			while request(worker, "test_whoami").await != name {
				tokio::time::sleep(Duration::from_millis(10)).await;
			}
		})
		.await
		.unwrap();
	}

	/// Wait until the worker has seen a best block header.
	async fn wait_for_best_header(worker: &TokioSender<RpcDispatcherMessage>) {
		let (tx, mut rx) = futures::channel::mpsc::channel(1);
		worker.send(RpcDispatcherMessage::RegisterBestHeadListener(tx)).await.unwrap();
		tokio::time::timeout(TEST_TIMEOUT, rx.next()).await.unwrap().unwrap();
	}

	async fn wait_for_best_number(worker: &TokioSender<RpcDispatcherMessage>, number: u32) {
		let (tx, mut rx) = futures::channel::mpsc::channel(10);
		worker
			.send(RpcDispatcherMessage::RegisterBestHeadListener(tx))
			.await
			.expect("worker is running");
		tokio::time::timeout(TEST_TIMEOUT, async {
			while let Some(header) = rx.next().await {
				if header.number == number {
					return;
				}
			}
			panic!("best header stream closed");
		})
		.await
		.expect("worker receives the expected best header");
	}

	#[test]
	fn url_to_string_works() {
		let url = Url::parse("wss://something/path").unwrap();
		assert_eq!(Some("wss://something:443/path".to_string()), url_to_string_with_port(url));

		let url = Url::parse("ws://something/path").unwrap();
		assert_eq!(Some("ws://something:80/path".to_string()), url_to_string_with_port(url));

		let url = Url::parse("wss://something:100/path").unwrap();
		assert_eq!(Some("wss://something:100/path".to_string()), url_to_string_with_port(url));

		let url = Url::parse("wss://something:100/path").unwrap();
		assert_eq!(Some("wss://something:100/path".to_string()), url_to_string_with_port(url));

		let url = Url::parse("wss://something/path?query=yes").unwrap();
		assert_eq!(
			Some("wss://something:443/path?query=yes".to_string()),
			url_to_string_with_port(url)
		);

		let url = Url::parse("wss://something:9090/path?query=yes").unwrap();
		assert_eq!(
			Some("wss://something:9090/path?query=yes".to_string()),
			url_to_string_with_port(url)
		);
	}

	#[tokio::test]
	// Testing the retry logic at full means increasing CI with half a minute according
	// to the current logic, so lets test it best effort.
	async fn connection_retry_logic() {
		let [port] = unused_ports();
		let server = tokio::spawn(async move {
			tokio::time::sleep(Duration::from_secs(SERVER_STARTUP_DELAY_SECONDS)).await;
			MockRelayNode::start(port, "A", 100, false)
		});

		// Connection refusals while the server starts should be retried.
		let res = connect_next_available_rpc_server(&[format!("ws://127.0.0.1:{port}")], 0).await;
		assert!(res.is_ok());
		let _server = server.await.expect("server task completes");
	}

	#[tokio::test]
	async fn switches_back_only_after_preferred_server_caught_up() {
		let [port_a, port_b] = unused_ports();
		let _b = MockRelayNode::start(port_b, "B", 100, false);
		let worker = start_worker(&[port_a, port_b]).await;
		wait_until_served_by(&worker, "B").await;
		wait_for_best_header(&worker).await;

		// A is reachable again, but behind the best block already seen from B.
		let a = MockRelayNode::start(port_a, "A", 1, false);
		wait_until(|| a.subscriptions.load(Ordering::SeqCst) >= 9).await;
		assert_eq!(request(&worker, "test_whoami").await, "B");

		// Once A has caught up, the worker switches back to it.
		a.best.store(1_000, Ordering::SeqCst);
		wait_until_served_by(&worker, "A").await;
	}

	#[tokio::test]
	async fn stale_failure_after_switch_back_keeps_preferred_server() {
		let [port_a, port_b, port_c] = unused_ports();
		let b = MockRelayNode::start(port_b, "B", 100, true);
		let c = MockRelayNode::start(port_c, "C", 100, false);
		let worker = start_worker(&[port_a, port_b, port_c]).await;
		wait_until_served_by(&worker, "B").await;

		let in_flight = tokio::spawn({
			let worker = worker.clone();
			async move { request(&worker, "test_hang").await }
		});
		wait_until(|| b.hang_requests.load(Ordering::SeqCst) == 1).await;

		let _a = MockRelayNode::start(port_a, "A", 100, false);
		wait_until_served_by(&worker, "A").await;

		// B dies while still serving a request. The request must be retried on A, without
		// reconnecting, which would start at the server after A and land on C.
		drop(b);
		assert_eq!(in_flight.await.unwrap(), "A");
		assert_eq!(request(&worker, "test_whoami").await, "A");
		assert_eq!(c.subscriptions.load(Ordering::SeqCst), 0);
	}

	#[tokio::test]
	async fn request_on_dead_active_server_is_retried_on_next() {
		let [port_a, port_b] = unused_ports();
		let a = MockRelayNode::start(port_a, "A", 100, true);
		let _b = MockRelayNode::start(port_b, "B", 100, false);
		let worker = start_worker(&[port_a, port_b]).await;
		wait_until_served_by(&worker, "A").await;

		let in_flight = tokio::spawn({
			let worker = worker.clone();
			async move { request(&worker, "test_hang").await }
		});
		wait_until(|| a.hang_requests.load(Ordering::SeqCst) == 1).await;

		drop(a);
		assert_eq!(in_flight.await.unwrap(), "B");
		assert_eq!(request(&worker, "test_whoami").await, "B");
	}

	#[tokio::test]
	async fn failover_does_not_wait_for_requests_on_previous_connection() {
		let [port_a, port_b, port_c] = unused_ports();
		let b = MockRelayNode::start(port_b, "B", 100, true);
		let _c = MockRelayNode::start(port_c, "C", 100, false);
		let worker = start_worker(&[port_a, port_b, port_c]).await;
		wait_until_served_by(&worker, "B").await;

		let in_flight = tokio::spawn({
			let worker = worker.clone();
			async move { request(&worker, "test_hang").await }
		});
		wait_until(|| b.hang_requests.load(Ordering::SeqCst) == 1).await;

		let a = MockRelayNode::start(port_a, "A", 100, false);
		wait_until_served_by(&worker, "A").await;
		drop(a);

		// B's old request is still pending, but fresh requests and headers must resume
		// without waiting for its 60-second RPC timeout.
		tokio::time::timeout(Duration::from_secs(5), async {
			wait_until_served_by(&worker, "B").await;
			b.best.store(101, Ordering::SeqCst);
			wait_for_best_number(&worker, 101).await;
		})
		.await
		.expect("failover is independent of old requests");
		assert!(!in_flight.is_finished());

		// The old request is still tracked and can be retried if its connection dies.
		drop(b);
		assert_eq!(in_flight.await.expect("request task completes"), "C");
	}

	#[tokio::test]
	async fn preferred_height_is_checked_after_subscription_setup() {
		let [port_a, port_b] = unused_ports();
		let b = MockRelayNode::start(port_b, "B", 100, false);
		let worker = start_worker(&[port_a, port_b]).await;
		wait_until_served_by(&worker, "B").await;
		wait_for_best_number(&worker, 100).await;

		let gate = Arc::new(Semaphore::new(0));
		let a = MockRelayNode::start_with_gates(
			port_a,
			"A",
			100,
			false,
			ResponseGates { subscriptions: Some(gate.clone()), ..Default::default() },
		);
		wait_until(|| a.subscriptions.load(Ordering::SeqCst) == 1).await;

		// Both nodes advance while A is opening subscriptions. The worker must keep
		// serving B, then validate A's new height once setup completes.
		b.best.store(200, Ordering::SeqCst);
		a.best.store(200, Ordering::SeqCst);
		wait_for_best_number(&worker, 200).await;
		assert_eq!(request(&worker, "test_whoami").await, "B");
		gate.add_permits(3);
		tokio::time::timeout(Duration::from_secs(5), wait_until_served_by(&worker, "A"))
			.await
			.expect("the first probe uses the height after subscription setup");
	}

	#[tokio::test]
	async fn stale_preferred_candidate_does_not_skip_remaining_candidates() {
		let [port_a, port_b, port_c] = unused_ports();
		let c = MockRelayNode::start(port_c, "C", 100, false);
		let worker = start_worker(&[port_a, port_b, port_c]).await;
		wait_until_served_by(&worker, "C").await;
		wait_for_best_number(&worker, 100).await;

		let gate = Arc::new(Semaphore::new(0));
		let a = MockRelayNode::start_with_gates(
			port_a,
			"A",
			100,
			false,
			ResponseGates { best_heads: Some(gate.clone()), ..Default::default() },
		);
		wait_until(|| a.subscriptions.load(Ordering::SeqCst) == 3).await;

		// C advances before A sends its initial best-head notification at height 100.
		c.best.store(200, Ordering::SeqCst);
		let _b = MockRelayNode::start(port_b, "B", 200, false);
		wait_for_best_number(&worker, 200).await;
		gate.add_permits(1);

		// A second round would wait for another notification until the 20-second timeout.
		// Reaching B promptly proves rejection continued the current round.
		tokio::time::timeout(Duration::from_secs(5), wait_until_served_by(&worker, "B"))
			.await
			.expect("the probe continues to B after rejecting A");
	}

	#[tokio::test]
	async fn switch_publishes_latest_buffered_best_head_and_preserves_later_reorgs() {
		let [port_a, port_b] = unused_ports();
		let _b = MockRelayNode::start(port_b, "B", 101, false);
		let worker = start_worker(&[port_a, port_b]).await;
		wait_until_served_by(&worker, "B").await;
		wait_for_best_number(&worker, 101).await;
		let (tx, mut heads) = futures::channel::mpsc::channel(100);
		worker
			.send(RpcDispatcherMessage::RegisterBestHeadListener(tx))
			.await
			.expect("worker is running");

		// Allow the import and best subscriptions to open, but hold the final subscription
		// until two best-head notifications have been queued on the candidate connection.
		let subscriptions = Arc::new(Semaphore::new(2));
		let notifications = Arc::new(Semaphore::new(0));
		let a = MockRelayNode::start_with_gates(
			port_a,
			"A",
			100,
			false,
			ResponseGates {
				subscriptions: Some(subscriptions.clone()),
				best_heads: Some(notifications.clone()),
			},
		);
		wait_until(|| a.subscriptions.load(Ordering::SeqCst) == 3).await;
		notifications.add_permits(1);
		wait_until(|| a.best_notifications.load(Ordering::SeqCst) == 1).await;
		a.best.store(102, Ordering::SeqCst);
		notifications.add_permits(1);
		wait_until(|| a.best_notifications.load(Ordering::SeqCst) == 2).await;
		subscriptions.add_permits(1);

		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let head = heads.next().await.expect("best-head stream stays open");
				assert!(head.number >= 101, "handoff must not replay the stale initial head");
				if head.number == 102 {
					break;
				}
			}
			wait_until_served_by(&worker, "A").await;
		})
		.await
		.expect("handoff publishes the buffered current head without waiting for another block");

		// Once installed, a lower best head can represent a genuine reorg. Forward it,
		// and do not publish the retained handoff header a second time.
		a.best.store(100, Ordering::SeqCst);
		notifications.add_permits(1);
		let head = tokio::time::timeout(Duration::from_secs(5), heads.next())
			.await
			.expect("reorg notification arrives")
			.expect("best-head stream stays open");
		assert_eq!(head.number, 100);
	}

	#[tokio::test]
	async fn preferred_server_without_initial_head_times_out() {
		let [port_a, port_b, port_c] = unused_ports();
		let _c = MockRelayNode::start(port_c, "C", 100, false);
		let worker = start_worker(&[port_a, port_b, port_c]).await;
		wait_until_served_by(&worker, "C").await;
		wait_for_best_number(&worker, 100).await;

		let a = MockRelayNode::start_with_gates(
			port_a,
			"A",
			100,
			false,
			ResponseGates { best_heads: Some(Arc::new(Semaphore::new(0))), ..Default::default() },
		);
		wait_until(|| a.subscriptions.load(Ordering::SeqCst) == 3).await;
		let _b = MockRelayNode::start(port_b, "B", 100, false);

		// A accepts subscriptions but sends no best head. C must remain usable while
		// the existing probe timeout bounds the wait before trying B.
		tokio::time::timeout(Duration::from_secs(5), async {
			assert_eq!(request(&worker, "test_whoami").await, "C");
			wait_for_best_number(&worker, 100).await;
		})
		.await
		.expect("a silent candidate does not block the active connection");
		wait_until_served_by(&worker, "B").await;
	}
}
