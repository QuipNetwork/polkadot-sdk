// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use crate::utils::{
	env_or_default, initialize_network, log_line_absent, resolve_db_snapshot_height,
	wait_for_warp_sync_logs, BEEFY_BEST_BLOCK_METRIC, BEST_BLOCK_METRIC, CHAIN_SPEC_ENV,
	DB_SNAPSHOT_ENV, DEFAULT_CHAIN_SPEC, DEFAULT_DB_SNAPSHOT_URL, DEFAULT_SUBSTRATE_IMAGE,
	FULLNODE_ROLE_VALUE, INTEGRATION_IMAGE_ENV, VALIDATOR_ROLE_VALUE,
};
use anyhow::{anyhow, Context, Result};
use env_logger::Env;
use zombienet_sdk::{NetworkConfig, NetworkConfigBuilder, NetworkNode};

const ROLE_TIMEOUT_SECS: u64 = 60;
const PEER_TIMEOUT_SECS: u64 = 60;
const BOOTSTRAP_TIMEOUT_SECS: u64 = 180;
const METRIC_TIMEOUT_SECS: u64 = 60;
const NEW_BLOCK_TIMEOUT_SECS: u64 = 240;
const LOG_ERROR_TIMEOUT_SECS: u64 = 10;
const BEEFY_SYNC_TIMEOUT_SECS: u64 = 180;
const BEEFY_PROGRESS_TIMEOUT_SECS: u64 = 60;

const PEERS_THRESHOLD: f64 = 2.0;
const MIN_BOOTSTRAP_BLOCK: f64 = 1.0;

const VALIDATORS: [&str; 2] = ["alice", "bob"];
const FULLNODES: [&str; 2] = ["charlie", "dave"];

#[tokio::test(flavor = "multi_thread")]
async fn block_building_warp_sync() -> Result<()> {
	let _ = env_logger::Builder::from_env(Env::default().default_filter_or("info")).try_init();

	log::info!("Spawning network");
	let config = build_network_config()?;
	let network = initialize_network(config).await?;

	network.wait_until_is_up(BOOTSTRAP_TIMEOUT_SECS).await?;

	check_node_roles(&network).await?;
	check_peers(&network).await?;

	let db_snapshot_height = resolve_db_snapshot_height(&network, "alice").await?;

	verify_bootstrap_height(&network, db_snapshot_height).await?;
	verify_new_blocks(&network, db_snapshot_height).await?;

	let dave = network.get_node("dave")?;
	verify_node_progress(dave, db_snapshot_height).await?;
	wait_for_warp_sync_logs(dave).await?;
	verify_node_beefy(dave, db_snapshot_height).await?;
	check_error_logs(dave).await?;

	network.destroy().await?;

	Ok(())
}

fn build_network_config() -> Result<NetworkConfig> {
	let integration_image = env_or_default(INTEGRATION_IMAGE_ENV, DEFAULT_SUBSTRATE_IMAGE);
	let db_snapshot = env_or_default(DB_SNAPSHOT_ENV, DEFAULT_DB_SNAPSHOT_URL);
	let chain_spec = env_or_default(CHAIN_SPEC_ENV, DEFAULT_CHAIN_SPEC);

	NetworkConfigBuilder::new()
		.with_relaychain(|relaychain| {
			relaychain
				.with_chain("local")
				.with_default_command("substrate")
				.with_default_image(integration_image.as_str())
				.with_chain_spec_path(chain_spec.as_str())
				.with_validator(|node| {
					node.with_name("alice").with_db_snapshot(db_snapshot.as_str())
				})
				.with_validator(|node| node.with_name("bob").with_db_snapshot(db_snapshot.as_str()))
				.with_fullnode(|node| {
					node.with_name("charlie").with_db_snapshot(db_snapshot.as_str())
				})
				.with_fullnode(|node| {
					node.with_name("dave")
						.with_args(vec!["--sync=warp".into(), "-ldb::blockchain".into()])
				})
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
		})
}

async fn check_node_roles(
	network: &zombienet_sdk::Network<zombienet_sdk::LocalFileSystem>,
) -> Result<()> {
	for validator in VALIDATORS {
		network
			.get_node(validator)?
			.wait_metric_with_timeout(
				"node_roles",
				|role| role == VALIDATOR_ROLE_VALUE,
				ROLE_TIMEOUT_SECS,
			)
			.await?;
	}

	for follower in FULLNODES {
		network
			.get_node(follower)?
			.wait_metric_with_timeout(
				"node_roles",
				|role| role == FULLNODE_ROLE_VALUE,
				ROLE_TIMEOUT_SECS,
			)
			.await?;
	}

	Ok(())
}

async fn check_peers(
	network: &zombienet_sdk::Network<zombienet_sdk::LocalFileSystem>,
) -> Result<()> {
	for &node_name in VALIDATORS.iter().chain(FULLNODES.iter()) {
		network
			.get_node(node_name)?
			.wait_metric_with_timeout(
				"substrate_sub_libp2p_peers_count",
				|peers| peers >= PEERS_THRESHOLD,
				PEER_TIMEOUT_SECS,
			)
			.await?;
	}

	Ok(())
}

async fn verify_bootstrap_height(
	network: &zombienet_sdk::Network<zombienet_sdk::LocalFileSystem>,
	db_snapshot_height: f64,
) -> Result<()> {
	for &node_name in VALIDATORS.iter().chain(FULLNODES.iter()) {
		network
			.get_node(node_name)?
			.wait_metric_with_timeout(
				BEST_BLOCK_METRIC,
				|height| height >= MIN_BOOTSTRAP_BLOCK,
				BOOTSTRAP_TIMEOUT_SECS,
			)
			.await?;
		network
			.get_node(node_name)?
			.wait_metric_with_timeout(
				BEST_BLOCK_METRIC,
				|height| height >= db_snapshot_height,
				METRIC_TIMEOUT_SECS,
			)
			.await?;
	}

	Ok(())
}

async fn verify_new_blocks(
	network: &zombienet_sdk::Network<zombienet_sdk::LocalFileSystem>,
	db_snapshot_height: f64,
) -> Result<()> {
	for node_name in VALIDATORS {
		network
			.get_node(node_name)?
			.wait_metric_with_timeout(
				BEST_BLOCK_METRIC,
				|height| height > db_snapshot_height,
				NEW_BLOCK_TIMEOUT_SECS,
			)
			.await?;
	}

	let charlie = network.get_node("charlie")?;
	charlie
		.wait_metric_with_timeout(
			BEST_BLOCK_METRIC,
			|height| height > db_snapshot_height,
			NEW_BLOCK_TIMEOUT_SECS,
		)
		.await?;

	Ok(())
}

async fn verify_node_progress(node: &NetworkNode, db_snapshot_height: f64) -> Result<()> {
	let node_name = node.name();

	node.wait_metric_with_timeout(BEST_BLOCK_METRIC, |height| height >= 1.0, METRIC_TIMEOUT_SECS)
		.await
		.with_context(|| {
			format!(
				"{node_name} did not report BEST_BLOCK_METRIC >= 1 within {METRIC_TIMEOUT_SECS}s"
			)
		})?;
	node.wait_metric_with_timeout(
		BEST_BLOCK_METRIC,
		|height| height >= db_snapshot_height,
		METRIC_TIMEOUT_SECS,
	)
	.await
	.with_context(|| {
		format!(
			"{node_name} did not catch up to snapshot height {db_snapshot_height} within {METRIC_TIMEOUT_SECS}s"
		)
	})?;
	node.wait_metric_with_timeout(
		BEST_BLOCK_METRIC,
		|height| height > db_snapshot_height,
		METRIC_TIMEOUT_SECS,
	)
	.await
	.with_context(|| {
		format!(
			"{node_name} did not produce blocks beyond snapshot height {db_snapshot_height} within {METRIC_TIMEOUT_SECS}s"
		)
	})?;

	Ok(())
}

async fn verify_node_beefy(node: &NetworkNode, db_snapshot_height: f64) -> Result<()> {
	let node_name = node.name();

	node.wait_metric_with_timeout(
		BEEFY_BEST_BLOCK_METRIC,
		|height| height >= db_snapshot_height,
		BEEFY_SYNC_TIMEOUT_SECS,
	)
	.await
	.with_context(|| {
			format!(
				"{node_name} did not sync BEEFY best block to snapshot height {db_snapshot_height} within {BEEFY_SYNC_TIMEOUT_SECS}s"
			)
		})?;
	node.wait_metric_with_timeout(
		BEEFY_BEST_BLOCK_METRIC,
		|height| height > db_snapshot_height,
		BEEFY_PROGRESS_TIMEOUT_SECS,
	)
	.await
	.with_context(|| {
			format!(
				"{node_name} did not advance BEEFY best block beyond snapshot height {db_snapshot_height} within {BEEFY_PROGRESS_TIMEOUT_SECS}s"
			)
		})?;

	Ok(())
}

async fn check_error_logs(node: &NetworkNode) -> Result<()> {
	let node_name = node.name();

	node.wait_log_line_count_with_timeout(
		r"error(?! importing block .*: block has an unknown parent)",
		false,
		log_line_absent(LOG_ERROR_TIMEOUT_SECS),
	)
	.await
	.with_context(|| format!("{node_name} logged disallowed errors"))?;
	node.wait_log_line_count_with_timeout(
		"verification failed",
		false,
		log_line_absent(LOG_ERROR_TIMEOUT_SECS),
	)
	.await
	.with_context(|| format!("{node_name} logged 'verification failed'"))?;

	Ok(())
}
