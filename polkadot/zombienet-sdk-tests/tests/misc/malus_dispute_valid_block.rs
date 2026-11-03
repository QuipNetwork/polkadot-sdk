// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Misc malus
//!
//! Integration test for malus node.

use crate::utils::{
	assert_nodes_are_validators, check_metrics, env_or_default, initialize_network,
	MetricCheckSetup, BLOCK_HEIGHT_FINALIZED_METRIC, COL_IMAGE_ENV, INTEGRATION_IMAGE_ENV,
	MALUS_IMAGE_ENV,
};
use anyhow::anyhow;
use zombienet_sdk::{NetworkConfig, NetworkConfigBuilder, NetworkNode};

#[tokio::test(flavor = "multi_thread")]
async fn malus_dispute_valid_block_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let config = build_network_config()?;
	let network = initialize_network(config).await?;

	let validator_nodes = network.relaychain().nodes();

	// Check authority status
	log::info!("Checking validator node roles");
	assert_nodes_are_validators(&validator_nodes).await?;
	log::info!("All validators confirmed as authorities");

	let honest_validators: Vec<&NetworkNode> =
		validator_nodes.into_iter().filter(|n| n.name() != "malus").collect();
	let metric_checks: Vec<MetricCheckSetup> = vec![
		("sub_libp2p_is_major_syncing", Box::new(|v| v == 0.0), 0),
		(BLOCK_HEIGHT_FINALIZED_METRIC, Box::new(|v| v >= 2.0), 30),
		("sub_libp2p_peers_count", Box::new(|v| v >= 2.0), 0),
		(
			"polkadot_parachain_candidate_dispute_votes{validity=\"valid\"}",
			Box::new(|v| v >= 2.0),
			90,
		),
		(
			"polkadot_parachain_candidate_dispute_concluded{validity=\"valid\"}",
			Box::new(|v| v >= 1.0),
			90,
		),
		(
			"polkadot_parachain_candidate_dispute_concluded{validity=\"invalid\"}",
			Box::new(|v| v == 0.0),
			90,
		),
	];

	check_metrics(&honest_validators, &metric_checks).await?;

	log::info!("Test finished successfully");
	Ok(())
}

fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let polkadot_image = env_or_default(INTEGRATION_IMAGE_ENV, images.polkadot.as_str());
	let col_image = env_or_default(COL_IMAGE_ENV, images.cumulus.as_str());
	let malus_image = env_or_default(MALUS_IMAGE_ENV, images.cumulus.as_str());

	NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(polkadot_image.as_str())
				.with_default_args(vec!["-lparachain=debug".into()])
				.with_default_resources(|r| {
					r.with_limit_memory("4G")
						.with_limit_cpu("2")
						.with_request_memory("2G")
						.with_request_cpu("1")
				})
				.with_node_group(|g| g.with_count(3).with_base_node(|n| n.with_name("validator")))
				// Add malus validator
				.with_validator(|node| {
					node.with_name("malus")
						.with_image(malus_image.as_str())
						.with_command("malus")
						.with_subcommand("dispute-ancestor")
						.with_args(vec![
							"-lparachain=debug,MALUS=trace".into(),
							"--insecure-validator-i-know-what-i-do".into(),
						])
				})
		})
		.with_parachain(|p| {
			p.with_id(100)
				.cumulus_based(false)
				.with_default_image(col_image.as_str())
				.with_default_command("adder-collator")
				.with_collator(|n| n.with_name("collator"))
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
