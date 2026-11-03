// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Disputes garbage candidate
//!
//! Test dispute finality lag when 1/3 of parachain validators always attempt to include an invalid
//! block.

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

const MALUS_VALIDATORS: [&str; 1] = ["malus"];
const HONEST_VALIDATORS: [&str; 3] = ["honest-0", "honest-1", "honest-2"];
const PARAS: [u32; 3] = [2000, 2001, 2002];
const DISPUTE_CONCLUSION_LOG_PATTERN: &str = "*reverted due to a bad parachain block*";
const MALUS_LOG_PATTERN: &str = "*Voted for a candidate that was concluded invalid*";

#[tokio::test(flavor = "multi_thread")]
async fn parachains_disputes_garbage_candidate_test() -> Result<(), anyhow::Error> {
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

	// Parachains should be making progress even if we have up to 1/4 malicious validators.
	// Check that all parachains produce at least 2 blocks within 1 session and 9 blocks (RC)
	log::info!("Checking parachain block production (all paras registered at genesis)");
	let para_throughput: [(ParaId, Range<u32>); 3] = PARAS.map(|id| (ParaId::from(id), 2..10));
	assert_para_throughput(&relay_client, 9, para_throughput, []).await?;
	log::info!("All parachains producing blocks");

	log::info!("Check there is an offence report after dispute conclusion.");
	for name in HONEST_VALIDATORS {
		let honest = network.get_node(name)?;
		let result = honest
			.wait_event_count_with_timeout(
				"Offences",
				"Offence",
				CountOptions::new(|n| n >= 1, Duration::from_secs(180), false),
			)
			.await?;

		assert!(
			result.success(),
			"Can't find a matching event (Offences Offence) in Validator {name}"
		);
	}
	log::info!("All honest validators pass the event match - (Offences Offence)");

	log::info!("Check for chain reversion after dispute conclusion.");
	for name in HONEST_VALIDATORS {
		let honest = network.get_node(name)?;
		let result = honest
			.wait_log_line_count_with_timeout(
				DISPUTE_CONCLUSION_LOG_PATTERN,
				true,
				LogLineCountOptions::new(|n| n >= 1, Duration::from_secs(180), false),
			)
			.await?;

		assert!(
			result.success(),
			"Can't find a matching line ({DISPUTE_CONCLUSION_LOG_PATTERN}) in Validator {name}"
		);
	}
	log::info!("All honest validators pass the log line match - {DISPUTE_CONCLUSION_LOG_PATTERN}");

	log::info!("Check if disputes are concluded in less than 2 blocks.");
	for name in HONEST_VALIDATORS {
		let honest = network.get_node(name)?;
		let result = honest
			.assert_with("polkadot_parachain_disputes_finality_lag", |v| v <= 2.0)
			.await
			.map_err(|e| {
				anyhow!("Validator {name} check metric 'polkadot_parachain_disputes_finality_lag' failed: {e}")
			})?;

		assert!(result, "Validator {name} with disputes finality lag > 2");
	}

	log::info!("All honest validators pass the check");

	log::info!(
		"Check that garbage parachain blocks included by malicious validators are being disputed."
	);
	for name in HONEST_VALIDATORS {
		let honest = network.get_node(name)?;
		honest
			.wait_metric_with_timeout(
				"polkadot_parachain_candidate_disputes_total",
				|v| v >= 2.0,
				45u64,
			)
			.await
			.map_err(|e| {
				anyhow!("Validator {name} check (polkadot_parachain_candidate_disputes_total) failed: {e}")
			})?;
	}

	log::info!("All honest validators pass the check");

	log::info!("Disputes should always end as \"invalid\".");
	let honest_0 = network.get_node(HONEST_VALIDATORS[0])?;
	honest_0
			.wait_metric_with_timeout("polkadot_parachain_candidate_dispute_concluded{validity=\"invalid\"}", |v| v >= 2.0, 15u64)
			.await
			.map_err(|e| anyhow!("Validator {} check (polkadot_parachain_candidate_dispute_concluded{{validity=\"invalid\"}}) failed: {e}", HONEST_VALIDATORS[0]))?;

	let honest_1 = network.get_node(HONEST_VALIDATORS[1])?;
	honest_1
			.wait_metric_with_timeout("polkadot_parachain_candidate_dispute_concluded{validity=\"valid\"}", |v| v == 0.0, 15u64)
			.await
			.map_err(|e| anyhow!("Validator {} check (polkadot_parachain_candidate_dispute_concluded{{validity=\"valid\"}}) failed: {e}", HONEST_VALIDATORS[1]))?;

	log::info!("Dispute conclusion checked");

	log::info!("Check participating in the losing side of a dispute logged.");
	let malus = network.get_node(MALUS_VALIDATORS[0])?;
	let result = malus
		.wait_log_line_count_with_timeout(
			MALUS_LOG_PATTERN,
			true,
			LogLineCountOptions::new(|n| n >= 1, Duration::from_secs(180), false),
		)
		.await?;

	assert!(
		result.success(),
		"Can't find a matching line ({MALUS_LOG_PATTERN}) in Validator {}",
		MALUS_VALIDATORS[0]
	);

	log::info!("Test finished successfully");
	Ok(())
}

fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let polkadot_image = env_or_default(INTEGRATION_IMAGE_ENV, images.polkadot.as_str());
	let col_image = env_or_default(COL_IMAGE_ENV, images.cumulus.as_str());
	let malus_image = env_or_default(MALUS_IMAGE_ENV, images.malus.as_str());

	let mut builder = NetworkConfigBuilder::new().with_relaychain(|r| {
		r.with_chain("rococo-local")
			.with_default_command("polkadot")
			.with_default_image(polkadot_image.as_str())
			.with_default_args(maybe_enable_experimental_collator_protocol(vec![
				"-lparachain=debug,runtime=debug".into(),
			]))
			.with_genesis_overrides(json!({
				"patch": {
					"configuration": {
						"config": {
							"needed_approvals": 2,
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
			.with_node_group(|g| g.with_count(3).with_base_node(|node| node.with_name("honest")))
			.with_validator(|node| {
				node.with_name(MALUS_VALIDATORS[0])
					.with_image(malus_image.as_str())
					.with_command("malus")
					.with_subcommand("suggest-garbage-candidate")
					.with_args(vec![
						"-lMALUS=trace,parachain=debug".into(),
						"--insecure-validator-i-know-what-i-do".into(),
					])
			})
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
