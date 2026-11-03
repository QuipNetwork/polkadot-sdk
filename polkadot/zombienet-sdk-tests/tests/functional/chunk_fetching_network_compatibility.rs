// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Setup a network with 4 validators, with 2 in the current version (built in the workflow)
//! and two old ones (from a version that doesn't send out v2 receipts). Also, 2 parachains
//! with the same old version.

use crate::utils::{
	assert_nodes_are_validators, check_log_lines, check_metrics, env_or_default,
	initialize_network, MetricCheckSetup, APPROVAL_CHECKING_FINALITY_LAG_METRIC,
	APPROVAL_NO_SHOWS_TOTAL_METRIC, AVAILABILITY_RECOVERY_RECOVERIES_FINISHED,
	BLOCK_HEIGHT_FINALIZED_METRIC, COL_IMAGE_ENV, DATA_RECOVERY_CHUNKS_PATTERN,
	DATA_RECOVERY_FROM_SYSTEMATIC_CHUNKS_COMPLETE_PATTERN,
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

#[tokio::test(flavor = "multi_thread")]
async fn chunk_fetching_network_compatibility_test() -> Result<(), anyhow::Error> {
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

	// Check that all parachains produce at least 5 blocks within 1 session and 5 blocks (RC)
	let relay_client = validator_nodes[0].wait_client().await?;
	log::info!("Checking parachain block production (all paras registered at genesis)");
	let para_throughput: [(ParaId, Range<u32>); 2] = PARAS.map(|id| (ParaId::from(id), 2..6));
	assert_para_throughput(&relay_client, 5, para_throughput, []).await?;
	log::info!("All parachains producing blocks");

	log::info!("Ensure approval checking works.");
	let metric_checks: Vec<MetricCheckSetup> = vec![
		(BLOCK_HEIGHT_FINALIZED_METRIC, Box::new(|v| v == 30.0), 400),
		(APPROVAL_CHECKING_FINALITY_LAG_METRIC, Box::new(|v| v < 3.0), 0),
		(APPROVAL_NO_SHOWS_TOTAL_METRIC, Box::new(|v| v < 3.0), 100),
	];

	check_metrics(&validator_nodes, &metric_checks).await?;

	log::info!("Ensure that there are no failed recoveries.");
	let log_lines_checks = vec![
		(DATA_RECOVERY_CHUNKS_PATTERN, LogLineCountOptions::at_least(10, Duration::from_secs(300))),
		(
			DATA_RECOVERY_FROM_SYSTEMATIC_CHUNKS_NOT_POSSIBLE_PATTERN,
			LogLineCountOptions::no_occurences_within_timeout(Duration::from_secs(10)),
		),
	];
	check_log_lines(&validator_nodes, &log_lines_checks).await?;

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

	log::info!("Ensure we used the fallback network request and systematic recovery was not used.");

	// Patterns used in log line counts
	let log_lines_checks = vec![
		(
			"*Trying the fallback protocol*",
			LogLineCountOptions::at_least(1, Duration::from_secs(100)),
		),
		(
			DATA_RECOVERY_FROM_SYSTEMATIC_CHUNKS_COMPLETE_PATTERN,
			LogLineCountOptions::no_occurences_within_timeout(Duration::from_secs(10)),
		),
	];
	check_log_lines(&validator_nodes, &log_lines_checks).await?;

	log::info!("Ensure availability-distribution worked fine.");
	let metric_checks: Vec<MetricCheckSetup> = vec![
		(
			"polkadot_parachain_fetched_chunks_total{success=\"succeeded\"}",
			Box::new(|v| v == 10.0),
			400,
		),
		("polkadot_parachain_fetched_chunks_total{success=\"failed\"}", Box::new(|v| v == 0.0), 10),
		(
			"polkadot_parachain_fetched_chunks_total{success=\"not-found\"}",
			Box::new(|v| v == 0.0),
			10,
		),
	];

	check_metrics(&validator_nodes, &metric_checks).await?;

	log::info!("Test finished successfully");
	Ok(())
}

fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let polkadot_image = env_or_default(INTEGRATION_IMAGE_ENV, images.polkadot.as_str());
	let col_image = env_or_default(COL_IMAGE_ENV, images.cumulus.as_str());
	let old_suffix = std::env::var("OLD_SUFFIX").unwrap_or_else(|_| "-old".to_string());

	let mut builder = NetworkConfigBuilder::new().with_relaychain(|r| {
		r
        .with_chain("rococo-local")
        .with_default_command("polkadot")
        .with_default_image(polkadot_image.as_str())
        .with_default_args(vec!["-lparachain=debug,runtime=debug".into()])
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
        .with_node_group(|g| {
            g.with_count(2)
			.with_base_node(|node| {
                node.with_name("old")
                    // Use a version that doesn't speak /req_chunk/2 protocol.
                    .with_command(format!("polkadot{old_suffix}").as_str())
                    .with_args(vec!["-lparachain=debug,parachain::availability-recovery=trace,parachain::availability-distribution=trace".into()])
            })
		})
        .with_node_group(|g| {
            g.with_count(2)
			.with_base_node(|node| {
                node.with_name("new")
                    .with_command("polkadot")
                    .with_args(vec!["-lparachain=debug,parachain::availability-recovery=trace,parachain::availability-distribution=trace,sub-libp2p=trace".into()])
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
				// Use an old image that does not send out v2 receipts, as the old validators will
				// still check the collator signatures.
				.with_default_command(format!("polkadot-parachain{old_suffix}").as_str())
				.with_default_args(vec!["-lparachain=debug".into()])
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
