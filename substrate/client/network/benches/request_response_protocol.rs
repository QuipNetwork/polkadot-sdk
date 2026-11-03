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

use criterion::{
	criterion_group, criterion_main, AxisScale, BenchmarkId, Criterion, PlotConfiguration,
	Throughput,
};
use sc_network::{
	config::{
		FullNetworkConfiguration, IncomingRequest, NetworkConfiguration, NonReservedPeerMode,
		NotificationHandshake, OutgoingResponse, Params, ProtocolId, Role, SetConfig,
	},
	service::traits::NetworkService,
	IfDisconnected, Litep2pNetworkBackend, NetworkBackend, NetworkRequest, NetworkWorker,
	NotificationMetrics, NotificationService, PeerId, Roles,
};
use sc_network_common::{sync::message::BlockAnnouncesHandshake, ExHashT};
use sp_core::H256;
use sp_runtime::traits::{Block as BlockT, Zero};
use std::{sync::Arc, time::Duration};
use substrate_test_runtime_client::runtime;
use tokio::{sync::Mutex, task::JoinHandle};

const MAX_SIZE: u64 = 2u64.pow(30);
const NUMBER_OF_REQUESTS: usize = 100;
const PAYLOAD: &[(u32, &'static str)] = &[
	// (Exponent of size, label)
	(6, "64B"),
	(9, "512B"),
	(12, "4KB"),
	(15, "64KB"),
	(18, "256KB"),
	(21, "2MB"),
	(24, "16MB"),
];

pub fn create_network_worker<B, H, N>() -> (
	N,
	Arc<dyn NetworkService>,
	async_channel::Receiver<IncomingRequest>,
	Arc<Mutex<Box<dyn NotificationService>>>,
)
where
	B: BlockT<Hash = H256> + 'static,
	H: ExHashT,
	N: NetworkBackend<B, H>,
{
	let (tx, rx) = async_channel::bounded(10);
	let request_response_config = N::request_response_config(
		"/request-response/1".into(),
		vec![],
		MAX_SIZE,
		MAX_SIZE,
		Duration::from_secs(2),
		Some(tx),
	);
	let role = Role::Full;
	let net_conf = NetworkConfiguration::new_local();
	let mut network_config = FullNetworkConfiguration::new(&net_conf, None);
	network_config.add_request_response_protocol(request_response_config);
	let genesis_hash = runtime::Hash::zero();
	let (block_announce_config, notification_service) = N::notification_config(
		"/block-announces/1".into(),
		vec![],
		1024,
		Some(NotificationHandshake::new(BlockAnnouncesHandshake::<runtime::Block>::build(
			Roles::from(&Role::Full),
			Zero::zero(),
			genesis_hash,
			genesis_hash,
		))),
		SetConfig {
			in_peers: 1,
			out_peers: 1,
			reserved_nodes: vec![],
			non_reserved_mode: NonReservedPeerMode::Accept,
		},
		NotificationMetrics::new(None),
		network_config.peer_store_handle(),
	);
	let worker = N::new(Params::<B, H, N> {
		block_announce_config,
		role,
		executor: Box::new(|f| {
			tokio::spawn(f);
		}),
		genesis_hash: runtime::Hash::zero(),
		network_config,
		protocol_id: ProtocolId::from("bench-request-response-protocol"),
		fork_id: None,
		metrics_registry: None,
		ipfs_config: None,
		notification_metrics: NotificationMetrics::new(None),
	})
	.unwrap();
	let notification_service = Arc::new(Mutex::new(notification_service));
	let network_service = worker.network_service();

	(worker, network_service, rx, notification_service)
}

struct BenchSetup {
	#[allow(dead_code)]
	notification_service1: Arc<Mutex<Box<dyn NotificationService>>>,
	#[allow(dead_code)]
	notification_service2: Arc<Mutex<Box<dyn NotificationService>>>,
	network_service1: Arc<dyn NetworkService>,
	peer_id2: PeerId,
	handle1: JoinHandle<()>,
	handle2: JoinHandle<()>,
	#[allow(dead_code)]
	rx1: async_channel::Receiver<IncomingRequest>,
	rx2: async_channel::Receiver<IncomingRequest>,
}

impl Drop for BenchSetup {
	fn drop(&mut self) {
		self.handle1.abort();
		self.handle2.abort();
	}
}

fn setup_workers<B, H, N>(rt: &tokio::runtime::Runtime) -> Arc<BenchSetup>
where
	B: BlockT<Hash = H256> + 'static,
	H: ExHashT,
	N: NetworkBackend<B, H>,
{
	let _guard = rt.enter();

	let (worker1, network_service1, rx1, notification_service1) =
		create_network_worker::<B, H, N>();
	let (worker2, network_service2, rx2, notification_service2) =
		create_network_worker::<B, H, N>();
	let peer_id2 = worker2.network_service().local_peer_id();
	let handle1 = tokio::spawn(worker1.run());
	let handle2 = tokio::spawn(worker2.run());

	let _ = tokio::spawn({
		let rx2 = rx2.clone();

		async move {
			let req = rx2.recv().await.unwrap();
			req.pending_response
				.send(OutgoingResponse {
					result: Ok(vec![0; 2usize.pow(25)]),
					reputation_changes: vec![],
					sent_feedback: None,
				})
				.unwrap();
		}
	});

	let ready = tokio::spawn({
		let network_service1 = Arc::clone(&network_service1);

		async move {
			let listen_address2 = {
				while network_service2.listen_addresses().is_empty() {
					tokio::time::sleep(Duration::from_millis(10)).await;
				}
				network_service2.listen_addresses()[0].clone()
			};
			network_service1.add_known_address(peer_id2, listen_address2.into());
			let _ = network_service1
				.request(
					peer_id2.into(),
					"/request-response/1".into(),
					vec![0; 2],
					None,
					IfDisconnected::TryConnect,
				)
				.await
				.unwrap();
		}
	});

	tokio::task::block_in_place(|| {
		let _ = tokio::runtime::Handle::current().block_on(ready);
	});

	Arc::new(BenchSetup {
		notification_service1,
		notification_service2,
		network_service1,
		peer_id2,
		handle1,
		handle2,
		rx1,
		rx2,
	})
}

async fn run_serially(setup: Arc<BenchSetup>, size: usize, limit: usize) {
	let (break_tx, break_rx) = async_channel::bounded(1);
	let network1 = tokio::spawn({
		let network_service1 = Arc::clone(&setup.network_service1);
		let peer_id2 = setup.peer_id2;
		async move {
			for _ in 0..limit {
				let _ = network_service1
					.request(
						peer_id2.into(),
						"/request-response/1".into(),
						vec![0; 2],
						None,
						IfDisconnected::TryConnect,
					)
					.await
					.unwrap();
			}
			let _ = break_tx.send(()).await;
		}
	});
	let network2 = tokio::spawn({
		let rx2 = setup.rx2.clone();
		async move {
			loop {
				tokio::select! {
					req = rx2.recv() => {
						let IncomingRequest { pending_response, .. } = req.unwrap();
						pending_response.send(OutgoingResponse {
							result: Ok(vec![0; size]),
							reputation_changes: vec![],
							sent_feedback: None,
						}).unwrap();
					},
					_ = break_rx.recv() => break,
				}
			}
		}
	});

	let _ = tokio::join!(network1, network2);
}

// The libp2p request-response implementation does not provide any backpressure feedback.
// So this benchmark is useless until we implement it for litep2p.
#[allow(dead_code)]
async fn run_with_backpressure(setup: Arc<BenchSetup>, size: usize, limit: usize) {
	let (break_tx, break_rx) = async_channel::bounded(1);
	let requests = futures::future::join_all((0..limit).into_iter().map(|_| {
		let (tx, rx) = futures::channel::oneshot::channel();
		setup.network_service1.start_request(
			setup.peer_id2.into(),
			"/request-response/1".into(),
			vec![0; 8],
			None,
			tx,
			IfDisconnected::TryConnect,
		);
		rx
	}));

	let network1 = tokio::spawn(async move {
		let responses = requests.await;
		for res in responses {
			res.unwrap().unwrap();
		}
		let _ = break_tx.send(()).await;
	});
	let network2 = tokio::spawn(async move {
		for _ in 0..limit {
			let IncomingRequest { pending_response, .. } = setup.rx2.recv().await.unwrap();
			pending_response
				.send(OutgoingResponse {
					result: Ok(vec![0; size]),
					reputation_changes: vec![],
					sent_feedback: None,
				})
				.unwrap();
		}
		break_rx.recv().await
	});

	let _ = tokio::join!(network1, network2);
}

fn run_benchmark(c: &mut Criterion) {
	let rt = tokio::runtime::Runtime::new().unwrap();
	let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
	let mut group = c.benchmark_group("request_response_protocol");
	group.plot_config(plot_config);
	group.sample_size(10);

	let libp2p_setup = setup_workers::<runtime::Block, runtime::Hash, NetworkWorker<_, _>>(&rt);
	for &(exponent, label) in PAYLOAD.iter() {
		let size = 2usize.pow(exponent);
		group.throughput(Throughput::Bytes(NUMBER_OF_REQUESTS as u64 * size as u64));
		group.bench_with_input(BenchmarkId::new("libp2p/serially", label), &size, |b, &size| {
			b.to_async(&rt)
				.iter(|| run_serially(Arc::clone(&libp2p_setup), size, NUMBER_OF_REQUESTS));
		});
	}
	drop(libp2p_setup);

	let litep2p_setup = setup_workers::<runtime::Block, runtime::Hash, Litep2pNetworkBackend>(&rt);
	for &(exponent, label) in PAYLOAD.iter() {
		let size = 2usize.pow(exponent);
		group.throughput(Throughput::Bytes(NUMBER_OF_REQUESTS as u64 * size as u64));
		group.bench_with_input(BenchmarkId::new("litep2p/serially", label), &size, |b, &size| {
			b.to_async(&rt)
				.iter(|| run_serially(Arc::clone(&litep2p_setup), size, NUMBER_OF_REQUESTS));
		});
	}
	drop(litep2p_setup);
}

criterion_group!(benches, run_benchmark);
criterion_main!(benches);
