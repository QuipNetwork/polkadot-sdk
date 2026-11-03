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

//! Polkadot CLI library.

pub use polkadot_node_primitives::NODE_VERSION;

use clap::Parser;
use std::path::PathBuf;

#[allow(missing_docs)]
#[derive(Debug, Parser)]
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

	/// Validate blocks.
	CheckBlock(sc_cli::CheckBlockCmd),

	/// Export blocks.
	ExportBlocks(sc_cli::ExportBlocksCmd),

	/// Export the state of a given block into a chain spec.
	ExportState(sc_cli::ExportStateCmd),

	/// Import blocks.
	ImportBlocks(sc_cli::ImportBlocksCmd),

	/// Remove the whole chain.
	PurgeChain(sc_cli::PurgeChainCmd),

	/// Revert the chain to a previous state.
	Revert(sc_cli::RevertCmd),

	/// Sub-commands concerned with benchmarking.
	/// The pallet benchmarking moved to the `pallet` sub-command.
	#[command(subcommand)]
	Benchmark(frame_benchmarking_cli::BenchmarkCmd),

	/// Key management CLI utilities
	#[command(subcommand)]
	Key(sc_cli::KeySubcommand),

	/// Db meta columns information.
	ChainInfo(sc_cli::ChainInfoCmd),
}

#[allow(missing_docs)]
#[derive(Debug, Parser)]
#[group(skip)]
pub struct RunCmd {
	#[clap(flatten)]
	pub base: sc_cli::RunCmd,

	/// Force using Kusama native runtime.
	#[arg(long = "force-kusama")]
	pub force_kusama: bool,

	/// Force using Westend native runtime.
	#[arg(long = "force-westend")]
	pub force_westend: bool,

	/// Force using Rococo native runtime.
	#[arg(long = "force-rococo")]
	pub force_rococo: bool,

	/// Disable the BEEFY gadget.
	///
	/// Currently enabled by default.
	#[arg(long)]
	pub no_beefy: bool,

	/// Allows a validator to run insecurely outside of Secure Validator Mode. Security features
	/// are still enabled on a best-effort basis, but missing features are no longer required. For
	/// more information see <https://github.com/w3f/polkadot-wiki/issues/4881>.
	#[arg(long = "insecure-validator-i-know-what-i-do", requires = "validator")]
	pub insecure_validator: bool,

	/// Enable the block authoring backoff that is triggered when finality is lagging.
	#[arg(long)]
	pub force_authoring_backoff: bool,

	/// Add the destination address to the `pyroscope` agent.
	///
	/// Must be valid socket address, of format `IP:Port` (commonly `127.0.0.1:4040`).
	#[arg(long)]
	pub pyroscope_server: Option<String>,

	/// Disable automatic hardware benchmarks.
	///
	/// By default these benchmarks are automatically ran at startup and measure
	/// the CPU speed, the memory bandwidth and the disk speed.
	///
	/// The results are then printed out in the logs, and also sent as part of
	/// telemetry, if telemetry is enabled.
	#[arg(long)]
	pub no_hardware_benchmarks: bool,

	/// Overseer message capacity override.
	///
	/// **Dangerous!** Do not touch unless explicitly advised to.
	#[arg(long)]
	pub overseer_channel_capacity_override: Option<usize>,

	/// Path to the directory where auxiliary worker binaries reside.
	///
	/// If not specified, the main binary's directory is searched first, then
	/// `/usr/lib/polkadot` is searched.
	///
	/// TESTING ONLY: if the path points to an executable rather then directory,
	/// that executable is used both as preparation and execution worker.
	#[arg(long, value_name = "PATH")]
	pub workers_path: Option<PathBuf>,

	/// Override the maximum number of pvf execute workers.
	///
	///  **Dangerous!** Do not touch unless explicitly advised to.
	#[arg(long)]
	pub execute_workers_max_num: Option<usize>,
	/// Override the maximum number of pvf workers that can be spawned in the pvf prepare
	/// pool for tasks with the priority below critical.
	///
	///  **Dangerous!** Do not touch unless explicitly advised to.

	#[arg(long)]
	pub prepare_workers_soft_max_num: Option<usize>,
	/// Override the absolute number of pvf workers that can be spawned in the pvf prepare pool.
	///
	///  **Dangerous!** Do not touch unless explicitly advised to.
	#[arg(long)]
	pub prepare_workers_hard_max_num: Option<usize>,
	/// TESTING ONLY: disable the version check between nodes and workers.
	#[arg(long, hide = true)]
	pub disable_worker_version_check: bool,

	/// How long finalized data should be kept in the availability store (in hours).
	/// Only used for testnets. If not specified, set to 1 hour. Always set to 25 hours for live
	/// networks.
	#[arg(long)]
	pub keep_finalized_for: Option<u32>,

	/// Overrides `HOLD_OFF_DURATION` in collator_protocol/validator_side. The value is in
	/// milliseconds.
	///
	///  **Dangerous!** Do not touch unless explicitly advised to.
	#[arg(long, hide = true)]
	pub collator_protocol_hold_off: Option<u64>,

	/// Enable experimental collator protocol. TESTING ONLY! Don't use on production
	/// Defaults to enabled on Kusama and disabled everywhere else. Pass `=false` to opt out.
	#[arg(
		long,
		hide = true,
		// to support `--experimental-collator-protocol=[true|false]` and
		// just `--experimental-collator-protocol`
		num_args = 0..=1,
		// otherwise the parameter becomes greedy and consumes the first value after it,
		// e.g. `--experimental-collator-protocol chain-info`
		require_equals = true,
		// means that `--experimental-collator-protocol` w/o explicit value equals to "on" (`true`)
		default_missing_value = "true"
	)]
	pub experimental_collator_protocol: Option<bool>,

	/// Collator reputation persistence interval in seconds.
	/// If not specified, defaults to 600 seconds (10 minutes).
	/// This should be used only with experimental_collator_protocol
	/// and only on validators.
	#[arg(long, requires = "experimental_collator_protocol", requires = "validator")]
	pub collator_reputation_persist_interval: Option<u64>,
}

#[allow(missing_docs)]
#[derive(Debug, Parser)]
pub struct Cli {
	#[command(subcommand)]
	pub subcommand: Option<Subcommand>,

	#[clap(flatten)]
	pub run: RunCmd,

	#[clap(flatten)]
	pub storage_monitor: sc_storage_monitor::StorageMonitorParams,
}
