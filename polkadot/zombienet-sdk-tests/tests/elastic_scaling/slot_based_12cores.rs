// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Test that a parachain that uses a single slot-based collator with elastic scaling can use 12
// cores in order to achieve 500ms blocks.

use crate::utils::maybe_enable_experimental_collator_protocol;
use std::time::Duration;

use anyhow::anyhow;

use cumulus_zombienet_sdk_helpers::{
	assert_finality_lag, assert_para_throughput, assign_cores, wait_for_pvf_prepare,
};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfigBuilder,
};

#[tokio::test(flavor = "multi_thread")]
async fn slot_based_12cores_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let images = zombienet_sdk::environment::get_images_from_env();

	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(maybe_enable_experimental_collator_protocol(vec![
					("-lparachain=debug").into(),
				]))
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"num_cores": 11,
								"max_validators_per_core": 1
							},
							"async_backing_params": {
								"max_candidate_depth": 24,
								"allowed_ancestry_len": 2
							}
						}
					}
				}))
				// Have to set a `with_validator` outside of the loop below, so that `r` has the
				// right type.
				.with_validator(|node| node.with_name("validator-0"));

			(1..12).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-{i}")))
			})
		})
		.with_parachain(|p| {
			p.with_id(2300)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain("elastic-scaling-500ms")
				.with_default_args(vec![
					"--authoring=slot-based".into(),
					("-lparachain=debug,aura=debug,parachain::collator-protocol=trace").into(),
				])
				.with_collator(|n| n.with_name("collator-0"))
				.with_collator(|n| n.with_name("collator-1"))
				.with_collator(|n| n.with_name("collator-2"))
				.with_collator(|n| n.with_name("collator-3"))
				.with_collator(|n| n.with_name("collator-4"))
				.with_collator(|n| n.with_name("collator-5"))
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_node = network.get_node("validator-0")?;
	let para_node = network.get_node("collator-5")?;

	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;

	// Assign 11 extra cores to the parachain.
	assign_cores(&relay_client, 2300, (0..11).collect()).await?;

	// Wait for PVF preparation to complete.
	wait_for_pvf_prepare(&network, 1).await?;

	// Expect a backed candidate count of at least 170 in 15 relay chain blocks
	// (11.33 candidates per para per relay chain block).
	// Note that only blocks after the first session change and blocks that don't contain a session
	// change will be counted.
	// Since the calculated backed candidate count is theoretical and the CI tests are observed to
	// occasionally fail, let's apply 15% tolerance to the expected range: 170 - 15% = 144
	assert_para_throughput(&relay_client, 15, [(ParaId::from(2300), 144..181)], []).await?;

	// Expect that `collator-5` claims at least 3 slots during this run.
	let result = para_node
		.wait_log_line_count_with_timeout(
			"*Received PreConnectToBackingGroups message*",
			true,
			LogLineCountOptions::new(|n| n >= 3, Duration::from_secs(120), false),
		)
		.await?;
	assert!(result.success());

	// It should disconnect at least 3 times after it's slot passes.
	let result = para_node
		.wait_log_line_count_with_timeout(
			"*Received DisconnectFromBackingGroups message*",
			true,
			LogLineCountOptions::new(|n| n >= 3, Duration::from_secs(120), false),
		)
		.await?;
	assert!(result.success());

	// Assert the parachain finalized block height is also on par with the number of backed
	// candidates.
	assert_finality_lag(&para_node.wait_client().await?, 60).await?;

	log::info!("Test finished successfully");

	Ok(())
}
