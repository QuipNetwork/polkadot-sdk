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
		FullNetworkConfiguration, MultiaddrWithPeerId, NetworkConfiguration, NonReservedPeerMode,
		NotificationHandshake, Params, ProtocolId, Role, SetConfig,
	},
	service::traits::{NetworkService, NotificationEvent},
	Litep2pNetworkBackend, NetworkBackend, NetworkWorker, NotificationMetrics, NotificationService,
	PeerId, Roles,
};
use sc_network_common::{sync::message::BlockAnnouncesHandshake, ExHashT};
use sp_core::H256;
use sp_runtime::traits::{Block as BlockT, Zero};
use std::{sync::Arc, time::Duration};
use substrate_test_runtime_client::runtime;
use tokio::{sync::Mutex, task::JoinHandle};

const NUMBER_OF_NOTIFICATIONS: usize = 100;
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
const MAX_SIZE: u64 = 2u64.pow(30);

fn create_network_worker<B, H, N>(
) -> (N, Arc<dyn NetworkService>, Arc<Mutex<Box<dyn NotificationService>>>)
where
	B: BlockT<Hash = H256> + 'static,
	H: ExHashT,
	N: NetworkBackend<B, H>,
{
	let role = Role::Full;
	let net_conf = NetworkConfiguration::new_local();
	let network_config = FullNetworkConfiguration::<B, H, N>::new(&net_conf, None);
	let genesis_hash = runtime::Hash::zero();
	let (block_announce_config, notification_service) = N::notification_config(
		"/block-announces/1".into(),
		vec!["/bench-notifications-protocol/block-announces/1".into()],
		MAX_SIZE,
		Some(NotificationHandshake::new(BlockAnnouncesHandshake::<runtime::Block>::build(
			Roles::from(&role),
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
		genesis_hash,
		network_config,
		protocol_id: ProtocolId::from("bench-protocol-name"),
		fork_id: None,
		metrics_registry: None,
		ipfs_config: None,
		notification_metrics: NotificationMetrics::new(None),
	})
	.unwrap();
	let network_service = worker.network_service();
	let notification_service = Arc::new(Mutex::new(notification_service));

	(worker, network_service, notification_service)
}

struct BenchSetup {
	notification_service1: Arc<Mutex<Box<dyn NotificationService>>>,
	notification_service2: Arc<Mutex<Box<dyn NotificationService>>>,
	peer_id2: PeerId,
	handle1: JoinHandle<()>,
	handle2: JoinHandle<()>,
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

	let (worker1, network_service1, notification_service1) = create_network_worker::<B, H, N>();
	let (worker2, network_service2, notification_service2) = create_network_worker::<B, H, N>();
	let peer_id2: sc_network::PeerId = network_service2.local_peer_id().into();
	let handle1 = tokio::spawn(worker1.run());
	let handle2 = tokio::spawn(worker2.run());

	let ready = tokio::spawn({
		let notification_service1 = Arc::clone(&notification_service1);
		let notification_service2 = Arc::clone(&notification_service2);

		async move {
			let listen_address2 = {
				while network_service2.listen_addresses().is_empty() {
					tokio::time::sleep(Duration::from_millis(10)).await;
				}
				network_service2.listen_addresses()[0].clone()
			};
			network_service1
				.add_reserved_peer(MultiaddrWithPeerId {
					multiaddr: listen_address2,
					peer_id: peer_id2,
				})
				.unwrap();

			let mut notification_service1 = notification_service1.lock().await;
			let mut notification_service2 = notification_service2.lock().await;
			loop {
				tokio::select! {
					Some(event) = notification_service1.next_event() => {
						if let NotificationEvent::NotificationStreamOpened { .. } = event {
							// Send a 32MB notification to preheat the network
							notification_service1.send_async_notification(&peer_id2, vec![0; 2usize.pow(25)]).await.unwrap();
						}
					},
					Some(event) = notification_service2.next_event() => {
						match event {
							NotificationEvent::ValidateInboundSubstream { result_tx, .. } => {
								result_tx.send(sc_network::service::traits::ValidationResult::Accept).unwrap();
							},
							NotificationEvent::NotificationReceived { .. } => {
								break;
							}
							_ => {}
						}
					},
				}
			}
		}
	});

	tokio::task::block_in_place(|| {
		let _ = tokio::runtime::Handle::current().block_on(ready);
	});

	Arc::new(BenchSetup {
		notification_service1,
		notification_service2,
		peer_id2,
		handle1,
		handle2,
	})
}

async fn run_serially(setup: Arc<BenchSetup>, size: usize, limit: usize) {
	let (tx, rx) = async_channel::bounded(1);
	let _ = tx.send(Some(())).await;
	let network1 = tokio::spawn({
		let notification_service1 = Arc::clone(&setup.notification_service1);
		let peer_id2 = setup.peer_id2;
		async move {
			let mut notification_service1 = notification_service1.lock().await;
			while let Ok(message) = rx.recv().await {
				let Some(_) = message else { break };
				notification_service1
					.send_async_notification(&peer_id2, vec![0; size])
					.await
					.unwrap();
			}
		}
	});
	let network2 = tokio::spawn({
		let notification_service2 = Arc::clone(&setup.notification_service2);
		async move {
			let mut notification_service2 = notification_service2.lock().await;
			let mut received_counter = 0;
			while let Some(event) = notification_service2.next_event().await {
				if let NotificationEvent::NotificationReceived { .. } = event {
					received_counter += 1;
					if received_counter >= limit {
						let _ = tx.send(None).await;
						break;
					}
					let _ = tx.send(Some(())).await;
				}
			}
		}
	});

	let _ = tokio::join!(network1, network2);
}

async fn run_with_backpressure(setup: Arc<BenchSetup>, size: usize, limit: usize) {
	let (tx, rx) = async_channel::bounded(1);
	let network1 = tokio::spawn({
		let setup = Arc::clone(&setup);
		async move {
			let mut notification_service1 = setup.notification_service1.lock().await;
			for _ in 0..limit {
				notification_service1
					.send_async_notification(&setup.peer_id2, vec![0; size])
					.await
					.unwrap();
			}
			let _ = rx.recv().await;
		}
	});
	let network2 = tokio::spawn({
		let setup = Arc::clone(&setup);
		async move {
			let mut notification_service2 = setup.notification_service2.lock().await;
			let mut received_counter = 0;
			while let Some(event) = notification_service2.next_event().await {
				if let NotificationEvent::NotificationReceived { .. } = event {
					received_counter += 1;
					if received_counter >= limit {
						let _ = tx.send(()).await;
						break;
					}
				}
			}
		}
	});

	let _ = tokio::join!(network1, network2);
}

fn run_benchmark(c: &mut Criterion) {
	let rt = tokio::runtime::Runtime::new().unwrap();
	let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
	let mut group = c.benchmark_group("notifications_protocol");
	group.plot_config(plot_config);
	group.sample_size(10);

	let libp2p_setup = setup_workers::<runtime::Block, runtime::Hash, NetworkWorker<_, _>>(&rt);
	for &(exponent, label) in PAYLOAD.iter() {
		let size = 2usize.pow(exponent);
		group.throughput(Throughput::Bytes(NUMBER_OF_NOTIFICATIONS as u64 * size as u64));
		group.bench_with_input(BenchmarkId::new("libp2p/serially", label), &size, |b, &size| {
			b.to_async(&rt)
				.iter(|| run_serially(Arc::clone(&libp2p_setup), size, NUMBER_OF_NOTIFICATIONS));
		});
		group.bench_with_input(
			BenchmarkId::new("libp2p/with_backpressure", label),
			&size,
			|b, &size| {
				b.to_async(&rt).iter(|| {
					run_with_backpressure(Arc::clone(&libp2p_setup), size, NUMBER_OF_NOTIFICATIONS)
				});
			},
		);
	}
	drop(libp2p_setup);

	let litep2p_setup = setup_workers::<runtime::Block, runtime::Hash, Litep2pNetworkBackend>(&rt);
	for &(exponent, label) in PAYLOAD.iter() {
		let size = 2usize.pow(exponent);
		group.throughput(Throughput::Bytes(NUMBER_OF_NOTIFICATIONS as u64 * size as u64));
		group.bench_with_input(BenchmarkId::new("litep2p/serially", label), &size, |b, &size| {
			b.to_async(&rt)
				.iter(|| run_serially(Arc::clone(&litep2p_setup), size, NUMBER_OF_NOTIFICATIONS));
		});
		group.bench_with_input(
			BenchmarkId::new("litep2p/with_backpressure", label),
			&size,
			|b, &size| {
				b.to_async(&rt).iter(|| {
					run_with_backpressure(Arc::clone(&litep2p_setup), size, NUMBER_OF_NOTIFICATIONS)
				});
			},
		);
	}
	drop(litep2p_setup);
}

criterion_group!(benches, run_benchmark);
criterion_main!(benches);
