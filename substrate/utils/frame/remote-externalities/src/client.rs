// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! WebSocket client management for remote externalities.

use jsonrpsee::ws_client::{WsClient, WsClientBuilder};
use log::*;
use std::{future::Future, ops::Deref, sync::Arc, time::Duration};

use crate::{Result, LOG_TARGET};

/// Default timeout for RPC requests.
pub const RPC_TIMEOUT: Duration = Duration::from_secs(60);

/// Execute an RPC call with timeout. Returns `Err` if the timeout is hit.
pub(crate) async fn with_timeout<T, F: Future<Output = T>>(
	future: F,
	timeout: Duration,
) -> std::result::Result<T, ()> {
	tokio::time::timeout(timeout, future).await.map_err(|_| ())
}

/// A WebSocket client with version tracking for reconnection.
#[derive(Debug, Clone)]
pub(crate) struct Client {
	pub(crate) ws_client: Arc<WsClient>,
	pub(crate) version: u64,
	uri: String,
}

impl Deref for Client {
	type Target = WsClient;
	fn deref(&self) -> &Self::Target {
		&self.ws_client
	}
}

impl Client {
	/// Create a WebSocket client for the given URI.
	async fn create_ws_client(uri: &str) -> std::result::Result<WsClient, String> {
		debug!(target: LOG_TARGET, "initializing remote client to {:?}", uri);

		WsClientBuilder::default()
			.max_request_size(u32::MAX)
			.max_response_size(u32::MAX)
			.request_timeout(Duration::from_secs(60))
			.build(uri)
			.await
			.map_err(|e| format!("{e:?}"))
	}

	pub(crate) fn uri(&self) -> &str {
		&self.uri
	}

	/// Create a new Client from a URI.
	///
	/// Returns `None` if the initial connection fails or times out.
	pub async fn new(uri: impl Into<String>) -> Option<Self> {
		let uri = uri.into();
		let result = with_timeout(Self::create_ws_client(&uri), RPC_TIMEOUT).await;

		match result {
			Ok(Ok(ws_client)) => Some(Self { ws_client: Arc::new(ws_client), version: 0, uri }),
			Ok(Err(e)) => {
				warn!(target: LOG_TARGET, "Connection to {uri} failed: {e}. Ignoring this URI.");
				None
			},
			Err(()) => {
				warn!(target: LOG_TARGET, "Connection to {uri} timed out. Ignoring this URI.");
				None
			},
		}
	}

	/// Recreate the WebSocket client using the stored URI if the version matches.
	pub(crate) async fn recreate(&mut self, expected_version: u64) {
		if self.version > expected_version {
			return;
		}

		debug!(target: LOG_TARGET, "Recreating client for `{}`", self.uri);
		let result = with_timeout(Self::create_ws_client(&self.uri), RPC_TIMEOUT).await;

		match result {
			Ok(Ok(ws_client)) => {
				self.ws_client = Arc::new(ws_client);
				self.version = expected_version + 1;
			},
			Ok(Err(e)) => {
				debug!(target: LOG_TARGET, "Failed to recreate client for `{}`: {e}", self.uri);
			},
			Err(()) => {
				debug!(target: LOG_TARGET, "Timeout recreating client for `{}`", self.uri);
			},
		}
	}
}

/// Manages WebSocket client connections for parallel workers.
///
/// The pool is shared across all worker clones, so dropping a provider is seen by every worker. The
/// connecting URI is stored next to each client so the pool can be filtered without locking.
#[derive(Clone)]
pub(crate) struct ConnectionManager {
	clients: Arc<tokio::sync::RwLock<Vec<(String, Arc<tokio::sync::Mutex<Client>>)>>>,
}

impl ConnectionManager {
	pub(crate) fn new(clients: Vec<(String, Arc<tokio::sync::Mutex<Client>>)>) -> Result<Self> {
		if clients.is_empty() {
			return Err("At least one client must be provided");
		}

		Ok(Self { clients: Arc::new(tokio::sync::RwLock::new(clients)) })
	}

	pub(crate) async fn num_clients(&self) -> usize {
		self.clients.read().await.len()
	}

	/// Drop the `failed` provider from the pool (matched by URI).
	///
	/// The last remaining provider is never removed, so that the pool is never emptied.
	pub(crate) async fn remove_client(&self, failed: &Client) {
		let mut clients = self.clients.write().await;
		if clients.len() <= 1 {
			return;
		}

		let before = clients.len();
		clients.retain(|(uri, _)| uri != failed.uri());
		if clients.len() < before {
			warn!(
				target: LOG_TARGET,
				"⚠️ dropping RPC provider `{}` ({} provider(s) left)",
				failed.uri(),
				clients.len(),
			);
		}
	}

	async fn slot(&self, worker_index: usize) -> Arc<tokio::sync::Mutex<Client>> {
		let clients = self.clients.read().await;
		clients[worker_index % clients.len()].1.clone()
	}

	/// Get a client for `worker_index` to issue RPC calls with.
	pub(crate) async fn get(&self, worker_index: usize) -> Client {
		self.slot(worker_index).await.lock().await.clone()
	}

	/// Reconnect the client assigned to `worker_index`, unless `failed` is already stale.
	pub(crate) async fn recreate_client(&self, worker_index: usize, failed: Client) {
		self.slot(worker_index).await.lock().await.recreate(failed.version).await;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Start a local WS server in a random port.
	///
	/// Used to test WS client building for remote externality client initialization.
	async fn start_local_ws_server() -> (String, jsonrpsee::server::ServerHandle) {
		let server = jsonrpsee::server::ServerBuilder::default()
			.build("127.0.0.1:0")
			.await
			.expect("local ws server should start");

		let addr = server.local_addr().expect("local ws server should have a local addr");
		let handle = server.start(jsonrpsee::RpcModule::new(()));

		println!("local ws server started at {addr}");
		(format!("ws://{addr}"), handle)
	}

	#[tokio::test]
	async fn can_init_client() {
		let (uri, _server) = start_local_ws_server().await;

		let client = Client::new(uri.clone()).await;
		assert!(client.is_some(), "Client should initialize successfully with valid WebSocket URI");

		// Test create_ws_client directly
		let ws_client = Client::create_ws_client(&uri).await;
		assert!(ws_client.is_ok(), "create_ws_client should succeed with valid URI");
	}

	#[tokio::test]
	async fn cannot_init_client_with_invalid_uri() {
		// HTTP/HTTPS are invalid, only WS/WSS are supported
		assert!(Client::new("http://try-runtime.polkadot.io:443").await.is_none());
		assert!(Client::create_ws_client("http://try-runtime.polkadot.io:443").await.is_err());

		assert!(Client::new("https://try-runtime.polkadot.io:443").await.is_none());
		assert!(Client::create_ws_client("https://try-runtime.polkadot.io:443").await.is_err());

		// Invalid URIs/garbage
		assert!(Client::new("wsss://try-runtime.polkadot.io:443").await.is_none());
		assert!(Client::create_ws_client("wsss://try-runtime.polkadot.io:443").await.is_err());
		assert!(Client::new("garbage").await.is_none());
		assert!(Client::create_ws_client("garbage").await.is_err());
	}
}
