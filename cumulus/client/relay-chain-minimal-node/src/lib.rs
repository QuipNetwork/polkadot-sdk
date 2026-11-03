// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus. If not, see <https://www.gnu.org/licenses/>.

use collator_overseer::NewMinimalNode;

use cumulus_client_bootnodes::bootnode_request_response_config;
use cumulus_relay_chain_interface::{RelayChainError, RelayChainInterface, RelayChainResult};
use cumulus_relay_chain_rpc_interface::{RelayChainRpcClient, RelayChainRpcInterface, Url};
use network::build_collator_network;
use polkadot_network_bridge::{peer_sets_info, IsAuthority};
use polkadot_node_network_protocol::{
	peer_set::{PeerSet, PeerSetProtocolNames},
	request_response::{
		v1, v2, v3, IncomingRequest, IncomingRequestReceiver, Protocol, ReqProtocolNames,
	},
};

use polkadot_core_primitives::{Block as RelayBlock, Hash as RelayHash};
use polkadot_node_subsystem_util::metrics::prometheus::Registry;
use polkadot_primitives::CollatorPair;
use polkadot_service::{overseer::OverseerGenArgs, IsParachainNode};

use sc_authority_discovery::Service as AuthorityDiscoveryService;
use sc_network::{
	config::FullNetworkConfiguration, request_responses::IncomingRequest as GenericIncomingRequest,
	service::traits::NetworkService, Event, NetworkBackend, NetworkEventStream,
};
use sc_service::{config::PrometheusConfig, Configuration, TaskManager};
use sp_runtime::{app_crypto::Pair, traits::Block as BlockT};

use futures::{FutureExt, StreamExt};
use std::sync::Arc;

mod blockchain_rpc_client;
mod collator_overseer;
mod network;

pub use blockchain_rpc_client::BlockChainRpcClient;

const LOG_TARGET: &str = "minimal-relaychain-node";

fn build_authority_discovery_service<Block: BlockT>(
	task_manager: &TaskManager,
	client: Arc<BlockChainRpcClient>,
	config: &Configuration,
	network: Arc<dyn NetworkService>,
	prometheus_registry: Option<Registry>,
) -> AuthorityDiscoveryService {
	let auth_disc_publish_non_global_ips = config.network.allow_non_globals_in_dht;
	let auth_disc_public_addresses = config.network.public_addresses.clone();
	let authority_discovery_role = sc_authority_discovery::Role::Discover;
	let dht_event_stream = network.event_stream("authority-discovery").filter_map(|e| async move {
		match e {
			Event::Dht(e) => Some(e),
			_ => None,
		}
	});
	let net_config_path = config.network.net_config_path.clone();
	let (worker, service) = sc_authority_discovery::new_worker_and_service_with_config(
		sc_authority_discovery::WorkerConfig {
			publish_non_global_ips: auth_disc_publish_non_global_ips,
			public_addresses: auth_disc_public_addresses,
			// Require that authority discovery records are signed.
			strict_record_validation: true,
			persisted_cache_directory: net_config_path,
			..Default::default()
		},
		client,
		Arc::new(network.clone()),
		Box::pin(dht_event_stream),
		authority_discovery_role,
		prometheus_registry,
		task_manager.spawn_handle(),
	);

	task_manager.spawn_handle().spawn(
		"authority-discovery-worker",
		Some("authority-discovery"),
		worker.run(),
	);
	service
}

async fn build_interface(
	polkadot_config: Configuration,
	task_manager: &mut TaskManager,
	client: RelayChainRpcClient,
) -> RelayChainResult<(
	Arc<dyn RelayChainInterface + 'static>,
	Option<CollatorPair>,
	Arc<dyn NetworkService>,
	async_channel::Receiver<GenericIncomingRequest>,
)> {
	let collator_pair = CollatorPair::generate().0;
	let blockchain_rpc_client = Arc::new(BlockChainRpcClient::new(client.clone()));
	let collator_node = match polkadot_config.network.network_backend {
		sc_network::config::NetworkBackendType::Libp2p => {
			new_minimal_relay_chain::<RelayBlock, sc_network::NetworkWorker<RelayBlock, RelayHash>>(
				polkadot_config,
				collator_pair.clone(),
				blockchain_rpc_client,
			)
			.await?
		},
		sc_network::config::NetworkBackendType::Litep2p => {
			new_minimal_relay_chain::<RelayBlock, sc_network::Litep2pNetworkBackend>(
				polkadot_config,
				collator_pair.clone(),
				blockchain_rpc_client,
			)
			.await?
		},
	};
	task_manager.add_child(collator_node.task_manager);
	Ok((
		Arc::new(RelayChainRpcInterface::new(client, collator_node.overseer_handle)),
		Some(collator_pair),
		collator_node.network_service,
		collator_node.paranode_rx,
	))
}

pub async fn build_minimal_relay_chain_node_with_rpc(
	relay_chain_config: Configuration,
	parachain_prometheus_registry: Option<&Registry>,
	task_manager: &mut TaskManager,
	relay_chain_url: Vec<Url>,
) -> RelayChainResult<(
	Arc<dyn RelayChainInterface + 'static>,
	Option<CollatorPair>,
	Arc<dyn NetworkService>,
	async_channel::Receiver<GenericIncomingRequest>,
)> {
	let client = cumulus_relay_chain_rpc_interface::create_client_and_start_worker(
		relay_chain_url,
		task_manager,
		parachain_prometheus_registry,
	)
	.await?;

	build_interface(relay_chain_config, task_manager, client).await
}

/// Builds a minimal relay chain node. Chain data is fetched
/// via [`BlockChainRpcClient`] and fed into the overseer and its subsystems.
///
/// Instead of spawning all subsystems, this minimal node will only spawn subsystems
/// required to collate:
/// - AvailabilityRecovery
/// - CollationGeneration
/// - CollatorProtocol
/// - NetworkBridgeRx
/// - NetworkBridgeTx
/// - RuntimeApi
#[sc_tracing::logging::prefix_logs_with("Relaychain")]
async fn new_minimal_relay_chain<Block: BlockT, Network: NetworkBackend<RelayBlock, RelayHash>>(
	config: Configuration,
	collator_pair: CollatorPair,
	relay_chain_rpc_client: Arc<BlockChainRpcClient>,
) -> Result<NewMinimalNode, RelayChainError> {
	let role = config.role;
	let mut net_config = sc_network::config::FullNetworkConfiguration::<_, _, Network>::new(
		&config.network,
		config.prometheus_config.as_ref().map(|cfg| cfg.registry.clone()),
	);
	let metrics = Network::register_notification_metrics(
		config.prometheus_config.as_ref().map(|cfg| &cfg.registry),
	);
	let peer_store_handle = net_config.peer_store_handle();

	let prometheus_registry = config.prometheus_registry();
	let task_manager = TaskManager::new(config.tokio_handle.clone(), prometheus_registry)?;

	if let Some(PrometheusConfig { port, registry }) = config.prometheus_config.clone() {
		task_manager.spawn_handle().spawn(
			"prometheus-endpoint",
			None,
			prometheus_endpoint::init_prometheus(port, registry).map(drop),
		);
	}

	let genesis_hash = relay_chain_rpc_client.block_get_hash(Some(0)).await?.unwrap_or_default();
	let peerset_protocol_names =
		PeerSetProtocolNames::new(genesis_hash, config.chain_spec.fork_id());
	let is_authority = if role.is_authority() { IsAuthority::Yes } else { IsAuthority::No };
	let notification_services = peer_sets_info::<_, Network>(
		is_authority,
		&peerset_protocol_names,
		metrics.clone(),
		Arc::clone(&peer_store_handle),
	)
	.into_iter()
	.map(|(config, (peerset, service))| {
		net_config.add_notification_protocol(config);
		(peerset, service)
	})
	.collect::<std::collections::HashMap<PeerSet, Box<dyn sc_network::NotificationService>>>();

	let request_protocol_names = ReqProtocolNames::new(genesis_hash, config.chain_spec.fork_id());
	let (collation_req_v2_receiver, collation_req_v3_receiver, available_data_req_receiver) =
		build_request_response_protocol_receivers(&request_protocol_names, &mut net_config);

	let (cfg, paranode_rx) = bootnode_request_response_config::<_, _, Network>(
		genesis_hash,
		config.chain_spec.fork_id(),
	);
	net_config.add_request_response_protocol(cfg);

	let best_header = relay_chain_rpc_client
		.chain_get_header(None)
		.await?
		.ok_or_else(|| RelayChainError::RpcCallError("Unable to fetch best header".to_string()))?;
	let (network, sync_service) = build_collator_network::<Network>(
		&config,
		net_config,
		task_manager.spawn_handle(),
		genesis_hash,
		best_header,
		metrics,
	)
	.map_err(|e| RelayChainError::Application(Box::new(e)))?;

	let authority_discovery_service = build_authority_discovery_service::<Block>(
		&task_manager,
		relay_chain_rpc_client.clone(),
		&config,
		network.clone(),
		prometheus_registry.cloned(),
	);

	let overseer_args = OverseerGenArgs {
		runtime_client: relay_chain_rpc_client.clone(),
		network_service: network.clone(),
		sync_service,
		authority_discovery_service,
		collation_req_v2_receiver,
		collation_req_v3_receiver,
		available_data_req_receiver,
		registry: prometheus_registry,
		spawner: task_manager.spawn_handle(),
		is_parachain_node: IsParachainNode::Collator(collator_pair),
		overseer_message_channel_capacity_override: None,
		req_protocol_names: request_protocol_names,
		peerset_protocol_names,
		notification_services,
	};

	let overseer_handle =
		collator_overseer::spawn_overseer(overseer_args, &task_manager, relay_chain_rpc_client)?;

	Ok(NewMinimalNode { task_manager, overseer_handle, network_service: network, paranode_rx })
}

fn build_request_response_protocol_receivers<
	Block: BlockT,
	Network: NetworkBackend<Block, <Block as BlockT>::Hash>,
>(
	request_protocol_names: &ReqProtocolNames,
	config: &mut FullNetworkConfiguration<Block, <Block as BlockT>::Hash, Network>,
) -> (
	IncomingRequestReceiver<v2::CollationFetchingRequest>,
	IncomingRequestReceiver<v3::CollationFetchingRequest>,
	IncomingRequestReceiver<v1::AvailableDataFetchingRequest>,
) {
	let (collation_req_v2_receiver, cfg) =
		IncomingRequest::get_config_receiver::<_, Network>(request_protocol_names);
	config.add_request_response_protocol(cfg);
	let (collation_req_v3_receiver, cfg) =
		IncomingRequest::get_config_receiver::<_, Network>(request_protocol_names);
	config.add_request_response_protocol(cfg);
	let (available_data_req_receiver, cfg) =
		IncomingRequest::get_config_receiver::<_, Network>(request_protocol_names);
	config.add_request_response_protocol(cfg);
	let cfg =
		Protocol::ChunkFetchingV1.get_outbound_only_config::<_, Network>(request_protocol_names);
	config.add_request_response_protocol(cfg);
	let cfg =
		Protocol::ChunkFetchingV2.get_outbound_only_config::<_, Network>(request_protocol_names);
	config.add_request_response_protocol(cfg);
	(collation_req_v2_receiver, collation_req_v3_receiver, available_data_req_receiver)
}
