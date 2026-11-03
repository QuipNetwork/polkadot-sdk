// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Setup a network with 4 validators, one marked as `unstable` and will
//! be stopped and resumed.

use crate::utils::{
	assert_nodes_are_validators, check_metrics, env_or_default, initialize_network,
	maybe_enable_experimental_collator_protocol, MetricCheckSetup, INTEGRATION_IMAGE_ENV,
};
use anyhow::anyhow;
use cumulus_zombienet_sdk_helpers::wait_for_nth_session_change;
use futures::future::try_join_all;
use std::collections::HashMap;
use zombienet_sdk::{
	subxt::{
		self,
		ext::subxt_rpcs::client::{rpc_params, RpcParams},
		OnlineClient, PolkadotConfig,
	},
	NetworkConfig, NetworkConfigBuilder, NetworkNode,
};

#[tokio::test(flavor = "multi_thread")]
async fn beefy_and_mmr_test() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let config = build_network_config()?;
	let network = initialize_network(config).await?;

	let validator_nodes = network.relaychain().nodes();

	// Check authority status
	log::info!("Checking validator node roles");
	assert_nodes_are_validators(&validator_nodes).await?;
	log::info!("All validators confirmed as authorities");

	let metric_checks: Vec<MetricCheckSetup> = vec![
		// BEEFY sanity checks.
		("substrate_beefy_validator_set_id", Box::new(|v| v == 0.0), 0),
		// Verify voting happens and 1st mandatory block is finalized within 1st session.
		("substrate_beefy_best_block", Box::new(|v| v >= 1.0), 60),
	];

	check_metrics(&validator_nodes, &metric_checks).await?;

	log::info!("Pause validator-unstable and test chain is making progress without it.");
	let unstable_node = network.get_node("validator-unstable")?;
	unstable_node.pause().await?;

	let metric_checks: Vec<MetricCheckSetup> = vec![
		// Verify validator sets get changed on new sessions.
		("substrate_beefy_validator_set_id", Box::new(|v| v >= 1.0), 180),
		// Check next session too.
		("substrate_beefy_validator_set_id", Box::new(|v| v >= 2.0), 180),
		// Verify voting happens and blocks are being finalized for new sessions too:
		// since we verified we're at least in the 3rd session, verify BEEFY finalized mandatory
		// #21.
		("substrate_beefy_best_block", Box::new(|v| v >= 21.0), 180),
	];

	let stable_validators: Vec<&NetworkNode> = network
		.relaychain()
		.nodes()
		.into_iter()
		.filter(|v| v.name() != "validator-unstable")
		.collect();
	check_metrics(&stable_validators, &metric_checks).await?;

	log::info!("Test BEEFY RPCs.");
	// from original line: js-script ./0003-beefy-finalized-heads.js with
	// "validator-0,validator-1,validator-2")
	beefy_finalized_heads(&stable_validators, 21).await?;
	log::info!("Test BEEFY RPCs completed.");

	log::info!("Test MMR RPCs.");
	// from original line: js-script ./0003-mmr-leaves.js with "21" return is 1 within 60 seconds
	mmr_leaves(&stable_validators, 21).await?;
	// from original line: js-script ./0003-mmr-generate-and-verify-proof.js with
	// "validator-0,validator-1,validator-2" return is 1 within 60 seconds
	mmr_generate_and_verify_proof(&stable_validators, 21).await?;
	log::info!("Test MMR RPCs completed.");

	log::info!(
		"Resume validator-unstable and verify it imports all BEEFY justification and catches up."
	);

	unstable_node.resume().await?;

	// Wait at least 1 full sessions
	log::info!("Waiting for at least 1 full session");
	let relay_node = stable_validators
		.first()
		.ok_or(anyhow!("stable-validators should have one node"))?;
	let relay_client = relay_node.wait_client().await?;
	let mut blocks_sub = relay_client.blocks().subscribe_finalized().await?;
	// in order to ensure to wait at least one full session
	// we wait 2 session change events.
	wait_for_nth_session_change(&mut blocks_sub, 2).await?;
	log::info!("Full session passed");

	let metric_checks: Vec<MetricCheckSetup> = vec![
		// wait syncing
		("substrate_beefy_best_block", Box::new(|v| v >= 21.0), 120u64),
		("substrate_beefy_validator_set_id", Box::new(|v| v >= 3.0), 60u64),
	];
	check_metrics(&[unstable_node], &metric_checks).await?;

	log::info!("Test finished successfully");
	Ok(())
}

async fn mmr_generate_and_verify_proof(
	nodes: &[&NetworkNode],
	target: u64,
) -> Result<(), anyhow::Error> {
	let fut = nodes.iter().map(|n| n.rpc());
	let rpcs = try_join_all(fut).await?;
	let rpc = &rpcs[0];
	let at: String = rpc.request("chain_getBlockHash", rpc_params![target]).await?;
	let root: String = rpc.request("mmr_root", rpc_params![at.clone()]).await?;
	let proof: serde_json::Value =
		rpc.request("mmr_generateProof", rpc_params![[1, 9, 20], target, at]).await?;
	log::debug!("proof: {proof}");

	let fut = nodes.iter().map(|n| n.rpc());
	let rpcs = try_join_all(fut).await?;

	let proof_verifications_fut = rpcs
		.iter()
		.map(|rpc| rpc.request("mmr_verifyProof", rpc_params![proof.clone()]));
	let proof_verifications: Vec<bool> = try_join_all(proof_verifications_fut).await?;
	log::debug!("proof_verifications: {proof_verifications:?}");

	let res = proof_verifications.into_iter().all(|r| r);
	assert!(res, "mmr_verifyProof verification fails");

	let proof_verifications_stateless_fut = rpcs.iter().map(|rpc| {
		rpc.request("mmr_verifyProofStateless", rpc_params![root.clone(), proof.clone()])
	});
	let proof_verifications_stateless: Vec<bool> =
		try_join_all(proof_verifications_stateless_fut).await?;
	log::debug!("proof_verifications_stateless: {proof_verifications_stateless:?}");

	let res = proof_verifications_stateless.into_iter().all(|r| r);
	assert!(res, "mmr_verifyProofStateless verification fails");

	Ok(())
}

async fn get_mmr_num_of_leaves(node: &NetworkNode) -> Result<u128, anyhow::Error> {
	let client: OnlineClient<PolkadotConfig> = node.wait_client().await?;
	let storage_query = subxt::dynamic::storage("Mmr", "NumberOfLeaves", vec![]);
	let result = client.storage().at_latest().await?.fetch(&storage_query).await?;
	let value = result
		.ok_or(anyhow!("Should have a valid response"))?
		.to_value()?
		.as_u128()
		.ok_or(anyhow!("Should be a valid u128"))?;
	log::debug!("NumberOfLeaves is: {value}");
	Ok(value)
}

async fn mmr_leaves(nodes: &[&NetworkNode], target: u64) -> Result<(), anyhow::Error> {
	let fut = nodes.iter().map(|n| get_mmr_num_of_leaves(n));
	let responses = try_join_all(fut).await?;

	let res = responses.into_iter().all(|r| r >= target as u128);
	assert!(res, "mmr_NumberOfLeaves verification fails");

	Ok(())
}

#[derive(Debug)]
struct FinalizedHead {
	hash: String,
	height: u64,
}

async fn beefy_finalized_heads(nodes: &[&NetworkNode], target: u64) -> Result<(), anyhow::Error> {
	let fut = nodes.iter().map(|n| n.rpc());
	let rpcs = try_join_all(fut).await?;

	let mut finalized_heads: HashMap<usize, FinalizedHead> = Default::default();
	for (i, rpc) in rpcs.iter().enumerate() {
		let params: RpcParams = rpc_params![];
		let finalized_head: String = rpc.request("beefy_getFinalizedHead", params).await?;
		log::debug!("finalized_head: {finalized_head:?}");
		let header_json: serde_json::Value =
			rpc.request("chain_getHeader", rpc_params![&finalized_head]).await?;
		let hex_number_str =
			&header_json["number"].as_str().ok_or(anyhow!("Header.number must be valid"))?[2..];
		let height = u64::from_str_radix(hex_number_str, 16).unwrap();
		finalized_heads.insert(i, FinalizedHead { hash: finalized_head, height });
	}
	log::debug!("finalized_head: {finalized_heads:?}");

	// select the node with the highest finalized height
	let highest_finalized_height = finalized_heads
		.iter()
		.max_by(|a, b| a.1.height.cmp(&b.1.height))
		.ok_or(anyhow!("Should have at least one value"))?;

	// get all block hashes up until the highest finalized height
	let mut block_hashes = vec![];
	let rpc_highest = &rpcs[*highest_finalized_height.0];
	for block_number in 0..highest_finalized_height.1.height + 1 {
		let block_hash: String =
			rpc_highest.request("chain_getBlockHash", rpc_params![block_number]).await?;
		block_hashes.push(block_hash);
	}

	// verify that height(finalized_head) is at least as high as the substrate_beefy_best_block test
	// already verified
	let res = finalized_heads.iter().all(|(_i, finalized_head)| {
		finalized_head.height >= target &&
			finalized_head.hash == block_hashes[finalized_head.height as usize]
	});

	assert!(res, "finalized_heads verification fails");

	Ok(())
}

fn build_network_config() -> Result<NetworkConfig, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let polkadot_image = env_or_default(INTEGRATION_IMAGE_ENV, images.polkadot.as_str());

	let mut builder = NetworkConfigBuilder::new().with_relaychain(|r| {
		r.with_chain("rococo-local")
			.with_default_command("polkadot")
			.with_default_image(polkadot_image.as_str())
			.with_default_args(maybe_enable_experimental_collator_protocol(vec![
				"--log=beefy=debug".into(),
				"--enable-offchain-indexing=true".into(),
			]))
			.with_default_resources(|r| {
				r.with_limit_memory("4G")
					.with_limit_cpu("2")
					.with_request_memory("2G")
					.with_request_cpu("1")
			})
			.with_node_group(|g| g.with_count(3).with_base_node(|node| node.with_name("validator")))
			.with_validator(|n| n.with_name("validator-unstable"))
	});

	builder = builder.with_global_settings(|global_settings| {
		match std::env::var("ZOMBIENET_SDK_BASE_DIR") {
			Ok(val) => global_settings.with_base_dir(val),
			_ => global_settings,
		}
	});

	builder.build().map_err(|e| {
		let errs = e.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
		anyhow!("config errs: {errs}")
	})
}
