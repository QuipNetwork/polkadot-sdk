// This file is part of Substrate

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

//! Implementation of indexed transaction publishing to IPFS Kademlia DHT.

use crate::{
	config::MultiaddrWithPeerId,
	ipfs_block_provider::{BlockProvider, Change},
};
use futures::StreamExt;
use litep2p::{
	protocol::libp2p::kademlia::{
		Config, ConfigBuilder, KademliaEvent, KademliaHandle, Quorum, RecordKey,
	},
	types::multiaddr::Multiaddr,
	PeerId,
};
use log::{debug, trace};
use sp_core::hexdisplay::HexDisplay;
use std::{
	collections::{HashMap, HashSet},
	num::NonZeroUsize,
	time::Duration,
};
use tokio::time::MissedTickBehavior;

/// Log target for this file.
const LOG_TARGET: &str = "sub-libp2p::ipfs::dht";

/// IPFS Kademlia protocol name.
const KAD_PROTOCOL: &str = "/ipfs/kad/1.0.0";

/// Quorum to treat publishing successful. Note that litep2p tries to publish a provider to all
/// target peers and does not terminate the query once the quorum is reached.
const QUORUM: Quorum = Quorum::N(NonZeroUsize::new(10).expect("10 > 0; qed"));

/// Maximum number of provider keys in the local Kademlia memory store. Set to approximately 5x
/// number of blocks with indexed transactions kept + average number of incoming IPFS provider
/// records.
///
/// As of November 2025 there are 6.5k reachable IPFS peers with maximum 250M CIDs. This means on
/// average 250M / (6.5k / 20) ~= 800k CIDs per peer.
const MAX_PROVIDER_KEYS: usize = 2_000_000;

/// Raw codec type.
// TODO: index codec along with transaction data and use it instead of the hardcoded one.
const RAW_CODEC: u64 = 0x55;

/// Interval of Kademlia random walks. Needed to keep the routing table "warm".
const RANDOM_WALK_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Maximum allowed number of in-flight Kademlia queries. We need this limit for the networking
/// stack to not get overwhelmed by unlimited amount of simultaneous `ADD_PROVIDER` queries.
///
/// For a maximum 200-second `ADD_PROVIDER` query duration and 100 in-flight queries we have on
/// average 2 seconds spent on publishing a single provider or 1/3 of the block time. This means
/// in practice we will spend about one week catching up on transactions in the past two weeks,
/// before starting publishing new transactions, if we have on average one indexed transaction
/// per block. So, one transaction per block is a practical limit on the number of indexed
/// transactions published to IPFS.
const MAX_INFLIGHT_QUERIES: usize = 100;

/// CID with allocated size of 32 bytes for hash value.
type Cid = cid::CidGeneric<32>;

pub(crate) struct IpfsDht {
	kademlia_handle: KademliaHandle,
	block_provider: Box<dyn BlockProvider>,
}

impl IpfsDht {
	pub fn new(
		bootnodes: Vec<MultiaddrWithPeerId>,
		block_provider: Box<dyn BlockProvider>,
	) -> (Self, Config) {
		let known_peers = {
			// Convert bootnodes into expected format deduplicating the addresses.
			let mut known_peers: HashMap<PeerId, HashSet<Multiaddr>> = HashMap::new();

			for address in bootnodes {
				let peer_id = address.peer_id.into();
				let multiaddr = address.concat().into();

				known_peers.entry(peer_id).or_default().insert(multiaddr);
			}

			known_peers.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect()
		};

		let (config, kademlia_handle) = ConfigBuilder::new()
			.with_protocol_names(vec![KAD_PROTOCOL.into()])
			.with_known_peers(known_peers)
			.with_max_provider_keys(MAX_PROVIDER_KEYS)
			.build();

		(Self { kademlia_handle, block_provider }, config)
	}

	pub async fn run(mut self) {
		let mut changes = self.block_provider.changes();
		let mut inflight_queries = HashMap::new();
		let mut random_walk_query = None;
		let mut random_walk_interval = tokio::time::interval(RANDOM_WALK_INTERVAL);
		random_walk_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

		loop {
			tokio::select! {
				change = changes.next(), if inflight_queries.len() < MAX_INFLIGHT_QUERIES => {
					match change {
						None => {
							debug!(target: LOG_TARGET, "BlockProvider terminated, terminating IpfsDht");
							return
						},
						Some(Change::Added(multihash)) => {
							let key = RecordKey::new(&multihash.to_bytes());

							trace!(
								target: LOG_TARGET,
								"IPFS DHT start providing key: {}, CID: {}",
								HexDisplay::from(&key.as_ref()),
								Cid::new_v1(RAW_CODEC, multihash),
							);

							let query_id = self.kademlia_handle.start_providing(key, QUORUM).await;
							inflight_queries.insert(query_id, multihash);
						},
						Some(Change::Removed(multihash)) => {
							let key = RecordKey::new(&multihash.to_bytes());

							trace!(
								target: LOG_TARGET,
								"IPFS DHT stop providing key: {}, CID: {}",
								HexDisplay::from(&key.as_ref()),
								Cid::new_v1(RAW_CODEC, multihash),
							);

							self.kademlia_handle.stop_providing(key).await;
						},
					}
				},
				_ = random_walk_interval.tick() => {
					if random_walk_query.is_some() {
						// Do not start a new random walk if the previous one hasn't finished.
						continue;
					}

					random_walk_query = Some(self.kademlia_handle.find_node(PeerId::random()).await);
				},
				event = self.kademlia_handle.next() => {
					match event {
						None => {
							debug!(target: LOG_TARGET, "IPFS Kademlia terminated, terminating IpfsDht");
							return
						}
						Some(KademliaEvent::AddProviderSuccess { query_id, provided_key }) => {
							if let Some(multihash) = inflight_queries.remove(&query_id) {
								trace!(
									target: LOG_TARGET,
									"IPFS DHT provider publish success, key: {}, CID: {}",
									HexDisplay::from(&provided_key.as_ref()),
									Cid::new_v1(RAW_CODEC, multihash),
								);
							} else {
								trace!(
									target: LOG_TARGET,
									"IPFS DHT provider refresh success, key: {}",
									HexDisplay::from(&provided_key.as_ref()),
								);
							}
						},
						Some(KademliaEvent::FindNodeSuccess { query_id, peers, .. }) => {
							debug_assert_eq!(Some(query_id), random_walk_query);
							trace!(target: LOG_TARGET, "DHT random walk yielded {} peers", peers.len());

							random_walk_query = None;
						},
						Some(KademliaEvent::QueryFailed { query_id }) => {
							if Some(query_id) == random_walk_query {
								trace!(target: LOG_TARGET, "DHT random walk failed");

								random_walk_query = None;
							} else if let Some(multihash) = inflight_queries.remove(&query_id) {
								trace!(
									target: LOG_TARGET,
									"IPFS DHT provider publish failed, key: {}, CID: {}",
									HexDisplay::from(&multihash.to_bytes()),
									Cid::new_v1(RAW_CODEC, multihash),
								);
							} else {
								trace!(
									target: LOG_TARGET,
									"IPFS DHT provider refresh failed",
								);
							}
						},
						// We are not interested in other events.
						Some(_) => {},
					}
				}
			}
		}
	}
}
