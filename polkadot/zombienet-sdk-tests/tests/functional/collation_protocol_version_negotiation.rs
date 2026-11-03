// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Collation protocol version negotiation across a mixed validator set.
//!
//! Spawns 3 experimental validators (which advertise collation protocol V4) and 2 classic
//! validators (which cap at V3), all backing the same para, plus a single V4-capable collator.
//! The collator converts per peer, so we can assert both directions of the negotiation:
//!
//! - experimental validators negotiate collation V4 and receive V4 *segment* advertisements;
//! - classic validators negotiate collation V3 and receive classic *collation* advertisements.

use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::assert_para_throughput;
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use std::time::Duration;
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfigBuilder,
};

#[tokio::test(flavor = "multi_thread")]
async fn collation_protocol_version_negotiation() -> Result<(), anyhow::Error> {
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
					("-lparachain=debug,parachain::collator-protocol=trace".into()),
				])
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"max_validators_per_core": 5
							}
						}
					}
				}));
			let r = r.with_validator(|node| {
				node.with_name("validator-exp-0").with_args(vec![
					("-lparachain=debug,parachain::collator-protocol=trace").into(),
					("--experimental-collator-protocol").into(),
				])
			});
			let r = (1..3).fold(r, |acc, i| {
				acc.with_validator(|node| {
					node.with_name(&format!("validator-exp-{i}")).with_args(vec![
						("-lparachain=debug,parachain::collator-protocol=trace").into(),
						("--experimental-collator-protocol").into(),
					])
				})
			});
			// Classic validators — cap at collation protocol V3.
			(0..2).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-classic-{i}")))
			})
		})
		.with_parachain(|p| {
			p.with_id(2000)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_default_args(vec![
					("-lparachain=debug,aura=debug".into()),
					("--authoring", "slot-based").into(),
				])
				.with_collator(|node| node.with_name("collator-2000"))
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_client: OnlineClient<PolkadotConfig> =
		network.get_node("validator-exp-0")?.wait_client().await?;

	// Sanity: the para keeps making progress with the mixed validator set.
	assert_para_throughput(&relay_client, 10, [(ParaId::from(2000), 9..11)], []).await?;

	let opts = LogLineCountOptions::new(|n| n >= 1, Duration::from_secs(30), false);
	let absence_opts = LogLineCountOptions::new(|n| n == 0, Duration::from_secs(10), true);

	// Experimental validators negotiate V4 and receive segment advertisements.
	for i in 0..3 {
		let node = network.get_node(format!("validator-exp-{i}"))?;
		node.wait_log_line_count_with_timeout("peer_set=Collation version=4", false, opts.clone())
			.await?
			.success()
			.then_some(())
			.ok_or_else(|| anyhow!("validator-exp-{i} did not negotiate collation V4"))?;
		node.wait_log_line_count_with_timeout(
			"Received a segment advertisement",
			false,
			opts.clone(),
		)
		.await?
		.success()
		.then_some(())
		.ok_or_else(|| anyhow!("validator-exp-{i} did not receive a segment advertisement"))?;
	}

	// Classic validators negotiate V3 and receive classic collation advertisements.
	for i in 0..2 {
		let node = network.get_node(format!("validator-classic-{i}"))?;
		node.wait_log_line_count_with_timeout("peer_set=Collation version=3", false, opts.clone())
			.await?
			.success()
			.then_some(())
			.ok_or_else(|| anyhow!("validator-classic-{i} did not negotiate collation V3"))?;
		node.wait_log_line_count_with_timeout("Received advertise collation", false, opts.clone())
			.await?
			.success()
			.then_some(())
			.ok_or_else(|| {
				anyhow!("validator-classic-{i} did not receive a collation advertisement")
			})?;
		node.wait_log_line_count_with_timeout(
			"peer_set=Collation version=4",
			false,
			absence_opts.clone(),
		)
		.await?
		.success()
		.then_some(())
		.ok_or_else(|| anyhow!("validator-classic-{i} accepted collation V4"))?;
		node.wait_log_line_count_with_timeout(
			"Received a segment advertisement",
			false,
			absence_opts.clone(),
		)
		.await?
		.success()
		.then_some(())
		.ok_or_else(|| anyhow!("validator-classic-{i} received a segment advertisement"))?;
	}

	Ok(())
}
