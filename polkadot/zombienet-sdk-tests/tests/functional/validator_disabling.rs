// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Test checks that misbehaving validators disabled.
use crate::utils::maybe_enable_experimental_collator_protocol;
use anyhow::anyhow;
use codec::Decode;
use cumulus_zombienet_sdk_helpers::assert_para_throughput;
use polkadot_primitives::{
	BlockNumber, CandidateHash, DisputeState, SessionIndex, ValidatorId, ValidatorIndex,
};
use serde_json::json;
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfigBuilder,
};

#[tokio::test(flavor = "multi_thread")]
async fn validator_disabling_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);
	let images = zombienet_sdk::environment::get_images_from_env();
	let config_builder = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("westend-local") // Use westend-local so the disabling can take effect.
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(maybe_enable_experimental_collator_protocol(vec![
					("-lparachain=debug").into(),
				]))
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"group_rotation_frequency": 10,
								"max_validators_per_core": 1
							},
							"needed_approvals": 2,
						}
					}
				}))
				// Adding malicious validator.
				.with_validator(|node| {
					node.with_name("malus-validator")
						.with_image(
							std::env::var("MALUS_IMAGE")
								.unwrap_or("docker.io/paritypr/malus".to_string())
								.as_str(),
						)
						.with_command("malus")
						.with_subcommand("suggest-garbage-candidate")
						.with_args(vec![
							"-lMALUS=trace".into(),
							// Without this the malus validator won't run on macOS.
							"--insecure-validator-i-know-what-i-do".into(),
						])
						// Make it vulenrable so disabling really happens
						.invulnerable(false)
				});
			// Also honest validators.
			let r = (0..3).fold(r, |acc, i| {
				acc.with_validator(|node| {
					node.with_name(&format!("honest-validator-{i}"))
						.with_args(vec![("-lparachain=debug,runtime::staking=debug".into())])
						.invulnerable(false)
				})
			});
			r
		})
		.with_parachain(|p| {
			p.with_id(1000)
				.with_default_command("adder-collator")
				.cumulus_based(false)
				.with_default_image(images.cumulus.as_str())
				.with_default_args(vec!["-lparachain=debug".into()])
				.with_collator(|n| n.with_name("alice"))
		})
		.with_global_settings(|global_settings| global_settings.with_tear_down_on_failure(false))
		.build()
		.map_err(|e| {
			let errors = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errors: {errors}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	log::info!("Spawning network");
	let network = spawn_fn(config_builder).await?;

	log::info!("Waiting for parablocks to be produced");
	let honest_validator = network.get_node("honest-validator-0")?;
	let relay_client: OnlineClient<PolkadotConfig> = honest_validator.wait_client().await?;
	assert_para_throughput(&relay_client, 20, [(polkadot_primitives::Id::from(1000), 10..30)], [])
		.await?;

	log::info!("Wait for a dispute to be initialized.");
	let mut best_blocks = relay_client.blocks().subscribe_best().await?;
	let mut dispute_session: u32 = u32::MAX;
	let mut block_hash = None;
	// Check next new block from the current best fork
	while let Some(block) = best_blocks.next().await {
		let current_hash = block?.hash();
		let disputes = Vec::<(SessionIndex, CandidateHash, DisputeState<BlockNumber>)>::decode(
			&mut &relay_client
				.runtime_api()
				.at(current_hash)
				.call_raw("ParachainHost_disputes", None)
				.await?[..],
		)?;
		if let Some((session, _, _)) = disputes.first() {
			block_hash = Some(current_hash);
			dispute_session = *session;
			break;
		}
	}

	assert_ne!(dispute_session, u32::MAX);
	log::info!("Dispute initiated.");
	let concluded_dispute_metric =
		"polkadot_parachain_candidate_dispute_concluded{validity=\"invalid\"}";
	let parachain_candidate_dispute_metric = "parachain_candidate_disputes_total";
	// Check that we have at least one dispute
	honest_validator
		.wait_metric_with_timeout(parachain_candidate_dispute_metric, |d| d >= 1.0, 600_u64)
		.await?;
	// Check that we have at least one concluded dispute.
	honest_validator
		.wait_metric_with_timeout(concluded_dispute_metric, |d| d >= 1.0, 200_u64)
		.await?;

	let disabled_validators = Vec::<ValidatorIndex>::decode(
		&mut &relay_client
			.runtime_api()
			.at(block_hash.unwrap())
			.call_raw("ParachainHost_disabled_validators", None)
			.await?[..],
	)?;
	// We should have at least one disable validator.
	assert!(!disabled_validators.is_empty());

	let session_validators = Vec::<ValidatorId>::decode(
		&mut &relay_client
			.runtime_api()
			.at(block_hash.unwrap())
			.call_raw("ParachainHost_validators", None)
			.await?[..],
	)?;
	// We have a single malicious node, hence the index of the malus-node is the first
	// entry in the disabled validators list.
	let disabled_node_public_address = &session_validators[(disabled_validators[0].0) as usize];

	let disabled_node_public_key_hex = disabled_node_public_address
		.clone()
		.into_inner()
		.0
		.iter()
		.map(|byte| format!("{byte:02x}"))
		.collect::<String>();

	let json_value: serde_json::Value =
		serde_json::to_value(network.get_node("malus-validator")?.spec())?;
	let malus_public_address =
		json_value.pointer("/accounts/accounts/sr/public_key").unwrap().to_string();

	assert_eq!(disabled_node_public_key_hex, malus_public_address.trim_matches('"'));
	Ok(())
}
