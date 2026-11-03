// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

use clap::ValueEnum;
use cumulus_client_cli::{ExportGenesisHeadCommand, ExportGenesisWasmCommand};
use polkadot_service::{ChainSpec, ParaId, PrometheusConfig};
use sc_cli::{
	CliConfiguration, DefaultConfigurationValues, ImportParams, KeystoreParams, NetworkParams,
	Result as CliResult, RpcEndpoint, SharedParams, SubstrateCli,
};
use sc_service::BasePath;
use std::{
	fmt::{Display, Formatter},
	path::PathBuf,
};

#[derive(Debug, clap::Parser)]
#[command(
	version,
	propagate_version = true,
	args_conflicts_with_subcommands = true,
	subcommand_negates_reqs = true
)]
pub struct TestCollatorCli {
	#[command(subcommand)]
	pub subcommand: Option<Subcommand>,

	#[command(flatten)]
	pub run: cumulus_client_cli::RunCmd,

	/// Relay chain arguments
	#[arg(raw = true)]
	pub relaychain_args: Vec<String>,

	#[arg(long)]
	pub disable_block_announcements: bool,

	#[arg(long)]
	pub fail_pov_recovery: bool,

	/// Authoring style to use.
	#[arg(long, default_value_t = AuthoringPolicy::Lookahead)]
	pub authoring: AuthoringPolicy,

	/// Upper bound on collator reserved-peer mesh slots. `0` disables the mesh.
	#[arg(long, value_name = "N", default_value_t = 32)]
	pub collator_reserved_slots: usize,
}

/// Collator implementation to use.
#[derive(PartialEq, Debug, ValueEnum, Clone, Copy)]
pub enum AuthoringPolicy {
	/// Use the lookahead collator. Builds blocks once per relay chain block,
	/// builds on relay chain forks.
	Lookahead,
	/// Use the slot-based collator which can handle elastic-scaling. Builds blocks based on time
	/// and can utilize multiple cores, always builds on the best relay chain block available.
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

#[derive(Debug, clap::Subcommand)]
pub enum Subcommand {
	/// Build a chain specification.
	/// DEPRECATED: `build-spec` command will be removed after 1/04/2026. Use `export-chain-spec`
	/// command instead.
	#[deprecated(
		note = "build-spec command will be removed after 1/04/2026. Use export-chain-spec command instead"
	)]
	BuildSpec(sc_cli::BuildSpecCmd),

	/// Export the chain specification.
	ExportChainSpec(sc_cli::ExportChainSpecCmd),

	/// Export the genesis state of the parachain.
	#[command(alias = "export-genesis-state")]
	ExportGenesisHead(ExportGenesisHeadCommand),

	/// Export the genesis wasm of the parachain.
	ExportGenesisWasm(ExportGenesisWasmCommand),
}

#[derive(Debug)]
pub struct RelayChainCli {
	/// The actual relay chain cli object.
	pub base: polkadot_cli::RunCmd,

	/// Optional chain id that should be passed to the relay chain.
	pub chain_id: Option<String>,

	/// The base path that should be used by the relay chain.
	pub base_path: Option<PathBuf>,
}

impl RelayChainCli {
	/// Parse the relay chain CLI parameters using the para chain `Configuration`.
	pub fn new<'a>(
		para_config: &sc_service::Configuration,
		relay_chain_args: impl Iterator<Item = &'a String>,
	) -> Self {
		let base_path = para_config.base_path.path().join("polkadot");
		Self {
			base_path: Some(base_path),
			chain_id: None,
			base: clap::Parser::parse_from(relay_chain_args),
		}
	}
}

impl CliConfiguration<Self> for RelayChainCli {
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

	fn base_path(&self) -> CliResult<Option<BasePath>> {
		Ok(self
			.shared_params()
			.base_path()?
			.or_else(|| self.base_path.clone().map(Into::into)))
	}

	fn rpc_addr(&self, default_listen_port: u16) -> CliResult<Option<Vec<RpcEndpoint>>> {
		self.base.base.rpc_addr(default_listen_port)
	}

	fn prometheus_config(
		&self,
		default_listen_port: u16,
		chain_spec: &Box<dyn ChainSpec>,
	) -> CliResult<Option<PrometheusConfig>> {
		self.base.base.prometheus_config(default_listen_port, chain_spec)
	}

	fn init<F>(
		&self,
		_support_url: &String,
		_impl_version: &String,
		_logger_hook: F,
	) -> CliResult<()>
	where
		F: FnOnce(&mut sc_cli::LoggerBuilder),
	{
		unreachable!("PolkadotCli is never initialized; qed");
	}

	fn chain_id(&self, is_dev: bool) -> CliResult<String> {
		let chain_id = self.base.base.chain_id(is_dev)?;

		Ok(if chain_id.is_empty() { self.chain_id.clone().unwrap_or_default() } else { chain_id })
	}

	fn role(&self, is_dev: bool) -> CliResult<sc_service::Role> {
		self.base.base.role(is_dev)
	}

	fn transaction_pool(
		&self,
		is_dev: bool,
	) -> CliResult<sc_service::config::TransactionPoolOptions> {
		self.base.base.transaction_pool(is_dev)
	}

	fn trie_cache_maximum_size(&self) -> CliResult<Option<usize>> {
		self.base.base.trie_cache_maximum_size()
	}

	fn rpc_methods(&self) -> CliResult<sc_service::config::RpcMethods> {
		self.base.base.rpc_methods()
	}

	fn rpc_max_connections(&self) -> CliResult<u32> {
		self.base.base.rpc_max_connections()
	}

	fn rpc_cors(&self, is_dev: bool) -> CliResult<Option<Vec<String>>> {
		self.base.base.rpc_cors(is_dev)
	}

	fn default_heap_pages(&self) -> CliResult<Option<u64>> {
		self.base.base.default_heap_pages()
	}

	fn force_authoring(&self) -> CliResult<bool> {
		self.base.base.force_authoring()
	}

	fn disable_grandpa(&self) -> CliResult<bool> {
		self.base.base.disable_grandpa()
	}

	fn max_runtime_instances(&self) -> CliResult<Option<usize>> {
		self.base.base.max_runtime_instances()
	}

	fn announce_block(&self) -> CliResult<bool> {
		self.base.base.announce_block()
	}

	fn telemetry_endpoints(
		&self,
		chain_spec: &Box<dyn ChainSpec>,
	) -> CliResult<Option<sc_telemetry::TelemetryEndpoints>> {
		self.base.base.telemetry_endpoints(chain_spec)
	}

	fn node_name(&self) -> CliResult<String> {
		self.base.base.node_name()
	}
}

impl DefaultConfigurationValues for RelayChainCli {
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

impl SubstrateCli for TestCollatorCli {
	fn impl_name() -> String {
		"Cumulus zombienet test parachain".into()
	}

	fn impl_version() -> String {
		String::new()
	}

	fn description() -> String {
		format!(
			"Cumulus zombienet test parachain\n\nThe command-line arguments provided first will be \
		passed to the parachain node, while the arguments provided after -- will be passed \
		to the relaychain node.\n\n\
		{} [parachain-args] -- [relaychain-args]",
			Self::executable_name()
		)
	}

	fn author() -> String {
		env!("CARGO_PKG_AUTHORS").into()
	}

	fn support_url() -> String {
		"https://github.com/paritytech/polkadot-sdk/issues/new".into()
	}

	fn copyright_start_year() -> i32 {
		2017
	}

	fn load_spec(&self, id: &str) -> std::result::Result<Box<dyn sc_service::ChainSpec>, String> {
		Ok(Box::new(match id {
			"" => {
				tracing::info!("Using default test service chain spec.");
				cumulus_test_service::get_chain_spec(
					cumulus_test_runtime::WASM_BINARY,
					Some(ParaId::from(2000)),
				)
			},
			"elastic-scaling-mvp" => {
				tracing::info!("Using elastic-scaling mvp chain spec.");
				cumulus_test_service::get_chain_spec(
					cumulus_test_runtime::elastic_scaling::WASM_BINARY,
					Some(ParaId::from(2100)),
				)
			},
			"elastic-scaling" => {
				tracing::info!("Using elastic-scaling chain spec.");
				cumulus_test_service::get_chain_spec(
					cumulus_test_runtime::elastic_scaling::WASM_BINARY,
					Some(ParaId::from(2200)),
				)
			},
			"elastic-scaling-500ms" => {
				tracing::info!("Using elastic-scaling 500ms chain spec.");
				cumulus_test_service::get_chain_spec(
					cumulus_test_runtime::elastic_scaling_500ms::WASM_BINARY,
					Some(ParaId::from(2300)),
				)
			},
			"block-bundling" => {
				tracing::info!("Using block-bundling chain spec.");
				cumulus_test_service::get_chain_spec(
					cumulus_test_runtime::block_bundling::WASM_BINARY,
					Some(ParaId::from(2400)),
				)
			},
			"sync-backing" => {
				tracing::info!("Using sync backing chain spec.");
				cumulus_test_service::get_chain_spec(
					cumulus_test_runtime::sync_backing::WASM_BINARY,
					Some(ParaId::from(2500)),
				)
			},
			"async-backing" => {
				tracing::info!("Using async backing chain spec.");
				cumulus_test_service::get_chain_spec(
					cumulus_test_runtime::WASM_BINARY,
					Some(ParaId::from(2500)),
				)
			},
			"relay-parent-offset-2" => cumulus_test_service::get_chain_spec(
				cumulus_test_runtime::relay_parent_offset_2::WASM_BINARY,
				Some(ParaId::from(2600)),
			),
			"with-authority-discovery" => {
				tracing::info!("Using with-authority-discovery chain spec.");
				cumulus_test_service::get_chain_spec(
					cumulus_test_runtime::with_authority_discovery::WASM_BINARY,
					Some(ParaId::from(1000)),
				)
			},
			"v3" => {
				tracing::info!("Using async backing V3 chain spec.");
				cumulus_test_service::get_chain_spec(
					cumulus_test_runtime::v3::WASM_BINARY,
					Some(ParaId::from(2700)),
				)
			},
			"v3-rpo-2" => {
				tracing::info!("Using async backing V3 with relay parent offset 2 chain spec.");
				cumulus_test_service::get_chain_spec(
					cumulus_test_runtime::v3_rpo_2::WASM_BINARY,
					Some(ParaId::from(2700)),
				)
			},
			"v3-rpo-4" => {
				tracing::info!("Using async backing V3 with relay parent offset 4 chain spec.");
				cumulus_test_service::get_chain_spec(
					cumulus_test_runtime::v3_rpo_4::WASM_BINARY,
					Some(ParaId::from(2700)),
				)
			},
			"v3-rpo-6" => {
				tracing::info!("Using async backing V3 with relay parent offset 6 chain spec.");
				cumulus_test_service::get_chain_spec(
					cumulus_test_runtime::v3_rpo_6::WASM_BINARY,
					Some(ParaId::from(2700)),
				)
			},
			"v3-rpo-15" => {
				tracing::info!("Using async backing V3 with relay parent offset 15 chain spec.");
				cumulus_test_service::get_chain_spec(
					cumulus_test_runtime::v3_rpo_15::WASM_BINARY,
					Some(ParaId::from(2700)),
				)
			},
			"elastic-scaling-v3" => {
				tracing::info!("Using elastic scaling V3 chain spec.");
				cumulus_test_service::get_chain_spec(
					cumulus_test_runtime::elastic_scaling_v3::WASM_BINARY,
					Some(ParaId::from(2800)),
				)
			},
			path => {
				let chain_spec: sc_chain_spec::GenericChainSpec =
					sc_chain_spec::GenericChainSpec::from_json_file(path.into())?;
				chain_spec
			},
		}))
	}
}

impl SubstrateCli for RelayChainCli {
	fn impl_name() -> String {
		"Polkadot collator".into()
	}

	fn impl_version() -> String {
		String::new()
	}

	fn description() -> String {
		format!(
			"Polkadot collator\n\nThe command-line arguments provided first will be \
		passed to the parachain node, while the arguments provided after -- will be passed \
		to the relay chain node.\n\n\
		{} [parachain-args] -- [relay_chain-args]",
			Self::executable_name()
		)
	}

	fn author() -> String {
		env!("CARGO_PKG_AUTHORS").into()
	}

	fn support_url() -> String {
		"https://github.com/paritytech/polkadot-sdk/issues/new".into()
	}

	fn copyright_start_year() -> i32 {
		2017
	}

	fn load_spec(&self, id: &str) -> std::result::Result<Box<dyn sc_service::ChainSpec>, String> {
		<polkadot_cli::Cli as SubstrateCli>::from_iter([RelayChainCli::executable_name()].iter())
			.load_spec(id)
	}
}
