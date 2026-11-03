// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use super::common::{
	add_filter_unstable, assert_no_more_statements, assert_statements_match, base_dir,
	collator_args, create_chain_spec_with_allowances, expect_one_statement,
	expect_statements_unordered, online_client_from_node, remove_filter_unstable, spawn_network,
	spawn_network_sudo, spawn_network_with_injected_allowances, submit_statement,
	submit_statement_unstable, subscribe_topic, subscribe_topic_filter, subscribe_unstable,
	unstable_subscription_id, wait_for_first_block, UnstableAddFilterResponse,
	UnstableStatementEvent, COLLATOR_INFO_LOG_FILTER, COLLATOR_TRACE_LOG_FILTER,
};
use codec::Encode;
use futures::future::join_all;
use log::{debug, info};
use sc_network_statement::config::STATEMENTS_BURST_COEFFICIENT;
use sc_statement_store::{
	subxt_client::{
		create_attest_call, create_consumer_registration_params, create_increase_allowance_call,
		submit_extrinsic, CustomConfig, MSG_PREFIX,
	},
	test_utils::{create_allowance_items, create_test_statement, get_keypair},
};
use sp_core::{sr25519, Bytes, Pair};
use sp_runtime::BoundedVec;
use sp_statement_store::{
	hash_encoded, RejectionReason, Statement, StatementAllowance, SubmitOutcome, SubmitResult,
	Topic, TopicFilter,
};
use std::{
	cell::Cell,
	collections::HashSet,
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};
use subxt::transactions::Signer;
use verifiable::{ring_vrf_impl::BandersnatchVrfVerifiable as Crypto, GenerateVerifiable};
use zombienet_sdk::{LocalFileSystem, Network, NetworkConfigBuilder};

async fn expect_unstable_event(
	subscription: &mut zombienet_sdk::subxt::ext::subxt_rpcs::client::RpcSubscription<
		UnstableStatementEvent,
	>,
	timeout_secs: u64,
) -> Result<UnstableStatementEvent, anyhow::Error> {
	tokio::time::timeout(Duration::from_secs(timeout_secs), subscription.next())
		.await
		.map_err(|_| anyhow::anyhow!("Timeout waiting for unstable statement event"))?
		.ok_or_else(|| anyhow::anyhow!("Unstable statement subscription ended"))?
		.map_err(|e| anyhow::anyhow!("Unstable statement subscription error: {}", e))
}

async fn collect_unstable_replay(
	subscription: &mut zombienet_sdk::subxt::ext::subxt_rpcs::client::RpcSubscription<
		UnstableStatementEvent,
	>,
	filter_id: &str,
) -> Result<Vec<Bytes>, anyhow::Error> {
	let mut statements = Vec::new();
	loop {
		match expect_unstable_event(subscription, 20).await? {
			UnstableStatementEvent::ReplayStatements { filter_id: id, statements: chunk }
				if id == filter_id =>
			{
				statements.extend(chunk)
			},
			UnstableStatementEvent::ReplayDone { filter_id: id } if id == filter_id => {
				return Ok(statements)
			},
			event => anyhow::bail!("Unexpected unstable event before replayDone: {:?}", event),
		}
	}
}

fn match_all_filter(topic: Topic) -> TopicFilter {
	TopicFilter::MatchAll(BoundedVec::truncate_from(vec![topic]))
}

fn filter_id(response: UnstableAddFilterResponse) -> String {
	match response {
		UnstableAddFilterResponse::Ok(id) => id,
		UnstableAddFilterResponse::LimitReached(result) => {
			panic!("Expected filter id, got {result:?}")
		},
	}
}

/// Verifies basic statement propagation and data integrity across two nodes
///
/// Tests uses the genesis-injection approach for setting allowances
#[tokio::test(flavor = "multi_thread")]
async fn statement_store_basic_propagation() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let network = spawn_network_with_injected_allowances(&["charlie", "dave"], 8).await?;

	let charlie = network.get_node("charlie")?;
	let dave = network.get_node("dave")?;

	let charlie_rpc = charlie.rpc().await?;
	let dave_rpc = dave.rpc().await?;

	let topic: Topic = [0u8; 32].into();
	let keypair = get_keypair(0);
	let statement = create_test_statement(&keypair, &[topic], None, vec![1, 2, 3], u32::MAX, 0);
	let expected: Bytes = statement.encode().into();

	let mut sub = subscribe_topic(&dave_rpc, topic).await?;
	let result = submit_statement(&charlie_rpc, &statement).await?;
	assert_eq!(result, SubmitResult::New);

	let received = expect_one_statement(&mut sub, 20).await?;
	assert_eq!(received, expected, "Statement data mismatch");
	info!("Basic propagation: verified");

	Ok(())
}

/// End-to-end smoke test of the v2 unstable statement RPC API on a two-node network: replay and
/// live delivery on the submitting node, cross-node live delivery, and multi-filter attribution
/// plus filter removal
#[tokio::test(flavor = "multi_thread")]
async fn statement_store_unstable_rpc_smoke() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let network = spawn_network_with_injected_allowances(&["alice", "bob"], 6).await?;
	let alice = network.get_node("alice")?;
	let bob = network.get_node("bob")?;
	let alice_rpc = alice.rpc().await?;
	let bob_rpc = bob.rpc().await?;

	let topic: Topic = [0xA1; 32].into();
	let pre_existing = create_test_statement(&get_keypair(0), &[topic], None, vec![1], u32::MAX, 0);
	assert_eq!(submit_statement_unstable(&alice_rpc, &pre_existing).await?, SubmitOutcome::New);

	let mut subscription = subscribe_unstable(&alice_rpc).await?;
	let subscription_id = unstable_subscription_id(&subscription)?;
	let alice_filter = filter_id(
		add_filter_unstable(&alice_rpc, &subscription_id, match_all_filter(topic)).await?,
	);

	let replayed = collect_unstable_replay(&mut subscription, &alice_filter).await?;
	assert_eq!(replayed, vec![Bytes::from(pre_existing.encode())]);

	let live = create_test_statement(&get_keypair(1), &[topic], None, vec![2], u32::MAX, 0);
	assert_eq!(submit_statement_unstable(&alice_rpc, &live).await?, SubmitOutcome::New);

	match expect_unstable_event(&mut subscription, 20).await? {
		UnstableStatementEvent::NewStatements { statements } => {
			assert_eq!(statements.len(), 1);
			assert_eq!(statements[0].statement, Bytes::from(live.encode()));
			assert_eq!(statements[0].filter_ids, vec![alice_filter.clone()]);
		},
		event => anyhow::bail!("Expected newStatements event, got {:?}", event),
	}

	// Cross-node: a statement submitted on alice reaches a subscription on bob live. A fresh topic
	// keeps bob's replay empty, so the assertion does not race statement propagation
	let cross_topic: Topic = [0xB2; 32].into();
	let mut bob_sub = subscribe_unstable(&bob_rpc).await?;
	let bob_sub_id = unstable_subscription_id(&bob_sub)?;
	let bob_filter =
		filter_id(add_filter_unstable(&bob_rpc, &bob_sub_id, match_all_filter(cross_topic)).await?);

	let bob_replay = collect_unstable_replay(&mut bob_sub, &bob_filter).await?;
	assert!(bob_replay.is_empty());

	let cross = create_test_statement(&get_keypair(2), &[cross_topic], None, vec![3], u32::MAX, 0);
	assert_eq!(submit_statement_unstable(&alice_rpc, &cross).await?, SubmitOutcome::New);

	match expect_unstable_event(&mut bob_sub, 20).await? {
		UnstableStatementEvent::NewStatements { statements } => {
			assert_eq!(statements.len(), 1);
			assert_eq!(statements[0].statement, Bytes::from(cross.encode()));
			assert_eq!(statements[0].filter_ids, vec![bob_filter]);
		},
		event => anyhow::bail!("Expected cross-node newStatements on bob, got {:?}", event),
	}

	// Multi-filter attribution and removal: a statement matching two filters reports both ids;
	// after the first filter is removed, the next statement reports only the remaining one
	let second_topic: Topic = [0xA2; 32].into();
	let alice_filter_2 = filter_id(
		add_filter_unstable(&alice_rpc, &subscription_id, match_all_filter(second_topic)).await?,
	);
	let replay = collect_unstable_replay(&mut subscription, &alice_filter_2).await?;
	assert!(replay.is_empty());

	let multi =
		create_test_statement(&get_keypair(3), &[topic, second_topic], None, vec![4], u32::MAX, 0);
	assert_eq!(submit_statement_unstable(&alice_rpc, &multi).await?, SubmitOutcome::New);

	match expect_unstable_event(&mut subscription, 20).await? {
		UnstableStatementEvent::NewStatements { statements } => {
			assert_eq!(statements.len(), 1);
			assert_eq!(statements[0].statement, Bytes::from(multi.encode()));
			let ids: HashSet<_> = statements[0].filter_ids.iter().cloned().collect();
			assert_eq!(ids, HashSet::from([alice_filter.clone(), alice_filter_2.clone()]));
		},
		event => anyhow::bail!("Expected multi-filter newStatements on alice, got {:?}", event),
	}

	// The removal is enqueued to the single matcher channel before the notification for the next
	// submit enters it, so the removed filter can never race into the next event
	remove_filter_unstable(&alice_rpc, &subscription_id, &alice_filter).await?;

	let after_remove =
		create_test_statement(&get_keypair(4), &[topic, second_topic], None, vec![5], u32::MAX, 0);
	assert_eq!(submit_statement_unstable(&alice_rpc, &after_remove).await?, SubmitOutcome::New);

	match expect_unstable_event(&mut subscription, 20).await? {
		UnstableStatementEvent::NewStatements { statements } => {
			assert_eq!(statements.len(), 1);
			assert_eq!(statements[0].statement, Bytes::from(after_remove.encode()));
			assert_eq!(statements[0].filter_ids, vec![alice_filter_2]);
		},
		event => anyhow::bail!("Expected newStatements after removal, got {:?}", event),
	}

	Ok(())
}

/// Verifies concurrent propagation, quota enforcement, and priority eviction
///
/// Spawns a single 4-node network with mixed allowances:
/// - keypair_0: tight quota (max_count=3) for quota/eviction testing
/// - keypairs 1-8: generous quota for concurrent propagation
///
/// Test uses sudo-based allowances
#[tokio::test(flavor = "multi_thread")]
async fn statement_store_check_propagation_and_quota_invariants() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let mut entries: Vec<(u32, StatementAllowance)> =
		vec![(0, StatementAllowance { max_count: 3, max_size: 1_000_000 })];
	for i in 1..9u32 {
		entries.push((i, StatementAllowance { max_count: 100, max_size: 1_000_000 }));
	}
	let items = create_allowance_items(&entries);

	let network =
		spawn_network_sudo(&["alice", "bob", "charlie", "dave"], items, COLLATOR_INFO_LOG_FILTER)
			.await?;

	let alice = network.get_node("alice")?;
	let bob = network.get_node("bob")?;
	let charlie = network.get_node("charlie")?;
	let dave = network.get_node("dave")?;

	let alice_rpc = alice.rpc().await?;
	let bob_rpc = bob.rpc().await?;
	let charlie_rpc = charlie.rpc().await?;
	let dave_rpc = dave.rpc().await?;

	// Concurrent multi-account propagation
	let topic: Topic = [10u8; 32].into();
	let mut alice_sub = subscribe_topic(&alice_rpc, topic).await?;
	let mut bob_sub = subscribe_topic(&bob_rpc, topic).await?;
	let mut charlie_sub = subscribe_topic(&charlie_rpc, topic).await?;
	let mut dave_sub = subscribe_topic(&dave_rpc, topic).await?;

	// Use keypairs 1-8 for concurrent submissions
	let statements: Vec<_> = (1u32..9)
		.map(|idx| {
			let keypair = get_keypair(idx);
			create_test_statement(&keypair, &[topic], None, vec![idx as u8], u32::MAX, idx * 100)
		})
		.collect();

	// Distribute submissions across all nodes (round-robin) to test multi-source concurrent ingress
	let nodes = [&alice, &bob, &charlie, &dave];
	let mut handles = Vec::new();
	for (i, stmt) in statements.iter().enumerate() {
		let target = nodes[i % nodes.len()];
		let rpc = target.rpc().await?;
		let stmt = stmt.clone();
		let idx = i + 1;
		handles.push(tokio::spawn(async move {
			let result = submit_statement(&rpc, &stmt).await?;
			assert_eq!(result, SubmitResult::New, "Participant {} should be accepted", idx);
			Ok::<_, anyhow::Error>(())
		}));
	}

	for handle in handles {
		handle.await??;
	}
	info!("All 8 concurrent submissions accepted");

	// Verify content identity: every node must receive exactly the 8 submitted statements
	let expected_encoded: Vec<Vec<u8>> = statements.iter().map(|s| s.encode()).collect();

	for (name, sub) in [
		("alice", &mut alice_sub),
		("bob", &mut bob_sub),
		("charlie", &mut charlie_sub),
		("dave", &mut dave_sub),
	] {
		assert_statements_match(sub, &expected_encoded, 60, name).await?;
	}

	for (name, sub) in [
		("alice", &mut alice_sub),
		("bob", &mut bob_sub),
		("charlie", &mut charlie_sub),
		("dave", &mut dave_sub),
	] {
		assert_no_more_statements(sub, 10).await?;
		info!("No extra statements on {}", name);
	}

	// Quota enforcement and priority eviction
	let quota_topic: Topic = [2u8; 32].into();
	let keypair_0 = get_keypair(0);

	// Fill keypair_0's quota (max_count: 3)
	for seq in [100u32, 200, 300] {
		let stmt =
			create_test_statement(&keypair_0, &[quota_topic], None, vec![seq as u8], u32::MAX, seq);
		assert_eq!(submit_statement(&alice_rpc, &stmt).await?, SubmitResult::New);
	}

	// Rejected: lower priority than all existing (50 < 100)
	let low = create_test_statement(&keypair_0, &[quota_topic], None, vec![0], u32::MAX, 50);
	assert!(matches!(
		submit_statement(&alice_rpc, &low).await?,
		SubmitResult::Rejected(RejectionReason::AccountFull { .. })
	));
	info!("AccountFull verified");

	// Rejected: keypair_10 has no allowance
	let keypair_10 = get_keypair(10);
	let no_allow = create_test_statement(&keypair_10, &[quota_topic], None, vec![1], u32::MAX, 0);
	assert!(matches!(
		submit_statement(&alice_rpc, &no_allow).await?,
		SubmitResult::Rejected(RejectionReason::NoAllowance)
	));

	// Priority eviction: seq=150 evicts seq=100 → store: [150, 200, 300]
	let mut bob_evict_sub = subscribe_topic(&bob_rpc, quota_topic).await?;
	let mut charlie_evict_sub = subscribe_topic(&charlie_rpc, quota_topic).await?;
	let mut dave_evict_sub = subscribe_topic(&dave_rpc, quota_topic).await?;

	let mid = create_test_statement(&keypair_0, &[quota_topic], None, vec![15], u32::MAX, 150);
	assert_eq!(submit_statement(&alice_rpc, &mid).await?, SubmitResult::New);

	// seq=250 evicts seq=150 → store: [200, 250, 300]
	let high = create_test_statement(&keypair_0, &[quota_topic], None, vec![25], u32::MAX, 250);
	assert_eq!(submit_statement(&alice_rpc, &high).await?, SubmitResult::New);

	// seq=190 rejected — slots now hold 200, 250, 300
	let too_low = create_test_statement(&keypair_0, &[quota_topic], None, vec![19], u32::MAX, 190);
	assert!(matches!(
		submit_statement(&alice_rpc, &too_low).await?,
		SubmitResult::Rejected(RejectionReason::AccountFull { .. })
	));

	// Verify eviction-triggered statements propagate to all nodes
	for (name, sub) in [
		("bob", &mut bob_evict_sub),
		("charlie", &mut charlie_evict_sub),
		("dave", &mut dave_evict_sub),
	] {
		let received = expect_statements_unordered(sub, 1, 30).await?;
		info!("{}: eviction statements propagated ({} received)", name, received.len());
	}

	Ok(())
}

async fn spawn_flooding_network(
	rate_limit: u32,
	participant_count: u32,
) -> Result<Network<LocalFileSystem>, anyhow::Error> {
	let images = zombienet_sdk::environment::get_images_from_env();
	let base_dir = base_dir()?;
	let chain_spec_path = create_chain_spec_with_allowances(participant_count, &base_dir)?;

	let default_args = collator_args(participant_count, COLLATOR_TRACE_LOG_FILTER);
	let mut bob_args = default_args.clone();
	bob_args.push(format!("--statement-rate-limit={rate_limit}").as_str().into());

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
			p.with_id(1004)
				.with_chain_spec_path(chain_spec_path.to_str().expect("Valid UTF-8 path"))
				.with_default_command("polkadot-parachain")
				.with_default_image(images.cumulus.as_str())
				.with_default_args(default_args)
				.with_collator(|n| n.with_name("alice"))
				.with_collator(|n| n.with_name("bob").with_args(bob_args))
		})
		.with_global_settings(|global_settings| {
			global_settings.with_base_dir(base_dir.to_str().expect("Valid UTF-8 path"))
		})
		.build()
		.map_err(super::common::format_build_errors)?;

	let network = crate::utils::initialize_network(config).await?;
	assert!(network.wait_until_is_up(60).await.is_ok());
	Ok(network)
}

/// Verifies sustained-rate flooding detection.
///
/// Submissions arrive faster than the sustained rate limit allows. Early batches
/// fit within the burst allowance and are accepted, but tokens drain over time
/// until the rate limiter kicks in.
#[tokio::test(flavor = "multi_thread")]
async fn statement_store_sustained_rate_flooding() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	// Low enough that batches can be submitted within 1s via RPC.
	let rate_limit = 50u32;
	let bucket_capacity = rate_limit * STATEMENTS_BURST_COEFFICIENT;
	// Half the burst capacity so the bucket drains gradually over several batches.
	let batch_size = bucket_capacity / 2;
	// Enough batches to drain the bucket, plus a margin.
	let batches_needed = bucket_capacity / (batch_size - rate_limit) + 1;
	let network = spawn_flooding_network(rate_limit, batch_size * batches_needed).await?;

	let alice = network.get_node("alice")?;
	let bob = network.get_node("bob")?;

	wait_for_first_block(&[alice, bob], 300).await?;

	let bob_peers_before = Cell::new(0.0f64);
	bob.wait_metric_with_timeout(
		"substrate_sub_libp2p_peers_count",
		|v| {
			bob_peers_before.set(v);
			true
		},
		10u64,
	)
	.await?;

	let alice_rpc = Arc::new(alice.rpc().await?);

	let submit_handle = tokio::spawn({
		let alice_rpc = Arc::clone(&alice_rpc);
		async move {
			let topic: Topic = [42u8; 32].into();
			for batch in 0..batches_needed {
				let now = tokio::time::Instant::now();
				let start = batch * batch_size;
				for idx in start..start + batch_size {
					let keypair = get_keypair(idx);
					let statement = create_test_statement(
						&keypair,
						&[topic],
						None,
						vec![idx as u8],
						u32::MAX,
						0,
					);
					let _ = submit_statement(&alice_rpc, &statement).await;
				}
				info!("Batch {}: submitted {} statements", batch, batch_size);
				let elapsed = now.elapsed();
				if elapsed < Duration::from_secs(1) {
					tokio::time::sleep(Duration::from_secs(1) - elapsed).await;
				}
			}
		}
	});

	bob.wait_metric_with_timeout(
		"substrate_sync_statement_flooding_detected",
		|count| count >= 1.0,
		120u64,
	)
	.await?;
	info!("Bob detected sustained-rate flooding");

	bob.wait_metric_with_timeout(
		"substrate_sub_libp2p_peers_count",
		|count| count < bob_peers_before.get(),
		30u64,
	)
	.await?;
	info!("Bob disconnected the flooding peer");

	submit_handle.abort();

	let bob_submitted = Cell::new(0.0f64);
	bob.wait_metric_with_timeout(
		"substrate_sub_statement_store_submitted_statements",
		|v| {
			bob_submitted.set(v);
			true
		},
		10u64,
	)
	.await?;
	assert!(
		bob_submitted.get() > 0.0,
		"Bob should have accepted early batches before flooding (got {})",
		bob_submitted.get()
	);
	info!("Bob accepted {} statements before flooding (sustained, not burst)", bob_submitted.get());

	Ok(())
}

/// Verifies burst flooding detection end-to-end.
///
/// The very first gossip batch already exceeds the burst allowance, so bob
/// rejects all statements immediately without accepting any.
#[tokio::test(flavor = "multi_thread")]
async fn statement_store_burst_flooding() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	// Low enough that batches can be submitted within 1s via RPC.
	let rate_limit = 50u32;
	// One more than the burst capacity so the first gossip batch overflows the bucket.
	let bucket_capacity = rate_limit * STATEMENTS_BURST_COEFFICIENT + 1;
	let network = spawn_flooding_network(rate_limit, bucket_capacity).await?;

	let alice = network.get_node("alice")?;
	let bob = network.get_node("bob")?;

	wait_for_first_block(&[alice, bob], 300).await?;

	let bob_peers_before = Cell::new(0.0f64);
	bob.wait_metric_with_timeout(
		"substrate_sub_libp2p_peers_count",
		|v| {
			bob_peers_before.set(v);
			true
		},
		10u64,
	)
	.await?;

	let alice_rpc = alice.rpc().await?;
	let topic: Topic = [43u8; 32].into();

	// Pre-create statements so submission isn't paced by keypair derivation.
	let statements: Vec<Statement> = (0..bucket_capacity)
		.map(|idx| {
			let keypair = get_keypair(idx);
			create_test_statement(&keypair, &[topic], None, vec![idx as u8], u32::MAX, 0)
		})
		.collect();

	// Submit concurrently so the full burst reaches alice within a single
	// `PROPAGATE_TIMEOUT` tick (1s). Sequential RPC submissions on CI can
	// straddle a tick, splitting the burst into two batches that each fit
	// inside the token-bucket refill — flooding would never be detected.
	let submissions = statements.iter().map(|statement| submit_statement(&alice_rpc, statement));
	join_all(submissions).await;
	info!("Submitted {} statements to alice", bucket_capacity);

	bob.wait_metric_with_timeout(
		"substrate_sync_statement_flooding_detected",
		|count| count >= 1.0,
		60u64,
	)
	.await?;
	info!("Bob detected burst flooding");

	bob.wait_metric_with_timeout(
		"substrate_sub_libp2p_peers_count",
		|count| count < bob_peers_before.get(),
		30u64,
	)
	.await?;
	info!("Bob disconnected the flooding peer");

	let bob_submitted = Cell::new(0.0f64);
	bob.wait_metric_with_timeout(
		"substrate_sub_statement_store_submitted_statements",
		|v| {
			bob_submitted.set(v);
			true
		},
		10u64,
	)
	.await?;
	assert_eq!(
		bob_submitted.get() as u64,
		0,
		"Bob should not have accepted any statements (burst, not sustained)"
	);
	info!("Bob accepted 0 statements (burst flooding confirmed)");

	Ok(())
}

/// Verifies that a node recovers its full statement store state after a crash/restart,
/// that other nodes remain unaffected during the outage, and that all statements
/// converge after recovery.
///
/// Scenario:
/// 1. Submit statements to alice and bob concurrently
/// 2. Wait for bob to receive at least one alice statement; a dedicated subscription keeps counting
///    until the kill, proving the crash caught the sync incomplete
/// 3. Restart bob (simulating crash mid-sync)
/// 4. While bob is restarting, submit statements to charlie
/// 5. After bob recovers, verify all recoverable statements converge on every node, and that bob
///    served most of them from its recovered database rather than re-sync
///
/// Each node's statements use a distinct topic so we can track provenance.
/// Statements are ~0.6 MiB each so only one fits per gossip notification,
/// creating a real time window for mid-sync interruption.
///
/// Known issue: ParityDB fsyncs asynchronously, so SIGKILL can lose the
/// last write. The test tolerates at most 1 lost statement.
///
/// Test uses the genesis-injection approach for setting allowances.
#[tokio::test(flavor = "multi_thread")]
async fn statement_store_crash_mid_sync() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let alice_count: usize = 50;
	let bob_count: usize = 10;
	let charlie_count: usize = 50;
	let total_stmts = alice_count + bob_count + charlie_count;
	let topic_alice: Topic = [0xA0; 32].into();
	let topic_bob: Topic = [0xB0; 32].into();
	let topic_charlie: Topic = [0xC0; 32].into();
	// Each statement is ~0.6 MiB so that only one fits per gossip notification
	// (limited to 1 MiB). This forces statements to be sent individually, creating
	// a real time window where bob can be killed mid-sync with partial state.
	let data_size = 600 * 1024;

	let mut keypair_idx = 0u32;
	let mut make_statements = |topic: Topic, count: usize| -> Vec<Statement> {
		(0..count)
			.map(|_| {
				let keypair = get_keypair(keypair_idx);
				keypair_idx += 1;
				create_test_statement(&keypair, &[topic], None, vec![0u8; data_size], u32::MAX, 0)
			})
			.collect()
	};

	let alice_stmts = make_statements(topic_alice, alice_count);
	let bob_stmts = make_statements(topic_bob, bob_count);
	let charlie_stmts = make_statements(topic_charlie, charlie_count);
	let hashes = |stmts: &[Statement]| stmts.iter().map(|s| s.hash()).collect::<HashSet<_>>();
	let (alice_hashes, bob_hashes, charlie_hashes) =
		(hashes(&alice_stmts), hashes(&bob_stmts), hashes(&charlie_stmts));

	let network =
		spawn_network_with_injected_allowances(&["alice", "bob", "charlie"], total_stmts as u32)
			.await?;

	let alice = network.get_node("alice")?;
	let bob = network.get_node("bob")?;
	let charlie = network.get_node("charlie")?;

	info!("Submitting statements: {} to alice, {} to bob", alice_count, bob_count);

	let alice_rpc = alice.rpc().await?;
	let alice_handle = tokio::spawn(async move {
		for (i, stmt) in alice_stmts.iter().enumerate() {
			let result = submit_statement(&alice_rpc, stmt).await?;
			assert_eq!(result, SubmitResult::New, "alice stmt[{}] rejected", i);
		}
		Ok::<_, anyhow::Error>(())
	});

	let bob_rpc = bob.rpc().await?;
	let bob_handle = tokio::spawn(async move {
		for (i, stmt) in bob_stmts.iter().enumerate() {
			let result = submit_statement(&bob_rpc, stmt).await?;
			assert_eq!(result, SubmitResult::New, "bob stmt[{}] rejected", i);
		}
		Ok::<_, anyhow::Error>(())
	});

	let bob_rpc = bob.rpc().await?;
	let gossip_handle = tokio::spawn(async move {
		let mut bob_alice_sub = subscribe_topic(&bob_rpc, topic_alice).await?;
		expect_statements_unordered(&mut bob_alice_sub, 1, 30).await
	});

	// Track bob's alice-sync progress on a dedicated subscription. It counts until the
	// stream dies together with bob's RPC server, so the total is exactly the set of alice
	// statements bob had been handed when it was killed: the proof that the crash was
	// mid-sync, and the lower bound for the recovery check after the restart.
	let bob_rpc = bob.rpc().await?;
	let sync_progress_handle = tokio::spawn(async move {
		let mut bob_alice_sub = subscribe_topic(&bob_rpc, topic_alice).await?;
		let mut received = 0usize;
		while let Ok(batch) = expect_statements_unordered(&mut bob_alice_sub, 1, 60).await {
			received += batch.len();
		}
		Ok::<_, anyhow::Error>(received)
	});

	// Restart is chained via map to ensure it fires immediately after try_join
	// completes, with no log output or other work in between that could give
	// bob extra time to sync. Do not decouple these operations.
	tokio::try_join!(bob_handle, gossip_handle)
		.map(|(bob_res, gossip_res)| {
			bob_res.expect("bob submissions failed");
			gossip_res.expect("gossip check failed");
			bob.restart(None)
		})?
		.await?;

	info!("Submissions completed, bob restarted (crash mid-sync)");
	assert!(bob.wait_until_is_up(1u64).await.is_err(), "Bob came up too fast");

	info!("Submitting {} statements to charlie while bob is restarting", charlie_count);
	let charlie_rpc = charlie.rpc().await?;
	for (i, stmt) in charlie_stmts.iter().enumerate() {
		let result = submit_statement(&charlie_rpc, stmt).await?;
		assert_eq!(result, SubmitResult::New, "charlie stmt[{}] rejected", i);
	}
	assert!(bob.wait_until_is_up(1u64).await.is_err(), "Bob was up during charlie submissions");

	info!("Waiting for bob to come back up");
	bob.wait_until_is_up(120u64).await?;

	// gossip_handle already confirmed bob received at least one alice statement,
	// so it's fine if alice finishes submitting after bob's restart.
	alice_handle.await?.expect("alice submissions failed");

	// The sync-progress subscription died with bob's RPC server, so its count is bob's
	// alice-sync state at the moment of the kill.
	let alice_at_kill = sync_progress_handle.await??;
	info!("Bob had received {}/{} alice statements when it was killed", alice_at_kill, alice_count);
	assert!(
		alice_at_kill < alice_count,
		"Bob had the complete alice set before the kill; the crash was not mid-sync",
	);

	// Everything recoverable must converge on every node. Alice's and charlie's statements
	// were submitted to nodes that never crashed, so none of them may be lost. Of bob's own,
	// SIGKILL may cost the last write (ParityDB fsyncs asynchronously, so SubmitResult::New
	// does not imply durability) — and that statement is only truly lost if it also never
	// propagated before the kill.
	let filter =
		TopicFilter::MatchAny(vec![topic_alice, topic_bob, topic_charlie].try_into().unwrap());
	let alice_rpc = alice.rpc().await?;
	let bob_rpc = bob.rpc().await?;
	let charlie_rpc = charlie.rpc().await?;
	let mut node_sets = Vec::new();
	for (name, rpc) in [("alice", &alice_rpc), ("bob", &bob_rpc), ("charlie", &charlie_rpc)] {
		let mut sub = subscribe_topic_filter(rpc, filter.clone()).await?;
		let mut received = expect_statements_unordered(&mut sub, total_stmts - 1, 120).await?;
		// Whether the potentially-lost statement survived is only known once it shows up;
		// give it a short grace period before concluding it is gone.
		if let Ok(extra) = expect_statements_unordered(&mut sub, 1, 15).await {
			received.extend(extra);
		}
		let set: HashSet<[u8; 32]> = received.iter().map(|encoded| hash_encoded(encoded)).collect();
		assert!(alice_hashes.is_subset(&set), "{} lost alice statements", name);
		assert!(charlie_hashes.is_subset(&set), "{} lost charlie statements", name);
		let bob_missing = bob_hashes.difference(&set).count();
		assert!(
			bob_missing <= 1,
			"{} lost {} bob statements, expected at most 1",
			name,
			bob_missing,
		);
		if bob_missing == 1 {
			log::warn!("Bob lost 1 statement due to crash (unflushed ParityDB write)");
		}
		debug!("{}: {} statements verified", name, set.len());
		node_sets.push(set);
	}
	assert!(
		node_sets.iter().all(|set| set == &node_sets[0]),
		"Nodes did not converge to the same statement set",
	);
	let converged_count = node_sets[0].len();
	info!("All nodes converged to the same {} statements", converged_count);

	// Distinguish recovery from re-sync: bob's post-restart metrics start at zero, so
	// `submitted_statements` counts only what gossip refilled after the crash, and the rest
	// of what bob serves now must have been recovered from its database. Bob had two write
	// streams going when it was killed — its own submissions and the incoming alice
	// statements — so allow for one unflushed tail write in each.
	let bob_resubmitted = Cell::new(0.0f64);
	bob.wait_metric_with_timeout(
		"substrate_sub_statement_store_submitted_statements",
		|v| {
			bob_resubmitted.set(v);
			true
		},
		10u64,
	)
	.await?;
	let bob_resubmitted = bob_resubmitted.get() as usize;
	assert!(
		bob_resubmitted <= converged_count,
		"Bob accepted {} statements after the restart but serves only {}",
		bob_resubmitted,
		converged_count,
	);
	let recovered = converged_count - bob_resubmitted;
	info!("Bob recovered {} statements from disk and re-synced {}", recovered, bob_resubmitted);
	assert!(
		recovered + 2 >= bob_count + alice_at_kill,
		"Bob recovered {} statements from its database, but at least {} were persisted \
		 before the crash",
		recovered,
		bob_count + alice_at_kill - 2,
	);

	info!("Node crash recovery test passed");
	Ok(())
}

/// Tests statement store submit+propagate using a lite person registered via extrinsics
///
/// Unlike the basic tests that use genesis-baked allowances, this test registers a lite person
/// via real extrinsics (increase_attestation_allowance + attest), and then verifies the registered
/// candidate can submit and propagate statements
#[tokio::test(flavor = "multi_thread")]
async fn statement_store_lite_person_submit_and_propagate() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let network = spawn_network(&["alice", "bob"], COLLATOR_INFO_LOG_FILTER).await?;

	let alice_node = network.get_node("alice")?;
	let bob_node = network.get_node("bob")?;
	let para_client = online_client_from_node(alice_node).await?;

	let alice = subxt_signer::sr25519::dev::alice();
	let alice_account_id =
		<subxt_signer::sr25519::Keypair as Signer<CustomConfig>>::account_id(&alice);

	info!("Granting attestation allowance to Alice...");
	let increase_call = create_increase_allowance_call(alice_account_id.0.to_vec(), 1);
	let mut nonce = para_client.tx().await?.account_nonce(&alice_account_id).await?;
	info!("Alice nonce before increase_allowance: {nonce}");
	let _block_hash = submit_extrinsic(&para_client, &increase_call, &alice, nonce).await?;
	nonce += 1;
	info!("Attestation allowance granted");

	let candidate_pair = sr25519::Pair::from_seed(&[77u8; 32]);
	let candidate_account: [u8; 32] = candidate_pair.public().0;

	// Generate ring-VRF keypair
	let ring_secret = Crypto::new_secret([42u8; 32]);
	let ring_member = Crypto::member_from_secret(&ring_secret);
	let msg = {
		let candidate_encoded = candidate_account.encode();
		let ring_member_encoded = ring_member.encode();
		[MSG_PREFIX.as_slice(), &candidate_encoded, &ring_member_encoded].concat()
	};
	let candidate_sig = candidate_pair.sign(&msg);

	let proof_of_ownership =
		Crypto::sign(&ring_secret, &msg).expect("ring VRF signing should succeed");

	// Consumer registration: Alice registers herself as consumer.
	// The consumer signs the payload; verifier is Alice (the attest origin)
	let alice_sp_pair =
		sr25519::Pair::from_string("//Alice", None).expect("Alice dev key should be valid");
	let consumer_registration = create_consumer_registration_params(
		&alice_sp_pair,
		&alice_account_id.0,
		&alice_account_id.0,
	);

	info!("Submitting PeopleLite::attest call with nonce {nonce}...");
	let attest_call = create_attest_call(
		candidate_account.to_vec(),
		candidate_sig.0.to_vec(),
		ring_member.0.to_vec(),
		proof_of_ownership.to_vec(),
		Some(consumer_registration),
	);
	submit_extrinsic(&para_client, &attest_call, &alice, nonce).await?;
	info!("Attest call succeeded — lite person registered with consumer allowance");

	let bob_rpc = bob_node.rpc().await?;
	let topic: Topic = [0u8; 32].into();
	let mut bob_sub = subscribe_topic(&bob_rpc, topic).await?;

	// Statement must be signed by Alice (the consumer) who has the statement store allowance
	let statement =
		create_test_statement(&alice_sp_pair, &[topic], None, vec![1, 2, 3], u32::MAX, 0);
	let expected: Bytes = statement.encode().into();

	let alice_rpc = alice_node.rpc().await?;
	let result = submit_statement(&alice_rpc, &statement).await?;
	assert_eq!(result, SubmitResult::New);

	let received = expect_one_statement(&mut bob_sub, 20).await?;
	assert_eq!(received, expected);
	assert_no_more_statements(&mut bob_sub, 20).await?;

	Ok(())
}

/// Verifies the `deferred_peers` buffer delivers statements to a late-joining node.
///
/// Dave joins after charlie has produced ~10 blocks and enters major sync. While syncing,
/// dave's statement handler holds charlie/alice's peer IDs in `deferred_peers` — no statement
/// substream opens until sync ends. Statements submitted both before and during dave's sync
/// window must all arrive via the single initial sync that fires when `drain_deferred_peers`
/// runs on sync completion.
#[tokio::test(flavor = "multi_thread")]
async fn statement_store_recovery_after_major_sync() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	const PRE_JOIN_COUNT: usize = 3;
	const DURING_SYNC_COUNT: usize = 2;
	const TOTAL: usize = PRE_JOIN_COUNT + DURING_SYNC_COUNT;
	let items = create_allowance_items(&[(
		0,
		StatementAllowance { max_count: TOTAL as u32, max_size: 1_000_000 },
	)]);
	let mut network =
		spawn_network_sudo(&["charlie", "alice"], items, COLLATOR_TRACE_LOG_FILTER).await?;

	let charlie = network.get_node("charlie")?;
	let charlie_rpc = charlie.rpc().await?;

	// Wait for at least 10 blocks so any late joiner reliably enters major sync
	let charlie_height = {
		let h = Cell::new(0.0f64);
		charlie
			.wait_metric_with_timeout(
				"block_height{status=\"best\"}",
				|v| {
					h.set(v);
					v >= 10.0
				},
				360u64,
			)
			.await
			.map_err(|_| anyhow::anyhow!("Charlie did not reach block 10 within 360s"))?;
		h.get()
	};
	info!("Charlie at block {:.0} before dave joins", charlie_height);

	let topic: Topic = [0u8; 32].into();
	let keypair = get_keypair(0);
	let pre_join: Vec<_> = (0..PRE_JOIN_COUNT as u32)
		.map(|seq| create_test_statement(&keypair, &[topic], None, vec![seq as u8], u32::MAX, seq))
		.collect();
	for stmt in &pre_join {
		assert_eq!(submit_statement(&charlie_rpc, stmt).await?, SubmitResult::New);
	}

	info!("Adding dave as late-joining collator");
	let dave_join_time = std::time::Instant::now();
	network.add_collator("dave", Default::default(), 1004).await?;
	let dave = network.get_node("dave")?;
	let dave_rpc = dave.rpc().await?;

	// Subscribe immediately after dave starts — the deferred_peers buffer prevents any
	// substream from opening while dave is syncing, so this subscription starts empty.
	let mut sub = subscribe_topic(&dave_rpc, topic).await?;

	// Dave holds charlie/alice's peer IDs in deferred_peers during sync; on sync-end
	// drain fires, substream opens, and charlie's initial sync delivers both batches
	let during_sync: Vec<_> = (PRE_JOIN_COUNT as u32..(PRE_JOIN_COUNT + DURING_SYNC_COUNT) as u32)
		.map(|seq| create_test_statement(&keypair, &[topic], None, vec![seq as u8], u32::MAX, seq))
		.collect();
	for stmt in &during_sync {
		assert_eq!(submit_statement(&charlie_rpc, stmt).await?, SubmitResult::New);
	}

	dave.wait_metric_with_timeout("block_height{status=\"best\"}", |h| h >= charlie_height, 120u64)
		.await
		.map_err(|_| {
			anyhow::anyhow!("Dave did not reach block height {:.0} within 120s", charlie_height)
		})?;
	let sync_end = dave_join_time.elapsed();
	info!("Dave synced to block {:.0} in {:.1}s", charlie_height, sync_end.as_secs_f64());

	let received = expect_statements_unordered(&mut sub, TOTAL, 30).await?;
	let mut expected: Vec<Vec<u8>> =
		pre_join.iter().chain(during_sync.iter()).map(|s| s.encode()).collect();
	expected.sort();
	let mut received_bytes: Vec<Vec<u8>> = received.into_iter().map(|b| b.to_vec()).collect();
	received_bytes.sort();
	assert_eq!(received_bytes, expected, "Dave must receive all {TOTAL} statements after sync");
	info!(
		"All {TOTAL} statements ({PRE_JOIN_COUNT} pre-join + {DURING_SYNC_COUNT} during-sync) \
		 arrived {:.1}s after dave finished syncing",
		dave_join_time.elapsed().as_secs_f64() - sync_end.as_secs_f64(),
	);

	// Verify drain_deferred_peers fired
	let dave_logs = dave.logs().await?;
	assert!(dave_logs.lines().any(|l| l.contains("Major sync complete, adding")));
	Ok(())
}

/// Verifies that a reconnecting subscriber receives the full current state
///
/// Scenario:
/// 1. Subscribe on bob, submit statements to alice → bob receives via gossip
/// 2. Drop subscription (disconnect)
/// 3. Submit more statements while bob is unsubscribed, wait for gossip
/// 4. Re-subscribe on bob → initial snapshot must contain ALL current statements
/// 5. Submit another statement → verify live delivery still works after reconnection
#[tokio::test(flavor = "multi_thread")]
async fn statement_store_subscription_reconnect() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let network = spawn_network_with_injected_allowances(&["alice", "bob"], 5).await?;
	let alice = network.get_node("alice")?;
	let bob = network.get_node("bob")?;
	let alice_rpc = alice.rpc().await?;
	let bob_rpc = bob.rpc().await?;

	let topic: Topic = [1u8; 32].into();
	let stmts: Vec<_> = (0..5u32)
		.map(|idx| {
			let keypair = get_keypair(idx);
			create_test_statement(&keypair, &[topic], None, vec![idx as u8], u32::MAX, 0)
		})
		.collect();

	let mut sub = subscribe_topic(&bob_rpc, topic).await?;
	for s in &stmts[..2] {
		assert_eq!(submit_statement(&alice_rpc, s).await?, SubmitResult::New);
	}
	let received = expect_statements_unordered(&mut sub, 2, 30).await?;
	assert_eq!(received.len(), 2);

	// Disconnect bob subs
	drop(sub);

	// Submit 2 more while bob is unsubscribed
	for s in &stmts[2..4] {
		assert_eq!(submit_statement(&alice_rpc, s).await?, SubmitResult::New);
	}
	tokio::time::sleep(Duration::from_secs(10)).await;

	// Re-subscribe → initial snapshot must contain all 4 statements
	let mut sub = subscribe_topic(&bob_rpc, topic).await?;
	let expected: Vec<Vec<u8>> = stmts[..4].iter().map(|s| s.encode()).collect();
	assert_statements_match(&mut sub, &expected, 30, "bob").await?;

	assert_eq!(submit_statement(&alice_rpc, &stmts[4]).await?, SubmitResult::New);

	let received = expect_one_statement(&mut sub, 30).await?;
	assert_eq!(received, Bytes::from(stmts[4].encode()), "Post-reconnect live delivery mismatch");
	assert_no_more_statements(&mut sub, 10).await?;

	Ok(())
}

/// Verifies that multiple new peers joining a stable network each receive the
/// complete statement set via `schedule_initial_sync_for_peer` round-robin delivery
///
/// Scenario:
/// 1. Spawn a stable 2-node network (alice, bob) with injected allowances
/// 2. Submit 20 statements from a single keypair on a single topic
/// 3. Wait for full propagation to bob
/// 4. Add 3 new collators (charlie, dave, eve)
/// 5. Verify each new node receives all 20 statements with correct content
/// 6. Verify initial_sync_statements_sent metric increased on sender nodes
#[tokio::test(flavor = "multi_thread")]
async fn statement_store_initial_sync() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	const TOTAL_STMTS: usize = 20;

	let topic: Topic = [0xA1; 32].into();
	let filter = TopicFilter::MatchAll(vec![topic].try_into().unwrap());

	let mut network = spawn_network_with_injected_allowances(&["alice", "bob"], 1).await?;

	let keypair = get_keypair(0);
	let all_statements: Vec<Statement> = (0..TOTAL_STMTS as u32)
		.map(|seq| create_test_statement(&keypair, &[topic], None, vec![seq as u8], u32::MAX, seq))
		.collect();

	let expected_encoded: Vec<Vec<u8>> = all_statements.iter().map(|s| s.encode()).collect();

	{
		let alice = network.get_node("alice")?;
		let bob = network.get_node("bob")?;
		let alice_rpc = alice.rpc().await?;
		let bob_rpc = bob.rpc().await?;

		// Submit all statements to alice; subscribe on bob to verify propagation
		let mut bob_sub = subscribe_topic_filter(&bob_rpc, filter.clone()).await?;
		for (i, stmt) in all_statements.iter().enumerate() {
			let result = submit_statement(&alice_rpc, stmt).await?;
			assert_eq!(result, SubmitResult::New, "Statement {} rejected", i);
		}
		assert_statements_match(&mut bob_sub, &expected_encoded, 60, "bob").await?;
	}

	let new_collators = ["charlie", "dave", "eve"];
	for name in &new_collators {
		network.add_collator(*name, Default::default(), 1004).await?;
	}

	let alice = network.get_node("alice")?;
	let bob = network.get_node("bob")?;
	let charlie = network.get_node("charlie")?;
	let dave = network.get_node("dave")?;
	let eve = network.get_node("eve")?;

	let charlie_rpc = charlie.rpc().await?;
	let dave_rpc = dave.rpc().await?;
	let eve_rpc = eve.rpc().await?;

	let mut charlie_sub = subscribe_topic_filter(&charlie_rpc, filter.clone()).await?;
	let mut dave_sub = subscribe_topic_filter(&dave_rpc, filter.clone()).await?;
	let mut eve_sub = subscribe_topic_filter(&eve_rpc, filter).await?;

	let alice_height = Cell::new(0.0f64);
	alice
		.wait_metric_with_timeout(
			"block_height{status=\"best\"}",
			|v| {
				alice_height.set(v);
				true
			},
			10u64,
		)
		.await?;
	let target_height = alice_height.get();

	for (name, node) in [("charlie", &charlie), ("dave", &dave), ("eve", &eve)] {
		node.wait_metric_with_timeout(
			"block_height{status=\"best\"}",
			|h| h >= target_height,
			120u64,
		)
		.await
		.map_err(|_| {
			anyhow::anyhow!("{} did not reach block height {:.0} within 120s", name, target_height)
		})?;
		info!("{} synced to block {:.0}", name, target_height);
	}

	for (name, sub) in
		[("charlie", &mut charlie_sub), ("dave", &mut dave_sub), ("eve", &mut eve_sub)]
	{
		assert_statements_match(sub, &expected_encoded, 60, name).await?;
		assert_no_more_statements(sub, 10).await?;
	}

	let alice_sent_after = Cell::new(0.0f64);
	alice
		.wait_metric_with_timeout(
			"substrate_sync_initial_sync_statements_sent",
			|v| {
				alice_sent_after.set(v);
				true
			},
			10u64,
		)
		.await?;
	let bob_sent_after = Cell::new(0.0f64);
	bob.wait_metric_with_timeout(
		"substrate_sync_initial_sync_statements_sent",
		|v| {
			bob_sent_after.set(v);
			true
		},
		10u64,
	)
	.await?;
	let total_sent = alice_sent_after.get() + bob_sent_after.get();
	assert!(
		total_sent >= TOTAL_STMTS as f64,
		"Initial sync sent only {} statements total (alice: {}, bob: {}), expected at least {}",
		total_sent,
		alice_sent_after.get(),
		bob_sent_after.get(),
		TOTAL_STMTS,
	);

	Ok(())
}

/// Verifies that concurrent `submit_statement` calls are not lost while
/// `enforce_limits` evicts a large batch of expired entries from the index
/// and DB.
///
/// Scenario:
/// 1. Insert 10000 ephemeral statements (20 accounts × 500, `ttl = 360s`) and wait until a
///    subscription confirms all are indexed.
/// 2. Starting 5s before TTL expiry, stream 500 persistent statements (5 accounts × 100) over a 95s
///    window. `enforce_limits` runs every 62s, so the window is guaranteed to overlap a cleanup
///    pass — inserts and bulk eviction of the 10000 expired ephemerals hit the index and DB at the
///    same time.
/// 3. Immediately after the window closes, a fresh subscription (which replays from the DB) must
///    yield exactly the 500 persistent statements — proving no insert was dropped and every expired
///    ephemeral was removed from index and DB within the overlap window itself.
#[tokio::test(flavor = "multi_thread")]
async fn statement_store_mass_expiration() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let network = spawn_network_with_injected_allowances(&["alice", "bob"], 25).await?;
	let alice = network.get_node("alice")?;
	let alice_rpc = alice.rpc().await?;

	let topic_a: Topic = [30u8; 32].into();
	let now_secs = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.expect("Time went backwards")
		.as_secs() as u32;

	// `enforce_limits` runs every `ENFORCE_LIMITS_PERIOD = 31s` with a two-phase
	// design: the 1st tick only snapshots accounts and returns, the 2nd actually
	// evicts — so a full eviction pass takes 2 × 31s = 62s.
	//
	// - pre_expiry_lead (5s):  start persistents before TTL expiry so inserts cross the expiry
	//   boundary mid-stream.
	// - overlap_window  (95s): > 62s guarantees a full eviction pass (snapshot tick + work tick)
	//   lands inside the window regardless of phase. A single work tick can remove up to 10k
	//   entries (`MAX_EXPIRY_STATEMENTS_PER_ITERATION`), so all 10k expired ephemerals must be
	//   drained before the window closes.
	let ephemeral_ttl: u32 = 360;
	let pre_expiry_lead: u64 = 5;
	let overlap_window: u64 = 95;
	let ephemeral_expiry = now_secs + ephemeral_ttl;

	// 10_000 ephemeral statements across 20 accounts (500 per account)
	let num_ephemeral_accounts: u32 = 20;
	let stmts_per_ephemeral: u32 = 500;
	let ephemeral_stmts: Vec<_> = (0..num_ephemeral_accounts)
		.flat_map(|kp| {
			let keypair = get_keypair(kp);
			(0..stmts_per_ephemeral).map(move |seq| {
				let mut data = kp.to_le_bytes().to_vec();
				data.extend_from_slice(&seq.to_le_bytes());
				create_test_statement(
					&keypair,
					&[topic_a],
					None,
					data,
					ephemeral_expiry,
					kp * 1000 + seq,
				)
			})
		})
		.collect();

	let alice_rpc_arc = Arc::new(alice.rpc().await?);
	let mut handles = Vec::new();
	for stmt in &ephemeral_stmts {
		let stmt = stmt.clone();
		let rpc = Arc::clone(&alice_rpc_arc);
		handles.push(tokio::spawn(async move {
			let result = submit_statement(&rpc, &stmt).await?;
			assert_eq!(result, SubmitResult::New);
			Ok::<_, anyhow::Error>(())
		}));
	}
	for handle in handles {
		handle.await??;
	}

	let eph_encoded: Vec<Vec<u8>> = ephemeral_stmts.iter().map(|s| s.encode()).collect();
	let mut fill_sub = subscribe_topic(&alice_rpc, topic_a).await?;
	assert_statements_match(&mut fill_sub, &eph_encoded, 180, "alice").await?;
	drop(fill_sub);

	// 500 persistent statements across 5 accounts (100 per account)
	let num_persistent_accounts: u32 = 5;
	let stmts_per_persistent: u32 = 100;
	let persistent_base = num_ephemeral_accounts;
	let persistent_stmts: Vec<_> = (persistent_base..persistent_base + num_persistent_accounts)
		.flat_map(|kp| {
			let keypair = get_keypair(kp);
			(0..stmts_per_persistent).map(move |seq| {
				let mut data = kp.to_le_bytes().to_vec();
				data.extend_from_slice(&seq.to_le_bytes());
				create_test_statement(&keypair, &[topic_a], None, data, u32::MAX, kp * 1000 + seq)
			})
		})
		.collect();

	let elapsed = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.expect("Time went backwards")
		.as_secs() as u32 -
		now_secs;
	let wait_until_overlap = (ephemeral_ttl as u64)
		.saturating_sub(elapsed as u64)
		.saturating_sub(pre_expiry_lead);
	info!("Waiting {}s before opening overlap window", wait_until_overlap);
	tokio::time::sleep(Duration::from_secs(wait_until_overlap)).await;

	// Submit persistents at uniform intervals across the overlap window
	let interval_ms = (overlap_window * 1000) / persistent_stmts.len() as u64;
	let mut handles = Vec::new();
	for (i, stmt) in persistent_stmts.iter().cloned().enumerate() {
		let rpc = Arc::clone(&alice_rpc_arc);
		let delay = Duration::from_millis(interval_ms * i as u64);
		handles.push(tokio::spawn(async move {
			tokio::time::sleep(delay).await;
			let result = submit_statement(&rpc, &stmt).await?;
			assert_eq!(result, SubmitResult::New);
			Ok::<_, anyhow::Error>(())
		}));
	}
	for handle in handles {
		handle.await??;
	}

	// Fresh subscription must see exactly the 500 persistent statements
	let persistent_encoded: Vec<Vec<u8>> = persistent_stmts.iter().map(|s| s.encode()).collect();
	let mut verify_sub = subscribe_topic(&alice_rpc, topic_a).await?;
	assert_statements_match(&mut verify_sub, &persistent_encoded, 120, "alice").await?;
	assert_no_more_statements(&mut verify_sub, 10).await?;

	Ok(())
}
