// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use crate::utils::initialize_network;
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::{assert_para_throughput, assign_cores};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfig, NetworkConfigBuilder,
};

const PARA_ID: u32 = 2100;

/// This test checks if parachain node is importing blocks using PoV recovery even
/// after more cores have been assigned for the parachain.
#[tokio::test(flavor = "multi_thread")]
async fn block_bundling_pov_recovery() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	log::info!("Spawning network with relay chain only");
	let config = build_network_config().await?;
	let network = initialize_network(config).await?;

	let collator = network.get_node("collator")?;
	collator.pause().await?;

	let recovery_target = network.get_node("recovery-target")?;

	// Wait for the node to be ready. We have the collator in between paused, this ensures that it
	// doesn't produce any blocks in between. This is important as the recovery node needs to be up
	// to observe the candidates on the relay chain, to recover them.
	recovery_target.wait_until_is_up(120u64).await?;

	collator.resume().await?;

	let alice = network.get_node("alice")?;

	let relay_client: OnlineClient<PolkadotConfig> = alice.wait_client().await?;

	assign_cores(&relay_client, PARA_ID, vec![0, 1]).await?;

	log::info!("Ensuring parachain making progress");
	assert_para_throughput(&relay_client, 20, [(ParaId::from(PARA_ID), 40..65)], []).await?;

	// We want to make sure that none of the consensus hook checks fail, even if the chain makes
	// progress. If below log line occurred 1 or more times then test failed.
	log::info!("Ensuring none of the consensus hook checks fail at {}", collator.name());
	let result = collator
		.wait_log_line_count_with_timeout(
			"set_validation_data inherent needs to be present in every block",
			false,
			LogLineCountOptions::no_occurences_within_timeout(Duration::from_secs(10)),
		)
		.await?;

	if !result.success() {
		return Err(anyhow!("Consensus hook failed at {}: {:?}", collator.name(), result));
	}

	// Wait (up to 10 seconds) until pattern occurs more than 35 times
	let options = LogLineCountOptions {
		predicate: Arc::new(|n| n > 35),
		timeout: Duration::from_secs(10),
		wait_until_timeout_elapses: false,
	};

	log::info!("Ensuring blocks are imported using PoV recovery by {}", recovery_target.name());
	let result = recovery_target
		.wait_log_line_count_with_timeout(
			"Importing blocks retrieved using pov_recovery",
			false,
			options,
		)
		.await?;

	if !result.success() {
		return Err(anyhow!(
			"Failed importing blocks using PoV recovery by {}: {result:?}",
			recovery_target.name()
		));
	}

	log::info!("Test finished successfully");
	Ok(())
}

async fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	// images are not relevant for `native`, but we leave it here in case we use `k8s` some day
	let images = zombienet_sdk::environment::get_images_from_env();
	log::info!("Using images: {images:?}");

	// Network setup:
	// - relaychain nodes:
	// 	 - alice
	// 	   - validator
	// 	 - validator[0-3]
	// 	   - validator
	// 	   - synchronize only with alice
	// - parachain nodes
	//   - recovery-target
	//     - full node
	//   - collator-elastic
	//     - collator which is the only one producing blocks
	NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_resources(|resources| {
					// These settings are applicable only for `k8s` provider.
					// Leaving them in case we switch to `k8s` some day.
					resources
						.with_request_cpu(1)
						.with_request_memory("2G")
						.with_limit_cpu(2)
						.with_limit_memory("4G")
				})
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"num_cores": 3,
								"max_validators_per_core": 1
							},
							"approval_voting_params": {
								"max_approval_coalesce_count": 5
							}
						}
					}
				}))
				// Have to set a `with_validator` outside of the loop below, so that `r` has the right
				// type.
				.with_validator(|node| node.with_name("alice").with_args(vec![]));

			(0..4).fold(r, |acc, i| {
				acc.with_validator(|node| {
					node.with_name(&format!("validator-{i}")).with_args(vec![
						("-lruntime=debug,parachain=trace").into(),
						("--reserved-only").into(),
						("--reserved-nodes", "{{ZOMBIE:alice:multiaddr}}").into(),
					])
				})
			})
		})
		.with_parachain(|p| {
			p.with_id(PARA_ID)
				.with_chain("block-bundling")
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_default_resources(|resources| {
					// These settings are applicable only for `k8s` provider.
					// Leaving them in case we switch to `k8s` some day.
					resources
						.with_request_cpu(1)
						.with_request_memory("2G")
						.with_limit_cpu(2)
						.with_limit_memory("4G")
				})
				.with_fullnode(|n|
					n.with_name("recovery-target")
						.with_args(vec![
						("-lparachain::availability=trace,sync=debug,parachain=debug,cumulus-pov-recovery=debug,cumulus-consensus=debug").into(),
						("--disable-block-announcements").into(),
						("--in-peers", "0").into(),
						("--out-peers", "0").into(),
						("--").into(),
						("--reserved-only").into(),
						("--reserved-nodes", "{{ZOMBIE:alice:multiaddr}}").into()
					]))
				.with_collator(|n| n.with_name("collator")
					.with_args(vec![
						("--reserved-nodes", "{{ZOMBIE:alice:multiaddr}}").into(),
						("-laura=trace,runtime=info,cumulus-consensus=trace,consensus::common=trace,parachain::collation-generation=trace,parachain::collator-protocol=trace,parachain=debug").into(),
						("--disable-block-announcements").into(),
						("--force-authoring").into(),
						("--authoring", "slot-based").into()
					])
			)
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
