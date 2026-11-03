// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Test if parachains progress when group is getting spammed by statement distribution requests.

use crate::utils::maybe_enable_experimental_collator_protocol;
use anyhow::anyhow;
use tokio::time::Duration;

use cumulus_zombienet_sdk_helpers::assert_para_throughput;
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfigBuilder,
};

#[tokio::test(flavor = "multi_thread")]
async fn spam_statement_distribution_requests_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let images = zombienet_sdk::environment::get_images_from_env();

	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(maybe_enable_experimental_collator_protocol(vec![
					// parachain::statement-distribution=trace to find
					// "Peer already being served, dropping request"
					("-lparachain=debug,parachain::statement-distribution=trace").into(),
				]))
				.with_default_resources(|r| {
					r.with_limit_cpu("2")
						.with_limit_memory("4G")
						.with_request_cpu("1")
						.with_request_memory("2G")
				})
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"needed_approvals": 2,
							"scheduler_params": {
								"max_validators_per_core": 5
							}
						}
					}
				}))
				.with_validator(|node| node.with_name("honest-0"))
				.with_validator(|node| node.with_name("honest-1"))
				.with_validator(|node| node.with_name("honest-2"))
				.with_validator(|node| node.with_name("honest-3"))
				.with_validator(|node| {
					node.with_name("malus")
						.with_image(
							std::env::var("MALUS_IMAGE")
								.unwrap_or("docker.io/paritypr/malus".to_string())
								.as_str(),
						)
						.with_command("malus")
						.with_subcommand("spam-statement-requests")
						.with_args(vec![
							"--alice".into(),
							"--spam-factor=1000".into(),
							"--insecure-validator-i-know-what-i-do".into(),
							"-lMALUS=trace,parachain=debug".into(),
						])
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
				.with_default_args(vec![("-lparachain=debug").into()])
				.with_collator(|n| n.with_name("collator-2000"))
		})
		.with_parachain(|p| {
			p.with_id(2001)
				.with_default_command("undying-collator")
				.cumulus_based(false)
				.with_default_image(
					std::env::var("COL_IMAGE")
						.unwrap_or("docker.io/paritypr/colander:latest".to_string())
						.as_str(),
				)
				.with_default_args(vec![("-lparachain=debug").into()])
				.with_collator(|n| n.with_name("collator-2001"))
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let malus = network.get_node("malus")?;
	let honest = network.get_node("honest-0")?;
	let relay_client: OnlineClient<PolkadotConfig> = honest.wait_client().await?;
	let _malus_client: OnlineClient<PolkadotConfig> = malus.wait_client().await?;

	// Check authority status and peers.
	malus.assert("node_roles", 4.0).await?;
	honest.assert("node_roles", 4.0).await?;

	// Ensure parachains are registered.
	assert_para_throughput(
		&relay_client,
		2,
		[(ParaId::from(2000), 2..3), (ParaId::from(2001), 2..3)],
		[],
	)
	.await?;

	// Ensure that malus is already attempting to DoS
	let result = malus
		.wait_log_line_count_with_timeout(
			"*Duplicating AttestedCandidateV2 request*",
			true,
			LogLineCountOptions::new(|n| n == 1, Duration::from_secs(90), false),
		)
		.await?;
	assert!(result.success());

	// Ensure parachains made progress.
	assert_para_throughput(
		&relay_client,
		10,
		[(ParaId::from(2000), 9..11), (ParaId::from(2001), 9..11)],
		[],
	)
	.await?;

	// Ensure that honest nodes drop extra requests.
	let result = honest
		.wait_log_line_count_with_timeout(
			"*Peer already being served, dropping request*",
			true,
			LogLineCountOptions::new(|n| n == 1, Duration::from_secs(60), false),
		)
		.await?;
	assert!(result.success());

	// Check lag - approval
	honest
		.assert(
			"polkadot_parachain_approval_checking_finality_lag{chain=\"rococo_local_testnet\"}",
			0.0,
		)
		.await?;

	// Check lag - dispute conclusion
	honest
		.assert("polkadot_parachain_disputes_finality_lag{chain=\"rococo_local_testnet\"}", 0.0)
		.await?;

	log::info!("Test finished successfully");

	Ok(())
}
