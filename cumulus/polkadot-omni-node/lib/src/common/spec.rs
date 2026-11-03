// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{
	chain_spec::Extensions,
	cli::DevSealMode,
	common::{
		command::NodeCommandRunner,
		rpc::BuildRpcExtensions,
		statement_store::{build_statement_store, new_statement_handler_proto},
		types::{
			ParachainBackend, ParachainBlockImport, ParachainClient, ParachainHostFunctions,
			ParachainService,
		},
		ConstructNodeRuntimeApi, NodeBlock, NodeExtraArgs,
	},
};
use codec::Encode;
use cumulus_client_bootnodes::{start_bootnode_tasks, StartBootnodeTasksParams};
use cumulus_client_cli::CollatorOptions;
use cumulus_client_service::{
	build_network, build_relay_chain_interface, prepare_node_config, start_relay_chain_tasks,
	BuildNetworkParams, DARecoveryProfile, ParachainTracingExecuteBlock,
	StartRelayChainTasksParams,
};
use cumulus_primitives_core::{BlockT, GetParachainInfo, ParaId};
use cumulus_relay_chain_interface::{OverseerHandle, RelayChainInterface};
use futures::FutureExt;
use log::{debug, info};
use parachains_common_types::Hash;
use polkadot_primitives::CollatorPair;
use prometheus_endpoint::Registry;
use sc_client_api::Backend;
use sc_consensus::DefaultImportQueue;
use sc_executor::{HeapAllocStrategy, DEFAULT_HEAP_ALLOC_STRATEGY};
use sc_network::{
	config::FullNetworkConfiguration, NetworkBackend, NetworkBlock, NetworkStateInfo, PeerId,
};
use sc_service::{Configuration, ImportQueue, PartialComponents, TaskManager};
use sc_statement_store::Store;
use sc_storage_chain_sync::{
	BitswapHandleSlot, IndexedTransactionFetcher, StorageChainBlockImport,
};
use sc_sysinfo::HwBench;
use sc_telemetry::{TelemetryHandle, TelemetryWorker};
use sc_tracing::tracing::Instrument;
use sc_transaction_pool::TransactionPoolHandle;
use sc_transaction_pool_api::OffchainTransactionPoolFactory;
use sp_api::{ApiExt, ProvideRuntimeApi};
use sp_keystore::KeystorePtr;
use sp_runtime::traits::AccountIdConversion;
use std::{
	future::Future,
	pin::Pin,
	sync::{Arc, OnceLock},
	time::Duration,
};

// Override default idle connection timeout of 10 seconds to give IPFS clients more
// time to query data over Bitswap. This is needed when manually adding our node
// to a swarm of an IPFS node, because the IPFS node doesn't keep any active
// substreams with us and our node closes a connection after
// `idle_connection_timeout`.
const IPFS_WORKAROUND_TIMEOUT: Duration = Duration::from_secs(3600);

pub(crate) trait BuildImportQueue<
	Block: BlockT,
	RuntimeApi,
	BlockImport: sc_consensus::BlockImport<Block>,
>
{
	fn build_import_queue(
		client: Arc<ParachainClient<Block, RuntimeApi>>,
		block_import: ParachainBlockImport<Block, BlockImport>,
		config: &Configuration,
		telemetry_handle: Option<TelemetryHandle>,
		task_manager: &TaskManager,
	) -> sc_service::error::Result<DefaultImportQueue<Block>>;
}

pub(crate) trait StartConsensus<Block: BlockT, RuntimeApi, BI, BIAuxiliaryData>
where
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>>,
{
	fn start_consensus(
		client: Arc<ParachainClient<Block, RuntimeApi>>,
		block_import: ParachainBlockImport<Block, BI>,
		prometheus_registry: Option<&Registry>,
		telemetry: Option<TelemetryHandle>,
		task_manager: &TaskManager,
		relay_chain_interface: Arc<dyn RelayChainInterface>,
		transaction_pool: Arc<TransactionPoolHandle<Block, ParachainClient<Block, RuntimeApi>>>,
		keystore: KeystorePtr,
		relay_chain_slot_duration: Duration,
		para_id: ParaId,
		collator_key: CollatorPair,
		collator_peer_id: PeerId,
		overseer_handle: OverseerHandle,
		announce_block: Arc<dyn Fn(Hash, Option<Vec<u8>>) + Send + Sync>,
		backend: Arc<ParachainBackend<Block>>,
		node_extra_args: NodeExtraArgs,
		block_import_extra_return_value: BIAuxiliaryData,
	) -> Result<(), sc_service::Error>;
}

/// Checks that the hardware meets the requirements and print a warning otherwise.
fn warn_if_slow_hardware(hwbench: &sc_sysinfo::HwBench) {
	// Polkadot para-chains should generally use these requirements to ensure that the relay-chain
	// will not take longer than expected to import its blocks.
	if let Err(err) =
		frame_benchmarking_cli::SUBSTRATE_REFERENCE_HARDWARE.check_hardware(hwbench, false)
	{
		log::warn!(
			"⚠️  The hardware does not meet the minimal requirements {} for role 'Authority' find out more at:\n\
			https://docs.polkadot.com/infrastructure/running-a-validator/requirements/#minimum-hardware-requirements",
			err
		);
	}
}

pub(crate) trait InitBlockImport<Block: BlockT, RuntimeApi> {
	type BlockImport: sc_consensus::BlockImport<Block, Error = sp_consensus::Error>
		+ Clone
		+ Send
		+ Sync;
	type BlockImportAuxiliaryData;

	/// Build the path-specific outer block import on top of the supplied
	/// `StorageChainBlockImport`.
	fn init_block_import(
		client: Arc<ParachainClient<Block, RuntimeApi>>,
		storage_chain_block_import: StorageChainBlockImport<
			Block,
			Arc<ParachainClient<Block, RuntimeApi>>,
			ParachainClient<Block, RuntimeApi>,
		>,
	) -> sc_service::error::Result<(Self::BlockImport, Self::BlockImportAuxiliaryData)>;
}

pub(crate) struct ClientBlockImport;

impl<Block, RuntimeApi> InitBlockImport<Block, RuntimeApi> for ClientBlockImport
where
	Block: BlockT<Hash = sc_client_db::DbHash>,
	RuntimeApi: Send + ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>>,
{
	type BlockImport = StorageChainBlockImport<
		Block,
		Arc<ParachainClient<Block, RuntimeApi>>,
		ParachainClient<Block, RuntimeApi>,
	>;
	type BlockImportAuxiliaryData = ();

	fn init_block_import(
		_client: Arc<ParachainClient<Block, RuntimeApi>>,
		storage_chain_block_import: StorageChainBlockImport<
			Block,
			Arc<ParachainClient<Block, RuntimeApi>>,
			ParachainClient<Block, RuntimeApi>,
		>,
	) -> sc_service::error::Result<(Self::BlockImport, Self::BlockImportAuxiliaryData)> {
		Ok((storage_chain_block_import, ()))
	}
}

pub(crate) trait BaseNodeSpec {
	type Block: NodeBlock;

	type RuntimeApi: ConstructNodeRuntimeApi<
		Self::Block,
		ParachainClient<Self::Block, Self::RuntimeApi>,
	>;

	type BuildImportQueue: BuildImportQueue<
		Self::Block,
		Self::RuntimeApi,
		<Self::InitBlockImport as InitBlockImport<Self::Block, Self::RuntimeApi>>::BlockImport,
	>;

	type InitBlockImport: self::InitBlockImport<Self::Block, Self::RuntimeApi>;

	/// Retrieves parachain id.
	fn parachain_id(
		client: &ParachainClient<Self::Block, Self::RuntimeApi>,
		parachain_config: &Configuration,
	) -> Option<ParaId> {
		let best_hash = client.chain_info().best_hash;
		let para_id = if client
			.runtime_api()
			.has_api::<dyn GetParachainInfo<Self::Block>>(best_hash)
			.ok()
			.filter(|has_api| *has_api)
			.is_some()
		{
			client
				.runtime_api()
				.parachain_id(best_hash)
				.inspect_err(|err| {
					log::error!(
						"`cumulus_primitives_core::GetParachainInfo` runtime API call errored with {}",
						err
					);
				})
				.ok()?
		} else {
			ParaId::from(
				Extensions::try_get(&*parachain_config.chain_spec).and_then(|ext| ext.para_id())?,
			)
		};

		let parachain_account =
			AccountIdConversion::<polkadot_primitives::AccountId>::into_account_truncating(
				&para_id,
			);

		info!("🪪 Parachain id: {:?}", para_id);
		info!("🧾 Parachain Account: {}", parachain_account);

		Some(para_id)
	}

	/// Starts a `ServiceBuilder` for a full service.
	///
	/// Use this macro if you don't actually need the full service, but just the builder in order to
	/// be able to perform chain operations.
	fn new_partial(
		config: &Configuration,
	) -> sc_service::error::Result<
		ParachainService<
			Self::Block,
			Self::RuntimeApi,
			<Self::InitBlockImport as InitBlockImport<Self::Block, Self::RuntimeApi>>::BlockImport,
			<Self::InitBlockImport as InitBlockImport<Self::Block, Self::RuntimeApi>>::BlockImportAuxiliaryData
		>
	>{
		let telemetry = config
			.telemetry_endpoints
			.clone()
			.filter(|x| !x.is_empty())
			.map(|endpoints| -> Result<_, sc_telemetry::Error> {
				let worker = TelemetryWorker::new(16)?;
				let telemetry = worker.handle().new_telemetry(endpoints);
				Ok((worker, telemetry))
			})
			.transpose()?;

		let heap_pages =
			config.executor.default_heap_pages.map_or(DEFAULT_HEAP_ALLOC_STRATEGY, |h| {
				HeapAllocStrategy::Static { extra_pages: h as _ }
			});

		let executor = sc_executor::WasmExecutor::<ParachainHostFunctions>::builder()
			.with_execution_method(config.executor.wasm_method)
			.with_max_runtime_instances(config.executor.max_runtime_instances)
			.with_runtime_cache_size(config.executor.runtime_cache_size)
			.with_onchain_heap_alloc_strategy(heap_pages)
			.with_offchain_heap_alloc_strategy(heap_pages)
			.build();

		let (client, backend, keystore_container, task_manager) =
			sc_service::new_full_parts_record_import::<Self::Block, Self::RuntimeApi, _>(
				config,
				telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
				executor,
				true,
				Default::default(),
			)?;
		let client = Arc::new(client);

		let telemetry_worker_handle = telemetry.as_ref().map(|(worker, _)| worker.handle());

		let telemetry = telemetry.map(|(worker, telemetry)| {
			task_manager.spawn_handle().spawn("telemetry", None, worker.run());
			telemetry
		});

		let transaction_pool = Arc::from(
			sc_transaction_pool::Builder::new(
				task_manager.spawn_essential_handle(),
				client.clone(),
				config.role.is_authority().into(),
			)
			.with_options(config.transaction_pool.clone())
			.with_prometheus(config.prometheus_registry())
			.build(),
		);

		let bitswap_slot: BitswapHandleSlot = Arc::new(OnceLock::new());

		let fetcher = IndexedTransactionFetcher::new(Arc::clone(&bitswap_slot));

		let storage_chain_block_import =
			StorageChainBlockImport::new(client.clone(), client.clone(), fetcher);

		let (outer_block_import, block_import_auxiliary_data) =
			Self::InitBlockImport::init_block_import(client.clone(), storage_chain_block_import)?;

		let block_import = ParachainBlockImport::new(outer_block_import, backend.clone());

		let import_queue = Self::BuildImportQueue::build_import_queue(
			client.clone(),
			block_import.clone(),
			config,
			telemetry.as_ref().map(|telemetry| telemetry.handle()),
			&task_manager,
		)?;

		Ok(PartialComponents {
			backend,
			client,
			import_queue,
			keystore_container,
			task_manager,
			transaction_pool,
			select_chain: (),
			other: (
				block_import,
				telemetry,
				telemetry_worker_handle,
				block_import_auxiliary_data,
				bitswap_slot,
			),
		})
	}
}

pub(crate) trait NodeSpec: BaseNodeSpec {
	type BuildRpcExtensions: BuildRpcExtensions<
		ParachainClient<Self::Block, Self::RuntimeApi>,
		ParachainBackend<Self::Block>,
		TransactionPoolHandle<Self::Block, ParachainClient<Self::Block, Self::RuntimeApi>>,
		Store,
	>;

	type StartConsensus: StartConsensus<
		Self::Block,
		Self::RuntimeApi,
		<Self::InitBlockImport as InitBlockImport<Self::Block, Self::RuntimeApi>>::BlockImport,
		<Self::InitBlockImport as InitBlockImport<Self::Block, Self::RuntimeApi>>::BlockImportAuxiliaryData,
	>;

	fn start_dev_node(
		_config: Configuration,
		_mode: DevSealMode,
		_node_extra_args: NodeExtraArgs,
	) -> sc_service::error::Result<TaskManager> {
		Err(sc_service::Error::Other("Dev not supported for this node type".into()))
	}

	/// Start a node with the given parachain spec.
	///
	/// This is the actual implementation that is abstract over the executor and the runtime api.
	fn start_node<Net>(
		parachain_config: Configuration,
		polkadot_config: Configuration,
		collator_options: CollatorOptions,
		hwbench: Option<sc_sysinfo::HwBench>,
		node_extra_args: NodeExtraArgs,
	) -> Pin<Box<dyn Future<Output = sc_service::error::Result<TaskManager>>>>
	where
		Net: NetworkBackend<Self::Block, Hash>,
	{
		let fut = async move {
			let mut parachain_config = prepare_node_config(parachain_config);

			// Some additional customization in relation to starting the node as an ipfs server.
			if parachain_config.network.idle_connection_timeout < IPFS_WORKAROUND_TIMEOUT &&
				parachain_config.network.ipfs_server
			{
				debug!(
					"Overriding `config.network.idle_connection_timeout` to allow long-lived connections with IPFS nodes. The old value: {:?} is replaced by: {:?}.",
					parachain_config.network.idle_connection_timeout, IPFS_WORKAROUND_TIMEOUT
				);
				parachain_config.network.idle_connection_timeout = IPFS_WORKAROUND_TIMEOUT;
			}

			let parachain_public_addresses = parachain_config.network.public_addresses.clone();
			let parachain_fork_id = parachain_config.chain_spec.fork_id().map(ToString::to_string);
			let advertise_non_global_ips = parachain_config.network.allow_non_globals_in_dht;

			let params = Self::new_partial(&parachain_config)?;
			let (
				block_import,
				mut telemetry,
				telemetry_worker_handle,
				block_import_auxiliary_data,
				bitswap_slot,
			) = params.other;
			let client = params.client.clone();
			let backend = params.backend.clone();
			let mut task_manager = params.task_manager;

			// Resolve parachain id based on runtime, or based on chain spec.
			let para_id = Self::parachain_id(&client, &parachain_config)
				.ok_or("Failed to retrieve the parachain id")?;
			let relay_chain_fork_id = polkadot_config.chain_spec.fork_id().map(ToString::to_string);
			let (relay_chain_interface, collator_key, relay_chain_network, paranode_rx) =
				build_relay_chain_interface(
					polkadot_config,
					&parachain_config,
					telemetry_worker_handle,
					&mut task_manager,
					collator_options.clone(),
					hwbench.clone(),
				)
				.await
				.map_err(|e| sc_service::Error::Application(Box::new(e)))?;

			let validator = parachain_config.role.is_authority();
			let prometheus_registry = parachain_config.prometheus_registry().cloned();
			let transaction_pool = params.transaction_pool.clone();
			let import_queue_service = params.import_queue.service();
			let mut net_config = FullNetworkConfiguration::<_, _, Net>::new(
				&parachain_config.network,
				prometheus_registry.clone(),
			);

			let metrics = Net::register_notification_metrics(
				parachain_config.prometheus_config.as_ref().map(|config| &config.registry),
			);

			let statement_handler_proto = node_extra_args.statement_store_config.map(|config| {
				let proto = new_statement_handler_proto(
					&*client,
					&parachain_config,
					&metrics,
					&mut net_config,
				);
				(proto, config)
			});

			let (network, system_rpc_tx, tx_handler_controller, sync_service, bitswap_handle) =
				build_network(BuildNetworkParams {
					parachain_config: &parachain_config,
					net_config,
					client: client.clone(),
					transaction_pool: transaction_pool.clone(),
					para_id,
					spawn_handle: task_manager.spawn_handle(),
					spawn_essential_handle: task_manager.spawn_essential_handle(),
					relay_chain_interface: relay_chain_interface.clone(),
					import_queue: params.import_queue,
					metrics,
					gap_sync_body_policy: Some(crate::common::gap_sync_body_policy_provider(
						client.clone(),
						parachain_config.blocks_pruning,
					)),
				})
				.await?;

			if let Some(handle) = bitswap_handle {
				let _ = bitswap_slot.set(Arc::new(handle));
			}

			let peer_id = relay_chain_network.local_peer_id();

			if validator && node_extra_args.collator_reserved_slots > 0 {
				cumulus_client_collator_discovery::start_collator_discovery(
					cumulus_client_collator_discovery::StartCollatorDiscoveryParams {
						max_reserved: node_extra_args.collator_reserved_slots,
						client: client.clone(),
						authority_discovery: client.clone(),
						network: network.clone(),
						sync_service: sync_service.clone(),
						network_event_stream: network.event_stream("para-authority-discovery"),
						keystore: params.keystore_container.keystore(),
						genesis_hash: client.chain_info().genesis_hash,
						fork_id: parachain_fork_id.clone(),
						publish_non_global_ips: parachain_config.network.allow_non_globals_in_dht,
						public_addresses: parachain_config.network.public_addresses.clone(),
						persisted_cache_directory: parachain_config.network.net_config_path.clone(),
						prometheus_registry: prometheus_registry.clone(),
						spawn_handle: task_manager.spawn_handle(),
					},
				)
				.map_err(|e| sc_service::Error::Application(Box::new(e)))?;
			}

			let statement_store = statement_handler_proto
				.map(|(statement_handler_proto, config)| {
					build_statement_store(
						&parachain_config,
						&mut task_manager,
						client.clone(),
						network.clone(),
						sync_service.clone(),
						params.keystore_container.local_keystore(),
						statement_handler_proto,
						config,
					)
				})
				.transpose()?;

			let hop_pool = node_extra_args.hop.as_ref().and_then(|params| {
				match params.build_pool(
					parachain_config.database.path().map(|p| p.to_path_buf()),
					prometheus_registry.as_ref(),
				) {
					Ok(pool) => Some(pool),
					Err(e) => {
						log::warn!(
							target: "hop",
							"Failed to initialize HOP data pool, continuing without HOP: {e}",
						);
						None
					},
				}
			});
			if let (Some(pool), Some(hop)) = (hop_pool.as_ref(), node_extra_args.hop.as_ref()) {
				let task = sc_hop::build_maintenance_task::<Self::Block, _, _>(
					&client,
					&transaction_pool,
					pool.clone(),
					hop.promotion_buffer_secs,
					hop.check_interval,
				);
				task_manager.spawn_handle().spawn("hop-maintenance", None, task.run());
			}

			if parachain_config.offchain_worker.enabled {
				let custom_extensions = {
					let statement_store = statement_store.clone();
					move |_hash| {
						if let Some(statement_store) = &statement_store {
							vec![Box::new(statement_store.clone().as_statement_store_ext())
								as Box<_>]
						} else {
							vec![]
						}
					}
				};

				let offchain_workers =
					sc_offchain::OffchainWorkers::new(sc_offchain::OffchainWorkerOptions {
						runtime_api_provider: client.clone(),
						keystore: Some(params.keystore_container.keystore()),
						offchain_db: backend.offchain_storage(),
						transaction_pool: Some(OffchainTransactionPoolFactory::new(
							transaction_pool.clone(),
						)),
						network_provider: Arc::new(network.clone()),
						is_validator: parachain_config.role.is_authority(),
						enable_http_requests: true,
						custom_extensions,
					})?;
				task_manager.spawn_handle().spawn(
					"offchain-workers-runner",
					"offchain-work",
					offchain_workers.run(client.clone(), task_manager.spawn_handle()).boxed(),
				);
			}

			let spawn_handle = Arc::new(task_manager.spawn_handle());

			let rpc_builder = {
				let client = client.clone();
				let transaction_pool = transaction_pool.clone();
				let backend_for_rpc = backend.clone();
				let statement_store = statement_store.clone();
				let hop_pool = hop_pool.clone();
				Box::new(move |_| {
					Self::BuildRpcExtensions::build_rpc_extensions(
						client.clone(),
						backend_for_rpc.clone(),
						transaction_pool.clone(),
						statement_store.clone(),
						hop_pool.clone(),
						spawn_handle.clone(),
					)
				})
			};

			let database_path = parachain_config.database.path().map(|p| p.to_path_buf());

			sc_service::spawn_tasks(sc_service::SpawnTasksParams {
				rpc_builder,
				client: client.clone(),
				transaction_pool: transaction_pool.clone(),
				task_manager: &mut task_manager,
				config: parachain_config,
				keystore: params.keystore_container.keystore(),
				backend: backend.clone(),
				network: network.clone(),
				sync_service: sync_service.clone(),
				system_rpc_tx,
				tx_handler_controller,
				telemetry: telemetry.as_mut(),
				tracing_execute_block: Some(Arc::new(ParachainTracingExecuteBlock::new(
					client.clone(),
				))),
			})?;

			// Spawn the storage monitor
			if let Some(database_path) = database_path {
				sc_storage_monitor::StorageMonitorService::try_spawn(
					node_extra_args.storage_monitor.clone(),
					database_path,
					&task_manager.spawn_essential_handle(),
				)
				.map_err(|e| sc_service::Error::Application(Box::new(e) as Box<_>))?;
			}

			if let Some(hwbench) = hwbench {
				sc_sysinfo::print_hwbench(&hwbench);
				if validator {
					warn_if_slow_hardware(&hwbench);
				}

				if let Some(ref mut telemetry) = telemetry {
					let telemetry_handle = telemetry.handle();
					task_manager.spawn_handle().spawn(
						"telemetry_hwbench",
						None,
						sc_sysinfo::initialize_hwbench_telemetry(telemetry_handle, hwbench),
					);
				}
			}

			let announce_block = {
				let sync_service = sync_service.clone();
				Arc::new(move |hash, data| sync_service.announce_block(hash, data))
			};

			let relay_chain_slot_duration = Duration::from_secs(6);

			let overseer_handle = relay_chain_interface
				.overseer_handle()
				.map_err(|e| sc_service::Error::Application(Box::new(e)))?;

			start_relay_chain_tasks(StartRelayChainTasksParams {
				client: client.clone(),
				announce_block: announce_block.clone(),
				para_id,
				relay_chain_interface: relay_chain_interface.clone(),
				task_manager: &mut task_manager,
				da_recovery_profile: if validator {
					DARecoveryProfile::Collator
				} else {
					DARecoveryProfile::FullNode
				},
				import_queue: import_queue_service,
				relay_chain_slot_duration,
				recovery_handle: Box::new(overseer_handle.clone()),
				sync_service: sync_service.clone(),
				prometheus_registry: prometheus_registry.as_ref(),
			})?;

			start_bootnode_tasks(StartBootnodeTasksParams {
				embedded_dht_bootnode: collator_options.embedded_dht_bootnode,
				dht_bootnode_discovery: collator_options.dht_bootnode_discovery,
				para_id,
				task_manager: &mut task_manager,
				relay_chain_interface: relay_chain_interface.clone(),
				relay_chain_fork_id,
				relay_chain_network,
				request_receiver: paranode_rx,
				parachain_network: network.clone(),
				advertise_non_global_ips,
				parachain_genesis_hash: client.chain_info().genesis_hash.encode(),
				parachain_fork_id,
				parachain_public_addresses,
			});

			if validator {
				Self::StartConsensus::start_consensus(
					client.clone(),
					block_import,
					prometheus_registry.as_ref(),
					telemetry.as_ref().map(|t| t.handle()),
					&task_manager,
					relay_chain_interface.clone(),
					transaction_pool,
					params.keystore_container.keystore(),
					relay_chain_slot_duration,
					para_id,
					collator_key.expect("Command line arguments do not allow this. qed"),
					peer_id,
					overseer_handle,
					announce_block,
					backend.clone(),
					node_extra_args,
					block_import_auxiliary_data,
				)?;
			}

			Ok(task_manager)
		};

		Box::pin(Instrument::instrument(
			fut,
			sc_tracing::tracing::info_span!(
				sc_tracing::logging::PREFIX_LOG_SPAN,
				name = "Parachain"
			),
		))
	}
}

pub(crate) trait DynNodeSpec: NodeCommandRunner {
	/// Start node with manual or instant seal consensus.
	fn start_dev_node(
		self: Box<Self>,
		config: Configuration,
		mode: DevSealMode,
		node_extra_args: NodeExtraArgs,
	) -> sc_service::error::Result<TaskManager>;

	/// Start the node.
	fn start_node(
		self: Box<Self>,
		parachain_config: Configuration,
		polkadot_config: Configuration,
		collator_options: CollatorOptions,
		hwbench: Option<HwBench>,
		node_extra_args: NodeExtraArgs,
	) -> Pin<Box<dyn Future<Output = sc_service::error::Result<TaskManager>>>>;
}

impl<T> DynNodeSpec for T
where
	T: NodeSpec + NodeCommandRunner,
{
	fn start_dev_node(
		self: Box<Self>,
		config: Configuration,
		mode: DevSealMode,
		node_extra_args: NodeExtraArgs,
	) -> sc_service::error::Result<TaskManager> {
		<Self as NodeSpec>::start_dev_node(config, mode, node_extra_args)
	}

	fn start_node(
		self: Box<Self>,
		parachain_config: Configuration,
		polkadot_config: Configuration,
		collator_options: CollatorOptions,
		hwbench: Option<HwBench>,
		node_extra_args: NodeExtraArgs,
	) -> Pin<Box<dyn Future<Output = sc_service::error::Result<TaskManager>>>> {
		match parachain_config.network.network_backend {
			sc_network::config::NetworkBackendType::Libp2p => {
				<Self as NodeSpec>::start_node::<sc_network::NetworkWorker<_, _>>(
					parachain_config,
					polkadot_config,
					collator_options,
					hwbench,
					node_extra_args,
				)
			},
			sc_network::config::NetworkBackendType::Litep2p => {
				<Self as NodeSpec>::start_node::<sc_network::Litep2pNetworkBackend>(
					parachain_config,
					polkadot_config,
					collator_options,
					hwbench,
					node_extra_args,
				)
			},
		}
	}
}
