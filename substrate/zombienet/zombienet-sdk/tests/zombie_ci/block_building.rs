// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use crate::utils::{
	env_or_default, initialize_network, log_line_absent, BEST_BLOCK_METRIC,
	DEFAULT_SUBSTRATE_IMAGE, INTEGRATION_IMAGE_ENV, NODE_ROLE_METRIC, PEER_COUNT_METRIC,
};
use anyhow::{anyhow, Result};
use zombienet_sdk::{
	subxt::{self, config::substrate::SubstrateConfig, dynamic::tx, OnlineClient},
	subxt_signer::sr25519::dev,
	Arg, NetworkConfig, NetworkConfigBuilder, NetworkNode,
};

const NODE_NAMES: [&str; 2] = ["alice", "bob"];

const ROLE_VALIDATOR_VALUE: f64 = 4.0;
const PEER_MIN_THRESHOLD: f64 = 1.0;
const BLOCK_TARGET: f64 = 5.0;

const NETWORK_READY_TIMEOUT_SECS: u64 = 180;
const METRIC_TIMEOUT_SECS: u64 = 60;
const LOG_TIMEOUT_SECS: u64 = 2;
const TRANSACTION_TIMEOUT_SECS: u64 = 30;

const REMARK_PAYLOAD: &[u8] = b"block-building-test";
const LARGE_REMARK_SIZE: usize = 8 * 1024 * 1024;
const LARGE_REMARK_FINALIZATION_TIMEOUT_SECS: u64 = 120;
#[tokio::test(flavor = "multi_thread")]
async fn block_building_test() -> Result<()> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	log::info!("Spawning network");
	let config = build_network_config()?;
	let network = initialize_network(config).await?;

	network.wait_until_is_up(NETWORK_READY_TIMEOUT_SECS).await?;

	for node_name in NODE_NAMES {
		let node = network.get_node(node_name)?;
		assert_node_health(node).await?;
	}

	let alice = network.get_node("alice")?;
	submit_transaction_and_wait_finalization(alice).await?;

	let large_remark_block = submit_large_remark_and_wait_finalization(alice).await?;
	let target_height = large_remark_block as f64;

	let bob = network.get_node("bob")?;
	bob.wait_metric_with_timeout(
		BEST_BLOCK_METRIC,
		|height| height >= target_height,
		METRIC_TIMEOUT_SECS,
	)
	.await?;
	log::info!("Bob imported block #{large_remark_block} containing the large remark");

	network.destroy().await?;

	Ok(())
}

fn build_network_config() -> Result<NetworkConfig> {
	let integration_image = env_or_default(INTEGRATION_IMAGE_ENV, DEFAULT_SUBSTRATE_IMAGE);
	let wasm_runtime_overrides = std::env::var("WASM_RUNTIME_OVERRIDES").ok();
	let mut default_args = vec![
		Arg::from("--rpc-max-request-size=100"),
		Arg::from("--rpc-max-response-size=100"),
		Arg::from("--log=wasm-heap=trace"),
	];
	if let Some(path) = wasm_runtime_overrides.as_ref() {
		default_args.push(Arg::from(format!("--wasm-runtime-overrides={path}").as_str()));
	}

	let config = NetworkConfigBuilder::new()
		.with_relaychain(|relaychain| {
			relaychain
				.with_chain("local")
				.with_default_command("substrate")
				.with_default_image(integration_image.as_str())
				.with_chain_spec_command(
					"substrate build-spec --chain {{chainName}} --disable-default-bootnode --raw",
				)
				.chain_spec_command_is_local(true)
				.with_default_args(default_args)
				.with_validator(|node| node.with_name("alice"))
				.with_validator(|node| node.with_name("bob"))
		})
		.with_global_settings(|global_settings| match std::env::var("ZOMBIENET_SDK_BASE_DIR") {
			Ok(val) => global_settings.with_base_dir(val),
			_ => global_settings,
		})
		.build()
		.map_err(|errs| {
			let message =
				errs.into_iter().map(|err| err.to_string()).collect::<Vec<_>>().join(", ");
			anyhow!("config errs: {message}")
		})?;

	Ok(config)
}

async fn assert_node_health(node: &NetworkNode) -> Result<()> {
	node.wait_until_is_up(METRIC_TIMEOUT_SECS).await?;

	node.wait_metric_with_timeout(
		NODE_ROLE_METRIC,
		|role| role == ROLE_VALIDATOR_VALUE,
		METRIC_TIMEOUT_SECS,
	)
	.await?;

	node.wait_metric_with_timeout(
		PEER_COUNT_METRIC,
		|peers| peers >= PEER_MIN_THRESHOLD,
		METRIC_TIMEOUT_SECS,
	)
	.await?;

	node.wait_metric_with_timeout(
		BEST_BLOCK_METRIC,
		|height| height >= BLOCK_TARGET,
		METRIC_TIMEOUT_SECS,
	)
	.await?;

	node.wait_log_line_count_with_timeout("error", false, log_line_absent(LOG_TIMEOUT_SECS))
		.await?;

	Ok(())
}

async fn submit_transaction_and_wait_finalization(node: &NetworkNode) -> Result<()> {
	let client: OnlineClient<SubstrateConfig> = node.wait_client::<SubstrateConfig>().await?;
	let signer = dev::alice();

	let remark_call =
		tx("System", "remark", vec![subxt::dynamic::Value::from_bytes(REMARK_PAYLOAD)]);

	tokio::time::timeout(Duration::from_secs(TRANSACTION_TIMEOUT_SECS), async {
		client
			.tx()
			.sign_and_submit_then_watch_default(&remark_call, &signer)
			.await?
			.wait_for_finalized_success()
			.await
	})
	.await
	.map_err(|_| anyhow!("transaction timed out"))??;

	Ok(())
}

async fn build_client_with_large_payload(url: &str) -> Result<OnlineClient<SubstrateConfig>> {
	use subxt::ext::jsonrpsee::{
		client_transport::ws::{Url, WsTransportClientBuilder},
		core::client::Client,
	};

	let url = Url::parse(url).map_err(|e| anyhow!("invalid URL: {e}"))?;
	let (sender, receiver) = WsTransportClientBuilder::default()
		.max_request_size(100 * 1024 * 1024)
		.max_response_size(100 * 1024 * 1024)
		.build(url)
		.await
		.map_err(|e| anyhow!("WS transport failed: {e}"))?;

	let rpc_client = Client::builder()
		.max_buffer_capacity_per_subscription(4096)
		.build_with_tokio(sender, receiver);

	OnlineClient::<SubstrateConfig>::from_rpc_client(rpc_client)
		.await
		.map_err(|e| anyhow!("OnlineClient creation failed: {e}"))
}

async fn submit_large_remark_and_wait_finalization(node: &NetworkNode) -> Result<u32> {
	let client = build_client_with_large_payload(node.ws_uri()).await?;
	let signer = dev::alice();

	let large_payload = vec![0u8; LARGE_REMARK_SIZE];
	let remark_call =
		tx("System", "remark", vec![subxt::dynamic::Value::from_bytes(&large_payload)]);

	log::info!("Submitting {} MiB remark transaction", LARGE_REMARK_SIZE / (1024 * 1024));

	let block_number =
		tokio::time::timeout(Duration::from_secs(LARGE_REMARK_FINALIZATION_TIMEOUT_SECS), async {
			let in_block = client
				.tx()
				.sign_and_submit_then_watch_default(&remark_call, &signer)
				.await?
				.wait_for_finalized()
				.await?;
			let block_hash = in_block.block_hash();
			in_block.wait_for_success().await?;
			let block = client.blocks().at(block_hash).await?;
			Ok::<u32, subxt::Error>(block.number())
		})
		.await
		.map_err(|_| anyhow!("large remark transaction timed out"))??;

	log::info!("Large remark transaction finalized in block #{block_number}");

	Ok(block_number)
}
