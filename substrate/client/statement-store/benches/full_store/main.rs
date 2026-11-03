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

//! Benchmarks against a full store (~4M statements, the default capacity).
//!
//! Unlike `benches/statement_store.rs`, the fixture is far too expensive to rebuild per
//! iteration, so it is built once at `$STMT_FIXTURE_DIR/db` (and reused if present — run
//! `examples/full_store_memory.rs` first to build it while measuring memory), and all benchmarks
//! run against it in phases:
//!
//! 1. reads on the shared store (point lookups, bounded / diverse / scaled queries, scans);
//! 2. writes through a dedicated sacrificial account (fresh high-range ids, so the fixture accounts
//!    are never perturbed);
//! 3. sacrificial-account cleanup (`remove_by`), restoring exact fixture counts;
//! 4. eviction: the store is reopened with a per-account allowance of exactly the fixture
//!    per-account count, so every submit by a fixture account evicts one statement;
//! 5. index load: timed open/close cycles over the 4M-statement DB (startup cost).
//!
//! Uses only the public `StatementStore` API, so the same file compiles and runs against any
//! revision of the crate.

mod common;

use common::*;
use criterion::{BatchSize, Criterion};
use sp_core::Pair as _;
use sp_statement_store::{OptimizedTopicFilter, StatementSource, SubmitResult};
use std::{
	collections::HashSet,
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc,
	},
	time::{Duration, Instant},
};

/// Expiry ladder for the eviction bench: every batch outranks both the fixture statements and all
/// previously submitted eviction statements, so victims never run out.
static EVICT_EXPIRY: AtomicU64 = AtomicU64::new(LOW_EXPIRY + 1);

fn sacrificial_batch(count: usize, topics_per: usize) -> Vec<sp_statement_store::Statement> {
	let keypair = sacrifice_keypair();
	let base = fresh_id_base();
	// Hold the encoded size ~constant across topic counts (a topic is 32 bytes).
	let data_size = STATEMENT_DATA_SIZE.saturating_sub(topics_per * 32);
	(0..count as u64)
		.map(|k| {
			let id = base + k;
			let topics: Vec<_> =
				(0..topics_per as u64).map(|t| topic(30_000_000_000 + id * 4 + t)).collect();
			create_statement(id, &topics, None, data_size, LOW_EXPIRY, &keypair)
		})
		.collect()
}

fn submit_concurrently(store: &Arc<Store>, statements: Vec<sp_statement_store::Statement>) {
	std::thread::scope(|s| {
		for t in 0..NUM_THREADS {
			let store = store.clone();
			let chunk = statements[t * OPS_PER_THREAD..(t + 1) * OPS_PER_THREAD].to_vec();
			s.spawn(move || {
				for statement in chunk {
					let result = store.submit(statement, StatementSource::Local);
					assert!(matches!(result, SubmitResult::New), "submit rejected: {:?}", result);
				}
			});
		}
	});
}

fn point_reads(c: &mut Criterion, store: &Arc<Store>) {
	let keypairs: Vec<_> = (0..ACCOUNTS).map(account_keypair).collect();
	let hit = fixture_statement(1, &keypairs).hash();
	let miss = [0xABu8; 32];
	assert!(store.has_statement(&hit), "fixture statement 1 must be present");
	assert!(!store.has_statement(&miss));

	let mut g = c.benchmark_group("full4m_point");
	g.sample_size(50);
	g.bench_function("has_statement_hit", |b| b.iter(|| store.has_statement(&hit)));
	g.bench_function("has_statement_miss", |b| b.iter(|| store.has_statement(&miss)));
	g.bench_function("statement_hit", |b| b.iter(|| store.statement(&hit)));
	g.finish();
}

fn query_reads(c: &mut Criterion, store: &Arc<Store>) {
	let t01 = t01();
	let dtopic = [diverse_topic()];
	let dkey = diverse_key();

	let mut g = c.benchmark_group("full4m_query");
	g.sample_size(30);
	g.bench_function("broadcasts_100", |b| b.iter(|| store.broadcasts(&t01)));
	g.bench_function("broadcasts_diverse_6", |b| b.iter(|| store.broadcasts(&dtopic)));
	g.bench_function("posted_100", |b| b.iter(|| store.posted(&[], dk42())));
	g.bench_function("posted_diverse_4", |b| b.iter(|| store.posted(&[], dkey)));
	g.finish();

	// 64 threads × 10 ops, the same shape as `broadcasts` / `posted` in the standard suite.
	let mut g = c.benchmark_group("full4m_query_concurrent");
	g.sample_size(10);
	g.bench_function("broadcasts_100_x640", |b| {
		b.iter(|| {
			std::thread::scope(|s| {
				for _ in 0..NUM_THREADS {
					let store = store.clone();
					let topics = t01.clone();
					s.spawn(move || {
						for _ in 0..OPS_PER_THREAD {
							let _ = store.broadcasts(&topics);
						}
					});
				}
			});
		})
	});
	g.bench_function("posted_100_x640", |b| {
		b.iter(|| {
			std::thread::scope(|s| {
				for _ in 0..NUM_THREADS {
					let store = store.clone();
					s.spawn(move || {
						for _ in 0..OPS_PER_THREAD {
							let _ = store.posted(&[], dk42());
						}
					});
				}
			});
		})
	});
	g.finish();
}

fn scan_reads(c: &mut Criterion, store: &Arc<Store>) {
	let t23 = t23();
	let filter = OptimizedTopicFilter::MatchAll(HashSet::from([diverse_topic()]));

	let mut g = c.benchmark_group("full4m_scan");
	g.sample_size(10);
	g.measurement_time(Duration::from_secs(30));
	g.bench_function("broadcasts_scaled_419k", |b| b.iter(|| store.broadcasts(&t23)));
	g.bench_function("subscribe_topic_diverse", |b| {
		// The returned stream unsubscribes on drop; this measures the snapshot retrieval.
		b.iter(|| {
			let _ = store.subscribe_statement(filter.clone());
		})
	});
	g.bench_function("statements_all_4m", |b| b.iter(|| store.statements()));
	g.finish();
}

fn write_benches(c: &mut Criterion, store: &Arc<Store>) {
	let mut g = c.benchmark_group("full4m_write");
	g.sample_size(10);

	g.bench_function("submit_640", |b| {
		b.iter_batched(
			|| sacrificial_batch(TOTAL_OPS, 0),
			|statements| submit_concurrently(store, statements),
			BatchSize::LargeInput,
		)
	});

	g.bench_function("submit_topics4_640", |b| {
		b.iter_batched(
			|| sacrificial_batch(TOTAL_OPS, 4),
			|statements| submit_concurrently(store, statements),
			BatchSize::LargeInput,
		)
	});

	g.bench_function("remove_640", |b| {
		b.iter_batched(
			|| {
				let statements = sacrificial_batch(TOTAL_OPS, 0);
				let hashes: Vec<_> = statements.iter().map(|s| s.hash()).collect();
				for statement in statements {
					let result = store.submit(statement, StatementSource::Local);
					assert!(matches!(result, SubmitResult::New));
				}
				hashes
			},
			|hashes| {
				std::thread::scope(|s| {
					for t in 0..NUM_THREADS {
						let store = store.clone();
						let chunk = hashes[t * OPS_PER_THREAD..(t + 1) * OPS_PER_THREAD].to_vec();
						s.spawn(move || {
							for hash in chunk {
								let _ = store.remove(&hash);
							}
						});
					}
				});
			},
			BatchSize::LargeInput,
		)
	});

	g.bench_function("maintain_after_640_removals", |b| {
		b.iter_batched(
			|| {
				let statements = sacrificial_batch(TOTAL_OPS, 0);
				let hashes: Vec<_> = statements.iter().map(|s| s.hash()).collect();
				for statement in statements {
					let result = store.submit(statement, StatementSource::Local);
					assert!(matches!(result, SubmitResult::New));
				}
				for hash in &hashes {
					let _ = store.remove(hash);
				}
			},
			|_| store.maintain(),
			BatchSize::LargeInput,
		)
	});

	g.bench_function("propagate_1000", |b| {
		b.iter_batched(
			|| {
				// Drain whatever previous benches left in `recent`, then stage exactly 1000.
				let _ = store.take_recent_statements();
				for statement in sacrificial_batch(1000, 0) {
					let result = store.submit(statement, StatementSource::Local);
					assert!(matches!(result, SubmitResult::New));
				}
			},
			|_| {
				let recent = store.take_recent_statements().unwrap();
				assert_eq!(recent.len(), 1000);
			},
			BatchSize::LargeInput,
		)
	});

	g.finish();
}

fn mixed_benches(c: &mut Criterion, store: &Arc<Store>) {
	let mut g = c.benchmark_group("full4m_mixed");
	g.sample_size(10);

	// Submissions carry the scaled topics (T2/T3, ~419k members) so relative drift is negligible;
	// queries read the bounded set (T0/T1, 100 members) so the read half stays stationary.
	g.bench_function("mixed_workload_640", |b| {
		b.iter_batched(
			|| {
				let keypair = sacrifice_keypair();
				let base = fresh_id_base();
				(0..TOTAL_OPS as u64)
					.map(|k| {
						create_statement(
							base + k,
							&t23(),
							None,
							STATEMENT_DATA_SIZE,
							LOW_EXPIRY,
							&keypair,
						)
					})
					.collect::<Vec<_>>()
			},
			|statements| {
				std::thread::scope(|s| {
					for t in 0..NUM_THREADS {
						let store = store.clone();
						let chunk =
							statements[t * OPS_PER_THREAD..(t + 1) * OPS_PER_THREAD].to_vec();
						let topics = t01();
						s.spawn(move || {
							for statement in chunk {
								let result = store.submit(statement, StatementSource::Local);
								assert!(matches!(result, SubmitResult::New));
								let _ = store.broadcasts(&topics);
							}
						});
					}
				});
			},
			BatchSize::LargeInput,
		)
	});

	g.bench_function("contention_32r_32w", |b| {
		b.iter_batched(
			|| {
				let keypair = sacrifice_keypair();
				let base = fresh_id_base();
				((NUM_THREADS / 2) * OPS_PER_THREAD..NUM_THREADS * OPS_PER_THREAD)
					.map(|k| {
						create_statement(
							base + k as u64,
							&t23(),
							None,
							STATEMENT_DATA_SIZE,
							LOW_EXPIRY,
							&keypair,
						)
					})
					.collect::<Vec<_>>()
			},
			|writes| {
				std::thread::scope(|s| {
					for w in 0..NUM_THREADS / 2 {
						let store = store.clone();
						let chunk = writes[w * OPS_PER_THREAD..(w + 1) * OPS_PER_THREAD].to_vec();
						s.spawn(move || {
							for statement in chunk {
								let _ = store.submit(statement, StatementSource::Local);
							}
						});
					}
					for _ in 0..NUM_THREADS / 2 {
						let store = store.clone();
						let topics = t01();
						s.spawn(move || {
							for _ in 0..OPS_PER_THREAD {
								let _ = store.broadcasts(&topics);
							}
						});
					}
				});
			},
			BatchSize::LargeInput,
		)
	});

	g.finish();
}

/// Prices `submit`'s access to per-account state when the submitting account rotates, one submit
/// per fixture account per iteration.
///
/// Channel-less rotating submits are the gossip-shaped workload (many authors interleaved): the
/// admission can be answered from lightweight per-account summaries. Channel-carrying submits
/// force the store to consult the account's full channel state — with the on-disk submit index
/// this is the cache-miss worst case, materialising a different fixture account's ~65k-row
/// record for every admission. The same channel-carrying submits from one hot account isolate
/// the channel logic itself from the rotation cost.
fn account_rotation_benches(c: &mut Criterion, store: &Arc<Store>) {
	let keypairs: Vec<_> = (0..ACCOUNTS).map(account_keypair).collect();
	// Rotating submits target fixture accounts, so they cannot be swept up by the sacrificial
	// `remove_by` cleanup; collect their hashes and remove them one by one instead.
	let added = std::sync::Mutex::new(Vec::new());

	let mut g = c.benchmark_group("full4m_account_rotation");
	g.sample_size(10);

	g.bench_function("submit_rotating_64", |b| {
		b.iter_batched(
			|| {
				let base = fresh_id_base();
				let batch: Vec<_> = (0..ACCOUNTS as u64)
					.map(|k| {
						create_statement(
							base + k,
							&[],
							None,
							STATEMENT_DATA_SIZE,
							LOW_EXPIRY,
							&keypairs[k as usize % ACCOUNTS],
						)
					})
					.collect();
				added.lock().unwrap().extend(batch.iter().map(|s| s.hash()));
				batch
			},
			|statements| {
				for statement in statements {
					let result = store.submit(statement, StatementSource::Local);
					assert!(matches!(result, SubmitResult::New), "submit rejected: {:?}", result);
				}
			},
			BatchSize::LargeInput,
		)
	});

	g.bench_function("submit_rotating_channel_64", |b| {
		b.iter_batched(
			|| {
				let base = fresh_id_base();
				let batch: Vec<_> = (0..ACCOUNTS as u64)
					.map(|k| {
						let id = base + k;
						create_channel_statement(
							id,
							channel(40_000_000_000 + id),
							STATEMENT_DATA_SIZE,
							LOW_EXPIRY,
							&keypairs[k as usize % ACCOUNTS],
						)
					})
					.collect();
				added.lock().unwrap().extend(batch.iter().map(|s| s.hash()));
				batch
			},
			|statements| {
				for statement in statements {
					let result = store.submit(statement, StatementSource::Local);
					assert!(matches!(result, SubmitResult::New), "submit rejected: {:?}", result);
				}
			},
			BatchSize::LargeInput,
		)
	});

	// Same channel-carrying submits, single hot account (cleaned up later by `remove_by`).
	g.bench_function("submit_channel_hot_64", |b| {
		let keypair = sacrifice_keypair();
		b.iter_batched(
			|| {
				let base = fresh_id_base();
				(0..ACCOUNTS as u64)
					.map(|k| {
						let id = base + k;
						create_channel_statement(
							id,
							channel(50_000_000_000 + id),
							STATEMENT_DATA_SIZE,
							LOW_EXPIRY,
							&keypair,
						)
					})
					.collect::<Vec<_>>()
			},
			|statements| {
				for statement in statements {
					let result = store.submit(statement, StatementSource::Local);
					assert!(matches!(result, SubmitResult::New), "submit rejected: {:?}", result);
				}
			},
			BatchSize::LargeInput,
		)
	});

	g.finish();

	// Put the fixture accounts back exactly as they were.
	let added = added.into_inner().expect("no panics while collecting; qed");
	let started = Instant::now();
	for hash in &added {
		store.remove(hash).expect("remove succeeds");
	}
	store.maintain();
	eprintln!(
		"rotation cleanup: {} statements removed in {:.1}s",
		added.len(),
		started.elapsed().as_secs_f64()
	);
}

/// Removes everything the sacrificial account submitted, restoring exact fixture counts for the
/// eviction phase and for later reruns against the same fixture.
fn cleanup_sacrifice(store: &Store) {
	let public = sacrifice_keypair().public();
	let who: [u8; 32] = AsRef::<[u8]>::as_ref(&public)
		.try_into()
		.expect("ed25519 public key is 32 bytes; qed");
	let started = Instant::now();
	store.remove_by(who).expect("remove_by succeeds");
	store.maintain();
	eprintln!("cleanup: sacrificial statements removed in {:.1}s", started.elapsed().as_secs_f64());
}

fn eviction_bench(c: &mut Criterion, dir: &std::path::Path) {
	let db = dir.join("db");
	let store = Arc::new(open_store_retry(&db, TestClient::capped(per_account() as u32)));
	let acc0 = account_keypair(0);

	let mut g = c.benchmark_group("full4m_evict");
	g.sample_size(10);
	g.warm_up_time(Duration::from_millis(500));
	g.bench_function("submit_evict_640", |b| {
		b.iter_batched(
			|| {
				let expiry = EVICT_EXPIRY.fetch_add(1, Ordering::Relaxed);
				let base = fresh_id_base();
				(0..TOTAL_OPS as u64)
					.map(|k| {
						create_statement(base + k, &[], None, STATEMENT_DATA_SIZE, expiry, &acc0)
					})
					.collect::<Vec<_>>()
			},
			|statements| submit_concurrently(&store, statements),
			BatchSize::LargeInput,
		)
	});
	g.finish();

	drop(Arc::try_unwrap(store).ok().expect("all eviction bench references dropped"));
}

fn index_load(dir: &std::path::Path) {
	let db = dir.join("db");
	for cycle in 0..3 {
		let started = Instant::now();
		let store = open_store_retry(&db, TestClient::generous());
		let secs = started.elapsed().as_secs_f64();
		println!("FULL4M_META index_load_cycle{}_secs={:.2}", cycle, secs);
		drop(store);
	}
}

fn main() {
	let dir = std::path::PathBuf::from(
		std::env::var("STMT_FIXTURE_DIR")
			.expect("STMT_FIXTURE_DIR must point at the fixture directory"),
	);
	let (store, built) = open_or_build_fixture(&dir);
	if let Some(secs) = built {
		println!("FULL4M_META build_secs={:.1}", secs);
	}
	println!("FULL4M_META db_size_bytes={}", db_size_bytes(&dir));

	// Drain `recent` so no benchmark pays for a backlog of up to 4M hashes.
	let drained = store.take_recent_statements().expect("take_recent_statements works").len();
	println!("FULL4M_META drained_recent={}", drained);

	// Result-set sizes; asserted only on a fresh build (eviction-bench reruns can erode a few
	// account-0 members of the bounded broadcast set).
	let b100 = store.broadcasts(&t01()).unwrap().len();
	let p100 = store.posted(&[], dk42()).unwrap().len();
	let d6 = store.broadcasts(&[diverse_topic()]).unwrap().len();
	let p4 = store.posted(&[], diverse_key()).unwrap().len();
	println!(
		"FULL4M_META broadcasts_100={} posted_100={} broadcasts_diverse={} posted_diverse={}",
		b100, p100, d6, p4
	);
	if built.is_some() {
		assert_eq!(b100, 100);
		assert_eq!(p100, 100);
		assert_eq!(d6, DIVERSE_TOPIC_MATCHES);
		assert_eq!(p4, DIVERSE_KEY_MATCHES);
	}

	let store = Arc::new(store);
	let mut c = Criterion::default().configure_from_args();

	point_reads(&mut c, &store);
	query_reads(&mut c, &store);
	scan_reads(&mut c, &store);
	write_benches(&mut c, &store);
	mixed_benches(&mut c, &store);
	account_rotation_benches(&mut c, &store);

	cleanup_sacrifice(&store);
	drop(Arc::try_unwrap(store).ok().expect("all bench references dropped"));

	eviction_bench(&mut c, &dir);
	index_load(&dir);

	c.final_summary();
}
