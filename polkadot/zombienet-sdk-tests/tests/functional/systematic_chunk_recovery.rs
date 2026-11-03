// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Setup a network with 4 validators and 2 parachains
//! First ensure regular chunk recovery, then enable
//! chunk mapping feature and check systematic chunk recovery.

use crate::utils::{
	assert_nodes_are_validators, check_log_lines, check_metrics, enable_node_features,
	env_or_default, initialize_network, maybe_enable_experimental_collator_protocol,
	MetricCheckSetup, APPROVAL_CHECKING_FINALITY_LAG_METRIC, APPROVAL_NO_SHOWS_TOTAL_METRIC,
	AVAILABILITY_RECOVERY_RECOVERIES_FINISHED, BLOCK_HEIGHT_FINALIZED_METRIC, COL_IMAGE_ENV,
	DATA_RECOVERY_CHUNKS_PATTERN, DATA_RECOVERY_FROM_SYSTEMATIC_CHUNKS_COMPLETE_PATTERN,
	DATA_RECOVERY_FROM_SYSTEMATIC_CHUNKS_NOT_POSSIBLE_PATTERN, INTEGRATION_IMAGE_ENV,
};
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::assert_para_throughput;
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use std::{ops::Range, time::Duration};
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{NetworkConfig, NetworkConfigBuilder};

const PARAS: [u32; 2] = [2000, 2001];
pub const DATA_RECOVERY_CHUNKS_NOT_POSSIBLE_PATTERN: &str =
	"*Data recovery from chunks is not possible*";

#[tokio::test(flavor = "multi_thread")]
async fn systematic_chunk_recovery_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let config = build_network_config()?;
	let network = initialize_network(config).await?;
	let mut validator_nodes = network.relaychain().nodes();

	// Check authority status
	log::info!("Checking validator node roles");
	assert_nodes_are_validators(&validator_nodes).await?;
	log::info!("All validators confirmed as authorities");

	// Get a relay client for parachain throughput checks
	let alice = network.get_node("alice")?;
	let alice_client = alice.wait_client().await?;

	// Check that all parachains produce at least 5 blocks within 1 session and 5 blocks (RC)
	log::info!("Checking parachain block production (all paras registered at genesis)");
	let para_throughput: [(ParaId, Range<u32>); 2] = PARAS.map(|id| (ParaId::from(id), 2..6));
	assert_para_throughput(&alice_client, 5, para_throughput, []).await?;
	log::info!("All parachains producing blocks");

	// remove alice  and use the others validators for the rest of the checks.
	validator_nodes.retain(|n| n.name() != "alice");

	let metric_checks: Vec<MetricCheckSetup> = vec![
		(BLOCK_HEIGHT_FINALIZED_METRIC, Box::new(|v| v >= 30.0), 400),
		(APPROVAL_CHECKING_FINALITY_LAG_METRIC, Box::new(|v| v < 3.0), 0),
		(APPROVAL_NO_SHOWS_TOTAL_METRIC, Box::new(|v| v < 3.0), 100),
	];

	check_metrics(&validator_nodes, &metric_checks).await?;

	log::info!("Ensure we used regular chunk recovery and that there are no failed recoveries.");
	let log_lines_first_checks = vec![
		(DATA_RECOVERY_CHUNKS_PATTERN, LogLineCountOptions::at_least(10, Duration::from_secs(300))),
		(
			DATA_RECOVERY_FROM_SYSTEMATIC_CHUNKS_COMPLETE_PATTERN,
			LogLineCountOptions::no_occurences_within_timeout(Duration::from_secs(10)),
		),
		(
			DATA_RECOVERY_FROM_SYSTEMATIC_CHUNKS_NOT_POSSIBLE_PATTERN,
			LogLineCountOptions::no_occurences_within_timeout(Duration::from_secs(10)),
		),
		(
			DATA_RECOVERY_CHUNKS_NOT_POSSIBLE_PATTERN,
			LogLineCountOptions::no_occurences_within_timeout(Duration::from_secs(10)),
		),
	];
	check_log_lines(&validator_nodes, &log_lines_first_checks).await?;

	for validator in &validator_nodes {
		validator
			.wait_metric_with_timeout(
				AVAILABILITY_RECOVERY_RECOVERIES_FINISHED,
				|x| x == 0.0,
				10u64,
			)
			.await?;
	}
	log::info!("All validators pass metric check - {AVAILABILITY_RECOVERY_RECOVERIES_FINISHED}");

	log::info!("Enable the chunk mapping feature.");
	let res = enable_node_features(&alice_client, &[2]).await;
	assert!(res.is_ok(), "Extrinsic failed to finalize: {:?}", res.unwrap_err());
	log::info!("Configuration::set_node_feature updated");

	let metric_checks: Vec<MetricCheckSetup> = vec![
		(BLOCK_HEIGHT_FINALIZED_METRIC, Box::new(|v| v >= 60.0), 400),
		(APPROVAL_CHECKING_FINALITY_LAG_METRIC, Box::new(|v| v < 3.0), 0),
		(APPROVAL_NO_SHOWS_TOTAL_METRIC, Box::new(|v| v < 3.0), 100),
	];

	check_metrics(&validator_nodes, &metric_checks).await?;

	log::info!("Ensure we used systematic chunk recovery and that there are no failed recoveries.");
	let log_lines_checks = vec![
		(
			DATA_RECOVERY_FROM_SYSTEMATIC_CHUNKS_COMPLETE_PATTERN,
			LogLineCountOptions::at_least(10, Duration::from_secs(300)),
		),
		(
			DATA_RECOVERY_FROM_SYSTEMATIC_CHUNKS_NOT_POSSIBLE_PATTERN,
			LogLineCountOptions::no_occurences_within_timeout(Duration::from_secs(10)),
		),
		(
			DATA_RECOVERY_CHUNKS_NOT_POSSIBLE_PATTERN,
			LogLineCountOptions::no_occurences_within_timeout(Duration::from_secs(10)),
		),
	];
	check_log_lines(&validator_nodes, &log_lines_checks).await?;

	for validator in &validator_nodes {
		let result = validator
			.wait_log_line_count_with_timeout(
				AVAILABILITY_RECOVERY_RECOVERIES_FINISHED,
				true,
				LogLineCountOptions::no_occurences_within_timeout(Duration::from_secs(10)),
			)
			.await?;

		assert!(result.success(), "Can't find a matching line ({AVAILABILITY_RECOVERY_RECOVERIES_FINISHED}) in Validator {}", validator.name());
	}
	log::info!(
		"All validators pass the log line match - {AVAILABILITY_RECOVERY_RECOVERIES_FINISHED}"
	);

	log::info!("Test finished successfully");
	Ok(())
}

fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let polkadot_image = env_or_default(INTEGRATION_IMAGE_ENV, images.polkadot.as_str());
	let col_image = env_or_default(COL_IMAGE_ENV, images.cumulus.as_str());

	let mut builder = NetworkConfigBuilder::new().with_relaychain(|r| {
		r
        .with_chain("rococo-local")
        .with_default_command("polkadot")
        .with_default_image(polkadot_image.as_str())
        .with_default_args(maybe_enable_experimental_collator_protocol(
            vec!["-lparachain=debug,runtime=debug".into()],
        ))
        .with_genesis_overrides(json!({
            "patch": {
                "configuration": {
                    "config": {
                        "needed_approvals": 4,
                        "scheduler_params": {
                            "max_validators_per_core": 2
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
        .with_validator(|node| node.with_name("alice"))
        .with_node_group(|g| {
            g.with_count(3)
			.with_base_node(|node| {
                node.with_name("validator")
                    .with_args(vec!["-lparachain=debug,parachain::availability-recovery=trace,parachain::availability-distribution=trace".into()])
            })
		})
	});

	builder = PARAS.into_iter().fold(builder, |acc, para_id| {
		acc.with_parachain(|p| {
			p.with_id(para_id)
				.with_chain(format!("glutton-westend-local-{para_id}").as_str())
				.with_genesis_overrides(json!({
					"patch": {
						"glutton": {
							"compute": "50000000",
							"storage": "2500000000",
							"trashDataCount": 5120
						}
					}
				}))
				.with_default_image(col_image.as_str())
				.with_default_command("polkadot-parachain")
				.with_default_args(vec![
					"-lparachain=debug".into(),
					"--authoring=slot-based".into(),
				])
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
