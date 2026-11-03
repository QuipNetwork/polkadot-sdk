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

//! Remote tests for bags-list pallet.

use clap::{Parser, ValueEnum};

#[derive(Clone, Debug, ValueEnum)]
#[value(rename_all = "PascalCase")]
enum Command {
	CheckMigration,
	SanityCheck,
	Snapshot,
}

#[derive(Clone, Debug, ValueEnum)]
#[value(rename_all = "PascalCase")]
enum Runtime {
	WestendAssetHub,
}

#[derive(Parser)]
struct Cli {
	#[arg(long, short, default_value = "wss://westend-asset-hub-rpc.polkadot.io:443")]
	uri: String,
	#[arg(long, short, ignore_case = true, value_enum, default_value_t = Runtime::WestendAssetHub)]
	runtime: Runtime,
	#[arg(long, short, ignore_case = true, value_enum, default_value_t = Command::SanityCheck)]
	command: Command,
	#[arg(long, short)]
	snapshot_limit: Option<usize>,
}

#[tokio::main]
async fn main() {
	let options = Cli::parse();
	sp_tracing::try_init_simple();

	log::info!(
		target: "remote-ext-tests",
		"using runtime {:?} / command: {:?}",
		options.runtime,
		options.command
	);

	use pallet_bags_list_remote_tests::*;
	match options.runtime {
		Runtime::WestendAssetHub => sp_core::crypto::set_default_ss58_version(
			<asset_hub_westend_runtime::Runtime as frame_system::Config>::SS58Prefix::get()
				.try_into()
				.unwrap(),
		),
	};

	match (options.runtime, options.command) {
		(Runtime::WestendAssetHub, Command::CheckMigration) => {
			use asset_hub_westend_runtime::{Block, Runtime};
			use testnet_parachains_constants::westend::currency::UNITS;
			migration::execute::<Runtime, Block>(UNITS as u64, "WND", options.uri.clone()).await;
		},
		(Runtime::WestendAssetHub, Command::SanityCheck) => {
			use asset_hub_westend_runtime::{Block, Runtime};
			use testnet_parachains_constants::westend::currency::UNITS;
			try_state::execute::<Runtime, Block>(UNITS as u64, "WND", options.uri.clone()).await;
		},
		(Runtime::WestendAssetHub, Command::Snapshot) => {
			use asset_hub_westend_runtime::{Block, Runtime};
			use testnet_parachains_constants::westend::currency::UNITS;
			snapshot::execute::<Runtime, Block>(
				options.snapshot_limit,
				UNITS.try_into().unwrap(),
				options.uri.clone(),
			)
			.await;
		},
	}
}
