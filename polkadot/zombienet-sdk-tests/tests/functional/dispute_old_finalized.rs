// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

// Test if disputes triggered on finalized blocks out of scope never get to be confirmed and
// concluded.

use crate::utils::maybe_enable_experimental_collator_protocol;
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::assert_para_throughput;
use serde_json::json;
use tokio::time::Duration;
use zombienet_orchestrator::network::node::LogLineCountOptions;

#[tokio::test(flavor = "multi_thread")]
async fn dispute_old_finalized() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);
	let images = zombienet_sdk::environment::get_images_from_env();
	let config = zombienet_sdk::NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(maybe_enable_experimental_collator_protocol(vec![
					("-lparachain=debug").into(),
				]))
				.with_genesis_overrides(json!({
					"patch": {
						"configuration" : {
							"config": {
								"needed_approvals": 1,
								"scheduler_params": {
									"max_validators_per_core": 1,
								}
							}
						}
					}
				}))
				.with_default_resources(|r| {
					r.with_limit_memory("4G")
						.with_limit_cpu("2")
						.with_request_memory("2G")
						.with_request_cpu("1")
				});
			// Add malus validator
			let r = r.with_validator(|node| {
				node.with_name("malus")
					.with_args(vec![
						"-lparachain=debug,MALUS=trace".into(),
						"--dispute-offset=14".into(),
						"--alice".into(),
						"--insecure-validator-i-know-what-i-do".into(),
					])
					.with_image(
						std::env::var("MALUS_IMAGE")
							.unwrap_or("docker.io/paritypr/malus".to_string())
							.as_str(),
					)
					.with_command("malus")
					.with_subcommand("dispute-finalized-candidates")
					.invulnerable(false)
			});
			// Add honest validators
			let r = (0..6).fold(r, |acc, i| {
				acc.with_validator(|node| {
					node.with_name(&format!("honest-{i}"))
						.with_args(vec!["-lparachain=debug".into()])
				})
			});
			r
		})
		.with_parachain(|p| {
			p.with_id(2000)
				.cumulus_based(false)
				.with_default_image(
					std::env::var("COL_IMAGE")
						.unwrap_or("docker.io/paritypr/colander:latest".to_string())
						.as_str(),
				)
				.with_default_command("undying-collator")
				.with_default_args(vec!["-lparachain=debug".into()])
				.with_collator(|n| n.with_name("collator"))
		})
		.build()
		.map_err(|e| {
			let errors = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errors: {errors}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let honest = network.get_node("honest-0")?;
	let relay_client = honest.wait_client().await?;
	let malus = network.get_node("malus")?;

	log::info!("Waiting for parablocks to be produced");
	assert_para_throughput(&relay_client, 20, [(polkadot_primitives::Id::from(2000), 10..30)], [])
		.await?;

	let result = malus
		.wait_log_line_count_with_timeout(
			"*😈 Disputing candidate with hash:*",
			true,
			LogLineCountOptions::new(|n| n == 1, Duration::from_secs(180_u64), false),
		)
		.await?;
	assert!(result.success());
	let result = honest
		.wait_log_line_count_with_timeout(
			"*Dispute on candidate concluded*",
			true,
			LogLineCountOptions::new(|n| n == 0, Duration::from_secs(180_u64), false),
		)
		.await?;
	assert!(result.success());
	Ok(())
}
