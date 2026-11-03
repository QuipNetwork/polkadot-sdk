// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use crate::utils::initialize_network;

use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::{
	assert_para_throughput, assign_cores, wait_for_first_session_change, wait_for_pvf_prepare,
};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfig, NetworkConfigBuilder,
};

const PARA_ID: u32 = 1000;

async fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	// images are not relevant for `native`, but we leave it here in case we use `k8s` some day
	let images = zombienet_sdk::environment::get_images_from_env();
	log::info!("Using images: {images:?}");

	NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("westend-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![("-lparachain=trace").into()])
				.with_default_resources(|resources| {
					// These settings are applicable only for `k8s` provider.
					// Leaving them in case we switch to `k8s` some day.
					resources.with_request_cpu(4).with_request_memory("4G")
				})
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"num_cores": 3,
								"max_validators_per_core": 1,
								"group_rotation_frequency": 5,
								"lookahead": 5,
							}
						}
					}
				}))
				// Have to set a `with_validator` outside of the loop below, so that `r` has the right
				// type.
				.with_validator(|node| node.with_name("validator-0"));
			(1..6).fold(r, |acc, i| acc.with_validator(|node| node.with_name(&format!("validator-{i}"))))
		})
		.with_parachain(|p| {
			p.with_id(PARA_ID)
				.with_default_command("polkadot-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain("asset-hub-westend-local")
				.with_default_args(vec![
                    ("-laura=trace,runtime=info,cumulus-consensus=trace,consensus::common=trace,parachain::collation-generation=trace,parachain::collator-protocol=trace,parachain=debug").into(),
					("--force-authoring").into(),
                    ("--authoring", "slot-based").into(),
				])
				.with_collator(|n| n.with_name("collator-0"))
				.with_collator(|n| n.with_name("collator-1"))
				.with_collator(|n| n.with_name("collator-2"))
		})
		.with_global_settings(|global_settings| match std::env::var("ZOMBIENET_SDK_BASE_DIR") {
			Ok(val) => global_settings.with_base_dir(val),
			_ => global_settings,
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})
}

#[tokio::test(flavor = "multi_thread")]
async fn elastic_scaling_asset_hub_westend() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	log::info!("Spawning network");
	let config = build_network_config().await?;
	let network = initialize_network(config).await?;

	let relay_node = network.get_node("validator-0")?;
	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;

	let mut blocks_sub = relay_client.blocks().subscribe_finalized().await?;
	wait_for_first_session_change(&mut blocks_sub).await?;

	// Wait for PVF preparation to complete.
	wait_for_pvf_prepare(&network, 1).await?;

	assert_para_throughput(&relay_client, 10, [(ParaId::from(PARA_ID), 8..11)], []).await?;

	// 1 core is assigned by default, we are assigning 2 more cores: 0 and 1.
	assign_cores(&relay_client, PARA_ID, vec![0, 1]).await?;

	log::info!("Ensure elastic scaling works, 3 blocks should be produced in each 6s slot");

	assert_para_throughput(&relay_client, 20, [(ParaId::from(PARA_ID), 50..61)], []).await?;

	log::info!("Test finished successfully.");

	Ok(())
}
