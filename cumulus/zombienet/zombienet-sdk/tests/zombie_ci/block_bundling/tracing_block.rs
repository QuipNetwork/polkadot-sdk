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

use crate::utils::initialize_network;
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::submit_extrinsic_and_wait_for_finalization_success;
use serde_json::json;
use sp_rpc::tracing::TraceBlockResponse;
use zombienet_sdk::{
	subxt::{dynamic::Value, ext::subxt_rpcs::rpc_params, OnlineClient, PolkadotConfig},
	subxt_signer::sr25519::dev,
	NetworkConfig, NetworkConfigBuilder,
};

const PARA_ID: u32 = 2400;

/// A test that sends a transfer transaction, waits for it to be finalized, and then runs the
/// tracing_block rpc for the block containing the transfer.
#[tokio::test(flavor = "multi_thread")]
async fn block_bundling_tracing_block() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	log::info!("Spawning network");
	let config = build_network_config().await?;
	let network = initialize_network(config).await?;

	let para_node = network.get_node("collator-0")?;
	let para_client: OnlineClient<PolkadotConfig> = para_node.wait_client().await?;

	// Create a balance transfer transaction
	let alice = dev::alice();
	let bob = dev::bob().public_key();
	let transfer_amount = 1_000_000_000_000u128; // 1 unit with 12 decimals

	log::info!("Creating balance transfer transaction");
	let transfer_call = zombienet_sdk::subxt::dynamic::tx(
		"Balances",
		"transfer_allow_death",
		vec![Value::unnamed_variant("Id", [Value::from_bytes(bob)]), Value::u128(transfer_amount)],
	);

	// Submit the transfer transaction and wait for finalization
	log::info!("Submitting transfer transaction and waiting for finalization");
	let transfer_block_hash =
		submit_extrinsic_and_wait_for_finalization_success(&para_client, &transfer_call, &alice)
			.await?;

	log::info!("Transfer transaction finalized in block: {:?}", transfer_block_hash);

	// Get RPC client to make tracing_block call
	let rpc_client = para_node.rpc().await?;

	log::info!("Calling tracing_block RPC for the block containing the transfer");

	// Make the tracing_block RPC call for the block containing our transfer
	let trace_result: TraceBlockResponse = rpc_client
		.request(
			"state_traceBlock",
			rpc_params![
				format!("{:?}", transfer_block_hash),
				None::<String>,
				None::<String>,
				None::<String>
			],
		)
		.await?;

	log::info!("Successfully received tracing result for transfer block");

	// Decode and verify the BlockTrace is successful
	match trace_result {
		TraceBlockResponse::TraceError(error) => {
			Err(anyhow!("Block tracing failed: {}", error.error))
		},
		TraceBlockResponse::BlockTrace(_) => {
			log::info!("✅ Block trace successful!");
			Ok(())
		},
	}
}

async fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	// images are not relevant for `native`, but we leave it here in case we use `k8s` some day
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
					// These settings are applicable only for `k8s` provider.
					// Leaving them in case we switch to `k8s` some day.
					resources.with_request_cpu(4).with_request_memory("4G")
				})
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"num_cores": 7,
								"max_validators_per_core": 1
							}
						}
					}
				}))
				// Have to set a `with_validator` outside of the loop below, so that `r` has the
				// right type.
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
					("-lparachain=debug,aura=trace").into(),
					("--enable-offchain-indexing=true").into(),
				])
				.with_collator(|n| n.with_name("collator-0"))
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
