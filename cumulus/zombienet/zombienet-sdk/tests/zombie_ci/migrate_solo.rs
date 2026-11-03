// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use anyhow::anyhow;
use serde_json::json;
use std::{path::Path, str::FromStr};

use crate::utils::{initialize_network, BEST_BLOCK_METRIC};

use cumulus_zombienet_sdk_helpers::assert_para_throughput;
use polkadot_primitives::Id as ParaId;
use sp_core::{hexdisplay::AsBytesRef, Bytes};
use zombienet_sdk::{
	subxt::{
		self, dynamic::Value, tx::DynamicPayload, OnlineClient, PolkadotConfig, SubstrateConfig,
	},
	subxt_signer::sr25519::dev,
	NetworkConfig, NetworkConfigBuilder, RegistrationStrategy,
};

const PARA_ID: u32 = 2000;

async fn create_migrate_solo_to_para_call(
	base_dir: &str,
	solo_dir: &str,
) -> Result<DynamicPayload, anyhow::Error> {
	let file_path = Path::new(base_dir).join(solo_dir).join("genesis-state");

	// genesis state is stored as hex string
	let genesis_state = std::fs::read_to_string(file_path)?;
	let genesis_head = Value::from_bytes(Bytes::from_str(&genesis_state)?.as_bytes_ref());

	let call =
		subxt::dynamic::tx("TestPallet", "set_custom_validation_head_data", vec![genesis_head]);
	Ok(call)
}

#[tokio::test(flavor = "multi_thread")]
async fn migrate_solo_to_para() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	log::info!("Spawning network");
	let config = build_network_config().await?;
	let network = initialize_network(config).await?;

	let alice = network.get_node("alice")?;
	let alice_client: OnlineClient<PolkadotConfig> = alice.wait_client().await?;

	log::info!("Ensuring parachain making progress");
	assert_para_throughput(&alice_client, 20, [(ParaId::from(PARA_ID), 2..40)], []).await?;

	let dave = network.get_node("dave")?;

	// alice: parachain 2000 block height is at least 10 within 250 seconds
	log::info!("Ensuring dave reports expected block height");
	assert!(dave
		.wait_metric_with_timeout(BEST_BLOCK_METRIC, |b| b >= 10.0, 250u64)
		.await
		.is_ok());

	let eve = network.get_node("eve")?;
	// solo node should not produce blocks
	log::info!("Ensuring eve reports expected block height");
	assert!(eve
		.wait_metric_with_timeout(BEST_BLOCK_METRIC, |b| b == 0.0, 20u64)
		.await
		.is_ok());

	log::info!("Migrating solo to para");
	let base_dir = network.base_dir().ok_or(anyhow!("failed to get base dir"))?;
	let call = create_migrate_solo_to_para_call(base_dir, "2000-1").await?;
	let dave_client: OnlineClient<SubstrateConfig> = dave.wait_client().await?;

	// Don't wait for finalization. dave will be disconnected after transaction success and it won't
	// be able to get its status
	let res = dave_client.tx().sign_and_submit_then_watch_default(&call, &dev::alice()).await;
	assert!(res.is_ok(), "Extrinsic failed to submit: {:?}", res.unwrap_err());

	// eve (solo node) should produce blocks now
	log::info!("Ensuring eve reports expected block height");
	assert!(eve
		.wait_metric_with_timeout(BEST_BLOCK_METRIC, |b| b == 5.0, 200u64)
		.await
		.is_ok());

	let dave_best_block = dave.reports(BEST_BLOCK_METRIC).await?;

	log::info!("Ensuring eve reports expected block height");
	assert!(eve
		.wait_metric_with_timeout(BEST_BLOCK_METRIC, |b| b == 10.0, 50u64)
		.await
		.is_ok());

	log::info!("Ensuring dave no longer produces blocks");
	assert_eq!(dave_best_block, dave.reports(BEST_BLOCK_METRIC).await?);

	Ok(())
}

async fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	// images are not relevant for `native`, but we leave it here in case we use `k8s` some day
	let images = zombienet_sdk::environment::get_images_from_env();
	log::info!("Using images: {images:?}");

	// Network setup:
	// - relaychain nodes:
	// 	 - alice   - validator
	// 	 - bob     - validator
	// - parachain A nodes:
	//   - dave    - validator initially produces blocks, after setting custom validation head data
	//     to parachain B header it stops producing blocks
	// - parachain B nodes:
	//   - eve     - validator initially does not produce blocks because of the parachain header
	//     mismatch, after setting custom validation head data to parachain B header it starts
	//     producing blocks
	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![("-lparachain=debug").into()])
				.with_validator(|node| node.with_name("alice"))
				.with_validator(|node| node.with_name("bob"))
		})
		.with_parachain(|p| {
			// parachain A
			p.with_id(PARA_ID)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_collator(|n| {
					n.with_name("dave").with_args(vec![("-lparachain=debug").into()])
				})
		})
		.with_parachain(|p| {
			// parachain B
			p.with_id(PARA_ID)
				.with_registration_strategy(RegistrationStrategy::Manual)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				// modify genesis to produce different parachain header than for parachain A
				.with_genesis_overrides(json!({
					"sudo": {
						"key": "5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty"
					}
				}))
				.with_collator(|n| {
					n.with_name("eve").with_args(vec![("-lparachain=debug").into()]).bootnode(true)
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
		})?;

	Ok(config)
}
