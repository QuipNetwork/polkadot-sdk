// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Test that a parachain can keep producing blocks even if the other parachain with which it's
// sharing a core doesn't

use crate::utils::maybe_enable_experimental_collator_protocol;
use anyhow::anyhow;

use cumulus_zombienet_sdk_helpers::{assert_finality_lag, assert_para_throughput};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use zombienet_sdk::{
	subxt::{self, ext::scale_value::value, OnlineClient, PolkadotConfig},
	subxt_signer::sr25519::dev,
	NetworkConfigBuilder,
};

#[tokio::test(flavor = "multi_thread")]
async fn shared_core_idle_parachain_test() -> Result<(), anyhow::Error> {
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
				.with_default_args(maybe_enable_experimental_collator_protocol(vec![
					("-lparachain=debug").into(),
				]))
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"num_cores": 4,
								"group_rotation_frequency": 4
							}
						}
					}
				}))
				.with_validator(|node| node.with_name("validator-0"));

			(1..4).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-{i}")))
			})
		})
		.with_parachain(|p| {
			p.with_id(2000)
				// Don't onboard as parachain, as this would automatically add one more core and
				// assign it to the para.
				.onboard_as_parachain(false)
				.with_default_command("polkadot-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_default_args(vec![
					("-lparachain=debug,aura=debug").into(),
					"--authoring=slot-based".into(),
				])
				.with_collator(|n| n.with_name("collator-2000"))
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_node = network.get_node("validator-0")?;
	let para_node_2000 = network.get_node("collator-2000")?;

	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;
	let alice = dev::alice();

	// Assign core 0 to be shared between paraid 2000 and another, non-existant paraid 2001.
	let assign_core_call = subxt::tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			Coretime(assign_core { core: 0, begin: 0, assignment: ((Task(2000), 28800), (Task(2001), 28800)), end_hint: None() })
		}],
	);
	relay_client
		.tx()
		.sign_and_submit_then_watch_default(&assign_core_call, &alice)
		.await?
		.wait_for_finalized_success()
		.await?;

	log::info!("Assigned core 0 to be shared between paras");

	// Check that para 2000 is essentially getting 12-second block time, while para 2001 does not
	// produce anything.
	assert_para_throughput(&relay_client, 15, [(ParaId::from(2000), 5..9)], []).await?;

	assert_finality_lag(&para_node_2000.wait_client().await?, 5).await?;

	log::info!("Test finished successfully");

	Ok(())
}
