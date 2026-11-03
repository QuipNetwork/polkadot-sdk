// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Test that verifies correct claim queue behavior across coretime assignment boundaries.
//!
//! This test ensures that when a core's assignment changes from one set of parachains to another,
//! the scheduler's claim queue correctly reflects the transition at the boundary block.
//!
//! Before the peek-assigner changes, the claim queue would be populated with the old assignment
//! even after crossing the boundary. With peek functionality, the scheduler can look ahead and
//! properly handle assignment transitions.
//!
//! The claim queue has a lookahead depth L. At block N, it contains assignments for [N+1, ...,
//! N+L]. With lookahead=5 and boundary at block B (A assigned for 0..B-1, B assigned for B+):
//! - Block B-L-1: claim queue = [A, A, A, A, A]
//! - Block B-L:   claim queue = [A, A, A, A, B] ← First B appears!
//! - Block B-L+1: claim queue = [A, A, A, B, B]
//! - ...
//! - Block B-1:   claim queue = [B, B, B, B, B]
//! - Block B+:    claim queue = [B, B, B, B, B]

use crate::utils::maybe_enable_experimental_collator_protocol;
use anyhow::anyhow;
use codec::Decode;
use polkadot_primitives::{CoreIndex, Id as ParaId};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use subxt::{ext::scale_value::value, OnlineClient, PolkadotConfig};

use zombienet_sdk::{subxt, subxt_signer::sr25519::dev, NetworkConfig, NetworkConfigBuilder};

const PARA_A: u32 = 2000;
const PARA_B: u32 = 2001;
const LOOKAHEAD: u32 = 5;
/// How far ahead of the current claim queue window to place the boundary.
/// This ensures `assign_core(begin=boundary)` is never auto-adjusted by the stable claim queue
/// invariant.
const BOUNDARY_MARGIN: u32 = 15;

#[tokio::test(flavor = "multi_thread")]
async fn coretime_assignment_boundary_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	log::info!("Building network configuration");
	let config = build_network_config().await?;

	log::info!("Spawning network");
	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let mut network = spawn_fn(config).await?;

	let relay_alice = network.get_node("alice")?;
	let relay_client: OnlineClient<PolkadotConfig> = relay_alice.wait_client().await?;
	let alice = dev::alice();

	log::info!("Registering both parachains");
	network.register_parachain(PARA_A).await?;
	network.register_parachain(PARA_B).await?;

	log::info!(
		"Both parachains registered (not yet onboarded - will be activated via core assignment)"
	);

	// Assign core 0 to para A starting from block 0.
	log::info!("Assigning core 0 to para {} (initial assignment)", PARA_A);
	let assign_core_a = subxt::tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			Coretime(assign_core { core: 0, begin: 0, assignment: ((Task(PARA_A), 57600)), end_hint: None() })
		}],
	);
	relay_client
		.tx()
		.sign_and_submit_then_watch_default(&assign_core_a, &alice)
		.await?
		.wait_for_finalized_success()
		.await?;

	// Determine the boundary dynamically based on the current chain height.
	// `assign_core()` auto-adjusts `begin` forward if it falls within the current claim queue
	// window (block_number + 1 + lookahead). We place the boundary well beyond that window.
	let current_block = relay_client.blocks().at_latest().await?.number();
	let boundary = current_block + 1 + LOOKAHEAD + BOUNDARY_MARGIN;

	log::info!(
		"Chain is at block {}. Setting boundary at block {} (current + 1 + lookahead + margin = {} + 1 + {} + {})",
		current_block,
		boundary,
		current_block,
		LOOKAHEAD,
		BOUNDARY_MARGIN
	);

	// Assign core 0 to para B from the boundary block onwards.
	log::info!("Assigning core 0 to para {} for blocks {}+", PARA_B, boundary);
	let assign_core_b = subxt::tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			Coretime(assign_core { core: 0, begin: boundary, assignment: ((Task(PARA_B), 57600)), end_hint: None() })
		}],
	);
	relay_client
		.tx()
		.sign_and_submit_then_watch_default(&assign_core_b, &alice)
		.await?
		.wait_for_finalized_success()
		.await?;

	log::info!(
		"Core assignments configured: A for blocks 0-{}, B for blocks {}+",
		boundary - 1,
		boundary
	);

	// Build expected claim queue transitions dynamically.
	// At block N, claim queue = [N+1, N+2, ..., N+L] (for lookahead=L).
	// The transition happens when the claim queue window starts overlapping the boundary.
	let mut expected_transitions: HashMap<u32, Vec<ParaId>> = HashMap::new();

	// Block before transition window: all A
	let pre_transition = boundary - LOOKAHEAD - 1;
	expected_transitions.insert(pre_transition, vec![ParaId::from(PARA_A); LOOKAHEAD as usize]);

	// Transition blocks: B gradually replaces A from the end
	for i in 0..LOOKAHEAD {
		let block = boundary - LOOKAHEAD + i;
		let num_a = (LOOKAHEAD - 1 - i) as usize;
		let num_b = (1 + i) as usize;
		let mut queue = vec![ParaId::from(PARA_A); num_a];
		queue.extend(vec![ParaId::from(PARA_B); num_b]);
		expected_transitions.insert(block, queue);
	}

	// After boundary: all B
	expected_transitions.insert(boundary + 4, vec![ParaId::from(PARA_B); LOOKAHEAD as usize]);

	log::info!("Monitoring claim queue transitions around boundary (block {})...", boundary);
	for (block, expected) in expected_transitions.iter() {
		log::debug!("  Expected at block {}: {:?}", block, expected);
	}

	let mut blocks_sub = relay_client.blocks().subscribe_finalized().await?;
	let mut verified_blocks = HashSet::new();

	while let Some(block_result) = blocks_sub.next().await {
		let block = block_result?;
		let block_number = block.number();

		// Query the claim queue for this block
		let claim_queue = BTreeMap::<CoreIndex, VecDeque<ParaId>>::decode(
			&mut &relay_client
				.runtime_api()
				.at(block.hash())
				.call_raw("ParachainHost_claim_queue", None)
				.await?[..],
		)?;

		if let Some(queue) = claim_queue.get(&CoreIndex(0)) {
			let queue_vec: Vec<ParaId> = queue.iter().copied().collect();

			log::info!(
				"Block #{}: Claim queue = {:?} (predicting blocks {}-{})",
				block_number,
				queue_vec,
				block_number + 1,
				block_number + LOOKAHEAD
			);

			// Check if this block is one we want to verify
			if let Some(expected_queue) = expected_transitions.get(&block_number) {
				if &queue_vec == expected_queue {
					log::info!(
						"  ✓ Block {}: Claim queue matches expected {:?}",
						block_number,
						expected_queue
					);
					verified_blocks.insert(block_number);
				} else {
					return Err(anyhow!(
						"FAIL: At block {}, expected claim queue {:?} but got {:?}.\n\
						Boundary is at block {}.\n\
						This indicates the peek-assigner is not working correctly!",
						block_number,
						expected_queue,
						queue_vec,
						boundary
					));
				}
			}
		} else {
			log::debug!("Block #{}: No claim queue entry for core 0", block_number);
		}

		// Check if we've verified all required blocks
		let required_blocks: HashSet<_> = expected_transitions.keys().copied().collect();

		if required_blocks.is_subset(&verified_blocks) {
			log::info!("All claim queue transitions verified successfully!");
			break;
		}

		// Safety: don't wait forever
		if block_number > boundary + 10 {
			let missing: Vec<_> = required_blocks.difference(&verified_blocks).collect();
			return Err(anyhow!(
				"Failed to verify all expected claim queue transitions. Missing blocks: {:?}\n\
				Verified: {:?}\n\
				Boundary: {}\n\
				This suggests the test didn't observe all required blocks.",
				missing,
				verified_blocks,
				boundary
			));
		}
	}

	log::info!("Test completed successfully!");
	log::info!("The peek-assigner correctly populates the claim queue by looking ahead at upcoming assignments.");

	Ok(())
}

async fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	log::info!("Using images: {images:?}");

	// Use asset-hub-rococo-local as the base chain spec for both parachains.
	// Asset Hub is the standard system parachain used for testing and provides a minimal,
	// well-tested runtime without unnecessary overhead (unlike Glutton which is designed
	// for stress testing and intentionally consumes weight).
	let chain_a = "asset-hub-rococo-local";
	let chain_b = "asset-hub-rococo-local";

	// Network setup:
	// - Relay chain with 4 validators
	// - Two parachains that will be assigned to the same core at different times (partitioning)
	// - Lookahead set to 5 to match production scenario
	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(maybe_enable_experimental_collator_protocol(vec![
					("-lruntime=debug").into(),
					("-lparachain=debug").into(),
					("-lruntime::parachains::scheduler=trace").into(),
				]))
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"max_validators_per_core": 1,
								"num_cores": 1,
								"lookahead": 5
							}
						}
					}
				}))
				.with_validator(|node| node.with_name("alice"))
				.with_validator(|node| node.with_name("bob"))
				.with_validator(|node| node.with_name("charlie"))
				.with_validator(|node| node.with_name("dave"))
		})
		.with_parachain(|p| {
			p.with_id(PARA_A)
				.with_chain(chain_a)
				.with_default_command("polkadot-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_registration_strategy(zombienet_sdk::RegistrationStrategy::Manual)
				.onboard_as_parachain(false)
				.with_default_args(vec![("-lparachain=debug").into()])
				.with_collator(|n| n.with_name("collator-a").validator(true))
		})
		.with_parachain(|p| {
			p.with_id(PARA_B)
				.with_chain(chain_b)
				.with_default_command("polkadot-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_registration_strategy(zombienet_sdk::RegistrationStrategy::Manual)
				.onboard_as_parachain(false)
				.with_default_args(vec![("-lparachain=debug").into()])
				.with_collator(|n| n.with_name("collator-b").validator(true))
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
