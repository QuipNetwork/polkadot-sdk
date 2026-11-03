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

//! Fuzz test emulates network events and peer connection handling by `Peerset`
//! and `PeerStore` to discover possible inconsistencies in peer management.

use crate::{
	litep2p::{
		peerstore::Peerstore,
		shim::notification::peerset::{OpenResult, Peerset, PeersetCommand},
	},
	service::{
		metrics::NotificationMetrics,
		traits::{Direction, PeerStore, ValidationResult},
	},
	ProtocolName,
};

use futures::{FutureExt, StreamExt};
use litep2p::protocol::notification::NotificationError;
use rand::{
	distributions::{Distribution, Uniform, WeightedIndex},
	seq::IteratorRandom,
};

use sc_network_common::types::ReputationChange;
use sc_network_types::PeerId;

use std::{
	collections::{HashMap, HashSet},
	sync::Arc,
};

#[tokio::test]
#[cfg(debug_assertions)]
async fn run() {
	sp_tracing::try_init_simple();

	for _ in 0..50 {
		test_once().await;
	}
}

#[cfg(debug_assertions)]
async fn test_once() {
	// PRNG to use.
	let mut rng = rand::thread_rng();

	// peers that the peerset knows about.
	let mut known_peers = HashSet::<PeerId>::new();

	// peers that we have reserved. Always a subset of `known_peers`.
	let mut reserved_peers = HashSet::<PeerId>::new();

	// reserved only mode
	let mut reserved_only = Uniform::new_inclusive(0, 10).sample(&mut rng) == 0;

	// Bootnodes for `PeerStore` initialization.
	let bootnodes = (0..Uniform::new_inclusive(0, 4).sample(&mut rng))
		.map(|_| {
			let id = PeerId::random();
			known_peers.insert(id);
			id
		})
		.collect();

	let peerstore = Peerstore::new(bootnodes, None);
	let peer_store_handle = peerstore.handle();

	let (mut peerset, to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		Uniform::new_inclusive(0, 25).sample(&mut rng),
		Uniform::new_inclusive(0, 25).sample(&mut rng),
		reserved_only,
		(0..Uniform::new_inclusive(0, 2).sample(&mut rng))
			.map(|_| {
				let id = PeerId::random();
				known_peers.insert(id);
				reserved_peers.insert(id);
				id
			})
			.collect(),
		Default::default(),
		Arc::clone(&peer_store_handle),
		NotificationMetrics::new(None),
	);

	tokio::spawn(peerstore.run());

	// opening substreams
	let mut opening = HashMap::<PeerId, Direction>::new();

	// open substreams
	let mut open = HashMap::<PeerId, Direction>::new();

	// closing substreams
	let mut closing = HashSet::<PeerId>::new();

	// closed substreams
	let mut closed = HashSet::<PeerId>::new();

	// perform a certain number of actions while checking that the state is consistent.
	//
	// if we reach the end of the loop, the run has succeeded
	let _ = tokio::task::spawn_blocking(move || {
		// PRNG to use in `spawn_blocking` context.
		let mut rng = rand::thread_rng();

		for _ in 0..2500 {
			// each of these weights corresponds to an action that we may perform
			let action_weights =
				[300, 110, 110, 110, 110, 90, 70, 30, 110, 110, 110, 110, 20, 110, 50, 110];

			match WeightedIndex::new(&action_weights).unwrap().sample(&mut rng) {
				0 => match peerset.next().now_or_never() {
					Some(Some(command)) => {
						// open substreams to `peers`
						for peer in command.open_peers {
							opening.insert(peer, Direction::Outbound);
							closed.remove(&peer);

							assert!(!closing.contains(&peer));
							assert!(!open.contains_key(&peer));
						}

						// close substreams to `peers`
						for peer in command.close_peers {
							assert!(closing.insert(peer));
							assert!(open.remove(&peer).is_some());
							assert!(!opening.contains_key(&peer));
						}
					},
					Some(None) => panic!("peerset exited"),
					None => {},
				},
				// get inbound connection from an unknown peer
				1 => {
					let new_peer = PeerId::random();
					peer_store_handle.add_known_peer(new_peer);

					match peerset.report_inbound_substream(new_peer) {
						ValidationResult::Accept => {
							opening.insert(new_peer, Direction::Inbound);
						},
						ValidationResult::Reject => {},
					}
				},
				// substream opened successfully
				//
				// remove peer from `opening` (which contains its direction), report the open
				// substream to `Peerset` and move peer state to `open`.
				//
				// if the substream was canceled while it was opening, move peer to `closing`
				2 => {
					if let Some(peer) = opening.keys().choose(&mut rng).copied() {
						let direction = opening.remove(&peer).unwrap();
						match peerset.report_substream_opened(peer, direction) {
							OpenResult::Accept { .. } => {
								assert!(open.insert(peer, direction).is_none());
							},
							OpenResult::Reject => {
								assert!(closing.insert(peer));
							},
						}
					}
				},
				// substream failed to open
				3 => {
					if let Some(peer) = opening.keys().choose(&mut rng).copied() {
						let _ = opening.remove(&peer).unwrap();
						peerset.report_substream_open_failure(peer, NotificationError::Rejected);
					}
				},
				// substream was closed by remote peer
				4 => {
					if let Some(peer) = open.keys().choose(&mut rng).copied() {
						let _ = open.remove(&peer).unwrap();
						peerset.report_substream_closed(peer);
						assert!(closed.insert(peer));
					}
				},
				// substream was closed by local node
				5 => {
					if let Some(peer) = closing.iter().choose(&mut rng).copied() {
						assert!(closing.remove(&peer));
						assert!(closed.insert(peer));
						peerset.report_substream_closed(peer);
					}
				},
				// random connected peer was disconnected by the protocol
				6 => {
					if let Some(peer) = open.keys().choose(&mut rng).copied() {
						to_peerset.unbounded_send(PeersetCommand::DisconnectPeer { peer }).unwrap();
					}
				},
				// ban random peer
				7 => {
					if let Some(peer) = known_peers.iter().choose(&mut rng).copied() {
						peer_store_handle.report_peer(peer, ReputationChange::new_fatal(""));
					}
				},
				// inbound substream is received for a peer that was considered
				// outbound
				8 => {
					let outbound_peers = opening
						.iter()
						.filter_map(|(peer, direction)| {
							std::matches!(direction, Direction::Outbound).then_some(*peer)
						})
						.collect::<HashSet<_>>();

					if let Some(peer) = outbound_peers.iter().choose(&mut rng).copied() {
						match peerset.report_inbound_substream(peer) {
							ValidationResult::Accept => {
								opening.insert(peer, Direction::Inbound);
							},
							ValidationResult::Reject => {},
						}
					}
				},
				// set reserved peers
				//
				// choose peers from all available sets (open, opening, closing, closed) + some new
				// peers
				9 => {
					let num_open = Uniform::new_inclusive(0, open.len()).sample(&mut rng);
					let num_opening = Uniform::new_inclusive(0, opening.len()).sample(&mut rng);
					let num_closing = Uniform::new_inclusive(0, closing.len()).sample(&mut rng);
					let num_closed = Uniform::new_inclusive(0, closed.len()).sample(&mut rng);

					let peers = open
						.keys()
						.copied()
						.choose_multiple(&mut rng, num_open)
						.into_iter()
						.chain(
							opening
								.keys()
								.copied()
								.choose_multiple(&mut rng, num_opening)
								.into_iter(),
						)
						.chain(
							closing
								.iter()
								.copied()
								.choose_multiple(&mut rng, num_closing)
								.into_iter(),
						)
						.chain(
							closed
								.iter()
								.copied()
								.choose_multiple(&mut rng, num_closed)
								.into_iter(),
						)
						.chain((0..5).map(|_| {
							let peer = PeerId::random();
							known_peers.insert(peer);
							peer_store_handle.add_known_peer(peer);
							peer
						}))
						.filter(|peer| !reserved_peers.contains(peer))
						.collect::<HashSet<_>>();

					reserved_peers.extend(peers.clone().into_iter());
					to_peerset.unbounded_send(PeersetCommand::SetReservedPeers { peers }).unwrap();
				},
				// add reserved peers
				10 => {
					let num_open = Uniform::new_inclusive(0, open.len()).sample(&mut rng);
					let num_opening = Uniform::new_inclusive(0, opening.len()).sample(&mut rng);
					let num_closing = Uniform::new_inclusive(0, closing.len()).sample(&mut rng);
					let num_closed = Uniform::new_inclusive(0, closed.len()).sample(&mut rng);

					let peers = open
						.keys()
						.copied()
						.choose_multiple(&mut rng, num_open)
						.into_iter()
						.chain(
							opening
								.keys()
								.copied()
								.choose_multiple(&mut rng, num_opening)
								.into_iter(),
						)
						.chain(
							closing
								.iter()
								.copied()
								.choose_multiple(&mut rng, num_closing)
								.into_iter(),
						)
						.chain(
							closed
								.iter()
								.copied()
								.choose_multiple(&mut rng, num_closed)
								.into_iter(),
						)
						.chain((0..5).map(|_| {
							let peer = PeerId::random();
							known_peers.insert(peer);
							peer_store_handle.add_known_peer(peer);
							peer
						}))
						.filter(|peer| !reserved_peers.contains(peer))
						.collect::<HashSet<_>>();

					reserved_peers.extend(peers.clone().into_iter());
					to_peerset.unbounded_send(PeersetCommand::AddReservedPeers { peers }).unwrap();
				},
				// remove reserved peers
				11 => {
					let num_to_remove =
						Uniform::new_inclusive(0, reserved_peers.len()).sample(&mut rng);
					let peers = reserved_peers
						.iter()
						.copied()
						.choose_multiple(&mut rng, num_to_remove)
						.into_iter()
						.collect::<HashSet<_>>();

					peers.iter().for_each(|peer| {
						assert!(reserved_peers.remove(peer));
					});

					to_peerset
						.unbounded_send(PeersetCommand::RemoveReservedPeers { peers })
						.unwrap();
				},
				// set reserved only
				12 => {
					reserved_only = !reserved_only;

					let _ = to_peerset
						.unbounded_send(PeersetCommand::SetReservedOnly { reserved_only });
				},
				// discover a new node.
				13 => {
					let new_peer = PeerId::random();
					known_peers.insert(new_peer);
					peer_store_handle.add_known_peer(new_peer);
				},
				// protocol rejected a substream that was accepted by `Peerset`
				14 => {
					let inbound_peers = opening
						.iter()
						.filter_map(|(peer, direction)| {
							std::matches!(direction, Direction::Inbound).then_some(*peer)
						})
						.collect::<HashSet<_>>();

					if let Some(peer) = inbound_peers.iter().choose(&mut rng).copied() {
						peerset.report_substream_rejected(peer);
						opening.remove(&peer);
					}
				},
				// inbound substream received for a peer in `closed`
				15 => {
					if let Some(peer) = closed.iter().choose(&mut rng).copied() {
						match peerset.report_inbound_substream(peer) {
							ValidationResult::Accept => {
								assert!(closed.remove(&peer));
								opening.insert(peer, Direction::Inbound);
							},
							ValidationResult::Reject => {},
						}
					}
				},
				_ => unreachable!(),
			}
		}
	})
	.await
	.unwrap();
}
