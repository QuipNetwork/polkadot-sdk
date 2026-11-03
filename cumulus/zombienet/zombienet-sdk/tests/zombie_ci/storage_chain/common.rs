// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{anyhow, Context, Result};
use codec::Decode;
use cumulus_zombienet_sdk_helpers::wait_for_first_session_change;
use std::time::Duration;
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{
	subxt::{
		backend::rpc::RpcClient,
		config::polkadot::{PolkadotConfig, PolkadotExtrinsicParamsBuilder},
		dynamic::{tx, Value},
		ext::subxt_rpcs::rpc_params,
		OnlineClient,
	},
	subxt_signer::sr25519::dev,
	NetworkConfig, NetworkConfigBuilder, NetworkNode,
};

pub use crate::utils::initialize_network;

pub const NODE_ROLE_METRIC: &str = "node_roles";
pub const IS_MAJOR_SYNCING_METRIC: &str = "substrate_sub_libp2p_is_major_syncing";

pub const FULLNODE_ROLE_VALUE: f64 = 1.0;
pub const IDLE_VALUE: f64 = 0.0;

pub const NETWORK_READY_TIMEOUT_SECS: u64 = 180;
pub const METRIC_TIMEOUT_SECS: u64 = 60;
pub const BLOCK_PRODUCTION_TIMEOUT_SECS: u64 = 300;
pub const SYNC_TIMEOUT_SECS: u64 = 180;
pub const LOG_TIMEOUT_SECS: u64 = 60;
pub const LOG_ERROR_TIMEOUT_SECS: u64 = 10;

pub const NODE_LOG_CONFIG: &str = "-lsync=trace,sub-libp2p=trace,litep2p=trace,request-response=trace,transaction-storage=trace,bitswap=trace,storage-chain-block-import=debug,storage-chain-fetcher=debug,db=debug,rpc-spec-v2=debug,state=trace";

pub const RELAY_CHAIN: &str = "westend-local";
pub const PARA_ID: u32 = 2487;

pub const RELAY_BINARY: &str = "polkadot";
pub const PARACHAIN_BINARY: &str = "polkadot-parachain";
pub const PARACHAIN_CHAIN_SPEC: &str =
	"tests/zombie_ci/storage_chain/fixtures/bulletin-westend-spec.json";

pub struct ParachainSnapshots<'a> {
	pub collator: &'a str,
	pub relay: &'a str,
	pub chain_spec: &'a str,
	pub relay_chain_spec: &'a str,
}

pub fn build_parachain_network_config(
	para_node_args: Vec<String>,
	snapshots: Option<ParachainSnapshots>,
) -> Result<NetworkConfig> {
	let relay_snapshot = snapshots.as_ref().filter(|s| !s.relay.is_empty()).map(|s| s.relay);
	let collator_snapshot = snapshots.as_ref().map(|s| s.collator);
	let relay_chain_spec = snapshots
		.as_ref()
		.filter(|s| !s.relay_chain_spec.is_empty())
		.map(|s| s.relay_chain_spec);
	let para_chain_spec = snapshots
		.as_ref()
		.filter(|s| !s.chain_spec.is_empty())
		.map(|s| s.chain_spec)
		.unwrap_or(PARACHAIN_CHAIN_SPEC);

	let relay_args: Vec<_> = vec!["-lruntime=debug"].into_iter().map(Into::into).collect();
	let para_args: Vec<_> = para_node_args.iter().map(|s| s.as_str().into()).collect();

	NetworkConfigBuilder::new()
		.with_relaychain(|relaychain| {
			let relaychain = relaychain
				.with_chain(RELAY_CHAIN)
				.with_default_command(RELAY_BINARY)
				.with_optional_default_db_snapshot(relay_snapshot);
			let relaychain = match relay_chain_spec {
				Some(spec) => relaychain.with_chain_spec_path(spec),
				None => relaychain,
			};
			relaychain
				.with_validator(|node| {
					node.with_name("alice").validator(true).with_args(relay_args.clone())
				})
				.with_validator(|node| {
					node.with_name("bob").validator(true).with_args(relay_args.clone())
				})
				.with_validator(|node| {
					node.with_name("charlie").validator(true).with_args(relay_args)
				})
		})
		.with_parachain(|parachain| {
			parachain
				.with_id(PARA_ID)
				.with_chain_spec_path(para_chain_spec)
				.cumulus_based(true)
				.with_collator(|node| {
					node.with_name("collator-1")
						.validator(true)
						.with_command(PARACHAIN_BINARY)
						.with_args(para_args)
						.with_optional_db_snapshot(collator_snapshot)
				})
		})
		.with_global_settings(|settings| match std::env::var("ZOMBIENET_SDK_BASE_DIR") {
			Ok(base_dir) => settings.with_base_dir(base_dir),
			Err(_) => settings,
		})
		.build()
		.map_err(|errs| {
			let message = errs.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(", ");
			anyhow!("config errs: {message}")
		})
}

pub fn log_line_at_least_once(timeout_secs: u64) -> LogLineCountOptions {
	LogLineCountOptions::new(|count| count >= 1, Duration::from_secs(timeout_secs), false)
}

pub fn log_line_absent(timeout_secs: u64) -> LogLineCountOptions {
	LogLineCountOptions::no_occurences_within_timeout(Duration::from_secs(timeout_secs))
}

pub async fn expect_log_line(
	node: &NetworkNode,
	pattern: &str,
	timeout_secs: u64,
	error_msg: &str,
) -> Result<()> {
	let result = node
		.wait_log_line_count_with_timeout(pattern, false, log_line_at_least_once(timeout_secs))
		.await
		.context(format!("Failed to check log: {}", pattern))?;
	if !result.success() {
		anyhow::bail!("{}", error_msg);
	}
	Ok(())
}

pub async fn expect_no_log_line(
	node: &NetworkNode,
	pattern: &str,
	timeout_secs: u64,
	error_msg: &str,
) -> Result<()> {
	let result = node
		.wait_log_line_count_with_timeout(pattern, false, log_line_absent(timeout_secs))
		.await
		.context(format!("Failed to check absence of log: {}", pattern))?;
	if !result.success() {
		anyhow::bail!("{}", error_msg);
	}
	Ok(())
}

pub async fn verify_warp_sync_completed(node: &NetworkNode) -> Result<()> {
	expect_log_line(
		node,
		"Warp sync is complete",
		LOG_TIMEOUT_SECS,
		"Node did not complete warp sync",
	)
	.await?;
	node.wait_metric_with_timeout(
		IS_MAJOR_SYNCING_METRIC,
		|value| value == IDLE_VALUE,
		SYNC_TIMEOUT_SECS,
	)
	.await
	.context("Node did not reach idle state after warp sync")?;
	expect_no_log_line(
		node,
		"verification failed",
		LOG_ERROR_TIMEOUT_SECS,
		"Node logged verification errors",
	)
	.await
}

pub async fn wait_for_relay_chain_to_sync(node: &NetworkNode, timeout_secs: u64) -> Result<()> {
	let result = node
		.wait_log_line_count_with_timeout(
			r"Update at relay chain block.*included: #[1-9]",
			false,
			log_line_at_least_once(timeout_secs),
		)
		.await
		.context("Failed to check relay chain sync status")?;
	if !result.success() {
		anyhow::bail!("Embedded relay chain did not sync within {}s", timeout_secs);
	}
	Ok(())
}

pub async fn wait_for_session_change_on_node(node: &NetworkNode, timeout_secs: u64) -> Result<()> {
	let client: OnlineClient<PolkadotConfig> = node.wait_client().await?;
	let wait = async {
		let mut blocks = client.blocks().subscribe_finalized().await?;
		wait_for_first_session_change(&mut blocks).await
	};
	tokio::time::timeout(Duration::from_secs(timeout_secs), wait)
		.await
		.map_err(|_| anyhow!("Timeout waiting for session change after {}s", timeout_secs))?
}

pub struct RenewOutcome {
	pub renewed_at_block: u64,
	pub renewed_index: u32,
}

fn renewed_content_hash(
	events: &zombienet_sdk::subxt::blocks::ExtrinsicEvents<PolkadotConfig>,
) -> Result<(u32, [u8; 32])> {
	for event in events.iter() {
		let event = event?;
		if event.pallet_name() == "TransactionStorage" && event.variant_name() == "Renewed" {
			let (index, content_hash): (u32, [u8; 32]) =
				Decode::decode(&mut &event.field_bytes()[..])?;
			return Ok((index, content_hash));
		}
	}
	anyhow::bail!("Renewed event not found in extrinsic events")
}

#[cfg(feature = "generate-snapshots")]
pub async fn wait_for_in_best_block(
	mut progress: zombienet_sdk::subxt::tx::TxProgress<
		PolkadotConfig,
		OnlineClient<PolkadotConfig>,
	>,
) -> Result<(
	zombienet_sdk::subxt::utils::H256,
	zombienet_sdk::subxt::blocks::ExtrinsicEvents<PolkadotConfig>,
)> {
	use zombienet_sdk::subxt::tx::TxStatus;

	while let Some(status) = progress.next().await {
		match status? {
			TxStatus::InBestBlock(tx_in_block) => {
				let block_hash = tx_in_block.block_hash();
				let events = tx_in_block.wait_for_success().await?;
				return Ok((block_hash, events));
			},
			TxStatus::Error { message } |
			TxStatus::Invalid { message } |
			TxStatus::Dropped { message } => anyhow::bail!("Transaction failed: {}", message),
			_ => continue,
		}
	}
	anyhow::bail!("Transaction stream ended without InBestBlock status")
}

pub async fn wait_for_finalized(
	mut progress: zombienet_sdk::subxt::tx::TxProgress<
		PolkadotConfig,
		OnlineClient<PolkadotConfig>,
	>,
) -> Result<(
	zombienet_sdk::subxt::utils::H256,
	zombienet_sdk::subxt::blocks::ExtrinsicEvents<PolkadotConfig>,
)> {
	use zombienet_sdk::subxt::tx::TxStatus;

	while let Some(status) = progress.next().await {
		match status? {
			TxStatus::InFinalizedBlock(tx_in_block) => {
				let block_hash = tx_in_block.block_hash();
				let events = tx_in_block.wait_for_success().await?;
				return Ok((block_hash, events));
			},
			TxStatus::Error { message } |
			TxStatus::Invalid { message } |
			TxStatus::Dropped { message } => anyhow::bail!("Transaction failed: {}", message),
			_ => continue,
		}
	}
	anyhow::bail!("Transaction stream ended without InFinalizedBlock status")
}

#[cfg(feature = "generate-snapshots")]
pub async fn get_alice_nonce(node: &NetworkNode) -> Result<u64> {
	let client: OnlineClient<PolkadotConfig> = node.wait_client().await?;
	let alice = dev::alice().public_key().to_account_id();
	client.tx().account_nonce(&alice).await.map_err(Into::into)
}

pub async fn renew_data_with_content_hash(
	client: &OnlineClient<PolkadotConfig>,
	expected_hash: [u8; 32],
	nonce: u64,
) -> Result<RenewOutcome> {
	let signer = dev::bob();
	let renew_call =
		tx("TransactionStorage", "renew_content_hash", vec![Value::from_bytes(expected_hash)]);
	let params = PolkadotExtrinsicParamsBuilder::new().nonce(nonce).immortal().build();

	let (block_hash, events) = tokio::time::timeout(Duration::from_secs(120), async {
		let progress = client.tx().sign_and_submit_then_watch(&renew_call, &signer, params).await?;
		wait_for_finalized(progress).await
	})
	.await
	.map_err(|_| anyhow!("renew_content_hash timed out (nonce={})", nonce))??;

	let (renewed_index, content_hash) = renewed_content_hash(&events)?;
	anyhow::ensure!(content_hash == expected_hash, "Renewed event hash mismatch");
	let block = client.blocks().at(block_hash).await?;
	Ok(RenewOutcome { renewed_at_block: block.number() as u64, renewed_index })
}

#[derive(Debug, thiserror::Error)]
pub enum BitswapRpcError {
	#[error("node is major syncing")]
	MajorSyncing,
	#[error("rpc transport: {0}")]
	Transport(String),
	#[error("hex decoding failed: {0}")]
	Decoding(String),
}

pub async fn bitswap_v1_get(
	node: &NetworkNode,
	cid: &str,
) -> std::result::Result<Option<Vec<u8>>, BitswapRpcError> {
	let rpc = RpcClient::from_url(node.ws_uri())
		.await
		.map_err(|e| BitswapRpcError::Transport(format!("connect: {e}")))?;

	match rpc.request::<String>("bitswap_v1_get", rpc_params![cid]).await {
		Ok(hex_str) => hex::decode(hex_str.trim_start_matches("0x"))
			.map(Some)
			.map_err(|e| BitswapRpcError::Decoding(e.to_string())),
		Err(e) => {
			let message = e.to_string();
			if message.contains("-32812") {
				Err(BitswapRpcError::MajorSyncing)
			} else if message.contains("-32810") {
				Ok(None)
			} else {
				Err(BitswapRpcError::Transport(message))
			}
		},
	}
}

pub async fn expect_dont_have(node: &NetworkNode, cid: &str, timeout: Duration) -> Result<()> {
	let deadline = std::time::Instant::now() + timeout;
	while std::time::Instant::now() < deadline {
		match bitswap_v1_get(node, cid).await {
			Ok(None) => return Ok(()),
			Ok(Some(bytes)) => {
				anyhow::bail!("expect_dont_have({cid}): node has {} bytes", bytes.len())
			},
			Err(BitswapRpcError::MajorSyncing) => tokio::time::sleep(Duration::from_secs(1)).await,
			Err(other) => anyhow::bail!("bitswap_v1_get: {other}"),
		}
	}
	anyhow::bail!("expect_dont_have({cid}) timed out after {:?}", timeout)
}
