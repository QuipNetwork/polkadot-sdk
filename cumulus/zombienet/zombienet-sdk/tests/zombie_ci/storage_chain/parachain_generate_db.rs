// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Snapshot builder for the storage-chain tip-sync test.
//!
//! Produces a single self-contained `bundle.tar.gz` that holds:
//!   * `parachain-db.tgz` — collator DB (`data/` + embedded `relay-data/`)
//!   * `relaychain-db.tgz` — relay validator DB (`data/`)
//!   * `manifest.json` — schema + `user_data` carrying [`BundleUserData`] (test metadata plus the
//!     two raw chain specs the consumer needs to restart against the same chains).
//!
//! Uses the zombienet-sdk 0.4.13 snapshot API
//! ([`Network::pause`] / [`NetworkNode::snapshot_db`] / [`BundleBuilder`])
//! so we don't hand-roll `tar` invocations or sidecar JSON files.

use super::{
	common::{
		get_alice_nonce, initialize_network, wait_for_in_best_block,
		wait_for_session_change_on_node, NETWORK_READY_TIMEOUT_SECS, NODE_LOG_CONFIG,
		PARACHAIN_BINARY, PARACHAIN_CHAIN_SPEC, PARA_ID, RELAY_BINARY, RELAY_CHAIN,
		SYNC_TIMEOUT_SECS,
	},
	fixture::{
		algorithm, content_hash, payload, BundleUserData, HashingAlgorithm, SnapshotMetadata,
		FIXTURE_RETENTION_PERIOD, N_STORES, PAYLOAD_SIZE_MAX, PAYLOAD_SIZE_MIN,
		TIP_SYNC_TARGET_BLOCKS,
	},
};
use crate::utils::{BEST_BLOCK_METRIC, FINALIZED_BLOCK_METRIC};
use anyhow::{anyhow, Context, Result};
use env_logger::Env;
use std::{collections::HashSet, path::PathBuf};
use zombienet_sdk::{
	subxt::{
		config::polkadot::{PolkadotConfig, PolkadotExtrinsicParamsBuilder},
		dynamic::Value,
		ext::scale_value::value,
		OnlineClient,
	},
	subxt_signer::sr25519::dev,
	BundleBuilder, NetworkConfig, NetworkConfigBuilder,
};

const SESSION_CHANGE_TIMEOUT_SECS: u64 = 300;
const BUNDLE_OUTPUT_DIR_ENV: &str = "BUNDLE_OUTPUT_DIR";
const DEFAULT_BUNDLE_OUTPUT_DIR: &str = "./zombienet/test-databases";
const STORE_START_BLOCK: u64 = 50;
const GEN_TIMEOUT_SECS: u64 = 1800;

/// Filename of the produced bundle inside the output dir.
pub const BUNDLE_FILENAME: &str = "tip-sync-100-bundle.tar.gz";

fn build_gendb_network_config(pruning_blocks: u32) -> Result<NetworkConfig> {
	let relay_args: Vec<_> = vec!["-lruntime=debug"].into_iter().map(Into::into).collect();
	let collator_args: Vec<_> =
		vec!["--ipfs-server", NODE_LOG_CONFIG].into_iter().map(Into::into).collect();
	let pruning_flag = format!("--blocks-pruning={pruning_blocks}");
	let pruned_args: Vec<_> =
		vec!["--sync=full", "--ipfs-server", pruning_flag.as_str(), NODE_LOG_CONFIG]
			.into_iter()
			.map(Into::into)
			.collect();

	NetworkConfigBuilder::new()
		.with_relaychain(|relaychain| {
			relaychain
				.with_chain(RELAY_CHAIN)
				.with_default_command(RELAY_BINARY)
				.with_validator(|node| {
					node.with_name("alice").validator(true).with_args(relay_args.clone())
				})
				.with_validator(|node| node.with_name("bob").validator(true).with_args(relay_args))
		})
		.with_parachain(|parachain| {
			parachain
				.with_id(PARA_ID)
				.with_chain_spec_path(PARACHAIN_CHAIN_SPEC)
				.cumulus_based(true)
				.with_collator(|node| {
					node.with_name("collator-1")
						.validator(true)
						.with_command(PARACHAIN_BINARY)
						.with_args(collator_args)
				})
				.with_collator(|node| {
					node.with_name("pruned-node")
						.validator(false)
						.with_command(PARACHAIN_BINARY)
						.with_args(pruned_args)
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

async fn authorize_account(
	client: &OnlineClient<PolkadotConfig>,
	who: &zombienet_sdk::subxt_signer::sr25519::Keypair,
	transactions: u32,
	bytes: u64,
	nonce: u64,
	label: &str,
) -> Result<()> {
	let signer = dev::alice();
	let call = zombienet_sdk::subxt::tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			TransactionStorage(authorize_account {
				who: Value::from_bytes(who.public_key().0),
				transactions: transactions,
				bytes: bytes
			})
		}],
	);
	let params = PolkadotExtrinsicParamsBuilder::new().nonce(nonce).build();
	tokio::time::timeout(std::time::Duration::from_secs(60), async {
		let progress = client.tx().sign_and_submit_then_watch(&call, &signer, params).await?;
		wait_for_in_best_block(progress).await?;
		Ok::<_, anyhow::Error>(())
	})
	.await
	.map_err(|_| anyhow!("{label} authorization timed out"))??;
	Ok(())
}

async fn store_data(
	client: &OnlineClient<PolkadotConfig>,
	data: &[u8],
	nonce: u64,
	algo: HashingAlgorithm,
) -> Result<u64> {
	let algo_name = match algo {
		HashingAlgorithm::Blake2b256 => "Blake2b256",
		HashingAlgorithm::Sha2_256 => "Sha2_256",
		HashingAlgorithm::Keccak256 => "Keccak256",
	};
	let cid_config = Value::named_composite(vec![
		("codec", Value::u128(0x55)),
		("hashing", Value::unnamed_variant(algo_name, vec![])),
	]);
	let call = zombienet_sdk::subxt::tx::dynamic(
		"TransactionStorage",
		"store_with_cid_config",
		vec![cid_config, Value::from_bytes(data)],
	);
	let params = PolkadotExtrinsicParamsBuilder::new().nonce(nonce).build();
	let (block_hash, _) = tokio::time::timeout(std::time::Duration::from_secs(120), async {
		let progress = client.tx().sign_and_submit_then_watch(&call, &dev::alice(), params).await?;
		wait_for_in_best_block(progress).await
	})
	.await
	.map_err(|_| anyhow!("store transaction timed out (nonce={nonce})"))??;
	Ok(client.blocks().at(block_hash).await?.number() as u64)
}

fn payload_stats() -> Result<u64> {
	let mut hashes_seen = HashSet::new();
	let mut total_payload_bytes = 0;
	for i in 0..N_STORES {
		let bytes = payload(i);
		anyhow::ensure!((PAYLOAD_SIZE_MIN..=PAYLOAD_SIZE_MAX).contains(&bytes.len()));
		let hash = content_hash(i);
		anyhow::ensure!(hash == algorithm(i).hash(&bytes));
		anyhow::ensure!(hashes_seen.insert(hash), "duplicate content_hash at i={i}");
		total_payload_bytes += bytes.len() as u64;
	}
	Ok(total_payload_bytes)
}

fn read_chain_spec(
	base_dir: &std::path::Path,
	node: &str,
	file: &str,
) -> Result<serde_json::Value> {
	let path = base_dir.join(node).join("cfg").join(file);
	let bytes = std::fs::read(&path)
		.with_context(|| format!("Failed to read chain spec {}", path.display()))?;
	serde_json::from_slice(&bytes)
		.with_context(|| format!("Failed to parse chain spec {}", path.display()))
}

#[tokio::test(flavor = "multi_thread")]
async fn parachain_generate_databases() -> Result<()> {
	let _ = env_logger::Builder::from_env(Env::default().default_filter_or("info")).try_init();

	let total_payload_bytes = payload_stats()?;
	let output_dir = std::env::var(BUNDLE_OUTPUT_DIR_ENV)
		.map(PathBuf::from)
		.unwrap_or_else(|_| DEFAULT_BUNDLE_OUTPUT_DIR.into());
	std::fs::create_dir_all(&output_dir)
		.with_context(|| format!("Failed to create output dir {}", output_dir.display()))?;
	log::info!("Generating storage-chain bundle into {}", output_dir.display());

	let config = build_gendb_network_config(FIXTURE_RETENTION_PERIOD)?;
	let network = initialize_network(config).await?;
	network.wait_until_is_up(NETWORK_READY_TIMEOUT_SECS).await?;

	let relay_alice = network.get_node("alice")?;
	wait_for_session_change_on_node(relay_alice, SESSION_CHANGE_TIMEOUT_SECS).await?;

	let collator = network.get_node("collator-1")?;
	let client: OnlineClient<PolkadotConfig> = collator.wait_client().await?;
	let authorize_transactions = N_STORES + 10;
	let authorize_bytes = total_payload_bytes.saturating_mul(2).saturating_add(1024 * 1024);

	let mut nonce = get_alice_nonce(collator).await?;
	authorize_account(
		&client,
		&dev::alice(),
		authorize_transactions,
		authorize_bytes,
		nonce,
		"alice",
	)
	.await?;
	nonce += 1;
	authorize_account(&client, &dev::bob(), authorize_transactions, authorize_bytes, nonce, "bob")
		.await?;
	nonce += 1;

	collator
		.wait_metric_with_timeout(
			BEST_BLOCK_METRIC,
			|height| height >= STORE_START_BLOCK as f64,
			GEN_TIMEOUT_SECS,
		)
		.await
		.context(format!("Node did not reach block height {STORE_START_BLOCK}"))?;

	let mut first_store_block = u64::MAX;
	let mut last_store_block = 0;
	for i in 0..N_STORES {
		let data = payload(i);
		let included = store_data(&client, &data, nonce, algorithm(i)).await?;
		nonce += 1;
		first_store_block = first_store_block.min(included);
		last_store_block = last_store_block.max(included);
		log::info!("Store {}/{} included at block {}", i + 1, N_STORES, included);
	}

	anyhow::ensure!(
		last_store_block <= TIP_SYNC_TARGET_BLOCKS,
		"last store landed at block {last_store_block}, beyond target {TIP_SYNC_TARGET_BLOCKS}",
	);

	let finalize_target = TIP_SYNC_TARGET_BLOCKS.max(last_store_block);
	collator
		.wait_metric_with_timeout(
			FINALIZED_BLOCK_METRIC,
			|height| height >= finalize_target as f64,
			GEN_TIMEOUT_SECS,
		)
		.await
		.context(format!("Node did not finalize block height {finalize_target}"))?;
	let pruned_node = network.get_node("pruned-node")?;
	pruned_node
		.wait_metric_with_timeout(
			BEST_BLOCK_METRIC,
			|height| height >= finalize_target as f64,
			SYNC_TIMEOUT_SECS,
		)
		.await
		.context(format!("Node did not reach block height {finalize_target}"))?;
	pruned_node
		.wait_metric_with_timeout(
			FINALIZED_BLOCK_METRIC,
			|height| height >= finalize_target as f64,
			GEN_TIMEOUT_SECS,
		)
		.await
		.context(format!("Node did not finalize block height {finalize_target}"))?;

	let base_dir: PathBuf = network
		.base_dir()
		.ok_or_else(|| anyhow!("Failed to get network base directory"))?
		.into();
	let para_chain_spec = read_chain_spec(&base_dir, "collator-1", &format!("{}.json", PARA_ID))?;
	let relay_chain_spec = read_chain_spec(&base_dir, "alice", "westend-local.json")?;

	let metadata = SnapshotMetadata {
		total_blocks: TIP_SYNC_TARGET_BLOCKS,
		retention_period: FIXTURE_RETENTION_PERIOD,
		n_stores: N_STORES,
		payload_size_min: PAYLOAD_SIZE_MIN,
		payload_size_max: PAYLOAD_SIZE_MAX,
		snapshot_height: finalize_target,
		first_store_block,
		last_store_block,
	};

	// Pause the network so RocksDB on disk is consistent while we tar.
	log::info!("Pausing network for snapshot");
	network.pause().await.context("Failed to pause network")?;

	let para_snapshot = pruned_node
		.snapshot_db(output_dir.join("parachain-db.tgz"))
		.await
		.context("Failed to snapshot parachain DB")?;
	let relay_snapshot = relay_alice
		.snapshot_db(output_dir.join("relaychain-db.tgz"))
		.await
		.context("Failed to snapshot relaychain DB")?;

	log::info!("Resuming network");
	network.resume().await.context("Failed to resume network")?;

	let user_data = BundleUserData { metadata, para_chain_spec, relay_chain_spec };

	let bundle_path = output_dir.join(BUNDLE_FILENAME);
	let bundle = BundleBuilder::new()
		.add(para_snapshot)
		.add(relay_snapshot)
		.user_data(&user_data)
		.build(&bundle_path)
		.context("Failed to assemble snapshot bundle")?;

	log::info!(
		"Bundle written: {} ({} bytes, sha256={})",
		bundle.path.display(),
		bundle.size,
		bundle.sha256,
	);
	Ok(())
}
