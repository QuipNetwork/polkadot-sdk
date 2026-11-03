// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Coretime Smoke Test
//!
//! This test verifies that the coretime/broker chain can properly configure
//! and onboard parachains using bulk core assignment.
//!
//! The test sets up:
//! - A relay chain with 3 validators
//! - A coretime chain (parachain 1005)
//! - A test parachain (parachain 100) that starts unregistered
//!
//! It then configures the relay and broker chains to onboard parachain 100
//! and verifies it starts producing blocks.

use crate::utils::{env_or_default, initialize_network, CUMULUS_IMAGE_ENV, INTEGRATION_IMAGE_ENV};
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::{
	assert_para_throughput, submit_extrinsic_and_wait_for_finalization_success,
};
use polkadot_primitives::Id as ParaId;
use zombienet_sdk::{
	subxt::{
		dynamic::Value, ext::scale_value::value, tx::DynamicPayload, OnlineClient, PolkadotConfig,
	},
	subxt_signer::sr25519::dev,
	tx_helper::parachain::{fetch_genesis_header, fetch_validation_code},
	NetworkConfig, NetworkConfigBuilder, RegistrationStrategy,
};

use serde_json::json;

const CORETIME_PARA_ID: u32 = 1005;
const TEST_PARA_ID: u32 = 100;

/// Coretime smoke test that verifies bulk core assignment and parachain onboarding.
///
/// - Configures relay chain with coretime cores and registers parachain 100
/// - Waits for coretime chain to produce blocks
/// - Configures broker chain with leases
/// - Verifies parachain 100 produces blocks
#[tokio::test(flavor = "multi_thread")]
async fn coretime_smoke_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let config = build_network_config()?;
	let network = initialize_network(config).await?;

	let alice = network.get_node("alice")?;
	let coretime_node = network.get_node("coretime-collator")?;
	let para_100_node = network.get_node("collator-para-100")?;

	let alice_client: OnlineClient<PolkadotConfig> = alice.wait_client().await?;
	let coretime_client: OnlineClient<PolkadotConfig> = coretime_node.wait_client().await?;
	let para_100_client: OnlineClient<PolkadotConfig> = para_100_node.wait_client().await?;

	// Get genesis header and validation code for parachain 100
	log::info!("Fetching genesis header and validation code for parachain {}", TEST_PARA_ID);
	let genesis_header = fetch_genesis_header(&para_100_client).await?;
	let validation_code = fetch_validation_code(&para_100_client).await?;

	log::info!(
		"Genesis header: {} bytes, Validation code: {} bytes",
		genesis_header.len(),
		validation_code.len()
	);

	// Configure relay chain
	log::info!("Configuring relay chain");
	let alice_account = Value::from_bytes(dev::alice().public_key().0);
	let configure_relay_call =
		create_configure_relay_call(genesis_header, validation_code, TEST_PARA_ID, alice_account);
	submit_extrinsic_and_wait_for_finalization_success(
		&alice_client,
		&configure_relay_call,
		&dev::alice(),
	)
	.await?;
	log::info!("Relay chain configured");

	// Wait for coretime chain to produce blocks
	log::info!("Waiting for coretime chain to produce blocks");
	assert_para_throughput(&alice_client, 30, [(ParaId::from(CORETIME_PARA_ID), 5..31)], [])
		.await?;
	log::info!("Coretime chain is producing blocks");

	// Configure broker chain
	log::info!("Configuring broker chain");
	let configure_broker_call = create_configure_broker_call(TEST_PARA_ID);
	submit_extrinsic_and_wait_for_finalization_success(
		&coretime_client,
		&configure_broker_call,
		&dev::alice(),
	)
	.await?;
	log::info!("Broker chain configured");

	// Wait for parachain 100  produce blocks
	log::info!("Waiting for parachain {} to produce blocks", TEST_PARA_ID);
	assert_para_throughput(
		&alice_client,
		30,
		[(ParaId::from(CORETIME_PARA_ID), 5..31), (ParaId::from(TEST_PARA_ID), 5..31)],
		[],
	)
	.await?;
	log::info!("Parachain {} is producing blocks", TEST_PARA_ID);

	log::info!("Test finished successfully");
	Ok(())
}

fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let polkadot_image = env_or_default(INTEGRATION_IMAGE_ENV, images.polkadot.as_str());
	let cumulus_image = env_or_default(CUMULUS_IMAGE_ENV, images.cumulus.as_str());

	let alice_account_str = dev::alice().public_key().to_account_id().to_string();

	NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("westend-local")
				.with_default_command("polkadot")
				.with_default_image(polkadot_image.as_str())
				.with_validator(|node| {
					node.with_name("alice")
						// Set Alice's initial balance to 200 WND, the original value before the
						// removal of the staking pallet from the RC.
						.with_initial_balance(200_000_000_000_000u128)
						.with_args(vec![("-lruntime=debug,xcm=trace").into()])
				})
				.with_validator(|node| {
					node.with_name("bob")
						.with_args(vec![("-lruntime=debug,parachain=trace").into()])
				})
				.with_validator(|node| {
					node.with_name("charlie")
						.with_args(vec![("-lruntime=debug,parachain=trace").into()])
				})
		})
		.with_parachain(|p| {
			p.with_id(CORETIME_PARA_ID)
				.with_default_command("polkadot-parachain")
				.with_default_image(cumulus_image.as_str())
				.with_chain("coretime-westend-local")
				// Add sudo key
				.with_genesis_overrides(json!({
					"sudo": { "key": alice_account_str }
				}))
				.with_collator(|n| {
					n.with_name("coretime-collator")
						.with_args(vec![("-lruntime=debug,xcm=trace").into()])
				})
		})
		.with_parachain(|p| {
			p.with_id(TEST_PARA_ID)
				.with_registration_strategy(RegistrationStrategy::Manual)
				.with_default_command("test-parachain")
				.with_default_image(cumulus_image.as_str())
				.with_collator(|n| {
					n.with_name("collator-para-100").with_args(vec![
						("-lruntime=debug,parachain=trace,aura=trace").into(),
						("--force-authoring").into(),
					])
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
		})
}

/// Creates a sudo batch call to configure the relay chain for coretime
fn create_configure_relay_call(
	genesis_header: Vec<u8>,
	validation_code: Vec<u8>,
	para_id: u32,
	registrar_account: Value,
) -> DynamicPayload {
	let genesis_head_value = Value::from_bytes(&genesis_header);
	let validation_code_value = Value::from_bytes(&validation_code);

	let set_coretime_cores_call = value! {
		Configuration(set_coretime_cores { new: 1u32 })
	};

	let assign_core_call = value! {
		Coretime(assign_core { core: 0u32, begin: 20u32, assignment: ((Task(1005u32), 57600u16)), end_hint: None() })
	};

	let force_register_call = value! {
		Registrar(force_register { who: registrar_account, deposit: 0u128, id: para_id, genesis_head: genesis_head_value, validation_code: validation_code_value })
	};

	let calls = vec![set_coretime_cores_call, assign_core_call, force_register_call];

	zombienet_sdk::subxt::tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			Utility(batch { calls: calls })
		}],
	)
}

/// Creates a sudo batch call to configure the broker chain
fn create_configure_broker_call(para_id: u32) -> DynamicPayload {
	let config_value = value! {
		{
			advance_notice: 5u32,
			interlude_length: 1u32,
			leadin_length: 1u32,
			region_length: 1u32,
			ideal_bulk_proportion: 100u32,
			limit_cores_offered: None(),
			renewal_bump: 10u32,
			contribution_timeout: 5u32
		}
	};

	let configure_call = value! {
		Broker(configure { config: config_value })
	};

	let request_core_count_call = value! {
		Broker(request_core_count { core_count: 2u16 })
	};

	let set_lease_coretime_call = value! {
		Broker(set_lease { task: 1005u32, until: 1000u32 })
	};

	let set_lease_para_call = value! {
		Broker(set_lease { task: para_id, until: 1000u32 })
	};

	let start_sales_call = value! {
		Broker(start_sales { end_price: 1u128, extra_cores: 0u16 })
	};

	let calls = vec![
		configure_call,
		request_core_count_call,
		set_lease_coretime_call,
		set_lease_para_call,
		start_sales_call,
	];

	zombienet_sdk::subxt::tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			Utility(batch { calls: calls })
		}],
	)
}
