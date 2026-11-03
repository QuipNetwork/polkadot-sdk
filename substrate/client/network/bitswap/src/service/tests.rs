// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Substrate.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate. If not, see <https://www.gnu.org/licenses/>.

//! Bitswap service tests.

use super::*;
use crate::{
	BLAKE2B_256_MULTIHASH_CODE, KECCAK_256_MULTIHASH_CODE, RAW_CODEC, SHA2_256_MULTIHASH_CODE,
};
use cid::multihash::Multihash as CidMultihash;
use rand::{rngs::StdRng, SeedableRng};
use rstest::rstest;
use sc_block_builder::BlockBuilderBuilder;
use sc_network_sync::SyncEvent;
use sc_network_types::PeerId as TypesPeerId;
use sp_consensus::BlockOrigin;
use sp_runtime::codec::Encode;
use substrate_test_runtime::ExtrinsicBuilder;
use substrate_test_runtime_client::{prelude::*, TestClientBuilder};
use tokio::{
	sync::Mutex as AsyncMutex,
	time::{sleep, timeout},
};

struct MockTransport {
	inbound: AsyncMutex<mpsc::Receiver<TransportEvent>>,
	outbound_req_tx: mpsc::Sender<(litep2p::PeerId, Vec<(Cid, WantType)>)>,
	outbound_resp_tx: mpsc::Sender<(litep2p::PeerId, Vec<ResponseType>)>,
}

#[async_trait]
impl BitswapTransport for MockTransport {
	async fn next_event(&mut self) -> Option<TransportEvent> {
		self.inbound.get_mut().recv().await
	}

	async fn send_request(&self, peer: litep2p::PeerId, cids: Vec<(Cid, WantType)>) {
		let _ = self.outbound_req_tx.send((peer, cids)).await;
	}

	async fn send_response(&self, peer: litep2p::PeerId, responses: Vec<ResponseType>) {
		let _ = self.outbound_resp_tx.send((peer, responses)).await;
	}
}

struct TestRig {
	user_handle: BitswapHandle,
	sync_event_tx: mpsc::Sender<SyncEvent>,
	inbound_tx: mpsc::Sender<TransportEvent>,
	outbound_req_rx: mpsc::Receiver<(litep2p::PeerId, Vec<(Cid, WantType)>)>,
	outbound_resp_rx: mpsc::Receiver<(litep2p::PeerId, Vec<ResponseType>)>,
	_handle: tokio::task::JoinHandle<()>,
}

fn build_rig_with(
	client: Arc<dyn BlockBackend<substrate_test_runtime::Block> + Send + Sync>,
	max_live_cids: usize,
) -> TestRig {
	build_rig_with_inbound_limits(
		client,
		max_live_cids,
		MAX_CONCURRENT_INBOUND_LOOKUPS,
		MAX_QUEUED_INBOUND_ENTRIES_PER_PEER,
	)
}

/// A not-yet-running service plus the channel ends tests interact with. Most tests
/// spawn the service via [`build_rig_with_inbound_limits`]; tests that need to call
/// handlers directly keep the service instead.
struct TestParts {
	service: BitswapService<substrate_test_runtime::Block>,
	user_handle: BitswapHandle,
	sync_event_tx: mpsc::Sender<SyncEvent>,
	inbound_tx: mpsc::Sender<TransportEvent>,
	outbound_req_rx: mpsc::Receiver<(litep2p::PeerId, Vec<(Cid, WantType)>)>,
	outbound_resp_rx: mpsc::Receiver<(litep2p::PeerId, Vec<ResponseType>)>,
}

fn build_parts(
	client: Arc<dyn BlockBackend<substrate_test_runtime::Block> + Send + Sync>,
	max_live_cids: usize,
	max_lookups: usize,
	queued_entries_per_peer: usize,
) -> TestParts {
	let (inbound_tx, inbound_rx) = mpsc::channel(64);
	let (outbound_req_tx, outbound_req_rx) = mpsc::channel(64);
	let (outbound_resp_tx, outbound_resp_rx) = mpsc::channel(64);
	let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAPACITY);
	let (sync_event_tx, sync_event_rx) = mpsc::channel::<SyncEvent>(64);
	let metrics = BitswapMetrics::default();
	let (inbound_lookup_pool, inbound_lookup_rx) =
		InboundLookupPool::new(client, max_lookups, metrics.clone());

	let transport =
		MockTransport { inbound: AsyncMutex::new(inbound_rx), outbound_req_tx, outbound_resp_tx };

	let sync_event_stream: Pin<Box<dyn Stream<Item = SyncEvent> + Send>> =
		Box::pin(tokio_stream::wrappers::ReceiverStream::new(sync_event_rx));

	let service: BitswapService<substrate_test_runtime::Block> = BitswapService {
		handle: Box::new(transport),
		cmd_rx,
		cmd_channel_closed: false,
		sync_event_stream,
		inbound_lookup_pool,
		inbound_lookup_rx,
		inbound_queue: InboundQueue::new(queued_entries_per_peer),
		connected_peers: HashSet::new(),
		scheduler: RequestScheduler::new(max_live_cids),
		user_requests: SlotMap::with_key(),
		metrics,
	};

	let user_handle = BitswapHandle::new(cmd_tx);

	TestParts { service, user_handle, sync_event_tx, inbound_tx, outbound_req_rx, outbound_resp_rx }
}

fn build_rig_with_inbound_limits(
	client: Arc<dyn BlockBackend<substrate_test_runtime::Block> + Send + Sync>,
	max_live_cids: usize,
	max_lookups: usize,
	queued_entries_per_peer: usize,
) -> TestRig {
	let TestParts {
		service,
		user_handle,
		sync_event_tx,
		inbound_tx,
		outbound_req_rx,
		outbound_resp_rx,
	} = build_parts(client, max_live_cids, max_lookups, queued_entries_per_peer);
	let _handle = tokio::spawn(async move { service.run().await });

	TestRig { user_handle, sync_event_tx, inbound_tx, outbound_req_rx, outbound_resp_rx, _handle }
}

fn empty_rig() -> TestRig {
	small_window_rig(MAX_LIVE_CIDS)
}

fn small_window_rig(max_live_cids: usize) -> TestRig {
	let client = Arc::new(substrate_test_runtime_client::new());
	build_rig_with(client, max_live_cids)
}

fn cid_for_data(mh_code: u64, data: &[u8]) -> Cid {
	let digest = match mh_code {
		BLAKE2B_256_MULTIHASH_CODE => sp_crypto_hashing::blake2_256(data),
		SHA2_256_MULTIHASH_CODE => sp_crypto_hashing::sha2_256(data),
		KECCAK_256_MULTIHASH_CODE => sp_crypto_hashing::keccak_256(data),
		_ => panic!("unsupported multihash code"),
	};
	let mh = CidMultihash::<64>::wrap(mh_code, &digest).unwrap();
	Cid::new_v1(RAW_CODEC, mh)
}

fn cid_for_digest(mh_code: u64, digest: [u8; 32]) -> Cid {
	let mh = CidMultihash::<64>::wrap(mh_code, &digest).unwrap();
	Cid::new_v1(RAW_CODEC, mh)
}

async fn drain_next<T>(rx: &mut mpsc::Receiver<T>) -> Option<T> {
	timeout(Duration::from_secs(2), rx.recv()).await.ok().flatten()
}

fn to_types_peer(peer: litep2p::PeerId) -> TypesPeerId {
	TypesPeerId::from_bytes(&peer.to_bytes()).expect("peer ID bytes are valid")
}

fn sync_connected(peer: litep2p::PeerId) -> SyncEvent {
	SyncEvent::PeerConnected { peer_id: to_types_peer(peer), roles: Roles::FULL }
}

fn sync_connected_light(peer: litep2p::PeerId) -> SyncEvent {
	SyncEvent::PeerConnected { peer_id: to_types_peer(peer), roles: Roles::LIGHT }
}

fn sync_disconnected(peer: litep2p::PeerId) -> SyncEvent {
	SyncEvent::PeerDisconnected(to_types_peer(peer))
}

impl TestRig {
	async fn connect(&self, peer: litep2p::PeerId) {
		self.sync_event_tx.send(sync_connected(peer)).await.unwrap();
	}

	async fn send_response(&self, peer: litep2p::PeerId, responses: Vec<TransportResponse>) {
		self.inbound_tx
			.send(TransportEvent::Response { peer, responses })
			.await
			.unwrap();
	}

	async fn send_block(&self, peer: litep2p::PeerId, cid: Cid, data: &[u8]) {
		self.send_response(
			peer,
			vec![TransportResponse::VerifiedBlock { cid, bytes: Bytes::copy_from_slice(data) }],
		)
		.await;
	}

	async fn send_presence(&self, peer: litep2p::PeerId, cid: Cid, presence: BlockPresenceType) {
		self.send_response(peer, vec![TransportResponse::Presence { cid, presence }])
			.await;
	}

	async fn send_wantlist(&self, peer: litep2p::PeerId, cids: Vec<(Cid, WantType)>) {
		self.inbound_tx.send(TransportEvent::Request { peer, cids }).await.unwrap();
	}
}

async fn expect_block(rx: &mut mpsc::Receiver<FetchItem>, cid: Cid, data: &[u8]) {
	match drain_next(rx).await.expect("stream item") {
		Ok((got_cid, bytes)) => {
			assert_eq!(got_cid, cid);
			assert_eq!(bytes.as_ref(), data);
		},
		other => panic!("expected block, got {other:?}"),
	}
}

fn assert_single_dont_have(responses: &[ResponseType], cid: Cid) {
	assert!(matches!(
		responses,
		[ResponseType::Presence { cid: got, presence: BlockPresenceType::DontHave }]
			if *got == cid
	));
}

#[test]
fn want_set_removes_cid_after_last_waiter_and_peer_complete() {
	let cid = cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [0xaa; 32]);
	let peer = litep2p::PeerId::random();
	let mut waiter_ids = SlotMap::with_key();
	let waiter_id = waiter_ids.insert(());
	let mut wants = RequestScheduler::new(MAX_LIVE_CIDS);
	let mut rng = StdRng::seed_from_u64(0);

	wants.add_user_request(cid, waiter_id);
	let selected = wants
		.next_peer_to_request(cid, &HashSet::from([peer]), Instant::now(), &mut rng)
		.unwrap();
	assert_eq!(selected, peer);

	wants.remove_user_request(cid, waiter_id);
	assert!(wants.contains(&cid));

	wants.mark_peer_done_for_cid(peer, cid);
	assert!(!wants.contains(&cid));
}

#[test]
fn want_set_window_queues_and_promotes() {
	let cid_a = cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [0x01; 32]);
	let cid_b = cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [0x02; 32]);
	let peer = litep2p::PeerId::random();
	let peers = HashSet::from([peer]);
	let mut waiter_ids = SlotMap::with_key();
	let waiter_id = waiter_ids.insert(());
	let mut wants = RequestScheduler::new(1);
	let mut rng = StdRng::seed_from_u64(0);

	wants.add_user_request(cid_a, waiter_id);
	wants.add_user_request(cid_b, waiter_id);

	assert_eq!(wants.next_peer_to_request(cid_a, &peers, Instant::now(), &mut rng), Some(peer));
	assert_eq!(wants.next_peer_to_request(cid_b, &peers, Instant::now(), &mut rng), None);
	assert!(!wants.has_window_capacity());

	wants.take_user_requests_for_delivered_cid(cid_a);
	assert!(wants.has_window_capacity());
	assert_eq!(wants.pop_pending(), Some(cid_b));
	assert_eq!(wants.next_peer_to_request(cid_b, &peers, Instant::now(), &mut rng), Some(peer));
	assert_eq!(wants.pop_pending(), None);
}

#[test]
fn peer_selection_varies_across_cids() {
	let peers: HashSet<_> = (0..3).map(|_| litep2p::PeerId::random()).collect();
	let mut waiter_ids = SlotMap::with_key();
	let mut wants = RequestScheduler::new(32);
	let mut rng = StdRng::seed_from_u64(0);
	let mut selected = HashSet::new();

	for byte in 0..32 {
		let cid = cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [byte; 32]);
		let waiter = waiter_ids.insert(());
		wants.add_user_request(cid, waiter);
		selected.insert(
			wants
				.next_peer_to_request(cid, &peers, Instant::now(), &mut rng)
				.expect("a connected peer is eligible"),
		);
	}

	assert!(selected.len() > 1, "fresh CIDs should not all select the same peer");
}

#[test]
fn inbound_queue_rotates_peers_between_batches() {
	let peer_a = litep2p::PeerId::random();
	let peer_b = litep2p::PeerId::random();
	let entry = |i: u8| (cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [i; 32]), WantType::Block);
	let mut queue = InboundQueue::new(MAX_QUEUED_INBOUND_ENTRIES_PER_PEER);

	assert_eq!(queue.enqueue(peer_a, (0..(MAX_WANTED_BLOCKS + 4) as u8).map(entry).collect()), 0);
	assert_eq!(queue.enqueue(peer_b, vec![entry(0xff)]), 0);

	let (peer, batch) = queue.next_batch().unwrap();
	assert_eq!((peer, batch.len()), (peer_a, MAX_WANTED_BLOCKS));
	let (peer, batch) = queue.next_batch().unwrap();
	assert_eq!((peer, batch.len()), (peer_b, 1));
	let (peer, batch) = queue.next_batch().unwrap();
	assert_eq!((peer, batch.len()), (peer_a, 4));
	assert!(queue.next_batch().is_none());
}

#[test]
fn inbound_queue_skips_inconsistent_rotation_entries() {
	let stale_peer = litep2p::PeerId::random();
	let empty_peer = litep2p::PeerId::random();
	let ready_peer = litep2p::PeerId::random();
	let entry = (cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [0xaa; 32]), WantType::Block);
	let mut queue = InboundQueue::new(MAX_QUEUED_INBOUND_ENTRIES_PER_PEER);

	queue.rotation.extend([stale_peer, empty_peer]);
	queue.per_peer.insert(empty_peer, VecDeque::new());
	queue.enqueue(ready_peer, vec![entry]);

	let (peer, batch) = queue.next_batch().expect("ready peer remains serviceable");
	assert_eq!(peer, ready_peer);
	assert_eq!(batch, vec![entry]);
	assert!(!queue.per_peer.contains_key(&empty_peer));
	assert!(queue.next_batch().is_none());
}

#[test]
fn inbound_queue_drops_overflow_and_counts_it() {
	let peer = litep2p::PeerId::random();
	let entry = |i: u8| (cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [i; 32]), WantType::Block);
	let mut queue = InboundQueue::new(4);

	assert_eq!(queue.enqueue(peer, (0..3).map(entry).collect()), 0);
	assert_eq!(queue.enqueue(peer, (3..6).map(entry).collect()), 2);

	let (_, batch) = queue.next_batch().unwrap();
	let kept: Vec<Cid> = batch.into_iter().map(|(cid, _)| cid).collect();
	assert_eq!(
		kept,
		(0..4)
			.map(|i| cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [i; 32]))
			.collect::<Vec<_>>()
	);
	assert!(queue.next_batch().is_none());
}

#[test]
fn inbound_queue_remove_peer_purges_queue_and_rotation() {
	let peer_a = litep2p::PeerId::random();
	let peer_b = litep2p::PeerId::random();
	let entry = |i: u8| (cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [i; 32]), WantType::Block);
	let mut queue = InboundQueue::new(MAX_QUEUED_INBOUND_ENTRIES_PER_PEER);

	assert_eq!(queue.enqueue(peer_a, (0..4).map(entry).collect()), 0);
	assert_eq!(queue.enqueue(peer_b, vec![entry(0xff)]), 0);

	assert_eq!(queue.remove_peer(&peer_a), 4);
	assert_eq!(queue.remove_peer(&peer_a), 0);

	let (peer, batch) = queue.next_batch().unwrap();
	assert_eq!((peer, batch.len()), (peer_b, 1));
	assert!(queue.next_batch().is_none());
}

#[tokio::test]
async fn disconnect_purges_queued_inbound_entries_of_peer() {
	let client = Arc::new(substrate_test_runtime_client::new());
	let mut parts = build_parts(client, MAX_LIVE_CIDS, 1, MAX_QUEUED_INBOUND_ENTRIES_PER_PEER);
	let peer_a = litep2p::PeerId::random();
	let peer_b = litep2p::PeerId::random();
	let entry = |i: u8| (cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [i; 32]), WantType::Block);

	// Park the only lookup worker so both wantlists stay queued.
	let permit = parts.service.inbound_lookup_pool.try_acquire_worker().expect("worker is idle");
	parts.service.on_inbound_request(peer_a, vec![entry(1)]);
	parts.service.on_inbound_request(peer_b, vec![entry(2)]);

	parts.service.on_peer_disconnected(peer_a).await;

	// Once the worker frees up, only the still-connected peer's wantlist is served.
	drop(permit);
	parts.service.dispatch_inbound_lookups();
	let (served_peer, responses, _permit) =
		drain_next(&mut parts.service.inbound_lookup_rx).await.expect("lookup result");
	assert_eq!(served_peer, peer_b);
	assert_single_dont_have(&responses, entry(2).0);
	assert!(parts.service.inbound_queue.next_batch().is_none());
}

#[tokio::test(start_paused = true)]
async fn litep2p_transport_send_times_out_instead_of_blocking_forever() {
	// The config owns the command receiver; keeping it alive without running the protocol
	// models a wedged litep2p task that no longer drains the channel.
	let (_config, handle) = litep2p::protocol::libp2p::bitswap::Config::new();
	let peer = litep2p::PeerId::random();
	let want = vec![(cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [0x0c; 32]), WantType::Block)];

	// Fill the command channel until a raw (unbounded) send blocks.
	let mut filled = false;
	for _ in 0..100_000 {
		let raw_send = Litep2pBitswapHandle::send_request(&handle, peer, want.clone());
		if timeout(Duration::from_secs(60), raw_send).await.is_err() {
			filled = true;
			break;
		}
	}
	assert!(filled, "command channel never filled");

	// The transport wrapper must give up instead of parking the service actor forever.
	let started = tokio::time::Instant::now();
	BitswapTransport::send_request(&handle, peer, want.clone()).await;
	BitswapTransport::send_response(&handle, peer, Vec::new()).await;
	assert!(started.elapsed() >= 2 * TRANSPORT_SEND_TIMEOUT);
}

#[tokio::test(start_paused = true)]
async fn single_cid_single_peer_block_response() {
	let mut rig = empty_rig();
	let peer = litep2p::PeerId::random();
	let data = b"payload-a".to_vec();
	let cid = cid_for_data(BLAKE2B_256_MULTIHASH_CODE, &data);

	rig.connect(peer).await;
	let mut rx = rig.user_handle.request_stream(HashSet::from([cid])).unwrap();

	let (out_peer, out_cids) = drain_next(&mut rig.outbound_req_rx).await.expect("outbound WANT");
	assert_eq!(out_peer, peer);
	assert_eq!(out_cids, vec![(cid, WantType::Block)]);

	rig.send_block(peer, cid, &data).await;
	expect_block(&mut rx, cid, &data).await;
}

#[rstest]
#[case::after_dont_have(true)]
#[case::after_timeout(false)]
#[tokio::test(start_paused = true)]
async fn exhausted_round_reasks_peer_after_delay(#[case] answer_dont_have: bool) {
	let mut rig = empty_rig();
	let peer = litep2p::PeerId::random();
	let data = b"payload-round".to_vec();
	let cid = cid_for_data(BLAKE2B_256_MULTIHASH_CODE, &data);

	rig.connect(peer).await;
	let mut rx = rig.user_handle.request_stream(HashSet::from([cid])).unwrap();
	let _ = drain_next(&mut rig.outbound_req_rx).await.expect("first WANT");

	if answer_dont_have {
		rig.send_presence(peer, cid, BlockPresenceType::DontHave).await;
		let no_req =
			timeout(ROUND_RETRY_DELAY - Duration::from_secs(1), rig.outbound_req_rx.recv()).await;
		assert!(no_req.is_err());
	}

	let (out_peer, out_cids) =
		timeout(PER_PEER_TIMEOUT + ROUND_RETRY_DELAY * 2, rig.outbound_req_rx.recv())
			.await
			.expect("new-round WANT")
			.expect("transport channel open");
	assert_eq!(out_peer, peer);
	assert_eq!(out_cids, vec![(cid, WantType::Block)]);

	rig.send_block(peer, cid, &data).await;
	expect_block(&mut rx, cid, &data).await;
}

#[tokio::test(start_paused = true)]
async fn new_peer_is_asked_immediately_while_round_is_parked() {
	let mut rig = empty_rig();
	let peer_a = litep2p::PeerId::random();
	let peer_b = litep2p::PeerId::random();
	let data = b"payload-round-3".to_vec();
	let cid = cid_for_data(BLAKE2B_256_MULTIHASH_CODE, &data);

	rig.connect(peer_a).await;
	let mut rx = rig.user_handle.request_stream(HashSet::from([cid])).unwrap();
	let _ = drain_next(&mut rig.outbound_req_rx).await.expect("first WANT");

	rig.send_presence(peer_a, cid, BlockPresenceType::DontHave).await;

	rig.connect(peer_b).await;
	let (out_peer, _) = drain_next(&mut rig.outbound_req_rx).await.expect("WANT to new peer");
	assert_eq!(out_peer, peer_b);

	rig.send_block(peer_b, cid, &data).await;
	expect_block(&mut rx, cid, &data).await;

	tokio::time::advance(ROUND_RETRY_DELAY * 2).await;
	assert!(rig.outbound_req_rx.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn light_client_peers_are_not_tracked() {
	let mut rig = empty_rig();
	let light_peer = litep2p::PeerId::random();
	let full_peer = litep2p::PeerId::random();
	let cid = cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [9u8; 32]);

	rig.sync_event_tx.send(sync_connected_light(light_peer)).await.unwrap();
	let _rx = rig.user_handle.request_stream(HashSet::from([cid])).unwrap();

	assert!(drain_next(&mut rig.outbound_req_rx).await.is_none());

	rig.connect(full_peer).await;
	let (out_peer, out_cids) = drain_next(&mut rig.outbound_req_rx).await.expect("outbound WANT");
	assert_eq!(out_peer, full_peer);
	assert_eq!(out_cids, vec![(cid, WantType::Block)]);
}

#[tokio::test(start_paused = true)]
async fn dont_have_from_only_peer_leaves_stream_open() {
	let mut rig = empty_rig();
	let peer = litep2p::PeerId::random();
	let cid = cid_for_data(BLAKE2B_256_MULTIHASH_CODE, b"the-real-payload");

	rig.connect(peer).await;
	let mut rx = rig.user_handle.request_stream(HashSet::from([cid])).unwrap();

	let _ = drain_next(&mut rig.outbound_req_rx).await.expect("outbound WANT");

	rig.send_presence(peer, cid, BlockPresenceType::DontHave).await;

	tokio::time::advance(Duration::from_secs(60)).await;
	assert!(matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
}

enum FailoverTrigger {
	DontHave,
	Timeout,
	Disconnect,
}

/// However the first peer fails the request — DONT_HAVE, an unanswered request timing
/// out, a block failing CID verification, or disconnecting — the want fails over to
/// the other connected peer.
#[rstest]
#[case::dont_have(FailoverTrigger::DontHave)]
#[case::timeout(FailoverTrigger::Timeout)]
#[case::disconnect(FailoverTrigger::Disconnect)]
#[tokio::test(start_paused = true)]
async fn first_peer_failure_triggers_failover(#[case] trigger: FailoverTrigger) {
	let mut rig = empty_rig();
	let peer_a = litep2p::PeerId::random();
	let peer_b = litep2p::PeerId::random();
	let data = b"failover-payload".to_vec();
	let cid = cid_for_data(BLAKE2B_256_MULTIHASH_CODE, &data);

	rig.connect(peer_a).await;
	rig.connect(peer_b).await;
	let mut rx = rig.user_handle.request_stream(HashSet::from([cid])).unwrap();
	let (first_peer, _) = drain_next(&mut rig.outbound_req_rx).await.expect("first WANT");

	match trigger {
		FailoverTrigger::DontHave => {
			rig.send_presence(first_peer, cid, BlockPresenceType::DontHave).await
		},
		FailoverTrigger::Timeout => {
			tokio::time::advance(PER_PEER_TIMEOUT + Duration::from_secs(2)).await
		},
		FailoverTrigger::Disconnect => {
			rig.sync_event_tx.send(sync_disconnected(first_peer)).await.unwrap()
		},
	}

	let (second_peer, _) = drain_next(&mut rig.outbound_req_rx).await.expect("failover WANT");
	assert_ne!(first_peer, second_peer);

	rig.send_block(second_peer, cid, &data).await;
	expect_block(&mut rx, cid, &data).await;
}

#[tokio::test(start_paused = true)]
async fn receiver_drop_cancels_wants() {
	let mut rig = empty_rig();
	let peer = litep2p::PeerId::random();
	let cid = cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [1u8; 32]);

	let rx = rig.user_handle.request_stream(HashSet::from([cid])).unwrap();

	// No peers connected: nothing is dispatched, the want just sits there.
	assert!(drain_next(&mut rig.outbound_req_rx).await.is_none());

	// The caller gives up; the sweep drops the abandoned waiter.
	drop(rx);
	tokio::time::advance(Duration::from_secs(2)).await;

	// A peer connecting afterwards must not trigger a WANT for the cancelled request.
	rig.connect(peer).await;
	assert!(drain_next(&mut rig.outbound_req_rx).await.is_none());
}

#[tokio::test(start_paused = true)]
async fn two_waiters_overlapping_cid_both_get_block() {
	let mut rig = empty_rig();
	let peer = litep2p::PeerId::random();
	let data = b"shared".to_vec();
	let cid = cid_for_data(BLAKE2B_256_MULTIHASH_CODE, &data);

	rig.connect(peer).await;

	let mut rx_a = rig.user_handle.request_stream(HashSet::from([cid])).unwrap();
	let mut rx_b = rig.user_handle.request_stream(HashSet::from([cid])).unwrap();

	let _ = drain_next(&mut rig.outbound_req_rx).await.expect("outbound");

	rig.send_block(peer, cid, &data).await;
	expect_block(&mut rx_a, cid, &data).await;
	expect_block(&mut rx_b, cid, &data).await;
}

#[tokio::test(start_paused = true)]
async fn waiter_drop_does_not_break_other_waiter() {
	let mut rig = empty_rig();
	let peer = litep2p::PeerId::random();
	let data = b"survivor".to_vec();
	let cid = cid_for_data(BLAKE2B_256_MULTIHASH_CODE, &data);

	rig.connect(peer).await;

	let rx_a = rig.user_handle.request_stream(HashSet::from([cid])).unwrap();
	let mut rx_b = rig.user_handle.request_stream(HashSet::from([cid])).unwrap();

	drop(rx_a);
	sleep(Duration::from_millis(1)).await;

	let _ = drain_next(&mut rig.outbound_req_rx).await.expect("outbound");

	rig.send_block(peer, cid, &data).await;
	expect_block(&mut rx_b, cid, &data).await;
}

#[tokio::test(start_paused = true)]
async fn dispatch_window_queues_excess_cids_and_promotes_on_delivery() {
	let mut rig = small_window_rig(4);
	let peer = litep2p::PeerId::random();
	rig.connect(peer).await;

	let payloads: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 8]).collect();
	let cids: Vec<Cid> =
		payloads.iter().map(|p| cid_for_data(BLAKE2B_256_MULTIHASH_CODE, p)).collect();

	let mut rx = rig.user_handle.request_stream(cids.iter().copied().collect()).unwrap();

	// Only the window (4 CIDs) is dispatched; the fifth is queued.
	let (_, entries) = drain_next(&mut rig.outbound_req_rx).await.expect("first bundle");
	assert_eq!(entries.len(), 4);
	let dispatched: HashSet<Cid> = entries.iter().map(|(cid, _)| *cid).collect();
	let queued_idx = cids.iter().position(|cid| !dispatched.contains(cid)).expect("one CID queued");

	// Answering one dispatched CID frees a slot and promotes the queued CID.
	let answered_idx = cids.iter().position(|cid| dispatched.contains(cid)).unwrap();
	rig.send_block(peer, cids[answered_idx], &payloads[answered_idx]).await;
	expect_block(&mut rx, cids[answered_idx], &payloads[answered_idx]).await;

	let (_, promoted) = drain_next(&mut rig.outbound_req_rx).await.expect("promoted WANT");
	assert_eq!(promoted, vec![(cids[queued_idx], WantType::Block)]);
}

#[tokio::test(start_paused = true)]
async fn inbound_request_with_known_block_serves_it() {
	let client = TestClientBuilder::with_tx_storage(u32::MAX).build();
	let mut block_builder = BlockBuilderBuilder::new(&client)
		.on_parent_block(client.chain_info().genesis_hash)
		.with_parent_block_number(0)
		.build()
		.unwrap();

	let ext = ExtrinsicBuilder::new_indexed_call(vec![0x42, 0x42, 0x42, 0x42]).build();
	let pattern_index = ext.encoded_size() - 4;
	let data_hash = sp_crypto_hashing::blake2_256(&ext.encode()[pattern_index..]);
	let cid = cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, data_hash);

	block_builder.push(ext.clone()).unwrap();
	let block = block_builder.build().unwrap().block;
	client.import(BlockOrigin::File, block).await.unwrap();

	let mut rig = build_rig_with(Arc::new(client), MAX_LIVE_CIDS);

	let peer = litep2p::PeerId::random();
	rig.send_wantlist(peer, vec![(cid, WantType::Block)]).await;

	let (resp_peer, responses) = drain_next(&mut rig.outbound_resp_rx).await.expect("response");
	assert_eq!(resp_peer, peer);
	assert_eq!(responses.len(), 1);
	match &responses[0] {
		ResponseType::Block { cid: got_cid, block } => {
			assert_eq!(*got_cid, cid);
			assert_eq!(*block, vec![0x42, 0x42, 0x42, 0x42]);
		},
		other => panic!("expected Block, got {other:?}"),
	}
}

type BlockchainResult<T> = sc_client_api::blockchain::Result<T>;

/// A backend whose `indexed_transaction` parks until the paired sender fires, keeping
/// an inbound lookup worker occupied for as long as the test needs.
struct GatedBackend {
	gate: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl BlockBackend<substrate_test_runtime::Block> for GatedBackend {
	fn block_body(
		&self,
		_hash: H256,
	) -> BlockchainResult<Option<Vec<substrate_test_runtime::Extrinsic>>> {
		unimplemented!()
	}

	fn block_indexed_body(&self, _hash: H256) -> BlockchainResult<Option<Vec<Vec<u8>>>> {
		unimplemented!()
	}

	fn block_indexed_hashes(&self, _hash: H256) -> BlockchainResult<Option<Vec<H256>>> {
		unimplemented!()
	}

	fn block(
		&self,
		_hash: H256,
	) -> BlockchainResult<Option<sp_runtime::generic::SignedBlock<substrate_test_runtime::Block>>>
	{
		unimplemented!()
	}

	fn block_status(&self, _hash: H256) -> BlockchainResult<sp_consensus::BlockStatus> {
		unimplemented!()
	}

	fn justifications(&self, _hash: H256) -> BlockchainResult<Option<sp_runtime::Justifications>> {
		unimplemented!()
	}

	fn block_hash(&self, _number: u64) -> BlockchainResult<Option<H256>> {
		unimplemented!()
	}

	fn indexed_transaction(&self, _hash: H256) -> BlockchainResult<Option<Vec<u8>>> {
		let _ = self.gate.lock().unwrap().recv();
		Ok(None)
	}

	fn requires_full_sync(&self) -> bool {
		false
	}
}

#[tokio::test]
async fn busy_pool_queues_wantlists_until_worker_frees() {
	let (gate_tx, gate_rx) = std::sync::mpsc::channel();
	let backend = Arc::new(GatedBackend { gate: std::sync::Mutex::new(gate_rx) });
	let mut rig = build_rig_with_inbound_limits(
		backend,
		MAX_LIVE_CIDS,
		1,
		MAX_QUEUED_INBOUND_ENTRIES_PER_PEER,
	);

	let peer = litep2p::PeerId::random();
	let first_cid = cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [0x0a; 32]);
	let second_cid = cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [0x0b; 32]);

	// The first wantlist occupies the only lookup worker (parked on the gate); the
	// second waits in the peer's queue instead of being refused.
	for cid in [first_cid, second_cid] {
		rig.send_wantlist(peer, vec![(cid, WantType::Block)]).await;
	}
	let no_response = timeout(Duration::from_millis(300), rig.outbound_resp_rx.recv()).await;
	assert!(no_response.is_err(), "nothing must be answered while the worker is parked");

	// Releasing the worker serves both wantlists, in arrival order.
	gate_tx.send(()).unwrap();
	let (resp_peer, responses) =
		drain_next(&mut rig.outbound_resp_rx).await.expect("first response");
	assert_eq!(resp_peer, peer);
	assert_single_dont_have(&responses, first_cid);

	gate_tx.send(()).unwrap();
	let (_, responses) = drain_next(&mut rig.outbound_resp_rx).await.expect("second response");
	assert_single_dont_have(&responses, second_cid);
}

#[tokio::test]
async fn inbound_queue_round_robins_between_peers() {
	let (gate_tx, gate_rx) = std::sync::mpsc::channel();
	let backend = Arc::new(GatedBackend { gate: std::sync::Mutex::new(gate_rx) });
	let mut rig = build_rig_with_inbound_limits(
		backend,
		MAX_LIVE_CIDS,
		1,
		MAX_QUEUED_INBOUND_ENTRIES_PER_PEER,
	);

	let peer_a = litep2p::PeerId::random();
	let peer_b = litep2p::PeerId::random();
	let wantlist = |tag: u8, len: usize| -> Vec<(Cid, WantType)> {
		(0..len as u8)
			.map(|i| {
				let mut digest = [tag; 32];
				digest[1] = i;
				(cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, digest), WantType::Block)
			})
			.collect()
	};

	// Peer A backlogs three batches: the first is dispatched immediately, two queue.
	rig.send_wantlist(peer_a, wantlist(0xaa, 3 * MAX_WANTED_BLOCKS)).await;
	// Peer B queues one batch behind A's backlog.
	rig.send_wantlist(peer_b, wantlist(0xbb, MAX_WANTED_BLOCKS)).await;

	// Release one batch worth of lookups at a time and record who gets answered.
	let mut served_order = Vec::new();
	for _ in 0..4 {
		for _ in 0..MAX_WANTED_BLOCKS {
			gate_tx.send(()).unwrap();
		}
		let (resp_peer, responses) =
			drain_next(&mut rig.outbound_resp_rx).await.expect("batch response");
		assert_eq!(responses.len(), MAX_WANTED_BLOCKS);
		served_order.push(resp_peer);
	}

	// Round-robin: B's batch is interleaved into A's backlog instead of waiting for
	// all of it. (A goes twice first: its second batch re-entered the rotation
	// before B arrived.)
	assert_eq!(served_order, vec![peer_a, peer_a, peer_b, peer_a]);
}

#[tokio::test]
async fn per_peer_queue_overflow_drops_newest_entries() {
	let (gate_tx, gate_rx) = std::sync::mpsc::channel();
	let backend = Arc::new(GatedBackend { gate: std::sync::Mutex::new(gate_rx) });
	// Single gated worker, queue capped at 4 entries per peer.
	let mut rig = build_rig_with_inbound_limits(backend, MAX_LIVE_CIDS, 1, 4);

	let peer = litep2p::PeerId::random();
	let cid = |i: u8| cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [i; 32]);

	// The first wantlist occupies the worker; the next four fill the queue; the
	// sixth overflows the per-peer cap and is dropped silently.
	for i in 0..6u8 {
		rig.send_wantlist(peer, vec![(cid(i), WantType::Block)]).await;
	}

	// Serve everything: the five accepted entries are answered, the sixth never is.
	for _ in 0..5 {
		gate_tx.send(()).unwrap();
	}
	let mut answered = HashSet::new();
	while answered.len() < 5 {
		let (_, responses) = drain_next(&mut rig.outbound_resp_rx).await.expect("response");
		for response in responses {
			match response {
				ResponseType::Presence { cid, presence: BlockPresenceType::DontHave } => {
					assert!(answered.insert(cid), "duplicate reply for {cid}");
				},
				other => panic!("expected DONT_HAVE, got {other:?}"),
			}
		}
	}
	assert_eq!(answered, (0..5u8).map(cid).collect());
	let no_more = timeout(Duration::from_millis(300), rig.outbound_resp_rx.recv()).await;
	assert!(no_more.is_err(), "the overflowed entry must not be answered");
}

#[tokio::test]
async fn large_wantlist_is_served_in_batches_covering_every_entry() {
	let mut rig = empty_rig();
	let peer = litep2p::PeerId::random();

	let cids: Vec<(Cid, WantType)> = (0..(MAX_WANTED_BLOCKS + 2) as u8)
		.map(|i| {
			let mut digest = [0u8; 32];
			digest[0] = i;
			(cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, digest), WantType::Block)
		})
		.collect();

	rig.send_wantlist(peer, cids.clone()).await;

	// A wantlist larger than `MAX_WANTED_BLOCKS` is accepted whole and served in
	// batches. Every entry gets a reply (DONT_HAVE: the test client holds no
	// indexed data).
	let mut answered = HashSet::new();
	while answered.len() < cids.len() {
		let (resp_peer, responses) =
			drain_next(&mut rig.outbound_resp_rx).await.expect("reply for every entry");
		assert_eq!(resp_peer, peer);
		for response in responses {
			match response {
				ResponseType::Presence { cid, presence: BlockPresenceType::DontHave } => {
					assert!(answered.insert(cid), "duplicate reply for {cid}");
				},
				other => panic!("expected DONT_HAVE, got {other:?}"),
			}
		}
	}
	assert_eq!(answered, cids.into_iter().map(|(cid, _)| cid).collect());
}

#[tokio::test]
async fn unsupported_cid_in_wantlist_gets_dont_have() {
	let mut rig = empty_rig();
	let peer = litep2p::PeerId::random();
	let unsupported = Cid::new_v1(
		RAW_CODEC,
		CidMultihash::<64>::wrap(0x99 /* unsupported */, &[0u8; 32]).unwrap(),
	);

	rig.send_wantlist(peer, vec![(unsupported, WantType::Block)]).await;

	let (resp_peer, responses) = drain_next(&mut rig.outbound_resp_rx).await.expect("reply");
	assert_eq!(resp_peer, peer);
	assert_single_dont_have(&responses, unsupported);
}

#[tokio::test(start_paused = true)]
async fn inbound_serving_continues_after_all_handles_dropped() {
	let mut rig = empty_rig();
	let peer = litep2p::PeerId::random();
	let cid = cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [3u8; 32]);

	drop(rig.user_handle);
	sleep(Duration::from_millis(1)).await;

	rig.inbound_tx
		.send(TransportEvent::Request { peer, cids: vec![(cid, WantType::Block)] })
		.await
		.unwrap();

	let (resp_peer, responses) = drain_next(&mut rig.outbound_resp_rx)
		.await
		.expect("service must keep serving inbound after all handles are dropped");
	assert_eq!(resp_peer, peer);
	assert_single_dont_have(&responses, cid);
}

#[tokio::test(start_paused = true)]
async fn outbound_wants_bundled_and_split_at_message_cap() {
	let mut rig = empty_rig();
	let peer = litep2p::PeerId::random();
	rig.connect(peer).await;

	let cids: Vec<Cid> = (0..=MAX_WANTED_BLOCKS as u8)
		.map(|i| {
			let mut d = [0u8; 32];
			d[0] = i;
			cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, d)
		})
		.collect();

	let _rx = rig.user_handle.request_stream(cids.iter().copied().collect()).unwrap();

	let (peer_a, first) = drain_next(&mut rig.outbound_req_rx).await.expect("first bundle");
	let (peer_b, second) = drain_next(&mut rig.outbound_req_rx).await.expect("second bundle");
	assert_eq!(peer_a, peer);
	assert_eq!(peer_b, peer);

	let mut sizes = [first.len(), second.len()];
	sizes.sort();
	assert_eq!(sizes, [1, MAX_WANTED_BLOCKS]);

	let sent: HashSet<Cid> = first.into_iter().chain(second).map(|(cid, _)| cid).collect();
	assert_eq!(sent, cids.into_iter().collect::<HashSet<_>>());
}

#[tokio::test]
async fn admission_invalid_cid_rejected() {
	let rig = empty_rig();
	let bad = Cid::new_v1(
		RAW_CODEC,
		CidMultihash::<64>::wrap(0x99 /* unsupported */, &[0u8; 32]).unwrap(),
	);

	let err = rig.user_handle.request_stream(HashSet::from([bad])).err().expect("err");
	assert!(matches!(err, BitswapError::InvalidCid { .. }));
}

#[tokio::test]
async fn admission_empty_returns_closed_receiver() {
	let rig = empty_rig();
	let mut rx = rig.user_handle.request_stream(HashSet::new()).unwrap();
	assert!(rx.recv().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn service_shutdown_emits_service_closed() {
	let mut rig = empty_rig();
	let peer = litep2p::PeerId::random();
	let cid = cid_for_digest(BLAKE2B_256_MULTIHASH_CODE, [0xee; 32]);

	rig.connect(peer).await;
	let mut rx = rig.user_handle.request_stream(HashSet::from([cid])).unwrap();
	let _ = drain_next(&mut rig.outbound_req_rx).await;

	drop(rig.inbound_tx);

	let item = drain_next(&mut rx).await.expect("expect Err");
	assert!(matches!(item, Err(BitswapError::ServiceClosed)));
}

#[tokio::test(start_paused = true)]
async fn late_response_after_receiver_drop_is_ignored_and_cid_refetchable() {
	let mut rig = empty_rig();
	let peer = litep2p::PeerId::random();
	let data = b"too-late".to_vec();
	let cid = cid_for_data(BLAKE2B_256_MULTIHASH_CODE, &data);

	rig.connect(peer).await;
	let rx = rig.user_handle.request_stream(HashSet::from([cid])).unwrap();
	let _ = drain_next(&mut rig.outbound_req_rx).await.expect("outbound");

	// Caller gives up; the sweep drops the waiter while the peer request is still
	// in flight.
	drop(rx);
	tokio::time::advance(Duration::from_secs(2)).await;

	// The late response hits no waiter and is dropped.
	rig.send_block(peer, cid, &data).await;
	// Let the actor process the late response before the fresh request goes in.
	sleep(Duration::from_millis(1)).await;

	// A fresh request for the same CID starts from a clean slate: the peer is asked
	// again and the block is delivered.
	let mut rx = rig.user_handle.request_stream(HashSet::from([cid])).unwrap();
	let _ = drain_next(&mut rig.outbound_req_rx).await.expect("fresh WANT");
	rig.send_block(peer, cid, &data).await;
	expect_block(&mut rx, cid, &data).await;
}

mod proptests {
	use super::*;
	use crate::{BLAKE2B_256_MULTIHASH_CODE, RAW_CODEC};
	use cid::multihash::Multihash as CidMultihash;
	use proptest::prelude::*;
	use rand::{rngs::StdRng, SeedableRng};

	const NUM_CIDS: u8 = 8;
	const NUM_PEERS: usize = 4;
	const SMALL_WINDOW: usize = 3;

	#[derive(Debug, Clone)]
	enum Op {
		AddWaiter(u8),
		RemoveWaiter(usize),
		Deliver(u8),
		MarkPeerDone(usize, u8),
		Connect(usize),
		Disconnect(usize),
		Sweep(u64),
	}

	fn op_strategy() -> impl Strategy<Value = Op> {
		prop_oneof![
			(0..NUM_CIDS).prop_map(Op::AddWaiter),
			any::<usize>().prop_map(Op::RemoveWaiter),
			(0..NUM_CIDS).prop_map(Op::Deliver),
			(0..NUM_PEERS, 0..NUM_CIDS).prop_map(|(p, c)| Op::MarkPeerDone(p, c)),
			(0..NUM_PEERS).prop_map(Op::Connect),
			(0..NUM_PEERS).prop_map(Op::Disconnect),
			(1u64..8).prop_map(Op::Sweep),
		]
	}

	struct Harness {
		scheduler: RequestScheduler,
		user_requests: SlotMap<UserRequestId, Cid>,
		peers: Vec<litep2p::PeerId>,
		cids: Vec<Cid>,
		connected: HashSet<litep2p::PeerId>,
		now: Instant,
		rng: StdRng,
	}

	impl Harness {
		fn new() -> Self {
			let cids = (0..NUM_CIDS)
				.map(|i| {
					let mh =
						CidMultihash::<64>::wrap(BLAKE2B_256_MULTIHASH_CODE, &[i; 32]).unwrap();
					Cid::new_v1(RAW_CODEC, mh)
				})
				.collect();
			Self {
				scheduler: RequestScheduler::new(SMALL_WINDOW),
				user_requests: SlotMap::with_key(),
				peers: (0..NUM_PEERS).map(|_| litep2p::PeerId::random()).collect(),
				cids,
				connected: HashSet::new(),
				now: Instant::now(),
				rng: StdRng::seed_from_u64(0),
			}
		}

		fn apply(&mut self, op: Op) {
			match op {
				Op::AddWaiter(c) => {
					let cid = self.cids[c as usize];
					let id = self.user_requests.insert(cid);
					self.scheduler.add_user_request(cid, id);
				},
				Op::RemoveWaiter(seed) => {
					let Some(id) =
						self.user_requests.keys().nth(seed % self.user_requests.len().max(1))
					else {
						return;
					};
					let cid = self.user_requests.remove(id).expect("listed waiter exists");
					self.scheduler.remove_user_request(cid, id);
				},
				Op::Deliver(c) => {
					let waiter_ids = self
						.scheduler
						.take_user_requests_for_delivered_cid(self.cids[c as usize])
						.unwrap_or_default();
					for id in waiter_ids {
						self.user_requests.remove(id);
					}
				},
				Op::MarkPeerDone(p, c) => {
					self.scheduler.mark_peer_done_for_cid(self.peers[p], self.cids[c as usize])
				},
				Op::Connect(p) => {
					self.connected.insert(self.peers[p]);
				},
				Op::Disconnect(p) => {
					self.connected.remove(&self.peers[p]);
					let _ = self.scheduler.remove_in_flight_peer(self.peers[p]);
				},
				Op::Sweep(secs) => {
					self.now += Duration::from_secs(secs);
					let _ = self.scheduler.expire_peer_timeouts(self.now);
					let _ = self.scheduler.restart_exhausted_rounds(self.now);
				},
			}
		}

		fn top_up(&mut self) {
			for cid in self.scheduler.all_cids() {
				let _ = self.scheduler.next_peer_to_request(
					cid,
					&self.connected,
					self.now,
					&mut self.rng,
				);
			}
			while self.scheduler.has_window_capacity() {
				let Some(cid) = self.scheduler.pop_pending() else { break };
				let _ = self.scheduler.next_peer_to_request(
					cid,
					&self.connected,
					self.now,
					&mut self.rng,
				);
			}
		}

		fn check_invariants(&self) {
			let live = self
				.scheduler
				.cid_states
				.values()
				.filter(|state| matches!(state.phase, CidRequestPhase::InFlight { .. }))
				.count();
			assert_eq!(self.scheduler.live_cids, live, "live counter drifted");
			assert!(
				self.scheduler.live_cids <= self.scheduler.max_live_cids,
				"dispatch window overrun"
			);
			let queued =
				self.scheduler.cid_states.values().filter(|state| state.is_queued()).count();
			assert_eq!(self.scheduler.queued_cids, queued, "queued counter drifted");

			for (cid, state) in &self.scheduler.cid_states {
				assert!(!state.is_idle(), "idle entry retained for {cid}");
				if let CidRequestPhase::InFlight { peer, .. } = state.phase {
					assert!(!state.tried_peers.contains(&peer), "peer both tried and in flight");
				}
				if state.is_queued() {
					assert!(
						self.scheduler.pending.contains(cid),
						"queued CID without queue entry for {cid}",
					);
				}
				let retry_at = match state.phase {
					CidRequestPhase::Queued { retry_at } => retry_at,
					CidRequestPhase::RetryAt(at) => Some(at),
					CidRequestPhase::Ready | CidRequestPhase::InFlight { .. } => None,
				};
				if let Some(at) = retry_at {
					assert!(at > self.now, "overdue round never restarted for {cid}");
				}
				if state.has_user_requests() && !self.connected.is_empty() {
					assert!(
						matches!(
							state.phase,
							CidRequestPhase::Queued { .. } |
								CidRequestPhase::InFlight { .. } |
								CidRequestPhase::RetryAt(_)
						),
						"stranded CID {cid}",
					);
				}
			}
		}
	}

	proptest! {
		#[test]
		fn want_set_invariants_hold(ops in prop::collection::vec(op_strategy(), 1..256)) {
			let mut harness = Harness::new();
			for op in ops {
				harness.apply(op);
				harness.top_up();
				harness.check_invariants();
			}
		}
	}
}
