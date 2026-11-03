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

//! Remote tests for pallet-psm against live Asset Hub state.

use clap::{Parser, ValueEnum};
use pallet_psm_remote_tests::PsmTestConfigOf;

#[derive(Clone, Debug, ValueEnum)]
#[value(rename_all = "PascalCase")]
enum Runtime {
	AssetHubWestend,
}

#[derive(Parser)]
struct Cli {
	#[arg(long, short, default_value = "wss://westend-asset-hub-rpc.polkadot.io:443")]
	uri: String,
	#[arg(long, short, ignore_case = true, value_enum, default_value_t = Runtime::AssetHubWestend)]
	runtime: Runtime,
	/// External stablecoin asset ID to use for testing (e.g., USDT = 1984).
	#[arg(long, default_value_t = 1984)]
	asset_id: u32,
}

fn asset_hub_westend_config(asset_id: u32) -> PsmTestConfigOf<asset_hub_westend_runtime::Runtime> {
	use frame_support::PalletId;
	use sp_runtime::{traits::AccountIdConversion, Permill};
	use xcm::latest::prelude::*;
	PsmTestConfigOf::<asset_hub_westend_runtime::Runtime> {
		internal_asset_id: Location::new(0, [PalletInstance(50), GeneralIndex(50_000_342)]),
		external_asset_id: Location::new(0, [PalletInstance(50), GeneralIndex(asset_id.into())]),
		internal_asset_decimals: 6,
		fee_destination: PalletId(*b"psm/test").into_account_truncating(),
		max_debt: 5_000_000 * 1_000_000, // 5M units (6 decimals)
		minting_fee: Permill::zero(),
		redemption_fee: Permill::from_rational(1u32, 10_000u32),
		ceiling_weight: Permill::from_percent(100),
		assets_pallet_name: "Assets".to_string(),
		pre_create_hook: None,
	}
}

#[tokio::main]
async fn main() {
	let options = Cli::parse();
	sp_tracing::try_init_simple();

	log::info!(
		target: "remote-ext-tests",
		"using runtime {:?} / asset_id: {}",
		options.runtime,
		options.asset_id,
	);

	match options.runtime {
		Runtime::AssetHubWestend => {
			use asset_hub_westend_runtime::{Block, Runtime};
			sp_core::defer!(pallet_psm_remote_tests::clear_ext());

			let config = asset_hub_westend_config(options.asset_id);

			// Fetch state once. The first call downloads from RPC and saves a
			// snapshot; the second call loads from the snapshot instantly.
			let mut ext = pallet_psm_remote_tests::build_ext::<Block>(
				options.uri.clone(),
				config.assets_pallet_name.clone(),
			)
			.await;

			pallet_psm_remote_tests::mint_and_redeem::<Runtime, Block>(&mut ext, &config);

			// Build a fresh externalities for the circuit breaker test so it
			// starts from clean state (loads from the snapshot, no RPC needed).
			let mut ext = pallet_psm_remote_tests::build_ext::<Block>(
				options.uri,
				config.assets_pallet_name.clone(),
			)
			.await;

			pallet_psm_remote_tests::circuit_breaker::<Runtime, Block>(&mut ext, &config);
		},
	}
}
