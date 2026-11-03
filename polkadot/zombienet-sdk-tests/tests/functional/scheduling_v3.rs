// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Test that V3 candidate descriptors with scheduling_parent work correctly.
//!
//! Each test runs a mixed fleet of collators: a V3-capable collator (para 2700) alongside a
//! V2 collator (para 2500), verifying both descriptor versions
//! are backed and finalized by the same validator set.

use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::{
	assert_finality_lag, assert_para_throughput_with, assign_cores, wait_for_pvf_prepare,
};
use polkadot_primitives::{CandidateDescriptorVersion, Id as ParaId};
use rstest::rstest;
use serde_json::json;
use std::{collections::HashMap, ops::Range};
use zombienet_sdk::{
	subxt::{OnlineClient, PolkadotConfig},
	NetworkConfigBuilder,
};

use crate::utils::{assert_candidates_version, assert_validator_backed_candidates};

/// Test that spawns a relay chain with 2 parachains:
/// - a V2 parachain with async backing
/// - a V3 parachain with async backing
/// and checks that the candidates for both parachains are being backed at expected throughput.
///
/// RPO = relay parent offset
///
/// TODO: For the moment the v3 throughput is low and unpredictable. This should stabilise after
/// adding resubmissions support and a catch-up mechanism:
/// - https://github.com/paritytech/polkadot-sdk/issues/10836
/// - https://github.com/paritytech/polkadot-sdk/issues/11903
/// The expected throughput ranges should be adjusted after we have this.
#[rstest]
#[case::rpo_0_max_session_age_0("v3", 0, 20, 16..21)]
#[case::rpo_2_max_session_age_0("v3-rpo-2", 0, 20, 6..20)]
#[case::rpo_2_max_session_age_1("v3-rpo-2", 1, 40, 15..30)]
#[case::rpo_4_max_session_age_1("v3-rpo-4", 1, 40, 15..30)]
#[case::rpo_6_max_session_age_1("v3-rpo-6", 1, 40, 15..30)]
#[case::rpo_15_max_session_age_2("v3-rpo-15", 2, 60, 24..40)]
#[tokio::test(flavor = "multi_thread")]
async fn scheduling_v2_and_v3_collator_with_v3_validators(
	#[case] parachain: &str,
	#[case] max_relay_parent_session_age: u32,
	#[case] relay_blocks_count: u32,
	#[case] expected_throughput_v3: Range<u32>,
) -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let images = zombienet_sdk::environment::get_images_from_env();

	// V2 (bit 4) and V3 (bit 3) enabled
	let node_features_with_v3 = json!({"bits": 8, "data": [0b00011000]});

	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![("-lparachain=debug,runtime=debug,parachain::candidate-backing=debug,parachain::provisioner=debug,parachain::prospective-parachains=debug,runtime::parachains::scheduler=debug,parachain::collator-protocol=debug,basic-authorship=debug,parachain::statement-distribution=debug").into()])
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								"max_validators_per_core": 3,
							},
							"node_features": node_features_with_v3,
							"max_relay_parent_session_age": max_relay_parent_session_age,
						}
					}
				}))
				.with_validator(|node| node.with_name("validator-0"));

			let r = (1..3).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-{i}")))
			});

			// Experimental collator protocol validators.
			(3..6).fold(r, |acc, i| {
				acc.with_validator(|node| {
					node.with_name(&format!("validator-{i}")).with_args(vec![
						("--experimental-collator-protocol").into(),
					])
				})
			})
		})
		// Para 2700: V3-capable collator.
		.with_parachain(|p| {
			p.with_id(2700)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain(parachain)
				.with_default_args(vec![
					("-lparachain=debug,aura=debug,cumulus-collator=debug,parachain::collator-protocol=trace,parachain::collator-protocol::stats=trace,basic-authorship=debug,aura::cumulus=trace").into(),
					"--authoring=slot-based".into(),
				])
				.with_collator(|n| n.with_name("collator-2700"))
		})
		// Para 2500: V2 collator.
		.with_parachain(|p| {
			p.with_id(2500)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain("async-backing")
				.with_default_args(vec![
					("-lparachain=debug,aura=debug,cumulus-collator=debug").into(),
					"--authoring=slot-based".into(),
				])
				.with_collator(|n| n.with_name("collator-2500"))
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_node = network.get_node("validator-0")?;
	let para_v3_node = network.get_node("collator-2700")?;
	let para_v2_node = network.get_node("collator-2500")?;

	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;

	let para_v3 = ParaId::from(2700);
	let para_v2 = ParaId::from(2500);

	wait_for_pvf_prepare(&network, 2).await?;

	// Verify both V3 and V2 candidates are backed in the same relay chain block window.
	assert_para_throughput_with(
		&relay_client,
		relay_blocks_count,
		HashMap::from([
			(para_v2, relay_blocks_count * 9 / 10..relay_blocks_count + 1),
			(para_v3, expected_throughput_v3),
		]),
		|receipt| {
			let para_id = receipt.descriptor.para_id();
			let version = receipt.descriptor.version();
			log::info!(
				"Para {} candidate backed: version={:?}, relay_parent={:?}, \
				 session_index={:?}, scheduling_parent={:?}",
				para_id,
				version,
				receipt.descriptor.relay_parent(),
				receipt.descriptor.session_index(),
				receipt.descriptor.scheduling_parent(),
			);

			if para_id == para_v3 && version != CandidateDescriptorVersion::V3 {
				return Err(anyhow!("Para {} expected V3 candidate, got {:?}", para_id, version,));
			}
			if para_id == para_v2 && version != CandidateDescriptorVersion::V2 {
				return Err(anyhow!("Para {} expected V2 candidate, got {:?}", para_id, version,));
			}

			Ok(true)
		},
	)
	.await?;

	relay_node
		.wait_metric_with_timeout(
			"polkadot_parachain_candidate_disputes_total",
			|v| v == 0.0,
			30u64,
		)
		.await?;

	for i in 0..6 {
		let node = network.get_node(format!("validator-{i}"))?;
		assert_validator_backed_candidates(node, 30).await?;
	}

	assert_finality_lag(&para_v3_node.wait_client().await?, 10).await?;
	assert_finality_lag(&para_v2_node.wait_client().await?, 10).await?;

	log::info!("V3 scheduling test ({parachain}) finished successfully");
	Ok(())
}

/// Test that V3 candidates work correctly with elastic scaling (multiple cores).
///
/// This test assigns 3 cores to a single parachain and verifies that V3 candidates are
/// being backed at elastic scaling throughput.
#[tokio::test(flavor = "multi_thread")]
async fn scheduling_v3_es_collator_with_v3_validators() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let images = zombienet_sdk::environment::get_images_from_env();

	// V2 (bit 4) and V3 (bit 3) enabled
	let node_features_with_v3 = json!({"bits": 8, "data": [0b00011000]});

	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			let r = r
				.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec![("-lparachain=debug,runtime=debug,parachain::candidate-backing=debug,parachain::provisioner=debug,parachain::prospective-parachains=debug,runtime::parachains::scheduler=debug,parachain::collator-protocol=debug,basic-authorship=debug,parachain::statement-distribution=debug").into()])
				.with_genesis_overrides(json!({
					"configuration": {
						"config": {
							"scheduler_params": {
								// 2 extra cores to assign, plus 1 auto-assigned by zombienet
								"num_cores": 2,
								"max_validators_per_core": 2,
							},
							"node_features": node_features_with_v3,
						}
					}
				}))
				.with_validator(|node| node.with_name("validator-0"));

			let r = (1..3).fold(r, |acc, i| {
				acc.with_validator(|node| node.with_name(&format!("validator-{i}")))
			});

			// Experimental collator protocol validators.
			(3..6).fold(r, |acc, i| {
				acc.with_validator(|node| {
					node.with_name(&format!("validator-{i}")).with_args(vec![
						("--experimental-collator-protocol").into(),
					])
				})
			})
		})
		.with_parachain(|p| {
			p.with_id(2800)
				.with_default_command("test-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_chain("elastic-scaling-v3")
				.with_default_args(vec![
					("-lparachain=debug,aura=debug,cumulus-collator=debug,parachain::collator-protocol=trace,parachain::collator-protocol::stats=trace,basic-authorship=debug,aura::cumulus=trace").into(),
					"--authoring=slot-based".into(),
				])
				.with_collator(|n| n.with_name("collator-2800"))
		})
		.build()
		.map_err(|e| {
			let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
			anyhow!("config errs: {errs}")
		})?;

	let spawn_fn = zombienet_sdk::environment::get_spawn_fn();
	let network = spawn_fn(config).await?;

	let relay_node = network.get_node("validator-0")?;
	let para_node = network.get_node("collator-2800")?;

	let relay_client: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;

	// Assign 2 additional cores to the parachain (zombienet already assigns 1)
	assign_cores(&relay_client, 2800, vec![0, 1]).await?;

	// With 3 cores, expect at max 3 candidates per relay block → ~60 in 20 blocks.
	assert_candidates_version(
		&relay_client,
		CandidateDescriptorVersion::V3,
		HashMap::from([(ParaId::from(2800), 40..61)]),
		20,
	)
	.await?;

	for i in 0..6 {
		let node = network.get_node(format!("validator-{i}"))?;
		assert_validator_backed_candidates(node, 24).await?;
	}

	assert_finality_lag(&para_node.wait_client().await?, 15).await?;

	log::info!("V3 elastic scaling test finished successfully");
	Ok(())
}
