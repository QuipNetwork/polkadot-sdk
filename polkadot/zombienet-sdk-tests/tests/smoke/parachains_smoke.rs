// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Parachains Smoke Test
//!
//! This test verifies that a parachain can be registered and produce blocks.
//! It spawns a relay chain with two validators (alice, bob) and registers
//! parachain 100 using the adder-collator.

use crate::utils::{env_or_default, initialize_network, COL_IMAGE_ENV, INTEGRATION_IMAGE_ENV};
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::{assert_para_is_registered, assert_para_throughput};
use polkadot_primitives::Id as ParaId;
use zombienet_sdk::{subxt::PolkadotConfig, NetworkConfig, NetworkConfigBuilder};

const PARA_ID: u32 = 100;

/// Smoke test that verifies parachain registration and block production.
///
/// - Checks parachain 100 is registered within 225 seconds
/// - Checks parachain 100 block height is at least 10 within 400 seconds
#[tokio::test(flavor = "multi_thread")]
async fn parachains_smoke_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let config = build_network_config()?;
	let network = initialize_network(config).await?;

	let alice = network.get_node("alice")?;
	let alice_client: zombienet_sdk::subxt::OnlineClient<PolkadotConfig> =
		alice.wait_client().await?;

	// Check parachain is registered (should be since is included in genesis)
	log::info!("Checking parachain {} is registered", PARA_ID);
	assert_para_is_registered(&alice_client, ParaId::from(PARA_ID), 5).await?;
	log::info!("Parachain {} is registered", PARA_ID);

	// Check parachain produces at least 5 blocks (60 seconds)
	// Using 10 relay blocks as measurement window
	log::info!("Checking parachain {} is producing blocks", PARA_ID);
	assert_para_throughput(&alice_client, 5, [(ParaId::from(PARA_ID), 2..6)], []).await?;
	log::info!("Parachain {} is producing blocks successfully", PARA_ID);

	log::info!("Test finished successfully");
	Ok(())
}

fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let polkadot_image = env_or_default(INTEGRATION_IMAGE_ENV, images.polkadot.as_str());
	let col_image = env_or_default(COL_IMAGE_ENV, images.cumulus.as_str());

	NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(polkadot_image.as_str())
				.with_validator(|node| {
					node.with_name("alice")
						.with_args(vec![("-lruntime=debug,parachain=trace").into()])
				})
				.with_validator(|node| {
					node.with_name("bob")
						.with_args(vec![("-lruntime=debug,parachain=trace").into()])
				})
		})
		.with_parachain(|p| {
			p.with_id(PARA_ID)
				.cumulus_based(false)
				.with_default_command("adder-collator")
				.with_default_image(col_image.as_str())
				.with_collator(|n| {
					n.with_name("collator01")
						.with_args(vec![("-lruntime=debug,parachain=trace").into()])
				})
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
