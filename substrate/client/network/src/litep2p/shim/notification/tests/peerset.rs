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

use crate::{
	litep2p::{
		peerstore::peerstore_handle_test,
		shim::notification::peerset::{
			Direction, OpenResult, PeerState, Peerset, PeersetCommand, Reserved,
		},
	},
	service::{
		metrics::NotificationMetrics,
		traits::{self, ValidationResult},
	},
	ProtocolName,
};

use futures::prelude::*;
use litep2p::protocol::notification::NotificationError;

use sc_network_types::PeerId;

use std::{
	collections::HashSet,
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	},
	task::Poll,
};

// outbound substream was initiated for a peer but an inbound substream from that same peer
// was receied while the `Peerset` was waiting for the outbound substream to be opened
//
// verify that the peer state is updated correctly
#[tokio::test]
async fn inbound_substream_for_outbound_peer() {
	let peerstore_handle = Arc::new(peerstore_handle_test());
	let peers = (0..3)
		.map(|_| {
			let peer = PeerId::random();
			peerstore_handle.add_known_peer(peer);
			peer
		})
		.collect::<Vec<_>>();
	let inbound_peer = *peers.iter().next().unwrap();

	let (mut peerset, _to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		25,
		25,
		false,
		Default::default(),
		Default::default(),
		peerstore_handle,
		NotificationMetrics::new(None),
	);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);

	match peerset.next().await {
		Some(command) => {
			assert!(command.close_peers.is_empty());
			let out_peers = command.open_peers;

			assert_eq!(out_peers.len(), 3usize);
			assert_eq!(peerset.num_in(), 0usize);
			assert_eq!(peerset.num_out(), 3usize);
			assert_eq!(
				peerset.peers().get(&inbound_peer),
				Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::No) })
			);
		},
		event => panic!("invalid event: {event:?}"),
	}

	// inbound substream was received from peer who was marked outbound
	//
	// verify that the peer state and inbound/outbound counts are updated correctly
	assert_eq!(peerset.report_inbound_substream(inbound_peer), ValidationResult::Accept);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 3usize);
	assert_eq!(
		peerset.peers().get(&inbound_peer),
		Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::No) })
	);
}

// substream was opening to peer but then it was canceled and before the substream
// was fully closed, the peer got banned
#[tokio::test]
async fn canceled_peer_gets_banned() {
	sp_tracing::try_init_simple();

	let peerstore_handle = Arc::new(peerstore_handle_test());
	let peers = HashSet::from_iter([PeerId::random(), PeerId::random(), PeerId::random()]);

	let (mut peerset, to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		0,
		0,
		true,
		peers.clone(),
		Default::default(),
		peerstore_handle,
		NotificationMetrics::new(None),
	);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);

	match peerset.next().await {
		Some(command) => {
			assert!(command.close_peers.is_empty());
			let out_peers = command.open_peers;
			assert_eq!(peerset.num_in(), 0usize);
			assert_eq!(peerset.num_out(), 0usize);

			for outbound_peer in &out_peers {
				assert!(peers.contains(outbound_peer));
				assert_eq!(
					peerset.peers().get(&outbound_peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) })
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}

	// remove all reserved peers
	to_peerset
		.unbounded_send(PeersetCommand::RemoveReservedPeers { peers: peers.clone() })
		.unwrap();

	match peerset.next().await {
		Some(command) => {
			assert!(command.open_peers.is_empty());
			assert!(command.close_peers.is_empty());
		},
		event => panic!("invalid event: {event:?}"),
	}

	// verify all reserved peers are canceled
	for (_, state) in peerset.peers() {
		assert_eq!(state, &PeerState::Canceled { direction: Direction::Outbound(Reserved::Yes) });
	}
}

#[tokio::test]
async fn peer_added_and_removed_from_peerset() {
	sp_tracing::try_init_simple();

	let peerstore_handle = Arc::new(peerstore_handle_test());
	let (mut peerset, to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		0,
		0,
		true,
		Default::default(),
		Default::default(),
		peerstore_handle,
		NotificationMetrics::new(None),
	);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);

	// add peers to reserved set
	let peers = HashSet::from_iter([PeerId::random(), PeerId::random(), PeerId::random()]);
	to_peerset
		.unbounded_send(PeersetCommand::AddReservedPeers { peers: peers.clone() })
		.unwrap();

	match peerset.next().await {
		Some(command) => {
			assert!(command.close_peers.is_empty());
			let out_peers = command.open_peers;
			assert_eq!(peerset.num_in(), 0usize);
			assert_eq!(peerset.num_out(), 0usize);

			for outbound_peer in &out_peers {
				assert!(peers.contains(outbound_peer));
				assert!(peerset.reserved_peers().contains(outbound_peer));
				assert_eq!(
					peerset.peers().get(&outbound_peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) })
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}

	// report that all substreams were opened
	for peer in &peers {
		assert!(std::matches!(
			peerset.report_substream_opened(*peer, traits::Direction::Outbound),
			OpenResult::Accept { .. }
		));
		assert_eq!(
			peerset.peers().get(peer),
			Some(&PeerState::Connected { direction: Direction::Outbound(Reserved::Yes) })
		);
	}

	// remove all reserved peers
	to_peerset
		.unbounded_send(PeersetCommand::RemoveReservedPeers { peers: peers.clone() })
		.unwrap();

	match peerset.next().await {
		Some(command) => {
			assert!(command.open_peers.is_empty());

			let out_peers = command.close_peers;
			assert!(!out_peers.is_empty());

			for peer in &out_peers {
				assert!(peers.contains(peer));
				assert!(!peerset.reserved_peers().contains(peer));
				assert_eq!(
					peerset.peers().get(peer),
					Some(&PeerState::Closing { direction: Direction::Outbound(Reserved::Yes) }),
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}

	// add the peers again and verify that the command is ignored because the substreams are closing
	to_peerset
		.unbounded_send(PeersetCommand::AddReservedPeers { peers: peers.clone() })
		.unwrap();

	match peerset.next().await {
		Some(command) => {
			assert!(command.close_peers.is_empty());
			let out_peers = command.open_peers;
			assert!(out_peers.is_empty());

			for peer in &peers {
				assert!(peerset.reserved_peers().contains(peer));
				assert_eq!(
					peerset.peers().get(peer),
					Some(&PeerState::Closing { direction: Direction::Outbound(Reserved::Yes) }),
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}

	// remove the peers again and verify the state remains as `Closing`
	to_peerset
		.unbounded_send(PeersetCommand::RemoveReservedPeers { peers: peers.clone() })
		.unwrap();

	match peerset.next().await {
		Some(command) => {
			assert!(command.open_peers.is_empty());
			assert!(command.close_peers.is_empty());

			for peer in &peers {
				assert!(!peerset.reserved_peers().contains(peer));
				assert_eq!(
					peerset.peers().get(peer),
					Some(&PeerState::Closing { direction: Direction::Outbound(Reserved::Yes) }),
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}
}

#[tokio::test]
async fn set_reserved_peers() {
	sp_tracing::try_init_simple();

	let reserved = HashSet::from_iter([PeerId::random(), PeerId::random(), PeerId::random()]);
	let (mut peerset, to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		25,
		25,
		true,
		reserved.clone(),
		Default::default(),
		Arc::new(peerstore_handle_test()),
		NotificationMetrics::new(None),
	);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);

	match peerset.next().await {
		Some(command) => {
			assert!(command.close_peers.is_empty());
			let out_peers = command.open_peers;
			assert_eq!(peerset.num_in(), 0usize);
			assert_eq!(peerset.num_out(), 0usize);

			for outbound_peer in &out_peers {
				assert!(reserved.contains(outbound_peer));
				assert!(peerset.reserved_peers().contains(outbound_peer));
				assert_eq!(
					peerset.peers().get(&outbound_peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) })
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}

	// report that all substreams were opened
	for peer in &reserved {
		assert!(std::matches!(
			peerset.report_substream_opened(*peer, traits::Direction::Outbound),
			OpenResult::Accept { .. }
		));
		assert_eq!(
			peerset.peers().get(peer),
			Some(&PeerState::Connected { direction: Direction::Outbound(Reserved::Yes) })
		);
	}

	// add a totally new set of reserved peers
	let new_reserved_peers =
		HashSet::from_iter([PeerId::random(), PeerId::random(), PeerId::random()]);
	to_peerset
		.unbounded_send(PeersetCommand::SetReservedPeers { peers: new_reserved_peers.clone() })
		.unwrap();

	match peerset.next().await {
		Some(command) => {
			assert_eq!(command.open_peers.len(), 3);
			for peer in command.open_peers {
				assert!(new_reserved_peers.contains(&peer));
				assert!(peerset.reserved_peers().contains(&peer));
				assert_eq!(
					peerset.peers().get(&peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) }),
				);
			}

			let out_peers = command.close_peers;
			assert_eq!(out_peers.len(), 3);

			for peer in &out_peers {
				assert!(reserved.contains(peer));
				assert!(!peerset.reserved_peers().contains(peer));
				assert_eq!(
					peerset.peers().get(peer),
					Some(&PeerState::Closing { direction: Direction::Outbound(Reserved::Yes) }),
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}
}

#[tokio::test]
async fn set_reserved_peers_one_peer_already_in_the_set() {
	sp_tracing::try_init_simple();

	let reserved = HashSet::from_iter([PeerId::random(), PeerId::random(), PeerId::random()]);
	let common_peer = *reserved.iter().next().unwrap();
	let (mut peerset, to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		25,
		25,
		true,
		reserved.clone(),
		Default::default(),
		Arc::new(peerstore_handle_test()),
		NotificationMetrics::new(None),
	);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);

	match peerset.next().await {
		Some(command) => {
			assert!(command.close_peers.is_empty());
			let out_peers = command.open_peers;
			assert_eq!(peerset.num_in(), 0usize);
			assert_eq!(peerset.num_out(), 0usize);

			for outbound_peer in &out_peers {
				assert!(reserved.contains(outbound_peer));
				assert!(peerset.reserved_peers().contains(outbound_peer));
				assert_eq!(
					peerset.peers().get(&outbound_peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) })
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}

	// report that all substreams were opened
	for peer in &reserved {
		assert!(std::matches!(
			peerset.report_substream_opened(*peer, traits::Direction::Outbound),
			OpenResult::Accept { .. }
		));
		assert_eq!(
			peerset.peers().get(peer),
			Some(&PeerState::Connected { direction: Direction::Outbound(Reserved::Yes) })
		);
	}

	// add a new set of reserved peers with one peer from the original set
	let new_reserved_peers = HashSet::from_iter([PeerId::random(), PeerId::random(), common_peer]);
	to_peerset
		.unbounded_send(PeersetCommand::SetReservedPeers { peers: new_reserved_peers.clone() })
		.unwrap();

	match peerset.next().await {
		Some(command) => {
			assert_eq!(command.open_peers.len(), 2);
			for peer in command.open_peers {
				assert!(new_reserved_peers.contains(&peer));
				assert!(peerset.reserved_peers().contains(&peer));
				assert_eq!(
					peerset.peers().get(&peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) }),
				);
			}
			let out_peers = command.close_peers;
			assert_eq!(out_peers.len(), 2);

			for peer in &out_peers {
				assert!(reserved.contains(peer));

				if peer != &common_peer {
					assert!(!peerset.reserved_peers().contains(peer));
					assert_eq!(
						peerset.peers().get(peer),
						Some(&PeerState::Closing { direction: Direction::Outbound(Reserved::Yes) }),
					);
				} else {
					panic!("common peer disconnected");
				}
			}

			for peer in &new_reserved_peers {
				assert!(peerset.reserved_peers().contains(peer));
			}
		},
		event => panic!("invalid event: {event:?}"),
	}

	// verify the `common_peer` peer between the reserved sets is still in the state `Open`
	assert_eq!(
		peerset.peers().get(&common_peer),
		Some(&PeerState::Connected { direction: Direction::Outbound(Reserved::Yes) })
	);
}

#[tokio::test]
async fn add_reserved_peers_one_peer_already_in_the_set() {
	sp_tracing::try_init_simple();

	let peerstore_handle = Arc::new(peerstore_handle_test());
	let reserved = (0..3)
		.map(|_| {
			let peer = PeerId::random();
			peerstore_handle.add_known_peer(peer);
			peer
		})
		.collect::<Vec<_>>();
	let common_peer = *reserved.iter().next().unwrap();
	let (mut peerset, to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		25,
		25,
		true,
		reserved.iter().cloned().collect(),
		Default::default(),
		peerstore_handle,
		NotificationMetrics::new(None),
	);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);

	match peerset.next().await {
		Some(command) => {
			assert!(command.close_peers.is_empty());
			let out_peers = command.open_peers;
			assert_eq!(peerset.num_in(), 0usize);
			assert_eq!(peerset.num_out(), 0usize);
			assert_eq!(out_peers.len(), 3);

			for outbound_peer in &out_peers {
				assert!(reserved.contains(outbound_peer));
				assert!(peerset.reserved_peers().contains(outbound_peer));
				assert_eq!(
					peerset.peers().get(&outbound_peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) })
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}

	// report that all substreams were opened
	for peer in &reserved {
		assert!(std::matches!(
			peerset.report_substream_opened(*peer, traits::Direction::Outbound),
			OpenResult::Accept { .. }
		));
		assert_eq!(
			peerset.peers().get(peer),
			Some(&PeerState::Connected { direction: Direction::Outbound(Reserved::Yes) })
		);
	}

	// add a new set of reserved peers with one peer from the original set
	let new_reserved_peers = HashSet::from_iter([PeerId::random(), PeerId::random(), common_peer]);
	to_peerset
		.unbounded_send(PeersetCommand::AddReservedPeers { peers: new_reserved_peers.clone() })
		.unwrap();

	match peerset.next().await {
		Some(command) => {
			assert!(command.close_peers.is_empty());
			let out_peers = command.open_peers;
			assert_eq!(out_peers.len(), 2);
			assert!(!out_peers.iter().any(|peer| peer == &common_peer));

			for peer in &out_peers {
				assert!(!reserved.contains(peer));

				if peer != &common_peer {
					assert!(peerset.reserved_peers().contains(peer));
					assert_eq!(
						peerset.peers().get(peer),
						Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) }),
					);
				}
			}
		},
		event => panic!("invalid event: {event:?}"),
	}

	// verify the `common_peer` peer between the reserved sets is still in the state `Open`
	assert_eq!(
		peerset.peers().get(&common_peer),
		Some(&PeerState::Connected { direction: Direction::Outbound(Reserved::Yes) })
	);
}

#[tokio::test]
async fn opening_peer_gets_canceled_and_disconnected() {
	sp_tracing::try_init_simple();

	let peerstore_handle = Arc::new(peerstore_handle_test());
	let _known_peers = (0..1)
		.map(|_| {
			let peer = PeerId::random();
			peerstore_handle.add_known_peer(peer);
			peer
		})
		.collect::<Vec<_>>();
	let num_connected = Arc::new(Default::default());
	let (mut peerset, to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		25,
		25,
		false,
		Default::default(),
		Arc::clone(&num_connected),
		peerstore_handle,
		NotificationMetrics::new(None),
	);
	assert_eq!(peerset.num_in(), 0);
	assert_eq!(peerset.num_out(), 0);

	let peer = match peerset.next().await {
		Some(command) => {
			assert!(command.close_peers.is_empty());
			let out_peers = command.open_peers;
			assert_eq!(peerset.num_in(), 0);
			assert_eq!(peerset.num_out(), 1);
			assert_eq!(out_peers.len(), 1);

			for peer in &out_peers {
				assert_eq!(
					peerset.peers().get(&peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::No) })
				);
			}

			out_peers[0]
		},
		event => panic!("invalid event: {event:?}"),
	};

	// disconnect the now-opening peer
	to_peerset.unbounded_send(PeersetCommand::DisconnectPeer { peer }).unwrap();

	// poll `Peerset` to register the command and verify the peer is now in state `Canceled`
	futures::future::poll_fn(|cx| match peerset.poll_next_unpin(cx) {
		Poll::Pending => Poll::Ready(()),
		_ => panic!("unexpected event"),
	})
	.await;

	assert_eq!(
		peerset.peers().get(&peer),
		Some(&PeerState::Canceled { direction: Direction::Outbound(Reserved::No) })
	);
	assert_eq!(peerset.num_out(), 1);

	// report to `Peerset` that the substream was opened, verify that it gets closed
	assert!(std::matches!(
		peerset.report_substream_opened(peer, traits::Direction::Outbound),
		OpenResult::Reject { .. }
	));
	assert_eq!(
		peerset.peers().get(&peer),
		Some(&PeerState::Closing { direction: Direction::Outbound(Reserved::No) })
	);
	assert_eq!(num_connected.load(Ordering::SeqCst), 1);
	assert_eq!(peerset.num_out(), 1);

	// report close event to `Peerset` and verify state
	peerset.report_substream_closed(peer);
	assert_eq!(peerset.num_out(), 0);
	assert_eq!(num_connected.load(Ordering::SeqCst), 0);
	assert_eq!(peerset.peers().get(&peer), Some(&PeerState::Backoff));
}

#[tokio::test]
async fn open_failure_for_canceled_peer() {
	sp_tracing::try_init_simple();

	let peerstore_handle = Arc::new(peerstore_handle_test());
	let _known_peers = (0..1)
		.map(|_| {
			let peer = PeerId::random();
			peerstore_handle.add_known_peer(peer);
			peer
		})
		.collect::<Vec<_>>();
	let (mut peerset, to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		25,
		25,
		false,
		Default::default(),
		Default::default(),
		peerstore_handle,
		NotificationMetrics::new(None),
	);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);

	let peer = match peerset.next().await {
		Some(command) => {
			assert!(command.close_peers.is_empty());
			let out_peers = command.open_peers;
			assert_eq!(peerset.num_in(), 0usize);
			assert_eq!(peerset.num_out(), 1usize);
			assert_eq!(out_peers.len(), 1);

			for peer in &out_peers {
				assert_eq!(
					peerset.peers().get(&peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::No) })
				);
			}

			out_peers[0]
		},
		event => panic!("invalid event: {event:?}"),
	};

	// disconnect the now-opening peer
	to_peerset.unbounded_send(PeersetCommand::DisconnectPeer { peer }).unwrap();

	// poll `Peerset` to register the command and verify the peer is now in state `Canceled`
	futures::future::poll_fn(|cx| match peerset.poll_next_unpin(cx) {
		Poll::Pending => Poll::Ready(()),
		_ => panic!("unexpected event"),
	})
	.await;

	assert_eq!(
		peerset.peers().get(&peer),
		Some(&PeerState::Canceled { direction: Direction::Outbound(Reserved::No) })
	);

	// the substream failed to open, verify that peer state is now `Backoff`
	// and that `Peerset` doesn't emit any events
	peerset.report_substream_open_failure(peer, NotificationError::NoConnection);
	assert_eq!(peerset.peers().get(&peer), Some(&PeerState::Backoff));

	futures::future::poll_fn(|cx| match peerset.poll_next_unpin(cx) {
		Poll::Pending => Poll::Ready(()),
		_ => panic!("unexpected event"),
	})
	.await;
}

#[tokio::test]
async fn peer_disconnected_when_being_validated_then_rejected() {
	sp_tracing::try_init_simple();

	let peerstore_handle = Arc::new(peerstore_handle_test());
	let (mut peerset, _to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		25,
		25,
		false,
		Default::default(),
		Default::default(),
		peerstore_handle,
		NotificationMetrics::new(None),
	);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);

	// inbound substream received
	let peer = PeerId::random();
	assert_eq!(peerset.report_inbound_substream(peer), ValidationResult::Accept);

	// substream failed to open while it was being validated by the protocol
	peerset.report_substream_open_failure(peer, NotificationError::NoConnection);
	assert_eq!(peerset.peers().get(&peer), Some(&PeerState::Backoff));

	// protocol rejected substream, verify
	peerset.report_substream_rejected(peer);
	assert_eq!(peerset.peers().get(&peer), Some(&PeerState::Backoff));
}

#[tokio::test]
async fn removed_reserved_peer_kept_due_to_free_slots() {
	sp_tracing::try_init_simple();

	let peerstore_handle = Arc::new(peerstore_handle_test());
	let peers = HashSet::from_iter([PeerId::random(), PeerId::random(), PeerId::random()]);

	let (mut peerset, to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		25,
		25,
		true,
		peers.clone(),
		Default::default(),
		peerstore_handle,
		NotificationMetrics::new(None),
	);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);

	match peerset.next().await {
		Some(command) => {
			assert!(command.close_peers.is_empty());
			let out_peers = command.open_peers;
			assert_eq!(peerset.num_in(), 0usize);
			assert_eq!(peerset.num_out(), 0usize);

			for outbound_peer in &out_peers {
				assert!(peers.contains(outbound_peer));
				assert_eq!(
					peerset.peers().get(&outbound_peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) })
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}

	// remove all reserved peers
	to_peerset
		.unbounded_send(PeersetCommand::RemoveReservedPeers { peers: peers.clone() })
		.unwrap();

	match peerset.next().await {
		Some(command) => {
			assert!(command.open_peers.is_empty());
			assert!(command.close_peers.is_empty());
		},
		event => panic!("invalid event: {event:?}"),
	}

	// verify all reserved peers are canceled
	for (_, state) in peerset.peers() {
		assert_eq!(state, &PeerState::Opening { direction: Direction::Outbound(Reserved::No) });
	}

	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 3usize);
}

#[tokio::test]
async fn set_reserved_peers_but_available_slots() {
	sp_tracing::try_init_simple();

	let peerstore_handle = Arc::new(peerstore_handle_test());
	let known_peers = (0..3)
		.map(|_| {
			let peer = PeerId::random();
			peerstore_handle.add_known_peer(peer);
			peer
		})
		.collect::<Vec<_>>();

	// one peer is common across operations meaning an outbound substream will be opened to them
	// when `Peerset` is polled (along with two random peers) and later on `SetReservedPeers`
	// is called with the common peer and with two new random peers
	let common_peer = *known_peers.iter().next().unwrap();

	let (mut peerset, to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		25,
		25,
		false,
		Default::default(),
		Default::default(),
		peerstore_handle,
		NotificationMetrics::new(None),
	);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);

	// We have less than 25 outbound peers connected. At the next slot allocation we
	// query the `peerstore_handle` for more peers to connect to.
	match peerset.next().await {
		Some(command) => {
			assert!(command.close_peers.is_empty());
			let out_peers = command.open_peers;
			assert_eq!(out_peers.len(), 3);

			for peer in &out_peers {
				assert_eq!(
					peerset.peers().get(&peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::No) })
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}

	// verify all three peers are counted as outbound peers
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 3usize);

	// report that all substreams were opened
	for peer in &known_peers {
		assert!(std::matches!(
			peerset.report_substream_opened(*peer, traits::Direction::Outbound),
			OpenResult::Accept { .. }
		));
		assert_eq!(
			peerset.peers().get(peer),
			Some(&PeerState::Connected { direction: Direction::Outbound(Reserved::No) })
		);
	}

	// set reserved peers with `common_peer` being one of them
	let reserved_peers = HashSet::from_iter([common_peer, PeerId::random(), PeerId::random()]);
	to_peerset
		.unbounded_send(PeersetCommand::SetReservedPeers { peers: reserved_peers.clone() })
		.unwrap();

	// The command `SetReservedPeers` might evict currently reserved peers if
	// we don't have enough slot capacity to move them to regular nodes.
	// In this case, we did not have previously any reserved peers.
	match peerset.next().await {
		Some(command) => {
			// This ensures we don't disconnect peers when receiving `SetReservedPeers`.
			assert!(command.close_peers.is_empty());

			// verify that `Peerset` is aware of five peers, with two of them as outbound.
			assert_eq!(peerset.peers().len(), 5);
			assert_eq!(peerset.num_in(), 0usize);
			assert_eq!(peerset.num_out(), 2usize);
			assert_eq!(peerset.reserved_peers().len(), 3usize);

			// Expect two outbound requests for the new reserved peers (excluding the common peer).
			let peers = command.open_peers;
			assert_eq!(peers.len(), 2);

			assert_eq!(peers.len(), 2);
			assert!(!peers.contains(&common_peer));
			for peer in &peers {
				assert!(reserved_peers.contains(peer));
				assert!(peerset.reserved_peers().contains(peer));
				assert_eq!(
					peerset.peers().get(peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) }),
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}
}

#[tokio::test]
async fn set_reserved_peers_move_previously_reserved() {
	sp_tracing::try_init_simple();

	let peerstore_handle = Arc::new(peerstore_handle_test());
	let known_peers = (0..3)
		.map(|_| {
			let peer = PeerId::random();
			peerstore_handle.add_known_peer(peer);
			peer
		})
		.collect::<Vec<_>>();

	// We'll keep this peer as reserved and move the the others to regular nodes.
	let common_peer = *known_peers.iter().next().unwrap();
	let moved_peers = known_peers.iter().skip(1).copied().collect::<HashSet<_>>();
	let known_peers = known_peers.into_iter().collect::<HashSet<_>>();
	assert_eq!(moved_peers.len(), 2);

	let (mut peerset, to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		25,
		25,
		false,
		known_peers.clone(),
		Default::default(),
		peerstore_handle,
		NotificationMetrics::new(None),
	);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);

	// We are not connected to the reserved peers.
	match peerset.next().await {
		Some(command) => {
			assert!(command.close_peers.is_empty());
			let out_peers = command.open_peers;
			assert_eq!(out_peers.len(), 3);

			for peer in &out_peers {
				assert_eq!(
					peerset.peers().get(&peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) })
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}

	// verify all three peers are marked as reserved peers and they don't count towards
	// slot allocation.
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);
	assert_eq!(peerset.reserved_peers().len(), 3usize);

	// report that all substreams were opened
	for peer in &known_peers {
		assert!(std::matches!(
			peerset.report_substream_opened(*peer, traits::Direction::Outbound),
			OpenResult::Accept { .. }
		));
		assert_eq!(
			peerset.peers().get(peer),
			Some(&PeerState::Connected { direction: Direction::Outbound(Reserved::Yes) })
		);
	}

	// set reserved peers with `common_peer` being one of them
	let reserved_peers = HashSet::from_iter([common_peer, PeerId::random(), PeerId::random()]);
	to_peerset
		.unbounded_send(PeersetCommand::SetReservedPeers { peers: reserved_peers.clone() })
		.unwrap();

	// The command `SetReservedPeers` might evict currently reserved peers if
	// we don't have enough slot capacity to move them to regular nodes.
	// In this case, we have enough capacity.
	let Some(command) = peerset.next().await else {
		panic!("expected command");
	};

	// This ensures we don't disconnect peers when receiving `SetReservedPeers`.
	assert!(command.close_peers.is_empty());

	// verify that `Peerset` is aware of five peers.
	// 2 of the previously reserved peers are moved as outbound regular peers and
	// count towards slot allocation.
	assert_eq!(peerset.peers().len(), 5);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 2usize);
	assert_eq!(peerset.reserved_peers().len(), 3usize);

	// Ensure the previously reserved are not regular nodes.
	for (peer, state) in peerset.peers() {
		// This peer was previously reserved and remained reserved after `SetReservedPeers`.
		if peer == &common_peer {
			assert_eq!(
				state,
				&PeerState::Connected { direction: Direction::Outbound(Reserved::Yes) }
			);
			continue;
		}

		// Part of the new reserved nodes.
		// Reserved peers are moved to the `Opening` state directly
		// without waiting for the slot timer to allocate them.
		if reserved_peers.contains(peer) {
			assert_eq!(
				peerset.peers().get(peer),
				Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) }),
			);
			continue;
		}

		// Previously reserved, but remained connected.
		if moved_peers.contains(peer) {
			// This was previously `Reseved::Yes` but moved to regular nodes.
			assert_eq!(
				state,
				&PeerState::Connected { direction: Direction::Outbound(Reserved::No) }
			);
			continue;
		}
		panic!("Invalid state peer={peer:?} state={state:?}");
	}

	// Open desires with newly reserved.
	let peers = command.open_peers;
	assert_eq!(peers.len(), 2);
	assert!(!peers.contains(&common_peer));
	for peer in &peers {
		assert!(reserved_peers.contains(peer));
		assert!(peerset.reserved_peers().contains(peer));
		assert_eq!(
			peerset.peers().get(peer),
			Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) }),
		);
	}
}

#[tokio::test]
async fn set_reserved_peers_cannot_move_previously_reserved() {
	sp_tracing::try_init_simple();

	let peerstore_handle = Arc::new(peerstore_handle_test());
	let known_peers = (0..3)
		.map(|_| {
			let peer = PeerId::random();
			peerstore_handle.add_known_peer(peer);
			peer
		})
		.collect::<Vec<_>>();

	// We'll keep this peer as reserved and move the the others to regular nodes.
	let common_peer = *known_peers.iter().next().unwrap();
	let moved_peers = known_peers.iter().skip(1).copied().collect::<HashSet<_>>();
	let known_peers = known_peers.into_iter().collect::<HashSet<_>>();
	assert_eq!(moved_peers.len(), 2);

	// We don't have capacity to move peers.
	let (mut peerset, to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		0,
		0,
		false,
		known_peers.clone(),
		Default::default(),
		peerstore_handle,
		NotificationMetrics::new(None),
	);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);

	// We are not connected to the reserved peers.
	match peerset.next().await {
		Some(command) => {
			assert!(command.close_peers.is_empty());
			let out_peers = command.open_peers;
			assert_eq!(out_peers.len(), 3);

			for peer in &out_peers {
				assert_eq!(
					peerset.peers().get(&peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) })
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}

	// verify all three peers are marked as reserved peers and they don't count towards
	// slot allocation.
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);
	assert_eq!(peerset.reserved_peers().len(), 3usize);

	// report that all substreams were opened
	for peer in &known_peers {
		assert!(std::matches!(
			peerset.report_substream_opened(*peer, traits::Direction::Outbound),
			OpenResult::Accept { .. }
		));
		assert_eq!(
			peerset.peers().get(peer),
			Some(&PeerState::Connected { direction: Direction::Outbound(Reserved::Yes) })
		);
	}

	// set reserved peers with `common_peer` being one of them
	let reserved_peers = HashSet::from_iter([common_peer, PeerId::random(), PeerId::random()]);
	to_peerset
		.unbounded_send(PeersetCommand::SetReservedPeers { peers: reserved_peers.clone() })
		.unwrap();

	// The command `SetReservedPeers` might evict currently reserved peers if
	// we don't have enough slot capacity to move them to regular nodes.
	// In this case, we don't have enough capacity.
	match peerset.next().await {
		Some(command) => {
			// One of the new reserved peers is already connected.
			assert_eq!(command.open_peers.len(), 2);
			for peer in &command.open_peers {
				assert!(reserved_peers.contains(peer));
				assert!(peerset.reserved_peers().contains(peer));
				assert_eq!(
					peerset.peers().get(peer),
					Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) }),
				);
			}

			let peers = command.close_peers;
			// This ensures we don't disconnect peers when receiving `SetReservedPeers`.
			assert_eq!(peers.len(), 2);

			for peer in peers {
				// Ensure common peer is not disconnected.
				assert_ne!(common_peer, peer);

				assert_eq!(
					peerset.peers().get(&peer),
					Some(&PeerState::Closing { direction: Direction::Outbound(Reserved::Yes) })
				);
			}
		},
		event => panic!("invalid event: {event:?}"),
	}

	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);
	assert_eq!(peerset.reserved_peers().len(), 3usize);
}

#[tokio::test]
async fn reserved_only_rejects_non_reserved_peers() {
	sp_tracing::try_init_simple();

	let peerstore_handle = Arc::new(peerstore_handle_test());
	let reserved_peers = HashSet::from_iter([PeerId::random(), PeerId::random(), PeerId::random()]);

	let connected_peers = Arc::new(AtomicUsize::new(0));
	let (mut peerset, to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		3,
		3,
		true,
		reserved_peers.clone(),
		connected_peers.clone(),
		peerstore_handle,
		NotificationMetrics::new(None),
	);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);

	// Step 1. Connect reserved peers.
	{
		match peerset.next().await {
			Some(command) => {
				assert!(command.close_peers.is_empty());
				let out_peers = command.open_peers;
				assert_eq!(peerset.num_in(), 0usize);
				assert_eq!(peerset.num_out(), 0usize);

				for outbound_peer in &out_peers {
					assert!(reserved_peers.contains(outbound_peer));
					assert_eq!(
						peerset.peers().get(&outbound_peer),
						Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) })
					);
				}
			},
			event => panic!("invalid event: {event:?}"),
		}
		// Report the reserved peers as connected.
		for peer in &reserved_peers {
			assert!(std::matches!(
				peerset.report_substream_opened(*peer, traits::Direction::Outbound),
				OpenResult::Accept { .. }
			));
			assert_eq!(
				peerset.peers().get(peer),
				Some(&PeerState::Connected { direction: Direction::Outbound(Reserved::Yes) })
			);
		}
		assert_eq!(connected_peers.load(Ordering::Relaxed), 3usize);
	}

	// Step 2. Ensure non-reserved peers are rejected.
	let normal_peers: Vec<PeerId> = vec![PeerId::random(), PeerId::random(), PeerId::random()];
	{
		// Report the peers as inbound for validation purposes.
		for peer in &normal_peers {
			// We are running in reserved only mode.
			let result = peerset.report_inbound_substream(*peer);
			assert_eq!(result, ValidationResult::Reject);

			// The peer must be kept in the disconnected state.
			assert_eq!(peerset.peers().get(peer), Some(&PeerState::Disconnected));
		}
		// Ensure slots are not used.
		assert_eq!(peerset.num_in(), 0usize);
		assert_eq!(peerset.num_out(), 0usize);

		// Report that all substreams were opened.
		for peer in &normal_peers {
			// We must reject them because the peers were rejected prior by
			// `report_inbound_substream` and therefore set into the disconnected state.
			let result = peerset.report_substream_opened(*peer, traits::Direction::Inbound);
			assert_eq!(result, OpenResult::Reject);

			// Peer remains disconnected.
			assert_eq!(peerset.peers().get(&peer), Some(&PeerState::Disconnected));
		}
		assert_eq!(connected_peers.load(Ordering::Relaxed), 3usize);

		// Because we have returned `Reject` from `report_substream_opened`
		// the substreams will later be closed.
		for peer in &normal_peers {
			peerset.report_substream_closed(*peer);

			// Peer moves into the backoff state.
			assert_eq!(peerset.peers().get(peer), Some(&PeerState::Backoff));
		}
		// The slots are not used / altered.
		assert_eq!(connected_peers.load(Ordering::Relaxed), 3usize);
	}

	// Move peers out of the backoff state (ie simulate 5s elapsed time).
	for (peer, state) in peerset.peers_mut() {
		if normal_peers.contains(peer) {
			match state {
				PeerState::Backoff => *state = PeerState::Disconnected,
				state => panic!("invalid state peer={peer:?} state={state:?}"),
			}
		} else if reserved_peers.contains(peer) {
			match state {
				PeerState::Connected { direction: Direction::Outbound(Reserved::Yes) } => {},
				state => panic!("invalid state peer={peer:?} state={state:?}"),
			}
		} else {
			panic!("invalid peer={peer:?} not present");
		}
	}

	// Step 3. Allow connections from non-reserved peers.
	{
		to_peerset
			.unbounded_send(PeersetCommand::SetReservedOnly { reserved_only: false })
			.unwrap();
		// This will activate the non-reserved peers and give us the best outgoing
		// candidates to connect to.
		match peerset.next().await {
			Some(command) => {
				assert!(command.close_peers.is_empty());
				let peers = command.open_peers;

				// These are the non-reserved peers we informed the peerset above.
				assert_eq!(peers.len(), 3);
				for peer in &peers {
					assert!(!reserved_peers.contains(peer));
					assert_eq!(
						peerset.peers().get(peer),
						Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::No) })
					);
					assert!(normal_peers.contains(peer));
				}
			},
			event => panic!("invalid event : {event:?}"),
		}
		// Ensure slots are used.
		assert_eq!(peerset.num_in(), 0usize);
		assert_eq!(peerset.num_out(), 3usize);

		for peer in &normal_peers {
			let result = peerset.report_inbound_substream(*peer);
			assert_eq!(result, ValidationResult::Accept);
			// Direction is kept from the outbound slot allocation.
			assert_eq!(
				peerset.peers().get(peer),
				Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::No) })
			);
		}
		// Ensure slots are used.
		assert_eq!(peerset.num_in(), 0usize);
		assert_eq!(peerset.num_out(), 3usize);
		// Peers are only reported as connected once the substream is opened.
		// 3 represents the reserved peers that are already connected.
		assert_eq!(connected_peers.load(Ordering::Relaxed), 3usize);

		let (success, failure) = normal_peers.split_at(2);
		for peer in success {
			assert!(std::matches!(
				peerset.report_substream_opened(*peer, traits::Direction::Outbound),
				OpenResult::Accept { .. }
			));
			assert_eq!(
				peerset.peers().get(peer),
				Some(&PeerState::Connected { direction: Direction::Outbound(Reserved::No) })
			);
		}
		// Simulate one failure.
		let failure = failure[0];
		peerset.report_substream_open_failure(failure, NotificationError::ChannelClogged);
		assert_eq!(peerset.peers().get(&failure), Some(&PeerState::Backoff));
		assert_eq!(peerset.num_in(), 0usize);
		assert_eq!(peerset.num_out(), 2usize);
		assert_eq!(connected_peers.load(Ordering::Relaxed), 5usize);
	}
}
