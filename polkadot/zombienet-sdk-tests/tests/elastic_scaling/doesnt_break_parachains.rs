// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Test that a paraid that doesn't use elastic scaling which acquired multiple cores does not brick
// itself if ElasticScalingMVP feature is enabled in genesis.

use crate::utils::maybe_enable_experimental_collator_protocol;
use anyhow::anyhow;
use codec::Decode;
use cumulus_zombienet_sdk_helpers::{
	assert_finality_lag, assert_para_throughput, assign_cores, wait_for_pvf_prepare,
};
use polkadot_primitives::{CoreIndex, Id as ParaId};
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfigBuilder,
};

#[tokio::test(flavor = "multi_thread")]
async fn doesnt_break_parachains_test() -> Result<(), anyhow::Error> {
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
								"num_cores": 1,
								"max_validators_per_core": 2,
							}
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
			// Use default, which has 6 second slot time. Also, don't use slot-based collator.
			p.with_id(2000)
				.with_default_command("polkadot-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_default_args(vec![("-lparachain=debug,aura=debug").into()])
				.with_collator(|n| n.with_name("collator-2000"))
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_node = network.get_node("validator-0")?;
	let para_node = network.get_node("collator-2000")?;

	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;

	assign_cores(&relay_client, 2000, vec![0]).await?;

	let para_id = ParaId::from(2000);
	// Wait for PVF preparation to complete.
	wait_for_pvf_prepare(&network, 1).await?;
	// Expect the parachain to be making normal progress, 1 candidate backed per relay chain block.
	// Lowering to 12 to make sure CI passes.
	assert_para_throughput(&relay_client, 15, [(para_id, 12..16)], []).await?;

	let para_client = para_node.wait_client().await?;
	// Assert the parachain finalized block height is also on par with the number of backed
	// candidates.
	// Increasing to 6 to make sure CI passes.
	assert_finality_lag(&para_client, 6).await?;

	// Sanity check that indeed the parachain has two assigned cores.
	let cq = BTreeMap::<CoreIndex, VecDeque<ParaId>>::decode(
		&mut &relay_client
			.runtime_api()
			.at_latest()
			.await?
			.call_raw("ParachainHost_claim_queue", None)
			.await?[..],
	)?;

	// Get looakahead config
	let lookahead = u32::decode(
		&mut &relay_client
			.runtime_api()
			.at_latest()
			.await?
			.call_raw("ParachainHost_scheduling_lookahead", None)
			.await?[..],
	)?;

	assert_eq!(
		cq,
		[
			(CoreIndex(0), std::iter::repeat_n(para_id, lookahead as usize).collect()),
			(CoreIndex(1), std::iter::repeat_n(para_id, lookahead as usize).collect()),
		]
		.into_iter()
		.collect()
	);

	log::info!("Test finished successfully");

	Ok(())
}
