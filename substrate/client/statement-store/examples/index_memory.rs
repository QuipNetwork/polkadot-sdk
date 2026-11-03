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

//! Measures the memory footprint of the statement-store index — the *benefit* side of moving the
//! index to disk (the latency *cost* is in `benches/statement_store.rs`).
//!
//! It reports two numbers after loading `N` statements into a fresh store:
//! - **heap held by the index** — live bytes from a tracking global allocator, taken as the delta
//!   between an empty store and the loaded store. This isolates the in-RAM index structures and
//!   excludes the on-disk data (and the kernel page cache / mmapped DB pages);
//! - **process RSS** — resident set size from `/proc/self/statm`, a coarse figure that *includes*
//!   mmapped DB pages, allocator retention and everything else in the process.
//!
//! Comparing the two shows how accurate RSS is as a proxy for "memory the index costs".
//!
//! Uses only the public `StatementStore` API, so the same file runs against any revision
//! (stage 1 in-memory, stage 2a on-disk read index, stage 2b on-disk write index). Compare by
//! running it on each revision:
//!
//! ```text
//! for n in 10000 50000 100000; do cargo run --release --example index_memory -- "$n"; done
//! ```

use sc_statement_store::Store;
use sp_core::Pair;
use sp_runtime::codec::Encode;
use sp_statement_store::{
	DecryptionKey, Statement, StatementSource, StatementStore, SubmitResult, Topic,
};
use std::{
	alloc::{GlobalAlloc, Layout, System},
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	},
};

// --- Tracking global allocator: counts live (current) and peak heap bytes. -----------------------
//
// A tiny wrapper around the system allocator. Only `alloc`/`dealloc` are overridden; the default
// `realloc` is implemented in terms of them, so it is accounted for too.
// `staging-tracking-allocator` was considered, but it reports peak-between-checkpoints (it is built
// for PVF memory limiting) and would add a substrate -> polkadot dependency, whereas here we want
// the steady-state live bytes.

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
	unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
		let ptr = System.alloc(layout);
		if !ptr.is_null() {
			let current = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
			PEAK.fetch_max(current, Ordering::Relaxed);
		}
		ptr
	}

	unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
		System.dealloc(ptr, layout);
		CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
	}
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

/// Resident set size of this process, in bytes (Linux; assumes a 4 KiB page).
fn rss_bytes() -> usize {
	let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
	let resident_pages: usize =
		statm.split_whitespace().nth(1).and_then(|f| f.parse().ok()).unwrap_or(0);
	resident_pages * 4096
}

// --- Minimal client backing the store (mirrors benches/statement_store.rs).
// -----------------------

const STATEMENT_DATA_SIZE: usize = 256;
const CORRECT_BLOCK_HASH: [u8; 32] = [1u8; 32];

type Extrinsic = sp_runtime::OpaqueExtrinsic;
type Hash = sp_core::H256;
type Hashing = sp_runtime::traits::BlakeTwo256;
type BlockNumber = u64;
type Header = sp_runtime::generic::Header<BlockNumber, Hashing>;
type Block = sp_runtime::generic::Block<Header, Extrinsic>;
type TestBackend = sc_client_api::in_mem::Backend<Block>;

#[derive(Clone)]
struct TestClient;

impl sc_client_api::StorageProvider<Block, TestBackend> for TestClient {
	fn storage(
		&self,
		_hash: Hash,
		_key: &sc_client_api::StorageKey,
	) -> sp_blockchain::Result<Option<sc_client_api::StorageData>> {
		Ok(Some(sc_client_api::StorageData((2_000_000, 256 * 1024 * 1024).encode())))
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
	let mut statement = Statement::new();
	let mut data = vec![0u8; STATEMENT_DATA_SIZE];
	data[0..8].copy_from_slice(&id.to_le_bytes());
	statement.set_plain_data(data);
	for (i, topic) in topics.iter().enumerate() {
		statement.set_topic(i, *topic);
	}
	if let Some(key) = dec_key {
		statement.set_decryption_key(key);
	}
	statement.set_expiry(u64::MAX);
	statement.sign_ed25519_private(keypair);
	statement
}

fn empty_store() -> (Store, tempfile::TempDir) {
	let temp_dir = tempfile::Builder::new().tempdir().expect("Error creating test dir");
	let mut path: std::path::PathBuf = temp_dir.path().into();
	path.push("db");
	let store = Store::new::<Block, TestClient, TestBackend>(
		&path,
		Default::default(),
		Arc::new(TestClient),
		Arc::new(sc_keystore::LocalKeystore::in_memory()),
		None,
		Box::new(sp_core::testing::TaskExecutor::new()),
	)
	.unwrap();
	(store, temp_dir)
}

fn main() {
	let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);
	let keypair = sp_core::ed25519::Pair::from_string("//Bench", None).unwrap();

	// Baseline: an empty store plus the harness. The index heap is the growth from here.
	let (store, _temp) = empty_store();
	let baseline = CURRENT.load(Ordering::Relaxed);
	PEAK.store(baseline, Ordering::Relaxed);

	for i in 0..n {
		let topics: Vec<Topic> = if i % 10 == 0 { vec![topic(0), topic(1)] } else { vec![] };
		let key = if i % 10 == 5 { Some(dec_key(42)) } else { None };
		let statement = create_signed_statement(i as u64, &topics, key, &keypair);
		assert!(matches!(store.submit(statement, StatementSource::Local), SubmitResult::New));
	}

	let heap_immediate = CURRENT.load(Ordering::Relaxed);
	// Let parity-db's background worker flush its commit log to disk, so the on-disk index is not
	// counted as heap through the still-unflushed in-memory commit overlay.
	std::thread::sleep(std::time::Duration::from_secs(3));
	let heap_settled = CURRENT.load(Ordering::Relaxed);
	let peak = PEAK.load(Ordering::Relaxed);
	let rss = rss_bytes();
	// Keep the store (and its index) alive across the measurement.
	std::hint::black_box(&store);

	// CSV: n,heap_settled,heap_immediate,peak,rss (bytes; heap values are deltas from the
	// empty-store baseline, rss is absolute process resident size).
	println!(
		"{},{},{},{},{}",
		n,
		heap_settled.saturating_sub(baseline),
		heap_immediate.saturating_sub(baseline),
		peak.saturating_sub(baseline),
		rss
	);

	drop(store);
}
