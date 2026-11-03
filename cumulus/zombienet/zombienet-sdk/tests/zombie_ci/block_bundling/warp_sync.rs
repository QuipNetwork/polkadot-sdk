// This file is part of Cumulus.

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

use crate::utils::{initialize_network, BEST_BLOCK_METRIC, FINALIZED_BLOCK_METRIC};
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::assign_cores;
use serde_json::json;
use std::time::Duration;
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	AddCollatorOptions, NetworkConfig, NetworkConfigBuilder,
};

const PARA_ID: u32 = 2400;

/// Warp-sync regression test for block bundling.
///
/// Verifies that a fresh full node can warp-sync a chain that already has bundled blocks
/// (with BundleInfo/CoreInfo digests).
///
/// When a fresh node joins, it warp-syncs the relay chain (jumping to a finalized target
/// with `StateAction::ApplyChanges`), then backfills the gap (blocks #1..#target) via
/// gap sync with `StateAction::Skip`.
///
/// `SlotBasedBlockImport::import_block` must respect both `StateAction::Skip` and
/// `ApplyChanges`, and not attempt to call `execute_block_and_collect_storage_proof`
/// for these blocks, since the parent state is unavailable.
///
/// If the guard is wrong, the full node fails to import blocks and never catches up.
#[tokio::test(flavor = "multi_thread")]
async fn warp_sync_with_bundled_blocks() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	log::info!("Spawning network without full node");
	let config = build_network_config().await?;
	let mut network = initialize_network(config).await?;

	let relay_node = network.get_node("validator-0")?;
	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;

	// Assign 2 extra cores (zombienet auto-assigns 1), for 3 total.
	assign_cores(&relay_client, PARA_ID, vec![0, 1]).await?;

	// Wait for steady-state bundled block production: collator finalizes parachain block #72.
	log::info!("Waiting for collator to finalize parachain block #72");
	network
		.get_node("collator-0")?
		.wait_metric_with_timeout(FINALIZED_BLOCK_METRIC, |b| b >= 72.0, 200u64)
		.await?;

	// Query collator's current best block to set a sync target.
	let target_block = network.get_node("collator-0")?.reports(BEST_BLOCK_METRIC).await? as u64;
	log::info!("Full node sync target: #{target_block}");

	// Add a fresh full node that will warp-sync to the already-running chain.
	log::info!("Adding fresh full node with warp sync");
	let col_opts = AddCollatorOptions {
		is_validator: false,
		args: vec![
			("--sync=warp").into(),
			("-lsync=debug,parachain=debug,sync::cumulus=debug,aura=trace").into(),
			("--relay-chain-rpc-urls", "{{ZOMBIE:validator-0:ws_uri}}").into(),
		],
		..Default::default()
	};
	network.add_collator("para-full-node", col_opts, PARA_ID).await?;

	let full_node = network.get_node("para-full-node")?;

	// Wait for the full node to sync and catch up.
	// If the bug is present, the node fails to import bundled blocks and never advances.
	log::info!("Waiting for full node best block to reach #{target_block}");
	full_node
		.wait_metric_with_timeout(BEST_BLOCK_METRIC, |b| b >= target_block as f64, 120u64)
		.await?;
	log::info!("Full node synced past #{target_block}");

	// Verify the full node actually used warp sync (not full sync).
	log::info!("Verifying warp sync was used");
	let option_1_line = LogLineCountOptions::new(|n| n == 1, Duration::from_secs(5), false);
	let result = full_node
		.wait_log_line_count_with_timeout(
			r"\[Parachain\] Warp sync is complete",
			false,
			option_1_line,
		)
		.await?;
	if !result.success() {
		return Err(anyhow!("Full node did not complete parachain warp sync"));
	}

	// Make sure the full node keeps progressing on live blocks after the initial sync.
	// Wait for it to advance 24 blocks beyond the collator's current best.
	let collator_best = network.get_node("collator-0")?.reports(BEST_BLOCK_METRIC).await? as u64;
	let live_target = (collator_best + 24) as f64;
	log::info!("Collator best: #{collator_best}, waiting for full node to reach #{live_target}");

	full_node
		.wait_metric_with_timeout(BEST_BLOCK_METRIC, |b| b >= live_target, 120u64)
		.await?;

	log::info!("Test finished successfully");
	Ok(())
}

async fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	log::info!("Using images: {images:?}");

	NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![("-lparachain=trace").into()])
				.with_default_resources(|resources| {
					resources.with_request_cpu(4).with_request_memory("4G")
				})
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"num_cores": 2,
								"max_validators_per_core": 1
							}
						}
					}
				}))
				.with_validator(|node| node.with_name("validator-0"));
			(1..9).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-{i}")))
			})
		})
		.with_parachain(|p| {
			p.with_id(PARA_ID)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain("block-bundling")
				.with_default_args(vec![
					("--authoring").into(),
					("slot-based").into(),
					("-lparachain=trace,aura=trace,sync::cumulus=trace,consensus::common::parent_search=debug,runtime::parachain-system=debug").into(),
				])
				.with_genesis_overrides(json!({
					"testPallet": {
						"enableBigValueMove": true
					}
				}))
				.with_collator(|n| n.with_name("collator-0"))
				.with_collator(|n| n.with_name("collator-1"))
		})
		.with_global_settings(|global_settings| match std::env::var("ZOMBIENET_SDK_BASE_DIR") {
			Ok(val) => global_settings.with_base_dir(val),
			_ => global_settings,
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})
}
