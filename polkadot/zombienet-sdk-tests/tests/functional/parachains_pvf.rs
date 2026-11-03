// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Parachains PVF
//!
//! This test verifies the PVF preparation & execution time.
//! It sets up a network with 8 validators and 8 parachains,

use crate::utils::{
	assert_nodes_are_validators, env_or_default, initialize_network,
	maybe_enable_experimental_collator_protocol, BLOCK_HEIGHT_FINALIZED_METRIC, COL_IMAGE_ENV,
	INTEGRATION_IMAGE_ENV,
};
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::assert_para_throughput;
use polkadot_primitives::Id as ParaId;
use std::{collections::HashMap, ops::Range};
use zombienet_sdk::{NetworkConfig, NetworkConfigBuilder};

const VALIDATORS: [&str; 8] = ["alice", "bob", "charlie", "dave", "ferdie", "eve", "one", "two"];
const PARAS: [u32; 8] = [2000, 2001, 2002, 2003, 2004, 2005, 2006, 2007];
const PVF_PREPARATION_TIME_HISTOGRAM: &str = "polkadot_pvf_preparation_time";
const PVF_EXECUTION_TIME_HISTOGRAM: &str = "polkadot_pvf_execution_time";

#[tokio::test(flavor = "multi_thread")]
async fn parachains_pvf_preparation_and_execution_test() -> Result<(), anyhow::Error> {
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
	let relay_node = network.get_node(VALIDATORS[0])?;
	let relay_client = relay_node.wait_client().await?;

	// Check that all parachains produce at least 5 blocks within 180 seconds
	// Using 60 relay blocks as window (~180 seconds with 3s block time)
	log::info!("Checking parachain block production");
	let para_throughput: [(ParaId, Range<u32>); 8] = PARAS.map(|id| (ParaId::from(id), 5..61));
	assert_para_throughput(&relay_client, 60, para_throughput, []).await?;
	log::info!("All parachains producing blocks");

	relay_node
		.wait_metric_with_timeout(BLOCK_HEIGHT_FINALIZED_METRIC, |count| count >= 30.0, 400u64)
		.await?;

	// Check preparation time is under 10s.
	// Check all buckets <= 10.
	log::info!("Checking preparation time is under 10s.");
	for name in VALIDATORS {
		let validator = network.get_node(name)?;
		let buckets = validator.get_histogram_buckets(PVF_PREPARATION_TIME_HISTOGRAM, None).await?;
		assert_buckets_count(buckets, &["0.1", "0.5", "1", "2", "3", "10"], |x| x >= 1)?;
	}
	log::info!("All validators passed the preparation time <= 10");

	// Check all buckets >= 20.
	log::info!("Checking preparation time >= 20 has 0 samples.");
	for name in VALIDATORS {
		let validator = network.get_node(name)?;
		let buckets = validator.get_histogram_buckets(PVF_PREPARATION_TIME_HISTOGRAM, None).await?;
		assert_buckets_count(buckets, &["20", "30", "60", "120", "+Inf"], |x| x == 0)?;
	}
	log::info!("All validators passed the preparation time >= 20 have 0 samples");

	// Check execution time.
	// There are two different timeout conditions: DEFAULT_BACKING_EXECUTION_TIMEOUT(2s) and
	// DEFAULT_APPROVAL_EXECUTION_TIMEOUT(12s). Currently these are not differentiated by metrics
	// because the metrics are defined in `polkadot-node-core-pvf` which is a level below
	// the relevant subsystems.
	// That being said, we will take the simplifying assumption of testing only the
	// 2s timeout.
	// We do this check by ensuring all executions fall into bucket le="2" or lower.
	// First, check if we have at least 1 sample, but we should have many more.
	log::info!("Checking executions time <= 2s.");
	for name in VALIDATORS {
		let validator = network.get_node(name)?;
		let buckets = validator.get_histogram_buckets(PVF_EXECUTION_TIME_HISTOGRAM, None).await?;
		assert_buckets_count(buckets, &["0.01", "0.025", "0.05", "0.1", "0.5", "1", "2"], |x| {
			x >= 1
		})?;
	}
	log::info!("All validators passed the executions time <= 2s have at least 1 samples");

	log::info!("Checking execution time have no samples > 2s.");
	for name in VALIDATORS {
		let validator = network.get_node(name)?;
		let buckets = validator.get_histogram_buckets(PVF_EXECUTION_TIME_HISTOGRAM, None).await?;
		assert_buckets_count(buckets, &["4", "5", "6", "+Inf"], |x| x == 0)?;
	}
	log::info!("All validators passed the executions time > 2s have 0 samples");

	log::info!("Test finished successfully");
	Ok(())
}

fn assert_buckets_count(
	buckets: HashMap<String, u64>,
	targets: &[&str],
	cmp_fn: impl Fn(u64) -> bool,
) -> Result<(), anyhow::Error> {
	let mut count = 0;
	for target in targets.iter() {
		count += buckets.get(*target).ok_or(anyhow!("Bucket {} not found in metrics", target))?;
		if cmp_fn(count) {
			return Ok(());
		}
	}

	// last check
	if cmp_fn(count) {
		Ok(())
	} else {
		log::info!("buckets distribution: {buckets:?}");
		Err(anyhow!("buckets distributions doesn't pass the predicatated"))
	}
}

fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let polkadot_image = env_or_default(INTEGRATION_IMAGE_ENV, images.polkadot.as_str());
	let col_image = env_or_default(COL_IMAGE_ENV, images.cumulus.as_str());

	let mut builder = NetworkConfigBuilder::new().with_relaychain(|r| {
		let r = r
			.with_chain("rococo-local")
			.with_default_command("polkadot")
			.with_default_image(polkadot_image.as_str())
			.with_default_args(maybe_enable_experimental_collator_protocol(vec![
				"-lparachain=debug,runtime=debug".into(),
			]))
			.with_default_resources(|r| {
				r.with_limit_memory("4G")
					.with_limit_cpu("2")
					.with_request_memory("2G")
					.with_request_cpu("1")
			});

		let r = r.with_validator(|node| node.with_name(VALIDATORS[0]));

		VALIDATORS[1..]
			.iter()
			.fold(r, |acc, name| acc.with_validator(|node| node.with_name(*name)))
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
