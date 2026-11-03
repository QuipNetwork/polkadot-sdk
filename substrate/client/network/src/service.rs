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

//! Main entry point of the sc-network crate.
//!
//! There are two main structs in this module: [`NetworkWorker`] and [`NetworkService`].
//! The [`NetworkWorker`] *is* the network. Network is driven by [`NetworkWorker::run`] future that
//! terminates only when all instances of the control handles [`NetworkService`] were dropped.
//! The [`NetworkService`] is merely a shared version of the [`NetworkWorker`]. You can obtain an
//! `Arc<NetworkService>` by calling [`NetworkWorker::service`].
//!
//! The methods of the [`NetworkService`] are implemented by sending a message over a channel,
//! which is then processed by [`NetworkWorker::next_action`].

use crate::{
	behaviour::{self, Behaviour, BehaviourOut},
	config::{
		parse_addr, FullNetworkConfiguration, IncomingRequest, MultiaddrWithPeerId,
		NonDefaultSetConfig, NotificationHandshake, Params, SetConfig, TransportConfig,
	},
	discovery::DiscoveryConfig,
	error::Error,
	event::{DhtEvent, Event},
	network_state::{
		NetworkState, NotConnectedPeer as NetworkStateNotConnectedPeer, Peer as NetworkStatePeer,
	},
	peer_store::{PeerStore, PeerStoreProvider},
	protocol::{self, Protocol, Ready},
	protocol_controller::{self, ProtoSetConfig, ProtocolController, SetId},
	request_responses::{IfDisconnected, ProtocolConfig as RequestResponseConfig, RequestFailure},
	service::{
		signature::{Signature, SigningError},
		traits::{
			BandwidthSink, NetworkBackend, NetworkDHTProvider, NetworkEventStream, NetworkPeers,
			NetworkRequest, NetworkService as NetworkServiceT, NetworkSigner, NetworkStateInfo,
			NetworkStatus, NetworkStatusProvider, NotificationSender as NotificationSenderT,
			NotificationSenderError, NotificationSenderReady as NotificationSenderReadyT,
		},
	},
	transport,
	types::ProtocolName,
	NotificationService, ReputationChange,
};

use codec::DecodeAll;
use futures::{channel::oneshot, prelude::*};
use libp2p::{
	connection_limits::{ConnectionLimits, Exceeded},
	core::{upgrade, ConnectedPoint, Endpoint},
	identify::Info as IdentifyInfo,
	identity::ed25519,
	multiaddr::{self, Multiaddr},
	swarm::{
		Config as SwarmConfig, ConnectionError, ConnectionId, DialError, Executor, ListenError,
		NetworkBehaviour, Swarm, SwarmEvent,
	},
	PeerId,
};
use log::{debug, error, info, trace, warn};
use metrics::{Histogram, MetricSources, Metrics};
use parking_lot::Mutex;
use prometheus_endpoint::Registry;
use sc_network_types::kad::{Key as KademliaKey, Record};

use sc_network_common::{
	role::{ObservedRole, Roles},
	ExHashT,
};
use sc_utils::mpsc::{tracing_unbounded, TracingUnboundedReceiver, TracingUnboundedSender};
use sp_runtime::traits::Block as BlockT;

pub use behaviour::{InboundFailure, OutboundFailure, ResponseFailure};
pub use libp2p::identity::{DecodingError, Keypair, PublicKey};
pub use metrics::NotificationMetrics;
pub use protocol::NotificationsSink;
use std::{
	collections::{HashMap, HashSet},
	fs, iter,
	marker::PhantomData,
	num::NonZeroUsize,
	pin::Pin,
	str,
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	},
	time::{Duration, Instant},
};

pub(crate) mod metrics;
pub(crate) mod out_events;

pub mod signature;
pub mod traits;

/// Logging target for the file.
const LOG_TARGET: &str = "sub-libp2p";

struct Libp2pBandwidthSink {
	#[allow(deprecated)]
	sink: Arc<transport::BandwidthSinks>,
}

impl BandwidthSink for Libp2pBandwidthSink {
	fn total_inbound(&self) -> u64 {
		self.sink.total_inbound()
	}

	fn total_outbound(&self) -> u64 {
		self.sink.total_outbound()
	}
}

/// Substrate network service. Handles network IO and manages connectivity.
pub struct NetworkService<B: BlockT + 'static, H: ExHashT> {
	/// Number of peers we're connected to.
	num_connected: Arc<AtomicUsize>,
	/// The local external addresses.
	external_addresses: Arc<Mutex<HashSet<Multiaddr>>>,
	/// Listen addresses. Do **NOT** include a trailing `/p2p/` with our `PeerId`.
	listen_addresses: Arc<Mutex<HashSet<Multiaddr>>>,
	/// Local copy of the `PeerId` of the local node.
	local_peer_id: PeerId,
	/// The `KeyPair` that defines the `PeerId` of the local node.
	local_identity: Keypair,
	/// Bandwidth logging system. Can be queried to know the average bandwidth consumed.
	bandwidth: Arc<dyn BandwidthSink>,
	/// Channel that sends messages to the actual worker.
	to_worker: TracingUnboundedSender<ServiceToWorkerMsg>,
	/// Protocol name -> `SetId` mapping for notification protocols. The map never changes after
	/// initialization.
	notification_protocol_ids: HashMap<ProtocolName, SetId>,
	/// Handles to manage peer connections on notification protocols. The vector never changes
	/// after initialization.
	protocol_handles: Vec<protocol_controller::ProtocolHandle>,
	/// Shortcut to sync protocol handle (`protocol_handles[0]`).
	sync_protocol_handle: protocol_controller::ProtocolHandle,
	/// Handle to `PeerStore`.
	peer_store_handle: Arc<dyn PeerStoreProvider>,
	/// Marker to pin the `H` generic. Serves no purpose except to not break backwards
	/// compatibility.
	_marker: PhantomData<H>,
	/// Marker for block type
	_block: PhantomData<B>,
}

#[async_trait::async_trait]
impl<B, H> NetworkBackend<B, H> for NetworkWorker<B, H>
where
	B: BlockT + 'static,
	H: ExHashT,
{
	type NotificationProtocolConfig = NonDefaultSetConfig;
	type RequestResponseProtocolConfig = RequestResponseConfig;
	type NetworkService<Block, Hash> = Arc<NetworkService<B, H>>;
	type PeerStore = PeerStore;

	fn new(params: Params<B, H, Self>) -> Result<Self, Error>
	where
		Self: Sized,
	{
		NetworkWorker::new(params)
	}

	/// Get handle to `NetworkService` of the `NetworkBackend`.
	fn network_service(&self) -> Arc<dyn NetworkServiceT> {
		self.service.clone()
	}

	/// Create `PeerStore`.
	fn peer_store(
		bootnodes: Vec<sc_network_types::PeerId>,
		metrics_registry: Option<Registry>,
	) -> Self::PeerStore {
		PeerStore::new(bootnodes.into_iter().map(From::from).collect(), metrics_registry)
	}

	fn register_notification_metrics(registry: Option<&Registry>) -> NotificationMetrics {
		NotificationMetrics::new(registry)
	}

	/// Create notification protocol configuration.
	fn notification_config(
		protocol_name: ProtocolName,
		fallback_names: Vec<ProtocolName>,
		max_notification_size: u64,
		handshake: Option<NotificationHandshake>,
		set_config: SetConfig,
		_metrics: NotificationMetrics,
		_peerstore_handle: Arc<dyn PeerStoreProvider>,
	) -> (Self::NotificationProtocolConfig, Box<dyn NotificationService>) {
		NonDefaultSetConfig::new(
			protocol_name,
			fallback_names,
			max_notification_size,
			handshake,
			set_config,
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
		Self::RequestResponseProtocolConfig {
			name: protocol_name,
			fallback_names,
			max_request_size,
			max_response_size,
			request_timeout,
			inbound_queue,
		}
	}

	/// Start [`NetworkBackend`] event loop.
	async fn run(mut self) {
		self.run().await
	}
}

impl<B, H> NetworkWorker<B, H>
where
	B: BlockT + 'static,
	H: ExHashT,
{
	/// Creates the network service.
	///
	/// Returns a `NetworkWorker` that implements `Future` and must be regularly polled in order
	/// for the network processing to advance. From it, you can extract a `NetworkService` using
	/// `worker.service()`. The `NetworkService` can be shared through the codebase.
	pub fn new(params: Params<B, H, Self>) -> Result<Self, Error> {
		let peer_store_handle = params.network_config.peer_store_handle();
		let FullNetworkConfiguration {
			notification_protocols,
			request_response_protocols,
			mut network_config,
			..
		} = params.network_config;

		// Private and public keys configuration.
		let local_identity = network_config.node_key.clone().into_keypair()?;
		let local_public = local_identity.public();
		let local_peer_id = local_public.to_peer_id();

		// Convert to libp2p types.
		let local_identity: ed25519::Keypair = local_identity.into();
		let local_public: ed25519::PublicKey = local_public.into();
		let local_peer_id: PeerId = local_peer_id.into();

		network_config.boot_nodes = network_config
			.boot_nodes
			.into_iter()
			.filter(|boot_node| boot_node.peer_id != local_peer_id.into())
			.collect();
		network_config.default_peers_set.reserved_nodes = network_config
			.default_peers_set
			.reserved_nodes
			.into_iter()
			.filter(|reserved_node| {
				if reserved_node.peer_id == local_peer_id.into() {
					warn!(
						target: LOG_TARGET,
						"Local peer ID used in reserved node, ignoring: {}",
						reserved_node,
					);
					false
				} else {
					true
				}
			})
			.collect();

		// Ensure the listen addresses are consistent with the transport.
		ensure_addresses_consistent_with_transport(
			network_config.listen_addresses.iter(),
			&network_config.transport,
		)?;
		ensure_addresses_consistent_with_transport(
			network_config.boot_nodes.iter().map(|x| &x.multiaddr),
			&network_config.transport,
		)?;
		ensure_addresses_consistent_with_transport(
			network_config.default_peers_set.reserved_nodes.iter().map(|x| &x.multiaddr),
			&network_config.transport,
		)?;
		for notification_protocol in &notification_protocols {
			ensure_addresses_consistent_with_transport(
				notification_protocol.set_config().reserved_nodes.iter().map(|x| &x.multiaddr),
				&network_config.transport,
			)?;
		}
		ensure_addresses_consistent_with_transport(
			network_config.public_addresses.iter(),
			&network_config.transport,
		)?;

		let (to_worker, from_service) = tracing_unbounded("mpsc_network_worker", 100_000);

		if let Some(path) = &network_config.net_config_path {
			fs::create_dir_all(path)?;
		}

		info!(
			target: LOG_TARGET,
			"🏷  Local node identity is: {}",
			local_peer_id.to_base58(),
		);
		info!(target: LOG_TARGET, "Running libp2p network backend");

		let (transport, bandwidth) = {
			let config_mem = match network_config.transport {
				TransportConfig::MemoryOnly => true,
				TransportConfig::Normal { .. } => false,
			};

			transport::build_transport(local_identity.clone().into(), config_mem)
		};

		let (to_notifications, from_protocol_controllers) =
			tracing_unbounded("mpsc_protocol_controllers_to_notifications", 10_000);

		// We must prepend a hardcoded default peer set to notification protocols.
		let all_peer_sets_iter = iter::once(&network_config.default_peers_set)
			.chain(notification_protocols.iter().map(|protocol| protocol.set_config()));

		let (protocol_handles, protocol_controllers): (Vec<_>, Vec<_>) = all_peer_sets_iter
			.enumerate()
			.map(|(set_id, set_config)| {
				let proto_set_config = ProtoSetConfig {
					in_peers: set_config.in_peers,
					out_peers: set_config.out_peers,
					reserved_nodes: set_config
						.reserved_nodes
						.iter()
						.map(|node| node.peer_id.into())
						.collect(),
					reserved_only: set_config.non_reserved_mode.is_reserved_only(),
				};

				ProtocolController::new(
					SetId::from(set_id),
					proto_set_config,
					to_notifications.clone(),
					Arc::clone(&peer_store_handle),
				)
			})
			.unzip();

		// Shortcut to default (sync) peer set protocol handle.
		let sync_protocol_handle = protocol_handles[0].clone();

		// Spawn `ProtocolController` runners.
		protocol_controllers
			.into_iter()
			.for_each(|controller| (params.executor)(controller.run().boxed()));

		// Protocol name to protocol id mapping. The first protocol is always block announce (sync)
		// protocol, aka default (hardcoded) peer set.
		let notification_protocol_ids: HashMap<ProtocolName, SetId> =
			iter::once(&params.block_announce_config)
				.chain(notification_protocols.iter())
				.enumerate()
				.map(|(index, protocol)| (protocol.protocol_name().clone(), SetId::from(index)))
				.collect();

		let known_addresses = {
			// Collect all reserved nodes and bootnodes addresses.
			let mut addresses: Vec<_> = network_config
				.default_peers_set
				.reserved_nodes
				.iter()
				.map(|reserved| (reserved.peer_id, reserved.multiaddr.clone()))
				.chain(notification_protocols.iter().flat_map(|protocol| {
					protocol
						.set_config()
						.reserved_nodes
						.iter()
						.map(|reserved| (reserved.peer_id, reserved.multiaddr.clone()))
				}))
				.chain(
					network_config
						.boot_nodes
						.iter()
						.map(|bootnode| (bootnode.peer_id, bootnode.multiaddr.clone())),
				)
				.collect();

			// Remove possible duplicates.
			addresses.sort();
			addresses.dedup();

			addresses
		};

		// Check for duplicate bootnodes.
		network_config.boot_nodes.iter().try_for_each(|bootnode| {
			if let Some(other) = network_config
				.boot_nodes
				.iter()
				.filter(|o| o.multiaddr == bootnode.multiaddr)
				.find(|o| o.peer_id != bootnode.peer_id)
			{
				Err(Error::DuplicateBootnode {
					address: bootnode.multiaddr.clone().into(),
					first_id: bootnode.peer_id.into(),
					second_id: other.peer_id.into(),
				})
			} else {
				Ok(())
			}
		})?;

		// List of bootnode multiaddresses.
		let mut boot_node_ids = HashMap::<PeerId, Vec<Multiaddr>>::new();

		for bootnode in network_config.boot_nodes.iter() {
			boot_node_ids
				.entry(bootnode.peer_id.into())
				.or_default()
				.push(bootnode.multiaddr.clone().into());
		}

		let boot_node_ids = Arc::new(boot_node_ids);

		let num_connected = Arc::new(AtomicUsize::new(0));
		let external_addresses = Arc::new(Mutex::new(HashSet::new()));

		let (protocol, notif_protocol_handles) = Protocol::new(
			From::from(&params.role),
			params.notification_metrics,
			notification_protocols,
			params.block_announce_config,
			Arc::clone(&peer_store_handle),
			protocol_handles.clone(),
			from_protocol_controllers,
		)?;

		// Build the swarm.
		let (mut swarm, bandwidth): (Swarm<Behaviour<B>>, _) = {
			let user_agent =
				format!("{} ({})", network_config.client_version, network_config.node_name);

			let discovery_config = {
				let mut config = DiscoveryConfig::new(local_peer_id);
				config.with_permanent_addresses(
					known_addresses
						.iter()
						.map(|(peer, address)| (peer.into(), address.clone().into()))
						.collect::<Vec<_>>(),
				);
				config.discovery_limit(u64::from(network_config.default_peers_set.out_peers) + 15);
				config.with_kademlia(
					params.genesis_hash,
					params.fork_id.as_deref(),
					&params.protocol_id,
				);
				config.with_dht_random_walk(network_config.enable_dht_random_walk);
				config.allow_non_globals_in_dht(network_config.allow_non_globals_in_dht);
				config.use_kademlia_disjoint_query_paths(
					network_config.kademlia_disjoint_query_paths,
				);
				config.with_kademlia_replication_factor(network_config.kademlia_replication_factor);

				match network_config.transport {
					TransportConfig::MemoryOnly => {
						config.with_mdns(false);
						config.allow_private_ip(false);
					},
					TransportConfig::Normal {
						enable_mdns,
						allow_private_ip: allow_private_ipv4,
						..
					} => {
						config.with_mdns(enable_mdns);
						config.allow_private_ip(allow_private_ipv4);
					},
				}

				config
			};

			let behaviour = {
				let result = Behaviour::new(
					protocol,
					user_agent,
					local_public.into(),
					discovery_config,
					request_response_protocols,
					Arc::clone(&peer_store_handle),
					external_addresses.clone(),
					network_config.public_addresses.iter().cloned().map(Into::into).collect(),
					ConnectionLimits::default()
						.with_max_established_per_peer(Some(crate::MAX_CONNECTIONS_PER_PEER as u32))
						.with_max_established_incoming(Some(
							crate::MAX_CONNECTIONS_ESTABLISHED_INCOMING,
						)),
				);

				match result {
					Ok(b) => b,
					Err(crate::request_responses::RegisterError::DuplicateProtocol(proto)) => {
						return Err(Error::DuplicateRequestResponseProtocol { protocol: proto })
					},
				}
			};

			let swarm = {
				struct SpawnImpl<F>(F);
				impl<F: Fn(Pin<Box<dyn Future<Output = ()> + Send>>)> Executor for SpawnImpl<F> {
					fn exec(&self, f: Pin<Box<dyn Future<Output = ()> + Send>>) {
						(self.0)(f)
					}
				}

				let config = SwarmConfig::with_executor(SpawnImpl(params.executor))
					.with_substream_upgrade_protocol_override(upgrade::Version::V1)
					.with_notify_handler_buffer_size(NonZeroUsize::new(32).expect("32 != 0; qed"))
					// NOTE: 24 is somewhat arbitrary and should be tuned in the future if
					// necessary. See <https://github.com/paritytech/substrate/pull/6080>
					.with_per_connection_event_buffer_size(24)
					.with_max_negotiating_inbound_streams(2048)
					.with_idle_connection_timeout(network_config.idle_connection_timeout);

				Swarm::new(transport, behaviour, local_peer_id, config)
			};

			(swarm, Arc::new(Libp2pBandwidthSink { sink: bandwidth }))
		};

		// Initialize the metrics.
		let metrics = match &params.metrics_registry {
			Some(registry) => Some(metrics::register(
				registry,
				MetricSources {
					bandwidth: bandwidth.clone(),
					connected_peers: num_connected.clone(),
				},
			)?),
			None => None,
		};

		// Listen on multiaddresses.
		for addr in &network_config.listen_addresses {
			if let Err(err) = Swarm::<Behaviour<B>>::listen_on(&mut swarm, addr.clone().into()) {
				warn!(target: LOG_TARGET, "Can't listen on {} because: {:?}", addr, err)
			}
		}

		// Add external addresses.
		for addr in &network_config.public_addresses {
			Swarm::<Behaviour<B>>::add_external_address(&mut swarm, addr.clone().into());
		}

		let listen_addresses_set = Arc::new(Mutex::new(HashSet::new()));

		let service = Arc::new(NetworkService {
			bandwidth,
			external_addresses,
			listen_addresses: listen_addresses_set.clone(),
			num_connected: num_connected.clone(),
			local_peer_id,
			local_identity: local_identity.into(),
			to_worker,
			notification_protocol_ids,
			protocol_handles,
			sync_protocol_handle,
			peer_store_handle: Arc::clone(&peer_store_handle),
			_marker: PhantomData,
			_block: Default::default(),
		});

		Ok(NetworkWorker {
			listen_addresses: listen_addresses_set,
			num_connected,
			network_service: swarm,
			service,
			from_service,
			event_streams: out_events::OutChannels::new(params.metrics_registry.as_ref())?,
			metrics,
			boot_node_ids,
			reported_invalid_boot_nodes: Default::default(),
			peer_store_handle: Arc::clone(&peer_store_handle),
			notif_protocol_handles,
			_marker: Default::default(),
			_block: Default::default(),
		})
	}

	/// High-level network status information.
	pub fn status(&self) -> NetworkStatus {
		NetworkStatus {
			num_connected_peers: self.num_connected_peers(),
			total_bytes_inbound: self.total_bytes_inbound(),
			total_bytes_outbound: self.total_bytes_outbound(),
		}
	}

	/// Returns the total number of bytes received so far.
	pub fn total_bytes_inbound(&self) -> u64 {
		self.service.bandwidth.total_inbound()
	}

	/// Returns the total number of bytes sent so far.
	pub fn total_bytes_outbound(&self) -> u64 {
		self.service.bandwidth.total_outbound()
	}

	/// Returns the number of peers we're connected to.
	pub fn num_connected_peers(&self) -> usize {
		self.network_service.behaviour().user_protocol().num_sync_peers()
	}

	/// Adds an address for a node.
	pub fn add_known_address(&mut self, peer_id: PeerId, addr: Multiaddr) {
		self.network_service.behaviour_mut().add_known_address(peer_id, addr);
	}

	/// Return a `NetworkService` that can be shared through the code base and can be used to
	/// manipulate the worker.
	pub fn service(&self) -> &Arc<NetworkService<B, H>> {
		&self.service
	}

	/// Returns the local `PeerId`.
	pub fn local_peer_id(&self) -> &PeerId {
		Swarm::<Behaviour<B>>::local_peer_id(&self.network_service)
	}

	/// Returns the list of addresses we are listening on.
	///
	/// Does **NOT** include a trailing `/p2p/` with our `PeerId`.
	pub fn listen_addresses(&self) -> impl Iterator<Item = &Multiaddr> {
		Swarm::<Behaviour<B>>::listeners(&self.network_service)
	}

	/// Get network state.
	///
	/// **Note**: Use this only for debugging. This API is unstable. There are warnings literally
	/// everywhere about this. Please don't use this function to retrieve actual information.
	pub fn network_state(&mut self) -> NetworkState {
		let swarm = &mut self.network_service;
		let open = swarm.behaviour_mut().user_protocol().open_peers().cloned().collect::<Vec<_>>();
		let connected_peers = {
			let swarm = &mut *swarm;
			open.iter()
				.filter_map(move |peer_id| {
					let known_addresses = if let Ok(addrs) =
						NetworkBehaviour::handle_pending_outbound_connection(
							swarm.behaviour_mut(),
							ConnectionId::new_unchecked(0), // dummy value
							Some(*peer_id),
							&vec![],
							Endpoint::Listener,
						) {
						addrs.into_iter().collect()
					} else {
						error!(target: LOG_TARGET, "Was not able to get known addresses for {:?}", peer_id);
						return None;
					};

					let endpoint = if let Some(e) =
						swarm.behaviour_mut().node(peer_id).and_then(|i| i.endpoint())
					{
						e.clone().into()
					} else {
						error!(target: LOG_TARGET, "Found state inconsistency between custom protocol \
						and debug information about {:?}", peer_id);
						return None;
					};

					Some((
						peer_id.to_base58(),
						NetworkStatePeer {
							endpoint,
							version_string: swarm
								.behaviour_mut()
								.node(peer_id)
								.and_then(|i| i.client_version().map(|s| s.to_owned())),
							latest_ping_time: swarm
								.behaviour_mut()
								.node(peer_id)
								.and_then(|i| i.latest_ping()),
							known_addresses,
						},
					))
				})
				.collect()
		};

		let not_connected_peers = {
			let swarm = &mut *swarm;
			swarm
				.behaviour_mut()
				.known_peers()
				.into_iter()
				.filter(|p| open.iter().all(|n| n != p))
				.map(move |peer_id| {
					let known_addresses = if let Ok(addrs) =
						NetworkBehaviour::handle_pending_outbound_connection(
							swarm.behaviour_mut(),
							ConnectionId::new_unchecked(0), // dummy value
							Some(peer_id),
							&vec![],
							Endpoint::Listener,
						) {
						addrs.into_iter().collect()
					} else {
						error!(target: LOG_TARGET, "Was not able to get known addresses for {:?}", peer_id);
						Default::default()
					};

					(
						peer_id.to_base58(),
						NetworkStateNotConnectedPeer {
							version_string: swarm
								.behaviour_mut()
								.node(&peer_id)
								.and_then(|i| i.client_version().map(|s| s.to_owned())),
							latest_ping_time: swarm
								.behaviour_mut()
								.node(&peer_id)
								.and_then(|i| i.latest_ping()),
							known_addresses,
						},
					)
				})
				.collect()
		};

		let peer_id = Swarm::<Behaviour<B>>::local_peer_id(swarm).to_base58();
		let listened_addresses = swarm.listeners().cloned().collect();
		let external_addresses = swarm.external_addresses().cloned().collect();

		NetworkState {
			peer_id,
			listened_addresses,
			external_addresses,
			connected_peers,
			not_connected_peers,
			// TODO: Check what info we can include here.
			//       Issue reference: https://github.com/paritytech/substrate/issues/14160.
			peerset: serde_json::json!(
				"Unimplemented. See https://github.com/paritytech/substrate/issues/14160."
			),
		}
	}

	/// Removes a `PeerId` from the list of reserved peers.
	pub fn remove_reserved_peer(&self, peer: PeerId) {
		self.service.remove_reserved_peer(peer.into());
	}

	/// Adds a `PeerId` and its `Multiaddr` as reserved.
	pub fn add_reserved_peer(&self, peer: MultiaddrWithPeerId) -> Result<(), String> {
		self.service.add_reserved_peer(peer)
	}
}

impl<B: BlockT + 'static, H: ExHashT> NetworkService<B, H> {
	/// Get network state.
	///
	/// **Note**: Use this only for debugging. This API is unstable. There are warnings literally
	/// everywhere about this. Please don't use this function to retrieve actual information.
	///
	/// Returns an error if the `NetworkWorker` is no longer running.
	pub async fn network_state(&self) -> Result<NetworkState, ()> {
		let (tx, rx) = oneshot::channel();

		let _ = self
			.to_worker
			.unbounded_send(ServiceToWorkerMsg::NetworkState { pending_response: tx });

		match rx.await {
			Ok(v) => v.map_err(|_| ()),
			// The channel can only be closed if the network worker no longer exists.
			Err(_) => Err(()),
		}
	}

	/// Utility function to extract `PeerId` from each `Multiaddr` for peer set updates.
	///
	/// Returns an `Err` if one of the given addresses is invalid or contains an
	/// invalid peer ID (which includes the local peer ID).
	fn split_multiaddr_and_peer_id(
		&self,
		peers: HashSet<Multiaddr>,
	) -> Result<Vec<(PeerId, Multiaddr)>, String> {
		peers
			.into_iter()
			.map(|mut addr| {
				let peer = match addr.pop() {
					Some(multiaddr::Protocol::P2p(peer_id)) => peer_id,
					_ => return Err("Missing PeerId from address".to_string()),
				};

				// Make sure the local peer ID is never added to the PSM
				// or added as a "known address", even if given.
				if peer == self.local_peer_id {
					Err("Local peer ID in peer set.".to_string())
				} else {
					Ok((peer, addr))
				}
			})
			.collect::<Result<Vec<(PeerId, Multiaddr)>, String>>()
	}
}

impl<B, H> NetworkStateInfo for NetworkService<B, H>
where
	B: sp_runtime::traits::Block,
	H: ExHashT,
{
	/// Returns the local external addresses.
	fn external_addresses(&self) -> Vec<sc_network_types::multiaddr::Multiaddr> {
		self.external_addresses.lock().iter().cloned().map(Into::into).collect()
	}

	/// Returns the listener addresses (without trailing `/p2p/` with our `PeerId`).
	fn listen_addresses(&self) -> Vec<sc_network_types::multiaddr::Multiaddr> {
		self.listen_addresses.lock().iter().cloned().map(Into::into).collect()
	}

	/// Returns the local Peer ID.
	fn local_peer_id(&self) -> sc_network_types::PeerId {
		self.local_peer_id.into()
	}
}

impl<B, H> NetworkSigner for NetworkService<B, H>
where
	B: sp_runtime::traits::Block,
	H: ExHashT,
{
	fn sign_with_local_identity(&self, msg: Vec<u8>) -> Result<Signature, SigningError> {
		let public_key = self.local_identity.public();
		let bytes = self.local_identity.sign(msg.as_ref())?;

		Ok(Signature {
			public_key: crate::service::signature::PublicKey::Libp2p(public_key),
			bytes,
		})
	}

	fn verify(
		&self,
		peer_id: sc_network_types::PeerId,
		public_key: &Vec<u8>,
		signature: &Vec<u8>,
		message: &Vec<u8>,
	) -> Result<bool, String> {
		let public_key =
			PublicKey::try_decode_protobuf(&public_key).map_err(|error| error.to_string())?;
		let peer_id: PeerId = peer_id.into();
		let remote: libp2p::PeerId = public_key.to_peer_id();

		Ok(peer_id == remote && public_key.verify(message, signature))
	}
}

impl<B, H> NetworkDHTProvider for NetworkService<B, H>
where
	B: BlockT + 'static,
	H: ExHashT,
{
	/// Start finding closest peerst to the target peer ID in the DHT.
	///
	/// This will generate either a `ClosestPeersFound` or a `ClosestPeersNotFound` event and pass
	/// it as an item on the [`NetworkWorker`] stream.
	fn find_closest_peers(&self, target: sc_network_types::PeerId) {
		let _ = self
			.to_worker
			.unbounded_send(ServiceToWorkerMsg::FindClosestPeers(target.into()));
	}

	/// Start getting a value from the DHT.
	///
	/// This will generate either a `ValueFound` or a `ValueNotFound` event and pass it as an
	/// item on the [`NetworkWorker`] stream.
	fn get_value(&self, key: &KademliaKey) {
		let _ = self.to_worker.unbounded_send(ServiceToWorkerMsg::GetValue(key.clone()));
	}

	/// Start putting a value in the DHT.
	///
	/// This will generate either a `ValuePut` or a `ValuePutFailed` event and pass it as an
	/// item on the [`NetworkWorker`] stream.
	fn put_value(&self, key: KademliaKey, value: Vec<u8>) {
		let _ = self.to_worker.unbounded_send(ServiceToWorkerMsg::PutValue(key, value));
	}

	fn put_record_to(
		&self,
		record: Record,
		peers: HashSet<sc_network_types::PeerId>,
		update_local_storage: bool,
	) {
		let _ = self.to_worker.unbounded_send(ServiceToWorkerMsg::PutRecordTo {
			record,
			peers,
			update_local_storage,
		});
	}

	fn store_record(
		&self,
		key: KademliaKey,
		value: Vec<u8>,
		publisher: Option<sc_network_types::PeerId>,
		expires: Option<Instant>,
	) {
		let _ = self.to_worker.unbounded_send(ServiceToWorkerMsg::StoreRecord(
			key,
			value,
			publisher.map(Into::into),
			expires,
		));
	}

	fn start_providing(&self, key: KademliaKey) {
		let _ = self.to_worker.unbounded_send(ServiceToWorkerMsg::StartProviding(key));
	}

	fn stop_providing(&self, key: KademliaKey) {
		let _ = self.to_worker.unbounded_send(ServiceToWorkerMsg::StopProviding(key));
	}

	fn get_providers(&self, key: KademliaKey) {
		let _ = self.to_worker.unbounded_send(ServiceToWorkerMsg::GetProviders(key));
	}
}

#[async_trait::async_trait]
impl<B, H> NetworkStatusProvider for NetworkService<B, H>
where
	B: BlockT + 'static,
	H: ExHashT,
{
	async fn status(&self) -> Result<NetworkStatus, ()> {
		let (tx, rx) = oneshot::channel();

		let _ = self
			.to_worker
			.unbounded_send(ServiceToWorkerMsg::NetworkStatus { pending_response: tx });

		match rx.await {
			Ok(v) => v.map_err(|_| ()),
			// The channel can only be closed if the network worker no longer exists.
			Err(_) => Err(()),
		}
	}

	async fn network_state(&self) -> Result<NetworkState, ()> {
		let (tx, rx) = oneshot::channel();

		let _ = self
			.to_worker
			.unbounded_send(ServiceToWorkerMsg::NetworkState { pending_response: tx });

		match rx.await {
			Ok(v) => v.map_err(|_| ()),
			// The channel can only be closed if the network worker no longer exists.
			Err(_) => Err(()),
		}
	}
}

#[async_trait::async_trait]
impl<B, H> NetworkPeers for NetworkService<B, H>
where
	B: BlockT + 'static,
	H: ExHashT,
{
	fn set_authorized_peers(&self, peers: HashSet<sc_network_types::PeerId>) {
		self.sync_protocol_handle
			.set_reserved_peers(peers.iter().map(|peer| (*peer).into()).collect());
	}

	fn set_authorized_only(&self, reserved_only: bool) {
		self.sync_protocol_handle.set_reserved_only(reserved_only);
	}

	fn add_known_address(
		&self,
		peer_id: sc_network_types::PeerId,
		addr: sc_network_types::multiaddr::Multiaddr,
	) {
		let _ = self
			.to_worker
			.unbounded_send(ServiceToWorkerMsg::AddKnownAddress(peer_id.into(), addr.into()));
	}

	fn report_peer(&self, peer_id: sc_network_types::PeerId, cost_benefit: ReputationChange) {
		self.peer_store_handle.report_peer(peer_id, cost_benefit);
	}

	fn peer_reputation(&self, peer_id: &sc_network_types::PeerId) -> i32 {
		self.peer_store_handle.peer_reputation(peer_id)
	}

	fn disconnect_peer(&self, peer_id: sc_network_types::PeerId, protocol: ProtocolName) {
		let _ = self
			.to_worker
			.unbounded_send(ServiceToWorkerMsg::DisconnectPeer(peer_id.into(), protocol));
	}

	fn accept_unreserved_peers(&self) {
		self.sync_protocol_handle.set_reserved_only(false);
	}

	fn deny_unreserved_peers(&self) {
		self.sync_protocol_handle.set_reserved_only(true);
	}

	fn add_reserved_peer(&self, peer: MultiaddrWithPeerId) -> Result<(), String> {
		// Make sure the local peer ID is never added as a reserved peer.
		if peer.peer_id == self.local_peer_id.into() {
			return Err("Local peer ID cannot be added as a reserved peer.".to_string());
		}

		let _ = self.to_worker.unbounded_send(ServiceToWorkerMsg::AddKnownAddress(
			peer.peer_id.into(),
			peer.multiaddr.into(),
		));
		self.sync_protocol_handle.add_reserved_peer(peer.peer_id.into());

		Ok(())
	}

	fn remove_reserved_peer(&self, peer_id: sc_network_types::PeerId) {
		self.sync_protocol_handle.remove_reserved_peer(peer_id.into());
	}

	fn set_reserved_peers(
		&self,
		protocol: ProtocolName,
		peers: HashSet<sc_network_types::multiaddr::Multiaddr>,
	) -> Result<(), String> {
		let Some(set_id) = self.notification_protocol_ids.get(&protocol) else {
			return Err(format!("Cannot set reserved peers for unknown protocol: {}", protocol));
		};

		let peers: HashSet<Multiaddr> = peers.into_iter().map(Into::into).collect();
		let peers_addrs = self.split_multiaddr_and_peer_id(peers)?;

		let mut peers: HashSet<PeerId> = HashSet::with_capacity(peers_addrs.len());

		for (peer_id, addr) in peers_addrs.into_iter() {
			// Make sure the local peer ID is never added to the PSM.
			if peer_id == self.local_peer_id {
				return Err("Local peer ID cannot be added as a reserved peer.".to_string());
			}

			peers.insert(peer_id.into());

			if !addr.is_empty() {
				let _ = self
					.to_worker
					.unbounded_send(ServiceToWorkerMsg::AddKnownAddress(peer_id, addr));
			}
		}

		self.protocol_handles[usize::from(*set_id)].set_reserved_peers(peers);

		Ok(())
	}

	fn add_peers_to_reserved_set(
		&self,
		protocol: ProtocolName,
		peers: HashSet<sc_network_types::multiaddr::Multiaddr>,
	) -> Result<(), String> {
		let Some(set_id) = self.notification_protocol_ids.get(&protocol) else {
			return Err(format!(
				"Cannot add peers to reserved set of unknown protocol: {}",
				protocol
			));
		};

		let peers: HashSet<Multiaddr> = peers.into_iter().map(Into::into).collect();
		let peers = self.split_multiaddr_and_peer_id(peers)?;

		for (peer_id, addr) in peers.into_iter() {
			// Make sure the local peer ID is never added to the PSM.
			if peer_id == self.local_peer_id {
				return Err("Local peer ID cannot be added as a reserved peer.".to_string());
			}

			if !addr.is_empty() {
				let _ = self
					.to_worker
					.unbounded_send(ServiceToWorkerMsg::AddKnownAddress(peer_id, addr));
			}

			self.protocol_handles[usize::from(*set_id)].add_reserved_peer(peer_id);
		}

		Ok(())
	}

	fn remove_peers_from_reserved_set(
		&self,
		protocol: ProtocolName,
		peers: Vec<sc_network_types::PeerId>,
	) -> Result<(), String> {
		let Some(set_id) = self.notification_protocol_ids.get(&protocol) else {
			return Err(format!(
				"Cannot remove peers from reserved set of unknown protocol: {}",
				protocol
			));
		};

		for peer_id in peers.into_iter() {
			self.protocol_handles[usize::from(*set_id)].remove_reserved_peer(peer_id.into());
		}

		Ok(())
	}

	fn sync_num_connected(&self) -> usize {
		self.num_connected.load(Ordering::Relaxed)
	}

	fn peer_role(
		&self,
		peer_id: sc_network_types::PeerId,
		handshake: Vec<u8>,
	) -> Option<ObservedRole> {
		match Roles::decode_all(&mut &handshake[..]) {
			Ok(role) => Some(role.into()),
			Err(_) => {
				log::debug!(target: LOG_TARGET, "handshake doesn't contain peer role: {handshake:?}");
				self.peer_store_handle.peer_role(&(peer_id.into()))
			},
		}
	}

	/// Get the list of reserved peers.
	///
	/// Returns an error if the `NetworkWorker` is no longer running.
	async fn reserved_peers(&self) -> Result<Vec<sc_network_types::PeerId>, ()> {
		let (tx, rx) = oneshot::channel();

		self.sync_protocol_handle.reserved_peers(tx);

		// The channel can only be closed if `ProtocolController` no longer exists.
		rx.await
			.map(|peers| peers.into_iter().map(From::from).collect())
			.map_err(|_| ())
	}
}

impl<B, H> NetworkEventStream for NetworkService<B, H>
where
	B: BlockT + 'static,
	H: ExHashT,
{
	fn event_stream(&self, name: &'static str) -> Pin<Box<dyn Stream<Item = Event> + Send>> {
		let (tx, rx) = out_events::channel(name, 100_000);
		let _ = self.to_worker.unbounded_send(ServiceToWorkerMsg::EventStream(tx));
		Box::pin(rx)
	}
}

#[async_trait::async_trait]
impl<B, H> NetworkRequest for NetworkService<B, H>
where
	B: BlockT + 'static,
	H: ExHashT,
{
	async fn request(
		&self,
		target: sc_network_types::PeerId,
		protocol: ProtocolName,
		request: Vec<u8>,
		fallback_request: Option<(Vec<u8>, ProtocolName)>,
		connect: IfDisconnected,
	) -> Result<(Vec<u8>, ProtocolName), RequestFailure> {
		let (tx, rx) = oneshot::channel();

		self.start_request(target.into(), protocol, request, fallback_request, tx, connect);

		match rx.await {
			Ok(v) => v,
			// The channel can only be closed if the network worker no longer exists. If the
			// network worker no longer exists, then all connections to `target` are necessarily
			// closed, and we legitimately report this situation as a "ConnectionClosed".
			Err(_) => Err(RequestFailure::Network(OutboundFailure::ConnectionClosed)),
		}
	}

	fn start_request(
		&self,
		target: sc_network_types::PeerId,
		protocol: ProtocolName,
		request: Vec<u8>,
		fallback_request: Option<(Vec<u8>, ProtocolName)>,
		tx: oneshot::Sender<Result<(Vec<u8>, ProtocolName), RequestFailure>>,
		connect: IfDisconnected,
	) {
		let _ = self.to_worker.unbounded_send(ServiceToWorkerMsg::Request {
			target: target.into(),
			protocol: protocol.into(),
			request,
			fallback_request,
			pending_response: tx,
			connect,
		});
	}
}

/// A `NotificationSender` allows for sending notifications to a peer with a chosen protocol.
#[must_use]
pub struct NotificationSender {
	sink: NotificationsSink,

	/// Name of the protocol on the wire.
	protocol_name: ProtocolName,

	/// Field extracted from the [`Metrics`] struct and necessary to report the
	/// notifications-related metrics.
	notification_size_metric: Option<Histogram>,
}

#[async_trait::async_trait]
impl NotificationSenderT for NotificationSender {
	async fn ready(
		&self,
	) -> Result<Box<dyn NotificationSenderReadyT + '_>, NotificationSenderError> {
		Ok(Box::new(NotificationSenderReady {
			ready: match self.sink.reserve_notification().await {
				Ok(r) => Some(r),
				Err(()) => return Err(NotificationSenderError::Closed),
			},
			peer_id: self.sink.peer_id(),
			protocol_name: &self.protocol_name,
			notification_size_metric: self.notification_size_metric.clone(),
		}))
	}
}

/// Reserved slot in the notifications buffer, ready to accept data.
#[must_use]
pub struct NotificationSenderReady<'a> {
	ready: Option<Ready<'a>>,

	/// Target of the notification.
	peer_id: &'a PeerId,

	/// Name of the protocol on the wire.
	protocol_name: &'a ProtocolName,

	/// Field extracted from the [`Metrics`] struct and necessary to report the
	/// notifications-related metrics.
	notification_size_metric: Option<Histogram>,
}

impl<'a> NotificationSenderReadyT for NotificationSenderReady<'a> {
	fn send(&mut self, notification: Vec<u8>) -> Result<(), NotificationSenderError> {
		if let Some(notification_size_metric) = &self.notification_size_metric {
			notification_size_metric.observe(notification.len() as f64);
		}

		trace!(
			target: LOG_TARGET,
			"External API => Notification({:?}, {}, {} bytes)",
			self.peer_id, self.protocol_name, notification.len(),
		);
		trace!(target: LOG_TARGET, "Handler({:?}) <= Async notification", self.peer_id);

		self.ready
			.take()
			.ok_or(NotificationSenderError::Closed)?
			.send(notification)
			.map_err(|()| NotificationSenderError::Closed)
	}
}

/// Messages sent from the `NetworkService` to the `NetworkWorker`.
///
/// Each entry corresponds to a method of `NetworkService`.
enum ServiceToWorkerMsg {
	FindClosestPeers(PeerId),
	GetValue(KademliaKey),
	PutValue(KademliaKey, Vec<u8>),
	PutRecordTo {
		record: Record,
		peers: HashSet<sc_network_types::PeerId>,
		update_local_storage: bool,
	},
	StoreRecord(KademliaKey, Vec<u8>, Option<PeerId>, Option<Instant>),
	StartProviding(KademliaKey),
	StopProviding(KademliaKey),
	GetProviders(KademliaKey),
	AddKnownAddress(PeerId, Multiaddr),
	EventStream(out_events::Sender),
	Request {
		target: PeerId,
		protocol: ProtocolName,
		request: Vec<u8>,
		fallback_request: Option<(Vec<u8>, ProtocolName)>,
		pending_response: oneshot::Sender<Result<(Vec<u8>, ProtocolName), RequestFailure>>,
		connect: IfDisconnected,
	},
	NetworkStatus {
		pending_response: oneshot::Sender<Result<NetworkStatus, RequestFailure>>,
	},
	NetworkState {
		pending_response: oneshot::Sender<Result<NetworkState, RequestFailure>>,
	},
	DisconnectPeer(PeerId, ProtocolName),
}

/// Main network worker. Must be polled in order for the network to advance.
///
/// You are encouraged to poll this in a separate background thread or task.
#[must_use = "The NetworkWorker must be polled in order for the network to advance"]
pub struct NetworkWorker<B, H>
where
	B: BlockT + 'static,
	H: ExHashT,
{
	/// Updated by the `NetworkWorker` and loaded by the `NetworkService`.
	listen_addresses: Arc<Mutex<HashSet<Multiaddr>>>,
	/// Updated by the `NetworkWorker` and loaded by the `NetworkService`.
	num_connected: Arc<AtomicUsize>,
	/// The network service that can be extracted and shared through the codebase.
	service: Arc<NetworkService<B, H>>,
	/// The *actual* network.
	network_service: Swarm<Behaviour<B>>,
	/// Messages from the [`NetworkService`] that must be processed.
	from_service: TracingUnboundedReceiver<ServiceToWorkerMsg>,
	/// Senders for events that happen on the network.
	event_streams: out_events::OutChannels,
	/// Prometheus network metrics.
	metrics: Option<Metrics>,
	/// The `PeerId`'s of all boot nodes mapped to the registered addresses.
	boot_node_ids: Arc<HashMap<PeerId, Vec<Multiaddr>>>,
	/// Boot nodes that we already have reported as invalid.
	reported_invalid_boot_nodes: HashSet<PeerId>,
	/// Peer reputation store handle.
	peer_store_handle: Arc<dyn PeerStoreProvider>,
	/// Notification protocol handles.
	notif_protocol_handles: Vec<protocol::ProtocolHandle>,
	/// Marker to pin the `H` generic. Serves no purpose except to not break backwards
	/// compatibility.
	_marker: PhantomData<H>,
	/// Marker for block type
	_block: PhantomData<B>,
}

impl<B, H> NetworkWorker<B, H>
where
	B: BlockT + 'static,
	H: ExHashT,
{
	/// Run the network.
	pub async fn run(mut self) {
		while self.next_action().await {}
	}

	/// Perform one action on the network.
	///
	/// Returns `false` when the worker should be shutdown.
	/// Use in tests only.
	pub async fn next_action(&mut self) -> bool {
		futures::select! {
			// Next message from the service.
			msg = self.from_service.next() => {
				if let Some(msg) = msg {
					self.handle_worker_message(msg);
				} else {
					return false
				}
			},
			// Next event from `Swarm` (the stream guaranteed to never terminate).
			event = self.network_service.select_next_some() => {
				self.handle_swarm_event(event);
			},
		};

		// Update the `num_connected` count shared with the `NetworkService`.
		let num_connected_peers = self.network_service.behaviour().user_protocol().num_sync_peers();
		self.num_connected.store(num_connected_peers, Ordering::Relaxed);

		if let Some(metrics) = self.metrics.as_ref() {
			if let Some(buckets) = self.network_service.behaviour_mut().num_entries_per_kbucket() {
				for (lower_ilog2_bucket_bound, num_entries) in buckets {
					metrics
						.kbuckets_num_nodes
						.with_label_values(&[&lower_ilog2_bucket_bound.to_string()])
						.set(num_entries as u64);
				}
			}
			if let Some(num_entries) = self.network_service.behaviour_mut().num_kademlia_records() {
				metrics.kademlia_records_count.set(num_entries as u64);
			}
			if let Some(num_entries) =
				self.network_service.behaviour_mut().kademlia_records_total_size()
			{
				metrics.kademlia_records_sizes_total.set(num_entries as u64);
			}

			metrics.pending_connections.set(
				Swarm::network_info(&self.network_service).connection_counters().num_pending()
					as u64,
			);
		}

		true
	}

	/// Process the next message coming from the `NetworkService`.
	fn handle_worker_message(&mut self, msg: ServiceToWorkerMsg) {
		match msg {
			ServiceToWorkerMsg::FindClosestPeers(target) => {
				self.network_service.behaviour_mut().find_closest_peers(target)
			},
			ServiceToWorkerMsg::GetValue(key) => {
				self.network_service.behaviour_mut().get_value(key.into())
			},
			ServiceToWorkerMsg::PutValue(key, value) => {
				self.network_service.behaviour_mut().put_value(key.into(), value)
			},
			ServiceToWorkerMsg::PutRecordTo { record, peers, update_local_storage } => self
				.network_service
				.behaviour_mut()
				.put_record_to(record.into(), peers, update_local_storage),
			ServiceToWorkerMsg::StoreRecord(key, value, publisher, expires) => self
				.network_service
				.behaviour_mut()
				.store_record(key.into(), value, publisher, expires),
			ServiceToWorkerMsg::StartProviding(key) => {
				self.network_service.behaviour_mut().start_providing(key.into())
			},
			ServiceToWorkerMsg::StopProviding(key) => {
				self.network_service.behaviour_mut().stop_providing(&key.into())
			},
			ServiceToWorkerMsg::GetProviders(key) => {
				self.network_service.behaviour_mut().get_providers(key.into())
			},
			ServiceToWorkerMsg::AddKnownAddress(peer_id, addr) => {
				self.network_service.behaviour_mut().add_known_address(peer_id, addr)
			},
			ServiceToWorkerMsg::EventStream(sender) => self.event_streams.push(sender),
			ServiceToWorkerMsg::Request {
				target,
				protocol,
				request,
				fallback_request,
				pending_response,
				connect,
			} => {
				self.network_service.behaviour_mut().send_request(
					&target,
					protocol,
					request,
					fallback_request,
					pending_response,
					connect,
				);
			},
			ServiceToWorkerMsg::NetworkStatus { pending_response } => {
				let _ = pending_response.send(Ok(self.status()));
			},
			ServiceToWorkerMsg::NetworkState { pending_response } => {
				let _ = pending_response.send(Ok(self.network_state()));
			},
			ServiceToWorkerMsg::DisconnectPeer(who, protocol_name) => self
				.network_service
				.behaviour_mut()
				.user_protocol_mut()
				.disconnect_peer(&who, protocol_name),
		}
	}

	/// Process the next event coming from `Swarm`.
	fn handle_swarm_event(&mut self, event: SwarmEvent<BehaviourOut>) {
		match event {
			SwarmEvent::Behaviour(BehaviourOut::InboundRequest { protocol, result, .. }) => {
				if let Some(metrics) = self.metrics.as_ref() {
					match result {
						Ok(serve_time) => {
							metrics
								.requests_in_success_total
								.with_label_values(&[&protocol])
								.observe(serve_time.as_secs_f64());
						},
						Err(err) => {
							let reason = match err {
								ResponseFailure::Network(InboundFailure::Timeout) => {
									Some("timeout")
								},
								ResponseFailure::Network(InboundFailure::UnsupportedProtocols) =>
								// `UnsupportedProtocols` is reported for every single
								// inbound request whenever a request with an unsupported
								// protocol is received. This is not reported in order to
								// avoid confusions.
								{
									None
								},
								ResponseFailure::Network(InboundFailure::ResponseOmission) => {
									Some("busy-omitted")
								},
								ResponseFailure::Network(InboundFailure::ConnectionClosed) => {
									Some("connection-closed")
								},
								ResponseFailure::Network(InboundFailure::Io(_)) => Some("io"),
							};

							if let Some(reason) = reason {
								metrics
									.requests_in_failure_total
									.with_label_values(&[&protocol, reason])
									.inc();
							}
						},
					}
				}
			},
			SwarmEvent::Behaviour(BehaviourOut::RequestFinished {
				protocol,
				duration,
				result,
				..
			}) => {
				if let Some(metrics) = self.metrics.as_ref() {
					match result {
						Ok(_) => {
							metrics
								.requests_out_success_total
								.with_label_values(&[&protocol])
								.observe(duration.as_secs_f64());
						},
						Err(err) => {
							let reason = match err {
								RequestFailure::NotConnected => "not-connected",
								RequestFailure::UnknownProtocol => "unknown-protocol",
								RequestFailure::InvalidRequest => "invalid-request",
								RequestFailure::Refused => "refused",
								RequestFailure::Obsolete => "obsolete",
								RequestFailure::Network(OutboundFailure::DialFailure) => {
									"dial-failure"
								},
								RequestFailure::Network(OutboundFailure::Timeout) => "timeout",
								RequestFailure::Network(OutboundFailure::ConnectionClosed) => {
									"connection-closed"
								},
								RequestFailure::Network(OutboundFailure::UnsupportedProtocols) => {
									"unsupported"
								},
								RequestFailure::Network(OutboundFailure::Io(_)) => "io",
							};

							metrics
								.requests_out_failure_total
								.with_label_values(&[&protocol, reason])
								.inc();
						},
					}
				}
			},
			SwarmEvent::Behaviour(BehaviourOut::ReputationChanges { peer, changes }) => {
				for change in changes {
					self.peer_store_handle.report_peer(peer.into(), change);
				}
			},
			SwarmEvent::Behaviour(BehaviourOut::PeerIdentify {
				peer_id,
				info:
					IdentifyInfo {
						protocol_version, agent_version, mut listen_addrs, protocols, ..
					},
			}) => {
				if listen_addrs.len() > 30 {
					debug!(
						target: LOG_TARGET,
						"Node {:?} has reported more than 30 addresses; it is identified by {:?} and {:?}",
						peer_id, protocol_version, agent_version
					);
					listen_addrs.truncate(30);
				}
				for addr in listen_addrs {
					self.network_service.behaviour_mut().add_self_reported_address_to_dht(
						&peer_id,
						&protocols,
						addr.clone(),
					);
				}
				self.peer_store_handle.add_known_peer(peer_id.into());
			},
			SwarmEvent::Behaviour(BehaviourOut::Discovered(peer_id)) => {
				self.peer_store_handle.add_known_peer(peer_id.into());
			},
			SwarmEvent::Behaviour(BehaviourOut::RandomKademliaStarted) => {
				if let Some(metrics) = self.metrics.as_ref() {
					metrics.kademlia_random_queries_total.inc();
				}
			},
			SwarmEvent::Behaviour(BehaviourOut::NotificationStreamOpened {
				remote,
				set_id,
				direction,
				negotiated_fallback,
				notifications_sink,
				received_handshake,
			}) => {
				let _ = self.notif_protocol_handles[usize::from(set_id)].report_substream_opened(
					remote,
					direction,
					received_handshake,
					negotiated_fallback,
					notifications_sink,
				);
			},
			SwarmEvent::Behaviour(BehaviourOut::NotificationStreamReplaced {
				remote,
				set_id,
				notifications_sink,
			}) => {
				let _ = self.notif_protocol_handles[usize::from(set_id)]
					.report_notification_sink_replaced(remote, notifications_sink);

				// TODO: Notifications might have been lost as a result of the previous
				// connection being dropped, and as a result it would be preferable to notify
				// the users of this fact by simulating the substream being closed then
				// reopened.
				// The code below doesn't compile because `role` is unknown. Propagating the
				// handshake of the secondary connections is quite an invasive change and
				// would conflict with https://github.com/paritytech/substrate/issues/6403.
				// Considering that dropping notifications is generally regarded as
				// acceptable, this bug is at the moment intentionally left there and is
				// intended to be fixed at the same time as
				// https://github.com/paritytech/substrate/issues/6403.
				// self.event_streams.send(Event::NotificationStreamClosed {
				// remote,
				// protocol,
				// });
				// self.event_streams.send(Event::NotificationStreamOpened {
				// remote,
				// protocol,
				// role,
				// });
			},
			SwarmEvent::Behaviour(BehaviourOut::NotificationStreamClosed { remote, set_id }) => {
				let _ = self.notif_protocol_handles[usize::from(set_id)]
					.report_substream_closed(remote);
			},
			SwarmEvent::Behaviour(BehaviourOut::NotificationsReceived {
				remote,
				set_id,
				notification,
			}) => {
				let _ = self.notif_protocol_handles[usize::from(set_id)]
					.report_notification_received(remote, notification);
			},
			SwarmEvent::Behaviour(BehaviourOut::Dht(event, duration)) => {
				match (self.metrics.as_ref(), duration) {
					(Some(metrics), Some(duration)) => {
						let query_type = match event {
							DhtEvent::ClosestPeersFound(_, _) => "peers-found",
							DhtEvent::ClosestPeersNotFound(_) => "peers-not-found",
							DhtEvent::ValueFound(_) => "value-found",
							DhtEvent::ValueNotFound(_) => "value-not-found",
							DhtEvent::ValuePut(_) => "value-put",
							DhtEvent::ValuePutFailed(_) => "value-put-failed",
							DhtEvent::PutRecordRequest(_, _, _, _) => "put-record-request",
							DhtEvent::StartedProviding(_) => "started-providing",
							DhtEvent::StartProvidingFailed(_) => "start-providing-failed",
							DhtEvent::ProvidersFound(_, _) => "providers-found",
							DhtEvent::NoMoreProviders(_) => "no-more-providers",
							DhtEvent::ProvidersNotFound(_) => "providers-not-found",
						};
						metrics
							.kademlia_query_duration
							.with_label_values(&[query_type])
							.observe(duration.as_secs_f64());
					},
					_ => {},
				}

				self.event_streams.send(Event::Dht(event));
			},
			SwarmEvent::Behaviour(BehaviourOut::None) => {
				// Ignored event from lower layers.
			},
			SwarmEvent::ConnectionEstablished {
				peer_id,
				endpoint,
				num_established,
				concurrent_dial_errors,
				..
			} => {
				if let Some(errors) = concurrent_dial_errors {
					debug!(target: LOG_TARGET, "Libp2p => Connected({:?}) with errors: {:?}", peer_id, errors);
				} else {
					debug!(target: LOG_TARGET, "Libp2p => Connected({:?})", peer_id);
				}

				if let Some(metrics) = self.metrics.as_ref() {
					let direction = match endpoint {
						ConnectedPoint::Dialer { .. } => "out",
						ConnectedPoint::Listener { .. } => "in",
					};
					metrics.connections_opened_total.with_label_values(&[direction]).inc();

					if num_established.get() == 1 {
						metrics.distinct_peers_connections_opened_total.inc();
					}
				}
			},
			SwarmEvent::ConnectionClosed {
				connection_id,
				peer_id,
				cause,
				endpoint,
				num_established,
			} => {
				debug!(target: LOG_TARGET, "Libp2p => Disconnected({peer_id:?} via {connection_id:?}, {cause:?})");
				if let Some(metrics) = self.metrics.as_ref() {
					let direction = match endpoint {
						ConnectedPoint::Dialer { .. } => "out",
						ConnectedPoint::Listener { .. } => "in",
					};
					let reason = match cause {
						Some(ConnectionError::IO(_)) => "transport-error",
						Some(ConnectionError::KeepAliveTimeout) => "keep-alive-timeout",
						None => "actively-closed",
					};
					metrics.connections_closed_total.with_label_values(&[direction, reason]).inc();

					// `num_established` represents the number of *remaining* connections.
					if num_established == 0 {
						metrics.distinct_peers_connections_closed_total.inc();
					}
				}
			},
			SwarmEvent::NewListenAddr { address, .. } => {
				trace!(target: LOG_TARGET, "Libp2p => NewListenAddr({})", address);
				if let Some(metrics) = self.metrics.as_ref() {
					metrics.listeners_local_addresses.inc();
				}
				self.listen_addresses.lock().insert(address.clone());
			},
			SwarmEvent::ExpiredListenAddr { address, .. } => {
				info!(target: LOG_TARGET, "📪 No longer listening on {}", address);
				if let Some(metrics) = self.metrics.as_ref() {
					metrics.listeners_local_addresses.dec();
				}
				self.listen_addresses.lock().remove(&address);
			},
			SwarmEvent::OutgoingConnectionError { connection_id, peer_id, error } => {
				if let Some(peer_id) = peer_id {
					trace!(
						target: LOG_TARGET,
						"Libp2p => Failed to reach {peer_id:?} via {connection_id:?}: {error}",
					);

					let not_reported = !self.reported_invalid_boot_nodes.contains(&peer_id);

					if let Some(addresses) =
						not_reported.then(|| self.boot_node_ids.get(&peer_id)).flatten()
					{
						if let DialError::WrongPeerId { obtained, endpoint } = &error {
							if let ConnectedPoint::Dialer {
								address,
								role_override: _,
								port_use: _,
							} = endpoint
							{
								let address_without_peer_id = parse_addr(address.clone().into())
									.map_or_else(|_| address.clone(), |r| r.1.into());

								// Only report for address of boot node that was added at startup of
								// the node and not for any address that the node learned of the
								// boot node.
								if addresses.iter().any(|a| address_without_peer_id == *a) {
									warn!(
										"💔 The bootnode you want to connect to at `{address}` provided a \
										 different peer ID `{obtained}` than the one you expect `{peer_id}`.",
									);

									self.reported_invalid_boot_nodes.insert(peer_id);
								}
							}
						}
					}
				}

				if let Some(metrics) = self.metrics.as_ref() {
					let reason = match error {
						DialError::Denied { cause } => {
							if cause.downcast::<Exceeded>().is_ok() {
								Some("limit-reached")
							} else {
								None
							}
						},
						DialError::LocalPeerId { .. } => Some("local-peer-id"),
						DialError::WrongPeerId { .. } => Some("invalid-peer-id"),
						DialError::Transport(_) => Some("transport-error"),
						DialError::NoAddresses |
						DialError::DialPeerConditionFalse(_) |
						DialError::Aborted => None, // ignore them
					};
					if let Some(reason) = reason {
						metrics.pending_connections_errors_total.with_label_values(&[reason]).inc();
					}
				}
			},
			SwarmEvent::Dialing { connection_id, peer_id } => {
				trace!(target: LOG_TARGET, "Libp2p => Dialing({peer_id:?}) via {connection_id:?}")
			},
			SwarmEvent::IncomingConnection { connection_id, local_addr, send_back_addr } => {
				trace!(target: LOG_TARGET, "Libp2p => IncomingConnection({local_addr},{send_back_addr} via {connection_id:?}))");
				if let Some(metrics) = self.metrics.as_ref() {
					metrics.incoming_connections_total.inc();
				}
			},
			SwarmEvent::IncomingConnectionError {
				connection_id,
				local_addr,
				send_back_addr,
				error,
			} => {
				debug!(
					target: LOG_TARGET,
					"Libp2p => IncomingConnectionError({local_addr},{send_back_addr} via {connection_id:?}): {error}"
				);
				if let Some(metrics) = self.metrics.as_ref() {
					let reason = match error {
						ListenError::Denied { cause } => {
							if cause.downcast::<Exceeded>().is_ok() {
								Some("limit-reached")
							} else {
								None
							}
						},
						ListenError::WrongPeerId { .. } | ListenError::LocalPeerId { .. } => {
							Some("invalid-peer-id")
						},
						ListenError::Transport(_) => Some("transport-error"),
						ListenError::Aborted => None, // ignore it
					};

					if let Some(reason) = reason {
						metrics
							.incoming_connections_errors_total
							.with_label_values(&[reason])
							.inc();
					}
				}
			},
			SwarmEvent::ListenerClosed { reason, addresses, .. } => {
				if let Some(metrics) = self.metrics.as_ref() {
					metrics.listeners_local_addresses.sub(addresses.len() as u64);
				}
				let mut listen_addresses = self.listen_addresses.lock();
				for addr in &addresses {
					listen_addresses.remove(addr);
				}
				drop(listen_addresses);

				let addrs =
					addresses.into_iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
				match reason {
					Ok(()) => error!(
						target: LOG_TARGET,
						"📪 Libp2p listener ({}) closed gracefully",
						addrs
					),
					Err(e) => error!(
						target: LOG_TARGET,
						"📪 Libp2p listener ({}) closed: {}",
						addrs, e
					),
				}
			},
			SwarmEvent::ListenerError { error, .. } => {
				debug!(target: LOG_TARGET, "Libp2p => ListenerError: {}", error);
				if let Some(metrics) = self.metrics.as_ref() {
					metrics.listeners_errors_total.inc();
				}
			},
			SwarmEvent::NewExternalAddrCandidate { address } => {
				trace!(target: LOG_TARGET, "Libp2p => NewExternalAddrCandidate: {address:?}");
			},
			SwarmEvent::ExternalAddrConfirmed { address } => {
				trace!(target: LOG_TARGET, "Libp2p => ExternalAddrConfirmed: {address:?}");
			},
			SwarmEvent::ExternalAddrExpired { address } => {
				trace!(target: LOG_TARGET, "Libp2p => ExternalAddrExpired: {address:?}");
			},
			SwarmEvent::NewExternalAddrOfPeer { peer_id, address } => {
				trace!(target: LOG_TARGET, "Libp2p => NewExternalAddrOfPeer({peer_id:?}): {address:?}")
			},
			event => {
				warn!(target: LOG_TARGET, "New unknown SwarmEvent libp2p event: {event:?}");
			},
		}
	}
}

impl<B, H> Unpin for NetworkWorker<B, H>
where
	B: BlockT + 'static,
	H: ExHashT,
{
}

pub(crate) fn ensure_addresses_consistent_with_transport<'a>(
	addresses: impl Iterator<Item = &'a sc_network_types::multiaddr::Multiaddr>,
	transport: &TransportConfig,
) -> Result<(), Error> {
	use sc_network_types::multiaddr::Protocol;

	if matches!(transport, TransportConfig::MemoryOnly) {
		let addresses: Vec<_> = addresses
			.filter(|x| x.iter().any(|y| !matches!(y, Protocol::Memory(_))))
			.cloned()
			.collect();

		if !addresses.is_empty() {
			return Err(Error::AddressesForAnotherTransport {
				transport: transport.clone(),
				addresses,
			});
		}
	} else {
		let addresses: Vec<_> = addresses
			.filter(|x| x.iter().any(|y| matches!(y, Protocol::Memory(_))))
			.cloned()
			.collect();

		if !addresses.is_empty() {
			return Err(Error::AddressesForAnotherTransport {
				transport: transport.clone(),
				addresses,
			});
		}
	}

	Ok(())
}
