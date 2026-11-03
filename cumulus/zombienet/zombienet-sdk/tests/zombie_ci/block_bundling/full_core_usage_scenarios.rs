// This file is part of Cumulus.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::anyhow;
use cumulus_primitives_core::relay_chain::MAX_POV_SIZE;
use cumulus_zombienet_sdk_helpers::{
	assign_cores, ensure_is_last_block_in_core, ensure_is_only_block_in_core,
	submit_extrinsic_and_wait_for_finalization_success, BlockToCheck,
};
use frame_support::weights::constants::WEIGHT_REF_TIME_PER_SECOND;
use serde_json::json;
use zombienet_sdk::{
	subxt::{ext::scale_value::value, tx::DynamicPayload, OnlineClient, PolkadotConfig},
	subxt_signer::sr25519::dev,
	NetworkConfig, NetworkConfigBuilder,
};

const PARA_ID: u32 = 2400;

/// A test that sends transactions using `pallet-utility` `with_weight` through `pallet-sudo`.
///
/// This test starts with 3 cores assigned and sends two transactions:
/// 1. One with 1s ref_time
/// 2. One with a PoV size bigger than what one block alone is allowed to process.
/// Each transaction is sent after the other and waits for finalization.
#[tokio::test(flavor = "multi_thread")]
async fn block_bundling_full_core_usage_scenarios() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let config = build_network_config().await?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_node = network.get_node("validator-0")?;
	let para_node = network.get_node("collator-1")?;

	let para_client: OnlineClient<PolkadotConfig> = para_node.wait_client().await?;
	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;
	let alice = dev::alice();

	// Assign cores 0 and 1 to start with 3 cores total (core 2 is assigned by Zombienet)
	assign_cores(&relay_client, PARA_ID, vec![0, 1]).await?;

	// Create and send first transaction: 1s ref_time using utility.with_weight
	//
	// While we only should have 500ms available.
	let ref_time_1s = WEIGHT_REF_TIME_PER_SECOND;
	let first_call = create_utility_with_weight_call(ref_time_1s, 0);
	let sudo_first_call = create_sudo_call(first_call);

	log::info!("Testing scenario 1: Sending a transaction with 1s ref time weight usage");
	let block_hash =
		submit_extrinsic_and_wait_for_finalization_success(&para_client, &sudo_first_call, &alice)
			.await?;

	ensure_is_only_block_in_core(&para_client, BlockToCheck::Exact(block_hash)).await?;

	// Create a transaction that uses more than the allowed POV size per block.
	let pov_size = MAX_POV_SIZE / 4 + 512 * 1024;
	let second_call = create_utility_with_weight_call(0, pov_size as u64);
	let sudo_second_call = create_sudo_call(second_call);

	log::info!("Testing scenario 2: Sending a transaction with ~2.5MiB storage weight usage");
	let block_hash =
		submit_extrinsic_and_wait_for_finalization_success(&para_client, &sudo_second_call, &alice)
			.await?;

	ensure_is_only_block_in_core(&para_client, BlockToCheck::Exact(block_hash)).await?;

	let third_call = create_schedule_weight_registration_call();
	let sudo_third_call = create_sudo_call(third_call);

	log::info!("Testing scenario 5: Enabling `on_initialize` to use 1s ref time");
	let block_hash =
		submit_extrinsic_and_wait_for_finalization_success(&para_client, &sudo_third_call, &alice)
			.await?;

	ensure_is_only_block_in_core(&para_client, BlockToCheck::NextFirstBundleBlock(block_hash))
		.await?;

	let inherent_weight_call = create_set_inherent_weight_consume_call(ref_time_1s, 0);
	let sudo_inherent_weight_call = create_sudo_call(inherent_weight_call);

	log::info!("Testing scenario 4: Enabling an inherent that will use 1s ref time");
	let block_hash = submit_extrinsic_and_wait_for_finalization_success(
		&para_client,
		&sudo_inherent_weight_call,
		&alice,
	)
	.await?;

	// The next block should contain the consume_weight_inherent and consume the 1s ref_time
	ensure_is_only_block_in_core(&para_client, BlockToCheck::NextFirstBundleBlock(block_hash))
		.await?;

	let use_more_weight_than_announced = create_use_more_weight_than_announced_call(true);

	log::info!(
		"Testing scenario 5: Sending a transaction which uses more weight than what \
		it registered and transactions appears in the first block of a core"
	);
	let block_hash = submit_extrinsic_and_wait_for_finalization_success(
		&para_client,
		&use_more_weight_than_announced,
		&alice,
	)
	.await?;

	ensure_is_only_block_in_core(&para_client, BlockToCheck::Exact(block_hash)).await?;

	let use_more_weight_than_announced = create_use_more_weight_than_announced_call(false);

	// Here we are testing that a transaction that uses more weight than registered makes the block
	// production stop for this core. Even as the block is not the first block in the core.
	log::info!(
		"Testing scenario 6: Sending a transaction which uses more weight than what \
		it registered and transactions appears in the last block of a core"
	);
	let block_hash = submit_extrinsic_and_wait_for_finalization_success(
		&para_client,
		&use_more_weight_than_announced,
		&alice,
	)
	.await?;

	ensure_is_last_block_in_core(&para_client, block_hash).await?;

	Ok(())
}

/// Creates a `pallet-utility` `with_weight` call
fn create_utility_with_weight_call(ref_time: u64, proof_size: u64) -> DynamicPayload {
	// Create a simple remark call as the inner call
	let remark_data = vec![0u8; proof_size as usize]; // Fill with dummy data for PoV size
	let inner_call =
		zombienet_sdk::subxt::tx::dynamic("System", "remark", vec![value!(remark_data)]);

	// Create the weight struct
	let weight = value!({
		ref_time: ref_time,
		proof_size: proof_size
	});

	// Create the utility.with_weight call
	zombienet_sdk::subxt::tx::dynamic(
		"Utility",
		"with_weight",
		vec![inner_call.into_value(), weight],
	)
}

/// Creates a `pallet-sudo` `sudo` call wrapping the inner call
fn create_sudo_call(inner_call: DynamicPayload) -> DynamicPayload {
	zombienet_sdk::subxt::tx::dynamic("Sudo", "sudo", vec![inner_call.into_value()])
}

/// Creates a `test-pallet` `schedule_weight_registration` call
fn create_schedule_weight_registration_call() -> DynamicPayload {
	zombienet_sdk::subxt::tx::dynamic(
		"TestPallet",
		"schedule_weight_registration",
		vec![] as Vec<zombienet_sdk::subxt::ext::scale_value::Value>,
	)
}

/// Creates a `test-pallet` `use_more_weight_than_announced` call
fn create_use_more_weight_than_announced_call(must_be_first_block_in_core: bool) -> DynamicPayload {
	zombienet_sdk::subxt::tx::dynamic(
		"TestPallet",
		"use_more_weight_than_announced",
		vec![value![must_be_first_block_in_core]]
			as Vec<zombienet_sdk::subxt::ext::scale_value::Value>,
	)
}

/// Creates a `test-pallet` `set_inherent_weight_consume` call
fn create_set_inherent_weight_consume_call(ref_time: u64, proof_size: u64) -> DynamicPayload {
	let weight = value!({
		ref_time: ref_time,
		proof_size: proof_size
	});

	zombienet_sdk::subxt::tx::dynamic("TestPallet", "set_inherent_weight_consume", vec![weight])
}

async fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	log::info!("Using images: {images:?}");
	NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![("-lparachain=trace").into()])
				.with_default_resources(|resources| {
					resources.with_request_cpu(4).with_request_memory("4G")
				})
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"num_cores": 3,
								"max_validators_per_core": 1
							}
						}
					}
				}))
				.with_validator(|node| node.with_name("validator-0"));
			(1..9).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-{i}")))
			})
		})
		.with_parachain(|p| {
			p.with_id(PARA_ID)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain("block-bundling")
				.with_default_args(vec![
					("--authoring").into(),
					("slot-based").into(),
					("-lparachain=debug,aura=trace,runtime=trace").into(),
				])
				.with_collator(|n| n.with_name("collator-0"))
				.with_collator(|n| n.with_name("collator-1"))
				.with_collator(|n| n.with_name("collator-2"))
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
