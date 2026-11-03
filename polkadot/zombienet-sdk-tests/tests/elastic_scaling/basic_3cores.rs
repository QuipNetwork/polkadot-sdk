// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Test that a parachain that uses a basic collator (like adder-collator) with elastic scaling
// can achieve full throughput of 3 candidates per block.

use crate::utils::maybe_enable_experimental_collator_protocol;
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::{assert_para_throughput, assign_cores, wait_for_pvf_prepare};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfigBuilder,
};

#[tokio::test(flavor = "multi_thread")]
async fn basic_3cores_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let images = zombienet_sdk::environment::get_images_from_env();

	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(maybe_enable_experimental_collator_protocol(vec![
					("-lparachain=debug").into(),
				]))
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"num_cores": 2,
								"max_validators_per_core": 1
							},
						}
					}
				}))
				// Have to set a `with_validator` outside of the loop below, so that `r` has the
				// right type.
				.with_validator(|node| node.with_name("validator-0"));

			(1..4).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-{i}")))
			})
		})
		.with_parachain(|p| {
			p.with_id(2000)
				.with_default_command("adder-collator")
				.cumulus_based(false)
				.with_default_image(images.cumulus.as_str())
				.with_default_args(vec![("-lparachain=debug").into()])
				.with_collator(|n| n.with_name("adder-2000"))
		})
		.with_parachain(|p| {
			p.with_id(2001)
				.with_default_command("adder-collator")
				.cumulus_based(false)
				.with_default_image(images.cumulus.as_str())
				.with_default_args(vec![("-lparachain=debug").into()])
				.with_collator(|n| n.with_name("adder-2001"))
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_node = network.get_node("validator-0")?;

	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;

	// Assign two extra cores to adder-2000.
	assign_cores(&relay_client, 2000, vec![0, 1]).await?;

	// Wait for PVF preparation to complete.
	wait_for_pvf_prepare(&network, 1).await?;

	assert_para_throughput(
		&relay_client,
		15,
		[(ParaId::from(2000), 41..46), (ParaId::from(2001), 13..16)],
		[],
	)
	.await?;

	log::info!("Test finished successfully");

	Ok(())
}
