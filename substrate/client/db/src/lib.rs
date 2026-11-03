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

//! Client backend that is backed by a database.
//!
//! # Canonicality vs. Finality
//!
//! Finality indicates that a block will not be reverted, according to the consensus algorithm,
//! while canonicality indicates that the block may be reverted, but we will be unable to do so,
//! having discarded heavy state that will allow a chain reorganization.
//!
//! Finality implies canonicality but not vice-versa.

#![warn(missing_docs)]

pub mod offchain;

pub mod bench;

mod children;
mod parity_db;
mod pinned_blocks_cache;
mod record_stats_state;
mod stats;
#[cfg(any(feature = "rocksdb", test))]
mod upgrade;
mod utils;

use linked_hash_map::LinkedHashMap;
use log::{debug, trace, warn};
use parking_lot::{Mutex, RwLock};
use prometheus_endpoint::Registry;
use std::{
	collections::{HashMap, HashSet},
	io,
	path::{Path, PathBuf},
	sync::Arc,
};

use crate::{
	pinned_blocks_cache::PinnedBlocksCache,
	record_stats_state::RecordStatsState,
	stats::StateUsageStats,
	utils::{meta_keys, read_db, read_meta, remove_from_db, DatabaseType, Meta},
};
use codec::{Decode, Encode};
use hash_db::Prefix;
use sc_client_api::{
	backend::NewBlockState,
	blockchain::{BlockGap, BlockGapType},
	leaves::{FinalizationOutcome, LeafSet},
	utils::is_descendent_of,
	IoInfo, MemoryInfo, MemorySize, TrieCacheContext, UsageInfo,
};
use sc_state_db::{IsPruned, LastCanonicalized, StateDb};
use sp_arithmetic::traits::Saturating;
use sp_blockchain::{
	Backend as _, CachedHeaderMetadata, DisplacedLeavesAfterFinalization, Error as ClientError,
	HeaderBackend, HeaderMetadata, HeaderMetadataCache, Result as ClientResult,
};
use sp_core::{
	offchain::OffchainOverlayedChange,
	storage::{well_known_keys, ChildInfo},
};
use sp_database::Transaction;
use sp_runtime::{
	generic::BlockId,
	traits::{
		Block as BlockT, Hash, HashingFor, Header as HeaderT, NumberFor, One, SaturatedConversion,
		Zero,
	},
	Justification, Justifications, StateVersion, Storage,
};
use sp_state_machine::{
	backend::{AsTrieBackend, Backend as StateBackend},
	BackendTransaction, ChildStorageCollection, DBValue, IndexOperation, IterArgs,
	OffchainChangesCollection, StateMachineStats, StorageCollection, StorageIterator, StorageKey,
	StorageValue, UsageInfo as StateUsageInfo,
};
use sp_trie::{cache::SharedTrieCache, prefixed_key, MemoryDB, MerkleValue, PrefixedMemoryDB};
use utils::BLOCK_GAP_CURRENT_VERSION;

// Re-export the Database trait so that one can pass an implementation of it.
pub use sc_state_db::PruningMode;
pub use sp_database::Database;

pub use bench::BenchmarkingState;

/// Filter to determine if a block should be excluded from pruning.
///
/// Note: This filter only affects **block body** (and future header) pruning.
/// It does **not** affect state pruning, which is configured separately.
pub trait PruningFilter: Send + Sync {
	/// Check if a block with the given justifications should be preserved.
	///
	/// Returns `true` to preserve the block, `false` to allow pruning.
	fn should_retain(&self, justifications: &Justifications) -> bool;
}

impl<F> PruningFilter for F
where
	F: Fn(&Justifications) -> bool + Send + Sync,
{
	fn should_retain(&self, justifications: &Justifications) -> bool {
		(self)(justifications)
	}
}

const CACHE_HEADERS: usize = 8;

/// DB-backed patricia trie state, transaction type is an overlay of changes to commit.
pub type DbState<H> = sp_state_machine::TrieBackend<Arc<dyn sp_state_machine::Storage<H>>, H>;

/// Builder for [`DbState`].
pub type DbStateBuilder<Hasher> =
	sp_state_machine::TrieBackendBuilder<Arc<dyn sp_state_machine::Storage<Hasher>>, Hasher>;

/// Length of a [`DbHash`].
const DB_HASH_LEN: usize = 32;

/// Hash type that this backend uses for the database.
pub type DbHash = sp_core::H256;

/// An extrinsic entry in the database.
#[derive(Debug, Encode, Decode)]
enum DbExtrinsic<B: BlockT> {
	/// Extrinsic that contains indexed data.
	Indexed {
		/// Hash of the indexed part.
		hash: DbHash,
		/// Extrinsic header.
		header: Vec<u8>,
	},
	/// Complete extrinsic data.
	Full(B::Extrinsic),
	/// Extrinsic that renews multiple indexed data items within a single call.
	///
	/// `hashes` is in submission order: the proof-of-storage inherent provider
	/// walks `block_indexed_body` linearly and the runtime indexes a parallel
	/// `Vec<TransactionInfo>` by the same position, so reordering here would
	/// desync proof construction from verification.
	MultiRenew {
		/// Submission order; see variant docs.
		hashes: Vec<DbHash>,
		extrinsic: Vec<u8>,
	},
}

/// A reference tracking state.
///
/// It makes sure that the hash we are using stays pinned in storage
/// until this structure is dropped.
pub struct RefTrackingState<Block: BlockT> {
	state: DbState<HashingFor<Block>>,
	storage: Arc<StorageDb<Block>>,
	parent_hash: Option<Block::Hash>,
}

impl<B: BlockT> RefTrackingState<B> {
	fn new(
		state: DbState<HashingFor<B>>,
		storage: Arc<StorageDb<B>>,
		parent_hash: Option<B::Hash>,
	) -> Self {
		RefTrackingState { state, parent_hash, storage }
	}
}

impl<B: BlockT> Drop for RefTrackingState<B> {
	fn drop(&mut self) {
		if let Some(hash) = &self.parent_hash {
			self.storage.state_db.unpin(hash);
		}
	}
}

impl<Block: BlockT> std::fmt::Debug for RefTrackingState<Block> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "Block {:?}", self.parent_hash)
	}
}

/// A raw iterator over the `RefTrackingState`.
pub struct RawIter<B: BlockT> {
	inner: <DbState<HashingFor<B>> as StateBackend<HashingFor<B>>>::RawIter,
}

impl<B: BlockT> StorageIterator<HashingFor<B>> for RawIter<B> {
	type Backend = RefTrackingState<B>;
	type Error = <DbState<HashingFor<B>> as StateBackend<HashingFor<B>>>::Error;

	fn next_key(&mut self, backend: &Self::Backend) -> Option<Result<StorageKey, Self::Error>> {
		self.inner.next_key(&backend.state)
	}

	fn next_pair(
		&mut self,
		backend: &Self::Backend,
	) -> Option<Result<(StorageKey, StorageValue), Self::Error>> {
		self.inner.next_pair(&backend.state)
	}

	fn was_complete(&self) -> bool {
		self.inner.was_complete()
	}
}

impl<B: BlockT> StateBackend<HashingFor<B>> for RefTrackingState<B> {
	type Error = <DbState<HashingFor<B>> as StateBackend<HashingFor<B>>>::Error;
	type TrieBackendStorage =
		<DbState<HashingFor<B>> as StateBackend<HashingFor<B>>>::TrieBackendStorage;
	type RawIter = RawIter<B>;

	fn storage(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		self.state.storage(key)
	}

	fn storage_hash(&self, key: &[u8]) -> Result<Option<B::Hash>, Self::Error> {
		self.state.storage_hash(key)
	}

	fn child_storage(
		&self,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Result<Option<Vec<u8>>, Self::Error> {
		self.state.child_storage(child_info, key)
	}

	fn child_storage_hash(
		&self,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Result<Option<B::Hash>, Self::Error> {
		self.state.child_storage_hash(child_info, key)
	}

	fn closest_merkle_value(
		&self,
		key: &[u8],
	) -> Result<Option<MerkleValue<B::Hash>>, Self::Error> {
		self.state.closest_merkle_value(key)
	}

	fn child_closest_merkle_value(
		&self,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Result<Option<MerkleValue<B::Hash>>, Self::Error> {
		self.state.child_closest_merkle_value(child_info, key)
	}

	fn exists_storage(&self, key: &[u8]) -> Result<bool, Self::Error> {
		self.state.exists_storage(key)
	}

	fn exists_child_storage(
		&self,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Result<bool, Self::Error> {
		self.state.exists_child_storage(child_info, key)
	}

	fn next_storage_key(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		self.state.next_storage_key(key)
	}

	fn next_child_storage_key(
		&self,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Result<Option<Vec<u8>>, Self::Error> {
		self.state.next_child_storage_key(child_info, key)
	}

	fn storage_root<'a>(
		&self,
		delta: impl Iterator<Item = (&'a [u8], Option<&'a [u8]>)>,
		state_version: StateVersion,
	) -> (B::Hash, BackendTransaction<HashingFor<B>>) {
		self.state.storage_root(delta, state_version)
	}

	fn child_storage_root<'a>(
		&self,
		child_info: &ChildInfo,
		delta: impl Iterator<Item = (&'a [u8], Option<&'a [u8]>)>,
		state_version: StateVersion,
	) -> (B::Hash, bool, BackendTransaction<HashingFor<B>>) {
		self.state.child_storage_root(child_info, delta, state_version)
	}

	fn raw_iter(&self, args: IterArgs) -> Result<Self::RawIter, Self::Error> {
		self.state.raw_iter(args).map(|inner| RawIter { inner })
	}

	fn register_overlay_stats(&self, stats: &StateMachineStats) {
		self.state.register_overlay_stats(stats);
	}

	fn usage_info(&self) -> StateUsageInfo {
		self.state.usage_info()
	}
}

impl<B: BlockT> AsTrieBackend<HashingFor<B>> for RefTrackingState<B> {
	type TrieBackendStorage =
		<DbState<HashingFor<B>> as StateBackend<HashingFor<B>>>::TrieBackendStorage;

	fn as_trie_backend(
		&self,
	) -> &sp_state_machine::TrieBackend<Self::TrieBackendStorage, HashingFor<B>> {
		&self.state.as_trie_backend()
	}
}

/// Database settings.
pub struct DatabaseSettings {
	/// The maximum trie cache size in bytes.
	///
	/// If `None` is given, the cache is disabled.
	pub trie_cache_maximum_size: Option<usize>,
	/// Requested state pruning mode.
	pub state_pruning: Option<PruningMode>,
	/// Where to find the database.
	pub source: DatabaseSource,
	/// Block pruning mode.
	///
	/// NOTE: only finalized blocks are subject for removal!
	pub blocks_pruning: BlocksPruning,
	/// Filters to exclude blocks from pruning.
	///
	/// If any filter returns `true` for a block's justifications, the block body
	/// (and in the future, the header) will be preserved even when it falls
	/// outside the pruning window. Does not affect state pruning.
	pub pruning_filters: Vec<Arc<dyn PruningFilter>>,
	/// Prometheus metrics registry.
	pub metrics_registry: Option<Registry>,
}

/// Block pruning settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BlocksPruning {
	/// Keep full block history, of every block that was ever imported.
	KeepAll,
	/// Keep full finalized block history.
	KeepFinalized,
	/// Keep N recent finalized blocks.
	Some(u32),
}

impl BlocksPruning {
	/// True if this is an archive pruning mode (either KeepAll or KeepFinalized).
	pub fn is_archive(&self) -> bool {
		match *self {
			BlocksPruning::KeepAll | BlocksPruning::KeepFinalized => true,
			BlocksPruning::Some(_) => false,
		}
	}
}

/// Where to find the database..
#[derive(Debug, Clone)]
pub enum DatabaseSource {
	/// Check given path, and see if there is an existing database there. If it's either `RocksDb`
	/// or `ParityDb`, use it. If there is none, create a new instance of `ParityDb`.
	Auto {
		/// Path to the paritydb database.
		paritydb_path: PathBuf,
		/// Path to the rocksdb database.
		rocksdb_path: PathBuf,
		/// Cache size in MiB. Used only by `RocksDb` variant of `DatabaseSource`.
		cache_size: usize,
	},
	/// Load a RocksDB database from a given path. Recommended for most uses.
	#[cfg(feature = "rocksdb")]
	RocksDb {
		/// Path to the database.
		path: PathBuf,
		/// Cache size in MiB.
		cache_size: usize,
	},

	/// Load a ParityDb database from a given path.
	ParityDb {
		/// Path to the database.
		path: PathBuf,
	},

	/// Use a custom already-open database.
	Custom {
		/// the handle to the custom storage
		db: Arc<dyn Database<DbHash>>,

		/// if set, the `create` flag will be required to open such datasource
		require_create_flag: bool,
	},
}

impl DatabaseSource {
	/// Return path for databases that are stored on disk.
	pub fn path(&self) -> Option<&Path> {
		match self {
			// as per https://github.com/paritytech/substrate/pull/9500#discussion_r684312550
			//
			// IIUC this is needed for polkadot to create its own dbs, so until it can use parity db
			// I would think rocksdb, but later parity-db.
			DatabaseSource::Auto { paritydb_path, .. } => Some(paritydb_path),
			#[cfg(feature = "rocksdb")]
			DatabaseSource::RocksDb { path, .. } => Some(path),
			DatabaseSource::ParityDb { path } => Some(path),
			DatabaseSource::Custom { .. } => None,
		}
	}

	/// Set path for databases that are stored on disk.
	pub fn set_path(&mut self, p: &Path) -> bool {
		match self {
			DatabaseSource::Auto { ref mut paritydb_path, .. } => {
				*paritydb_path = p.into();
				true
			},
			#[cfg(feature = "rocksdb")]
			DatabaseSource::RocksDb { ref mut path, .. } => {
				*path = p.into();
				true
			},
			DatabaseSource::ParityDb { ref mut path } => {
				*path = p.into();
				true
			},
			DatabaseSource::Custom { .. } => false,
		}
	}
}

impl std::fmt::Display for DatabaseSource {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let name = match self {
			DatabaseSource::Auto { .. } => "Auto",
			#[cfg(feature = "rocksdb")]
			DatabaseSource::RocksDb { .. } => "RocksDb",
			DatabaseSource::ParityDb { .. } => "ParityDb",
			DatabaseSource::Custom { .. } => "Custom",
		};
		write!(f, "{}", name)
	}
}

pub(crate) mod columns {
	pub const META: u32 = crate::utils::COLUMN_META;
	pub const STATE: u32 = 1;
	pub const STATE_META: u32 = 2;
	/// maps hashes to lookup keys and numbers to canon hashes.
	pub const KEY_LOOKUP: u32 = 3;
	pub const HEADER: u32 = 4;
	pub const BODY: u32 = 5;
	pub const JUSTIFICATIONS: u32 = 6;
	pub const AUX: u32 = 8;
	/// Offchain workers local storage
	pub const OFFCHAIN: u32 = 9;
	/// Transactions
	pub const TRANSACTION: u32 = 11;
	pub const BODY_INDEX: u32 = 12;
}

struct PendingBlock<Block: BlockT> {
	header: Block::Header,
	justifications: Option<Justifications>,
	body: Option<Vec<Block::Extrinsic>>,
	indexed_body: Option<Vec<Vec<u8>>>,
	leaf_state: NewBlockState,
	register_as_leaf: bool,
}

// wrapper that implements trait required for state_db
#[derive(Clone)]
struct StateMetaDb(Arc<dyn Database<DbHash>>);

impl sc_state_db::MetaDb for StateMetaDb {
	type Error = sp_database::error::DatabaseError;

	fn get_meta(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		Ok(self.0.get(columns::STATE_META, key))
	}
}

struct MetaUpdate<Block: BlockT> {
	pub hash: Block::Hash,
	pub number: NumberFor<Block>,
	pub is_best: bool,
	pub is_finalized: bool,
	pub with_state: bool,
}

fn cache_header<Hash: std::cmp::Eq + std::hash::Hash, Header>(
	cache: &mut LinkedHashMap<Hash, Option<Header>>,
	hash: Hash,
	header: Option<Header>,
) {
	cache.insert(hash, header);
	while cache.len() > CACHE_HEADERS {
		cache.pop_front();
	}
}

/// Block database
pub struct BlockchainDb<Block: BlockT> {
	db: Arc<dyn Database<DbHash>>,
	meta: Arc<RwLock<Meta<NumberFor<Block>, Block::Hash>>>,
	leaves: RwLock<LeafSet<Block::Hash, NumberFor<Block>>>,
	header_metadata_cache: Arc<HeaderMetadataCache<Block>>,
	header_cache: Mutex<LinkedHashMap<Block::Hash, Option<Block::Header>>>,
	pinned_blocks_cache: Arc<RwLock<PinnedBlocksCache<Block>>>,
}

impl<Block: BlockT> BlockchainDb<Block> {
	fn new(db: Arc<dyn Database<DbHash>>) -> ClientResult<Self> {
		let meta = read_meta::<Block>(&*db, columns::HEADER)?;
		let leaves = LeafSet::read_from_db(&*db, columns::META, meta_keys::LEAF_PREFIX)?;
		Ok(BlockchainDb {
			db,
			leaves: RwLock::new(leaves),
			meta: Arc::new(RwLock::new(meta)),
			header_metadata_cache: Arc::new(HeaderMetadataCache::default()),
			header_cache: Default::default(),
			pinned_blocks_cache: Arc::new(RwLock::new(PinnedBlocksCache::new())),
		})
	}

	fn update_meta(&self, update: MetaUpdate<Block>) {
		let MetaUpdate { hash, number, is_best, is_finalized, with_state } = update;
		let mut meta = self.meta.write();
		if number.is_zero() {
			meta.genesis_hash = hash;
		}

		if is_best {
			meta.best_number = number;
			meta.best_hash = hash;
		}

		if is_finalized {
			if with_state {
				meta.finalized_state = Some((hash, number));
			}
			meta.finalized_number = number;
			meta.finalized_hash = hash;

			// A finalized block is canonical and must be reflected by the best block.
			// If the current best block is behind the newly finalized one, advance it
			// to maintain the invariant `best_number >= finalized_number`.
			if number > meta.best_number {
				meta.best_number = number;
				meta.best_hash = hash;
			}
		}
	}

	fn update_block_gap(&self, gap: Option<BlockGap<NumberFor<Block>>>) {
		let mut meta = self.meta.write();
		meta.block_gap = gap;
	}

	/// Empty the cache of pinned items.
	fn clear_pinning_cache(&self) {
		self.pinned_blocks_cache.write().clear();
	}

	/// Load a justification into the cache of pinned items.
	/// Reference count of the item will not be increased. Use this
	/// to load values for items into the cache which have already been pinned.
	fn insert_justifications_if_pinned(&self, hash: Block::Hash, justification: Justification) {
		let mut cache = self.pinned_blocks_cache.write();
		if !cache.contains(hash) {
			return;
		}

		let justifications = Justifications::from(justification);
		cache.insert_justifications(hash, Some(justifications));
	}

	/// Load a justification from the db into the cache of pinned items.
	/// Reference count of the item will not be increased. Use this
	/// to load values for items into the cache which have already been pinned.
	fn insert_persisted_justifications_if_pinned(&self, hash: Block::Hash) -> ClientResult<()> {
		let mut cache = self.pinned_blocks_cache.write();
		if !cache.contains(hash) {
			return Ok(());
		}

		let justifications = self.justifications_uncached(hash)?;
		cache.insert_justifications(hash, justifications);
		Ok(())
	}

	/// Load a block body from the db into the cache of pinned items.
	/// Reference count of the item will not be increased. Use this
	/// to load values for items items into the cache which have already been pinned.
	fn insert_persisted_body_if_pinned(&self, hash: Block::Hash) -> ClientResult<()> {
		let mut cache = self.pinned_blocks_cache.write();
		if !cache.contains(hash) {
			return Ok(());
		}

		let body = self.body_uncached(hash)?;
		cache.insert_body(hash, body);
		Ok(())
	}

	/// Bump reference count for pinned item.
	fn bump_ref(&self, hash: Block::Hash) {
		self.pinned_blocks_cache.write().pin(hash);
	}

	/// Decrease reference count for pinned item and remove if reference count is 0.
	fn unpin(&self, hash: Block::Hash) {
		self.pinned_blocks_cache.write().unpin(hash);
	}

	fn justifications_uncached(&self, hash: Block::Hash) -> ClientResult<Option<Justifications>> {
		match read_db(
			&*self.db,
			columns::KEY_LOOKUP,
			columns::JUSTIFICATIONS,
			BlockId::<Block>::Hash(hash),
		)? {
			Some(justifications) => match Decode::decode(&mut &justifications[..]) {
				Ok(justifications) => Ok(Some(justifications)),
				Err(err) => {
					return Err(sp_blockchain::Error::Backend(format!(
						"Error decoding justifications: {err}"
					)))
				},
			},
			None => Ok(None),
		}
	}

	fn body_uncached(&self, hash: Block::Hash) -> ClientResult<Option<Vec<Block::Extrinsic>>> {
		if let Some(body) =
			read_db(&*self.db, columns::KEY_LOOKUP, columns::BODY, BlockId::Hash::<Block>(hash))?
		{
			// Plain body
			match Decode::decode(&mut &body[..]) {
				Ok(body) => return Ok(Some(body)),
				Err(err) => {
					return Err(sp_blockchain::Error::Backend(format!(
						"Error decoding body: {err}"
					)))
				},
			}
		}

		if let Some(index) = read_db(
			&*self.db,
			columns::KEY_LOOKUP,
			columns::BODY_INDEX,
			BlockId::Hash::<Block>(hash),
		)? {
			match Vec::<DbExtrinsic<Block>>::decode(&mut &index[..]) {
				Ok(index) => {
					let mut body = Vec::new();
					for ex in index {
						match ex {
							DbExtrinsic::Indexed { hash, header } => {
								match self.db.get(columns::TRANSACTION, hash.as_ref()) {
									Some(t) => {
										let mut input =
											utils::join_input(header.as_ref(), t.as_ref());
										let ex = Block::Extrinsic::decode(&mut input).map_err(
											|err| {
												sp_blockchain::Error::Backend(format!(
													"Error decoding indexed extrinsic: {err}"
												))
											},
										)?;
										body.push(ex);
									},
									None => {
										return Err(sp_blockchain::Error::Backend(format!(
											"Missing indexed transaction {hash:?}"
										)))
									},
								};
							},
							DbExtrinsic::Full(ex) => {
								body.push(ex);
							},
							DbExtrinsic::MultiRenew { extrinsic, .. } => {
								// Multi-renewal extrinsic: header contains the full
								// encoded extrinsic (no indexed data to join).
								let ex = Block::Extrinsic::decode(&mut &extrinsic[..]).map_err(
									|err| {
										sp_blockchain::Error::Backend(format!(
											"Error decoding multi-renew extrinsic: {err}"
										))
									},
								)?;
								body.push(ex);
							},
						}
					}
					return Ok(Some(body));
				},
				Err(err) => {
					return Err(sp_blockchain::Error::Backend(format!(
						"Error decoding body list: {err}",
					)))
				},
			}
		}
		Ok(None)
	}

	fn block_indexed_hashes_iter(
		&self,
		hash: Block::Hash,
	) -> ClientResult<Option<impl Iterator<Item = DbHash>>> {
		let Some(body) = read_db(
			&*self.db,
			columns::KEY_LOOKUP,
			columns::BODY_INDEX,
			BlockId::<Block>::Hash(hash),
		)?
		else {
			return Ok(None);
		};
		match Vec::<DbExtrinsic<Block>>::decode(&mut &body[..]) {
			Ok(index) => Ok(Some(index.into_iter().flat_map(|ex| match ex {
				DbExtrinsic::Indexed { hash, .. } => vec![hash],
				DbExtrinsic::MultiRenew { hashes, .. } => hashes.into_iter().collect(),
				_ => vec![],
			}))),
			Err(err) => {
				Err(sp_blockchain::Error::Backend(format!("Error decoding body list: {err}")))
			},
		}
	}
}

impl<Block: BlockT> sc_client_api::blockchain::HeaderBackend<Block> for BlockchainDb<Block> {
	fn header(&self, hash: Block::Hash) -> ClientResult<Option<Block::Header>> {
		let mut cache = self.header_cache.lock();
		if let Some(result) = cache.get_refresh(&hash) {
			return Ok(result.clone());
		}
		let header = utils::read_header(
			&*self.db,
			columns::KEY_LOOKUP,
			columns::HEADER,
			BlockId::<Block>::Hash(hash),
		)?;
		cache_header(&mut cache, hash, header.clone());
		Ok(header)
	}

	fn info(&self) -> sc_client_api::blockchain::Info<Block> {
		let meta = self.meta.read();
		sc_client_api::blockchain::Info {
			best_hash: meta.best_hash,
			best_number: meta.best_number,
			genesis_hash: meta.genesis_hash,
			finalized_hash: meta.finalized_hash,
			finalized_number: meta.finalized_number,
			finalized_state: meta.finalized_state,
			number_leaves: self.leaves.read().count(),
			block_gap: meta.block_gap,
		}
	}

	fn status(&self, hash: Block::Hash) -> ClientResult<sc_client_api::blockchain::BlockStatus> {
		match self.header(hash)?.is_some() {
			true => Ok(sc_client_api::blockchain::BlockStatus::InChain),
			false => Ok(sc_client_api::blockchain::BlockStatus::Unknown),
		}
	}

	fn number(&self, hash: Block::Hash) -> ClientResult<Option<NumberFor<Block>>> {
		Ok(self.header_metadata(hash).ok().map(|header_metadata| header_metadata.number))
	}

	fn hash(&self, number: NumberFor<Block>) -> ClientResult<Option<Block::Hash>> {
		Ok(utils::read_header::<Block>(
			&*self.db,
			columns::KEY_LOOKUP,
			columns::HEADER,
			BlockId::Number(number),
		)?
		.map(|header| header.hash()))
	}
}

impl<Block: BlockT> sc_client_api::blockchain::Backend<Block> for BlockchainDb<Block> {
	fn body(&self, hash: Block::Hash) -> ClientResult<Option<Vec<Block::Extrinsic>>> {
		let cache = self.pinned_blocks_cache.read();
		if let Some(result) = cache.body(&hash) {
			return Ok(result.clone());
		}

		self.body_uncached(hash)
	}

	fn justifications(&self, hash: Block::Hash) -> ClientResult<Option<Justifications>> {
		let cache = self.pinned_blocks_cache.read();
		if let Some(result) = cache.justifications(&hash) {
			return Ok(result.clone());
		}

		self.justifications_uncached(hash)
	}

	fn last_finalized(&self) -> ClientResult<Block::Hash> {
		Ok(self.meta.read().finalized_hash)
	}

	fn leaves(&self) -> ClientResult<Vec<Block::Hash>> {
		Ok(self.leaves.read().hashes())
	}

	fn children(&self, parent_hash: Block::Hash) -> ClientResult<Vec<Block::Hash>> {
		children::read_children(&*self.db, columns::META, meta_keys::CHILDREN_PREFIX, parent_hash)
	}

	fn indexed_transaction(&self, hash: DbHash) -> ClientResult<Option<Vec<u8>>> {
		Ok(self.db.get(columns::TRANSACTION, hash.as_ref()))
	}

	fn has_indexed_transaction(&self, hash: DbHash) -> ClientResult<bool> {
		Ok(self.db.contains(columns::TRANSACTION, hash.as_ref()))
	}

	fn block_indexed_hashes(&self, hash: Block::Hash) -> ClientResult<Option<Vec<DbHash>>> {
		self.block_indexed_hashes_iter(hash).map(|hashes| hashes.map(Iterator::collect))
	}

	fn block_indexed_body(&self, hash: Block::Hash) -> ClientResult<Option<Vec<Vec<u8>>>> {
		match self.block_indexed_hashes_iter(hash) {
			Ok(Some(hashes)) => Ok(Some(
				hashes
					.map(|hash| match self.db.get(columns::TRANSACTION, hash.as_ref()) {
						Some(t) => Ok(t),
						None => Err(sp_blockchain::Error::Backend(format!(
							"Missing indexed transaction {hash:?}",
						))),
					})
					.collect::<Result<_, _>>()?,
			)),
			Ok(None) => Ok(None),
			Err(err) => Err(err),
		}
	}
}

impl<Block: BlockT> HeaderMetadata<Block> for BlockchainDb<Block> {
	type Error = sp_blockchain::Error;

	fn header_metadata(
		&self,
		hash: Block::Hash,
	) -> Result<CachedHeaderMetadata<Block>, Self::Error> {
		self.header_metadata_cache.header_metadata(hash).map_or_else(
			|| {
				self.header(hash)?
					.map(|header| {
						let header_metadata = CachedHeaderMetadata::from(&header);
						self.header_metadata_cache
							.insert_header_metadata(header_metadata.hash, header_metadata.clone());
						header_metadata
					})
					.ok_or_else(|| {
						ClientError::UnknownBlock(format!(
							"Header was not found in the database: {hash:?}",
						))
					})
			},
			Ok,
		)
	}

	fn insert_header_metadata(&self, hash: Block::Hash, metadata: CachedHeaderMetadata<Block>) {
		self.header_metadata_cache.insert_header_metadata(hash, metadata)
	}

	fn remove_header_metadata(&self, hash: Block::Hash) {
		self.header_cache.lock().remove(&hash);
		self.header_metadata_cache.remove_header_metadata(hash);
	}
}

/// Database transaction
pub struct BlockImportOperation<Block: BlockT> {
	old_state: RecordStatsState<RefTrackingState<Block>, Block>,
	db_updates: PrefixedMemoryDB<HashingFor<Block>>,
	storage_updates: StorageCollection,
	child_storage_updates: ChildStorageCollection,
	offchain_storage_updates: OffchainChangesCollection,
	pending_block: Option<PendingBlock<Block>>,
	aux_ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
	finalized_blocks: Vec<(Block::Hash, Option<Justification>)>,
	set_head: Option<Block::Hash>,
	commit_state: bool,
	create_gap: bool,
	reset_storage: bool,
	index_ops: Vec<IndexOperation>,
	prefetched_indexed_transactions: HashMap<DbHash, Vec<u8>>,
}

impl<Block: BlockT> BlockImportOperation<Block> {
	fn apply_offchain(&mut self, transaction: &mut Transaction<DbHash>) {
		let mut count = 0;
		for ((prefix, key), value_operation) in self.offchain_storage_updates.drain(..) {
			count += 1;
			let key = crate::offchain::concatenate_prefix_and_key(&prefix, &key);
			match value_operation {
				OffchainOverlayedChange::SetValue(val) => {
					transaction.set_from_vec(columns::OFFCHAIN, &key, val)
				},
				OffchainOverlayedChange::Remove => transaction.remove(columns::OFFCHAIN, &key),
			}
		}

		if count > 0 {
			log::debug!(target: "sc_offchain", "Applied {count} offchain indexing changes.");
		}
	}

	fn apply_aux(&mut self, transaction: &mut Transaction<DbHash>) {
		for (key, maybe_val) in self.aux_ops.drain(..) {
			match maybe_val {
				Some(val) => transaction.set_from_vec(columns::AUX, &key, val),
				None => transaction.remove(columns::AUX, &key),
			}
		}
	}

	fn apply_new_state(
		&mut self,
		storage: Storage,
		state_version: StateVersion,
	) -> ClientResult<Block::Hash> {
		if storage.top.keys().any(|k| well_known_keys::is_child_storage_key(k)) {
			return Err(sp_blockchain::Error::InvalidState);
		}

		let child_delta = storage.children_default.values().map(|child_content| {
			(
				&child_content.child_info,
				child_content.data.iter().map(|(k, v)| (&k[..], Some(&v[..]))),
			)
		});

		let (root, transaction) = self.old_state.full_storage_root(
			storage.top.iter().map(|(k, v)| (&k[..], Some(&v[..]))),
			child_delta,
			state_version,
		);

		self.db_updates = transaction;
		Ok(root)
	}
}

impl<Block: BlockT> sc_client_api::backend::BlockImportOperation<Block>
	for BlockImportOperation<Block>
{
	type State = RecordStatsState<RefTrackingState<Block>, Block>;

	fn state(&self) -> ClientResult<Option<&Self::State>> {
		Ok(Some(&self.old_state))
	}

	fn set_block_data(
		&mut self,
		header: Block::Header,
		body: Option<Vec<Block::Extrinsic>>,
		indexed_body: Option<Vec<Vec<u8>>>,
		justifications: Option<Justifications>,
		leaf_state: NewBlockState,
		register_as_leaf: bool,
	) -> ClientResult<()> {
		assert!(self.pending_block.is_none(), "Only one block per operation is allowed");
		self.pending_block = Some(PendingBlock {
			header,
			body,
			indexed_body,
			justifications,
			leaf_state,
			register_as_leaf,
		});
		Ok(())
	}

	fn update_db_storage(
		&mut self,
		update: PrefixedMemoryDB<HashingFor<Block>>,
	) -> ClientResult<()> {
		self.db_updates = update;
		Ok(())
	}

	fn reset_storage(
		&mut self,
		storage: Storage,
		state_version: StateVersion,
	) -> ClientResult<Block::Hash> {
		let root = self.apply_new_state(storage, state_version)?;
		self.commit_state = true;
		self.reset_storage = true;
		Ok(root)
	}

	fn set_genesis_state(
		&mut self,
		storage: Storage,
		commit: bool,
		state_version: StateVersion,
	) -> ClientResult<Block::Hash> {
		let root = self.apply_new_state(storage, state_version)?;
		self.commit_state = commit;
		Ok(root)
	}

	fn insert_aux<I>(&mut self, ops: I) -> ClientResult<()>
	where
		I: IntoIterator<Item = (Vec<u8>, Option<Vec<u8>>)>,
	{
		self.aux_ops.append(&mut ops.into_iter().collect());
		Ok(())
	}

	fn update_storage(
		&mut self,
		update: StorageCollection,
		child_update: ChildStorageCollection,
	) -> ClientResult<()> {
		self.storage_updates = update;
		self.child_storage_updates = child_update;
		Ok(())
	}

	fn update_offchain_storage(
		&mut self,
		offchain_update: OffchainChangesCollection,
	) -> ClientResult<()> {
		self.offchain_storage_updates = offchain_update;
		Ok(())
	}

	fn mark_finalized(
		&mut self,
		block: Block::Hash,
		justification: Option<Justification>,
	) -> ClientResult<()> {
		self.finalized_blocks.push((block, justification));
		Ok(())
	}

	fn mark_head(&mut self, hash: Block::Hash) -> ClientResult<()> {
		assert!(self.set_head.is_none(), "Only one set head per operation is allowed");
		self.set_head = Some(hash);
		Ok(())
	}

	fn update_transaction_index(&mut self, index_ops: Vec<IndexOperation>) -> ClientResult<()> {
		self.index_ops = index_ops;
		Ok(())
	}

	fn set_renew_payloads(&mut self, payloads: HashMap<DbHash, Vec<u8>>) -> ClientResult<()> {
		self.prefetched_indexed_transactions = payloads;
		Ok(())
	}

	fn set_create_gap(&mut self, create_gap: bool) {
		self.create_gap = create_gap;
	}
}

struct StorageDb<Block: BlockT> {
	pub db: Arc<dyn Database<DbHash>>,
	pub state_db: StateDb<Block::Hash, Vec<u8>, StateMetaDb>,
	prefix_keys: bool,
}

impl<Block: BlockT> sp_state_machine::Storage<HashingFor<Block>> for StorageDb<Block> {
	fn get(&self, key: &Block::Hash, prefix: Prefix) -> Result<Option<DBValue>, String> {
		if self.prefix_keys {
			let key = prefixed_key::<HashingFor<Block>>(key, prefix);
			self.state_db.get(&key, self)
		} else {
			self.state_db.get(key.as_ref(), self)
		}
		.map_err(|e| format!("Database backend error: {e:?}"))
	}
}

impl<Block: BlockT> sc_state_db::NodeDb for StorageDb<Block> {
	type Error = io::Error;
	type Key = [u8];

	fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		Ok(self.db.get(columns::STATE, key))
	}
}

struct DbGenesisStorage<Block: BlockT> {
	root: Block::Hash,
	storage: PrefixedMemoryDB<HashingFor<Block>>,
}

impl<Block: BlockT> DbGenesisStorage<Block> {
	pub fn new(root: Block::Hash, storage: PrefixedMemoryDB<HashingFor<Block>>) -> Self {
		DbGenesisStorage { root, storage }
	}
}

impl<Block: BlockT> sp_state_machine::Storage<HashingFor<Block>> for DbGenesisStorage<Block> {
	fn get(&self, key: &Block::Hash, prefix: Prefix) -> Result<Option<DBValue>, String> {
		use hash_db::HashDB;
		Ok(self.storage.get(key, prefix))
	}
}

struct EmptyStorage<Block: BlockT>(pub Block::Hash);

impl<Block: BlockT> EmptyStorage<Block> {
	pub fn new() -> Self {
		let mut root = Block::Hash::default();
		let mut mdb = MemoryDB::<HashingFor<Block>>::default();
		// both triedbmut are the same on empty storage.
		sp_trie::trie_types::TrieDBMutBuilderV1::<HashingFor<Block>>::new(&mut mdb, &mut root)
			.build();
		EmptyStorage(root)
	}
}

impl<Block: BlockT> sp_state_machine::Storage<HashingFor<Block>> for EmptyStorage<Block> {
	fn get(&self, _key: &Block::Hash, _prefix: Prefix) -> Result<Option<DBValue>, String> {
		Ok(None)
	}
}

/// Frozen `value` at time `at`.
///
/// Used as inner structure under lock in `FrozenForDuration`.
struct Frozen<T: Clone> {
	at: std::time::Instant,
	value: Option<T>,
}

/// Some value frozen for period of time.
///
/// If time `duration` not passed since the value was instantiated,
/// current frozen value is returned. Otherwise, you have to provide
/// a new value which will be again frozen for `duration`.
pub(crate) struct FrozenForDuration<T: Clone> {
	duration: std::time::Duration,
	value: parking_lot::Mutex<Frozen<T>>,
}

impl<T: Clone> FrozenForDuration<T> {
	fn new(duration: std::time::Duration) -> Self {
		Self { duration, value: Frozen { at: std::time::Instant::now(), value: None }.into() }
	}

	fn take_or_else<F>(&self, f: F) -> T
	where
		F: FnOnce() -> T,
	{
		let mut lock = self.value.lock();
		let now = std::time::Instant::now();
		match lock.value.as_ref() {
			Some(value) if now.saturating_duration_since(lock.at) <= self.duration => value.clone(),
			_ => {
				let new_value = f();
				lock.at = now;
				lock.value = Some(new_value.clone());
				new_value
			},
		}
	}
}

/// Disk backend.
///
/// Disk backend keeps data in a key-value store. In archive mode, trie nodes are kept from all
/// blocks. Otherwise, trie nodes are kept only from some recent blocks.
pub struct Backend<Block: BlockT> {
	storage: Arc<StorageDb<Block>>,
	offchain_storage: offchain::LocalStorage,
	blockchain: BlockchainDb<Block>,
	canonicalization_delay: u64,
	import_lock: Arc<RwLock<()>>,
	is_archive: bool,
	blocks_pruning: BlocksPruning,
	io_stats: FrozenForDuration<(kvdb::IoStats, StateUsageInfo)>,
	state_usage: Arc<StateUsageStats>,
	genesis_state: RwLock<Option<Arc<DbGenesisStorage<Block>>>>,
	shared_trie_cache: Option<sp_trie::cache::SharedTrieCache<HashingFor<Block>>>,
	pruning_filters: Vec<Arc<dyn PruningFilter>>,
}

impl<Block: BlockT> Backend<Block> {
	/// Create a new instance of database backend.
	///
	/// The pruning window is how old a block must be before the state is pruned.
	pub fn new(db_config: DatabaseSettings, canonicalization_delay: u64) -> ClientResult<Self> {
		use utils::OpenDbError;

		let db_source = &db_config.source;

		let (needs_init, db) =
			match crate::utils::open_database::<Block>(db_source, DatabaseType::Full, false) {
				Ok(db) => (false, db),
				Err(OpenDbError::DoesNotExist) => {
					let db =
						crate::utils::open_database::<Block>(db_source, DatabaseType::Full, true)?;
					(true, db)
				},
				Err(as_is) => return Err(as_is.into()),
			};

		Self::from_database(db as Arc<_>, canonicalization_delay, &db_config, needs_init)
	}

	/// Reset the shared trie cache.
	pub fn reset_trie_cache(&self) {
		if let Some(cache) = &self.shared_trie_cache {
			cache.reset();
		}
	}

	/// Create new memory-backed client backend for tests.
	#[cfg(any(test, feature = "test-helpers"))]
	pub fn new_test(blocks_pruning: u32, canonicalization_delay: u64) -> Self {
		Self::new_test_with_tx_storage(BlocksPruning::Some(blocks_pruning), canonicalization_delay)
	}

	/// Create new memory-backed client backend for tests with custom pruning filters.
	#[cfg(any(test, feature = "test-helpers"))]
	pub fn new_test_with_pruning_filters(
		blocks_pruning: u32,
		canonicalization_delay: u64,
		pruning_filters: Vec<Arc<dyn PruningFilter>>,
	) -> Self {
		Self::new_test_with_tx_storage_and_filters(
			BlocksPruning::Some(blocks_pruning),
			canonicalization_delay,
			pruning_filters,
		)
	}

	/// Create new memory-backed client backend for tests.
	#[cfg(any(test, feature = "test-helpers"))]
	pub fn new_test_with_tx_storage(
		blocks_pruning: BlocksPruning,
		canonicalization_delay: u64,
	) -> Self {
		Self::new_test_with_tx_storage_and_filters(
			blocks_pruning,
			canonicalization_delay,
			Default::default(),
		)
	}

	/// Create new memory-backed client backend for tests with custom pruning filters.
	#[cfg(any(test, feature = "test-helpers"))]
	pub fn new_test_with_tx_storage_and_filters(
		blocks_pruning: BlocksPruning,
		canonicalization_delay: u64,
		pruning_filters: Vec<Arc<dyn PruningFilter>>,
	) -> Self {
		let db = kvdb_memorydb::create(crate::utils::NUM_COLUMNS);
		let db = sp_database::as_database(db);
		Self::new_test_with_tx_storage_source(
			blocks_pruning,
			canonicalization_delay,
			DatabaseSource::Custom { db, require_create_flag: true },
			pruning_filters,
		)
	}

	/// Test backend with caller-chosen `DatabaseSource` (memdb / rocksdb / parity-db).
	#[cfg(any(test, feature = "test-helpers"))]
	pub fn new_test_with_tx_storage_source(
		blocks_pruning: BlocksPruning,
		canonicalization_delay: u64,
		source: DatabaseSource,
		pruning_filters: Vec<Arc<dyn PruningFilter>>,
	) -> Self {
		let state_pruning = match blocks_pruning {
			BlocksPruning::KeepAll => PruningMode::ArchiveAll,
			BlocksPruning::KeepFinalized => PruningMode::ArchiveCanonical,
			BlocksPruning::Some(n) => PruningMode::blocks_pruning(n),
		};
		let db_setting = DatabaseSettings {
			trie_cache_maximum_size: Some(16 * 1024 * 1024),
			state_pruning: Some(state_pruning),
			source,
			blocks_pruning,
			pruning_filters,
			metrics_registry: None,
		};

		Self::new(db_setting, canonicalization_delay).expect("failed to create test-db")
	}

	/// Expose the Database that is used by this backend.
	/// The second argument is the Column that stores the State.
	///
	/// Should only be needed for benchmarking.
	#[cfg(feature = "runtime-benchmarks")]
	pub fn expose_db(&self) -> (Arc<dyn sp_database::Database<DbHash>>, sp_database::ColumnId) {
		(self.storage.db.clone(), columns::STATE)
	}

	/// Expose the Storage that is used by this backend.
	///
	/// Should only be needed for benchmarking.
	#[cfg(feature = "runtime-benchmarks")]
	pub fn expose_storage(&self) -> Arc<dyn sp_state_machine::Storage<HashingFor<Block>>> {
		self.storage.clone()
	}

	/// Expose the shared trie cache that is used by this backend.
	///
	/// Should only be needed for benchmarking.
	#[cfg(feature = "runtime-benchmarks")]
	pub fn expose_shared_trie_cache(
		&self,
	) -> Option<sp_trie::cache::SharedTrieCache<HashingFor<Block>>> {
		self.shared_trie_cache.clone()
	}

	fn from_database(
		db: Arc<dyn Database<DbHash>>,
		canonicalization_delay: u64,
		config: &DatabaseSettings,
		should_init: bool,
	) -> ClientResult<Self> {
		let mut db_init_transaction = Transaction::new();

		let requested_state_pruning = config.state_pruning.clone();
		let state_meta_db = StateMetaDb(db.clone());
		let map_e = sp_blockchain::Error::from_state_db;

		let (state_db_init_commit_set, state_db) = StateDb::open(
			state_meta_db,
			requested_state_pruning,
			!db.supports_ref_counting(),
			should_init,
		)
		.map_err(map_e)?;

		apply_state_commit(&mut db_init_transaction, state_db_init_commit_set);

		let state_pruning_used = state_db.pruning_mode();
		let is_archive_pruning = state_pruning_used.is_archive();
		let blockchain = BlockchainDb::new(db.clone())?;

		let storage_db =
			StorageDb { db: db.clone(), state_db, prefix_keys: !db.supports_ref_counting() };

		let offchain_storage = offchain::LocalStorage::new(db.clone());

		let shared_trie_cache = config.trie_cache_maximum_size.map(|maximum_size| {
			let system_memory = sysinfo::System::new_all();
			let used_memory = system_memory.used_memory();
			let total_memory = system_memory.total_memory();

			debug!("Initializing shared trie cache with size {} bytes, {}% of total memory", maximum_size, (maximum_size as f64 / total_memory as f64 * 100.0));
			if maximum_size as u64 > total_memory - used_memory {
				warn!(
					"Not enough memory to initialize shared trie cache. Cache size: {} bytes. System memory: used {} bytes, total {} bytes",
					maximum_size, used_memory, total_memory,
				);
			}

			SharedTrieCache::new(sp_trie::cache::CacheSize::new(maximum_size), config.metrics_registry.as_ref())
		});

		let backend = Backend {
			storage: Arc::new(storage_db),
			offchain_storage,
			blockchain,
			canonicalization_delay,
			import_lock: Default::default(),
			is_archive: is_archive_pruning,
			io_stats: FrozenForDuration::new(std::time::Duration::from_secs(1)),
			state_usage: Arc::new(StateUsageStats::new()),
			blocks_pruning: config.blocks_pruning,
			genesis_state: RwLock::new(None),
			shared_trie_cache,
			pruning_filters: config.pruning_filters.clone(),
		};

		// Older DB versions have no last state key. Check if the state is available and set it.
		let info = backend.blockchain.info();
		if info.finalized_state.is_none() &&
			info.finalized_hash != Default::default() &&
			sc_client_api::Backend::have_state_at(
				&backend,
				info.finalized_hash,
				info.finalized_number,
			) {
			backend.blockchain.update_meta(MetaUpdate {
				hash: info.finalized_hash,
				number: info.finalized_number,
				is_best: info.finalized_hash == info.best_hash,
				is_finalized: true,
				with_state: true,
			});
		}

		// Non archive nodes cannot fill the missing block gap with bodies.
		// If the gap is present, it means that every restart will try to fill the gap:
		// - a block request is made for each and every block in the gap
		// - the request is fulfilled putting pressure on the network and other nodes
		// - upon receiving the block, the block cannot be executed since the state
		//  of the parent block might have been discarded
		// - then the sync engine closes the gap in memory, but never in DB.
		//
		// This leads to inefficient syncing and high CPU usage on every restart. To mitigate this,
		// remove the gap from the DB if we detect it and the current node is not an archive.
		match (backend.is_archive, info.block_gap) {
			(false, Some(gap)) if matches!(gap.gap_type, BlockGapType::MissingBody) => {
				warn!(
					"Detected a missing body gap for non-archive nodes. Removing the gap={:?}",
					gap
				);

				db_init_transaction.remove(columns::META, meta_keys::BLOCK_GAP);
				db_init_transaction.remove(columns::META, meta_keys::BLOCK_GAP_VERSION);
				backend.blockchain.update_block_gap(None);
			},
			_ => {},
		}

		db.commit(db_init_transaction)?;

		Ok(backend)
	}

	/// Handle setting head within a transaction. `route_to` should be the last
	/// block that existed in the database. `best_to` should be the best block
	/// to be set.
	///
	/// In the case where the new best block is a block to be imported, `route_to`
	/// should be the parent of `best_to`. In the case where we set an existing block
	/// to be best, `route_to` should equal to `best_to`.
	fn set_head_with_transaction(
		&self,
		transaction: &mut Transaction<DbHash>,
		route_to: Block::Hash,
		best_to: (NumberFor<Block>, Block::Hash),
	) -> ClientResult<(Vec<Block::Hash>, Vec<Block::Hash>)> {
		let mut enacted = Vec::default();
		let mut retracted = Vec::default();

		let (best_number, best_hash) = best_to;

		let meta = self.blockchain.meta.read();

		if meta.best_number.saturating_sub(best_number).saturated_into::<u64>() >
			self.canonicalization_delay
		{
			return Err(sp_blockchain::Error::SetHeadTooOld);
		}

		let parent_exists =
			self.blockchain.status(route_to)? == sp_blockchain::BlockStatus::InChain;

		// Cannot find tree route with empty DB or when imported a detached block.
		if meta.best_hash != Default::default() && parent_exists {
			let tree_route = sp_blockchain::tree_route(&self.blockchain, meta.best_hash, route_to)?;

			// uncanonicalize: check safety violations and ensure the numbers no longer
			// point to these block hashes in the key mapping.
			for r in tree_route.retracted() {
				if r.hash == meta.finalized_hash {
					warn!(
						"Potential safety failure: reverting finalized block {:?}",
						(&r.number, &r.hash)
					);

					return Err(sp_blockchain::Error::NotInFinalizedChain);
				}

				retracted.push(r.hash);
				utils::remove_number_to_key_mapping(transaction, columns::KEY_LOOKUP, r.number)?;
			}

			// canonicalize: set the number lookup to map to this block's hash.
			for e in tree_route.enacted() {
				enacted.push(e.hash);
				utils::insert_number_to_key_mapping(
					transaction,
					columns::KEY_LOOKUP,
					e.number,
					e.hash,
				)?;
			}
		}

		let lookup_key = utils::number_and_hash_to_lookup_key(best_number, &best_hash)?;
		transaction.set_from_vec(columns::META, meta_keys::BEST_BLOCK, lookup_key);
		utils::insert_number_to_key_mapping(
			transaction,
			columns::KEY_LOOKUP,
			best_number,
			best_hash,
		)?;

		Ok((enacted, retracted))
	}

	fn ensure_sequential_finalization(
		&self,
		header: &Block::Header,
		last_finalized: Option<Block::Hash>,
	) -> ClientResult<()> {
		let last_finalized =
			last_finalized.unwrap_or_else(|| self.blockchain.meta.read().finalized_hash);
		if last_finalized != self.blockchain.meta.read().genesis_hash &&
			*header.parent_hash() != last_finalized
		{
			return Err(sp_blockchain::Error::NonSequentialFinalization(format!(
				"Last finalized {last_finalized:?} not parent of {:?}",
				header.hash()
			)));
		}
		Ok(())
	}

	/// `remove_displaced` can be set to `false` if this is not the last of many subsequent calls
	/// for performance reasons.
	fn finalize_block_with_transaction(
		&self,
		transaction: &mut Transaction<DbHash>,
		hash: Block::Hash,
		header: &Block::Header,
		last_finalized: Option<Block::Hash>,
		justification: Option<Justification>,
		current_transaction_justifications: &mut HashMap<Block::Hash, Justification>,
		remove_displaced: bool,
	) -> ClientResult<MetaUpdate<Block>> {
		// TODO: ensure best chain contains this block.
		let number = *header.number();
		self.ensure_sequential_finalization(header, last_finalized)?;
		let with_state = sc_client_api::Backend::have_state_at(self, hash, number);

		self.note_finalized(
			transaction,
			header,
			hash,
			with_state,
			current_transaction_justifications,
			remove_displaced,
		)?;

		if let Some(justification) = justification {
			transaction.set_from_vec(
				columns::JUSTIFICATIONS,
				&utils::number_and_hash_to_lookup_key(number, hash)?,
				Justifications::from(justification.clone()).encode(),
			);
			current_transaction_justifications.insert(hash, justification);
		}
		Ok(MetaUpdate { hash, number, is_best: false, is_finalized: true, with_state })
	}

	// performs forced canonicalization with a delay after importing a non-finalized block.
	fn force_delayed_canonicalize(
		&self,
		transaction: &mut Transaction<DbHash>,
	) -> ClientResult<()> {
		let best_canonical = match self.storage.state_db.last_canonicalized() {
			LastCanonicalized::None => 0,
			LastCanonicalized::Block(b) => b,
			// Nothing needs to be done when canonicalization is not happening.
			LastCanonicalized::NotCanonicalizing => return Ok(()),
		};

		let info = self.blockchain.info();
		let best_number: u64 = self.blockchain.info().best_number.saturated_into();

		for to_canonicalize in
			best_canonical + 1..=best_number.saturating_sub(self.canonicalization_delay)
		{
			let hash_to_canonicalize = sc_client_api::blockchain::HeaderBackend::hash(
				&self.blockchain,
				to_canonicalize.saturated_into(),
			)?
			.ok_or_else(|| {
				let best_hash = info.best_hash;

				sp_blockchain::Error::Backend(format!(
					"Can't canonicalize missing block number #{to_canonicalize} when for best block {best_hash:?} (#{best_number})",
				))
			})?;

			if !sc_client_api::Backend::have_state_at(
				self,
				hash_to_canonicalize,
				to_canonicalize.saturated_into(),
			) {
				return Ok(());
			}

			trace!(target: "db", "Canonicalize block #{to_canonicalize} ({hash_to_canonicalize:?})");
			let commit = self.storage.state_db.canonicalize_block(&hash_to_canonicalize).map_err(
				sp_blockchain::Error::from_state_db::<
					sc_state_db::Error<sp_database::error::DatabaseError>,
				>,
			)?;
			apply_state_commit(transaction, commit);
		}

		Ok(())
	}

	fn try_commit_operation(&self, mut operation: BlockImportOperation<Block>) -> ClientResult<()> {
		let mut transaction = Transaction::new();

		operation.apply_aux(&mut transaction);
		operation.apply_offchain(&mut transaction);

		let mut meta_updates = Vec::with_capacity(operation.finalized_blocks.len());
		let (best_num, mut last_finalized_hash, mut last_finalized_num, mut block_gap) = {
			let meta = self.blockchain.meta.read();
			(meta.best_number, meta.finalized_hash, meta.finalized_number, meta.block_gap)
		};

		let mut block_gap_updated = false;

		let mut current_transaction_justifications: HashMap<Block::Hash, Justification> =
			HashMap::new();
		let mut finalized_blocks = operation.finalized_blocks.into_iter().peekable();
		while let Some((block_hash, justification)) = finalized_blocks.next() {
			let block_header = self.blockchain.expect_header(block_hash)?;
			meta_updates.push(self.finalize_block_with_transaction(
				&mut transaction,
				block_hash,
				&block_header,
				Some(last_finalized_hash),
				justification,
				&mut current_transaction_justifications,
				finalized_blocks.peek().is_none(),
			)?);
			last_finalized_hash = block_hash;
			last_finalized_num = *block_header.number();
		}

		let imported = if let Some(pending_block) = operation.pending_block {
			let hash = pending_block.header.hash();

			let parent_hash = *pending_block.header.parent_hash();
			let number = *pending_block.header.number();
			let highest_leaf = self
				.blockchain
				.leaves
				.read()
				.highest_leaf()
				.map(|(n, _)| n)
				.unwrap_or(Zero::zero());
			let header_exists_in_db =
				number <= highest_leaf && self.blockchain.header(hash)?.is_some();
			// Body in DB (not incoming block) - needed to update gap when adding body to existing
			// header.
			let body_exists_in_db = self.blockchain.body(hash)?.is_some();
			// Incoming block has body - used for fast sync gap handling.
			let incoming_has_body = pending_block.body.is_some();

			// blocks are keyed by number + hash.
			let lookup_key = utils::number_and_hash_to_lookup_key(number, hash)?;

			if pending_block.leaf_state.is_best() {
				self.set_head_with_transaction(&mut transaction, parent_hash, (number, hash))?;
			};

			utils::insert_hash_to_key_mapping(&mut transaction, columns::KEY_LOOKUP, number, hash)?;

			transaction.set_from_vec(columns::HEADER, &lookup_key, pending_block.header.encode());
			if let Some(body) = pending_block.body {
				// If we have index ops, store body in indexed format; otherwise store as a
				// plain blob.
				if operation.index_ops.is_empty() {
					transaction.set_from_vec(columns::BODY, &lookup_key, body.encode());
				} else {
					let body = apply_index_ops::<Block>(
						&mut transaction,
						body,
						operation.index_ops,
						operation.prefetched_indexed_transactions,
					);
					transaction.set_from_vec(columns::BODY_INDEX, &lookup_key, body);
				}
			}
			if let Some(body) = pending_block.indexed_body {
				apply_indexed_body::<Block>(&mut transaction, body);
			}
			if let Some(justifications) = pending_block.justifications {
				transaction.set_from_vec(
					columns::JUSTIFICATIONS,
					&lookup_key,
					justifications.encode(),
				);
			}

			if number.is_zero() {
				transaction.set(columns::META, meta_keys::GENESIS_HASH, hash.as_ref());

				if operation.commit_state {
					transaction.set_from_vec(columns::META, meta_keys::FINALIZED_STATE, lookup_key);
				} else {
					// When we don't want to commit the genesis state, we still preserve it in
					// memory to bootstrap consensus. It is queried for an initial list of
					// authorities, etc.
					*self.genesis_state.write() = Some(Arc::new(DbGenesisStorage::new(
						*pending_block.header.state_root(),
						operation.db_updates.clone(),
					)));
				}
			}

			let finalized = if operation.commit_state {
				let mut changeset: sc_state_db::ChangeSet<Vec<u8>> =
					sc_state_db::ChangeSet::default();
				let mut ops: u64 = 0;
				let mut bytes: u64 = 0;
				let mut removal: u64 = 0;
				let mut bytes_removal: u64 = 0;
				for (mut key, (val, rc)) in operation.db_updates.drain() {
					self.storage.db.sanitize_key(&mut key);
					if rc > 0 {
						ops += 1;
						bytes += key.len() as u64 + val.len() as u64;
						if rc == 1 {
							changeset.inserted.push((key, val.to_vec()));
						} else {
							changeset.inserted.push((key.clone(), val.to_vec()));
							for _ in 0..rc - 1 {
								changeset.inserted.push((key.clone(), Default::default()));
							}
						}
					} else if rc < 0 {
						removal += 1;
						bytes_removal += key.len() as u64;
						if rc == -1 {
							changeset.deleted.push(key);
						} else {
							for _ in 0..-rc {
								changeset.deleted.push(key.clone());
							}
						}
					}
				}
				self.state_usage.tally_writes_nodes(ops, bytes);
				self.state_usage.tally_removed_nodes(removal, bytes_removal);

				let mut ops: u64 = 0;
				let mut bytes: u64 = 0;
				for (key, value) in operation
					.storage_updates
					.iter()
					.chain(operation.child_storage_updates.iter().flat_map(|(_, s)| s.iter()))
				{
					ops += 1;
					bytes += key.len() as u64;
					if let Some(v) = value.as_ref() {
						bytes += v.len() as u64;
					}
				}
				self.state_usage.tally_writes(ops, bytes);
				let number_u64 = number.saturated_into::<u64>();
				let commit = self
					.storage
					.state_db
					.insert_block(&hash, number_u64, pending_block.header.parent_hash(), changeset)
					.map_err(|e: sc_state_db::Error<sp_database::error::DatabaseError>| {
						sp_blockchain::Error::from_state_db(e)
					})?;
				apply_state_commit(&mut transaction, commit);
				if number <= last_finalized_num {
					// Canonicalize in the db when re-importing existing blocks with state.
					let commit = self.storage.state_db.canonicalize_block(&hash).map_err(
						sp_blockchain::Error::from_state_db::<
							sc_state_db::Error<sp_database::error::DatabaseError>,
						>,
					)?;
					apply_state_commit(&mut transaction, commit);
					meta_updates.push(MetaUpdate {
						hash,
						number,
						is_best: false,
						is_finalized: true,
						with_state: true,
					});
				}

				// Check if need to finalize. Genesis is always finalized instantly.
				let finalized = number_u64 == 0 || pending_block.leaf_state.is_final();
				finalized
			} else {
				(number.is_zero() && last_finalized_num.is_zero()) ||
					pending_block.leaf_state.is_final()
			};

			let header = &pending_block.header;
			let is_best = pending_block.leaf_state.is_best();
			trace!(
				target: "db",
				"DB Commit {hash:?} ({number}), best={is_best}, state={}, header_in_db={header_exists_in_db} body_in_db={body_exists_in_db} incoming_body={incoming_has_body}, finalized={finalized}",
				operation.commit_state,
			);

			self.state_usage.merge_sm(operation.old_state.usage_info());

			// release state reference so that it can be finalized
			// VERY IMPORTANT
			drop(operation.old_state);

			if finalized {
				// TODO: ensure best chain contains this block.
				self.ensure_sequential_finalization(header, Some(last_finalized_hash))?;
				let mut current_transaction_justifications = HashMap::new();
				self.note_finalized(
					&mut transaction,
					header,
					hash,
					operation.commit_state,
					&mut current_transaction_justifications,
					true,
				)?;
			} else {
				// canonicalize blocks which are old enough, regardless of finality.
				self.force_delayed_canonicalize(&mut transaction)?
			}

			if !header_exists_in_db {
				// Add a new leaf if the block has the potential to be finalized.
				if pending_block.register_as_leaf &&
					(number > last_finalized_num || last_finalized_num.is_zero())
				{
					let mut leaves = self.blockchain.leaves.write();
					leaves.import(hash, number, parent_hash);
					leaves.prepare_transaction(
						&mut transaction,
						columns::META,
						meta_keys::LEAF_PREFIX,
					);
				}

				let mut children = children::read_children(
					&*self.storage.db,
					columns::META,
					meta_keys::CHILDREN_PREFIX,
					parent_hash,
				)?;
				if !children.contains(&hash) {
					children.push(hash);
					children::write_children(
						&mut transaction,
						columns::META,
						meta_keys::CHILDREN_PREFIX,
						parent_hash,
						children,
					);
				}
			}

			let should_check_block_gap = !header_exists_in_db || !body_exists_in_db;
			debug!(
				target: "db",
				"should_check_block_gap = {should_check_block_gap}",
			);

			if should_check_block_gap {
				let update_gap =
					|transaction: &mut Transaction<DbHash>,
					 new_gap: BlockGap<NumberFor<Block>>,
					 block_gap: &mut Option<BlockGap<NumberFor<Block>>>| {
						transaction.set(columns::META, meta_keys::BLOCK_GAP, &new_gap.encode());
						transaction.set(
							columns::META,
							meta_keys::BLOCK_GAP_VERSION,
							&BLOCK_GAP_CURRENT_VERSION.encode(),
						);
						block_gap.replace(new_gap);
						debug!(target: "db", "Update block gap. {block_gap:?}");
					};

				let remove_gap =
					|transaction: &mut Transaction<DbHash>,
					 block_gap: &mut Option<BlockGap<NumberFor<Block>>>| {
						transaction.remove(columns::META, meta_keys::BLOCK_GAP);
						transaction.remove(columns::META, meta_keys::BLOCK_GAP_VERSION);
						*block_gap = None;
						debug!(target: "db", "Removed block gap.");
					};

				if let Some(mut gap) = block_gap {
					match gap.gap_type {
						BlockGapType::MissingHeaderAndBody => {
							// Handle blocks at gap start or immediately following (possibly
							// indicating blocks already imported during warp sync where
							// start was not updated).
							if number == gap.start {
								gap.start = number + One::one();
								utils::insert_number_to_key_mapping(
									&mut transaction,
									columns::KEY_LOOKUP,
									number,
									hash,
								)?;
								if gap.start > gap.end {
									remove_gap(&mut transaction, &mut block_gap);
								} else {
									update_gap(&mut transaction, gap, &mut block_gap);
								}
								block_gap_updated = true;
							}
						},
						BlockGapType::MissingBody => {
							// Gap increased when syncing the header chain during fast sync.
							if number == gap.end + One::one() && !incoming_has_body {
								gap.end += One::one();
								utils::insert_number_to_key_mapping(
									&mut transaction,
									columns::KEY_LOOKUP,
									number,
									hash,
								)?;
								update_gap(&mut transaction, gap, &mut block_gap);
								block_gap_updated = true;
							// Gap decreased when downloading the full blocks.
							} else if number == gap.start && incoming_has_body {
								gap.start += One::one();
								if gap.start > gap.end {
									remove_gap(&mut transaction, &mut block_gap);
								} else {
									update_gap(&mut transaction, gap, &mut block_gap);
								}
								block_gap_updated = true;
							}
						},
					}
				} else if operation.create_gap {
					if number > best_num + One::one() &&
						self.blockchain.header(parent_hash)?.is_none()
					{
						let gap = BlockGap {
							start: best_num + One::one(),
							end: number - One::one(),
							gap_type: BlockGapType::MissingHeaderAndBody,
						};
						update_gap(&mut transaction, gap, &mut block_gap);
						block_gap_updated = true;
						debug!(target: "db", "Detected block gap (warp sync) {block_gap:?}");
					} else if number == best_num + One::one() &&
						self.blockchain.header(parent_hash)?.is_some() &&
						!incoming_has_body
					{
						let gap = BlockGap {
							start: number,
							end: number,
							gap_type: BlockGapType::MissingBody,
						};
						update_gap(&mut transaction, gap, &mut block_gap);
						block_gap_updated = true;
						debug!(target: "db", "Detected block gap (fast sync) {block_gap:?}");
					}
				}
			}

			meta_updates.push(MetaUpdate {
				hash,
				number,
				is_best: pending_block.leaf_state.is_best(),
				is_finalized: finalized,
				with_state: operation.commit_state,
			});
			Some((pending_block.header, hash))
		} else {
			None
		};

		if let Some(set_head) = operation.set_head {
			if let Some(header) =
				sc_client_api::blockchain::HeaderBackend::header(&self.blockchain, set_head)?
			{
				let number = header.number();
				let hash = header.hash();

				self.set_head_with_transaction(&mut transaction, hash, (*number, hash))?;

				meta_updates.push(MetaUpdate {
					hash,
					number: *number,
					is_best: true,
					is_finalized: false,
					with_state: false,
				});
			} else {
				return Err(sp_blockchain::Error::UnknownBlock(format!(
					"Cannot set head {set_head:?}",
				)));
			}
		}

		self.storage.db.commit(transaction)?;

		// `reset_storage == true` means the entire state got replaced.
		// In this case we optimize the `STATE` column to improve read performance.
		if operation.reset_storage {
			if let Err(e) = self.storage.db.optimize_db_col(columns::STATE) {
				warn!(target: "db", "Failed to optimize database after state import: {e:?}");
			}
		}

		// Apply all in-memory state changes.
		// Code beyond this point can't fail.

		if let Some((header, hash)) = imported {
			trace!(target: "db", "DB Commit done {hash:?}");
			let header_metadata = CachedHeaderMetadata::from(&header);
			self.blockchain.insert_header_metadata(header_metadata.hash, header_metadata);
			cache_header(&mut self.blockchain.header_cache.lock(), hash, Some(header));
		}

		for m in meta_updates {
			self.blockchain.update_meta(m);
		}
		if block_gap_updated {
			self.blockchain.update_block_gap(block_gap);
		}

		Ok(())
	}

	// Write stuff to a transaction after a new block is finalized. This canonicalizes finalized
	// blocks. Fails if called with a block which was not a child of the last finalized block.
	/// `remove_displaced` can be set to `false` if this is not the last of many subsequent calls
	/// for performance reasons.
	fn note_finalized(
		&self,
		transaction: &mut Transaction<DbHash>,
		f_header: &Block::Header,
		f_hash: Block::Hash,
		with_state: bool,
		current_transaction_justifications: &mut HashMap<Block::Hash, Justification>,
		remove_displaced: bool,
	) -> ClientResult<()> {
		let f_num = *f_header.number();

		let lookup_key = utils::number_and_hash_to_lookup_key(f_num, f_hash)?;
		if with_state {
			transaction.set_from_vec(columns::META, meta_keys::FINALIZED_STATE, lookup_key.clone());
		}
		transaction.set_from_vec(columns::META, meta_keys::FINALIZED_BLOCK, lookup_key);

		let requires_canonicalization = match self.storage.state_db.last_canonicalized() {
			LastCanonicalized::None => true,
			LastCanonicalized::Block(b) => f_num.saturated_into::<u64>() > b,
			LastCanonicalized::NotCanonicalizing => false,
		};

		if requires_canonicalization && sc_client_api::Backend::have_state_at(self, f_hash, f_num) {
			let commit = self.storage.state_db.canonicalize_block(&f_hash).map_err(
				sp_blockchain::Error::from_state_db::<
					sc_state_db::Error<sp_database::error::DatabaseError>,
				>,
			)?;
			apply_state_commit(transaction, commit);
		}

		if remove_displaced {
			let new_displaced = self.blockchain.displaced_leaves_after_finalizing(
				f_hash,
				f_num,
				*f_header.parent_hash(),
			)?;

			self.blockchain.leaves.write().remove_displaced_leaves(FinalizationOutcome::new(
				new_displaced.displaced_leaves.iter().copied(),
			));

			if !matches!(self.blocks_pruning, BlocksPruning::KeepAll) {
				self.prune_displaced_branches(transaction, &new_displaced)?;
			}
		}

		self.prune_blocks(transaction, f_num, current_transaction_justifications)?;

		Ok(())
	}

	fn prune_blocks(
		&self,
		transaction: &mut Transaction<DbHash>,
		finalized_number: NumberFor<Block>,
		current_transaction_justifications: &mut HashMap<Block::Hash, Justification>,
	) -> ClientResult<()> {
		if let BlocksPruning::Some(blocks_pruning) = self.blocks_pruning {
			// Always keep the last finalized block
			let keep = std::cmp::max(blocks_pruning, 1);
			if finalized_number >= keep.into() {
				let number = finalized_number.saturating_sub(keep.into());

				// Before we prune a block, check if it is pinned
				if let Some(hash) = self.blockchain.hash(number)? {
					// Check if any pruning filter wants to preserve this block.
					// We need to check both the current transaction justifications (not yet in DB)
					// and the DB itself (for justifications from previous transactions).
					if !self.pruning_filters.is_empty() {
						let justifications = match current_transaction_justifications.get(&hash) {
							Some(j) => Some(Justifications::from(j.clone())),
							None => self.blockchain.justifications(hash)?,
						};

						let should_retain = justifications
							.map(|j| self.pruning_filters.iter().any(|f| f.should_retain(&j)))
							.unwrap_or(false);

						// We can just return here, pinning can be ignored since the block will
						// remain in the DB.
						if should_retain {
							debug!(
								target: "db",
								"Preserving block #{number} ({hash}) due to keep predicate match"
							);
							return Ok(());
						}
					}

					self.blockchain.insert_persisted_body_if_pinned(hash)?;

					// If the block was finalized in this transaction, it will not be in the db
					// yet.
					if let Some(justification) = current_transaction_justifications.remove(&hash) {
						self.blockchain.insert_justifications_if_pinned(hash, justification);
					} else {
						self.blockchain.insert_persisted_justifications_if_pinned(hash)?;
					}
				};

				self.prune_block(transaction, BlockId::<Block>::number(number))?;
			}
		}
		Ok(())
	}

	fn prune_displaced_branches(
		&self,
		transaction: &mut Transaction<DbHash>,
		displaced: &DisplacedLeavesAfterFinalization<Block>,
	) -> ClientResult<()> {
		// Discard all blocks from displaced branches
		for &hash in displaced.displaced_blocks.iter() {
			self.blockchain.insert_persisted_body_if_pinned(hash)?;
			self.prune_block(transaction, BlockId::<Block>::hash(hash))?;
		}
		Ok(())
	}

	fn prune_block(
		&self,
		transaction: &mut Transaction<DbHash>,
		id: BlockId<Block>,
	) -> ClientResult<()> {
		debug!(target: "db", "Removing block #{id}");
		utils::remove_from_db(
			transaction,
			&*self.storage.db,
			columns::KEY_LOOKUP,
			columns::BODY,
			id,
		)?;
		utils::remove_from_db(
			transaction,
			&*self.storage.db,
			columns::KEY_LOOKUP,
			columns::JUSTIFICATIONS,
			id,
		)?;
		if let Some(index) =
			read_db(&*self.storage.db, columns::KEY_LOOKUP, columns::BODY_INDEX, id)?
		{
			utils::remove_from_db(
				transaction,
				&*self.storage.db,
				columns::KEY_LOOKUP,
				columns::BODY_INDEX,
				id,
			)?;
			match Vec::<DbExtrinsic<Block>>::decode(&mut &index[..]) {
				Ok(index) => {
					for ex in index {
						match ex {
							DbExtrinsic::Indexed { hash, .. } => {
								transaction.release(columns::TRANSACTION, hash);
							},
							DbExtrinsic::MultiRenew { hashes, .. } => {
								for hash in hashes {
									transaction.release(columns::TRANSACTION, hash);
								}
							},
							DbExtrinsic::Full(_) => {},
						}
					}
				},
				Err(err) => {
					return Err(sp_blockchain::Error::Backend(format!(
						"Error decoding body list: {err}",
					)))
				},
			}
		}
		Ok(())
	}

	fn empty_state(&self) -> RecordStatsState<RefTrackingState<Block>, Block> {
		let root = EmptyStorage::<Block>::new().0; // Empty trie
		let db_state = DbStateBuilder::<HashingFor<Block>>::new(self.storage.clone(), root)
			.with_optional_cache(self.shared_trie_cache.as_ref().map(|c| c.local_cache_untrusted()))
			.build();
		let state = RefTrackingState::new(db_state, self.storage.clone(), None);
		RecordStatsState::new(state, None, self.state_usage.clone())
	}
}

fn apply_state_commit(
	transaction: &mut Transaction<DbHash>,
	commit: sc_state_db::CommitSet<Vec<u8>>,
) {
	for (key, val) in commit.data.inserted.into_iter() {
		transaction.set_from_vec(columns::STATE, &key[..], val);
	}
	for key in commit.data.deleted.into_iter() {
		transaction.remove(columns::STATE, &key[..]);
	}
	for (key, val) in commit.meta.inserted.into_iter() {
		transaction.set_from_vec(columns::STATE_META, &key[..], val);
	}
	for key in commit.meta.deleted.into_iter() {
		transaction.remove(columns::STATE_META, &key[..]);
	}
}

fn apply_index_ops<Block: BlockT>(
	transaction: &mut Transaction<DbHash>,
	body: Vec<Block::Extrinsic>,
	ops: Vec<IndexOperation>,
	mut prefetched: HashMap<DbHash, Vec<u8>>,
) -> Vec<u8> {
	let mut extrinsic_index: Vec<DbExtrinsic<Block>> = Vec::with_capacity(body.len());
	let mut index_map = HashMap::new();
	// Submission order matters; see `DbExtrinsic::MultiRenew`. Duplicates are kept so
	// per-occurrence refcount inc/dec stays symmetric with prune-time release.
	let mut renewed_map: HashMap<u32, Vec<DbHash>> = HashMap::new();
	for op in ops {
		match op {
			IndexOperation::Insert { extrinsic, hash, size } => {
				index_map.insert(extrinsic, (hash, size));
			},
			IndexOperation::Renew { extrinsic, hash } => {
				renewed_map
					.entry(extrinsic)
					.or_default()
					.push(DbHash::from_slice(hash.as_ref()));
			},
		}
	}
	let mut store_or_reference = |tx: &mut Transaction<DbHash>, hash: DbHash| {
		if let Some(bytes) = prefetched.remove(&hash) {
			tx.store(columns::TRANSACTION, hash, bytes);
		} else {
			tx.reference(columns::TRANSACTION, hash);
		}
	};
	let mut n_inserted = 0usize;
	let mut n_renew_slots = 0usize;
	let mut n_renew_hashes = 0usize;
	let mut n_full = 0usize;
	for (index, extrinsic) in body.into_iter().enumerate() {
		let db_extrinsic = if let Some(hashes) = renewed_map.remove(&(index as u32)) {
			n_renew_slots += 1;
			n_renew_hashes += hashes.len();
			let encoded = extrinsic.encode();
			if hashes.len() == 1 {
				// Single renewal: backwards-compatible Indexed variant
				let hash = hashes[0];
				store_or_reference(transaction, hash);
				DbExtrinsic::Indexed { hash, header: encoded }
			} else {
				// Multi-renewal: bump ref counter for each hash
				for hash in &hashes {
					store_or_reference(transaction, *hash);
				}
				DbExtrinsic::MultiRenew { hashes, extrinsic: encoded }
			}
		} else {
			match index_map.get(&(index as u32)) {
				Some((hash, size)) => {
					let encoded = extrinsic.encode();
					if *size as usize <= encoded.len() {
						n_inserted += 1;
						let offset = encoded.len() - *size as usize;
						transaction.store(
							columns::TRANSACTION,
							DbHash::from_slice(hash.as_ref()),
							encoded[offset..].to_vec(),
						);
						DbExtrinsic::Indexed {
							hash: DbHash::from_slice(hash.as_ref()),
							header: encoded[..offset].to_vec(),
						}
					} else {
						// Invalid indexed slice. Just store full data and don't index anything.
						n_full += 1;
						DbExtrinsic::Full(extrinsic)
					}
				},
				_ => {
					n_full += 1;
					DbExtrinsic::Full(extrinsic)
				},
			}
		};
		extrinsic_index.push(db_extrinsic);
	}
	debug!(
		target: "db",
		"DB transaction index: {} inserted, {} slots renewed ({} hashes), {} full",
		n_inserted,
		n_renew_slots,
		n_renew_hashes,
		n_full,
	);
	extrinsic_index.encode()
}

fn apply_indexed_body<Block: BlockT>(transaction: &mut Transaction<DbHash>, body: Vec<Vec<u8>>) {
	for extrinsic in body {
		let hash = sp_runtime::traits::BlakeTwo256::hash(&extrinsic);
		transaction.store(columns::TRANSACTION, DbHash::from_slice(hash.as_ref()), extrinsic);
	}
}

impl<Block> sc_client_api::backend::AuxStore for Backend<Block>
where
	Block: BlockT,
{
	fn insert_aux<
		'a,
		'b: 'a,
		'c: 'a,
		I: IntoIterator<Item = &'a (&'c [u8], &'c [u8])>,
		D: IntoIterator<Item = &'a &'b [u8]>,
	>(
		&self,
		insert: I,
		delete: D,
	) -> ClientResult<()> {
		let mut transaction = Transaction::new();
		for (k, v) in insert {
			transaction.set(columns::AUX, k, v);
		}
		for k in delete {
			transaction.remove(columns::AUX, k);
		}
		self.storage.db.commit(transaction)?;
		Ok(())
	}

	fn get_aux(&self, key: &[u8]) -> ClientResult<Option<Vec<u8>>> {
		Ok(self.storage.db.get(columns::AUX, key))
	}
}

impl<Block: BlockT> sc_client_api::backend::Backend<Block> for Backend<Block> {
	type BlockImportOperation = BlockImportOperation<Block>;
	type Blockchain = BlockchainDb<Block>;
	type State = RecordStatsState<RefTrackingState<Block>, Block>;
	type OffchainStorage = offchain::LocalStorage;

	fn begin_operation(&self) -> ClientResult<Self::BlockImportOperation> {
		Ok(BlockImportOperation {
			pending_block: None,
			old_state: self.empty_state(),
			db_updates: PrefixedMemoryDB::default(),
			storage_updates: Default::default(),
			child_storage_updates: Default::default(),
			offchain_storage_updates: Default::default(),
			aux_ops: Vec::new(),
			finalized_blocks: Vec::new(),
			set_head: None,
			commit_state: false,
			create_gap: true,
			reset_storage: false,
			index_ops: Default::default(),
			prefetched_indexed_transactions: Default::default(),
		})
	}

	fn begin_state_operation(
		&self,
		operation: &mut Self::BlockImportOperation,
		block: Block::Hash,
	) -> ClientResult<()> {
		if block == Default::default() {
			operation.old_state = self.empty_state();
		} else {
			operation.old_state = self.state_at(block, TrieCacheContext::Untrusted)?;
		}

		operation.commit_state = true;
		Ok(())
	}

	fn commit_operation(&self, operation: Self::BlockImportOperation) -> ClientResult<()> {
		let usage = operation.old_state.usage_info();
		self.state_usage.merge_sm(usage);

		if let Err(e) = self.try_commit_operation(operation) {
			let state_meta_db = StateMetaDb(self.storage.db.clone());
			self.storage
				.state_db
				.reset(state_meta_db)
				.map_err(sp_blockchain::Error::from_state_db)?;
			self.blockchain.clear_pinning_cache();
			Err(e)
		} else {
			self.storage.state_db.sync();
			Ok(())
		}
	}

	fn finalize_block(
		&self,
		hash: Block::Hash,
		justification: Option<Justification>,
	) -> ClientResult<()> {
		let mut transaction = Transaction::new();
		let header = self.blockchain.expect_header(hash)?;

		let mut current_transaction_justifications = HashMap::new();
		let m = self.finalize_block_with_transaction(
			&mut transaction,
			hash,
			&header,
			None,
			justification,
			&mut current_transaction_justifications,
			true,
		)?;

		self.storage.db.commit(transaction)?;
		self.blockchain.update_meta(m);
		Ok(())
	}

	fn append_justification(
		&self,
		hash: Block::Hash,
		justification: Justification,
	) -> ClientResult<()> {
		let mut transaction: Transaction<DbHash> = Transaction::new();
		let header = self.blockchain.expect_header(hash)?;
		let number = *header.number();

		// Check if the block is finalized first.
		let is_descendent_of = is_descendent_of(&self.blockchain, None);
		let last_finalized = self.blockchain.last_finalized()?;

		// We can do a quick check first, before doing a proper but more expensive check
		if number > self.blockchain.info().finalized_number ||
			(hash != last_finalized && !is_descendent_of(&hash, &last_finalized)?)
		{
			return Err(ClientError::NotInFinalizedChain);
		}

		let justifications = if let Some(mut stored_justifications) =
			self.blockchain.justifications(hash)?
		{
			if !stored_justifications.append(justification) {
				return Err(ClientError::BadJustification("Duplicate consensus engine ID".into()));
			}
			stored_justifications
		} else {
			Justifications::from(justification)
		};

		transaction.set_from_vec(
			columns::JUSTIFICATIONS,
			&utils::number_and_hash_to_lookup_key(number, hash)?,
			justifications.encode(),
		);

		self.storage.db.commit(transaction)?;

		Ok(())
	}

	fn offchain_storage(&self) -> Option<Self::OffchainStorage> {
		Some(self.offchain_storage.clone())
	}

	fn usage_info(&self) -> Option<UsageInfo> {
		let (io_stats, state_stats) = self.io_stats.take_or_else(|| {
			(
				// TODO: implement DB stats and cache size retrieval
				kvdb::IoStats::empty(),
				self.state_usage.take(),
			)
		});
		let database_cache = MemorySize::from_bytes(0);
		let state_cache = MemorySize::from_bytes(
			self.shared_trie_cache.as_ref().map_or(0, |c| c.used_memory_size()),
		);

		Some(UsageInfo {
			memory: MemoryInfo { state_cache, database_cache },
			io: IoInfo {
				transactions: io_stats.transactions,
				bytes_read: io_stats.bytes_read,
				bytes_written: io_stats.bytes_written,
				writes: io_stats.writes,
				reads: io_stats.reads,
				average_transaction_size: io_stats.avg_transaction_size() as u64,
				state_reads: state_stats.reads.ops,
				state_writes: state_stats.writes.ops,
				state_writes_cache: state_stats.overlay_writes.ops,
				state_reads_cache: state_stats.cache_reads.ops,
				state_writes_nodes: state_stats.nodes_writes.ops,
			},
		})
	}

	fn revert(
		&self,
		n: NumberFor<Block>,
		revert_finalized: bool,
	) -> ClientResult<(NumberFor<Block>, HashSet<Block::Hash>)> {
		let mut reverted_finalized = HashSet::new();

		let info = self.blockchain.info();

		let highest_leaf = self
			.blockchain
			.leaves
			.read()
			.highest_leaf()
			.and_then(|(n, h)| h.last().map(|h| (n, *h)));

		let best_number = info.best_number;
		let best_hash = info.best_hash;

		let finalized = info.finalized_number;

		let revertible = best_number - finalized;
		let n = if !revert_finalized && revertible < n { revertible } else { n };

		let (n, mut number_to_revert, mut hash_to_revert) = match highest_leaf {
			Some((l_n, l_h)) => (n + (l_n - best_number), l_n, l_h),
			None => (n, best_number, best_hash),
		};

		let mut revert_blocks = || -> ClientResult<NumberFor<Block>> {
			for c in 0..n.saturated_into::<u64>() {
				if number_to_revert.is_zero() {
					return Ok(c.saturated_into::<NumberFor<Block>>());
				}
				let mut transaction = Transaction::new();
				let removed = self.blockchain.header(hash_to_revert)?.ok_or_else(|| {
					sp_blockchain::Error::UnknownBlock(format!(
						"Error reverting to {hash_to_revert}. Block header not found.",
					))
				})?;
				let removed_hash = hash_to_revert;

				let prev_number = number_to_revert.saturating_sub(One::one());
				let prev_hash =
					if prev_number == best_number { best_hash } else { *removed.parent_hash() };

				if !self.have_state_at(prev_hash, prev_number) {
					return Ok(c.saturated_into::<NumberFor<Block>>());
				}

				match self.storage.state_db.revert_one() {
					Some(commit) => {
						apply_state_commit(&mut transaction, commit);

						number_to_revert = prev_number;
						hash_to_revert = prev_hash;

						let update_finalized = number_to_revert < finalized;

						let key = utils::number_and_hash_to_lookup_key(
							number_to_revert,
							&hash_to_revert,
						)?;
						if update_finalized {
							transaction.set_from_vec(
								columns::META,
								meta_keys::FINALIZED_BLOCK,
								key.clone(),
							);

							reverted_finalized.insert(removed_hash);
							if let Some((hash, _)) = self.blockchain.info().finalized_state {
								if hash == hash_to_revert {
									if !number_to_revert.is_zero() &&
										self.have_state_at(prev_hash, prev_number)
									{
										let lookup_key = utils::number_and_hash_to_lookup_key(
											prev_number,
											prev_hash,
										)?;
										transaction.set_from_vec(
											columns::META,
											meta_keys::FINALIZED_STATE,
											lookup_key,
										);
									} else {
										transaction
											.remove(columns::META, meta_keys::FINALIZED_STATE);
									}
								}
							}
						}

						transaction.set_from_vec(columns::META, meta_keys::BEST_BLOCK, key);
						transaction.remove(columns::KEY_LOOKUP, removed_hash.as_ref());
						children::remove_children(
							&mut transaction,
							columns::META,
							meta_keys::CHILDREN_PREFIX,
							hash_to_revert,
						);
						self.prune_block(&mut transaction, BlockId::Hash(removed_hash))?;
						remove_from_db::<Block>(
							&mut transaction,
							&*self.storage.db,
							columns::KEY_LOOKUP,
							columns::HEADER,
							BlockId::Hash(removed_hash),
						)?;

						self.storage.db.commit(transaction)?;

						// Clean the cache
						self.blockchain.remove_header_metadata(removed_hash);

						let is_best = number_to_revert < best_number;

						self.blockchain.update_meta(MetaUpdate {
							hash: hash_to_revert,
							number: number_to_revert,
							is_best,
							is_finalized: update_finalized,
							with_state: false,
						});
					},
					None => return Ok(c.saturated_into::<NumberFor<Block>>()),
				}
			}

			Ok(n)
		};

		let reverted = revert_blocks()?;

		let revert_leaves = || -> ClientResult<()> {
			let mut transaction = Transaction::new();
			let mut leaves = self.blockchain.leaves.write();

			leaves.revert(hash_to_revert, number_to_revert).into_iter().try_for_each(
				|(h, _)| {
					self.blockchain.remove_header_metadata(h);
					transaction.remove(columns::KEY_LOOKUP, h.as_ref());

					self.prune_block(&mut transaction, BlockId::Hash(h))?;
					remove_from_db::<Block>(
						&mut transaction,
						&*self.storage.db,
						columns::KEY_LOOKUP,
						columns::HEADER,
						BlockId::Hash(h),
					)?;

					Ok::<_, ClientError>(())
				},
			)?;
			leaves.prepare_transaction(&mut transaction, columns::META, meta_keys::LEAF_PREFIX);
			self.storage.db.commit(transaction)?;

			Ok(())
		};

		revert_leaves()?;

		Ok((reverted, reverted_finalized))
	}

	fn remove_leaf_block(&self, hash: Block::Hash) -> ClientResult<()> {
		let best_hash = self.blockchain.info().best_hash;

		if best_hash == hash {
			return Err(sp_blockchain::Error::Backend(format!("Can't remove best block {hash:?}")));
		}

		let hdr = self.blockchain.header_metadata(hash)?;
		if !self.have_state_at(hash, hdr.number) {
			return Err(sp_blockchain::Error::UnknownBlock(format!(
				"State already discarded for {hash:?}",
			)));
		}

		let mut leaves = self.blockchain.leaves.write();
		if !leaves.contains(hdr.number, hash) {
			return Err(sp_blockchain::Error::Backend(format!(
				"Can't remove non-leaf block {hash:?}",
			)));
		}

		let mut transaction = Transaction::new();
		if let Some(commit) = self.storage.state_db.remove(&hash) {
			apply_state_commit(&mut transaction, commit);
		}
		transaction.remove(columns::KEY_LOOKUP, hash.as_ref());

		let children: Vec<_> = self
			.blockchain()
			.children(hdr.parent)?
			.into_iter()
			.filter(|child_hash| *child_hash != hash)
			.collect();
		let parent_leaf = if children.is_empty() {
			children::remove_children(
				&mut transaction,
				columns::META,
				meta_keys::CHILDREN_PREFIX,
				hdr.parent,
			);
			Some(hdr.parent)
		} else {
			children::write_children(
				&mut transaction,
				columns::META,
				meta_keys::CHILDREN_PREFIX,
				hdr.parent,
				children,
			);
			None
		};

		let remove_outcome = leaves.remove(hash, hdr.number, parent_leaf);
		leaves.prepare_transaction(&mut transaction, columns::META, meta_keys::LEAF_PREFIX);
		if let Err(e) = self.storage.db.commit(transaction) {
			if let Some(outcome) = remove_outcome {
				leaves.undo().undo_remove(outcome);
			}
			return Err(e.into());
		}
		self.blockchain().remove_header_metadata(hash);
		Ok(())
	}

	fn blockchain(&self) -> &BlockchainDb<Block> {
		&self.blockchain
	}

	fn state_at(
		&self,
		hash: Block::Hash,
		trie_cache_context: TrieCacheContext,
	) -> ClientResult<Self::State> {
		if hash == self.blockchain.meta.read().genesis_hash {
			if let Some(genesis_state) = &*self.genesis_state.read() {
				let root = genesis_state.root;
				let db_state =
					DbStateBuilder::<HashingFor<Block>>::new(genesis_state.clone(), root)
						.with_optional_cache(self.shared_trie_cache.as_ref().map(|c| {
							if matches!(trie_cache_context, TrieCacheContext::Trusted) {
								c.local_cache_trusted()
							} else {
								c.local_cache_untrusted()
							}
						}))
						.build();

				let state = RefTrackingState::new(db_state, self.storage.clone(), None);
				return Ok(RecordStatsState::new(state, None, self.state_usage.clone()));
			}
		}

		match self.blockchain.header_metadata(hash) {
			Ok(ref hdr) => {
				let hint = || {
					sc_state_db::NodeDb::get(self.storage.as_ref(), hdr.state_root.as_ref())
						.unwrap_or(None)
						.is_some()
				};

				if let Ok(()) =
					self.storage.state_db.pin(&hash, hdr.number.saturated_into::<u64>(), hint)
				{
					let root = hdr.state_root;
					let db_state =
						DbStateBuilder::<HashingFor<Block>>::new(self.storage.clone(), root)
							.with_optional_cache(self.shared_trie_cache.as_ref().map(|c| {
								if matches!(trie_cache_context, TrieCacheContext::Trusted) {
									c.local_cache_trusted()
								} else {
									c.local_cache_untrusted()
								}
							}))
							.build();
					let state = RefTrackingState::new(db_state, self.storage.clone(), Some(hash));
					Ok(RecordStatsState::new(state, Some(hash), self.state_usage.clone()))
				} else {
					Err(sp_blockchain::Error::UnknownBlock(format!(
						"State already discarded for {hash:?}",
					)))
				}
			},
			Err(e) => Err(e),
		}
	}

	fn have_state_at(&self, hash: Block::Hash, number: NumberFor<Block>) -> bool {
		if self.is_archive {
			match self.blockchain.header_metadata(hash) {
				Ok(header) => sp_state_machine::Storage::get(
					self.storage.as_ref(),
					&header.state_root,
					(&[], None),
				)
				.unwrap_or(None)
				.is_some(),
				_ => false,
			}
		} else {
			match self.storage.state_db.is_pruned(&hash, number.saturated_into::<u64>()) {
				IsPruned::Pruned => false,
				IsPruned::NotPruned => true,
				IsPruned::MaybePruned => match self.blockchain.header_metadata(hash) {
					Ok(header) => sp_state_machine::Storage::get(
						self.storage.as_ref(),
						&header.state_root,
						(&[], None),
					)
					.unwrap_or(None)
					.is_some(),
					_ => false,
				},
			}
		}
	}

	fn get_import_lock(&self) -> &RwLock<()> {
		&self.import_lock
	}

	fn requires_full_sync(&self) -> bool {
		matches!(
			self.storage.state_db.pruning_mode(),
			PruningMode::ArchiveAll | PruningMode::ArchiveCanonical
		)
	}

	fn pin_block(&self, hash: <Block as BlockT>::Hash) -> sp_blockchain::Result<()> {
		let hint = || {
			let header_metadata = self.blockchain.header_metadata(hash);
			header_metadata
				.map(|hdr| {
					sc_state_db::NodeDb::get(self.storage.as_ref(), hdr.state_root.as_ref())
						.unwrap_or(None)
						.is_some()
				})
				.unwrap_or(false)
		};

		if let Some(number) = self.blockchain.number(hash)? {
			self.storage.state_db.pin(&hash, number.saturated_into::<u64>(), hint).map_err(
				|_| {
					sp_blockchain::Error::UnknownBlock(format!(
						"Unable to pin: state already discarded for `{hash:?}`",
					))
				},
			)?;
		} else {
			return Err(ClientError::UnknownBlock(format!(
				"Can not pin block with hash `{hash:?}`. Block not found.",
			)));
		}

		if self.blocks_pruning != BlocksPruning::KeepAll {
			// Only increase reference count for this hash. Value is loaded once we prune.
			self.blockchain.bump_ref(hash);
		}
		Ok(())
	}

	fn unpin_block(&self, hash: <Block as BlockT>::Hash) {
		self.storage.state_db.unpin(&hash);

		if self.blocks_pruning != BlocksPruning::KeepAll {
			self.blockchain.unpin(hash);
		}
	}
}

impl<Block: BlockT> sc_client_api::backend::LocalBackend<Block> for Backend<Block> {}

#[cfg(test)]
pub(crate) mod tests {
	use super::*;
	use crate::{columns, utils::number_and_hash_to_lookup_key};
	use hash_db::{HashDB, EMPTY_PREFIX};
	use sc_client_api::{
		backend::{Backend as BTrait, BlockImportOperation as Op},
		blockchain::Backend as BLBTrait,
	};
	use sp_blockchain::{lowest_common_ancestor, tree_route};
	use sp_core::H256;
	use sp_runtime::{
		testing::{Block as RawBlock, Header, MockCallU64, TestXt},
		traits::{BlakeTwo256, Hash},
		ConsensusEngineId, StateVersion,
	};

	const CONS0_ENGINE_ID: ConsensusEngineId = *b"CON0";
	const CONS1_ENGINE_ID: ConsensusEngineId = *b"CON1";

	type UncheckedXt = TestXt<MockCallU64, ()>;
	pub(crate) type Block = RawBlock<UncheckedXt>;

	pub fn insert_header(
		backend: &Backend<Block>,
		number: u64,
		parent_hash: H256,
		changes: Option<Vec<(Vec<u8>, Vec<u8>)>>,
		extrinsics_root: H256,
	) -> H256 {
		insert_block(backend, number, parent_hash, changes, extrinsics_root, Vec::new(), None)
			.unwrap()
	}

	pub fn insert_block(
		backend: &Backend<Block>,
		number: u64,
		parent_hash: H256,
		_changes: Option<Vec<(Vec<u8>, Vec<u8>)>>,
		extrinsics_root: H256,
		body: Vec<UncheckedXt>,
		transaction_index: Option<Vec<IndexOperation>>,
	) -> Result<H256, sp_blockchain::Error> {
		insert_block_with_prefetched(
			backend,
			number,
			parent_hash,
			extrinsics_root,
			body,
			transaction_index,
			HashMap::new(),
		)
	}

	pub fn insert_block_with_prefetched(
		backend: &Backend<Block>,
		number: u64,
		parent_hash: H256,
		extrinsics_root: H256,
		body: Vec<UncheckedXt>,
		transaction_index: Option<Vec<IndexOperation>>,
		prefetched: HashMap<H256, Vec<u8>>,
	) -> Result<H256, sp_blockchain::Error> {
		use sp_runtime::testing::Digest;

		let digest = Digest::default();
		let mut header =
			Header { number, parent_hash, state_root: Default::default(), digest, extrinsics_root };

		let block_hash = if number == 0 { Default::default() } else { parent_hash };
		let mut op = backend.begin_operation().unwrap();
		backend.begin_state_operation(&mut op, block_hash).unwrap();
		if !prefetched.is_empty() {
			op.set_renew_payloads(prefetched).unwrap();
		}
		if let Some(index) = transaction_index {
			op.update_transaction_index(index).unwrap();
		}

		let (root, overlay) = op.old_state.storage_root(
			vec![(block_hash.as_ref(), Some(block_hash.as_ref()))].into_iter(),
			StateVersion::V1,
		);
		op.update_db_storage(overlay).unwrap();
		header.state_root = root.into();

		op.set_block_data(header.clone(), Some(body), None, None, NewBlockState::Best, true)
			.unwrap();

		backend.commit_operation(op)?;

		Ok(header.hash())
	}

	/// Mirrors `apply_block` so runtime ops override wrapper-supplied ones when both are present.
	pub fn insert_block_with_synthetic_ops(
		backend: &Backend<Block>,
		number: u64,
		parent_hash: H256,
		extrinsics_root: H256,
		body: Vec<UncheckedXt>,
		runtime_index_ops: Vec<IndexOperation>,
		synthetic_index_ops: Vec<IndexOperation>,
		renew_payloads: HashMap<H256, Vec<u8>>,
	) -> Result<H256, sp_blockchain::Error> {
		use sp_runtime::testing::Digest;

		let digest = Digest::default();
		let mut header =
			Header { number, parent_hash, state_root: Default::default(), digest, extrinsics_root };

		let block_hash = if number == 0 { Default::default() } else { parent_hash };
		let mut op = backend.begin_operation().unwrap();
		backend.begin_state_operation(&mut op, block_hash).unwrap();
		op.set_renew_payloads(renew_payloads).unwrap();
		op.update_transaction_index(synthetic_index_ops).unwrap();
		if !runtime_index_ops.is_empty() {
			op.update_transaction_index(runtime_index_ops).unwrap();
		}

		let (root, overlay) = op.old_state.storage_root(
			vec![(block_hash.as_ref(), Some(block_hash.as_ref()))].into_iter(),
			StateVersion::V1,
		);
		op.update_db_storage(overlay).unwrap();
		header.state_root = root.into();

		op.set_block_data(header.clone(), Some(body), None, None, NewBlockState::Best, true)
			.unwrap();

		backend.commit_operation(op)?;

		Ok(header.hash())
	}

	pub fn insert_disconnected_header(
		backend: &Backend<Block>,
		number: u64,
		parent_hash: H256,
		extrinsics_root: H256,
		best: bool,
	) -> H256 {
		use sp_runtime::testing::Digest;

		let digest = Digest::default();
		let header =
			Header { number, parent_hash, state_root: Default::default(), digest, extrinsics_root };

		let mut op = backend.begin_operation().unwrap();

		op.set_block_data(
			header.clone(),
			Some(vec![]),
			None,
			None,
			if best { NewBlockState::Best } else { NewBlockState::Normal },
			true,
		)
		.unwrap();

		backend.commit_operation(op).unwrap();

		header.hash()
	}

	pub fn insert_header_no_head(
		backend: &Backend<Block>,
		number: u64,
		parent_hash: H256,
		extrinsics_root: H256,
	) -> H256 {
		use sp_runtime::testing::Digest;

		let digest = Digest::default();
		let mut header =
			Header { number, parent_hash, state_root: Default::default(), digest, extrinsics_root };
		let mut op = backend.begin_operation().unwrap();

		let root = backend
			.state_at(parent_hash, TrieCacheContext::Untrusted)
			.unwrap_or_else(|_| {
				if parent_hash == Default::default() {
					backend.empty_state()
				} else {
					panic!("Unknown block: {parent_hash:?}")
				}
			})
			.storage_root(
				vec![(parent_hash.as_ref(), Some(parent_hash.as_ref()))].into_iter(),
				StateVersion::V1,
			)
			.0;
		header.state_root = root.into();

		op.set_block_data(header.clone(), None, None, None, NewBlockState::Normal, true)
			.unwrap();
		backend.commit_operation(op).unwrap();

		header.hash()
	}

	#[test]
	fn block_hash_inserted_correctly() {
		let backing = {
			let db = Backend::<Block>::new_test(1, 0);
			for i in 0..10 {
				assert!(db.blockchain().hash(i).unwrap().is_none());

				{
					let hash = if i == 0 {
						Default::default()
					} else {
						db.blockchain.hash(i - 1).unwrap().unwrap()
					};

					let mut op = db.begin_operation().unwrap();
					db.begin_state_operation(&mut op, hash).unwrap();
					let header = Header {
						number: i,
						parent_hash: hash,
						state_root: Default::default(),
						digest: Default::default(),
						extrinsics_root: Default::default(),
					};

					op.set_block_data(header, Some(vec![]), None, None, NewBlockState::Best, true)
						.unwrap();
					db.commit_operation(op).unwrap();
				}

				assert!(db.blockchain().hash(i).unwrap().is_some())
			}
			db.storage.db.clone()
		};

		let backend = Backend::<Block>::new(
			DatabaseSettings {
				trie_cache_maximum_size: Some(16 * 1024 * 1024),
				state_pruning: Some(PruningMode::blocks_pruning(1)),
				source: DatabaseSource::Custom { db: backing, require_create_flag: false },
				blocks_pruning: BlocksPruning::KeepFinalized,
				pruning_filters: Default::default(),
				metrics_registry: None,
			},
			0,
		)
		.unwrap();
		assert_eq!(backend.blockchain().info().best_number, 9);
		for i in 0..10 {
			assert!(backend.blockchain().hash(i).unwrap().is_some())
		}
	}

	#[test]
	fn set_state_data() {
		set_state_data_inner(StateVersion::V0);
		set_state_data_inner(StateVersion::V1);
	}
	fn set_state_data_inner(state_version: StateVersion) {
		let db = Backend::<Block>::new_test(2, 0);
		let hash = {
			let mut op = db.begin_operation().unwrap();
			let mut header = Header {
				number: 0,
				parent_hash: Default::default(),
				state_root: Default::default(),
				digest: Default::default(),
				extrinsics_root: Default::default(),
			};

			let storage = vec![(vec![1, 3, 5], vec![2, 4, 6]), (vec![1, 2, 3], vec![9, 9, 9])];

			header.state_root = op
				.old_state
				.storage_root(storage.iter().map(|(x, y)| (&x[..], Some(&y[..]))), state_version)
				.0
				.into();
			let hash = header.hash();

			op.reset_storage(
				Storage {
					top: storage.into_iter().collect(),
					children_default: Default::default(),
				},
				state_version,
			)
			.unwrap();
			op.set_block_data(header.clone(), Some(vec![]), None, None, NewBlockState::Best, true)
				.unwrap();

			db.commit_operation(op).unwrap();

			let state = db.state_at(hash, TrieCacheContext::Untrusted).unwrap();

			assert_eq!(state.storage(&[1, 3, 5]).unwrap(), Some(vec![2, 4, 6]));
			assert_eq!(state.storage(&[1, 2, 3]).unwrap(), Some(vec![9, 9, 9]));
			assert_eq!(state.storage(&[5, 5, 5]).unwrap(), None);

			hash
		};

		{
			let mut op = db.begin_operation().unwrap();
			db.begin_state_operation(&mut op, hash).unwrap();
			let mut header = Header {
				number: 1,
				parent_hash: hash,
				state_root: Default::default(),
				digest: Default::default(),
				extrinsics_root: Default::default(),
			};

			let storage = vec![(vec![1, 3, 5], None), (vec![5, 5, 5], Some(vec![4, 5, 6]))];

			let (root, overlay) = op.old_state.storage_root(
				storage.iter().map(|(k, v)| (k.as_slice(), v.as_ref().map(|v| &v[..]))),
				state_version,
			);
			op.update_db_storage(overlay).unwrap();
			header.state_root = root.into();

			op.update_storage(storage, Vec::new()).unwrap();
			op.set_block_data(header.clone(), Some(vec![]), None, None, NewBlockState::Best, true)
				.unwrap();

			db.commit_operation(op).unwrap();

			let state = db.state_at(header.hash(), TrieCacheContext::Untrusted).unwrap();

			assert_eq!(state.storage(&[1, 3, 5]).unwrap(), None);
			assert_eq!(state.storage(&[1, 2, 3]).unwrap(), Some(vec![9, 9, 9]));
			assert_eq!(state.storage(&[5, 5, 5]).unwrap(), Some(vec![4, 5, 6]));
		}
	}

	#[test]
	fn delete_only_when_negative_rc() {
		sp_tracing::try_init_simple();
		let state_version = StateVersion::default();
		let key;
		let backend = Backend::<Block>::new_test(1, 0);

		let hash = {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, Default::default()).unwrap();
			let mut header = Header {
				number: 0,
				parent_hash: Default::default(),
				state_root: Default::default(),
				digest: Default::default(),
				extrinsics_root: Default::default(),
			};

			header.state_root =
				op.old_state.storage_root(std::iter::empty(), state_version).0.into();
			let hash = header.hash();

			op.reset_storage(
				Storage { top: Default::default(), children_default: Default::default() },
				state_version,
			)
			.unwrap();

			key = op.db_updates.insert(EMPTY_PREFIX, b"hello");
			op.set_block_data(header, Some(vec![]), None, None, NewBlockState::Best, true)
				.unwrap();

			backend.commit_operation(op).unwrap();
			assert_eq!(
				backend
					.storage
					.db
					.get(columns::STATE, &sp_trie::prefixed_key::<BlakeTwo256>(&key, EMPTY_PREFIX))
					.unwrap(),
				&b"hello"[..]
			);
			hash
		};

		let hashof1 = {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, hash).unwrap();
			let mut header = Header {
				number: 1,
				parent_hash: hash,
				state_root: Default::default(),
				digest: Default::default(),
				extrinsics_root: Default::default(),
			};

			let storage: Vec<(_, _)> = vec![];

			header.state_root = op
				.old_state
				.storage_root(storage.iter().cloned().map(|(x, y)| (x, Some(y))), state_version)
				.0
				.into();
			let hash = header.hash();

			op.db_updates.insert(EMPTY_PREFIX, b"hello");
			op.db_updates.remove(&key, EMPTY_PREFIX);
			op.set_block_data(header, Some(vec![]), None, None, NewBlockState::Best, true)
				.unwrap();

			backend.commit_operation(op).unwrap();
			assert_eq!(
				backend
					.storage
					.db
					.get(columns::STATE, &sp_trie::prefixed_key::<BlakeTwo256>(&key, EMPTY_PREFIX))
					.unwrap(),
				&b"hello"[..]
			);
			hash
		};

		let hashof2 = {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, hashof1).unwrap();
			let mut header = Header {
				number: 2,
				parent_hash: hashof1,
				state_root: Default::default(),
				digest: Default::default(),
				extrinsics_root: Default::default(),
			};

			let storage: Vec<(_, _)> = vec![];

			header.state_root = op
				.old_state
				.storage_root(storage.iter().cloned().map(|(x, y)| (x, Some(y))), state_version)
				.0
				.into();
			let hash = header.hash();

			op.db_updates.remove(&key, EMPTY_PREFIX);
			op.set_block_data(header, Some(vec![]), None, None, NewBlockState::Best, true)
				.unwrap();

			backend.commit_operation(op).unwrap();

			assert!(backend
				.storage
				.db
				.get(columns::STATE, &sp_trie::prefixed_key::<BlakeTwo256>(&key, EMPTY_PREFIX))
				.is_some());
			hash
		};

		let hashof3 = {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, hashof2).unwrap();
			let mut header = Header {
				number: 3,
				parent_hash: hashof2,
				state_root: Default::default(),
				digest: Default::default(),
				extrinsics_root: Default::default(),
			};

			let storage: Vec<(_, _)> = vec![];

			header.state_root = op
				.old_state
				.storage_root(storage.iter().cloned().map(|(x, y)| (x, Some(y))), state_version)
				.0
				.into();
			let hash = header.hash();

			op.set_block_data(header, Some(vec![]), None, None, NewBlockState::Best, true)
				.unwrap();

			backend.commit_operation(op).unwrap();
			hash
		};

		let hashof4 = {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, hashof3).unwrap();
			let mut header = Header {
				number: 4,
				parent_hash: hashof3,
				state_root: Default::default(),
				digest: Default::default(),
				extrinsics_root: Default::default(),
			};

			let storage: Vec<(_, _)> = vec![];

			header.state_root = op
				.old_state
				.storage_root(storage.iter().cloned().map(|(x, y)| (x, Some(y))), state_version)
				.0
				.into();
			let hash = header.hash();

			op.set_block_data(header, Some(vec![]), None, None, NewBlockState::Best, true)
				.unwrap();

			backend.commit_operation(op).unwrap();
			assert!(backend
				.storage
				.db
				.get(columns::STATE, &sp_trie::prefixed_key::<BlakeTwo256>(&key, EMPTY_PREFIX))
				.is_none());
			hash
		};

		backend.finalize_block(hashof1, None).unwrap();
		backend.finalize_block(hashof2, None).unwrap();
		backend.finalize_block(hashof3, None).unwrap();
		backend.finalize_block(hashof4, None).unwrap();
		assert!(backend
			.storage
			.db
			.get(columns::STATE, &sp_trie::prefixed_key::<BlakeTwo256>(&key, EMPTY_PREFIX))
			.is_none());
	}

	#[test]
	fn tree_route_works() {
		let backend = Backend::<Block>::new_test(1000, 100);
		let blockchain = backend.blockchain();
		let block0 = insert_header(&backend, 0, Default::default(), None, Default::default());

		// fork from genesis: 3 prong.
		let a1 = insert_header(&backend, 1, block0, None, Default::default());
		let a2 = insert_header(&backend, 2, a1, None, Default::default());
		let a3 = insert_header(&backend, 3, a2, None, Default::default());

		// fork from genesis: 2 prong.
		let b1 = insert_header(&backend, 1, block0, None, H256::from([1; 32]));
		let b2 = insert_header(&backend, 2, b1, None, Default::default());

		{
			let tree_route = tree_route(blockchain, a1, a1).unwrap();

			assert_eq!(tree_route.common_block().hash, a1);
			assert!(tree_route.retracted().is_empty());
			assert!(tree_route.enacted().is_empty());
		}

		{
			let tree_route = tree_route(blockchain, a3, b2).unwrap();

			assert_eq!(tree_route.common_block().hash, block0);
			assert_eq!(
				tree_route.retracted().iter().map(|r| r.hash).collect::<Vec<_>>(),
				vec![a3, a2, a1]
			);
			assert_eq!(
				tree_route.enacted().iter().map(|r| r.hash).collect::<Vec<_>>(),
				vec![b1, b2]
			);
		}

		{
			let tree_route = tree_route(blockchain, a1, a3).unwrap();

			assert_eq!(tree_route.common_block().hash, a1);
			assert!(tree_route.retracted().is_empty());
			assert_eq!(
				tree_route.enacted().iter().map(|r| r.hash).collect::<Vec<_>>(),
				vec![a2, a3]
			);
		}

		{
			let tree_route = tree_route(blockchain, a3, a1).unwrap();

			assert_eq!(tree_route.common_block().hash, a1);
			assert_eq!(
				tree_route.retracted().iter().map(|r| r.hash).collect::<Vec<_>>(),
				vec![a3, a2]
			);
			assert!(tree_route.enacted().is_empty());
		}

		{
			let tree_route = tree_route(blockchain, a2, a2).unwrap();

			assert_eq!(tree_route.common_block().hash, a2);
			assert!(tree_route.retracted().is_empty());
			assert!(tree_route.enacted().is_empty());
		}
	}

	#[test]
	fn tree_route_child() {
		let backend = Backend::<Block>::new_test(1000, 100);
		let blockchain = backend.blockchain();

		let block0 = insert_header(&backend, 0, Default::default(), None, Default::default());
		let block1 = insert_header(&backend, 1, block0, None, Default::default());

		{
			let tree_route = tree_route(blockchain, block0, block1).unwrap();

			assert_eq!(tree_route.common_block().hash, block0);
			assert!(tree_route.retracted().is_empty());
			assert_eq!(
				tree_route.enacted().iter().map(|r| r.hash).collect::<Vec<_>>(),
				vec![block1]
			);
		}
	}

	#[test]
	fn lowest_common_ancestor_works() {
		let backend = Backend::<Block>::new_test(1000, 100);
		let blockchain = backend.blockchain();
		let block0 = insert_header(&backend, 0, Default::default(), None, Default::default());

		// fork from genesis: 3 prong.
		let a1 = insert_header(&backend, 1, block0, None, Default::default());
		let a2 = insert_header(&backend, 2, a1, None, Default::default());
		let a3 = insert_header(&backend, 3, a2, None, Default::default());

		// fork from genesis: 2 prong.
		let b1 = insert_header(&backend, 1, block0, None, H256::from([1; 32]));
		let b2 = insert_header(&backend, 2, b1, None, Default::default());

		{
			let lca = lowest_common_ancestor(blockchain, a3, b2).unwrap();

			assert_eq!(lca.hash, block0);
			assert_eq!(lca.number, 0);
		}

		{
			let lca = lowest_common_ancestor(blockchain, a1, a3).unwrap();

			assert_eq!(lca.hash, a1);
			assert_eq!(lca.number, 1);
		}

		{
			let lca = lowest_common_ancestor(blockchain, a3, a1).unwrap();

			assert_eq!(lca.hash, a1);
			assert_eq!(lca.number, 1);
		}

		{
			let lca = lowest_common_ancestor(blockchain, a2, a3).unwrap();

			assert_eq!(lca.hash, a2);
			assert_eq!(lca.number, 2);
		}

		{
			let lca = lowest_common_ancestor(blockchain, a2, a1).unwrap();

			assert_eq!(lca.hash, a1);
			assert_eq!(lca.number, 1);
		}

		{
			let lca = lowest_common_ancestor(blockchain, a2, a2).unwrap();

			assert_eq!(lca.hash, a2);
			assert_eq!(lca.number, 2);
		}
	}

	#[test]
	fn displaced_leaves_after_finalizing_works_with_disconnect() {
		// In this test we will create a situation that can typically happen after warp sync.
		// The situation looks like this:
		// g -> <unimported> -> a3 -> a4
		// Basically there is a gap of unimported blocks at some point in the chain.
		let backend = Backend::<Block>::new_test(1000, 100);
		let blockchain = backend.blockchain();
		let genesis_number = 0;
		let genesis_hash =
			insert_header(&backend, genesis_number, Default::default(), None, Default::default());

		let a3_number = 3;
		let a3_hash = insert_disconnected_header(
			&backend,
			a3_number,
			H256::from([200; 32]),
			H256::from([1; 32]),
			true,
		);

		let a4_number = 4;
		let a4_hash =
			insert_disconnected_header(&backend, a4_number, a3_hash, H256::from([2; 32]), true);
		{
			let displaced = blockchain
				.displaced_leaves_after_finalizing(a3_hash, a3_number, H256::from([200; 32]))
				.unwrap();
			assert_eq!(blockchain.leaves().unwrap(), vec![a4_hash, genesis_hash]);
			assert_eq!(displaced.displaced_leaves, vec![(genesis_number, genesis_hash)]);
			assert_eq!(displaced.displaced_blocks, vec![]);
		}

		{
			let displaced = blockchain
				.displaced_leaves_after_finalizing(a4_hash, a4_number, a3_hash)
				.unwrap();
			assert_eq!(blockchain.leaves().unwrap(), vec![a4_hash, genesis_hash]);
			assert_eq!(displaced.displaced_leaves, vec![(genesis_number, genesis_hash)]);
			assert_eq!(displaced.displaced_blocks, vec![]);
		}

		// Import block a1 which has the genesis block as parent.
		// g -> a1 -> <unimported> -> a3(f) -> a4
		let a1_number = 1;
		let a1_hash = insert_disconnected_header(
			&backend,
			a1_number,
			genesis_hash,
			H256::from([123; 32]),
			false,
		);
		{
			let displaced = blockchain
				.displaced_leaves_after_finalizing(a3_hash, a3_number, H256::from([2; 32]))
				.unwrap();
			assert_eq!(blockchain.leaves().unwrap(), vec![a4_hash, a1_hash]);
			assert_eq!(displaced.displaced_leaves, vec![]);
			assert_eq!(displaced.displaced_blocks, vec![]);
		}

		// Import block b1 which has the genesis block as parent.
		// g -> a1 -> <unimported> -> a3(f) -> a4
		//  \-> b1
		let b1_number = 1;
		let b1_hash = insert_disconnected_header(
			&backend,
			b1_number,
			genesis_hash,
			H256::from([124; 32]),
			false,
		);
		{
			let displaced = blockchain
				.displaced_leaves_after_finalizing(a3_hash, a3_number, H256::from([2; 32]))
				.unwrap();
			assert_eq!(blockchain.leaves().unwrap(), vec![a4_hash, a1_hash, b1_hash]);
			assert_eq!(displaced.displaced_leaves, vec![]);
			assert_eq!(displaced.displaced_blocks, vec![]);
		}

		// If branch of b blocks is higher in number than a branch, we
		// should still not prune disconnected leafs.
		// g -> a1 -> <unimported> -> a3(f) -> a4
		//  \-> b1 -> b2 ----------> b3 ----> b4 -> b5
		let b2_number = 2;
		let b2_hash =
			insert_disconnected_header(&backend, b2_number, b1_hash, H256::from([40; 32]), false);
		let b3_number = 3;
		let b3_hash =
			insert_disconnected_header(&backend, b3_number, b2_hash, H256::from([41; 32]), false);
		let b4_number = 4;
		let b4_hash =
			insert_disconnected_header(&backend, b4_number, b3_hash, H256::from([42; 32]), false);
		let b5_number = 5;
		let b5_hash =
			insert_disconnected_header(&backend, b5_number, b4_hash, H256::from([43; 32]), false);
		{
			let displaced = blockchain
				.displaced_leaves_after_finalizing(a3_hash, a3_number, H256::from([2; 32]))
				.unwrap();
			assert_eq!(blockchain.leaves().unwrap(), vec![b5_hash, a4_hash, a1_hash]);
			assert_eq!(displaced.displaced_leaves, vec![]);
			assert_eq!(displaced.displaced_blocks, vec![]);
		}

		// Even though there is a disconnect, diplace should still detect
		// branches above the block gap.
		//                              /-> c4
		// g -> a1 -> <unimported> -> a3 -> a4(f)
		//  \-> b1 -> b2 ----------> b3 -> b4 -> b5
		let c4_number = 4;
		let c4_hash =
			insert_disconnected_header(&backend, c4_number, a3_hash, H256::from([44; 32]), false);
		{
			let displaced = blockchain
				.displaced_leaves_after_finalizing(a4_hash, a4_number, a3_hash)
				.unwrap();
			assert_eq!(blockchain.leaves().unwrap(), vec![b5_hash, a4_hash, c4_hash, a1_hash]);
			assert_eq!(displaced.displaced_leaves, vec![(c4_number, c4_hash)]);
			assert_eq!(displaced.displaced_blocks, vec![c4_hash]);
		}
	}

	#[test]
	fn disconnected_blocks_do_not_become_leaves_and_warp_sync_scenario() {
		// Simulate a realistic case:
		//
		// 1. Import genesis (block #0) normally — becomes a leaf.
		// 2. Import warp sync proof blocks at #5, #10, #15 without leaf registration. Their parents
		//    are NOT in the DB. They must NOT appear as leaves.
		// 3. Import block #20 as Final. Its parent (#19) is not in the DB. Being Final, it updates
		//    finalized number to 20.
		// 4. Import blocks #1..#19 with Normal state (gap sync). Since last_finalized_num is now 20
		//    and each block number < 20, the leaf condition (number > last_finalized_num ||
		//    last_finalized_num.is_zero()) is FALSE — they must NOT become leaves.
		// 5. Assert throughout and verify displaced_leaves_after_finalizing works cleanly with no
		//    disconnected proof blocks in the displaced list.

		let backend = Backend::<Block>::new_test(1000, 100);
		let blockchain = backend.blockchain();

		let insert_block_raw = |number: u64,
		                        parent_hash: H256,
		                        ext_root: H256,
		                        state: NewBlockState,
		                        register_as_leaf: bool|
		 -> H256 {
			use sp_runtime::testing::Digest;
			let digest = Digest::default();
			let header = Header {
				number,
				parent_hash,
				state_root: Default::default(),
				digest,
				extrinsics_root: ext_root,
			};
			let mut op = backend.begin_operation().unwrap();
			op.set_block_data(header.clone(), Some(vec![]), None, None, state, register_as_leaf)
				.unwrap();
			backend.commit_operation(op).unwrap();
			header.hash()
		};

		// --- Step 1: import genesis ---
		let genesis_hash = insert_header(&backend, 0, Default::default(), None, Default::default());
		assert_eq!(blockchain.leaves().unwrap(), vec![genesis_hash]);

		// --- Step 2: import warp sync proof blocks without leaf registration ---
		// These simulate authority-set-change blocks from the warp sync proof.
		// Their parents are NOT in the DB.
		let _proof5_hash = insert_block_raw(
			5,
			H256::from([5; 32]),
			H256::from([50; 32]),
			NewBlockState::Normal,
			false,
		);
		let _proof10_hash = insert_block_raw(
			10,
			H256::from([10; 32]),
			H256::from([100; 32]),
			NewBlockState::Normal,
			false,
		);
		let _proof15_hash = insert_block_raw(
			15,
			H256::from([15; 32]),
			H256::from([150; 32]),
			NewBlockState::Normal,
			false,
		);

		// Leaves must still only contain genesis.
		assert_eq!(blockchain.leaves().unwrap(), vec![genesis_hash]);

		// The disconnected blocks should still be retrievable from the DB.
		assert!(blockchain.header(_proof5_hash).unwrap().is_some());
		assert!(blockchain.header(_proof10_hash).unwrap().is_some());
		assert!(blockchain.header(_proof15_hash).unwrap().is_some());

		// --- Step 3: import warp sync target block #20 as Final ---
		// Parent (#19) is not in the DB. Use the same low-level approach but with
		// NewBlockState::Final. Being Final, it will be set as best + finalized.
		let block20_hash = insert_block_raw(
			20,
			H256::from([19; 32]),
			H256::from([200; 32]),
			NewBlockState::Final,
			true,
		);

		// Block #20 should now be a leaf (it's best and finalized).
		let leaves = blockchain.leaves().unwrap();
		assert!(leaves.contains(&block20_hash));
		// Verify finalized number was updated to 20.
		assert_eq!(blockchain.info().finalized_number, 20);
		assert_eq!(blockchain.info().finalized_hash, block20_hash);
		// Disconnected proof blocks must still not be leaves.
		assert!(!leaves.contains(&_proof5_hash));
		assert!(!leaves.contains(&_proof10_hash));
		assert!(!leaves.contains(&_proof15_hash));

		// --- Step 4: import gap sync blocks #1..#19 with Normal state ---
		// Since last_finalized_num is 20, each block with number < 20 should NOT
		// become a leaf (the condition `number > last_finalized_num` is false).
		// Build the chain: genesis -> #1 -> #2 -> ... -> #19.
		let mut prev_hash = genesis_hash;
		let mut gap_hashes = Vec::new();
		for n in 1..=19 {
			let h = insert_disconnected_header(&backend, n, prev_hash, Default::default(), false);
			gap_hashes.push(h);
			prev_hash = h;
		}

		// Verify gap sync blocks did NOT create new leaves.
		let leaves = blockchain.leaves().unwrap();
		for (i, gap_hash) in gap_hashes.iter().enumerate() {
			assert!(
				!leaves.contains(gap_hash),
				"Gap sync block #{} should not be a leaf, but it is",
				i + 1,
			);
		}
		// Block #20 should still be a leaf.
		assert!(leaves.contains(&block20_hash));
		// Disconnected proof blocks must still not be leaves.
		assert!(!leaves.contains(&_proof5_hash));
		assert!(!leaves.contains(&_proof10_hash));
		assert!(!leaves.contains(&_proof15_hash));

		// --- Step 5: verify displaced_leaves_after_finalizing works cleanly ---
		// Call it for block #20 to verify no disconnected proof blocks appear
		// in the displaced list and it completes without errors.
		{
			let displaced = blockchain
				.displaced_leaves_after_finalizing(
					block20_hash,
					20,
					H256::from([19; 32]), // parent hash of block #20
				)
				.unwrap();
			// Disconnected proof blocks were never leaves, so they must not
			// appear in displaced_leaves.
			assert!(!displaced.displaced_leaves.iter().any(|(_, h)| *h == _proof5_hash),);
			assert!(!displaced.displaced_leaves.iter().any(|(_, h)| *h == _proof10_hash),);
			assert!(!displaced.displaced_leaves.iter().any(|(_, h)| *h == _proof15_hash),);
			// None of the gap sync blocks should be displaced leaves either
			// (they were never added as leaves).
			for gap_hash in &gap_hashes {
				assert!(!displaced.displaced_leaves.iter().any(|(_, h)| h == gap_hash),);
			}
		}
	}

	#[test]
	fn displaced_leaves_after_finalizing_works() {
		let backend = Backend::<Block>::new_test(1000, 100);
		let blockchain = backend.blockchain();
		let genesis_number = 0;
		let genesis_hash =
			insert_header(&backend, genesis_number, Default::default(), None, Default::default());

		// fork from genesis: 3 prong.
		// block 0 -> a1 -> a2 -> a3
		//        \
		//         -> b1 -> b2 -> c1 -> c2
		//              \
		//               -> d1 -> d2
		let a1_number = 1;
		let a1_hash = insert_header(&backend, a1_number, genesis_hash, None, Default::default());
		let a2_number = 2;
		let a2_hash = insert_header(&backend, a2_number, a1_hash, None, Default::default());
		let a3_number = 3;
		let a3_hash = insert_header(&backend, a3_number, a2_hash, None, Default::default());

		{
			let displaced = blockchain
				.displaced_leaves_after_finalizing(genesis_hash, genesis_number, Default::default())
				.unwrap();
			assert_eq!(displaced.displaced_leaves, vec![]);
			assert_eq!(displaced.displaced_blocks, vec![]);
		}
		{
			let displaced_a1 = blockchain
				.displaced_leaves_after_finalizing(a1_hash, a1_number, genesis_hash)
				.unwrap();
			assert_eq!(displaced_a1.displaced_leaves, vec![]);
			assert_eq!(displaced_a1.displaced_blocks, vec![]);

			let displaced_a2 = blockchain
				.displaced_leaves_after_finalizing(a2_hash, a2_number, a1_hash)
				.unwrap();
			assert_eq!(displaced_a2.displaced_leaves, vec![]);
			assert_eq!(displaced_a2.displaced_blocks, vec![]);

			let displaced_a3 = blockchain
				.displaced_leaves_after_finalizing(a3_hash, a3_number, a2_hash)
				.unwrap();
			assert_eq!(displaced_a3.displaced_leaves, vec![]);
			assert_eq!(displaced_a3.displaced_blocks, vec![]);
		}
		{
			// Finalized block is above leaves and not imported yet.
			// We will not be able to make a connection,
			// nothing can be marked as displaced.
			let displaced = blockchain
				.displaced_leaves_after_finalizing(H256::from([57; 32]), 10, H256::from([56; 32]))
				.unwrap();
			assert_eq!(displaced.displaced_leaves, vec![]);
			assert_eq!(displaced.displaced_blocks, vec![]);
		}

		// fork from genesis: 2 prong.
		let b1_number = 1;
		let b1_hash = insert_header(&backend, b1_number, genesis_hash, None, H256::from([1; 32]));
		let b2_number = 2;
		let b2_hash = insert_header(&backend, b2_number, b1_hash, None, Default::default());

		// fork from b2.
		let c1_number = 3;
		let c1_hash = insert_header(&backend, c1_number, b2_hash, None, H256::from([2; 32]));
		let c2_number = 4;
		let c2_hash = insert_header(&backend, c2_number, c1_hash, None, Default::default());

		// fork from b1.
		let d1_number = 2;
		let d1_hash = insert_header(&backend, d1_number, b1_hash, None, H256::from([3; 32]));
		let d2_number = 3;
		let d2_hash = insert_header(&backend, d2_number, d1_hash, None, Default::default());

		{
			let displaced_a1 = blockchain
				.displaced_leaves_after_finalizing(a1_hash, a1_number, genesis_hash)
				.unwrap();
			assert_eq!(
				displaced_a1.displaced_leaves,
				vec![(c2_number, c2_hash), (d2_number, d2_hash)]
			);
			let mut displaced_blocks = vec![b1_hash, b2_hash, c1_hash, c2_hash, d1_hash, d2_hash];
			displaced_blocks.sort();
			assert_eq!(displaced_a1.displaced_blocks, displaced_blocks);

			let displaced_a2 = blockchain
				.displaced_leaves_after_finalizing(a2_hash, a2_number, a1_hash)
				.unwrap();
			assert_eq!(displaced_a1.displaced_leaves, displaced_a2.displaced_leaves);
			assert_eq!(displaced_a1.displaced_blocks, displaced_a2.displaced_blocks);

			let displaced_a3 = blockchain
				.displaced_leaves_after_finalizing(a3_hash, a3_number, a2_hash)
				.unwrap();
			assert_eq!(displaced_a1.displaced_leaves, displaced_a3.displaced_leaves);
			assert_eq!(displaced_a1.displaced_blocks, displaced_a3.displaced_blocks);
		}
		{
			let displaced = blockchain
				.displaced_leaves_after_finalizing(b1_hash, b1_number, genesis_hash)
				.unwrap();
			assert_eq!(displaced.displaced_leaves, vec![(a3_number, a3_hash)]);
			let mut displaced_blocks = vec![a1_hash, a2_hash, a3_hash];
			displaced_blocks.sort();
			assert_eq!(displaced.displaced_blocks, displaced_blocks);
		}
		{
			let displaced = blockchain
				.displaced_leaves_after_finalizing(b2_hash, b2_number, b1_hash)
				.unwrap();
			assert_eq!(
				displaced.displaced_leaves,
				vec![(a3_number, a3_hash), (d2_number, d2_hash)]
			);
			let mut displaced_blocks = vec![a1_hash, a2_hash, a3_hash, d1_hash, d2_hash];
			displaced_blocks.sort();
			assert_eq!(displaced.displaced_blocks, displaced_blocks);
		}
		{
			let displaced = blockchain
				.displaced_leaves_after_finalizing(c2_hash, c2_number, c1_hash)
				.unwrap();
			assert_eq!(
				displaced.displaced_leaves,
				vec![(a3_number, a3_hash), (d2_number, d2_hash)]
			);
			let mut displaced_blocks = vec![a1_hash, a2_hash, a3_hash, d1_hash, d2_hash];
			displaced_blocks.sort();
			assert_eq!(displaced.displaced_blocks, displaced_blocks);
		}
	}

	#[test]
	fn test_tree_route_regression() {
		// NOTE: this is a test for a regression introduced in #3665, the result
		// of tree_route would be erroneously computed, since it was taking into
		// account the `ancestor` in `CachedHeaderMetadata` for the comparison.
		// in this test we simulate the same behavior with the side-effect
		// triggering the issue being eviction of a previously fetched record
		// from the cache, therefore this test is dependent on the LRU cache
		// size for header metadata, which is currently set to 5000 elements.
		let backend = Backend::<Block>::new_test(10000, 10000);
		let blockchain = backend.blockchain();

		let genesis = insert_header(&backend, 0, Default::default(), None, Default::default());

		let block100 = (1..=100).fold(genesis, |parent, n| {
			insert_header(&backend, n, parent, None, Default::default())
		});

		let block7000 = (101..=7000).fold(block100, |parent, n| {
			insert_header(&backend, n, parent, None, Default::default())
		});

		// This will cause the ancestor of `block100` to be set to `genesis` as a side-effect.
		lowest_common_ancestor(blockchain, genesis, block100).unwrap();

		// While traversing the tree we will have to do 6900 calls to
		// `header_metadata`, which will make sure we will exhaust our cache
		// which only takes 5000 elements. In particular, the `CachedHeaderMetadata` struct for
		// block #100 will be evicted and will get a new value (with ancestor set to its parent).
		let tree_route = tree_route(blockchain, block100, block7000).unwrap();

		assert!(tree_route.retracted().is_empty());
	}

	#[test]
	fn test_leaves_with_complex_block_tree() {
		let backend: Arc<Backend<substrate_test_runtime_client::runtime::Block>> =
			Arc::new(Backend::new_test(20, 20));
		substrate_test_runtime_client::trait_tests::test_leaves_for_backend(backend);
	}

	#[test]
	fn test_children_with_complex_block_tree() {
		let backend: Arc<Backend<substrate_test_runtime_client::runtime::Block>> =
			Arc::new(Backend::new_test(20, 20));
		substrate_test_runtime_client::trait_tests::test_children_for_backend(backend);
	}

	#[test]
	fn test_blockchain_query_by_number_gets_canonical() {
		let backend: Arc<Backend<substrate_test_runtime_client::runtime::Block>> =
			Arc::new(Backend::new_test(20, 20));
		substrate_test_runtime_client::trait_tests::test_blockchain_query_by_number_gets_canonical(
			backend,
		);
	}

	#[test]
	fn test_leaves_pruned_on_finality() {
		//   / 1b - 2b - 3b
		// 0 - 1a - 2a
		//   \ 1c
		let backend: Backend<Block> = Backend::new_test(10, 10);
		let block0 = insert_header(&backend, 0, Default::default(), None, Default::default());

		let block1_a = insert_header(&backend, 1, block0, None, Default::default());
		let block1_b = insert_header(&backend, 1, block0, None, [1; 32].into());
		let block1_c = insert_header(&backend, 1, block0, None, [2; 32].into());

		assert_eq!(backend.blockchain().leaves().unwrap(), vec![block1_a, block1_b, block1_c]);

		let block2_a = insert_header(&backend, 2, block1_a, None, Default::default());
		let block2_b = insert_header(&backend, 2, block1_b, None, Default::default());

		let block3_b = insert_header(&backend, 3, block2_b, None, [3; 32].into());

		assert_eq!(backend.blockchain().leaves().unwrap(), vec![block3_b, block2_a, block1_c]);

		backend.finalize_block(block1_a, None).unwrap();
		backend.finalize_block(block2_a, None).unwrap();

		// All leaves are pruned that are known to not belong to canonical branch
		assert_eq!(backend.blockchain().leaves().unwrap(), vec![block2_a]);
	}

	#[test]
	fn test_aux() {
		let backend: Backend<substrate_test_runtime_client::runtime::Block> =
			Backend::new_test(0, 0);
		assert!(backend.get_aux(b"test").unwrap().is_none());
		backend.insert_aux(&[(&b"test"[..], &b"hello"[..])], &[]).unwrap();
		assert_eq!(b"hello", &backend.get_aux(b"test").unwrap().unwrap()[..]);
		backend.insert_aux(&[], &[&b"test"[..]]).unwrap();
		assert!(backend.get_aux(b"test").unwrap().is_none());
	}

	#[test]
	fn test_finalize_block_with_justification() {
		use sc_client_api::blockchain::Backend as BlockChainBackend;

		let backend = Backend::<Block>::new_test(10, 10);

		let block0 = insert_header(&backend, 0, Default::default(), None, Default::default());
		let block1 = insert_header(&backend, 1, block0, None, Default::default());

		let justification = Some((CONS0_ENGINE_ID, vec![1, 2, 3]));
		backend.finalize_block(block1, justification.clone()).unwrap();

		assert_eq!(
			backend.blockchain().justifications(block1).unwrap(),
			justification.map(Justifications::from),
		);
	}

	#[test]
	fn test_append_justification_to_finalized_block() {
		use sc_client_api::blockchain::Backend as BlockChainBackend;

		let backend = Backend::<Block>::new_test(10, 10);

		let block0 = insert_header(&backend, 0, Default::default(), None, Default::default());
		let block1 = insert_header(&backend, 1, block0, None, Default::default());

		let just0 = (CONS0_ENGINE_ID, vec![1, 2, 3]);
		backend.finalize_block(block1, Some(just0.clone().into())).unwrap();

		let just1 = (CONS1_ENGINE_ID, vec![4, 5]);
		backend.append_justification(block1, just1.clone()).unwrap();

		let just2 = (CONS1_ENGINE_ID, vec![6, 7]);
		assert!(matches!(
			backend.append_justification(block1, just2),
			Err(ClientError::BadJustification(_))
		));

		let justifications = {
			let mut just = Justifications::from(just0);
			just.append(just1);
			just
		};
		assert_eq!(backend.blockchain().justifications(block1).unwrap(), Some(justifications),);
	}

	#[test]
	fn finalize_block_does_not_leave_best_behind_finalized() {
		let backend = Backend::<Block>::new_test(10, 10);

		let block0 = insert_header(&backend, 0, Default::default(), None, Default::default());
		let block1 = insert_header(&backend, 1, block0, None, Default::default());
		let block2 = insert_header(&backend, 2, block1, None, Default::default());
		let block3 = insert_header(&backend, 3, block2, None, Default::default());
		let block4 = insert_header(&backend, 4, block3, None, Default::default());

		assert_eq!(backend.blockchain().info().best_number, 4);

		// Move `best` back to block 3, e.g. as the result of a re-org.
		let mut op = backend.begin_operation().unwrap();
		op.mark_head(block3).unwrap();
		backend.commit_operation(op).unwrap();
		assert_eq!(backend.blockchain().info().best_hash, block3);
		assert_eq!(backend.blockchain().info().best_number, 3);

		// Finalizing block 4 must not leave `best_number` behind `finalized_number`.
		backend.finalize_block(block1, None).unwrap();
		backend.finalize_block(block2, None).unwrap();
		backend.finalize_block(block3, None).unwrap();
		backend.finalize_block(block4, None).unwrap();

		let info = backend.blockchain().info();
		assert_eq!(info.finalized_number, 4);
		assert_eq!(info.finalized_hash, block4);
		assert!(info.best_number >= info.finalized_number);
		assert_eq!(info.best_hash, block4);
	}

	#[test]
	fn test_finalize_multiple_blocks_in_single_op() {
		let backend = Backend::<Block>::new_test(10, 10);

		let block0 = insert_header(&backend, 0, Default::default(), None, Default::default());
		let block1 = insert_header(&backend, 1, block0, None, Default::default());
		let block2 = insert_header(&backend, 2, block1, None, Default::default());
		let block3 = insert_header(&backend, 3, block2, None, Default::default());
		let block4 = insert_header(&backend, 4, block3, None, Default::default());
		{
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, block0).unwrap();
			op.mark_finalized(block1, None).unwrap();
			op.mark_finalized(block2, None).unwrap();
			backend.commit_operation(op).unwrap();
		}
		{
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, block2).unwrap();
			op.mark_finalized(block3, None).unwrap();
			op.mark_finalized(block4, None).unwrap();
			backend.commit_operation(op).unwrap();
		}
	}

	#[test]
	fn storage_hash_is_cached_correctly() {
		let state_version = StateVersion::default();
		let backend = Backend::<Block>::new_test(10, 10);

		let hash0 = {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, Default::default()).unwrap();
			let mut header = Header {
				number: 0,
				parent_hash: Default::default(),
				state_root: Default::default(),
				digest: Default::default(),
				extrinsics_root: Default::default(),
			};

			let storage = vec![(b"test".to_vec(), b"test".to_vec())];

			header.state_root = op
				.old_state
				.storage_root(storage.iter().map(|(x, y)| (&x[..], Some(&y[..]))), state_version)
				.0
				.into();
			let hash = header.hash();

			op.reset_storage(
				Storage {
					top: storage.into_iter().collect(),
					children_default: Default::default(),
				},
				state_version,
			)
			.unwrap();
			op.set_block_data(header.clone(), Some(vec![]), None, None, NewBlockState::Best, true)
				.unwrap();

			backend.commit_operation(op).unwrap();

			hash
		};

		let block0_hash = backend
			.state_at(hash0, TrieCacheContext::Untrusted)
			.unwrap()
			.storage_hash(&b"test"[..])
			.unwrap();

		let hash1 = {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, hash0).unwrap();
			let mut header = Header {
				number: 1,
				parent_hash: hash0,
				state_root: Default::default(),
				digest: Default::default(),
				extrinsics_root: Default::default(),
			};

			let storage = vec![(b"test".to_vec(), Some(b"test2".to_vec()))];

			let (root, overlay) = op.old_state.storage_root(
				storage.iter().map(|(k, v)| (k.as_slice(), v.as_ref().map(|v| &v[..]))),
				state_version,
			);
			op.update_db_storage(overlay).unwrap();
			header.state_root = root.into();
			let hash = header.hash();

			op.update_storage(storage, Vec::new()).unwrap();
			op.set_block_data(header, Some(vec![]), None, None, NewBlockState::Normal, true)
				.unwrap();

			backend.commit_operation(op).unwrap();

			hash
		};

		{
			let header = backend.blockchain().header(hash1).unwrap().unwrap();
			let mut op = backend.begin_operation().unwrap();
			op.set_block_data(header, None, None, None, NewBlockState::Best, true).unwrap();
			backend.commit_operation(op).unwrap();
		}

		let block1_hash = backend
			.state_at(hash1, TrieCacheContext::Untrusted)
			.unwrap()
			.storage_hash(&b"test"[..])
			.unwrap();

		assert_ne!(block0_hash, block1_hash);
	}

	#[test]
	fn test_finalize_non_sequential() {
		let backend = Backend::<Block>::new_test(10, 10);

		let block0 = insert_header(&backend, 0, Default::default(), None, Default::default());
		let block1 = insert_header(&backend, 1, block0, None, Default::default());
		let block2 = insert_header(&backend, 2, block1, None, Default::default());
		{
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, block0).unwrap();
			op.mark_finalized(block2, None).unwrap();
			backend.commit_operation(op).unwrap_err();
		}
	}

	#[test]
	fn prune_blocks_on_finalize() {
		let pruning_modes =
			vec![BlocksPruning::Some(2), BlocksPruning::KeepFinalized, BlocksPruning::KeepAll];

		for pruning_mode in pruning_modes {
			let backend = Backend::<Block>::new_test_with_tx_storage(pruning_mode, 0);
			let mut blocks = Vec::new();
			let mut prev_hash = Default::default();
			for i in 0..5 {
				let hash = insert_block(
					&backend,
					i,
					prev_hash,
					None,
					Default::default(),
					vec![UncheckedXt::new_transaction(i.into(), ())],
					None,
				)
				.unwrap();
				blocks.push(hash);
				prev_hash = hash;
			}

			{
				let mut op = backend.begin_operation().unwrap();
				backend.begin_state_operation(&mut op, blocks[4]).unwrap();
				for i in 1..5 {
					op.mark_finalized(blocks[i], None).unwrap();
				}
				backend.commit_operation(op).unwrap();
			}
			let bc = backend.blockchain();

			if matches!(pruning_mode, BlocksPruning::Some(_)) {
				assert_eq!(None, bc.body(blocks[0]).unwrap());
				assert_eq!(None, bc.body(blocks[1]).unwrap());
				assert_eq!(None, bc.body(blocks[2]).unwrap());
				assert_eq!(
					Some(vec![UncheckedXt::new_transaction(3.into(), ())]),
					bc.body(blocks[3]).unwrap()
				);
				assert_eq!(
					Some(vec![UncheckedXt::new_transaction(4.into(), ())]),
					bc.body(blocks[4]).unwrap()
				);
			} else {
				for i in 0..5 {
					assert_eq!(
						Some(vec![UncheckedXt::new_transaction((i as u64).into(), ())]),
						bc.body(blocks[i]).unwrap()
					);
				}
			}
		}
	}

	#[test]
	fn prune_blocks_on_finalize_with_fork() {
		sp_tracing::try_init_simple();

		let pruning_modes =
			vec![BlocksPruning::Some(2), BlocksPruning::KeepFinalized, BlocksPruning::KeepAll];

		for pruning in pruning_modes {
			let backend = Backend::<Block>::new_test_with_tx_storage(pruning, 10);
			let mut blocks = Vec::new();
			let mut prev_hash = Default::default();
			for i in 0..5 {
				let hash = insert_block(
					&backend,
					i,
					prev_hash,
					None,
					Default::default(),
					vec![UncheckedXt::new_transaction(i.into(), ())],
					None,
				)
				.unwrap();
				blocks.push(hash);
				prev_hash = hash;
			}

			// insert a fork at block 2
			let fork_hash_root = insert_block(
				&backend,
				2,
				blocks[1],
				None,
				H256::random(),
				vec![UncheckedXt::new_transaction(2.into(), ())],
				None,
			)
			.unwrap();
			insert_block(
				&backend,
				3,
				fork_hash_root,
				None,
				H256::random(),
				vec![
					UncheckedXt::new_transaction(3.into(), ()),
					UncheckedXt::new_transaction(11.into(), ()),
				],
				None,
			)
			.unwrap();
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, blocks[4]).unwrap();
			op.mark_head(blocks[4]).unwrap();
			backend.commit_operation(op).unwrap();

			let bc = backend.blockchain();
			assert_eq!(
				Some(vec![UncheckedXt::new_transaction(2.into(), ())]),
				bc.body(fork_hash_root).unwrap()
			);

			for i in 1..5 {
				let mut op = backend.begin_operation().unwrap();
				backend.begin_state_operation(&mut op, blocks[4]).unwrap();
				op.mark_finalized(blocks[i], None).unwrap();
				backend.commit_operation(op).unwrap();
			}

			if matches!(pruning, BlocksPruning::Some(_)) {
				assert_eq!(None, bc.body(blocks[0]).unwrap());
				assert_eq!(None, bc.body(blocks[1]).unwrap());
				assert_eq!(None, bc.body(blocks[2]).unwrap());

				assert_eq!(
					Some(vec![UncheckedXt::new_transaction(3.into(), ())]),
					bc.body(blocks[3]).unwrap()
				);
				assert_eq!(
					Some(vec![UncheckedXt::new_transaction(4.into(), ())]),
					bc.body(blocks[4]).unwrap()
				);
			} else {
				for i in 0..5 {
					assert_eq!(
						Some(vec![UncheckedXt::new_transaction((i as u64).into(), ())]),
						bc.body(blocks[i]).unwrap()
					);
				}
			}

			if matches!(pruning, BlocksPruning::KeepAll) {
				assert_eq!(
					Some(vec![UncheckedXt::new_transaction(2.into(), ())]),
					bc.body(fork_hash_root).unwrap()
				);
			} else {
				assert_eq!(None, bc.body(fork_hash_root).unwrap());
			}

			assert_eq!(bc.info().best_number, 4);
			for i in 0..5 {
				assert!(bc.hash(i).unwrap().is_some());
			}
		}
	}

	#[test]
	fn prune_blocks_on_finalize_and_reorg() {
		// 	0 - 1b
		// 	\ - 1a - 2a - 3a
		// 	     \ - 2b

		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(10), 10);

		let make_block = |index, parent, val: u64| {
			insert_block(
				&backend,
				index,
				parent,
				None,
				H256::random(),
				vec![UncheckedXt::new_transaction(val.into(), ())],
				None,
			)
			.unwrap()
		};

		let block_0 = make_block(0, Default::default(), 0x00);
		let block_1a = make_block(1, block_0, 0x1a);
		let block_1b = make_block(1, block_0, 0x1b);
		let block_2a = make_block(2, block_1a, 0x2a);
		let block_2b = make_block(2, block_1a, 0x2b);
		let block_3a = make_block(3, block_2a, 0x3a);

		// Make sure 1b is head
		let mut op = backend.begin_operation().unwrap();
		backend.begin_state_operation(&mut op, block_0).unwrap();
		op.mark_head(block_1b).unwrap();
		backend.commit_operation(op).unwrap();

		// Finalize 3a
		let mut op = backend.begin_operation().unwrap();
		backend.begin_state_operation(&mut op, block_0).unwrap();
		op.mark_head(block_3a).unwrap();
		op.mark_finalized(block_1a, None).unwrap();
		op.mark_finalized(block_2a, None).unwrap();
		op.mark_finalized(block_3a, None).unwrap();
		backend.commit_operation(op).unwrap();

		let bc = backend.blockchain();
		assert_eq!(None, bc.body(block_1b).unwrap());
		assert_eq!(None, bc.body(block_2b).unwrap());
		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(0x00.into(), ())]),
			bc.body(block_0).unwrap()
		);
		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(0x1a.into(), ())]),
			bc.body(block_1a).unwrap()
		);
		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(0x2a.into(), ())]),
			bc.body(block_2a).unwrap()
		);
		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(0x3a.into(), ())]),
			bc.body(block_3a).unwrap()
		);
	}

	#[test]
	fn indexed_data_block_body() {
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(1), 10);

		let x0 = UncheckedXt::new_transaction(0.into(), ()).encode();
		let x1 = UncheckedXt::new_transaction(1.into(), ()).encode();
		let x0_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x0[1..]);
		let x1_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x1[1..]);
		let index = vec![
			IndexOperation::Insert {
				extrinsic: 0,
				hash: x0_hash.as_ref().to_vec(),
				size: (x0.len() - 1) as u32,
			},
			IndexOperation::Insert {
				extrinsic: 1,
				hash: x1_hash.as_ref().to_vec(),
				size: (x1.len() - 1) as u32,
			},
		];
		let hash = insert_block(
			&backend,
			0,
			Default::default(),
			None,
			Default::default(),
			vec![
				UncheckedXt::new_transaction(0.into(), ()),
				UncheckedXt::new_transaction(1.into(), ()),
			],
			Some(index),
		)
		.unwrap();
		let bc = backend.blockchain();
		assert_eq!(bc.indexed_transaction(x0_hash).unwrap().unwrap(), &x0[1..]);
		assert_eq!(bc.indexed_transaction(x1_hash).unwrap().unwrap(), &x1[1..]);

		let hashof0 = bc.info().genesis_hash;
		// Push one more blocks and make sure block is pruned and transaction index is cleared.
		let block1 =
			insert_block(&backend, 1, hash, None, Default::default(), vec![], None).unwrap();
		backend.finalize_block(block1, None).unwrap();
		assert_eq!(bc.body(hashof0).unwrap(), None);
		assert_eq!(bc.indexed_transaction(x0_hash).unwrap(), None);
		assert_eq!(bc.indexed_transaction(x1_hash).unwrap(), None);
	}

	#[test]
	fn index_invalid_size() {
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(1), 10);

		let x0 = UncheckedXt::new_transaction(0.into(), ()).encode();
		let x1 = UncheckedXt::new_transaction(1.into(), ()).encode();

		let x0_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x0[..]);
		let x1_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x1[..]);
		let index = vec![
			IndexOperation::Insert {
				extrinsic: 0,
				hash: x0_hash.as_ref().to_vec(),
				size: (x0.len()) as u32,
			},
			IndexOperation::Insert {
				extrinsic: 1,
				hash: x1_hash.as_ref().to_vec(),
				size: (x1.len() + 1) as u32,
			},
		];
		insert_block(
			&backend,
			0,
			Default::default(),
			None,
			Default::default(),
			vec![
				UncheckedXt::new_transaction(0.into(), ()),
				UncheckedXt::new_transaction(1.into(), ()),
			],
			Some(index),
		)
		.unwrap();
		let bc = backend.blockchain();
		assert_eq!(bc.indexed_transaction(x0_hash).unwrap().unwrap(), &x0[..]);
		assert_eq!(bc.indexed_transaction(x1_hash).unwrap(), None);
	}

	#[test]
	fn renew_transaction_storage() {
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(2), 10);
		let mut blocks = Vec::new();
		let mut prev_hash = Default::default();
		let x1 = UncheckedXt::new_transaction(0.into(), ()).encode();
		let x1_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x1[1..]);
		for i in 0..10 {
			let mut index = Vec::new();
			if i == 0 {
				index.push(IndexOperation::Insert {
					extrinsic: 0,
					hash: x1_hash.as_ref().to_vec(),
					size: (x1.len() - 1) as u32,
				});
			} else if i < 5 {
				// keep renewing 1st
				index.push(IndexOperation::Renew { extrinsic: 0, hash: x1_hash.as_ref().to_vec() });
			} // else stop renewing
			let hash = insert_block(
				&backend,
				i,
				prev_hash,
				None,
				Default::default(),
				vec![UncheckedXt::new_transaction(i.into(), ())],
				Some(index),
			)
			.unwrap();
			blocks.push(hash);
			prev_hash = hash;
		}

		for i in 1..10 {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, blocks[4]).unwrap();
			op.mark_finalized(blocks[i], None).unwrap();
			backend.commit_operation(op).unwrap();
			let bc = backend.blockchain();
			if i < 6 {
				assert!(bc.indexed_transaction(x1_hash).unwrap().is_some());
			} else {
				assert!(bc.indexed_transaction(x1_hash).unwrap().is_none());
			}
		}
	}

	#[test]
	fn multi_renew_transaction_storage() {
		// Test that multiple renewals within a single extrinsic work correctly
		// and that data survives across the renewal window.
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(2), 10);
		let mut blocks = Vec::new();
		let mut prev_hash = Default::default();

		// Two distinct data items
		let x1 = UncheckedXt::new_transaction(0.into(), ()).encode();
		let x2 = UncheckedXt::new_transaction(1.into(), ()).encode();
		let x1_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x1[1..]);
		let x2_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x2[1..]);

		for i in 0..10 {
			let mut index = Vec::new();
			if i == 0 {
				// Block 0: Insert both items as separate extrinsics
				index.push(IndexOperation::Insert {
					extrinsic: 0,
					hash: x1_hash.as_ref().to_vec(),
					size: (x1.len() - 1) as u32,
				});
				index.push(IndexOperation::Insert {
					extrinsic: 1,
					hash: x2_hash.as_ref().to_vec(),
					size: (x2.len() - 1) as u32,
				});
			} else if i < 5 {
				// Blocks 1-4: Renew BOTH items in a single extrinsic (multi-renew)
				index.push(IndexOperation::Renew { extrinsic: 0, hash: x1_hash.as_ref().to_vec() });
				index.push(IndexOperation::Renew { extrinsic: 0, hash: x2_hash.as_ref().to_vec() });
			}
			// Blocks 5+: stop renewing

			let body = if i == 0 {
				vec![
					UncheckedXt::new_transaction(0.into(), ()),
					UncheckedXt::new_transaction(1.into(), ()),
				]
			} else {
				vec![UncheckedXt::new_transaction(i.into(), ())]
			};
			let hash =
				insert_block(&backend, i, prev_hash, None, Default::default(), body, Some(index))
					.unwrap();
			blocks.push(hash);
			prev_hash = hash;
		}

		// Finalize progressively and check that both items survive while renewed
		for i in 1..10 {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, blocks[4]).unwrap();
			op.mark_finalized(blocks[i], None).unwrap();
			backend.commit_operation(op).unwrap();
			let bc = backend.blockchain();
			if i < 6 {
				assert!(
					bc.indexed_transaction(x1_hash).unwrap().is_some(),
					"x1 should exist at finalization step {i}"
				);
				assert!(
					bc.indexed_transaction(x2_hash).unwrap().is_some(),
					"x2 should exist at finalization step {i}"
				);
			} else {
				assert!(
					bc.indexed_transaction(x1_hash).unwrap().is_none(),
					"x1 should be pruned at finalization step {i}"
				);
				assert!(
					bc.indexed_transaction(x2_hash).unwrap().is_none(),
					"x2 should be pruned at finalization step {i}"
				);
			}
		}
	}

	#[test]
	fn multi_renew_block_indexed_body() {
		// Test that block_indexed_body returns data for all hashes in a MultiRenew extrinsic.
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(10), 10);

		let x1 = UncheckedXt::new_transaction(0.into(), ()).encode();
		let x2 = UncheckedXt::new_transaction(1.into(), ()).encode();
		let x1_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x1[1..]);
		let x2_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x2[1..]);

		// Block 0: Insert both items
		let block0 = insert_block(
			&backend,
			0,
			Default::default(),
			None,
			Default::default(),
			vec![
				UncheckedXt::new_transaction(0.into(), ()),
				UncheckedXt::new_transaction(1.into(), ()),
			],
			Some(vec![
				IndexOperation::Insert {
					extrinsic: 0,
					hash: x1_hash.as_ref().to_vec(),
					size: (x1.len() - 1) as u32,
				},
				IndexOperation::Insert {
					extrinsic: 1,
					hash: x2_hash.as_ref().to_vec(),
					size: (x2.len() - 1) as u32,
				},
			]),
		)
		.unwrap();

		// Block 1: Multi-renew both in a single extrinsic
		let block1 = insert_block(
			&backend,
			1,
			block0,
			None,
			Default::default(),
			vec![UncheckedXt::new_transaction(10.into(), ())],
			Some(vec![
				IndexOperation::Renew { extrinsic: 0, hash: x1_hash.as_ref().to_vec() },
				IndexOperation::Renew { extrinsic: 0, hash: x2_hash.as_ref().to_vec() },
			]),
		)
		.unwrap();

		let bc = backend.blockchain();
		let indexed_body = bc.block_indexed_body(block1).unwrap().unwrap();
		assert_eq!(indexed_body.len(), 2, "Should have 2 indexed data blobs");
		assert_eq!(&indexed_body[0][..], &x1[1..]);
		assert_eq!(&indexed_body[1][..], &x2[1..]);
	}

	#[test]
	fn multi_renew_prune_releases_all() {
		// Test that pruning a block with MultiRenew correctly releases all ref counts.
		// Use BlocksPruning::Some(2) and build enough blocks so both the insert block
		// and the multi-renew block get pruned.
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(2), 10);
		let mut blocks = Vec::new();
		let mut prev_hash = Default::default();

		let x1 = UncheckedXt::new_transaction(0.into(), ()).encode();
		let x2 = UncheckedXt::new_transaction(1.into(), ()).encode();
		let x1_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x1[1..]);
		let x2_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x2[1..]);

		for i in 0..6 {
			let mut index = Vec::new();
			let body = if i == 0 {
				// Block 0: Insert both items
				index.push(IndexOperation::Insert {
					extrinsic: 0,
					hash: x1_hash.as_ref().to_vec(),
					size: (x1.len() - 1) as u32,
				});
				index.push(IndexOperation::Insert {
					extrinsic: 1,
					hash: x2_hash.as_ref().to_vec(),
					size: (x2.len() - 1) as u32,
				});
				vec![
					UncheckedXt::new_transaction(0.into(), ()),
					UncheckedXt::new_transaction(1.into(), ()),
				]
			} else if i == 1 {
				// Block 1: Multi-renew both in one extrinsic
				index.push(IndexOperation::Renew { extrinsic: 0, hash: x1_hash.as_ref().to_vec() });
				index.push(IndexOperation::Renew { extrinsic: 0, hash: x2_hash.as_ref().to_vec() });
				vec![UncheckedXt::new_transaction(10.into(), ())]
			} else {
				// Blocks 2+: empty, just advancing
				vec![UncheckedXt::new_transaction(i.into(), ())]
			};
			let hash =
				insert_block(&backend, i, prev_hash, None, Default::default(), body, Some(index))
					.unwrap();
			blocks.push(hash);
			prev_hash = hash;
		}

		let bc = backend.blockchain();
		// Before finalization, data exists
		assert!(bc.indexed_transaction(x1_hash).unwrap().is_some());
		assert!(bc.indexed_transaction(x2_hash).unwrap().is_some());

		// Finalize progressively
		for i in 1..6 {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, blocks[4]).unwrap();
			op.mark_finalized(blocks[i], None).unwrap();
			backend.commit_operation(op).unwrap();
		}

		// After finalizing block 5 with pruning=2, blocks 0-3 are pruned.
		// Both insert (block 0) and multi-renew (block 1) refs are released.
		assert!(
			bc.indexed_transaction(x1_hash).unwrap().is_none(),
			"x1 should be gone after all referring blocks are pruned"
		);
		assert!(
			bc.indexed_transaction(x2_hash).unwrap().is_none(),
			"x2 should be gone after all referring blocks are pruned"
		);
	}

	#[test]
	fn multi_renew_body_reconstruction() {
		// Test that body_uncached can reconstruct extrinsics from MultiRenew blocks.
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(10), 10);

		let x1 = UncheckedXt::new_transaction(0.into(), ()).encode();
		let x2 = UncheckedXt::new_transaction(1.into(), ()).encode();
		let x1_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x1[1..]);
		let x2_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x2[1..]);

		// Block 0: Insert both
		let block0 = insert_block(
			&backend,
			0,
			Default::default(),
			None,
			Default::default(),
			vec![
				UncheckedXt::new_transaction(0.into(), ()),
				UncheckedXt::new_transaction(1.into(), ()),
			],
			Some(vec![
				IndexOperation::Insert {
					extrinsic: 0,
					hash: x1_hash.as_ref().to_vec(),
					size: (x1.len() - 1) as u32,
				},
				IndexOperation::Insert {
					extrinsic: 1,
					hash: x2_hash.as_ref().to_vec(),
					size: (x2.len() - 1) as u32,
				},
			]),
		)
		.unwrap();

		// Block 1: Multi-renew both in one extrinsic
		let renew_xt = UncheckedXt::new_transaction(10.into(), ());
		let block1 = insert_block(
			&backend,
			1,
			block0,
			None,
			Default::default(),
			vec![renew_xt.clone()],
			Some(vec![
				IndexOperation::Renew { extrinsic: 0, hash: x1_hash.as_ref().to_vec() },
				IndexOperation::Renew { extrinsic: 0, hash: x2_hash.as_ref().to_vec() },
			]),
		)
		.unwrap();

		// Reconstruct body from block 1
		let bc = backend.blockchain();
		let body = bc.body(block1).unwrap().unwrap();
		assert_eq!(body.len(), 1, "Block 1 has one extrinsic");
		assert_eq!(body[0], renew_xt, "Extrinsic should be reconstructed correctly");
	}

	#[test]
	fn single_renew_backwards_compatible() {
		// Verify that a single renewal per extrinsic still uses DbExtrinsic::Indexed,
		// preserving backwards compatibility.
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(2), 10);
		let mut prev_hash = Default::default();

		let x1 = UncheckedXt::new_transaction(0.into(), ()).encode();
		let x1_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x1[1..]);

		// Block 0: Insert
		let block0 = insert_block(
			&backend,
			0,
			prev_hash,
			None,
			Default::default(),
			vec![UncheckedXt::new_transaction(0.into(), ())],
			Some(vec![IndexOperation::Insert {
				extrinsic: 0,
				hash: x1_hash.as_ref().to_vec(),
				size: (x1.len() - 1) as u32,
			}]),
		)
		.unwrap();
		prev_hash = block0;

		// Block 1: Single renew (should produce Indexed, not MultiRenew)
		let block1 = insert_block(
			&backend,
			1,
			prev_hash,
			None,
			Default::default(),
			vec![UncheckedXt::new_transaction(1.into(), ())],
			Some(vec![IndexOperation::Renew { extrinsic: 0, hash: x1_hash.as_ref().to_vec() }]),
		)
		.unwrap();

		// Verify data is accessible
		let bc = backend.blockchain();
		assert!(bc.indexed_transaction(x1_hash).unwrap().is_some());

		// Verify body can be reconstructed (confirms Indexed variant works)
		let body = bc.body(block1).unwrap().unwrap();
		assert_eq!(body.len(), 1);
		assert_eq!(body[0], UncheckedXt::new_transaction(1.into(), ()));

		// Verify block_indexed_body returns the data
		let indexed = bc.block_indexed_body(block1).unwrap().unwrap();
		assert_eq!(indexed.len(), 1);
		assert_eq!(&indexed[0][..], &x1[1..]);
	}

	#[test]
	fn multi_renew_duplicate_hash_balanced_lifecycle() {
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(2), 10);
		let mut blocks = Vec::new();
		let mut prev_hash = Default::default();

		let x1 = UncheckedXt::new_transaction(0.into(), ()).encode();
		let x1_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x1[1..]);

		for i in 0..6 {
			let mut index = Vec::new();
			let body = if i == 0 {
				index.push(IndexOperation::Insert {
					extrinsic: 0,
					hash: x1_hash.as_ref().to_vec(),
					size: (x1.len() - 1) as u32,
				});
				vec![UncheckedXt::new_transaction(0.into(), ())]
			} else if i == 1 {
				index.push(IndexOperation::Renew { extrinsic: 0, hash: x1_hash.as_ref().to_vec() });
				index.push(IndexOperation::Renew { extrinsic: 0, hash: x1_hash.as_ref().to_vec() });
				vec![UncheckedXt::new_transaction(10.into(), ())]
			} else {
				vec![UncheckedXt::new_transaction(i.into(), ())]
			};
			let hash =
				insert_block(&backend, i, prev_hash, None, Default::default(), body, Some(index))
					.unwrap();
			blocks.push(hash);
			prev_hash = hash;
		}

		let bc = backend.blockchain();
		assert!(bc.indexed_transaction(x1_hash).unwrap().is_some());

		for i in 1..6 {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, blocks[4]).unwrap();
			op.mark_finalized(blocks[i], None).unwrap();
			backend.commit_operation(op).unwrap();
		}

		assert!(bc.indexed_transaction(x1_hash).unwrap().is_none());
	}

	#[test]
	fn multi_renew_mixed_duplicates_and_uniques() {
		// Ops [W, X, Y, W, Z]: insertion-order preserved, duplicate W kept.
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(2), 10);
		let mut blocks = Vec::new();
		let mut prev_hash = Default::default();

		let w = UncheckedXt::new_transaction(0.into(), ()).encode();
		let x = UncheckedXt::new_transaction(1.into(), ()).encode();
		let y = UncheckedXt::new_transaction(2.into(), ()).encode();
		let z = UncheckedXt::new_transaction(3.into(), ()).encode();
		let w_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&w[1..]);
		let x_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x[1..]);
		let y_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&y[1..]);
		let z_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&z[1..]);

		for i in 0..6 {
			let mut index = Vec::new();
			let body = if i == 0 {
				index.push(IndexOperation::Insert {
					extrinsic: 0,
					hash: w_hash.as_ref().to_vec(),
					size: (w.len() - 1) as u32,
				});
				index.push(IndexOperation::Insert {
					extrinsic: 1,
					hash: x_hash.as_ref().to_vec(),
					size: (x.len() - 1) as u32,
				});
				index.push(IndexOperation::Insert {
					extrinsic: 2,
					hash: y_hash.as_ref().to_vec(),
					size: (y.len() - 1) as u32,
				});
				index.push(IndexOperation::Insert {
					extrinsic: 3,
					hash: z_hash.as_ref().to_vec(),
					size: (z.len() - 1) as u32,
				});
				vec![
					UncheckedXt::new_transaction(0.into(), ()),
					UncheckedXt::new_transaction(1.into(), ()),
					UncheckedXt::new_transaction(2.into(), ()),
					UncheckedXt::new_transaction(3.into(), ()),
				]
			} else if i == 1 {
				// 5 ops: W appears twice (positions 0 and 3), X/Y/Z once each.
				index.push(IndexOperation::Renew { extrinsic: 0, hash: w_hash.as_ref().to_vec() });
				index.push(IndexOperation::Renew { extrinsic: 0, hash: x_hash.as_ref().to_vec() });
				index.push(IndexOperation::Renew { extrinsic: 0, hash: y_hash.as_ref().to_vec() });
				index.push(IndexOperation::Renew { extrinsic: 0, hash: w_hash.as_ref().to_vec() });
				index.push(IndexOperation::Renew { extrinsic: 0, hash: z_hash.as_ref().to_vec() });
				vec![UncheckedXt::new_transaction(10.into(), ())]
			} else {
				vec![UncheckedXt::new_transaction(i.into(), ())]
			};
			let hash =
				insert_block(&backend, i, prev_hash, None, Default::default(), body, Some(index))
					.unwrap();
			blocks.push(hash);
			prev_hash = hash;
		}

		let bc = backend.blockchain();

		let indexed_body = bc.block_indexed_body(blocks[1]).unwrap().unwrap();
		assert_eq!(indexed_body.len(), 5);
		assert_eq!(&indexed_body[0][..], &w[1..]);
		assert_eq!(&indexed_body[1][..], &x[1..]);
		assert_eq!(&indexed_body[2][..], &y[1..]);
		assert_eq!(&indexed_body[3][..], &w[1..]);
		assert_eq!(&indexed_body[4][..], &z[1..]);

		for i in 1..6 {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, blocks[4]).unwrap();
			op.mark_finalized(blocks[i], None).unwrap();
			backend.commit_operation(op).unwrap();
		}

		assert!(bc.indexed_transaction(w_hash).unwrap().is_none(), "W deleted");
		assert!(bc.indexed_transaction(x_hash).unwrap().is_none(), "X deleted");
		assert!(bc.indexed_transaction(y_hash).unwrap().is_none(), "Y deleted");
		assert!(bc.indexed_transaction(z_hash).unwrap().is_none(), "Z deleted");
	}

	#[test]
	fn block_indexed_body_preserves_renew_op_submission_order() {
		// `block_indexed_body(N)` returns blobs in submission order of the underlying
		// Renew ops. Sorting (e.g. via BTreeSet) would desync off-chain proof
		// construction from on-chain verification.
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::KeepAll, 10);

		let payloads: Vec<Vec<u8>> = (0..5)
			.map(|i: u64| UncheckedXt::new_transaction(i.into(), ()).encode())
			.collect();
		let hashes: Vec<<HashingFor<Block> as sp_core::Hasher>::Out> = payloads
			.iter()
			.map(|p| <HashingFor<Block> as sp_core::Hasher>::hash(&p[1..]))
			.collect();

		let mut prev_hash = Default::default();
		let insert_ops: Vec<IndexOperation> = (0..5)
			.map(|i| IndexOperation::Insert {
				extrinsic: i as u32,
				hash: hashes[i].as_ref().to_vec(),
				size: (payloads[i].len() - 1) as u32,
			})
			.collect();
		let body0: Vec<UncheckedXt> =
			(0..5).map(|i| UncheckedXt::new_transaction((i as u64).into(), ())).collect();
		prev_hash =
			insert_block(&backend, 0, prev_hash, None, Default::default(), body0, Some(insert_ops))
				.unwrap();

		// Non-monotonic submission order so any sort would visibly disturb it.
		let submission_order = [4usize, 1, 0, 3, 2];
		let renew_ops: Vec<IndexOperation> = submission_order
			.iter()
			.map(|&i| IndexOperation::Renew { extrinsic: 0, hash: hashes[i].as_ref().to_vec() })
			.collect();
		let block1 = insert_block(
			&backend,
			1,
			prev_hash,
			None,
			Default::default(),
			vec![UncheckedXt::new_transaction(100.into(), ())],
			Some(renew_ops),
		)
		.unwrap();

		let bc = backend.blockchain();
		let body_index_bytes = read_db(
			&*backend.storage.db,
			columns::KEY_LOOKUP,
			columns::BODY_INDEX,
			BlockId::<Block>::Hash(block1),
		)
		.unwrap()
		.expect("block 1 must have a BODY_INDEX entry");
		let decoded: Vec<DbExtrinsic<Block>> =
			Decode::decode(&mut &body_index_bytes[..]).expect("must decode");
		assert_eq!(decoded.len(), 1);
		match &decoded[0] {
			DbExtrinsic::MultiRenew { hashes: stored_hashes, .. } => {
				assert_eq!(stored_hashes.len(), 5);
				for (i, &order_idx) in submission_order.iter().enumerate() {
					assert_eq!(stored_hashes[i].as_ref(), hashes[order_idx].as_ref());
				}
			},
			other => panic!("expected MultiRenew; got {other:?}"),
		}

		let blobs = bc.block_indexed_body(block1).unwrap().unwrap();
		assert_eq!(blobs.len(), 5);
		for (i, &order_idx) in submission_order.iter().enumerate() {
			assert_eq!(blobs[i].as_slice(), &payloads[order_idx][1..]);
		}
	}

	#[test]
	fn insert_and_renew_same_index_renew_wins() {
		// Documents the pre-existing precedence in apply_index_ops: when both an Insert
		// and a Renew op target the same extrinsic_index, the Renew wins and the Insert
		// is silently discarded — the Insert's data write to the TRANSACTION column
		// never happens.
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(10), 10);

		let x = UncheckedXt::new_transaction(0.into(), ()).encode();
		let y = UncheckedXt::new_transaction(1.into(), ()).encode();
		let x_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x[1..]);
		let y_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&y[1..]);

		// Block 0: Insert X normally — X is now stored.
		let block0 = insert_block(
			&backend,
			0,
			Default::default(),
			None,
			Default::default(),
			vec![UncheckedXt::new_transaction(0.into(), ())],
			Some(vec![IndexOperation::Insert {
				extrinsic: 0,
				hash: x_hash.as_ref().to_vec(),
				size: (x.len() - 1) as u32,
			}]),
		)
		.unwrap();

		// Block 1: ops contain BOTH Insert{0, Y, ...} and Renew{0, X} for the same extrinsic.
		// Per apply_index_ops precedence, Renew wins and Insert{Y} is silently dropped.
		let block1 = insert_block(
			&backend,
			1,
			block0,
			None,
			Default::default(),
			vec![UncheckedXt::new_transaction(99.into(), ())],
			Some(vec![
				IndexOperation::Insert {
					extrinsic: 0,
					hash: y_hash.as_ref().to_vec(),
					size: (y.len() - 1) as u32,
				},
				IndexOperation::Renew { extrinsic: 0, hash: x_hash.as_ref().to_vec() },
			]),
		)
		.unwrap();

		let bc = backend.blockchain();

		assert!(bc.indexed_transaction(x_hash).unwrap().is_some());
		assert!(
			bc.indexed_transaction(y_hash).unwrap().is_none(),
			"Insert at the same extrinsic index as a Renew is silently dropped",
		);

		let indexed = bc.block_indexed_body(block1).unwrap().unwrap();
		assert_eq!(indexed.len(), 1);
		assert_eq!(&indexed[0][..], &x[1..]);
	}

	#[test]
	fn db_extrinsic_encoding_round_trip() {
		let entries: Vec<DbExtrinsic<Block>> = vec![
			DbExtrinsic::Indexed { hash: H256::repeat_byte(0xAA), header: vec![0x01, 0x02, 0x03] },
			DbExtrinsic::Full(UncheckedXt::new_transaction(42.into(), ())),
			DbExtrinsic::MultiRenew {
				hashes: vec![H256::repeat_byte(0xBB), H256::repeat_byte(0xCC)],
				extrinsic: vec![0x04, 0x05, 0x06, 0x07],
			},
		];

		let encoded = entries.encode();
		let decoded: Vec<DbExtrinsic<Block>> =
			Decode::decode(&mut &encoded[..]).expect("encoded DbExtrinsic vec must decode");
		assert_eq!(encoded, decoded.encode());
	}

	#[test]
	fn apply_index_ops_deterministic() {
		let body = vec![
			UncheckedXt::new_transaction(0.into(), ()),
			UncheckedXt::new_transaction(1.into(), ()),
		];
		let h1 = H256::repeat_byte(0x11).as_ref().to_vec();
		let h2 = H256::repeat_byte(0x22).as_ref().to_vec();
		let h3 = H256::repeat_byte(0x33).as_ref().to_vec();

		let ops = vec![
			IndexOperation::Renew { extrinsic: 0, hash: h1.clone() },
			IndexOperation::Renew { extrinsic: 0, hash: h2.clone() },
			IndexOperation::Renew { extrinsic: 0, hash: h1.clone() },
			IndexOperation::Renew { extrinsic: 1, hash: h3.clone() },
		];

		let mut tx1: Transaction<DbHash> = Transaction::new();
		let bytes1 = apply_index_ops::<Block>(&mut tx1, body.clone(), ops.clone(), HashMap::new());

		let mut tx2: Transaction<DbHash> = Transaction::new();
		let bytes2 = apply_index_ops::<Block>(&mut tx2, body, ops, HashMap::new());

		assert_eq!(bytes1, bytes2);

		let decoded: Vec<DbExtrinsic<Block>> =
			Decode::decode(&mut &bytes1[..]).expect("apply_index_ops output must decode");
		assert_eq!(decoded.len(), 2);
		match &decoded[0] {
			DbExtrinsic::MultiRenew { hashes, .. } => {
				assert_eq!(hashes.len(), 3);
				assert_eq!(hashes[0].as_ref(), h1.as_slice());
				assert_eq!(hashes[1].as_ref(), h2.as_slice());
				assert_eq!(hashes[2].as_ref(), h1.as_slice());
			},
			other => panic!("expected MultiRenew, got {other:?}"),
		}
		assert!(matches!(decoded[1], DbExtrinsic::Indexed { .. }));
	}

	#[test]
	fn multi_renew_in_one_block_indexed_in_another() {
		// X across three blocks: Insert, single Renew, duplicate Renew. Refcount peaks at 4.
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(2), 10);
		let mut blocks = Vec::new();
		let mut prev_hash = Default::default();

		let x = UncheckedXt::new_transaction(0.into(), ()).encode();
		let x_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x[1..]);

		for i in 0..6 {
			let mut index = Vec::new();
			let body = if i == 0 {
				index.push(IndexOperation::Insert {
					extrinsic: 0,
					hash: x_hash.as_ref().to_vec(),
					size: (x.len() - 1) as u32,
				});
				vec![UncheckedXt::new_transaction(0.into(), ())]
			} else if i == 1 {
				index.push(IndexOperation::Renew { extrinsic: 0, hash: x_hash.as_ref().to_vec() });
				vec![UncheckedXt::new_transaction(10.into(), ())]
			} else if i == 2 {
				index.push(IndexOperation::Renew { extrinsic: 0, hash: x_hash.as_ref().to_vec() });
				index.push(IndexOperation::Renew { extrinsic: 0, hash: x_hash.as_ref().to_vec() });
				vec![UncheckedXt::new_transaction(20.into(), ())]
			} else {
				vec![UncheckedXt::new_transaction(i.into(), ())]
			};
			let hash =
				insert_block(&backend, i, prev_hash, None, Default::default(), body, Some(index))
					.unwrap();
			blocks.push(hash);
			prev_hash = hash;
		}

		let bc = backend.blockchain();
		assert!(bc.indexed_transaction(x_hash).unwrap().is_some());

		for i in 1..6 {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, blocks[4]).unwrap();
			op.mark_finalized(blocks[i], None).unwrap();
			backend.commit_operation(op).unwrap();
		}

		assert!(bc.indexed_transaction(x_hash).unwrap().is_none());
	}

	#[test]
	fn remove_leaf_block_works() {
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(2), 10);
		let mut blocks = Vec::new();
		let mut prev_hash = Default::default();
		for i in 0..2 {
			let hash = insert_block(
				&backend,
				i,
				prev_hash,
				None,
				Default::default(),
				vec![UncheckedXt::new_transaction(i.into(), ())],
				None,
			)
			.unwrap();
			blocks.push(hash);
			prev_hash = hash;
		}

		for i in 0..2 {
			let hash = insert_block(
				&backend,
				2,
				blocks[1],
				None,
				sp_core::H256::random(),
				vec![UncheckedXt::new_transaction(i.into(), ())],
				None,
			)
			.unwrap();
			blocks.push(hash);
		}

		// insert a fork at block 1, which becomes best block
		let best_hash = insert_block(
			&backend,
			1,
			blocks[0],
			None,
			sp_core::H256::random(),
			vec![UncheckedXt::new_transaction(42.into(), ())],
			None,
		)
		.unwrap();

		assert_eq!(backend.blockchain().info().best_hash, best_hash);
		assert!(backend.remove_leaf_block(best_hash).is_err());

		assert_eq!(backend.blockchain().leaves().unwrap(), vec![blocks[2], blocks[3], best_hash]);
		assert_eq!(backend.blockchain().children(blocks[1]).unwrap(), vec![blocks[2], blocks[3]]);

		assert!(backend.have_state_at(blocks[3], 2));
		assert!(backend.blockchain().header(blocks[3]).unwrap().is_some());
		backend.remove_leaf_block(blocks[3]).unwrap();
		assert!(!backend.have_state_at(blocks[3], 2));
		assert!(backend.blockchain().header(blocks[3]).unwrap().is_none());
		assert_eq!(backend.blockchain().leaves().unwrap(), vec![blocks[2], best_hash]);
		assert_eq!(backend.blockchain().children(blocks[1]).unwrap(), vec![blocks[2]]);

		assert!(backend.have_state_at(blocks[2], 2));
		assert!(backend.blockchain().header(blocks[2]).unwrap().is_some());
		backend.remove_leaf_block(blocks[2]).unwrap();
		assert!(!backend.have_state_at(blocks[2], 2));
		assert!(backend.blockchain().header(blocks[2]).unwrap().is_none());
		assert_eq!(backend.blockchain().leaves().unwrap(), vec![best_hash, blocks[1]]);
		assert_eq!(backend.blockchain().children(blocks[1]).unwrap(), vec![]);

		assert!(backend.have_state_at(blocks[1], 1));
		assert!(backend.blockchain().header(blocks[1]).unwrap().is_some());
		backend.remove_leaf_block(blocks[1]).unwrap();
		assert!(!backend.have_state_at(blocks[1], 1));
		assert!(backend.blockchain().header(blocks[1]).unwrap().is_none());
		assert_eq!(backend.blockchain().leaves().unwrap(), vec![best_hash]);
		assert_eq!(backend.blockchain().children(blocks[0]).unwrap(), vec![best_hash]);
	}

	#[test]
	fn test_import_existing_block_as_new_head() {
		let backend: Backend<Block> = Backend::new_test(10, 3);
		let block0 = insert_header(&backend, 0, Default::default(), None, Default::default());
		let block1 = insert_header(&backend, 1, block0, None, Default::default());
		let block2 = insert_header(&backend, 2, block1, None, Default::default());
		let block3 = insert_header(&backend, 3, block2, None, Default::default());
		let block4 = insert_header(&backend, 4, block3, None, Default::default());
		let block5 = insert_header(&backend, 5, block4, None, Default::default());
		assert_eq!(backend.blockchain().info().best_hash, block5);

		// Insert 1 as best again. This should fail because canonicalization_delay == 3 and best ==
		// 5
		let header = Header {
			number: 1,
			parent_hash: block0,
			state_root: BlakeTwo256::trie_root(Vec::new(), StateVersion::V1),
			digest: Default::default(),
			extrinsics_root: Default::default(),
		};
		let mut op = backend.begin_operation().unwrap();
		op.set_block_data(header, None, None, None, NewBlockState::Best, true).unwrap();
		assert!(matches!(backend.commit_operation(op), Err(sp_blockchain::Error::SetHeadTooOld)));

		// Insert 2 as best again.
		let header = backend.blockchain().header(block2).unwrap().unwrap();
		let mut op = backend.begin_operation().unwrap();
		op.set_block_data(header, None, None, None, NewBlockState::Best, true).unwrap();
		backend.commit_operation(op).unwrap();
		assert_eq!(backend.blockchain().info().best_hash, block2);
	}

	#[test]
	fn test_import_existing_block_as_final() {
		let backend: Backend<Block> = Backend::new_test(10, 10);
		let block0 = insert_header(&backend, 0, Default::default(), None, Default::default());
		let block1 = insert_header(&backend, 1, block0, None, Default::default());
		let _block2 = insert_header(&backend, 2, block1, None, Default::default());
		// Genesis is auto finalized, the rest are not.
		assert_eq!(backend.blockchain().info().finalized_hash, block0);

		// Insert 1 as final again.
		let header = backend.blockchain().header(block1).unwrap().unwrap();

		let mut op = backend.begin_operation().unwrap();
		op.set_block_data(header, None, None, None, NewBlockState::Final, true).unwrap();
		backend.commit_operation(op).unwrap();

		assert_eq!(backend.blockchain().info().finalized_hash, block1);
	}

	#[test]
	fn test_import_existing_state_fails() {
		let backend: Backend<Block> = Backend::new_test(10, 10);
		let genesis =
			insert_block(&backend, 0, Default::default(), None, Default::default(), vec![], None)
				.unwrap();

		insert_block(&backend, 1, genesis, None, Default::default(), vec![], None).unwrap();
		let err = insert_block(&backend, 1, genesis, None, Default::default(), vec![], None)
			.err()
			.unwrap();
		match err {
			sp_blockchain::Error::StateDatabase(m) if m == "Block already exists" => (),
			e @ _ => panic!("Unexpected error {:?}", e),
		}
	}

	#[test]
	fn test_leaves_not_created_for_ancient_blocks() {
		let backend: Backend<Block> = Backend::new_test(10, 10);
		let block0 = insert_header(&backend, 0, Default::default(), None, Default::default());

		let block1_a = insert_header(&backend, 1, block0, None, Default::default());
		let block2_a = insert_header(&backend, 2, block1_a, None, Default::default());
		backend.finalize_block(block1_a, None).unwrap();
		assert_eq!(backend.blockchain().leaves().unwrap(), vec![block2_a]);

		// Insert a fork prior to finalization point. Leave should not be created.
		insert_header_no_head(&backend, 1, block0, [1; 32].into());
		assert_eq!(backend.blockchain().leaves().unwrap(), vec![block2_a]);
	}

	#[test]
	fn revert_non_best_blocks() {
		let backend = Backend::<Block>::new_test(10, 10);

		let genesis =
			insert_block(&backend, 0, Default::default(), None, Default::default(), vec![], None)
				.unwrap();

		let block1 =
			insert_block(&backend, 1, genesis, None, Default::default(), vec![], None).unwrap();

		let block2 =
			insert_block(&backend, 2, block1, None, Default::default(), vec![], None).unwrap();

		let block3 = {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, block1).unwrap();
			let header = Header {
				number: 3,
				parent_hash: block2,
				state_root: BlakeTwo256::trie_root(Vec::new(), StateVersion::V1),
				digest: Default::default(),
				extrinsics_root: Default::default(),
			};

			op.set_block_data(
				header.clone(),
				Some(Vec::new()),
				None,
				None,
				NewBlockState::Normal,
				true,
			)
			.unwrap();

			backend.commit_operation(op).unwrap();

			header.hash()
		};

		let block4 = {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, block2).unwrap();
			let header = Header {
				number: 4,
				parent_hash: block3,
				state_root: BlakeTwo256::trie_root(Vec::new(), StateVersion::V1),
				digest: Default::default(),
				extrinsics_root: Default::default(),
			};

			op.set_block_data(
				header.clone(),
				Some(Vec::new()),
				None,
				None,
				NewBlockState::Normal,
				true,
			)
			.unwrap();

			backend.commit_operation(op).unwrap();

			header.hash()
		};

		let block3_fork = {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, block2).unwrap();
			let header = Header {
				number: 3,
				parent_hash: block2,
				state_root: BlakeTwo256::trie_root(Vec::new(), StateVersion::V1),
				digest: Default::default(),
				extrinsics_root: H256::from_low_u64_le(42),
			};

			op.set_block_data(
				header.clone(),
				Some(Vec::new()),
				None,
				None,
				NewBlockState::Normal,
				true,
			)
			.unwrap();

			backend.commit_operation(op).unwrap();

			header.hash()
		};

		assert!(backend.have_state_at(block1, 1));
		assert!(backend.have_state_at(block2, 2));
		assert!(backend.have_state_at(block3, 3));
		assert!(backend.have_state_at(block4, 4));
		assert!(backend.have_state_at(block3_fork, 3));

		assert_eq!(backend.blockchain.leaves().unwrap(), vec![block4, block3_fork]);
		assert_eq!(4, backend.blockchain.leaves.read().highest_leaf().unwrap().0);

		assert_eq!(3, backend.revert(1, false).unwrap().0);

		assert!(backend.have_state_at(block1, 1));

		let ensure_pruned = |hash, number: u32| {
			assert_eq!(
				backend.blockchain.status(hash).unwrap(),
				sc_client_api::blockchain::BlockStatus::Unknown
			);
			assert!(
				backend
					.blockchain
					.db
					.get(columns::BODY, &number_and_hash_to_lookup_key(number, hash).unwrap())
					.is_none(),
				"{number}"
			);
			assert!(
				backend
					.blockchain
					.db
					.get(columns::HEADER, &number_and_hash_to_lookup_key(number, hash).unwrap())
					.is_none(),
				"{number}"
			);
		};

		ensure_pruned(block2, 2);
		ensure_pruned(block3, 3);
		ensure_pruned(block4, 4);
		ensure_pruned(block3_fork, 3);

		assert_eq!(backend.blockchain.leaves().unwrap(), vec![block1]);
		assert_eq!(1, backend.blockchain.leaves.read().highest_leaf().unwrap().0);
	}

	#[test]
	fn revert_finalized_blocks() {
		let pruning_modes = [BlocksPruning::Some(10), BlocksPruning::KeepAll];

		// we will create a chain with 11 blocks, finalize block #8 and then
		// attempt to revert 5 blocks.
		for pruning_mode in pruning_modes {
			let backend = Backend::<Block>::new_test_with_tx_storage(pruning_mode, 1);

			let mut parent = Default::default();
			for i in 0..=10 {
				parent = insert_block(&backend, i, parent, None, Default::default(), vec![], None)
					.unwrap();
			}

			assert_eq!(backend.blockchain().info().best_number, 10);

			let block8 = backend.blockchain().hash(8).unwrap().unwrap();
			backend.finalize_block(block8, None).unwrap();
			backend.revert(5, true).unwrap();

			match pruning_mode {
				// we can only revert to blocks for which we have state, if pruning is enabled
				// then the last state available will be that of the latest finalized block
				BlocksPruning::Some(_) => {
					assert_eq!(backend.blockchain().info().finalized_number, 8)
				},
				// otherwise if we're not doing state pruning we can revert past finalized blocks
				_ => assert_eq!(backend.blockchain().info().finalized_number, 5),
			}
		}
	}

	#[test]
	fn test_no_duplicated_leaves_allowed() {
		let backend: Backend<Block> = Backend::new_test(10, 10);
		let block0 = insert_header(&backend, 0, Default::default(), None, Default::default());
		let block1 = insert_header(&backend, 1, block0, None, Default::default());
		// Add block 2 not as the best block
		let block2 = insert_header_no_head(&backend, 2, block1, Default::default());
		assert_eq!(backend.blockchain().leaves().unwrap(), vec![block2]);
		assert_eq!(backend.blockchain().info().best_hash, block1);

		// Add block 2 as the best block
		let block2 = insert_header(&backend, 2, block1, None, Default::default());
		assert_eq!(backend.blockchain().leaves().unwrap(), vec![block2]);
		assert_eq!(backend.blockchain().info().best_hash, block2);
	}

	#[test]
	fn force_delayed_canonicalize_waiting_for_blocks_to_be_finalized() {
		let pruning_modes =
			[BlocksPruning::Some(10), BlocksPruning::KeepAll, BlocksPruning::KeepFinalized];

		for pruning_mode in pruning_modes {
			eprintln!("Running with pruning mode: {:?}", pruning_mode);

			let backend = Backend::<Block>::new_test_with_tx_storage(pruning_mode, 1);

			let genesis = insert_block(
				&backend,
				0,
				Default::default(),
				None,
				Default::default(),
				vec![],
				None,
			)
			.unwrap();

			let block1 = {
				let mut op = backend.begin_operation().unwrap();
				backend.begin_state_operation(&mut op, genesis).unwrap();
				let mut header = Header {
					number: 1,
					parent_hash: genesis,
					state_root: Default::default(),
					digest: Default::default(),
					extrinsics_root: Default::default(),
				};

				let storage = vec![(vec![1, 3, 5], None), (vec![5, 5, 5], Some(vec![4, 5, 6]))];

				let (root, overlay) = op.old_state.storage_root(
					storage.iter().map(|(k, v)| (k.as_slice(), v.as_ref().map(|v| &v[..]))),
					StateVersion::V1,
				);
				op.update_db_storage(overlay).unwrap();
				header.state_root = root.into();

				op.update_storage(storage, Vec::new()).unwrap();

				op.set_block_data(
					header.clone(),
					Some(Vec::new()),
					None,
					None,
					NewBlockState::Normal,
					true,
				)
				.unwrap();

				backend.commit_operation(op).unwrap();

				header.hash()
			};

			if matches!(pruning_mode, BlocksPruning::Some(_)) {
				assert_eq!(
					LastCanonicalized::Block(0),
					backend.storage.state_db.last_canonicalized()
				);
			}

			// This should not trigger any forced canonicalization as we didn't have imported any
			// best block by now.
			let block2 = {
				let mut op = backend.begin_operation().unwrap();
				backend.begin_state_operation(&mut op, block1).unwrap();
				let mut header = Header {
					number: 2,
					parent_hash: block1,
					state_root: Default::default(),
					digest: Default::default(),
					extrinsics_root: Default::default(),
				};

				let storage = vec![(vec![5, 5, 5], Some(vec![4, 5, 6, 2]))];

				let (root, overlay) = op.old_state.storage_root(
					storage.iter().map(|(k, v)| (k.as_slice(), v.as_ref().map(|v| &v[..]))),
					StateVersion::V1,
				);
				op.update_db_storage(overlay).unwrap();
				header.state_root = root.into();

				op.update_storage(storage, Vec::new()).unwrap();

				op.set_block_data(
					header.clone(),
					Some(Vec::new()),
					None,
					None,
					NewBlockState::Normal,
					true,
				)
				.unwrap();

				backend.commit_operation(op).unwrap();

				header.hash()
			};

			if matches!(pruning_mode, BlocksPruning::Some(_)) {
				assert_eq!(
					LastCanonicalized::Block(0),
					backend.storage.state_db.last_canonicalized()
				);
			}

			// This should also not trigger it yet, because we import a best block, but the best
			// block from the POV of the db is still at `0`.
			let block3 = {
				let mut op = backend.begin_operation().unwrap();
				backend.begin_state_operation(&mut op, block2).unwrap();
				let mut header = Header {
					number: 3,
					parent_hash: block2,
					state_root: Default::default(),
					digest: Default::default(),
					extrinsics_root: Default::default(),
				};

				let storage = vec![(vec![5, 5, 5], Some(vec![4, 5, 6, 3]))];

				let (root, overlay) = op.old_state.storage_root(
					storage.iter().map(|(k, v)| (k.as_slice(), v.as_ref().map(|v| &v[..]))),
					StateVersion::V1,
				);
				op.update_db_storage(overlay).unwrap();
				header.state_root = root.into();

				op.update_storage(storage, Vec::new()).unwrap();

				op.set_block_data(
					header.clone(),
					Some(Vec::new()),
					None,
					None,
					NewBlockState::Best,
					true,
				)
				.unwrap();

				backend.commit_operation(op).unwrap();

				header.hash()
			};

			// Now it should kick in.
			let block4 = {
				let mut op = backend.begin_operation().unwrap();
				backend.begin_state_operation(&mut op, block3).unwrap();
				let mut header = Header {
					number: 4,
					parent_hash: block3,
					state_root: Default::default(),
					digest: Default::default(),
					extrinsics_root: Default::default(),
				};

				let storage = vec![(vec![5, 5, 5], Some(vec![4, 5, 6, 4]))];

				let (root, overlay) = op.old_state.storage_root(
					storage.iter().map(|(k, v)| (k.as_slice(), v.as_ref().map(|v| &v[..]))),
					StateVersion::V1,
				);
				op.update_db_storage(overlay).unwrap();
				header.state_root = root.into();

				op.update_storage(storage, Vec::new()).unwrap();

				op.set_block_data(
					header.clone(),
					Some(Vec::new()),
					None,
					None,
					NewBlockState::Best,
					true,
				)
				.unwrap();

				backend.commit_operation(op).unwrap();

				header.hash()
			};

			if matches!(pruning_mode, BlocksPruning::Some(_)) {
				assert_eq!(
					LastCanonicalized::Block(2),
					backend.storage.state_db.last_canonicalized()
				);
			}

			assert_eq!(block1, backend.blockchain().hash(1).unwrap().unwrap());
			assert_eq!(block2, backend.blockchain().hash(2).unwrap().unwrap());
			assert_eq!(block3, backend.blockchain().hash(3).unwrap().unwrap());
			assert_eq!(block4, backend.blockchain().hash(4).unwrap().unwrap());
		}
	}

	#[test]
	fn test_pinned_blocks_on_finalize() {
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(1), 10);
		let mut blocks = Vec::new();
		let mut prev_hash = Default::default();

		let build_justification = |i: u64| ([0, 0, 0, 0], vec![i.try_into().unwrap()]);
		// Block tree:
		//   0 -> 1 -> 2 -> 3 -> 4
		for i in 0..5 {
			let hash = insert_block(
				&backend,
				i,
				prev_hash,
				None,
				Default::default(),
				vec![UncheckedXt::new_transaction(i.into(), ())],
				None,
			)
			.unwrap();
			blocks.push(hash);
			// Avoid block pruning.
			backend.pin_block(blocks[i as usize]).unwrap();

			prev_hash = hash;
		}

		let bc = backend.blockchain();

		// Check that we can properly access values when there is reference count
		// but no value.
		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(1.into(), ())]),
			bc.body(blocks[1]).unwrap()
		);

		// Block 1 gets pinned three times
		backend.pin_block(blocks[1]).unwrap();
		backend.pin_block(blocks[1]).unwrap();

		// Finalize all blocks. This will trigger pruning.
		let mut op = backend.begin_operation().unwrap();
		backend.begin_state_operation(&mut op, blocks[4]).unwrap();
		for i in 1..5 {
			op.mark_finalized(blocks[i], Some(build_justification(i.try_into().unwrap())))
				.unwrap();
		}
		backend.commit_operation(op).unwrap();

		// Block 0, 1, 2, 3 are pinned, so all values should be cached.
		// Block 4 is inside the pruning window, its value is in db.
		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(0.into(), ())]),
			bc.body(blocks[0]).unwrap()
		);

		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(1.into(), ())]),
			bc.body(blocks[1]).unwrap()
		);
		assert_eq!(
			Some(Justifications::from(build_justification(1))),
			bc.justifications(blocks[1]).unwrap()
		);

		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(2.into(), ())]),
			bc.body(blocks[2]).unwrap()
		);
		assert_eq!(
			Some(Justifications::from(build_justification(2))),
			bc.justifications(blocks[2]).unwrap()
		);

		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(3.into(), ())]),
			bc.body(blocks[3]).unwrap()
		);
		assert_eq!(
			Some(Justifications::from(build_justification(3))),
			bc.justifications(blocks[3]).unwrap()
		);

		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(4.into(), ())]),
			bc.body(blocks[4]).unwrap()
		);
		assert_eq!(
			Some(Justifications::from(build_justification(4))),
			bc.justifications(blocks[4]).unwrap()
		);

		// Unpin all blocks. Values should be removed from cache.
		for block in &blocks {
			backend.unpin_block(*block);
		}

		assert!(bc.body(blocks[0]).unwrap().is_none());
		// Block 1 was pinned twice, we expect it to be still cached
		assert!(bc.body(blocks[1]).unwrap().is_some());
		assert!(bc.justifications(blocks[1]).unwrap().is_some());
		// Headers should also be available while pinned
		assert!(bc.header(blocks[1]).ok().flatten().is_some());
		assert!(bc.body(blocks[2]).unwrap().is_none());
		assert!(bc.justifications(blocks[2]).unwrap().is_none());
		assert!(bc.body(blocks[3]).unwrap().is_none());
		assert!(bc.justifications(blocks[3]).unwrap().is_none());

		// After these unpins, block 1 should also be removed
		backend.unpin_block(blocks[1]);
		assert!(bc.body(blocks[1]).unwrap().is_some());
		assert!(bc.justifications(blocks[1]).unwrap().is_some());
		backend.unpin_block(blocks[1]);
		assert!(bc.body(blocks[1]).unwrap().is_none());
		assert!(bc.justifications(blocks[1]).unwrap().is_none());

		// Block 4 is inside the pruning window and still kept
		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(4.into(), ())]),
			bc.body(blocks[4]).unwrap()
		);
		assert_eq!(
			Some(Justifications::from(build_justification(4))),
			bc.justifications(blocks[4]).unwrap()
		);

		// Block tree:
		//   0 -> 1 -> 2 -> 3 -> 4 -> 5
		let hash = insert_block(
			&backend,
			5,
			prev_hash,
			None,
			Default::default(),
			vec![UncheckedXt::new_transaction(5.into(), ())],
			None,
		)
		.unwrap();
		blocks.push(hash);

		backend.pin_block(blocks[4]).unwrap();
		// Mark block 5 as finalized.
		let mut op = backend.begin_operation().unwrap();
		backend.begin_state_operation(&mut op, blocks[5]).unwrap();
		op.mark_finalized(blocks[5], Some(build_justification(5))).unwrap();
		backend.commit_operation(op).unwrap();

		assert!(bc.body(blocks[0]).unwrap().is_none());
		assert!(bc.body(blocks[1]).unwrap().is_none());
		assert!(bc.body(blocks[2]).unwrap().is_none());
		assert!(bc.body(blocks[3]).unwrap().is_none());

		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(4.into(), ())]),
			bc.body(blocks[4]).unwrap()
		);
		assert_eq!(
			Some(Justifications::from(build_justification(4))),
			bc.justifications(blocks[4]).unwrap()
		);
		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(5.into(), ())]),
			bc.body(blocks[5]).unwrap()
		);
		assert!(bc.header(blocks[5]).ok().flatten().is_some());

		backend.unpin_block(blocks[4]);
		assert!(bc.body(blocks[4]).unwrap().is_none());
		assert!(bc.justifications(blocks[4]).unwrap().is_none());

		// Append a justification to block 5.
		backend.append_justification(blocks[5], ([0, 0, 0, 1], vec![42])).unwrap();

		let hash = insert_block(
			&backend,
			6,
			blocks[5],
			None,
			Default::default(),
			vec![UncheckedXt::new_transaction(6.into(), ())],
			None,
		)
		.unwrap();
		blocks.push(hash);

		// Pin block 5 so it gets loaded into the cache on prune
		backend.pin_block(blocks[5]).unwrap();

		// Finalize block 6 so block 5 gets pruned. Since it is pinned both justifications should be
		// in memory.
		let mut op = backend.begin_operation().unwrap();
		backend.begin_state_operation(&mut op, blocks[6]).unwrap();
		op.mark_finalized(blocks[6], None).unwrap();
		backend.commit_operation(op).unwrap();

		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(5.into(), ())]),
			bc.body(blocks[5]).unwrap()
		);
		assert!(bc.header(blocks[5]).ok().flatten().is_some());
		let mut expected = Justifications::from(build_justification(5));
		expected.append(([0, 0, 0, 1], vec![42]));
		assert_eq!(Some(expected), bc.justifications(blocks[5]).unwrap());
	}

	#[test]
	fn test_pinned_blocks_on_finalize_with_fork() {
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(1), 10);
		let mut blocks = Vec::new();
		let mut prev_hash = Default::default();

		// Block tree:
		//   0 -> 1 -> 2 -> 3 -> 4
		for i in 0..5 {
			let hash = insert_block(
				&backend,
				i,
				prev_hash,
				None,
				Default::default(),
				vec![UncheckedXt::new_transaction(i.into(), ())],
				None,
			)
			.unwrap();
			blocks.push(hash);

			// Avoid block pruning.
			backend.pin_block(blocks[i as usize]).unwrap();

			prev_hash = hash;
		}

		// Insert a fork at the second block.
		// Block tree:
		//   0 -> 1 -> 2 -> 3 -> 4
		//        \ -> 2 -> 3
		let fork_hash_root = insert_block(
			&backend,
			2,
			blocks[1],
			None,
			H256::random(),
			vec![UncheckedXt::new_transaction(2.into(), ())],
			None,
		)
		.unwrap();
		let fork_hash_3 = insert_block(
			&backend,
			3,
			fork_hash_root,
			None,
			H256::random(),
			vec![
				UncheckedXt::new_transaction(3.into(), ()),
				UncheckedXt::new_transaction(11.into(), ()),
			],
			None,
		)
		.unwrap();

		// Do not prune the fork hash.
		backend.pin_block(fork_hash_3).unwrap();

		let mut op = backend.begin_operation().unwrap();
		backend.begin_state_operation(&mut op, blocks[4]).unwrap();
		op.mark_head(blocks[4]).unwrap();
		backend.commit_operation(op).unwrap();

		for i in 1..5 {
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, blocks[4]).unwrap();
			op.mark_finalized(blocks[i], None).unwrap();
			backend.commit_operation(op).unwrap();
		}

		let bc = backend.blockchain();
		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(0.into(), ())]),
			bc.body(blocks[0]).unwrap()
		);
		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(1.into(), ())]),
			bc.body(blocks[1]).unwrap()
		);
		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(2.into(), ())]),
			bc.body(blocks[2]).unwrap()
		);
		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(3.into(), ())]),
			bc.body(blocks[3]).unwrap()
		);
		assert_eq!(
			Some(vec![UncheckedXt::new_transaction(4.into(), ())]),
			bc.body(blocks[4]).unwrap()
		);
		// Check the fork hashes.
		assert_eq!(None, bc.body(fork_hash_root).unwrap());
		assert_eq!(
			Some(vec![
				UncheckedXt::new_transaction(3.into(), ()),
				UncheckedXt::new_transaction(11.into(), ())
			]),
			bc.body(fork_hash_3).unwrap()
		);

		// Unpin all blocks, except the forked one.
		for block in &blocks {
			backend.unpin_block(*block);
		}
		assert!(bc.body(blocks[0]).unwrap().is_none());
		assert!(bc.body(blocks[1]).unwrap().is_none());
		assert!(bc.body(blocks[2]).unwrap().is_none());
		assert!(bc.body(blocks[3]).unwrap().is_none());

		assert!(bc.body(fork_hash_3).unwrap().is_some());
		backend.unpin_block(fork_hash_3);
		assert!(bc.body(fork_hash_3).unwrap().is_none());
	}

	#[test]
	fn prune_blocks_with_empty_predicates_prunes_all() {
		// Test backward compatibility: empty predicates means all blocks are pruned
		let backend = Backend::<Block>::new_test_with_tx_storage_and_filters(
			BlocksPruning::Some(2),
			0,
			vec![], // Empty predicates
		);

		let mut blocks = Vec::new();
		let mut prev_hash = Default::default();

		// Create 5 blocks
		for i in 0..5 {
			let hash = insert_block(
				&backend,
				i,
				prev_hash,
				None,
				Default::default(),
				vec![UncheckedXt::new_transaction(i.into(), ())],
				None,
			)
			.unwrap();
			blocks.push(hash);
			prev_hash = hash;
		}

		// Justification - but no predicate to preserve it
		let justification = (CONS0_ENGINE_ID, vec![1, 2, 3]);

		// Finalize blocks, adding justification to block 1
		{
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, blocks[4]).unwrap();
			op.mark_finalized(blocks[1], Some(justification.clone())).unwrap();
			op.mark_finalized(blocks[2], None).unwrap();
			op.mark_finalized(blocks[3], None).unwrap();
			op.mark_finalized(blocks[4], None).unwrap();
			backend.commit_operation(op).unwrap();
		}

		let bc = backend.blockchain();

		// All blocks outside pruning window should be pruned, even with justification
		assert_eq!(None, bc.body(blocks[0]).unwrap());
		assert_eq!(None, bc.body(blocks[1]).unwrap()); // Has justification but no predicate
		assert_eq!(None, bc.body(blocks[2]).unwrap());

		// Blocks 3 and 4 are within the pruning window
		assert!(bc.body(blocks[3]).unwrap().is_some());
		assert!(bc.body(blocks[4]).unwrap().is_some());
	}

	#[test]
	fn prune_blocks_multiple_filters_or_logic() {
		// Test that multiple filters use OR logic: if ANY filter matches, block is kept
		let backend = Backend::<Block>::new_test_with_tx_storage_and_filters(
			BlocksPruning::Some(2),
			0,
			vec![
				Arc::new(|j: &Justifications| j.get(CONS0_ENGINE_ID).is_some()),
				Arc::new(|j: &Justifications| j.get(CONS1_ENGINE_ID).is_some()),
			],
		);

		let mut blocks = Vec::new();
		let mut prev_hash = Default::default();

		// Create 7 blocks
		for i in 0..7 {
			let hash = insert_block(
				&backend,
				i,
				prev_hash,
				None,
				Default::default(),
				vec![UncheckedXt::new_transaction(i.into(), ())],
				None,
			)
			.unwrap();
			blocks.push(hash);
			prev_hash = hash;
		}

		let cons0_justification = (CONS0_ENGINE_ID, vec![1, 2, 3]);
		let cons1_justification = (CONS1_ENGINE_ID, vec![4, 5, 6]);

		// Finalize blocks with different justification patterns
		{
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, blocks[6]).unwrap();
			// Block 1: CONS0 only - should be preserved
			op.mark_finalized(blocks[1], Some(cons0_justification.clone())).unwrap();
			// Block 2: CONS1 only - should be preserved
			op.mark_finalized(blocks[2], Some(cons1_justification.clone())).unwrap();
			// Block 3: No justification - should be pruned
			op.mark_finalized(blocks[3], None).unwrap();
			// Block 4: Random/unknown engine ID - should be pruned
			op.mark_finalized(blocks[4], Some(([9, 9, 9, 9], vec![7, 8, 9]))).unwrap();
			op.mark_finalized(blocks[5], None).unwrap();
			op.mark_finalized(blocks[6], None).unwrap();
			backend.commit_operation(op).unwrap();
		}

		let bc = backend.blockchain();

		// Block 0 should be pruned (outside window, no justification)
		assert_eq!(None, bc.body(blocks[0]).unwrap());

		// Block 1 should be preserved (has CONS0 justification)
		assert!(bc.body(blocks[1]).unwrap().is_some());

		// Block 2 should be preserved (has CONS1 justification)
		assert!(bc.body(blocks[2]).unwrap().is_some());

		// Block 3 should be pruned (no justification)
		assert_eq!(None, bc.body(blocks[3]).unwrap());

		// Block 4 should be pruned (unknown engine ID)
		assert_eq!(None, bc.body(blocks[4]).unwrap());

		// Blocks 5 and 6 are within the pruning window
		assert!(bc.body(blocks[5]).unwrap().is_some());
		assert!(bc.body(blocks[6]).unwrap().is_some());
	}

	#[test]
	fn prune_blocks_filter_only_matches_specific_engine() {
		// Test that a filter for one engine ID does NOT preserve blocks with a different engine ID
		let backend = Backend::<Block>::new_test_with_tx_storage_and_filters(
			BlocksPruning::Some(2),
			0,
			vec![Arc::new(|j: &Justifications| j.get(CONS0_ENGINE_ID).is_some())],
		);

		let mut blocks = Vec::new();
		let mut prev_hash = Default::default();

		// Create 5 blocks
		for i in 0..5 {
			let hash = insert_block(
				&backend,
				i,
				prev_hash,
				None,
				Default::default(),
				vec![UncheckedXt::new_transaction(i.into(), ())],
				None,
			)
			.unwrap();
			blocks.push(hash);
			prev_hash = hash;
		}

		let cons1_justification = (CONS1_ENGINE_ID, vec![4, 5, 6]);

		// Finalize blocks, adding CONS1 justification to block 1
		{
			let mut op = backend.begin_operation().unwrap();
			backend.begin_state_operation(&mut op, blocks[4]).unwrap();
			// Block 1 gets CONS1 justification - should NOT be preserved by CONS0 filter
			op.mark_finalized(blocks[1], Some(cons1_justification.clone())).unwrap();
			op.mark_finalized(blocks[2], None).unwrap();
			op.mark_finalized(blocks[3], None).unwrap();
			op.mark_finalized(blocks[4], None).unwrap();
			backend.commit_operation(op).unwrap();
		}

		let bc = backend.blockchain();

		// Block 0 should be pruned
		assert_eq!(None, bc.body(blocks[0]).unwrap());

		// Block 1 should also be pruned (CONS1 justification, but only CONS0 filter)
		assert_eq!(None, bc.body(blocks[1]).unwrap());

		// Block 2 should be pruned
		assert_eq!(None, bc.body(blocks[2]).unwrap());

		// Blocks 3 and 4 are within the pruning window
		assert!(bc.body(blocks[3]).unwrap().is_some());
		assert!(bc.body(blocks[4]).unwrap().is_some());
	}

	/// Insert a header without body as best block. This triggers `MissingBody` gap creation
	/// when the parent header exists and `create_gap` is true.
	fn insert_header_no_body_as_best(
		backend: &Backend<Block>,
		number: u64,
		parent_hash: H256,
	) -> H256 {
		use sp_runtime::testing::Digest;

		let digest = Digest::default();
		let header = Header {
			number,
			parent_hash,
			state_root: Default::default(),
			digest,
			extrinsics_root: Default::default(),
		};

		let mut op = backend.begin_operation().unwrap();
		// body = None triggers MissingBody gap when parent exists
		op.set_block_data(header.clone(), None, None, None, NewBlockState::Best, true)
			.unwrap();
		backend.commit_operation(op).unwrap();

		header.hash()
	}

	/// Re-open a backend from an existing database with the given blocks pruning mode.
	fn reopen_backend(
		db: Arc<dyn sp_database::Database<DbHash>>,
		blocks_pruning: BlocksPruning,
	) -> Backend<Block> {
		let state_pruning = match blocks_pruning {
			BlocksPruning::KeepAll => PruningMode::ArchiveAll,
			BlocksPruning::KeepFinalized => PruningMode::ArchiveCanonical,
			BlocksPruning::Some(n) => PruningMode::blocks_pruning(n),
		};
		Backend::<Block>::new(
			DatabaseSettings {
				trie_cache_maximum_size: Some(16 * 1024 * 1024),
				state_pruning: Some(state_pruning),
				source: DatabaseSource::Custom { db, require_create_flag: false },
				blocks_pruning,
				pruning_filters: Default::default(),
				metrics_registry: None,
			},
			0,
		)
		.unwrap()
	}

	#[test]
	fn missing_body_gap_is_removed_for_non_archive_node() {
		// Create a non-archive backend and produce a multi-block MissingBody gap.
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(100), 0);
		assert!(!backend.is_archive);

		let genesis_hash = insert_header(&backend, 0, Default::default(), None, Default::default());

		// Insert blocks 1..3 without bodies — creates a MissingBody gap spanning blocks 1 to 3.
		let hash_1 = insert_header_no_body_as_best(&backend, 1, genesis_hash);
		let hash_2 = insert_header_no_body_as_best(&backend, 2, hash_1);
		insert_header_no_body_as_best(&backend, 3, hash_2);

		let info = backend.blockchain().info();
		assert!(info.block_gap.is_some(), "MissingBody gap should have been created");
		let gap = info.block_gap.unwrap();
		assert!(matches!(gap.gap_type, BlockGapType::MissingBody));
		assert_eq!(gap.start, 1);
		assert_eq!(gap.end, 3);

		// Re-open the same database as a non-archive node.
		let db = backend.storage.db.clone();
		let backend = reopen_backend(db, BlocksPruning::Some(100));
		assert!(!backend.is_archive);

		// The multi-block gap should have been removed on re-open.
		let info = backend.blockchain().info();
		assert!(
			info.block_gap.is_none(),
			"MissingBody gap should be removed for non-archive nodes, got: {:?}",
			info.block_gap,
		);
	}

	#[test]
	fn missing_body_gap_is_preserved_for_archive_node() {
		// Create a backend with archive pruning and produce a multi-block MissingBody gap.
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::KeepAll, 0);
		assert!(backend.is_archive);

		let genesis_hash = insert_header(&backend, 0, Default::default(), None, Default::default());

		// Insert blocks 1..3 without bodies — creates a MissingBody gap spanning blocks 1 to 3.
		let hash_1 = insert_header_no_body_as_best(&backend, 1, genesis_hash);
		let hash_2 = insert_header_no_body_as_best(&backend, 2, hash_1);
		insert_header_no_body_as_best(&backend, 3, hash_2);

		let info = backend.blockchain().info();
		assert!(info.block_gap.is_some(), "MissingBody gap should have been created");
		let gap = info.block_gap.unwrap();
		assert!(matches!(gap.gap_type, BlockGapType::MissingBody));
		assert_eq!(gap.start, 1);
		assert_eq!(gap.end, 3);

		// Re-open the same database as an archive node.
		let db = backend.storage.db.clone();
		let backend = reopen_backend(db, BlocksPruning::KeepAll);
		assert!(backend.is_archive);

		// The gap should be preserved for archive nodes.
		let info = backend.blockchain().info();
		assert!(info.block_gap.is_some(), "MissingBody gap should be preserved for archive nodes",);
		let gap = info.block_gap.unwrap();
		assert!(matches!(gap.gap_type, BlockGapType::MissingBody));
		assert_eq!(gap.start, 1);
		assert_eq!(gap.end, 3);
	}

	#[test]
	fn missing_header_and_body_gap_is_preserved_for_non_archive_node() {
		// Create a non-archive backend and produce a MissingHeaderAndBody gap (from warp sync).
		let backend = Backend::<Block>::new_test_with_tx_storage(BlocksPruning::Some(100), 0);
		assert!(!backend.is_archive);

		let _genesis_hash =
			insert_header(&backend, 0, Default::default(), None, Default::default());

		// Insert a disconnected block at height 3 with a fake parent to create a
		// MissingHeaderAndBody gap (blocks 1..2 are missing).
		insert_disconnected_header(&backend, 3, H256::from([200; 32]), Default::default(), true);

		let info = backend.blockchain().info();
		assert!(info.block_gap.is_some(), "Gap should have been created");
		let gap = info.block_gap.unwrap();
		assert!(matches!(gap.gap_type, BlockGapType::MissingHeaderAndBody));
		assert_eq!(gap.start, 1);
		assert_eq!(gap.end, 2);

		// Re-open the same database as a non-archive node.
		let db = backend.storage.db.clone();
		let backend = reopen_backend(db, BlocksPruning::Some(100));
		assert!(!backend.is_archive);

		// The MissingHeaderAndBody gap should NOT be removed — only MissingBody gaps are removed.
		let info = backend.blockchain().info();
		assert!(
			info.block_gap.is_some(),
			"MissingHeaderAndBody gap should be preserved for non-archive nodes",
		);
		let gap = info.block_gap.unwrap();
		assert!(matches!(gap.gap_type, BlockGapType::MissingHeaderAndBody));
		assert_eq!(gap.start, 1);
		assert_eq!(gap.end, 2);
	}

	mod indexed_transaction_tests {
		use super::*;
		use crate::utils::NUM_COLUMNS;
		use rstest::rstest;
		use sp_database::Transaction as DbTransaction;
		use std::{path::PathBuf, sync::Arc};
		use tempfile::TempDir;

		#[derive(Debug, Clone, Copy)]
		enum BackendKind {
			KvdbMemdb,
			ParityDb,
			RocksDb,
		}

		enum DbFactory {
			Persistent(Arc<dyn Database<DbHash>>),
			OnDisk { path: PathBuf, kind: BackendKind, _tmp: TempDir },
		}

		impl DbFactory {
			fn new(kind: BackendKind) -> Self {
				match kind {
					BackendKind::KvdbMemdb => Self::Persistent(sp_database::as_database(
						kvdb_memorydb::create(NUM_COLUMNS),
					)),
					BackendKind::ParityDb | BackendKind::RocksDb => {
						let tmp = TempDir::new().unwrap();
						let path = tmp.path().to_path_buf();
						Self::OnDisk { path, kind, _tmp: tmp }
					},
				}
			}

			fn open(&self) -> Arc<dyn Database<DbHash>> {
				match self {
					Self::Persistent(arc) => arc.clone(),
					Self::OnDisk { path, kind: BackendKind::ParityDb, .. } => {
						crate::parity_db::open::<DbHash>(path, DatabaseType::Full, true, false)
							.expect("parity-db open succeeds in test")
					},
					Self::OnDisk { path, kind: BackendKind::RocksDb, .. } => {
						let mut cfg = kvdb_rocksdb::DatabaseConfig::with_columns(NUM_COLUMNS);
						cfg.create_if_missing = true;
						let db = kvdb_rocksdb::Database::open(&cfg, path)
							.expect("kvdb-rocksdb open succeeds in test");
						sp_database::as_database(db)
					},
					Self::OnDisk { kind: BackendKind::KvdbMemdb, .. } => unreachable!(),
				}
			}
		}

		const TEST_COL: u32 = columns::TRANSACTION;

		fn hash(seed: u8) -> DbHash {
			DbHash::repeat_byte(seed)
		}

		fn commit_store(factory: &DbFactory, h: DbHash, bytes: Vec<u8>) {
			let db = factory.open();
			let mut tx = DbTransaction::new();
			tx.store(TEST_COL, h, bytes);
			db.commit(tx).unwrap();
		}

		fn commit_reference(factory: &DbFactory, h: DbHash) {
			let db = factory.open();
			let mut tx = DbTransaction::new();
			tx.reference(TEST_COL, h);
			db.commit(tx).unwrap();
		}

		fn commit_release(factory: &DbFactory, h: DbHash) {
			let db = factory.open();
			let mut tx = DbTransaction::new();
			tx.release(TEST_COL, h);
			db.commit(tx).unwrap();
		}

		fn get_value(factory: &DbFactory, h: DbHash) -> Option<Vec<u8>> {
			factory.open().get(TEST_COL, h.as_ref())
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn store_then_get(#[case] kind: BackendKind) {
			let factory = DbFactory::new(kind);
			let h = hash(0xA1);
			let bytes = b"a1-bytes".to_vec();
			commit_store(&factory, h, bytes.clone());
			assert_eq!(get_value(&factory, h).as_deref(), Some(bytes.as_slice()));
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn store_release_separate_commits(#[case] kind: BackendKind) {
			let factory = DbFactory::new(kind);
			let h = hash(0xA2);
			let bytes = b"a2-bytes".to_vec();
			commit_store(&factory, h, bytes);
			assert!(get_value(&factory, h).is_some(), "present after store");
			commit_release(&factory, h);
			assert!(get_value(&factory, h).is_none(), "gone after release");
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn store_reference_release_release_separate_commits(#[case] kind: BackendKind) {
			let factory = DbFactory::new(kind);
			let h = hash(0xA3);
			let bytes = b"a3-bytes".to_vec();
			commit_store(&factory, h, bytes);
			commit_reference(&factory, h);
			assert!(get_value(&factory, h).is_some(), "rc=2 after reference");
			commit_release(&factory, h);
			assert!(get_value(&factory, h).is_some(), "rc=1 still present");
			commit_release(&factory, h);
			assert!(get_value(&factory, h).is_none(), "rc=0 removed");
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn store_then_reference_same_commit_keeps_value(#[case] kind: BackendKind) {
			let factory = DbFactory::new(kind);
			let h = hash(0xA4);
			let bytes = b"a4-bytes".to_vec();
			{
				let db = factory.open();
				let mut tx = DbTransaction::new();
				tx.store(TEST_COL, h, bytes.clone());
				tx.reference(TEST_COL, h);
				db.commit(tx).unwrap();
			}
			assert_eq!(
				get_value(&factory, h).as_deref(),
				Some(bytes.as_slice()),
				"Store + Reference on fresh hash in a single commit must keep the value \
				 (observed via fresh DB handle so overlay caching is bypassed)",
			);
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn store_then_two_references_same_commit_keeps_value(#[case] kind: BackendKind) {
			let factory = DbFactory::new(kind);
			let h = hash(0xA5);
			let bytes = b"a5-bytes".to_vec();
			{
				let db = factory.open();
				let mut tx = DbTransaction::new();
				tx.store(TEST_COL, h, bytes.clone());
				tx.reference(TEST_COL, h);
				tx.reference(TEST_COL, h);
				db.commit(tx).unwrap();
			}
			assert_eq!(
				get_value(&factory, h).as_deref(),
				Some(bytes.as_slice()),
				"Store + 2x Reference on fresh hash must keep the value (post-sync observation)",
			);
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn reference_on_missing_hash_is_noop(#[case] kind: BackendKind) {
			let factory = DbFactory::new(kind);
			let h = hash(0xA6);
			commit_reference(&factory, h);
			assert!(get_value(&factory, h).is_none(), "reference on missing key is a no-op");
			let bytes = b"a6-bytes".to_vec();
			commit_store(&factory, h, bytes.clone());
			assert_eq!(get_value(&factory, h).as_deref(), Some(bytes.as_slice()));
			commit_release(&factory, h);
			assert!(get_value(&factory, h).is_none(), "single release balances the store");
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn release_on_missing_hash_is_noop(#[case] kind: BackendKind) {
			let factory = DbFactory::new(kind);
			let h = hash(0xA7);
			commit_release(&factory, h);
			assert!(get_value(&factory, h).is_none(), "release on missing key is a no-op");
			let bytes = b"a7-bytes".to_vec();
			commit_store(&factory, h, bytes.clone());
			assert_eq!(get_value(&factory, h).as_deref(), Some(bytes.as_slice()));
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn release_then_store_missing_same_commit_stores_value(#[case] kind: BackendKind) {
			let factory = DbFactory::new(kind);
			let h = hash(0xAB);
			let bytes = b"ab-bytes".to_vec();
			{
				let db = factory.open();
				let mut tx = DbTransaction::new();
				tx.release(TEST_COL, h);
				tx.store(TEST_COL, h, bytes.clone());
				db.commit(tx).unwrap();
			}
			assert_eq!(get_value(&factory, h).as_deref(), Some(bytes.as_slice()));
			commit_release(&factory, h);
			assert!(get_value(&factory, h).is_none(), "single release balances the store");
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn reference_then_store_missing_same_commit_single_release_removes_value(
			#[case] kind: BackendKind,
		) {
			let factory = DbFactory::new(kind);
			let h = hash(0xAC);
			let bytes = b"ac-bytes".to_vec();
			{
				let db = factory.open();
				let mut tx = DbTransaction::new();
				tx.reference(TEST_COL, h);
				tx.store(TEST_COL, h, bytes.clone());
				db.commit(tx).unwrap();
			}
			assert_eq!(get_value(&factory, h).as_deref(), Some(bytes.as_slice()));
			commit_release(&factory, h);
			assert!(get_value(&factory, h).is_none(), "missing-key reference is a no-op");
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn release_then_reference_at_one_same_commit_removes_value(#[case] kind: BackendKind) {
			let factory = DbFactory::new(kind);
			let h = hash(0xAD);
			let bytes = b"ad-bytes".to_vec();
			commit_store(&factory, h, bytes);
			{
				let db = factory.open();
				let mut tx = DbTransaction::new();
				tx.release(TEST_COL, h);
				tx.reference(TEST_COL, h);
				db.commit(tx).unwrap();
			}
			assert!(get_value(&factory, h).is_none(), "reference after removal is a no-op");
		}

		// Same-commit multi-op refcount tests: each Store/Reference/Release must compose.

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn store_then_two_releases_same_commit_removes_value(#[case] kind: BackendKind) {
			let factory = DbFactory::new(kind);
			let h = hash(0xA8);
			let bytes = b"a8-bytes".to_vec();
			commit_store(&factory, h, bytes);
			commit_reference(&factory, h);
			assert!(get_value(&factory, h).is_some(), "rc=2 after store + reference");
			{
				let db = factory.open();
				let mut tx = DbTransaction::new();
				tx.release(TEST_COL, h);
				tx.release(TEST_COL, h);
				db.commit(tx).unwrap();
			}
			assert!(get_value(&factory, h).is_none(), "rc 2 -> 0 after two releases");
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn store_then_two_references_same_commit_increments_twice(#[case] kind: BackendKind) {
			let factory = DbFactory::new(kind);
			let h = hash(0xA9);
			let bytes = b"a9-bytes".to_vec();
			commit_store(&factory, h, bytes.clone());
			{
				let db = factory.open();
				let mut tx = DbTransaction::new();
				tx.reference(TEST_COL, h);
				tx.reference(TEST_COL, h);
				db.commit(tx).unwrap();
			}
			commit_release(&factory, h);
			commit_release(&factory, h);
			assert_eq!(
				get_value(&factory, h).as_deref(),
				Some(bytes.as_slice()),
				"rc 3 -> 1 after two releases, value still present",
			);
			commit_release(&factory, h);
			assert!(get_value(&factory, h).is_none(), "rc=0 after final release");
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn store_then_release_same_commit_net_zero_removes_value(#[case] kind: BackendKind) {
			let factory = DbFactory::new(kind);
			let h = hash(0xAA);
			let bytes = b"aa-bytes".to_vec();
			{
				let db = factory.open();
				let mut tx = DbTransaction::new();
				tx.store(TEST_COL, h, bytes);
				tx.release(TEST_COL, h);
				db.commit(tx).unwrap();
			}
			assert!(get_value(&factory, h).is_none(), "store + release nets to rc=0");
		}

		struct BackendFactory {
			backend: Option<Backend<Block>>,
			kind: BackendKind,
			blocks_pruning: BlocksPruning,
			tmp_path: Option<PathBuf>,
			_tmp: Option<TempDir>,
		}

		impl BackendFactory {
			fn new(kind: BackendKind, blocks_pruning: BlocksPruning) -> Self {
				match kind {
					BackendKind::KvdbMemdb => Self {
						backend: Some(Backend::new_test_with_tx_storage(blocks_pruning, 10)),
						kind,
						blocks_pruning,
						tmp_path: None,
						_tmp: None,
					},
					BackendKind::ParityDb => {
						let tmp = TempDir::new().unwrap();
						let tmp_path = tmp.path().to_path_buf();
						let backend = Backend::new_test_with_tx_storage_source(
							blocks_pruning,
							10,
							DatabaseSource::ParityDb { path: tmp_path.clone() },
							Default::default(),
						);
						Self {
							backend: Some(backend),
							kind,
							blocks_pruning,
							tmp_path: Some(tmp_path),
							_tmp: Some(tmp),
						}
					},
					BackendKind::RocksDb => {
						let tmp = TempDir::new().unwrap();
						let tmp_path = tmp.path().to_path_buf();
						let mut cfg = kvdb_rocksdb::DatabaseConfig::with_columns(NUM_COLUMNS);
						cfg.create_if_missing = true;
						let db = kvdb_rocksdb::Database::open(&cfg, &tmp_path)
							.expect("kvdb-rocksdb open succeeds in test");
						let db = sp_database::as_database(db);
						let backend = Backend::new_test_with_tx_storage_source(
							blocks_pruning,
							10,
							DatabaseSource::Custom { db, require_create_flag: true },
							Default::default(),
						);
						Self {
							backend: Some(backend),
							kind,
							blocks_pruning,
							tmp_path: Some(tmp_path),
							_tmp: Some(tmp),
						}
					},
				}
			}

			fn backend(&self) -> &Backend<Block> {
				self.backend.as_ref().expect("backend present")
			}

			// parity-db drains commit_overlay on `Drop`. Drop+reopen before
			// `is_none()` assertions to avoid flakes. No-op for memdb/rocksdb.
			fn refresh_for_assertion(&mut self) {
				if !matches!(self.kind, BackendKind::ParityDb) {
					return;
				}
				let path = self.tmp_path.clone().expect("paritydb has tmp_path");
				self.backend = None;
				self.backend = Some(Backend::new_test_with_tx_storage_source(
					self.blocks_pruning,
					10,
					DatabaseSource::ParityDb { path },
					Default::default(),
				));
			}
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn prefetched_multi_renew_same_hash_balanced_lifecycle(#[case] kind: BackendKind) {
			let mut factory = BackendFactory::new(kind, BlocksPruning::Some(2));
			let payload = b"prefetched-blob".to_vec();
			let payload_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&payload);
			let payload_hash_arr: [u8; 32] = payload_hash.into();

			let mut blocks = Vec::new();
			let block0 = insert_block_with_prefetched(
				factory.backend(),
				0,
				Default::default(),
				Default::default(),
				vec![UncheckedXt::new_transaction(0.into(), ())],
				Some(vec![
					IndexOperation::Renew { extrinsic: 0, hash: payload_hash_arr.into() },
					IndexOperation::Renew { extrinsic: 0, hash: payload_hash_arr.into() },
				]),
				HashMap::from([(payload_hash, payload.clone())]),
			)
			.unwrap();
			blocks.push(block0);

			assert!(factory
				.backend()
				.blockchain()
				.indexed_transaction(payload_hash)
				.unwrap()
				.is_some());

			let mut prev = block0;
			for i in 1..6u64 {
				prev = insert_block(
					factory.backend(),
					i,
					prev,
					None,
					Default::default(),
					vec![UncheckedXt::new_transaction(i.into(), ())],
					None,
				)
				.unwrap();
				blocks.push(prev);
			}

			for i in 1..6 {
				let mut op = factory.backend().begin_operation().unwrap();
				factory.backend().begin_state_operation(&mut op, blocks[4]).unwrap();
				op.mark_finalized(blocks[i], None).unwrap();
				factory.backend().commit_operation(op).unwrap();
			}

			factory.refresh_for_assertion();
			assert!(factory
				.backend()
				.blockchain()
				.indexed_transaction(payload_hash)
				.unwrap()
				.is_none());
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn prefetched_single_renew_full_lifecycle(#[case] kind: BackendKind) {
			let mut factory = BackendFactory::new(kind, BlocksPruning::Some(2));
			let payload = b"prefetched-blob".to_vec();
			let payload_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&payload);
			let payload_hash_arr: [u8; 32] = payload_hash.into();

			let mut blocks = Vec::new();
			let block0 = insert_block_with_prefetched(
				factory.backend(),
				0,
				Default::default(),
				Default::default(),
				vec![UncheckedXt::new_transaction(0.into(), ())],
				Some(vec![IndexOperation::Renew { extrinsic: 0, hash: payload_hash_arr.into() }]),
				HashMap::from([(payload_hash, payload.clone())]),
			)
			.unwrap();
			blocks.push(block0);

			assert_eq!(
				factory
					.backend()
					.blockchain()
					.indexed_transaction(payload_hash)
					.unwrap()
					.as_deref(),
				Some(payload.as_slice()),
			);

			let mut prev = block0;
			for i in 1..6u64 {
				prev = insert_block(
					factory.backend(),
					i,
					prev,
					None,
					Default::default(),
					vec![UncheckedXt::new_transaction(i.into(), ())],
					None,
				)
				.unwrap();
				blocks.push(prev);
			}

			for i in 1..6 {
				let mut op = factory.backend().begin_operation().unwrap();
				factory.backend().begin_state_operation(&mut op, blocks[4]).unwrap();
				op.mark_finalized(blocks[i], None).unwrap();
				factory.backend().commit_operation(op).unwrap();
			}

			factory.refresh_for_assertion();
			assert!(factory
				.backend()
				.blockchain()
				.indexed_transaction(payload_hash)
				.unwrap()
				.is_none());
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn redundant_prefetch_on_local_data_balanced_lifecycle(#[case] kind: BackendKind) {
			let mut factory = BackendFactory::new(kind, BlocksPruning::Some(2));
			let payload_xt = UncheckedXt::new_transaction(5.into(), ()).encode();
			let payload = payload_xt[1..].to_vec();
			let payload_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&payload);
			let payload_hash_arr: [u8; 32] = payload_hash.into();

			let mut blocks = Vec::new();
			let block0 = insert_block(
				factory.backend(),
				0,
				Default::default(),
				None,
				Default::default(),
				vec![UncheckedXt::new_transaction(5.into(), ())],
				Some(vec![IndexOperation::Insert {
					extrinsic: 0,
					hash: payload_hash_arr.into(),
					size: payload.len() as u32,
				}]),
			)
			.unwrap();
			blocks.push(block0);

			let block1 = insert_block_with_prefetched(
				factory.backend(),
				1,
				block0,
				Default::default(),
				vec![UncheckedXt::new_transaction(99.into(), ())],
				Some(vec![IndexOperation::Renew { extrinsic: 0, hash: payload_hash_arr.into() }]),
				HashMap::from([(payload_hash, payload.clone())]),
			)
			.unwrap();
			blocks.push(block1);

			assert!(factory
				.backend()
				.blockchain()
				.indexed_transaction(payload_hash)
				.unwrap()
				.is_some());

			let mut prev = block1;
			for i in 2..7u64 {
				prev = insert_block(
					factory.backend(),
					i,
					prev,
					None,
					Default::default(),
					vec![UncheckedXt::new_transaction(i.into(), ())],
					None,
				)
				.unwrap();
				blocks.push(prev);
			}

			for i in 1..7 {
				let mut op = factory.backend().begin_operation().unwrap();
				factory.backend().begin_state_operation(&mut op, blocks[5]).unwrap();
				op.mark_finalized(blocks[i], None).unwrap();
				factory.backend().commit_operation(op).unwrap();
			}

			factory.refresh_for_assertion();
			assert!(
				factory
					.backend()
					.blockchain()
					.indexed_transaction(payload_hash)
					.unwrap()
					.is_none(),
				"redundant prefetch must not leak refcount through prune",
			);
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn same_block_insert_and_renew_different_indices_with_prefetch(#[case] kind: BackendKind) {
			let mut factory = BackendFactory::new(kind, BlocksPruning::Some(2));
			let x_xt = UncheckedXt::new_transaction(0.into(), ()).encode();
			let x = x_xt[1..].to_vec();
			let x_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&x);
			let x_hash_arr: [u8; 32] = x_hash.into();

			let mut blocks = Vec::new();

			let block0 = insert_block(
				factory.backend(),
				0,
				Default::default(),
				None,
				Default::default(),
				vec![UncheckedXt::new_transaction(0.into(), ())],
				Some(vec![IndexOperation::Insert {
					extrinsic: 0,
					hash: x_hash_arr.into(),
					size: x.len() as u32,
				}]),
			)
			.unwrap();
			blocks.push(block0);

			let block1 = insert_block_with_prefetched(
				factory.backend(),
				1,
				block0,
				Default::default(),
				vec![
					UncheckedXt::new_transaction(0.into(), ()),
					UncheckedXt::new_transaction(99.into(), ()),
				],
				Some(vec![
					IndexOperation::Insert {
						extrinsic: 0,
						hash: x_hash_arr.into(),
						size: x.len() as u32,
					},
					IndexOperation::Renew { extrinsic: 1, hash: x_hash_arr.into() },
				]),
				HashMap::from([(x_hash, x.clone())]),
			)
			.unwrap();
			blocks.push(block1);

			assert!(factory.backend().blockchain().indexed_transaction(x_hash).unwrap().is_some());

			let mut prev = block1;
			for i in 2..8u64 {
				prev = insert_block(
					factory.backend(),
					i,
					prev,
					None,
					Default::default(),
					vec![UncheckedXt::new_transaction(i.into(), ())],
					None,
				)
				.unwrap();
				blocks.push(prev);
			}

			for i in 1..8 {
				let mut op = factory.backend().begin_operation().unwrap();
				factory.backend().begin_state_operation(&mut op, blocks[6]).unwrap();
				op.mark_finalized(blocks[i], None).unwrap();
				factory.backend().commit_operation(op).unwrap();
			}

			factory.refresh_for_assertion();
			assert!(
				factory.backend().blockchain().indexed_transaction(x_hash).unwrap().is_none(),
				"same-block Insert+Renew with prefetch must balance refcount through prune",
			);
		}

		#[rstest]
		#[case::kvdb_memdb(BackendKind::KvdbMemdb)]
		#[case::paritydb(BackendKind::ParityDb)]
		#[case::rocksdb(BackendKind::RocksDb)]
		fn sequential_renew_blocks_all_prefetched_eventually_pruned(#[case] kind: BackendKind) {
			let mut factory = BackendFactory::new(kind, BlocksPruning::Some(2));
			let payload = b"prefetched-blob".to_vec();
			let payload_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&payload);
			let payload_hash_arr: [u8; 32] = payload_hash.into();

			let mut blocks = Vec::new();
			let mut prev = Default::default();
			for i in 0..4u64 {
				let block = insert_block_with_prefetched(
					factory.backend(),
					i,
					prev,
					Default::default(),
					vec![UncheckedXt::new_transaction(i.into(), ())],
					Some(vec![IndexOperation::Renew {
						extrinsic: 0,
						hash: payload_hash_arr.into(),
					}]),
					HashMap::from([(payload_hash, payload.clone())]),
				)
				.unwrap();
				blocks.push(block);
				prev = block;
			}

			assert!(factory
				.backend()
				.blockchain()
				.indexed_transaction(payload_hash)
				.unwrap()
				.is_some());

			for i in 4..10u64 {
				prev = insert_block(
					factory.backend(),
					i,
					prev,
					None,
					Default::default(),
					vec![UncheckedXt::new_transaction(i.into(), ())],
					None,
				)
				.unwrap();
				blocks.push(prev);
			}

			for i in 1..10 {
				let mut op = factory.backend().begin_operation().unwrap();
				factory.backend().begin_state_operation(&mut op, blocks[8]).unwrap();
				op.mark_finalized(blocks[i], None).unwrap();
				factory.backend().commit_operation(op).unwrap();
			}

			factory.refresh_for_assertion();
			assert!(
				factory
					.backend()
					.blockchain()
					.indexed_transaction(payload_hash)
					.unwrap()
					.is_none(),
				"sequential prefetched renews must all release through prune",
			);
		}

		// Synthetic-ops precedence tests. kvdb-memdb only — backend-agnostic logic.

		#[test]
		fn runtime_index_ops_win_over_synthetic() {
			let factory = BackendFactory::new(BackendKind::KvdbMemdb, BlocksPruning::KeepAll);
			let payload_xt = UncheckedXt::new_transaction(11.into(), ()).encode();
			let payload = payload_xt[1..].to_vec();
			let payload_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&payload);
			let payload_hash_arr: [u8; 32] = payload_hash.into();

			let bogus_hash_arr = [0xAAu8; 32];

			insert_block_with_synthetic_ops(
				factory.backend(),
				0,
				Default::default(),
				Default::default(),
				vec![UncheckedXt::new_transaction(11.into(), ())],
				vec![IndexOperation::Insert {
					extrinsic: 0,
					hash: payload_hash_arr.into(),
					size: payload.len() as u32,
				}],
				vec![IndexOperation::Insert {
					extrinsic: 0,
					hash: bogus_hash_arr.into(),
					size: payload.len() as u32,
				}],
				HashMap::new(),
			)
			.unwrap();

			assert_eq!(
				factory
					.backend()
					.blockchain()
					.indexed_transaction(payload_hash)
					.unwrap()
					.as_deref(),
				Some(payload.as_slice()),
				"runtime ops win",
			);
			assert!(
				factory
					.backend()
					.blockchain()
					.indexed_transaction(bogus_hash_arr.into())
					.unwrap()
					.is_none(),
				"synthetic dropped",
			);
		}

		#[test]
		fn empty_both_falls_back_to_plain_body() {
			let factory = BackendFactory::new(BackendKind::KvdbMemdb, BlocksPruning::KeepAll);
			let body = vec![UncheckedXt::new_transaction(42.into(), ())];

			let block_hash = insert_block_with_synthetic_ops(
				factory.backend(),
				0,
				Default::default(),
				Default::default(),
				body.clone(),
				Vec::new(),
				Vec::new(),
				HashMap::new(),
			)
			.unwrap();

			let stored_body = factory.backend().blockchain().body(block_hash).unwrap();
			assert_eq!(stored_body, Some(body));
		}

		#[test]
		fn synthetic_renew_uses_prefetched_payload() {
			let factory = BackendFactory::new(BackendKind::KvdbMemdb, BlocksPruning::KeepAll);
			let payload = b"prefetched-blob".to_vec();
			let payload_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&payload);
			let payload_hash_arr: [u8; 32] = payload_hash.into();

			insert_block_with_synthetic_ops(
				factory.backend(),
				0,
				Default::default(),
				Default::default(),
				vec![UncheckedXt::new_transaction(1.into(), ())],
				Vec::new(),
				vec![IndexOperation::Renew { extrinsic: 0, hash: payload_hash_arr.into() }],
				HashMap::from([(payload_hash, payload.clone())]),
			)
			.unwrap();

			assert_eq!(
				factory
					.backend()
					.blockchain()
					.indexed_transaction(payload_hash)
					.unwrap()
					.as_deref(),
				Some(payload.as_slice()),
			);
		}

		#[test]
		fn synthetic_renew_without_prefetched_references_existing() {
			let factory = BackendFactory::new(BackendKind::KvdbMemdb, BlocksPruning::KeepAll);
			let payload_xt = UncheckedXt::new_transaction(5.into(), ()).encode();
			let payload = payload_xt[1..].to_vec();
			let payload_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&payload);
			let payload_hash_arr: [u8; 32] = payload_hash.into();

			let block0 = insert_block(
				factory.backend(),
				0,
				Default::default(),
				None,
				Default::default(),
				vec![UncheckedXt::new_transaction(5.into(), ())],
				Some(vec![IndexOperation::Insert {
					extrinsic: 0,
					hash: payload_hash_arr.into(),
					size: payload.len() as u32,
				}]),
			)
			.unwrap();

			insert_block_with_synthetic_ops(
				factory.backend(),
				1,
				block0,
				Default::default(),
				vec![UncheckedXt::new_transaction(6.into(), ())],
				Vec::new(),
				vec![IndexOperation::Renew { extrinsic: 0, hash: payload_hash_arr.into() }],
				HashMap::new(),
			)
			.unwrap();

			assert_eq!(
				factory
					.backend()
					.blockchain()
					.indexed_transaction(payload_hash)
					.unwrap()
					.as_deref(),
				Some(payload.as_slice()),
			);
		}

		#[test]
		fn synthetic_insert_extracts_tail_from_body() {
			let factory = BackendFactory::new(BackendKind::KvdbMemdb, BlocksPruning::KeepAll);
			let payload_xt = UncheckedXt::new_transaction(13.into(), ()).encode();
			let tail_size = 4u32;
			let tail_start = payload_xt.len() - tail_size as usize;
			let expected_tail = payload_xt[tail_start..].to_vec();
			let tail_hash = <HashingFor<Block> as sp_core::Hasher>::hash(&expected_tail);
			let tail_hash_arr: [u8; 32] = tail_hash.into();

			insert_block_with_synthetic_ops(
				factory.backend(),
				0,
				Default::default(),
				Default::default(),
				vec![UncheckedXt::new_transaction(13.into(), ())],
				Vec::new(),
				vec![IndexOperation::Insert {
					extrinsic: 0,
					hash: tail_hash_arr.into(),
					size: tail_size,
				}],
				HashMap::new(),
			)
			.unwrap();

			assert_eq!(
				factory
					.backend()
					.blockchain()
					.indexed_transaction(tail_hash)
					.unwrap()
					.as_deref(),
				Some(expected_tail.as_slice()),
			);
		}

		#[test]
		fn synthetic_insert_oversized_size_falls_back_to_full_extrinsic() {
			let factory = BackendFactory::new(BackendKind::KvdbMemdb, BlocksPruning::KeepAll);
			let payload_xt = UncheckedXt::new_transaction(17.into(), ()).encode();
			let bogus_hash_arr = [0xBBu8; 32];
			let oversized = (payload_xt.len() + 1) as u32;

			let block_hash = insert_block_with_synthetic_ops(
				factory.backend(),
				0,
				Default::default(),
				Default::default(),
				vec![UncheckedXt::new_transaction(17.into(), ())],
				Vec::new(),
				vec![IndexOperation::Insert {
					extrinsic: 0,
					hash: bogus_hash_arr.into(),
					size: oversized,
				}],
				HashMap::new(),
			)
			.unwrap();

			assert!(factory
				.backend()
				.blockchain()
				.indexed_transaction(bogus_hash_arr.into())
				.unwrap()
				.is_none());
			let stored_body = factory.backend().blockchain().body(block_hash).unwrap();
			assert_eq!(stored_body, Some(vec![UncheckedXt::new_transaction(17.into(), ())]));
		}

		#[test]
		fn multiple_synthetic_ops_per_block_apply_in_order() {
			let factory = BackendFactory::new(BackendKind::KvdbMemdb, BlocksPruning::KeepAll);
			let xt_a = UncheckedXt::new_transaction(21.into(), ()).encode();
			let xt_b = UncheckedXt::new_transaction(22.into(), ()).encode();
			let payload_a = xt_a[1..].to_vec();
			let payload_b = xt_b[1..].to_vec();
			let hash_a = <HashingFor<Block> as sp_core::Hasher>::hash(&payload_a);
			let hash_b = <HashingFor<Block> as sp_core::Hasher>::hash(&payload_b);
			let hash_a_arr: [u8; 32] = hash_a.into();
			let hash_b_arr: [u8; 32] = hash_b.into();

			insert_block_with_synthetic_ops(
				factory.backend(),
				0,
				Default::default(),
				Default::default(),
				vec![
					UncheckedXt::new_transaction(21.into(), ()),
					UncheckedXt::new_transaction(22.into(), ()),
				],
				Vec::new(),
				vec![
					IndexOperation::Insert {
						extrinsic: 0,
						hash: hash_a_arr.into(),
						size: payload_a.len() as u32,
					},
					IndexOperation::Insert {
						extrinsic: 1,
						hash: hash_b_arr.into(),
						size: payload_b.len() as u32,
					},
				],
				HashMap::new(),
			)
			.unwrap();

			assert_eq!(
				factory.backend().blockchain().indexed_transaction(hash_a).unwrap().as_deref(),
				Some(payload_a.as_slice()),
				"first op",
			);
			assert_eq!(
				factory.backend().blockchain().indexed_transaction(hash_b).unwrap().as_deref(),
				Some(payload_b.as_slice()),
				"second op",
			);
		}
	}
}
