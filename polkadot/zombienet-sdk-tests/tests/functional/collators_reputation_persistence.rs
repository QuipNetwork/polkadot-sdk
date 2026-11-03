// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Comprehensive Collator Reputation Persistence Test
//!
//! This test verifies multiple aspects of the collator reputation persistence system:
//! 1. Basic persistence on graceful shutdown
//! 2. Startup lookback with different gap sizes
//! 3. Pruning when parachains are deregistered
//!
//! ## Test Phases
//!
//! ### Phase 1: Large Gap Lookback (>= 20 blocks)
//! - Spawn network with 2 parachains
//! - Wait for initial persistence
//! - Pause validator-0 to create a 30+ block gap
//! - Restart and verify lookback processes MAX_STARTUP_ANCESTRY_LOOKBACK blocks
//!
//! ### Phase 2: Small Gap Lookback (< 20 blocks)
//! - Pause validator-0 again
//! - Create a ~10 block gap
//! - Restart and verify lookback processes the entire gap
//!
//! ### Phase 3: Pruning on Parachain Deregistration
//! - Deregister parachain 2001 using sudo
//! - Wait for session change (triggers pruning)
//! - Verify pruning logs show para 2001 removed
//! - Restart validator-0
//! - Verify only para 2000's reputation was loaded
//!
//! ## Success Criteria
//! - All persistence and restart operations succeed
//! - Lookback correctly handles both large and small gaps
//! - Pruning removes deregistered parachain data
//! - System continues normal operation after each phase

use anyhow::anyhow;
use regex::Regex;
use tokio::time::Duration;

use cumulus_zombienet_sdk_helpers::{assert_para_throughput, wait_for_first_session_change};
use polkadot_primitives::Id as ParaId;
use serde_json::json;
use zombienet_orchestrator::network::node::LogLineCountOptions;
use zombienet_sdk::{
	subxt::{ext::scale_value::value, OnlineClient, PolkadotConfig},
	subxt_signer::sr25519::dev,
	NetworkConfigBuilder,
};

const PARA_ID_1: u32 = 2000;
const PARA_ID_2: u32 = 2001;

#[tokio::test(flavor = "multi_thread")]
async fn comprehensive_reputation_persistence_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	// === Network Setup ===
	let images = zombienet_sdk::environment::get_images_from_env();

	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![
					("-lparachain=debug,parachain::collator-protocol=trace").into(),
					("--experimental-collator-protocol").into(),
					("--collator-reputation-persist-interval").into(),
					("30").into(),
				])
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"group_rotation_frequency": 4,
								"num_cores": 2
							}
						}
					}
				}))
				.with_validator(|node| node.with_name("validator-0"));

			(1..4).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-{i}")))
			})
		})
		.with_parachain(|p| {
			p.with_id(PARA_ID_1)
				.with_default_command("undying-collator")
				.cumulus_based(false)
				.with_default_image(
					std::env::var("COL_IMAGE")
						.unwrap_or("docker.io/paritypr/colander:latest".to_string())
						.as_str(),
				)
				.with_default_args(vec![
					("-lparachain=debug").into(),
					("--experimental-send-approved-peer").into(),
				])
				.with_collator(|n| n.with_name("collator-1"))
		})
		.with_parachain(|p| {
			p.with_id(PARA_ID_2)
				.with_default_command("undying-collator")
				.cumulus_based(false)
				.with_default_image(
					std::env::var("COL_IMAGE")
						.unwrap_or("docker.io/paritypr/colander:latest".to_string())
						.as_str(),
				)
				.with_default_args(vec![
					("-lparachain=debug").into(),
					("--experimental-send-approved-peer").into(),
				])
				.with_collator(|n| n.with_name("collator-2"))
		})
		.with_global_settings(|global_settings| {
			let global_settings = match std::env::var("ZOMBIENET_SDK_BASE_DIR") {
				Ok(val) => global_settings.with_base_dir(val),
				_ => global_settings,
			};
			global_settings.with_tear_down_on_failure(false)
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let validator_0 = network.get_node("validator-0")?;
	let validator0_client: OnlineClient<PolkadotConfig> = validator_0.wait_client().await?;

	// Verify fresh start (no existing data)
	verify_db_initialized(validator_0, 1, 0).await?;

	log::info!("Network spawned, waiting for both parachains to produce blocks");
	assert_para_throughput(
		&validator0_client,
		10,
		[(ParaId::from(PARA_ID_1), 8..11), (ParaId::from(PARA_ID_2), 8..11)],
		[],
	)
	.await?;

	// Wait for initial persistence
	log::info!("Parachains producing blocks, waiting for initial periodic persistence");
	wait_for_persistence(validator_0, 1).await?;

	log::info!("Pausing validator-0 to create a block gap");
	validator_0.pause().await?;

	let block_at_persistence = extract_last_finalized_from_logs(validator_0).await?;
	log::info!("Initial persistence completed at finalized block {}", block_at_persistence);

	log::info!("=== Phase 1: Testing Startup Lookback with Large Gap (>= 20 blocks) ===");

	let validator_1 = network.get_node("validator-1")?;
	let validator_1_client: OnlineClient<PolkadotConfig> = validator_1.wait_client().await?;
	let mut finalized_blocks_1 = validator_1_client.blocks().subscribe_finalized().await?;

	let target_gap = 30u32;
	let block_at_restart =
		wait_for_block_gap(&mut finalized_blocks_1, block_at_persistence, target_gap, "large gap")
			.await?;

	log::info!("Restarting validator-0 (first restart - large gap)");
	validator_0.restart(None).await?;
	let _: OnlineClient<PolkadotConfig> = validator_0.wait_client().await?;

	// Verify loaded with both paras' reputation
	verify_db_initialized(validator_0, 2, 2).await?;

	let blocks_processed = verify_lookback_completed(validator_0, 1).await?;
	assert_eq!(
		blocks_processed, 20,
		"Expected blocks_processed ({blocks_processed}) == MAX_STARTUP_ANCESTRY_LOOKBACK (20)",
	);
	log::info!(
		"Phase 1 passed: Lookback processed {blocks_processed} blocks (capped at MAX), actual gap was {}",
		block_at_restart.saturating_sub(block_at_persistence)
	);

	let relay_client: OnlineClient<PolkadotConfig> = validator_0.wait_client().await?;
	wait_for_peer_reconnection(&relay_client).await?;

	// Verify validator resumes normal operation
	assert_para_throughput(
		&relay_client,
		5,
		[(ParaId::from(PARA_ID_1), 3..7), (ParaId::from(PARA_ID_2), 3..7)],
		[],
	)
	.await?;

	log::info!("=== Phase 2: Testing Startup Lookback with Small Gap (< 20 blocks) ===");

	// Wait for another persistence to get a precise starting point
	wait_for_persistence(validator_0, 2).await?;

	validator_0.pause().await?;
	log::info!("Pausing validator-0 again to create a smaller gap");

	let block_before_second_pause = extract_last_finalized_from_logs(validator_0).await?;
	log::info!("Second persistence completed at finalized block {}", block_before_second_pause);

	// Fresh subscription to avoid stale buffered blocks from Phase 1
	let mut finalized_blocks_2 = validator_1_client.blocks().subscribe_finalized().await?;

	let small_gap_target = 10u32;
	let block_at_second_restart = wait_for_block_gap(
		&mut finalized_blocks_2,
		block_before_second_pause,
		small_gap_target,
		"small gap",
	)
	.await?;

	log::info!("Restarting validator-0 (second restart - small gap)");
	validator_0.restart(None).await?;
	let validator0_client: OnlineClient<PolkadotConfig> = validator_0.wait_client().await?;
	wait_for_peer_reconnection(&validator0_client).await?;

	// Verify loaded with both paras' reputation
	verify_db_initialized(validator_0, 3, 2).await?;
	let processed_second = verify_lookback_completed(validator_0, 2).await?;
	let expected_gap = block_at_second_restart.saturating_sub(block_before_second_pause);

	assert!(expected_gap < 20, "Expected second gap to be < 20, but got {expected_gap}");

	// The key invariant: lookback should NOT be capped at MAX (20),
	// and should process approximately the gap. Cross-node timing
	// differences mean the exact value can drift, so use wide bounds.
	assert!(
		processed_second < 20,
		"Small gap should process fewer than MAX_STARTUP_ANCESTRY_LOOKBACK blocks, got {processed_second}",
	);
	assert!(
		processed_second >= expected_gap.saturating_sub(6),
		"Expected lookback to process approximately {expected_gap} blocks, got {processed_second}",
	);

	log::info!(
		"Phase 2 passed: Lookback processed {} blocks (entire gap of {})",
		processed_second,
		expected_gap
	);

	log::info!("=== Phase 3: Testing Pruning on Parachain Deregistration ===");

	// Wait for another persistence to ensure both paras are on disk
	wait_for_persistence(validator_0, 4).await?;

	// Verify both paras have reputation before pruning
	let para_count_before = extract_para_count_from_persistence_logs(validator_0).await?;
	log::info!("Before pruning: para_count={}", para_count_before);
	assert_eq!(
		para_count_before, 2,
		"Expected 2 paras with reputation before pruning, but found {para_count_before}",
	);

	// Deregister parachain 2001
	log::info!(
		"Deregistering parachain {} using ParasSudoWrapper::sudo_schedule_para_cleanup",
		PARA_ID_2
	);
	let alice = dev::alice();
	let cleanup_calls = vec![
		value! {
			ParasSudoWrapper(sudo_schedule_para_cleanup { id: PARA_ID_2 })
		},
		value! {
			Paras(force_queue_action { para: PARA_ID_2 })
		},
	];
	let sudo_batch_call = zombienet_sdk::subxt::tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			Utility(batch_all { calls: cleanup_calls })
		}],
	);

	let tx_progress = validator0_client
		.tx()
		.sign_and_submit_then_watch_default(&sudo_batch_call, &alice)
		.await?;
	let _finalized = tx_progress.wait_for_finalized_success().await?;
	log::info!("Para cleanup scheduled successfully");

	// Stop the collator for para 2001
	log::info!("Stopping collator-2 for the deregistered parachain {}", PARA_ID_2);
	let collator_2 = network.get_node("collator-2")?;
	collator_2.pause().await?;

	// Wait for session change to trigger pruning
	log::info!("Waiting for session change to trigger pruning");
	let mut best_blocks = validator0_client.blocks().subscribe_best().await?;
	wait_for_first_session_change(&mut best_blocks).await?;
	log::info!("Session change detected, para {} should now be offboarded", PARA_ID_2);

	// Verify pruning happened
	verify_pruning(validator_0).await?;
	log::info!("Pruning verified: pruned 1 para, 1 remaining");

	// Restart validator-0 to verify only para 2000's reputation loads
	log::info!("Restarting validator-0 to verify pruned state persisted");
	validator_0.restart(None).await?;
	let validator0_client_after: OnlineClient<PolkadotConfig> = validator_0.wait_client().await?;
	wait_for_peer_reconnection(&validator0_client_after).await?;

	// Verify loaded with only para 2000's reputation (para 2001 pruned)
	verify_db_initialized(validator_0, 4, 1).await?;

	// Double-check via log parsing (redundant but shows consistency)
	let para_count_after = extract_para_count_from_init_logs(validator_0).await?;
	log::info!("After restart: para_count={}", para_count_after);
	assert!(
		para_count_after <= 1,
		"Expected at most 1 para after pruning, but found {para_count_after}",
	);

	// Verify para 2000 continues normal operation
	log::info!("Verifying para {} continues normal operation", PARA_ID_1);
	assert_para_throughput(&validator0_client_after, 5, [(ParaId::from(PARA_ID_1), 3..7)], [])
		.await?;

	log::info!("Phase 3 passed: Pruning successfully removed deregistered parachain");
	Ok(())
}

// === Helper Functions ===

/// Wait for a few finalized blocks to allow peers to reconnect after a node restart.
async fn wait_for_peer_reconnection(
	client: &OnlineClient<PolkadotConfig>,
) -> Result<(), anyhow::Error> {
	log::info!("Waiting for peers to reconnect after restart");
	let mut blocks = client.blocks().subscribe_finalized().await?;
	for _ in 0..3 {
		let _ = blocks.next().await;
	}
	Ok(())
}

async fn verify_db_initialized(
	validator: &zombienet_sdk::NetworkNode,
	expected_count: u32,
	expected_para_count: u32,
) -> Result<(), anyhow::Error> {
	let result = validator
		.wait_log_line_count_with_timeout(
			"Reputation DB initialized",
			false,
			LogLineCountOptions::new(move |n| n >= expected_count, Duration::from_secs(60), false),
		)
		.await?;
	assert!(
		result.success(),
		"Expected validator to log 'Reputation DB initialized' (count >= {expected_count})",
	);

	// Parse and verify para_count
	let logs = validator.logs().await?;
	let init_re = Regex::new(r"Reputation DB initialized.*para_count=(\d+)")?;

	let mut para_count: Option<u32> = None;
	for line in logs.lines().rev() {
		if let Some(caps) = init_re.captures(line) {
			para_count = caps.get(1).and_then(|m| m.as_str().parse().ok());
			if para_count.is_some() {
				break;
			}
		}
	}

	let actual = para_count.ok_or(anyhow!("Could not parse para_count from init log"))?;
	assert_eq!(
		actual, expected_para_count,
		"Expected para_count={expected_para_count}, but got {actual}",
	);
	log::info!("DB initialization verified: para_count={}", actual);

	Ok(())
}

async fn wait_for_persistence(
	validator: &zombienet_sdk::NetworkNode,
	expected_count: u32,
) -> Result<(), anyhow::Error> {
	let result = validator
		.wait_log_line_count_with_timeout(
			"Periodic persistence completed:",
			false,
			LogLineCountOptions::new(move |n| n >= expected_count, Duration::from_secs(60), false),
		)
		.await?;
	assert!(
		result.success(),
		"Periodic persistence should have completed (count >= {expected_count})",
	);
	Ok(())
}

async fn verify_lookback_completed(
	validator: &zombienet_sdk::NetworkNode,
	expected_count: u32,
) -> Result<u32, anyhow::Error> {
	let result = validator
		.wait_log_line_count_with_timeout(
			"Startup lookback completed",
			false,
			LogLineCountOptions::new(move |n| n >= expected_count, Duration::from_secs(30), false),
		)
		.await?;
	assert!(
		result.success(),
		"Expected 'Startup lookback completed' log (count >= {expected_count})",
	);

	let logs = validator.logs().await?;
	let lookback_re = Regex::new(r"Startup lookback completed.*blocks_processed=(\d+)")?;

	// Find the last occurrence (most recent)
	let mut blocks_processed: Option<u32> = None;
	for line in logs.lines().rev() {
		if let Some(caps) = lookback_re.captures(line) {
			blocks_processed = caps.get(1).and_then(|m| m.as_str().parse().ok());
			if blocks_processed.is_some() {
				break;
			}
		}
	}

	blocks_processed.ok_or(anyhow!("Could not parse blocks_processed from lookback log"))
}

async fn extract_last_finalized_from_logs(
	validator: &zombienet_sdk::NetworkNode,
) -> Result<u32, anyhow::Error> {
	let logs = validator.logs().await?;
	let persistence_re =
		Regex::new(r"Periodic persistence completed:.*last_finalized=Some\((\d+)\)")?;

	// Find the last occurrence
	let mut last_finalized: Option<u32> = None;
	for line in logs.lines().rev() {
		if let Some(caps) = persistence_re.captures(line) {
			last_finalized = caps.get(1).and_then(|m| m.as_str().parse().ok());
			if last_finalized.is_some() {
				break;
			}
		}
	}

	last_finalized.ok_or(anyhow!("Could not parse last_finalized from persistence log"))
}

async fn extract_para_count_from_persistence_logs(
	validator: &zombienet_sdk::NetworkNode,
) -> Result<u32, anyhow::Error> {
	let logs = validator.logs().await?;
	let para_count_re = Regex::new(r"Periodic persistence completed:.* para_count=(\d+)")?;

	let mut para_count: Option<u32> = None;
	for line in logs.lines().rev() {
		if let Some(caps) = para_count_re.captures(line) {
			para_count = caps.get(1).and_then(|m| m.as_str().parse().ok());
			if para_count.is_some() {
				break;
			}
		}
	}

	para_count.ok_or(anyhow!("Could not parse para_count from persistence log"))
}

async fn extract_para_count_from_init_logs(
	validator: &zombienet_sdk::NetworkNode,
) -> Result<u32, anyhow::Error> {
	let logs = validator.logs().await?;
	let init_re = Regex::new(r"Reputation DB initialized.*para_count=(\d+)")?;

	let mut para_count: Option<u32> = None;
	for line in logs.lines().rev() {
		if let Some(caps) = init_re.captures(line) {
			para_count = caps.get(1).and_then(|m| m.as_str().parse().ok());
			if para_count.is_some() {
				break;
			}
		}
	}

	para_count.ok_or(anyhow!("Could not parse para_count from init log"))
}

async fn wait_for_block_gap(
	finalized_blocks: &mut zombienet_sdk::subxt::backend::StreamOfResults<
		zombienet_sdk::subxt::blocks::Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
	>,
	start_block: u32,
	target_gap: u32,
	gap_name: &str,
) -> Result<u32, anyhow::Error> {
	log::info!("Waiting for {} blocks while validator-0 is paused", target_gap);
	let mut current_block = start_block;
	while current_block < start_block + target_gap {
		if let Some(Ok(block)) = finalized_blocks.next().await {
			current_block = block.number();
			log::info!(
				"Finalized block {} (gap: {})",
				current_block,
				current_block.saturating_sub(start_block)
			);
		}
	}
	log::info!(
		"{} created: finalized block now at {}, gap of {} blocks",
		gap_name,
		current_block,
		current_block.saturating_sub(start_block)
	);
	Ok(current_block)
}

async fn verify_pruning(validator: &zombienet_sdk::NetworkNode) -> Result<(), anyhow::Error> {
	let result = validator
		.wait_log_line_count_with_timeout(
			"Prune paras persisted to disk immediately pruned_para_count=1 remaining_para_count=1 registered_para_count=1",
			false,
			LogLineCountOptions::new(|n| n >= 1, Duration::from_secs(90), false),
		)
		.await?;
	assert!(
		result.success(),
		"Expected validator to log pruning with pruned=1, remaining=1, registered=1"
	);
	Ok(())
}
