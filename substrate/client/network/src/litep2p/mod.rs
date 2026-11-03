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

//! `NetworkBackend` implementation for `litep2p`.

use crate::{
	config::{
		FullNetworkConfiguration, IncomingRequest, NodeKeyConfig, NotificationHandshake, Params,
		SetConfig, TransportConfig,
	},
	error::Error,
	event::{DhtEvent, Event},
	litep2p::{
		discovery::{Discovery, DiscoveryEvent},
		ipfs_dht::IpfsDht,
		peerstore::Peerstore,
		service::{Litep2pNetworkService, NetworkServiceCommand},
		shim::{
			notification::{
				config::{NotificationProtocolConfig, ProtocolControlHandle},
				peerset::PeersetCommand,
			},
			request_response::{RequestResponseConfig, RequestResponseProtocol},
		},
	},
	peer_store::PeerStoreProvider,
	service::{
		metrics::{register_without_sources, MetricSources, Metrics, NotificationMetrics},
		out_events,
		traits::{BandwidthSink, NetworkBackend, NetworkService},
	},
	webrtc, NetworkStatus, NotificationService, ProtocolName,
};

use codec::Encode;
use futures::StreamExt;
use litep2p::{
	config::ConfigBuilder,
	crypto::ed25519::Keypair,
	error::{DialError, NegotiationError},
	executor::Executor,
	protocol::{
		libp2p::kademlia::{QueryId, Record},
		request_response::ConfigBuilder as RequestResponseConfigBuilder,
	},
	transport::{
		tcp::config::Config as TcpTransportConfig, webrtc::config::Config as WebRtcTransportConfig,
		websocket::config::Config as WebSocketTransportConfig, ConnectionLimitsConfig, Endpoint,
	},
	types::{
		multiaddr::{Multiaddr, Protocol},
		ConnectionId,
	},
	Litep2p, Litep2pEvent, ProtocolName as Litep2pProtocolName,
};
use prometheus_endpoint::Registry;

use sc_network_common::{role::Roles, ExHashT};
use sc_network_types::{
	kad::{Key as RecordKey, PeerRecord, Record as P2PRecord},
	multiaddr::Protocol as NetworkProtocol,
	PeerId,
};
use sc_utils::mpsc::{tracing_unbounded, TracingUnboundedReceiver};
use sp_runtime::traits::Block as BlockT;

use std::{
	cmp,
	collections::{hash_map::Entry, HashMap, HashSet},
	fs,
	future::Future,
	iter,
	pin::Pin,
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	},
	time::{Duration, Instant},
};

mod discovery;
mod ipfs_dht;
mod peerstore;
mod service;
mod shim;

/// Litep2p bandwidth sink.
struct Litep2pBandwidthSink {
	sink: litep2p::BandwidthSink,
}

impl BandwidthSink for Litep2pBandwidthSink {
	fn total_inbound(&self) -> u64 {
		self.sink.inbound() as u64
	}

	fn total_outbound(&self) -> u64 {
		self.sink.outbound() as u64
	}
}

/// Litep2p task executor.
struct Litep2pExecutor {
	/// Executor.
	executor: Box<dyn Fn(Pin<Box<dyn Future<Output = ()> + Send>>) + Send + Sync>,
}

impl Executor for Litep2pExecutor {
	fn run(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
		(self.executor)(future)
	}

	fn run_with_name(&self, _: &'static str, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
		(self.executor)(future)
	}
}

/// Logging target for the file.
const LOG_TARGET: &str = "sub-libp2p";

/// Peer context.
struct ConnectionContext {
	/// Peer endpoints.
	endpoints: HashMap<ConnectionId, Endpoint>,

	/// Number of active connections.
	num_connections: usize,
}

/// Kademlia query we are tracking.
#[derive(Debug)]
enum KadQuery {
	/// `FIND_NODE` query for target and when it was initiated.
	FindNode(PeerId, Instant),
	/// `GET_VALUE` query for key and when it was initiated.
	GetValue(RecordKey, Instant),
	/// `PUT_VALUE` query for key and when it was initiated.
	PutValue(RecordKey, Instant),
	/// `GET_PROVIDERS` query for key and when it was initiated.
	GetProviders(RecordKey, Instant),
	/// `ADD_PROVIDER` query for key and when it was initiated.
	AddProvider(RecordKey, Instant),
}

/// Networking backend for `litep2p`.
pub struct Litep2pNetworkBackend {
	/// Main `litep2p` object.
	litep2p: Litep2p,

	/// `NetworkService` implementation for `Litep2pNetworkBackend`.
	network_service: Arc<dyn NetworkService>,

	/// RX channel for receiving commands from `Litep2pNetworkService`.
	cmd_rx: TracingUnboundedReceiver<NetworkServiceCommand>,

	/// `Peerset` handles to notification protocols.
	peerset_handles: HashMap<ProtocolName, ProtocolControlHandle>,

	/// Pending Kademlia queries.
	pending_queries: HashMap<QueryId, KadQuery>,

	/// Discovery.
	discovery: Discovery,

	/// Number of connected peers.
	num_connected: Arc<AtomicUsize>,

	/// Connected peers.
	peers: HashMap<litep2p::PeerId, ConnectionContext>,

	/// Peerstore.
	peerstore_handle: Arc<dyn PeerStoreProvider>,

	/// Block announce protocol name.
	block_announce_protocol: ProtocolName,

	/// Sender for DHT events.
	event_streams: out_events::OutChannels,

	/// Prometheus metrics.
	metrics: Option<Metrics>,
}

impl Litep2pNetworkBackend {
	/// From an iterator of multiaddress(es), parse and group all addresses of peers
	/// so that litep2p can consume the information easily.
	fn parse_addresses(
		addresses: impl Iterator<Item = Multiaddr>,
	) -> HashMap<PeerId, Vec<Multiaddr>> {
		addresses
			.into_iter()
			.filter_map(|address| match address.iter().next() {
				Some(
					Protocol::Dns(_) |
					Protocol::Dns4(_) |
					Protocol::Dns6(_) |
					Protocol::Ip6(_) |
					Protocol::Ip4(_),
				) => match address.iter().find(|protocol| std::matches!(protocol, Protocol::P2p(_)))
				{
					Some(Protocol::P2p(peer_id)) => Some((peer_id.into(), Some(address))),
					_ => None,
				},
				Some(Protocol::P2p(peer_id)) => Some((peer_id.into(), None)),
				_ => None,
			})
			.fold(HashMap::new(), |mut acc, (peer, maybe_address)| {
				let entry = acc.entry(peer).or_default();
				maybe_address.map(|address| entry.push(address));

				acc
			})
	}

	/// Add new known addresses to `litep2p` and return the parsed peer IDs.
	fn add_addresses(&mut self, peers: impl Iterator<Item = Multiaddr>) -> HashSet<PeerId> {
		Self::parse_addresses(peers.into_iter())
			.into_iter()
			.filter_map(|(peer, addresses)| {
				// `peers` contained multiaddress in the form `/p2p/<peer ID>`
				if addresses.is_empty() {
					return Some(peer);
				}

				if self.litep2p.add_known_address(peer.into(), addresses.clone().into_iter()) == 0 {
					log::warn!(
						target: LOG_TARGET,
						"couldn't add any addresses for {peer:?} and it won't be added as reserved peer",
					);
					return None;
				}

				self.peerstore_handle.add_known_peer(peer);
				Some(peer)
			})
			.collect()
	}
}

impl Litep2pNetworkBackend {
	/// Get `litep2p` keypair from `NodeKeyConfig`.
	fn get_keypair(node_key: &NodeKeyConfig) -> Result<(Keypair, litep2p::PeerId), Error> {
		let secret: litep2p::crypto::ed25519::SecretKey =
			node_key.clone().into_keypair()?.secret().into();

		let local_identity = Keypair::from(secret);
		let local_public = local_identity.public();
		let local_peer_id = local_public.to_peer_id();

		Ok((local_identity, local_peer_id))
	}

	/// Configure transport protocols for `Litep2pNetworkBackend`.
	fn configure_transport<B: BlockT + 'static, H: ExHashT>(
		config: &FullNetworkConfiguration<B, H, Self>,
		keypair: Keypair,
	) -> Result<ConfigBuilder, Error> {
		let _ = match config.network_config.transport {
			TransportConfig::MemoryOnly => panic!("memory transport not supported"),
			TransportConfig::Normal { .. } => false,
		};
		let config_builder = ConfigBuilder::new();

		let listen_addr_len = config.network_config.listen_addresses.len();
		let mut tcp_addresses = Vec::with_capacity(listen_addr_len);
		let mut websocket_addresses = Vec::with_capacity(listen_addr_len);
		let mut webrtc_addresses = Vec::with_capacity(listen_addr_len);

		for addr in &config.network_config.listen_addresses {
			let mut iter = addr.iter();

			let ip_version = iter.next();
			let Some(NetworkProtocol::Ip4(_) | NetworkProtocol::Ip6(_)) = ip_version else {
				log::error!(
					target: LOG_TARGET,
					"unknown protocol {ip_version:?}, ignoring {addr:?}",
				);
				continue;
			};

			let transport_layer = iter.next();
			let protocol_type = iter.next();

			match (&transport_layer, &protocol_type) {
				// Plain TCP address.
				(Some(NetworkProtocol::Tcp(_)), Some(NetworkProtocol::P2p(_)) | None) => {
					tcp_addresses.push(addr.clone());
				},
				// Websocket address.
				(
					Some(NetworkProtocol::Tcp(_)),
					Some(NetworkProtocol::Ws(_) | NetworkProtocol::Wss(_)),
				) => {
					websocket_addresses.push(addr.clone());
				},
				// WebRTC-Direct address.
				(Some(NetworkProtocol::Udp(_)), Some(NetworkProtocol::WebRTCDirect)) => {
					// An address carrying anything past `webrtc-direct` is rejected.
					webrtc::validate_listen_address(addr)?;
					webrtc_addresses.push(addr.clone());
				},
				_ => {
					log::error!(
						target: LOG_TARGET,
						"unknown transport layer {transport_layer:?} and protocol type {protocol_type:?}, ignoring {addr:?}",
					);
				},
			};
		}

		let mut config_builder = config_builder
			.with_websocket(WebSocketTransportConfig {
				listen_addresses: websocket_addresses.into_iter().map(Into::into).collect(),
				yamux_config: litep2p::yamux::Config::default(),
				nodelay: true,
				..Default::default()
			})
			.with_tcp(TcpTransportConfig {
				listen_addresses: tcp_addresses.into_iter().map(Into::into).collect(),
				yamux_config: litep2p::yamux::Config::default(),
				nodelay: true,
				..Default::default()
			});

		if !webrtc_addresses.is_empty() {
			let certificate =
				webrtc::derive_certificate(keypair.secret()).map_err(Error::Litep2p)?;
			log::info!(target: LOG_TARGET, "WebRTC certhash: {}", certificate.certhash_b64());
			config_builder = config_builder.with_webrtc(WebRtcTransportConfig {
				listen_addresses: webrtc_addresses.into_iter().map(Into::into).collect(),
				certificate: Some(certificate),
				..Default::default()
			});
		}

		Ok(config_builder.with_keypair(keypair))
	}
}

#[async_trait::async_trait]
impl<B: BlockT + 'static, H: ExHashT> NetworkBackend<B, H> for Litep2pNetworkBackend {
	const SUPPORTS_IPFS: bool = true;

	type NotificationProtocolConfig = NotificationProtocolConfig;
	type RequestResponseProtocolConfig = RequestResponseConfig;
	type NetworkService<Block, Hash> = Arc<Litep2pNetworkService>;
	type PeerStore = Peerstore;

	fn new(mut params: Params<B, H, Self>) -> Result<Self, Error>
	where
		Self: Sized,
	{
		// Install the ring CryptoProvider for rustls before any TLS connections are made.
		if let Err(err) = rustls::crypto::ring::default_provider().install_default() {
			log::warn!(
				target: LOG_TARGET,
				"failed to install ring CryptoProvider for rustls, another provider might be installed: {err:?}",
			);
		}

		let (keypair, local_peer_id) =
			Self::get_keypair(&params.network_config.network_config.node_key)?;
		let (cmd_tx, cmd_rx) = tracing_unbounded("mpsc_network_worker", 100_000);

		params.network_config.network_config.boot_nodes = params
			.network_config
			.network_config
			.boot_nodes
			.into_iter()
			.filter(|boot_node| boot_node.peer_id != local_peer_id.into())
			.collect();
		params.network_config.network_config.default_peers_set.reserved_nodes = params
			.network_config
			.network_config
			.default_peers_set
			.reserved_nodes
			.into_iter()
			.filter(|reserved_node| {
				if reserved_node.peer_id == local_peer_id.into() {
					log::warn!(
						target: LOG_TARGET,
						"Local peer ID used in reserved node, ignoring: {reserved_node}",
					);
					false
				} else {
					true
				}
			})
			.collect();

		if let Some(path) = &params.network_config.network_config.net_config_path {
			fs::create_dir_all(path)?;
		}

		log::info!(target: LOG_TARGET, "Local node identity is: {local_peer_id}");
		log::info!(target: LOG_TARGET, "Running litep2p network backend");

		params.network_config.sanity_check_addresses()?;
		params.network_config.sanity_check_bootnodes()?;

		let mut config_builder =
			Self::configure_transport(&params.network_config, keypair.clone())?;
		let known_addresses = params.network_config.known_addresses();
		let peer_store_handle = params.network_config.peer_store_handle();
		let executor = Arc::new(Litep2pExecutor { executor: params.executor });

		let FullNetworkConfiguration {
			notification_protocols,
			request_response_protocols,
			network_config,
			..
		} = params.network_config;

		// initialize notification protocols
		//
		// pass the protocol configuration to `Litep2pConfigBuilder` and save the TX channel
		// to the protocol's `Peerset` together with the protocol name to allow other subsystems
		// of Polkadot SDK to control connectivity of the notification protocol
		let block_announce_protocol = params.block_announce_config.protocol_name().clone();
		let mut notif_protocols = HashMap::from_iter([(
			params.block_announce_config.protocol_name().clone(),
			params.block_announce_config.handle,
		)]);

		// handshake for all but the syncing protocol is set to node role
		config_builder = notification_protocols
			.into_iter()
			.fold(config_builder, |config_builder, mut config| {
				config.config.set_handshake(Roles::from(&params.role).encode());
				notif_protocols.insert(config.protocol_name, config.handle);

				config_builder.with_notification_protocol(config.config)
			})
			.with_notification_protocol(params.block_announce_config.config);

		// initialize request-response protocols
		let metrics = match &params.metrics_registry {
			Some(registry) => Some(register_without_sources(registry)?),
			None => None,
		};

		// create channels that are used to send request before initializing protocols so the
		// senders can be passed onto all request-response protocols
		//
		// all protocols must have each others' senders so they can send the fallback request in
		// case the main protocol is not supported by the remote peer and user specified a fallback
		let (mut request_response_receivers, request_response_senders): (
			HashMap<_, _>,
			HashMap<_, _>,
		) = request_response_protocols
			.iter()
			.map(|config| {
				let (tx, rx) = tracing_unbounded("outbound-requests", 10_000);
				((config.protocol_name.clone(), rx), (config.protocol_name.clone(), tx))
			})
			.unzip();

		config_builder = request_response_protocols.into_iter().fold(
			config_builder,
			|config_builder, config| {
				let (protocol_config, handle) = RequestResponseConfigBuilder::new(
					Litep2pProtocolName::from(config.protocol_name.clone()),
				)
				.with_max_size(cmp::max(config.max_request_size, config.max_response_size) as usize)
				.with_fallback_names(config.fallback_names.into_iter().map(From::from).collect())
				.with_timeout(config.request_timeout)
				.build();

				let protocol = RequestResponseProtocol::new(
					config.protocol_name.clone(),
					handle,
					Arc::clone(&peer_store_handle),
					config.inbound_queue,
					request_response_receivers
						.remove(&config.protocol_name)
						.expect("receiver exists as it was just added and there are no duplicate protocols; qed"),
					request_response_senders.clone(),
					metrics.clone(),
				);

				executor.run(Box::pin(async move {
					protocol.run().await;
				}));

				config_builder.with_request_response_protocol(protocol_config)
			},
		);

		// collect known addresses
		let known_addresses: HashMap<litep2p::PeerId, Vec<Multiaddr>> =
			known_addresses.into_iter().fold(HashMap::new(), |mut acc, (peer, address)| {
				let address = match address.iter().last() {
					Some(
						NetworkProtocol::Ws(_) | NetworkProtocol::Wss(_) | NetworkProtocol::Tcp(_),
					) => address.with(NetworkProtocol::P2p(peer.into())),
					Some(NetworkProtocol::WebRTCDirect | NetworkProtocol::Certhash(_)) => {
						address.with(NetworkProtocol::P2p(peer.into()))
					},
					Some(NetworkProtocol::P2p(_)) => address,
					_ => return acc,
				};

				acc.entry(peer.into()).or_default().push(address.into());
				peer_store_handle.add_known_peer(peer);

				acc
			});

		// enable ipfs ping, identify and kademlia, and potentially mdns if user enabled it
		let listen_addresses = Arc::new(Default::default());
		let (discovery, ping_config, identify_config, kademlia_config, maybe_mdns_config) =
			Discovery::new(
				local_peer_id,
				&network_config,
				params.genesis_hash,
				params.fork_id.as_deref(),
				&params.protocol_id,
				known_addresses.clone(),
				Arc::clone(&listen_addresses),
				Arc::clone(&peer_store_handle),
			);

		if let Some(ipfs) = params.ipfs_config {
			config_builder = config_builder.with_libp2p_bitswap(ipfs.litep2p_bitswap_config);

			if !ipfs.bootnodes.is_empty() {
				let (ipfs_dht, kad_config) = IpfsDht::new(ipfs.bootnodes, ipfs.block_provider);
				config_builder = config_builder.with_libp2p_kademlia(kad_config);
				executor.run(Box::pin(ipfs_dht.run()));
			} else {
				log::warn!(
					target: LOG_TARGET,
					"Not starting IPFS DHT publisher because no IPFS bootnodes are configured. \
					 Only direct Bitswap requests will be handled.",
				);
			}
		}

		config_builder = config_builder
			.with_known_addresses(known_addresses.clone().into_iter())
			.with_libp2p_ping(ping_config)
			.with_libp2p_identify(identify_config)
			.with_libp2p_kademlia(kademlia_config)
			.with_connection_limits(ConnectionLimitsConfig::default().max_incoming_connections(
				Some(crate::MAX_CONNECTIONS_ESTABLISHED_INCOMING as usize),
			))
			.with_keep_alive_timeout(network_config.idle_connection_timeout)
			// Use system DNS resolver to enable intranet domain resolution and administrator
			// control over DNS lookup.
			.with_system_resolver()
			.with_executor(executor);

		if let Some(config) = maybe_mdns_config {
			config_builder = config_builder.with_mdns(config);
		}

		let litep2p =
			Litep2p::new(config_builder.build()).map_err(|error| Error::Litep2p(error))?;

		litep2p.listen_addresses().for_each(|address| {
			log::debug!(target: LOG_TARGET, "listening on: {address}");

			listen_addresses.write().insert(address.clone());
		});

		let public_addresses = litep2p.public_addresses();
		for address in network_config.public_addresses.iter() {
			if let Err(err) = public_addresses.add_address(address.clone().into()) {
				log::warn!(
					target: LOG_TARGET,
					"failed to add public address {address:?}: {err:?}",
				);
			}
		}

		let network_service = Arc::new(Litep2pNetworkService::new(
			local_peer_id,
			keypair.clone(),
			cmd_tx,
			Arc::clone(&peer_store_handle),
			notif_protocols.clone(),
			block_announce_protocol.clone(),
			request_response_senders,
			Arc::clone(&listen_addresses),
			public_addresses,
		));

		// register rest of the metrics now that `Litep2p` has been created
		let num_connected = Arc::new(Default::default());
		let bandwidth: Arc<dyn BandwidthSink> =
			Arc::new(Litep2pBandwidthSink { sink: litep2p.bandwidth_sink() });

		if let Some(registry) = &params.metrics_registry {
			MetricSources::register(registry, bandwidth, Arc::clone(&num_connected))?;
		}

		Ok(Self {
			network_service,
			cmd_rx,
			metrics,
			peerset_handles: notif_protocols,
			num_connected,
			discovery,
			pending_queries: HashMap::new(),
			peerstore_handle: peer_store_handle,
			block_announce_protocol,
			event_streams: out_events::OutChannels::new(None)?,
			peers: HashMap::new(),
			litep2p,
		})
	}

	fn network_service(&self) -> Arc<dyn NetworkService> {
		Arc::clone(&self.network_service)
	}

	fn peer_store(
		bootnodes: Vec<sc_network_types::PeerId>,
		metrics_registry: Option<Registry>,
	) -> Self::PeerStore {
		Peerstore::new(bootnodes, metrics_registry)
	}

	fn register_notification_metrics(registry: Option<&Registry>) -> NotificationMetrics {
		NotificationMetrics::new(registry)
	}

	/// Create notification protocol configuration for `protocol`.
	fn notification_config(
		protocol_name: ProtocolName,
		fallback_names: Vec<ProtocolName>,
		max_notification_size: u64,
		handshake: Option<NotificationHandshake>,
		set_config: SetConfig,
		metrics: NotificationMetrics,
		peerstore_handle: Arc<dyn PeerStoreProvider>,
	) -> (Self::NotificationProtocolConfig, Box<dyn NotificationService>) {
		Self::NotificationProtocolConfig::new(
			protocol_name,
			fallback_names,
			max_notification_size as usize,
			handshake,
			set_config,
			metrics,
			peerstore_handle,
		)
	}

	/// Create request-response protocol configuration.
	fn request_response_config(
		protocol_name: ProtocolName,
		fallback_names: Vec<ProtocolName>,
		max_request_size: u64,
		max_response_size: u64,
		request_timeout: Duration,
		inbound_queue: Option<async_channel::Sender<IncomingRequest>>,
	) -> Self::RequestResponseProtocolConfig {
		Self::RequestResponseProtocolConfig::new(
			protocol_name,
			fallback_names,
			max_request_size,
			max_response_size,
			request_timeout,
			inbound_queue,
		)
	}

	/// Start [`Litep2pNetworkBackend`] event loop.
	async fn run(mut self) {
		log::debug!(target: LOG_TARGET, "starting litep2p network backend");

		loop {
			let num_connected_peers = self
				.peerset_handles
				.get(&self.block_announce_protocol)
				.map_or(0usize, |handle| handle.connected_peers.load(Ordering::Relaxed));
			self.num_connected.store(num_connected_peers, Ordering::Relaxed);

			tokio::select! {
				command = self.cmd_rx.next() => match command {
					None => return,
					Some(command) => match command {
						NetworkServiceCommand::FindClosestPeers { target } => {
							let query_id = self.discovery.find_node(target.into()).await;
							self.pending_queries.insert(query_id, KadQuery::FindNode(target, Instant::now()));
						}
						NetworkServiceCommand::GetValue{ key } => {
							let query_id = self.discovery.get_value(key.clone()).await;
							self.pending_queries.insert(query_id, KadQuery::GetValue(key, Instant::now()));
						}
						NetworkServiceCommand::PutValue { key, value } => {
							let query_id = self.discovery.put_value(key.clone(), value).await;
							self.pending_queries.insert(query_id, KadQuery::PutValue(key, Instant::now()));
						}
						NetworkServiceCommand::PutValueTo { record, peers, update_local_storage} => {
							let kademlia_key = record.key.clone();
							let query_id = self.discovery.put_value_to_peers(record.into(), peers, update_local_storage).await;
							self.pending_queries.insert(query_id, KadQuery::PutValue(kademlia_key, Instant::now()));
						}
						NetworkServiceCommand::StoreRecord { key, value, publisher, expires } => {
							self.discovery.store_record(key, value, publisher.map(Into::into), expires).await;
						}
						NetworkServiceCommand::StartProviding { key } => {
							let query_id = self.discovery.start_providing(key.clone()).await;
							self.pending_queries.insert(query_id, KadQuery::AddProvider(key, Instant::now()));
						}
						NetworkServiceCommand::StopProviding { key } => {
							self.discovery.stop_providing(key).await;
						}
						NetworkServiceCommand::GetProviders { key } => {
							let query_id = self.discovery.get_providers(key.clone()).await;
							self.pending_queries.insert(query_id, KadQuery::GetProviders(key, Instant::now()));
						}
						NetworkServiceCommand::EventStream { tx } => {
							self.event_streams.push(tx);
						}
						NetworkServiceCommand::Status { tx } => {
							let _ = tx.send(NetworkStatus {
								num_connected_peers: self
									.peerset_handles
									.get(&self.block_announce_protocol)
									.map_or(0usize, |handle| handle.connected_peers.load(Ordering::Relaxed)),
								total_bytes_inbound: self.litep2p.bandwidth_sink().inbound() as u64,
								total_bytes_outbound: self.litep2p.bandwidth_sink().outbound() as u64,
							});
						}
						NetworkServiceCommand::AddPeersToReservedSet {
							protocol,
							peers,
						} => {
							let peers = self.add_addresses(peers.into_iter().map(Into::into));

							match self.peerset_handles.get(&protocol) {
								Some(handle) => {
									let _ = handle.tx.unbounded_send(PeersetCommand::AddReservedPeers { peers });
								}
								None => log::warn!(target: LOG_TARGET, "protocol {protocol} doens't exist"),
							};
						}
						NetworkServiceCommand::AddKnownAddress { peer, address } => {
							let mut address: Multiaddr = address.into();

							if !address.iter().any(|protocol| std::matches!(protocol, Protocol::P2p(_))) {
								address.push(Protocol::P2p(litep2p::PeerId::from(peer).into()));
							}

							if self.litep2p.add_known_address(peer.into(), iter::once(address.clone())) > 0 {
								// libp2p backend generates `DiscoveryOut::Discovered(peer_id)`
								// event when a new address is added for a peer, which leads to the
								// peer being added to peerstore. Do the same directly here.
								self.peerstore_handle.add_known_peer(peer);
							} else {
								log::debug!(
									target: LOG_TARGET,
									"couldn't add known address ({address}) for {peer:?}, unsupported transport"
								);
							}
						},
						NetworkServiceCommand::SetReservedPeers { protocol, peers } => {
							let peers = self.add_addresses(peers.into_iter().map(Into::into));

							match self.peerset_handles.get(&protocol) {
								Some(handle) => {
									let _ = handle.tx.unbounded_send(PeersetCommand::SetReservedPeers { peers });
								}
								None => log::warn!(target: LOG_TARGET, "protocol {protocol} doens't exist"),
							}

						},
						NetworkServiceCommand::DisconnectPeer {
							protocol,
							peer,
						} => {
							let Some(handle) = self.peerset_handles.get(&protocol) else {
								log::warn!(target: LOG_TARGET, "protocol {protocol} doens't exist");
								continue
							};

							let _ = handle.tx.unbounded_send(PeersetCommand::DisconnectPeer { peer });
						}
						NetworkServiceCommand::SetReservedOnly {
							protocol,
							reserved_only,
						} => {
							let Some(handle) = self.peerset_handles.get(&protocol) else {
								log::warn!(target: LOG_TARGET, "protocol {protocol} doens't exist");
								continue
							};

							let _ = handle.tx.unbounded_send(PeersetCommand::SetReservedOnly { reserved_only });
						}
						NetworkServiceCommand::RemoveReservedPeers {
							protocol,
							peers,
						} => {
							let Some(handle) = self.peerset_handles.get(&protocol) else {
								log::warn!(target: LOG_TARGET, "protocol {protocol} doens't exist");
								continue
							};

							let _ = handle.tx.unbounded_send(PeersetCommand::RemoveReservedPeers { peers });
						}
					}
				},
				event = self.discovery.next() => match event {
					None => return,
					Some(DiscoveryEvent::Discovered { addresses }) => {
						// if at least one address was added for the peer, report the peer to `Peerstore`
						for (peer, addresses) in Litep2pNetworkBackend::parse_addresses(addresses.into_iter()) {
							if self.litep2p.add_known_address(peer.into(), addresses.clone().into_iter()) > 0 {
								self.peerstore_handle.add_known_peer(peer);
							}
						}
					}
					Some(DiscoveryEvent::RoutingTableUpdate { peers }) => {
						for peer in peers {
							self.peerstore_handle.add_known_peer(peer.into());
						}
					}
					Some(DiscoveryEvent::FindNodeSuccess { query_id, target, peers }) => {
						match self.pending_queries.remove(&query_id) {
							Some(KadQuery::FindNode(_, started)) => {
								log::trace!(
									target: LOG_TARGET,
									"`FIND_NODE` for {target:?} ({query_id:?}) succeeded",
								);

								self.event_streams.send(
									Event::Dht(
										DhtEvent::ClosestPeersFound(
											target.into(),
											peers
												.into_iter()
												.map(|(peer, addrs)| (
													peer.into(),
													addrs.into_iter().map(Into::into).collect(),
												))
												.collect(),
										)
									)
								);

								if let Some(ref metrics) = self.metrics {
									metrics
										.kademlia_query_duration
										.with_label_values(&["node-find"])
										.observe(started.elapsed().as_secs_f64());
								}
							},
							query => {
								log::error!(
									target: LOG_TARGET,
									"Missing/invalid pending query for `FIND_NODE`: {query:?}"
								);
								debug_assert!(false);
							}
						}
					},
					Some(DiscoveryEvent::GetRecordPartialResult { query_id, record }) => {
						if !self.pending_queries.contains_key(&query_id) {
							log::error!(
								target: LOG_TARGET,
								"Missing/invalid pending query for `GET_VALUE` partial result: {query_id:?}"
							);

							continue
						}

						let peer_id: sc_network_types::PeerId = record.peer.into();
						let record = PeerRecord {
							record: P2PRecord {
								key: record.record.key.to_vec().into(),
								value: record.record.value,
								publisher: record.record.publisher.map(|peer_id| {
									let peer_id: sc_network_types::PeerId = peer_id.into();
									peer_id.into()
								}),
								expires: record.record.expires,
							},
							peer: Some(peer_id.into()),
						};

						self.event_streams.send(
							Event::Dht(
								DhtEvent::ValueFound(
									record.into()
								)
							)
						);
					}
					Some(DiscoveryEvent::GetRecordSuccess { query_id }) => {
						match self.pending_queries.remove(&query_id) {
							Some(KadQuery::GetValue(key, started)) => {
								log::trace!(
									target: LOG_TARGET,
									"`GET_VALUE` for {key:?} ({query_id:?}) succeeded",
								);

								if let Some(ref metrics) = self.metrics {
									metrics
										.kademlia_query_duration
										.with_label_values(&["value-get"])
										.observe(started.elapsed().as_secs_f64());
								}
							},
							query => {
								log::error!(
									target: LOG_TARGET,
									"Missing/invalid pending query for `GET_VALUE`: {query:?}"
								);
								debug_assert!(false);
							},
						}
					}
					Some(DiscoveryEvent::PutRecordSuccess { query_id }) => {
						match self.pending_queries.remove(&query_id) {
							Some(KadQuery::PutValue(key, started)) => {
								log::trace!(
									target: LOG_TARGET,
									"`PUT_VALUE` for {key:?} ({query_id:?}) succeeded",
								);

								self.event_streams.send(Event::Dht(
									DhtEvent::ValuePut(key)
								));

								if let Some(ref metrics) = self.metrics {
									metrics
										.kademlia_query_duration
										.with_label_values(&["value-put"])
										.observe(started.elapsed().as_secs_f64());
								}
							},
							query => {
								log::error!(
									target: LOG_TARGET,
									"Missing/invalid pending query for `PUT_VALUE`: {query:?}"
								);
								debug_assert!(false);
							}
						}
					}
					Some(DiscoveryEvent::GetProvidersSuccess { query_id, providers }) => {
						match self.pending_queries.remove(&query_id) {
							Some(KadQuery::GetProviders(key, started)) => {
								log::trace!(
									target: LOG_TARGET,
									"`GET_PROVIDERS` for {key:?} ({query_id:?}) succeeded",
								);

								// We likely requested providers to connect to them,
								// so let's add their addresses to litep2p's transport manager.
								// Consider also looking the addresses of providers up with `FIND_NODE`
								// query, as it can yield more up to date addresses.
								providers.iter().for_each(|p| {
									self.litep2p.add_known_address(p.peer, p.addresses.clone().into_iter());
								});

								self.event_streams.send(Event::Dht(
									DhtEvent::ProvidersFound(
										key.clone().into(),
										providers.into_iter().map(|p| p.peer.into()).collect()
									)
								));

								// litep2p returns all providers in a single event, so we let
								// subscribers know no more providers will be yielded.
								self.event_streams.send(Event::Dht(
									DhtEvent::NoMoreProviders(key.into())
								));

								if let Some(ref metrics) = self.metrics {
									metrics
										.kademlia_query_duration
										.with_label_values(&["providers-get"])
										.observe(started.elapsed().as_secs_f64());
								}
							},
							query => {
								log::error!(
									target: LOG_TARGET,
									"Missing/invalid pending query for `GET_PROVIDERS`: {query:?}"
								);
								debug_assert!(false);
							}
						}
					}
					Some(DiscoveryEvent::AddProviderSuccess { query_id, provided_key }) => {
						match self.pending_queries.remove(&query_id) {
							Some(KadQuery::AddProvider(key, started)) => {
								debug_assert_eq!(key, provided_key.into());

								log::trace!(
									target: LOG_TARGET,
									"`ADD_PROVIDER` for {key:?} ({query_id:?}) succeeded",
								);

								self.event_streams.send(Event::Dht(
									DhtEvent::StartedProviding(key.into())
								));

								if let Some(ref metrics) = self.metrics {
									metrics
										.kademlia_query_duration
										.with_label_values(&["provider-add"])
										.observe(started.elapsed().as_secs_f64());
								}
							}
							Some(_) => {
								log::error!(
									target: LOG_TARGET,
									"Invalid pending query for `ADD_PROVIDER`: {query_id:?}"
								);
								debug_assert!(false);
							}
							None => {
								log::trace!(
									target: LOG_TARGET,
									"`ADD_PROVIDER` for key {provided_key:?} ({query_id:?}) succeeded (republishing)",
								);
							}
						}
					}
					Some(DiscoveryEvent::QueryFailed { query_id }) => {
						match self.pending_queries.remove(&query_id) {
							Some(KadQuery::FindNode(peer_id, started)) => {
								log::debug!(
									target: LOG_TARGET,
									"`FIND_NODE` ({query_id:?}) failed for target {peer_id:?}",
								);

								self.event_streams.send(Event::Dht(
									DhtEvent::ClosestPeersNotFound(peer_id.into())
								));

								if let Some(ref metrics) = self.metrics {
									metrics
										.kademlia_query_duration
										.with_label_values(&["node-find-failed"])
										.observe(started.elapsed().as_secs_f64());
								}
							},
							Some(KadQuery::GetValue(key, started)) => {
								log::debug!(
									target: LOG_TARGET,
									"`GET_VALUE` ({query_id:?}) failed for key {key:?}",
								);

								self.event_streams.send(Event::Dht(
									DhtEvent::ValueNotFound(key)
								));

								if let Some(ref metrics) = self.metrics {
									metrics
										.kademlia_query_duration
										.with_label_values(&["value-get-failed"])
										.observe(started.elapsed().as_secs_f64());
								}
							},
							Some(KadQuery::PutValue(key, started)) => {
								log::debug!(
									target: LOG_TARGET,
									"`PUT_VALUE` ({query_id:?}) failed for key {key:?}",
								);

								self.event_streams.send(Event::Dht(
									DhtEvent::ValuePutFailed(key)
								));

								if let Some(ref metrics) = self.metrics {
									metrics
										.kademlia_query_duration
										.with_label_values(&["value-put-failed"])
										.observe(started.elapsed().as_secs_f64());
								}
							},
							Some(KadQuery::GetProviders(key, started)) => {
								log::debug!(
									target: LOG_TARGET,
									"`GET_PROVIDERS` ({query_id:?}) failed for key {key:?}"
								);

								self.event_streams.send(Event::Dht(
									DhtEvent::ProvidersNotFound(key)
								));

								if let Some(ref metrics) = self.metrics {
									metrics
										.kademlia_query_duration
										.with_label_values(&["providers-get-failed"])
										.observe(started.elapsed().as_secs_f64());
								}
							},
							Some(KadQuery::AddProvider(key, started)) => {
								log::debug!(
									target: LOG_TARGET,
									"`ADD_PROVIDER` ({query_id:?}) failed with key {key:?}",
								);

								self.event_streams.send(Event::Dht(
									DhtEvent::StartProvidingFailed(key)
								));

								if let Some(ref metrics) = self.metrics {
									metrics
										.kademlia_query_duration
										.with_label_values(&["provider-add-failed"])
										.observe(started.elapsed().as_secs_f64());
								}
							},
							None => {
								log::debug!(
									target: LOG_TARGET,
									"non-existent query (likely republishing a provider) failed ({query_id:?})",
								);
							}
						}
					}
					Some(DiscoveryEvent::Identified { peer, listen_addresses, supported_protocols, .. }) => {
						self.discovery.add_self_reported_address(peer, supported_protocols, listen_addresses).await;
					}
					Some(DiscoveryEvent::ExternalAddressDiscovered { address }) => {
						match self.litep2p.public_addresses().add_address(address.clone().into()) {
							Ok(inserted) => if inserted {
								log::info!(target: LOG_TARGET, "🔍 Discovered new external address for our node: {address}");
							},
							Err(err) => {
								log::warn!(
									target: LOG_TARGET,
									"🔍 Failed to add discovered external address {address:?}: {err:?}",
								);
							},
						}
					}
					Some(DiscoveryEvent::ExternalAddressExpired{ address }) => {
						let local_peer_id = self.litep2p.local_peer_id();

						// Litep2p requires the peer ID to be present in the address.
						let address = if !std::matches!(address.iter().last(), Some(Protocol::P2p(_))) {
							address.with(Protocol::P2p((*local_peer_id).into()))
						} else {
							address
						};

						if self.litep2p.public_addresses().remove_address(&address) {
							log::info!(target: LOG_TARGET, "🔍 Expired external address for our node: {address}");
						} else {
							log::warn!(
								target: LOG_TARGET,
								"🔍 Failed to remove expired external address {address:?}"
							);
						}
					}
					Some(DiscoveryEvent::Ping { peer, rtt }) => {
						log::trace!(
							target: LOG_TARGET,
							"ping time with {peer:?}: {rtt:?}",
						);
					}
					Some(DiscoveryEvent::IncomingRecord { record: Record { key, value, publisher, expires }} ) => {
						self.event_streams.send(Event::Dht(
							DhtEvent::PutRecordRequest(
								key.into(),
								value,
								publisher.map(Into::into),
								expires,
							)
						));
					},

					Some(DiscoveryEvent::RandomKademliaStarted) => {
						if let Some(metrics) = self.metrics.as_ref() {
							metrics.kademlia_random_queries_total.inc();
						}
					}
				},
				event = self.litep2p.next_event() => match event {
					Some(Litep2pEvent::ConnectionEstablished { peer, endpoint }) => {
						let Some(metrics) = &self.metrics else {
							continue;
						};

						let direction = match endpoint {
							Endpoint::Dialer { .. } => "out",
							Endpoint::Listener { .. } => {
								// Increment incoming connections counter.
								//
								// Note: For litep2p these are represented by established negotiated connections,
								// while for libp2p (legacy) these represent not-yet-negotiated connections.
								metrics.incoming_connections_total.inc();

								"in"
							},
						};
						metrics.connections_opened_total.with_label_values(&[direction]).inc();

						match self.peers.entry(peer) {
							Entry::Vacant(entry) => {
								entry.insert(ConnectionContext {
									endpoints: HashMap::from_iter([(endpoint.connection_id(), endpoint)]),
									num_connections: 1usize,
								});
								metrics.distinct_peers_connections_opened_total.inc();
							}
							Entry::Occupied(entry) => {
								let entry = entry.into_mut();
								entry.num_connections += 1;
								entry.endpoints.insert(endpoint.connection_id(), endpoint);
							}
						}
					}
					Some(Litep2pEvent::ConnectionClosed { peer, connection_id }) => {
						let Some(metrics) = &self.metrics else {
							continue;
						};

						let Some(context) = self.peers.get_mut(&peer) else {
							log::debug!(target: LOG_TARGET, "unknown peer disconnected: {peer:?} ({connection_id:?})");
							continue
						};

						let direction = match context.endpoints.remove(&connection_id) {
							None => {
								log::debug!(target: LOG_TARGET, "connection {connection_id:?} doesn't exist for {peer:?} ");
								continue
							}
							Some(endpoint) => {
								context.num_connections -= 1;

								match endpoint {
									Endpoint::Dialer { .. } => "out",
									Endpoint::Listener { .. } => "in",
								}
							}
						};

						metrics.connections_closed_total.with_label_values(&[direction, "actively-closed"]).inc();

						if context.num_connections == 0 {
							self.peers.remove(&peer);
							metrics.distinct_peers_connections_closed_total.inc();
						}
					}
					Some(Litep2pEvent::DialFailure { address, error }) => {
						log::debug!(
							target: LOG_TARGET,
							"failed to dial peer at {address:?}: {error:?}",
						);

						if let Some(metrics) = &self.metrics {
							let reason = match error {
								DialError::Timeout => "timeout",
								DialError::AddressError(_) => "invalid-address",
								DialError::DnsError(_) => "cannot-resolve-dns",
								DialError::NegotiationError(error) => match error {
									NegotiationError::Timeout => "timeout",
									NegotiationError::PeerIdMissing => "missing-peer-id",
									NegotiationError::StateMismatch => "state-mismatch",
									NegotiationError::PeerIdMismatch(_,_) => "peer-id-missmatch",
									NegotiationError::MultistreamSelectError(_) => "multistream-select-error",
									NegotiationError::SnowError(_) => "noise-error",
									NegotiationError::ParseError(_) => "parse-error",
									NegotiationError::IoError(_) => "io-error",
									NegotiationError::WebSocket(_) => "webscoket-error",
									NegotiationError::BadSignature => "bad-signature",
								}
							};

							metrics.pending_connections_errors_total.with_label_values(&[&reason]).inc();
						}
					}
					Some(Litep2pEvent::ListDialFailures { errors }) => {
						log::debug!(
							target: LOG_TARGET,
							"failed to dial peer on multiple addresses {errors:?}",
						);

						if let Some(metrics) = &self.metrics {
							metrics.pending_connections_errors_total.with_label_values(&["transport-errors"]).inc();
						}
					}
					None => {
						log::error!(
								target: LOG_TARGET,
								"Litep2p backend terminated"
						);
						return
					}
				},
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		config::{ed25519, NetworkConfiguration, ProtocolId, Role, Secret},
		service::traits::NetworkStateInfo,
	};
	use sc_network_types::{
		multiaddr::Multiaddr as NetworkMultiaddr, multihash::Multihash as NetworkMultihash,
	};
	use sp_core::H256;
	use substrate_test_runtime_client::runtime::Block;

	/// `--listen-addr` of a node behind a NAT. Port `0` so concurrent tests don't collide.
	const WEBRTC_LISTEN_ADDRESS: &str = "/ip4/127.0.0.1/udp/0/webrtc-direct";

	/// `--public-addr` of that node: the routable coordinate peers are told to dial.
	/// This is the shape an operator supplies when the node sits behind a proxy.
	const WEBRTC_PUBLIC_ADDRESS: &str = "/ip4/203.0.113.9/udp/31234/webrtc-direct";

	/// The `/certhash` component of `address`.
	fn certhash(address: &NetworkMultiaddr) -> Option<NetworkMultihash> {
		address.iter().find_map(|protocol| match protocol {
			NetworkProtocol::Certhash(hash) => Some(hash),
			_ => None,
		})
	}

	/// Bring up the backend from `network_config` through its real entry point.
	fn start_backend(
		network_config: &NetworkConfiguration,
	) -> Result<Litep2pNetworkBackend, Error> {
		let config = FullNetworkConfiguration::<Block, H256, Litep2pNetworkBackend>::new(
			network_config,
			None,
		);

		let (block_announce_config, _notification_service) =
			<Litep2pNetworkBackend as NetworkBackend<Block, H256>>::notification_config(
				"/block-announces/1".into(),
				vec![],
				1024,
				None,
				SetConfig::default(),
				NotificationMetrics::new(None),
				config.peer_store_handle(),
			);

		<Litep2pNetworkBackend as NetworkBackend<Block, H256>>::new(Params {
			role: Role::Full,
			executor: Box::new(|future| {
				tokio::spawn(future);
			}),
			network_config: config,
			protocol_id: ProtocolId::from("test"),
			genesis_hash: H256::zero(),
			fork_id: None,
			metrics_registry: None,
			block_announce_config,
			ipfs_config: None,
			notification_metrics: NotificationMetrics::new(None),
		})
	}

	/// Both the address the node binds and the one it tells peers to dial must carry the node's
	/// `/certhash`: a `webrtc-direct` dialer has no other way to verify the DTLS handshake.
	#[tokio::test]
	async fn webrtc_addresses_advertised_with_certhash() {
		let mut network_config = NetworkConfiguration::new_local();
		network_config.listen_addresses = vec![WEBRTC_LISTEN_ADDRESS.parse().unwrap()];
		network_config.public_addresses = vec![WEBRTC_PUBLIC_ADDRESS.parse().unwrap()];
		// Fixed, so the certificate the node will present can be derived here as well.
		network_config.node_key = NodeKeyConfig::Ed25519(Secret::Input(
			ed25519::SecretKey::try_from_bytes([7u8; 32]).unwrap(),
		));
		network_config.validate_and_complete_webrtc_addresses().unwrap();

		let (keypair, _peer_id) =
			Litep2pNetworkBackend::get_keypair(&network_config.node_key).unwrap();
		let node_certhash: NetworkMultihash =
			webrtc::derive_certificate(keypair.secret()).unwrap().certhash().into();

		// Held for the duration of the test: dropping it closes the node's sockets.
		let backend = start_backend(&network_config).unwrap();
		let network_service =
			<Litep2pNetworkBackend as NetworkBackend<Block, H256>>::network_service(&backend);

		let advertised = network_service.listen_addresses();
		assert_eq!(advertised.len(), 1);
		assert_eq!(certhash(&advertised[0]), Some(node_certhash));

		let advertised = network_service.external_addresses();
		assert_eq!(advertised.len(), 1);
		assert_eq!(certhash(&advertised[0]), Some(node_certhash));
	}

	/// The default node key (`Secret::New`) generates a fresh key per `into_keypair()` call:
	/// completing the addresses must pin the resolved key, or the backend would serve a
	/// certificate matching neither the advertised `/certhash` nor each other's.
	#[tokio::test]
	async fn webrtc_certhash_consistent_with_default_node_key() {
		let mut network_config = NetworkConfiguration::new_local();
		network_config.listen_addresses = vec![WEBRTC_LISTEN_ADDRESS.parse().unwrap()];
		network_config.public_addresses = vec![WEBRTC_PUBLIC_ADDRESS.parse().unwrap()];
		network_config.validate_and_complete_webrtc_addresses().unwrap();

		// Completing the addresses pinned the key, so this is the key the backend serves.
		let (keypair, _peer_id) =
			Litep2pNetworkBackend::get_keypair(&network_config.node_key).unwrap();
		let node_certhash: NetworkMultihash =
			webrtc::derive_certificate(keypair.secret()).unwrap().certhash().into();

		// Held for the duration of the test: dropping it closes the node's sockets.
		let backend = start_backend(&network_config).unwrap();
		let network_service =
			<Litep2pNetworkBackend as NetworkBackend<Block, H256>>::network_service(&backend);

		let advertised = network_service.listen_addresses();
		assert_eq!(advertised.len(), 1);
		assert_eq!(certhash(&advertised[0]), Some(node_certhash));

		let advertised = network_service.external_addresses();
		assert_eq!(advertised.len(), 1);
		assert_eq!(certhash(&advertised[0]), Some(node_certhash));
	}

	#[tokio::test]
	async fn webrtc_public_address_completed_at_config_creation_accepted() {
		let mut network_config = NetworkConfiguration::new_local();
		network_config.listen_addresses = vec![WEBRTC_LISTEN_ADDRESS.parse().unwrap()];
		network_config.public_addresses = vec![WEBRTC_PUBLIC_ADDRESS.parse().unwrap()];
		network_config.node_key = NodeKeyConfig::Ed25519(Secret::Input(
			ed25519::SecretKey::try_from_bytes([7u8; 32]).unwrap(),
		));
		network_config.validate_and_complete_webrtc_addresses().unwrap();

		// Held for the duration of the test: dropping it closes the node's sockets.
		let backend = start_backend(&network_config).unwrap();
		let network_service =
			<Litep2pNetworkBackend as NetworkBackend<Block, H256>>::network_service(&backend);
		let local_peer_id = network_service.local_peer_id();

		assert_eq!(
			network_service.external_addresses(),
			vec![network_config.public_addresses[0]
				.clone()
				.with(NetworkProtocol::P2p(local_peer_id.into()))],
		);
	}

	#[tokio::test]
	async fn webrtc_listen_address_with_certhash_refuses_to_start() {
		let listen_address: NetworkMultiaddr = WEBRTC_LISTEN_ADDRESS.parse().unwrap();

		let mut network_config = NetworkConfiguration::new_local();
		network_config.listen_addresses =
			vec![listen_address.with(NetworkProtocol::Certhash(a_certhash()))];

		assert!(matches!(start_backend(&network_config), Err(Error::InvalidWebRtcAddress { .. }),));
	}

	#[tokio::test]
	async fn non_webrtc_public_address_untouched() {
		let public_address: NetworkMultiaddr = "/ip4/203.0.113.9/tcp/31234".parse().unwrap();

		let mut network_config = NetworkConfiguration::new_local();
		network_config.listen_addresses = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
		network_config.public_addresses = vec![public_address.clone()];

		// Held for the duration of the test: dropping it closes the node's sockets.
		let backend = start_backend(&network_config).unwrap();
		let network_service =
			<Litep2pNetworkBackend as NetworkBackend<Block, H256>>::network_service(&backend);
		let local_peer_id = network_service.local_peer_id();

		// litep2p appends the local `/p2p` to every public address; nothing else is added.
		assert_eq!(
			network_service.external_addresses(),
			vec![public_address.with(NetworkProtocol::P2p(local_peer_id.into()))],
		);
	}

	/// A certhash standing in for the node's own.
	fn a_certhash() -> NetworkMultihash {
		sc_network_types::multihash::Code::Sha2_256.digest(b"certificate")
	}
}
