// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Test past-session slashing when a malicious validator backs an invalid
//! candidate and a dispute concluding in a future session. We achieve that by
//! making some of the honest nodes go offline.

use anyhow::anyhow;
use codec::Decode;
use cumulus_zombienet_sdk_helpers::{
	assert_blocks_are_being_finalized, assert_para_throughput, wait_for_first_session_change,
};
use polkadot_primitives::{BlockNumber, CandidateHash, DisputeState, Id as ParaId, SessionIndex};
use serde_json::json;
use tokio::time::Duration;
use tokio_util::time::FutureExt;
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfigBuilder,
};

#[tokio::test(flavor = "multi_thread")]
async fn dispute_past_session_slashing() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let images = zombienet_sdk::environment::get_images_from_env();

	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("westend-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![
					"--no-hardware-benchmarks".into(),
					"-lparachain=debug,runtime=debug".into(),
				])
				.with_genesis_overrides(json!({
					"patch": {
						"configuration": {
							"config": {
								"scheduler_params": {
									"group_rotation_frequency": 3,
									"max_validators_per_core": 1,
								},
								"needed_approvals": 2
							}
						}
					}
				}))
				.with_validator(|node| node.with_name("honest-validator-0"))
				.with_validator(|node| node.with_name("honest-validator-1"))
				.with_validator(|node| node.with_name("honest-flaky-validator-0"))
				.with_validator(|node| {
					node.with_name("malicious-backer")
						.with_image(
							std::env::var("MALUS_IMAGE")
								.unwrap_or("docker.io/paritypr/malus".to_string())
								.as_str(),
						)
						.with_command("malus")
						.with_subcommand("suggest-garbage-candidate")
						.with_args(vec![
							"--no-hardware-benchmarks".into(),
							"--insecure-validator-i-know-what-i-do".into(),
							"-lMALUS=trace,parachain=debug".into(),
						])
				})
		})
		.with_parachain(|p| {
			p.with_id(1337)
				.with_default_command("polkadot-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_default_args(vec!["-lparachain=debug".into()])
				.with_collator(|n| n.with_name("collator-1337"))
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let malus = network.get_node("malicious-backer")?;
	malus.pause().await?;

	let honest = network.get_node("honest-validator-0")?;
	let relay_client: OnlineClient<PolkadotConfig> = honest.wait_client().await?;

	// Wait for some para blocks being produced
	assert_para_throughput(&relay_client, 20, [(ParaId::from(1337), 10..20)], []).await?;

	// Let's initiate a dispute
	malus.resume().await?;
	// Pause flaky nodes, so a dispute doesn't conclude
	let flaky_0 = network.get_node("honest-flaky-validator-0")?;
	flaky_0.pause().await?;

	// wait for a dispute to be initiated
	let mut best_blocks = relay_client.blocks().subscribe_best().await?;
	let mut dispute_session: u32 = u32::MAX;
	while let Some(block) = best_blocks.next().await {
		// NOTE: we can't use `at_latest` here, because it will utilize latest *finalized* block
		// and finality is stalled...
		let disputes = Vec::<(SessionIndex, CandidateHash, DisputeState<BlockNumber>)>::decode(
			&mut &relay_client
				.runtime_api()
				.at(block?.hash())
				.call_raw("ParachainHost_disputes", None)
				.await?[..],
		)?;
		if let Some((session, _, _)) = disputes.first() {
			dispute_session = *session;
			break;
		}
	}

	assert_ne!(dispute_session, u32::MAX, "dispute should be initiated");
	log::info!("Dispute initiated, now waiting for a new session");

	wait_for_first_session_change(&mut best_blocks).await?;

	// We don't need malus anymore
	malus.pause().await?;

	let concluded_dispute_metric =
		"polkadot_parachain_candidate_dispute_concluded{validity=\"invalid\"}";

	let timeout_secs: u64 = 120;
	// with one offline honest node, dispute should not conclude
	honest
		.wait_metric_with_timeout(concluded_dispute_metric, |d| d < 1.0, timeout_secs)
		.await?;

	// Now resume flaky validators
	log::info!("Resuming flaky nodes - dispute should conclude");
	flaky_0.resume().await?;

	honest
		.wait_metric_with_timeout(concluded_dispute_metric, |d| d > 0.0, timeout_secs)
		.await?;
	log::info!("A dispute has concluded");

	let result = honest
		.wait_log_line_count_with_timeout(
			"*Successfully reported pending slash*",
			true,
			LogLineCountOptions::new(|n| n == 1, Duration::from_secs(timeout_secs), false),
		)
		.await?;

	assert!(result.success());

	assert_blocks_are_being_finalized(&relay_client)
		.timeout(Duration::from_secs(400)) // enough for the aggression to kick in
		.await?
		.unwrap();

	log::info!("Test finished successfully");

	Ok(())
}
