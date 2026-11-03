// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Parachains Disputes
//!
//! It sets up a network with 8 validators (2 malus) and 4 parachains,
//! configured with high needed_approvals (8) and max_approval_coalesce_count (5).

use crate::utils::{
	assert_nodes_are_validators, env_or_default, initialize_network,
	maybe_enable_experimental_collator_protocol, COL_IMAGE_ENV, INTEGRATION_IMAGE_ENV,
	MALUS_IMAGE_ENV,
};
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::assert_para_throughput;
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use std::{ops::Range, time::Duration};
use zombienet_orchestrator::network::node::{CountOptions, LogLineCountOptions};
use zombienet_sdk::{NetworkConfig, NetworkConfigBuilder};

const MALUS_VALIDATORS: [&str; 2] = ["alice", "bob"];
const HONEST_VALIDATORS: [&str; 6] = ["charlie", "dave", "ferdie", "eve", "one", "two"];
const PARAS: [u32; 4] = [2000, 2001, 2002, 2003];

#[tokio::test(flavor = "multi_thread")]
async fn parachains_disputes_test() -> Result<(), anyhow::Error> {
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

	// Get a relay client for parachain throughput checks
	let relay_node = network.get_node(HONEST_VALIDATORS[0])?;
	let relay_client = relay_node.wait_client().await?;

	// Check that all parachains produce at least 5 blocks within 1 session and 5 blocks (RC)
	log::info!("Checking parachain block production (all paras registered at genesis)");
	let para_throughput: [(ParaId, Range<u32>); 4] = PARAS.map(|id| (ParaId::from(id), 2..6));
	assert_para_throughput(&relay_client, 5, para_throughput, []).await?;
	log::info!("All parachains producing blocks");

	// Check if disputes are initiated and concluded.
	// TODO (from original setup): check if disputes are concluded faster than initiated.
	log::info!("Check if disputes are initiated and concluded");
	let eve = network.get_node("eve")?;
	eve.wait_metric_with_timeout(
		"polkadot_parachain_candidate_disputes_total",
		|v| v >= 10.0,
		15u64,
	)
	.await
	.map_err(|e| {
		anyhow!("polkadot_parachain_candidate_disputes_total < 10 in 15s. - Err: {}", e)
	})?;

	eve
        .wait_metric_with_timeout("polkadot_parachain_candidate_dispute_concluded{validity=\"valid\"}", |v| v >= 10.0, 15u64)
        .await
        .map_err(|e| anyhow!("polkadot_parachain_candidate_dispute_concluded{{validity=\"valid\"}} < 10 in 15s. - Err: {}", e))?;

	eve
        .wait_metric_with_timeout("polkadot_parachain_candidate_dispute_concluded{validity=\"invalid\"}", |v| v == 0.0, 15u64)
        .await
        .map_err(|e| anyhow!("polkadot_parachain_candidate_dispute_concluded{{validity=\"invalid\"}} > 0 in 15s. - Err: {}", e))?;

	// alice: system event contains "There is an offence reported" within 60 seconds
	log::info!("Checks that the system events contain at least one Offence in 10 blocks");
	let alice = network.get_node("alice")?;
	alice
		.wait_event_count_with_timeout(
			"Offences",
			"Offence",
			CountOptions::new(|n| n >= 1, Duration::from_secs(60), false),
		)
		.await?;

	log::info!("Check lag - approval");
	for validator in &validator_nodes {
		validator
			.wait_metric_with_timeout(
				"polkadot_parachain_approval_checking_finality_lag",
				|v| v == 0.0,
				120u64,
			)
			.await
			.map_err(|e| {
				anyhow!(
					"Validator {} polkadot_parachain_approval_checking_finality_lag: {}",
					validator.name(),
					e
				)
			})?;
	}
	log::info!("All validators passed the lag - approval check");

	log::info!("Check lag - dispute conclusion");
	for validator in &validator_nodes {
		validator
			.wait_metric_with_timeout(
				"polkadot_parachain_disputes_finality_lag",
				|v| v == 0.0,
				120u64,
			)
			.await
			.map_err(|e| {
				anyhow!(
					"Validator {} polkadot_parachain_disputes_finality_lag: {}",
					validator.name(),
					e
				)
			})?;
	}
	log::info!("All validators passed the lag - dispute conclusion check");

	log::info!("Check participating in the losing side of a dispute logged");
	let result = alice
		.wait_log_line_count_with_timeout(
			"*Voted against a candidate that was concluded valid.*",
			true,
			LogLineCountOptions::new(|n| n >= 1, Duration::from_secs(180), false),
		)
		.await?;

	assert!(result.success());
	log::info!("Log line found.");

	log::info!("Test finished successfully");
	Ok(())
}

fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let polkadot_image = env_or_default(INTEGRATION_IMAGE_ENV, images.polkadot.as_str());
	let col_image = env_or_default(COL_IMAGE_ENV, images.cumulus.as_str());
	let malus_image = env_or_default(MALUS_IMAGE_ENV, images.malus.as_str());

	let mut builder = NetworkConfigBuilder::new().with_relaychain(|r| {
		let r = r
			.with_chain("rococo-local")
			.with_default_command("polkadot")
			.with_default_image(polkadot_image.as_str())
			.with_default_args(maybe_enable_experimental_collator_protocol(vec![
				"-lparachain=debug".into(),
			]))
			.with_genesis_overrides(json!({
				"patch": {
					"configuration": {
						"config": {
							"needed_approvals": 8,
							"scheduler_params": {
								"max_validators_per_core": 5
							},
							"approval_voting_params": {
								"max_approval_coalesce_count": 5
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
			});

		// set the malus validators
		let r = r
			.with_validator(|node| {
				node.with_name(MALUS_VALIDATORS[0])
					.with_image(malus_image.as_str())
					.with_command("malus")
					.with_subcommand("dispute-ancestor")
					.with_args(vec![
						("--fake-validation", "approval-invalid").into(),
						"-lMALUS=trace,parachain=debug".into(),
						"--insecure-validator-i-know-what-i-do".into(),
					])
			})
			.with_validator(|node| {
				node.with_name(MALUS_VALIDATORS[1])
					.with_image(malus_image.as_str())
					.with_command("malus")
					.with_subcommand("dispute-ancestor")
					.with_args(vec![
						("--fake-validation", "approval-invalid").into(),
						"-lMALUS=trace,parachain=debug".into(),
						"--insecure-validator-i-know-what-i-do".into(),
					])
			});

		HONEST_VALIDATORS
			.into_iter()
			.fold(r, |acc, name| acc.with_validator(|node| node.with_name(name)))
	});

	builder = PARAS.into_iter().fold(builder, |acc, para_id| {
		acc.with_parachain(|p| {
			let pov_size = 10000*(para_id-1999);
			let pvf_complexity = para_id - 1999;

			p.with_id(para_id)
			.cumulus_based(false)
			.with_default_image(col_image.as_str())
			.with_default_command("undying-collator")
			.with_default_args(vec![
				"-lruntime=debug,parachain=trace".into(),
				format!("--pov-size={pov_size}").as_str().into(),
				"--pvf-complexity=1".into(),
			])
			.with_genesis_state_generator(
				format!("undying-collator export-genesis-state --pov-size={pov_size} --pvf-complexity={pvf_complexity}").as_str(),
			)
			.with_collator(|n| n.with_name(&format!("collator-{para_id}")))
		})
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
