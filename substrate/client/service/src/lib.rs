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

//! Substrate service. Starts a thread that spins up the network, client, and extrinsic pool.
//! Manages communication between them.

#![warn(missing_docs)]
#![recursion_limit = "1024"]

pub mod chain_ops;
pub mod client;
pub mod config;
pub mod error;

mod builder;
mod metrics;
mod task_manager;

use crate::config::Multiaddr;
use std::{
	collections::HashMap,
	net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
};

use codec::{Decode, Encode};
use futures::{pin_mut, FutureExt, StreamExt};
use jsonrpsee::RpcModule;
use log::{debug, error, trace, warn};
use sc_client_api::{blockchain::HeaderBackend, BlockBackend, BlockchainEvents, ProofProvider};
use sc_network::{
	config::MultiaddrWithPeerId, multiaddr::Protocol, service::traits::NetworkService,
	NetworkBackend, NetworkBlock, NetworkPeers, NetworkStateInfo,
};
use sc_network_sync::SyncingService;
use sc_network_types::PeerId;
pub use sc_rpc_server::create_rpc_runtime;
use sc_rpc_server::Server;
use sc_utils::mpsc::TracingUnboundedReceiver;
use sp_blockchain::HeaderMetadata;
use sp_consensus::SyncOracle;
use sp_runtime::traits::{Block as BlockT, Header as HeaderT};

pub use self::{
	builder::{
		build_default_block_downloader, build_default_syncing_engine, build_network,
		build_network_advanced, build_polkadot_syncing_strategy, default_gap_sync_body_policy,
		gen_rpc_module, init_telemetry, new_client, new_db_backend, new_full_client,
		new_full_parts, new_full_parts_record_import, new_full_parts_with_genesis_builder,
		new_wasm_executor, propagate_transaction_notifications, spawn_tasks,
		BuildNetworkAdvancedParams, BuildNetworkParams, DefaultSyncingEngineConfig,
		KeystoreContainer, SpawnTasksParams, TFullBackend, TFullCallExecutor, TFullClient,
	},
	client::{ClientConfig, LocalCallExecutor},
	error::Error,
	metrics::MetricsService,
};
pub use sc_chain_spec::{
	construct_genesis_block, resolve_state_version_from_wasm, BuildGenesisBlock,
	GenesisBlockBuilder,
};

pub use config::{
	BasePath, BlocksPruning, Configuration, DatabaseSource, PruningMode, Role, RpcMethods, TaskType,
};
pub use sc_chain_spec::{
	ChainSpec, ChainType, Extension as ChainSpecExtension, GenericChainSpec, NoExtension,
	Properties,
};
pub use sc_client_db::PruningFilter;

use crate::config::RpcConfiguration;
use prometheus_endpoint::Registry;
pub use sc_consensus::ImportQueue;
pub use sc_network_sync::WarpSyncConfig;
#[doc(hidden)]
pub use sc_network_transactions::config::{TransactionImport, TransactionImportFuture};
pub use sc_rpc::{RandomIntegerSubscriptionId, RandomStringSubscriptionId};
pub use sc_tracing::TracingReceiver;
pub use sc_transaction_pool::TransactionPoolOptions;
pub use sc_transaction_pool_api::{error::IntoPoolError, InPoolTransaction, TransactionPool};
#[doc(hidden)]
pub use std::{ops::Deref, result::Result, sync::Arc};
pub use task_manager::{
	SpawnEssentialTaskHandle, SpawnTaskHandle, Task, TaskManager, TaskRegistry, DEFAULT_GROUP_NAME,
};
use tokio::runtime::Handle;

const DEFAULT_PROTOCOL_ID: &str = "sup";

/// A running RPC service that can perform in-memory RPC queries.
#[derive(Clone)]
pub struct RpcHandlers {
	// This is legacy and may be removed at some point, it was for WASM stuff before smoldot was a
	// thing. https://github.com/paritytech/polkadot-sdk/pull/5038#discussion_r1694971805
	rpc_module: Arc<RpcModule<()>>,

	// This can be used to introspect the port the RPC server is listening on. SDK consumers are
	// depending on this and it should be supported even if in-memory query support is removed.
	listen_addresses: Vec<Multiaddr>,
}

impl RpcHandlers {
	/// Create PRC handlers instance.
	pub fn new(rpc_module: Arc<RpcModule<()>>, listen_addresses: Vec<Multiaddr>) -> Self {
		Self { rpc_module, listen_addresses }
	}

	/// Starts an RPC query.
	///
	/// The query is passed as a string and must be valid JSON-RPC request object.
	///
	/// Returns a response and a stream if the call successful, fails if the
	/// query could not be decoded as a JSON-RPC request object.
	///
	/// If the request subscribes you to events, the `stream` can be used to
	/// retrieve the events.
	pub async fn rpc_query(
		&self,
		json_query: &str,
	) -> Result<(String, tokio::sync::mpsc::Receiver<String>), serde_json::Error> {
		// Because `tokio::sync::mpsc::channel` is used under the hood
		// it will panic if it's set to usize::MAX.
		//
		// This limit is used to prevent panics and is large enough.
		const TOKIO_MPSC_MAX_SIZE: usize = tokio::sync::Semaphore::MAX_PERMITS;

		self.rpc_module.raw_json_request(json_query, TOKIO_MPSC_MAX_SIZE).await
	}

	/// Provides access to the underlying `RpcModule`
	pub fn handle(&self) -> Arc<RpcModule<()>> {
		self.rpc_module.clone()
	}

	/// Provides access to listen addresses
	pub fn listen_addresses(&self) -> &[Multiaddr] {
		&self.listen_addresses[..]
	}
}

/// An incomplete set of chain components, but enough to run the chain ops subcommands.
pub struct PartialComponents<Client, Backend, SelectChain, ImportQueue, TransactionPool, Other> {
	/// A shared client instance.
	pub client: Arc<Client>,
	/// A shared backend instance.
	pub backend: Arc<Backend>,
	/// The chain task manager.
	pub task_manager: TaskManager,
	/// A keystore container instance.
	pub keystore_container: KeystoreContainer,
	/// A chain selection algorithm instance.
	pub select_chain: SelectChain,
	/// An import queue.
	pub import_queue: ImportQueue,
	/// A shared transaction pool.
	pub transaction_pool: Arc<TransactionPool>,
	/// Everything else that needs to be passed into the main build function.
	pub other: Other,
}

/// Builds a future that continuously polls the network.
async fn build_network_future<
	B: BlockT,
	C: BlockchainEvents<B>
		+ HeaderBackend<B>
		+ BlockBackend<B>
		+ HeaderMetadata<B, Error = sp_blockchain::Error>
		+ ProofProvider<B>
		+ Send
		+ Sync
		+ 'static,
	H: sc_network_common::ExHashT,
	N: NetworkBackend<B, <B as BlockT>::Hash>,
>(
	network: N,
	client: Arc<C>,
	sync_service: Arc<SyncingService<B>>,
	announce_imported_blocks: bool,
) {
	let mut imported_blocks_stream = client.import_notification_stream().fuse();

	// Stream of finalized blocks reported by the client.
	let mut finality_notification_stream = client.finality_notification_stream().fuse();

	let network_run = network.run().fuse();
	pin_mut!(network_run);

	loop {
		futures::select! {
			// List of blocks that the client has imported.
			notification = imported_blocks_stream.next() => {
				let notification = match notification {
					Some(n) => n,
					// If this stream is shut down, that means the client has shut down, and the
					// most appropriate thing to do for the network future is to shut down too.
					None => {
						warn!("Block import stream has terminated, shutting down the network future. Ignore if the node is stopping.");
						return
					},
				};

				if announce_imported_blocks {
					sync_service.announce_block(notification.hash, None);
				}

				if notification.is_new_best {
					sync_service.new_best_block_imported(
						notification.hash,
						*notification.header.number(),
					);
				}
			}

			// List of blocks that the client has finalized.
			notification = finality_notification_stream.select_next_some() => {
				sync_service.on_block_finalized(notification.hash, notification.header);
			}

			// Drive the network. Shut down the network future if `NetworkWorker` has terminated.
			_ = network_run => {
				warn!("`NetworkWorker` has terminated, shutting down the network future. Ignore if the node is stopping.");
				return
			}
		}
	}
}

/// Builds a future that processes system RPC requests.
pub async fn build_system_rpc_future<
	B: BlockT,
	C: BlockchainEvents<B>
		+ HeaderBackend<B>
		+ BlockBackend<B>
		+ HeaderMetadata<B, Error = sp_blockchain::Error>
		+ ProofProvider<B>
		+ Send
		+ Sync
		+ 'static,
	H: sc_network_common::ExHashT,
>(
	role: Role,
	network_service: Arc<dyn NetworkService>,
	sync_service: Arc<SyncingService<B>>,
	client: Arc<C>,
	mut rpc_rx: TracingUnboundedReceiver<sc_rpc::system::Request<B>>,
	should_have_peers: bool,
) {
	// Current best block at initialization, to report to the RPC layer.
	let starting_block = client.info().best_number;

	loop {
		// Answer incoming RPC requests.
		let Some(req) = rpc_rx.next().await else {
			debug!("RPC requests stream has terminated, shutting down the system RPC future.");
			return;
		};

		match req {
			sc_rpc::system::Request::Health(sender) => match sync_service.peers_info().await {
				Ok(info) => {
					let _ = sender.send(sc_rpc::system::Health {
						peers: info.len(),
						is_syncing: sync_service.is_major_syncing(),
						should_have_peers,
					});
				},
				Err(_) => log::error!("`SyncingEngine` shut down"),
			},
			sc_rpc::system::Request::LocalPeerId(sender) => {
				let _ = sender.send(network_service.local_peer_id().to_base58());
			},
			sc_rpc::system::Request::LocalListenAddresses(sender) => {
				let local_peer_id = network_service.local_peer_id();
				let addresses = network_service
					.listen_addresses()
					.iter()
					.map(|address| with_local_peer_id(address.clone(), local_peer_id).to_string())
					.collect();
				let _ = sender.send(addresses);
			},
			sc_rpc::system::Request::Peers(sender) => match sync_service.peers_info().await {
				Ok(info) => {
					let _ = sender.send(
						info.into_iter()
							.map(|(peer_id, p)| sc_rpc::system::PeerInfo {
								peer_id: peer_id.to_base58(),
								roles: format!("{:?}", p.roles),
								best_hash: p.best_hash,
								best_number: p.best_number,
							})
							.collect(),
					);
				},
				Err(_) => log::error!("`SyncingEngine` shut down"),
			},
			sc_rpc::system::Request::NetworkState(sender) => {
				let network_state = network_service.network_state().await;
				if let Ok(network_state) = network_state {
					if let Ok(network_state) = serde_json::to_value(network_state) {
						let _ = sender.send(network_state);
					}
				} else {
					break;
				}
			},
			sc_rpc::system::Request::NetworkAddReservedPeer(peer_addr, sender) => {
				let result = match MultiaddrWithPeerId::try_from(peer_addr) {
					Ok(peer) => network_service.add_reserved_peer(peer),
					Err(err) => Err(err.to_string()),
				};
				let x = result.map_err(sc_rpc::system::error::Error::MalformattedPeerArg);
				let _ = sender.send(x);
			},
			sc_rpc::system::Request::NetworkRemoveReservedPeer(peer_id, sender) => {
				let _ = match peer_id.parse::<PeerId>() {
					Ok(peer_id) => {
						network_service.remove_reserved_peer(peer_id);
						sender.send(Ok(()))
					},
					Err(e) => sender.send(Err(sc_rpc::system::error::Error::MalformattedPeerArg(
						e.to_string(),
					))),
				};
			},
			sc_rpc::system::Request::NetworkReservedPeers(sender) => {
				let Ok(reserved_peers) = network_service.reserved_peers().await else {
					break;
				};

				let _ =
					sender.send(reserved_peers.iter().map(|peer_id| peer_id.to_base58()).collect());
			},
			sc_rpc::system::Request::NodeRoles(sender) => {
				use sc_rpc::system::NodeRole;

				let node_role = match role {
					Role::Authority { .. } => NodeRole::Authority,
					Role::Full => NodeRole::Full,
				};

				let _ = sender.send(vec![node_role]);
			},
			sc_rpc::system::Request::SyncState(sender) => {
				use sc_rpc::system::SyncState;

				match sync_service.status().await.map(|status| status.best_seen_block) {
					Ok(best_seen_block) => {
						let best_number = client.info().best_number;
						let _ = sender.send(SyncState {
							starting_block,
							current_block: best_number,
							highest_block: best_seen_block.unwrap_or(best_number),
						});
					},
					Err(_) => log::error!("`SyncingEngine` shut down"),
				}
			},
		}
	}

	debug!("`NetworkWorker` has terminated, shutting down the system RPC future.");
}

/// Appends `/p2p/<local peer id>` unless the address already carries one.
fn with_local_peer_id(address: Multiaddr, local_peer_id: PeerId) -> Multiaddr {
	// litep2p backend appends local peer id to every listen address it reports,
	// the libp2p backend does not.
	match address.iter().last() {
		Some(Protocol::P2p(_)) => address,
		_ => address.with(Protocol::P2p(local_peer_id.into())),
	}
}

/// Starts RPC servers.
pub fn start_rpc_servers(
	rpc_configuration: &RpcConfiguration,
	registry: Option<&Registry>,
	tokio_handle: &Handle,
	rpc_api: RpcModule<()>,
	rpc_runtime: tokio::runtime::Runtime,
	rpc_id_provider: Option<Box<dyn sc_rpc_server::SubscriptionIdProvider>>,
) -> Result<Server, error::Error> {
	let endpoints: Vec<sc_rpc_server::RpcEndpoint> = if let Some(endpoints) =
		rpc_configuration.addr.as_ref()
	{
		endpoints.clone()
	} else {
		let ipv6 =
			SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, rpc_configuration.port, 0, 0));
		let ipv4 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, rpc_configuration.port));

		vec![
			sc_rpc_server::RpcEndpoint {
				batch_config: rpc_configuration.batch_config,
				cors: rpc_configuration.cors.clone(),
				listen_addr: ipv4,
				max_buffer_capacity_per_connection: rpc_configuration.message_buffer_capacity,
				max_connections: rpc_configuration.max_connections,
				max_payload_in_mb: rpc_configuration.max_request_size,
				max_payload_out_mb: rpc_configuration.max_response_size,
				max_subscriptions_per_connection: rpc_configuration.max_subs_per_conn,
				rpc_methods: rpc_configuration.methods.into(),
				rate_limit: rpc_configuration.rate_limit,
				rate_limit_trust_proxy_headers: rpc_configuration.rate_limit_trust_proxy_headers,
				rate_limit_whitelisted_ips: rpc_configuration.rate_limit_whitelisted_ips.clone(),
				retry_random_port: true,
				is_optional: false,
			},
			sc_rpc_server::RpcEndpoint {
				batch_config: rpc_configuration.batch_config,
				cors: rpc_configuration.cors.clone(),
				listen_addr: ipv6,
				max_buffer_capacity_per_connection: rpc_configuration.message_buffer_capacity,
				max_connections: rpc_configuration.max_connections,
				max_payload_in_mb: rpc_configuration.max_request_size,
				max_payload_out_mb: rpc_configuration.max_response_size,
				max_subscriptions_per_connection: rpc_configuration.max_subs_per_conn,
				rpc_methods: rpc_configuration.methods.into(),
				rate_limit: rpc_configuration.rate_limit,
				rate_limit_trust_proxy_headers: rpc_configuration.rate_limit_trust_proxy_headers,
				rate_limit_whitelisted_ips: rpc_configuration.rate_limit_whitelisted_ips.clone(),
				retry_random_port: true,
				is_optional: true,
			},
		]
	};

	let metrics = sc_rpc_server::RpcMetrics::new(registry)?;

	let server_config = sc_rpc_server::Config {
		endpoints,
		metrics,
		rpc_api,
		id_provider: rpc_id_provider,
		request_logger_limit: rpc_configuration.request_logger_limit,
		rpc_runtime,
	};

	// TODO: https://github.com/paritytech/substrate/issues/13773
	//
	// `block_in_place` is a hack to allow callers to call `block_on` prior to
	// calling `start_rpc_servers`.
	match tokio::task::block_in_place(|| {
		tokio_handle.block_on(sc_rpc_server::start_server(server_config))
	}) {
		Ok(server) => Ok(server),
		Err(e) => Err(Error::Application(e)),
	}
}

/// Transaction pool adapter.
pub struct TransactionPoolAdapter<C, P> {
	pool: Arc<P>,
	client: Arc<C>,
}

impl<C, P> TransactionPoolAdapter<C, P> {
	/// Constructs a new instance of [`TransactionPoolAdapter`].
	pub fn new(pool: Arc<P>, client: Arc<C>) -> Self {
		Self { pool, client }
	}
}

/// Get transactions for propagation.
///
/// Function extracted to simplify the test and prevent creating `ServiceFactory`.
fn transactions_to_propagate<Pool, B, H, E>(pool: &Pool) -> Vec<(H, Arc<B::Extrinsic>)>
where
	Pool: TransactionPool<Block = B, Hash = H, Error = E>,
	B: BlockT,
	H: std::hash::Hash + Eq + sp_runtime::traits::Member + sp_runtime::traits::MaybeSerialize,
	E: IntoPoolError + From<sc_transaction_pool_api::error::Error>,
{
	pool.ready()
		.filter(|t| t.is_propagable())
		.map(|t| {
			let hash = t.hash().clone();
			let ex = t.data().clone();
			(hash, ex)
		})
		.collect()
}

impl<B, H, C, Pool, E> sc_network_transactions::config::TransactionPool<H, B>
	for TransactionPoolAdapter<C, Pool>
where
	C: HeaderBackend<B>
		+ BlockBackend<B>
		+ HeaderMetadata<B, Error = sp_blockchain::Error>
		+ ProofProvider<B>
		+ Send
		+ Sync
		+ 'static,
	Pool: 'static + TransactionPool<Block = B, Hash = H, Error = E>,
	B: BlockT,
	H: std::hash::Hash + Eq + sp_runtime::traits::Member + sp_runtime::traits::MaybeSerialize,
	E: 'static + IntoPoolError + From<sc_transaction_pool_api::error::Error>,
{
	fn transactions(&self) -> Vec<(H, Arc<B::Extrinsic>)> {
		transactions_to_propagate(&*self.pool)
	}

	fn hash_of(&self, transaction: &B::Extrinsic) -> H {
		self.pool.hash_of(transaction)
	}

	fn import(&self, transaction: B::Extrinsic) -> TransactionImportFuture {
		let encoded = transaction.encode();
		let uxt = match Decode::decode(&mut &encoded[..]) {
			Ok(uxt) => uxt,
			Err(e) => {
				debug!(target: sc_transaction_pool::LOG_TARGET, "Transaction invalid: {:?}", e);
				return Box::pin(futures::future::ready(TransactionImport::Bad));
			},
		};

		let start = std::time::Instant::now();
		let pool = self.pool.clone();
		let client = self.client.clone();
		Box::pin(async move {
			match pool
				.submit_one(
					client.info().best_hash,
					sc_transaction_pool_api::TransactionSource::External,
					uxt,
				)
				.await
			{
				Ok(_) => {
					let elapsed = start.elapsed();
					trace!(target: sc_transaction_pool::LOG_TARGET, "import transaction: {elapsed:?}");
					TransactionImport::NewGood
				},
				Err(e) => match e.into_pool_error() {
					Ok(sc_transaction_pool_api::error::Error::AlreadyImported(_)) => {
						TransactionImport::KnownGood
					},
					Ok(_) => TransactionImport::Bad,
					Err(_) => {
						// it is not bad at least, just some internal node logic error, so peer is
						// innocent.
						TransactionImport::KnownGood
					},
				},
			}
		})
	}

	fn on_broadcasted(&self, propagations: HashMap<H, Vec<String>>) {
		self.pool.on_broadcasted(propagations)
	}

	fn transaction(&self, hash: &H) -> Option<Arc<B::Extrinsic>> {
		self.pool.ready_transaction(hash).and_then(
			// Only propagable transactions should be resolved for network service.
			|tx| tx.is_propagable().then(|| tx.data().clone()),
		)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use futures::executor::block_on;
	use sc_network_types::multihash::Code;
	use sc_transaction_pool::BasicPool;
	use sp_consensus::SelectChain;
	use substrate_test_runtime_client::{
		prelude::*,
		runtime::{ExtrinsicBuilder, Transfer, TransferData},
	};

	/// `/ip4/1.2.3.4/tcp/30333`, as the libp2p backend reports its listen addresses.
	fn tcp_address() -> Multiaddr {
		Multiaddr::empty()
			.with(Protocol::Ip4([1, 2, 3, 4].into()))
			.with(Protocol::Tcp(30333))
	}

	#[test]
	fn peer_id_appended_when_missing() {
		let peer_id = PeerId::random();

		assert_eq!(
			with_local_peer_id(tcp_address(), peer_id),
			tcp_address().with(Protocol::P2p(peer_id.into())),
		);
	}

	#[test]
	fn peer_id_not_appended_twice() {
		// The litep2p backend hands out addresses that already carry the local peer id.
		let peer_id = PeerId::random();
		let address = tcp_address().with(Protocol::P2p(peer_id.into()));

		assert_eq!(with_local_peer_id(address.clone(), peer_id), address);
	}

	#[test]
	fn webrtc_certhash_preserved() {
		// This is the address shape a WebRTC-enabled node hands to smoldot.
		let peer_id = PeerId::random();
		let address = Multiaddr::empty()
			.with(Protocol::Ip4([1, 2, 3, 4].into()))
			.with(Protocol::Udp(30334))
			.with(Protocol::WebRTCDirect)
			.with(Protocol::Certhash(Code::Sha2_256.digest(b"certificate")))
			.with(Protocol::P2p(peer_id.into()));

		assert_eq!(with_local_peer_id(address.clone(), peer_id), address);
	}

	#[test]
	fn should_not_propagate_transactions_that_are_marked_as_such() {
		// given
		let (client, longest_chain) = TestClientBuilder::new().build_with_longest_chain();
		let client = Arc::new(client);
		let spawner = sp_core::testing::TaskExecutor::new();
		let pool = Arc::from(BasicPool::new_full(
			Default::default(),
			true.into(),
			None,
			spawner,
			client.clone(),
		));
		let source = sp_runtime::transaction_validity::TransactionSource::External;
		let best = block_on(longest_chain.best_chain()).unwrap();
		let transaction = Transfer {
			amount: 5,
			nonce: 0,
			from: Sr25519Keyring::Alice.into(),
			to: Sr25519Keyring::Bob.into(),
		}
		.into_unchecked_extrinsic();
		block_on(pool.submit_one(best.hash(), source, transaction.clone())).unwrap();
		block_on(pool.submit_one(
			best.hash(),
			source,
			ExtrinsicBuilder::new_call_do_not_propagate().nonce(1).build(),
		))
		.unwrap();
		assert_eq!(pool.status().ready, 2);

		// when
		let transactions = transactions_to_propagate(&*pool);

		// then
		assert_eq!(transactions.len(), 1);
		assert!(TransferData::try_from(&*transactions[0].1).is_ok());
	}
}
