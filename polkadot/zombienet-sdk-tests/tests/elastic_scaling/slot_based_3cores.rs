// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Test that parachains that use a single slot-based collator with elastic scaling MVP and with
// elastic scaling with RFC103 can achieve full throughput of 3 candidates per block.

use crate::utils::maybe_enable_experimental_collator_protocol;
use anyhow::anyhow;

use cumulus_zombienet_sdk_helpers::{
	assert_finality_lag, assert_para_throughput, assign_cores, wait_for_pvf_prepare,
};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfigBuilder,
};

#[tokio::test(flavor = "multi_thread")]
async fn slot_based_3cores_test() -> Result<(), anyhow::Error> {
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
								// Num cores is 4, because 2 extra will be added automatically when registering the paras.
								"num_cores": 4,
								"max_validators_per_core": 2
							}
						}
					}
				}))
				.with_default_resources(|resources| {
					resources.with_request_cpu(4).with_request_memory("4G")
				})
				// Have to set a `with_validator` outside of the loop below, so that `r` has the
				// right type.
				.with_validator(|node| node.with_name("validator-0"));

			(1..12).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-{i}")))
			})
		})
		.with_parachain(|p| {
			p.with_id(2100)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain("elastic-scaling-mvp")
				.with_default_args(vec![
					"--authoring=slot-based".into(),
					("-lparachain=debug,aura=debug").into(),
				])
				.with_collator(|n| n.with_name("collator-elastic-mvp"))
		})
		.with_parachain(|p| {
			p.with_id(2200)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain("elastic-scaling")
				.with_default_args(vec![
					"--authoring=slot-based".into(),
					("-lparachain=debug,aura=debug").into(),
				])
				.with_collator(|n| n.with_name("collator-elastic"))
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_node = network.get_node("validator-0")?;
	let para_node_elastic = network.get_node("collator-elastic")?;
	let para_node_elastic_mvp = network.get_node("collator-elastic-mvp")?;

	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;

	// Assign two extra cores to each parachain.
	// We need to execute both call one after another to ensure that the internal logic fetches the
	// correct nonce.
	assign_cores(&relay_client, 2100, vec![0, 1]).await?;
	assign_cores(&relay_client, 2200, vec![2, 3]).await?;

	// Wait for PVF preparation to complete.
	wait_for_pvf_prepare(&network, 1).await?;

	// Expect a backed candidate count of at least 39 for each parachain in 15 relay chain blocks
	// (2.6 candidates per para per relay chain block).
	// Note that only blocks after the first session change and blocks that don't contain a session
	// change will be counted.
	// Since the calculated backed candidate count is theoretical and the CI tests are observed to
	// occasionally fail, let's apply 12.5% tolerance to the expected range: 39 - 12.5% =~ 34
	assert_para_throughput(
		&relay_client,
		15,
		[(ParaId::from(2100), 40..46), (ParaId::from(2200), 40..46)],
		[],
	)
	.await?;

	// Assert the parachain finalized block height is also on par with the number of backed
	// candidates.
	assert_finality_lag(&para_node_elastic.wait_client().await?, 15).await?;
	assert_finality_lag(&para_node_elastic_mvp.wait_client().await?, 15).await?;

	log::info!("Test finished successfully");

	Ok(())
}
