// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use anyhow::anyhow;

use cumulus_zombienet_sdk_helpers::{assert_finality_lag, assert_para_throughput};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfigBuilder,
};

// 5d5a639ea7de2b74628232427d5d8d7ce9fb5e8e - Send PeerId via UMP (#10145)
// fe0113539b123b282654ddaa7c7d548a1b59a58b - last commit w/o ApprovedPeer UMP signal support
const PRE_APPROVED_UMP_SIGNAL_COLLATOR_IMAGE: &str = "docker.io/paritypr/colander:master-fe011353";
// Async backing is introduced with 5174b9d2d7a. The last commit in master before it is 4f699c70a46
// Since neither polkadot 1.0 (the last release with collator protocol v1 support only) or the
// commit above is available as a docker image
// a dummy PR (https://github.com/paritytech/polkadot-sdk/pull/12658) was used to build a collator
// supporting V1 only.
const V1_COLLATOR_IMAGE: &str = "docker.io/paritypr/colander:12658-b4d6619f";

const PRE_APPROVED_UMP_SIGNAL_PARA_ID: u32 = 2000;
const V1_PARA_ID: u32 = 2001;

#[tokio::test(flavor = "multi_thread")]
async fn approved_peer_mixed_collators_test() -> Result<(), anyhow::Error> {
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
				.with_default_args(vec![
					("--experimental-collator-protocol").into(),
					("-lparachain=debug").into(),
				])
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"group_rotation_frequency": 4,
								"max_validators_per_core": 2,
							}
						}
					}
				}))
				.with_validator(|node| node.with_name("validator-0"));

			(1..4).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-{i}")))
			})
		})
		.with_parachain(|p| {
			p.with_id(PRE_APPROVED_UMP_SIGNAL_PARA_ID)
				.with_default_command("polkadot-parachain")
				.with_collator(|n| {
					n.with_name("collator-pre-approved-ump")
						.with_image(PRE_APPROVED_UMP_SIGNAL_COLLATOR_IMAGE)
						.with_args(vec![("-lparachain=debug").into()])
				})
		})
		.with_parachain(|p| {
			p.with_id(V1_PARA_ID)
				.with_default_command("polkadot-parachain")
				.with_collator(|n| {
					n.with_name("collator-v1")
						.with_image(V1_COLLATOR_IMAGE)
						.with_args(vec![("-lparachain=debug").into()])
				})
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

	// The parachain should keep producing blocks at a healthy rate, regardless of which collator
	// (recent or old) authored a given block.
	//
	// Both paras are bulk-scheduled in genesis and `num_cores` resolves to 2, so each para owns
	// one core for the whole run. One core backs at most one candidate per relay block, so over
	// the 20 counted blocks the ceiling is 20 candidates per para -- and both paras are expected
	// to sit at that ceiling.
	assert_para_throughput(
		&relay_client,
		20,
		[
			(ParaId::from(PRE_APPROVED_UMP_SIGNAL_PARA_ID), 19..21),
			(ParaId::from(V1_PARA_ID), 19..21),
		],
		[],
	)
	.await?;

	// Finality should not be affected by the mixed collator fleet.
	assert_finality_lag(&relay_node.wait_client().await?, 5).await?;

	// Check that no disputes are raised.
	assert!(relay_node
		.wait_metric_with_timeout("polkadot_parachain_candidate_disputes_total", |v| v > 0.0, 30u64)
		.await
		.is_err());

	relay_node
		.assert("polkadot_parachain_approval_checking_finality_lag", 0.0)
		.await?;

	relay_node.assert("polkadot_parachain_disputes_finality_lag", 0.0).await?;

	log::info!("Test finished successfully");

	Ok(())
}
