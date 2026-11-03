// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Test that a parachain using a malus undying collator, sending the same collation to all assigned
// cores, does not break the relay chain and that blocks are included, backed by a normal collator.

use crate::utils::maybe_enable_experimental_collator_protocol;
use anyhow::anyhow;
use tokio::time::Duration;

use cumulus_zombienet_sdk_helpers::{assert_para_throughput, assign_cores};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfigBuilder,
};

const VALIDATOR_COUNT: u8 = 3;

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_collations_test() -> Result<(), anyhow::Error> {
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
								"num_cores": 2
							},
							"async_backing_params": {
								"max_candidate_depth": 6
							}
						}
					}
				}))
				// Have to set a `with_validator` outside of the loop below, so that `r` has the
				// right type.
				.with_validator(|node| node.with_name("validator-0"));

			(1..VALIDATOR_COUNT).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-{i}")))
			})
		})
		.with_parachain(|p| {
			p.with_id(2000)
				.with_default_command("undying-collator")
				.cumulus_based(false)
				.with_default_image(
					std::env::var("COL_IMAGE")
						.unwrap_or("docker.io/paritypr/colander:latest".to_string())
						.as_str(),
				)
				.with_collator(|n| {
					n.with_name("normal-collator").with_args(vec![("-lparachain=debug").into()])
				})
				.with_collator(|n| {
					n.with_name("malus-collator").with_args(vec![
						("-lparachain=debug").into(),
						("--malus-type=duplicate-collations").into(),
					])
				})
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_node = network.get_node("validator-0")?;

	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;

	// Assign two extra cores to parachain-2000.
	assign_cores(&relay_client, 2000, vec![0, 1]).await?;

	log::info!("2 more cores assigned to parachain-2000");

	assert_para_throughput(&relay_client, 15, [(ParaId::from(2000), 40..46)], []).await?;

	let log_line_options = LogLineCountOptions::new(
		|n| n == 1,
		// Since we have this check after the para throughput check, all validators
		// should have already detected the malicious collator, and all expected logs
		// should have already appeared, so there is no need to wait more than 1 second.
		Duration::from_secs(1),
		false,
	);
	// Verify that all validators detect the malicious collator by checking their logs. This check
	// must be performed after the para throughput check because the validator group needs to rotate
	// at least once. This ensures that all validators have had a chance to detect the malicious
	// behavior.
	for i in 0..VALIDATOR_COUNT {
		let validator_name = &format!("validator-{i}");
		let validator_node = network.get_node(validator_name)?;
		let result = validator_node
			.wait_log_line_count_with_timeout(
				"Invalid UMP signals: The core index in commitments doesn't match the one in descriptor",
				false,
				log_line_options.clone(),
			)
			.await?;

		assert!(result.success(), "Expected log not found for {validator_name}",);
	}

	log::info!("Test finished successfully");

	Ok(())
}
