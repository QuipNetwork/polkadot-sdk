// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Contains the [`OverheadCmd`] as entry point for the CLI to execute
//! the *overhead* benchmarks.

use crate::{
	extrinsic::{
		bench::{Benchmark, BenchmarkParams as ExtrinsicBenchmarkParams},
		ExtrinsicBuilder,
	},
	overhead::{
		command::ChainType::{Parachain, Relaychain, Unknown},
		fake_runtime_api,
		remark_builder::{RuntimeVersion, SubstrateRemarkBuilder},
		template::TemplateData,
	},
	shared::{
		genesis_state,
		genesis_state::{GenesisStateHandler, SpecGenesisSource},
		HostInfoParams, WeightParams,
	},
};
use clap::{error::ErrorKind, Args, CommandFactory, Parser};
use codec::{Decode, Encode};
use cumulus_client_parachain_inherent::MockValidationDataInherentDataProvider;
use cumulus_primitives_core::RelayParentOffsetApi;
use fake_runtime_api::RuntimeApi as FakeRuntimeApi;
use frame_support::Deserialize;
use genesis_state::WARN_SPEC_GENESIS_CTOR;
use log::info;
use polkadot_parachain_primitives::primitives::Id as ParaId;
use sc_block_builder::BlockBuilderApi;
use sc_chain_spec::{ChainSpec, ChainSpecExtension, GenesisBlockBuilder};
use sc_cli::{CliConfiguration, Database, ImportParams, Result, SharedParams};
use sc_client_api::{execution_extensions::ExecutionExtensions, UsageProvider};
use sc_client_db::{BlocksPruning, DatabaseSettings};
use sc_executor::WasmExecutor;
use sc_runtime_utilities::fetch_latest_metadata_from_code_blob;
use sc_service::{new_client, new_db_backend, BasePath, ClientConfig, TFullClient, TaskManager};
use serde::Serialize;
use serde_json::{json, Value};
use sp_api::{ApiExt, CallApiAt, Core, ProvideRuntimeApi};
use sp_blockchain::HeaderBackend;
use sp_core::H256;
use sp_inherents::{InherentData, InherentDataProvider};
use sp_runtime::{
	generic,
	traits::{BlakeTwo256, Block as BlockT},
	DigestItem, OpaqueExtrinsic,
};
use sp_storage::Storage;
use sp_wasm_interface::HostFunctions;
use std::{
	fmt::{Display, Formatter},
	fs,
	path::PathBuf,
	sync::Arc,
};
use subxt::{ext::futures, Metadata};

const DEFAULT_PARA_ID: u32 = 100;
const LOG_TARGET: &'static str = "polkadot_sdk_frame::benchmark::overhead";

/// Benchmark the execution overhead per-block and per-extrinsic.
#[derive(Debug, Parser)]
pub struct OverheadCmd {
	#[allow(missing_docs)]
	#[clap(flatten)]
	pub shared_params: SharedParams,

	#[allow(missing_docs)]
	#[clap(flatten)]
	pub import_params: ImportParams,

	#[allow(missing_docs)]
	#[clap(flatten)]
	pub params: OverheadParams,
}

/// Configures the benchmark, the post-processing and weight generation.
#[derive(Debug, Default, Serialize, Clone, PartialEq, Args)]
pub struct OverheadParams {
	#[allow(missing_docs)]
	#[clap(flatten)]
	pub weight: WeightParams,

	#[allow(missing_docs)]
	#[clap(flatten)]
	pub bench: ExtrinsicBenchmarkParams,

	#[allow(missing_docs)]
	#[clap(flatten)]
	pub hostinfo: HostInfoParams,

	/// Add a header to the generated weight output file.
	///
	/// Good for adding LICENSE headers.
	#[arg(long, value_name = "PATH")]
	pub header: Option<PathBuf>,

	/// Enable the Trie cache.
	///
	/// This should only be used for performance analysis and not for final results.
	#[arg(long)]
	pub enable_trie_cache: bool,

	/// Optional runtime blob to use instead of the one from the genesis config.
	#[arg(
		long,
		value_name = "PATH",
		conflicts_with = "chain",
		required_if_eq("genesis_builder", "runtime")
	)]
	pub runtime: Option<PathBuf>,

	/// The preset that we expect to find in the GenesisBuilder runtime API.
	///
	/// This can be useful when a runtime has a dedicated benchmarking preset instead of using the
	/// default one.
	#[arg(long, default_value = sp_genesis_builder::DEV_RUNTIME_PRESET)]
	pub genesis_builder_preset: String,

	/// How to construct the genesis state.
	///
	/// Can be used together with `--chain` to determine whether the
	/// genesis state should be initialized with the values from the
	/// provided chain spec or a runtime-provided genesis preset.
	#[arg(long, value_enum, alias = "genesis-builder-policy")]
	pub genesis_builder: Option<GenesisBuilderPolicy>,

	/// Parachain Id to use for parachains. If not specified, the benchmark code will choose
	/// a para-id and patch the state accordingly.
	#[arg(long)]
	pub para_id: Option<u32>,

	/// Path to a JSON file containing a patch to apply to the genesis state.
	///
	/// This allows modifying the genesis state after it's built but before benchmarking.
	/// Useful for creating specific testing scenarios like many accounts for benchmarking.
	#[arg(long)]
	pub genesis_patch: Option<PathBuf>,
}

/// How the genesis state for benchmarking should be built.
#[derive(clap::ValueEnum, Debug, Eq, PartialEq, Clone, Copy, Serialize)]
#[clap(rename_all = "kebab-case")]
pub enum GenesisBuilderPolicy {
	/// Let the runtime build the genesis state through its `BuildGenesisConfig` runtime API.
	/// This will use the `development` preset by default.
	Runtime,
	/// Use the runtime from the Spec file to build the genesis state.
	SpecRuntime,
	/// Use the spec file to build the genesis state. This fails when there is no spec.
	#[value(alias = "spec")]
	SpecGenesis,
}

/// Type of a benchmark.
#[derive(Serialize, Clone, PartialEq, Copy)]
pub(crate) enum BenchmarkType {
	/// Measure the per-extrinsic execution overhead.
	Extrinsic,
	/// Measure the per-block execution overhead.
	Block,
}

/// Hostfunctions that are typically used by parachains.
pub type ParachainHostFunctions = (
	cumulus_primitives_proof_size_hostfunction::storage_proof_size::HostFunctions,
	sp_io::SubstrateHostFunctions,
);

pub type BlockNumber = u32;

/// Typical block header.
pub type Header = generic::Header<BlockNumber, BlakeTwo256>;

/// Typical block type using `OpaqueExtrinsic`.
pub type OpaqueBlock = generic::Block<Header, OpaqueExtrinsic>;

/// Client type used throughout the benchmarking code.
type OverheadClient<Block, HF> = TFullClient<Block, FakeRuntimeApi, WasmExecutor<HF>>;

/// Creates inherent data for a given parachain ID.
///
/// This function constructs the inherent data required for block execution,
/// including the relay chain state and validation data. Not all of these
/// inherents are required for every chain. The runtime will pick the ones
/// it requires based on their identifier.
fn create_inherent_data<Client: UsageProvider<Block> + HeaderBackend<Block>, Block: BlockT>(
	client: &Arc<Client>,
	chain_type: &ChainType,
	relay_parent_offset: u32,
) -> InherentData {
	let genesis = client.usage_info().chain.best_hash;
	let header = client.header(genesis).unwrap().unwrap();

	let mut inherent_data = InherentData::new();

	// Para inherent can only makes sense when we are handling a parachain.
	if let Parachain(para_id) = chain_type {
		let parachain_validation_data_provider = MockValidationDataInherentDataProvider::<()> {
			para_id: ParaId::from(*para_id),
			current_para_block_head: Some(header.encode().into()),
			relay_offset: 0,
			relay_parent_offset,
			..Default::default()
		};
		let _ = futures::executor::block_on(
			parachain_validation_data_provider.provide_inherent_data(&mut inherent_data),
		);
	}

	// Parachain inherent that is used on relay chains to perform parachain validation.
	let para_inherent = polkadot_primitives::InherentData {
		bitfields: Vec::new(),
		backed_candidates: Vec::new(),
		disputes: Vec::new(),
		parent_header: header,
	};

	// Timestamp inherent that is very common in substrate chains.
	let timestamp = sp_timestamp::InherentDataProvider::new(std::time::Duration::default().into());

	let _ = futures::executor::block_on(timestamp.provide_inherent_data(&mut inherent_data));
	let _ =
		inherent_data.put_data(polkadot_primitives::PARACHAINS_INHERENT_IDENTIFIER, &para_inherent);

	inherent_data
}

/// Identifies what kind of chain we are dealing with.
///
/// Chains containing the `ParachainSystem` and `ParachainInfo` pallet are considered parachains.
/// Chains containing the `ParaInherent` pallet are considered relay chains.
fn identify_chain(metadata: &Metadata, para_id: Option<u32>) -> ChainType {
	let parachain_info_exists = metadata.pallet_by_name("ParachainInfo").is_some();
	let parachain_system_exists = metadata.pallet_by_name("ParachainSystem").is_some();
	let para_inherent_exists = metadata.pallet_by_name("ParaInherent").is_some();

	log::debug!("{} ParachainSystem", if parachain_system_exists { "✅" } else { "❌" });
	log::debug!("{} ParachainInfo", if parachain_info_exists { "✅" } else { "❌" });
	log::debug!("{} ParaInherent", if para_inherent_exists { "✅" } else { "❌" });

	let chain_type = if parachain_system_exists && parachain_info_exists {
		Parachain(para_id.unwrap_or(DEFAULT_PARA_ID))
	} else if para_inherent_exists {
		Relaychain
	} else {
		Unknown
	};

	log::info!(target: LOG_TARGET, "Identified Chain type from metadata: {}", chain_type);

	chain_type
}

#[derive(Deserialize, Serialize, Clone, ChainSpecExtension)]
pub struct ParachainExtension {
	/// The id of the Parachain.
	pub para_id: Option<u32>,
}

impl OverheadCmd {
	fn state_handler_from_cli<HF: HostFunctions>(
		&self,
		chain_spec_from_api: Option<Box<dyn ChainSpec>>,
	) -> Result<(GenesisStateHandler, Option<u32>)> {
		let genesis_builder_to_source = || match self.params.genesis_builder {
			Some(GenesisBuilderPolicy::Runtime) | Some(GenesisBuilderPolicy::SpecRuntime) => {
				SpecGenesisSource::Runtime(self.params.genesis_builder_preset.clone())
			},
			Some(GenesisBuilderPolicy::SpecGenesis) | None => {
				log::warn!(target: LOG_TARGET, "{WARN_SPEC_GENESIS_CTOR}");
				SpecGenesisSource::SpecJson
			},
		};

		// First handle chain-spec passed in via API parameter.
		if let Some(chain_spec) = chain_spec_from_api {
			log::debug!(target: LOG_TARGET, "Initializing state handler with chain-spec from API: {:?}", chain_spec);

			let source = genesis_builder_to_source();
			return Ok((GenesisStateHandler::ChainSpec(chain_spec, source), self.params.para_id));
		};

		// Handle chain-spec passed in via CLI.
		if let Some(chain_spec_path) = &self.shared_params.chain {
			log::debug!(target: LOG_TARGET,
				"Initializing state handler with chain-spec from path: {:?}",
				chain_spec_path
			);
			let (chain_spec, para_id_from_chain_spec) =
				genesis_state::chain_spec_from_path::<HF>(chain_spec_path.to_string().into())?;

			let source = genesis_builder_to_source();

			return Ok((
				GenesisStateHandler::ChainSpec(chain_spec, source),
				self.params.para_id.or(para_id_from_chain_spec),
			));
		};

		// Check for runtimes. In general, we make sure that `--runtime` and `--chain` are
		// incompatible on the CLI level.
		if let Some(runtime_path) = &self.params.runtime {
			log::debug!(target: LOG_TARGET, "Initializing state handler with runtime from path: {:?}", runtime_path);

			let runtime_blob = fs::read(runtime_path)?;
			return Ok((
				GenesisStateHandler::Runtime(
					runtime_blob,
					Some(self.params.genesis_builder_preset.clone()),
				),
				self.params.para_id,
			));
		};

		Err("Neither a runtime nor a chain-spec were specified".to_string().into())
	}

	fn check_args(
		&self,
		chain_spec: &Option<Box<dyn ChainSpec>>,
	) -> std::result::Result<(), (ErrorKind, String)> {
		if chain_spec.is_none() &&
			self.params.runtime.is_none() &&
			self.shared_params.chain.is_none()
		{
			return Err((
				ErrorKind::MissingRequiredArgument,
				"Provide either a runtime via `--runtime` or a chain spec via `--chain`"
					.to_string(),
			));
		}

		match self.params.genesis_builder {
			Some(GenesisBuilderPolicy::SpecGenesis | GenesisBuilderPolicy::SpecRuntime) => {
				if chain_spec.is_none() && self.shared_params.chain.is_none() {
					return Err((
						ErrorKind::MissingRequiredArgument,
						"Provide a chain spec via `--chain`.".to_string(),
					));
				}
			},
			_ => {},
		};
		Ok(())
	}

	/// Run the overhead benchmark with the default extrinsic builder.
	///
	/// This will use [SubstrateRemarkBuilder] to build the extrinsic. It is
	/// designed to match common configurations found in substrate chains.
	pub fn run_with_default_builder_and_spec<Block, ExtraHF>(
		&self,
		chain_spec: Option<Box<dyn ChainSpec>>,
	) -> Result<()>
	where
		Block: BlockT<Extrinsic = OpaqueExtrinsic, Hash = H256>,
		ExtraHF: HostFunctions,
	{
		self.run_with_extrinsic_builder_and_spec::<Block, ExtraHF>(
			Box::new(|metadata, hash, version| {
				Box::new(SubstrateRemarkBuilder::new(metadata, hash, version)) as Box<_>
			}),
			chain_spec,
		)
	}

	/// Run the benchmark overhead command.
	///
	/// The provided [ExtrinsicBuilder] will be used to build extrinsics for
	/// block-building. It is expected that the provided implementation builds
	/// a `System::remark` extrinsic.
	pub fn run_with_extrinsic_builder_and_spec<Block, ExtraHF>(
		&self,
		ext_builder_provider: Box<
			dyn FnOnce(Metadata, Block::Hash, RuntimeVersion) -> Box<dyn ExtrinsicBuilder>,
		>,
		chain_spec: Option<Box<dyn ChainSpec>>,
	) -> Result<()>
	where
		Block: BlockT<Extrinsic = OpaqueExtrinsic>,
		ExtraHF: HostFunctions,
	{
		if let Err((error_kind, msg)) = self.check_args(&chain_spec) {
			let mut cmd = OverheadCmd::command();
			cmd.error(error_kind, msg).exit();
		};

		let (state_handler, para_id) =
			self.state_handler_from_cli::<(ParachainHostFunctions, ExtraHF)>(chain_spec)?;

		let user_genesis_patcher = if let Some(ref patch_path) = self.params.genesis_patch {
			let patch_content = fs::read_to_string(patch_path)
				.map_err(|e| format!("Failed to read genesis patch file: {}", e))?;

			let patch_value: serde_json::Value = serde_json::from_str(&patch_content)
				.map_err(|e| format!("Failed to parse genesis patch JSON: {}", e))?;

			Some(patch_value)
		} else {
			None
		};

		let executor = WasmExecutor::<(ParachainHostFunctions, ExtraHF)>::builder()
			.with_allow_missing_host_functions(true)
			.build();

		let opaque_metadata =
			fetch_latest_metadata_from_code_blob(&executor, state_handler.get_code_bytes()?)
				.map_err(|_| {
					<&str as Into<sc_cli::Error>>::into("Unable to fetch latest stable metadata")
				})?;
		let metadata = subxt::Metadata::decode(&mut (*opaque_metadata).as_slice())?;

		// At this point we know what kind of chain we are dealing with.
		let chain_type = identify_chain(&metadata, para_id);

		// If we are dealing  with a parachain, make sure that the para id in genesis will
		// match what we expect.
		let genesis_patcher = match chain_type {
			Parachain(para_id) => Some(Box::new(move |value| {
				let mut patched_value = patch_genesis(value, Some(para_id));

				if let Some(user_patch) = &user_genesis_patcher {
					sc_chain_spec::json_patch::merge(&mut patched_value, user_patch.clone());
				}

				patched_value
			}) as Box<_>),
			_ => user_genesis_patcher.map(|user_patch| {
				Box::new(move |value| {
					let mut patched_value = value;
					sc_chain_spec::json_patch::merge(&mut patched_value, user_patch);

					patched_value
				}) as Box<_>
			}),
		};

		let client = self.build_client_components::<Block, (ParachainHostFunctions, ExtraHF)>(
			state_handler.build_storage::<(ParachainHostFunctions, ExtraHF)>(genesis_patcher)?,
			executor,
			&chain_type,
		)?;

		// Fetch the relay parent offset from the runtime.
		let relay_parent_offset = {
			let genesis = client.usage_info().chain.best_hash;
			client.runtime_api().relay_parent_offset(genesis).unwrap_or_else(|_| {
				log::debug!(
					target: LOG_TARGET,
					"Runtime does not implement RelayParentOffsetApi, using default offset of 0"
				);
				0
			})
		};

		let inherent_data = create_inherent_data(&client, &chain_type, relay_parent_offset);

		let (ext_builder, runtime_name) = {
			let genesis = client.usage_info().chain.best_hash;
			let version = client.runtime_api().version(genesis).unwrap();
			let runtime_name = version.spec_name;
			let runtime_version = RuntimeVersion {
				spec_version: version.spec_version,
				transaction_version: version.transaction_version,
			};

			(ext_builder_provider(metadata, genesis, runtime_version), runtime_name)
		};

		self.run(
			runtime_name.to_string(),
			client,
			inherent_data,
			Default::default(),
			&*ext_builder,
			chain_type.requires_proof_recording(),
		)
	}

	/// Run the benchmark overhead command.
	pub fn run_with_extrinsic_builder<Block, ExtraHF>(
		&self,
		ext_builder_provider: Box<
			dyn FnOnce(Metadata, Block::Hash, RuntimeVersion) -> Box<dyn ExtrinsicBuilder>,
		>,
	) -> Result<()>
	where
		Block: BlockT<Extrinsic = OpaqueExtrinsic>,
		ExtraHF: HostFunctions,
	{
		self.run_with_extrinsic_builder_and_spec::<Block, ExtraHF>(ext_builder_provider, None)
	}

	fn build_client_components<Block, HF>(
		&self,
		genesis_storage: Storage,
		executor: WasmExecutor<HF>,
		chain_type: &ChainType,
	) -> Result<Arc<OverheadClient<Block, HF>>>
	where
		Block: BlockT,
		HF: HostFunctions,
	{
		let extensions = ExecutionExtensions::new(None, Arc::new(executor.clone()));

		let base_path = match &self.shared_params.base_path {
			None => BasePath::new_temp_dir()?,
			Some(path) => BasePath::from(path.clone()),
		};

		let database_source = self.database_config(
			&base_path.path().to_path_buf(),
			self.database_cache_size()?.unwrap_or(1024),
			self.database()?.unwrap_or(Database::Auto),
		)?;

		let backend = new_db_backend(DatabaseSettings {
			trie_cache_maximum_size: self.trie_cache_maximum_size()?,
			state_pruning: None,
			blocks_pruning: BlocksPruning::KeepAll,
			pruning_filters: Default::default(),
			source: database_source,
			metrics_registry: None,
		})?;

		let genesis_block_builder = GenesisBlockBuilder::new_with_storage(
			genesis_storage,
			true,
			backend.clone(),
			executor.clone(),
		)?;

		let tokio_runtime = sc_cli::build_runtime()?;
		let task_manager = TaskManager::new(tokio_runtime.handle().clone(), None)
			.map_err(|_| "Unable to build task manager")?;

		let client: Arc<OverheadClient<Block, HF>> = Arc::new(new_client(
			backend.clone(),
			executor,
			genesis_block_builder,
			Default::default(),
			Default::default(),
			extensions,
			Box::new(task_manager.spawn_handle()),
			None,
			None,
			ClientConfig {
				offchain_worker_enabled: false,
				offchain_indexing_api: false,
				wasm_runtime_overrides: None,
				no_genesis: false,
				wasm_runtime_substitutes: Default::default(),
				enable_import_proof_recording: chain_type.requires_proof_recording(),
			},
		)?);

		Ok(client)
	}

	/// Measure the per-block and per-extrinsic execution overhead.
	///
	/// Writes the results to console and into two instances of the
	/// `weights.hbs` template, one for each benchmark.
	pub fn run<Block, C>(
		&self,
		chain_name: String,
		client: Arc<C>,
		inherent_data: sp_inherents::InherentData,
		digest_items: Vec<DigestItem>,
		ext_builder: &dyn ExtrinsicBuilder,
		should_record_proof: bool,
	) -> Result<()>
	where
		Block: BlockT<Extrinsic = OpaqueExtrinsic>,
		C: ProvideRuntimeApi<Block>
			+ CallApiAt<Block>
			+ UsageProvider<Block>
			+ sp_blockchain::HeaderBackend<Block>,
		C::Api: ApiExt<Block> + BlockBuilderApi<Block>,
	{
		if ext_builder.pallet() != "system" || ext_builder.extrinsic() != "remark" {
			return Err(format!("The extrinsic builder is required to build `System::Remark` extrinsics but builds `{}` extrinsics instead", ext_builder.name()).into());
		}

		let bench = Benchmark::new(
			client,
			self.params.bench.clone(),
			inherent_data,
			digest_items,
			should_record_proof,
		);

		// per-block execution overhead
		{
			let (stats, proof_size) = bench.bench_block()?;
			info!(target: LOG_TARGET, "Per-block execution overhead [ns]:\n{:?}", stats);
			let template = TemplateData::new(
				BenchmarkType::Block,
				&chain_name,
				&self.params,
				&stats,
				proof_size,
			)?;
			template.write(&self.params.weight.weight_path)?;
		}
		// per-extrinsic execution overhead
		{
			let (stats, proof_size) = bench.bench_extrinsic(ext_builder)?;
			info!(target: LOG_TARGET, "Per-extrinsic execution overhead [ns]:\n{:?}", stats);
			let template = TemplateData::new(
				BenchmarkType::Extrinsic,
				&chain_name,
				&self.params,
				&stats,
				proof_size,
			)?;
			template.write(&self.params.weight.weight_path)?;
		}

		Ok(())
	}
}

impl BenchmarkType {
	/// Short name of the benchmark type.
	pub(crate) fn short_name(&self) -> &'static str {
		match self {
			Self::Extrinsic => "extrinsic",
			Self::Block => "block",
		}
	}

	/// Long name of the benchmark type.
	pub(crate) fn long_name(&self) -> &'static str {
		match self {
			Self::Extrinsic => "ExtrinsicBase",
			Self::Block => "BlockExecution",
		}
	}
}

#[derive(Clone, PartialEq, Debug)]
enum ChainType {
	Parachain(u32),
	Relaychain,
	Unknown,
}

impl Display for ChainType {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			ChainType::Parachain(id) => write!(f, "Parachain(paraid = {})", id),
			ChainType::Relaychain => write!(f, "Relaychain"),
			ChainType::Unknown => write!(f, "Unknown"),
		}
	}
}

impl ChainType {
	fn requires_proof_recording(&self) -> bool {
		match self {
			Parachain(_) => true,
			Relaychain => false,
			Unknown => false,
		}
	}
}

/// Patch the parachain id into the genesis config. This is necessary since the inherents
/// also contain a parachain id and they need to match.
fn patch_genesis(mut input_value: Value, para_id: Option<u32>) -> Value {
	// If we identified a parachain we should patch a parachain id into the genesis config.
	// This ensures compatibility with the inherents that we provide to successfully build a
	// block.
	if let Some(para_id) = para_id {
		sc_chain_spec::json_patch::merge(
			&mut input_value,
			json!({
				"parachainInfo": {
					"parachainId": para_id,
				}
			}),
		);
		log::debug!(target: LOG_TARGET, "Genesis Config Json");
		log::debug!(target: LOG_TARGET, "{}", input_value);
	}
	input_value
}

// Boilerplate
impl CliConfiguration for OverheadCmd {
	fn shared_params(&self) -> &SharedParams {
		&self.shared_params
	}

	fn import_params(&self) -> Option<&ImportParams> {
		Some(&self.import_params)
	}

	fn base_path(&self) -> Result<Option<BasePath>> {
		Ok(Some(BasePath::new_temp_dir()?))
	}

	fn trie_cache_maximum_size(&self) -> Result<Option<usize>> {
		if self.params.enable_trie_cache {
			Ok(self.import_params().map(|x| x.trie_cache_maximum_size()).unwrap_or_default())
		} else {
			Ok(None)
		}
	}
}

#[cfg(test)]
mod tests {
	use crate::{
		overhead::command::{identify_chain, ChainType, ParachainHostFunctions, DEFAULT_PARA_ID},
		OverheadCmd,
	};
	use clap::Parser;
	use codec::Decode;
	use sc_executor::WasmExecutor;

	#[test]
	fn test_chain_type_relaychain() {
		let executor: WasmExecutor<ParachainHostFunctions> = WasmExecutor::builder().build();
		let code_bytes = westend_runtime::WASM_BINARY
			.expect("To run this test, build the wasm binary of westend-runtime")
			.to_vec();
		let opaque_metadata =
			super::fetch_latest_metadata_from_code_blob(&executor, code_bytes.into()).unwrap();
		let metadata = subxt::Metadata::decode(&mut (*opaque_metadata).as_slice()).unwrap();
		let chain_type = identify_chain(&metadata, None);
		assert_eq!(chain_type, ChainType::Relaychain);
		assert_eq!(chain_type.requires_proof_recording(), false);
	}

	#[test]
	fn test_chain_type_parachain() {
		let executor: WasmExecutor<ParachainHostFunctions> = WasmExecutor::builder().build();
		let code_bytes = cumulus_test_runtime::WASM_BINARY
			.expect("To run this test, build the wasm binary of cumulus-test-runtime")
			.to_vec();
		let opaque_metadata =
			super::fetch_latest_metadata_from_code_blob(&executor, code_bytes.into()).unwrap();
		let metadata = subxt::Metadata::decode(&mut (*opaque_metadata).as_slice()).unwrap();
		let chain_type = identify_chain(&metadata, Some(100));
		assert_eq!(chain_type, ChainType::Parachain(100));
		assert!(chain_type.requires_proof_recording());
		assert_eq!(identify_chain(&metadata, None), ChainType::Parachain(DEFAULT_PARA_ID));
	}

	#[test]
	fn test_chain_type_custom() {
		let executor: WasmExecutor<ParachainHostFunctions> = WasmExecutor::builder().build();
		let code_bytes = substrate_test_runtime::WASM_BINARY
			.expect("To run this test, build the wasm binary of substrate-test-runtime")
			.to_vec();
		let opaque_metadata =
			super::fetch_latest_metadata_from_code_blob(&executor, code_bytes.into()).unwrap();
		let metadata = subxt::Metadata::decode(&mut (*opaque_metadata).as_slice()).unwrap();
		let chain_type = identify_chain(&metadata, None);
		assert_eq!(chain_type, ChainType::Unknown);
		assert_eq!(chain_type.requires_proof_recording(), false);
	}

	fn cli_succeed(args: &[&str]) -> Result<(), clap::Error> {
		let cmd = OverheadCmd::try_parse_from(args)?;
		assert!(cmd.check_args(&None).is_ok());
		Ok(())
	}

	fn cli_fail(args: &[&str]) {
		let cmd = OverheadCmd::try_parse_from(args);
		if let Ok(cmd) = cmd {
			assert!(cmd.check_args(&None).is_err());
		}
	}

	#[test]
	fn test_cli_conflicts() -> Result<(), clap::Error> {
		// Runtime tests
		cli_succeed(&["test", "--runtime", "path/to/runtime", "--genesis-builder", "runtime"])?;
		cli_succeed(&["test", "--runtime", "path/to/runtime"])?;
		cli_succeed(&[
			"test",
			"--runtime",
			"path/to/runtime",
			"--genesis-builder-preset",
			"preset",
		])?;

		// Test genesis-patch with runtime
		cli_succeed(&[
			"test",
			"--runtime",
			"path/to/runtime",
			"--genesis-patch",
			"path/to/patch.json",
		])?;
		cli_succeed(&[
			"test",
			"--runtime",
			"path/to/runtime",
			"--genesis-builder",
			"runtime",
			"--genesis-patch",
			"path/to/patch.json",
		])?;
		cli_succeed(&[
			"test",
			"--runtime",
			"path/to/runtime",
			"--genesis-builder-preset",
			"preset",
			"--genesis-patch",
			"path/to/patch.json",
		])?;

		cli_fail(&["test", "--runtime", "path/to/spec", "--genesis-builder", "spec"]);
		cli_fail(&["test", "--runtime", "path/to/spec", "--genesis-builder", "spec-genesis"]);
		cli_fail(&["test", "--runtime", "path/to/spec", "--genesis-builder", "spec-runtime"]);

		// Spec tests
		cli_succeed(&["test", "--chain", "path/to/spec"])?;
		cli_succeed(&["test", "--chain", "path/to/spec", "--genesis-builder", "spec"])?;
		cli_succeed(&["test", "--chain", "path/to/spec", "--genesis-builder", "spec-genesis"])?;
		cli_succeed(&["test", "--chain", "path/to/spec", "--genesis-builder", "spec-runtime"])?;

		// Test genesis-patch with chain spec
		cli_succeed(&["test", "--chain", "path/to/spec", "--genesis-patch", "path/to/patch.json"])?;
		cli_succeed(&[
			"test",
			"--chain",
			"path/to/spec",
			"--genesis-builder",
			"spec",
			"--genesis-patch",
			"path/to/patch.json",
		])?;
		cli_succeed(&[
			"test",
			"--chain",
			"path/to/spec",
			"--genesis-builder",
			"spec-genesis",
			"--genesis-patch",
			"path/to/patch.json",
		])?;
		cli_succeed(&[
			"test",
			"--chain",
			"path/to/spec",
			"--genesis-builder",
			"spec-runtime",
			"--genesis-patch",
			"path/to/patch.json",
		])?;

		cli_fail(&["test", "--chain", "path/to/spec", "--genesis-builder", "none"]);
		cli_fail(&["test", "--chain", "path/to/spec", "--genesis-builder", "runtime"]);
		cli_fail(&[
			"test",
			"--chain",
			"path/to/spec",
			"--genesis-builder",
			"runtime",
			"--genesis-builder-preset",
			"preset",
		]);
		Ok(())
	}
}
