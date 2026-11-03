// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Test that checks approval voting coalescing does not lag finality.

use crate::utils::maybe_enable_experimental_collator_protocol;
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::{assert_finality_lag, assert_para_throughput};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfigBuilder,
};

#[tokio::test(flavor = "multi_thread")]
async fn approval_voting_coalescing_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let images = zombienet_sdk::environment::get_images_from_env();
	let no_show_slots = 4;
	let mut config_builder = NetworkConfigBuilder::new().with_relaychain(|r| {
		let r = r
			.with_chain("rococo-local")
			.with_default_command("polkadot")
			.with_default_image(images.polkadot.as_str())
			.with_default_args(maybe_enable_experimental_collator_protocol(vec![
				("-lparachain=debug,runtime=debug").into(),
			]))
			.with_genesis_overrides(json!({
				"configuration": {
					"config": {
						"needed_approvals": 4,
						"relay_vrf_modulo_samples": 6,
						"no_show_slots": no_show_slots,
						"approval_voting_params": {
							"max_approval_coalesce_count": 5
						}
					}
				}
			}))
			.with_validator(|node| node.with_name("validator-0"));

		(1..12)
			.fold(r, |acc, i| acc.with_validator(|node| node.with_name(&format!("validator-{i}"))))
	});

	for para_id in 2000..2008 {
		let collator_name = format!("collator-undying-{para_id}");
		config_builder = config_builder.with_parachain(|p| {
			p.with_id(para_id)
				.with_default_command("undying-collator")
				.with_default_image(
					std::env::var("COL_IMAGE")
						.unwrap_or("docker.io/paritypr/colander:latest".to_string())
						.as_str(),
				)
				.cumulus_based(false)
				.with_default_args(vec![("-lparachain=debug").into()])
				.with_collator(|n| n.with_name(&collator_name))
		});
	}

	let config = config_builder.build().map_err(|e| {
		let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
		anyhow!("config errs: {errs}")
	})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();

	log::info!("Spawning network");
	let network = spawn_fn(config).await?;

	log::info!("Waiting for network to initialize");
	let relay_node = network.get_node("validator-0")?;

	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;

	log::info!("Waiting for parachains to advance to block 15");
	assert_para_throughput(
		&relay_client,
		30,
		[
			(ParaId::from(2000), 11..35),
			(ParaId::from(2001), 11..35),
			(ParaId::from(2002), 11..35),
			(ParaId::from(2003), 11..35),
			(ParaId::from(2004), 11..35),
			(ParaId::from(2005), 11..35),
			(ParaId::from(2006), 11..35),
			(ParaId::from(2007), 11..35),
		],
		[],
	)
	.await?;

	log::info!("Checking finality does not lag and no-shows are within range");
	for node in network.nodes() {
		assert_finality_lag(&node.wait_client().await?, no_show_slots).await?;
		assert!(
			node.reports("polkadot_parachain_approvals_no_shows_total").await.unwrap() < 3.0,
			"No-shows should be less than 3"
		);
	}

	log::info!("Test finished successfully");

	Ok(())
}
