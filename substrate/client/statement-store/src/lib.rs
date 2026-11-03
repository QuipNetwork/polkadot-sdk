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

#![doc = include_str!("../docs/overview.md")]
#![doc = include_str!("../docs/usage.md")]
//! # Implementation notes
//!
//! This crate contains a disk-backed implementation of `sp_statement_store::StatementStore`.
//!
//! ## Constraint management
//!
//! The statement store validates statements using node-side signature verification and
//! static runtime allowance limits.
//! The following constraints are then checked:
//! * For a given account id, there may be at most `max_count` statements with `max_size` total data
//!   size. To satisfy this, statements for this account ID are removed from the store starting with
//!   the lowest priority until a constraint is satisfied.
//! * There may not be more than `MAX_TOTAL_STATEMENTS` total statements with `MAX_TOTAL_SIZE` size.
//!   To satisfy this, statements are removed from the store starting with the lowest
//!   `global_priority` until a constraint is satisfied.
//!
//! When a new statement is inserted that would not satisfy constraints in the first place, no
//! statements are deleted and a `Rejected` result is returned.
//! The order in which statements with the same priority are deleted is unspecified.
//!
//! ## Statement expiration
//!
//! Each time a statement is removed from the store (Either evicted by higher priority statement or
//! explicitly with the `remove` function) the statement is marked as expired. Expired statements
//! can't be added to the store for `Options::purge_after_sec` seconds. This is to prevent old
//! statements from being propagated on the network.

#![warn(missing_docs)]
#![warn(unused_extern_crates)]

mod metrics;
mod subscription;

#[cfg(feature = "test-helpers")]
pub mod subxt_client;
#[cfg(feature = "test-helpers")]
pub mod test_utils;

use crate::subscription::{SubscriptionStatementsStream, SubscriptionsHandle};
use futures::FutureExt;
use metrics::MetricsLink as PrometheusMetrics;
use parking_lot::RwLock;
use prometheus_endpoint::Registry as PrometheusRegistry;
use sc_client_api::{backend::StorageProvider, Backend, StorageKey};
use sc_keystore::LocalKeystore;
use schnellru::{ByLength, LruMap};
use sp_blockchain::HeaderBackend;
use sp_core::{crypto::UncheckedFrom, hexdisplay::HexDisplay, traits::SpawnNamed, Decode, Encode};
use sp_runtime::traits::Block as BlockT;
use sp_statement_store::{
	runtime_api::{StatementSource, StatementStoreExt},
	AccountId, AdmittedBatch, BlockHash, Channel, DecryptionKey, FilterDecision, Hash,
	InvalidReason, OptimizedTopicFilter, RejectionReason, Result, SignatureVerificationResult,
	Statement, StatementAllowance, StatementEvent, SubmitResult, Topic,
};
pub use sp_statement_store::{Error, StatementStore, MAX_TOPICS};
use std::{
	collections::{BTreeMap, HashMap, HashSet},
	sync::{
		atomic::{AtomicUsize, Ordering as AtomicOrdering},
		Arc, Weak,
	},
	time::{Duration, Instant},
};
pub use subscription::{
	AddFilterError, MultiFilterEventStream, MultiFilterSubscriptionApi,
	MultiFilterSubscriptionEvent, StatementStoreSubscriptionApi, SubscriptionHandle,
	MAX_FILTERS_PER_SUBSCRIPTION,
};
use subscription::{ReplayBatch, ReplaySnapshotProvider, REPLAY_CHUNK_RAW_BYTES};

const KEY_VERSION: &[u8] = b"version".as_slice();
const CURRENT_VERSION: u32 = 2;

/// Meta column key of the persisted global counters, SCALE-encoded as
/// `(statement_count: u64, total_size: u64, next_seq: u64)`. The row is rewritten as part of
/// every mutating commit (all of which happen under the submit-index write lock), so it is always
/// consistent with the statement data and makes startup independent of the store size.
const KEY_COUNTERS: &[u8] = b"counters".as_slice();

const MIGRATION_COMMIT_CHUNK: usize = 100_000;

/// A single raw database operation: writes `value` under `key` in `column`, or deletes the key
/// when `value` is `None`.
struct DbOperation {
	column: u8,
	key: Vec<u8>,
	value: Option<Vec<u8>>,
}

impl From<(u8, Vec<u8>, Option<Vec<u8>>)> for DbOperation {
	fn from((column, key, value): (u8, Vec<u8>, Option<Vec<u8>>)) -> Self {
		Self { column, key, value }
	}
}

struct MigrationBatch<'a> {
	db: &'a parity_db::Db,
	operations: Vec<DbOperation>,
	committed: usize,
}

impl<'a> MigrationBatch<'a> {
	fn new(db: &'a parity_db::Db) -> Self {
		Self { db, operations: Vec::new(), committed: 0 }
	}

	fn push(&mut self, operation: DbOperation) -> Result<()> {
		self.operations.push(operation);
		self.flush_if_full()
	}

	fn extend(&mut self, operations: impl IntoIterator<Item = DbOperation>) -> Result<()> {
		self.operations.extend(operations);
		self.flush_if_full()
	}

	fn flush_if_full(&mut self) -> Result<()> {
		if self.operations.len() >= MIGRATION_COMMIT_CHUNK {
			self.flush()?;
		}
		Ok(())
	}

	fn flush(&mut self) -> Result<()> {
		if self.operations.is_empty() {
			return Ok(());
		}
		let operations = std::mem::take(&mut self.operations);
		let count = operations.len();
		self.db
			.commit(
				operations
					.into_iter()
					.map(|operation| (operation.column, operation.key, operation.value)),
			)
			.map_err(|error| Error::Db(error.to_string()))?;
		self.committed += count;
		Ok(())
	}

	fn finish(mut self) -> Result<usize> {
		self.flush()?;
		Ok(self.committed)
	}
}

const LOG_TARGET: &str = "statement-store";

/// The amount of time an expired statement is kept before it is removed from the store entirely.
pub const DEFAULT_PURGE_AFTER_SEC: u64 = 2 * 24 * 60 * 60; // 48h
/// The maximum number of statements the statement store can hold.
pub const DEFAULT_MAX_TOTAL_STATEMENTS: usize = 4 * 1024 * 1024; // ~4 million
/// The maximum amount of data the statement store can hold, regardless of the number of
/// statements from which the data originates.
pub const DEFAULT_MAX_TOTAL_SIZE: usize = 2 * 1024 * 1024 * 1024; // 2GiB
/// The maximum size of a single statement in bytes.
/// Accounts for the 1-byte vector length prefix when statements are gossiped as `Vec<Statement>`.
pub const MAX_STATEMENT_SIZE: usize =
	sc_network_statement::config::MAX_STATEMENT_NOTIFICATION_SIZE as usize - 1;

/// Maximum number of statements to expire in a single iteration.
const MAX_EXPIRY_STATEMENTS_PER_ITERATION: usize = 10_000;
/// Maximum number of accounts to check for expiry in a single iteration.
const MAX_EXPIRY_ACCOUNTS_PER_ITERATION: usize = 10_000;
/// Maximum time in milliseconds to spend checking for expiry in a single iteration.
const MAX_EXPIRY_TIME_PER_ITERATION: Duration = Duration::from_millis(100);

/// Number of subscription filter worker tasks.
const NUM_FILTER_WORKERS: usize = 1;

const MAINTENANCE_PERIOD: std::time::Duration = std::time::Duration::from_secs(29);

/// Specifies which block hash to use when reading statement allowances.
enum AllowanceBlock {
	/// Use the best (latest) block hash.
	Best,
	/// Use the finalized block hash.
	Finalized,
}

// Period between enforcing limits (checking for expired statements and making sure statements stay
// within allowances). Different from maintenance period to avoid keeping the lock for too long for
// maintenance tasks.
const ENFORCE_LIMITS_PERIOD: std::time::Duration = std::time::Duration::from_secs(31);

mod col {
	pub const META: u8 = 0;
	pub const STATEMENTS: u8 = 1;
	pub const EXPIRED: u8 = 2;
	pub const INDEX_BY_TOPIC: u8 = 3;
	pub const INDEX_BY_DEC_KEY: u8 = 4;
	pub const INDEX_EVICTED: u8 = 5;
	pub const ADMISSION_SEQ: u8 = 6;
	pub const INDEX_BY_ACCOUNT: u8 = 7;
	pub const INDEX_BY_EXPIRY: u8 = 8;

	pub const COUNT: u8 = 9;
}

/// Btree-indexed columns (ordered keys with prefix scans), as opposed to the hash columns.
const BTREE_COLUMNS: [u8; 6] = [
	col::INDEX_BY_TOPIC,
	col::INDEX_BY_DEC_KEY,
	col::INDEX_EVICTED,
	col::ADMISSION_SEQ,
	col::INDEX_BY_ACCOUNT,
	col::INDEX_BY_EXPIRY,
];

/// Budget of the per-account details cache, measured in cached statements across all account
/// records (a record costs at least one unit). Bounds cache memory regardless of how statements
/// are distributed over accounts.
#[cfg(not(test))]
const DETAILS_CACHE_BUDGET: usize = 65_536;
#[cfg(test)]
const DETAILS_CACHE_BUDGET: usize = 8;

/// Maximum number of per-account count/size summaries kept in memory.
#[cfg(not(test))]
const SUMMARY_CACHE_ACCOUNTS: u32 = 131_072;
#[cfg(test)]
const SUMMARY_CACHE_ACCOUNTS: u32 = 4;

const INDEX_EMPTY_VALUE: &[u8] = &[];
const DEC_KEY_TAG_NONE: u8 = 0;
const DEC_KEY_TAG_SOME: u8 = 1;

fn topic_index_key(topic: &Topic, hash: &Hash) -> Vec<u8> {
	let mut key = Vec::with_capacity(topic.len() + hash.len());
	key.extend_from_slice(&topic[..]);
	key.extend_from_slice(&hash[..]);
	key
}

fn dec_key_index_prefix(dec_key: &Option<DecryptionKey>) -> Vec<u8> {
	match dec_key {
		None => vec![DEC_KEY_TAG_NONE],
		Some(dec_key) => {
			let mut prefix = Vec::with_capacity(1 + dec_key.len());
			prefix.push(DEC_KEY_TAG_SOME);
			prefix.extend_from_slice(&dec_key[..]);
			prefix
		},
	}
}

fn dec_key_index_key(dec_key: &Option<DecryptionKey>, hash: &Hash) -> Vec<u8> {
	let mut key = dec_key_index_prefix(dec_key);
	key.extend_from_slice(&hash[..]);
	key
}

/// Reverses [`dec_key_index_prefix`]; `None` if `prefix` does not have that layout.
fn parse_dec_key_index_prefix(prefix: &[u8]) -> Option<Option<DecryptionKey>> {
	match prefix.split_first() {
		Some((&DEC_KEY_TAG_NONE, [])) => Some(None),
		Some((&DEC_KEY_TAG_SOME, dec_key)) if dec_key.len() == size_of::<DecryptionKey>() => {
			dec_key.try_into().ok().map(Some)
		},
		_ => {
			log::error!(
				target: LOG_TARGET,
				"Corrupt decryption-key index prefix: {:?}",
				HexDisplay::from(&prefix)
			);
			None
		},
	}
}

fn evicted_index_key(purge_at: u64, hash: &Hash) -> Vec<u8> {
	let mut key = Vec::with_capacity(8 + hash.len());
	key.extend_from_slice(&purge_at.to_be_bytes());
	key.extend_from_slice(&hash[..]);
	key
}

/// Key of a statement's row in [`col::INDEX_BY_ACCOUNT`]
fn account_index_key(account: &AccountId, expiry: Expiry, hash: &Hash) -> Vec<u8> {
	let mut key = Vec::with_capacity(account.len() + size_of::<u64>() + hash.len());
	key.extend_from_slice(&account[..]);
	key.extend_from_slice(&expiry.0.to_be_bytes());
	key.extend_from_slice(&hash[..]);
	key
}

/// Reverses [`account_index_key`]; `None` if `key` does not have that layout.
fn parse_account_index_key(key: &[u8]) -> Option<(AccountId, Expiry, Hash)> {
	if key.len() != size_of::<AccountId>() + size_of::<u64>() + size_of::<Hash>() {
		log::error!(
			target: LOG_TARGET,
			"Corrupt account index key: {:?}",
			HexDisplay::from(&key)
		);
		return None;
	}
	let (account, rest) = key.split_at(size_of::<AccountId>());
	let (expiry, hash) = rest.split_at(size_of::<u64>());
	Some((
		account.try_into().ok()?,
		Expiry(u64::from_be_bytes(expiry.try_into().ok()?)),
		hash.try_into().ok()?,
	))
}

/// Key of a statement's row in [`col::INDEX_BY_EXPIRY`]: the whole column iterates in expiry
/// order, so the statements due for expiry form a prefix of it.
fn expiry_index_key(expiry: Expiry, hash: &Hash) -> Vec<u8> {
	let mut key = Vec::with_capacity(size_of::<u64>() + hash.len());
	key.extend_from_slice(&expiry.0.to_be_bytes());
	key.extend_from_slice(&hash[..]);
	key
}

/// Reverses the shared `u64_be ‖ hash` layout of [`expiry_index_key`] and [`evicted_index_key`];
/// `None` if `key` does not have that layout.
fn parse_time_index_key(key: &[u8]) -> Option<(u64, Hash)> {
	if key.len() != size_of::<u64>() + size_of::<Hash>() {
		log::error!(target: LOG_TARGET, "Corrupt time index key: {:?}", HexDisplay::from(&key));
		return None;
	}
	let (time, hash) = key.split_at(size_of::<u64>());
	Some((u64::from_be_bytes(time.try_into().ok()?), hash.try_into().ok()?))
}

/// Per-statement details tracked inside a per-account record; also the (SCALE-encoded) value of
/// the statement's [`col::INDEX_BY_ACCOUNT`] row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct EntryDetails {
	channel: Option<Channel>,
	data_len: usize,
	admission_seq: u64,
}

impl Encode for EntryDetails {
	fn size_hint(&self) -> usize {
		self.channel.size_hint() +
			(self.data_len as u32).size_hint() +
			self.admission_seq.size_hint()
	}

	fn encode_to<T: codec::Output + ?Sized>(&self, dest: &mut T) {
		self.channel.encode_to(dest);
		(self.data_len as u32).encode_to(dest);
		self.admission_seq.encode_to(dest);
	}
}

impl Decode for EntryDetails {
	fn decode<I: codec::Input>(input: &mut I) -> std::result::Result<Self, codec::Error> {
		let (channel, data_len, admission_seq) = <(Option<Channel>, u32, u64)>::decode(input)?;
		Ok(EntryDetails { channel, data_len: data_len as usize, admission_seq })
	}
}

/// The two rows tying a statement into the per-account and expiry indexes; written on insert
/// (`details` carries the account row's value) and deleted with `None`.
fn account_index_ops(
	account: &AccountId,
	expiry: Expiry,
	hash: &Hash,
	details: Option<&EntryDetails>,
) -> [(u8, Vec<u8>, Option<Vec<u8>>); 2] {
	[
		(
			col::INDEX_BY_ACCOUNT,
			account_index_key(account, expiry, hash),
			details.map(EntryDetails::encode),
		),
		(
			col::INDEX_BY_EXPIRY,
			expiry_index_key(expiry, hash),
			details.map(|_| INDEX_EMPTY_VALUE.to_vec()),
		),
	]
}

/// The account id immediately after `account` in the index ordering, or `None` when `account`
/// is the maximum id.
fn next_account_id(mut account: AccountId) -> Option<AccountId> {
	for byte in account.iter_mut().rev() {
		match byte.checked_add(1) {
			Some(incremented) => {
				*byte = incremented;
				return Some(account);
			},
			None => *byte = 0,
		}
	}
	None
}

/// Extracts the trailing hash from a composite index key, if it is long enough.
fn hash_from_index_key(key: &[u8]) -> Option<Hash> {
	let Some(prefix_len) = key.len().checked_sub(size_of::<Hash>()) else {
		log::error!(target: LOG_TARGET, "Corrupt index key: {:?}", HexDisplay::from(&key));
		return None;
	};
	key[prefix_len..].try_into().ok()
}

/// The prefix of a composite index key — everything before the trailing hash — if it is long
/// enough.
fn prefix_from_index_key(key: &[u8]) -> Option<&[u8]> {
	let Some(prefix_len) = key.len().checked_sub(size_of::<Hash>()) else {
		log::error!(target: LOG_TARGET, "Corrupt index key: {:?}", HexDisplay::from(&key));
		return None;
	};
	Some(&key[..prefix_len])
}

/// Builds the index-column operations for a statement's topic and decryption-key entries. With
/// `insert == true` the entries are written; otherwise they are deleted. Designed to be folded
/// into the same atomic [`parity_db::Db::commit`] as the statement body.
fn statement_index_ops(
	hash: &Hash,
	statement: &Statement,
	insert: bool,
) -> Vec<(u8, Vec<u8>, Option<Vec<u8>>)> {
	let value = insert.then(|| INDEX_EMPTY_VALUE.to_vec());
	let mut ops = Vec::new();
	let mut nt = 0;
	while let Some(topic) = statement.topic(nt) {
		ops.push((col::INDEX_BY_TOPIC, topic_index_key(&topic, hash), value.clone()));
		nt += 1;
	}
	let dec_key = statement.decryption_key();
	ops.push((col::INDEX_BY_DEC_KEY, dec_key_index_key(&dec_key, hash), value));
	ops
}

#[derive(Eq, PartialEq, Debug, Ord, PartialOrd, Clone, Copy)]
struct Expiry(u64);

impl Expiry {
	/// Returns the expiration timestamp in seconds
	fn get_expiration_timestamp_secs(self) -> u64 {
		self.0 >> 32
	}
}

#[derive(PartialEq, Eq, Clone, Copy)]
struct PriorityKey {
	hash: Hash,
	expiry: Expiry,
}

impl PartialOrd for PriorityKey {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl Ord for PriorityKey {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.expiry.cmp(&other.expiry).then_with(|| self.hash.cmp(&other.hash))
	}
}

#[derive(PartialEq, Eq)]
struct ChannelEntry {
	hash: Hash,
	expiry: Expiry,
}

#[derive(Default)]
struct StatementsForAccount {
	// Statements ordered by priority.
	by_priority: BTreeMap<PriorityKey, EntryDetails>,
	// Channel to statement map. Only one statement per channel is allowed.
	channels: HashMap<Channel, ChannelEntry>,
	// Sum of all `Data` field sizes.
	data_size: usize,
}

impl StatementsForAccount {
	fn insert_entry(&mut self, hash: Hash, expiry: Expiry, details: EntryDetails) {
		self.data_size += details.data_len;
		if let Some(channel) = details.channel {
			self.channels.insert(channel, ChannelEntry { hash, expiry });
		}
		self.by_priority.insert(PriorityKey { hash, expiry }, details);
	}

	fn remove_entry(&mut self, key: &PriorityKey) -> Option<EntryDetails> {
		let details = self.by_priority.remove(key)?;
		self.data_size -= details.data_len;
		if let Some(channel) = details.channel {
			self.channels.remove(&channel);
		}
		Some(details)
	}
}

/// Default number of concurrent workers for statement validation.
pub const DEFAULT_NETWORK_WORKERS: usize = 1;

/// Default maximum statements per second per peer before rate limiting kicks in.
pub use sc_network_statement::config::DEFAULT_STATEMENTS_PER_SECOND as DEFAULT_RATE_LIMIT;

/// Statement store and network handler configuration.
#[derive(Debug, Clone, Copy)]
pub struct Config {
	/// Maximum statements allowed in the store. Once this limit is reached lower-priority
	/// statements may be evicted.
	pub max_total_statements: usize,
	/// Maximum total data size allowed in the store. Once this limit is reached lower-priority
	/// statements may be evicted.
	pub max_total_size: usize,
	/// Number of seconds for which removed statements won't be allowed to be added back in.
	pub purge_after_sec: u64,
	/// Number of concurrent workers for statement validation from the network.
	pub network_workers: usize,
	/// Maximum statements per second per peer before rate limiting kicks in.
	pub rate_limit: u32,
}

impl Config {
	/// Validate the configuration, returning an error if any values are invalid.
	pub fn validate(&self) -> Result<()> {
		if self.max_total_statements == 0 {
			return Err(Error::InvalidConfig(
				"max_total_statements must be greater than zero".into(),
			));
		}
		if self.max_total_size == 0 {
			return Err(Error::InvalidConfig("max_total_size must be greater than zero".into()));
		}
		if self.network_workers == 0 {
			return Err(Error::InvalidConfig("network_workers must be greater than zero".into()));
		}
		Ok(())
	}
}

impl Default for Config {
	fn default() -> Self {
		Config {
			max_total_statements: DEFAULT_MAX_TOTAL_STATEMENTS,
			max_total_size: DEFAULT_MAX_TOTAL_SIZE,
			purge_after_sec: DEFAULT_PURGE_AFTER_SEC,
			network_workers: DEFAULT_NETWORK_WORKERS,
			rate_limit: DEFAULT_RATE_LIMIT,
		}
	}
}

/// In-memory part of the read index.
struct QueryIndex {
	// TODO: Remove counters; replace them with a merge-join/leapfrog1
	topic_counts: HashMap<Topic, usize>,
	dec_key_counts: HashMap<Option<DecryptionKey>, usize>,
	recent: HashMap<Hash, u64>,
}

impl QueryIndex {
	fn new() -> Self {
		QueryIndex {
			topic_counts: HashMap::new(),
			dec_key_counts: HashMap::new(),
			recent: HashMap::new(),
		}
	}

	/// Records a newly inserted statement: bumps cardinalities and marks the hash as recent
	/// under its admission sequence number.
	fn note_insert(&mut self, hash: Hash, statement: &Statement, seq: u64) {
		let mut nt = 0;
		while let Some(topic) = statement.topic(nt) {
			*self.topic_counts.entry(topic).or_insert(0) += 1;
			nt += 1;
		}
		let dec_key = statement.decryption_key();
		*self.dec_key_counts.entry(dec_key).or_insert(0) += 1;
		self.recent.insert(hash, seq);
	}

	/// Records a removed statement: decrements cardinalities and drops the hash from `recent`.
	fn note_remove(&mut self, hash: &Hash, statement: &Statement) {
		let mut nt = 0;
		while let Some(topic) = statement.topic(nt) {
			if let Some(count) = self.topic_counts.get_mut(&topic) {
				*count = count.saturating_sub(1);
				if *count == 0 {
					self.topic_counts.remove(&topic);
				}
			}
			nt += 1;
		}
		let dec_key = statement.decryption_key();
		if let Some(count) = self.dec_key_counts.get_mut(&dec_key) {
			*count = count.saturating_sub(1);
			if *count == 0 {
				self.dec_key_counts.remove(&dec_key);
			}
		}
		self.recent.remove(hash);
	}

	/// Takes and clears the set of recently added hashes with their admission sequence numbers.
	fn take_recent(&mut self) -> HashMap<Hash, u64> {
		std::mem::take(&mut self.recent)
	}
}

/// Statement count and total data size of one account.
#[derive(Clone, Copy)]
struct AccountSummary {
	count: usize,
	data_size: usize,
}

/// In-memory part of the submit index (constraint checking, quota enforcement).
///
/// The authoritative per-statement data lives on disk in [`col::INDEX_BY_ACCOUNT`] /
/// [`col::INDEX_BY_EXPIRY`]; this structure holds bounded write-through caches over it plus the
/// global counters.
struct SubmitIndex {
	/// Cached per-account detail records. Bounded by [`Self::cached_statement_count`]
	/// against [`DETAILS_CACHE_BUDGET`].
	account_statements: LruMap<AccountId, StatementsForAccount>,
	/// Total number of statements across all cached records in [`Self::account_statements`].
	cached_statement_count: usize,
	/// Cached per-account count/size summaries. Cheap enough to cover many more accounts than
	/// the details cache; lets `submit` skip loading the full record for channel-less statements
	/// well within their quota, and lets `enforce_limits` skip accounts within their allowance.
	summaries: LruMap<AccountId, AccountSummary>,
	/// Resume point of the incremental per-account allowance sweep in `enforce_limits`: the
	/// account id to continue the scan from, or `None` when a new pass starts from the beginning.
	allowance_cursor: Option<AccountId>,
	/// Number of accounts already seen by the current allowance sweep pass.
	allowance_cycle_seen: usize,
	/// Store configuration (global limits, purge period).
	config: Config,
	/// Number of stored statements.
	statement_count: usize,
	/// Running total of data size across all stored statements.
	total_size: usize,
	evicted_count: usize,
	// Monotonic sequence number assigned to each statement as it is inserted.
	next_seq: u64,
	// Sequence numbers of recently inserted statements, kept only while at least one subscription
	// snapshot scan is in progress and only back to the oldest such scan's watermark.
	recent_seqs: HashMap<Hash, u64>,
	// Watermarks of the currently running snapshot scans.
	active_scan_floors: BTreeMap<u64, usize>,
}

struct ClientWrapper<Block, Client, BE> {
	client: Arc<Client>,
	_block: std::marker::PhantomData<Block>,
	_backend: std::marker::PhantomData<BE>,
}

impl<Block, Client, BE> ClientWrapper<Block, Client, BE>
where
	Block: BlockT,
	Block::Hash: From<BlockHash>,
	BE: Backend<Block> + 'static,
	Client: HeaderBackend<Block> + StorageProvider<Block, BE> + Send + Sync + 'static,
{
	fn read_allowance(
		&self,
		account_id: &AccountId,
		allowance_block: AllowanceBlock,
	) -> Result<Option<StatementAllowance>> {
		use sp_statement_store::{statement_allowance_key, StatementAllowance};

		let block_hash = match allowance_block {
			AllowanceBlock::Best => self.client.info().best_hash,
			AllowanceBlock::Finalized => self.client.info().finalized_hash,
		};
		let key = statement_allowance_key(account_id);
		let storage_key = StorageKey(key);
		self.client
			.storage(block_hash, &storage_key)
			.map_err(|e| Error::Storage(format!("Failed to read allowance: {:?}", e)))?
			.map(|value| {
				StatementAllowance::decode(&mut &value.0[..])
					.map_err(|e| Error::Decode(format!("Failed to decode allowance: {:?}", e)))
			})
			.transpose()
	}
}

/// Statement store.
pub struct Store {
	db: parity_db::Db,
	submit_index: RwLock<SubmitIndex>,
	query_index: RwLock<QueryIndex>,
	read_allowance_fn:
		Box<dyn Fn(&AccountId, AllowanceBlock) -> Result<Option<StatementAllowance>> + Send + Sync>,
	subscription_manager: SubscriptionsHandle,
	keystore: Arc<LocalKeystore>,
	/// Number of accounts with stored statements. Reported by `maintain`; may lag the
	/// exact value by up to one sweep cycle.
	known_accounts_count: AtomicUsize,
	// Used for testing
	time_override: Option<u64>,
	metrics: PrometheusMetrics,
}

impl ReplaySnapshotProvider for Weak<Store> {
	fn register_replay(&self, enqueue: &mut dyn FnMut(u64) -> bool) -> Result<Option<u64>> {
		let Some(store) = self.upgrade() else {
			return Err(Error::InvalidConfig("statement store is closed".into()));
		};
		store.register_replay(enqueue)
	}

	fn replay_batch(
		&self,
		filter: &OptimizedTopicFilter,
		cursor: u64,
		watermark: u64,
	) -> Result<ReplayBatch> {
		let Some(store) = self.upgrade() else {
			return Err(Error::InvalidConfig("statement store is closed".into()));
		};
		store.replay_batch(filter, cursor, watermark)
	}
}

/// What admitting a new statement will claim and evict, as decided by
/// [`SubmitIndex::plan_insert`].
struct InsertPlan {
	/// Admission sequence the new statement will claim.
	seq: u64,
	/// The account's own statements that must be evicted to make room. Their bodies and on-disk
	/// index rows must be deleted.
	evicted: Vec<(PriorityKey, EntryDetails)>,
	/// The subset of `evicted` that is banned from re-acceptance, with its purge deadline. These
	/// must be recorded in the on-disk evicted journal.
	banned: Vec<(Hash, u64)>,
}

/// A single on-disk index set referenced during a query: either the set of hashes carrying a
/// topic, or the set of hashes for a decryption key.
#[derive(Clone)]
enum IndexSet {
	Topic(Topic),
	DecKey(Option<DecryptionKey>),
}

impl IndexSet {
	fn column(&self) -> u8 {
		match self {
			IndexSet::Topic(_) => col::INDEX_BY_TOPIC,
			IndexSet::DecKey(_) => col::INDEX_BY_DEC_KEY,
		}
	}

	/// Prefix selecting every entry of this set within its column.
	fn prefix(&self) -> Vec<u8> {
		match self {
			IndexSet::Topic(t) => t[..].to_vec(),
			IndexSet::DecKey(k) => dec_key_index_prefix(k),
		}
	}

	/// Full key of `hash` within this set's column, for point membership lookups.
	fn member_key(&self, hash: &Hash) -> Vec<u8> {
		match self {
			IndexSet::Topic(t) => topic_index_key(t, hash),
			IndexSet::DecKey(k) => dec_key_index_key(k, hash),
		}
	}

	/// Cardinality of this set, read from the in-memory cardinality counters.
	fn len(&self, read_index: &QueryIndex) -> usize {
		match self {
			IndexSet::Topic(t) => read_index.topic_counts.get(t).copied().unwrap_or(0),
			IndexSet::DecKey(k) => read_index.dec_key_counts.get(k).copied().unwrap_or(0),
		}
	}
}

impl SubmitIndex {
	fn new(config: Config) -> SubmitIndex {
		SubmitIndex {
			// The real bound is [`Self::cached_statement_count`], enforced by
			// [`Self::cache_record`]; this length limiter must never trigger on its own, and
			// cannot: every record costs at least one unit, so the cost trimming keeps the
			// length at or below the budget.
			account_statements: LruMap::new(ByLength::new(u32::MAX)),
			summaries: LruMap::new(ByLength::new(SUMMARY_CACHE_ACCOUNTS)),
			config,
			statement_count: 0,
			total_size: 0,
			next_seq: 0,
			cached_statement_count: 0,
			allowance_cursor: None,
			allowance_cycle_seen: 0,
			evicted_count: 0,
			recent_seqs: HashMap::new(),
			active_scan_floors: BTreeMap::new(),
		}
	}

	/// The [`KEY_COUNTERS`] row reflecting the given post-commit totals. Folded into every
	/// mutating commit.
	fn counters_op(
		statement_count: usize,
		total_size: usize,
		next_seq: u64,
	) -> (u8, Vec<u8>, Option<Vec<u8>>) {
		(
			col::META,
			KEY_COUNTERS.to_vec(),
			Some((statement_count as u64, total_size as u64, next_seq).encode()),
		)
	}

	/// Removes the account's record from the details cache, keeping the cost accounting exact.
	fn uncache_record(&mut self, account: &AccountId) -> Option<StatementsForAccount> {
		let record = self.account_statements.remove(account)?;
		self.cached_statement_count -= record.by_priority.len();
		Some(record)
	}

	/// Puts a record (back) into the details cache, then evicts the least-recently-used records
	/// while the total number of cached statements exceeds the budget. A single record larger
	/// than the whole budget is kept: the cache never trims below one entry.
	fn cache_record(&mut self, account: AccountId, record: StatementsForAccount) {
		if record.by_priority.is_empty() {
			return;
		}
		self.cached_statement_count += record.by_priority.len();
		self.account_statements.insert(account, record);
		while self.cached_statement_count > DETAILS_CACHE_BUDGET &&
			self.account_statements.len() > 1
		{
			match self.account_statements.pop_oldest() {
				Some((_, evicted)) => self.cached_statement_count -= evicted.by_priority.len(),
				None => break,
			}
		}
	}

	/// Puts a record into the details cache together with the summary derived from it. An
	/// account with no statements is represented in both caches by absence, so an empty record
	/// refreshes neither.
	fn cache_record_with_summary(&mut self, account: AccountId, record: StatementsForAccount) {
		if !record.by_priority.is_empty() {
			self.summaries.insert(
				account,
				AccountSummary { count: record.by_priority.len(), data_size: record.data_size },
			);
		}
		self.cache_record(account, record);
	}

	/// Records a sequence number in the snapshot window, once its statement and admission entries
	/// have committed atomically. The number itself is claimed earlier, by [`Self::insert`].
	fn note_seq(&mut self, hash: Hash, seq: u64) {
		if !self.active_scan_floors.is_empty() {
			self.recent_seqs.insert(hash, seq);
		}
	}

	/// Registers a subscription snapshot scan and returns its watermark.
	fn begin_scan(&mut self) -> u64 {
		let watermark = self.next_seq;
		*self.active_scan_floors.entry(watermark).or_insert(0) += 1;
		watermark
	}

	/// Deregisters a snapshot scan previously registered with [`Self::begin_scan`] and prunes
	/// `recent_seqs` down to the smallest still-active watermark.
	fn end_scan(&mut self, watermark: u64) {
		if let Some(count) = self.active_scan_floors.get_mut(&watermark) {
			*count -= 1;
			if *count == 0 {
				self.active_scan_floors.remove(&watermark);
			}
		}
		match self.active_scan_floors.keys().next() {
			Some(&floor) => self.recent_seqs.retain(|_, seq| *seq >= floor),
			None => self.recent_seqs.clear(),
		}
	}

	/// Whether the statement `hash` belongs in the snapshot of a scan with the given `watermark`.
	fn seq_covered_by_snapshot(&self, hash: &Hash, watermark: u64) -> bool {
		match self.recent_seqs.get(hash) {
			Some(&seq) => seq < watermark,
			None => true,
		}
	}

	/// Pure constraint check for admitting `statement`: decides, without mutating anything, which
	/// of the account's own statements must be evicted to make room, or why the statement cannot
	/// be admitted. `record` is the account's current state; global limits are checked against
	/// the in-memory counters. The store never evicts other accounts' statements to admit a new
	/// one — when global limits cannot be met from this account alone, the statement is rejected.
	fn plan_insert(
		&self,
		record: &StatementsForAccount,
		hash: Hash,
		statement: &Statement,
		account: &AccountId,
		validation: &StatementAllowance,
		current_time: u64,
	) -> std::result::Result<InsertPlan, RejectionReason> {
		let statement_len = statement.data_len();
		if statement_len > validation.max_size as usize {
			log::debug!(
				target: LOG_TARGET,
				"Ignored oversize message from account {}: {:?} ({} bytes)",
				HexDisplay::from(account),
				HexDisplay::from(&hash),
				statement_len,
			);
			return Err(RejectionReason::DataTooLarge {
				submitted_size: statement_len,
				available_size: validation.max_size as usize,
			});
		}

		let mut evicted: Vec<(PriorityKey, EntryDetails)> = Vec::new();
		let mut evicted_hashes = HashSet::new();
		let mut would_free_size = 0;
		let expiry = Expiry(statement.expiry());
		let (max_size, max_count) = (validation.max_size as usize, validation.max_count as usize);
		// It may happen that we can't delete enough lower priority messages
		// to satisfy size constraints. We check for that before deleting anything,
		// taking into account channel message replacement.
		if let Some(channel) = statement.channel() {
			if let Some(channel_record) = record.channels.get(&channel) {
				if expiry <= channel_record.expiry {
					// Trying to replace channel message with lower expiry.
					log::debug!(
						target: LOG_TARGET,
						"Ignored lower priority channel message from account {}: {:?} {:?} <= {:?}",
						HexDisplay::from(account),
						HexDisplay::from(&hash),
						expiry,
						channel_record.expiry,
					);
					return Err(RejectionReason::ChannelPriorityTooLow {
						submitted_expiry: expiry.0,
						min_expiry: channel_record.expiry.0,
					});
				} else {
					// Would replace channel message. Still need to check for size constraints
					// below.
					log::debug!(
						target: LOG_TARGET,
						"Replacing higher priority channel message from account {}: {:?} ({:?}) > {:?} ({:?})",
						HexDisplay::from(account),
						HexDisplay::from(&hash),
						expiry,
						HexDisplay::from(&channel_record.hash),
						channel_record.expiry,
					);
					let key =
						PriorityKey { hash: channel_record.hash, expiry: channel_record.expiry };
					if let Some(details) = record.by_priority.get(&key) {
						would_free_size += details.data_len;
						evicted_hashes.insert(key.hash);
						evicted.push((key, *details));
					}
				}
			}
		}
		// Check if we can evict enough lower priority statements to satisfy constraints
		for (entry, details) in record.by_priority.iter() {
			if (record.data_size - would_free_size + statement_len <= max_size) &&
				record.by_priority.len() + 1 - evicted_hashes.len() <= max_count
			{
				// Satisfied
				break;
			}
			if evicted_hashes.contains(&entry.hash) {
				// Already accounted for above
				continue;
			}
			if entry.expiry >= expiry {
				log::debug!(
					target: LOG_TARGET,
					"Ignored message from account {} due to constraints {:?} {:?} < {:?}",
					HexDisplay::from(account),
					HexDisplay::from(&hash),
					expiry,
					entry.expiry,
				);
				let retained_size = record.data_size - would_free_size;
				if retained_size + statement_len > max_size {
					return Err(RejectionReason::DataTooLarge {
						submitted_size: statement_len,
						available_size: max_size.saturating_sub(retained_size),
					});
				}
				return Err(RejectionReason::AccountFull {
					submitted_expiry: expiry.0,
					min_expiry: entry.expiry.0,
				});
			}
			evicted_hashes.insert(entry.hash);
			would_free_size += details.data_len;
			evicted.push((*entry, *details));
		}
		// Now check global constraints as well.
		if !((self.total_size - would_free_size + statement_len <= self.config.max_total_size) &&
			self.statement_count + 1 - evicted.len() <= self.config.max_total_statements)
		{
			log::debug!(
				target: LOG_TARGET,
				"Ignored statement {} from account {} because the store is full (size={}, count={})",
				HexDisplay::from(&hash),
				HexDisplay::from(account),
				self.total_size,
				self.statement_count,
			);
			return Err(RejectionReason::StoreFull);
		}

		let banned = evicted
			.iter()
			.filter_map(|(key, _)| {
				let expiry_ts = key.expiry.get_expiration_timestamp_secs();
				(current_time < expiry_ts).then(|| {
					(
						key.hash,
						expiry_ts.min(current_time.saturating_add(self.config.purge_after_sec)),
					)
				})
			})
			.collect();
		Ok(InsertPlan { seq: self.next_seq, evicted, banned })
	}

	/// Applies a committed insertion to the caches and counters. `loaded_record` carries the
	/// account's record when planning had to load it from disk; with `None` the cached copy (if
	/// any) is updated in place, which also covers the summary fast path where no record was
	/// materialised at all.
	fn apply_insert(
		&mut self,
		account: &AccountId,
		loaded_record: Option<StatementsForAccount>,
		hash: Hash,
		statement: &Statement,
		plan: &InsertPlan,
	) {
		let statement_len = statement.data_len();
		let freed: usize = plan.evicted.iter().map(|(_, details)| details.data_len).sum();
		self.statement_count = self.statement_count + 1 - plan.evicted.len();
		self.total_size = self.total_size + statement_len - freed;
		self.evicted_count += plan.banned.len();
		self.next_seq = plan.seq.saturating_add(1);
		self.note_seq(hash, plan.seq);
		for (key, _) in &plan.evicted {
			log::trace!(target: LOG_TARGET, "Expired statement {:?}", HexDisplay::from(&key.hash));
		}
		let details = EntryDetails {
			channel: statement.channel(),
			data_len: statement_len,
			admission_seq: plan.seq,
		};
		match loaded_record.or_else(|| self.uncache_record(account)) {
			Some(mut record) => {
				for (key, _) in &plan.evicted {
					record.remove_entry(key);
				}
				record.insert_entry(hash, Expiry(statement.expiry()), details);
				self.cache_record_with_summary(*account, record);
			},
			None => {
				// Summary fast path: no record was materialised, only the summary is maintained.
				assert!(plan.evicted.is_empty());
				if let Some(summary) = self.summaries.get(account) {
					summary.count += 1;
					summary.data_size += statement_len;
				}
			},
		}
	}

	/// Applies the committed removal of one statement to the caches and counters.
	fn apply_removal(
		&mut self,
		account: &AccountId,
		key: &PriorityKey,
		data_len: usize,
		banned: bool,
	) {
		self.statement_count = self.statement_count.saturating_sub(1);
		self.total_size = self.total_size.saturating_sub(data_len);
		if banned {
			self.evicted_count += 1;
		}
		if let Some(record) = self.account_statements.peek_mut(account) {
			if record.remove_entry(key).is_some() {
				self.cached_statement_count -= 1;
			}
			if record.by_priority.is_empty() {
				self.account_statements.remove(account);
			}
		}
		if let Some(summary) = self.summaries.peek_mut(account) {
			summary.count = summary.count.saturating_sub(1);
			summary.data_size = summary.data_size.saturating_sub(data_len);
			if summary.count == 0 {
				self.summaries.remove(account);
			}
		}
	}

	/// Applies the committed removal of an account's every statement to the caches and counters.
	fn apply_account_removal(
		&mut self,
		account: &AccountId,
		removed_count: usize,
		freed_size: usize,
		banned_count: usize,
	) {
		self.statement_count = self.statement_count.saturating_sub(removed_count);
		self.total_size = self.total_size.saturating_sub(freed_size);
		self.evicted_count += banned_count;
		if let Some(record) = self.account_statements.remove(account) {
			self.cached_statement_count -= record.by_priority.len();
		}
		self.summaries.remove(account);
	}
}

impl Store {
	/// Create a new shared store instance. There should only be one per process.
	/// `path` will be used to open a statement database or create a new one if it does not exist.
	pub fn new_shared<Block, Client, BE>(
		path: &std::path::Path,
		config: Config,
		client: Arc<Client>,
		keystore: Arc<LocalKeystore>,
		prometheus: Option<&PrometheusRegistry>,
		task_spawner: Box<dyn SpawnNamed>,
	) -> Result<Arc<Store>>
	where
		Block: BlockT,
		Block::Hash: From<BlockHash>,
		BE: Backend<Block> + 'static,
		Client: HeaderBackend<Block> + StorageProvider<Block, BE> + Send + Sync + 'static,
	{
		let store =
			Arc::new(Self::new(path, config, client, keystore, prometheus, task_spawner.clone())?);

		// Perform periodic statement store maintenance
		let worker_store = store.clone();
		task_spawner.spawn(
			"statement-store-maintenance",
			Some("statement-store"),
			Box::pin(async move {
				let mut maintenance_interval = tokio::time::interval(MAINTENANCE_PERIOD);
				let mut enforce_limits_interval = tokio::time::interval(ENFORCE_LIMITS_PERIOD);
				loop {
					futures::select! {
						_ = maintenance_interval.tick().fuse() => {worker_store.maintain();}
						_ = enforce_limits_interval.tick().fuse() => {worker_store.enforce_limits();}
					}
				}
			}),
		);

		Ok(store)
	}

	/// Create a new instance.
	/// `path` will be used to open a statement database or create a new one if it does not exist.
	#[doc(hidden)]
	pub fn new<Block, Client, BE>(
		path: &std::path::Path,
		config: Config,
		client: Arc<Client>,
		keystore: Arc<LocalKeystore>,
		prometheus: Option<&PrometheusRegistry>,
		task_spawner: Box<dyn SpawnNamed>,
	) -> Result<Store>
	where
		Block: BlockT,
		Block::Hash: From<BlockHash>,
		BE: Backend<Block> + 'static,
		Client: HeaderBackend<Block> + StorageProvider<Block, BE> + Send + Sync + 'static,
	{
		config.validate()?;

		let mut path: std::path::PathBuf = path.into();
		path.push("statements");

		Self::migrate_columns(&path)?;
		let db = Self::open_db(&path)?;
		let needs_index_migration = Self::check_db_version(&db)?;

		let storage_reader =
			ClientWrapper { client, _block: Default::default(), _backend: Default::default() };
		let read_allowance_fn =
			Box::new(move |account_id: &AccountId, allowance_block: AllowanceBlock| {
				storage_reader.read_allowance(account_id, allowance_block)
			});

		let store = Store {
			db,
			submit_index: RwLock::new(SubmitIndex::new(config)),
			query_index: RwLock::new(QueryIndex::new()),
			read_allowance_fn,
			keystore,
			known_accounts_count: AtomicUsize::new(0),
			time_override: None,
			metrics: PrometheusMetrics::new(prometheus),
			subscription_manager: SubscriptionsHandle::new(
				task_spawner.clone(),
				NUM_FILTER_WORKERS,
			),
		};
		store.populate(needs_index_migration)?;
		Ok(store)
	}

	/// Migrate the column layout of an existing database to the current schema.
	fn migrate_columns(path: &std::path::Path) -> Result<()> {
		let Some(metadata) =
			parity_db::Options::load_metadata(path).map_err(|e| Error::Db(e.to_string()))?
		else {
			return Ok(());
		};
		if metadata.columns.len() >= col::COUNT as usize {
			return Ok(());
		}
		let mut migrate_config = parity_db::Options::with_columns(path, 0);
		migrate_config.columns = metadata.columns;
		while migrate_config.columns.len() < col::COUNT as usize {
			// `add_column` takes the options by value, so build a fresh one each iteration.
			let mut new_column_options = parity_db::ColumnOptions::default();
			let column = migrate_config.columns.len() as u8;
			new_column_options.btree_index = BTREE_COLUMNS.contains(&column);
			parity_db::Db::add_column(&mut migrate_config, new_column_options)
				.map_err(|e| Error::Db(e.to_string()))?;
		}
		Ok(())
	}

	/// Open (or create) the statement database with the column options expected by the current
	/// schema.
	fn open_db(path: &std::path::Path) -> Result<parity_db::Db> {
		let mut db_config = parity_db::Options::with_columns(path, col::COUNT);
		let statement_col = &mut db_config.columns[col::STATEMENTS as usize];
		statement_col.ref_counted = false;
		statement_col.preimage = true;
		statement_col.uniform = true;
		for c in BTREE_COLUMNS {
			db_config.columns[c as usize].btree_index = true;
		}
		parity_db::Db::open_or_create(&db_config).map_err(|e| Error::Db(e.to_string()))
	}

	/// Read the on-disk database version and reconcile it with [`CURRENT_VERSION`].
	///
	/// A brand new database has its version initialised and needs no migration. An existing
	/// database from a newer version is rejected. Returns `true` if the on-disk indexes predate
	/// the current version and therefore need to be rebuilt.
	fn check_db_version(db: &parity_db::Db) -> Result<bool> {
		match db.get(col::META, &KEY_VERSION).map_err(|e| Error::Db(e.to_string()))? {
			Some(version) => {
				let version = u32::from_le_bytes(
					version
						.try_into()
						.map_err(|_| Error::Db("Error reading database version".into()))?,
				);
				if version > CURRENT_VERSION {
					return Err(Error::Db(format!("Unsupported database version: {version}")));
				}
				Ok(version < CURRENT_VERSION)
			},
			None => {
				// Brand new database: the index columns start empty, nothing to migrate.
				db.commit([(
					col::META,
					KEY_VERSION.to_vec(),
					Some(CURRENT_VERSION.to_le_bytes().to_vec()),
				)])
				.map_err(|e| Error::Db(e.to_string()))?;
				Ok(false)
			},
		}
	}

	/// Restore the in-memory state from the database at startup. Statement bodies are never
	/// decoded here, so startup cost does not depend on the size of the stored statements.
	///
	/// A database written by an older version is first migrated with [`Self::migrate_database`].
	// This function should only be used on startup. There should be no other DB operations when
	// iterating the index.
	fn populate(&self, migrate_index: bool) -> Result<()> {
		if migrate_index {
			self.migrate_database()?;
		}

		{
			let mut submit_index = self.submit_index.write();
			let mut query_index = self.query_index.write();

			if let Some(counters) =
				self.db.get(col::META, KEY_COUNTERS).map_err(|e| Error::Db(e.to_string()))?
			{
				let (statement_count, total_size, next_seq) =
					<(u64, u64, u64)>::decode(&mut counters.as_slice())
						.map_err(|_| Error::Db("Error reading the store counters".into()))?;
				submit_index.statement_count = statement_count as usize;
				submit_index.total_size = total_size as usize;
				submit_index.next_seq = next_seq;
			}
			// While `next_seq` already holds the correct admission sequence number, it costs close
			// to nothing to check if the `next_seq` is consistent on the startup, and it can
			// catch a bug which could otherwise escape silently.
			let mut iter =
				self.db.iter(col::ADMISSION_SEQ).map_err(|e| Error::Db(e.to_string()))?;
			iter.seek_to_last().map_err(|e| Error::Db(e.to_string()))?;
			if let Some((key, _)) = iter.prev().map_err(|e| Error::Db(e.to_string()))? {
				let seq = u64::from_be_bytes(
					key.try_into()
						.map_err(|_| Error::Db("Invalid admission sequence key".into()))?,
				);
				if submit_index.next_seq <= seq {
					log::error!(
						target: LOG_TARGET,
						"The store counters lag the admission journal (next_seq {} <= last \
						 admission {}); recovering. This must be a bug, please report.",
						submit_index.next_seq,
						seq,
					);
					submit_index.next_seq = seq.saturating_add(1);
				}
			}

			let mut evicted_count = 0usize;
			self.db
				.iter_column_while(col::EXPIRED, |_| {
					evicted_count += 1;
					true
				})
				.map_err(|e| Error::Db(e.to_string()))?;
			submit_index.evicted_count = evicted_count;

			// Read-side cardinality counters, rebuilt from the index keys alone.
			for (prefix, count) in self.count_index_prefixes(col::INDEX_BY_TOPIC)? {
				let Ok(topic) = <[u8; 32]>::try_from(prefix.as_slice()).map(Topic::from) else {
					log::error!(
						target: LOG_TARGET,
						"Corrupt topic index prefix: {:?}",
						HexDisplay::from(&prefix)
					);
					continue;
				};
				query_index.topic_counts.insert(topic, count);
			}
			for (prefix, count) in self.count_index_prefixes(col::INDEX_BY_DEC_KEY)? {
				let Some(dec_key) = parse_dec_key_index_prefix(&prefix) else { continue };
				query_index.dec_key_counts.insert(dec_key, count);
			}
		}

		self.known_accounts_count.store(self.count_accounts()?, AtomicOrdering::Relaxed);
		self.maintain();
		Ok(())
	}

	/// Rebuilds every derived column from the authoritative
	/// `STATEMENTS` and `EXPIRED` columns. Existing rows are rewritten in place, so the migration
	/// is idempotent and safe to re-run after an interruption; the version is bumped only after
	/// everything else has been committed.
	fn migrate_database(&self) -> Result<()> {
		let purge_after_sec = self.submit_index.read().config.purge_after_sec;
		// Admission entries persisted by an earlier version keep their sequence numbers;
		// statements lacking one (all of them, on a migration from version 1) get fresh numbers.
		let mut admission_seqs = HashMap::new();
		let mut next_seq = 0u64;
		{
			let mut iter =
				self.db.iter(col::ADMISSION_SEQ).map_err(|e| Error::Db(e.to_string()))?;
			iter.seek_to_first().map_err(|e| Error::Db(e.to_string()))?;
			while let Some((key, value)) = iter.next().map_err(|e| Error::Db(e.to_string()))? {
				let seq = u64::from_be_bytes(
					key.try_into()
						.map_err(|_| Error::Db("Invalid admission sequence key".into()))?,
				);
				let hash: Hash = value
					.as_slice()
					.try_into()
					.map_err(|_| Error::Db("Invalid admission sequence hash".into()))?;
				admission_seqs.insert(hash, seq);
				next_seq = next_seq.max(seq.saturating_add(1));
			}
		}

		let mut migration = MigrationBatch::new(&self.db);
		let mut migration_error = None;
		let mut statement_count = 0usize;
		let mut total_size = 0usize;
		self.db
			.iter_column_while(col::STATEMENTS, |item| {
				let Ok(statement) = Statement::decode(&mut item.value.as_slice()) else {
					log::error!(
						target: LOG_TARGET,
						"Corrupt statement {:?}",
						HexDisplay::from(&sp_statement_store::hash_encoded(&item.value))
					);
					return true;
				};
				let hash = statement.hash();
				let Some(account) = statement.account_id() else {
					log::error!(
						target: LOG_TARGET,
						"Statement without an account id loaded from the DB: {:?}",
						HexDisplay::from(&hash)
					);
					return true;
				};
				log::trace!(target: LOG_TARGET, "Statement loaded {:?}", HexDisplay::from(&hash));
				let persisted_seq = admission_seqs.remove(&hash);
				let seq = persisted_seq.unwrap_or_else(|| {
					let seq = next_seq;
					next_seq = seq.saturating_add(1);
					seq
				});
				statement_count += 1;
				total_size += statement.data_len();
				let details = EntryDetails {
					channel: statement.channel(),
					data_len: statement.data_len(),
					admission_seq: seq,
				};
				let admission_op = persisted_seq.is_none().then(|| DbOperation {
					column: col::ADMISSION_SEQ,
					key: seq.to_be_bytes().to_vec(),
					value: Some(hash.to_vec()),
				});
				let operations = statement_index_ops(&hash, &statement, true)
					.into_iter()
					.chain(account_index_ops(
						&account,
						Expiry(statement.expiry()),
						&hash,
						Some(&details),
					))
					.map(DbOperation::from)
					.chain(admission_op);
				if let Err(error) = migration.extend(operations) {
					migration_error = Some(error);
					return false;
				}
				true
			})
			.map_err(|e| Error::Db(e.to_string()))?;
		if let Some(error) = migration_error.take() {
			return Err(error);
		}

		self.db
			.iter_column_while(col::EXPIRED, |item| {
				if let Ok((hash, timestamp)) = <(Hash, u64)>::decode(&mut item.value.as_slice()) {
					log::trace!(
						target: LOG_TARGET,
						"Statement loaded (expired): {:?}",
						HexDisplay::from(&hash)
					);
					let purge_at = timestamp.saturating_add(purge_after_sec);
					let operation = DbOperation {
						column: col::INDEX_EVICTED,
						key: evicted_index_key(purge_at, &hash),
						value: Some(INDEX_EMPTY_VALUE.to_vec()),
					};
					if let Err(error) = migration.push(operation) {
						migration_error = Some(error);
						return false;
					}
				}
				true
			})
			.map_err(|e| Error::Db(e.to_string()))?;
		if let Some(error) = migration_error {
			return Err(error);
		}

		migration.push(SubmitIndex::counters_op(statement_count, total_size, next_seq).into())?;
		let migrated_entries = migration.finish()?;
		self.db
			.commit([(
				col::META,
				KEY_VERSION.to_vec(),
				Some(CURRENT_VERSION.to_le_bytes().to_vec()),
			)])
			.map_err(|e| Error::Db(e.to_string()))?;
		log::info!(
			target: LOG_TARGET,
			"Migrated the statement store index to the on-disk format ({} rows)",
			migrated_entries
		);
		Ok(())
	}

	/// Counts, for every distinct prefix (the key minus its trailing 32-byte hash), the number of
	/// entries in a btree index column. The keys iterate in order, so each prefix's entries form
	/// one contiguous run.
	fn count_index_prefixes(&self, column: u8) -> Result<Vec<(Vec<u8>, usize)>> {
		let mut counts: Vec<(Vec<u8>, usize)> = Vec::new();
		let mut iter = self.db.iter(column).map_err(|e| Error::Db(e.to_string()))?;
		iter.seek_to_first().map_err(|e| Error::Db(e.to_string()))?;
		while let Some((key, _)) = iter.next().map_err(|e| Error::Db(e.to_string()))? {
			let Some(prefix) = prefix_from_index_key(&key) else { continue };
			match counts.last_mut() {
				Some((last, count)) if last.as_slice() == prefix => *count += 1,
				_ => counts.push((prefix.to_vec(), 1)),
			}
		}
		Ok(counts)
	}

	/// First account at or after `from` in the on-disk per-account index. Corrupt keys are skipped
	/// rather than treated as the end of the index: they must not disable allowance enforcement
	/// for the accounts sorted after them.
	fn next_account_from(&self, from: &AccountId) -> Result<Option<AccountId>> {
		let mut iter = self.db.iter(col::INDEX_BY_ACCOUNT).map_err(|e| Error::Db(e.to_string()))?;
		iter.seek(&from[..]).map_err(|e| Error::Db(e.to_string()))?;
		while let Some((key, _)) = iter.next().map_err(|e| Error::Db(e.to_string()))? {
			if let Some((account, _, _)) = parse_account_index_key(&key) {
				return Ok(Some(account));
			}
		}
		Ok(None)
	}

	/// Number of accounts with at least one stored statement, counted by hopping over the
	/// distinct account prefixes of the on-disk per-account index.
	fn count_accounts(&self) -> Result<usize> {
		let mut count = 0usize;
		let mut cursor = [0u8; 32];
		while let Some(account) = self.next_account_from(&cursor)? {
			count += 1;
			match next_account_id(account) {
				Some(next) => cursor = next,
				None => break,
			}
		}
		Ok(count)
	}

	/// Reads every per-account index row of `account`, in priority order.
	fn load_account_entries(
		&self,
		account: &AccountId,
	) -> Result<Vec<(PriorityKey, EntryDetails)>> {
		let mut entries = Vec::new();
		let mut iter = self.db.iter(col::INDEX_BY_ACCOUNT).map_err(|e| Error::Db(e.to_string()))?;
		iter.seek(&account[..]).map_err(|e| Error::Db(e.to_string()))?;
		while let Some((key, value)) = iter.next().map_err(|e| Error::Db(e.to_string()))? {
			if !key.starts_with(&account[..]) {
				break;
			}
			let Some((_, expiry, hash)) = parse_account_index_key(&key) else { continue };
			let Ok(details) = EntryDetails::decode(&mut value.as_slice()) else {
				log::error!(
					target: LOG_TARGET,
					"Corrupt account index row for statement {:?}",
					HexDisplay::from(&hash)
				);
				continue;
			};
			entries.push((PriorityKey { hash, expiry }, details));
		}
		Ok(entries)
	}

	/// Assembles an account's in-memory record from its on-disk index rows.
	fn load_account_record(&self, account: &AccountId) -> Result<StatementsForAccount> {
		let mut record = StatementsForAccount::default();
		for (key, details) in self.load_account_entries(account)? {
			record.insert_entry(key.hash, key.expiry, details);
		}
		Ok(record)
	}

	/// Scans an on-disk btree index column for every hash whose key starts with `prefix`.
	fn scan_index_prefix(&self, column: u8, prefix: &[u8]) -> Result<HashSet<Hash>> {
		let mut set = HashSet::new();
		let mut iter = self.db.iter(column).map_err(|e| Error::Db(e.to_string()))?;
		iter.seek(prefix).map_err(|e| Error::Db(e.to_string()))?;
		while let Some((key, _)) = iter.next().map_err(|e| Error::Db(e.to_string()))? {
			if !key.starts_with(prefix) {
				break;
			}
			if let Some(hash) = hash_from_index_key(&key) {
				set.insert(hash);
			}
		}
		Ok(set)
	}

	/// Enumerates the hashes of all active statements. Each statement has exactly one entry in
	/// [`col::INDEX_BY_DEC_KEY`], so scanning that column's keys yields every hash exactly once,
	/// without decoding any bodies.
	fn enumerate_hashes(&self) -> Result<Vec<Hash>> {
		let mut hashes = Vec::new();
		let mut iter = self.db.iter(col::INDEX_BY_DEC_KEY).map_err(|e| Error::Db(e.to_string()))?;
		iter.seek_to_first().map_err(|e| Error::Db(e.to_string()))?;
		while let Some((key, _)) = iter.next().map_err(|e| Error::Db(e.to_string()))? {
			if let Some(hash) = hash_from_index_key(&key) {
				hashes.push(hash);
			}
		}
		Ok(hashes)
	}

	/// Tests, against the on-disk column, whether `hash` belongs to an index set.
	fn index_set_contains(&self, set: &IndexSet, hash: &Hash) -> Result<bool> {
		Ok(self
			.db
			.get_size(set.column(), &set.member_key(hash))
			.map_err(|e| Error::Db(e.to_string()))?
			.is_some())
	}

	/// Whether `hash` is present in every one of `sets`, tested against the on-disk index. Used to
	/// intersect a materialised candidate set with the remaining topic / decryption-key sets.
	fn hash_in_all_sets(&self, hash: &Hash, sets: &[IndexSet]) -> Result<bool> {
		sets.iter().try_fold(true, |acc, set| {
			Ok::<bool, Error>(acc && self.index_set_contains(set, hash)?)
		})
	}

	/// Enumerates matching hashes for `key` / `topic`, reading candidates directly from the on-disk
	/// index for every filter kind. Used both for ad-hoc reads and for subscription snapshots.
	fn iterate_with(
		&self,
		key: Option<DecryptionKey>,
		topic_filter: &OptimizedTopicFilter,
		f: impl FnMut(&Hash) -> Result<()>,
	) -> Result<()> {
		match topic_filter {
			OptimizedTopicFilter::Any => self.iterate_with_any(key, f),
			OptimizedTopicFilter::MatchAll(topics) => self.iterate_with_match_all(key, topics, f),
			OptimizedTopicFilter::MatchAny(topics) => self.iterate_with_match_any(key, topics, f),
		}
	}

	/// Streams every hash for a decryption key directly from disk.
	fn iterate_with_any(
		&self,
		key: Option<DecryptionKey>,
		mut f: impl FnMut(&Hash) -> Result<()>,
	) -> Result<()> {
		let prefix = IndexSet::DecKey(key).prefix();
		let mut iter = self.db.iter(col::INDEX_BY_DEC_KEY).map_err(|e| Error::Db(e.to_string()))?;
		iter.seek(&prefix).map_err(|e| Error::Db(e.to_string()))?;
		while let Some((k, _)) = iter.next().map_err(|e| Error::Db(e.to_string()))? {
			if !k.starts_with(&prefix) {
				break;
			}
			if let Some(hash) = hash_from_index_key(&k) {
				f(&hash)?;
			}
		}
		Ok(())
	}

	/// For each requested topic, streams its hashes from disk and yields those that also belong to
	/// the decryption-key set. A hash carrying several requested topics is yielded once per topic,
	/// matching the in-memory behaviour callers already tolerate.
	fn iterate_with_match_any(
		&self,
		key: Option<DecryptionKey>,
		topics: &HashSet<Topic>,
		mut f: impl FnMut(&Hash) -> Result<()>,
	) -> Result<()> {
		let key_set = IndexSet::DecKey(key);
		for topic in topics {
			let prefix = topic[..].to_vec();
			let mut iter =
				self.db.iter(col::INDEX_BY_TOPIC).map_err(|e| Error::Db(e.to_string()))?;
			iter.seek(&prefix).map_err(|e| Error::Db(e.to_string()))?;
			while let Some((k, _)) = iter.next().map_err(|e| Error::Db(e.to_string()))? {
				if !k.starts_with(&prefix) {
					break;
				}
				if let Some(hash) = hash_from_index_key(&k) {
					if self.index_set_contains(&key_set, &hash)? {
						f(&hash)?;
					}
				}
			}
		}
		Ok(())
	}

	/// Intersects the decryption-key set with all requested topic sets, reading candidates directly
	/// from the on-disk index. The lock is taken only briefly to order the sets by cardinality so
	/// the smallest is materialised first; this ordering is a best-effort hint (stale counters can
	/// only misorder the sets, never drop a statement). Materialising that set and probing the rest
	/// against disk then happen without holding the lock.
	fn iterate_with_match_all(
		&self,
		key: Option<DecryptionKey>,
		topics: &HashSet<Topic>,
		mut f: impl FnMut(&Hash) -> Result<()>,
	) -> Result<()> {
		if topics.len() > MAX_TOPICS {
			return Ok(());
		}
		let mut sets = Vec::with_capacity(topics.len() + 1);
		sets.push(IndexSet::DecKey(key));
		for topic in topics {
			sets.push(IndexSet::Topic(*topic));
		}
		{
			// Ordering only (best-effort): stale counters can misorder but never drop a statement.
			let query_index = self.query_index.read();
			sets.sort_by_key(|s| s.len(&query_index));
		}
		let smallest = self.scan_index_prefix(sets[0].column(), &sets[0].prefix())?;
		let others = &sets[1..];
		for hash in &smallest {
			if self.hash_in_all_sets(hash, others)? {
				log::trace!(
					target: LOG_TARGET,
					"Iterating by topic/key: statement {:?}",
					HexDisplay::from(hash)
				);
				f(hash)?;
			}
		}
		Ok(())
	}

	/// Reads the raw SCALE-encoded body of `hash` from `col::STATEMENTS`, or `None` if it is absent
	/// (a benign DB race: the statement was removed concurrently). The stored value is exactly
	/// `statement.encode()`, so it can be forwarded verbatim.
	fn read_statement_encoded(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
		match self.db.get(col::STATEMENTS, hash).map_err(|e| Error::Db(e.to_string()))? {
			Some(entry) => Ok(Some(entry)),
			None => {
				log::debug!(target: LOG_TARGET, "Missing statement {:?}", HexDisplay::from(hash));
				Ok(None)
			},
		}
	}

	/// Reads and decodes the statement `hash`, returning `None` if it is absent or its stored body
	/// fails to decode (a corrupt DB row, which is logged and skipped).
	fn read_statement(&self, hash: &Hash) -> Result<Option<Statement>> {
		let Some(entry) = self.read_statement_encoded(hash)? else { return Ok(None) };
		match Statement::decode(&mut entry.as_slice()) {
			Ok(statement) => Ok(Some(statement)),
			Err(_) => {
				log::error!(target: LOG_TARGET, "Corrupt statement {:?}", HexDisplay::from(hash));
				Ok(None)
			},
		}
	}

	/// Collects statements matching `key` / `topic_filter`. Reads never hold the query-index lock
	/// across disk I/O: `Any` / `MatchAny` touch only the (thread-safe) database, and `MatchAll`
	/// takes the lock only momentarily to order the candidate sets.
	fn collect_statements<R>(
		&self,
		key: Option<DecryptionKey>,
		topic_filter: &OptimizedTopicFilter,
		mut f: impl FnMut(Statement) -> Option<R>,
	) -> Result<Vec<R>> {
		let mut result = Vec::new();
		self.iterate_with(key, topic_filter, |hash| {
			if let Some(statement) = self.read_statement(hash)? {
				if let Some(data) = f(statement) {
					result.push(data);
				}
			}
			Ok(())
		})?;
		Ok(result)
	}

	/// Evicts the lowest-priority statements of `account` while it exceeds the larger of its
	/// allowances at the finalized and the best block, spending at most `budget` evictions.
	/// Admission checks only the best block, so taking it here too keeps what admission accepted
	/// while finality lags. Expired statements neither count nor get evicted here; the expiry
	/// sweep reaps them.
	fn enforce_account_allowance(
		&self,
		account: &AccountId,
		current_time: u64,
		budget: &mut usize,
	) {
		let read_allowance =
			|allowance_block| match (self.read_allowance_fn)(account, allowance_block) {
				Ok(Some(allowance)) => Some(allowance),
				Ok(None) => {
					log::debug!(
						target: LOG_TARGET,
						"No allowance found for account {:?}, treating as zero allowance",
						HexDisplay::from(account)
					);
					Some(StatementAllowance { max_count: 0, max_size: 0 })
				},
				Err(e) => {
					log::error!(target: LOG_TARGET, "Error reading allowance: {:?}", e);
					// Skip allowance enforcement for this account on error
					None
				},
			};
		let fits = |allowance: &StatementAllowance, count: usize, size: usize| {
			count <= allowance.max_count as usize && size <= allowance.max_size as usize
		};
		let Some(finalized) = read_allowance(AllowanceBlock::Finalized) else { return };

		// A cached summary proving the account within its allowance saves the disk scan. The
		// summary also counts expired-but-unswept statements, so it can only overestimate usage,
		// which is fine for a within-limit conclusion; the over-limit path recounts from disk.
		if let Some(summary) = self.submit_index.read().summaries.peek(account) {
			if fits(&finalized, summary.count, summary.data_size) {
				return;
			}
		}

		let entries = match self.load_account_entries(account) {
			Ok(entries) => entries,
			Err(e) => {
				log::warn!(target: LOG_TARGET, "Error reading the account index: {:?}", e);
				return;
			},
		};
		let expiry_bound = Expiry(current_time << 32);
		let mut remaining_count = 0usize;
		let mut remaining_size = 0usize;
		for (key, details) in &entries {
			if key.expiry >= expiry_bound {
				remaining_count += 1;
				remaining_size += details.data_len;
			}
		}
		if fits(&finalized, remaining_count, remaining_size) {
			return;
		}
		// The best block is read only for accounts already over their finalized allowance
		let Some(best) = read_allowance(AllowanceBlock::Best) else { return };
		let allowance = StatementAllowance {
			max_count: finalized.max_count.max(best.max_count),
			max_size: finalized.max_size.max(best.max_size),
		};
		if fits(&allowance, remaining_count, remaining_size) {
			return;
		}
		log::debug!(
			target: LOG_TARGET,
			"Account {:?} exceeds allowance: count={}/{}, size={}/{}",
			HexDisplay::from(account),
			remaining_count,
			allowance.max_count,
			remaining_size,
			allowance.max_size
		);

		// Evict lowest priority statements that exceed allowance
		for (key, details) in &entries {
			if fits(&allowance, remaining_count, remaining_size) || *budget == 0 {
				break;
			}
			if key.expiry < expiry_bound {
				continue;
			}
			log::debug!(
				target: LOG_TARGET,
				"Evicting statement {:?} due to allowance enforcement",
				HexDisplay::from(&key.hash)
			);
			if let Err(e) = self.remove(&key.hash) {
				log::debug!(
					target: LOG_TARGET,
					"Error evicting statement {:?}: {:?}",
					HexDisplay::from(&key.hash),
					e
				);
			} else {
				*budget -= 1;
				remaining_count -= 1;
				remaining_size -= details.data_len;
			}
		}
	}

	// Reaps expired statements and enforces per-account allowances against the on-disk index.
	//
	// Expired statements form a prefix of the global expiry index ([`col::INDEX_BY_EXPIRY`]), so
	// the sweep costs time proportional to the number of due statements, not to the store size.
	// Allowance enforcement then walks the accounts of [`col::INDEX_BY_ACCOUNT`] incrementally,
	// resuming from where the previous call stopped (`allowance_cursor`), so the process
	// eventually covers all accounts across multiple invocations.
	//
	// The two phases are budgeted independently: each gets its own
	// `MAX_EXPIRY_STATEMENTS_PER_ITERATION` statement budget and its own
	// `MAX_EXPIRY_TIME_PER_ITERATION` time slice, and allowance enforcement is additionally
	// bounded by `MAX_EXPIRY_ACCOUNTS_PER_ITERATION` accounts checked. The isolation matters:
	// with shared budgets, a store whose expiry inflow persistently exceeds the expiry budget
	// (a full store with a short TTL) would never get an allowance pass at all.
	//
	// Statements are considered expired when their expiry (which encodes the expiration
	// timestamp in the upper 32 bits) is less than the current timestamp.
	/// Reap expired statements and enforce per-account allowances (one bounded pass); runs
	/// periodically from the background task. Hidden: exposed only for the benchmarks.
	#[doc(hidden)]
	pub fn enforce_limits(&self) {
		self.enforce_limits_bounded(
			MAX_EXPIRY_STATEMENTS_PER_ITERATION,
			MAX_EXPIRY_ACCOUNTS_PER_ITERATION,
			MAX_EXPIRY_TIME_PER_ITERATION,
		)
	}

	/// Body of [`Self::enforce_limits`] with the per-call bounds as parameters, letting the
	/// tests drive the incremental sweep with budgets small enough to need several calls.
	/// `statement_budget` and `time_budget` apply to each phase separately.
	fn enforce_limits_bounded(
		&self,
		statement_budget: usize,
		account_budget: usize,
		time_budget: Duration,
	) {
		let _start_check_expiration_timer = self.metrics.start_check_expiration_timer();
		let current_time = self.timestamp();
		let start = Instant::now();
		let mut expired = 0u64;

		// Phase 1: reap statements past their expiry, straight off the expiry index.
		let mut due = Vec::new();
		let scan = (|| -> Result<()> {
			let mut iter =
				self.db.iter(col::INDEX_BY_EXPIRY).map_err(|e| Error::Db(e.to_string()))?;
			iter.seek_to_first().map_err(|e| Error::Db(e.to_string()))?;
			while let Some((key, _)) = iter.next().map_err(|e| Error::Db(e.to_string()))? {
				let Some((expiry, hash)) = parse_time_index_key(&key) else { continue };
				if expiry >= (current_time << 32) {
					// Entries are ordered by expiry, so nothing further is due.
					break;
				}
				due.push((expiry, hash));
				if due.len() >= statement_budget || start.elapsed() >= time_budget {
					break;
				}
			}
			Ok(())
		})();
		if let Err(e) = scan {
			log::warn!(target: LOG_TARGET, "Error scanning the expiry index: {:?}", e);
		}
		for (expiry, hash) in due {
			match self.remove_statement(&hash) {
				Ok(true) => {
					expired += 1;
					log::trace!(
						target: LOG_TARGET,
						"Marked statement {:?} as expired",
						HexDisplay::from(&hash)
					);
				},
				// The row survived its statement: either a concurrent removal won the race (and
				// deleted the row along with the body), or the store is inconsistent.
				Ok(false) => self.report_orphan_expiry_row(expiry, &hash),
				Err(e) => {
					log::debug!(
						target: LOG_TARGET,
						"Error marking statement {:?} as expired: {:?}",
						HexDisplay::from(&hash),
						e
					);
				},
			}
		}

		// Phase 2: enforce allowances account by account, resuming from the cursor. The phase
		// runs on its own statement and time budgets: with shared ones, a sustained expiry
		// backlog exhausting phase 1's budget on every pass would starve allowance enforcement
		// indefinitely.
		let allowance_start = Instant::now();
		let mut allowance_budget = statement_budget;
		let mut cursor = self.submit_index.read().allowance_cursor;
		let mut checked = 0usize;
		let mut wrapped = false;
		while checked < account_budget &&
			allowance_budget > 0 &&
			allowance_start.elapsed() < time_budget
		{
			let from = cursor.unwrap_or([0u8; 32]);
			let account = match self.next_account_from(&from) {
				Ok(Some(account)) => account,
				Ok(None) => {
					wrapped = true;
					break;
				},
				Err(e) => {
					log::warn!(target: LOG_TARGET, "Error reading the account index: {:?}", e);
					break;
				},
			};
			checked += 1;
			let budget_before = allowance_budget;
			self.enforce_account_allowance(&account, current_time, &mut allowance_budget);
			expired += (budget_before - allowance_budget) as u64;
			cursor = match next_account_id(account) {
				next @ Some(_) => next,
				None => {
					wrapped = true;
					break;
				},
			};
		}
		{
			let mut submit_index = self.submit_index.write();
			submit_index.allowance_cycle_seen += checked;
			if wrapped {
				// A full pass over the account index just completed; it is the only place where
				// the total number of accounts is (re)counted.
				self.known_accounts_count
					.store(submit_index.allowance_cycle_seen, AtomicOrdering::Relaxed);
				submit_index.allowance_cycle_seen = 0;
				submit_index.allowance_cursor = None;
			} else {
				submit_index.allowance_cursor = cursor;
			}
		}

		drop(_start_check_expiration_timer);

		self.metrics.report(|metrics| {
			metrics.statements_expired_total.inc_by(expired);
		});
	}

	/// Drains the on-disk evicted journal of entries whose purge deadline has passed. Returns how
	/// many entries were drained.
	fn drain_due_evicted(&self, current_time: u64) -> Result<usize> {
		let mut commit: Vec<(u8, Vec<u8>, Option<Vec<u8>>)> = Vec::new();
		let mut drained = 0usize;
		{
			let mut iter =
				self.db.iter(col::INDEX_EVICTED).map_err(|e| Error::Db(e.to_string()))?;
			iter.seek_to_first().map_err(|e| Error::Db(e.to_string()))?;
			loop {
				let Some((key, _)) = iter.next().map_err(|e| Error::Db(e.to_string()))? else {
					break;
				};
				let Some((purge_at, hash)) = parse_time_index_key(&key) else { continue };
				if purge_at > current_time {
					// Entries are ordered by purge time, so nothing further is due.
					break;
				}
				commit.push((col::EXPIRED, hash.to_vec(), None));
				commit.push((col::INDEX_EVICTED, key, None));
				drained += 1;
			}
		}
		if !commit.is_empty() {
			self.db.commit(commit).map_err(|e| Error::Db(e.to_string()))?;
		}
		Ok(drained)
	}

	/// Perform periodic store maintenance: permanently delete statements whose purge period has
	/// elapsed and refresh store metrics.
	///
	/// Expired and evicted statements are not removed from the database immediately; they are kept
	/// in the `EXPIRED` column for [`DEFAULT_PURGE_AFTER_SEC`] (default 48h) to prevent
	/// re-acceptance while they may still be propagating over gossip. This method removes those
	/// whose purge period has passed.
	///
	/// Runs in a background task on a fixed interval (`MAINTENANCE_PERIOD`, 29s). Enforcing
	/// per-account and global limits — expiring over-quota statements — is handled separately by
	/// `enforce_limits` on its own interval (`ENFORCE_LIMITS_PERIOD`, 31s), kept distinct to avoid
	/// holding the index lock for too long during maintenance.
	pub fn maintain(&self) {
		log::trace!(target: LOG_TARGET, "Started store maintenance");
		let current_time = self.timestamp();
		let deleted_count = match self.drain_due_evicted(current_time) {
			Ok(count) => count as u64,
			Err(e) => {
				log::warn!(target: LOG_TARGET, "Error writing to the statement database: {:?}", e);
				0
			},
		};

		let (active_count, expired_count, total_size, capacity_statements, capacity_bytes) = {
			let mut submit_index = self.submit_index.write();
			submit_index.evicted_count =
				submit_index.evicted_count.saturating_sub(deleted_count as usize);
			(
				submit_index.statement_count,
				submit_index.evicted_count,
				submit_index.total_size,
				submit_index.config.max_total_statements,
				submit_index.config.max_total_size,
			)
		};
		let accounts_count = self.known_accounts_count.load(AtomicOrdering::Relaxed);

		if deleted_count > 0 {
			self.metrics.report(|metrics| metrics.statements_pruned.inc_by(deleted_count));
		}

		self.metrics.report(|metrics| {
			metrics.statements_total.set(active_count as u64);
			metrics.bytes_total.set(total_size as u64);
			metrics.accounts_total.set(accounts_count as u64);
			metrics.expired_total.set(expired_count as u64);
			metrics.capacity_statements.set(capacity_statements as u64);
			metrics.capacity_bytes.set(capacity_bytes as u64);
		});

		log::trace!(
			target: LOG_TARGET,
			"Completed store maintenance. Purged: {}, Active: {}, Expired: {}",
			deleted_count,
			active_count,
			expired_count
		);
	}

	fn timestamp(&self) -> u64 {
		self.time_override.unwrap_or_else(|| {
			std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.unwrap_or_default()
				.as_secs()
		})
	}

	#[cfg(test)]
	fn set_time(&mut self, time: u64) {
		self.time_override = Some(time);
	}

	/// Returns `self` as [`StatementStoreExt`].
	pub fn as_statement_store_ext(self: Arc<Self>) -> StatementStoreExt {
		StatementStoreExt::new(self)
	}

	/// Return information of all known statements whose decryption key is identified as
	/// `dest`. The key must be available to the client.
	fn posted_clear_inner<R>(
		&self,
		match_all_topics: &[Topic],
		dest: [u8; 32],
		// Map the statement and the decrypted data to the desired result.
		mut map_f: impl FnMut(Statement, Vec<u8>) -> R,
	) -> Result<Vec<R>> {
		self.collect_statements(
			Some(dest),
			&OptimizedTopicFilter::MatchAll(match_all_topics.iter().cloned().collect()),
			|statement| {
				if let (Some(key), Some(_)) = (statement.decryption_key(), statement.data()) {
					let public: sp_core::ed25519::Public = UncheckedFrom::unchecked_from(key);
					let public: sp_statement_store::ed25519::Public = public.into();
					match self.keystore.key_pair::<sp_statement_store::ed25519::Pair>(&public) {
						Err(e) => {
							log::debug!(
								target: LOG_TARGET,
								"Keystore error: {:?}, for statement {:?}",
								e,
								HexDisplay::from(&statement.hash())
							);
							None
						},
						Ok(None) => {
							log::debug!(
								target: LOG_TARGET,
								"Keystore is missing key for statement {:?}",
								HexDisplay::from(&statement.hash())
							);
							None
						},
						Ok(Some(pair)) => match statement.decrypt_private(&pair.into_inner()) {
							Ok(r) => r.map(|data| map_f(statement, data)),
							Err(e) => {
								log::debug!(
									target: LOG_TARGET,
									"Decryption error: {:?}, for statement {:?}",
									e,
									HexDisplay::from(&statement.hash())
								);
								None
							},
						},
					}
				} else {
					None
				}
			},
		)
	}
}

impl StatementStore for Store {
	/// Return every statement currently in the store.
	///
	/// Takes a read lock on the query index, iterates all indexed hashes, reads and SCALE-decodes
	/// each statement from the `STATEMENTS` database column, and skips any entry that fails to
	/// decode.
	fn statements(&self) -> Result<Vec<(Hash, Statement)>> {
		let hashes = self.enumerate_hashes()?;
		let mut result = Vec::with_capacity(hashes.len());
		for hash in hashes {
			let Some(encoded) =
				self.db.get(col::STATEMENTS, &hash).map_err(|e| Error::Db(e.to_string()))?
			else {
				continue;
			};
			match Statement::decode(&mut encoded.as_slice()) {
				Ok(statement) => result.push((hash, statement)),
				Err(_) => log::error!(
					target: LOG_TARGET,
					"Corrupt statement {:?}",
					HexDisplay::from(&hash)
				),
			}
		}
		Ok(result)
	}

	fn take_recent_statements(&self) -> Result<Vec<(u64, Hash, Statement)>> {
		let recent = self.query_index.write().take_recent();
		let mut result = Vec::with_capacity(recent.len());
		for (hash, seq) in recent {
			let Some(encoded) =
				self.db.get(col::STATEMENTS, &hash).map_err(|e| Error::Db(e.to_string()))?
			else {
				continue;
			};
			match Statement::decode(&mut encoded.as_slice()) {
				Ok(statement) => result.push((seq, hash, statement)),
				Err(_) => log::error!(
					target: LOG_TARGET,
					"Corrupt statement {:?}",
					HexDisplay::from(&hash)
				),
			}
		}
		result.sort_unstable_by_key(|(seq, ..)| *seq);
		Ok(result)
	}

	fn admission_watermark(&self) -> Result<u64> {
		// Every admission commits its journal row under the submit-index write lock before
		// releasing it, so a lock holder observes every sequence number below `next_seq`
		// already persisted.
		Ok(self.submit_index.read().next_seq)
	}

	fn admitted_statements(
		&self,
		mut cursor: u64,
		watermark: u64,
		scan_limit: usize,
		filter: &mut dyn FnMut(&Hash, &[u8], &Statement) -> FilterDecision,
	) -> Result<AdmittedBatch> {
		// A cursor past the watermark would snap back to it below, moving the caller's
		// position backwards.
		debug_assert!(
			cursor <= watermark,
			"admission cursor {cursor} must not exceed the watermark {watermark}"
		);
		debug_assert!(scan_limit > 0, "a zero scan limit cannot advance the cursor");
		let mut statements = Vec::new();
		let mut aborted = false;
		let mut scanned = 0usize;
		let mut iter = self.db.iter(col::ADMISSION_SEQ).map_err(|e| Error::Db(e.to_string()))?;
		iter.seek(&cursor.to_be_bytes()).map_err(|e| Error::Db(e.to_string()))?;

		while let Some((key, value)) = iter.next().map_err(|e| Error::Db(e.to_string()))? {
			let seq = u64::from_be_bytes(
				key.try_into().map_err(|_| Error::Db("Invalid admission sequence key".into()))?,
			);
			if seq >= watermark {
				cursor = watermark;
				break;
			}
			if scanned == scan_limit {
				aborted = true;
				break;
			}
			scanned += 1;
			let hash: Hash = value
				.as_slice()
				.try_into()
				.map_err(|_| Error::Db("Invalid admission sequence hash".into()))?;
			let next_cursor = seq.saturating_add(1);
			let Some(encoded) =
				self.db.get(col::STATEMENTS, &hash).map_err(|e| Error::Db(e.to_string()))?
			else {
				cursor = next_cursor;
				continue;
			};
			// The admission row is deleted atomically with the statement it admits, so re-reading
			// it after the body confirms that this sequence number is still the statement's
			// current admission (and not a stale row of an evicted-and-readmitted statement).
			let is_current = self
				.db
				.get(col::ADMISSION_SEQ, &seq.to_be_bytes())
				.map_err(|e| Error::Db(e.to_string()))?
				.is_some_and(|current| current.as_slice() == hash.as_slice());
			if !is_current {
				cursor = next_cursor;
				continue;
			}
			let statement = match Statement::decode(&mut encoded.as_slice()) {
				Ok(statement) => statement,
				Err(e) => {
					log::error!(
						target: LOG_TARGET,
						"Could not decode statement {:?} while walking the admission journal: {:?}",
						HexDisplay::from(&hash),
						e
					);
					cursor = next_cursor;
					continue;
				},
			};
			match filter(&hash, &encoded, &statement) {
				FilterDecision::Skip => cursor = next_cursor,
				FilterDecision::Take => {
					statements.push((hash, statement));
					cursor = next_cursor;
				},
				FilterDecision::Abort => {
					aborted = true;
					break;
				},
			}
		}

		// The journal can end below the watermark when the newest admissions were removed;
		// nothing is left to visit there, so the walk is complete.
		if !aborted && cursor < watermark {
			cursor = watermark;
		}
		Ok(AdmittedBatch { statements, cursor, done: cursor >= watermark })
	}

	/// Read a single statement directly from the `STATEMENTS` database column by hash and decode
	/// it. Returns `Ok(None)` if no statement with that hash is stored.
	fn statement(&self, hash: &Hash) -> Result<Option<Statement>> {
		Ok(
			match self
				.db
				.get(col::STATEMENTS, hash.as_slice())
				.map_err(|e| Error::Db(e.to_string()))?
			{
				Some(entry) => {
					log::trace!(
						target: LOG_TARGET,
						"Queried statement {:?}",
						HexDisplay::from(hash)
					);
					Some(Statement::decode(&mut entry.as_slice()).map_err(|e| {
						log::error!(
							target: LOG_TARGET,
							"Corrupt statement {:?}",
							HexDisplay::from(hash)
						);
						Error::Decode(e.to_string())
					})?)
				},
				None => {
					log::trace!(
						target: LOG_TARGET,
						"Queried missing statement {:?}",
						HexDisplay::from(hash)
					);
					None
				},
			},
		)
	}

	fn has_statement(&self, hash: &Hash) -> bool {
		match self.db.get_size(col::STATEMENTS, hash.as_slice()) {
			Ok(size) => size.is_some(),
			Err(e) => {
				log::debug!(
					target: LOG_TARGET,
					"Error checking statement presence {:?}: {:?}",
					HexDisplay::from(hash),
					e
				);
				false
			},
		}
	}

	fn statements_by_hashes(
		&self,
		hashes: &[Hash],
		filter: &mut dyn FnMut(&Hash, &[u8], &Statement) -> FilterDecision,
	) -> Result<(Vec<(Hash, Statement)>, usize)> {
		let mut result = Vec::new();
		let mut processed = 0;
		for hash in hashes {
			processed += 1;
			let Some(encoded) =
				self.db.get(col::STATEMENTS, hash).map_err(|e| Error::Db(e.to_string()))?
			else {
				continue;
			};
			let Ok(statement) = Statement::decode(&mut encoded.as_slice()) else {
				log::error!(target: LOG_TARGET, "Corrupt statement {:?}", HexDisplay::from(hash));
				continue;
			};
			match filter(hash, &encoded, &statement) {
				FilterDecision::Skip => {},
				FilterDecision::Take => {
					result.push((*hash, statement));
				},
				FilterDecision::Abort => {
					// We did not process it :)
					processed -= 1;
					break;
				},
			}
		}

		Ok((result, processed))
	}

	/// Return the `data` of all statements matching all of `match_all_topics` that have no
	/// decryption key (i.e. public broadcasts).
	///
	/// Filters the query index by topic (intersection; an empty list matches every broadcast),
	/// reads and decodes each match from the `STATEMENTS` column, and returns the plaintext data,
	/// skipping any inconsistent entries.
	fn broadcasts(&self, match_all_topics: &[Topic]) -> Result<Vec<Vec<u8>>> {
		self.collect_statements(
			None,
			&OptimizedTopicFilter::MatchAll(match_all_topics.iter().cloned().collect()),
			|statement| statement.into_data(),
		)
	}

	/// Return the (encrypted) `data` of all statements matching all of `match_all_topics` whose
	/// decryption key equals `dest`.
	///
	/// Same filtering and DB read as [`broadcasts`](Self::broadcasts), but keyed on `dest` rather
	/// than the absence of a decryption key.
	fn posted(&self, match_all_topics: &[Topic], dest: [u8; 32]) -> Result<Vec<Vec<u8>>> {
		self.collect_statements(
			Some(dest),
			&OptimizedTopicFilter::MatchAll(match_all_topics.iter().cloned().collect()),
			|statement| statement.into_data(),
		)
	}

	/// Like [`posted`](Self::posted) but returns the decrypted data.
	///
	/// For each match, looks up the ed25519 key identified by `dest` in the keystore and decrypts
	/// the statement data; statements are skipped when the key is unavailable or decryption fails.
	fn posted_clear(&self, match_all_topics: &[Topic], dest: [u8; 32]) -> Result<Vec<Vec<u8>>> {
		self.posted_clear_inner(match_all_topics, dest, |_statement, data| data)
	}

	/// Return the full SCALE-encoded statements matching all of `match_all_topics` that have no
	/// decryption key (i.e. public broadcasts).
	///
	/// Takes a read lock on the query index and filters by the absence of a decryption key and
	/// topics (intersection / AND — an empty topic list matches every broadcast), then reads,
	/// decodes and re-encodes each match from the `STATEMENTS` column, skipping inconsistent
	/// entries. Unlike [`broadcasts`](Self::broadcasts), which returns only the data, this returns
	/// the whole statement.
	fn broadcasts_stmt(&self, match_all_topics: &[Topic]) -> Result<Vec<Vec<u8>>> {
		self.collect_statements(
			None,
			&OptimizedTopicFilter::MatchAll(match_all_topics.iter().cloned().collect()),
			|statement| Some(statement.encode()),
		)
	}

	/// Return the full SCALE-encoded statements matching all of `match_all_topics` whose decryption
	/// key equals `dest`.
	///
	/// Takes a read lock on the query index and filters by decryption key (`dest`) and topics
	/// (intersection / AND — an empty topic list matches every statement keyed to `dest`), then
	/// reads, decodes and re-encodes each match from the `STATEMENTS` column, skipping inconsistent
	/// entries. Unlike [`posted`](Self::posted), which returns only the (still-encrypted) data,
	/// this returns the whole statement.
	fn posted_stmt(&self, match_all_topics: &[Topic], dest: [u8; 32]) -> Result<Vec<Vec<u8>>> {
		self.collect_statements(
			Some(dest),
			&OptimizedTopicFilter::MatchAll(match_all_topics.iter().cloned().collect()),
			|statement| Some(statement.encode()),
		)
	}

	/// Return, for each statement matching all of `match_all_topics` whose decryption key equals
	/// `dest`, the SCALE-encoded statement concatenated with its decrypted data.
	///
	/// Filters as [`posted_stmt`](Self::posted_stmt), then for each match looks up the ed25519 key
	/// identified by `dest` in the keystore and decrypts the statement data, appending the
	/// plaintext to the encoded statement. Statements are skipped when the key is unavailable or
	/// decryption fails.
	fn posted_clear_stmt(
		&self,
		match_all_topics: &[Topic],
		dest: [u8; 32],
	) -> Result<Vec<Vec<u8>>> {
		self.posted_clear_inner(match_all_topics, dest, |statement, data| {
			let mut res = Vec::with_capacity(statement.size_hint() + data.len());
			statement.encode_to(&mut res);
			res.extend_from_slice(&data);
			res
		})
	}

	/// Submit a statement to the store, validating it and enforcing constraints.
	///
	/// Runs the following pipeline, short-circuiting on the first failure:
	/// 1. **Expiry check** — reject if the statement's expiration timestamp is already in the past
	///    (`SubmitResult::Invalid(InvalidReason::AlreadyExpired)`).
	/// 2. **Encoding size check** — reject if the encoded statement exceeds [`MAX_STATEMENT_SIZE`]
	///    (`InvalidReason::EncodingTooLarge`).
	/// 3. **Duplicate check** — a statement already in the store is reported `Known`: the hash
	///    covers the whole content, so a duplicate carries nothing to renew. A recently evicted
	///    statement may be resubmitted (renewed) by `Chain` and `Local` sources but not by
	///    `Network` (`SubmitResult::KnownExpired`).
	/// 4. **Proof & signature** — extract the account from the proof and verify the signature
	///    (`InvalidReason::NoProof` / `InvalidReason::BadProof`).
	/// 5. **Allowance** — read the account's allowance (`StatementAllowance`: max count and size)
	///    directly from chain state at the best block (via the `statement_allowance_key` storage
	///    key — not a runtime call); reject with `SubmitResult::Rejected(NoAllowance)` if none is
	///    set. The best block is used for responsiveness. Limit enforcement takes the larger of the
	///    best and the finalized allowance, so a statement accepted here outlives the finality lag;
	///    it is evicted later if the allowance it relied on leaves the best chain or a lower
	///    allowance is finalized.
	/// 6. **Constraint check & eviction** — check the account's record, enforcing per-account
	///    limits (count, size, one statement per channel, higher priority replaces lower) and
	///    global limits ([`DEFAULT_MAX_TOTAL_STATEMENTS`], [`DEFAULT_MAX_TOTAL_SIZE`]), evicting
	///    lower-priority statements of the same account as needed (`SubmitResult::Rejected` if it
	///    still does not fit).
	/// 7. **Persist** — commit the statement, its index rows, any evictions and the refreshed
	///    counters atomically, then update the caches and the in-memory query index.
	///
	/// Returns `SubmitResult::New` on success.
	fn submit(&self, statement: Statement, source: StatementSource) -> SubmitResult {
		let _histogram_submit_start_timer = self.metrics.start_submit_timer();
		let hash = statement.hash();
		// Get unix timestamp
		if statement.is_expired(self.timestamp()) {
			log::debug!(
				target: LOG_TARGET,
				"Statement is already expired: {:?}",
				HexDisplay::from(&hash),
			);
			let reason = InvalidReason::AlreadyExpired;
			self.metrics.report(|metrics| {
				metrics.validations_invalid.with_label_values(&[reason.label()]).inc();
			});
			return SubmitResult::Invalid(reason);
		}
		let encoded_size = statement.encoded_size();
		if encoded_size > MAX_STATEMENT_SIZE {
			log::debug!(
				target: LOG_TARGET,
				"Statement is too big for propogation: {:?} ({}/{} bytes)",
				HexDisplay::from(&hash),
				statement.encoded_size(),
				MAX_STATEMENT_SIZE
			);
			let reason = InvalidReason::EncodingTooLarge {
				submitted_size: encoded_size,
				max_size: MAX_STATEMENT_SIZE,
			};
			self.metrics.report(|metrics| {
				metrics.validations_invalid.with_label_values(&[reason.label()]).inc();
			});
			return SubmitResult::Invalid(reason);
		}

		// Deduplicate against statements we already store (in-memory submit index) or have recently
		// evicted (on-disk evicted journal). Both checks are lock-free; a duplicate slipping
		// past this early check is caught again under the write lock below.
		if self.db.get_size(col::STATEMENTS, hash.as_slice()).ok().flatten().is_some() {
			if !source.can_be_resubmitted() {
				self.metrics.report(|metrics| {
					metrics.known_statements.with_label_values(&["known"]).inc();
				});
				return SubmitResult::Known;
			}
		} else if self.db.get_size(col::EXPIRED, hash.as_slice()).ok().flatten().is_some() {
			if !source.can_be_resubmitted() {
				self.metrics.report(|metrics| {
					metrics.known_statements.with_label_values(&["known_expired"]).inc();
				});
				return SubmitResult::KnownExpired;
			}
		}

		let Some(account_id) = statement.account_id() else {
			log::debug!(
				target: LOG_TARGET,
				"Statement validation failed: Missing proof ({:?})",
				HexDisplay::from(&hash),
			);
			let reason = InvalidReason::NoProof;
			self.metrics.report(|metrics| {
				metrics.validations_invalid.with_label_values(&[reason.label()]).inc();
			});
			return SubmitResult::Invalid(reason);
		};

		match statement.verify_signature() {
			SignatureVerificationResult::Valid(_) => {},
			SignatureVerificationResult::Invalid => {
				log::debug!(
					target: LOG_TARGET,
					"Statement validation failed: BadProof, {:?}",
					HexDisplay::from(&hash),
				);
				let reason = InvalidReason::BadProof;
				self.metrics.report(|metrics| {
					metrics.validations_invalid.with_label_values(&[reason.label()]).inc();
				});
				return SubmitResult::Invalid(reason);
			},
			SignatureVerificationResult::NoSignature => {
				log::debug!(
					target: LOG_TARGET,
					"Statement validation failed: NoProof, {:?}",
					HexDisplay::from(&hash),
				);
				let reason = InvalidReason::NoProof;
				self.metrics.report(|metrics| {
					metrics.validations_invalid.with_label_values(&[reason.label()]).inc();
				});
				return SubmitResult::Invalid(reason);
			},
		};

		// Check statement allowance for the account and evict statements if necessary to make room
		// for the new statement. We use the best block for allowance checks to allow for more
		// up-to-date allowances. This means that in some cases, a statement may be accepted but
		// then later evicted when we enforce limits, if the best_hash does not make it into the
		// finalized chain, but this is an acceptable tradeoff for better responsiveness to
		// allowance changes.
		let validation = match (self.read_allowance_fn)(&account_id, AllowanceBlock::Best) {
			Ok(Some(allowance)) if !allowance.is_depleted() => allowance,
			Ok(Some(_)) | Ok(None) => {
				log::debug!(
					target: LOG_TARGET,
					"Account {} has no usable statement allowance",
					HexDisplay::from(&account_id),
				);
				let reason = RejectionReason::NoAllowance;
				self.metrics.report(|metrics| {
					metrics.rejections.with_label_values(&[reason.label()]).inc();
				});
				return SubmitResult::Rejected(reason);
			},
			Err(e) => {
				log::debug!(
					target: LOG_TARGET,
					"Reading statement allowance for account {} failed",
					HexDisplay::from(&account_id),
				);
				self.metrics.report(|metrics| {
					metrics.internal_errors.with_label_values(&["read_allowance"]).inc();
				});
				return SubmitResult::InternalError(e);
			},
		};

		let current_time = self.timestamp();
		let seq = {
			let mut submit_index = self.submit_index.write();

			// Re-check for a duplicate under the write lock. Without
			// it, a double insertion would double-count the statement in the global counters and
			// in the account's quota.
			match self.db.get_size(col::STATEMENTS, hash.as_slice()) {
				Ok(Some(_)) => {
					self.metrics.report(|metrics| {
						metrics.known_statements.with_label_values(&["known"]).inc();
					});
					return SubmitResult::Known;
				},
				Ok(None) => {},
				Err(e) => {
					self.metrics.report(|metrics| {
						metrics.internal_errors.with_label_values(&["db_read"]).inc();
					});
					return SubmitResult::InternalError(Error::Db(e.to_string()));
				},
			}

			let statement_len = statement.data_len();
			// The account record is materialised only when the constraint check actually needs
			// it: a channel-less statement from an account whose cached summary shows headroom
			// is admitted without touching the account's on-disk index at all, and an oversize
			// statement is rejected by `plan_insert` before it ever looks at the record.
			let oversize = statement_len > validation.max_size as usize;
			let cached = submit_index.account_statements.peek(&account_id).is_some();
			let summary_admits = !cached &&
				statement.channel().is_none() &&
				submit_index.summaries.peek(&account_id).is_some_and(|summary| {
					summary.count < validation.max_count as usize &&
						summary.data_size + statement_len <= validation.max_size as usize
				}) && submit_index.statement_count <
				submit_index.config.max_total_statements &&
				submit_index.total_size + statement_len <= submit_index.config.max_total_size;
			let loaded_record = if cached || summary_admits || oversize {
				None
			} else {
				match self.load_account_record(&account_id) {
					Ok(record) => Some(record),
					Err(e) => {
						self.metrics.report(|metrics| {
							metrics.internal_errors.with_label_values(&["db_read"]).inc();
						});
						return SubmitResult::InternalError(e);
					},
				}
			};

			let empty_record = StatementsForAccount::default();
			let record = loaded_record
				.as_ref()
				.or_else(|| submit_index.account_statements.peek(&account_id))
				.unwrap_or(&empty_record);
			let plan = match submit_index.plan_insert(
				record,
				hash,
				&statement,
				&account_id,
				&validation,
				current_time,
			) {
				Ok(plan) => plan,
				Err(reason) => {
					self.metrics.report(|metrics| {
						metrics.rejections.with_label_values(&[reason.label()]).inc();
					});
					// The rejection left the store untouched, so a record loaded for planning
					// still mirrors the disk. Cache it: rejections cost the sender nothing, and
					// dropping the record here would let rejected submissions against a large
					// account rescan its whole on-disk index, under the write lock, on every
					// attempt.
					if let Some(record) = loaded_record {
						submit_index.cache_record_with_summary(account_id, record);
					}
					return SubmitResult::Rejected(reason);
				},
			};

			// Build the whole admission as one atomic commit
			let mut commit = Vec::new();
			commit.push((col::STATEMENTS, hash.to_vec(), Some(statement.encode())));
			commit.push((col::ADMISSION_SEQ, plan.seq.to_be_bytes().to_vec(), Some(hash.to_vec())));
			commit.extend(statement_index_ops(&hash, &statement, true));
			let details = EntryDetails {
				channel: statement.channel(),
				data_len: statement_len,
				admission_seq: plan.seq,
			};
			commit.extend(account_index_ops(
				&account_id,
				Expiry(statement.expiry()),
				&hash,
				Some(&details),
			));

			let mut evicted_statements = Vec::new();
			for (key, evicted_details) in &plan.evicted {
				commit.push((col::STATEMENTS, key.hash.to_vec(), None));
				commit.push((
					col::ADMISSION_SEQ,
					evicted_details.admission_seq.to_be_bytes().to_vec(),
					None,
				));
				commit.extend(account_index_ops(&account_id, key.expiry, &key.hash, None));
				match self.db.get(col::STATEMENTS, &key.hash) {
					Ok(Some(encoded)) => match Statement::decode(&mut encoded.as_slice()) {
						Ok(evicted_statement) => {
							commit.extend(statement_index_ops(
								&key.hash,
								&evicted_statement,
								false,
							));
							evicted_statements.push(evicted_statement);
						},
						Err(_) => log::error!(
							target: LOG_TARGET,
							"Corrupt statement {:?}",
							HexDisplay::from(&key.hash)
						),
					},
					Ok(None) => log::error!(
						target: LOG_TARGET,
						"Missing body of the indexed statement {:?}",
						HexDisplay::from(&key.hash)
					),
					Err(e) => log::warn!(
						target: LOG_TARGET,
						"Could not read evicted statement {:?} to clear its index: {:?}",
						HexDisplay::from(&key.hash),
						e
					),
				}
			}
			for (h, purge_at) in &plan.banned {
				commit.push((col::EXPIRED, h.to_vec(), Some((h, current_time).encode())));
				commit.push((
					col::INDEX_EVICTED,
					evicted_index_key(*purge_at, h),
					Some(INDEX_EMPTY_VALUE.to_vec()),
				));
			}
			let freed: usize = plan.evicted.iter().map(|(_, details)| details.data_len).sum();
			commit.push(SubmitIndex::counters_op(
				submit_index.statement_count + 1 - plan.evicted.len(),
				submit_index.total_size + statement_len - freed,
				plan.seq.saturating_add(1),
			));

			if let Err(e) = self.db.commit(commit) {
				log::debug!(
					target: LOG_TARGET,
					"Statement validation failed: database error {}, {:?}",
					e,
					statement
				);
				self.metrics.report(|metrics| {
					metrics.internal_errors.with_label_values(&["db_commit"]).inc();
				});
				return SubmitResult::InternalError(Error::Db(e.to_string()));
			}
			let seq = plan.seq;
			submit_index.apply_insert(&account_id, loaded_record, hash, &statement, &plan);
			// The query-index bookkeeping is applied under the same lock that ordered the
			// commit: a concurrent removal racing a resubmission of the same statement can then
			// never apply its stale update on top of this newer one (#12624). The notification
			// stays outside — its ordering is protected by the sequence watermark, not the lock.
			{
				let mut query_index = self.query_index.write();
				for evicted_statement in &evicted_statements {
					query_index.note_remove(&evicted_statement.hash(), evicted_statement);
				}
				query_index.note_insert(hash, &statement, plan.seq);
			}
			seq
		}; // Release submit index lock
		self.subscription_manager.notify(seq, statement);
		self.metrics.report(|metrics| metrics.submitted_statements.inc());
		log::trace!(target: LOG_TARGET, "Statement submitted: {:?}", HexDisplay::from(&hash));
		SubmitResult::New
	}

	/// Soft-delete a statement by hash: drop it and its index rows from the database, and record
	/// it in the `EXPIRED` column so it cannot be re-accepted until its purge period elapses (see
	/// [`maintain`](Self::maintain)). No-op if the statement is unknown.
	fn remove(&self, hash: &Hash) -> Result<()> {
		self.remove_statement(hash).map(|_| ())
	}

	/// Remove every statement authored by `who`, applying the same soft-delete as
	/// [`remove`](Self::remove) to each, in a single atomic commit.
	fn remove_by(&self, who: [u8; 32]) -> Result<()> {
		let current_time = self.timestamp();
		{
			let mut submit_index = self.submit_index.write();
			// The account's statements, from the cached record when there is one, else from the
			// on-disk index.
			let entries: Vec<(PriorityKey, EntryDetails)> =
				match submit_index.account_statements.peek(&who) {
					Some(record) => {
						record.by_priority.iter().map(|(key, details)| (*key, *details)).collect()
					},
					None => self.load_account_entries(&who)?,
				};
			if entries.is_empty() {
				return Ok(());
			}

			let mut commit = Vec::new();
			let mut removed_statements = Vec::new();
			let mut banned_count = 0usize;
			let mut freed_size = 0usize;
			for (key, details) in &entries {
				commit.push((col::STATEMENTS, key.hash.to_vec(), None));
				commit.push((
					col::ADMISSION_SEQ,
					details.admission_seq.to_be_bytes().to_vec(),
					None,
				));
				commit.extend(account_index_ops(&who, key.expiry, &key.hash, None));
				if current_time < key.expiry.get_expiration_timestamp_secs() {
					let purge_at = key
						.expiry
						.get_expiration_timestamp_secs()
						.min(current_time.saturating_add(submit_index.config.purge_after_sec));
					commit.push((
						col::EXPIRED,
						key.hash.to_vec(),
						Some((key.hash, current_time).encode()),
					));
					commit.push((
						col::INDEX_EVICTED,
						evicted_index_key(purge_at, &key.hash),
						Some(INDEX_EMPTY_VALUE.to_vec()),
					));
					banned_count += 1;
				}
				freed_size += details.data_len;
				match self.db.get(col::STATEMENTS, &key.hash) {
					Ok(Some(encoded)) => match Statement::decode(&mut encoded.as_slice()) {
						Ok(statement) => {
							commit.extend(statement_index_ops(&key.hash, &statement, false));
							removed_statements.push((key.hash, statement));
						},
						Err(_) => log::error!(
							target: LOG_TARGET,
							"Corrupt statement {:?}",
							HexDisplay::from(&key.hash)
						),
					},
					Ok(None) => log::error!(
						target: LOG_TARGET,
						"Missing body of the indexed statement {:?}",
						HexDisplay::from(&key.hash)
					),
					Err(e) => {
						log::warn!(
							target: LOG_TARGET,
							"Could not read statement {:?} to clear its index: {:?}",
							HexDisplay::from(&key.hash),
							e
						);
					},
				}
			}
			commit.push(SubmitIndex::counters_op(
				submit_index.statement_count.saturating_sub(entries.len()),
				submit_index.total_size.saturating_sub(freed_size),
				submit_index.next_seq,
			));
			self.db.commit(commit).map_err(|e| {
				log::debug!(
					target: LOG_TARGET,
					"Error removing statement: database error {}, remove by {:?}",
					e,
					HexDisplay::from(&who),
				);

				Error::Db(e.to_string())
			})?;
			submit_index.apply_account_removal(&who, entries.len(), freed_size, banned_count);
			// Applied under the same lock that ordered the commit (#12624).
			let mut query_index = self.query_index.write();
			for (hash, statement) in &removed_statements {
				query_index.note_remove(hash, statement);
			}
		}
		Ok(())
	}
}

/// RAII guard that deregisters a subscription snapshot scan (see [`SubmitIndex::begin_scan`] /
/// [`SubmitIndex::end_scan`]) when dropped, so the `recent_seqs` window is always released — on the
/// happy path, on an early `?` return, or on a panic during the snapshot.
struct ScanGuard<'a> {
	store: &'a Store,
	watermark: u64,
}

impl Drop for ScanGuard<'_> {
	fn drop(&mut self) {
		self.store.submit_index.write().end_scan(self.watermark);
	}
}

impl StatementStoreSubscriptionApi for Store {
	fn subscribe_statement(
		&self,
		topic_filter: OptimizedTopicFilter,
	) -> Result<(Vec<Vec<u8>>, async_channel::Sender<StatementEvent>, SubscriptionStatementsStream)>
	{
		// Avoid overlap between the subscribe-time snapshot and live delivery using a
		// sequence-number watermark. Under the submit-index write lock, atomically with respect
		// to sequence assignment, capture the current boundary `W` and enqueue the subscription
		// registration tagged with `W`.
		let (subscription_sender, subscription_stream, watermark) = {
			let mut submit_index = self.submit_index.write();
			let watermark = submit_index.begin_scan();
			let (sender, stream) =
				self.subscription_manager.subscribe(topic_filter.clone(), watermark);
			(sender, stream, watermark)
		};
		let _scan_guard = ScanGuard { store: self, watermark };

		let mut hashes = HashSet::new();
		self.iterate_with(None, &topic_filter, |hash| {
			hashes.insert(*hash);
			Ok(())
		})?;

		let hashes: Vec<Hash> = {
			let submit_index = self.submit_index.read();
			hashes
				.into_iter()
				.filter(|hash| submit_index.seq_covered_by_snapshot(hash, watermark))
				.collect()
		};

		let mut existing_statements = Vec::with_capacity(hashes.len());
		for hash in hashes {
			if let Some(entry) = self.read_statement_encoded(&hash)? {
				existing_statements.push(entry);
			}
		}

		if existing_statements.is_empty() {
			subscription_sender
				.send_blocking(StatementEvent::NewStatements {
					statements: vec![],
					remaining: Some(0),
				})
				.ok();
		}
		Ok((existing_statements, subscription_sender, subscription_stream))
	}
}

impl Store {
	/// Body of [`StatementStore::remove`], reporting whether a statement was actually removed.
	///
	/// `Ok(false)` means no (decodable) statement is stored under `hash` — it was already gone,
	/// or its body is corrupt. A corrupt body cannot be tied back to its index rows, so nothing
	/// is removed at all.
	fn remove_statement(&self, hash: &Hash) -> Result<bool> {
		let current_time = self.timestamp();
		{
			let mut submit_index = self.submit_index.write();
			// The body is read under the submit-index lock, so it cannot change under our feet
			let Some(encoded) =
				self.db.get(col::STATEMENTS, hash).map_err(|e| Error::Db(e.to_string()))?
			else {
				return Ok(false);
			};
			let Some((statement, account)) = Statement::decode(&mut encoded.as_slice())
				.ok()
				.and_then(|statement| statement.account_id().map(|account| (statement, account)))
			else {
				// A corrupt body cannot be tied back to its index rows
				log::error!(target: LOG_TARGET, "Corrupt statement {:?}", HexDisplay::from(hash));
				return Ok(false);
			};
			let expiry = Expiry(statement.expiry());
			let account_key = account_index_key(&account, expiry, hash);
			let details = self
				.db
				.get(col::INDEX_BY_ACCOUNT, &account_key)
				.map_err(|e| Error::Db(e.to_string()))?
				.as_deref()
				.and_then(|mut value| EntryDetails::decode(&mut value).ok());

			let mut commit = vec![(col::STATEMENTS, hash.to_vec(), None)];
			commit.extend(statement_index_ops(hash, &statement, false));
			commit.extend(account_index_ops(&account, expiry, hash, None));
			match &details {
				Some(details) => commit.push((
					col::ADMISSION_SEQ,
					details.admission_seq.to_be_bytes().to_vec(),
					None,
				)),
				// Nothing points at the admission entry any more; replay skips it once the body
				// is gone.
				None => log::error!(
					target: LOG_TARGET,
					"Missing or corrupt account index entry for statement {:?}",
					HexDisplay::from(hash)
				),
			}
			let banned = current_time < expiry.get_expiration_timestamp_secs();
			if banned {
				let purge_at = expiry
					.get_expiration_timestamp_secs()
					.min(current_time.saturating_add(submit_index.config.purge_after_sec));
				commit.push((col::EXPIRED, hash.to_vec(), Some((hash, current_time).encode())));
				commit.push((
					col::INDEX_EVICTED,
					evicted_index_key(purge_at, hash),
					Some(INDEX_EMPTY_VALUE.to_vec()),
				));
			}
			let data_len = details
				.as_ref()
				.map_or_else(|| statement.data_len(), |details| details.data_len);
			commit.push(SubmitIndex::counters_op(
				submit_index.statement_count.saturating_sub(1),
				submit_index.total_size.saturating_sub(data_len),
				submit_index.next_seq,
			));
			if let Err(e) = self.db.commit(commit) {
				log::debug!(
					target: LOG_TARGET,
					"Error removing statement: database error {}, {:?}",
					e,
					HexDisplay::from(hash),
				);
				return Err(Error::Db(e.to_string()));
			}
			submit_index.apply_removal(
				&account,
				&PriorityKey { hash: *hash, expiry },
				data_len,
				banned,
			);
			self.query_index.write().note_remove(hash, &statement);
			log::trace!(target: LOG_TARGET, "Expired statement {:?}", HexDisplay::from(hash));
		}
		Ok(true)
	}

	/// Reports an [`col::INDEX_BY_EXPIRY`] row that survived its statement,
	/// re-checking under the submit lock — every commit happens under it — that the row is indeed
	/// orphaned: a concurrent removal deletes the row along with its statement, and a concurrent
	/// re-admission of the same statement recreates the same content-derived key together with a
	/// body.
	fn report_orphan_expiry_row(&self, expiry: u64, hash: &Hash) {
		let _submit_index = self.submit_index.write();
		let body = self.db.get_size(col::STATEMENTS, hash.as_slice());
		let row = self.db.get(col::INDEX_BY_EXPIRY, &expiry_index_key(Expiry(expiry), hash));
		match (body, row) {
			(Ok(None), Ok(Some(_))) => log::error!(
				target: LOG_TARGET,
				"Orphan expiry index row for statement {:?}",
				HexDisplay::from(hash)
			),
			(Err(e), _) | (_, Err(e)) => {
				log::debug!(target: LOG_TARGET, "Error checking statement presence: {:?}", e)
			},
			_ => {},
		}
	}

	fn register_replay(&self, enqueue: &mut dyn FnMut(u64) -> bool) -> Result<Option<u64>> {
		let registered = {
			let submit_index = self.submit_index.write();
			let watermark = submit_index.next_seq;
			enqueue(watermark).then_some(watermark)
		}; // Release submit index lock
		Ok(registered)
	}

	fn replay_batch(
		&self,
		filter: &OptimizedTopicFilter,
		cursor: u64,
		watermark: u64,
	) -> Result<ReplayBatch> {
		let mut statements = Vec::new();
		let mut chunk_bytes = 0usize;
		// Replay batches are pulled by a local subscription task, not a remote peer, so the
		// walk is not budgeted.
		let batch = StatementStore::admitted_statements(
			self,
			cursor,
			watermark,
			usize::MAX,
			&mut |_, encoded, statement| {
				if !filter.matches(statement) {
					return FilterDecision::Skip;
				}
				if !statements.is_empty() && chunk_bytes + encoded.len() > REPLAY_CHUNK_RAW_BYTES {
					return FilterDecision::Abort;
				}
				chunk_bytes += encoded.len();
				statements.push(encoded.to_vec());
				// The encoded bytes are already captured above, so the statement is reported
				// as skipped to keep the walk from cloning it into the returned batch too.
				FilterDecision::Skip
			},
		)?;
		Ok(ReplayBatch { statements, cursor: batch.cursor, done: batch.done })
	}
}

impl MultiFilterSubscriptionApi for Arc<Store> {
	fn create_subscription(&self) -> (SubscriptionHandle, MultiFilterEventStream) {
		let inner =
			Arc::new(parking_lot::Mutex::new(crate::subscription::SubscriptionHandleInner::new()));
		let snapshot_provider: Arc<dyn ReplaySnapshotProvider> = Arc::new(Arc::downgrade(self));
		let (sub_id, stream) = self.subscription_manager.subscribe_empty(snapshot_provider.clone());

		let handle = SubscriptionHandle {
			sub_id,
			inner,
			matchers: self.subscription_manager.matchers(),
			snapshot_provider,
		};
		(handle, stream)
	}
}

#[cfg(test)]
impl Store {
	/// Number of hashes currently in the on-disk evicted journal (per the in-memory counter).
	fn evicted_count(&self) -> usize {
		self.submit_index.read().evicted_count
	}

	/// Whether `hash` is currently banned from re-acceptance (present in the evicted journal).
	fn is_evicted(&self, hash: &Hash) -> bool {
		self.db.get_size(col::EXPIRED, hash.as_slice()).ok().flatten().is_some()
	}

	/// Whether the on-disk topic index links `topic` to `hash`.
	fn index_has_topic(&self, topic: &Topic, hash: &Hash) -> bool {
		self.index_set_contains(&IndexSet::Topic(*topic), hash).unwrap_or(false)
	}

	/// Whether the on-disk decryption-key index links `key` to `hash`.
	fn index_has_dec_key(&self, key: &Option<DecryptionKey>, hash: &Hash) -> bool {
		self.index_set_contains(&IndexSet::DecKey(*key), hash).unwrap_or(false)
	}

	/// Number of stored statements, per the in-memory counter.
	fn statement_count(&self) -> usize {
		self.submit_index.read().statement_count
	}

	/// Whether the details cache currently holds `who`'s record.
	fn details_cached(&self, who: &AccountId) -> bool {
		self.submit_index.read().account_statements.peek(who).is_some()
	}

	/// Whether the summary cache currently holds `who`'s entry.
	fn summary_cached(&self, who: &AccountId) -> bool {
		self.submit_index.read().summaries.peek(who).is_some()
	}

	/// Total stored data size, per the in-memory counter.
	fn total_size(&self) -> usize {
		self.submit_index.read().total_size
	}

	/// Whether `who` has at least one statement in the on-disk account index.
	fn has_account(&self, who: &AccountId) -> bool {
		self.next_account_from(who).expect("failed to read the account index") == Some(*who)
	}

	/// Number of accounts in the on-disk account index.
	fn account_count(&self) -> usize {
		self.count_accounts().expect("failed to count the accounts")
	}

	/// Inserts `statement` bypassing signature and allowance validation — used to seed
	/// over-allowance states that `submit` would refuse to create.
	fn force_insert(&self, statement: &Statement) {
		let hash = statement.hash();
		let account = statement.account_id().expect("test statements are signed; qed");
		let mut submit_index = self.submit_index.write();
		let seq = submit_index.next_seq;
		let details = EntryDetails {
			channel: statement.channel(),
			data_len: statement.data_len(),
			admission_seq: seq,
		};
		let mut commit = vec![
			(col::STATEMENTS, hash.to_vec(), Some(statement.encode())),
			(col::ADMISSION_SEQ, seq.to_be_bytes().to_vec(), Some(hash.to_vec())),
		];
		commit.extend(statement_index_ops(&hash, statement, true));
		commit.extend(account_index_ops(
			&account,
			Expiry(statement.expiry()),
			&hash,
			Some(&details),
		));
		commit.push(SubmitIndex::counters_op(
			submit_index.statement_count + 1,
			submit_index.total_size + statement.data_len(),
			seq.saturating_add(1),
		));
		self.db.commit(commit).expect("failed to commit the statement");
		let plan = InsertPlan { seq, evicted: Vec::new(), banned: Vec::new() };
		submit_index.apply_insert(&account, None, hash, statement, &plan);
		self.query_index.write().note_insert(hash, statement, seq);
	}
}

#[cfg(test)]
mod tests {

	use crate::{col, AllowanceBlock, Store, KEY_VERSION};
	use sc_keystore::Keystore;
	use sp_core::{Decode, Encode, Pair};
	use sp_statement_store::{
		AccountId, Channel, DecryptionKey, FilterDecision, InvalidReason, OptimizedTopicFilter,
		Proof, RejectionReason, Statement, StatementAllowance, StatementSource, StatementStore,
		SubmitResult, Topic,
	};

	type Extrinsic = sp_runtime::OpaqueExtrinsic;
	type Hash = sp_core::H256;
	type Hashing = sp_runtime::traits::BlakeTwo256;
	type BlockNumber = u64;
	type Header = sp_runtime::generic::Header<BlockNumber, Hashing>;
	type Block = sp_runtime::generic::Block<Header, Extrinsic>;

	const TEST_BEST_BLOCK_HASH: [u8; 32] = [1u8; 32];

	/// Maximum seed value used by `account(seed)`/`statement(seed, ...)` in this
	/// test module. Increase if you add tests that pass larger seed values to
	/// `statement(..)`. The reverse-lookup table in `TestClient::storage` is
	/// populated lazily for seeds in `0..=MAX_TEST_ACCOUNT_SEED`.
	const MAX_TEST_ACCOUNT_SEED: u64 = 64;

	/// Reverse-lookup table from a real sr25519 public key back to the synthetic
	/// `u64` seed it was derived from. Populated once with seeds in
	/// `0..=MAX_TEST_ACCOUNT_SEED`, then consulted by `TestClient::storage` to
	/// figure out which allowance bucket to return for a given account.
	fn account_seed_table() -> &'static std::collections::BTreeMap<AccountId, u64> {
		use std::sync::OnceLock;
		static TABLE: OnceLock<std::collections::BTreeMap<AccountId, u64>> = OnceLock::new();
		TABLE.get_or_init(|| {
			let mut t = std::collections::BTreeMap::new();
			for seed in 0..=MAX_TEST_ACCOUNT_SEED {
				t.insert(account_keypair(seed).public().0, seed);
			}
			t
		})
	}

	#[derive(Clone)]
	pub(crate) struct TestClient;

	pub(crate) type TestBackend = sc_client_api::in_mem::Backend<Block>;

	impl sc_client_api::StorageProvider<Block, TestBackend> for TestClient {
		fn storage(
			&self,
			_hash: Hash,
			key: &sc_client_api::StorageKey,
		) -> sp_blockchain::Result<Option<sc_client_api::StorageData>> {
			use sp_statement_store::StatementAllowance;

			assert_eq!(&key.0[0..21], b":statement_allowance:" as &[u8],);

			// Recover the synthetic test seed from the account id. Unknown accounts
			// (e.g. //Alice for `signed_statement`) fall through to a generic default.
			let account_bytes: AccountId = key.0[21..53].try_into().unwrap();
			let seed = account_seed_table().get(&account_bytes).copied();
			let allowance = match seed {
				// Account 0 has no allowance (used to test eviction of all statements)
				Some(0) => return Ok(None),
				Some(1) => StatementAllowance::new(1, 1000),
				Some(2) => StatementAllowance::new(2, 1000),
				Some(3) => StatementAllowance::new(3, 1000),
				Some(4) => StatementAllowance::new(4, 1000),
				Some(42) => StatementAllowance::new(42, (42 * crate::MAX_STATEMENT_SIZE) as u32),
				// Depleted on one axis, but still present in state.
				Some(50) => StatementAllowance::new(0, 1000),
				Some(51) => StatementAllowance::new(5, 0),
				Some(_) | None => StatementAllowance::new(100, 1000),
			};
			Ok(Some(sc_client_api::StorageData(allowance.encode())))
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
				best_hash: TEST_BEST_BLOCK_HASH.into(),
				best_number: 0,
				genesis_hash: Default::default(),
				finalized_hash: TEST_BEST_BLOCK_HASH.into(),
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

	fn test_store() -> (Store, tempfile::TempDir) {
		sp_tracing::init_for_tests();
		let temp_dir = tempfile::Builder::new().tempdir().expect("Error creating test dir");

		let client = std::sync::Arc::new(TestClient);
		let mut path: std::path::PathBuf = temp_dir.path().into();
		path.push("db");
		let keystore = std::sync::Arc::new(sc_keystore::LocalKeystore::in_memory());
		let store = Store::new::<Block, TestClient, TestBackend>(
			&path,
			Default::default(),
			client,
			keystore,
			None,
			Box::new(sp_core::testing::TaskExecutor::new()),
		)
		.unwrap();
		(store, temp_dir) // return order is important. Store must be dropped before TempDir
	}

	pub fn signed_statement(data: u8) -> Statement {
		signed_statement_with_topics(data, &[], None)
	}

	fn signed_statement_with_topics(
		data: u8,
		topics: &[Topic],
		dec_key: Option<DecryptionKey>,
	) -> Statement {
		let mut statement = Statement::new();
		statement.set_plain_data(vec![data]);
		statement.set_expiry(u64::MAX);

		for i in 0..topics.len() {
			statement.set_topic(i, topics[i]);
		}
		if let Some(key) = dec_key {
			statement.set_decryption_key(key);
		}
		let kp = sp_core::ed25519::Pair::from_string("//Alice", None).unwrap();
		statement.sign_ed25519_private(&kp);
		statement
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

	/// Returns the deterministic ed25519 keypair used to author statements for the
	/// synthetic test account `seed`.
	///
	/// Uses ed25519 rather than sr25519 because schnorrkel signing is non-deterministic
	/// (the signature depends on RNG state), so calling `statement(id, prio, ch, len)`
	/// twice would produce different hashes. Several tests compare statement hashes
	/// against pre-computed values; ed25519 keeps those comparisons stable.
	fn account_keypair(seed: u64) -> sp_core::ed25519::Pair {
		sp_core::ed25519::Pair::from_string(&format!("//StatementAccount{seed}"), None)
			.expect("Derivation path is valid; qed")
	}

	fn account(id: u64) -> AccountId {
		account_keypair(id).public().0
	}

	/// Signs `stmt` with `account_id`'s test keypair. Tests that build a statement via
	/// `unsigned_statement(..)` and then mutate it call this exactly once at the end.
	fn sign_with(stmt: &mut Statement, account_id: u64) {
		stmt.sign_ed25519_private(&account_keypair(account_id));
	}

	fn channel(id: u64) -> Channel {
		let mut channel: Channel = Default::default();
		channel[0..8].copy_from_slice(&id.to_le_bytes());
		channel
	}

	/// Builds a test statement without signing it. Use this when a test needs to mutate
	/// the statement (encryption, expiry change, topic update, etc.) before submission —
	/// call `sign_with(&mut stmt, account_id)` once after all mutations.
	fn unsigned_statement(
		account_id: u64,
		priority: u32,
		c: Option<u64>,
		data_len: usize,
	) -> Statement {
		assert!(
			account_id <= MAX_TEST_ACCOUNT_SEED,
			"account_id {account_id} exceeds MAX_TEST_ACCOUNT_SEED ({MAX_TEST_ACCOUNT_SEED}); \
			 raise the constant if you need a wider range",
		);
		let mut statement = Statement::new();
		let mut data = Vec::new();
		data.resize(data_len, 0);
		statement.set_plain_data(data);
		statement.set_expiry_from_parts(u32::MAX, priority);
		if let Some(c) = c {
			statement.set_channel(channel(c));
		}
		statement
	}

	fn statement(account_id: u64, priority: u32, c: Option<u64>, data_len: usize) -> Statement {
		let mut statement = unsigned_statement(account_id, priority, c, data_len);
		sign_with(&mut statement, account_id);
		statement
	}

	#[test]
	fn submit_one() {
		let (store, _temp) = test_store();
		let statement0 = signed_statement(0);
		assert_eq!(store.submit(statement0, StatementSource::Network), SubmitResult::New);
		let statement1 = statement(1, 1, None, 0);
		assert_eq!(store.submit(statement1, StatementSource::Network), SubmitResult::New);
	}

	#[test]
	fn depleted_count_allowance_admits_nothing() {
		let (store, _temp) = test_store();
		// Account 50 has `max_count: 0`.
		let result = store.submit(statement(50, 1, None, 100), StatementSource::Network);
		assert_eq!(result, SubmitResult::Rejected(RejectionReason::NoAllowance));
		assert_eq!(store.statement_count(), 0);
		assert_eq!(store.total_size(), 0);
	}

	#[test]
	fn depleted_size_allowance_admits_nothing() {
		let (store, _temp) = test_store();
		// Account 51 has `max_size: 0`; a zero-length statement passes the length gate.
		let result = store.submit(statement(51, 1, None, 0), StatementSource::Network);
		assert_eq!(result, SubmitResult::Rejected(RejectionReason::NoAllowance));
		assert_eq!(store.statement_count(), 0);
		assert_eq!(store.total_size(), 0);
	}

	#[test]
	fn save_and_load_statements() {
		let (store, temp) = test_store();
		let statement0 = signed_statement(0);
		let statement1 = signed_statement(1);
		let statement2 = signed_statement(2);
		assert_eq!(store.submit(statement0.clone(), StatementSource::Network), SubmitResult::New);
		assert_eq!(store.submit(statement1.clone(), StatementSource::Network), SubmitResult::New);
		assert_eq!(store.submit(statement2.clone(), StatementSource::Network), SubmitResult::New);
		assert_eq!(store.statements().unwrap().len(), 3);
		assert_eq!(store.broadcasts(&[]).unwrap().len(), 3);
		assert_eq!(store.statement(&statement1.hash()).unwrap(), Some(statement1.clone()));
		let keystore = store.keystore.clone();
		drop(store);

		let client = std::sync::Arc::new(TestClient);
		let mut path: std::path::PathBuf = temp.path().into();
		path.push("db");
		let store = Store::new::<Block, TestClient, TestBackend>(
			&path,
			Default::default(),
			client,
			keystore,
			None,
			Box::new(sp_core::testing::TaskExecutor::new()),
		)
		.unwrap();
		assert_eq!(store.statements().unwrap().len(), 3);
		assert_eq!(store.broadcasts(&[]).unwrap().len(), 3);
		assert_eq!(store.statement(&statement1.hash()).unwrap(), Some(statement1));
	}

	#[test]
	fn admission_seq_is_monotonic_across_restarts() {
		// Sequence numbers must never be reused, even when the highest-numbered statements were
		// removed before a restart: a replay cursor as high as a dead sequence number would
		// silently skip any statement re-admitted under it.
		let (store, temp) = test_store();
		let first = signed_statement(10);
		let second = signed_statement(11);
		assert_eq!(store.submit(first.clone(), StatementSource::Network), SubmitResult::New);
		assert_eq!(store.submit(second.clone(), StatementSource::Network), SubmitResult::New);
		assert_eq!(
			store.db.get(col::ADMISSION_SEQ, &0u64.to_be_bytes()).unwrap(),
			Some(first.hash().to_vec())
		);
		assert_eq!(
			store.db.get(col::ADMISSION_SEQ, &1u64.to_be_bytes()).unwrap(),
			Some(second.hash().to_vec())
		);
		store.remove(&second.hash()).unwrap();
		assert_eq!(
			store.db.get(col::ADMISSION_SEQ, &0u64.to_be_bytes()).unwrap(),
			Some(first.hash().to_vec())
		);
		assert_eq!(store.db.get(col::ADMISSION_SEQ, &1u64.to_be_bytes()).unwrap(), None);
		let keystore = store.keystore.clone();
		drop(store);

		let mut path: std::path::PathBuf = temp.path().into();
		path.push("db");
		let store = Store::new::<Block, TestClient, TestBackend>(
			&path,
			Default::default(),
			std::sync::Arc::new(TestClient),
			keystore,
			None,
			Box::new(sp_core::testing::TaskExecutor::new()),
		)
		.unwrap();
		assert_eq!(store.submit_index.read().next_seq, 2);

		let replay = store.replay_batch(&OptimizedTopicFilter::Any, 0, 2).unwrap();
		assert_eq!(replay.statements, vec![first.encode()]);
		assert!(replay.done);

		// The removed statement's sequence number stays dead; the next admission claims a fresh
		// one.
		let third = signed_statement(12);
		assert_eq!(store.submit(third.clone(), StatementSource::Network), SubmitResult::New);
		assert_eq!(store.db.get(col::ADMISSION_SEQ, &1u64.to_be_bytes()).unwrap(), None);
		assert_eq!(
			store.db.get(col::ADMISSION_SEQ, &2u64.to_be_bytes()).unwrap(),
			Some(third.hash().to_vec())
		);
	}

	#[test]
	fn lagging_counters_row_does_not_reuse_admission_seqs() {
		// A counters row lagging the admission journal is impossible on a database written by
		// this version (they are committed atomically); should one appear anyway, startup must
		// recover `next_seq` from the journal instead of reusing live sequence numbers.
		let (store, temp) = test_store();
		let first = signed_statement(1);
		let second = signed_statement(2);
		assert_eq!(store.submit(first.clone(), StatementSource::Network), SubmitResult::New);
		assert_eq!(store.submit(second.clone(), StatementSource::Network), SubmitResult::New);
		// Corrupt the counters row: keep the totals, rewind the sequence counter.
		let (statement_count, total_size, _) = <(u64, u64, u64)>::decode(
			&mut store.db.get(col::META, crate::KEY_COUNTERS).unwrap().unwrap().as_slice(),
		)
		.unwrap();
		store
			.db
			.commit([(
				col::META,
				crate::KEY_COUNTERS.to_vec(),
				Some((statement_count, total_size, 0u64).encode()),
			)])
			.unwrap();
		let keystore = store.keystore.clone();
		drop(store);

		let mut path: std::path::PathBuf = temp.path().into();
		path.push("db");
		let store = Store::new::<Block, TestClient, TestBackend>(
			&path,
			Default::default(),
			std::sync::Arc::new(TestClient),
			keystore,
			None,
			Box::new(sp_core::testing::TaskExecutor::new()),
		)
		.unwrap();
		assert_eq!(store.submit_index.read().next_seq, 2);

		// A new admission claims a fresh sequence number; the existing entries are untouched.
		let third = signed_statement(3);
		assert_eq!(store.submit(third.clone(), StatementSource::Network), SubmitResult::New);
		assert_eq!(
			store.db.get(col::ADMISSION_SEQ, &0u64.to_be_bytes()).unwrap(),
			Some(first.hash().to_vec())
		);
		assert_eq!(
			store.db.get(col::ADMISSION_SEQ, &1u64.to_be_bytes()).unwrap(),
			Some(second.hash().to_vec())
		);
		assert_eq!(
			store.db.get(col::ADMISSION_SEQ, &2u64.to_be_bytes()).unwrap(),
			Some(third.hash().to_vec())
		);
	}

	#[test]
	fn statement_without_admission_entry_is_restored_by_migration() {
		// Admission entries are committed atomically with statement bodies, so a
		// current-version database cannot lose one short of external tampering. Statements
		// missing an admission entry (a database written before the entries existed) get one
		// from the version migration, which rebuilds every derived column from the bodies.
		let (store, temp) = test_store();
		let statement = signed_statement_with_topics(20, &[topic(1)], None);
		assert_eq!(store.submit(statement.clone(), StatementSource::Network), SubmitResult::New);
		// Drop the admission entry, leaving the body behind, and rewind the database version so
		// the next start migrates.
		store
			.db
			.commit([
				(col::ADMISSION_SEQ, 0u64.to_be_bytes().to_vec(), None),
				(col::META, KEY_VERSION.to_vec(), Some(1u32.to_le_bytes().to_vec())),
			])
			.unwrap();
		let filter = OptimizedTopicFilter::MatchAny([topic(1)].into_iter().collect());
		assert!(store.replay_batch(&filter, 0, 1).unwrap().statements.is_empty());
		let keystore = store.keystore.clone();
		drop(store);

		let mut path: std::path::PathBuf = temp.path().into();
		path.push("db");
		let store = Store::new::<Block, TestClient, TestBackend>(
			&path,
			Default::default(),
			std::sync::Arc::new(TestClient),
			keystore,
			None,
			Box::new(sp_core::testing::TaskExecutor::new()),
		)
		.unwrap();

		let watermark = store.submit_index.read().next_seq;
		assert_eq!(watermark, 1);
		assert_eq!(
			store.replay_batch(&filter, 0, watermark).unwrap().statements,
			vec![statement.encode()]
		);
	}

	#[test]
	fn admission_cursor_resumes_from_the_middle_of_the_range() {
		let (store, _temp) = test_store();
		let first = signed_statement(1);
		let second = signed_statement(2);
		let third = signed_statement(3);
		for statement in [&first, &second, &third] {
			assert_eq!(
				store.submit(statement.clone(), StatementSource::Network),
				SubmitResult::New
			);
		}
		let batch = store.replay_batch(&OptimizedTopicFilter::Any, 1, 3).unwrap();
		assert_eq!(batch.statements, vec![second.encode(), third.encode()]);
		assert_eq!(batch.cursor, 3);
		assert!(batch.done);
	}

	#[test]
	fn admission_cursor_replays_only_current_matching_admissions() {
		let (store, _temp) = test_store();
		let matching = signed_statement_with_topics(20, &[topic(1)], None);
		let non_matching = signed_statement_with_topics(21, &[topic(2)], None);
		let removed = signed_statement_with_topics(22, &[topic(1)], None);
		assert_eq!(store.submit(matching.clone(), StatementSource::Network), SubmitResult::New);
		assert_eq!(store.submit(non_matching, StatementSource::Network), SubmitResult::New);
		assert_eq!(store.submit(removed.clone(), StatementSource::Network), SubmitResult::New);
		store.remove(&removed.hash()).unwrap();

		let replay = store
			.replay_batch(&OptimizedTopicFilter::MatchAny([topic(1)].into_iter().collect()), 0, 3)
			.unwrap();
		assert_eq!(replay.statements, vec![matching.encode()]);
		assert_eq!(replay.cursor, 3);
		assert!(replay.done);
	}

	#[test]
	fn admission_watermark_is_one_past_the_newest_admission() {
		let (store, _temp) = test_store();
		assert_eq!(store.admission_watermark().unwrap(), 0);

		let first = signed_statement(1);
		let second = signed_statement(2);
		assert_eq!(store.submit(first.clone(), StatementSource::Network), SubmitResult::New);
		assert_eq!(store.submit(second, StatementSource::Network), SubmitResult::New);
		assert_eq!(store.admission_watermark().unwrap(), 2);

		// Sequence numbers are never reused, so a removal leaves the boundary in place.
		store.remove(&first.hash()).unwrap();
		assert_eq!(store.admission_watermark().unwrap(), 2);
	}

	#[test]
	fn admitted_statements_walk_applies_filter_decisions_and_resumes() {
		let (store, _temp) = test_store();
		let statements: Vec<_> = (1..=4).map(signed_statement).collect();
		for statement in &statements {
			assert_eq!(
				store.submit(statement.clone(), StatementSource::Network),
				SubmitResult::New
			);
		}
		// Seq 1 goes dead, the walk must pass over it.
		store.remove(&statements[1].hash()).unwrap();

		let batch = store
			.admitted_statements(0, 4, usize::MAX, &mut |hash, _encoded, _statement| {
				if *hash == statements[0].hash() {
					FilterDecision::Skip
				} else if *hash == statements[3].hash() {
					FilterDecision::Abort
				} else {
					FilterDecision::Take
				}
			})
			.unwrap();
		assert_eq!(batch.statements, vec![(statements[2].hash(), statements[2].clone())]);
		// The aborted statement sits at seq 3; the cursor points back at it.
		assert_eq!(batch.cursor, 3);
		assert!(!batch.done);

		// Resuming from the cursor visits the aborted statement first.
		let batch = store
			.admitted_statements(batch.cursor, 4, usize::MAX, &mut |_, _, _| FilterDecision::Take)
			.unwrap();
		assert_eq!(batch.statements, vec![(statements[3].hash(), statements[3].clone())]);
		assert_eq!(batch.cursor, 4);
		assert!(batch.done);
	}

	#[test]
	fn admitted_statements_walk_stops_at_the_scan_limit() {
		let (store, _temp) = test_store();
		let statements: Vec<_> = (1..=4).map(signed_statement).collect();
		for statement in &statements {
			assert_eq!(
				store.submit(statement.clone(), StatementSource::Network),
				SubmitResult::New
			);
		}

		// A filter that takes nothing must still visit no more than `scan_limit` entries.
		let mut visited = 0;
		let batch = store
			.admitted_statements(0, 4, 3, &mut |_, _, _| {
				visited += 1;
				FilterDecision::Skip
			})
			.unwrap();
		assert!(batch.statements.is_empty());
		assert_eq!(visited, 3);
		assert_eq!(batch.cursor, 3, "the cursor must sit after the last visited entry");
		assert!(!batch.done);

		// Resuming from the partial cursor completes the walk.
		let batch = store
			.admitted_statements(batch.cursor, 4, 3, &mut |_, _, _| FilterDecision::Skip)
			.unwrap();
		assert!(batch.statements.is_empty());
		assert_eq!(batch.cursor, 4);
		assert!(batch.done);
	}

	#[test]
	fn take_recent_statements_are_ordered_by_admission() {
		let (store, _temp) = test_store();
		let statements: Vec<_> = (1..=3).map(signed_statement).collect();
		for statement in &statements {
			assert_eq!(
				store.submit(statement.clone(), StatementSource::Network),
				SubmitResult::New
			);
		}

		let recent = store.take_recent_statements().unwrap();
		let expected: Vec<_> = statements
			.into_iter()
			.enumerate()
			.map(|(seq, statement)| (seq as u64, statement.hash(), statement))
			.collect();
		assert_eq!(recent, expected);
	}

	#[test]
	fn migrates_v1_database_to_on_disk_index() {
		sp_tracing::init_for_tests();
		let temp = tempfile::Builder::new().tempdir().expect("Error creating test dir");
		let mut path: std::path::PathBuf = temp.path().into();
		path.push("db");
		// The store appends `statements` to the path it is given.
		let mut db_path = path.clone();
		db_path.push("statements");

		// One addressed statement (topics 1 & 2, decryption key 9) and one broadcast (topic 1, no
		// key).
		let addressed = signed_statement_with_topics(1, &[topic(1), topic(2)], Some(dec_key(9)));
		let broadcast = signed_statement_with_topics(2, &[topic(1)], None);
		let h_addressed = addressed.hash();
		let h_broadcast = broadcast.hash();

		// A hash seeded into the legacy EXPIRED column, with a deadline far in the future so it
		// survives the maintenance pass triggered during migration.
		let expired_hash = topic(999);
		let expired_ts = 10_000_000_000u64;

		// Build a version-1 database by hand: three columns and no on-disk read index.
		{
			let mut cfg = parity_db::Options::with_columns(&db_path, 3);
			let statement_col = &mut cfg.columns[1];
			statement_col.ref_counted = false;
			statement_col.preimage = true;
			statement_col.uniform = true;
			let db = parity_db::Db::open_or_create(&cfg).unwrap();
			db.commit([
				(0u8, b"version".to_vec(), Some(1u32.to_le_bytes().to_vec())),
				(1u8, h_addressed.to_vec(), Some(addressed.encode())),
				(1u8, h_broadcast.to_vec(), Some(broadcast.encode())),
				(2u8, expired_hash.to_vec(), Some((expired_hash, expired_ts).encode())),
			])
			.unwrap();
		}

		let open = |path: &std::path::Path| {
			Store::new::<Block, TestClient, TestBackend>(
				path,
				Default::default(),
				std::sync::Arc::new(TestClient),
				std::sync::Arc::new(sc_keystore::LocalKeystore::in_memory()),
				None,
				Box::new(sp_core::testing::TaskExecutor::new()),
			)
			.unwrap()
		};

		// Re-open through the store: it must add the index columns, rebuild them from the bodies,
		// rebuild the evicted journal and admission sequence, and bump the database version.
		let store = open(&path);

		// Bodies survived.
		assert_eq!(store.statements().unwrap().len(), 2);
		assert!(store.statement(&h_addressed).unwrap().is_some());
		assert!(store.statement(&h_broadcast).unwrap().is_some());

		// The read index was rebuilt on disk.
		assert!(store.index_has_topic(&topic(1), &h_addressed));
		assert!(store.index_has_topic(&topic(1), &h_broadcast));
		assert!(store.index_has_topic(&topic(2), &h_addressed));
		assert!(store.index_has_dec_key(&Some(dec_key(9)), &h_addressed));
		assert!(store.index_has_dec_key(&None, &h_broadcast));
		let mut admitted = std::collections::HashSet::new();
		let mut iter = store.db.iter(col::ADMISSION_SEQ).unwrap();
		iter.seek_to_first().unwrap();
		while let Some((_key, value)) = iter.next().unwrap() {
			admitted.insert(value.as_slice().try_into().expect("admission hash is 32 bytes"));
		}
		assert_eq!(admitted, std::collections::HashSet::from([h_addressed, h_broadcast]));
		assert_eq!(store.submit_index.read().next_seq, 2);

		// And it answers queries: only the broadcast matches topic 1 with no key, only the
		// addressed statement matches topic 1 for key 9.
		assert_eq!(store.broadcasts(&[topic(1)]).unwrap().len(), 1);
		assert_eq!(store.posted(&[topic(1)], dec_key(9)).unwrap().len(), 1);

		// The evicted journal was rebuilt from the legacy EXPIRED column.
		assert!(store.is_evicted(&expired_hash));
		assert_eq!(store.evicted_count(), 1);

		// The account and expiry indexes were rebuilt too, along with the counters row.
		assert_eq!(store.statement_count(), 2);
		assert_eq!(store.total_size(), 2);
		assert!(store.has_account(&addressed.account_id().unwrap()));
		assert_eq!(store.account_count(), 1);

		// The database is now at the current version; re-opening does not migrate again.
		drop(store);
		let store = open(&path);
		assert_eq!(store.statements().unwrap().len(), 2);
		assert!(store.index_has_topic(&topic(1), &h_broadcast));
		assert!(store.is_evicted(&expired_hash));
	}

	#[test]
	fn interrupted_migration_resumes_preserving_admission_entries() {
		// The migration commits in chunks and bumps the version only at the very end, so a crash
		// mid-way leaves a version-1 database with some derived rows already written. Re-running
		// the migration must preserve the admission entries it already assigned — replay cursors
		// depend on them — and rebuild everything else.
		sp_tracing::init_for_tests();
		let temp = tempfile::Builder::new().tempdir().expect("Error creating test dir");
		let mut path: std::path::PathBuf = temp.path().into();
		path.push("db");
		let mut db_path = path.clone();
		db_path.push("statements");

		let s1 = signed_statement_with_topics(1, &[topic(1)], None);
		let s2 = statement(2, 5, Some(9), 200);

		// A version-1 database whose columns were already extended and whose first migration
		// attempt committed s1's read-index and admission rows before crashing: the version was
		// never bumped.
		{
			let mut cfg = parity_db::Options::with_columns(&db_path, col::COUNT);
			let statement_col = &mut cfg.columns[col::STATEMENTS as usize];
			statement_col.ref_counted = false;
			statement_col.preimage = true;
			statement_col.uniform = true;
			for c in crate::BTREE_COLUMNS {
				cfg.columns[c as usize].btree_index = true;
			}
			let db = parity_db::Db::open_or_create(&cfg).unwrap();
			let mut commit: Vec<(u8, Vec<u8>, Option<Vec<u8>>)> = vec![
				(col::META, b"version".to_vec(), Some(1u32.to_le_bytes().to_vec())),
				(col::STATEMENTS, s1.hash().to_vec(), Some(s1.encode())),
				(col::STATEMENTS, s2.hash().to_vec(), Some(s2.encode())),
				(col::ADMISSION_SEQ, 7u64.to_be_bytes().to_vec(), Some(s1.hash().to_vec())),
			];
			commit.extend(crate::statement_index_ops(&s1.hash(), &s1, true));
			db.commit(commit).unwrap();
		}

		let store = Store::new::<Block, TestClient, TestBackend>(
			&path,
			Default::default(),
			std::sync::Arc::new(TestClient),
			std::sync::Arc::new(sc_keystore::LocalKeystore::in_memory()),
			None,
			Box::new(sp_core::testing::TaskExecutor::new()),
		)
		.unwrap();

		// Everything was rebuilt: bodies, read index, account and expiry indexes, counters.
		assert_eq!(store.statements().unwrap().len(), 2);
		assert_eq!(store.statement_count(), 2);
		assert_eq!(store.total_size(), 1 + 200);
		assert!(store.has_account(&s1.account_id().unwrap()));
		assert!(store.has_account(&account(2)));
		assert!(store
			.db
			.get(
				col::INDEX_BY_EXPIRY,
				&crate::expiry_index_key(crate::Expiry(s2.expiry()), &s2.hash())
			)
			.unwrap()
			.is_some());

		// s1 kept the sequence number the first attempt assigned; s2 got a fresh one above it.
		assert_eq!(
			store.db.get(col::ADMISSION_SEQ, &7u64.to_be_bytes()).unwrap(),
			Some(s1.hash().to_vec())
		);
		assert_eq!(
			store.db.get(col::ADMISSION_SEQ, &8u64.to_be_bytes()).unwrap(),
			Some(s2.hash().to_vec())
		);
		assert_eq!(store.submit_index.read().next_seq, 9);
		let replay = store.replay_batch(&OptimizedTopicFilter::Any, 7, 9).unwrap();
		assert_eq!(replay.statements, vec![s1.encode(), s2.encode()]);

		// Channel state was rebuilt from the bodies: a lower-priority replacement is rejected.
		assert!(matches!(
			store.submit(statement(2, 4, Some(9), 100), StatementSource::Network),
			SubmitResult::Rejected(RejectionReason::ChannelPriorityTooLow { .. })
		));
	}

	#[test]
	fn counters_and_seq_survive_restart() {
		let (store, temp) = test_store();
		for i in 0..3u8 {
			assert_eq!(
				store.submit(signed_statement(i), StatementSource::Network),
				SubmitResult::New
			);
		}
		assert_eq!(
			store.submit(statement(1, 1, None, 100), StatementSource::Network),
			SubmitResult::New
		);
		assert_eq!(store.statement_count(), 4);
		assert_eq!(store.total_size(), 3 + 100);
		let keystore = store.keystore.clone();
		drop(store);

		let mut path: std::path::PathBuf = temp.path().into();
		path.push("db");
		let store = Store::new::<Block, TestClient, TestBackend>(
			&path,
			Default::default(),
			std::sync::Arc::new(TestClient),
			keystore,
			None,
			Box::new(sp_core::testing::TaskExecutor::new()),
		)
		.unwrap();
		assert_eq!(store.statement_count(), 4);
		assert_eq!(store.total_size(), 3 + 100);
		assert_eq!(store.submit_index.read().next_seq, 4);
		// `signed_statement` signs as Alice, so two distinct accounts are stored.
		assert_eq!(store.account_count(), 2);
	}

	#[test]
	fn resubmitting_live_statement_is_known_and_not_double_counted() {
		let (store, _temp) = test_store();
		let statement = statement(1, 1, None, 100);
		assert_eq!(store.submit(statement.clone(), StatementSource::Local), SubmitResult::New);
		assert_eq!(store.total_size(), 100);
		assert_eq!(store.submit(statement.clone(), StatementSource::Local), SubmitResult::Known);
		assert_eq!(store.submit(statement, StatementSource::Network), SubmitResult::Known);
		assert_eq!(store.total_size(), 100);
		assert_eq!(store.statement_count(), 1);
	}

	#[test]
	fn eviction_clears_all_index_rows() {
		let (mut store, _temp) = test_store();
		store.set_time(0);
		let mut stmt = unsigned_statement(1, 1, Some(7), 100);
		stmt.set_topic(0, topic(3));
		stmt.set_expiry_from_parts(500, 1);
		sign_with(&mut stmt, 1);
		let hash = stmt.hash();
		let expiry = crate::Expiry(stmt.expiry());
		let who = account(1);
		assert_eq!(store.submit(stmt, StatementSource::Network), SubmitResult::New);
		let account_key = crate::account_index_key(&who, expiry, &hash);
		let expiry_key = crate::expiry_index_key(expiry, &hash);
		assert!(store.db.get(col::INDEX_BY_ACCOUNT, &account_key).unwrap().is_some());
		assert!(store.db.get(col::INDEX_BY_EXPIRY, &expiry_key).unwrap().is_some());

		store.remove(&hash).unwrap();
		assert!(store.db.get(col::INDEX_BY_ACCOUNT, &account_key).unwrap().is_none());
		assert!(store.db.get(col::INDEX_BY_EXPIRY, &expiry_key).unwrap().is_none());
		assert!(!store.index_has_topic(&topic(3), &hash));
		assert_eq!(store.db.get(col::ADMISSION_SEQ, &0u64.to_be_bytes()).unwrap(), None);
		// Removed before its natural expiry: banned via the evicted journal.
		assert!(store.is_evicted(&hash));
		assert_eq!(store.statement_count(), 0);
		assert_eq!(store.total_size(), 0);
	}

	#[test]
	fn index_key_layouts_roundtrip() {
		// The key builders and their parsers are the single place that knows the on-disk key
		// layouts; whoever changes one side must keep the other in sync, and this test is the
		// tripwire for that.
		use crate::{
			account_index_key, dec_key_index_key, dec_key_index_prefix, evicted_index_key,
			expiry_index_key, hash_from_index_key, parse_account_index_key,
			parse_dec_key_index_prefix, parse_time_index_key, prefix_from_index_key,
			topic_index_key, Expiry,
		};

		let who = account(7);
		let hash: crate::Hash = account(8);
		let expiry = Expiry((123u64 << 32) | 45);

		let key = account_index_key(&who, expiry, &hash);
		assert_eq!(parse_account_index_key(&key), Some((who, expiry, hash)));
		// Any other length is rejected rather than misparsed.
		assert_eq!(parse_account_index_key(&key[..key.len() - 1]), None);
		assert_eq!(parse_account_index_key(&[]), None);

		let key = expiry_index_key(expiry, &hash);
		assert_eq!(parse_time_index_key(&key), Some((expiry.0, hash)));
		assert_eq!(parse_time_index_key(&key[1..]), None);

		let key = evicted_index_key(789, &hash);
		assert_eq!(parse_time_index_key(&key), Some((789, hash)));

		// Big-endian time keys keep numeric order; the expiry sweep and the evicted journal
		// drain rely on it to stop at the first entry that is not yet due.
		assert!(expiry_index_key(Expiry(256), &hash) > expiry_index_key(Expiry(1), &hash));
		assert!(evicted_index_key(256, &hash) > evicted_index_key(1, &hash));

		let key = topic_index_key(&topic(3), &hash);
		assert_eq!(hash_from_index_key(&key), Some(hash));
		assert_eq!(prefix_from_index_key(&key), Some(&topic(3)[..]));

		for dec_key in [None, Some(dec_key(5))] {
			assert_eq!(parse_dec_key_index_prefix(&dec_key_index_prefix(&dec_key)), Some(dec_key));
			let key = dec_key_index_key(&dec_key, &hash);
			assert_eq!(hash_from_index_key(&key), Some(hash));
			assert_eq!(
				prefix_from_index_key(&key).and_then(parse_dec_key_index_prefix),
				Some(dec_key)
			);
		}
	}

	#[test]
	fn summary_fast_path_stays_coherent_with_disk() {
		use crate::{DETAILS_CACHE_BUDGET, SUMMARY_CACHE_ACCOUNTS};

		let (store, _temp) = test_store();
		let source = StatementSource::Network;

		// The test walks three accounts (5, 6 and 7); every summary must stay cached throughout.
		assert!(SUMMARY_CACHE_ACCOUNTS >= 3, "three account summaries must fit the cache");
		// Two filler accounts grow their records until the details cache overflows by at least
		// one statement, so that account 5's record (of one statement) is the one evicted.
		let per_filler_account = DETAILS_CACHE_BUDGET.div_ceil(2);
		assert!(
			per_filler_account <= 100,
			"the filler accounts' allowance (100 statements, 1000 bytes of data) caps the cache \
			 budget this test can exercise",
		);

		// Account 5's record enters the details cache with its first statement.
		let s5_1 = statement(5, 1, None, 100);
		assert_eq!(store.submit(s5_1.clone(), source), SubmitResult::New);
		assert!(store.details_cached(&account(5)));

		// Grow the filler accounts' records past the cache budget; account 5's record is the
		// least recently used one, so it is evicted, while its summary stays cached.
		for acc in [6u64, 7] {
			for c in 1..=per_filler_account as u64 {
				assert_eq!(store.submit(statement(acc, 1, Some(c), 10), source), SubmitResult::New);
			}
		}
		assert!(!store.details_cached(&account(5)));

		// A channel-less statement within the account's allowance is admitted through the
		// summary alone: the record is not rematerialised.
		let s5_2 = statement(5, 2, None, 50);
		assert_eq!(store.submit(s5_2.clone(), source), SubmitResult::New);
		assert!(!store.details_cached(&account(5)));

		// Pushing the account over its size allowance (1000 bytes) forces the record to be
		// loaded back from disk. It must contain both earlier statements — including the one
		// admitted through the fast path — and evict exactly the lowest-priority one.
		let s5_3 = statement(5, 3, None, 900);
		assert_eq!(store.submit(s5_3.clone(), source), SubmitResult::New);
		assert!(store.details_cached(&account(5)));
		assert!(!store.has_statement(&s5_1.hash()), "lowest priority must be evicted");
		assert!(store.has_statement(&s5_2.hash()));
		assert!(store.has_statement(&s5_3.hash()));
		let filler_statements = 2 * per_filler_account;
		assert_eq!(store.statement_count(), 2 + filler_statements);
		assert_eq!(store.total_size(), 50 + 900 + filler_statements * 10);
	}

	#[test]
	fn rejected_submission_caches_the_loaded_record() {
		let (store, temp) = test_store();
		let source = StatementSource::Network;

		// Account 5 owns one channel statement.
		assert_eq!(store.submit(statement(5, 2, Some(1), 100), source), SubmitResult::New);

		// Reopen the store so that both caches start cold.
		let keystore = store.keystore.clone();
		drop(store);
		let mut path: std::path::PathBuf = temp.path().into();
		path.push("db");
		let store = Store::new::<Block, TestClient, TestBackend>(
			&path,
			Default::default(),
			std::sync::Arc::new(TestClient),
			keystore,
			None,
			Box::new(sp_core::testing::TaskExecutor::new()),
		)
		.unwrap();

		// An oversize statement is rejected before the account record is even loaded, so the
		// caches stay cold.
		assert_eq!(
			store.submit(statement(5, 3, None, 1500), source),
			SubmitResult::Rejected(RejectionReason::DataTooLarge {
				submitted_size: 1500,
				available_size: 1000,
			})
		);
		assert!(!store.details_cached(&account(5)));
		assert!(!store.summary_cached(&account(5)));

		// A channel statement of too low a priority is rejected only after planning against the
		// record loaded from disk. The record and its summary must stay cached: rejections cost
		// the sender nothing, so retries must not rescan the on-disk index every time.
		assert!(matches!(
			store.submit(statement(5, 1, Some(1), 100), source),
			SubmitResult::Rejected(RejectionReason::ChannelPriorityTooLow { .. })
		));
		assert!(store.details_cached(&account(5)));
		assert!(store.summary_cached(&account(5)));
	}

	#[test]
	fn take_recent_statements_clears_index() {
		let (store, _temp) = test_store();
		let statement0 = signed_statement(0);
		let statement1 = signed_statement(1);
		let statement2 = signed_statement(2);
		let statement3 = signed_statement(3);

		let _ = store.submit(statement0.clone(), StatementSource::Local);
		let _ = store.submit(statement1.clone(), StatementSource::Local);
		let _ = store.submit(statement2.clone(), StatementSource::Local);

		let recent1 = store.take_recent_statements().unwrap();
		let (recent1_hashes, recent1_statements): (Vec<_>, Vec<_>) =
			recent1.into_iter().map(|(_seq, hash, statement)| (hash, statement)).unzip();
		let expected1 = vec![statement0, statement1, statement2];
		assert!(expected1.iter().all(|s| recent1_hashes.contains(&s.hash())));
		assert!(expected1.iter().all(|s| recent1_statements.contains(s)));

		// Recent statements are cleared.
		let recent2 = store.take_recent_statements().unwrap();
		assert_eq!(recent2.len(), 0);

		store.submit(statement3.clone(), StatementSource::Network);

		let recent3 = store.take_recent_statements().unwrap();
		let (recent3_hashes, recent3_statements): (Vec<_>, Vec<_>) =
			recent3.into_iter().map(|(_seq, hash, statement)| (hash, statement)).unzip();
		let expected3 = vec![statement3];
		assert!(expected3.iter().all(|s| recent3_hashes.contains(&s.hash())));
		assert!(expected3.iter().all(|s| recent3_statements.contains(s)));

		// Recent statements are cleared, but statements remain in the store.
		assert_eq!(store.statements().unwrap().len(), 4);
	}

	#[test]
	fn search_by_topic_and_key() {
		let (store, _temp) = test_store();
		let statement0 = signed_statement(0);
		let statement1 = signed_statement_with_topics(1, &[topic(0)], None);
		let statement2 = signed_statement_with_topics(2, &[topic(0), topic(1)], Some(dec_key(2)));
		let statement3 = signed_statement_with_topics(3, &[topic(0), topic(1), topic(2)], None);
		let statement4 =
			signed_statement_with_topics(4, &[topic(0), topic(42), topic(2), topic(3)], None);
		let statements = vec![statement0, statement1, statement2, statement3, statement4];
		for s in &statements {
			store.submit(s.clone(), StatementSource::Network);
		}

		let assert_topics = |topics: &[u64], key: Option<u64>, expected: &[u8]| {
			let key = key.map(dec_key);
			let topics: Vec<_> = topics.iter().map(|t| topic(*t)).collect();
			let mut got_vals: Vec<_> = if let Some(key) = key {
				store.posted(&topics, key).unwrap().into_iter().map(|d| d[0]).collect()
			} else {
				store.broadcasts(&topics).unwrap().into_iter().map(|d| d[0]).collect()
			};
			got_vals.sort();
			assert_eq!(expected.to_vec(), got_vals);
		};

		assert_topics(&[], None, &[0, 1, 3, 4]);
		assert_topics(&[], Some(2), &[2]);
		assert_topics(&[0], None, &[1, 3, 4]);
		assert_topics(&[1], None, &[3]);
		assert_topics(&[2], None, &[3, 4]);
		assert_topics(&[3], None, &[4]);
		assert_topics(&[42], None, &[4]);

		assert_topics(&[0, 1], None, &[3]);
		assert_topics(&[0, 1], Some(2), &[2]);
		assert_topics(&[0, 1, 99], Some(2), &[]);
		assert_topics(&[1, 2], None, &[3]);
		assert_topics(&[99], None, &[]);
		assert_topics(&[0, 99], None, &[]);
		assert_topics(&[0, 1, 2, 3, 42], None, &[]);
	}

	#[test]
	fn constraints() {
		let (store, _temp) = test_store();

		store.submit_index.write().config.max_total_size = 3000;
		let source = StatementSource::Network;
		let ok = SubmitResult::New;

		// Account 1 (limit = 1 msg, 1000 bytes)

		// Oversized statement is not allowed. Limit for account 1 is 1 msg, 1000 bytes
		assert!(matches!(
			store.submit(statement(1, 1, Some(1), 2000), source),
			SubmitResult::Rejected(_)
		));
		assert_eq!(store.submit(statement(1, 1, Some(1), 500), source), ok);
		// Would not replace channel message with same priority
		assert!(matches!(
			store.submit(statement(1, 1, Some(1), 200), source),
			SubmitResult::Rejected(_)
		));
		assert_eq!(store.submit(statement(1, 2, Some(1), 600), source), ok);
		// Submit another message to another channel with lower priority. Should not be allowed
		// because msg count limit is 1
		assert!(matches!(
			store.submit(statement(1, 1, Some(2), 100), source),
			SubmitResult::Rejected(_)
		));
		assert_eq!(store.evicted_count(), 1);

		// Account 2 (limit = 2 msg, 1000 bytes)

		let s2_prio1 = statement(2, 1, None, 500);
		let s2_prio2 = statement(2, 2, None, 100);
		assert_eq!(store.submit(s2_prio1.clone(), source), ok);
		assert_eq!(store.submit(s2_prio2.clone(), source), ok);
		// Equal priority to lowest should be rejected
		assert!(matches!(
			store.submit(statement(2, 1, None, 50), source),
			SubmitResult::Rejected(RejectionReason::AccountFull { .. })
		));
		// Should evict priority 1
		let s2_prio3 = statement(2, 3, None, 500);
		assert_eq!(store.submit(s2_prio3.clone(), source), ok);
		assert_eq!(store.evicted_count(), 2);
		assert!(store.is_evicted(&s2_prio1.hash()));
		assert!(store.statement(&s2_prio1.hash()).unwrap().is_none());
		// Should evict all
		assert_eq!(store.submit(statement(2, 4, None, 1000), source), ok);
		assert_eq!(store.evicted_count(), 4);
		assert!(store.is_evicted(&s2_prio2.hash()));
		assert!(store.is_evicted(&s2_prio3.hash()));

		// Account 3 (limit = 3 msg, 1000 bytes)

		let s3_prio2 = statement(3, 2, Some(1), 300);
		let s3_prio3 = statement(3, 3, Some(2), 300);
		assert_eq!(store.submit(s3_prio2.clone(), source), ok);
		assert_eq!(store.submit(s3_prio3.clone(), source), ok);
		assert_eq!(store.submit(statement(3, 4, Some(3), 300), source), ok);
		// Should evict 2 and 3
		assert_eq!(store.submit(statement(3, 5, None, 500), source), ok);
		assert_eq!(store.evicted_count(), 6);
		assert!(store.is_evicted(&s3_prio2.hash()));
		assert!(store.is_evicted(&s3_prio3.hash()));

		assert_eq!(store.total_size(), 2400);
		assert_eq!(store.statement_count(), 4);

		// Should be over the global size limit
		assert!(matches!(
			store.submit(statement(1, 1, None, 700), source),
			SubmitResult::Rejected(_)
		));
		// Should be over the global count limit
		store.submit_index.write().config.max_total_statements = 4;
		assert!(matches!(
			store.submit(statement(1, 1, None, 100), source),
			SubmitResult::Rejected(_)
		));

		let mut expected_statements = vec![
			statement(1, 2, Some(1), 600).hash(),
			statement(2, 4, None, 1000).hash(),
			statement(3, 4, Some(3), 300).hash(),
			statement(3, 5, None, 500).hash(),
		];
		expected_statements.sort();
		let mut statements: Vec<_> =
			store.statements().unwrap().into_iter().map(|(hash, _)| hash).collect();
		statements.sort();
		assert_eq!(expected_statements, statements);
	}

	#[test]
	fn insufficient_remaining_account_bytes_is_data_too_large() {
		let (store, _temp) = test_store();
		let source = StatementSource::Network;

		// Account 4 allows four statements and 1000 data bytes. The count limit has room, but the
		// lower-priority submission cannot evict the existing statement to obtain enough bytes.
		assert_eq!(store.submit(statement(4, 2, None, 900), source), SubmitResult::New);
		assert_eq!(
			store.submit(statement(4, 1, None, 200), source),
			SubmitResult::Rejected(RejectionReason::DataTooLarge {
				submitted_size: 200,
				available_size: 100,
			})
		);
	}

	#[test]
	fn max_statement_size_for_gossiping() {
		let (store, _temp) = test_store();
		store.submit_index.write().config.max_total_size = 42 * crate::MAX_STATEMENT_SIZE;

		assert_eq!(
			store.submit(
				statement(42, 1, Some(1), crate::MAX_STATEMENT_SIZE - 500),
				StatementSource::Local
			),
			SubmitResult::New
		);

		assert!(matches!(
			store.submit(
				statement(42, 2, Some(1), 2 * crate::MAX_STATEMENT_SIZE),
				StatementSource::Local
			),
			SubmitResult::Invalid(_)
		));
	}

	#[test]
	fn expired_statements_are_purged() {
		use super::DEFAULT_PURGE_AFTER_SEC;
		let (mut store, temp) = test_store();
		let mut statement = unsigned_statement(1, 1, Some(3), 100);
		store.set_time(0);
		statement.set_topic(0, topic(4));
		sign_with(&mut statement, 1);
		store.submit(statement.clone(), StatementSource::Network);
		assert_eq!(store.statement_count(), 1);
		store.remove(&statement.hash()).unwrap();
		assert_eq!(store.statement_count(), 0);
		assert_eq!(store.account_count(), 0);
		store.set_time(DEFAULT_PURGE_AFTER_SEC + 1);
		store.maintain();
		assert_eq!(store.evicted_count(), 0);
		let keystore = store.keystore.clone();
		drop(store);

		let client = std::sync::Arc::new(TestClient);
		let mut path: std::path::PathBuf = temp.path().into();
		path.push("db");
		let store = Store::new::<Block, TestClient, TestBackend>(
			&path,
			Default::default(),
			client,
			keystore,
			None,
			Box::new(sp_core::testing::TaskExecutor::new()),
		)
		.unwrap();
		assert_eq!(store.statements().unwrap().len(), 0);
		assert_eq!(store.evicted_count(), 0);
	}

	#[test]
	fn posted_clear_decrypts() {
		let (store, _temp) = test_store();
		let public = store
			.keystore
			.ed25519_generate_new(sp_core::crypto::key_types::STATEMENT, None)
			.unwrap();
		let statement1 = statement(1, 1, None, 100);
		let mut statement2 = unsigned_statement(1, 2, None, 0);
		let plain = b"The most valuable secret".to_vec();
		statement2.encrypt(&plain, &public).unwrap();
		sign_with(&mut statement2, 1);
		store.submit(statement1, StatementSource::Network);
		store.submit(statement2, StatementSource::Network);
		let posted_clear = store.posted_clear(&[], public.into()).unwrap();
		assert_eq!(posted_clear, vec![plain]);
	}

	#[test]
	fn broadcasts_stmt_returns_encoded_statements() {
		let (store, _tmp) = test_store();

		// no key, no topic
		let s0 = signed_statement_with_topics(0, &[], None);
		// same, but with a topic = 42
		let s1 = signed_statement_with_topics(1, &[topic(42)], None);
		// has a decryption key -> must NOT be returned by broadcasts_stmt
		let s2 = signed_statement_with_topics(2, &[topic(42)], Some(dec_key(99)));

		for s in [&s0, &s1, &s2] {
			store.submit(s.clone(), StatementSource::Network);
		}

		// no topic filter
		let mut hashes: Vec<_> = store
			.broadcasts_stmt(&[])
			.unwrap()
			.into_iter()
			.map(|bytes| Statement::decode(&mut &bytes[..]).unwrap().hash())
			.collect();
		hashes.sort();
		let expected_hashes = {
			let mut e = vec![s0.hash(), s1.hash()];
			e.sort();
			e
		};
		assert_eq!(hashes, expected_hashes);

		// filter on topic 42
		let got = store.broadcasts_stmt(&[topic(42)]).unwrap();
		assert_eq!(got.len(), 1);
		let st = Statement::decode(&mut &got[0][..]).unwrap();
		assert_eq!(st.hash(), s1.hash());
	}

	#[test]
	fn posted_stmt_returns_encoded_statements_for_dest() {
		let (store, _tmp) = test_store();

		let public1 = store
			.keystore
			.ed25519_generate_new(sp_core::crypto::key_types::STATEMENT, None)
			.unwrap();
		let dest: [u8; 32] = public1.into();

		let public2 = store
			.keystore
			.ed25519_generate_new(sp_core::crypto::key_types::STATEMENT, None)
			.unwrap();

		// A statement that does have dec_key = dest
		let mut s_with_key = unsigned_statement(1, 1, None, 0);
		let plain1 = b"The most valuable secret".to_vec();
		s_with_key.encrypt(&plain1, &public1).unwrap();
		sign_with(&mut s_with_key, 1);

		// A statement with a different dec_key
		let mut s_other_key = unsigned_statement(2, 2, None, 0);
		let plain2 = b"The second most valuable secret".to_vec();
		s_other_key.encrypt(&plain2, &public2).unwrap();
		sign_with(&mut s_other_key, 2);

		// Submit them all
		for s in [&s_with_key, &s_other_key] {
			store.submit(s.clone(), StatementSource::Network);
		}

		// posted_stmt should only return the one with dec_key = dest
		let retrieved = store.posted_stmt(&[], dest).unwrap();
		assert_eq!(retrieved.len(), 1, "Only one statement has dec_key=dest");

		// Re-decode that returned statement to confirm it is correct
		let returned_stmt = Statement::decode(&mut &retrieved[0][..]).unwrap();
		assert_eq!(
			returned_stmt.hash(),
			s_with_key.hash(),
			"Returned statement must match s_with_key"
		);
	}

	#[test]
	fn posted_clear_stmt_returns_statement_followed_by_plain_data() {
		let (store, _tmp) = test_store();

		let public1 = store
			.keystore
			.ed25519_generate_new(sp_core::crypto::key_types::STATEMENT, None)
			.unwrap();
		let dest: [u8; 32] = public1.into();

		let public2 = store
			.keystore
			.ed25519_generate_new(sp_core::crypto::key_types::STATEMENT, None)
			.unwrap();

		// A statement that does have dec_key = dest
		let mut s_with_key = unsigned_statement(1, 1, None, 0);
		let plain1 = b"The most valuable secret".to_vec();
		s_with_key.encrypt(&plain1, &public1).unwrap();
		sign_with(&mut s_with_key, 1);

		// A statement with a different dec_key
		let mut s_other_key = unsigned_statement(2, 2, None, 0);
		let plain2 = b"The second most valuable secret".to_vec();
		s_other_key.encrypt(&plain2, &public2).unwrap();
		sign_with(&mut s_other_key, 2);

		// Submit them all
		for s in [&s_with_key, &s_other_key] {
			store.submit(s.clone(), StatementSource::Network);
		}

		// posted_stmt should only return the one with dec_key = dest
		let retrieved = store.posted_clear_stmt(&[], dest).unwrap();
		assert_eq!(retrieved.len(), 1, "Only one statement has dec_key=dest");

		// We expect: [ encoded Statement ] + [ the decrypted bytes ]
		let encoded_stmt = s_with_key.encode();
		let stmt_len = encoded_stmt.len();

		// 1) statement is first
		assert_eq!(&retrieved[0][..stmt_len], &encoded_stmt[..]);

		// 2) followed by the decrypted payload
		let trailing = &retrieved[0][stmt_len..];
		assert_eq!(trailing, &plain1[..]);
	}

	#[test]
	fn posted_clear_returns_plain_data_for_dest_and_topics() {
		let (store, _tmp) = test_store();

		// prepare two key-pairs
		let public_dest = store
			.keystore
			.ed25519_generate_new(sp_core::crypto::key_types::STATEMENT, None)
			.unwrap();
		let dest: [u8; 32] = public_dest.into();

		let public_other = store
			.keystore
			.ed25519_generate_new(sp_core::crypto::key_types::STATEMENT, None)
			.unwrap();

		// statement that SHOULD be returned (matches dest & topic 42)
		let mut s_good = unsigned_statement(1, 1, None, 0);
		let plaintext_good = b"The most valuable secret".to_vec();
		s_good.encrypt(&plaintext_good, &public_dest).unwrap();
		s_good.set_topic(0, topic(42));
		sign_with(&mut s_good, 1);

		// statement that should NOT be returned (same dest but different topic)
		let mut s_wrong_topic = unsigned_statement(2, 2, None, 0);
		s_wrong_topic.encrypt(b"Wrong topic", &public_dest).unwrap();
		s_wrong_topic.set_topic(0, topic(99));
		sign_with(&mut s_wrong_topic, 2);

		// statement that should NOT be returned (different dest)
		let mut s_other_dest = unsigned_statement(3, 3, None, 0);
		s_other_dest.encrypt(b"Other dest", &public_other).unwrap();
		s_other_dest.set_topic(0, topic(42));
		sign_with(&mut s_other_dest, 3);

		// submit all
		for s in [&s_good, &s_wrong_topic, &s_other_dest] {
			store.submit(s.clone(), StatementSource::Network);
		}

		// call posted_clear with the topic filter and dest
		let retrieved = store.posted_clear(&[topic(42)], dest).unwrap();

		// exactly one element, equal to the expected plaintext
		assert_eq!(retrieved, vec![plaintext_good]);
	}

	#[test]
	fn already_expired_statement_is_rejected() {
		let (mut store, _temp) = test_store();

		// Set current time to 1000 seconds
		store.set_time(1000);

		// Create a statement that has already expired (expiration at 500 seconds, before current
		// time)
		let mut expired_statement = unsigned_statement(1, 1, None, 100);
		// set_expiry_from_parts: first arg is expiration timestamp in seconds, second is priority
		expired_statement.set_expiry_from_parts(500, 1);
		sign_with(&mut expired_statement, 1);

		// Submit should fail with AlreadyExpired
		assert_eq!(
			store.submit(expired_statement, StatementSource::Network),
			SubmitResult::Invalid(InvalidReason::AlreadyExpired)
		);

		// Verify the statement was not added
		assert_eq!(store.statements().unwrap().len(), 0);

		// Now create a statement that is not expired (expiration at 2000 seconds, after current
		// time)
		let mut valid_statement = unsigned_statement(1, 1, None, 100);
		valid_statement.set_expiry_from_parts(2000, 1);
		sign_with(&mut valid_statement, 1);

		// Submit should succeed
		assert_eq!(store.submit(valid_statement, StatementSource::Network), SubmitResult::New);
		assert_eq!(store.statements().unwrap().len(), 1);
	}

	#[test]
	fn remove_by_covers_various_situations() {
		use sp_statement_store::{StatementSource, StatementStore, SubmitResult};

		// Use a fresh store and fixed time so we can control purging.
		let (mut store, _temp) = test_store();
		store.set_time(0);

		// Reuse helpers from this module.
		let t42 = topic(42);
		let k7 = dec_key(7);

		// Account A = 4 (has per-account limits (4, 1000) in the mock runtime)
		// - Mix of topic, decryption-key and channel to exercise every index.
		let mut s_a1 = unsigned_statement(4, 10, Some(100), 100);
		s_a1.set_topic(0, t42);
		sign_with(&mut s_a1, 4);
		let h_a1 = s_a1.hash();

		let mut s_a2 = unsigned_statement(4, 20, Some(200), 150);
		s_a2.set_decryption_key(k7);
		sign_with(&mut s_a2, 4);
		let h_a2 = s_a2.hash();

		let s_a3 = statement(4, 30, None, 50);
		let h_a3 = s_a3.hash();

		// Account B = 3 (control group that must remain untouched).
		let s_b1 = statement(3, 10, None, 100);
		let h_b1 = s_b1.hash();

		let mut s_b2 = unsigned_statement(3, 15, Some(300), 100);
		s_b2.set_topic(0, t42);
		s_b2.set_decryption_key(k7);
		sign_with(&mut s_b2, 3);
		let h_b2 = s_b2.hash();

		// Submit all statements.
		for s in [&s_a1, &s_a2, &s_a3, &s_b1, &s_b2] {
			assert_eq!(store.submit(s.clone(), StatementSource::Network), SubmitResult::New);
		}

		// --- Pre-conditions: everything is indexed as expected.
		{
			assert_eq!(store.statement_count(), 5, "all 5 should be present");
			assert!(store.has_account(&account(4)));
			assert!(store.has_account(&account(3)));
			assert_eq!(store.total_size(), 100 + 150 + 50 + 100 + 100);

			// Topic and key sets contain both A & B entries.
			assert!(store.index_has_topic(&t42, &h_a1) && store.index_has_topic(&t42, &h_b2));
			assert!(
				store.index_has_dec_key(&Some(k7), &h_a2) &&
					store.index_has_dec_key(&Some(k7), &h_b2)
			);
		}

		// --- Action: remove all statements by Account A.
		store.remove_by(account(4)).expect("remove_by should succeed");

		// --- Post-conditions: A's statements are gone and marked expired; B's remain.
		{
			// A's statements removed from DB view.
			for h in [h_a1, h_a2, h_a3] {
				assert!(store.statement(&h).unwrap().is_none(), "A's statement should be removed");
			}

			// B's statements still present.
			for h in [h_b1, h_b2] {
				assert!(store.statement(&h).unwrap().is_some(), "B's statement should remain");
			}

			// Account index updated.
			assert!(!store.has_account(&account(4)), "Account A must be gone");
			assert!(store.has_account(&account(3)), "Account B must remain");
			// Entry count & total_size reflect only B's data.
			assert_eq!(store.statement_count(), 2);
			assert_eq!(store.total_size(), 100 + 100);

			// Removed statements are banned in the on-disk evicted journal.
			assert!(store.is_evicted(&h_a1));
			assert!(store.is_evicted(&h_a2));
			assert!(store.is_evicted(&h_a3));
			assert_eq!(store.evicted_count(), 3);

			// Topic index: only B2 remains for topic 42.
			assert!(store.index_has_topic(&t42, &h_b2));
			assert!(!store.index_has_topic(&t42, &h_a1));

			// Decryption-key index: only B2 remains for key 7.
			assert!(store.index_has_dec_key(&Some(k7), &h_b2));
			assert!(!store.index_has_dec_key(&Some(k7), &h_a2));
		}

		// --- Idempotency: removing again is a no-op and should not error.
		store.remove_by(account(4)).expect("second remove_by should be a no-op");

		// --- Purge: advance time beyond TTL and run maintenance; expired entries disappear.
		let purge_after = store.submit_index.read().config.purge_after_sec;
		store.set_time(purge_after + 1);
		store.maintain();
		assert_eq!(store.evicted_count(), 0, "expired entries should be purged");

		// --- Reuse: Account A can submit again after purge.
		let s_new = statement(4, 40, None, 10);
		assert_eq!(store.submit(s_new, StatementSource::Network), SubmitResult::New);
	}

	#[test]
	fn check_expiration_expires_statements_past_current_time() {
		let (mut store, _temp) = test_store();

		// The check_expiration function compares Expiry(current_time << 32) against
		// Expiry(expiry) where expiry is the full 64-bit value with timestamp in high 32 bits.
		// Statements with expiration timestamp < current_time will be expired.

		store.set_time(100);

		// Create a statement that will expire at timestamp 500
		let mut expired_stmt = unsigned_statement(1, 1, None, 100);
		expired_stmt.set_expiry_from_parts(500, 1);
		sign_with(&mut expired_stmt, 1);
		let expired_hash = expired_stmt.hash();
		store.submit(expired_stmt, StatementSource::Network);

		// Create a statement that won't expire (far future expiry)
		let valid_stmt = statement(2, 1, None, 100); // Uses u32::MAX as timestamp
		let valid_hash = valid_stmt.hash();
		store.submit(valid_stmt, StatementSource::Network);

		// Verify both statements are in the store
		assert_eq!(store.statement_count(), 2);

		// Advance time past the expiry of the first statement
		store.set_time(1000);

		// The sweep should find and expire the statement
		store.enforce_limits();

		// Naturally-expired statements are not added to the expired map (AlreadyExpired check
		// in submit rejects them without consulting the map)
		assert!(
			!store.is_evicted(&expired_hash),
			"Naturally expired statement must not be added to the expired map"
		);
		assert!(
			!store.has_statement(&expired_hash),
			"Expired statement should be removed from the store"
		);

		// The valid statement should still be stored
		assert!(store.has_statement(&valid_hash), "Valid statement should still be stored");
		assert!(!store.is_evicted(&valid_hash), "Valid statement should not be expired");
	}

	#[test]
	fn allowance_sweep_resumes_from_cursor_across_calls() {
		use std::time::Duration;

		let (mut store, _temp) = test_store();
		store.set_time(0);

		// Five accounts, each 200 data bytes over its 1000-byte allowance, so enforcement
		// evicts exactly one statement per account processed.
		for seed in 10u64..15 {
			store.force_insert(&statement(seed, 1, None, 600));
			store.force_insert(&statement(seed, 2, None, 600));
		}
		assert_eq!(store.statement_count(), 10);
		assert!(store.submit_index.read().allowance_cursor.is_none());

		// With a budget of two accounts per call, the sweep needs three calls to cover all
		// five, resuming from the stored cursor each time.
		store.enforce_limits_bounded(usize::MAX, 2, Duration::from_secs(3600));
		assert_eq!(store.statement_count(), 8);
		assert!(store.submit_index.read().allowance_cursor.is_some());

		store.enforce_limits_bounded(usize::MAX, 2, Duration::from_secs(3600));
		assert_eq!(store.statement_count(), 6);
		assert!(store.submit_index.read().allowance_cursor.is_some());

		// The last call processes the fifth account, wraps, resets the cursor and refreshes
		// the account gauge with the number of accounts the pass has seen.
		store.enforce_limits_bounded(usize::MAX, 2, Duration::from_secs(3600));
		assert_eq!(store.statement_count(), 5);
		assert!(store.submit_index.read().allowance_cursor.is_none());
		assert_eq!(store.known_accounts_count.load(std::sync::atomic::Ordering::Relaxed), 5);

		// A further unrestricted pass finds every account within its allowance.
		store.enforce_limits();
		assert_eq!(store.statement_count(), 5);
	}

	#[test]
	fn allowance_eviction_budget_defers_the_residue_to_the_next_pass() {
		use std::time::Duration;

		let (mut store, _temp) = test_store();
		store.set_time(0);

		// Account 1's allowance is a single statement: three of the four must be evicted.
		for priority in 1..=4 {
			store.force_insert(&statement(1, priority, None, 100));
		}
		assert_eq!(store.statement_count(), 4);

		// The per-call eviction budget stops enforcement mid-account. The cursor has already
		// moved past the account, so it is not revisited within the same pass...
		store.enforce_limits_bounded(2, usize::MAX, Duration::from_secs(3600));
		assert_eq!(store.statement_count(), 2);

		// ...the pass first has to wrap, finding no further accounts...
		store.enforce_limits_bounded(2, usize::MAX, Duration::from_secs(3600));
		assert_eq!(store.statement_count(), 2);
		assert!(store.submit_index.read().allowance_cursor.is_none());

		// ...and the next pass evicts the remaining excess statement.
		store.enforce_limits_bounded(2, usize::MAX, Duration::from_secs(3600));
		assert_eq!(store.statement_count(), 1);
	}

	#[test]
	fn expiry_sweep_is_bounded_by_the_statement_budget() {
		use std::time::Duration;

		let (mut store, _temp) = test_store();
		store.set_time(100);

		// Five statements, all past their expiry once the clock advances.
		for seed in 1u64..=5 {
			let mut stmt = unsigned_statement(seed, 1, None, 100);
			stmt.set_expiry_from_parts(200, 1);
			sign_with(&mut stmt, seed);
			assert_eq!(store.submit(stmt, StatementSource::Network), SubmitResult::New);
		}
		store.set_time(300);

		// Each call reaps at most the statement budget off the expiry index; the index itself
		// is the resume point, so the backlog drains across calls with no cursor involved.
		store.enforce_limits_bounded(2, usize::MAX, Duration::from_secs(3600));
		assert_eq!(store.statement_count(), 3);
		store.enforce_limits_bounded(2, usize::MAX, Duration::from_secs(3600));
		assert_eq!(store.statement_count(), 1);
		store.enforce_limits_bounded(2, usize::MAX, Duration::from_secs(3600));
		assert_eq!(store.statement_count(), 0);
		// Naturally expired statements are not banned via the evicted journal.
		assert_eq!(store.evicted_count(), 0);
	}

	#[test]
	fn expiry_sweep_leaves_inconsistent_data_in_place() {
		let (mut store, _temp) = test_store();
		store.set_time(100);

		let expiry = crate::Expiry(200u64 << 32 | 1);
		let undecodable_hash = [0xAB_u8; 32];
		let undecodable_key = crate::expiry_index_key(expiry, &undecodable_hash);
		store
			.db
			.commit([
				(col::STATEMENTS, undecodable_hash.to_vec(), Some(vec![0xFF])),
				(col::INDEX_BY_EXPIRY, undecodable_key.clone(), Some(Vec::new())),
			])
			.unwrap();

		// The sweep cannot remove the statement; the corruption is logged as an error and
		// everything is left exactly as it is, on every pass.
		store.set_time(300);
		store.enforce_limits();
		store.enforce_limits();
		assert!(store.has_statement(&undecodable_hash));
		assert!(store.db.get(col::INDEX_BY_EXPIRY, &undecodable_key).unwrap().is_some());

		// Same for an expiry row with no body behind it at all: the sweep reports it and leaves
		// it in place.
		let orphan_hash = [0xCD_u8; 32];
		let orphan_key = crate::expiry_index_key(expiry, &orphan_hash);
		store
			.db
			.commit([(col::INDEX_BY_EXPIRY, orphan_key.clone(), Some(Vec::new()))])
			.unwrap();
		store.enforce_limits();
		assert!(!store.has_statement(&orphan_hash));
		assert!(store.db.get(col::INDEX_BY_EXPIRY, &orphan_key).unwrap().is_some());
	}

	#[test]
	fn corrupt_account_index_key_does_not_end_account_enumeration() {
		let (store, _temp) = test_store();
		assert_eq!(
			store.submit(statement(1, 1, None, 1), StatementSource::Network),
			SubmitResult::New
		);
		assert_eq!(
			store.submit(statement(2, 1, None, 1), StatementSource::Network),
			SubmitResult::New
		);
		assert_eq!(store.account_count(), 2);

		// Two unparseable keys: one sorting before every valid key (but after the scan's
		// starting cursor) and one sorting between the two accounts' rows. Both must be skipped
		// by account enumeration, not taken for the end of the index.
		let (first, second) = {
			let (a, b) = (account(1), account(2));
			if a < b {
				(a, b)
			} else {
				(b, a)
			}
		};
		let mut between = first.to_vec();
		between.extend_from_slice(&[0xFF; 41]);
		store
			.db
			.commit([
				(col::INDEX_BY_ACCOUNT, vec![0u8; 33], Some(Vec::new())),
				(col::INDEX_BY_ACCOUNT, between, Some(Vec::new())),
			])
			.unwrap();

		assert_eq!(store.account_count(), 2);
		assert!(store.has_account(&first));
		assert!(store.has_account(&second));

		// A full allowance pass still sees both accounts and reports the true account count.
		store.enforce_limits();
		assert_eq!(store.known_accounts_count.load(std::sync::atomic::Ordering::Relaxed), 2);
	}

	#[test]
	fn concurrent_remove_and_resubmit_keep_query_bookkeeping_consistent() {
		// Exercises the #12624 interleaving: a `remove` racing a resubmission of the same
		// statement must never apply its query-index bookkeeping on top of the newer submit's.
		// The invariant below holds for any timing, so the test is deterministic even though
		// the race window itself is only hit probabilistically.
		use std::sync::Arc;

		let (store, _temp) = test_store();
		let store = Arc::new(store);
		let mut stmt = unsigned_statement(1, 1, None, 100);
		stmt.set_topic(0, topic(7));
		sign_with(&mut stmt, 1);
		let hash = stmt.hash();

		for _ in 0..200 {
			// The statement is live at the start of every round.
			if !store.has_statement(&hash) {
				assert_eq!(store.submit(stmt.clone(), StatementSource::Local), SubmitResult::New);
			}
			let remover = {
				let store = store.clone();
				std::thread::spawn(move || store.remove(&hash).expect("remove succeeds"))
			};
			let resubmitter = {
				let store = store.clone();
				let stmt = stmt.clone();
				std::thread::spawn(move || store.submit(stmt, StatementSource::Local))
			};
			remover.join().expect("remover joins");
			let _ = resubmitter.join().expect("resubmitter joins");

			// Whatever the interleaving, the query-index bookkeeping must agree with the store.
			let query_index = store.query_index.read();
			let present = store.has_statement(&hash);
			assert_eq!(query_index.recent.contains_key(&hash), present);
			assert_eq!(
				query_index.topic_counts.get(&topic(7)).copied().unwrap_or(0),
				present as usize
			);
		}
	}

	#[test]
	fn check_expiration_handles_multiple_statements_per_account() {
		let (mut store, _temp) = test_store();
		store.set_time(100);

		// Create multiple statements for the same account with different expiry timestamps
		// Account 42 has limit of 42 statements
		let mut stmt1 = unsigned_statement(42, 1, Some(1), 100);
		stmt1.set_expiry_from_parts(200, 1); // Expires at timestamp 200
		sign_with(&mut stmt1, 42);
		let hash1 = stmt1.hash();
		store.submit(stmt1, StatementSource::Network);

		let mut stmt2 = unsigned_statement(42, 2, Some(2), 100);
		stmt2.set_expiry_from_parts(300, 2); // Expires at timestamp 300
		sign_with(&mut stmt2, 42);
		let hash2 = stmt2.hash();
		store.submit(stmt2, StatementSource::Network);

		let mut stmt3 = unsigned_statement(42, 3, Some(3), 100);
		stmt3.set_expiry_from_parts(500, 3); // Expires at timestamp 500
		sign_with(&mut stmt3, 42);
		let hash3 = stmt3.hash();
		store.submit(stmt3, StatementSource::Network);

		// Verify all statements are in the store
		assert_eq!(store.statement_count(), 3);

		// Advance time to 250 (stmt1 should expire since 250 > 200)
		store.set_time(250);
		store.enforce_limits();

		{
			// Naturally expired statements are not added to the expired map.
			assert!(!store.is_evicted(&hash1), "stmt1 naturally expired, not in map");
			assert!(!store.is_evicted(&hash2), "stmt2 should not be expired yet");
			assert!(!store.is_evicted(&hash3), "stmt3 should not be expired yet");
			assert_eq!(store.statement_count(), 2);
		}

		// Advance time to 400 (stmt2 should also expire since 400 > 300)
		store.set_time(400);
		store.enforce_limits();

		{
			assert!(!store.is_evicted(&hash1));
			assert!(!store.is_evicted(&hash2), "stmt2 naturally expired, not in map");
			assert!(!store.is_evicted(&hash3), "stmt3 should not be expired yet");
			assert_eq!(store.statement_count(), 1);
		}

		// Check again at time 600 (stmt3 should expire since 600 > 500)
		store.set_time(600);
		store.enforce_limits();

		{
			assert!(!store.is_evicted(&hash1));
			assert!(!store.is_evicted(&hash2));
			assert!(!store.is_evicted(&hash3), "stmt3 naturally expired, not in map");
			assert_eq!(store.statement_count(), 0);
		}
	}

	#[test]
	fn check_expiration_does_nothing_when_no_expired_statements() {
		let (mut store, _temp) = test_store();
		store.set_time(1000);

		// Create statement with expiry far in the future
		// The statement() helper uses set_expiry_from_parts(u32::MAX, priority)
		let stmt = statement(1, 1, None, 100);
		let hash = stmt.hash();
		store.submit(stmt, StatementSource::Network);

		// Check expiration - nothing should happen
		store.enforce_limits();

		// Statement should still be there
		assert!(store.has_statement(&hash));
		assert!(!store.is_evicted(&hash));
		assert_eq!(store.statement_count(), 1);
		assert_eq!(store.evicted_count(), 0);
	}

	#[test]
	fn check_expiration_correctly_updates_account_data() {
		let (mut store, _temp) = test_store();
		store.set_time(100);

		// Create a statement with expiry at timestamp 200
		let mut stmt = unsigned_statement(1, 1, Some(1), 100);
		stmt.set_expiry_from_parts(200, 1);
		sign_with(&mut stmt, 1);
		let hash = stmt.hash();
		store.submit(stmt, StatementSource::Network);

		// Verify account exists before expiration
		{
			assert!(store.has_account(&account(1)));
			assert_eq!(store.total_size(), 100);
		}

		// Expire
		store.set_time(300);
		store.enforce_limits();

		// Verify account is removed after its only statement expires
		{
			assert!(
				!store.has_account(&account(1)),
				"Account should be removed when all its statements expire"
			);
			assert_eq!(store.total_size(), 0, "Total size should be zero");
			assert!(!store.is_evicted(&hash), "Naturally expired, not in map");
		}
	}

	#[test]
	fn check_expiration_clears_topic_and_key_indexes() {
		let (mut store, _temp) = test_store();
		store.set_time(100);

		// Create a statement with topic and decryption key
		let mut stmt = unsigned_statement(1, 1, Some(1), 100);
		stmt.set_expiry_from_parts(200, 1);
		stmt.set_topic(0, topic(42));
		stmt.set_decryption_key(dec_key(7));
		sign_with(&mut stmt, 1);
		let hash = stmt.hash();
		store.submit(stmt, StatementSource::Network);

		// Verify indexes are populated
		{
			assert!(store.index_has_topic(&topic(42), &hash));
			assert!(store.index_has_dec_key(&Some(dec_key(7)), &hash));
		}

		// Expire
		store.set_time(300);
		store.enforce_limits();

		// Verify indexes are cleared
		{
			assert!(!store.index_has_topic(&topic(42), &hash), "Topic index should be cleared");
			assert!(
				!store.index_has_dec_key(&Some(dec_key(7)), &hash),
				"Decryption key index should be cleared"
			);
			assert!(!store.is_evicted(&hash), "Naturally expired, not in map");
		}
	}

	#[test]
	fn check_expiration_handles_empty_store() {
		let (mut store, _temp) = test_store();
		store.set_time(1000);

		// With no statements, check_expiration should not panic
		store.enforce_limits();

		// Second call should also work
		store.enforce_limits();

		assert!(store.submit_index.read().allowance_cursor.is_none());
		assert_eq!(store.statement_count(), 0);
		assert_eq!(store.evicted_count(), 0);
	}

	#[test]
	fn check_expiration_expires_properly_formatted_statements() {
		// With the fix (Expiry(current_time << 32)), check_expiration properly
		// compares timestamps and can expire statements submitted through normal flow.

		let (mut store, _temp) = test_store();
		store.set_time(1000);

		// Create a statement with expiration timestamp just 1 second in the future
		let mut stmt = unsigned_statement(1, 1, None, 100);
		stmt.set_expiry_from_parts(1001, 1); // Expires at timestamp 1001
		sign_with(&mut stmt, 1);
		let hash = stmt.hash();
		store.submit(stmt, StatementSource::Network);

		assert_eq!(store.statement_count(), 1);

		// Advance time past the expiration timestamp
		store.set_time(2000);
		store.enforce_limits();

		// Statement SHOULD be expired because check_expiration now compares
		// Expiry(2000 << 32) against Expiry(1001 << 32 | 1), and
		// (2000 << 32) > (1001 << 32 | 1)
		assert!(
			!store.has_statement(&hash),
			"Statement should be removed from the store after expiration"
		);
		// Naturally expired: timestamp 1001 < current_time 2000, not added to expired map.
		assert!(!store.is_evicted(&hash), "Naturally expired, not in map");
	}

	#[test]
	fn check_expiration_updates_database_columns() {
		// This test verifies that check_expiration properly updates the database.
		let (mut store, _temp) = test_store();
		store.set_time(100);

		// Create a statement with expiry at timestamp 200
		let mut stmt = unsigned_statement(1, 1, None, 100);
		stmt.set_expiry_from_parts(200, 1);
		sign_with(&mut stmt, 1);
		let hash = stmt.hash();
		store.submit(stmt.clone(), StatementSource::Network);

		// Verify statement is in the database
		let db_entry = store.db.get(col::STATEMENTS, &hash).unwrap();
		assert!(db_entry.is_some(), "Statement should be in col::STATEMENTS after submit");

		// Advance time past expiry and run check_expiration
		store.set_time(300);
		store.enforce_limits();

		// Verify the store state is updated correctly
		{
			assert_eq!(store.statement_count(), 0, "Statement should be removed from the store");
			// Naturally expired: not added to expired map, no need for suppression.
			assert!(
				!store.is_evicted(&hash),
				"Naturally expired statement must not be in the expired map"
			);
		}

		let db_entry = store.db.get(col::STATEMENTS, &hash).unwrap();
		assert!(
			db_entry.is_none(),
			"Statement should be removed from col::STATEMENTS after expiration"
		);

		// Naturally expired statements are not written to col::EXPIRED either, so that
		// the optimization survives node restarts.
		let expired_entry = store.db.get(col::EXPIRED, &hash).unwrap();
		assert!(expired_entry.is_none(), "Naturally expired: not written to col::EXPIRED");
	}

	#[test]
	fn enforce_allowances_evicts_excess_statements() {
		// This test verifies that check_expiration correctly evicts statements
		// when statements exceed the current allowance. We directly insert into
		// the index (bypassing submit's validation) to simulate statements that
		// existed before allowances were reduced.
		let (mut store, _temp) = test_store();
		store.set_time(0);

		// Account 4 has allowance (4 statements, 1000 bytes) from TestClient
		let s1 = statement(4, 10, None, 100); // lowest priority - will be evicted
		let s2 = statement(4, 20, None, 100);
		let s3 = statement(4, 30, None, 100);
		let s4 = statement(4, 40, None, 100);
		let s5 = statement(4, 50, None, 100); // highest priority

		let h1 = s1.hash();
		let h5 = s5.hash();

		// Directly insert into the store, bypassing `submit`'s allowance check
		for statement in [&s1, &s2, &s3, &s4, &s5] {
			store.force_insert(statement);
		}

		// Verify initial state - all 5 should be present
		assert_eq!(store.statement_count(), 5);
		assert_eq!(store.total_size(), 500);

		// Run check_expiration which handles both expiration and allowance enforcement
		// Since account 4 has max_count=4, one statement should be evicted
		store.enforce_limits();

		// Should evict the lowest priority statement (s1)
		assert_eq!(store.statement_count(), 4, "Should have 4 statements after eviction");
		assert!(!store.has_statement(&h1), "Lowest priority should be evicted");
		assert!(store.has_statement(&h5), "Highest priority should remain");
		assert_eq!(store.total_size(), 400);

		// Evicted statement should be marked as expired
		assert!(store.is_evicted(&h1));
	}

	#[test]
	fn enforce_allowances_evicts_all_when_no_allowance_found() {
		let (mut store, _temp) = test_store();
		store.set_time(0);

		// Account 0 has NO allowance in TestClient
		let s1 = statement(0, 10, None, 100);
		let s2 = statement(0, 20, None, 150);

		let h1 = s1.hash();
		let h2 = s2.hash();

		// Directly insert statements for account with no allowance
		store.force_insert(&s1);
		store.force_insert(&s2);

		assert_eq!(store.statement_count(), 2);

		// Run check_expiration - should evict ALL statements since no allowance exists
		store.enforce_limits();

		assert_eq!(store.statement_count(), 0, "All statements should be evicted");
		assert!(!store.has_account(&account(0)), "Account should be removed");
		assert!(store.is_evicted(&h1));
		assert!(store.is_evicted(&h2));
	}

	#[test]
	fn enforce_allowances_based_on_size() {
		// This test verifies that check_expiration evicts based on size limits.
		let (mut store, _temp) = test_store();
		store.set_time(0);

		// Account 2 has allowance (2, 1000) from TestClient
		// Insert 2 statements that together exceed 1000 bytes
		let s1 = statement(2, 10, None, 600); // lowest priority
		let s2 = statement(2, 20, None, 600); // higher priority

		let h1 = s1.hash();
		let h2 = s2.hash();

		// Directly insert both statements (total 1200 bytes > 1000 limit)
		store.force_insert(&s1);
		store.force_insert(&s2);

		assert_eq!(store.total_size(), 1200);

		// Run check_expiration - should evict s1 to get under 1000 bytes
		store.enforce_limits();

		assert_eq!(store.statement_count(), 1);
		assert!(store.has_statement(&h2), "Higher priority should remain");
		assert!(!store.has_statement(&h1), "Lower priority should be evicted");
		assert_eq!(store.total_size(), 600);
	}

	#[test]
	fn enforce_allowances_takes_larger_of_best_and_finalized() {
		// (max_count, max_size) at the finalized and the best block: the count, then the size,
		// raised and lowered in a best block not finalized yet
		let cases = [
			((2, 1000), (3, 1000)),
			((3, 1000), (2, 1000)),
			((4, 200), (4, 300)),
			((4, 300), (4, 200)),
		];
		for (finalized, best) in cases {
			let (mut store, _temp) = test_store();
			store.set_time(0);
			store.read_allowance_fn = Box::new(move |_who: &AccountId, block: AllowanceBlock| {
				let (max_count, max_size) = match block {
					AllowanceBlock::Finalized => finalized,
					AllowanceBlock::Best => best,
				};
				Ok(Some(StatementAllowance::new(max_count, max_size)))
			});
			let statements = [10, 20, 30, 40].map(|priority| statement(7, priority, None, 100));
			for statement in &statements {
				store.force_insert(statement);
			}

			store.enforce_limits();

			assert_eq!(store.statement_count(), 3);
			assert!(store.is_evicted(&statements[0].hash()));
		}
	}

	#[test]
	fn channel_replacement_only_higher_priority_succeeds() {
		let (store, _temp) = test_store();
		let source = StatementSource::Network;

		// Account 1: max_count=1, max_size=1000
		// Submit channel 1 with priority 5
		let s1 = statement(1, 5, Some(1), 100);
		let h1 = s1.hash();
		assert_eq!(store.submit(s1, source), SubmitResult::New);

		// Lower priority on same channel → ChannelPriorityTooLow
		let result = store.submit(statement(1, 3, Some(1), 100), source);
		assert!(
			matches!(result, SubmitResult::Rejected(RejectionReason::ChannelPriorityTooLow { .. })),
			"Lower priority should be rejected with ChannelPriorityTooLow, got: {result:?}"
		);

		// Equal priority on same channel → ChannelPriorityTooLow (check is <=)
		// Use different data_len to get a distinct hash with same priority
		let result = store.submit(statement(1, 5, Some(1), 101), source);
		assert!(
			matches!(result, SubmitResult::Rejected(RejectionReason::ChannelPriorityTooLow { .. })),
			"Equal priority should be rejected with ChannelPriorityTooLow, got: {result:?}"
		);

		// Higher priority on same channel → replaces
		let s2 = statement(1, 10, Some(1), 200);
		let h2 = s2.hash();
		assert_eq!(store.submit(s2, source), SubmitResult::New);

		{
			assert_eq!(store.statement_count(), 1);
			assert!(!store.has_statement(&h1), "Old channel message should be gone");
			assert!(store.has_statement(&h2), "New channel message should exist");
			assert!(store.is_evicted(&h1), "Old should be in expired");
			assert_eq!(store.total_size(), 200);
		}
	}

	#[test]
	fn submit_rejects_malformed_statements() {
		let (store, _temp) = test_store();

		let mut base = Statement::new();
		base.set_expiry(u64::MAX);
		base.set_plain_data(vec![1]);

		let ed_kp = sp_core::ed25519::Pair::from_string("//Alice", None).unwrap();
		let sr_kp = sp_core::sr25519::Pair::from_string("//Alice", None).unwrap();
		let ecdsa_kp = sp_core::ecdsa::Pair::from_string("//Alice", None).unwrap();

		assert_eq!(
			store.submit(base.clone(), StatementSource::Network),
			SubmitResult::Invalid(InvalidReason::NoProof)
		);

		let bad_proofs = [
			Proof::Ed25519 { signature: [0xAB; 64], signer: ed_kp.public().0 },
			Proof::Sr25519 { signature: [0xCD; 64], signer: sr_kp.public().0 },
			Proof::Secp256k1Ecdsa { signature: [0xEF; 65], signer: ecdsa_kp.public().0 },
		];
		for proof in bad_proofs {
			let mut s = base.clone();
			s.set_proof(proof);
			assert_eq!(
				store.submit(s, StatementSource::Network),
				SubmitResult::Invalid(InvalidReason::BadProof)
			);
		}

		let mut wrong_signer = base.clone();
		wrong_signer.sign_ed25519_private(&ed_kp);
		let alice_sig = match wrong_signer.proof().unwrap() {
			Proof::Ed25519 { signature, .. } => *signature,
			_ => panic!("expected Ed25519 proof after sign_ed25519_private"),
		};
		let bob_kp = sp_core::ed25519::Pair::from_string("//Bob", None).unwrap();
		wrong_signer.set_proof(Proof::Ed25519 { signature: alice_sig, signer: bob_kp.public().0 });
		assert_eq!(
			store.submit(wrong_signer, StatementSource::Network),
			SubmitResult::Invalid(InvalidReason::BadProof)
		);
	}

	#[test]
	fn channel_replacement_with_size_increase_evicts_others() {
		let (store, _temp) = test_store();
		let source = StatementSource::Network;

		// Account 3: max_count=3, max_size=1000
		// channel msg (200b) + two non-channel msgs (300b each) = 800b
		let s_ch = statement(3, 5, Some(1), 200);
		let s_low = statement(3, 2, None, 300);
		let s_mid = statement(3, 3, None, 300);
		let h_ch = s_ch.hash();
		let h_low = s_low.hash();
		let h_mid = s_mid.hash();

		assert_eq!(store.submit(s_ch, source), SubmitResult::New);
		assert_eq!(store.submit(s_low, source), SubmitResult::New);
		assert_eq!(store.submit(s_mid, source), SubmitResult::New);
		assert_eq!(store.total_size(), 800);

		// Replace channel with 600b message (priority 10 > 5)
		// Must evict lowest priority non-channel statement (priority 2) to fit
		let s_ch_big = statement(3, 10, Some(1), 600);
		let h_ch_big = s_ch_big.hash();
		assert_eq!(store.submit(s_ch_big, source), SubmitResult::New);

		{
			assert_eq!(store.statement_count(), 2);
			assert!(!store.has_statement(&h_ch), "Old channel message replaced");
			assert!(!store.has_statement(&h_low), "Priority 2 evicted to fit size");
			assert!(store.has_statement(&h_mid), "Priority 3 should remain");
			assert!(store.has_statement(&h_ch_big), "New channel message added");
			assert_eq!(store.total_size(), 900); // 300 (mid) + 600 (new channel)
		}
	}

	#[test]
	fn subscription_reconnect_receives_current_state() {
		use crate::StatementStoreSubscriptionApi;
		use sp_statement_store::OptimizedTopicFilter;

		let (store, _temp) = test_store();
		let source = StatementSource::Local;

		// Submit 3 statements
		for i in 0..3u8 {
			let res = store.submit(signed_statement(i), source);
			assert_eq!(res, SubmitResult::New);
		}

		// First subscribe → should get 3 existing statements
		let (existing, sender, stream) =
			store.subscribe_statement(OptimizedTopicFilter::Any).unwrap();
		assert_eq!(existing.len(), 3, "First subscribe should return 3 existing statements");

		// Drop stream
		drop(stream);
		drop(sender);

		// Submit 2 more while disconnected
		for i in 3..5u8 {
			assert_eq!(store.submit(signed_statement(i), source), SubmitResult::New);
		}
		let (existing, sender, stream) =
			store.subscribe_statement(OptimizedTopicFilter::Any).unwrap();
		assert_eq!(existing.len(), 5, "Re-subscribe should return all 5 current statements");

		// Drop and remove one statement
		drop(stream);
		drop(sender);
		let hash_to_remove = signed_statement(0).hash();
		store.remove(&hash_to_remove).unwrap();

		// Re-subscribe → should get 4
		let (existing, _sender, _stream) =
			store.subscribe_statement(OptimizedTopicFilter::Any).unwrap();
		assert_eq!(existing.len(), 4, "Re-subscribe after removal should return 4 statements");
	}

	#[test]
	fn subscription_reconnect_with_topic_filter() {
		use crate::StatementStoreSubscriptionApi;
		use sp_statement_store::OptimizedTopicFilter;

		let (store, _temp) = test_store();
		let source = StatementSource::Local;
		let topic_a = topic(1);
		let topic_b = topic(2);

		// s1: topic A only
		let s1 = signed_statement_with_topics(1, &[topic_a], None);
		// s2: topic B only
		let s2 = signed_statement_with_topics(2, &[topic_b], None);
		// s3: topics A + B
		let s3 = signed_statement_with_topics(3, &[topic_a, topic_b], None);

		assert_eq!(store.submit(s1, source), SubmitResult::New);
		assert_eq!(store.submit(s2, source), SubmitResult::New);
		assert_eq!(store.submit(s3, source), SubmitResult::New);

		// Subscribe with MatchAll([A]) → s1, s3
		let filter_a = OptimizedTopicFilter::MatchAll(std::collections::HashSet::from([topic_a]));
		let (existing, sender, stream) = store.subscribe_statement(filter_a.clone()).unwrap();
		assert_eq!(existing.len(), 2, "MatchAll([A]) should match s1 and s3");

		// Drop and add s4 with topic A
		drop(sender);
		drop(stream);
		let s4 = signed_statement_with_topics(4, &[topic_a], None);
		assert_eq!(store.submit(s4, source), SubmitResult::New);
		// Re-subscribe with same filter → s1, s3, s4
		let (existing, sender, stream) = store.subscribe_statement(filter_a).unwrap();
		assert_eq!(existing.len(), 3, "Re-subscribe MatchAll([A]) should return s1, s3, s4");

		// Drop and re-subscribe with different filter MatchAll([B]) → s2, s3
		drop(sender);
		drop(stream);
		let filter_b = OptimizedTopicFilter::MatchAll(std::collections::HashSet::from([topic_b]));
		let (existing, _sender, _stream) = store.subscribe_statement(filter_b).unwrap();
		assert_eq!(existing.len(), 2, "Re-subscribe MatchAll([B]) should return s2 and s3");
	}

	#[tokio::test]
	async fn subscription_delivers_each_statement_exactly_once_across_boundary() {
		// Exactly-once: a statement existing before the subscription is delivered only through the
		// initial snapshot, and a statement submitted afterwards only through the live stream —
		// never both (which was the at-least-once regression) and never neither.
		use crate::StatementStoreSubscriptionApi;
		use futures::StreamExt;
		use sp_statement_store::{OptimizedTopicFilter, StatementEvent};

		let (store, _temp) = test_store();
		let source = StatementSource::Local;

		// Two statements exist before the subscription is created.
		let a = signed_statement(0);
		let b = signed_statement(1);
		assert_eq!(store.submit(a.clone(), source), SubmitResult::New);
		assert_eq!(store.submit(b.clone(), source), SubmitResult::New);

		// The snapshot must contain exactly the pre-existing statements.
		let (existing, _sender, mut stream) =
			store.subscribe_statement(OptimizedTopicFilter::Any).unwrap();
		let mut snapshot: Vec<Statement> = existing
			.iter()
			.map(|bytes| Statement::decode(&mut &bytes[..]).unwrap())
			.collect();
		snapshot.sort_by_key(|s| s.hash());
		let mut expected_snapshot = vec![a.clone(), b.clone()];
		expected_snapshot.sort_by_key(|s| s.hash());
		assert_eq!(
			snapshot, expected_snapshot,
			"snapshot must contain exactly the pre-existing statements"
		);

		// A statement submitted after the subscription must arrive on the live stream, exactly
		// once.
		let c = signed_statement(2);
		assert_eq!(store.submit(c.clone(), source), SubmitResult::New);

		let event = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
			.await
			.expect("live statement should arrive within the timeout")
			.expect("stream should yield an event");
		let StatementEvent::NewStatements { statements, .. } = event;
		let live: Vec<Statement> = statements
			.iter()
			.map(|bytes| Statement::decode(&mut &bytes.0[..]).unwrap())
			.collect();
		assert_eq!(
			live,
			vec![c.clone()],
			"live stream must deliver exactly the post-subscribe statement"
		);

		// No duplicate: neither the new statement nor the snapshot statements are delivered again.
		assert!(
			tokio::time::timeout(std::time::Duration::from_millis(500), stream.next())
				.await
				.is_err(),
			"no further (duplicate) statements must be delivered"
		);
	}

	#[test]
	fn subscription_snapshot_deduplicates_multi_topic_match_any() {
		// A `MatchAny` snapshot must contain a statement matching several of the filter's topics
		// only once, not once per matching topic.
		use crate::StatementStoreSubscriptionApi;
		use sp_statement_store::OptimizedTopicFilter;

		let (store, _temp) = test_store();
		let topic_a = topic(1);
		let topic_b = topic(2);

		// A statement carrying BOTH topics.
		let s = signed_statement_with_topics(1, &[topic_a, topic_b], None);
		assert_eq!(store.submit(s, StatementSource::Local), SubmitResult::New);

		let filter =
			OptimizedTopicFilter::MatchAny(std::collections::HashSet::from([topic_a, topic_b]));
		let (existing, _sender, _stream) = store.subscribe_statement(filter).unwrap();
		assert_eq!(
			existing.len(),
			1,
			"MatchAny snapshot must not duplicate a multi-topic statement"
		);
	}

	#[tokio::test]
	async fn subscription_match_all_delivers_exactly_once_across_boundary() {
		// The `MatchAll` snapshot is enumerated authoritatively from disk, so a matching statement
		// present before the subscription is delivered via the snapshot (never lost to the
		// in-memory counters/cache lagging a commit), while one submitted afterwards is delivered
		// live exactly once.
		use crate::StatementStoreSubscriptionApi;
		use futures::StreamExt;
		use sp_statement_store::{OptimizedTopicFilter, StatementEvent};

		let (store, _temp) = test_store();
		let source = StatementSource::Local;
		let t = topic(7);

		// Two matching statements exist before the subscription.
		let a = signed_statement_with_topics(0, &[t], None);
		let b = signed_statement_with_topics(1, &[t], None);
		assert_eq!(store.submit(a, source), SubmitResult::New);
		assert_eq!(store.submit(b, source), SubmitResult::New);

		let filter = OptimizedTopicFilter::MatchAll(std::collections::HashSet::from([t]));
		let (existing, _sender, mut stream) = store.subscribe_statement(filter).unwrap();
		assert_eq!(
			existing.len(),
			2,
			"MatchAll snapshot must contain both pre-existing statements"
		);

		// A matching statement submitted afterwards must arrive live, exactly once.
		let c = signed_statement_with_topics(2, &[t], None);
		assert_eq!(store.submit(c.clone(), source), SubmitResult::New);

		let event = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
			.await
			.expect("live statement should arrive within the timeout")
			.expect("stream should yield an event");
		let StatementEvent::NewStatements { statements, .. } = event;
		let live: Vec<Statement> = statements
			.iter()
			.map(|bytes| Statement::decode(&mut &bytes.0[..]).unwrap())
			.collect();
		assert_eq!(
			live,
			vec![c],
			"MatchAll live stream must deliver exactly the post-subscribe statement"
		);

		assert!(
			tokio::time::timeout(std::time::Duration::from_millis(500), stream.next())
				.await
				.is_err(),
			"no further (duplicate) statements must be delivered"
		);
	}

	#[tokio::test]
	async fn subscription_no_loss_or_duplicate_under_concurrent_submits() {
		// Race a stream of submissions against the subscription registration: whatever the
		// interleaving, every matching statement must be delivered exactly once across the snapshot
		// and the live stream — no loss (the bug the on-disk `MatchAll` snapshot could cause) and
		// no duplicate (the original at-least-once regression). This assertion holds for any
		// timing, so the test is deterministic even though the race window is hit
		// non-deterministically.
		use crate::StatementStoreSubscriptionApi;
		use futures::StreamExt;
		use sp_statement_store::{OptimizedTopicFilter, StatementEvent};
		use std::collections::HashSet;

		let (store, _temp) = test_store();
		let store = std::sync::Arc::new(store);
		let t = topic(9);
		// Keep well under SUBSCRIPTION_BUFFER_SIZE (128) so the live channel never overflows (which
		// would auto-unsubscribe and legitimately drop statements).
		const N: u8 = 60;

		let all: Vec<Statement> =
			(0..N).map(|i| signed_statement_with_topics(i, &[t], None)).collect();
		let all_hashes: HashSet<_> = all.iter().map(|s| s.hash()).collect();

		// A handful exist before subscribing; the rest are submitted concurrently with the
		// subscribe.
		let split = 10usize;
		for s in &all[..split] {
			assert_eq!(store.submit(s.clone(), StatementSource::Local), SubmitResult::New);
		}
		let store2 = store.clone();
		let rest: Vec<Statement> = all[split..].to_vec();
		let submitter = std::thread::spawn(move || {
			for s in rest {
				let _ = store2.submit(s, StatementSource::Local);
			}
		});

		let filter = OptimizedTopicFilter::MatchAll(HashSet::from([t]));
		let (existing, _sender, mut stream) = store.subscribe_statement(filter).unwrap();

		submitter.join().unwrap();

		// Everything delivered so far, snapshot first.
		let mut seen = existing
			.iter()
			.map(|b| Statement::decode(&mut &b[..]).unwrap().hash())
			.collect::<Vec<_>>();
		// Drain the live stream until every statement is accounted for (or a timeout on loss).
		while seen.len() < N as usize {
			match tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await {
				Ok(Some(StatementEvent::NewStatements { statements, .. })) => {
					for b in statements {
						seen.push(Statement::decode(&mut &b.0[..]).unwrap().hash());
					}
				},
				_ => break,
			}
		}

		let seen_set: HashSet<_> = seen.iter().copied().collect();
		assert_eq!(
			seen.len(),
			N as usize,
			"each statement must be delivered exactly once (got {} deliveries for {} statements — loss or duplicate)",
			seen.len(),
			N
		);
		assert_eq!(seen_set, all_hashes, "delivered set must equal submitted set");
	}

	// Tests for the multi-filter subscription API (`MultiFilterSubscriptionApi` /
	// `create_subscription`), as opposed to the single-filter `subscribe_statement` tests above.
	mod multi_filter {
		use super::*;
		use crate::{
			MultiFilterEventStream, MultiFilterSubscriptionApi, MultiFilterSubscriptionEvent,
		};
		use futures::StreamExt;
		use sp_statement_store::OptimizedTopicFilter;
		use std::{collections::HashSet, sync::Arc, time::Duration};

		fn arc_test_store() -> (Arc<Store>, tempfile::TempDir) {
			let (store, dir) = test_store();
			(Arc::new(store), dir)
		}

		async fn drain_all(
			stream: &mut MultiFilterEventStream,
			idle: Duration,
		) -> Vec<MultiFilterSubscriptionEvent> {
			let mut events = Vec::new();
			while let Ok(Some(event)) = tokio::time::timeout(idle, stream.next()).await {
				events.push(event);
			}
			events
		}

		#[test]
		fn replay_watermark_excludes_later_admissions() {
			let (store, _dir) = test_store();
			let before = signed_statement(1);
			let after = signed_statement(2);
			assert_eq!(store.submit(before.clone(), StatementSource::Local), SubmitResult::New);

			let mut watermark = None;
			assert!(store
				.register_replay(&mut |captured| {
					watermark = Some(captured);
					true
				})
				.unwrap()
				.is_some());
			assert_eq!(store.submit(after.clone(), StatementSource::Local), SubmitResult::New);

			let replay =
				store.replay_batch(&OptimizedTopicFilter::Any, 0, watermark.unwrap()).unwrap();
			let hashes: HashSet<_> = replay
				.statements
				.iter()
				.map(|encoded| Statement::decode(&mut encoded.as_slice()).unwrap().hash())
				.collect();
			assert!(replay.done);
			assert!(hashes.contains(&before.hash()));
			assert!(!hashes.contains(&after.hash()));
		}

		#[tokio::test]
		async fn add_filter_replays_snapshot_and_delivers_later_submissions_live() {
			let (store, _dir) = arc_test_store();
			let (handle, mut stream) = store.create_subscription();

			const NUM_STATEMENTS: u8 = 50;
			const NUM_PRE_FILTER: u8 = 20;

			// Statements stored before the filter is attached; the replay must cover exactly these.
			for i in 0..NUM_PRE_FILTER {
				let stmt = signed_statement(i);
				assert_eq!(store.submit(stmt, StatementSource::Local), SubmitResult::New);
			}

			let filter_id = handle.add_filter(OptimizedTopicFilter::Any).unwrap();

			// The watermark is captured atomically with the filter registration, so statements
			// submitted after `add_filter` returns sit above it and arrive as live events only.
			for i in NUM_PRE_FILTER..NUM_STATEMENTS {
				let stmt = signed_statement(i);
				assert_eq!(store.submit(stmt, StatementSource::Local), SubmitResult::New);
			}

			let mut snapshot_hashes: HashSet<[u8; 32]> = HashSet::new();
			let mut live_with_filter: HashSet<[u8; 32]> = HashSet::new();
			for event in drain_all(&mut stream, Duration::from_millis(300)).await {
				match event {
					MultiFilterSubscriptionEvent::ReplayStatements { statements, .. } => {
						snapshot_hashes.extend(
							statements
								.iter()
								.map(|bytes| Statement::decode(&mut &bytes[..]).unwrap().hash()),
						);
					},
					MultiFilterSubscriptionEvent::NewStatement(event)
						if event.matched_filter_ids.contains(&filter_id) =>
					{
						live_with_filter.insert(event.hash);
					},
					_ => {},
				}
			}

			let expected_replayed: HashSet<[u8; 32]> =
				(0..NUM_PRE_FILTER).map(|i| signed_statement(i).hash()).collect();
			let expected_live: HashSet<[u8; 32]> =
				(NUM_PRE_FILTER..NUM_STATEMENTS).map(|i| signed_statement(i).hash()).collect();

			assert_eq!(snapshot_hashes, expected_replayed);
			assert_eq!(live_with_filter, expected_live);
		}
	}
}
