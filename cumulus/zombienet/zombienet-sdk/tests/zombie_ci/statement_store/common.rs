// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// The `people-westend-local-spec.json` chain spec template used by these tests
// was generated using the script at:
// https://github.com/paritytech/individuality/blob/main/runtimes/people-westend/chain-spec/create_people_westend_spec.sh
// To regenerate, run that script and replace the JSON file in this directory.

use anyhow::anyhow;
use codec::Encode;
use log::info;
use sc_statement_store::test_utils::get_keypair;
use sp_core::{hexdisplay::HexDisplay, Bytes, Pair};
use sp_statement_store::{
	statement_allowance_key, StatementAllowance, StatementEvent, SubmitOutcome, SubmitResult,
	Topic, TopicFilter,
};
use std::{
	path::{Path, PathBuf},
	time::Duration,
};
use zombienet_sdk::{
	subxt::{
		backend::rpc::RpcClient,
		ext::subxt_rpcs::{client::RpcSubscription, rpc_params},
	},
	LocalFileSystem, Network, NetworkConfigBuilder,
};

use sc_statement_store::subxt_client::CustomConfig;
use subxt::OnlineClient;
pub(super) const RPC_POOL_SIZE: usize = 10000;
pub(super) const COLLATOR_INFO_LOG_FILTER: &str = "info,statement-store=info,statement-gossip=info";
pub(super) const COLLATOR_TRACE_LOG_FILTER: &str =
	"info,statement-store=trace,statement-gossip=trace";

pub(super) use sp_statement_store::{
	AddFilterResponse as UnstableAddFilterResponse, SubscribeEvent as UnstableStatementEvent,
};

pub(super) async fn submit_statement(
	rpc: &RpcClient,
	statement: &sp_statement_store::Statement,
) -> Result<SubmitResult, anyhow::Error> {
	let encoded: Bytes = statement.encode().into();
	let result: SubmitResult = rpc.request("statement_submit", rpc_params![encoded]).await?;
	Ok(result)
}

// Helpers for the unstable statement JSON-RPC methods specified in:
// https://github.com/paritytech/json-rpc-interface-spec/pull/185
pub(super) async fn submit_statement_unstable(
	rpc: &RpcClient,
	statement: &sp_statement_store::Statement,
) -> Result<SubmitOutcome, anyhow::Error> {
	let encoded: Bytes = statement.encode().into();
	let result: SubmitOutcome =
		rpc.request("statement_unstable_submit", rpc_params![encoded]).await?;
	Ok(result)
}

pub(super) async fn subscribe_unstable(
	rpc: &RpcClient,
) -> Result<RpcSubscription<UnstableStatementEvent>, anyhow::Error> {
	let subscription = rpc
		.subscribe::<UnstableStatementEvent>(
			"statement_unstable_subscribe",
			rpc_params![],
			"statement_unstable_unsubscribe",
		)
		.await?;
	Ok(subscription)
}

pub(super) fn unstable_subscription_id(
	subscription: &RpcSubscription<UnstableStatementEvent>,
) -> Result<String, anyhow::Error> {
	subscription
		.subscription_id()
		.map(ToOwned::to_owned)
		.ok_or_else(|| anyhow!("Subscription was accepted without an id"))
}

pub(super) async fn add_filter_unstable(
	rpc: &RpcClient,
	subscription_id: &str,
	filter: TopicFilter,
) -> Result<UnstableAddFilterResponse, anyhow::Error> {
	let response = rpc
		.request("statement_unstable_add_filter", rpc_params![subscription_id, filter])
		.await?;
	Ok(response)
}

pub(super) async fn remove_filter_unstable(
	rpc: &RpcClient,
	subscription_id: &str,
	filter_id: &str,
) -> Result<(), anyhow::Error> {
	Ok(rpc
		.request("statement_unstable_remove_filter", rpc_params![subscription_id, filter_id])
		.await?)
}

pub(super) async fn expect_one_statement(
	subscription: &mut RpcSubscription<StatementEvent>,
	timeout_secs: u64,
) -> Result<Bytes, anyhow::Error> {
	loop {
		let item = tokio::time::timeout(Duration::from_secs(timeout_secs), subscription.next())
			.await
			.map_err(|_| anyhow!("Timeout waiting for statement after {}s", timeout_secs))?
			.ok_or_else(|| anyhow!("Subscription stream ended unexpectedly"))?
			.map_err(|e| anyhow!("Subscription error: {}", e))?;

		return match item {
			StatementEvent::NewStatements { statements: batch, .. } => {
				if batch.is_empty() {
					continue;
				}
				assert_eq!(batch.len(), 1, "Expected exactly one statement in batch");
				Ok(batch.into_iter().next().unwrap())
			},
		};
	}
}

pub(super) async fn assert_no_more_statements(
	subscription: &mut RpcSubscription<StatementEvent>,
	timeout_secs: u64,
) -> Result<(), anyhow::Error> {
	let result = tokio::time::timeout(Duration::from_secs(timeout_secs), subscription.next()).await;
	assert!(result.is_err(), "Expected no more statements but received one");
	Ok(())
}

/// Subscribes to statements matching a specific topic
pub(super) async fn subscribe_topic(
	rpc: &RpcClient,
	topic: Topic,
) -> Result<RpcSubscription<StatementEvent>, anyhow::Error> {
	let filter = TopicFilter::MatchAll(vec![topic].try_into().expect("Single topic"));
	subscribe_topic_filter(rpc, filter).await
}

pub(super) async fn subscribe_topic_filter(
	rpc: &RpcClient,
	filter: TopicFilter,
) -> Result<RpcSubscription<StatementEvent>, anyhow::Error> {
	let subscription = rpc
		.subscribe::<StatementEvent>(
			"statement_subscribeStatement",
			rpc_params![filter],
			"statement_unsubscribeStatement",
		)
		.await?;
	Ok(subscription)
}

/// Collects `count` statements from a subscription without assuming arrival order
///
/// Handles multi-item `NewStatements` batches by collecting all items from each batch
/// Returns the collected statements once the target count is reached
pub(super) async fn expect_statements_unordered(
	subscription: &mut RpcSubscription<StatementEvent>,
	count: usize,
	timeout_secs: u64,
) -> Result<Vec<Bytes>, anyhow::Error> {
	let mut collected = Vec::with_capacity(count);
	let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);

	while collected.len() < count {
		let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
		if remaining.is_zero() {
			return Err(anyhow!(
				"Timeout after {}s: collected {}/{} statements",
				timeout_secs,
				collected.len(),
				count
			));
		}

		let item = tokio::time::timeout(remaining, subscription.next())
			.await
			.map_err(|_| {
				anyhow!(
					"Timeout after {}s: collected {}/{} statements",
					timeout_secs,
					collected.len(),
					count
				)
			})?
			.ok_or_else(|| anyhow!("Subscription stream ended unexpectedly"))?
			.map_err(|e| anyhow!("Subscription error: {}", e))?;

		match item {
			StatementEvent::NewStatements { statements: batch, .. } => {
				for stmt in batch {
					collected.push(stmt);
				}
			},
		}
	}

	Ok(collected)
}

/// Collects `count` statements from a subscription, then asserts they match
/// `expected` (order-independent, compared as sorted encoded bytes)
pub(super) async fn assert_statements_match(
	subscription: &mut RpcSubscription<StatementEvent>,
	expected: &[Vec<u8>],
	timeout_secs: u64,
	node_name: &str,
) -> Result<(), anyhow::Error> {
	let received = expect_statements_unordered(subscription, expected.len(), timeout_secs).await?;
	assert_eq!(received.len(), expected.len());
	let mut received: Vec<Vec<u8>> = received.into_iter().map(|b| b.to_vec()).collect();
	received.sort();
	let mut expected = expected.to_vec();
	expected.sort();
	assert_eq!(received, expected, "Statement content mismatch on {}", node_name);
	Ok(())
}

/// Creates a custom chain spec with uniform allowances for all participants
pub(super) fn create_chain_spec_with_allowances(
	participant_count: u32,
	base_dir: &Path,
) -> Result<PathBuf, anyhow::Error> {
	let chain_spec_template = include_str!("people-westend-local-spec.json");
	let mut chain_spec: serde_json::Value = serde_json::from_str(chain_spec_template)
		.map_err(|e| anyhow!("Failed to parse chain spec JSON: {}", e))?;
	let genesis = chain_spec
		.get_mut("genesis")
		.and_then(|g| g.get_mut("raw"))
		.and_then(|r| r.get_mut("top"))
		.and_then(|t| t.as_object_mut())
		.ok_or_else(|| anyhow!("Failed to access genesis.raw.top in chain spec"))?;

	let allowance = StatementAllowance { max_count: 100_000, max_size: 1_000_000 };
	let allowance_hex = format!("0x{}", HexDisplay::from(&allowance.encode()));
	info!("Injecting statement allowance: {:}", allowance_hex);
	for idx in 0..participant_count {
		let keypair = get_keypair(idx);
		let account_id = keypair.public();

		let storage_key = statement_allowance_key(account_id.0);
		let storage_key_hex = format!("0x{}", HexDisplay::from(&storage_key));

		genesis.insert(storage_key_hex, serde_json::Value::String(allowance_hex.clone()));
	}

	let chain_spec_path = base_dir.join("people-westend-custom.json");
	let chain_spec_json = serde_json::to_string_pretty(&chain_spec)
		.map_err(|e| anyhow!("Failed to serialize chain spec: {}", e))?;

	std::fs::write(&chain_spec_path, chain_spec_json)
		.map_err(|e| anyhow!("Failed to write chain spec to file: {}", e))?;

	Ok(chain_spec_path)
}

/// Builds the standard collator CLI args for the statement-store zombienet tests
pub(super) fn collator_args(participant_count: u32, log_filter: &str) -> Vec<zombienet_sdk::Arg> {
	let mut args: Vec<String> = vec![
		"--force-authoring".to_string(),
		"--authoring=slot-based".to_string(),
		"--max-runtime-instances=32".to_string(),
		format!("-l{log_filter}"),
		"--enable-statement-store".to_string(),
	];
	let max_subs_per_conn = (participant_count * 16 / RPC_POOL_SIZE as u32).max(32);
	args.push(format!("--rpc-max-connections={}", participant_count + 1000));
	args.push(format!("--rpc-max-subscriptions-per-connection={max_subs_per_conn}"));
	args.iter().map(|s| s.as_str().into()).collect()
}

pub(super) fn base_dir() -> Result<PathBuf, anyhow::Error> {
	let path = std::env::var("ZOMBIENET_SDK_BASE_DIR")
		.ok()
		.map(PathBuf::from)
		.unwrap_or_else(|| std::env::temp_dir().join(format!("zombie-{}", std::process::id())));
	std::fs::create_dir_all(&path)
		.map_err(|e| anyhow!("Failed to create base directory: {}", e))?;
	Ok(path)
}

pub(super) fn format_build_errors(errors: Vec<anyhow::Error>) -> anyhow::Error {
	let errs = errors.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" ");
	anyhow!("config errs: {errs}")
}

/// Builds the network config, initialises the network, and waits for it to come up
async fn launch_network(
	collators: &[&str],
	chain_spec_path: &Path,
	collator_args: Vec<zombienet_sdk::Arg>,
) -> Result<Network<LocalFileSystem>, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let base_dir = base_dir()?;

	let config = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("westend-local")
				.with_default_command("polkadot")
				.with_default_image(images.polkadot.as_str())
				.with_default_args(vec!["-lparachain=debug".into()])
				.with_validator(|node| node.with_name("validator-0"))
				.with_validator(|node| node.with_name("validator-1"))
		})
		.with_parachain(|p| {
			let p = p
				.with_id(1004)
				.with_chain_spec_path(chain_spec_path.to_str().expect("Valid UTF-8 path"))
				.with_default_command("polkadot-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_default_args(collator_args)
				.with_collator(|n| n.with_name(collators[0]));

			collators[1..]
				.iter()
				.fold(p, |acc, &name| acc.with_collator(|n| n.with_name(name)))
		})
		.with_global_settings(|global_settings| {
			global_settings
				.with_base_dir(base_dir.to_str().expect("Valid UTF-8 path"))
				.with_tear_down_on_failure(false) // To allow restart nodes without failing in CI
		})
		.build()
		.map_err(format_build_errors)?;

	let network = crate::utils::initialize_network(config).await?;
	assert!(network.wait_until_is_up(60).await.is_ok());
	Ok(network)
}

/// Spawns a network with a sudo-enabled chain spec and sets allowances at runtime
pub(super) async fn spawn_network_sudo(
	collators: &[&str],
	allowance_items: Vec<(Vec<u8>, Vec<u8>)>,
	log_filter: &str,
) -> Result<Network<LocalFileSystem>, anyhow::Error> {
	let network = spawn_network_inner(collators, allowance_items.len(), log_filter).await?;
	let node = network.get_node(collators[0])?;
	sc_statement_store::subxt_client::set_allowances_via_sudo(node.ws_uri(), allowance_items)
		.await?;

	Ok(network)
}

/// Spawns a zombienet network with a custom chain spec containing injected statement allowances
pub(super) async fn spawn_network_with_injected_allowances(
	collators: &[&str],
	participant_count: u32,
) -> Result<Network<LocalFileSystem>, anyhow::Error> {
	assert!(!collators.is_empty());
	let base_dir = base_dir()?;
	let chain_spec_path = create_chain_spec_with_allowances(participant_count, &base_dir)?;
	let args = collator_args(participant_count, COLLATOR_TRACE_LOG_FILTER);
	launch_network(collators, &chain_spec_path, args).await
}

/// Spawns a network using `people-westend-local-spec.json`, waits for block production
async fn spawn_network_inner(
	collators: &[&str],
	participant_count: usize,
	log_filter: &str,
) -> Result<Network<LocalFileSystem>, anyhow::Error> {
	let base_dir = base_dir()?;
	let chain_spec_template = include_str!("people-westend-local-spec.json");
	let chain_spec_path = base_dir.join("people-westend-local-spec.json");
	std::fs::write(&chain_spec_path, chain_spec_template)
		.map_err(|e| anyhow!("Failed to write chain spec to file: {}", e))?;

	let participant_count_u32 = u32::try_from(participant_count)
		.expect("participant_count must fit in u32 for collator args");
	let args = collator_args(participant_count_u32, log_filter);
	let network = launch_network(collators, &chain_spec_path, args).await?;

	info!("Waiting for parachain to produce blocks...");
	let node = network.get_node(collators[0])?;
	node.wait_metric_with_timeout(crate::utils::BEST_BLOCK_METRIC, |height| height >= 1.0, 300u64)
		.await?;
	info!("Parachain is producing blocks");

	Ok(network)
}

/// Spawns a network without pre-injected allowances
pub(super) async fn spawn_network(
	collators: &[&str],
	log_filter: &str,
) -> Result<Network<LocalFileSystem>, anyhow::Error> {
	spawn_network_inner(collators, 0, log_filter).await
}

pub(super) async fn online_client_from_node(
	node: &zombienet_sdk::NetworkNode,
) -> Result<OnlineClient<CustomConfig>, anyhow::Error> {
	OnlineClient::<CustomConfig>::from_insecure_url_with_config(
		CustomConfig::default(),
		node.ws_uri(),
	)
	.await
	.map_err(Into::into)
}

/// Waits up to `timeout_secs` for each node to produce at least one block
pub(super) async fn wait_for_first_block(
	nodes: &[&zombienet_sdk::NetworkNode],
	timeout_secs: u64,
) -> Result<(), anyhow::Error> {
	for node in nodes {
		node.wait_metric_with_timeout(
			crate::utils::BEST_BLOCK_METRIC,
			|height| height >= 1.0,
			timeout_secs,
		)
		.await?;
	}
	Ok(())
}
