// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// The test sets up a network with 4 validators and 1 collator, then performs a runtime upgrade
// of the parachain to a runtime with an increased version and slot duration of 18 seconds, waits
// for the upgrade to complete and verifies that the relay chain is working and finalizing, and
// the parachain is producing blocks (waits for 10 blocks).

use crate::utils::initialize_network;
use anyhow::anyhow;
use cumulus_test_runtime::slot_duration_18s::WASM_BINARY as WASM_WITH_SLOT_DURATION_18S;
use cumulus_zombienet_sdk_helpers::{
	assert_blocks_are_being_finalized, assert_para_throughput, submit_sudo_runtime_upgrade,
	wait_for_pvf_prepare, wait_for_runtime_upgrade,
};
use futures::StreamExt;
use polkadot_primitives::Id as ParaId;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	subxt_signer::sr25519::dev,
	NetworkConfig, NetworkConfigBuilder,
};

const PARA_ID: u32 = 2000;

#[tokio::test(flavor = "multi_thread")]
async fn parachain_runtime_upgrade_slot_duration_18s() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	log::info!("Spawning network");
	let config = build_network_config().await?;
	let network = initialize_network(config).await?;

	let relay_node = network.get_node("validator-0")?;
	let collator_node = network.get_node("collator")?;

	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;
	let collator_client: OnlineClient<PolkadotConfig> = collator_node.wait_client().await?;

	let wasm = WASM_WITH_SLOT_DURATION_18S
		.expect("WASM binary for slot-duration-18s runtime should be available");

	let initial_slot_duration = get_slot_duration(&collator_client).await?;

	log::info!("Performing runtime upgrade for parachain {}", PARA_ID);
	submit_sudo_runtime_upgrade(&collator_client, wasm, &dev::alice()).await?;

	let block_hash_of_upgrade = wait_for_runtime_upgrade(&collator_client).await?;

	// since https://github.com/paritytech/polkadot-sdk/pull/6029
	// we need to wait to the next block to get the slot duration updated.
	log::info!("Waiting for next finalized block for parachain {PARA_ID}...");
	let mut finalized_blocks = collator_client.blocks().subscribe_finalized().await?.take(2);
	while let Some(block) = finalized_blocks.next().await {
		let block = block?;
		let hash = block.hash();
		log::info!("Checking Block #{} ({hash})", block.header().number);
		if block_hash_of_upgrade != hash {
			break;
		} else {
			log::info!("Same block where the upgrade was detected, waiting one more...");
		}
	}

	let slot_duration = get_slot_duration(&collator_client).await?;
	assert_ne!(
		initial_slot_duration, slot_duration,
		"Slot duration should have changed between the runtime upgrades"
	);
	assert_eq!(
		slot_duration, 18000,
		"Expected slot duration to be 18000 ms (18 seconds), but got {} ms",
		slot_duration
	);
	log::info!("Slot duration verified: {} ms", slot_duration);

	log::info!("Checking that parachain continues producing blocks after upgrade...");

	// Wait for post-upgrade PVF preparation to complete.
	wait_for_pvf_prepare(&network, 2).await?;

	assert_para_throughput(&relay_client, 15, [(ParaId::from(PARA_ID), 10..30)], []).await?;

	log::info!("Checking that relay chain is finalizing blocks...");
	assert_blocks_are_being_finalized(&relay_client).await?;
	Ok(())
}

async fn get_slot_duration(client: &OnlineClient<PolkadotConfig>) -> Result<u64, anyhow::Error> {
	let best_block = client.blocks().at_latest().await?;
	let block_hash = best_block.hash();

	use zombienet_sdk::subxt::dynamic::Value;
	let result = client
		.runtime_api()
		.at(block_hash)
		.call(zombienet_sdk::subxt::dynamic::runtime_api_call(
			"AuraApi",
			"slot_duration",
			Vec::<Value>::new(),
		))
		.await?;

	result.as_type().map_err(Into::into)
}

async fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();

	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![("-lparachain=debug").into()])
				.with_validator(|node| node.with_name("validator-0"));

			// Add 4 validators
			(1..4).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-{i}")))
			})
		})
		.with_parachain(|p| {
			p.with_id(PARA_ID)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_default_args(vec![("-lparachain=debug,aura=debug").into()])
				.with_collator(|n| n.with_name("collator").validator(true))
		})
		.with_global_settings(|global_settings| match std::env::var("ZOMBIENET_SDK_BASE_DIR") {
			Ok(val) => global_settings.with_base_dir(val),
			_ => global_settings,
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	Ok(config)
}
