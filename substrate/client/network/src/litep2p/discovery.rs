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

//! libp2p-related discovery code for litep2p backend.

use crate::{
	config::{
		NetworkConfiguration, ProtocolId, KADEMLIA_MAX_PROVIDER_KEYS, KADEMLIA_PROVIDER_RECORD_TTL,
		KADEMLIA_PROVIDER_REPUBLISH_INTERVAL,
	},
	peer_store::PeerStoreProvider,
};

use array_bytes::bytes2hex;
use futures::{FutureExt, Stream};
use futures_timer::Delay;
use ip_network::IpNetwork;
use litep2p::{
	protocol::{
		libp2p::{
			identify::{Config as IdentifyConfig, IdentifyEvent},
			kademlia::{
				Config as KademliaConfig, ConfigBuilder as KademliaConfigBuilder, ContentProvider,
				IncomingRecordValidationMode, KademliaEvent, KademliaHandle, PeerRecord, QueryId,
				Quorum, Record, RecordKey,
			},
			ping::{Config as PingConfig, PingEvent},
		},
		mdns::{Config as MdnsConfig, MdnsEvent},
	},
	types::multiaddr::{Multiaddr, Protocol},
	PeerId, ProtocolName,
};
use parking_lot::RwLock;
use sc_network_types::kad::Key as KademliaKey;
use schnellru::{ByLength, LruMap};

use std::{
	cmp,
	collections::{HashMap, HashSet, VecDeque},
	iter,
	num::NonZeroUsize,
	pin::Pin,
	sync::Arc,
	task::{Context, Poll},
	time::{Duration, Instant},
};

/// Logging target for the file.
const LOG_TARGET: &str = "sub-libp2p::discovery";

/// Kademlia query interval.
const KADEMLIA_QUERY_INTERVAL: Duration = Duration::from_secs(5);

/// mDNS query interval.
const MDNS_QUERY_INTERVAL: Duration = Duration::from_secs(30);

/// The minimum number of peers we expect an answer before we terminate the request.
const GET_RECORD_REDUNDANCY_FACTOR: usize = 4;

/// The maximum number of tracked external addresses we allow.
const MAX_EXTERNAL_ADDRESSES: u32 = 32;

/// Number of times observed address is received from different peers before it is confirmed as
/// external.
const MIN_ADDRESS_CONFIRMATIONS: usize = 3;

/// Quorum threshold to interpret `PUT_VALUE` & `ADD_PROVIDER` as successful.
///
/// As opposed to libp2p, litep2p does not finish the query as soon as the required number of
/// peers have reached. Instead, it tries to put the record to all target peers (typically 20) and
/// uses the quorum setting only to determine the success of the query.
///
/// We set the threshold to 50% of the target peers to account for unreachable peers. The actual
/// number of stored records may be higher.
const QUORUM_THRESHOLD: NonZeroUsize = NonZeroUsize::new(10).expect("10 > 0; qed");

/// Discovery events.
#[derive(Debug)]
pub enum DiscoveryEvent {
	/// Ping RTT measured for peer.
	Ping {
		/// Remote peer ID.
		peer: PeerId,

		/// Ping round-trip time.
		rtt: Duration,
	},

	/// Peer identified over `/ipfs/identify/1.0.0` protocol.
	Identified {
		/// Peer ID.
		peer: PeerId,

		/// Listen addresses.
		listen_addresses: Vec<Multiaddr>,

		/// Supported protocols.
		supported_protocols: HashSet<ProtocolName>,
	},

	/// One or more addresses discovered.
	///
	/// This event is emitted when a new peer is discovered over mDNS.
	Discovered {
		/// Discovered addresses.
		addresses: Vec<Multiaddr>,
	},

	/// Routing table has been updated.
	RoutingTableUpdate {
		/// Peers that were added to routing table.
		peers: HashSet<PeerId>,
	},

	/// New external address discovered.
	ExternalAddressDiscovered {
		/// Discovered address.
		address: Multiaddr,
	},

	/// The external address has expired.
	///
	/// This happens when the internal buffers exceed the maximum number of external addresses,
	/// and this address is the oldest one.
	ExternalAddressExpired {
		/// Expired address.
		address: Multiaddr,
	},

	/// `FIND_NODE` query succeeded.
	FindNodeSuccess {
		/// Query ID.
		query_id: QueryId,

		/// Target.
		target: PeerId,

		/// Found peers.
		peers: Vec<(PeerId, Vec<Multiaddr>)>,
	},

	/// `GetRecord` query succeeded.
	GetRecordSuccess {
		/// Query ID.
		query_id: QueryId,
	},

	/// Record was found from the DHT.
	GetRecordPartialResult {
		/// Query ID.
		query_id: QueryId,

		/// Record.
		record: PeerRecord,
	},

	/// Record was successfully stored on the DHT.
	PutRecordSuccess {
		/// Query ID.
		query_id: QueryId,
	},

	/// Providers were successfully retrieved.
	GetProvidersSuccess {
		/// Query ID.
		query_id: QueryId,
		/// Found providers sorted by distance to provided key.
		providers: Vec<ContentProvider>,
	},

	/// Provider was successfully published.
	AddProviderSuccess {
		/// Query ID.
		query_id: QueryId,
		/// Provided key.
		provided_key: RecordKey,
	},

	/// Query failed.
	QueryFailed {
		/// Query ID.
		query_id: QueryId,
	},

	/// Incoming record to store.
	IncomingRecord {
		/// Record.
		record: Record,
	},

	/// Started a random Kademlia query.
	RandomKademliaStarted,
}

/// Discovery.
pub struct Discovery {
	/// Local peer ID.
	local_peer_id: litep2p::PeerId,

	/// Ping event stream.
	ping_event_stream: Box<dyn Stream<Item = PingEvent> + Send + Unpin>,

	/// Identify event stream.
	identify_event_stream: Box<dyn Stream<Item = IdentifyEvent> + Send + Unpin>,

	/// mDNS event stream, if enabled.
	mdns_event_stream: Option<Box<dyn Stream<Item = MdnsEvent> + Send + Unpin>>,

	/// Kademlia handle.
	kademlia_handle: KademliaHandle,

	/// `Peerstore` handle.
	_peerstore_handle: Arc<dyn PeerStoreProvider>,

	/// Next Kademlia query for a random peer ID.
	///
	/// If `None`, there is currently a query pending.
	next_kad_query: Option<Delay>,

	/// Active `FIND_NODE` query if it exists.
	random_walk_query_id: Option<QueryId>,

	/// Pending events.
	pending_events: VecDeque<DiscoveryEvent>,

	/// Allow non-global addresses in the DHT.
	allow_non_global_addresses: bool,

	/// Protocols supported by the local node.
	local_protocols: HashSet<ProtocolName>,

	/// Public addresses.
	public_addresses: HashSet<Multiaddr>,

	/// Listen addresses.
	listen_addresses: Arc<RwLock<HashSet<Multiaddr>>>,

	/// External address confirmations.
	address_confirmations: LruMap<Multiaddr, HashSet<PeerId>>,

	/// Delay to next `FIND_NODE` query.
	duration_to_next_find_query: Duration,
}

/// Legacy (fallback) Kademlia protocol name based on `protocol_id`.
fn legacy_kademlia_protocol_name(id: &ProtocolId) -> ProtocolName {
	ProtocolName::from(format!("/{}/kad", id.as_ref()))
}

/// Kademlia protocol name based on `genesis_hash` and `fork_id`.
fn kademlia_protocol_name<Hash: AsRef<[u8]>>(
	genesis_hash: Hash,
	fork_id: Option<&str>,
) -> ProtocolName {
	let genesis_hash_hex = bytes2hex("", genesis_hash.as_ref());
	let protocol = if let Some(fork_id) = fork_id {
		format!("/{}/{}/kad", genesis_hash_hex, fork_id)
	} else {
		format!("/{}/kad", genesis_hash_hex)
	};

	ProtocolName::from(protocol)
}

impl Discovery {
	/// Create new [`Discovery`].
	///
	/// Enables `/ipfs/ping/1.0.0` and `/ipfs/identify/1.0.0` by default and starts
	/// the mDNS peer discovery if it was enabled.
	pub fn new<Hash: AsRef<[u8]> + Clone>(
		local_peer_id: litep2p::PeerId,
		config: &NetworkConfiguration,
		genesis_hash: Hash,
		fork_id: Option<&str>,
		protocol_id: &ProtocolId,
		known_peers: HashMap<PeerId, Vec<Multiaddr>>,
		listen_addresses: Arc<RwLock<HashSet<Multiaddr>>>,
		_peerstore_handle: Arc<dyn PeerStoreProvider>,
	) -> (Self, PingConfig, IdentifyConfig, KademliaConfig, Option<MdnsConfig>) {
		let (ping_config, ping_event_stream) = PingConfig::default();
		let user_agent = format!("{} ({}) (litep2p)", config.client_version, config.node_name);

		let (identify_config, identify_event_stream) =
			IdentifyConfig::new("/substrate/1.0".to_string(), Some(user_agent));

		let (mdns_config, mdns_event_stream) = match config.transport {
			crate::config::TransportConfig::Normal { enable_mdns, .. } => match enable_mdns {
				true => {
					let (mdns_config, mdns_event_stream) = MdnsConfig::new(MDNS_QUERY_INTERVAL);
					(Some(mdns_config), Some(mdns_event_stream))
				},
				false => (None, None),
			},
			_ => panic!("memory transport not supported"),
		};

		let (kademlia_config, kademlia_handle) = {
			let protocol_names = vec![
				kademlia_protocol_name(genesis_hash.clone(), fork_id),
				legacy_kademlia_protocol_name(protocol_id),
			];

			KademliaConfigBuilder::new()
				.with_known_peers(known_peers)
				.with_protocol_names(protocol_names)
				.with_incoming_records_validation_mode(IncomingRecordValidationMode::Manual)
				.with_provider_record_ttl(KADEMLIA_PROVIDER_RECORD_TTL)
				.with_provider_refresh_interval(KADEMLIA_PROVIDER_REPUBLISH_INTERVAL)
				.with_max_provider_keys(KADEMLIA_MAX_PROVIDER_KEYS)
				.build()
		};

		(
			Self {
				local_peer_id,
				ping_event_stream,
				identify_event_stream,
				mdns_event_stream,
				kademlia_handle,
				_peerstore_handle,
				listen_addresses,
				random_walk_query_id: None,
				pending_events: VecDeque::new(),
				duration_to_next_find_query: Duration::from_secs(1),
				address_confirmations: LruMap::new(ByLength::new(MAX_EXTERNAL_ADDRESSES)),
				allow_non_global_addresses: config.allow_non_globals_in_dht,
				public_addresses: config.public_addresses.iter().cloned().map(Into::into).collect(),
				next_kad_query: Some(Delay::new(KADEMLIA_QUERY_INTERVAL)),
				local_protocols: HashSet::from_iter([kademlia_protocol_name(
					genesis_hash,
					fork_id,
				)]),
			},
			ping_config,
			identify_config,
			kademlia_config,
			mdns_config,
		)
	}

	/// Add known peer to `Kademlia`.
	#[allow(unused)]
	pub async fn add_known_peer(&mut self, peer: PeerId, addresses: Vec<Multiaddr>) {
		self.kademlia_handle.add_known_peer(peer, addresses).await;
	}

	/// Add self-reported addresses to routing table if `peer` supports
	/// at least one of the locally supported DHT protocol.
	pub async fn add_self_reported_address(
		&mut self,
		peer: PeerId,
		supported_protocols: HashSet<ProtocolName>,
		addresses: Vec<Multiaddr>,
	) {
		if self.local_protocols.is_disjoint(&supported_protocols) {
			log::trace!(
				target: LOG_TARGET,
				"Ignoring self-reported address of peer {peer} as remote node is not part of the \
				 Kademlia DHT supported by the local node.",
			);
			return;
		}

		let addresses = addresses
			.into_iter()
			.filter_map(|address| {
				if !self.allow_non_global_addresses && !Discovery::can_add_to_dht(&address) {
					log::trace!(
						target: LOG_TARGET,
						"ignoring self-reported non-global address {address} from {peer}."
					);

					return None;
				}

				Some(address)
			})
			.collect();

		log::trace!(
			target: LOG_TARGET,
			"add self-reported addresses for {peer:?}: {addresses:?}",
		);

		self.kademlia_handle.add_known_peer(peer, addresses).await;
	}

	/// Start Kademlia `FIND_NODE` query for `target`.
	pub async fn find_node(&mut self, target: PeerId) -> QueryId {
		self.kademlia_handle.find_node(target).await
	}

	/// Start Kademlia `GET_VALUE` query for `key`.
	pub async fn get_value(&mut self, key: KademliaKey) -> QueryId {
		self.kademlia_handle
			.get_record(
				RecordKey::new(&key.to_vec()),
				Quorum::N(NonZeroUsize::new(GET_RECORD_REDUNDANCY_FACTOR).unwrap()),
			)
			.await
	}

	/// Publish value on the DHT using Kademlia `PUT_VALUE`.
	pub async fn put_value(&mut self, key: KademliaKey, value: Vec<u8>) -> QueryId {
		self.kademlia_handle
			.put_record(
				Record::new(RecordKey::new(&key.to_vec()), value),
				Quorum::N(QUORUM_THRESHOLD),
			)
			.await
	}

	/// Put record to given peers.
	pub async fn put_value_to_peers(
		&mut self,
		record: Record,
		peers: Vec<sc_network_types::PeerId>,
		update_local_storage: bool,
	) -> QueryId {
		self.kademlia_handle
			.put_record_to_peers(
				record,
				peers.into_iter().map(|peer| peer.into()).collect(),
				update_local_storage,
				// These are the peers that just returned the record to us in authority-discovery,
				// so we assume they are all reachable.
				Quorum::All,
			)
			.await
	}

	/// Store record in the local DHT store.
	pub async fn store_record(
		&mut self,
		key: KademliaKey,
		value: Vec<u8>,
		publisher: Option<sc_network_types::PeerId>,
		expires: Option<Instant>,
	) {
		log::debug!(
			target: LOG_TARGET,
			"Storing DHT record with key {key:?}, originally published by {publisher:?}, \
			 expires {expires:?}.",
		);

		self.kademlia_handle
			.store_record(Record {
				key: RecordKey::new(&key.to_vec()),
				value,
				publisher: publisher.map(Into::into),
				expires,
			})
			.await;
	}

	/// Start providing `key`.
	pub async fn start_providing(&mut self, key: KademliaKey) -> QueryId {
		self.kademlia_handle
			.start_providing(key.into(), Quorum::N(QUORUM_THRESHOLD))
			.await
	}

	/// Stop providing `key`.
	pub async fn stop_providing(&mut self, key: KademliaKey) {
		self.kademlia_handle.stop_providing(key.into()).await;
	}

	/// Get providers for `key`.
	pub async fn get_providers(&mut self, key: KademliaKey) -> QueryId {
		self.kademlia_handle.get_providers(key.into()).await
	}

	/// Check if the observed address is a known address.
	fn is_known_address(known: &Multiaddr, observed: &Multiaddr) -> bool {
		let mut known = known.iter();
		let mut observed = observed.iter();

		loop {
			match (known.next(), observed.next()) {
				(None, None) => return true,
				(None, Some(Protocol::P2p(_))) => return true,
				(Some(Protocol::P2p(_)), None) => return true,
				(known, observed) if known != observed => return false,
				_ => {},
			}
		}
	}

	/// Can `address` be added to DHT.
	fn can_add_to_dht(address: &Multiaddr) -> bool {
		let ip = match address.iter().next() {
			Some(Protocol::Ip4(ip)) => IpNetwork::from(ip),
			Some(Protocol::Ip6(ip)) => IpNetwork::from(ip),
			Some(Protocol::Dns(_)) | Some(Protocol::Dns4(_)) | Some(Protocol::Dns6(_)) => {
				return true
			},
			_ => return false,
		};

		ip.is_global()
	}

	/// Check if `address` can be considered a new external address.
	///
	/// If this address replaces an older address, the expired address is returned.
	fn is_new_external_address(
		&mut self,
		address: &Multiaddr,
		peer: PeerId,
	) -> (bool, Option<Multiaddr>) {
		log::trace!(target: LOG_TARGET, "verify new external address: {address}");

		if !self.allow_non_global_addresses && !Discovery::can_add_to_dht(&address) {
			log::trace!(
				target: LOG_TARGET,
				"ignoring externally reported non-global address {address} from {peer}."
			);

			return (false, None);
		}

		// is the address one of our known addresses
		if self
			.listen_addresses
			.read()
			.iter()
			.chain(self.public_addresses.iter())
			.any(|known_address| Discovery::is_known_address(&known_address, &address))
		{
			return (true, None);
		}

		match self.address_confirmations.get(address) {
			Some(confirmations) => {
				confirmations.insert(peer);

				if confirmations.len() >= MIN_ADDRESS_CONFIRMATIONS {
					return (true, None);
				}
			},
			None => {
				let oldest = (self.address_confirmations.len() >=
					self.address_confirmations.limiter().max_length() as usize)
					.then(|| {
						self.address_confirmations.pop_oldest().map(|(address, peers)| {
							if peers.len() >= MIN_ADDRESS_CONFIRMATIONS {
								return Some(address);
							} else {
								None
							}
						})
					})
					.flatten()
					.flatten();

				self.address_confirmations.insert(address.clone(), iter::once(peer).collect());

				return (false, oldest);
			},
		}

		(false, None)
	}
}

impl Stream for Discovery {
	type Item = DiscoveryEvent;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		let this = Pin::into_inner(self);

		if let Some(event) = this.pending_events.pop_front() {
			return Poll::Ready(Some(event));
		}

		if let Some(mut delay) = this.next_kad_query.take() {
			match delay.poll_unpin(cx) {
				Poll::Pending => {
					this.next_kad_query = Some(delay);
				},
				Poll::Ready(()) => {
					let peer = PeerId::random();

					log::trace!(target: LOG_TARGET, "start next kademlia query for {peer:?}");

					match this.kademlia_handle.try_find_node(peer) {
						Ok(query_id) => {
							this.random_walk_query_id = Some(query_id);
							return Poll::Ready(Some(DiscoveryEvent::RandomKademliaStarted));
						},
						Err(()) => {
							this.duration_to_next_find_query = cmp::min(
								this.duration_to_next_find_query * 2,
								Duration::from_secs(60),
							);
							this.next_kad_query =
								Some(Delay::new(this.duration_to_next_find_query));
						},
					}
				},
			}
		}

		match Pin::new(&mut this.kademlia_handle).poll_next(cx) {
			Poll::Pending => {},
			Poll::Ready(None) => return Poll::Ready(None),
			Poll::Ready(Some(KademliaEvent::FindNodeSuccess { query_id, peers, .. }))
				if Some(query_id) == this.random_walk_query_id =>
			{
				// the addresses are already inserted into the DHT and in `TransportManager` so
				// there is no need to add them again. The found peers must be registered to
				// `Peerstore` so other protocols are aware of them through `Peerset`.
				log::trace!(target: LOG_TARGET, "dht random walk yielded {} peers", peers.len());

				this.next_kad_query = Some(Delay::new(KADEMLIA_QUERY_INTERVAL));

				return Poll::Ready(Some(DiscoveryEvent::RoutingTableUpdate {
					peers: peers.into_iter().map(|(peer, _)| peer).collect(),
				}));
			},
			Poll::Ready(Some(KademliaEvent::FindNodeSuccess { query_id, target, peers })) => {
				log::trace!(target: LOG_TARGET, "find node query yielded {} peers", peers.len());

				return Poll::Ready(Some(DiscoveryEvent::FindNodeSuccess {
					query_id,
					target,
					peers,
				}));
			},
			Poll::Ready(Some(KademliaEvent::RoutingTableUpdate { peers })) => {
				log::trace!(target: LOG_TARGET, "routing table update, discovered {} peers", peers.len());

				return Poll::Ready(Some(DiscoveryEvent::RoutingTableUpdate {
					peers: peers.into_iter().collect(),
				}));
			},
			Poll::Ready(Some(KademliaEvent::GetRecordSuccess { query_id })) => {
				log::trace!(
					target: LOG_TARGET,
					"`GET_RECORD` succeeded for {query_id:?}",
				);

				return Poll::Ready(Some(DiscoveryEvent::GetRecordSuccess { query_id }));
			},
			Poll::Ready(Some(KademliaEvent::GetRecordPartialResult { query_id, record })) => {
				log::trace!(
					target: LOG_TARGET,
					"`GET_RECORD` intermediary succeeded for {query_id:?}: {record:?}",
				);

				return Poll::Ready(Some(DiscoveryEvent::GetRecordPartialResult {
					query_id,
					record,
				}));
			},
			Poll::Ready(Some(KademliaEvent::PutRecordSuccess { query_id, key: _ })) => {
				return Poll::Ready(Some(DiscoveryEvent::PutRecordSuccess { query_id }))
			},
			Poll::Ready(Some(KademliaEvent::QueryFailed { query_id })) => {
				match this.random_walk_query_id == Some(query_id) {
					true => {
						this.random_walk_query_id = None;
						this.duration_to_next_find_query =
							cmp::min(this.duration_to_next_find_query * 2, Duration::from_secs(60));
						this.next_kad_query = Some(Delay::new(this.duration_to_next_find_query));
					},
					false => return Poll::Ready(Some(DiscoveryEvent::QueryFailed { query_id })),
				}
			},
			Poll::Ready(Some(KademliaEvent::IncomingRecord { record })) => {
				log::trace!(
					target: LOG_TARGET,
					"incoming `PUT_RECORD` request with key {:?} from publisher {:?}",
					record.key,
					record.publisher,
				);

				return Poll::Ready(Some(DiscoveryEvent::IncomingRecord { record }));
			},
			Poll::Ready(Some(KademliaEvent::GetProvidersSuccess {
				provided_key,
				providers,
				query_id,
			})) => {
				log::trace!(
					target: LOG_TARGET,
					"`GET_PROVIDERS` for {query_id:?} with {provided_key:?} yielded {providers:?}",
				);

				return Poll::Ready(Some(DiscoveryEvent::GetProvidersSuccess {
					query_id,
					providers,
				}));
			},
			Poll::Ready(Some(KademliaEvent::AddProviderSuccess { query_id, provided_key })) => {
				log::trace!(
					target: LOG_TARGET,
					"`ADD_PROVIDER` for {query_id:?} with {provided_key:?} succeeded",
				);

				return Poll::Ready(Some(DiscoveryEvent::AddProviderSuccess {
					query_id,
					provided_key,
				}));
			},
			// We do not validate incoming providers.
			Poll::Ready(Some(KademliaEvent::IncomingProvider { .. })) => {},
		}

		match Pin::new(&mut this.identify_event_stream).poll_next(cx) {
			Poll::Pending => {},
			Poll::Ready(None) => return Poll::Ready(None),
			Poll::Ready(Some(IdentifyEvent::PeerIdentified {
				peer,
				listen_addresses,
				supported_protocols,
				observed_address,
				..
			})) => {
				let observed_address =
					if let Some(Protocol::P2p(peer_id)) = observed_address.iter().last() {
						if peer_id != this.local_peer_id.into() {
							log::warn!(
								target: LOG_TARGET,
								"Discovered external address for a peer that is not us: {observed_address}",
							);
							None
						} else {
							Some(observed_address)
						}
					} else {
						Some(observed_address.with(Protocol::P2p(this.local_peer_id.into())))
					};

				// Ensure that an external address with a different peer ID does not have
				// side effects of evicting other external addresses via `ExternalAddressExpired`.
				if let Some(observed_address) = observed_address {
					let (is_new, expired_address) =
						this.is_new_external_address(&observed_address, peer);

					if let Some(expired_address) = expired_address {
						log::trace!(
							target: LOG_TARGET,
							"Removing expired external address expired={expired_address} is_new={is_new} observed={observed_address}",
						);

						this.pending_events.push_back(DiscoveryEvent::ExternalAddressExpired {
							address: expired_address,
						});
					}

					if is_new {
						this.pending_events.push_back(DiscoveryEvent::ExternalAddressDiscovered {
							address: observed_address.clone(),
						});
					}
				}

				return Poll::Ready(Some(DiscoveryEvent::Identified {
					peer,
					listen_addresses,
					supported_protocols,
				}));
			},
		}

		match Pin::new(&mut this.ping_event_stream).poll_next(cx) {
			Poll::Pending => {},
			Poll::Ready(None) => return Poll::Ready(None),
			Poll::Ready(Some(PingEvent::Ping { peer, ping })) => {
				return Poll::Ready(Some(DiscoveryEvent::Ping { peer, rtt: ping }))
			},
		}

		if let Some(ref mut mdns_event_stream) = &mut this.mdns_event_stream {
			match Pin::new(mdns_event_stream).poll_next(cx) {
				Poll::Pending => {},
				Poll::Ready(None) => return Poll::Ready(None),
				Poll::Ready(Some(MdnsEvent::Discovered(addresses))) => {
					return Poll::Ready(Some(DiscoveryEvent::Discovered { addresses }))
				},
			}
		}

		Poll::Pending
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use std::sync::atomic::AtomicU32;

	use crate::{
		config::ProtocolId,
		peer_store::{PeerStore, PeerStoreProvider},
	};
	use futures::{stream::FuturesUnordered, StreamExt};
	use sp_core::H256;
	use sp_tracing::tracing_subscriber;

	use litep2p::{
		config::ConfigBuilder as Litep2pConfigBuilder, transport::tcp::config::Config as TcpConfig,
		Litep2p,
	};

	#[tokio::test]
	async fn litep2p_discovery_works() {
		let _ = tracing_subscriber::fmt()
			.with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
			.try_init();

		let mut known_peers = HashMap::new();
		let genesis_hash = H256::from_low_u64_be(1);
		let fork_id = Some("test-fork-id");
		let protocol_id = ProtocolId::from("dot");

		// Build backends such that the first peer is known to all other peers.
		let backends = (0..10)
			.map(|i| {
				let keypair = litep2p::crypto::ed25519::Keypair::generate();
				let peer_id: PeerId = keypair.public().to_peer_id().into();

				let listen_addresses = Arc::new(RwLock::new(HashSet::new()));

				let peer_store = PeerStore::new(vec![], None);
				let peer_store_handle: Arc<dyn PeerStoreProvider> = Arc::new(peer_store.handle());

				let (discovery, ping_config, identify_config, kademlia_config, _mdns) =
					Discovery::new(
						peer_id,
						&NetworkConfiguration::new_local(),
						genesis_hash,
						fork_id,
						&protocol_id,
						known_peers.clone(),
						listen_addresses.clone(),
						peer_store_handle,
					);

				let config = Litep2pConfigBuilder::new()
					.with_keypair(keypair)
					.with_tcp(TcpConfig {
						listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
						..Default::default()
					})
					.with_libp2p_ping(ping_config)
					.with_libp2p_identify(identify_config)
					.with_libp2p_kademlia(kademlia_config)
					.build();

				let mut litep2p = Litep2p::new(config).unwrap();

				let addresses = litep2p.listen_addresses().cloned().collect::<Vec<_>>();
				// Propagate addresses to discovery.
				addresses.iter().for_each(|address| {
					listen_addresses.write().insert(address.clone());
				});

				// Except the first peer, all other peers know the first peer addresses.
				if i == 0 {
					log::info!(target: LOG_TARGET, "First peer is {peer_id:?} with addresses {addresses:?}");
					known_peers.insert(peer_id, addresses.clone());
				} else {
					let (peer, addresses) = known_peers.iter().next().unwrap();

					let result = litep2p.add_known_address(*peer, addresses.into_iter().cloned());

					log::info!(target: LOG_TARGET, "{peer_id:?}: Adding known peer {peer:?} with addresses {addresses:?} result={result:?}");

				}

				(peer_id, litep2p, discovery)
			})
			.collect::<Vec<_>>();

		let total_peers = backends.len() as u32;
		let remaining_peers =
			backends.iter().map(|(peer_id, _, _)| *peer_id).collect::<HashSet<_>>();

		let first_peer = *known_peers.iter().next().unwrap().0;

		// Each backend must discover the whole network.
		let mut futures = FuturesUnordered::new();
		let num_finished = Arc::new(AtomicU32::new(0));

		for (peer_id, mut litep2p, mut discovery) in backends {
			// Remove the local peer id from the set.
			let mut remaining_peers = remaining_peers.clone();
			remaining_peers.remove(&peer_id);

			let num_finished = num_finished.clone();

			let future = async move {
				log::info!(target: LOG_TARGET, "{peer_id:?} starting loop");

				if peer_id != first_peer {
					log::info!(target: LOG_TARGET, "{peer_id:?} dialing {first_peer:?}");
					litep2p.dial(&first_peer).await.unwrap();
				}

				loop {
					// We need to keep the network alive until all peers are discovered.
					if num_finished.load(std::sync::atomic::Ordering::Relaxed) == total_peers {
						log::info!(target: LOG_TARGET, "{peer_id:?} all peers discovered");
						break;
					}

					tokio::select! {
						// Drive litep2p backend forward.
						event = litep2p.next_event() => {
							log::info!(target: LOG_TARGET, "{peer_id:?} Litep2p event: {event:?}");
						},

						// Detect discovery events.
						event = discovery.next() => {
							match event.unwrap() {
								// We have discovered the peer via kademlia and established
								// a connection on the identify protocol.
								DiscoveryEvent::Identified { peer, .. } => {
									log::info!(target: LOG_TARGET, "{peer_id:?} Peer {peer} identified");

									remaining_peers.remove(&peer);

									if remaining_peers.is_empty() {
										log::info!(target: LOG_TARGET, "{peer_id:?} All peers discovered");

										num_finished.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
									}
								},

								event => {
									log::info!(target: LOG_TARGET, "{peer_id:?} Discovery event: {event:?}");
								}
							}
						}
					}
				}
			};

			futures.push(future);
		}

		// Futures will exit when all peers are discovered.
		tokio::time::timeout(Duration::from_secs(60), futures.next())
			.await
			.expect("All peers should finish within 60 seconds");
	}
}
