// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! PVF Precompilation Smoke Test
//!
//! This test verifies that PVF precompilation
//! behaves correctly when validators are added/removed from the validator set.
//!
//! Key behaviors tested:
//! - Non-validators should not prepare PVFs
//! - PVF preparation happens at the correct time relative to session changes
//!
//! The test flow:
//! 1. Start with 4 validators (alice, bob, charlie, dave) and a parachain
//! 2. Deregister dave from the validator set
//! 3. Register the parachain while dave is not a validator
//! 4. Verify dave didn't prepare the PVF (since he wasn't a validator)
//! 5. Re-register dave and verify PVF preparation timing

use crate::utils::{
	env_or_default, initialize_network, ACTIVE_VALIDATOR_METRIC, CUMULUS_IMAGE_ENV,
	INTEGRATION_IMAGE_ENV, PARACHAIN_VALIDATOR_METRIC,
};
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::{
	assert_para_throughput, submit_extrinsic_and_wait_for_finalization_success,
	wait_for_first_session_change, wait_for_nth_session_change,
};
use polkadot_primitives::Id as ParaId;
use zombienet_sdk::{
	subxt::{dynamic::Value, OnlineClient, PolkadotConfig},
	subxt_signer::{sr25519::dev, DeriveJunction},
	tx_helper::parachain::{
		create_deregister_validator_call, create_register_para_call,
		create_register_validator_call, fetch_genesis_header, fetch_validation_code,
	},
	NetworkConfig, NetworkConfigBuilder,
};

const PARA_ID: u32 = 2000;
const PVF_PREPARATION_COUNT_METRIC: &str = "polkadot_pvf_preparation_time_count";

/// PVF precompilation smoke test.
///
/// This test verifies that:
/// - Non-validators don't prepare PVFs
/// - PVF preparation happens at the correct time when validators join
#[tokio::test(flavor = "multi_thread")]
async fn precompile_pvf_smoke_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let images = zombienet_sdk::environment::get_images_from_env();
	log::info!("Using images: {:?}", images);

	let config = build_network_config()?;
	let network = initialize_network(config).await?;

	let alice_node = network.get_node("alice")?;
	let dave_node = network.get_node("dave")?;
	let para_node = network.get_node("collator-2000")?;

	let relay_client: OnlineClient<PolkadotConfig> = alice_node.wait_client().await?;
	let para_client: OnlineClient<PolkadotConfig> = para_node.wait_client().await?;

	let dave_stash = dev::dave().derive([DeriveJunction::hard("stash")]);
	let dave_stash_account = Value::from_bytes(dave_stash.public_key().0);
	let alice = dev::alice();
	let alice_account = Value::from_bytes(alice.public_key().0);

	// Dave should be in the validator set
	log::info!("Checking dave is in the validator set");
	dave_node
		.wait_metric_with_timeout(PARACHAIN_VALIDATOR_METRIC, |v| v == 1.0, 240u64)
		.await
		.map_err(|e| anyhow!("Dave is not a parachain validator: {}", e))?;
	dave_node
		.wait_metric_with_timeout(ACTIVE_VALIDATOR_METRIC, |v| v == 1.0, 240u64)
		.await
		.map_err(|e| anyhow!("Dave is not an active validator: {}", e))?;
	log::info!("Dave is in the validator set");

	// Deregister Dave
	log::info!("Deregistering dave");
	let deregister_call = create_deregister_validator_call(dave_stash_account.clone());
	submit_extrinsic_and_wait_for_finalization_success(&relay_client, &deregister_call, &alice)
		.await?;
	log::info!("Deregistration transaction finalized");

	// Wait 2 sessions for the authority set change to be enacted
	log::info!("Waiting for 2 session boundaries");
	let mut blocks_sub = relay_client.blocks().subscribe_finalized().await?;
	wait_for_nth_session_change(&mut blocks_sub, 2).await?;
	log::info!("Session boundaries passed");

	// Check Dave is NOT in the validator set
	log::info!("Checking dave is NOT in the validator set");
	dave_node
		.wait_metric_with_timeout(PARACHAIN_VALIDATOR_METRIC, |v| v == 0.0, 180u64)
		.await
		.map_err(|e| anyhow!("Dave is still a parachain validator: {}", e))?;
	dave_node
		.wait_metric_with_timeout(ACTIVE_VALIDATOR_METRIC, |v| v == 0.0, 180u64)
		.await
		.map_err(|e| anyhow!("Dave is still an active validator: {}", e))?;
	log::info!("Dave is NOT in the validator set");

	// Register the parachain while Dave is not a validator
	log::info!("Registering parachain {}", PARA_ID);
	let genesis_header = fetch_genesis_header(&para_client).await?;
	let validation_code = fetch_validation_code(&para_client).await?;
	log::info!(
		"Genesis header: {} bytes, Validation code: {} bytes",
		genesis_header.len(),
		validation_code.len()
	);

	let register_para_call =
		create_register_para_call(&genesis_header, &validation_code, PARA_ID, alice_account);
	submit_extrinsic_and_wait_for_finalization_success(&relay_client, &register_para_call, &alice)
		.await?;
	log::info!("Parachain registration transaction finalized");

	// Wait for parachain to  produce blocks
	log::info!("Waiting for parachain {} to be registered and produce blocks", PARA_ID);
	assert_para_throughput(&relay_client, 20, [(ParaId::from(PARA_ID), 5..21)], []).await?;
	log::info!("Parachain {} is producing blocks", PARA_ID);

	// Check Dave didn't prepare PVF
	// The metric should be 1 since dave shouldn't have prepared any PVFs
	log::info!("Checking dave didn't prepare PVF");
	dave_node
		.wait_metric_with_timeout(PVF_PREPARATION_COUNT_METRIC, |v| v == 1.0, 30u64)
		.await
		.map_err(|e| anyhow!("Unexpected PVF preparation count: {}", e))?;
	log::info!("Dave didn't prepare PVF");

	// Register Dave again
	log::info!("Registering dave again");
	let register_call = create_register_validator_call(dave_stash_account);
	submit_extrinsic_and_wait_for_finalization_success(&relay_client, &register_call, &alice)
		.await?;
	log::info!("Registration transaction finalized");

	// Wait 1 session and check PVF preparation count is still 1
	log::info!("Waiting for 1 session boundary");
	let mut blocks_sub = relay_client.blocks().subscribe_finalized().await?;
	wait_for_nth_session_change(&mut blocks_sub, 1).await?;

	// Check PVF preparation count is still 1
	log::info!("Checking PVF preparation count is still 1");
	dave_node
		.wait_metric_with_timeout(PVF_PREPARATION_COUNT_METRIC, |v| v == 1.0, 30u64)
		.await
		.map_err(|e| anyhow!("Unexpected PVF preparation count: {}", e))?;
	log::info!("PVF preparation count is still 1 (as expected)");

	// Check Dave is still not in the validator set
	log::info!("Checking dave is still NOT in the validator set");
	dave_node
		.wait_metric_with_timeout(PARACHAIN_VALIDATOR_METRIC, |v| v == 0.0, 30u64)
		.await
		.map_err(|e| anyhow!("Dave is already a parachain validator: {}", e))?;
	dave_node
		.wait_metric_with_timeout(ACTIVE_VALIDATOR_METRIC, |v| v == 0.0, 30u64)
		.await
		.map_err(|e| anyhow!("Dave is already an active validator: {}", e))?;
	log::info!("Dave is still NOT in the validator set (as expected)");

	// Wait for Dave to be back in the validator set
	log::info!("Waiting for dave to be back in the validator set");
	dave_node
		.wait_metric_with_timeout(PARACHAIN_VALIDATOR_METRIC, |v| v == 1.0, 60u64)
		.await
		.map_err(|e| anyhow!("Dave is not a parachain validator: {}", e))?;
	dave_node
		.wait_metric_with_timeout(ACTIVE_VALIDATOR_METRIC, |v| v == 1.0, 60u64)
		.await
		.map_err(|e| anyhow!("Dave is not an active validator: {}", e))?;
	log::info!("Dave is back in the validator set");

	// Wait 1 session and check PVF preparation count is still 1
	// Verifies that PVF was already prepared before Dave became active
	log::info!("Waiting for 1 session boundary");
	wait_for_first_session_change(&mut blocks_sub).await?;
	log::info!("Session boundaries passed");

	log::info!("Checking final PVF preparation count");
	dave_node
		.wait_metric_with_timeout(PVF_PREPARATION_COUNT_METRIC, |v| v == 1.0, 60u64)
		.await
		.map_err(|e| anyhow!("Unexpected PVF preparation count: {}", e))?;
	log::info!("PVF preparation count is still 1 (as expected)");

	log::info!("Test finished successfully");
	Ok(())
}

fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let polkadot_image = env_or_default(INTEGRATION_IMAGE_ENV, images.polkadot.as_str());
	let cumulus_image = env_or_default(CUMULUS_IMAGE_ENV, images.cumulus.as_str());

	NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(polkadot_image.as_str())
				.with_default_args(vec![("-lruntime=debug,parachain=trace").into()])
				.with_validator(|node| node.with_name("alice"))
				.with_validator(|node| node.with_name("bob"))
				.with_validator(|node| node.with_name("charlie"))
				.with_validator(|node| node.with_name("dave"))
		})
		.with_parachain(|p| {
			p.with_id(PARA_ID)
				.with_default_command("polkadot-parachain")
				.with_default_image(cumulus_image.as_str())
				.with_collator(|n| n.with_name("collator-2000"))
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
