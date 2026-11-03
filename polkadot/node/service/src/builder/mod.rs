// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Polkadot service builder.

#![cfg(feature = "full-node")]

mod partial;
use partial::PolkadotPartialComponents;
pub(crate) use partial::{new_partial, new_partial_basics};

use crate::{
	grandpa_support, open_database,
	overseer::{ExtendedOverseerGenArgs, OverseerGen, OverseerGenArgs},
	parachains_db,
	relay_chain_selection::SelectRelayChain,
	workers, Chain, Error, FullBackend, FullClient, IdentifyVariant, IsParachainNode,
	GRANDPA_JUSTIFICATION_PERIOD, KEEP_FINALIZED_FOR_LIVE_NETWORKS,
};
use frame_benchmarking_cli::SUBSTRATE_REFERENCE_HARDWARE;
use gum::info;
use mmr_gadget::MmrGadget;
use polkadot_availability_recovery::FETCH_CHUNKS_THRESHOLD;
use polkadot_collator_protocol::ReputationConfig;
use polkadot_node_core_approval_voting::Config as ApprovalVotingConfig;
use polkadot_node_core_av_store::Config as AvailabilityConfig;
use polkadot_node_core_candidate_validation::Config as CandidateValidationConfig;
use polkadot_node_core_chain_selection::{
	self as chain_selection_subsystem, Config as ChainSelectionConfig,
};
use polkadot_node_core_dispute_coordinator::Config as DisputeCoordinatorConfig;
use polkadot_node_network_protocol::{
	peer_set::{CollationVersion, PeerSet, PeerSetProtocolNames},
	request_response::{IncomingRequest, Protocol, ReqProtocolNames},
};
use polkadot_node_subsystem_types::DefaultSubsystemClient;
use polkadot_overseer::{Handle, OverseerConnector};
use polkadot_primitives::Block;
use sc_client_api::Backend;
use sc_network::config::FullNetworkConfiguration;
use sc_network_sync::WarpSyncConfig;
use sc_service::{Configuration, RpcHandlers, TaskManager};
use sc_sysinfo::Metric;
use sc_telemetry::TelemetryWorkerHandle;
use sc_transaction_pool_api::OffchainTransactionPoolFactory;
use sp_consensus_beefy::ecdsa_crypto;
use sp_runtime::traits::Block as BlockT;
use std::{
	collections::{HashMap, HashSet},
	sync::Arc,
	time::Duration,
};

/// Polkadot node service initialization parameters.
pub struct NewFullParams<OverseerGenerator: OverseerGen> {
	pub is_parachain_node: IsParachainNode,
	pub enable_beefy: bool,
	/// Whether to enable the block authoring backoff on production networks
	/// where it isn't enabled by default.
	pub force_authoring_backoff: bool,
	pub telemetry_worker_handle: Option<TelemetryWorkerHandle>,
	/// The version of the node. TESTING ONLY: `None` can be passed to skip the node/worker version
	/// check, both on startup and in the workers.
	pub node_version: Option<String>,
	/// Whether the node is attempting to run as a secure validator.
	pub secure_validator_mode: bool,
	/// An optional path to a directory containing the workers.
	pub workers_path: Option<std::path::PathBuf>,
	/// Optional custom names for the prepare and execute workers.
	pub workers_names: Option<(String, String)>,
	/// An optional number of the maximum number of pvf execute workers.
	pub execute_workers_max_num: Option<usize>,
	/// An optional maximum number of pvf workers that can be spawned in the pvf prepare pool for
	/// tasks with the priority below critical.
	pub prepare_workers_soft_max_num: Option<usize>,
	/// An optional absolute number of pvf workers that can be spawned in the pvf prepare pool.
	pub prepare_workers_hard_max_num: Option<usize>,
	/// How long finalized data should be kept in the availability store (in hours)
	pub keep_finalized_for: Option<u32>,
	pub overseer_gen: OverseerGenerator,
	pub overseer_message_channel_capacity_override: Option<usize>,
	#[allow(dead_code)]
	pub malus_finality_delay: Option<u32>,
	pub hwbench: Option<sc_sysinfo::HwBench>,
	/// Set of invulnerable AH collator `PeerId`s
	pub invulnerable_ah_collators: HashSet<polkadot_node_network_protocol::PeerId>,
	/// Override for `HOLD_OFF_DURATION` constant .
	pub collator_protocol_hold_off: Option<Duration>,
	/// Use experimental collator protocol
	pub experimental_collator_protocol: bool,
	/// Collator reputation persistence interval. If None, defaults to 600 seconds.
	pub collator_reputation_persist_interval: Option<Duration>,
}

/// Completely built polkadot node service.
pub struct NewFull {
	pub task_manager: TaskManager,
	pub client: Arc<FullClient>,
	pub overseer_handle: Option<Handle>,
	pub network: Arc<dyn sc_network::service::traits::NetworkService>,
	pub sync_service: Arc<sc_network_sync::SyncingService<Block>>,
	pub rpc_handlers: RpcHandlers,
	pub backend: Arc<FullBackend>,
}

pub struct PolkadotServiceBuilder<OverseerGenerator, Network>
where
	OverseerGenerator: OverseerGen,
	Network: sc_network::NetworkBackend<Block, <Block as BlockT>::Hash>,
{
	config: Configuration,
	params: NewFullParams<OverseerGenerator>,
	overseer_connector: OverseerConnector,
	partial_components: PolkadotPartialComponents<SelectRelayChain<FullBackend>>,
	net_config: FullNetworkConfiguration<Block, <Block as BlockT>::Hash, Network>,
}

impl<OverseerGenerator, Network> PolkadotServiceBuilder<OverseerGenerator, Network>
where
	OverseerGenerator: OverseerGen,
	Network: sc_network::NetworkBackend<Block, <Block as BlockT>::Hash>,
{
	/// Create new polkadot service builder.
	pub fn new(
		mut config: Configuration,
		params: NewFullParams<OverseerGenerator>,
	) -> Result<PolkadotServiceBuilder<OverseerGenerator, Network>, Error> {
		let basics = new_partial_basics(&mut config, params.telemetry_worker_handle.clone())?;

		let prometheus_registry = config.prometheus_registry().cloned();
		let overseer_connector = OverseerConnector::default();
		let overseer_handle = Handle::new(overseer_connector.handle());
		let auth_or_collator = config.role.is_authority() || params.is_parachain_node.is_collator();

		let select_chain = if auth_or_collator {
			let metrics = polkadot_node_subsystem_util::metrics::Metrics::register(
				prometheus_registry.as_ref(),
			)?;

			SelectRelayChain::new_with_overseer(
				basics.backend.clone(),
				overseer_handle.clone(),
				metrics,
				Some(basics.task_manager.spawn_handle()),
			)
		} else {
			SelectRelayChain::new_longest_chain(basics.backend.clone())
		};

		let partial_components =
			new_partial::<SelectRelayChain<_>>(&mut config, basics, select_chain)?;

		let net_config = sc_network::config::FullNetworkConfiguration::<_, _, Network>::new(
			&config.network,
			config.prometheus_config.as_ref().map(|cfg| cfg.registry.clone()),
		);

		Ok(PolkadotServiceBuilder {
			config,
			params,
			overseer_connector,
			partial_components,
			net_config,
		})
	}

	/// Get the genesis hash of the polkadot service being built.
	pub fn genesis_hash(&self) -> <Block as BlockT>::Hash {
		self.partial_components.client.chain_info().genesis_hash
	}

	/// Add extra request-response protocol to the polkadot service.
	pub fn add_extra_request_response_protocol(
		&mut self,
		config: Network::RequestResponseProtocolConfig,
	) {
		self.net_config.add_request_response_protocol(config);
	}

	/// Build polkadot service.
	pub fn build(self) -> Result<NewFull, Error> {
		let Self {
			config,
			params:
				NewFullParams {
					is_parachain_node,
					enable_beefy,
					force_authoring_backoff,
					telemetry_worker_handle: _,
					node_version,
					secure_validator_mode,
					workers_path,
					workers_names,
					overseer_gen,
					overseer_message_channel_capacity_override,
					malus_finality_delay: _malus_finality_delay,
					hwbench,
					execute_workers_max_num,
					prepare_workers_soft_max_num,
					prepare_workers_hard_max_num,
					keep_finalized_for,
					invulnerable_ah_collators,
					collator_protocol_hold_off,
					experimental_collator_protocol,
					collator_reputation_persist_interval,
				},
			overseer_connector,
			partial_components:
				sc_service::PartialComponents::<_, _, SelectRelayChain<_>, _, _, _> {
					client,
					backend,
					mut task_manager,
					keystore_container,
					select_chain,
					import_queue,
					transaction_pool,
					other:
						(rpc_extensions_builder, import_setup, rpc_setup, slot_duration, mut telemetry),
				},
			mut net_config,
		} = self;

		let role = config.role;
		let auth_or_collator = config.role.is_authority() || is_parachain_node.is_collator();
		let is_offchain_indexing_enabled = config.offchain_worker.indexing_enabled;
		let force_authoring = config.force_authoring;
		let disable_grandpa = config.disable_grandpa;
		let name = config.network.node_name.clone();
		let backoff_authoring_blocks = if !force_authoring_backoff &&
			(config.chain_spec.is_polkadot() || config.chain_spec.is_kusama())
		{
			// the block authoring backoff is disabled by default on production networks
			None
		} else {
			let mut backoff = sc_consensus_slots::BackoffAuthoringOnFinalizedHeadLagging::default();

			if config.chain_spec.is_rococo() ||
				config.chain_spec.is_versi() ||
				config.chain_spec.is_dev()
			{
				// on testnets that are in flux (like rococo or versi), finality has stalled
				// sometimes due to operational issues and it's annoying to slow down block
				// production to 1 block per hour.
				backoff.max_interval = 10;
			}

			Some(backoff)
		};
		let shared_voter_state = rpc_setup;
		let auth_disc_publish_non_global_ips = config.network.allow_non_globals_in_dht;
		let auth_disc_public_addresses = config.network.public_addresses.clone();

		let genesis_hash = client.chain_info().genesis_hash;
		let peer_store_handle = net_config.peer_store_handle();

		let prometheus_registry = config.prometheus_registry().cloned();
		let metrics = Network::register_notification_metrics(
			config.prometheus_config.as_ref().map(|cfg| &cfg.registry),
		);

		// Note: GrandPa is pushed before the Polkadot-specific protocols. This doesn't change
		// anything in terms of behaviour, but makes the logs more consistent with the other
		// Substrate nodes.
		let grandpa_protocol_name =
			sc_consensus_grandpa::protocol_standard_name(&genesis_hash, &config.chain_spec);
		let (grandpa_protocol_config, grandpa_notification_service) =
			sc_consensus_grandpa::grandpa_peers_set_config::<_, Network>(
				grandpa_protocol_name.clone(),
				metrics.clone(),
				Arc::clone(&peer_store_handle),
			);
		net_config.add_notification_protocol(grandpa_protocol_config);

		let beefy_gossip_proto_name =
			sc_consensus_beefy::gossip_protocol_name(&genesis_hash, config.chain_spec.fork_id());
		// `beefy_on_demand_justifications_handler` is given to `beefy-gadget` task to be run,
		// while `beefy_req_resp_cfg` is added to `config.network.request_response_protocols`.
		let (beefy_on_demand_justifications_handler, beefy_req_resp_cfg) =
			sc_consensus_beefy::communication::request_response::BeefyJustifsRequestHandler::new::<
				_,
				Network,
			>(
				&genesis_hash,
				config.chain_spec.fork_id(),
				client.clone(),
				prometheus_registry.clone(),
			);
		let beefy_notification_service = match enable_beefy {
			false => None,
			true => {
				let (beefy_notification_config, beefy_notification_service) =
					sc_consensus_beefy::communication::beefy_peers_set_config::<_, Network>(
						beefy_gossip_proto_name.clone(),
						metrics.clone(),
						Arc::clone(&peer_store_handle),
					);

				net_config.add_notification_protocol(beefy_notification_config);
				net_config.add_request_response_protocol(beefy_req_resp_cfg);
				Some(beefy_notification_service)
			},
		};

		// validation/collation protocols are enabled only if `Overseer` is enabled
		let main_collation_version = if is_parachain_node.is_running_alongside_parachain_node() ||
			experimental_collator_protocol
		{
			None
		} else {
			Some(CollationVersion::V3)
		};
		let peerset_protocol_names = PeerSetProtocolNames::new_with_main_collation_version(
			genesis_hash,
			config.chain_spec.fork_id(),
			main_collation_version,
		);

		// If this is a validator or running alongside a parachain node, we need to enable the
		// networking protocols.
		//
		// Collators and parachain full nodes require the collator and validator networking to send
		// collations and to be able to recover PoVs.
		let notification_services = if role.is_authority() ||
			is_parachain_node.is_running_alongside_parachain_node()
		{
			use polkadot_network_bridge::{peer_sets_info, IsAuthority};
			let is_authority = if role.is_authority() { IsAuthority::Yes } else { IsAuthority::No };

			peer_sets_info::<_, Network>(
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
			.collect::<HashMap<PeerSet, Box<dyn sc_network::NotificationService>>>()
		} else {
			std::collections::HashMap::new()
		};

		let req_protocol_names = ReqProtocolNames::new(&genesis_hash, config.chain_spec.fork_id());

		let cfg = Protocol::CollationFetchingV1
			.get_outbound_only_config::<_, Network>(&req_protocol_names);
		net_config.add_request_response_protocol(cfg);
		let (collation_req_v2_receiver, cfg) =
			IncomingRequest::get_config_receiver::<_, Network>(&req_protocol_names);
		net_config.add_request_response_protocol(cfg);
		let (collation_req_v3_receiver, cfg) =
			IncomingRequest::get_config_receiver::<_, Network>(&req_protocol_names);
		net_config.add_request_response_protocol(cfg);
		let (available_data_req_receiver, cfg) =
			IncomingRequest::get_config_receiver::<_, Network>(&req_protocol_names);
		net_config.add_request_response_protocol(cfg);
		let (pov_req_receiver, cfg) =
			IncomingRequest::get_config_receiver::<_, Network>(&req_protocol_names);
		net_config.add_request_response_protocol(cfg);
		let (chunk_req_v1_receiver, cfg) =
			IncomingRequest::get_config_receiver::<_, Network>(&req_protocol_names);
		net_config.add_request_response_protocol(cfg);
		let (chunk_req_v2_receiver, cfg) =
			IncomingRequest::get_config_receiver::<_, Network>(&req_protocol_names);
		net_config.add_request_response_protocol(cfg);

		let grandpa_hard_forks = if config.chain_spec.is_kusama() {
			grandpa_support::kusama_hard_forks()
		} else {
			Vec::new()
		};

		let warp_sync = Arc::new(sc_consensus_grandpa::warp_proof::NetworkProvider::new(
			backend.clone(),
			import_setup.1.shared_authority_set().clone(),
			grandpa_hard_forks,
		));

		let ext_overseer_args = if is_parachain_node.is_running_alongside_parachain_node() {
			None
		} else {
			let parachains_db = open_database(&config.database)?;
			let candidate_validation_config = if role.is_authority() {
				let (prep_worker_path, exec_worker_path) = workers::determine_workers_paths(
					workers_path,
					workers_names,
					node_version.clone(),
				)?;
				log::info!("🚀 Using prepare-worker binary at: {:?}", prep_worker_path);
				log::info!("🚀 Using execute-worker binary at: {:?}", exec_worker_path);

				Some(CandidateValidationConfig {
					artifacts_cache_path: config
						.database
						.path()
						.ok_or(Error::DatabasePathRequired)?
						.join("pvf-artifacts"),
					node_version,
					secure_validator_mode,
					prep_worker_path,
					exec_worker_path,
					// Default execution workers is 4 because we have 8 cores on the reference
					// hardware, and this accounts for 50% of that cpu capacity.
					pvf_execute_workers_max_num: execute_workers_max_num.unwrap_or(4),
					pvf_prepare_workers_soft_max_num: prepare_workers_soft_max_num.unwrap_or(1),
					pvf_prepare_workers_hard_max_num: prepare_workers_hard_max_num.unwrap_or(2),
				})
			} else {
				None
			};
			let (candidate_req_v2_receiver, cfg) =
				IncomingRequest::get_config_receiver::<_, Network>(&req_protocol_names);
			net_config.add_request_response_protocol(cfg);
			let (dispute_req_receiver, cfg) =
				IncomingRequest::get_config_receiver::<_, Network>(&req_protocol_names);
			net_config.add_request_response_protocol(cfg);
			let approval_voting_config = ApprovalVotingConfig {
				col_approval_data: parachains_db::REAL_COLUMNS.col_approval_data,
				slot_duration_millis: slot_duration.as_millis() as u64,
			};
			let dispute_coordinator_config = DisputeCoordinatorConfig {
				col_dispute_data: parachains_db::REAL_COLUMNS.col_dispute_coordinator_data,
			};
			let chain_selection_config = ChainSelectionConfig {
				col_data: parachains_db::REAL_COLUMNS.col_chain_selection_data,
				stagnant_check_interval: Default::default(),
				stagnant_check_mode: chain_selection_subsystem::StagnantCheckMode::PruneOnly,
			};
			let reputation_config = ReputationConfig {
				col_reputation_data: parachains_db::REAL_COLUMNS.col_collator_reputation_data,
				persist_interval: collator_reputation_persist_interval,
			};

			// Kusama + testnets get a higher threshold, we are conservative on Polkadot for now.
			let fetch_chunks_threshold =
				if config.chain_spec.is_polkadot() { None } else { Some(FETCH_CHUNKS_THRESHOLD) };

			let availability_config = AvailabilityConfig {
				col_data: parachains_db::REAL_COLUMNS.col_availability_data,
				col_meta: parachains_db::REAL_COLUMNS.col_availability_meta,
				keep_finalized_for: if matches!(config.chain_spec.identify_chain(), Chain::Rococo) {
					keep_finalized_for.unwrap_or(1)
				} else {
					KEEP_FINALIZED_FOR_LIVE_NETWORKS
				},
			};

			Some(ExtendedOverseerGenArgs {
				keystore: keystore_container.local_keystore(),
				parachains_db,
				candidate_validation_config,
				availability_config,
				pov_req_receiver,
				chunk_req_v1_receiver,
				chunk_req_v2_receiver,
				candidate_req_v2_receiver,
				approval_voting_config,
				dispute_req_receiver,
				dispute_coordinator_config,
				chain_selection_config,
				fetch_chunks_threshold,
				invulnerable_ah_collators,
				collator_protocol_hold_off,
				experimental_collator_protocol,
				reputation_config,
			})
		};

		let (network, system_rpc_tx, tx_handler_controller, sync_service, _bitswap_handle) =
			sc_service::build_network(sc_service::BuildNetworkParams {
				config: &config,
				net_config,
				client: client.clone(),
				transaction_pool: transaction_pool.clone(),
				spawn_handle: task_manager.spawn_handle(),
				spawn_essential_handle: task_manager.spawn_essential_handle(),
				import_queue,
				block_announce_validator_builder: None,
				warp_sync_config: Some(WarpSyncConfig::WithProvider(warp_sync)),
				block_relay: None,
				metrics,
				gap_sync_body_policy: None,
			})?;

		if config.offchain_worker.enabled {
			use futures::FutureExt;

			task_manager.spawn_handle().spawn(
				"offchain-workers-runner",
				"offchain-work",
				sc_offchain::OffchainWorkers::new(sc_offchain::OffchainWorkerOptions {
					runtime_api_provider: client.clone(),
					keystore: Some(keystore_container.keystore()),
					offchain_db: backend.offchain_storage(),
					transaction_pool: Some(OffchainTransactionPoolFactory::new(
						transaction_pool.clone(),
					)),
					network_provider: Arc::new(network.clone()),
					is_validator: role.is_authority(),
					enable_http_requests: false,
					custom_extensions: move |_| vec![],
				})?
				.run(client.clone(), task_manager.spawn_handle())
				.boxed(),
			);
		}

		let network_config = config.network.clone();
		let rpc_handlers = sc_service::spawn_tasks(sc_service::SpawnTasksParams {
			config,
			backend: backend.clone(),
			client: client.clone(),
			keystore: keystore_container.keystore(),
			network: network.clone(),
			sync_service: sync_service.clone(),
			rpc_builder: Box::new(rpc_extensions_builder),
			transaction_pool: transaction_pool.clone(),
			task_manager: &mut task_manager,
			system_rpc_tx,
			tx_handler_controller,
			telemetry: telemetry.as_mut(),
			tracing_execute_block: None,
		})?;

		if let Some(hwbench) = hwbench {
			sc_sysinfo::print_hwbench(&hwbench);
			match SUBSTRATE_REFERENCE_HARDWARE.check_hardware(&hwbench, role.is_authority()) {
				Err(err) if role.is_authority() => {
					if err
						.0
						.iter()
						.any(|failure| matches!(failure.metric, Metric::Blake2256Parallel { .. }))
					{
						log::warn!(
						"⚠️  Starting January 2025 the hardware will fail the minimal physical CPU cores requirements {} for role 'Authority',\n\
						    find out more when this will become mandatory at:\n\
						    https://docs.polkadot.com/infrastructure/running-a-validator/requirements/#minimum-hardware-requirements",
						err
					);
					}
					if err
						.0
						.iter()
						.any(|failure| !matches!(failure.metric, Metric::Blake2256Parallel { .. }))
					{
						log::warn!(
						"⚠️  The hardware does not meet the minimal requirements {} for role 'Authority' find out more at:\n\
						https://docs.polkadot.com/infrastructure/running-a-validator/requirements/#minimum-hardware-requirements",
						err
					);
					}
				},
				_ => {},
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

		let (block_import, link_half, babe_link, beefy_links) = import_setup;

		let overseer_client = client.clone();
		let spawner = task_manager.spawn_handle();

		let authority_discovery_service =
		// We need the authority discovery if this node is either a validator or running alongside a parachain node.
		// Parachains node require the authority discovery for finding relay chain validators for sending
		// their PoVs or recovering PoVs.
		if role.is_authority() || is_parachain_node.is_running_alongside_parachain_node() {
			use futures::StreamExt;
			use sc_network::{Event, NetworkEventStream};

			let authority_discovery_role = if role.is_authority() {
				sc_authority_discovery::Role::PublishAndDiscover(keystore_container.keystore())
			} else {
				// don't publish our addresses when we're not an authority (collator, cumulus, ..)
				sc_authority_discovery::Role::Discover
			};
			let dht_event_stream =
				network.event_stream("authority-discovery").filter_map(|e| async move {
					match e {
						Event::Dht(e) => Some(e),
						_ => None,
					}
				});
			let (worker, service) = sc_authority_discovery::new_worker_and_service_with_config(
				sc_authority_discovery::WorkerConfig {
					publish_non_global_ips: auth_disc_publish_non_global_ips,
					public_addresses: auth_disc_public_addresses,
					// Require that authority discovery records are signed.
					strict_record_validation: true,
					persisted_cache_directory: network_config.net_config_path,
					..Default::default()
				},
				client.clone(),
				Arc::new(network.clone()),
				Box::pin(dht_event_stream),
				authority_discovery_role,
				prometheus_registry.clone(),
				task_manager.spawn_handle(),
			);

			task_manager.spawn_handle().spawn(
				"authority-discovery-worker",
				Some("authority-discovery"),
				Box::pin(worker.run()),
			);
			Some(service)
		} else {
			None
		};

		let runtime_client = Arc::new(DefaultSubsystemClient::new(
			overseer_client.clone(),
			OffchainTransactionPoolFactory::new(transaction_pool.clone()),
		));

		let overseer_handle = if let Some(authority_discovery_service) = authority_discovery_service
		{
			let (overseer, overseer_handle) = overseer_gen
				.generate::<sc_service::SpawnTaskHandle, DefaultSubsystemClient<FullClient>>(
					overseer_connector,
					OverseerGenArgs {
						runtime_client,
						network_service: network.clone(),
						sync_service: sync_service.clone(),
						authority_discovery_service,
						collation_req_v2_receiver,
						collation_req_v3_receiver,
						available_data_req_receiver,
						registry: prometheus_registry.as_ref(),
						spawner,
						is_parachain_node,
						overseer_message_channel_capacity_override,
						req_protocol_names,
						peerset_protocol_names,
						notification_services,
					},
					ext_overseer_args,
				)
				.map_err(|e| {
					gum::error!("Failed to init overseer: {}", e);
					e
				})?;
			let handle = Handle::new(overseer_handle.clone());

			{
				let handle = handle.clone();
				task_manager.spawn_essential_handle().spawn_blocking(
					"overseer",
					None,
					Box::pin(async move {
						use futures::{pin_mut, select, FutureExt};

						let forward = polkadot_overseer::forward_events(overseer_client, handle);

						let forward = forward.fuse();
						let overseer_fut = overseer.run().fuse();

						pin_mut!(overseer_fut);
						pin_mut!(forward);

						select! {
							() = forward => (),
							() = overseer_fut => (),
							complete => (),
						}
					}),
				);
			}
			Some(handle)
		} else {
			assert!(
				!auth_or_collator,
				"Precondition congruence (false) is guaranteed by manual checking. qed"
			);
			None
		};

		if role.is_authority() {
			let mut proposer = sc_basic_authorship::ProposerFactory::new(
				task_manager.spawn_handle(),
				client.clone(),
				transaction_pool.clone(),
				prometheus_registry.as_ref(),
				telemetry.as_ref().map(|x| x.handle()),
			);
			// We allow `15MiB` on the node side, but the actual block size limit is defined by the
			// runtime.
			proposer.set_default_block_size_limit(15 * 1024 * 1024);

			let client_clone = client.clone();
			let overseer_handle =
				overseer_handle.as_ref().ok_or(Error::AuthoritiesRequireRealOverseer)?.clone();
			let slot_duration = babe_link.config().slot_duration();
			let babe_config = sc_consensus_babe::BabeParams {
				keystore: keystore_container.keystore(),
				client: client.clone(),
				select_chain,
				block_import,
				env: proposer,
				sync_oracle: sync_service.clone(),
				justification_sync_link: sync_service.clone(),
				create_inherent_data_providers: move |parent, ()| {
					let client_clone = client_clone.clone();
					let overseer_handle = overseer_handle.clone();

					async move {
						let parachain =
						polkadot_node_core_parachains_inherent::ParachainsInherentDataProvider::new(
							client_clone,
							overseer_handle,
							parent,
						);

						let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

						let slot =
						sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
							*timestamp,
							slot_duration,
						);

						Ok((slot, timestamp, parachain))
					}
				},
				force_authoring,
				backoff_authoring_blocks,
				babe_link,
				block_proposal_slot_portion: sc_consensus_babe::SlotProportion::new(2f32 / 3f32),
				max_block_proposal_slot_portion: None,
				telemetry: telemetry.as_ref().map(|x| x.handle()),
			};

			let babe = sc_consensus_babe::start_babe(babe_config)?;
			task_manager.spawn_essential_handle().spawn_blocking("babe", None, babe);
		}

		// if the node isn't actively participating in consensus then it doesn't
		// need a keystore, regardless of which protocol we use below.
		let keystore_opt =
			if role.is_authority() { Some(keystore_container.keystore()) } else { None };

		// beefy is enabled if its notification service exists
		if let Some(notification_service) = beefy_notification_service {
			let justifications_protocol_name =
				beefy_on_demand_justifications_handler.protocol_name();
			let network_params = sc_consensus_beefy::BeefyNetworkParams {
				network: Arc::new(network.clone()),
				sync: sync_service.clone(),
				gossip_protocol_name: beefy_gossip_proto_name,
				justifications_protocol_name,
				notification_service,
				_phantom: core::marker::PhantomData::<Block>,
			};
			let payload_provider = sp_consensus_beefy::mmr::MmrRootProvider::new(client.clone());
			let beefy_params = sc_consensus_beefy::BeefyParams {
				client: client.clone(),
				backend: backend.clone(),
				payload_provider,
				runtime: client.clone(),
				key_store: keystore_opt.clone(),
				network_params,
				min_block_delta: 8,
				prometheus_registry: prometheus_registry.clone(),
				links: beefy_links,
				on_demand_justifications_handler: beefy_on_demand_justifications_handler,
				is_authority: role.is_authority(),
			};

			let gadget = sc_consensus_beefy::start_beefy_gadget::<
				_,
				_,
				_,
				_,
				_,
				_,
				_,
				ecdsa_crypto::AuthorityId,
			>(beefy_params);

			// BEEFY is part of consensus, if it fails we'll bring the node down with it to make
			// sure it is noticed.
			task_manager
				.spawn_essential_handle()
				.spawn_blocking("beefy-gadget", None, gadget);
		}
		// When offchain indexing is enabled, MMR gadget should also run.
		if is_offchain_indexing_enabled {
			task_manager.spawn_essential_handle().spawn_blocking(
				"mmr-gadget",
				None,
				MmrGadget::start(
					client.clone(),
					backend.clone(),
					sp_mmr_primitives::INDEXING_PREFIX.to_vec(),
				),
			);
		}

		let config = sc_consensus_grandpa::Config {
			// FIXME substrate#1578 make this available through chainspec
			// Grandpa performance can be improved a bit by tuning this parameter, see:
			// https://github.com/paritytech/polkadot/issues/5464
			gossip_duration: Duration::from_millis(1000),
			justification_generation_period: GRANDPA_JUSTIFICATION_PERIOD,
			name: Some(name),
			observer_enabled: false,
			keystore: keystore_opt,
			local_role: role,
			telemetry: telemetry.as_ref().map(|x| x.handle()),
			protocol_name: grandpa_protocol_name,
		};

		let enable_grandpa = !disable_grandpa;
		if enable_grandpa {
			// start the full GRANDPA voter
			// NOTE: unlike in substrate we are currently running the full
			// GRANDPA voter protocol for all full nodes (regardless of whether
			// they're validators or not). at this point the full voter should
			// provide better guarantees of block and vote data availability than
			// the observer.

			let mut voting_rules_builder = sc_consensus_grandpa::VotingRulesBuilder::default();

			#[cfg(not(feature = "malus"))]
			let _malus_finality_delay = None;

			if let Some(delay) = _malus_finality_delay {
				info!(?delay, "Enabling malus finality delay",);
				voting_rules_builder =
					voting_rules_builder.add(sc_consensus_grandpa::BeforeBestBlockBy(delay));
			};

			let grandpa_config = sc_consensus_grandpa::GrandpaParams {
				config,
				link: link_half,
				network: network.clone(),
				sync: sync_service.clone(),
				voting_rule: voting_rules_builder.build(),
				prometheus_registry: prometheus_registry.clone(),
				shared_voter_state,
				telemetry: telemetry.as_ref().map(|x| x.handle()),
				notification_service: grandpa_notification_service,
				offchain_tx_pool_factory: OffchainTransactionPoolFactory::new(
					transaction_pool.clone(),
				),
			};

			task_manager.spawn_essential_handle().spawn_blocking(
				"grandpa-voter",
				None,
				sc_consensus_grandpa::run_grandpa_voter(grandpa_config)?,
			);
		}

		Ok(NewFull {
			task_manager,
			client,
			overseer_handle,
			network,
			sync_service,
			rpc_handlers,
			backend,
		})
	}
}

/// Create a new full node of arbitrary runtime and executor.
///
/// This is an advanced feature and not recommended for general use. Generally, `build_full` is
/// a better choice.
///
/// `workers_path` is used to get the path to the directory where auxiliary worker binaries reside.
/// If not specified, the main binary's directory is searched first, then `/usr/lib/polkadot` is
/// searched. If the path points to an executable rather then directory, that executable is used
/// both as preparation and execution worker (supposed to be used for tests only).
pub fn new_full<
	OverseerGenerator: OverseerGen,
	Network: sc_network::NetworkBackend<Block, <Block as BlockT>::Hash>,
>(
	config: Configuration,
	params: NewFullParams<OverseerGenerator>,
) -> Result<NewFull, Error> {
	PolkadotServiceBuilder::<OverseerGenerator, Network>::new(config, params)?.build()
}
