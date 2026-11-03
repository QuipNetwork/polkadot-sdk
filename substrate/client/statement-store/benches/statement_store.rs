// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Benchmarks for the statement store.
//!
//! Every benchmark drives the store through its public `StatementStore` API only, so the same file
//! compiles and runs against any revision of the crate (the in-memory index of stage 1, the
//! on-disk index of stage 2a, and the disk-backed write index of stage 2b). To compare two
//! revisions, run the baseline first and then the candidate:
//!
//! ```text
//! git checkout master           && cargo bench -p sc-statement-store -- --save-baseline before
//! git checkout <feature-branch> && cargo bench -p sc-statement-store -- --baseline before
//! ```
//!
//! If the candidate adds benchmarks the baseline lacks, copy this file onto the baseline first
//! (`git checkout <branch> -- substrate/client/statement-store/benches/statement_store.rs`) so both
//! revisions expose the same benchmark ids.
//!
//! The groups added for the on-disk index target what moving it to disk actually costs:
//! - `read_scaling`: read latency as a function of store size (flat in RAM, grows on disk);
//! - `submit_index_cost`: submit throughput vs. the number of on-disk index writes per statement;
//! - `submit_eviction`: submit throughput when every submit evicts (the `db.get` under the lock);
//! - `subscribe_topic`: subscribe with a topic and pull matching statements from a near-limit
//!   store;
//! - `propagate`: `take_recent_statements` gather for one propagation interval;
//! - `contention_read_under_write`: read latency while writers run concurrently.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use sc_statement_store::{Config, StatementStoreSubscriptionApi, Store};
use sp_core::Pair;
use sp_runtime::codec::Encode;
use sp_statement_store::{
	DecryptionKey, OptimizedTopicFilter, Statement, StatementSource, StatementStore, SubmitResult,
	Topic, MAX_TOPICS,
};
use std::sync::Arc;

type Extrinsic = sp_runtime::OpaqueExtrinsic;
type Hash = sp_core::H256;
type Hashing = sp_runtime::traits::BlakeTwo256;
type BlockNumber = u64;
type Header = sp_runtime::generic::Header<BlockNumber, Hashing>;
type Block = sp_runtime::generic::Block<Header, Extrinsic>;

const CORRECT_BLOCK_HASH: [u8; 32] = [1u8; 32];
const STATEMENT_DATA_SIZE: usize = 256;
const INITIAL_STATEMENTS: usize = 1_000;
const NUM_THREADS: usize = 64;
const OPS_PER_THREAD: usize = 10;
const TOTAL_OPS: usize = NUM_THREADS * OPS_PER_THREAD;

/// Store sizes (pre-loaded statements) for the `read_scaling` group.
const SCALING_SIZES: &[usize] = &[1_000, 10_000, 50_000];

/// Reader / writer thread counts for the contention benchmark.
const CONTENTION_READERS: usize = 32;
const CONTENTION_WRITERS: usize = 32;
/// Statements pre-loaded before the contention benchmark runs.
const CONTENTION_PRELOAD: usize = 2_000;

/// Per-account statement cap for the eviction benchmark. The account is filled to this many
/// statements so that every timed submit must evict exactly one.
const EVICTION_CAP: u32 = TOTAL_OPS as u32;

/// Store size for the near-limit read user-story benches. This is `DEFAULT_MAX_TOTAL_STATEMENTS`
/// (~4M), i.e. a full store, so the on-disk index has production depth. Building it takes several
/// minutes (one ed25519 verify per statement), so `subscribe_topic` is a manual-only bench.
const NEAR_LIMIT: usize = 4 * 1024 * 1024;

/// Statements sharing each topic in the diverse-topic fixture. Small, so a single-topic
/// subscription pulls just a handful of statements out of a full store (exercising a deep on-disk
/// topic index).
const SUBSCRIBE_MATCHES: usize = 8;

/// Statements drained per propagation interval by `take_recent_statements`.
const PROPAGATE_BATCH: usize = 1_000;

#[derive(Clone)]
struct TestClient {
	max_count: u32,
	max_size: u32,
}

impl TestClient {
	/// Effectively unlimited per-account allowance, so a single account can hold even the
	/// near-limit pre-loads without hitting per-account eviction (the global `Config` caps the
	/// store instead).
	fn generous() -> Self {
		Self { max_count: u32::MAX, max_size: u32::MAX }
	}

	/// Small statement-count cap for the eviction benchmark: once the account holds `max_count`
	/// statements, every further submit must evict one.
	fn capped(max_count: u32) -> Self {
		Self { max_count, max_size: 256 * 1024 * 1024 }
	}
}

type TestBackend = sc_client_api::in_mem::Backend<Block>;

impl sc_client_api::StorageProvider<Block, TestBackend> for TestClient {
	fn storage(
		&self,
		_hash: Hash,
		_key: &sc_client_api::StorageKey,
	) -> sp_blockchain::Result<Option<sc_client_api::StorageData>> {
		// Per-account allowance (count, then size in bytes), configured per store.
		Ok(Some(sc_client_api::StorageData((self.max_count, self.max_size).encode())))
	}

	fn storage_hash(
		&self,
		_hash: Hash,
		_key: &sc_client_api::StorageKey,
	) -> sp_blockchain::Result<Option<Hash>> {
		unimplemented!()
	}

	fn storage_keys(
		&self,
		_hash: Hash,
		_prefix: Option<&sc_client_api::StorageKey>,
		_start_key: Option<&sc_client_api::StorageKey>,
	) -> sp_blockchain::Result<
		sc_client_api::backend::KeysIter<
			<TestBackend as sc_client_api::Backend<Block>>::State,
			Block,
		>,
	> {
		unimplemented!()
	}

	fn storage_pairs(
		&self,
		_hash: Hash,
		_prefix: Option<&sc_client_api::StorageKey>,
		_start_key: Option<&sc_client_api::StorageKey>,
	) -> sp_blockchain::Result<
		sc_client_api::backend::PairsIter<
			<TestBackend as sc_client_api::Backend<Block>>::State,
			Block,
		>,
	> {
		unimplemented!()
	}

	fn child_storage(
		&self,
		_hash: Hash,
		_child_info: &sc_client_api::ChildInfo,
		_key: &sc_client_api::StorageKey,
	) -> sp_blockchain::Result<Option<sc_client_api::StorageData>> {
		unimplemented!()
	}

	fn child_storage_keys(
		&self,
		_hash: Hash,
		_child_info: sc_client_api::ChildInfo,
		_prefix: Option<&sc_client_api::StorageKey>,
		_start_key: Option<&sc_client_api::StorageKey>,
	) -> sp_blockchain::Result<
		sc_client_api::backend::KeysIter<
			<TestBackend as sc_client_api::Backend<Block>>::State,
			Block,
		>,
	> {
		unimplemented!()
	}

	fn child_storage_hash(
		&self,
		_hash: Hash,
		_child_info: &sc_client_api::ChildInfo,
		_key: &sc_client_api::StorageKey,
	) -> sp_blockchain::Result<Option<Hash>> {
		unimplemented!()
	}

	fn closest_merkle_value(
		&self,
		_hash: Hash,
		_key: &sc_client_api::StorageKey,
	) -> sp_blockchain::Result<Option<sc_client_api::MerkleValue<Hash>>> {
		unimplemented!()
	}

	fn child_closest_merkle_value(
		&self,
		_hash: Hash,
		_child_info: &sc_client_api::ChildInfo,
		_key: &sc_client_api::StorageKey,
	) -> sp_blockchain::Result<Option<sc_client_api::MerkleValue<Hash>>> {
		unimplemented!()
	}
}

impl sp_blockchain::HeaderBackend<Block> for TestClient {
	fn header(&self, _hash: Hash) -> sp_blockchain::Result<Option<Header>> {
		unimplemented!()
	}
	fn info(&self) -> sp_blockchain::Info<Block> {
		sp_blockchain::Info {
			best_hash: CORRECT_BLOCK_HASH.into(),
			best_number: 0,
			genesis_hash: Default::default(),
			finalized_hash: CORRECT_BLOCK_HASH.into(),
			finalized_number: 1,
			finalized_state: None,
			number_leaves: 0,
			block_gap: None,
		}
	}
	fn status(&self, _hash: Hash) -> sp_blockchain::Result<sp_blockchain::BlockStatus> {
		unimplemented!()
	}
	fn number(&self, _hash: Hash) -> sp_blockchain::Result<Option<BlockNumber>> {
		unimplemented!()
	}
	fn hash(&self, _number: BlockNumber) -> sp_blockchain::Result<Option<Hash>> {
		unimplemented!()
	}
}

fn topic(data: u64) -> Topic {
	let mut bytes = [0u8; 32];
	bytes[0..8].copy_from_slice(&data.to_le_bytes());
	Topic::from(bytes)
}

fn dec_key(data: u64) -> DecryptionKey {
	let mut dec_key: DecryptionKey = Default::default();
	dec_key[0..8].copy_from_slice(&data.to_le_bytes());
	dec_key
}

fn create_signed_statement(
	id: u64,
	topics: &[Topic],
	dec_key: Option<DecryptionKey>,
	keypair: &sp_core::ed25519::Pair,
) -> Statement {
	create_signed_statement_sized(id, topics, dec_key, STATEMENT_DATA_SIZE, u64::MAX, keypair)
}

/// Like [`create_signed_statement`] but with an explicit plain-data length and expiry. The length
/// lets a benchmark hold the encoded statement size roughly constant while varying the topic count
/// (isolating index-write cost from body size); the expiry doubles as the statement's priority, so
/// the eviction benchmark can make new statements outrank the ones they evict.
fn create_signed_statement_sized(
	id: u64,
	topics: &[Topic],
	dec_key: Option<DecryptionKey>,
	data_size: usize,
	expiry: u64,
	keypair: &sp_core::ed25519::Pair,
) -> Statement {
	let mut statement = Statement::new();
	let mut data = vec![0u8; data_size];
	data[0..8].copy_from_slice(&id.to_le_bytes());
	statement.set_plain_data(data);

	for (i, topic) in topics.iter().enumerate() {
		statement.set_topic(i, *topic);
	}

	if let Some(key) = dec_key {
		statement.set_decryption_key(key);
	}

	// Expiry doubles as priority; callers pass a far-future value so the statement is accepted (the
	// default expiry of 0 is treated as already-expired and rejected).
	statement.set_expiry(expiry);
	statement.sign_ed25519_private(keypair);
	statement
}

fn setup_store(keypair: &sp_core::ed25519::Pair) -> (Store, tempfile::TempDir) {
	let temp_dir = tempfile::Builder::new().tempdir().expect("Error creating test dir");
	let client = Arc::new(TestClient::generous());
	let mut path: std::path::PathBuf = temp_dir.path().into();
	path.push("db");
	let keystore = Arc::new(sc_keystore::LocalKeystore::in_memory());
	let store = Store::new::<Block, TestClient, TestBackend>(
		&path,
		Default::default(),
		client,
		keystore,
		None,
		Box::new(sp_core::testing::TaskExecutor::new()),
	)
	.unwrap();

	for i in 0..INITIAL_STATEMENTS {
		let topics = if i % 10 == 0 { vec![topic(0), topic(1)] } else { vec![] };
		// Disjoint from the topic set, so `broadcasts` (no key) actually matches the topic-bearing
		// statements instead of being shadowed by a decryption key.
		let dec_key = if i % 10 == 5 { Some(dec_key(42)) } else { None };
		let statement = create_signed_statement(i as u64, &topics, dec_key, &keypair);
		store.submit(statement, StatementSource::Local);
	}

	(store, temp_dir)
}

/// Creates an empty store backed by a fresh temporary directory.
fn empty_store() -> (Store, tempfile::TempDir) {
	let temp_dir = tempfile::Builder::new().tempdir().expect("Error creating test dir");
	let client = Arc::new(TestClient::generous());
	let mut path: std::path::PathBuf = temp_dir.path().into();
	path.push("db");
	let keystore = Arc::new(sc_keystore::LocalKeystore::in_memory());
	let store = Store::new::<Block, TestClient, TestBackend>(
		&path,
		Default::default(),
		client,
		keystore,
		None,
		Box::new(sp_core::testing::TaskExecutor::new()),
	)
	.unwrap();
	(store, temp_dir)
}

/// Builds a store with `n` statements: every 10th carries the broadcast topics `[0, 1]` (no
/// decryption key), and a disjoint set carries decryption key 42. So `broadcasts(&[0, 1])` and
/// `posted(.., 42)` each match ~`n / 10` statements, growing with the store size.
fn setup_scaled(keypair: &sp_core::ed25519::Pair, n: usize) -> (Store, tempfile::TempDir) {
	let (store, temp) = empty_store();
	for i in 0..n {
		let topics: Vec<Topic> = if i % 10 == 0 { vec![topic(0), topic(1)] } else { vec![] };
		let key = if i % 10 == 5 { Some(dec_key(42)) } else { None };
		let statement = create_signed_statement(i as u64, &topics, key, keypair);
		assert!(matches!(store.submit(statement, StatementSource::Local), SubmitResult::New));
	}
	(store, temp)
}

fn bench_submit(c: &mut Criterion) {
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();
	let statements: Vec<_> = (INITIAL_STATEMENTS..INITIAL_STATEMENTS + TOTAL_OPS)
		.map(|i| create_signed_statement(i as u64, &[], None, &keypair))
		.collect();

	c.bench_function("submit", |b| {
		b.iter_batched(
			|| {
				let (store, _temp) = setup_store(&keypair);
				(Arc::new(store), _temp)
			},
			|(store, _temp)| {
				std::thread::scope(|s| {
					for thread_id in 0..NUM_THREADS {
						let store = store.clone();
						let start = thread_id * OPS_PER_THREAD;
						let end = start + OPS_PER_THREAD;
						let thread_statements = statements[start..end].to_vec();
						s.spawn(move || {
							for statement in thread_statements {
								let result = store.submit(statement, StatementSource::Local);
								assert!(matches!(result, SubmitResult::New));
							}
						});
					}
				});
			},
			criterion::BatchSize::LargeInput,
		)
	});
}

/// Marginal cost of the on-disk index writes on the submit hot path. Every submitted statement
/// writes one `INDEX_BY_DEC_KEY` entry plus one `INDEX_BY_TOPIC` entry per topic, all folded into
/// the same commit under the `submit_index` write lock. Varying the topic count moves the index
/// writes per submit from 1 (0 topics) to `MAX_TOPICS + 1`. The plain data is shrunk by one topic's
/// worth per topic so the encoded statement size stays ~constant across arms — otherwise a larger
/// body would confound the measurement — leaving the number of index writes as the only variable.
/// Signature verification runs off the lock, so under the 64-thread harness throughput is bound by
/// the serialized lock section: a flat curve means the index writes are lost in the noise, a rising
/// one means they are a real cost.
fn bench_submit_index_cost(c: &mut Criterion) {
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();

	let mut group = c.benchmark_group("submit_index_cost");
	group.sample_size(10);
	for &num_topics in &[0usize, 1, MAX_TOPICS] {
		let topics: Vec<Topic> = (0..num_topics).map(|t| topic(t as u64)).collect();
		// Hold the encoded size ~constant: a `Topic` is 32 bytes, so drop 32 data bytes per topic.
		let data_size = STATEMENT_DATA_SIZE.saturating_sub(num_topics * 32);
		let statements: Vec<Statement> = (INITIAL_STATEMENTS..INITIAL_STATEMENTS + TOTAL_OPS)
			.map(|i| {
				create_signed_statement_sized(
					i as u64,
					&topics,
					None,
					data_size,
					u64::MAX,
					&keypair,
				)
			})
			.collect();
		group.bench_with_input(
			BenchmarkId::from_parameter(num_topics),
			&statements,
			|b, statements| {
				b.iter_batched(
					|| {
						let (store, temp) = setup_store(&keypair);
						(Arc::new(store), temp)
					},
					|(store, _temp)| {
						std::thread::scope(|s| {
							for thread_id in 0..NUM_THREADS {
								let store = store.clone();
								let start = thread_id * OPS_PER_THREAD;
								let chunk = statements[start..start + OPS_PER_THREAD].to_vec();
								s.spawn(move || {
									for statement in chunk {
										let result =
											store.submit(statement, StatementSource::Local);
										assert!(matches!(result, SubmitResult::New));
									}
								});
							}
						});
					},
					criterion::BatchSize::LargeInput,
				)
			},
		);
	}
	group.finish();
}

/// A store whose per-account cap is `cap`, pre-filled with `cap` topicless statements at expiry
/// `expiry` so the account is exactly full. A later, higher-expiry statement then evicts one.
fn capped_store(
	keypair: &sp_core::ed25519::Pair,
	cap: u32,
	expiry: u64,
) -> (Store, tempfile::TempDir) {
	let temp_dir = tempfile::Builder::new().tempdir().expect("Error creating test dir");
	let client = Arc::new(TestClient::capped(cap));
	let mut path: std::path::PathBuf = temp_dir.path().into();
	path.push("db");
	let keystore = Arc::new(sc_keystore::LocalKeystore::in_memory());
	let store = Store::new::<Block, TestClient, TestBackend>(
		&path,
		Default::default(),
		client,
		keystore,
		None,
		Box::new(sp_core::testing::TaskExecutor::new()),
	)
	.unwrap();
	for i in 0..cap {
		let statement = create_signed_statement_sized(
			i as u64,
			&[],
			None,
			STATEMENT_DATA_SIZE,
			expiry,
			keypair,
		);
		assert!(matches!(store.submit(statement, StatementSource::Local), SubmitResult::New));
	}
	(store, temp_dir)
}

/// Submit cost when the account is full, so every submit evicts one statement. To delete an evicted
/// statement's index entries, `submit` reads its body back from `col::STATEMENTS` *under the
/// `submit_index` write lock* (`self.db.get`, to recover its topics). This measures whether that
/// synchronous read on the hot path is a bottleneck — compare against a build that moves the read
/// off the lock. Pre-loaded statements get a lower expiry (= priority) than the timed ones, so each
/// timed submit outranks and evicts one.
fn bench_submit_with_eviction(c: &mut Criterion) {
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();
	let low_expiry = u64::MAX / 2;
	let high_expiry = low_expiry + 1;
	let statements: Vec<Statement> = (0..TOTAL_OPS)
		.map(|i| {
			create_signed_statement_sized(
				EVICTION_CAP as u64 + i as u64,
				&[],
				None,
				STATEMENT_DATA_SIZE,
				high_expiry,
				&keypair,
			)
		})
		.collect();

	let mut group = c.benchmark_group("submit_eviction");
	group.sample_size(10);
	group.bench_function("evict_one", |b| {
		b.iter_batched(
			|| {
				let (store, temp) = capped_store(&keypair, EVICTION_CAP, low_expiry);
				(Arc::new(store), temp)
			},
			|(store, _temp)| {
				std::thread::scope(|s| {
					for thread_id in 0..NUM_THREADS {
						let store = store.clone();
						let start = thread_id * OPS_PER_THREAD;
						let chunk = statements[start..start + OPS_PER_THREAD].to_vec();
						s.spawn(move || {
							for statement in chunk {
								let result = store.submit(statement, StatementSource::Local);
								assert!(matches!(result, SubmitResult::New));
							}
						});
					}
				});
			},
			criterion::BatchSize::LargeInput,
		)
	});
	group.finish();
}

fn bench_remove(c: &mut Criterion) {
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();

	c.bench_function("remove", |b| {
		b.iter_batched(
			|| {
				let (store, _temp) = setup_store(&keypair);
				let hashes: Vec<_> = store
					.statements()
					.unwrap()
					.into_iter()
					.take(TOTAL_OPS)
					.map(|(hash, _)| hash)
					.collect();
				(Arc::new(store), hashes, _temp)
			},
			|(store, hashes, _temp)| {
				std::thread::scope(|s| {
					for thread_id in 0..NUM_THREADS {
						let store = store.clone();
						let start = thread_id * OPS_PER_THREAD;
						let end = start + OPS_PER_THREAD;
						let thread_hashes = hashes[start..end].to_vec();
						s.spawn(move || {
							for hash in thread_hashes {
								let _ = store.remove(&hash);
							}
						});
					}
				});
			},
			criterion::BatchSize::LargeInput,
		)
	});
}

fn bench_statement_lookup(c: &mut Criterion) {
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();

	c.bench_function("statement_lookup", |b| {
		b.iter_batched(
			|| {
				let (store, _temp) = setup_store(&keypair);
				let hashes: Vec<_> = store
					.statements()
					.unwrap()
					.into_iter()
					.take(TOTAL_OPS)
					.map(|(hash, _)| hash)
					.collect();
				(Arc::new(store), hashes, _temp)
			},
			|(store, hashes, _temp)| {
				std::thread::scope(|s| {
					for thread_id in 0..NUM_THREADS {
						let store = store.clone();
						let start = thread_id * OPS_PER_THREAD;
						let end = start + OPS_PER_THREAD;
						let thread_hashes = hashes[start..end].to_vec();
						s.spawn(move || {
							for hash in thread_hashes {
								let _ = store.statement(&hash);
							}
						});
					}
				});
			},
			criterion::BatchSize::LargeInput,
		)
	});
}

fn bench_statements_all(c: &mut Criterion) {
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();
	let (store, _temp) = setup_store(&keypair);
	let store = Arc::new(store);

	c.bench_function("statements_all", |b| {
		b.iter(|| {
			std::thread::scope(|s| {
				for _ in 0..NUM_THREADS {
					let store = store.clone();
					s.spawn(move || {
						for _ in 0..OPS_PER_THREAD {
							let _ = store.statements();
						}
					});
				}
			});
		})
	});
}

fn bench_broadcasts(c: &mut Criterion) {
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();
	let (store, _temp) = setup_store(&keypair);
	let store = Arc::new(store);
	let topics = vec![topic(0), topic(1)];

	c.bench_function("broadcasts", |b| {
		b.iter(|| {
			std::thread::scope(|s| {
				for _ in 0..NUM_THREADS {
					let store = store.clone();
					let topics = topics.clone();
					s.spawn(move || {
						for _ in 0..OPS_PER_THREAD {
							let _ = store.broadcasts(&topics);
						}
					});
				}
			});
		})
	});
}

fn bench_posted(c: &mut Criterion) {
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();
	let (store, _temp) = setup_store(&keypair);
	let store = Arc::new(store);
	let key = dec_key(42);

	c.bench_function("posted", |b| {
		b.iter(|| {
			std::thread::scope(|s| {
				for _ in 0..NUM_THREADS {
					let store = store.clone();
					s.spawn(move || {
						for _ in 0..OPS_PER_THREAD {
							let _ = store.posted(&[], key);
						}
					});
				}
			});
		})
	});
}

fn bench_maintain(c: &mut Criterion) {
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();

	c.bench_function("maintain", |b| {
		b.iter_batched(
			|| {
				let (store, _temp) = setup_store(&keypair);
				// Mark statements for expiration by removing them
				let hashes: Vec<_> = store
					.statements()
					.unwrap()
					.into_iter()
					.take(TOTAL_OPS)
					.map(|(hash, _)| hash)
					.collect();
				for hash in hashes {
					let _ = store.remove(&hash);
				}
				(store, _temp)
			},
			|(store, _temp)| {
				store.maintain();
			},
			criterion::BatchSize::LargeInput,
		)
	});
}

fn bench_mixed_workload(c: &mut Criterion) {
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();
	let statements: Vec<_> = (INITIAL_STATEMENTS..INITIAL_STATEMENTS + TOTAL_OPS)
		.map(|i| create_signed_statement(i as u64, &[topic(0), topic(1)], None, &keypair))
		.collect();

	c.bench_function("mixed_workload", |b| {
		b.iter_batched(
			|| {
				let (store, _temp) = setup_store(&keypair);
				(Arc::new(store), _temp)
			},
			|(store, _temp)| {
				std::thread::scope(|s| {
					for thread_id in 0..NUM_THREADS {
						let store = store.clone();
						let start = thread_id * OPS_PER_THREAD;
						let end = start + OPS_PER_THREAD;
						let thread_statements = statements[start..end].to_vec();
						let topics = vec![topic(0), topic(1)];
						s.spawn(move || {
							for statement in thread_statements {
								// Submit a statement
								let result = store.submit(statement, StatementSource::Local);
								assert!(matches!(result, SubmitResult::New));

								// Query broadcasts
								let _ = store.broadcasts(&topics);
							}
						});
					}
				});
			},
			criterion::BatchSize::LargeInput,
		)
	});
}

/// Read latency as a function of store size. In-memory indexes stay roughly flat; an on-disk index
/// grows with the data it has to scan, so this is the primary "cost of disk" axis.
fn bench_read_scaling(c: &mut Criterion) {
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();
	let topics = vec![topic(0), topic(1)];
	// A statement that is present at every size (id 0 carries topics [0, 1], no key).
	let known_hash = create_signed_statement(0, &topics, None, &keypair).hash();

	let mut group = c.benchmark_group("read_scaling");
	for &size in SCALING_SIZES {
		let (store, _temp) = setup_scaled(&keypair, size);
		// Full topic query (index scan + body fetch); its result set grows with the store.
		group.bench_with_input(BenchmarkId::new("broadcasts", size), &size, |b, _| {
			b.iter(|| store.broadcasts(&topics))
		});
		// Point existence check (pure index lookup, constant-size result).
		group.bench_with_input(BenchmarkId::new("has_statement", size), &size, |b, _| {
			b.iter(|| store.has_statement(&known_hash))
		});
	}
	group.finish();
}

/// Store of `n` statements, each carrying a single topic shared by `matches_per_topic` consecutive
/// statements (no decryption key). Builds a large, deep on-disk topic index while keeping any
/// single topic's match set small — the realistic "pull my few statements out of a full store"
/// shape.
fn setup_diverse_topics(
	keypair: &sp_core::ed25519::Pair,
	n: usize,
	matches_per_topic: usize,
) -> (Store, tempfile::TempDir) {
	let temp_dir = tempfile::Builder::new().tempdir().expect("Error creating test dir");
	let client = Arc::new(TestClient::generous());
	let mut path: std::path::PathBuf = temp_dir.path().into();
	path.push("db");
	let keystore = Arc::new(sc_keystore::LocalKeystore::in_memory());
	// Headroom above `n` so neither the global store cap nor the per-account allowance binds
	// mid-build (the default global cap is exactly ~4M statements / 2 GiB, which `n` can reach).
	let config = Config {
		max_total_statements: n.saturating_mul(2),
		max_total_size: n.saturating_mul(2 * 1024),
		..Default::default()
	};
	let store = Store::new::<Block, TestClient, TestBackend>(
		&path,
		config,
		client,
		keystore,
		None,
		Box::new(sp_core::testing::TaskExecutor::new()),
	)
	.unwrap();
	for i in 0..n {
		let t = topic((i / matches_per_topic) as u64);
		let statement = create_signed_statement(i as u64, &[t], None, keypair);
		assert!(matches!(store.submit(statement, StatementSource::Local), SubmitResult::New));
	}
	(store, temp_dir)
}

/// The primary retrieval user story: subscribe with a single topic and pull the matching statements
/// out of a near-limit store. `subscribe_statement` runs the snapshot scan (smallest index set +
/// membership probes) and reads the matching bodies, all against a full-size on-disk index.
fn bench_subscribe_topic(c: &mut Criterion) {
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();
	let (store, _temp) = setup_diverse_topics(&keypair, NEAR_LIMIT, SUBSCRIBE_MATCHES);
	let filter = OptimizedTopicFilter::MatchAll(std::collections::HashSet::from([topic(0)]));

	let mut group = c.benchmark_group("subscribe_topic");
	group.sample_size(10);
	group.bench_function(BenchmarkId::from_parameter(NEAR_LIMIT), |b| {
		// The returned stream unsubscribes on drop; we only measure the snapshot retrieval.
		b.iter(|| {
			let _ = store.subscribe_statement(filter.clone());
		})
	});
	group.finish();
}

/// Propagation gather: `take_recent_statements` drains the `recent` set and reads each body back —
/// what `do_propagate_statements` pulls each interval. Store size barely matters here (the
/// just-submitted bodies are hot in the cache), so a small store is used and the per-interval batch
/// size `PROPAGATE_BATCH` is the axis that matters.
fn bench_propagate(c: &mut Criterion) {
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();
	let statements: Vec<Statement> = (0..PROPAGATE_BATCH)
		.map(|i| create_signed_statement(i as u64, &[], None, &keypair))
		.collect();

	let mut group = c.benchmark_group("propagate");
	group.sample_size(10);
	group.bench_function(BenchmarkId::from_parameter(PROPAGATE_BATCH), |b| {
		b.iter_batched(
			|| {
				// Fresh store holding `PROPAGATE_BATCH` recently-submitted (undrained) statements.
				let (store, temp) = empty_store();
				for statement in &statements {
					assert!(matches!(
						store.submit(statement.clone(), StatementSource::Local),
						SubmitResult::New
					));
				}
				(store, temp)
			},
			|(store, _temp)| {
				let recent = store.take_recent_statements().unwrap();
				assert_eq!(recent.len(), PROPAGATE_BATCH);
			},
			criterion::BatchSize::LargeInput,
		)
	});
	group.finish();
}

/// Read latency while writers run concurrently. Measures how much the write/constraint path
/// interferes with reads — the contention that splitting the index (stage 1) and moving reads off
/// the write lock (stage 2a) is meant to reduce.
fn bench_contention(c: &mut Criterion) {
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();
	let topics = vec![topic(0), topic(1)];

	c.bench_function("contention_read_under_write", |b| {
		b.iter_batched(
			|| {
				let (store, temp) = setup_scaled(&keypair, CONTENTION_PRELOAD);
				let writes: Vec<Statement> = (CONTENTION_PRELOAD..
					CONTENTION_PRELOAD + CONTENTION_WRITERS * OPS_PER_THREAD)
					.map(|i| {
						create_signed_statement(i as u64, &[topic(0), topic(1)], None, &keypair)
					})
					.collect();
				(Arc::new(store), writes, temp)
			},
			|(store, writes, _temp)| {
				std::thread::scope(|s| {
					for w in 0..CONTENTION_WRITERS {
						let store = store.clone();
						let start = w * OPS_PER_THREAD;
						let chunk = writes[start..start + OPS_PER_THREAD].to_vec();
						s.spawn(move || {
							for statement in chunk {
								let _ = store.submit(statement, StatementSource::Local);
							}
						});
					}
					for _ in 0..CONTENTION_READERS {
						let store = store.clone();
						let topics = topics.clone();
						s.spawn(move || {
							for _ in 0..OPS_PER_THREAD {
								let _ = store.broadcasts(&topics);
							}
						});
					}
				});
			},
			criterion::BatchSize::LargeInput,
		)
	});
}

criterion_group!(
	benches,
	bench_submit,
	bench_submit_index_cost,
	bench_submit_with_eviction,
	bench_remove,
	bench_statement_lookup,
	bench_statements_all,
	bench_broadcasts,
	bench_posted,
	bench_maintain,
	bench_mixed_workload,
	bench_read_scaling,
	bench_subscribe_topic,
	bench_propagate,
	bench_contention
);
criterion_main!(benches);
