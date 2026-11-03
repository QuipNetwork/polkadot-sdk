// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use super::{
	common::{
		bitswap_v1_get, build_parachain_network_config, expect_dont_have, expect_log_line,
		expect_no_log_line, initialize_network, renew_data_with_content_hash,
		verify_warp_sync_completed, wait_for_relay_chain_to_sync, wait_for_session_change_on_node,
		BLOCK_PRODUCTION_TIMEOUT_SECS, FULLNODE_ROLE_VALUE, METRIC_TIMEOUT_SECS,
		NETWORK_READY_TIMEOUT_SECS, NODE_LOG_CONFIG, NODE_ROLE_METRIC, PARACHAIN_BINARY, PARA_ID,
		SYNC_TIMEOUT_SECS,
	},
	fixture::{
		algorithm, content_hash, hash_to_cid, payload, HashingAlgorithm, ResolvedSnapshots,
		FIXTURE_RETENTION_PERIOD, N_STORES, TIP_SYNC_TARGET_BLOCKS,
	},
};
use crate::utils::{BEST_BLOCK_METRIC, FINALIZED_BLOCK_METRIC};
use anyhow::{anyhow, Context, Result};
use env_logger::Env;
use futures::future::try_join_all;
use std::time::Duration;
use zombienet_orchestrator::AddCollatorOptions;
use zombienet_sdk::{
	subxt::{config::polkadot::PolkadotConfig, OnlineClient},
	NetworkNode,
};

const N_RENEW_EXERCISES: u32 = N_STORES;
const WARP_PRUNING_BLOCKS: u32 = 500;
/// Mirrors `GAP_SYNC_BODY_SAFETY_MARGIN` in `polkadot-omni-node-lib`: the node clamps
/// the margin to half the runtime retention period.
const GAP_SYNC_BODY_SAFETY_MARGIN: u32 =
	if FIXTURE_RETENTION_PERIOD / 2 < 256 { FIXTURE_RETENTION_PERIOD / 2 } else { 256 };
/// Number of blocks below the finalized tip for which the sync node backfills bodies
/// during gap sync.
const GAP_SYNC_BODY_WINDOW: u64 = (FIXTURE_RETENTION_PERIOD - GAP_SYNC_BODY_SAFETY_MARGIN) as u64;
/// Caps the sync node's gap-sync request size (`--max-blocks-per-request`).
///
/// Gap sync strips bodies only from request ranges that lie *entirely* at or below the
/// body cutoff; a range straddling the cutoff downloads bodies for *all* of its blocks
/// (it is never split). So bodies can leak up to one request-range below the cutoff.
/// Bounding the request size bounds that leak, and the matching margin baked into
/// `stores_below_window` keeps every store provably header-only regardless of how the
/// gap ranges happen to align with the cutoff.
const SYNC_NODE_MAX_BLOCKS_PER_REQUEST: u64 = 16;
/// Timeout for the chain to finalize past
/// `last_store_block + GAP_SYNC_BODY_WINDOW + SYNC_NODE_MAX_BLOCKS_PER_REQUEST` before the
/// sync node is added (~115 blocks; parachain finality advances at roughly one block per
/// ~10s once relay-chain finality lag is included).
const GAP_WINDOW_ADVANCE_TIMEOUT_SECS: u64 = 1200;
const SESSION_CHANGE_TIMEOUT_SECS: u64 = 300;
const BITSWAP_RPC_POLL_TIMEOUT_SECS: u64 = 600;
const RENEW_BLOCK_SYNC_TIMEOUT_SECS: u64 = 600;
const RENEW_BATCH_SIZE: usize = 5;

type Entry = ([u8; 32], HashingAlgorithm);

fn verify_metadata(metadata: &super::fixture::SnapshotMetadata) -> Result<()> {
	anyhow::ensure!(metadata.total_blocks == TIP_SYNC_TARGET_BLOCKS);
	anyhow::ensure!(metadata.retention_period == FIXTURE_RETENTION_PERIOD);
	anyhow::ensure!(metadata.n_stores == N_STORES);
	anyhow::ensure!(N_RENEW_EXERCISES <= metadata.n_stores);
	Ok(())
}

async fn add_sync_node(
	network: &mut zombienet_sdk::Network<zombienet_sdk::LocalFileSystem>,
) -> Result<()> {
	network
		.add_collator(
			"sync-node",
			AddCollatorOptions {
				command: Some(PARACHAIN_BINARY.try_into()?),
				args: vec![
					"--sync=warp".into(),
					"--ipfs-server".into(),
					format!("--blocks-pruning={WARP_PRUNING_BLOCKS}").as_str().into(),
					format!("--max-blocks-per-request={SYNC_NODE_MAX_BLOCKS_PER_REQUEST}")
						.as_str()
						.into(),
					NODE_LOG_CONFIG.into(),
				],
				is_validator: false,
				..Default::default()
			},
			PARA_ID,
		)
		.await?;
	Ok(())
}

async fn assert_missing_before_renewal(sync_node: &NetworkNode, entries: &[Entry]) -> Result<()> {
	for (i, (hash, algo)) in entries.iter().enumerate() {
		let cid = hash_to_cid(hash, *algo);
		expect_dont_have(sync_node, &cid, Duration::from_secs(BITSWAP_RPC_POLL_TIMEOUT_SECS))
			.await
			.with_context(|| format!("pre-renewal: sync-node should not have entry {i} ({cid})"))?;
	}
	Ok(())
}

async fn renew_entries(
	collator_client: &OnlineClient<PolkadotConfig>,
	collator: &NetworkNode,
	sync_node: &NetworkNode,
	entries: &[Entry],
) -> Result<Vec<Entry>> {
	let nonce = collator_client
		.tx()
		.account_nonce(
			&zombienet_sdk::subxt_signer::sr25519::dev::bob().public_key().to_account_id(),
		)
		.await?;
	let mut renewed = Vec::with_capacity(entries.len());

	for (batch_idx, chunk) in entries.chunks(RENEW_BATCH_SIZE).enumerate() {
		let batch_start = batch_idx * RENEW_BATCH_SIZE;
		let batch_outcomes = try_join_all(chunk.iter().copied().enumerate().map(
			|(local_offset, (hash, algo))| async move {
				let global_idx = batch_start + local_offset;
				let batch_nonce = nonce + global_idx as u64;
				let outcome = renew_data_with_content_hash(collator_client, hash, batch_nonce)
					.await
					.with_context(|| {
						format!("renewing entry {global_idx} (hash={})", hex::encode(hash))
					})?;
				Ok::<_, anyhow::Error>((global_idx, hash, algo, batch_nonce, outcome))
			},
		))
		.await?;

		let max_renewed_block = batch_outcomes
			.iter()
			.map(|(_, _, _, _, outcome)| outcome.renewed_at_block)
			.max()
			.context("renew batch produced no outcomes")?;

		for (global_idx, hash, algo, batch_nonce, outcome) in batch_outcomes {
			log::info!(
				"Renew batch {} entry {}/{}: algo={:?}, nonce={}, block={}, index={}",
				batch_idx + 1,
				global_idx + 1,
				entries.len(),
				algo,
				batch_nonce,
				outcome.renewed_at_block,
				outcome.renewed_index,
			);
			renewed.push((hash, algo));
		}

		collator
			.wait_metric_with_timeout(
				FINALIZED_BLOCK_METRIC,
				|height| height >= max_renewed_block as f64,
				BLOCK_PRODUCTION_TIMEOUT_SECS,
			)
			.await
			.context(format!("Node did not finalize block height {max_renewed_block}"))?;
		sync_node
			.wait_metric_with_timeout(
				BEST_BLOCK_METRIC,
				|height| height >= max_renewed_block as f64,
				RENEW_BLOCK_SYNC_TIMEOUT_SECS,
			)
			.await
			.context(format!("Node did not reach block height {max_renewed_block}"))?;
	}

	Ok(renewed)
}

async fn assert_served_after_renewal(sync_node: &NetworkNode, renewed: &[Entry]) -> Result<()> {
	let deadline = std::time::Instant::now() + Duration::from_secs(BITSWAP_RPC_POLL_TIMEOUT_SECS);
	loop {
		let mut served = 0;
		for (hash, algo) in renewed {
			let cid = hash_to_cid(hash, *algo);
			if matches!(bitswap_v1_get(sync_node, &cid).await, Ok(Some(bytes)) if algo.hash(&bytes) == *hash)
			{
				served += 1;
			}
		}

		if served == renewed.len() {
			break;
		}
		if std::time::Instant::now() >= deadline {
			return Err(anyhow!(
				"post-renewal: sync-node served only {served} of {} entries",
				renewed.len()
			));
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}

	for i in 0..N_RENEW_EXERCISES {
		let cid = hash_to_cid(&content_hash(i), algorithm(i));
		let Some(bytes) = bitswap_v1_get(sync_node, &cid).await? else {
			anyhow::bail!("bitswap_v1_get returned None for entry {i} after successful poll loop");
		};
		anyhow::ensure!(bytes == payload(i), "bitswap returned bytes do not match payload({i})");
	}

	Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn parachain_tip_sync_with_renewals_test() -> Result<()> {
	let _ = env_logger::Builder::from_env(Env::default().default_filter_or("info")).try_init();

	let snapshots = ResolvedSnapshots::load()?;
	verify_metadata(&snapshots.metadata)?;
	log::info!(
		"Loaded snapshot metadata: target={}, snapshot={}, stores={} ({}..{})",
		snapshots.metadata.total_blocks,
		snapshots.metadata.snapshot_height,
		snapshots.metadata.n_stores,
		snapshots.metadata.first_store_block,
		snapshots.metadata.last_store_block,
	);

	let config = build_parachain_network_config(
		vec!["--ipfs-server".into(), NODE_LOG_CONFIG.into()],
		Some(snapshots.as_parachain_snapshots()),
	)?;
	let mut network = initialize_network(config).await?;
	network.wait_until_is_up(NETWORK_READY_TIMEOUT_SECS).await?;

	let alice = network.get_node("alice")?;
	wait_for_session_change_on_node(alice, SESSION_CHANGE_TIMEOUT_SECS).await?;

	{
		let collator = network.get_node("collator-1")?;
		let snapshot_height = collator
			.reports(BEST_BLOCK_METRIC)
			.await
			.context("Failed to read best block metric")? as u64;
		let target_height = snapshot_height + 1;
		collator
			.wait_metric_with_timeout(
				BEST_BLOCK_METRIC,
				|height| height >= target_height as f64,
				BLOCK_PRODUCTION_TIMEOUT_SECS,
			)
			.await
			.context(format!("Node did not reach block height {target_height}"))?;

		// The sync node backfills gap bodies for the last `GAP_SYNC_BODY_WINDOW`
		// finalized blocks and would serve the original stores via bitswap if they
		// fell inside that window. Wait until finality has moved every store block
		// below the window, so the warp-synced node provably lacks the stored data
		// and the renewals below have to be fetched via bitswap.
		//
		// The extra `SYNC_NODE_MAX_BLOCKS_PER_REQUEST` margin accounts for gap sync
		// downloading bodies for a whole request range that straddles the cutoff: with
		// the cutoff at least one request range above `last_store_block`, the lowest
		// body-carrying range starts strictly above the stores, so they stay header-only.
		let stores_below_window = snapshots.metadata.last_store_block +
			GAP_SYNC_BODY_WINDOW +
			SYNC_NODE_MAX_BLOCKS_PER_REQUEST;
		collator
			.wait_metric_with_timeout(
				FINALIZED_BLOCK_METRIC,
				|height| height >= stores_below_window as f64,
				GAP_WINDOW_ADVANCE_TIMEOUT_SECS,
			)
			.await
			.context(format!(
				"Node did not finalize past {stores_below_window} to move the stores \
				 out of the gap sync body window"
			))?;
	}

	add_sync_node(&mut network).await?;
	let collator = network.get_node("collator-1")?;
	let sync_node = network.get_node("sync-node")?;
	sync_node
		.wait_metric_with_timeout(
			NODE_ROLE_METRIC,
			|role| role == FULLNODE_ROLE_VALUE,
			METRIC_TIMEOUT_SECS,
		)
		.await
		.context("Node did not become full node")?;
	wait_for_relay_chain_to_sync(sync_node, SYNC_TIMEOUT_SECS).await?;

	let warp_target = collator
		.reports(BEST_BLOCK_METRIC)
		.await
		.context("Failed to read best block metric")? as u64;
	sync_node
		.wait_metric_with_timeout(
			BEST_BLOCK_METRIC,
			|height| height >= warp_target as f64,
			SYNC_TIMEOUT_SECS,
		)
		.await
		.context(format!("Node did not reach block height {warp_target}"))?;
	verify_warp_sync_completed(sync_node).await?;

	let entries: Vec<Entry> =
		(0..N_RENEW_EXERCISES).map(|i| (content_hash(i), algorithm(i))).collect();
	assert_missing_before_renewal(sync_node, &entries).await?;

	let collator_client: OnlineClient<PolkadotConfig> = collator.wait_client().await?;
	let renewed = renew_entries(&collator_client, collator, sync_node, &entries).await?;
	assert_served_after_renewal(sync_node, &renewed).await?;

	expect_log_line(
		sync_node,
		"storage-chain-fetcher.*fetched .* bytes for",
		10,
		"sync-node did not log a successful bitswap fetch via storage-chain-fetcher; \
		 renewals appeared to succeed but the data may have arrived through another path",
	)
	.await?;

	expect_no_log_line(collator, "(?i)bitswap.*hash.mismatch", 10, "collator hash mismatch")
		.await?;
	expect_no_log_line(sync_node, "(?i)bitswap.*hash.mismatch", 10, "sync-node hash mismatch")
		.await?;

	network.destroy().await?;
	Ok(())
}
