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

//! `NetworkService` implementation for `litep2p`.

use crate::{
	config::MultiaddrWithPeerId,
	litep2p::shim::{
		notification::{config::ProtocolControlHandle, peerset::PeersetCommand},
		request_response::OutboundRequest,
	},
	network_state::NetworkState,
	peer_store::PeerStoreProvider,
	service::out_events,
	Event, IfDisconnected, NetworkDHTProvider, NetworkEventStream, NetworkPeers, NetworkRequest,
	NetworkSigner, NetworkStateInfo, NetworkStatus, NetworkStatusProvider, OutboundFailure,
	ProtocolName, RequestFailure, Signature,
};

use codec::DecodeAll;
use futures::{channel::oneshot, stream::BoxStream};
use libp2p::identity::SigningError;
use litep2p::{
	addresses::PublicAddresses, crypto::ed25519::Keypair,
	types::multiaddr::Multiaddr as LiteP2pMultiaddr,
};
use parking_lot::RwLock;
use sc_network_types::kad::{Key as KademliaKey, Record};

use sc_network_common::{
	role::{ObservedRole, Roles},
	types::ReputationChange,
};
use sc_network_types::{
	multiaddr::{Multiaddr, Protocol},
	PeerId,
};
use sc_utils::mpsc::TracingUnboundedSender;

use std::{
	collections::{HashMap, HashSet},
	sync::{atomic::Ordering, Arc},
	time::Instant,
};

/// Logging target for the file.
const LOG_TARGET: &str = "sub-libp2p";

/// Commands sent by [`Litep2pNetworkService`] to
/// [`Litep2pNetworkBackend`](super::Litep2pNetworkBackend).
#[derive(Debug)]
pub enum NetworkServiceCommand {
	/// Find peers closest to `target` in the DHT.
	FindClosestPeers {
		/// Target peer ID.
		target: PeerId,
	},

	/// Get value from DHT.
	GetValue {
		/// Record key.
		key: KademliaKey,
	},

	/// Put value to DHT.
	PutValue {
		/// Record key.
		key: KademliaKey,

		/// Record value.
		value: Vec<u8>,
	},

	/// Put value to DHT.
	PutValueTo {
		/// Record.
		record: Record,
		/// Peers we want to put the record.
		peers: Vec<sc_network_types::PeerId>,
		/// If we should update the local storage or not.
		update_local_storage: bool,
	},
	/// Store record in the local DHT store.
	StoreRecord {
		/// Record key.
		key: KademliaKey,

		/// Record value.
		value: Vec<u8>,

		/// Original publisher of the record.
		publisher: Option<PeerId>,

		/// Record expiration time as measured by a local, monothonic clock.
		expires: Option<Instant>,
	},

	/// Start providing `key`.
	StartProviding { key: KademliaKey },

	/// Stop providing `key`.
	StopProviding { key: KademliaKey },

	/// Get providers for `key`.
	GetProviders { key: KademliaKey },

	/// Query network status.
	Status {
		/// `oneshot::Sender` for sending the status.
		tx: oneshot::Sender<NetworkStatus>,
	},

	/// Add `peers` to `protocol`'s reserved set.
	AddPeersToReservedSet {
		/// Protocol.
		protocol: ProtocolName,

		/// Reserved peers.
		peers: HashSet<Multiaddr>,
	},

	/// Add known address for peer.
	AddKnownAddress {
		/// Peer ID.
		peer: PeerId,

		/// Address.
		address: Multiaddr,
	},

	/// Set reserved peers for `protocol`.
	SetReservedPeers {
		/// Protocol.
		protocol: ProtocolName,

		/// Reserved peers.
		peers: HashSet<Multiaddr>,
	},

	/// Disconnect peer from protocol.
	DisconnectPeer {
		/// Protocol.
		protocol: ProtocolName,

		/// Peer ID.
		peer: PeerId,
	},

	/// Set protocol to reserved only (true/false) mode.
	SetReservedOnly {
		/// Protocol.
		protocol: ProtocolName,

		/// Reserved only?
		reserved_only: bool,
	},

	/// Remove reserved peers from protocol.
	RemoveReservedPeers {
		/// Protocol.
		protocol: ProtocolName,

		/// Peers to remove from the reserved set.
		peers: HashSet<PeerId>,
	},

	/// Create event stream for DHT events.
	EventStream {
		/// Sender for the events.
		tx: out_events::Sender,
	},
}

/// `NetworkService` implementation for `litep2p`.
#[derive(Debug, Clone)]
pub struct Litep2pNetworkService {
	/// Local peer ID.
	local_peer_id: litep2p::PeerId,

	/// The `KeyPair` that defines the `PeerId` of the local node.
	keypair: Keypair,

	/// TX channel for sending commands to [`Litep2pNetworkBackend`](super::Litep2pNetworkBackend).
	cmd_tx: TracingUnboundedSender<NetworkServiceCommand>,

	/// Handle to `PeerStore`.
	peer_store_handle: Arc<dyn PeerStoreProvider>,

	/// Peerset handles.
	peerset_handles: HashMap<ProtocolName, ProtocolControlHandle>,

	/// Name for the block announce protocol.
	block_announce_protocol: ProtocolName,

	/// Installed request-response protocols.
	request_response_protocols: HashMap<ProtocolName, TracingUnboundedSender<OutboundRequest>>,

	/// Listen addresses.
	listen_addresses: Arc<RwLock<HashSet<LiteP2pMultiaddr>>>,

	/// External addresses.
	external_addresses: PublicAddresses,
}

impl Litep2pNetworkService {
	/// Create new [`Litep2pNetworkService`].
	pub(crate) fn new(
		local_peer_id: litep2p::PeerId,
		keypair: Keypair,
		cmd_tx: TracingUnboundedSender<NetworkServiceCommand>,
		peer_store_handle: Arc<dyn PeerStoreProvider>,
		peerset_handles: HashMap<ProtocolName, ProtocolControlHandle>,
		block_announce_protocol: ProtocolName,
		request_response_protocols: HashMap<ProtocolName, TracingUnboundedSender<OutboundRequest>>,
		listen_addresses: Arc<RwLock<HashSet<LiteP2pMultiaddr>>>,
		external_addresses: PublicAddresses,
	) -> Self {
		Self {
			local_peer_id,
			keypair,
			cmd_tx,
			peer_store_handle,
			peerset_handles,
			block_announce_protocol,
			request_response_protocols,
			listen_addresses,
			external_addresses,
		}
	}
}

impl NetworkSigner for Litep2pNetworkService {
	fn sign_with_local_identity(&self, msg: Vec<u8>) -> Result<Signature, SigningError> {
		let public_key = self.keypair.public();
		let bytes = self.keypair.sign(msg.as_ref());

		Ok(Signature {
			public_key: crate::service::signature::PublicKey::Litep2p(
				litep2p::crypto::PublicKey::Ed25519(public_key),
			),
			bytes,
		})
	}

	fn verify(
		&self,
		peer: PeerId,
		public_key: &Vec<u8>,
		signature: &Vec<u8>,
		message: &Vec<u8>,
	) -> Result<bool, String> {
		let identity = litep2p::PeerId::from_public_key_protobuf(&public_key);
		let public_key = litep2p::crypto::RemotePublicKey::from_protobuf_encoding(&public_key)
			.map_err(|error| error.to_string())?;
		let peer: litep2p::PeerId = peer.into();

		Ok(peer == identity && public_key.verify(message, signature))
	}
}

impl NetworkDHTProvider for Litep2pNetworkService {
	fn find_closest_peers(&self, target: PeerId) {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::FindClosestPeers { target });
	}

	fn get_value(&self, key: &KademliaKey) {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::GetValue { key: key.clone() });
	}

	fn put_value(&self, key: KademliaKey, value: Vec<u8>) {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::PutValue { key, value });
	}

	fn put_record_to(&self, record: Record, peers: HashSet<PeerId>, update_local_storage: bool) {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::PutValueTo {
			record: Record {
				key: record.key.to_vec().into(),
				value: record.value,
				publisher: record.publisher.map(|peer_id| {
					let peer_id: sc_network_types::PeerId = peer_id.into();
					peer_id.into()
				}),
				expires: record.expires,
			},
			peers: peers.into_iter().collect(),
			update_local_storage,
		});
	}

	fn store_record(
		&self,
		key: KademliaKey,
		value: Vec<u8>,
		publisher: Option<PeerId>,
		expires: Option<Instant>,
	) {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::StoreRecord {
			key,
			value,
			publisher,
			expires,
		});
	}

	fn start_providing(&self, key: KademliaKey) {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::StartProviding { key });
	}

	fn stop_providing(&self, key: KademliaKey) {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::StopProviding { key });
	}

	fn get_providers(&self, key: KademliaKey) {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::GetProviders { key });
	}
}

#[async_trait::async_trait]
impl NetworkStatusProvider for Litep2pNetworkService {
	async fn status(&self) -> Result<NetworkStatus, ()> {
		let (tx, rx) = oneshot::channel();
		self.cmd_tx
			.unbounded_send(NetworkServiceCommand::Status { tx })
			.map_err(|_| ())?;

		rx.await.map_err(|_| ())
	}

	async fn network_state(&self) -> Result<NetworkState, ()> {
		Ok(NetworkState {
			peer_id: self.local_peer_id.to_base58(),
			listened_addresses: self
				.listen_addresses
				.read()
				.iter()
				.cloned()
				.map(|a| Multiaddr::from(a).into())
				.collect(),
			external_addresses: self
				.external_addresses
				.get_addresses()
				.into_iter()
				.map(|a| Multiaddr::from(a).into())
				.collect(),
			connected_peers: HashMap::new(),
			not_connected_peers: HashMap::new(),
			// TODO: Check what info we can include here.
			//       Issue reference: https://github.com/paritytech/substrate/issues/14160.
			peerset: serde_json::json!(
				"Unimplemented. See https://github.com/paritytech/substrate/issues/14160."
			),
		})
	}
}

// Manual implementation to avoid extra boxing here
// TODO: functions modifying peerset state could be modified to call peerset directly if the
// `Multiaddr` only contains a `PeerId`
#[async_trait::async_trait]
impl NetworkPeers for Litep2pNetworkService {
	fn set_authorized_peers(&self, peers: HashSet<PeerId>) {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::SetReservedPeers {
			protocol: self.block_announce_protocol.clone(),
			peers: peers
				.into_iter()
				.map(|peer| Multiaddr::empty().with(Protocol::P2p(peer.into())))
				.collect(),
		});
	}

	fn set_authorized_only(&self, reserved_only: bool) {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::SetReservedOnly {
			protocol: self.block_announce_protocol.clone(),
			reserved_only,
		});
	}

	fn add_known_address(&self, peer: PeerId, address: Multiaddr) {
		let _ = self
			.cmd_tx
			.unbounded_send(NetworkServiceCommand::AddKnownAddress { peer, address });
	}

	fn peer_reputation(&self, peer_id: &PeerId) -> i32 {
		self.peer_store_handle.peer_reputation(peer_id)
	}

	fn report_peer(&self, peer: PeerId, cost_benefit: ReputationChange) {
		self.peer_store_handle.report_peer(peer, cost_benefit);
	}

	fn disconnect_peer(&self, peer: PeerId, protocol: ProtocolName) {
		let _ = self
			.cmd_tx
			.unbounded_send(NetworkServiceCommand::DisconnectPeer { protocol, peer });
	}

	fn accept_unreserved_peers(&self) {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::SetReservedOnly {
			protocol: self.block_announce_protocol.clone(),
			reserved_only: false,
		});
	}

	fn deny_unreserved_peers(&self) {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::SetReservedOnly {
			protocol: self.block_announce_protocol.clone(),
			reserved_only: true,
		});
	}

	fn add_reserved_peer(&self, peer: MultiaddrWithPeerId) -> Result<(), String> {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::AddPeersToReservedSet {
			protocol: self.block_announce_protocol.clone(),
			peers: HashSet::from_iter([peer.concat().into()]),
		});

		Ok(())
	}

	fn remove_reserved_peer(&self, peer: PeerId) {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::RemoveReservedPeers {
			protocol: self.block_announce_protocol.clone(),
			peers: HashSet::from_iter([peer]),
		});
	}

	fn set_reserved_peers(
		&self,
		protocol: ProtocolName,
		peers: HashSet<Multiaddr>,
	) -> Result<(), String> {
		let _ = self
			.cmd_tx
			.unbounded_send(NetworkServiceCommand::SetReservedPeers { protocol, peers });
		Ok(())
	}

	fn add_peers_to_reserved_set(
		&self,
		protocol: ProtocolName,
		peers: HashSet<Multiaddr>,
	) -> Result<(), String> {
		let _ = self
			.cmd_tx
			.unbounded_send(NetworkServiceCommand::AddPeersToReservedSet { protocol, peers });
		Ok(())
	}

	fn remove_peers_from_reserved_set(
		&self,
		protocol: ProtocolName,
		peers: Vec<PeerId>,
	) -> Result<(), String> {
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::RemoveReservedPeers {
			protocol,
			peers: peers.into_iter().map(From::from).collect(),
		});

		Ok(())
	}

	fn sync_num_connected(&self) -> usize {
		self.peerset_handles
			.get(&self.block_announce_protocol)
			.map_or(0usize, |handle| handle.connected_peers.load(Ordering::Relaxed))
	}

	fn peer_role(&self, peer: PeerId, handshake: Vec<u8>) -> Option<ObservedRole> {
		match Roles::decode_all(&mut &handshake[..]) {
			Ok(role) => Some(role.into()),
			Err(_) => {
				log::debug!(target: LOG_TARGET, "handshake doesn't contain peer role: {handshake:?}");
				self.peer_store_handle.peer_role(&(peer.into()))
			},
		}
	}

	/// Get the list of reserved peers.
	///
	/// Returns an error if the `NetworkWorker` is no longer running.
	async fn reserved_peers(&self) -> Result<Vec<PeerId>, ()> {
		let Some(handle) = self.peerset_handles.get(&self.block_announce_protocol) else {
			return Err(());
		};
		let (tx, rx) = oneshot::channel();

		handle
			.tx
			.unbounded_send(PeersetCommand::GetReservedPeers { tx })
			.map_err(|_| ())?;

		// the channel can only be closed if `Peerset` no longer exists
		rx.await.map_err(|_| ())
	}
}

impl NetworkEventStream for Litep2pNetworkService {
	fn event_stream(&self, stream_name: &'static str) -> BoxStream<'static, Event> {
		let (tx, rx) = out_events::channel(stream_name, 100_000);
		let _ = self.cmd_tx.unbounded_send(NetworkServiceCommand::EventStream { tx });
		Box::pin(rx)
	}
}

impl NetworkStateInfo for Litep2pNetworkService {
	fn external_addresses(&self) -> Vec<Multiaddr> {
		self.external_addresses.get_addresses().into_iter().map(Into::into).collect()
	}

	fn listen_addresses(&self) -> Vec<Multiaddr> {
		self.listen_addresses.read().iter().cloned().map(Into::into).collect()
	}

	fn local_peer_id(&self) -> PeerId {
		self.local_peer_id.into()
	}
}

// Manual implementation to avoid extra boxing here
#[async_trait::async_trait]
impl NetworkRequest for Litep2pNetworkService {
	async fn request(
		&self,
		target: PeerId,
		protocol: ProtocolName,
		request: Vec<u8>,
		fallback_request: Option<(Vec<u8>, ProtocolName)>,
		connect: IfDisconnected,
	) -> Result<(Vec<u8>, ProtocolName), RequestFailure> {
		let (tx, rx) = oneshot::channel();

		self.start_request(target, protocol, request, fallback_request, tx, connect);

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
		peer: PeerId,
		protocol: ProtocolName,
		request: Vec<u8>,
		fallback_request: Option<(Vec<u8>, ProtocolName)>,
		sender: oneshot::Sender<Result<(Vec<u8>, ProtocolName), RequestFailure>>,
		connect: IfDisconnected,
	) {
		match self.request_response_protocols.get(&protocol) {
			Some(tx) => {
				let _ = tx.unbounded_send(OutboundRequest::new(
					peer,
					request,
					sender,
					fallback_request,
					connect,
				));
			},
			None => log::warn!(
				target: LOG_TARGET,
				"{protocol} doesn't exist, cannot send request to {peer:?}"
			),
		}
	}
}
