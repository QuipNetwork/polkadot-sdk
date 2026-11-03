// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
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

use crate::cli::Consensus;
use polkadot_sdk::{
	sc_client_api::StorageProvider,
	sc_executor::WasmExecutor,
	sc_service::{error::Error as ServiceError, Configuration, TaskManager},
	sc_telemetry::{Telemetry, TelemetryWorker},
	sp_runtime::traits::Block as BlockT,
	*,
};
use revive_dev_runtime::{OpaqueBlock as Block, Runtime, RuntimeApi};
use std::sync::Arc;

type HostFunctions = (sp_io::SubstrateHostFunctions, sp_virtualization::HostFunctions);

#[docify::export]
pub(crate) type FullClient =
	sc_service::TFullClient<Block, RuntimeApi, WasmExecutor<HostFunctions>>;

type FullBackend = sc_service::TFullBackend<Block>;
type FullSelectChain = sc_consensus::LongestChain<FullBackend, Block>;

/// Assembly of PartialComponents (enough to run chain ops subcommands)
pub type Service = sc_service::PartialComponents<
	FullClient,
	FullBackend,
	FullSelectChain,
	sc_consensus::DefaultImportQueue<Block>,
	sc_transaction_pool::TransactionPoolHandle<Block, FullClient>,
	Option<Telemetry>,
>;

pub fn new_partial(config: &Configuration) -> Result<Service, ServiceError> {
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

	let executor = sc_service::new_wasm_executor(&config.executor);

	let (client, backend, keystore_container, task_manager) =
		sc_service::new_full_parts::<Block, RuntimeApi, _>(
			config,
			telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
			executor,
			Default::default(),
		)?;
	let client = Arc::new(client);

	let telemetry = telemetry.map(|(worker, telemetry)| {
		task_manager.spawn_handle().spawn("telemetry", None, worker.run());
		telemetry
	});

	let select_chain = sc_consensus::LongestChain::new(backend.clone());

	let transaction_pool = Arc::from(
		sc_transaction_pool::Builder::new(
			task_manager.spawn_essential_handle(),
			client.clone(),
			config.role.is_authority().into(),
		)
		.with_options(config.transaction_pool.clone())
		.build(),
	);

	let import_queue = sc_consensus_manual_seal::import_queue(
		Box::new(client.clone()),
		&task_manager.spawn_essential_handle(),
		None,
	);

	Ok(sc_service::PartialComponents {
		client,
		backend,
		task_manager,
		import_queue,
		keystore_container,
		select_chain,
		transaction_pool,
		other: (telemetry),
	})
}

/// Builds a new service for a full client.
pub fn new_full<Network: sc_network::NetworkBackend<Block, <Block as BlockT>::Hash>>(
	config: Configuration,
	consensus: Consensus,
) -> Result<TaskManager, ServiceError> {
	let sc_service::PartialComponents {
		client,
		backend,
		mut task_manager,
		import_queue,
		keystore_container,
		select_chain,
		transaction_pool,
		other: mut telemetry,
	} = new_partial(&config)?;

	let net_config = sc_network::config::FullNetworkConfiguration::<
		Block,
		<Block as BlockT>::Hash,
		Network,
	>::new(&config.network, None);
	let metrics = Network::register_notification_metrics(None);

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
			warp_sync_config: None,
			block_relay: None,
			metrics,
			gap_sync_body_policy: None,
		})?;

	let rpc_extensions_builder = {
		let client = client.clone();
		let pool = transaction_pool.clone();

		Box::new(move |_| {
			let deps =
				crate::rpc::FullDeps { client: client.clone(), pool: pool.clone(), consensus };
			crate::rpc::create_full(deps).map_err(Into::into)
		})
	};

	let _rpc_handlers = sc_service::spawn_tasks(sc_service::SpawnTasksParams {
		network,
		client: client.clone(),
		keystore: keystore_container.keystore(),
		task_manager: &mut task_manager,
		transaction_pool: transaction_pool.clone(),
		rpc_builder: rpc_extensions_builder,
		backend,
		system_rpc_tx,
		tx_handler_controller,
		sync_service,
		config,
		telemetry: telemetry.as_mut(),
		tracing_execute_block: None,
	})?;

	let proposer = sc_basic_authorship::ProposerFactory::new(
		task_manager.spawn_handle(),
		client.clone(),
		transaction_pool.clone(),
		None,
		telemetry.as_ref().map(|x| x.handle()),
	);

	// Due to instant seal or low block time multiple blocks can have the same timestamp.
	// This is because Etereum only uses second granularity (as opposed to ms).
	// Here we make sure that we increment by at least a second from the last block.
	//
	// # Warning
	//
	// This will lead to blocks with timestamps in the future. This might cause other issues
	// when dealing with off chain data. But for a development node it is more important to not
	// have duplicate timestamps. The only way to not have timestamps in the future and no
	// duplicates is to set the block time to at least one second (`--consensus manual-seal-1000`).
	let timestamp_provider = {
		let client = client.clone();
		move |parent, ()| {
			let client = client.clone();
			async move {
				let key = sp_core::storage::StorageKey(
					polkadot_sdk::pallet_timestamp::Now::<Runtime>::hashed_key().to_vec(),
				);
				let current = sp_timestamp::Timestamp::current();
				let next = client
					.storage(parent, &key)
					.ok()
					.flatten()
					.and_then(|data| data.0.try_into().ok())
					.map(|data| {
						let last = u64::from_le_bytes(data) / 1000;
						sp_timestamp::Timestamp::new((last + 1) * 1000)
					})
					.unwrap_or(current);
				Ok(sp_timestamp::InherentDataProvider::new(current.max(next)))
			}
		}
	};

	match consensus {
		Consensus::InstantSeal => {
			let params = sc_consensus_manual_seal::InstantSealParams {
				block_import: client.clone(),
				env: proposer,
				client,
				pool: transaction_pool,
				select_chain,
				consensus_data_provider: None,
				create_inherent_data_providers: timestamp_provider,
			};

			let authorship_future = sc_consensus_manual_seal::run_instant_seal_and_finalize(params);

			task_manager.spawn_essential_handle().spawn_blocking(
				"instant-seal",
				None,
				authorship_future,
			);
		},
		Consensus::ManualSeal(block_time) => {
			let (mut sink, commands_stream) = futures::channel::mpsc::channel(1024);
			task_manager.spawn_handle().spawn("block_authoring", None, async move {
				loop {
					futures_timer::Delay::new(std::time::Duration::from_millis(block_time)).await;
					sink.try_send(sc_consensus_manual_seal::EngineCommand::SealNewBlock {
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
				client,
				pool: transaction_pool,
				select_chain,
				commands_stream: Box::pin(commands_stream),
				consensus_data_provider: None,
				create_inherent_data_providers: timestamp_provider,
			};
			let authorship_future = sc_consensus_manual_seal::run_manual_seal(params);

			task_manager.spawn_essential_handle().spawn_blocking(
				"manual-seal",
				None,
				authorship_future,
			);
		},
		_ => {},
	}

	Ok(task_manager)
}
