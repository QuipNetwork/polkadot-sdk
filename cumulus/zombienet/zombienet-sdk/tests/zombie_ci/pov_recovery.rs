// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use anyhow::anyhow;

use crate::utils::{initialize_network, BEST_BLOCK_METRIC};

use cumulus_zombienet_sdk_helpers::{assert_para_is_registered, assert_para_throughput};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use zombienet_configuration::types::Arg;
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{
	environment::Provider,
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfig, NetworkConfigBuilder, RegistrationStrategy,
};

const PARA_ID: u32 = 2000;

// This tests makes sure that the recovering nodes are able to see all relay-chain
// notifications containing the candidates to recover.
#[tokio::test(flavor = "multi_thread")]
async fn pov_recovery() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	log::info!("Spawning network with relay chain only");
	let config = build_network_config().await?;
	let mut network = initialize_network(config).await?;

	log::info!("Checking if network nodes are up");
	let result = network.wait_until_is_up(200u64).await;
	assert!(result.is_ok(), "Network is not up: {:?}", result.unwrap_err());

	let validator_3 = network.get_node("validator-3")?;

	log::info!("Waiting 20 blocks to register parachain");
	// Wait 20 blocks and register parachain. This part is important for pov-recovery.
	assert!(validator_3
		.wait_metric_with_timeout(BEST_BLOCK_METRIC, |b| b >= 20.0, 250u64)
		.await
		.is_ok());

	log::info!("Registering parachain para_id = {PARA_ID}");
	network.register_parachain(PARA_ID).await?;

	let validator = network.get_node("validator-0")?;
	let validator_client: OnlineClient<PolkadotConfig> = validator.wait_client().await?;

	log::info!("Ensuring parachain is registered within 30 blocks");
	assert_para_is_registered(&validator_client, ParaId::from(PARA_ID), 30).await?;

	log::info!("Ensuring parachain making progress");
	assert_para_throughput(&validator_client, 20, [(ParaId::from(PARA_ID), 2..20)], []).await?;

	for (name, timeout_secs) in [("bob", 600u64)] {
		log::info!("Checking block production for {name} within {timeout_secs}s");
		assert!(network
			.get_node(name)?
			.wait_metric_with_timeout(BEST_BLOCK_METRIC, |b| b >= 20.0, timeout_secs)
			.await
			.is_ok());
	}

	for (name, timeout_secs) in
		[("alice", 600u64), ("charlie", 600u64), ("one", 800u64), ("two", 800u64), ("eve", 800u64)]
	{
		// Wait (up to timeout_secs) until pattern occurs at least 20 times
		let options = LogLineCountOptions {
			predicate: Arc::new(|n| n >= 20),
			timeout: Duration::from_secs(timeout_secs),
			wait_until_timeout_elapses: false,
		};

		log::info!("Ensuring blocks are imported using PoV recovery by {name}");
		let result = network
			.get_node(name)?
			.wait_log_line_count_with_timeout(
				"Importing blocks retrieved using pov_recovery",
				false,
				options.clone(),
			)
			.await?;

		assert!(
			result.success(),
			"Failed importing blocks using PoV recovery by {name}: {result:?}"
		);
	}

	Ok(())
}

async fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	// images are not relevant for `native`, but we leave it here in case we use `k8s` some day
	let images = zombienet_sdk::environment::get_images_from_env();
	log::info!("Using images: {images:?}");

	// If all nodes running on one machine and there are too much of them,
	// then they don't get enough CPU time and others might fail trying to connect to them.
	// eg. 'one' and 'two' trying to connect to validators rpc but it is still initializing.
	let validator_cnt = match zombienet_sdk::environment::get_provider_from_env() {
		Provider::K8s => 13,
		_ => 5,
	};

	// Provide multiple RPC urls to increase a chance that some node behind url
	// will be have RPC server up and running.
	let mut rpc_urls = vec![];
	rpc_urls.extend((0..validator_cnt).map(|i| format!("{{{{ZOMBIE:validator-{i}:ws_uri}}}}")));

	// Network setup:
	// - relaychain nodes:
	// 	 - validator[0-validator_cnt]
	// 	   - validator
	// 	   - synchronize only with validator-0
	// - parachain nodes
	//   - bob
	//     - collator which produces blocks, but doesn't announce them
	//   - alice
	//     - collator which produces blocks, but doesn't announce them
	//     - will need to recover the pov blocks through availability recovery
	//   - charlie
	//     - full node
	//     - will need to recover the pov blocks through availability recovery
	//   - eve
	//     - collator which produces blocks, but doesn't announce them
	//     - it fails recovery from time to time to test retries
	//   - one
	//     - RPC collator which produces blocks, but doesn't announce them
	//     - Uses an external relay chain node
	//   - two
	//     - RPC full node
	//     - Uses an external relay chain node
	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_genesis_overrides(json!({
						"configuration": {
							"config": {
								"scheduler_params": {
									"max_validators_per_core": 1,
									"group_rotation_frequency": 100
								}
							}
						}
				}))
				.with_validator(|node| {
					node.with_name("validator-0").validator(true).with_args(vec![
						("-lparachain::availability=trace,sync=info,parachain=debug,libp2p_mdns=debug,info").into(),
					])
				});

			(1..validator_cnt).fold(r, |acc, i| {
				acc.with_validator(|node| {
					node.with_name(&format!("validator-{i}")).with_args(vec![
						("-lparachain::availability=trace,sync=debug,parachain=debug,libp2p_mdns=debug").into(),
						("--reserved-only").into(),
						("--reserved-nodes", "{{ZOMBIE:validator-0:multiaddr}}").into(),
					])
				})
			})
		})
		.with_parachain(|p| {
			p.with_id(PARA_ID)
				.with_registration_strategy(RegistrationStrategy::Manual)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_collator(|c| {
					c.with_name("bob")
						.validator(true)
						.with_args(vec![
							("--disable-block-announcements").into(),
							("-lparachain::availability=trace,sync=debug,parachain=debug,cumulus-pov-recovery=debug,cumulus-consensus=debug,libp2p_mdns=debug,info").into(),
						])
				})
				.with_collator(|c| {
					c.with_name("alice")
						.validator(true)
						.with_args(build_collator_args(vec![]))
				})
				.with_collator(|c| {
					c.with_name("charlie").validator(false).with_args(build_collator_args(vec![]))
				})
				.with_collator(|c| {
					c.with_name("eve").validator(true).with_args(build_collator_args(vec![
						"--fail-pov-recovery".into()
					]))
				})
				.with_collator(|c| {
					c.with_name("one").validator(true).with_args(build_collator_args(vec![
						("--relay-chain-rpc-url", rpc_urls.clone()).into()
					]))
				})
				.with_collator(|c| {
					c.with_name("two").validator(false).with_args(build_collator_args(vec![
						("--relay-chain-rpc-url", rpc_urls).into()
					]))
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

fn build_collator_args(in_args: Vec<Arg>) -> Vec<Arg> {
	let start_args: Vec<Arg> = vec![
		("-lparachain::availability=trace,sync=debug,parachain=debug,cumulus-pov-recovery=debug,cumulus-consensus=debug,libp2p_mdns=debug,info").into(),
		("--disable-block-announcements").into(),
		("--in-peers=0").into(),
		("--out-peers=0").into(),
		("--bootnodes", "{{ZOMBIE:bob:multiaddr}}").into(),
	];

	let remaining_args: Vec<Arg> = vec![
		("--").into(),
		("--reserved-only").into(),
		("--reserved-nodes", "{{ZOMBIE:validator-0:multiaddr}}").into(),
	];

	[start_args, in_args, remaining_args].concat()
}
