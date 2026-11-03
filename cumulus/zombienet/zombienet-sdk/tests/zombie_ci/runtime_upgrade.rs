// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use crate::utils::initialize_network;
use anyhow::anyhow;
use cumulus_test_runtime::spec_version_incremented::WASM_BINARY as WASM_RUNTIME_UPGRADE;
use cumulus_zombienet_sdk_helpers::{
	assert_para_throughput, submit_sudo_runtime_upgrade, wait_for_pvf_prepare,
	wait_for_runtime_upgrade,
};
use polkadot_primitives::{Id as ParaId, MAX_CODE_SIZE};
use serde_json::json;
use zombienet_sdk::{
	subxt::{
		ext::jsonrpsee::{
			client_transport::ws::{Url, WsTransportClientBuilder},
			core::client::Client,
		},
		OnlineClient, PolkadotConfig,
	},
	subxt_signer::sr25519::dev,
	NetworkConfig, NetworkConfigBuilder,
};

async fn big_message_client(ws_uri: &str) -> Result<OnlineClient<PolkadotConfig>, anyhow::Error> {
	let url = Url::parse(ws_uri)?;
	let (sender, receiver) = WsTransportClientBuilder::default()
		.max_request_size(25 * 1024 * 1024)
		.max_response_size(25 * 1024 * 1024)
		.build(url)
		.await
		.map_err(|e| anyhow!("WS transport failed: {e}"))?;
	let rpc = Client::builder().build_with_tokio(sender, receiver);
	Ok(OnlineClient::<PolkadotConfig>::from_rpc_client(rpc).await?)
}

const PARA_ID: u32 = 2000;

/// Pad a zstd-compressed wasm blob up to `target` bytes by appending a zstd skippable frame.
///
/// The skippable frame is part of the zstd specification: any spec-compliant decoder skips it,
/// so the padded blob still decompresses to the original wasm.
fn pad_compressed_to(mut blob: Vec<u8>, target: usize) -> Vec<u8> {
	assert!(blob.len() <= target, "compressed blob already exceeds target");
	let need = target - blob.len();
	if need == 0 {
		return blob;
	}
	assert!(need >= 8, "cannot pad fewer than 8 bytes with a skippable frame");
	let payload = (need - 8) as u32;
	blob.extend_from_slice(&0x184D_2A50_u32.to_le_bytes());
	blob.extend_from_slice(&payload.to_le_bytes());
	blob.resize(blob.len() + payload as usize, 0);
	blob
}

// This tests makes sure that it is possible to upgrade parachain's runtime
// and parachain produces blocks after such upgrade.
#[tokio::test(flavor = "multi_thread")]
async fn runtime_upgrade() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	log::info!("Spawning network");
	let config = build_network_config().await?;
	let network = initialize_network(config).await?;

	let charlie = network.get_node("charlie")?;
	// Wait until the WS endpoint is reachable, then build a client with bumped message caps so
	// a `MAX_CODE_SIZE` upgrade fits in the JSON-RPC payload.
	let _: OnlineClient<PolkadotConfig> = charlie.wait_client().await?;
	let charlie_client = big_message_client(charlie.ws_uri()).await?;

	let alice = network.get_node("alice")?;
	let relay_client: OnlineClient<PolkadotConfig> = alice.wait_client().await?;

	let current_spec_version =
		charlie_client.backend().current_runtime_version().await?.spec_version;
	log::info!("Current runtime spec version {current_spec_version}");

	wait_for_pvf_prepare(&network, 1).await?;
	log::info!("Measuring parachain throughput before runtime upgrade...");
	assert_para_throughput(&relay_client, 15, [(ParaId::from(PARA_ID), 14..17)], []).await?;

	// IMPORTANT: `MAX_CODE_SIZE` + overhead must always stay strictly below the
	// `AttestedCandidateV2` request/response transport cap defined in
	// `polkadot/node/network/protocol/src/request_response/mod.rs` (currently 8 MiB).
	// If the compressed code exceeds the response cap, the response is rejected
	// at the transport layer and the candidate cannot be backed. Whenever `MAX_CODE_SIZE` is
	// raised, raise the transport cap first and ship the node update before Gov config change..
	let wasm = pad_compressed_to(
		WASM_RUNTIME_UPGRADE.expect("Wasm runtime not built").to_vec(),
		MAX_CODE_SIZE as usize,
	);
	assert_eq!(wasm.len(), MAX_CODE_SIZE as usize);
	log::info!("Performing runtime upgrade with padded wasm of {} bytes", wasm.len());
	submit_sudo_runtime_upgrade(&charlie_client, &wasm, &dev::alice()).await?;

	let dave = network.get_node("dave")?;
	let dave_client: OnlineClient<PolkadotConfig> = dave.wait_client().await?;
	let expected_spec_version = current_spec_version + 1;

	log::info!("Waiting for parachain runtime upgrade to version {}", expected_spec_version);
	wait_for_runtime_upgrade(&dave_client).await?;

	// Wait for every relay validator to finish preparing the post-upgrade PVF.
	// This should already happened due to PVF pre-check, but we do it for sanity.
	wait_for_pvf_prepare(&network, 2).await?;
	log::info!("Measuring parachain throughput after runtime upgrade...");
	assert_para_throughput(&relay_client, 15, [(ParaId::from(PARA_ID), 14..17)], []).await?;

	let spec_version_from_charlie =
		dave_client.backend().current_runtime_version().await?.spec_version;
	assert_eq!(expected_spec_version, spec_version_from_charlie, "Unexpected runtime spec version");

	Ok(())
}

async fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	// images are not relevant for `native`, but we leave it here in case we use `k8s` some day
	let images = zombienet_sdk::environment::get_images_from_env();
	log::info!("Using images: {images:?}");

	// Network setup:
	// - relaychain nodes:
	// 	 - alice   - validator
	// 	 - bob     - validator
	// - parachain nodes
	//   - charlie - validator
	//   - dave    - full node
	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![("-lparachain=debug").into()])
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"max_code_size": MAX_CODE_SIZE,
						}
					}
				}))
				.with_validator(|node| node.with_name("alice"))
				.with_validator(|node| node.with_name("bob"))
		})
		.with_parachain(|p| {
			p.with_id(PARA_ID)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_collator(|n| {
					n.with_name("charlie").validator(true).with_args(vec![
						("-lparachain=debug").into(),
						// Default JSON-RPC payload cap is 15 MiB; submitting a `MAX_CODE_SIZE`
						// runtime upgrade goes over that once hex-encoded and JSON-wrapped.
						("--rpc-max-request-size", "25").into(),
						("--rpc-max-response-size", "25").into(),
					])
				})
				.with_collator(|n| n.with_name("dave").validator(false))
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
