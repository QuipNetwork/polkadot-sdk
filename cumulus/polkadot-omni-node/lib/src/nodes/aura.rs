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
	cli::{AuthoringPolicy, DevSealMode},
	common::{
		aura::{AuraIdT, AuraRuntimeApi},
		rpc::{BuildParachainRpcExtensions, BuildRpcExtensions},
		spec::{
			BaseNodeSpec, BuildImportQueue, ClientBlockImport, DynNodeSpec, InitBlockImport,
			NodeSpec, StartConsensus,
		},
		types::{
			AccountId, Balance, Hash, Nonce, ParachainBackend, ParachainBlockImport,
			ParachainClient,
		},
		ConstructNodeRuntimeApi, NodeBlock, NodeExtraArgs,
	},
};
use codec::Encode;
use cumulus_client_collator::service::{
	CollatorService, ServiceInterface as CollatorServiceInterface,
};
#[docify::export(slot_based_colator_import)]
use cumulus_client_consensus_aura::collators::slot_based::{
	self as slot_based, Params as SlotBasedParams,
};
use cumulus_client_consensus_aura::{
	collators::{
		lookahead::{self as aura, Params as AuraParams},
		slot_based::{SlotBasedBlockImport, SlotBasedBlockImportHandle},
	},
	equivocation_import_queue::Verifier as EquivocationVerifier,
};
use cumulus_client_consensus_relay_chain::Verifier as RelayChainVerifier;
use cumulus_client_parachain_inherent::MockValidationDataInherentDataProvider;
use cumulus_primitives_core::{
	relay_chain::ValidationCode, CollectCollationInfo, GetParachainInfo, ParaId,
	RelayParentOffsetApi, TargetBlockRate,
};
use cumulus_relay_chain_interface::{OverseerHandle, RelayChainInterface};
use futures::{prelude::*, FutureExt};
use polkadot_primitives::{CollatorPair, UpgradeGoAhead};
use prometheus_endpoint::Registry;
use sc_client_api::{Backend, BlockchainEvents};
use sc_client_db::DbHash;
use sc_consensus::{
	import_queue::{BasicQueue, Verifier as VerifierT},
	BlockImportParams, DefaultImportQueue, LongestChain,
};
use sc_consensus_manual_seal::consensus::aura::AuraConsensusDataProvider;
use sc_network::{config::FullNetworkConfiguration, NotificationMetrics, PeerId};
use sc_service::{Configuration, Error, PartialComponents, TaskManager};
use sc_storage_chain_sync::StorageChainBlockImport;
use sc_telemetry::TelemetryHandle;
use sc_transaction_pool::TransactionPoolHandle;
use sc_transaction_pool_api::OffchainTransactionPoolFactory;
use sp_api::{ApiExt, ProvideRuntimeApi};
use sp_consensus::Environment;
use sp_core::traits::SpawnEssentialNamed;
use sp_inherents::CreateInherentDataProviders;
use sp_keystore::KeystorePtr;
use sp_runtime::{
	app_crypto::AppCrypto,
	traits::{Block as BlockT, Header as HeaderT, UniqueSaturatedInto},
};
use sp_transaction_storage_proof::runtime_api::TransactionStorageApi;
use std::{marker::PhantomData, ops::Sub, sync::Arc, time::Duration};

struct Verifier<Block, Client, AuraId> {
	client: Arc<Client>,
	aura_verifier: Box<dyn VerifierT<Block>>,
	relay_chain_verifier: Box<dyn VerifierT<Block>>,
	_phantom: PhantomData<AuraId>,
}

#[async_trait::async_trait]
impl<Block: BlockT, Client, AuraId> VerifierT<Block> for Verifier<Block, Client, AuraId>
where
	Client: ProvideRuntimeApi<Block> + Send + Sync,
	Client::Api: AuraRuntimeApi<Block, AuraId>,
	AuraId: AuraIdT + Sync,
{
	async fn verify(
		&self,
		block_import: BlockImportParams<Block>,
	) -> Result<BlockImportParams<Block>, String> {
		if self.client.runtime_api().has_aura_api(*block_import.header.parent_hash()) {
			self.aura_verifier.verify(block_import).await
		} else {
			self.relay_chain_verifier.verify(block_import).await
		}
	}
}

/// Build the import queue for parachain runtimes that started with relay chain consensus and
/// switched to aura.
pub(crate) struct BuildRelayToAuraImportQueue<Block, RuntimeApi, AuraId, BlockImport>(
	PhantomData<(Block, RuntimeApi, AuraId, BlockImport)>,
);

impl<Block: BlockT, RuntimeApi, AuraId, BlockImport>
	BuildImportQueue<Block, RuntimeApi, BlockImport>
	for BuildRelayToAuraImportQueue<Block, RuntimeApi, AuraId, BlockImport>
where
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId>,
	AuraId: AuraIdT + Sync,
	BlockImport:
		sc_consensus::BlockImport<Block, Error = sp_consensus::Error> + Send + Sync + 'static,
{
	fn build_import_queue(
		client: Arc<ParachainClient<Block, RuntimeApi>>,
		block_import: ParachainBlockImport<Block, BlockImport>,
		config: &Configuration,
		telemetry_handle: Option<TelemetryHandle>,
		task_manager: &TaskManager,
	) -> sc_service::error::Result<DefaultImportQueue<Block>> {
		let inherent_data_providers =
			move |_, _| async move { Ok(sp_timestamp::InherentDataProvider::from_system_time()) };
		let registry = config.prometheus_registry();
		let spawner = task_manager.spawn_essential_handle();

		let relay_chain_verifier =
			Box::new(RelayChainVerifier::new(client.clone(), inherent_data_providers));

		let equivocation_aura_verifier =
			EquivocationVerifier::<<AuraId as AppCrypto>::Pair, _, _, _>::new(
				client.clone(),
				inherent_data_providers,
				telemetry_handle,
			);

		let verifier = Verifier {
			client,
			aura_verifier: Box::new(equivocation_aura_verifier),
			relay_chain_verifier,
			_phantom: Default::default(),
		};

		Ok(BasicQueue::new(verifier, Box::new(block_import), None, &spawner, registry))
	}
}

/// Uses the lookahead collator to support async backing.
///
/// Start an aura powered parachain node. Some system chains use this.
pub(crate) struct AuraNode<Block, RuntimeApi, AuraId, StartConsensus, InitBlockImport>(
	pub PhantomData<(Block, RuntimeApi, AuraId, StartConsensus, InitBlockImport)>,
);

impl<Block, RuntimeApi, AuraId, StartConsensus, InitBlockImport> Default
	for AuraNode<Block, RuntimeApi, AuraId, StartConsensus, InitBlockImport>
{
	fn default() -> Self {
		Self(Default::default())
	}
}

impl<Block, RuntimeApi, AuraId, StartConsensus, InitBlockImport> BaseNodeSpec
	for AuraNode<Block, RuntimeApi, AuraId, StartConsensus, InitBlockImport>
where
	Block: NodeBlock,
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId>
		+ pallet_transaction_payment_rpc::TransactionPaymentRuntimeApi<Block, Balance>
		+ substrate_frame_rpc_system::AccountNonceApi<Block, AccountId, Nonce>,
	AuraId: AuraIdT + Sync,
	InitBlockImport: self::InitBlockImport<Block, RuntimeApi> + Send,
	InitBlockImport::BlockImport:
		sc_consensus::BlockImport<Block, Error = sp_consensus::Error> + 'static,
{
	type Block = Block;
	type RuntimeApi = RuntimeApi;
	type BuildImportQueue =
		BuildRelayToAuraImportQueue<Block, RuntimeApi, AuraId, InitBlockImport::BlockImport>;
	type InitBlockImport = InitBlockImport;
}

impl<Block, RuntimeApi, AuraId, StartConsensus, InitBlockImport> NodeSpec
	for AuraNode<Block, RuntimeApi, AuraId, StartConsensus, InitBlockImport>
where
	Block: NodeBlock,
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId>
		+ pallet_transaction_payment_rpc::TransactionPaymentRuntimeApi<Block, Balance>
		+ substrate_frame_rpc_system::AccountNonceApi<Block, AccountId, Nonce>,
	AuraId: AuraIdT + Sync + Send + 'static,
	StartConsensus: self::StartConsensus<
			Block,
			RuntimeApi,
			InitBlockImport::BlockImport,
			InitBlockImport::BlockImportAuxiliaryData,
		> + 'static,
	InitBlockImport: self::InitBlockImport<Block, RuntimeApi> + Send + 'static,
	InitBlockImport::BlockImport:
		sc_consensus::BlockImport<Block, Error = sp_consensus::Error> + 'static,
{
	type BuildRpcExtensions = BuildParachainRpcExtensions<Block, RuntimeApi>;
	type StartConsensus = StartConsensus;

	fn start_dev_node(
		mut config: Configuration,
		mode: DevSealMode,
		node_extra_args: NodeExtraArgs,
	) -> sc_service::error::Result<TaskManager> {
		// Destructure all fields so the compiler enforces handling new args.
		let NodeExtraArgs {
			authoring_policy,
			ref export_pov,
			max_pov_percentage,
			ref statement_store_config,
			ref storage_monitor,
			ref hop,
			collator_reserved_slots: _,
		} = node_extra_args;

		// Warn about args that have no effect in dev mode (collation-specific).
		if authoring_policy != AuthoringPolicy::Lookahead {
			log::warn!(
				"Authoring policy `{}` has no effect in dev mode (manual/instant seal is used).",
				authoring_policy,
			);
		}
		if export_pov.is_some() {
			log::warn!("`--export-pov` has no effect in dev mode (no PoVs are produced).");
		}
		if max_pov_percentage.is_some() {
			log::warn!("`--max-pov-percentage` has no effect in dev mode (no PoVs are produced).");
		}

		let PartialComponents {
			client,
			backend,
			mut task_manager,
			import_queue,
			keystore_container,
			select_chain: _,
			transaction_pool,
			other: (_, mut telemetry, _, _, _),
		} = Self::new_partial(&config)?;

		// Since this is a dev node, prevent it from connecting to peers.
		config.network.default_peers_set.in_peers = 0;
		config.network.default_peers_set.out_peers = 0;
		let mut net_config =
			FullNetworkConfiguration::<_, _, sc_network::Litep2pNetworkBackend>::new(
				&config.network,
				None,
			);

		let metrics = NotificationMetrics::new(None);

		let statement_handler_proto = statement_store_config.as_ref().map(|ss_config| {
			let proto = crate::common::statement_store::new_statement_handler_proto(
				&*client,
				&config,
				&metrics,
				&mut net_config,
			);
			(proto, *ss_config)
		});

		let (network, system_rpc_tx, tx_handler_controller, sync_service, _bitswap_handle) =
			sc_service::build_network(sc_service::BuildNetworkParams {
				config: &config,
				client: client.clone(),
				transaction_pool: transaction_pool.clone(),
				spawn_handle: task_manager.spawn_handle(),
				spawn_essential_handle: task_manager.spawn_essential_handle(),
				import_queue,
				net_config,
				block_announce_validator_builder: None,
				warp_sync_config: None,
				block_relay: None,
				metrics,
				gap_sync_body_policy: None,
			})?;

		let statement_store = statement_handler_proto
			.map(|(statement_handler_proto, ss_config)| {
				crate::common::statement_store::build_statement_store(
					&config,
					&mut task_manager,
					client.clone(),
					network.clone(),
					sync_service.clone(),
					keystore_container.local_keystore(),
					statement_handler_proto,
					ss_config,
				)
			})
			.transpose()?;

		if config.offchain_worker.enabled {
			let custom_extensions = {
				let statement_store = statement_store.clone();
				move |_| {
					if let Some(statement_store) = &statement_store {
						vec![Box::new(statement_store.clone().as_statement_store_ext()) as Box<_>]
					} else {
						vec![]
					}
				}
			};

			let offchain_workers =
				sc_offchain::OffchainWorkers::new(sc_offchain::OffchainWorkerOptions {
					runtime_api_provider: client.clone(),
					keystore: Some(keystore_container.keystore()),
					offchain_db: backend.offchain_storage(),
					transaction_pool: Some(OffchainTransactionPoolFactory::new(
						transaction_pool.clone(),
					)),
					network_provider: Arc::new(network.clone()),
					is_validator: config.role.is_authority(),
					enable_http_requests: true,
					custom_extensions,
				})?;
			task_manager.spawn_handle().spawn(
				"offchain-workers-runner",
				"offchain-work",
				offchain_workers.run(client.clone(), task_manager.spawn_handle()).boxed(),
			);
		}

		let proposer = sc_basic_authorship::ProposerFactory::new(
			task_manager.spawn_handle(),
			client.clone(),
			transaction_pool.clone(),
			None,
			None,
		);

		// Note: Changing slot durations are currently not supported
		let slot_duration = sc_consensus_aura::slot_duration(&*client)
			.expect("slot_duration is always present; qed.");

		// The aura digest provider will provide digests that match the provided timestamp data.
		// Without this, the AURA parachain runtimes complain about slot mismatches.
		let aura_digest_provider =
			AuraConsensusDataProvider::<Block>::new_with_slot_duration(slot_duration);

		let para_id =
			Self::parachain_id(&client, &config).ok_or("Failed to retrieve the parachain id")?;
		let create_inherent_data_providers =
			Self::create_dev_node_inherent_data_providers(client.clone(), para_id, slot_duration);

		match mode {
			DevSealMode::InstantSeal => {
				let params = sc_consensus_manual_seal::InstantSealParams {
					block_import: client.clone(),
					env: proposer,
					client: client.clone(),
					pool: transaction_pool.clone(),
					select_chain: LongestChain::new(backend.clone()),
					consensus_data_provider: Some(Box::new(aura_digest_provider)),
					create_inherent_data_providers,
				};

				let authorship_future = sc_consensus_manual_seal::run_instant_seal(params);
				task_manager.spawn_essential_handle().spawn_blocking(
					"instant-seal",
					None,
					authorship_future,
				);
			},
			DevSealMode::ManualSeal(block_time) => {
				let (manual_seal_sink, manual_seal_stream) = futures::channel::mpsc::channel(1024);
				let mut manual_seal_sink_clone = manual_seal_sink.clone();
				task_manager
					.spawn_essential_handle()
					.spawn("block_authoring", None, async move {
						loop {
							futures_timer::Delay::new(std::time::Duration::from_millis(block_time))
								.await;
							manual_seal_sink_clone
								.try_send(sc_consensus_manual_seal::EngineCommand::SealNewBlock {
									create_empty: true,
									finalize: true,
									parent_hash: None,
									sender: None,
								})
								.unwrap();
						}
					});

				let params = sc_consensus_manual_seal::ManualSealParams {
					block_import: client.clone(),
					env: proposer,
					client: client.clone(),
					pool: transaction_pool.clone(),
					select_chain: LongestChain::new(backend.clone()),
					commands_stream: Box::pin(manual_seal_stream),
					consensus_data_provider: Some(Box::new(aura_digest_provider)),
					create_inherent_data_providers,
				};

				let authorship_future = sc_consensus_manual_seal::run_manual_seal(params);
				task_manager.spawn_essential_handle().spawn_blocking(
					"manual-seal",
					None,
					authorship_future,
				);
			},
		}
		let hop_pool = hop
			.as_ref()
			.map(|params| {
				params.build_pool(
					config.database.path().map(|p| p.to_path_buf()),
					config.prometheus_registry(),
				)
			})
			.transpose()
			.map_err(|e| sc_service::Error::Application(Box::new(e)))?;
		if let (Some(pool), Some(hop)) = (hop_pool.as_ref(), hop.as_ref()) {
			let task = sc_hop::build_maintenance_task::<Block, _, _>(
				&client,
				&transaction_pool,
				pool.clone(),
				hop.promotion_buffer_secs,
				hop.check_interval,
			);
			task_manager.spawn_handle().spawn("hop-maintenance", None, task.run());
		}

		let spawn_handle = Arc::new(task_manager.spawn_handle());
		let rpc_extensions_builder = {
			let client = client.clone();
			let transaction_pool = transaction_pool.clone();
			let backend_for_rpc = backend.clone();
			let statement_store = statement_store.clone();
			let hop_pool = hop_pool.clone();

			Box::new(move |_| {
				let module = Self::BuildRpcExtensions::build_rpc_extensions(
					client.clone(),
					backend_for_rpc.clone(),
					transaction_pool.clone(),
					statement_store.clone(),
					hop_pool.clone(),
					spawn_handle.clone(),
				)?;
				Ok(module)
			})
		};

		let database_path = config.database.path().map(|p| p.to_path_buf());

		let _rpc_handlers = sc_service::spawn_tasks(sc_service::SpawnTasksParams {
			network,
			client,
			keystore: keystore_container.keystore(),
			task_manager: &mut task_manager,
			transaction_pool,
			rpc_builder: rpc_extensions_builder,
			backend,
			system_rpc_tx,
			tx_handler_controller,
			sync_service,
			config,
			telemetry: telemetry.as_mut(),
			tracing_execute_block: None,
		})?;

		// Spawn the storage monitor.
		if let Some(database_path) = database_path {
			sc_storage_monitor::StorageMonitorService::try_spawn(
				storage_monitor.clone(),
				database_path,
				&task_manager.spawn_essential_handle(),
			)
			.map_err(|e| sc_service::Error::Application(Box::new(e) as Box<_>))?;
		}

		Ok(task_manager)
	}
}

impl<Block, RuntimeApi, AuraId, StartConsensus, InitBlockImport>
	AuraNode<Block, RuntimeApi, AuraId, StartConsensus, InitBlockImport>
where
	Block: NodeBlock,
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId>,
	AuraId: AuraIdT + Sync,
{
	/// Creates the inherent data providers for manual and instant seal consensus.
	///
	/// This function sets up the timestamp and parachain validation data providers
	/// required for dev seal block production in a parachain environment.
	fn create_dev_node_inherent_data_providers(
		client: Arc<ParachainClient<Block, RuntimeApi>>,
		para_id: ParaId,
		slot_duration: sp_consensus_aura::SlotDuration,
	) -> impl Fn(
		Hash,
		(),
	) -> future::Ready<
		Result<
			(sp_timestamp::InherentDataProvider, MockValidationDataInherentDataProvider<()>),
			Box<dyn std::error::Error + Send + Sync>,
		>,
	> + Send
	       + Sync {
		const RELAY_CHAIN_SLOT_DURATION_MILLIS: u64 = 6000;

		// Start 2 hours in the past to avoid timestamps immediately running into the future.
		let initial_relay_slot = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.expect("Current time is always after UNIX_EPOCH; qed")
			.sub(Duration::from_secs(2 * 60 * 60))
			.as_millis() as u64 /
			RELAY_CHAIN_SLOT_DURATION_MILLIS;

		move |block: Hash, ()| {
			let current_para_head = client
				.header(block)
				.expect("Header lookup should succeed")
				.expect("Header passed in as parent should be present in backend.");

			let should_send_go_ahead = client
				.runtime_api()
				.collect_collation_info(block, &current_para_head)
				.map(|info| info.new_validation_code.is_some())
				.unwrap_or_default();

			let current_para_block_head =
				Some(polkadot_primitives::HeadData(current_para_head.encode()));
			let current_block_number =
				UniqueSaturatedInto::<u32>::unique_saturated_into(*current_para_head.number()) + 1;
			log::info!("Current block number: {current_block_number}");

			let relay_parent_offset =
				client.runtime_api().relay_parent_offset(block).unwrap_or_default();

			let relay_blocks_per_para_block =
				(slot_duration.as_millis() / RELAY_CHAIN_SLOT_DURATION_MILLIS).max(1) as u32;

			// Each para block gets a unique relay slot: initial_relay_slot +
			// relay_blocks_per_para_block * block_number
			let target_relay_slot = initial_relay_slot +
				u64::from(current_block_number) * u64::from(relay_blocks_per_para_block);

			let relay_offset = (target_relay_slot as u32)
				.saturating_sub(relay_blocks_per_para_block * current_block_number);

			let mocked_parachain = MockValidationDataInherentDataProvider::<()> {
				current_para_block: current_block_number,
				para_id,
				current_para_block_head,
				relay_blocks_per_para_block,
				relay_offset,
				relay_parent_offset,
				para_blocks_per_relay_epoch: 10,
				upgrade_go_ahead: should_send_go_ahead.then(|| {
					log::info!("Detected pending validation code, sending go-ahead signal.");
					UpgradeGoAhead::GoAhead
				}),
				..Default::default()
			};

			let timestamp = target_relay_slot * RELAY_CHAIN_SLOT_DURATION_MILLIS;

			let timestamp_provider = sp_timestamp::InherentDataProvider::new(timestamp.into());

			futures::future::ready(Ok((timestamp_provider, mocked_parachain)))
		}
	}
}

pub fn new_aura_node_spec<Block, RuntimeApi, AuraId>(
	extra_args: &NodeExtraArgs,
) -> Box<dyn DynNodeSpec>
where
	Block: NodeBlock,
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId>
		+ pallet_transaction_payment_rpc::TransactionPaymentRuntimeApi<Block, Balance>
		+ substrate_frame_rpc_system::AccountNonceApi<Block, AccountId, Nonce>
		+ TargetBlockRate<Block>
		+ GetParachainInfo<Block>,
	AuraId: AuraIdT + Sync + Send,
	<AuraId as AppCrypto>::Pair: Send + Sync,
{
	if extra_args.authoring_policy == AuthoringPolicy::SlotBased {
		Box::new(AuraNode::<
			Block,
			RuntimeApi,
			AuraId,
			StartSlotBasedAuraConsensus<Block, RuntimeApi, AuraId>,
			StartSlotBasedAuraConsensus<Block, RuntimeApi, AuraId>,
		>::default())
	} else {
		Box::new(AuraNode::<
			Block,
			RuntimeApi,
			AuraId,
			StartLookaheadAuraConsensus<Block, RuntimeApi, AuraId>,
			ClientBlockImport,
		>::default())
	}
}

/// Start consensus using the lookahead aura collator.
pub(crate) struct StartSlotBasedAuraConsensus<Block, RuntimeApi, AuraId>(
	PhantomData<(Block, RuntimeApi, AuraId)>,
);

impl<Block: BlockT<Hash = DbHash>, RuntimeApi, AuraId>
	StartSlotBasedAuraConsensus<Block, RuntimeApi, AuraId>
where
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId> + TargetBlockRate<Block>,
	AuraId: AuraIdT + Sync + Send,
	<AuraId as AppCrypto>::Pair: Send + Sync,
{
	#[docify::export_content]
	fn launch_slot_based_collator<CIDP, CHP, Proposer, CS, Spawner>(
		params_with_export: SlotBasedParams<
			Block,
			ParachainBlockImport<
				Block,
				SlotBasedBlockImport<
					Block,
					StorageChainBlockImport<
						Block,
						Arc<ParachainClient<Block, RuntimeApi>>,
						ParachainClient<Block, RuntimeApi>,
					>,
					ParachainClient<Block, RuntimeApi>,
				>,
			>,
			CIDP,
			ParachainClient<Block, RuntimeApi>,
			ParachainBackend<Block>,
			Arc<dyn RelayChainInterface>,
			CHP,
			Proposer,
			CS,
			Spawner,
		>,
	) where
		CIDP: CreateInherentDataProviders<Block, ()> + 'static,
		CIDP::InherentDataProviders: Send,
		CHP: cumulus_client_consensus_common::ValidationCodeHashProvider<Hash>
			+ Send
			+ Sync
			+ 'static,
		Proposer: Environment<Block> + Send + Sync + 'static,
		CS: CollatorServiceInterface<Block> + Send + Sync + Clone + 'static,
		Spawner: SpawnEssentialNamed + Clone + 'static,
	{
		slot_based::run::<Block, <AuraId as AppCrypto>::Pair, _, _, _, _, _, _, _, _, _>(
			params_with_export,
		);
	}
}

impl<Block: BlockT<Hash = DbHash>, RuntimeApi, AuraId>
	StartConsensus<
		Block,
		RuntimeApi,
		SlotBasedBlockImport<
			Block,
			StorageChainBlockImport<
				Block,
				Arc<ParachainClient<Block, RuntimeApi>>,
				ParachainClient<Block, RuntimeApi>,
			>,
			ParachainClient<Block, RuntimeApi>,
		>,
		SlotBasedBlockImportHandle<Block>,
	> for StartSlotBasedAuraConsensus<Block, RuntimeApi, AuraId>
where
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId> + TargetBlockRate<Block>,
	AuraId: AuraIdT + Sync + Send,
	<AuraId as AppCrypto>::Pair: Send + Sync,
{
	fn start_consensus(
		client: Arc<ParachainClient<Block, RuntimeApi>>,
		block_import: ParachainBlockImport<
			Block,
			SlotBasedBlockImport<
				Block,
				StorageChainBlockImport<
					Block,
					Arc<ParachainClient<Block, RuntimeApi>>,
					ParachainClient<Block, RuntimeApi>,
				>,
				ParachainClient<Block, RuntimeApi>,
			>,
		>,
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
		_overseer_handle: OverseerHandle,
		announce_block: Arc<dyn Fn(Hash, Option<Vec<u8>>) + Send + Sync>,
		backend: Arc<ParachainBackend<Block>>,
		node_extra_args: NodeExtraArgs,
		block_import_handle: SlotBasedBlockImportHandle<Block>,
	) -> Result<(), Error> {
		let proposer = sc_basic_authorship::ProposerFactory::new(
			task_manager.spawn_handle(),
			client.clone(),
			transaction_pool,
			prometheus_registry,
			telemetry.clone(),
		);

		let collator_service = CollatorService::new(client.clone(), announce_block, client.clone());

		let client_for_aura = client.clone();
		let client_clone = client.clone();
		let params = SlotBasedParams {
			create_inherent_data_providers: move |parent, ()| {
				let client_clone = client_clone.clone();
				async move {
					let has_tx_storage_api = client_clone
						.runtime_api()
						.has_api_with::<dyn TransactionStorageApi<Block>, _>(parent, |v| v >= 1)
						.unwrap_or(false);
					if has_tx_storage_api {
						let storage_proof =
							sp_transaction_storage_proof::registration::new_data_provider(
								&*client_clone,
								&parent,
								client_clone.runtime_api().retention_period(parent)?,
							)?;
						Ok(vec![storage_proof])
					} else {
						Ok(vec![])
					}
				}
			},
			block_import,
			para_client: client.clone(),
			para_backend: backend.clone(),
			relay_client: relay_chain_interface,
			relay_chain_slot_duration,
			code_hash_provider: move |block_hash| {
				client_for_aura.code_at(block_hash).ok().map(|c| ValidationCode::from(c).hash())
			},
			keystore,
			collator_key,
			collator_peer_id,
			para_id,
			proposer,
			collator_service,
			reinitialize: false,
			slot_offset: Duration::from_secs(1),
			block_import_handle,
			spawner: task_manager.spawn_essential_handle(),
			export_pov: node_extra_args.export_pov,
			max_pov_percentage: node_extra_args.max_pov_percentage,
		};

		let wait_client = client.clone();
		let fut = async move {
			wait_for_aura::<Block, RuntimeApi, AuraId>(wait_client).await;
			// We have a separate function only to be able to use `docify::export` on this
			// piece of code.
			Self::launch_slot_based_collator(params);
		};
		task_manager.spawn_handle().spawn("slot-based-collator-init", None, fut);

		Ok(())
	}
}

impl<Block: BlockT<Hash = DbHash>, RuntimeApi, AuraId> InitBlockImport<Block, RuntimeApi>
	for StartSlotBasedAuraConsensus<Block, RuntimeApi, AuraId>
where
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId>,
	AuraId: AuraIdT + Sync,
{
	type BlockImport = SlotBasedBlockImport<
		Block,
		StorageChainBlockImport<
			Block,
			Arc<ParachainClient<Block, RuntimeApi>>,
			ParachainClient<Block, RuntimeApi>,
		>,
		ParachainClient<Block, RuntimeApi>,
	>;
	type BlockImportAuxiliaryData = SlotBasedBlockImportHandle<Block>;

	fn init_block_import(
		client: Arc<ParachainClient<Block, RuntimeApi>>,
		storage_chain_block_import: StorageChainBlockImport<
			Block,
			Arc<ParachainClient<Block, RuntimeApi>>,
			ParachainClient<Block, RuntimeApi>,
		>,
	) -> sc_service::error::Result<(Self::BlockImport, Self::BlockImportAuxiliaryData)> {
		Ok(SlotBasedBlockImport::new(storage_chain_block_import, client))
	}
}

/// Wait for the Aura runtime API to appear on chain.
/// This is useful for chains that started out without Aura. Components that
/// are depending on Aura functionality will wait until Aura appears in the runtime.
async fn wait_for_aura<Block: BlockT, RuntimeApi, AuraId>(
	client: Arc<ParachainClient<Block, RuntimeApi>>,
) where
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId>,
	AuraId: AuraIdT + Sync,
{
	let finalized_hash = client.chain_info().finalized_hash;
	if client.runtime_api().has_aura_api(finalized_hash) {
		return;
	};

	let mut stream = client.finality_notification_stream();
	while let Some(notification) = stream.next().await {
		if client.runtime_api().has_aura_api(notification.hash) {
			return;
		}
	}
}

/// Start consensus using the lookahead aura collator.
pub(crate) struct StartLookaheadAuraConsensus<Block, RuntimeApi, AuraId>(
	PhantomData<(Block, RuntimeApi, AuraId)>,
);

impl<Block: BlockT<Hash = DbHash>, RuntimeApi, AuraId>
	StartConsensus<
		Block,
		RuntimeApi,
		StorageChainBlockImport<
			Block,
			Arc<ParachainClient<Block, RuntimeApi>>,
			ParachainClient<Block, RuntimeApi>,
		>,
		(),
	> for StartLookaheadAuraConsensus<Block, RuntimeApi, AuraId>
where
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<Block, RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId>,
	AuraId: AuraIdT + Sync + Send,
	<AuraId as AppCrypto>::Pair: Send + Sync,
{
	fn start_consensus(
		client: Arc<ParachainClient<Block, RuntimeApi>>,
		block_import: ParachainBlockImport<
			Block,
			StorageChainBlockImport<
				Block,
				Arc<ParachainClient<Block, RuntimeApi>>,
				ParachainClient<Block, RuntimeApi>,
			>,
		>,
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
		_: (),
	) -> Result<(), Error> {
		let proposer = sc_basic_authorship::ProposerFactory::new(
			task_manager.spawn_handle(),
			client.clone(),
			transaction_pool,
			prometheus_registry,
			telemetry.clone(),
		);
		let collator_service = CollatorService::new(client.clone(), announce_block, client.clone());

		let client_clone = client.clone();
		let params = aura::ParamsWithExport {
			export_pov: node_extra_args.export_pov,
			params: AuraParams {
				create_inherent_data_providers: move |parent, ()| {
					let client_clone = client_clone.clone();
					async move {
						let has_tx_storage_api = client_clone
							.runtime_api()
							.has_api_with::<dyn TransactionStorageApi<Block>, _>(parent, |v| v >= 1)
							.unwrap_or(false);
						if has_tx_storage_api {
							let storage_proof =
								sp_transaction_storage_proof::registration::new_data_provider(
									&*client_clone,
									&parent,
									client_clone.runtime_api().retention_period(parent)?,
								)?;
							Ok(vec![storage_proof])
						} else {
							Ok(vec![])
						}
					}
				},
				block_import,
				para_client: client.clone(),
				para_backend: backend,
				relay_client: relay_chain_interface,
				code_hash_provider: {
					let client = client.clone();
					move |block_hash| {
						client.code_at(block_hash).ok().map(|c| ValidationCode::from(c).hash())
					}
				},
				keystore,
				collator_key,
				collator_peer_id,
				para_id,
				overseer_handle,
				relay_chain_slot_duration,
				proposer,
				collator_service,
				authoring_duration: Duration::from_millis(2000),
				reinitialize: false,
				max_pov_percentage: node_extra_args.max_pov_percentage,
			},
		};

		let fut = async move {
			wait_for_aura(client).await;
			aura::run_with_export::<Block, <AuraId as AppCrypto>::Pair, _, _, _, _, _, _, _, _>(
				params,
			)
			.await;
		};
		task_manager.spawn_essential_handle().spawn("aura", None, fut);

		Ok(())
	}
}
