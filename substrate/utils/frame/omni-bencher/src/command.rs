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

use clap::Parser;
use frame_benchmarking_cli::{BenchmarkCmd, OpaqueBlock};
use sc_cli::Result;
use sp_runtime::traits::BlakeTwo256;

/// # Polkadot Omni Benchmarking CLI
///
/// The Polkadot Omni benchmarker allows to benchmark the extrinsics of any Polkadot runtime. It is
/// meant to replace the current manual integration of the `benchmark pallet` into every parachain
/// node. This reduces duplicate code and makes maintenance for builders easier. The CLI is
/// currently only able to benchmark extrinsics. In the future it is planned to extend this to some
/// other areas.
///
/// General FRAME runtimes could also be used with this benchmarker, as long as they don't utilize
/// any host functions that are not part of the Polkadot host specification.
///
/// ## Installation
///
/// Directly via crates.io:
///
/// ```sh
/// cargo install frame-omni-bencher --profile=production
/// ```
///
/// from GitHub:
///
/// ```sh
/// cargo install --git https://github.com/paritytech/polkadot-sdk frame-omni-bencher --profile=production
/// ```
///
/// or locally from the sources:
///
/// ```sh
/// cargo install --path substrate/utils/frame/omni-bencher --profile=production
/// ```
///
/// Check the installed version and print the docs:
///
/// ```sh
/// frame-omni-bencher --help
/// ```
///
/// ## Usage
///
/// First we need to ensure that there is a runtime available. As example we will build the Westend
/// runtime:
///
/// ```sh
/// cargo build -p westend-runtime --profile production --features runtime-benchmarks
/// ```
///
/// Now as an example, we benchmark the `balances` pallet:
///
/// ```sh
/// frame-omni-bencher v1 benchmark pallet \
///     --runtime target/release/wbuild/westend-runtime/westend-runtime.compact.compressed.wasm \
///     --pallet "pallet_balances" --extrinsic ""
/// ```
///
/// For the exact arguments of the `pallet` command, please refer to the `pallet` sub-module.
///
/// ## Backwards Compatibility
///
/// The exposed pallet sub-command is identical as the node-integrated CLI. The only difference is
/// that it needs to be prefixed with a `v1` to ensure drop-in compatibility.
#[derive(Parser, Debug)]
#[clap(author, version, about, verbatim_doc_comment)]
pub struct Command {
	#[command(subcommand)]
	sub: SubCommand,
}

/// Root-level subcommands.
#[derive(Debug, clap::Subcommand)]
pub enum SubCommand {
	/// Compatibility syntax with the old benchmark runner.
	V1(V1Command),
	// NOTE: Here we can add new commands in a forward-compatible way. For example when
	// transforming the CLI from a monolithic design to a data driven pipeline, there could be
	// commands like `measure`, `analyze` and `render`.
}

/// A command that conforms to the legacy `benchmark` argument syntax.
#[derive(Parser, Debug)]
pub struct V1Command {
	#[command(subcommand)]
	sub: V1SubCommand,
}

/// The `v1 benchmark` subcommand.
#[derive(Debug, clap::Subcommand)]
pub enum V1SubCommand {
	Benchmark(V1BenchmarkCommand),
}

/// Subcommands for `v1 benchmark`.
#[derive(Parser, Debug)]
pub struct V1BenchmarkCommand {
	#[command(subcommand)]
	sub: BenchmarkCmd,
}

type HostFunctions = (
	sp_statement_store::runtime_api::HostFunctions,
	cumulus_primitives_proof_size_hostfunction::storage_proof_size::HostFunctions,
	sp_crypto_ec_utils::HostFunctionsRfc163,
);

impl Command {
	pub fn run(self) -> Result<()> {
		match self.sub {
			SubCommand::V1(V1Command { sub }) => sub.run(),
		}
	}
}
impl V1SubCommand {
	pub fn run(self) -> Result<()> {
		match self {
			V1SubCommand::Benchmark(V1BenchmarkCommand { sub }) => match sub {
				BenchmarkCmd::Pallet(pallet) => {
					pallet.run_with_spec::<BlakeTwo256, HostFunctions>(None)
				},
				BenchmarkCmd::Overhead(overhead_cmd) =>
					overhead_cmd.run_with_default_builder_and_spec::<OpaqueBlock, HostFunctions>(None),
				_ =>
					return Err(
						"Only the `v1 benchmark pallet` and `v1 benchmark overhead` command is currently supported".into()
					),
			},
		}
	}
}
