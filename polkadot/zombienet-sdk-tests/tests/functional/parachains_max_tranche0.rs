// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Parachains Max Tranche0 Test
//!
//! This test verifies that parachains make progress with most of the approvals
//! being tranche0. It sets up a network with 8 validators and 5 parachains,
//! configured with high needed_approvals (7) and relay_vrf_modulo_samples (5)
//! to ensure most approvals come from tranche0.

use crate::utils::{
	assert_nodes_are_validators, env_or_default, initialize_network,
	maybe_enable_experimental_collator_protocol, APPROVAL_CHECKING_FINALITY_LAG_METRIC,
	COL_IMAGE_ENV, INTEGRATION_IMAGE_ENV,
};
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::assert_para_throughput;
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use zombienet_sdk::{NetworkConfig, NetworkConfigBuilder};

const NUM_VALIDATORS: u32 = 8;
const PARAS: [u32; 5] = [2000, 2001, 2002, 2003, 2004];
/// Test that parachains make progress with most approvals being tranche0.
///
/// This test configures the network with:
/// - 8 validators
/// - 5 parachains (2000-2004) with varying PoV sizes and PVF complexity
/// - needed_approvals = 7
/// - relay_vrf_modulo_samples = 5
/// - max_validators_per_core = 1
///
/// It verifies that:
/// - All validators are running as authorities
/// - All parachains produce at least 5 blocks
/// - Approval checking finality lag stays below 2
#[tokio::test(flavor = "multi_thread")]
async fn parachains_max_tranche0_test() -> Result<(), anyhow::Error> {
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
	let relay_node = network.get_node("some-validator-0")?;
	let relay_client = relay_node.wait_client().await?;

	// Check that all parachains produce at least 5 blocks within 180 seconds
	// Using 60 relay blocks as window (~180 seconds with 3s block time)
	log::info!("Checking parachain block production");
	assert_para_throughput(
		&relay_client,
		60,
		[
			(ParaId::from(2000u32), 5..100),
			(ParaId::from(2001u32), 5..100),
			(ParaId::from(2002u32), 5..100),
			(ParaId::from(2003u32), 5..100),
			(ParaId::from(2004u32), 5..100),
		],
		[],
	)
	.await?;
	log::info!("All parachains producing blocks");

	// Check approval finality lag for all validators
	log::info!("Checking approval finality lag");
	for i in 0..NUM_VALIDATORS {
		let validator = network.get_node(format!("some-validator-{i}"))?;
		validator
			.wait_metric_with_timeout(APPROVAL_CHECKING_FINALITY_LAG_METRIC, |v| v < 2.0, 30u64)
			.await
			.map_err(|e| anyhow!("Validator {} finality lag too high: {}", i, e))?;
	}
	log::info!("Approval finality lag within limits");

	log::info!("Test finished successfully");
	Ok(())
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
			.with_genesis_overrides(json!({
				"patch": {
					"configuration": {
						"config": {
							"needed_approvals": 7,
							"relay_vrf_modulo_samples": 5,
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
			});

		let r = r.with_validator(|node| {
			node.with_name("some-validator-0")
				.with_args(vec!["-lparachain=debug,runtime=debug".into()])
		});

		(1..NUM_VALIDATORS).fold(r, |acc, i| {
			acc.with_validator(|node| {
				node.with_name(&format!("some-validator-{i}"))
					.with_args(vec!["-lparachain=debug,runtime=debug".into()])
			})
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
