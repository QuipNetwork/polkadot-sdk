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

//! Shared fixture for the full-store (~4M statements) benchmarks and the memory harness.
//!
//! Uses only the public `StatementStore` API, so the same file compiles against any revision of
//! the crate (stage 1 in-memory index, stage 2a on-disk read index, ...). Included as a module by
//! `benches/full_store/main.rs` and `examples/full_store_memory.rs`.
//!
//! ## Fixture shape (`N_STATEMENTS` = 4,194,304 = `DEFAULT_MAX_TOTAL_STATEMENTS`)
//!
//! Statement `i` (0-based) is signed by account `i % 64` (65,536 statements per account), carries
//! 256 bytes of plain data embedding `i`, expiry `LOW_EXPIRY`, and:
//! - `i < 1000 && i % 10 == 0` → topics `[T0, T1]`      (100 statements; the *bounded* topic set,
//!   size-compatible with the 1k-store `setup_store` used by the standard benches);
//! - `i < 1000 && i % 10 == 5` → decryption key `DK42`  (100 statements; bounded key set);
//! - `i % 10 == 3`             → topics `T2, T3`        (419,431 statements; the *scaled* ~10% set,
//!   completing the `read_scaling` 1k/10k/50k curve at 4M);
//! - `i >= 1000`               → one diverse topic `topic(1000 + i/8)` (~524k distinct topics, 8
//!   statements each: a deep, realistic topic index);
//! - `i >= 1000 && i % 4 == 0` → diverse key `dec_key(100_000 + i/16)` (~262k distinct keys, 4
//!   statements each; `i % 10 == 3` is odd so the scaled set stays key-less, and the `None`-key set
//!   ends up with ~3.15M members).
//!
//! Write benches use a dedicated 65th ("sacrificial") account with fresh high-range ids, so they
//! never perturb the fixture accounts; the eviction bench reopens the store with a per-account
//! allowance of exactly 65,536 so every submit by a fixture account must evict.

use sp_core::Pair;
use sp_runtime::codec::Encode;
use sp_statement_store::{DecryptionKey, Statement, StatementSource, SubmitResult, Topic};
use std::sync::{
	atomic::{AtomicUsize, Ordering},
	Arc,
};

pub use sc_statement_store::{Config, StatementStoreSubscriptionApi, Store};
pub use sp_statement_store::StatementStore;

pub type Extrinsic = sp_runtime::OpaqueExtrinsic;
pub type Hash = sp_core::H256;
pub type Hashing = sp_runtime::traits::BlakeTwo256;
pub type BlockNumber = u64;
pub type Header = sp_runtime::generic::Header<BlockNumber, Hashing>;
pub type Block = sp_runtime::generic::Block<Header, Extrinsic>;
pub type TestBackend = sc_client_api::in_mem::Backend<Block>;

pub const CORRECT_BLOCK_HASH: [u8; 32] = [1u8; 32];
pub const STATEMENT_DATA_SIZE: usize = 256;

/// Fixture size: `DEFAULT_MAX_TOTAL_STATEMENTS`, i.e. a store at its default capacity.
pub const N_STATEMENTS: usize = 4 * 1024 * 1024;
/// Fixture accounts (statement `i` belongs to account `i % ACCOUNTS`).
pub const ACCOUNTS: usize = 64;

/// Fixture size, overridable for smoke tests via `STMT_FIXTURE_N` (must be a multiple of 64 and
/// at least 4096; the default is used for real measurements).
pub fn n_statements() -> usize {
	let n = std::env::var("STMT_FIXTURE_N")
		.ok()
		.and_then(|s| s.parse().ok())
		.unwrap_or(N_STATEMENTS);
	assert!(
		n >= 4096 && n.is_multiple_of(ACCOUNTS),
		"STMT_FIXTURE_N must be a multiple of 64, >= 4096"
	);
	n
}

/// Statements per fixture account.
pub fn per_account() -> usize {
	n_statements() / ACCOUNTS
}
/// Expiry (= priority) of every fixture statement; far future, with headroom above it so the
/// eviction bench can submit strictly higher-priority statements.
pub const LOW_EXPIRY: u64 = u64::MAX / 2;
/// Threads used to populate the fixture.
pub const BUILD_THREADS: usize = 16;

/// Benchmark concurrency, matching `benches/statement_store.rs`.
pub const NUM_THREADS: usize = 64;
pub const OPS_PER_THREAD: usize = 10;
pub const TOTAL_OPS: usize = NUM_THREADS * OPS_PER_THREAD;

#[derive(Clone)]
pub struct TestClient {
	max_count: u32,
	max_size: u32,
}

impl TestClient {
	/// Effectively unlimited per-account allowance; the global `Config` caps the store instead.
	pub fn generous() -> Self {
		Self { max_count: u32::MAX, max_size: u32::MAX }
	}

	/// Per-account statement-count cap; once an account holds `max_count` statements, every
	/// further submit for it must evict one.
	pub fn capped(max_count: u32) -> Self {
		Self { max_count, max_size: u32::MAX }
	}
}

impl sc_client_api::StorageProvider<Block, TestBackend> for TestClient {
	fn storage(
		&self,
		_hash: Hash,
		_key: &sc_client_api::StorageKey,
	) -> sp_blockchain::Result<Option<sc_client_api::StorageData>> {
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

pub fn topic(data: u64) -> Topic {
	let mut bytes = [0u8; 32];
	bytes[0..8].copy_from_slice(&data.to_le_bytes());
	Topic::from(bytes)
}

pub fn dec_key(data: u64) -> DecryptionKey {
	let mut key: DecryptionKey = Default::default();
	key[0..8].copy_from_slice(&data.to_le_bytes());
	key
}

/// The bounded broadcast topics (100 carriers).
pub fn t01() -> Vec<Topic> {
	vec![topic(0), topic(1)]
}

/// The scaled broadcast topics (~419k carriers, ~10% of the store).
pub fn t23() -> Vec<Topic> {
	vec![topic(2), topic(3)]
}

/// The bounded decryption key (100 carriers).
pub fn dk42() -> DecryptionKey {
	dec_key(42)
}

/// A diverse-topic group id near the middle of the fixture that is interior, key-diluted (6 of
/// its 8 members are key-less) and contains no account-0 statement (`g % 8 == 1`, so member ids
/// are ≡ 8..15 mod 64 and the eviction bench can never erode it).
pub fn diverse_topic_group() -> u64 {
	let g = (n_statements() as u64 / 2) / 8;
	g - (g % 8) + 1
}
/// Expected `broadcasts` result size for [`diverse_topic_group`].
pub const DIVERSE_TOPIC_MATCHES: usize = 6;

/// A diverse-key group id with 4 members, none of them account 0 (`j % 4 == 1`, so member ids are
/// ≡ 16..28 mod 64).
pub fn diverse_key_group() -> u64 {
	let j = (n_statements() as u64 / 2) / 16;
	j - (j % 4) + 1
}
/// Expected `posted` result size for [`diverse_key_group`].
pub const DIVERSE_KEY_MATCHES: usize = 4;

pub fn diverse_topic() -> Topic {
	topic(1000 + diverse_topic_group())
}

pub fn diverse_key() -> DecryptionKey {
	dec_key(100_000 + diverse_key_group())
}

/// Global limits with headroom above the fixture size, so ordinary write benches never trigger
/// global eviction (the per-account allowance drives the eviction bench instead).
pub fn fixture_config() -> Config {
	Config {
		max_total_statements: 2 * N_STATEMENTS,
		max_total_size: 16 * 1024 * 1024 * 1024,
		..Default::default()
	}
}

pub fn account_keypair(idx: usize) -> sp_core::ed25519::Pair {
	sp_core::ed25519::Pair::from_string(&format!("//Bench//{}", idx), None).unwrap()
}

/// The dedicated account used by write benches; never part of the fixture.
pub fn sacrifice_keypair() -> sp_core::ed25519::Pair {
	sp_core::ed25519::Pair::from_string("//Bench//sacrifice", None).unwrap()
}

pub fn create_statement(
	id: u64,
	topics: &[Topic],
	key: Option<DecryptionKey>,
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
	if let Some(key) = key {
		statement.set_decryption_key(key);
	}
	statement.set_expiry(expiry);
	statement.sign_ed25519_private(keypair);
	statement
}

/// A 32-byte channel id embedding `data`.
pub fn channel(data: u64) -> sp_statement_store::Channel {
	let mut channel: sp_statement_store::Channel = Default::default();
	channel[0..8].copy_from_slice(&data.to_le_bytes());
	channel
}

/// Like [`create_statement`] (topic- and key-less), but carrying a channel: admitting such a
/// statement forces the store to consult the account's full channel state.
pub fn create_channel_statement(
	id: u64,
	chan: sp_statement_store::Channel,
	data_size: usize,
	expiry: u64,
	keypair: &sp_core::ed25519::Pair,
) -> Statement {
	let mut statement = Statement::new();
	let mut data = vec![0u8; data_size];
	data[0..8].copy_from_slice(&id.to_le_bytes());
	statement.set_plain_data(data);
	statement.set_channel(chan);
	statement.set_expiry(expiry);
	statement.sign_ed25519_private(keypair);
	statement
}

/// The deterministic fixture statement `i` (see the module docs for the shape).
pub fn fixture_statement(i: u64, keypairs: &[sp_core::ed25519::Pair]) -> Statement {
	let mut topics: Vec<Topic> = Vec::with_capacity(3);
	if i < 1000 && i.is_multiple_of(10) {
		topics.push(topic(0));
		topics.push(topic(1));
	}
	if i >= 1000 {
		topics.push(topic(1000 + i / 8));
	}
	if i % 10 == 3 {
		topics.push(topic(2));
		topics.push(topic(3));
	}
	let key = if i < 1000 && i % 10 == 5 {
		Some(dec_key(42))
	} else if i >= 1000 && i.is_multiple_of(4) {
		Some(dec_key(100_000 + i / 16))
	} else {
		None
	};
	create_statement(
		i,
		&topics,
		key,
		STATEMENT_DATA_SIZE,
		LOW_EXPIRY,
		&keypairs[(i as usize) % ACCOUNTS],
	)
}

/// A fresh id base strictly above the fixture id range and (in practice) unique per call.
pub fn fresh_id_base() -> u64 {
	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.expect("system clock after epoch; qed");
	(now.as_secs() << 30) | ((now.subsec_nanos() as u64) >> 2)
}

/// Opens a store on `path` (creating an empty one if the directory does not exist).
pub fn try_open_store(path: &std::path::Path, client: TestClient) -> Result<Store, String> {
	let keystore = Arc::new(sc_keystore::LocalKeystore::in_memory());
	Store::new::<Block, TestClient, TestBackend>(
		path,
		fixture_config(),
		Arc::new(client),
		keystore,
		None,
		Box::new(sp_core::testing::TaskExecutor::new()),
	)
	.map_err(|e| format!("{:?}", e))
}

/// Opens a store on `path` (creating an empty one if the directory does not exist).
pub fn open_store(path: &std::path::Path, client: TestClient) -> Store {
	try_open_store(path, client).expect("statement store opens")
}

/// Opens a store on `path`, retrying for up to 30s: right after a previous instance is dropped,
/// its background workers may still hold the DB lock for a moment.
pub fn open_store_retry(path: &std::path::Path, client: TestClient) -> Store {
	let started = std::time::Instant::now();
	loop {
		match try_open_store(path, client.clone()) {
			Ok(store) => return store,
			Err(e) if started.elapsed() < std::time::Duration::from_secs(30) => {
				eprintln!("open_store_retry: {}; retrying", e);
				std::thread::sleep(std::time::Duration::from_millis(500));
			},
			Err(e) => panic!("statement store failed to open within 30s: {}", e),
		}
	}
}

/// Populates an already-open, empty store with the full fixture using [`BUILD_THREADS`] workers.
/// Returns the wall-clock build time in seconds.
pub fn populate_fixture(store: &Store) -> f64 {
	let n = n_statements();
	let keypairs: Vec<_> = (0..ACCOUNTS).map(account_keypair).collect();
	let started = std::time::Instant::now();
	let progress = AtomicUsize::new(0);
	std::thread::scope(|s| {
		let chunk = n.div_ceil(BUILD_THREADS);
		for t in 0..BUILD_THREADS {
			let keypairs = keypairs.clone();
			let progress = &progress;
			let start = t * chunk;
			let end = ((t + 1) * chunk).min(n);
			s.spawn(move || {
				for i in start..end {
					let statement = fixture_statement(i as u64, &keypairs);
					let result = store.submit(statement, StatementSource::Local);
					assert!(
						matches!(result, SubmitResult::New),
						"fixture statement {} rejected: {:?}",
						i,
						result
					);
					let done = progress.fetch_add(1, Ordering::Relaxed) + 1;
					if done.is_multiple_of(524_288) {
						eprintln!(
							"fixture: {}/{} statements ({:.0}s)",
							done,
							n,
							started.elapsed().as_secs_f64()
						);
					}
				}
			});
		}
	});
	started.elapsed().as_secs_f64()
}

/// Opens the fixture store at `dir/db`, building and populating it first if absent.
/// Returns the store and, when it was built, the population wall time in seconds.
pub fn open_or_build_fixture(dir: &std::path::Path) -> (Store, Option<f64>) {
	let db_path = dir.join("db");
	let existed = db_path.exists();
	std::fs::create_dir_all(dir).expect("fixture dir is creatable");
	let store = open_store(&db_path, TestClient::generous());
	if existed {
		eprintln!("fixture: reusing existing DB at {}", db_path.display());
		(store, None)
	} else {
		eprintln!("fixture: building {} statements at {}", n_statements(), db_path.display());
		let secs = populate_fixture(&store);
		eprintln!("fixture: built in {:.1}s", secs);
		(store, Some(secs))
	}
}

/// Total on-disk size of the fixture DB in bytes.
pub fn db_size_bytes(dir: &std::path::Path) -> u64 {
	fn walk(path: &std::path::Path) -> u64 {
		let mut total = 0;
		if let Ok(entries) = std::fs::read_dir(path) {
			for entry in entries.flatten() {
				let p = entry.path();
				if p.is_dir() {
					total += walk(&p);
				} else if let Ok(meta) = entry.metadata() {
					total += meta.len();
				}
			}
		}
		total
	}
	walk(&dir.join("db"))
}
