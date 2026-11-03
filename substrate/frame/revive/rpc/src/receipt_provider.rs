// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use crate::{
	Address, BlockInfoProvider, BlockNumberOrTag, Bytes, ChainMetadata, ClientError, Filter,
	FilterBlockOption, Log, ReceiptExtractor, ReceiptInfo, SubxtBlockInfoProvider, SyncLabel,
	SyncStateKey,
	block_sync::SyncCheckpoint,
	client::{SubstrateBlock, SubstrateBlockNumber},
};
use futures::future::OptionFuture;
use pallet_revive::evm::TransactionSigned;
use sp_core::{H256, U256};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool, query};
use std::{
	collections::{BTreeMap, HashMap},
	sync::Arc,
};
use tokio::sync::Mutex;

const LOG_TARGET: &str = "eth-rpc::receipt_provider";
const MAX_LOG_RESULTS: usize = 10_000;

/// Parse a SQLite row from the `logs` table into a [`Log`].
fn parse_log_row(row: sqlx::sqlite::SqliteRow) -> Result<Log, sqlx::Error> {
	let block_hash: Vec<u8> = row.try_get("block_hash")?;
	let transaction_index: i64 = row.try_get("transaction_index")?;
	let log_index: i64 = row.try_get("log_index")?;
	let address: Vec<u8> = row.try_get("address")?;
	let block_number: i64 = row.try_get("block_number")?;
	let transaction_hash: Vec<u8> = row.try_get("transaction_hash")?;
	let topic_0: Option<Vec<u8>> = row.try_get("topic_0")?;
	let topic_1: Option<Vec<u8>> = row.try_get("topic_1")?;
	let topic_2: Option<Vec<u8>> = row.try_get("topic_2")?;
	let topic_3: Option<Vec<u8>> = row.try_get("topic_3")?;
	let data: Option<Vec<u8>> = row.try_get("data")?;

	let topics = [topic_0, topic_1, topic_2, topic_3]
		.iter()
		.filter_map(|t| t.as_ref().map(|t| H256::from_slice(t)))
		.collect::<Vec<_>>();

	Ok(Log {
		address: Address::from_slice(&address),
		block_hash: H256::from_slice(&block_hash),
		block_number: U256::from(block_number as u64),
		data: data.map(Bytes::from),
		log_index: U256::from(log_index as u64),
		topics,
		transaction_hash: H256::from_slice(&transaction_hash),
		transaction_index: U256::from(transaction_index as u64),
		removed: false,
	})
}

/// SQLite connection pool with precomputed bulk-insert chunk sizes.
#[derive(Clone)]
pub struct DbContext {
	pool: SqlitePool,
	/// Max bound parameters per query.
	max_variable_number: usize,
	/// Chunk size for bulk INSERT into `transaction_hashes`.
	tx_insert_chunk_size: usize,
	/// Chunk size for bulk INSERT into `logs`.
	log_insert_chunk_size: usize,
}

impl DbContext {
	/// Conservative default for `SQLITE_LIMIT_VARIABLE_NUMBER`; SQLite >=3.32 uses 32766.
	pub const DEFAULT_MAX_VARIABLE_NUMBER: usize = 999;
	/// Columns in the `transaction_hashes` table.
	const TX_HASH_COLUMNS: usize = 3;
	/// Columns in the `logs` table.
	const LOG_COLUMNS: usize = 11;

	pub fn new(pool: SqlitePool, max_variable_number: usize) -> Self {
		assert!(
			max_variable_number >= Self::LOG_COLUMNS,
			"SQLite max_variable_number ({max_variable_number}) must be >= {}",
			Self::LOG_COLUMNS
		);
		Self {
			pool,
			max_variable_number,
			tx_insert_chunk_size: max_variable_number / Self::TX_HASH_COLUMNS,
			log_insert_chunk_size: max_variable_number / Self::LOG_COLUMNS,
		}
	}
}

/// ReceiptProvider stores transaction receipts and logs in a SQLite database.
#[derive(Clone)]
pub struct ReceiptProvider<B: BlockInfoProvider = SubxtBlockInfoProvider> {
	/// The database pool.
	db_ctx: DbContext,
	/// The block provider used to fetch blocks, and reconstruct receipts.
	block_provider: B,
	/// A means to extract receipts from extrinsics.
	receipt_extractor: ReceiptExtractor,
	/// When `Some`, old blocks will be pruned.
	keep_latest_n_blocks: Option<usize>,
	/// A Map of the latest block numbers to block hashes.
	block_number_to_hashes: Arc<Mutex<BTreeMap<SubstrateBlockNumber, BlockHashMap>>>,
}

/// Substrate block to Ethereum block mapping
#[derive(Clone, Debug, PartialEq, Eq)]
struct BlockHashMap {
	substrate_hash: H256,
	ethereum_hash: H256,
}

impl BlockHashMap {
	fn new(substrate_hash: H256, ethereum_hash: H256) -> Self {
		Self { substrate_hash, ethereum_hash }
	}
}

/// Provides information about a block,
/// This is an abstraction on top of [`SubstrateBlock`] that can't be mocked in tests.
/// Can be removed once <https://github.com/paritytech/subxt/issues/1883> is fixed.
pub trait BlockInfo {
	/// Returns the block hash.
	fn hash(&self) -> H256;
	/// Returns the block number.
	fn number(&self) -> SubstrateBlockNumber;
}

impl BlockInfo for SubstrateBlock {
	fn hash(&self) -> H256 {
		self.block_hash()
	}
	fn number(&self) -> SubstrateBlockNumber {
		self.block_number()
	}
}

/// Maximum number of entries kept in the block to hash map.
pub const MAX_CACHED_BLOCKS: usize = 256;

/// Upsert a sync label row, updating only when the existing `block_number`
/// compares with `$op` against the new value. `$op` must be `"<"` or `">"`.
macro_rules! upsert_sync_label {
	($pool:expr, $op:literal, $label:expr, $checkpoint:expr) => {{
		let label_str = $label.to_string();
		let block_number = $checkpoint.block_number as i64;
		let block_hash = $checkpoint.block_hash.map(|h| h.as_bytes().to_vec());
		query!(
			"INSERT INTO sync_state (label, block_number, block_hash)
			VALUES ($1, $2, $3)
			ON CONFLICT(label) DO UPDATE
				SET block_number = excluded.block_number, block_hash = excluded.block_hash
			WHERE sync_state.block_number " +
				$op + " excluded.block_number
			",
			label_str,
			block_number,
			block_hash
		)
		.execute($pool)
		.await?;
	}};
}

async fn insert_block_mapping<'e, E: sqlx::Executor<'e, Database = Sqlite>>(
	executor: E,
	block_map: &BlockHashMap,
) -> Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error> {
	let ethereum_hash_ref = block_map.ethereum_hash.as_ref();
	let substrate_hash_ref = block_map.substrate_hash.as_ref();
	query!(
		r#"
			INSERT OR REPLACE INTO eth_to_substrate_blocks (ethereum_block_hash, substrate_block_hash)
			VALUES ($1, $2)
			"#,
		ethereum_hash_ref,
		substrate_hash_ref,
	)
	.execute(executor)
	.await
}

impl<B: BlockInfoProvider> ReceiptProvider<B> {
	/// Create a new `ReceiptProvider` with the given database URL and block provider.
	pub async fn new(
		db_ctx: DbContext,
		block_provider: B,
		receipt_extractor: ReceiptExtractor,
		keep_latest_n_blocks: Option<usize>,
	) -> Result<Self, ClientError> {
		sqlx::migrate!()
			.run(&db_ctx.pool)
			.await
			.map_err(|e| sqlx::Error::Migrate(e.into()))?;

		let provider = Self {
			db_ctx,
			block_provider,
			receipt_extractor,
			keep_latest_n_blocks,
			block_number_to_hashes: Default::default(),
		};
		provider.restore_first_evm_block().await?;

		Ok(provider)
	}

	/// Returns `true` if the block is before the auto-discovered `first_evm_block`.
	pub fn is_before_earliest_block(&self, at: &BlockNumberOrTag) -> bool {
		match at {
			BlockNumberOrTag::Number(block_number) => {
				self.receipt_extractor.is_before_first_evm_block(*block_number)
			},
			BlockNumberOrTag::Latest |
			BlockNumberOrTag::Finalized |
			BlockNumberOrTag::Safe |
			BlockNumberOrTag::Earliest |
			BlockNumberOrTag::Pending => false,
		}
	}

	/// The auto-discovered first EVM block, or `None` if not yet discovered.
	pub fn first_evm_block(&self) -> Option<SubstrateBlockNumber> {
		self.receipt_extractor.first_evm_block()
	}

	/// Set the auto-discovered first EVM block (in-memory + persisted to DB).
	pub async fn set_first_evm_block(
		&self,
		block_number: SubstrateBlockNumber,
	) -> Result<(), ClientError> {
		self.receipt_extractor.set_first_evm_block(block_number);
		self.set_sync_label(ChainMetadata::FirstEvmBlock, SyncCheckpoint::from_number(block_number))
			.await
	}

	/// Restore `first_evm_block` from DB, clearing it if the boundary has shifted.
	async fn restore_first_evm_block(&self) -> Result<(), ClientError> {
		let Some(evm_first) =
			self.get_sync_label(ChainMetadata::FirstEvmBlock).await?.map(|c| c.block_number)
		else {
			return Ok(());
		};

		let has_evm_hash = |block_number: SubstrateBlockNumber| async move {
			match self.block_provider.block_by_number(block_number).await.ok().flatten() {
				Some(block) => self
					.receipt_extractor
					.get_ethereum_block_hash(&block.hash(), block_number)
					.await
					.is_some(),
				None => false,
			}
		};

		// Stale if evm_first no longer has an EVM hash, or its predecessor now does.
		let current_has_evm = has_evm_hash(evm_first).await;
		let predecessor_has_evm =
			if evm_first > 0 { has_evm_hash(evm_first - 1).await } else { false };

		if !current_has_evm || predecessor_has_evm {
			log::warn!(target: LOG_TARGET,
				"🗄️ Stored first-evm-block=#{evm_first} is stale \
				 (has_evm={current_has_evm}, predecessor_has_evm={predecessor_has_evm}), \
				 clearing.");
			if let Err(e) = self.delete_sync_label(ChainMetadata::FirstEvmBlock).await {
				log::error!(target: LOG_TARGET,
					"🗄️ Failed to clear stale first-evm-block from DB: {e:?}");
			}
		} else {
			self.receipt_extractor.set_first_evm_block(evm_first);
		}
		Ok(())
	}

	// Get block hash and transaction index by transaction hash
	pub async fn find_transaction(&self, transaction_hash: &H256) -> Option<(H256, usize)> {
		let transaction_hash_bytes = transaction_hash.as_ref();
		let result = query!(
			r#"
			SELECT block_hash, transaction_index
			FROM transaction_hashes
			WHERE transaction_hash = $1
			"#,
			transaction_hash_bytes
		)
		.fetch_optional(&self.db_ctx.pool)
		.await
		.inspect_err(|err| {
			log::trace!(target: LOG_TARGET,
				"find_transaction: DB query failed for tx {transaction_hash:?}: {err:?}");
		})
		.ok()?
		.or_else(|| {
			log::trace!(target: LOG_TARGET,
				"find_transaction: tx {transaction_hash:?} not found in DB");
			None
		})?;

		let block_hash = H256::from_slice(&result.block_hash[..]);
		let transaction_index = result.transaction_index.try_into().ok()?;
		Some((block_hash, transaction_index))
	}

	/// Get the Substrate block hash for the given Ethereum block hash.
	pub async fn get_substrate_hash(&self, ethereum_block_hash: &H256) -> Option<H256> {
		let ethereum_hash = ethereum_block_hash.as_ref();
		let result = query!(
			r#"
			SELECT substrate_block_hash
			FROM eth_to_substrate_blocks
			WHERE ethereum_block_hash = $1
			"#,
			ethereum_hash
		)
		.fetch_optional(&self.db_ctx.pool)
		.await
		.inspect_err(|e| {
			log::error!(target: LOG_TARGET, "failed to get block mapping for ethereum block {ethereum_block_hash:?}, err: {e:?}");
		})
		.ok()?
		.or_else(||{
			log::trace!(target: LOG_TARGET, "No block mapping found for ethereum block: {ethereum_block_hash:?}");
			None
		})?;

		log::trace!(target: LOG_TARGET, "Get block mapping ethereum block: {:?} -> substrate block: {ethereum_block_hash:?}", H256::from_slice(&result.substrate_block_hash[..]));

		Some(H256::from_slice(&result.substrate_block_hash[..]))
	}

	/// Get the Ethereum block hash for the given Substrate block hash.
	pub async fn get_ethereum_hash(&self, substrate_block_hash: &H256) -> Option<H256> {
		let substrate_hash = substrate_block_hash.as_ref();
		let result = query!(
			r#"
			SELECT ethereum_block_hash
			FROM eth_to_substrate_blocks
			WHERE substrate_block_hash = $1
			"#,
			substrate_hash
		)
		.fetch_optional(&self.db_ctx.pool)
		.await
		.inspect_err(|e| {
			log::error!(target: LOG_TARGET, "failed to get block mapping for substrate block {substrate_block_hash:?}, err: {e:?}");
		})
		.ok()?
		.or_else(||{
			log::trace!(target: LOG_TARGET, "No block mapping found for substrate block: {substrate_block_hash:?}");
			None
		})?;

		log::trace!(target: LOG_TARGET, "Get block mapping substrate block: {substrate_block_hash:?} -> ethereum block: {:?}", H256::from_slice(&result.ethereum_block_hash[..]));

		Some(H256::from_slice(&result.ethereum_block_hash[..]))
	}

	/// Deletes older records from the database.
	async fn remove(&self, block_mappings: &[BlockHashMap]) -> Result<(), ClientError> {
		if block_mappings.is_empty() {
			return Ok(());
		}
		log::debug!(target: LOG_TARGET, "Removing block hashes: {block_mappings:?}");

		let mut db_tx = self.db_ctx.pool.begin().await?;

		for chunk in block_mappings.chunks(self.db_ctx.max_variable_number) {
			let placeholders = vec!["?"; chunk.len()].join(", ");
			let sql_tx =
				format!("DELETE FROM transaction_hashes WHERE block_hash in ({placeholders})");
			let sql_logs = format!("DELETE FROM logs WHERE block_hash in ({placeholders})");
			let sql_mappings = format!(
				"DELETE FROM eth_to_substrate_blocks WHERE substrate_block_hash in ({placeholders})"
			);

			let mut delete_tx_query = sqlx::query(&sql_tx);
			let mut delete_logs_query = sqlx::query(&sql_logs);
			let mut delete_mappings_query = sqlx::query(&sql_mappings);

			for block_map in chunk {
				delete_tx_query = delete_tx_query.bind(block_map.substrate_hash.as_ref());
				delete_logs_query = delete_logs_query.bind(block_map.ethereum_hash.as_ref());
				delete_mappings_query =
					delete_mappings_query.bind(block_map.substrate_hash.as_ref());
			}

			delete_tx_query.execute(&mut *db_tx).await?;
			delete_logs_query.execute(&mut *db_tx).await?;
			delete_mappings_query.execute(&mut *db_tx).await?;
		}

		db_tx.commit().await?;
		Ok(())
	}

	/// Read a sync label entry.
	pub async fn get_sync_label(
		&self,
		label: impl SyncStateKey,
	) -> Result<Option<SyncCheckpoint>, ClientError> {
		let label_str = label.to_string();
		let row = query!(
			r#"
			SELECT block_number, block_hash
			FROM sync_state
			WHERE label = $1
			"#,
			label_str
		)
		.fetch_optional(&self.db_ctx.pool)
		.await?;

		match row {
			Some(row) => {
				let block_number: SubstrateBlockNumber =
					row.block_number.try_into().map_err(|_| {
						sqlx::Error::Decode(
							format!("block_number {} overflows u32", row.block_number).into(),
						)
					})?;
				Ok(Some(SyncCheckpoint {
					block_number,
					block_hash: row
						.block_hash
						.filter(|b| b.len() == 32)
						.map(|b| H256::from_slice(&b)),
				}))
			},
			None => Ok(None),
		}
	}

	/// Upsert a sync label entry.
	pub async fn set_sync_label(
		&self,
		label: impl SyncStateKey,
		checkpoint: SyncCheckpoint,
	) -> Result<(), ClientError> {
		let label_str = label.to_string();
		let block_number = checkpoint.block_number as i64;
		let block_hash = checkpoint.block_hash.map(|h| h.as_bytes().to_vec());
		query!(
			r#"
			INSERT OR REPLACE INTO sync_state (label, block_number, block_hash)
			VALUES ($1, $2, $3)
			"#,
			label_str,
			block_number,
			block_hash,
		)
		.execute(&self.db_ctx.pool)
		.await?;
		Ok(())
	}

	/// Delete a sync label entry.
	pub async fn delete_sync_label(&self, label: impl SyncStateKey) -> Result<(), ClientError> {
		let label_str = label.to_string();
		query!(
			r#"
			DELETE FROM sync_state WHERE label = $1
			"#,
			label_str,
		)
		.execute(&self.db_ctx.pool)
		.await?;
		Ok(())
	}

	/// Atomically update a sync label entry only if the new block number is strictly higher.
	///
	/// Inserts the row if it doesn't exist yet.
	pub async fn advance_sync_label(
		&self,
		label: SyncLabel,
		checkpoint: SyncCheckpoint,
	) -> Result<(), ClientError> {
		upsert_sync_label!(&self.db_ctx.pool, "<", label, checkpoint);
		Ok(())
	}

	/// Atomically update a sync label entry only if the new block number is lower.
	///
	/// Inserts the row if it doesn't exist yet.
	pub async fn recede_sync_label(
		&self,
		label: SyncLabel,
		checkpoint: SyncCheckpoint,
	) -> Result<(), ClientError> {
		upsert_sync_label!(&self.db_ctx.pool, ">", label, checkpoint);
		Ok(())
	}

	/// Look up the ethereum block hash for a previously processed block from the in-memory cache.
	pub async fn get_processed_eth_block_hash(
		&self,
		block_number: SubstrateBlockNumber,
		substrate_hash: H256,
	) -> Option<H256> {
		self.block_number_to_hashes
			.lock()
			.await
			.get(&block_number)
			.filter(|entry| entry.substrate_hash == substrate_hash)
			.map(|entry| entry.ethereum_hash)
	}

	/// Fetch receipts from the given block, using a pre-fetched ethereum block hash.
	pub async fn receipts_from_block(
		&self,
		block: &SubstrateBlock,
		ethereum_hash: H256,
	) -> Result<Vec<(TransactionSigned, ReceiptInfo)>, ClientError> {
		self.receipt_extractor
			.extract_from_block_with_eth_hash(block, ethereum_hash)
			.await
	}

	/// Fetch all receipts for the given block.
	pub async fn block_receipts(
		&self,
		block: &SubstrateBlock,
	) -> Result<Vec<(TransactionSigned, ReceiptInfo)>, ClientError> {
		self.receipt_extractor.extract_from_block(block).await
	}

	/// Like [`Self::insert_block_receipts`] but writes only to the DB (no cache update).
	/// Used for historic sync where fork detection is unnecessary.
	pub async fn insert_block_receipts_past(
		&self,
		block: &SubstrateBlock,
		ethereum_hash: &H256,
	) -> Result<(), ClientError> {
		let receipts = self
			.receipt_extractor
			.extract_from_block_with_eth_hash(block, *ethereum_hash)
			.await?;
		self.insert_into_db(block, &receipts, ethereum_hash).await?;
		Ok(())
	}

	/// Insert pre-extracted receipts and update the block cache (with fork detection).
	pub async fn insert_block_receipts(
		&self,
		block: &SubstrateBlock,
		receipts: &[(TransactionSigned, ReceiptInfo)],
		ethereum_hash: &H256,
	) -> Result<(), ClientError> {
		self.insert(block, receipts, ethereum_hash).await
	}

	/// Insert receipts into the provider, updating the in-memory block cache for fork detection.
	///
	/// Note: Can be merged into `insert_block_receipts` once <https://github.com/paritytech/subxt/issues/1883> is fixed and subxt let
	/// us create Mock `SubstrateBlock`
	async fn insert(
		&self,
		block: &impl BlockInfo,
		receipts: &[(TransactionSigned, ReceiptInfo)],
		ethereum_hash: &H256,
	) -> Result<(), ClientError> {
		let block_map = BlockHashMap::new(block.hash(), *ethereum_hash);
		self.prune_blocks(block.number(), &block_map).await?;
		self.insert_into_db(block, receipts, ethereum_hash).await?;
		Ok(())
	}

	/// Handle fork detection (always) and DB pruning (temporary mode only).
	async fn prune_blocks(
		&self,
		block_number: SubstrateBlockNumber,
		block_map: &BlockHashMap,
	) -> Result<(), ClientError> {
		let mut to_remove = Vec::new();
		let mut block_number_to_hash = self.block_number_to_hashes.lock().await;

		// Fork? - If inserting the same block number with a different hash, remove the old ones.
		match block_number_to_hash.insert(block_number, block_map.clone()) {
			Some(old_block_map) if &old_block_map != block_map => {
				to_remove.push(old_block_map);

				// Now loop through the blocks that were building on top of the old fork and remove
				// them.
				let mut next_block_number = block_number.saturating_add(1);
				while let Some(old_block_map) = block_number_to_hash.remove(&next_block_number) {
					to_remove.push(old_block_map);
					next_block_number = next_block_number.saturating_add(1);
				}
			},
			_ => {},
		}

		if let Some(keep_latest_n_blocks) = self.keep_latest_n_blocks {
			// If we have more blocks than we should keep, remove the oldest ones by count
			// (not by block number range, to handle gaps correctly)
			while block_number_to_hash.len() > keep_latest_n_blocks {
				// Remove the block with the smallest number (first in BTreeMap)
				if let Some((_, block_map)) = block_number_to_hash.pop_first() {
					to_remove.push(block_map);
				}
			}
		} else {
			// Evict oldest entries to prevent unbounded growth.
			// Forks deeper than MAX_CACHED_BLOCKS(256) are unlikely.
			while block_number_to_hash.len() > MAX_CACHED_BLOCKS {
				block_number_to_hash.pop_first();
			}
		}

		// Release the lock.
		drop(block_number_to_hash);

		if !to_remove.is_empty() {
			log::trace!(target: LOG_TARGET, "Pruning old blocks: {to_remove:?}");
			self.remove(&to_remove).await?;
		}

		Ok(())
	}

	/// Insert receipts into the database without updating the in-memory block cache.
	async fn insert_into_db(
		&self,
		block: &impl BlockInfo,
		receipts: &[(TransactionSigned, ReceiptInfo)],
		ethereum_hash: &H256,
	) -> Result<(), ClientError> {
		let block_number = block.number() as i64;
		let substrate_block_hash = block.hash();
		let substrate_hash_ref = substrate_block_hash.as_ref();
		let ethereum_hash_ref = ethereum_hash.as_ref();

		log::trace!(target: LOG_TARGET, "Inserting receipts for block #{block_number} ethereum: {ethereum_hash:?} substrate: {substrate_block_hash:?}");

		// Check if mapping already exists (eg. added when processing best block and we are now
		// processing finalized block)
		let result = sqlx::query!(
			r#"SELECT EXISTS(SELECT 1 FROM eth_to_substrate_blocks WHERE substrate_block_hash = $1) AS "exists!:bool""#, substrate_hash_ref
		)
		.fetch_one(&self.db_ctx.pool)
		.await?;

		// Assuming that if no mapping exists then no relevant entries in transaction_hashes and
		// logs exist
		if result.exists {
			log::trace!(target: LOG_TARGET,
				"Skipping receipt insert for block #{block_number} ({substrate_block_hash:?}): \
				 mapping already exists. ETH hash: {ethereum_hash:?}, receipts count: {count}",
				count = receipts.len(),
			);
			return Ok(());
		}

		let mut db_tx = self.db_ctx.pool.begin().await?;

		for chunk in receipts.chunks(self.db_ctx.tx_insert_chunk_size) {
			let mut query_builder = QueryBuilder::<Sqlite>::new(
				"INSERT OR REPLACE INTO transaction_hashes (transaction_hash, block_hash, transaction_index) ",
			);
			query_builder.push_values(chunk, |mut row, (_, receipt)| {
				row.push_bind(receipt.transaction_hash.as_ref() as &[u8])
					.push_bind(substrate_hash_ref)
					.push_bind(receipt.transaction_index.as_u32() as i32);
			});
			query_builder.build().execute(&mut *db_tx).await?;
		}

		let all_logs: Vec<(i32, &[u8], &Log)> = receipts
			.iter()
			.flat_map(|(_, receipt)| {
				let tx_index = receipt.transaction_index.as_u32() as i32;
				let tx_hash: &[u8] = receipt.transaction_hash.as_ref();
				receipt.logs.iter().map(move |log| (tx_index, tx_hash, log))
			})
			.collect();

		for chunk in all_logs.chunks(self.db_ctx.log_insert_chunk_size) {
			let mut query_builder = QueryBuilder::<Sqlite>::new(
				"INSERT OR REPLACE INTO logs(block_hash, transaction_index, log_index, address, block_number, transaction_hash, topic_0, topic_1, topic_2, topic_3, data) ",
			);
			query_builder.push_values(chunk, |mut row, (tx_index, tx_hash, log)| {
				row.push_bind(ethereum_hash_ref)
					.push_bind(*tx_index)
					.push_bind(log.log_index.as_u32() as i32)
					.push_bind(log.address.as_ref() as &[u8])
					.push_bind(block_number)
					.push_bind(*tx_hash)
					.push_bind(log.topics.first().map(|v| &v[..]))
					.push_bind(log.topics.get(1).map(|v| &v[..]))
					.push_bind(log.topics.get(2).map(|v| &v[..]))
					.push_bind(log.topics.get(3).map(|v| &v[..]))
					.push_bind(log.data.as_ref().map(|v| &v.0[..]));
			});
			query_builder.build().execute(&mut *db_tx).await?;
		}

		let block_map = BlockHashMap::new(substrate_block_hash, *ethereum_hash);
		insert_block_mapping(&mut *db_tx, &block_map).await?;

		db_tx.commit().await?;
		log::trace!(target: LOG_TARGET, "Inserted {} receipts for block #{block_number} ethereum: {ethereum_hash:?} substrate: {substrate_block_hash:?}", receipts.len());

		Ok(())
	}

	/// Get logs that match the given filter.
	///
	/// `resolve_block_number` converts a [`BlockNumberOrTag`] to a concrete block number.
	pub async fn logs<Fut>(
		&self,
		filter: Option<Filter>,
		resolve_block_number: impl Fn(BlockNumberOrTag) -> Fut,
	) -> anyhow::Result<Vec<Log>>
	where
		Fut: Future<Output = anyhow::Result<U256>>,
	{
		let mut qb = QueryBuilder::<Sqlite>::new("SELECT logs.* FROM logs WHERE 1=1");
		let filter = filter.unwrap_or_default();

		match filter.block_option {
			FilterBlockOption::AtBlockHash(hash) => {
				qb.push(" AND block_hash = ").push_bind(hash.as_slice().to_vec());
			},
			FilterBlockOption::Range { from_block, to_block } => {
				let from_block =
					OptionFuture::from(from_block.map(&resolve_block_number)).await.transpose()?;
				let to_block =
					OptionFuture::from(to_block.map(&resolve_block_number)).await.transpose()?;

				// Read the latest block *after* resolving the tags.
				let latest_block = U256::from(self.block_provider.latest_block_number().await);

				match (from_block, to_block) {
					(Some(block), _) | (_, Some(block)) if block > latest_block => {
						anyhow::bail!("block number exceeds latest block");
					},
					(Some(from_block), Some(to_block)) if from_block > to_block => {
						anyhow::bail!("invalid block range params");
					},
					(Some(from_block), Some(to_block)) if from_block == to_block => {
						qb.push(" AND block_number = ").push_bind(from_block.as_u64() as i64);
					},
					(Some(from_block), Some(to_block)) => {
						qb.push(" AND block_number BETWEEN ")
							.push_bind(from_block.as_u64() as i64)
							.push(" AND ")
							.push_bind(to_block.as_u64() as i64);
					},
					(Some(from_block), None) => {
						qb.push(" AND block_number >= ").push_bind(from_block.as_u64() as i64);
					},
					(None, Some(to_block)) => {
						qb.push(" AND block_number <= ").push_bind(to_block.as_u64() as i64);
					},
					(None, None) => {
						qb.push(" AND block_number = ").push_bind(latest_block.as_u64() as i64);
					},
				}
			},
		}

		if !filter.address.is_empty() {
			qb.push(" AND address IN (");
			let mut separated = qb.separated(", ");
			for addr in filter.address {
				separated.push_bind(addr.as_slice().to_vec());
			}
			separated.push_unseparated(")");
		}

		for (i, topic) in filter.topics.into_iter().enumerate() {
			if topic.is_empty() {
				continue;
			}

			qb.push(format_args!(" AND topic_{i} IN ("));
			let mut separated = qb.separated(", ");
			for hash in topic {
				separated.push_bind(hash.as_slice().to_vec());
			}
			separated.push_unseparated(")");
		}

		qb.push(" LIMIT ").push_bind(MAX_LOG_RESULTS as i64);

		let logs = qb.build().try_map(parse_log_row).fetch_all(&self.db_ctx.pool).await?;

		if logs.len() == MAX_LOG_RESULTS {
			log::warn!(
				target: LOG_TARGET,
				"Log query hit limit of {MAX_LOG_RESULTS}; results may be truncated",
			);
		}

		Ok(logs)
	}

	/// Fetch all logs for a given block from the database.
	pub async fn logs_by_block_number(
		&self,
		block_number: SubstrateBlockNumber,
		ethereum_hash: H256,
	) -> Result<Vec<Log>, ClientError> {
		let mut query_builder =
			QueryBuilder::<Sqlite>::new("SELECT logs.* FROM logs WHERE block_number = ");
		query_builder
			.push_bind(block_number as i64)
			.push(" AND block_hash = ")
			.push_bind(ethereum_hash.as_bytes().to_vec())
			.push(" ORDER BY log_index LIMIT ")
			.push_bind(MAX_LOG_RESULTS as i64);

		let logs = query_builder
			.build()
			.try_map(parse_log_row)
			.fetch_all(&self.db_ctx.pool)
			.await?;

		if logs.len() == MAX_LOG_RESULTS {
			log::warn!(
				target: LOG_TARGET,
				"Log query for block {block_number} hit limit of {MAX_LOG_RESULTS}; results may be truncated",
			);
		}

		Ok(logs)
	}

	/// Get the number of receipts per block.
	pub async fn receipts_count_per_block(&self, block_hash: &H256) -> Option<usize> {
		let block_hash = block_hash.as_ref();
		let row = query!(
			r#"
            SELECT COUNT(*) as count
            FROM transaction_hashes
            WHERE block_hash = $1
            "#,
			block_hash
		)
		.fetch_one(&self.db_ctx.pool)
		.await
		.ok()?;

		let count = row.count as usize;
		Some(count)
	}

	/// Return all transaction hashes for the given block hash.
	pub async fn block_transaction_hashes(
		&self,
		block_hash: &H256,
	) -> Option<HashMap<usize, H256>> {
		let block_hash = block_hash.as_ref();
		let rows = query!(
			r#"
		      SELECT transaction_index, transaction_hash
		      FROM transaction_hashes
		      WHERE block_hash = $1
		      "#,
			block_hash
		)
		.map(|row| {
			let transaction_index = row.transaction_index as usize;
			let transaction_hash = H256::from_slice(&row.transaction_hash);
			(transaction_index, transaction_hash)
		})
		.fetch_all(&self.db_ctx.pool)
		.await
		.ok()?;

		Some(rows.into_iter().collect())
	}

	/// Get the receipt for the given block hash and transaction index.
	pub async fn receipt_by_block_hash_and_index(
		&self,
		block_hash: &H256,
		transaction_index: usize,
	) -> Option<ReceiptInfo> {
		let block = self.block_provider.block_by_hash(block_hash).await.ok()??;
		let (_, receipt) = self
			.receipt_extractor
			.extract_from_transaction(&block, transaction_index)
			.await
			.ok()?;
		Some(receipt)
	}

	/// Get the receipt for the given transaction hash.
	pub async fn receipt_by_hash(&self, transaction_hash: &H256) -> Option<ReceiptInfo> {
		let (block_hash, transaction_index) = self.find_transaction(transaction_hash).await?;

		let block = match self.block_provider.block_by_hash(&block_hash).await {
			Ok(Some(b)) => b,
			Ok(None) => {
				log::trace!(target: LOG_TARGET,
					"receipt_by_hash: block {block_hash:?} not available from node (pruned?) for tx {transaction_hash:?}");
				return None;
			},
			Err(err) => {
				log::trace!(target: LOG_TARGET,
					"receipt_by_hash: failed to fetch block {block_hash:?} for tx {transaction_hash:?}: {err:?}");
				return None;
			},
		};

		match self.receipt_extractor.extract_from_transaction(&block, transaction_index).await {
			Ok((_, receipt)) => Some(receipt),
			Err(err) => {
				log::trace!(target: LOG_TARGET,
					"receipt_by_hash: extraction failed for tx {transaction_hash:?} in block {block_hash:?}: {err:?}");
				None
			},
		}
	}

	/// Get the signed transaction for the given transaction hash.
	pub async fn signed_tx_by_hash(&self, transaction_hash: &H256) -> Option<TransactionSigned> {
		let (block_hash, transaction_index) = self.find_transaction(transaction_hash).await?;

		let block = self.block_provider.block_by_hash(&block_hash).await.ok()??;
		let (signed_tx, _) = self
			.receipt_extractor
			.extract_from_transaction(&block, transaction_index)
			.await
			.inspect_err(|err| {
				log::trace!(target: LOG_TARGET,
					"signed_tx_by_hash: extraction failed for tx {transaction_hash:?} \
					 in block {block_hash:?}: {err:?}");
			})
			.ok()?;
		Some(signed_tx)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		ReceiptInfo,
		test::{MockBlockInfo, MockBlockInfoProvider},
	};
	use alloy_primitives::{Address as AlloyAddress, B256};
	use pallet_revive::evm::TransactionSigned;
	use pretty_assertions::assert_eq;
	use sp_core::{H160, H256};
	use sqlx::SqlitePool;

	async fn count(pool: &SqlitePool, table: &str, block_hash: Option<H256>) -> usize {
		let count: i64 = match block_hash {
			None => {
				sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
					.fetch_one(pool)
					.await
			},
			Some(hash) => {
				sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE block_hash = ?"))
					.bind(hash.as_ref())
					.fetch_one(pool)
					.await
			},
		}
		.unwrap();

		count as _
	}

	fn mock_provider() -> ReceiptProvider<MockBlockInfoProvider> {
		ReceiptProvider {
			db_ctx: DbContext::new(
				SqlitePool::connect_lazy("sqlite::memory:").unwrap(),
				DbContext::DEFAULT_MAX_VARIABLE_NUMBER,
			),
			block_provider: MockBlockInfoProvider {},
			receipt_extractor: ReceiptExtractor::new_mock(),
			keep_latest_n_blocks: None,
			block_number_to_hashes: Default::default(),
		}
	}

	/// Test resolver that handles Latest → `latest` and Earliest → 0.
	fn mock_resolve_block_number_with_latest(
		latest: u64,
	) -> impl Fn(BlockNumberOrTag) -> std::future::Ready<anyhow::Result<U256>> {
		move |block: BlockNumberOrTag| {
			std::future::ready(Ok(match block {
				BlockNumberOrTag::Number(v) => U256::from(v),
				BlockNumberOrTag::Earliest => U256::zero(),
				// The mock only tracks `latest`, so the remaining tags resolve to it.
				_ => U256::from(latest),
			}))
		}
	}

	impl ReceiptProvider<MockBlockInfoProvider> {
		fn with_db_ctx(mut self, db_ctx: DbContext) -> Self {
			self.db_ctx = db_ctx;
			self
		}

		fn with_extractor(mut self, extractor: ReceiptExtractor) -> Self {
			self.receipt_extractor = extractor;
			self
		}

		fn with_keep_latest(mut self, n: Option<usize>) -> Self {
			self.keep_latest_n_blocks = n;
			self
		}
	}

	async fn setup_sqlite_provider(pool: SqlitePool) -> ReceiptProvider<MockBlockInfoProvider> {
		mock_provider()
			.with_db_ctx(DbContext::new(pool, DbContext::DEFAULT_MAX_VARIABLE_NUMBER))
			.with_keep_latest(Some(10))
	}

	#[sqlx::test]
	async fn test_insert_remove(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let block = MockBlockInfo { hash: H256::default(), number: 0 };
		let receipts = vec![(
			TransactionSigned::default(),
			ReceiptInfo {
				logs: vec![Log { block_hash: block.hash, ..Default::default() }],
				..Default::default()
			},
		)];
		let ethereum_hash = H256::from([1_u8; 32]);
		let block_map = BlockHashMap::new(block.hash(), ethereum_hash);

		provider.insert(&block, &receipts, &ethereum_hash).await?;
		let row = provider.find_transaction(&receipts[0].1.transaction_hash).await;
		assert_eq!(row, Some((block.hash, 0)));
		assert_eq!(count(&provider.db_ctx.pool, "transaction_hashes", Some(block.hash())).await, 1);
		assert_eq!(count(&provider.db_ctx.pool, "logs", Some(ethereum_hash)).await, 1);

		provider.remove(&[block_map]).await?;
		assert_eq!(count(&provider.db_ctx.pool, "transaction_hashes", Some(block.hash())).await, 0);
		assert_eq!(count(&provider.db_ctx.pool, "logs", Some(ethereum_hash)).await, 0);
		Ok(())
	}

	#[sqlx::test]
	async fn test_prune(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let n = provider.keep_latest_n_blocks.unwrap();

		for i in 0..2 * n {
			let block = MockBlockInfo { hash: H256::from([i as u8; 32]), number: i as _ };
			let transaction_hash = H256::from([i as u8; 32]);
			let receipts = vec![(
				TransactionSigned::default(),
				ReceiptInfo {
					transaction_hash,
					logs: vec![Log {
						block_hash: block.hash,
						transaction_hash,
						..Default::default()
					}],
					..Default::default()
				},
			)];
			let ethereum_hash = H256::from([(i + 1) as u8; 32]);
			provider.insert(&block, &receipts, &ethereum_hash).await?;
		}
		assert_eq!(count(&provider.db_ctx.pool, "transaction_hashes", None).await, n);
		assert_eq!(count(&provider.db_ctx.pool, "logs", None).await, n);
		assert_eq!(count(&provider.db_ctx.pool, "eth_to_substrate_blocks", None).await, n);
		assert_eq!(provider.block_number_to_hashes.lock().await.len(), n);

		return Ok(());
	}

	#[sqlx::test]
	async fn test_fork(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;

		let build_block = |seed, number| {
			let block = MockBlockInfo { hash: H256::from([seed; 32]), number };
			let transaction_hash = H256::from([seed; 32]);
			let receipts = vec![(
				TransactionSigned::default(),
				ReceiptInfo {
					transaction_hash,
					logs: vec![Log {
						block_hash: block.hash,
						transaction_hash,
						..Default::default()
					}],
					..Default::default()
				},
			)];
			let ethereum_hash = H256::from([seed + 1; 32]);

			(block, receipts, ethereum_hash)
		};

		// Build 4 blocks on consecutive heights: 0,1,2,3.
		let (block0, receipts, ethereum_hash_0) = build_block(0, 0);
		provider.insert(&block0, &receipts, &ethereum_hash_0).await?;
		let (block1, receipts, ethereum_hash_1) = build_block(1, 1);
		provider.insert(&block1, &receipts, &ethereum_hash_1).await?;
		let (block2, receipts, ethereum_hash_2) = build_block(2, 2);
		provider.insert(&block2, &receipts, &ethereum_hash_2).await?;
		let (block3, receipts, ethereum_hash_3) = build_block(3, 3);
		provider.insert(&block3, &receipts, &ethereum_hash_3).await?;

		assert_eq!(count(&provider.db_ctx.pool, "transaction_hashes", None).await, 4);
		assert_eq!(count(&provider.db_ctx.pool, "logs", None).await, 4);
		assert_eq!(count(&provider.db_ctx.pool, "eth_to_substrate_blocks", None).await, 4);
		assert_eq!(
			provider.block_number_to_hashes.lock().await.clone(),
			[
				(0, BlockHashMap::new(block0.hash, ethereum_hash_0)),
				(1, BlockHashMap::new(block1.hash, ethereum_hash_1)),
				(2, BlockHashMap::new(block2.hash, ethereum_hash_2)),
				(3, BlockHashMap::new(block3.hash, ethereum_hash_3))
			]
			.into(),
		);

		// Now build another block on height 1.
		let (fork_block, receipts, ethereum_hash_fork) = build_block(4, 1);
		provider.insert(&fork_block, &receipts, &ethereum_hash_fork).await?;

		assert_eq!(count(&provider.db_ctx.pool, "transaction_hashes", None).await, 2);
		assert_eq!(count(&provider.db_ctx.pool, "logs", None).await, 2);
		assert_eq!(count(&provider.db_ctx.pool, "eth_to_substrate_blocks", None).await, 2);

		assert_eq!(
			provider.block_number_to_hashes.lock().await.clone(),
			[
				(0, BlockHashMap::new(block0.hash, ethereum_hash_0)),
				(1, BlockHashMap::new(fork_block.hash, ethereum_hash_fork))
			]
			.into(),
		);

		return Ok(());
	}

	#[sqlx::test]
	async fn test_reorg_same_transaction_hash(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;

		// Build two blocks at the same height with the same transaction hash
		let tx_hash = H256::from([42u8; 32]);

		// Block A at height 1
		let block_a = MockBlockInfo { hash: H256::from([1u8; 32]), number: 1 };
		let ethereum_hash_a = H256::from([2u8; 32]);
		let receipts_a = vec![(
			TransactionSigned::default(),
			ReceiptInfo {
				transaction_hash: tx_hash,
				transaction_index: U256::from(0),
				..Default::default()
			},
		)];

		provider.insert(&block_a, &receipts_a, &ethereum_hash_a).await?;

		// Verify transaction points to block A
		let (found_hash, _) = provider.find_transaction(&tx_hash).await.unwrap();
		assert_eq!(found_hash, block_a.hash);

		// Clear the in-memory map to simulate server restart
		provider.block_number_to_hashes.lock().await.clear();

		// Block B at same height 1 (re-org) with SAME transaction
		let block_b = MockBlockInfo { hash: H256::from([3u8; 32]), number: 1 };
		let ethereum_hash_b = H256::from([4u8; 32]);
		let receipts_b = vec![(
			TransactionSigned::default(),
			ReceiptInfo {
				transaction_hash: tx_hash, // Same tx hash!
				transaction_index: U256::from(0),
				..Default::default()
			},
		)];

		// This should NOT fail with UNIQUE constraint violation
		provider.insert(&block_b, &receipts_b, &ethereum_hash_b).await?;

		// Transaction should now point to block B
		let (found_hash, _) = provider.find_transaction(&tx_hash).await.unwrap();
		assert_eq!(found_hash, block_b.hash);

		Ok(())
	}

	#[sqlx::test]
	async fn test_receipts_count_per_block(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let block = MockBlockInfo { hash: H256::default(), number: 0 };
		let receipts = vec![
			(
				TransactionSigned::default(),
				ReceiptInfo { transaction_hash: H256::from([0u8; 32]), ..Default::default() },
			),
			(
				TransactionSigned::default(),
				ReceiptInfo { transaction_hash: H256::from([1u8; 32]), ..Default::default() },
			),
		];
		let ethereum_hash = H256::from([2u8; 32]);

		provider.insert(&block, &receipts, &ethereum_hash).await?;
		let count = provider.receipts_count_per_block(&block.hash).await;
		assert_eq!(count, Some(2));
		Ok(())
	}

	#[sqlx::test]
	async fn test_query_logs(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let block1 = MockBlockInfo { hash: H256::from([1u8; 32]), number: 1 };
		let block2 = MockBlockInfo { hash: H256::from([2u8; 32]), number: 2 };
		let ethereum_hash1 = H256::from([3u8; 32]);
		let ethereum_hash2 = H256::from([4u8; 32]);
		let log1 = Log {
			block_hash: ethereum_hash1,
			block_number: block1.number.into(),
			address: H160::from([1u8; 20]),
			topics: vec![H256::from([1u8; 32]), H256::from([2u8; 32])],
			data: Some(vec![0u8; 32].into()),
			transaction_hash: H256::default(),
			transaction_index: U256::from(1),
			log_index: U256::from(1),
			..Default::default()
		};
		let log2 = Log {
			block_hash: ethereum_hash2,
			block_number: block2.number.into(),
			address: H160::from([2u8; 20]),
			topics: vec![H256::from([2u8; 32]), H256::from([3u8; 32])],
			transaction_hash: H256::from([1u8; 32]),
			transaction_index: U256::from(2),
			log_index: U256::from(1),
			..Default::default()
		};

		provider
			.insert(
				&block1,
				&vec![(
					TransactionSigned::default(),
					ReceiptInfo {
						logs: vec![log1.clone()],
						transaction_hash: log1.transaction_hash,
						transaction_index: log1.transaction_index,
						..Default::default()
					},
				)],
				&ethereum_hash1,
			)
			.await?;
		provider
			.insert(
				&block2,
				&vec![(
					TransactionSigned::default(),
					ReceiptInfo {
						logs: vec![log2.clone()],
						transaction_hash: log2.transaction_hash,
						transaction_index: log2.transaction_index,
						..Default::default()
					},
				)],
				&ethereum_hash2,
			)
			.await?;

		let resolve_block_number = mock_resolve_block_number_with_latest(block2.number.into());

		// Empty filter
		let logs = provider.logs(None, &resolve_block_number).await?;
		assert_eq!(logs, vec![log2.clone()]);

		// from_block filter
		let logs = provider
			.logs(Some(Filter::new().from_block(log2.block_number.as_u64())), &resolve_block_number)
			.await?;
		assert_eq!(logs, vec![log2.clone()]);

		// from_block filter (using latest block)
		let logs = provider
			.logs(Some(Filter::new().from_block(BlockNumberOrTag::Latest)), &resolve_block_number)
			.await?;
		assert_eq!(logs, vec![log2.clone()]);

		// to_block filter
		let logs = provider
			.logs(Some(Filter::new().to_block(log1.block_number.as_u64())), &resolve_block_number)
			.await?;
		assert_eq!(logs, vec![log1.clone()]);

		// block_hash filter
		let logs = provider
			.logs(
				Some(Filter::new().at_block_hash(B256::from(log1.block_hash.0))),
				&resolve_block_number,
			)
			.await?;
		assert_eq!(logs, vec![log1.clone()]);

		// single address
		let logs = provider
			.logs(
				Some(
					Filter::new()
						.from_block(BlockNumberOrTag::Earliest)
						.address(AlloyAddress::from(log1.address.0)),
				),
				&resolve_block_number,
			)
			.await?;
		assert_eq!(logs, vec![log1.clone()]);

		// multiple addresses
		let logs = provider
			.logs(
				Some(Filter::new().from_block(BlockNumberOrTag::Earliest).address(vec![
					AlloyAddress::from(log1.address.0),
					AlloyAddress::from(log2.address.0),
				])),
				&resolve_block_number,
			)
			.await?;
		assert_eq!(logs, vec![log1.clone(), log2.clone()]);

		// single topic
		let logs = provider
			.logs(
				Some(
					Filter::new()
						.from_block(BlockNumberOrTag::Earliest)
						.event_signature(B256::from(log1.topics[0].0)),
				),
				&resolve_block_number,
			)
			.await?;
		assert_eq!(logs, vec![log1.clone()]);

		// multiple topic
		let logs = provider
			.logs(
				Some(
					Filter::new()
						.from_block(BlockNumberOrTag::Earliest)
						.event_signature(B256::from(log1.topics[0].0))
						.topic1(B256::from(log1.topics[1].0)),
				),
				&resolve_block_number,
			)
			.await?;
		assert_eq!(logs, vec![log1.clone()]);

		// multiple topic for topic_0
		let logs = provider
			.logs(
				Some(Filter::new().from_block(BlockNumberOrTag::Earliest).event_signature(vec![
					B256::from(log1.topics[0].0),
					B256::from(log2.topics[0].0),
				])),
				&resolve_block_number,
			)
			.await?;
		assert_eq!(logs, vec![log1.clone(), log2.clone()]);

		// Altogether
		let logs = provider
			.logs(
				Some(
					Filter::new()
						.from_block(BlockNumberOrTag::Earliest)
						.to_block(BlockNumberOrTag::Latest)
						.address(vec![
							AlloyAddress::from(log1.address.0),
							AlloyAddress::from(log2.address.0),
						])
						.event_signature(vec![
							B256::from(log1.topics[0].0),
							B256::from(log2.topics[0].0),
						]),
				),
				&resolve_block_number,
			)
			.await?;
		assert_eq!(logs, vec![log1.clone(), log2.clone()]);

		for tag in [
			BlockNumberOrTag::Latest,
			BlockNumberOrTag::Finalized,
			BlockNumberOrTag::Safe,
			BlockNumberOrTag::Pending,
		] {
			let logs = provider
				.logs(Some(Filter::new().from_block(tag)), &resolve_block_number)
				.await?;
			assert_eq!(logs, vec![log2.clone()], "tag {tag:?} should resolve to the latest block");
		}

		let logs = provider
			.logs(
				Some(
					Filter::new()
						.from_block(log1.block_number.as_u64())
						.to_block(log1.block_number.as_u64()),
				),
				&resolve_block_number,
			)
			.await?;
		assert_eq!(logs, vec![log1.clone()], "from == to selects the single block");

		let result = provider
			.logs(
				Some(
					Filter::new()
						.from_block(log2.block_number.as_u64())
						.to_block(log1.block_number.as_u64()),
				),
				&resolve_block_number,
			)
			.await;
		assert!(result.is_err(), "from > to should be rejected");

		let result = provider
			.logs(
				Some(Filter::new().from_block(log2.block_number.as_u64() + 100)),
				&resolve_block_number,
			)
			.await;
		assert!(result.is_err(), "block number beyond latest should be rejected");

		let failing = |_: BlockNumberOrTag| {
			std::future::ready(Err::<U256, anyhow::Error>(anyhow::anyhow!("resolver failed")))
		};
		let result = provider
			.logs(Some(Filter::new().from_block(BlockNumberOrTag::Latest)), &failing)
			.await;
		assert!(result.is_err(), "a resolver error should propagate");

		Ok(())
	}

	#[sqlx::test]
	async fn test_block_mapping_insert_get(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let ethereum_hash = H256::from([1u8; 32]);
		let substrate_hash = H256::from([2u8; 32]);
		let block_map = BlockHashMap::new(substrate_hash, ethereum_hash);

		// Insert mapping
		insert_block_mapping(&provider.db_ctx.pool, &block_map).await?;

		// Test forward lookup
		let resolved = provider.get_substrate_hash(&ethereum_hash).await;
		assert_eq!(resolved, Some(substrate_hash));

		// Test reverse lookup
		let resolved = provider.get_ethereum_hash(&substrate_hash).await;
		assert_eq!(resolved, Some(ethereum_hash));

		Ok(())
	}

	#[sqlx::test]
	async fn test_block_mapping_remove(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let ethereum_hash1 = H256::from([1u8; 32]);
		let ethereum_hash2 = H256::from([2u8; 32]);
		let substrate_hash1 = H256::from([3u8; 32]);
		let substrate_hash2 = H256::from([4u8; 32]);
		let block_map1 = BlockHashMap::new(substrate_hash1, ethereum_hash1);
		let block_map2 = BlockHashMap::new(substrate_hash2, ethereum_hash2);

		// Insert mappings
		insert_block_mapping(&provider.db_ctx.pool, &block_map1).await?;
		insert_block_mapping(&provider.db_ctx.pool, &block_map2).await?;

		// Verify they exist
		assert_eq!(
			provider.get_substrate_hash(&block_map1.ethereum_hash).await,
			Some(block_map1.substrate_hash)
		);
		assert_eq!(
			provider.get_substrate_hash(&block_map2.ethereum_hash).await,
			Some(block_map2.substrate_hash)
		);

		// Remove one mapping
		provider.remove(&[block_map1]).await?;

		// Verify removal
		assert_eq!(provider.get_substrate_hash(&ethereum_hash1).await, None);
		assert_eq!(provider.get_substrate_hash(&ethereum_hash2).await, Some(substrate_hash2));

		Ok(())
	}

	#[sqlx::test]
	async fn test_block_mapping_pruning_integration(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let ethereum_hash = H256::from([1u8; 32]);
		let substrate_hash = H256::from([2u8; 32]);
		let block_map = BlockHashMap::new(substrate_hash, ethereum_hash);

		// Insert mapping
		insert_block_mapping(&provider.db_ctx.pool, &block_map).await?;
		assert_eq!(
			provider.get_substrate_hash(&block_map.ethereum_hash).await,
			Some(block_map.substrate_hash)
		);

		// Remove substrate block (this should also remove the mapping)
		provider.remove(&[block_map.clone()]).await?;

		// Mapping should be gone
		assert_eq!(provider.get_substrate_hash(&block_map.ethereum_hash).await, None);

		Ok(())
	}

	#[sqlx::test]
	async fn test_logs_with_ethereum_block_hash_mapping(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let ethereum_hash = H256::from([1u8; 32]);
		let substrate_hash = H256::from([2u8; 32]);
		let block_number = 1u64;

		// Create a log with ethereum hash
		let log = Log {
			block_hash: ethereum_hash,
			block_number: block_number.into(),
			address: H160::from([1u8; 20]),
			topics: vec![H256::from([1u8; 32])],
			transaction_hash: H256::from([3u8; 32]),
			transaction_index: U256::from(0),
			log_index: U256::from(0),
			data: Some(vec![0u8; 32].into()),
			..Default::default()
		};

		// Insert the log
		let block = MockBlockInfo { hash: substrate_hash, number: block_number };
		let receipts = vec![(
			TransactionSigned::default(),
			ReceiptInfo {
				logs: vec![log.clone()],
				transaction_hash: log.transaction_hash,
				transaction_index: log.transaction_index,
				..Default::default()
			},
		)];
		provider.insert(&block, &receipts, &ethereum_hash).await?;

		// Query logs using Ethereum block hash (should resolve to substrate hash)
		let logs = provider
			.logs(
				Some(Filter::new().at_block_hash(B256::from(ethereum_hash.0))),
				mock_resolve_block_number_with_latest(block.number.into()),
			)
			.await?;
		assert_eq!(logs, vec![log]);

		Ok(())
	}

	#[sqlx::test]
	async fn test_mapping_count(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;

		// Initially no mappings
		assert_eq!(count(&provider.db_ctx.pool, "eth_to_substrate_blocks", None).await, 0);

		let block_map1 = BlockHashMap::new(H256::from([1u8; 32]), H256::from([2u8; 32]));
		let block_map2 = BlockHashMap::new(H256::from([3u8; 32]), H256::from([4u8; 32]));

		// Insert some mappings
		insert_block_mapping(&provider.db_ctx.pool, &block_map1).await?;
		insert_block_mapping(&provider.db_ctx.pool, &block_map2).await?;

		assert_eq!(count(&provider.db_ctx.pool, "eth_to_substrate_blocks", None).await, 2);

		// Remove one
		provider.remove(&[block_map1]).await?;
		assert_eq!(count(&provider.db_ctx.pool, "eth_to_substrate_blocks", None).await, 1);

		Ok(())
	}

	#[sqlx::test]
	async fn restore_first_evm_block_clears_stale(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;

		// Persist first_evm_block = 42.
		provider
			.set_sync_label(ChainMetadata::FirstEvmBlock, SyncCheckpoint::from_number(42))
			.await?;

		// MockBlockInfoProvider returns no blocks, so has_evm_hash is always false.
		// This means evm_first=42 is stale (no longer has an EVM hash).
		provider.restore_first_evm_block().await?;

		// The value should have been cleared (not restored to the extractor).
		assert_eq!(provider.first_evm_block(), None);

		// DB row should have been deleted.
		let checkpoint = provider.get_sync_label(ChainMetadata::FirstEvmBlock).await?;
		assert!(checkpoint.is_none());
		Ok(())
	}

	#[sqlx::test]
	async fn advance_sync_label_only_increases(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let hash_a = H256::repeat_byte(0xAA);
		let hash_b = H256::repeat_byte(0xBB);

		// First insert creates the row.
		provider
			.advance_sync_label(SyncLabel::Head, SyncCheckpoint::new(100, hash_a))
			.await?;
		let checkpoint = provider.get_sync_label(SyncLabel::Head).await?.unwrap();
		assert_eq!((checkpoint.block_number, checkpoint.block_hash), (100, Some(hash_a)));

		// Higher value advances.
		provider
			.advance_sync_label(SyncLabel::Head, SyncCheckpoint::new(200, hash_b))
			.await?;
		let checkpoint = provider.get_sync_label(SyncLabel::Head).await?.unwrap();
		assert_eq!((checkpoint.block_number, checkpoint.block_hash), (200, Some(hash_b)));

		// Lower and equal values are ignored (strict >).
		provider
			.advance_sync_label(SyncLabel::Head, SyncCheckpoint::new(50, hash_a))
			.await?;
		provider
			.advance_sync_label(SyncLabel::Head, SyncCheckpoint::new(200, hash_a))
			.await?;
		let checkpoint = provider.get_sync_label(SyncLabel::Head).await?.unwrap();
		assert_eq!((checkpoint.block_number, checkpoint.block_hash), (200, Some(hash_b)));

		Ok(())
	}

	#[sqlx::test]
	async fn recede_sync_label_only_decreases(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let hash_a = H256::repeat_byte(0xAA);
		let hash_b = H256::repeat_byte(0xBB);

		// First insert creates the row.
		provider
			.recede_sync_label(SyncLabel::Tail, SyncCheckpoint::new(100, hash_a))
			.await?;
		let checkpoint = provider.get_sync_label(SyncLabel::Tail).await?.unwrap();
		assert_eq!((checkpoint.block_number, checkpoint.block_hash), (100, Some(hash_a)));

		// Lower value recedes.
		provider
			.recede_sync_label(SyncLabel::Tail, SyncCheckpoint::new(50, hash_b))
			.await?;
		let checkpoint = provider.get_sync_label(SyncLabel::Tail).await?.unwrap();
		assert_eq!((checkpoint.block_number, checkpoint.block_hash), (50, Some(hash_b)));

		// Higher and equal values are ignored (strict <).
		provider
			.recede_sync_label(SyncLabel::Tail, SyncCheckpoint::new(200, hash_a))
			.await?;
		provider
			.recede_sync_label(SyncLabel::Tail, SyncCheckpoint::new(50, hash_a))
			.await?;
		let checkpoint = provider.get_sync_label(SyncLabel::Tail).await?.unwrap();
		assert_eq!((checkpoint.block_number, checkpoint.block_hash), (50, Some(hash_b)));

		Ok(())
	}

	#[tokio::test]
	async fn is_before_earliest_block_edge_cases() {
		// U256 > u32::MAX should never be considered "before floor"
		let extractor = ReceiptExtractor::new_mock();
		extractor.set_first_evm_block(10);
		let provider = mock_provider().with_extractor(extractor);

		let huge = BlockNumberOrTag::Number(u64::MAX);
		assert!(!provider.is_before_earliest_block(&huge));

		let just_over = BlockNumberOrTag::Number(u32::MAX as u64 + 1);
		assert!(!provider.is_before_earliest_block(&just_over));

		// Sentinel first_evm_block (u32::MAX) is permissive — no queries rejected.
		let provider = mock_provider();
		assert!(!provider.is_before_earliest_block(&BlockNumberOrTag::Number(0)));
		assert!(!provider.is_before_earliest_block(&BlockNumberOrTag::Number(1_000_000)));

		// Tag-based queries are never rejected.
		assert!(!provider.is_before_earliest_block(&BlockNumberOrTag::Latest));
	}

	#[sqlx::test]
	async fn persistent_mode_caps_in_memory_map(pool: SqlitePool) -> anyhow::Result<()> {
		// Persistent DB mode: keep_latest_n_blocks = None
		let provider = mock_provider()
			.with_db_ctx(DbContext::new(pool, DbContext::DEFAULT_MAX_VARIABLE_NUMBER));

		// Insert more than MAX_CACHED_BLOCKS blocks.
		let start_block: u64 = 1;
		let n = MAX_CACHED_BLOCKS + 1;
		let end_block = start_block + n as u64;
		for i in start_block..end_block {
			let block = MockBlockInfo { hash: H256::from_low_u64_be(i), number: i as _ };
			let receipts = vec![(
				TransactionSigned::default(),
				ReceiptInfo {
					transaction_hash: H256::from_low_u64_be(i),
					logs: vec![Log {
						block_hash: block.hash,
						transaction_hash: H256::from_low_u64_be(i),
						..Default::default()
					}],
					..Default::default()
				},
			)];
			let ethereum_hash = H256::from_low_u64_be(i + 1);
			provider.insert(&block, &receipts, &ethereum_hash).await?;
		}

		// The map is capped at MAX_CACHED_BLOCKS.
		let map = provider.block_number_to_hashes.lock().await;
		assert_eq!(map.len(), MAX_CACHED_BLOCKS);

		// The oldest block (1) should have been evicted, keeping blocks 2..=MAX+1.
		assert!(!map.contains_key(&1));
		assert!(map.contains_key(&2));
		assert!(map.contains_key(&(MAX_CACHED_BLOCKS as u64 + 1)));
		drop(map);

		// All blocks are still in the DB.
		assert_eq!(count(&provider.db_ctx.pool, "eth_to_substrate_blocks", None).await, n);

		Ok(())
	}

	fn make_hash(i: usize, fill: u8) -> H256 {
		let mut hash = [fill; 32];
		hash[..8].copy_from_slice(&i.to_le_bytes());
		H256::from(hash)
	}

	fn make_receipts(
		tx_offset: usize,
		n_tx: usize,
		n_logs: usize,
	) -> Vec<(TransactionSigned, ReceiptInfo)> {
		let mut receipts = Vec::with_capacity(n_tx);

		for i in 0..n_tx {
			let transaction_hash = make_hash(tx_offset + i, 0x00);

			let mut logs = Vec::with_capacity(n_logs);
			for j in 0..n_logs {
				logs.push(Log { transaction_hash, log_index: U256::from(j), ..Default::default() });
			}

			receipts.push((
				TransactionSigned::default(),
				ReceiptInfo {
					transaction_hash,
					transaction_index: U256::from(i),
					logs,
					..Default::default()
				},
			));
		}

		receipts
	}

	async fn assert_receipts_inserted(
		provider: &ReceiptProvider<MockBlockInfoProvider>,
		block: &MockBlockInfo,
		ethereum_hash: &H256,
		receipts: &[(TransactionSigned, ReceiptInfo)],
	) {
		let mut expected_logs = 0;
		for (_, receipt) in receipts {
			assert_eq!(
				provider.find_transaction(&receipt.transaction_hash).await,
				Some((block.hash(), receipt.transaction_index.as_u32() as usize))
			);
			expected_logs += receipt.logs.len();
		}
		assert_eq!(count(&provider.db_ctx.pool, "logs", Some(*ethereum_hash)).await, expected_logs);
	}

	#[sqlx::test]
	async fn test_bulk_insert(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await.with_keep_latest(None);
		let tx_chunk = provider.db_ctx.tx_insert_chunk_size;
		let log_chunk = provider.db_ctx.log_insert_chunk_size;

		let cases = [
			(tx_chunk, 1),             // exact tx chunk boundary
			(tx_chunk + 1, log_chunk), // crosses tx boundary; exact log chunk boundary
			(1000, 3),                 // multiple tx and log chunks
		];

		let mut tx_offset = 0;
		for (i, (n_tx, n_logs)) in cases.into_iter().enumerate() {
			let block = MockBlockInfo { hash: make_hash(i, 0x00), number: i as u64 + 1 };
			let ethereum_hash = make_hash(i, 0xff);
			let receipts = make_receipts(tx_offset, n_tx, n_logs);
			tx_offset += n_tx;
			provider.insert(&block, &receipts, &ethereum_hash).await?;
			assert_receipts_inserted(&provider, &block, &ethereum_hash, &receipts).await;
		}
		Ok(())
	}

	#[sqlx::test]
	async fn test_duplicate_insert_succeeds(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await.with_keep_latest(None);
		let block = MockBlockInfo { hash: make_hash(0, 0xAA), number: 1 };
		let ethereum_hash = make_hash(0, 0xBB);
		let receipts = make_receipts(0, 5, 3);

		// First insert.
		provider.insert_into_db(&block, &receipts, &ethereum_hash).await?;
		assert_eq!(count(&provider.db_ctx.pool, "transaction_hashes", None).await, 5);
		assert_eq!(count(&provider.db_ctx.pool, "logs", Some(ethereum_hash)).await, 15);
		assert_eq!(count(&provider.db_ctx.pool, "eth_to_substrate_blocks", None).await, 1);

		// Delete only the block mapping so the EXISTS guard won't short-circuit.
		sqlx::query("DELETE FROM eth_to_substrate_blocks")
			.execute(&provider.db_ctx.pool)
			.await?;
		assert_eq!(count(&provider.db_ctx.pool, "eth_to_substrate_blocks", None).await, 0);

		// Second insert hits the actual INSERT OR REPLACE statements.
		provider.insert_into_db(&block, &receipts, &ethereum_hash).await?;

		// Row counts unchanged — no duplicates.
		assert_eq!(count(&provider.db_ctx.pool, "transaction_hashes", None).await, 5);
		assert_eq!(count(&provider.db_ctx.pool, "logs", Some(ethereum_hash)).await, 15);
		assert_eq!(count(&provider.db_ctx.pool, "eth_to_substrate_blocks", None).await, 1);

		Ok(())
	}

	#[sqlx::test]
	async fn test_insert_empty_receipts(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await.with_keep_latest(None);
		let block = MockBlockInfo { hash: H256::from([1u8; 32]), number: 1 };
		let ethereum_hash = H256::from([2u8; 32]);

		provider.insert(&block, &[], &ethereum_hash).await?;

		// Block mapping is stored as a deduplication marker.
		assert_eq!(count(&provider.db_ctx.pool, "eth_to_substrate_blocks", None).await, 1);
		assert_eq!(count(&provider.db_ctx.pool, "transaction_hashes", None).await, 0);
		assert_eq!(count(&provider.db_ctx.pool, "logs", None).await, 0);

		// Second insert for the same block is a no-op, even with receipts.
		let receipts = make_receipts(0, 3, 2);
		provider.insert(&block, &receipts, &ethereum_hash).await?;
		assert_eq!(count(&provider.db_ctx.pool, "eth_to_substrate_blocks", None).await, 1);
		assert_eq!(count(&provider.db_ctx.pool, "transaction_hashes", None).await, 0);
		assert_eq!(count(&provider.db_ctx.pool, "logs", None).await, 0);

		Ok(())
	}

	#[sqlx::test]
	async fn test_bulk_delete(pool: SqlitePool) -> anyhow::Result<()> {
		// Use the smallest valid limit to force chunked INSERTs and DELETEs.
		let db_ctx = DbContext::new(pool, DbContext::LOG_COLUMNS);
		let provider = mock_provider().with_db_ctx(db_ctx).with_keep_latest(None);

		let n_blocks = 25;
		let n_tx_per_block = 5;
		let n_logs_per_receipt = 3;
		let mut block_mappings = Vec::new();

		for i in 0..n_blocks {
			let block = MockBlockInfo { hash: make_hash(i, 0xAA), number: i as u64 + 1 };
			let ethereum_hash = make_hash(i, 0xBB);
			let receipts = make_receipts(i * n_tx_per_block, n_tx_per_block, n_logs_per_receipt);
			provider.insert_into_db(&block, &receipts, &ethereum_hash).await?;
			block_mappings.push(BlockHashMap::new(block.hash, ethereum_hash));
		}

		assert_eq!(
			count(&provider.db_ctx.pool, "transaction_hashes", None).await,
			n_blocks * n_tx_per_block
		);
		assert_eq!(
			count(&provider.db_ctx.pool, "logs", None).await,
			n_blocks * n_tx_per_block * n_logs_per_receipt
		);
		assert_eq!(count(&provider.db_ctx.pool, "eth_to_substrate_blocks", None).await, n_blocks);

		provider.remove(&block_mappings).await?;

		assert_eq!(count(&provider.db_ctx.pool, "transaction_hashes", None).await, 0);
		assert_eq!(count(&provider.db_ctx.pool, "logs", None).await, 0);
		assert_eq!(count(&provider.db_ctx.pool, "eth_to_substrate_blocks", None).await, 0);

		Ok(())
	}

	#[sqlx::test]
	async fn test_get_processed_eth_block_hash(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let block = MockBlockInfo { hash: H256::from([0xAA; 32]), number: 10 };
		let ethereum_hash = H256::from([0xBB; 32]);
		let receipts = vec![(TransactionSigned::default(), ReceiptInfo::default())];

		// Not cached yet
		assert!(provider.get_processed_eth_block_hash(10, block.hash).await.is_none());

		// Insert also populates the in-memory cache
		provider.insert(&block, &receipts, &ethereum_hash).await?;
		assert_eq!(
			provider.get_processed_eth_block_hash(10, block.hash).await,
			Some(ethereum_hash)
		);

		// Wrong hash for same block number
		assert!(
			provider
				.get_processed_eth_block_hash(10, H256::from([0xCC; 32]))
				.await
				.is_none()
		);

		// Wrong block number
		assert!(provider.get_processed_eth_block_hash(11, block.hash).await.is_none());

		Ok(())
	}

	#[sqlx::test]
	async fn test_logs_by_block_number(pool: SqlitePool) -> anyhow::Result<()> {
		let provider = setup_sqlite_provider(pool).await;
		let substrate_hash = H256::from([0xAA; 32]);
		let tx_hash = H256::from([0xBB; 32]);
		let block = MockBlockInfo { hash: substrate_hash, number: 42 };
		let ethereum_hash = H256::from([0xCC; 32]);

		let log0 = Log {
			block_hash: ethereum_hash,
			block_number: U256::from(42),
			transaction_hash: tx_hash,
			log_index: U256::from(0),
			address: H160::from([0x01; 20]),
			..Default::default()
		};
		let log1 = Log {
			block_hash: ethereum_hash,
			block_number: U256::from(42),
			transaction_hash: tx_hash,
			log_index: U256::from(1),
			address: H160::from([0x02; 20]),
			..Default::default()
		};

		let receipts = vec![(
			TransactionSigned::default(),
			ReceiptInfo {
				transaction_hash: tx_hash,
				block_hash: ethereum_hash,
				logs: vec![log0.clone(), log1.clone()],
				..Default::default()
			},
		)];

		// No logs before insert
		let logs = provider.logs_by_block_number(42, ethereum_hash).await?;
		assert!(logs.is_empty());

		provider.insert(&block, &receipts, &ethereum_hash).await?;

		// Logs returned in log_index order
		let logs = provider.logs_by_block_number(42, ethereum_hash).await?;
		assert_eq!(logs.len(), 2);
		assert_eq!(logs[0].address, log0.address);
		assert_eq!(logs[1].address, log1.address);
		assert_eq!(logs[0].log_index, U256::from(0));
		assert_eq!(logs[1].log_index, U256::from(1));

		// Different block number returns empty
		let logs = provider.logs_by_block_number(43, ethereum_hash).await?;
		assert!(logs.is_empty());

		// Wrong ethereum hash returns empty
		let logs = provider.logs_by_block_number(42, H256::from([0xDD; 32])).await?;
		assert!(logs.is_empty());

		Ok(())
	}
}
