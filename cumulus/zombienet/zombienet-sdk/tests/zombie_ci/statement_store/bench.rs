// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Benchmarking statement store performance

use anyhow::anyhow;
use codec::Encode;
use futures::stream::{FuturesUnordered, StreamExt};
use log::{debug, info};
use sc_statement_store::{
	test_utils::get_keypair, DEFAULT_MAX_TOTAL_SIZE, DEFAULT_MAX_TOTAL_STATEMENTS,
};
use sp_core::{Bytes, Pair};
use sp_crypto_hashing::blake2_256;
use sp_statement_store::{Statement, StatementEvent, SubmitResult, Topic, TopicFilter};
use std::{cell::Cell, collections::HashMap, sync::Arc, time::Duration};
use tokio::{sync::Barrier, time::timeout};
use zombienet_sdk::subxt::{backend::rpc::RpcClient, ext::subxt_rpcs::rpc_params};

use super::common::{spawn_network_with_injected_allowances, RPC_POOL_SIZE};

/// Memory stress benchmark.
///
/// Tests statement store memory usage under extreme load. Network spawned with 6 collator nodes.
/// Concurrent tasks send statements to a single target node until the store is full. The test ends
/// when all statements are propagated.
///
/// # Output
/// Logs real-time metrics every 5 seconds with the following data per node:
/// - Submitted statements: total count, percentage of capacity, submission rate
/// - Propagated statements: total count, percentage of propagation capacity, propagation rate
/// - Elapsed time since test start
/// - Final completion status when submit capacity is reached across all nodes
#[tokio::test(flavor = "multi_thread")]
async fn statement_store_memory_stress_bench() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let total_tasks = 64 * 1024;
	let payload_size = 1024;
	let submit_capacity =
		DEFAULT_MAX_TOTAL_STATEMENTS.min(DEFAULT_MAX_TOTAL_SIZE / payload_size) as u64;
	let statements_per_task = submit_capacity / total_tasks as u64;

	let collator_names = ["alice", "bob", "charlie", "dave", "eve", "ferdie"];
	let network = spawn_network_with_injected_allowances(&collator_names, total_tasks).await?;

	let target_node = collator_names[0];
	let node = network.get_node(target_node)?;
	let mut rpc_pool = Vec::with_capacity(RPC_POOL_SIZE);
	for _ in 0..RPC_POOL_SIZE {
		rpc_pool.push(node.rpc().await?);
	}
	info!("Created RPC connection pool with {} connections to {}", RPC_POOL_SIZE, target_node);

	let num_collators = collator_names.len() as u64;
	let propagation_capacity = submit_capacity * (num_collators - 1); // 5x per node
	let start_time = std::time::Instant::now();

	info!(
		"Starting memory stress benchmark with {} tasks, each submitting {} statements of {}B payload, total submit capacity per node: {}, total propagation capacity: {}",
		total_tasks, statements_per_task, payload_size, submit_capacity, propagation_capacity
	);

	for idx in 0..total_tasks {
		let rpc_client = rpc_pool[idx as usize % RPC_POOL_SIZE].clone();
		tokio::spawn(async move {
			let keyring = get_keypair(idx);
			let public = keyring.public().0;

			for statement_count in 0..statements_per_task {
				let mut statement = Statement::new();
				let topic = |idx: usize| -> Topic {
					blake2_256(format!("{idx}{statement_count}{public:?}").as_bytes()).into()
				};
				statement.set_topic(0, topic(0));
				statement.set_topic(1, topic(1));
				statement.set_topic(2, topic(2));
				statement.set_topic(3, topic(3));
				statement.set_expiry_from_parts(u32::MAX, statement_count as u32);
				statement.set_plain_data(vec![0u8; payload_size]);
				statement.sign_sr25519_private(&keyring);

				loop {
					let statement_bytes: Bytes = statement.encode().into();
					let Err(err) = rpc_client
						.request::<SubmitResult>("statement_submit", rpc_params![statement_bytes])
						.await
					else {
						break; // Successfully submitted
					};

					if err.to_string().contains("Statement store error: Store is full") {
						info!(
							"Statement store is full, {}/{} statements submitted, `statements_per_task` overestimated",
							statement_count, statements_per_task
						);
						break;
					}

					info!("Failed to submit statement, retrying in {}ms: {:?}", 500, err);
					tokio::time::sleep(Duration::from_millis(500)).await;
				}
			}
		});
	}

	info!("All {} tasks spawned in {:.2}s", total_tasks, start_time.elapsed().as_secs_f64());

	let mut prev_submitted: HashMap<&str, u64> = HashMap::new();
	let mut prev_propagated: HashMap<&str, u64> = HashMap::new();
	for &name in &collator_names {
		prev_submitted.insert(name, 0);
		prev_propagated.insert(name, 0);
	}

	loop {
		let interval = 5;
		tokio::time::sleep(Duration::from_secs(interval)).await;
		let elapsed = start_time.elapsed().as_secs();

		// Collect submitted metrics
		let mut submitted_metrics = Vec::new();
		for &name in &collator_names {
			let node = network.get_node(name)?;
			let prev_count = prev_submitted.get(name).copied().unwrap_or(0);

			let current_count = Cell::new(0.0f64);
			node.wait_metric_with_timeout(
				"substrate_sub_statement_store_submitted_statements",
				|count| {
					current_count.set(count);
					true
				},
				30u64,
			)
			.await?;

			let count = current_count.get() as u64;
			let delta = count - prev_count;
			let rate = delta / interval;
			submitted_metrics.push((name, count, rate));
			prev_submitted.insert(name, count);
		}

		// Collect propagated metrics
		let mut propagated_metrics = Vec::new();
		for &name in &collator_names {
			let node = network.get_node(name)?;
			let prev_count = prev_propagated.get(name).copied().unwrap_or(0);

			let current_count = Cell::new(0.0f64);
			node.wait_metric_with_timeout(
				"substrate_sync_propagated_statements",
				|count| {
					current_count.set(count);
					true
				},
				30u64,
			)
			.await?;

			let count = current_count.get() as u64;
			let delta = count - prev_count;
			let rate = delta / interval;
			propagated_metrics.push((name, count, rate));
			prev_propagated.insert(name, count);
		}

		info!("[{:>3}s]  Statements  submitted                 propagated", elapsed);
		for i in 0..collator_names.len() {
			let (sub_name, sub_count, sub_rate) = submitted_metrics[i];
			let (prop_name, prop_count, prop_rate) = propagated_metrics[i];
			assert_eq!(sub_name, prop_name);

			let sub_percentage = sub_count * 100 / submit_capacity;
			let prop_percentage = prop_count * 100 / propagation_capacity;

			info!(
				"         {:<8}  {:>8} {:>3}% {:>8}/s   {:>8} {:>3}% {:>8}/s",
				sub_name,
				sub_count,
				sub_percentage,
				sub_rate,
				prop_count,
				prop_percentage,
				prop_rate
			);
		}

		let total_submitted: u64 = submitted_metrics.iter().map(|(_, count, _)| *count).sum();
		if total_submitted == submit_capacity * num_collators {
			info!(
				"Reached total submit capacity of {} statements per node in {}s, benchmark completed successfully",
				submit_capacity, elapsed
			);
			break;
		}
	}

	Ok(())
}

struct LatencyBenchConfig {
	num_rounds: usize,
	num_nodes: usize,
	num_clients: u32,
	max_retries: u32,
	interval_ms: u64,
	req_timeout_ms: u64,
	messages_pattern: &'static [(usize, usize)],
}

impl LatencyBenchConfig {
	fn messages_per_client(&self) -> usize {
		self.messages_pattern.iter().map(|(count, _)| count).sum()
	}
}

#[derive(Debug, Clone)]
struct RoundStats {
	send_duration: Duration,
	receive_duration: Duration,
	full_latency: Duration,
	sent_count: u32,
	received_count: u32,
	receive_attempts: u32,
}

#[tokio::test(flavor = "multi_thread")]
async fn statement_store_latency_bench() -> Result<(), anyhow::Error> {
	let _ = env_logger::try_init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let config = Arc::new(LatencyBenchConfig {
		num_nodes: 5,
		num_clients: 50000,
		interval_ms: 10000,
		num_rounds: 1,
		messages_pattern: &[(5, 1024 / 2)],
		max_retries: 500,
		req_timeout_ms: 3000,
	});

	let collator_names: Vec<String> =
		(0..config.num_nodes).map(|i| format!("collator{i}")).collect();
	let collator_names: Vec<&str> = collator_names.iter().map(|s| s.as_str()).collect();

	let network =
		spawn_network_with_injected_allowances(&collator_names, config.num_clients).await?;

	info!("Starting Latency benchmark");
	info!("");
	info!("Clients: {}", config.num_clients);
	info!("Nodes: {}", config.num_nodes);
	info!("Rounds: {}", config.num_rounds);
	info!("Interval, ms: {}", config.interval_ms);
	info!("Messages, per round: {}", config.messages_per_client() as u32 * config.num_clients);
	info!("Message pattern:");
	for &(count, size) in config.messages_pattern {
		info!(" - {} messages {} bytes", count, size);
	}
	info!("");

	let clients_per_node = config.num_clients as usize / config.num_nodes;
	let pool_size_per_node = RPC_POOL_SIZE.min(clients_per_node);
	let mut rpc_pools: Vec<Vec<RpcClient>> = Vec::new();
	for &name in &collator_names {
		let node = network.get_node(name)?;
		let mut pool = Vec::with_capacity(pool_size_per_node);
		for _ in 0..pool_size_per_node {
			pool.push(node.rpc().await?);
		}
		rpc_pools.push(pool);
	}
	info!(
		"Created RPC connection pool: {} connections x {} nodes = {} total",
		pool_size_per_node,
		collator_names.len(),
		pool_size_per_node * collator_names.len()
	);

	let barrier = Arc::new(Barrier::new(config.num_clients as usize));
	let sync_start = std::time::Instant::now();

	// Generate unique test run ID using timestamp to avoid interference with old data
	let test_run_id = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap()
		.as_micros() as u64;

	let handles: Vec<_> = (0..config.num_clients)
		.map(|client_id| {
			let config = Arc::clone(&config);
			let barrier = Arc::clone(&barrier);
			let keyring = get_keypair(client_id);
			let node_idx = (client_id as usize) % config.num_nodes;
			let conn_idx = (client_id as usize / config.num_nodes) % pool_size_per_node;
			let rpc_client = rpc_pools[node_idx][conn_idx].clone();
			let neighbour_id = (client_id + 1) % config.num_clients;
			let neighbour_node_idx = (neighbour_id as usize) % config.num_nodes;
			if node_idx == neighbour_node_idx && config.num_nodes > 1 {
				panic!(
					"Client {client_id} and neighbour {neighbour_id} are on the same node {node_idx}!"
				);
			}

			tokio::spawn(async move {
				barrier.wait().await;

				if client_id == 0 {
					let sync_time = sync_start.elapsed();
					debug!(
						"All {} tasks synchronized and starting work in {:.3}s",
						config.num_clients,
						sync_time.as_secs_f64()
					);
				}

				let submission_jitter = (client_id % 1000) as u64;
				tokio::time::sleep(Duration::from_millis(submission_jitter)).await;

				let mut rounds_stats = Vec::new();
				for round in 0..config.num_rounds {
					let round_start = std::time::Instant::now();

					// Create subscriptions for messages we expect to receive
					if client_id == 0 {
						info!("Creating subscriptions for expected messages");
					}

					let mut subscriptions = Vec::new();
					for msg_idx in 0..config.messages_per_client() as u32 {
						let topic_str = format!("{test_run_id}-{client_id}-{round}-{msg_idx}");

						if client_id == 0 {
							info!("Subscribed {msg_idx} message(s) {topic_str:?}");
						}

						let topic: Topic = blake2_256(topic_str.as_bytes()).into();

						let subscription = rpc_client
							.subscribe::<StatementEvent>(
								"statement_subscribeStatement",
								rpc_params![TopicFilter::MatchAll(
									vec![topic].try_into().expect("Single topic")
								)],
								"statement_unsubscribeStatement",
							)
							.await
							.map_err(|e| {
								anyhow!(
									"Client {}: Failed to subscribe for message {} from neighbour {}: {}",
									client_id,
									msg_idx,
									neighbour_id,
									e
								)
							})?;
						subscriptions.push((msg_idx, topic_str, subscription));
					}

					if client_id == 0 {
						info!("Created {} subscriptions", subscriptions.len());
					}

					// Step 2: Send messages
					let mut msg_idx: u32 = 0;

					if client_id == 0 {
						info!("Start sending messages");
					}

					for &(count, size) in config.messages_pattern {
						for _ in 0..count {
							let mut statement = Statement::new();

							let topic_str = format!("{test_run_id}-{client_id}-{round}-{msg_idx}");
							let topic = blake2_256(topic_str.as_bytes());
							let channel = blake2_256(msg_idx.to_le_bytes().as_ref());

							// Use timestamp for priority
							let timestamp_ms = std::time::SystemTime::now()
								.duration_since(std::time::UNIX_EPOCH)
								.unwrap()
								.as_millis() as u32;

							statement.set_channel(channel);
							statement.set_expiry_from_parts(u32::MAX, timestamp_ms);
							statement.set_topic(0, topic.into());
							statement.set_plain_data(vec![0u8; size]);
							statement.sign_sr25519_private(&keyring);

							let encoded: Bytes = statement.encode().into();
							let result: SubmitResult = rpc_client
								.request("statement_submit", rpc_params![encoded])
								.await?;

							msg_idx += 1;
							if client_id == 0 {
								info!("Sent {msg_idx} message(s) {topic_str:?}, {result:?}");
							}
						}
					}

					let sent_count = msg_idx;
					let send_duration = round_start.elapsed();

					// Step 3: Wait for subscriptions to receive messages
					let receive_start = std::time::Instant::now();
					let mut received_count = 0;
					let receive_attempts = subscriptions.len() as u32;

					if client_id == 0 {
						info!("Start receiving messages via subscriptions");
					}

					let total_timeout =
						Duration::from_millis(config.req_timeout_ms * config.max_retries as u64);

					let mut futures: FuturesUnordered<_> = subscriptions
						.into_iter()
						.map(|(msg_idx, topic_str, mut subscription)| async move {
							match timeout(total_timeout, subscription.next()).await {
								Ok(Some(Ok(StatementEvent::NewStatements { .. }))) => {
									Ok((msg_idx, topic_str))
								},
								Ok(Some(Err(e))) => Err(anyhow!(
									"Subscription error for message {}: {}",
									msg_idx,
									e
								)),
								Ok(None) => Err(anyhow!(
									"Subscription ended unexpectedly for message {}",
									msg_idx
								)),
								Err(_) => Err(anyhow!("Timeout waiting for message {}", msg_idx)),
							}
						})
						.collect();

					while let Some(result) = futures.next().await {
						match result {
							Ok((msg_idx, topic_str)) => {
								received_count += 1;
								if client_id == 0 {
									info!(
										"Received {received_count} message(s) {topic_str:?} (msg_idx: {})",
										msg_idx
									);
								}
							},
							Err(e) => {
								return Err(anyhow!(
									"Client {}: Failed to receive message from neighbour {}: {}",
									client_id,
									neighbour_id,
									e
								));
							},
						}
					}

					let receive_duration = receive_start.elapsed();
					let full_latency = round_start.elapsed();
					if full_latency < Duration::from_millis(config.interval_ms) {
						tokio::time::sleep(
							Duration::from_millis(config.interval_ms) - full_latency,
						)
						.await;
					}

					rounds_stats.push(RoundStats {
						send_duration,
						receive_duration,
						full_latency,
						sent_count,
						received_count,
						receive_attempts,
					});
				}

				// Verify all messages were sent and received
				let expected_count = config.messages_per_client() as u32;
				for stats in &rounds_stats {
					if stats.sent_count != expected_count {
						return Err(anyhow!(
							"Client {}: Expected {} messages sent, but got {}",
							client_id,
							expected_count,
							stats.sent_count
						));
					}
					if stats.received_count != expected_count {
						return Err(anyhow!(
							"Client {}: Expected {} messages received, but got {}",
							client_id,
							expected_count,
							stats.received_count
						));
					}
				}

				Ok::<_, anyhow::Error>(rounds_stats)
			})
		})
		.collect();

	let mut all_round_stats = Vec::new();
	for handle in handles {
		let stats = handle.await??;
		all_round_stats.extend(stats);
	}

	let calc_stats = |values: Vec<f64>| -> (f64, f64, f64) {
		let min = values.iter().copied().fold(f64::INFINITY, f64::min);
		let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
		let avg = values.iter().sum::<f64>() / values.len() as f64;
		(min, avg, max)
	};

	let send_s =
		calc_stats(all_round_stats.iter().map(|s| s.send_duration.as_secs_f64()).collect());
	let read_s =
		calc_stats(all_round_stats.iter().map(|s| s.receive_duration.as_secs_f64()).collect());
	let latency_s =
		calc_stats(all_round_stats.iter().map(|s| s.full_latency.as_secs_f64()).collect());
	let attempts = calc_stats(all_round_stats.iter().map(|s| s.receive_attempts as f64).collect());
	let attempts_per_msg = (
		attempts.0 / config.messages_per_client() as f64,
		attempts.1 / config.messages_per_client() as f64,
		attempts.2 / config.messages_per_client() as f64,
	);

	info!("");
	info!("                      Min       Avg       Max");
	info!("Send, s             {:>8.3}  {:>8.3}  {:>8.3}", send_s.0, send_s.1, send_s.2);
	info!("Receive, s          {:>8.3}  {:>8.3}  {:>8.3}", read_s.0, read_s.1, read_s.2);
	info!("Latency, s          {:>8.3}  {:>8.3}  {:>8.3}", latency_s.0, latency_s.1, latency_s.2);
	info!(
		"Attempts, per msg   {:>8.1}  {:>8.1}  {:>8.1}",
		attempts_per_msg.0, attempts_per_msg.1, attempts_per_msg.2
	);

	Ok(())
}
