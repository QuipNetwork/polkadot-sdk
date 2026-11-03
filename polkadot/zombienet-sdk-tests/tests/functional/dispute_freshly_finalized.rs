// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Dispute Freshly Finalized Test
//!
//! This test verifies that disputes triggered on finalized blocks within scope
//! always end as valid. It uses a malus node to dispute recently finalized
//! candidates and verifies that disputes are properly concluded.

use crate::utils::{
	assert_nodes_are_validators, check_metrics, env_or_default, initialize_network,
	maybe_enable_experimental_collator_protocol, MetricCheckSetup,
	APPROVAL_CHECKING_FINALITY_LAG_METRIC, COL_IMAGE_ENV, INTEGRATION_IMAGE_ENV, MALUS_IMAGE_ENV,
};

use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::{
	assert_finality_lag, assert_para_is_registered, assert_para_throughput,
};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use tokio::time::Duration;
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{NetworkConfig, NetworkConfigBuilder, NetworkNode};

const PARA_ID: u32 = 2000;

/// Test that disputes triggered on finalized blocks within scope always end as valid.
///
/// - Spawns 6 honest validators and 1 malus validator
/// - Malus disputes candidates with offset 3 (within finalization scope)
/// - Verifies that disputes are initiated and concluded as valid
/// - Checks finality lag metrics remain low
#[tokio::test(flavor = "multi_thread")]
async fn dispute_freshly_finalized_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let config = build_network_config()?;
	let network = initialize_network(config).await?;

	let validator_nodes = network.relaychain().nodes();
	let malus = network.get_node("malus")?;
	let honest = network.get_node("honest-0")?;
	let relay_client = honest.wait_client().await?;

	// Check authority status
	log::info!("Checking validator node roles");
	assert_nodes_are_validators(&validator_nodes).await?;
	log::info!("All validators confirmed as authorities");

	// Ensure parachain is registered
	log::info!("Checking parachain {} is registered", PARA_ID);
	assert_para_is_registered(&relay_client, ParaId::from(PARA_ID), 10).await?;
	log::info!("Parachain {} is registered", PARA_ID);

	// Ensure parachain made progress
	log::info!("Waiting for parachain {} to produce blocks", PARA_ID);
	assert_para_throughput(&relay_client, 5, [(ParaId::from(PARA_ID), 2..6)], []).await?;
	log::info!("Parachain {} is producing blocks", PARA_ID);

	// Ensure that malus is already attempting to dispute
	log::info!("Checking malus is disputing candidates");
	let result = malus
		.wait_log_line_count_with_timeout(
			"*😈 Disputing candidate with hash:*",
			true,
			LogLineCountOptions::new(|n| n >= 1, Duration::from_secs(180), false),
		)
		.await?;
	assert!(result.success(), "Malus not disputing candidates");
	log::info!("Malus is disputing candidates");

	let honest_validators: Vec<&NetworkNode> =
		validator_nodes.into_iter().filter(|n| n.name() != "malus").collect();

	log::info!("Check if disputes are initiated and concluded (valid/invalid).");
	let metric_checks: Vec<MetricCheckSetup> = vec![
		("polkadot_parachain_candidate_disputes_total", Box::new(|v| v >= 2.0), 100),
		(
			"polkadot_parachain_candidate_dispute_concluded{validity=\"valid\"}",
			Box::new(|v| v >= 2.0),
			100,
		),
		(
			"polkadot_parachain_candidate_dispute_concluded{validity=\"invalid\"}",
			Box::new(|v| v == 0.0),
			10,
		),
	];

	check_metrics(&honest_validators, &metric_checks).await?;
	log::info!("Check disputes concluded ok.");

	log::info!("Checking approval / dispute finality lag");
	let metric_checks: Vec<MetricCheckSetup> = vec![
		(APPROVAL_CHECKING_FINALITY_LAG_METRIC, Box::new(|v| v < 2.0), 30),
		("polkadot_parachain_disputes_finality_lag", Box::new(|v| v < 2.0), 30),
	];

	check_metrics(&honest_validators, &metric_checks).await?;
	log::info!("Check approval / dispute finality lag concluded ok");

	for honest_validator in honest_validators {
		assert_finality_lag(&honest_validator.wait_client().await?, 3).await?;
	}

	log::info!("Test finished successfully");
	Ok(())
}

fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let polkadot_image = env_or_default(INTEGRATION_IMAGE_ENV, images.polkadot.as_str());
	let col_image = env_or_default(COL_IMAGE_ENV, images.cumulus.as_str());
	let malus_image = env_or_default(MALUS_IMAGE_ENV, images.cumulus.as_str());

	let mut builder = NetworkConfigBuilder::new().with_relaychain(|r| {
		r.with_chain("rococo-local")
			.with_default_command("polkadot")
			.with_default_image(polkadot_image.as_str())
			.with_default_args(maybe_enable_experimental_collator_protocol(vec![
				("-lparachain=debug").into(),
			]))
			.with_genesis_overrides(json!({
				"patch": {
					"configuration": {
						"config": {
							"needed_approvals": 1,
							"scheduler_params": {
								"max_validators_per_core": 1
							}
						}
					}
				}
			}))
			.with_default_resources(|r| {
				r.with_limit_memory("4G")
					.with_limit_cpu("2")
					.with_request_memory("2G")
					.with_request_cpu("1")
			})
			// Add malus validator
			.with_validator(|node| {
				node.with_name("malus")
					.with_image(malus_image.as_str())
					.with_command("malus")
					.with_subcommand("dispute-finalized-candidates")
					.with_args(vec![
						"-lparachain=debug,MALUS=trace".into(),
						"--dispute-offset=3".into(),
						"--insecure-validator-i-know-what-i-do".into(),
					])
					.invulnerable(false)
			})
			.with_node_group(|g| {
				g.with_count(6).with_base_node(|node| {
					node.with_name("honest")
						.with_command("polkadot")
						.with_args(vec!["-lparachain=debug".into()])
				})
			})
	});

	builder = builder.with_parachain(|p| {
		p.with_id(PARA_ID)
			.cumulus_based(false)
			.with_default_image(col_image.as_str())
			.with_default_command("undying-collator")
			.with_default_args(vec!["-lparachain=debug".into()])
			.with_collator(|n| n.with_name("collator"))
	});

	builder = builder.with_global_settings(|global_settings| {
		match std::env::var("ZOMBIENET_SDK_BASE_DIR") {
			Ok(val) => global_settings.with_base_dir(val),
			_ => global_settings,
		}
	});

	builder.build().map_err(|e| {
		let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
		anyhow!("config errs: {errs}")
	})
}
