// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Rolling upgrade test: mixed V2/V3 validator fleet.
//!
//! Runs a network where some validators use the current binary (V3-capable) and others use an
//! older binary (`OLD_POLKADOT_IMAGE`) that does not understand V3 descriptors. V3 is enabled
//! at the runtime level. The collator produces V2 candidates throughout.
//!
//! Verifies that:
//! - V2 candidates are backed by the mixed fleet.
//! - Statement and availability distribution work across binary versions.
//! - GRANDPA finality does not stall.
//! - Parachain throughput is sustained.

use crate::utils::{
	assert_candidates_version, assert_validator_backed_candidates, enable_node_features,
};
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::assert_finality_lag;
use polkadot_primitives::{CandidateDescriptorVersion, Id as ParaId};
use serde_json::json;
use std::collections::HashMap;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfigBuilder,
};

#[tokio::test(flavor = "multi_thread")]
async fn v3_rolling_upgrade() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let images = zombienet_sdk::environment::get_images_from_env();

	let old_image = std::env::var("OLD_POLKADOT_IMAGE")
		.expect("OLD_POLKADOT_IMAGE must be set for rolling upgrade test");
	let old_command = std::env::var("OLD_POLKADOT_COMMAND").unwrap_or("polkadot".into());

	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![("-lparachain=debug,runtime=debug").into()])
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"max_validators_per_core": 3,
								"group_rotation_frequency": 4
							}
						}
					}
				}))
				.with_validator(|node| node.with_name("validator-0"));

			let r = (1..3).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-{i}")))
			});

			(3..6).fold(r, |acc, i| {
				acc.with_validator(|node| {
					node.with_name(&format!("old-validator-{i}"))
						.with_image(old_image.as_str())
						.with_command(old_command.as_str())
				})
			})
		})
		.with_parachain(|p| {
			p.with_id(3000)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain("async-backing")
				.with_default_args(vec![
					("--authoring=slot-based").into(),
					("-lparachain=debug,aura=debug").into(),
				])
				.with_collator(|n| n.with_name("collator-3000"))
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_node = network.get_node("validator-0")?;
	let para_node = network.get_node("collator-3000")?;
	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;

	// enabling v3 here does not overwrite all node_features
	enable_node_features(&relay_client, &[4]).await?;

	assert_candidates_version(
		&relay_client,
		CandidateDescriptorVersion::V2,
		HashMap::from([(ParaId::from(3000), 40..51)]),
		50,
	)
	.await?;

	// Verify no disputes are raised.
	relay_node
		.wait_metric_with_timeout(
			"polkadot_parachain_candidate_disputes_total",
			|v| v == 0.0,
			30u64,
		)
		.await?;

	// Verify both new and old validators sign backing statements.
	for name in [
		"validator-0",
		"validator-1",
		"validator-2",
		"old-validator-3",
		"old-validator-4",
		"old-validator-5",
	] {
		let node = network.get_node(name)?;
		assert_validator_backed_candidates(node, 30).await?;
	}

	assert_finality_lag(&para_node.wait_client().await?, 6).await?;

	log::info!("Rolling upgrade test finished successfully");

	Ok(())
}
