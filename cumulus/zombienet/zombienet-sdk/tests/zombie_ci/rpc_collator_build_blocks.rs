// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use anyhow::anyhow;
use std::time::Duration;

use crate::utils::{initialize_network, BEST_BLOCK_METRIC};

use cumulus_zombienet_sdk_helpers::assert_para_throughput;
use polkadot_primitives::Id as ParaId;
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfig, NetworkConfigBuilder,
};

const PARA_ID: u32 = 2000;

// This tests makes sure that RPC collator is able to build blocks
#[tokio::test(flavor = "multi_thread")]
async fn rpc_collator_builds_blocks() -> Result<(), anyhow::Error> {
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
	let eve = network.get_node("eve")?;
	for (node, timeout_secs) in [(eve, 250u64), (dave, 250u64)] {
		log::info!("Ensuring {} reports expected block height", node.name());
		assert!(node
			.wait_metric_with_timeout(BEST_BLOCK_METRIC, |b| b >= 12.0, timeout_secs)
			.await
			.is_ok());
	}

	log::info!("Restaring 'one' after 1 second");
	network.get_node("one")?.restart(Some(Duration::from_secs(1))).await?;

	log::info!("Ensuring dave reports expected block height");
	let dave = network.get_node("dave")?;
	assert!(dave
		.wait_metric_with_timeout(BEST_BLOCK_METRIC, |b| b >= 20.0, 200u64)
		.await
		.is_ok());

	log::info!("Restaring 'two' after 2 seconds");
	network.get_node("two")?.restart(Some(Duration::from_secs(1))).await?;

	log::info!("Restaring 'three' after 20 seconds");
	network.get_node("three")?.restart(Some(Duration::from_secs(20))).await?;

	log::info!("Checking if dave is up");
	assert!(dave.wait_until_is_up(10u64).await.is_ok());

	log::info!("Ensuring dave reports expected block height");
	assert!(dave
		.wait_metric_with_timeout(BEST_BLOCK_METRIC, |b| b >= 30.0, 200u64)
		.await
		.is_ok());

	// We want to make sure that none of the consensus hook checks fail, even if the chain makes
	// progress. If below log line occurred 1 or more times then test failed.
	for node in [eve, dave] {
		log::info!("Ensuring none of the consensus hook checks fail at {}", node.name());
		let result = node
			.wait_log_line_count_with_timeout(
				"set_validation_data inherent needs to be present in every block",
				false,
				LogLineCountOptions::no_occurences_within_timeout(Duration::from_secs(10)),
			)
			.await?;

		assert!(result.success(), "Consensus hook failed at {}: {:?}", node.name(), result);
	}

	Ok(())
}

// This test makes sure that an RPC collator that had to fail over to another relay chain RPC node
// switches back to the first one in its list once that node is available again.
#[tokio::test(flavor = "multi_thread")]
async fn rpc_collator_switches_back_to_preferred_rpc_node() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	log::info!("Spawning network");
	let config = build_network_config().await?;
	let network = initialize_network(config).await?;

	let dave = network.get_node("dave")?;
	let eve = network.get_node("eve")?;
	for node in [dave, eve] {
		log::info!("Ensuring {} reports expected block height", node.name());
		assert!(node
			.wait_metric_with_timeout(BEST_BLOCK_METRIC, |b| b >= 5.0, 250u64)
			.await
			.is_ok());
	}

	// 'one' is the first relay chain RPC node of both collators, so they fail over to 'two'.
	log::info!("Restarting 'one'");
	network.get_node("one")?.restart(None).await?;

	// The collators check for their preferred node every 5 minutes, allow for two checks in case
	// 'one' has not caught up by the first one.
	for node in [dave, eve] {
		log::info!("Ensuring {} switches back to 'one'", node.name());
		let result = node
			.wait_log_line_count_with_timeout(
				"Switched back to preferred external relay chain node",
				false,
				LogLineCountOptions::new(|n| n >= 1, Duration::from_secs(11 * 60), false),
			)
			.await?;
		assert!(result.success(), "{} did not switch back to 'one': {:?}", node.name(), result);
	}

	log::info!("Ensuring dave keeps producing blocks");
	let best = dave.reports(BEST_BLOCK_METRIC).await?;
	assert!(dave
		.wait_metric_with_timeout(BEST_BLOCK_METRIC, move |b| b >= best + 5.0, 200u64)
		.await
		.is_ok());

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
	// 	 - charlie - validator
	// 	 - one     - full node
	// 	 - two     - full node
	// 	 - three   - full node
	// - parachain nodes
	//   - dave    - validator; gets relay chain data only from full nodes (which are bootnodes too)
	//   - eve     - validator; gets relay chain data only from full nodes (which are bootnodes too)
	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![("-lparachain=debug").into()])
				.with_validator(|node| node.with_name("alice"))
				.with_validator(|node| node.with_name("bob"))
				.with_validator(|node| node.with_name("charlie"))
				.with_validator(|node| node.with_name("one").validator(false))
				.with_validator(|node| node.with_name("two").validator(false))
				.with_validator(|node| node.with_name("three").validator(false))
		})
		.with_parachain(|p| {
			p.with_id(PARA_ID)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_default_args(vec![
					("-lparachain=trace,blockchain-rpc-client=debug").into(),
					(
						"--bootnodes",
						vec![
							"{{ZOMBIE:one:multiaddr}}",
							"{{ZOMBIE:two:multiaddr}}",
							"{{ZOMBIE:three:multiaddr}}",
						],
					)
						.into(),
					(
						"--relay-chain-rpc-url",
						vec![
							"{{ZOMBIE:one:ws_uri}}",
							"{{ZOMBIE:two:ws_uri}}",
							"{{ZOMBIE:three:ws_uri}}",
						],
					)
						.into(),
				])
				.with_collator(|n| n.with_name("dave"))
				.with_collator(|n| n.with_name("eve"))
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
