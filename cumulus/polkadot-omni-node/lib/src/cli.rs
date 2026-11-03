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

//! CLI options of the omni-node. See [`Command`].

/// Default block time for dev mode when using `--dev` flag.
const DEFAULT_DEV_BLOCK_TIME_MS: u64 = 3000;

use crate::{
	chain_spec::DiskChainSpecLoader,
	common::{
		chain_spec::{Extensions, LoadSpec},
		NodeExtraArgs,
	},
};
use chain_spec_builder::ChainSpecBuilder;
use clap::{Command, CommandFactory, FromArgMatches, ValueEnum};
use sc_chain_spec::ChainSpec;
use sc_cli::{
	CliConfiguration, DefaultConfigurationValues, ImportParams, KeystoreParams, NetworkParams,
	RpcEndpoint, SharedParams, SubstrateCli,
};
use sc_service::{config::PrometheusConfig, BasePath};
use sc_storage_monitor::StorageMonitorParams;
use std::{
	fmt::{Display, Formatter},
	marker::PhantomData,
	path::PathBuf,
};
/// Trait that can be used to customize some of the customer-facing info related to the node binary
/// that is being built using this library.
///
/// The related info is shown to the customer as part of logs or help messages.
/// It does not impact functionality.
pub trait CliConfig {
	/// The version of the resulting node binary.
	fn impl_version() -> String;

	/// The description of the resulting node binary.
	fn description(executable_name: String) -> String {
		format!(
			"The command-line arguments provided first will be passed to the parachain node, \n\
			and the arguments provided after -- will be passed to the relay chain node. \n\
			\n\
			Example: \n\
			\n\
			{} [parachain-args] -- [relay-chain-args]",
			executable_name
		)
	}

	/// The author of the resulting node binary.
	fn author() -> String;

	/// The support URL for the resulting node binary.
	fn support_url() -> String;

	/// The starting copyright year of the resulting node binary.
	fn copyright_start_year() -> u16;
}

/// Sub-commands supported by the collator.
#[derive(Debug, clap::Subcommand)]
pub enum Subcommand {
	/// Key management CLI utilities
	#[command(subcommand)]
	Key(sc_cli::KeySubcommand),

	/// Build a chain specification.
	///
	/// The `build-spec` command relies on the chain specification built (hard-coded) into the node
	/// binary, and may utilize the genesis presets of the runtimes  also embedded in the nodes
	/// that support  this command. Since `polkadot-omni-node` does not contain any embedded
	/// runtime, and requires a `chain-spec` path to be passed to its `--chain` flag, the command
	/// isn't bringing significant value as it does for other node binaries (e.g. the
	///  `polkadot` binary).
	///
	/// For a more versatile `chain-spec` manipulation experience please check out the
	/// `polkadot-omni-node chain-spec-builder` subcommand.
	#[deprecated(
		note = "build-spec will be removed after 1/06/2025. Use chain-spec-builder instead"
	)]
	BuildSpec(sc_cli::BuildSpecCmd),

	/// Validate blocks.
	CheckBlock(sc_cli::CheckBlockCmd),

	/// Export blocks.
	ExportBlocks(sc_cli::ExportBlocksCmd),

	/// Export the state of a given block into a chain spec.
	ExportState(sc_cli::ExportStateCmd),

	/// Import blocks.
	ImportBlocks(sc_cli::ImportBlocksCmd),

	/// Revert the chain to a previous state.
	Revert(sc_cli::RevertCmd),

	/// Subcommand for generating and managing chain specifications.
	///
	/// A `chain-spec-builder` subcommand corresponds to the existing `chain-spec-builder` tool
	/// (<https://crates.io/crates/staging-chain-spec-builder>), which can be used already standalone.
	/// It provides the same functionality as the tool but bundled with `polkadot-omni-node` to
	/// enable easier access to chain-spec generation, patching, converting to raw or validation,
	/// from a single binary, which can be used as a parachain node tool
	/// For a detailed usage guide please check out the standalone tool's crates.io or docs.rs
	/// pages:
	/// - <https://crates.io/crates/staging-chain-spec-builder>
	/// - <https://docs.rs/staging-chain-spec-builder/latest/staging_chain_spec_builder/>
	ChainSpecBuilder(ChainSpecBuilder),

	/// Remove the whole chain.
	PurgeChain(cumulus_client_cli::PurgeChainCmd),
	/// Export the genesis state of the parachain.
	#[command(alias = "export-genesis-state")]
	ExportGenesisHead(cumulus_client_cli::ExportGenesisHeadCommand),

	/// Export the genesis wasm of the parachain.
	ExportGenesisWasm(cumulus_client_cli::ExportGenesisWasmCommand),

	/// Sub-commands concerned with benchmarking.
	/// The pallet benchmarking moved to the `pallet` sub-command.
	#[command(subcommand)]
	Benchmark(frame_benchmarking_cli::BenchmarkCmd),
}

/// CLI Options shipped with `polkadot-omni-node`.
#[derive(clap::Parser)]
#[command(
	propagate_version = true,
	args_conflicts_with_subcommands = true,
	subcommand_negates_reqs = true
)]
pub struct Cli<Config: CliConfig> {
	#[arg(skip)]
	pub(crate) chain_spec_loader: Option<Box<dyn LoadSpec>>,

	/// Possible subcommands. See [`Subcommand`].
	#[command(subcommand)]
	pub subcommand: Option<Subcommand>,

	/// The shared parameters with all cumulus-based parachain nodes.
	#[command(flatten)]
	pub run: cumulus_client_cli::RunCmd,

	/// Parameters for storage monitoring.
	#[command(flatten)]
	pub storage_monitor: StorageMonitorParams,

	/// Start a dev node that produces a block each `dev_block_time` ms.
	///
	/// This is a dev option. It enables a manual sealing, meaning blocks are produced manually
	/// rather than being part of an actual network consensus process. Using the option won't
	/// result in starting or connecting to a parachain network. The resulting node will work on
	/// its own, running the wasm blob and artificially producing a block each `dev_block_time` ms,
	/// as if it was part of a parachain.
	///
	/// The `--dev` flag sets the `dev_block_time` to a default value of 3000ms unless explicitly
	/// provided.
	#[arg(long, conflicts_with = "instant_seal")]
	pub dev_block_time: Option<u64>,

	/// Start a dev node with instant seal.
	///
	/// This is a dev option that enables instant sealing, meaning blocks are produced
	/// immediately when transactions are received, rather than at fixed intervals.
	/// Using this option won't result in starting or connecting to a parachain network.
	/// The resulting node will work on its own, running the wasm blob and producing blocks
	/// instantly upon receiving transactions.
	#[arg(long, conflicts_with = "dev_block_time")]
	pub instant_seal: bool,

	/// DEPRECATED: This feature has been stabilized, pLease use `--authoring slot-based` instead.
	///
	/// Use slot-based collator which can handle elastic scaling.
	/// Use with care, this flag is unstable and subject to change.
	#[arg(long, conflicts_with = "authoring")]
	pub experimental_use_slot_based: bool,

	/// Authoring style to use.
	#[arg(long, default_value_t = AuthoringPolicy::Lookahead)]
	pub authoring: AuthoringPolicy,

	/// Disable automatic hardware benchmarks.
	///
	/// By default these benchmarks are automatically ran at startup and measure
	/// the CPU speed, the memory bandwidth and the disk speed.
	///
	/// The results are then printed out in the logs, and also sent as part of
	/// telemetry, if telemetry is enabled.
	#[arg(long)]
	pub no_hardware_benchmarks: bool,

	/// Export all `PoVs` build by this collator to the given folder.
	///
	/// This is useful for debugging issues that are occurring while validating these `PoVs` on the
	/// relay chain.
	#[arg(long)]
	pub export_pov_to_path: Option<PathBuf>,

	/// Relay chain arguments
	#[arg(raw = true)]
	pub relay_chain_args: Vec<String>,

	/// Enable the statement store.
	///
	/// The statement store reads the storage of the chain to determine if users are allowed to
	/// store statements or not.
	#[arg(long)]
	pub enable_statement_store: bool,

	/// Number of concurrent workers for statement validation from the network.
	///
	/// Only relevant when `--enable-statement-store` is used.
	#[arg(long, default_value_t = sc_statement_store::DEFAULT_NETWORK_WORKERS)]
	pub statement_network_workers: usize,

	/// Maximum statements per second per peer before rate limiting kicks in.
	///
	/// Uses a token bucket algorithm that allows short bursts up to this limit
	/// while enforcing the average rate over time.
	///
	/// Only relevant when `--enable-statement-store` is used.
	#[arg(long, default_value_t = sc_statement_store::DEFAULT_RATE_LIMIT)]
	pub statement_rate_limit: u32,

	/// Maximum number of statements the statement store can hold.
	///
	/// Once this limit is reached, lower-priority statements may be evicted.
	///
	/// Only relevant when `--enable-statement-store` is used.
	#[arg(long, default_value_t = sc_statement_store::DEFAULT_MAX_TOTAL_STATEMENTS)]
	pub statement_store_max_total_statements: usize,

	/// Maximum total data size (in bytes) the statement store can hold.
	///
	/// Once this limit is reached, lower-priority statements may be evicted.
	///
	/// Only relevant when `--enable-statement-store` is used.
	#[arg(long, default_value_t = sc_statement_store::DEFAULT_MAX_TOTAL_SIZE)]
	pub statement_store_max_total_size: usize,

	/// Number of seconds for which removed statements won't be allowed to be added back.
	///
	/// This prevents old statements from being re-propagated on the network.
	///
	/// Only relevant when `--enable-statement-store` is used.
	#[arg(long, default_value_t = sc_statement_store::DEFAULT_PURGE_AFTER_SEC)]
	pub statement_store_purge_after_sec: u64,

	/// Upper bound on collator reserved-peer slots.
	#[arg(long, value_name = "N", default_value_t = 32)]
	pub collator_reserved_slots: usize,

	/// HOP (Hand-Off Protocol) configuration parameters.
	#[command(flatten)]
	pub hop: sc_hop::HopParams,

	#[arg(skip)]
	pub(crate) _phantom: PhantomData<Config>,
}

/// Development sealing mode.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DevSealMode {
	/// Produces blocks immediately upon receiving transactions.
	InstantSeal,
	/// Produces blocks at fixed time intervals.
	/// The u64 parameter represents the block time in milliseconds.
	ManualSeal(u64),
}

/// Collator implementation to use.
#[derive(PartialEq, Debug, ValueEnum, Clone, Copy)]
pub enum AuthoringPolicy {
	/// Use the lookahead collator. Builds a block once per imported relay chain block and
	/// on relay chain forks. Default for asynchronous backing chains.
	Lookahead,
	/// Use the slot-based collator. Builds a block based on time. Can utilize multiple cores,
	/// always builds on the best relay chain block available. Should be used with elastic-scaling
	/// chains.
	SlotBased,
}

impl Display for AuthoringPolicy {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			AuthoringPolicy::Lookahead => write!(f, "lookahead"),
			AuthoringPolicy::SlotBased => write!(f, "slot-based"),
		}
	}
}

impl<Config: CliConfig> Cli<Config> {
	pub(crate) fn node_extra_args(&self) -> NodeExtraArgs {
		NodeExtraArgs {
			authoring_policy: self
				.experimental_use_slot_based
				.then(|| AuthoringPolicy::SlotBased)
				.unwrap_or(self.authoring),
			export_pov: self.export_pov_to_path.clone(),
			max_pov_percentage: self.run.experimental_max_pov_percentage,
			statement_store_config: self.enable_statement_store.then_some(
				sc_statement_store::Config {
					max_total_statements: self.statement_store_max_total_statements,
					max_total_size: self.statement_store_max_total_size,
					purge_after_sec: self.statement_store_purge_after_sec,
					network_workers: self.statement_network_workers,
					rate_limit: self.statement_rate_limit,
				},
			),
			storage_monitor: self.storage_monitor.clone(),
			collator_reserved_slots: self.collator_reserved_slots,
			hop: self.hop.enabled.then(|| self.hop.clone()),
		}
	}

	/// Returns the dev seal mode if the node is in dev mode.
	pub(crate) fn dev_mode(&self) -> Option<DevSealMode> {
		if self.instant_seal {
			Some(DevSealMode::InstantSeal)
		} else if let Some(dev_block_time) = self.dev_block_time {
			Some(DevSealMode::ManualSeal(dev_block_time))
		} else if self.run.base.is_dev().unwrap_or(false) {
			Some(DevSealMode::ManualSeal(DEFAULT_DEV_BLOCK_TIME_MS))
		} else {
			None
		}
	}
}

impl<Config: CliConfig> SubstrateCli for Cli<Config> {
	fn impl_name() -> String {
		Self::executable_name()
	}

	fn impl_version() -> String {
		Config::impl_version()
	}

	fn description() -> String {
		Config::description(Self::executable_name())
	}

	fn author() -> String {
		Config::author()
	}

	fn support_url() -> String {
		Config::support_url()
	}

	fn copyright_start_year() -> i32 {
		Config::copyright_start_year() as i32
	}

	fn load_spec(&self, id: &str) -> Result<Box<dyn ChainSpec>, String> {
		match &self.chain_spec_loader {
			Some(chain_spec_loader) => chain_spec_loader.load_spec(id),
			None => DiskChainSpecLoader.load_spec(id),
		}
	}
}

/// The relay chain CLI flags. These are passed in after a `--` at the end.
#[derive(Debug)]
pub struct RelayChainCli<Config: CliConfig> {
	/// The actual relay chain cli object.
	pub base: polkadot_cli::RunCmd,

	/// Optional chain id that should be passed to the relay chain.
	pub chain_id: Option<String>,

	/// The base path that should be used by the relay chain.
	pub base_path: Option<PathBuf>,

	_phantom: PhantomData<Config>,
}

impl<Config: CliConfig> RelayChainCli<Config> {
	fn polkadot_cmd() -> Command {
		let help_template = color_print::cformat!(
			"The arguments that are passed to the relay chain node. \n\
			\n\
			<bold><underline>RELAY_CHAIN_ARGS:</></> \n\
			{{options}}",
		);

		polkadot_cli::RunCmd::command()
			.no_binary_name(true)
			.help_template(help_template)
	}

	/// Parse the relay chain CLI parameters using the parachain `Configuration`.
	pub fn new<'a>(
		para_config: &sc_service::Configuration,
		relay_chain_args: impl Iterator<Item = &'a String>,
	) -> Self {
		let polkadot_cmd = Self::polkadot_cmd();
		let matches = polkadot_cmd.get_matches_from(relay_chain_args);
		let base = FromArgMatches::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());

		let extension = Extensions::try_get(&*para_config.chain_spec);
		let chain_id = extension.map(|e| e.relay_chain());

		let base_path = para_config.base_path.path().join("polkadot");
		Self { base, chain_id, base_path: Some(base_path), _phantom: Default::default() }
	}
}

impl<Config: CliConfig> SubstrateCli for RelayChainCli<Config> {
	fn impl_name() -> String {
		Cli::<Config>::impl_name()
	}

	fn impl_version() -> String {
		Cli::<Config>::impl_version()
	}

	fn description() -> String {
		Cli::<Config>::description()
	}

	fn author() -> String {
		Cli::<Config>::author()
	}

	fn support_url() -> String {
		Cli::<Config>::support_url()
	}

	fn copyright_start_year() -> i32 {
		Cli::<Config>::copyright_start_year()
	}

	fn load_spec(&self, id: &str) -> std::result::Result<Box<dyn ChainSpec>, String> {
		polkadot_cli::Cli::from_iter([Self::executable_name()].iter()).load_spec(id)
	}
}

impl<Config: CliConfig> DefaultConfigurationValues for RelayChainCli<Config> {
	fn p2p_listen_port() -> u16 {
		30334
	}

	fn rpc_listen_port() -> u16 {
		9945
	}

	fn prometheus_listen_port() -> u16 {
		9616
	}
}

impl<Config: CliConfig> CliConfiguration<Self> for RelayChainCli<Config> {
	fn shared_params(&self) -> &SharedParams {
		self.base.base.shared_params()
	}

	fn import_params(&self) -> Option<&ImportParams> {
		self.base.base.import_params()
	}

	fn network_params(&self) -> Option<&NetworkParams> {
		self.base.base.network_params()
	}

	fn keystore_params(&self) -> Option<&KeystoreParams> {
		self.base.base.keystore_params()
	}

	fn base_path(&self) -> sc_cli::Result<Option<BasePath>> {
		Ok(self
			.shared_params()
			.base_path()?
			.or_else(|| self.base_path.clone().map(Into::into)))
	}

	fn rpc_addr(&self, default_listen_port: u16) -> sc_cli::Result<Option<Vec<RpcEndpoint>>> {
		self.base.base.rpc_addr(default_listen_port)
	}

	fn prometheus_config(
		&self,
		default_listen_port: u16,
		chain_spec: &Box<dyn ChainSpec>,
	) -> sc_cli::Result<Option<PrometheusConfig>> {
		self.base.base.prometheus_config(default_listen_port, chain_spec)
	}

	fn init<F>(
		&self,
		_support_url: &String,
		_impl_version: &String,
		_logger_hook: F,
	) -> sc_cli::Result<()>
	where
		F: FnOnce(&mut sc_cli::LoggerBuilder),
	{
		unreachable!("PolkadotCli is never initialized; qed");
	}

	fn chain_id(&self, is_dev: bool) -> sc_cli::Result<String> {
		let chain_id = self.base.base.chain_id(is_dev)?;

		Ok(if chain_id.is_empty() { self.chain_id.clone().unwrap_or_default() } else { chain_id })
	}

	fn role(&self, is_dev: bool) -> sc_cli::Result<sc_service::Role> {
		self.base.base.role(is_dev)
	}

	fn transaction_pool(
		&self,
		is_dev: bool,
	) -> sc_cli::Result<sc_service::config::TransactionPoolOptions> {
		self.base.base.transaction_pool(is_dev)
	}

	fn trie_cache_maximum_size(&self) -> sc_cli::Result<Option<usize>> {
		self.base.base.trie_cache_maximum_size()
	}

	fn rpc_methods(&self) -> sc_cli::Result<sc_service::config::RpcMethods> {
		self.base.base.rpc_methods()
	}

	fn rpc_max_connections(&self) -> sc_cli::Result<u32> {
		self.base.base.rpc_max_connections()
	}

	fn rpc_cors(&self, is_dev: bool) -> sc_cli::Result<Option<Vec<String>>> {
		self.base.base.rpc_cors(is_dev)
	}

	fn default_heap_pages(&self) -> sc_cli::Result<Option<u64>> {
		self.base.base.default_heap_pages()
	}

	fn force_authoring(&self) -> sc_cli::Result<bool> {
		self.base.base.force_authoring()
	}

	fn disable_grandpa(&self) -> sc_cli::Result<bool> {
		self.base.base.disable_grandpa()
	}

	fn max_runtime_instances(&self) -> sc_cli::Result<Option<usize>> {
		self.base.base.max_runtime_instances()
	}

	fn announce_block(&self) -> sc_cli::Result<bool> {
		self.base.base.announce_block()
	}

	fn telemetry_endpoints(
		&self,
		chain_spec: &Box<dyn ChainSpec>,
	) -> sc_cli::Result<Option<sc_telemetry::TelemetryEndpoints>> {
		self.base.base.telemetry_endpoints(chain_spec)
	}

	fn node_name(&self) -> sc_cli::Result<String> {
		self.base.base.node_name()
	}
}
