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

//! Transaction memory pool, container for watched and unwatched transactions.
//! Acts as a buffer which collect transactions before importing them to the views. Following are
//! the crucial use cases when it is needed:
//! - empty pool (no views yet)
//! - potential races between creation of view and submitting transaction (w/o intermediary buffer
//!   some transactions could be lost)
//! - the transaction can be invalid on some forks (and thus the associated views may not contain
//!   it), while on other forks tx can be valid. Depending on which view is chosen to be cloned,
//!   such transaction could not be present in the newly created view.
//!
//! Sync methods (with `_sync` suffix) are also exposed, and it should be safe to call them from
//! sync or non-tokio contenxt. These methods are required for implementing some non-async methods.
//! See <https://github.com/paritytech/polkadot-sdk/issues/8912> for some more information. The implementation of the
//! bridging is based on passing messages from sync context to tokio thread.

use futures::{future::join_all, FutureExt};
use itertools::Itertools;
use parking_lot::RwLock;
use sc_transaction_pool_api::{error::IntoMetricsLabel, TransactionPriority, TransactionSource};
use sp_blockchain::HashAndNumber;
use sp_runtime::{
	traits::Block as BlockT,
	transaction_validity::{InvalidTransaction, TransactionValidityError},
};
use std::{
	collections::{HashMap, HashSet},
	future::Future,
	pin::Pin,
	sync::{
		atomic::{self, AtomicU64},
		mpsc::{
			channel as sync_bridge_channel, Receiver as SyncBridgeReceiver,
			Sender as SyncBridgeSender,
		},
		Arc,
	},
	time::Instant,
};
use tracing::{debug, trace};

use crate::{
	common::tracing_log_xt::log_xt_trace,
	graph::{self, base_pool::TimedTransactionSource, ExtrinsicFor, ExtrinsicHash},
	ValidateTransactionPriority, LOG_TARGET,
};

use super::{
	metrics::MetricsLink as PrometheusMetrics, multi_view_listener::MultiViewListener,
	view_store::ViewStore,
};

mod tx_mem_pool_map;

/// The minimum interval between single transaction revalidations. Given in blocks.
pub(crate) const TXMEMPOOL_REVALIDATION_PERIOD: u64 = 10;

/// The number of transactions revalidated in single revalidation batch.
pub(crate) const TXMEMPOOL_MAX_REVALIDATION_BATCH_SIZE: usize = 1000;

const SYNC_BRIDGE_EXPECT: &str = "The mempool blocking task shall not be terminated. qed.";

#[derive(strum::Display)]
#[strum(serialize_all = "snake_case")]
/// Provides a type safe way of determining the category and reason of why
/// a transaction is marked as invalid, useful to extract labels in the context of
/// `mempool_revalidation_invalid_txs` metric.
pub(super) enum InvalidTxReason {
	/// Coresponds to invalid validity.
	Invalid(String),
	/// Corresponds to unknown validity.
	Unknown(String),
	/// Corresponds to a transaction which is marked as invalid because it is part of a subtree of
	/// a transaction with invalid or unknown validities.
	Subtree(String),
	/// Corresponds to a transaction for which a validity couldn't be determined due to a failure
	/// during the validation, or because of a missing runtime prerequisite.
	ValidationFailed(String),
}

impl InvalidTxReason {
	pub(super) fn reason(&self) -> &String {
		match self {
			InvalidTxReason::Invalid(inner) |
			InvalidTxReason::Unknown(inner) |
			InvalidTxReason::Subtree(inner) |
			InvalidTxReason::ValidationFailed(inner) => inner,
		}
	}

	pub(super) fn category(&self) -> String {
		self.to_string()
	}
}

/// Represents the transaction in the intermediary buffer.
pub(crate) struct TxInMemPool<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: graph::ChainApi<Block = Block> + 'static,
{
	/// Is the progress of transaction watched.
	///
	/// Indicates if transaction was sent with `submit_and_watch`. Serves only stats/testing
	/// purposes.
	watched: bool,
	/// Extrinsic actual body.
	tx: ExtrinsicFor<ChainApi>,
	/// Size of the extrinsics actual body.
	bytes: usize,
	/// Transaction source.
	source: TimedTransactionSource,
	/// When the transaction was revalidated, used to periodically revalidate the mem pool buffer.
	validated_at: AtomicU64,
	/// Priority of transaction at some block. It is assumed it will not be changed often. None if
	/// not known.
	priority: RwLock<Option<TransactionPriority>>,
	/// Indicates that this transaction has lost all referencing views and should be unbanned
	/// in view rotators once mempool revalidation confirms it is still valid at the finalized
	/// block.
	needs_unban: atomic::AtomicBool,
}

impl<ChainApi, Block> TxInMemPool<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: graph::ChainApi<Block = Block> + 'static,
{
	/// Shall the progress of transaction be watched.
	///
	/// Was transaction sent with `submit_and_watch`.
	pub(crate) fn is_watched(&self) -> bool {
		self.watched
	}

	/// Creates a new instance of wrapper for unwatched transaction.
	fn new_unwatched(
		source: TransactionSource,
		tx: ExtrinsicFor<ChainApi>,
		bytes: usize,
		validated_at: u64,
	) -> Self {
		Self::new(false, source, tx, bytes, validated_at)
	}

	/// Creates a new instance of wrapper for watched transaction.
	fn new_watched(
		source: TransactionSource,
		tx: ExtrinsicFor<ChainApi>,
		bytes: usize,
		validated_at: u64,
	) -> Self {
		Self::new(true, source, tx, bytes, validated_at)
	}

	/// Creates a new instance of wrapper for a transaction with no priority.
	fn new(
		watched: bool,
		source: TransactionSource,
		tx: ExtrinsicFor<ChainApi>,
		bytes: usize,
		validated_at: u64,
	) -> Self {
		Self::new_with_optional_priority(watched, source, tx, bytes, None, validated_at)
	}

	/// Creates a new instance of wrapper for a transaction with given priority.
	fn new_with_priority(
		watched: bool,
		source: TransactionSource,
		tx: ExtrinsicFor<ChainApi>,
		bytes: usize,
		priority: TransactionPriority,
		validated_at: u64,
	) -> Self {
		Self::new_with_optional_priority(watched, source, tx, bytes, Some(priority), validated_at)
	}

	/// Creates a new instance of wrapper for a transaction with optional priority.
	fn new_with_optional_priority(
		watched: bool,
		source: TransactionSource,
		tx: ExtrinsicFor<ChainApi>,
		bytes: usize,
		priority: Option<TransactionPriority>,
		validated_at: u64,
	) -> Self {
		Self {
			watched,
			tx,
			source: TimedTransactionSource::from_transaction_source(source, true),
			validated_at: AtomicU64::new(validated_at),
			bytes,
			priority: priority.into(),
			needs_unban: atomic::AtomicBool::new(false),
		}
	}

	/// Provides a clone of actual transaction body.
	///
	/// Operation is cheap, as the body is `Arc`.
	pub(crate) fn tx(&self) -> ExtrinsicFor<ChainApi> {
		self.tx.clone()
	}

	/// Returns the source of the transaction.
	pub(crate) fn source(&self) -> TimedTransactionSource {
		self.source.clone()
	}

	/// Returns the priority of the transaction.
	pub(crate) fn priority(&self) -> Option<TransactionPriority> {
		*self.priority.read()
	}

	/// Returns `true` if this transaction needs to be unbanned after mempool revalidation.
	pub(crate) fn needs_unban(&self) -> bool {
		self.needs_unban.load(atomic::Ordering::Relaxed)
	}

	/// Marks this transaction as needing to be unbanned after the next successful mempool
	/// revalidation.
	pub(crate) fn set_needs_unban(&self) {
		self.needs_unban.store(true, atomic::Ordering::Relaxed);
	}

	/// Clears the needs-unban flag.
	pub(crate) fn clear_needs_unban(&self) {
		self.needs_unban.store(false, atomic::Ordering::Relaxed);
	}
}

impl<ChainApi, Block> std::fmt::Debug for TxInMemPool<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: graph::ChainApi<Block = Block> + 'static,
{
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("TxInMemPool")
			.field("watched", &self.watched)
			.field("tx", &"...")
			.field("bytes", &self.bytes)
			.field("source", &self.source)
			.field("validated_at", &self.validated_at)
			.field("priority", &self.priority)
			.field("needs_unban", &self.needs_unban)
			.finish()
	}
}

impl<ChainApi, Block> std::cmp::PartialEq for TxInMemPool<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: graph::ChainApi<Block = Block> + 'static,
{
	fn eq(&self, other: &Self) -> bool {
		self.watched == other.watched &&
			self.tx == other.tx &&
			self.bytes == other.bytes &&
			self.source == other.source &&
			*self.priority.read() == *other.priority.read() &&
			self.validated_at.load(atomic::Ordering::Relaxed) ==
				other.validated_at.load(atomic::Ordering::Relaxed) &&
			self.needs_unban.load(atomic::Ordering::Relaxed) ==
				other.needs_unban.load(atomic::Ordering::Relaxed)
	}
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct MempoolTxPriority(pub Option<TransactionPriority>);

impl Ord for MempoolTxPriority {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		match (&self.0, &other.0) {
			(Some(a), Some(b)) => a.cmp(b),
			(Some(_), None) => std::cmp::Ordering::Less,
			(None, Some(_)) => std::cmp::Ordering::Greater,
			(None, None) => std::cmp::Ordering::Equal,
		}
	}
}
impl From<Option<TransactionPriority>> for MempoolTxPriority {
	fn from(value: Option<TransactionPriority>) -> Self {
		MempoolTxPriority(value)
	}
}

impl PartialOrd for MempoolTxPriority {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl<ChainApi, Block> tx_mem_pool_map::Size for Arc<TxInMemPool<ChainApi, Block>>
where
	Block: BlockT,
	ChainApi: graph::ChainApi<Block = Block> + 'static,
{
	fn size(&self) -> usize {
		self.bytes
	}
}

impl<ChainApi, Block> tx_mem_pool_map::PriorityAndTimestamp for Arc<TxInMemPool<ChainApi, Block>>
where
	Block: BlockT,
	ChainApi: graph::ChainApi<Block = Block> + 'static,
{
	type Priority = MempoolTxPriority;
	type Timestamp = Option<Instant>;

	fn priority(&self) -> Self::Priority {
		TxInMemPool::priority(self).into()
	}

	fn timestamp(&self) -> Self::Timestamp {
		self.source().timestamp
	}
}

type InternalTxMemPoolMap<ChainApi, Block> = tx_mem_pool_map::SizeTrackedStore<
	ExtrinsicHash<ChainApi>,
	tx_mem_pool_map::PriorityKey<MempoolTxPriority, Option<Instant>>,
	Arc<TxInMemPool<ChainApi, Block>>,
>;

/// Internal (blocking) task for bridging sync and async code.
///
/// Should be polled in blocking task.
pub type TxMemPoolBlockingTask = Pin<Box<dyn Future<Output = ()> + Send>>;

/// An intermediary transactions buffer.
///
/// Keeps all the transaction which are potentially valid. Transactions that were finalized or
/// transactions that are invalid at finalized blocks are removed, either while handling the
/// `Finalized` event, or during revalidation process.
///
/// All transactions from  a`TxMemPool` are submitted to the newly created views.
///
/// All newly submitted transactions goes into the `TxMemPool`.
pub(super) struct TxMemPool<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: graph::ChainApi<Block = Block> + 'static,
{
	/// A shared API instance necessary for blockchain related operations.
	api: Arc<ChainApi>,

	/// A shared instance of the `MultiViewListener`.
	///
	/// Provides a side-channel allowing to send per-transaction state changes notification.
	listener: Arc<MultiViewListener<ChainApi>>,

	/// Channel used to send the requests from the sync code.
	sync_channel: SyncBridgeSender<TxMemPoolSyncRequest<ChainApi, Block>>,

	///  A map that stores the transactions currently in the memory pool.
	///
	///  The key is the hash of the transaction, and the value is a wrapper
	///  structure, which contains the mempool specific details of the transaction.
	transactions: InternalTxMemPoolMap<ChainApi, Block>,

	/// Prometheus's metrics endpoint.
	metrics: PrometheusMetrics,

	/// Indicates the maximum number of transactions that can be maintained in the memory pool.
	max_transactions_count: usize,

	/// Maximal size of encodings of all transactions in the memory pool.
	max_transactions_total_bytes: usize,
}

/// Helper structure to encapsulate a result of [`TxMemPool::try_insert`].
#[derive(Debug)]
pub(super) struct InsertionInfo<Hash> {
	pub(super) hash: Hash,
	pub(super) source: TimedTransactionSource,
	pub(super) removed: Vec<Hash>,
}

impl<Hash> InsertionInfo<Hash> {
	fn new(hash: Hash, source: TimedTransactionSource) -> Self {
		Self::new_with_removed(hash, source, Default::default())
	}
	fn new_with_removed(hash: Hash, source: TimedTransactionSource, removed: Vec<Hash>) -> Self {
		Self { hash, source, removed }
	}
}

impl<ChainApi, Block> TxMemPool<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: graph::ChainApi<Block = Block> + 'static,
	<Block as BlockT>::Hash: Unpin,
{
	/// Creates a new `TxMemPool` instance with the given API, listener, metrics,
	/// and max transaction count.
	pub(super) fn new(
		api: Arc<ChainApi>,
		listener: Arc<MultiViewListener<ChainApi>>,
		metrics: PrometheusMetrics,
		max_transactions_count: usize,
		max_transactions_total_bytes: usize,
	) -> (Self, TxMemPoolBlockingTask) {
		let (sync_channel, rx) = sync_bridge_channel();
		let task = Self::sync_bridge_task(rx);
		(
			Self {
				api,
				listener,
				sync_channel,
				transactions: Default::default(),
				metrics,
				max_transactions_count,
				max_transactions_total_bytes,
			},
			task.boxed(),
		)
	}

	/// Creates a new `TxMemPool` instance for testing purposes.
	#[cfg(test)]
	fn new_test(
		api: Arc<ChainApi>,
		max_transactions_count: usize,
		max_transactions_total_bytes: usize,
	) -> Self {
		let (sync_channel, rx) = sync_bridge_channel();
		tokio::task::spawn_blocking(move || Self::sync_bridge_task(rx));
		Self {
			api,
			listener: Arc::from(MultiViewListener::new_with_worker(Default::default()).0),
			transactions: Default::default(),
			metrics: Default::default(),
			sync_channel,
			max_transactions_count,
			max_transactions_total_bytes,
		}
	}

	/// Retrieves a transaction by its hash if it exists in the memory pool.
	pub(super) async fn get_by_hash(
		&self,
		hash: ExtrinsicHash<ChainApi>,
	) -> Option<Arc<TxInMemPool<ChainApi, Block>>> {
		self.transactions.read().await.get(&hash).map(Clone::clone)
	}

	/// Returns a tuple with the count of unwatched and watched transactions in the memory pool.
	pub async fn unwatched_and_watched_count(&self) -> (usize, usize) {
		let transactions = self.transactions.read().await;
		let watched_count = transactions.values().filter(|t| t.is_watched()).count();
		(transactions.len() - watched_count, watched_count)
	}

	/// Returns a total number of transactions kept within mempool.
	pub fn len(&self) -> usize {
		self.transactions.len()
	}

	/// Returns the number of bytes used by all extrinsics in the the pool.
	#[cfg(test)]
	pub fn bytes(&self) -> usize {
		return self.transactions.bytes();
	}

	/// Returns true if provided values would exceed defined limits.
	fn is_limit_exceeded(&self, length: usize, current_total_bytes: usize) -> bool {
		length > self.max_transactions_count ||
			current_total_bytes > self.max_transactions_total_bytes
	}

	/// Attempts to insert a transaction into the memory pool, ensuring it does not
	/// exceed the maximum allowed transaction count.
	async fn try_insert(
		&self,
		tx_hash: ExtrinsicHash<ChainApi>,
		tx: TxInMemPool<ChainApi, Block>,
	) -> Result<InsertionInfo<ExtrinsicHash<ChainApi>>, sc_transaction_pool_api::error::Error> {
		let mut transactions = self.transactions.write().await;

		let bytes = self.transactions.bytes();

		let result = match (
			self.is_limit_exceeded(transactions.len() + 1, bytes + tx.bytes),
			transactions.contains_key(&tx_hash),
		) {
			(false, false) => {
				let source = tx.source();
				transactions.insert(tx_hash, Arc::from(tx));
				Ok(InsertionInfo::new(tx_hash, source))
			},
			(_, true) => {
				Err(sc_transaction_pool_api::error::Error::AlreadyImported(Box::new(tx_hash)))
			},
			(true, _) => Err(sc_transaction_pool_api::error::Error::ImmediatelyDropped),
		};
		trace!(
			target: LOG_TARGET,
			?tx_hash,
			result_hash = ?result.as_ref().map(|r| r.hash),
			"mempool::try_insert"
		);
		result
	}

	/// Attempts to insert a new transaction in the memory pool and drop some worse existing
	/// transactions.
	///
	/// A "worse" transaction means transaction with lower priority, or older transaction with the
	/// same prio.
	///
	/// This operation will not overflow the limit of the mempool. It means that cumulative
	/// size of removed transactions will be equal (or greated) then size of newly inserted
	/// transaction.
	///
	/// Returns a `Result` containing `InsertionInfo` if the new transaction is successfully
	/// inserted; otherwise, returns an appropriate error indicating the failure.
	pub(super) async fn try_insert_with_replacement(
		&self,
		new_tx: ExtrinsicFor<ChainApi>,
		priority: TransactionPriority,
		source: TransactionSource,
		validated_at: u64,
		watched: bool,
	) -> Result<InsertionInfo<ExtrinsicHash<ChainApi>>, sc_transaction_pool_api::error::Error> {
		let (hash, length) = self.api.hash_and_length(&new_tx);
		let new_tx =
			TxInMemPool::new_with_priority(watched, source, new_tx, length, priority, validated_at);
		if new_tx.bytes > self.max_transactions_total_bytes {
			return Err(sc_transaction_pool_api::error::Error::ImmediatelyDropped);
		}

		let mut transactions = self.transactions.write().await;

		if transactions.contains_key(&hash) {
			return Err(sc_transaction_pool_api::error::Error::AlreadyImported(Box::new(hash)));
		}

		// When pushing higher prio transaction, we need to find a number of lower prio txs, such
		// that the sum of their bytes is ge then size of new tx. Otherwise we could overflow size
		// limits. Naive way to do it - rev-sort by priority and eat the tail.

		// reverse (oldest, lowest prio last)
		let source = new_tx.source();
		let new_tx = Arc::new(new_tx);
		let insertion_result = transactions.try_insert_with_replacement(
			self.max_transactions_total_bytes,
			hash,
			new_tx,
		);
		debug_assert!(!self.is_limit_exceeded(transactions.len(), self.transactions.bytes()));
		match insertion_result {
			None => Err(sc_transaction_pool_api::error::Error::ImmediatelyDropped),
			Some(to_be_removed) => Ok(InsertionInfo::new_with_removed(hash, source, to_be_removed)),
		}
	}

	/// Adds a new unwatched transactions to the internal buffer not exceeding the limit.
	///
	/// Returns the vector of results for each transaction, the order corresponds to the input
	/// vector.
	pub(super) async fn extend_unwatched(
		&self,
		source: TransactionSource,
		validated_at: u64,
		xts: &[ExtrinsicFor<ChainApi>],
	) -> Vec<Result<InsertionInfo<ExtrinsicHash<ChainApi>>, sc_transaction_pool_api::error::Error>>
	{
		let insert_futures = xts.into_iter().map(|xt| {
			let api = self.api.clone();
			let xt = xt.clone();
			async move {
				let (hash, length) = api.hash_and_length(&xt);
				self.try_insert(hash, TxInMemPool::new_unwatched(source, xt, length, validated_at))
					.await
			}
		});

		join_all(insert_futures).await
	}

	/// Adds a new watched transaction to the memory pool if it does not exceed the maximum allowed
	/// transaction count.
	pub(super) async fn push_watched(
		&self,
		source: TransactionSource,
		validated_at: u64,
		xt: ExtrinsicFor<ChainApi>,
	) -> Result<InsertionInfo<ExtrinsicHash<ChainApi>>, sc_transaction_pool_api::error::Error> {
		let (hash, length) = self.api.hash_and_length(&xt);
		self.try_insert(hash, TxInMemPool::new_watched(source, xt.clone(), length, validated_at))
			.await
	}

	/// Provides read-only access to all transctions via an iterator.
	///
	/// This function allows to iterate over all stored transaction without cloning.
	/// The provided closure receives an iterator over references to keys and values.
	///
	/// Note: Typically some filtering should be applied and required items can be cloned and return
	/// outside the closure if required. Transacaction are `Arc` so clone shall be cheap.
	pub(super) async fn with_transactions<F, R>(&self, f: F) -> R
	where
		F: Fn(
			std::collections::hash_map::Iter<
				ExtrinsicHash<ChainApi>,
				Arc<TxInMemPool<ChainApi, Block>>,
			>,
		) -> R,
	{
		self.transactions.read().await.with_items(f)
	}

	/// Removes transactions with given hashes from the memory pool.
	pub(super) async fn remove_transactions(&self, tx_hashes: &[ExtrinsicHash<ChainApi>]) {
		log_xt_trace!(target: LOG_TARGET, tx_hashes, "mempool::remove_transaction");
		let mut transactions = self.transactions.write().await;
		for tx_hash in tx_hashes {
			transactions.remove(tx_hash);
		}
	}

	/// Revalidates a batch of transactions against the provided finalized block.
	///
	/// Returns a vector of invalid transaction hashes.
	async fn revalidate_inner(
		&self,
		view_store: Arc<ViewStore<ChainApi, Block>>,
		finalized_block: HashAndNumber<Block>,
	) -> HashSet<ExtrinsicHash<ChainApi>> {
		trace!(
			target: LOG_TARGET,
			?finalized_block,
			"mempool::revalidate_inner"
		);
		let start = Instant::now();

		let (total_count, to_be_validated) = {
			(
				self.transactions.len(),
				self.with_transactions(|iter| {
					let finalized_block_number = finalized_block.number.into().as_u64();
					// Prioritize transactions that need unbanning — they should be
					// revalidated first so stale bans can be cleared promptly.
					iter.filter(|(_, xt)| {
						xt.needs_unban() ||
							xt.validated_at.load(atomic::Ordering::Relaxed) +
								TXMEMPOOL_REVALIDATION_PERIOD <
								finalized_block_number
					})
					.sorted_by_key(|(_, tx)| {
						(
							// needs_unban txs come first (false < true, so negate)
							!tx.needs_unban(),
							tx.validated_at.load(atomic::Ordering::Relaxed),
						)
					})
					.take(TXMEMPOOL_MAX_REVALIDATION_BATCH_SIZE)
					.map(|(k, v)| (*k, v.clone()))
					.collect::<Vec<_>>()
				})
				.await,
			)
		};

		// Build a lookup from the batch so we can check needs_unban after validation
		// without async mempool access.
		let tx_lookup: HashMap<_, _> =
			to_be_validated.iter().map(|(k, v)| (*k, v.clone())).collect();

		let validations_futures = to_be_validated.into_iter().map(|(xt_hash, xt)| {
			self.api
				.validate_transaction(
					finalized_block.hash,
					xt.source.clone().into(),
					xt.tx(),
					ValidateTransactionPriority::Maintained,
				)
				.map(move |validation_result| {
					xt.validated_at
						.store(finalized_block.number.into().as_u64(), atomic::Ordering::Relaxed);
					(xt_hash, validation_result)
				})
		});
		let validation_results = futures::future::join_all(validations_futures).await;
		let validated_count = validation_results.len();

		let duration = start.elapsed();

		let invalid_hashes = validation_results
			.into_iter()
			.filter_map(|(tx_hash, validation_result)| match validation_result {
				Ok(Ok(_) | Err(TransactionValidityError::Invalid(InvalidTransaction::Future))) => {
					// Transaction is valid at finalized block. If it was marked for unbanning,
					// clear the ban in all active views so it can be picked up by
					// `update_view_with_mempool` on the next view creation.
					if let Some(tx) = tx_lookup.get(&tx_hash) {
						if tx.needs_unban() {
							trace!(
								target: LOG_TARGET,
								?tx_hash,
								"mempool::revalidate_inner: unbanning viewless transaction"
							);
							view_store.unban_transaction(&tx_hash);
							tx.clear_needs_unban();
						}
					}
					None
				},
				Err(ref error) => {
					trace!(
						target: LOG_TARGET,
						?tx_hash,
						?validation_result,
						"mempool::revalidate_inner error during revalidation"
					);
					Some((tx_hash, InvalidTxReason::ValidationFailed(error.label())))
				},
				Ok(Err(TransactionValidityError::Unknown(error))) => {
					trace!(
						target: LOG_TARGET,
						?tx_hash,
						?validation_result,
						"mempool::revalidate_inner cannot determine transaction validity"
					);
					Some((tx_hash, InvalidTxReason::Unknown(error.as_ref().to_string())))
				},
				Ok(Err(TransactionValidityError::Invalid(error))) => {
					trace!(
						target: LOG_TARGET,
						?tx_hash,
						?validation_result,
						"mempool::revalidate_inner transaction is invalid"
					);
					Some((tx_hash, InvalidTxReason::Invalid(error.as_ref().to_string())))
				},
			})
			.collect::<Vec<_>>();

		let mut invalid_hashes_subtrees = Vec::new();
		// Include also subtree txs.
		for (tx, reason) in &invalid_hashes {
			let txs_in_subtree = view_store
				.remove_transaction_subtree(*tx, |_, _| {})
				.into_iter()
				.map(|tx| (tx.hash, InvalidTxReason::Subtree(reason.to_string())));
			invalid_hashes_subtrees.extend(txs_in_subtree);
		}

		let revalidated_invalid_hashes_len = invalid_hashes.len();
		let invalid_hashes = invalid_hashes
			.into_iter()
			.chain(invalid_hashes_subtrees)
			.map(|(tx, reason)| {
				self.metrics
					.report(|metrics| metrics.mempool_revalidation_invalid_txs.observe(&reason, 1));
				tx
			})
			.collect::<HashSet<_>>();

		debug!(
			target: LOG_TARGET,
			?finalized_block,
			validated_count,
			total_count,
			invalid_hashes_subtrees_len = invalid_hashes.len(),
			revalidated_invalid_hashes_len,
			?duration,
			"mempool::revalidate_inner"
		);

		invalid_hashes
	}

	/// Removes the finalized transactions from the memory pool, using a provided list of hashes.
	pub(super) async fn purge_finalized_transactions(
		&self,
		finalized_xts: &Vec<ExtrinsicHash<ChainApi>>,
	) {
		debug!(
			target: LOG_TARGET,
			count = finalized_xts.len(),
			"purge_finalized_transactions"
		);
		log_xt_trace!(target: LOG_TARGET, finalized_xts, "purged finalized transactions");
		let mut transactions = self.transactions.write().await;
		finalized_xts.iter().for_each(|t| {
			transactions.remove(t);
		});
	}

	/// Revalidates transactions in the memory pool against a given finalized block and removes
	/// invalid ones.
	pub(super) async fn revalidate(
		&self,
		view_store: Arc<ViewStore<ChainApi, Block>>,
		finalized_block: HashAndNumber<Block>,
	) {
		let invalid_hashes_subtrees =
			self.revalidate_inner(view_store.clone(), finalized_block.clone()).await;
		{
			let mut transactions = self.transactions.write().await;
			invalid_hashes_subtrees.iter().for_each(|tx_hash| {
				transactions.remove(&tx_hash);
			});
		};

		// note: here the consistency is assumed: it is expected that transaction will be
		// actually removed from the listener with Invalid event. This means assumption that no view
		// is referencing tx as ready.
		let invalid_hashes_subtrees = invalid_hashes_subtrees.into_iter().collect::<Vec<_>>();
		self.listener.transactions_invalidated(invalid_hashes_subtrees.as_slice());
		view_store
			.import_notification_sink
			.clean_notified_items(invalid_hashes_subtrees.as_slice());
		view_store
			.dropped_stream_controller
			.remove_transactions(invalid_hashes_subtrees);

		trace!(
			target: LOG_TARGET,
			?finalized_block,
			"mempool::revalidate"
		);
	}

	/// Updates the priority of transaction stored in mempool using provided priority.
	pub(super) async fn update_transaction_priority(
		&self,
		hash: ExtrinsicHash<ChainApi>,
		prio: Option<TransactionPriority>,
	) {
		if let Some(priority) = prio {
			let mut transactions = self.transactions.write().await;

			transactions.update_item(&hash, |t| {
				*t.priority.write() = Some(priority);
			});
		}
	}

	/// Counts the number of transactions in the provided iterator of hashes
	/// that are not known to the pool.
	pub(super) async fn count_unknown_transactions<'a>(
		&self,
		hashes: impl Iterator<Item = &'a ExtrinsicHash<ChainApi>>,
	) -> usize {
		let transactions = self.transactions.read().await;
		hashes.filter(|tx_hash| !transactions.contains_key(tx_hash)).count()
	}
}

/// Convenient return type of extend_unwatched
type ExtendUnwatchedResult<ChainApi> =
	Vec<Result<InsertionInfo<ExtrinsicHash<ChainApi>>, sc_transaction_pool_api::error::Error>>;

/// Convenient return type of try_insert_with_replacement
type TryInsertWithReplacementResult<ChainApi> =
	Result<InsertionInfo<ExtrinsicHash<ChainApi>>, sc_transaction_pool_api::error::Error>;

/// Helper enum defining what requests can be made from sync code.
enum TxMemPoolSyncRequest<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: graph::ChainApi<Block = Block> + 'static,
{
	RemoveTransactions(
		Arc<TxMemPool<ChainApi, Block>>,
		Vec<ExtrinsicHash<ChainApi>>,
		SyncBridgeSender<()>,
	),
	ExtendUnwatched(
		Arc<TxMemPool<ChainApi, Block>>,
		TransactionSource,
		u64,
		Vec<ExtrinsicFor<ChainApi>>,
		SyncBridgeSender<ExtendUnwatchedResult<ChainApi>>,
	),
	UpdateTransactionPriority(
		Arc<TxMemPool<ChainApi, Block>>,
		ExtrinsicHash<ChainApi>,
		Option<TransactionPriority>,
		SyncBridgeSender<()>,
	),
	TryInsertWithReplacement(
		Arc<TxMemPool<ChainApi, Block>>,
		ExtrinsicFor<ChainApi>,
		TransactionPriority,
		TransactionSource,
		u64,
		bool,
		SyncBridgeSender<TryInsertWithReplacementResult<ChainApi>>,
	),
}

impl<ChainApi, Block> TxMemPoolSyncRequest<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: graph::ChainApi<Block = Block> + 'static,
{
	fn remove_transactions(
		mempool: Arc<TxMemPool<ChainApi, Block>>,
		hashes: Vec<ExtrinsicHash<ChainApi>>,
	) -> (SyncBridgeReceiver<()>, Self) {
		let (tx, rx) = sync_bridge_channel();
		(rx, Self::RemoveTransactions(mempool, hashes, tx))
	}

	fn extend_unwatched(
		mempool: Arc<TxMemPool<ChainApi, Block>>,
		source: TransactionSource,
		validated_at: u64,
		xts: Vec<ExtrinsicFor<ChainApi>>,
	) -> (SyncBridgeReceiver<ExtendUnwatchedResult<ChainApi>>, Self) {
		let (tx, rx) = sync_bridge_channel();
		(rx, Self::ExtendUnwatched(mempool, source, validated_at, xts, tx))
	}

	fn update_transaction_priority(
		mempool: Arc<TxMemPool<ChainApi, Block>>,
		hash: ExtrinsicHash<ChainApi>,
		prio: Option<TransactionPriority>,
	) -> (SyncBridgeReceiver<()>, Self) {
		let (tx, rx) = sync_bridge_channel();
		(rx, Self::UpdateTransactionPriority(mempool, hash, prio, tx))
	}

	fn try_insert_with_replacement(
		mempool: Arc<TxMemPool<ChainApi, Block>>,
		new_tx: ExtrinsicFor<ChainApi>,
		priority: TransactionPriority,
		source: TransactionSource,
		validated_at: u64,
		watched: bool,
	) -> (SyncBridgeReceiver<TryInsertWithReplacementResult<ChainApi>>, Self) {
		let (tx, rx) = sync_bridge_channel();
		(
			rx,
			Self::TryInsertWithReplacement(
				mempool,
				new_tx,
				priority,
				source,
				validated_at,
				watched,
				tx,
			),
		)
	}
}

impl<ChainApi, Block> TxMemPool<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: graph::ChainApi<Block = Block> + 'static,
	<Block as BlockT>::Hash: Unpin,
{
	async fn sync_bridge_task(rx: SyncBridgeReceiver<TxMemPoolSyncRequest<ChainApi, Block>>) {
		for request in rx {
			Self::handle_request(request).await;
		}
	}

	async fn handle_request(request: TxMemPoolSyncRequest<ChainApi, Block>) {
		match request {
			TxMemPoolSyncRequest::RemoveTransactions(mempool, hashes, tx) => {
				mempool.remove_transactions(&hashes).await;
				if let Err(error) = tx.send(()) {
					debug!(target: LOG_TARGET, ?error, "RemoveTransaction: sending response failed");
				}
			},
			TxMemPoolSyncRequest::ExtendUnwatched(mempool, source, validated_at, txs, tx) => {
				let result = mempool.extend_unwatched(source, validated_at, &txs).await;
				if let Err(error) = tx.send(result) {
					debug!(target: LOG_TARGET, ?error, "ExtendUnwatched: sending response failed");
				}
			},
			TxMemPoolSyncRequest::UpdateTransactionPriority(mempool, hash, prio, tx) => {
				let result = mempool.update_transaction_priority(hash, prio).await;
				if let Err(error) = tx.send(result) {
					debug!(target: LOG_TARGET, ?error, "UpdateTransactionPriority2: sending response failed");
				}
			},
			TxMemPoolSyncRequest::TryInsertWithReplacement(
				mempool,
				new_tx,
				priority,
				source,
				validated_at,
				watched,
				tx,
			) => {
				let result = mempool
					.try_insert_with_replacement(new_tx, priority, source, validated_at, watched)
					.await;
				if let Err(error) = tx.send(result) {
					debug!(target: LOG_TARGET, ?error, "TryInsertWithReplacementSync: sending response failed");
				}
			},
		}
	}

	pub(super) fn try_insert_with_replacement_sync(
		self: Arc<Self>,
		new_tx: ExtrinsicFor<ChainApi>,
		priority: TransactionPriority,
		source: TransactionSource,
		validated_at: u64,
		watched: bool,
	) -> Result<InsertionInfo<ExtrinsicHash<ChainApi>>, sc_transaction_pool_api::error::Error> {
		let (response, request) = TxMemPoolSyncRequest::try_insert_with_replacement(
			self.clone(),
			new_tx,
			priority,
			source,
			validated_at,
			watched,
		);
		let _ = self.sync_channel.send(request);
		response.recv().expect(SYNC_BRIDGE_EXPECT)
	}

	pub(super) fn extend_unwatched_sync(
		self: Arc<Self>,
		source: TransactionSource,
		validated_at: u64,
		xts: Vec<ExtrinsicFor<ChainApi>>,
	) -> Vec<Result<InsertionInfo<ExtrinsicHash<ChainApi>>, sc_transaction_pool_api::error::Error>>
	{
		let (response, request) =
			TxMemPoolSyncRequest::extend_unwatched(self.clone(), source, validated_at, xts);
		let _ = self.sync_channel.send(request);
		response.recv().expect(SYNC_BRIDGE_EXPECT)
	}

	pub(super) fn remove_transactions_sync(
		self: Arc<Self>,
		tx_hashes: Vec<ExtrinsicHash<ChainApi>>,
	) {
		let (response, request) =
			TxMemPoolSyncRequest::remove_transactions(self.clone(), tx_hashes);
		let _ = self.sync_channel.send(request);
		response.recv().expect(SYNC_BRIDGE_EXPECT)
	}

	pub(super) fn update_transaction_priority_sync(
		self: Arc<Self>,
		hash: ExtrinsicHash<ChainApi>,
		prio: Option<TransactionPriority>,
	) {
		let (response, request) =
			TxMemPoolSyncRequest::update_transaction_priority(self.clone(), hash, prio);
		let _ = self.sync_channel.send(request);
		response.recv().expect(SYNC_BRIDGE_EXPECT)
	}
}

#[cfg(test)]
mod tx_mem_pool_tests {
	use futures::future::join_all;
	use substrate_test_runtime::{AccountId, Extrinsic, ExtrinsicBuilder, Transfer, H256};
	use substrate_test_runtime_client::Sr25519Keyring::*;

	use crate::{
		common::tests::TestApi, fork_aware_txpool::view_store::ViewStoreSubmitOutcome,
		graph::ChainApi,
	};

	use super::*;

	fn uxt(nonce: u64) -> Extrinsic {
		crate::common::tests::uxt(Transfer {
			from: Alice.into(),
			to: AccountId::from_h256(H256::from_low_u64_be(2)),
			amount: 5,
			nonce,
		})
	}

	#[tokio::test]
	async fn extend_unwatched_obeys_limit() {
		let max = 10;
		let api = Arc::from(TestApi::default());
		let mempool = TxMemPool::new_test(api, max, usize::MAX);

		let xts = (0..max + 1).map(|x| Arc::from(uxt(x as _))).collect::<Vec<_>>();

		let results = mempool.extend_unwatched(TransactionSource::External, 0, &xts).await;
		assert!(results.iter().take(max).all(Result::is_ok));
		assert!(matches!(
			results.into_iter().last().unwrap().unwrap_err(),
			sc_transaction_pool_api::error::Error::ImmediatelyDropped
		));
	}

	#[tokio::test]
	async fn extend_unwatched_detects_already_imported() {
		sp_tracing::try_init_simple();
		let max = 10;
		let api = Arc::from(TestApi::default());
		let mempool = TxMemPool::new_test(api, max, usize::MAX);

		let mut xts = (0..max - 1).map(|x| Arc::from(uxt(x as _))).collect::<Vec<_>>();
		xts.push(xts.iter().last().unwrap().clone());

		let results = mempool.extend_unwatched(TransactionSource::External, 0, &xts).await;
		assert!(results.iter().take(max - 1).all(Result::is_ok));
		assert!(matches!(
			results.into_iter().last().unwrap().unwrap_err(),
			sc_transaction_pool_api::error::Error::AlreadyImported(_)
		));
	}

	#[tokio::test]
	async fn push_obeys_limit() {
		let max = 10;
		let api = Arc::from(TestApi::default());
		let mempool = TxMemPool::new_test(api, max, usize::MAX);

		let xts = (0..max).map(|x| Arc::from(uxt(x as _))).collect::<Vec<_>>();

		let results = mempool.extend_unwatched(TransactionSource::External, 0, &xts).await;
		assert!(results.iter().all(Result::is_ok));

		let xt = Arc::from(uxt(98));
		let result = mempool.push_watched(TransactionSource::External, 0, xt).await;
		assert!(matches!(
			result.unwrap_err(),
			sc_transaction_pool_api::error::Error::ImmediatelyDropped
		));
		let xt = Arc::from(uxt(99));
		let mut result = mempool.extend_unwatched(TransactionSource::External, 0, &[xt]).await;
		assert!(matches!(
			result.pop().unwrap().unwrap_err(),
			sc_transaction_pool_api::error::Error::ImmediatelyDropped
		));
	}

	#[tokio::test]
	async fn push_detects_already_imported() {
		let max = 10;
		let api = Arc::from(TestApi::default());
		let mempool = TxMemPool::new_test(api, 2 * max, usize::MAX);

		let xts = (0..max).map(|x| Arc::from(uxt(x as _))).collect::<Vec<_>>();
		let xt0 = xts.iter().last().unwrap().clone();
		let xt1 = xts.iter().next().unwrap().clone();

		let results = mempool.extend_unwatched(TransactionSource::External, 0, &xts).await;
		assert!(results.iter().all(Result::is_ok));

		let result = mempool.push_watched(TransactionSource::External, 0, xt0).await;
		assert!(matches!(
			result.unwrap_err(),
			sc_transaction_pool_api::error::Error::AlreadyImported(_)
		));
		let mut result = mempool.extend_unwatched(TransactionSource::External, 0, &[xt1]).await;
		assert!(matches!(
			result.pop().unwrap().unwrap_err(),
			sc_transaction_pool_api::error::Error::AlreadyImported(_)
		));
	}

	#[tokio::test]
	async fn count_works() {
		sp_tracing::try_init_simple();
		trace!(target:LOG_TARGET,line=line!(),"xxx");

		let max = 100;
		let api = Arc::from(TestApi::default());
		let mempool = TxMemPool::new_test(api, max, usize::MAX);
		trace!(target:LOG_TARGET,line=line!(),"xxx");

		let xts0 = (0..10).map(|x| Arc::from(uxt(x as _))).collect::<Vec<_>>();
		trace!(target:LOG_TARGET,line=line!(),"xxx");

		let results = mempool.extend_unwatched(TransactionSource::External, 0, &xts0).await;
		trace!(target:LOG_TARGET,line=line!(),"xxx");
		assert!(results.iter().all(Result::is_ok));
		trace!(target:LOG_TARGET,line=line!(),"xxx");

		let xts1 = (0..5).map(|x| Arc::from(uxt(2 * x))).collect::<Vec<_>>();
		trace!(target:LOG_TARGET,line=line!(),"xxx");
		let results = xts1
			.into_iter()
			.map(|t| mempool.push_watched(TransactionSource::External, 0, t));
		trace!(target:LOG_TARGET,line=line!(),"xxx");
		let results = join_all(results).await;
		trace!(target:LOG_TARGET,line=line!(),"xxx");
		assert!(results.iter().all(Result::is_ok));
		assert_eq!(mempool.unwatched_and_watched_count().await, (10, 5));
	}

	/// size of large extrinsic
	const LARGE_XT_SIZE: usize = 1129;

	fn large_uxt(x: usize) -> Extrinsic {
		ExtrinsicBuilder::new_include_data(vec![x as u8; 1024]).build()
	}

	#[tokio::test]
	async fn push_obeys_size_limit() {
		sp_tracing::try_init_simple();
		let max = 10;
		let api = Arc::from(TestApi::default());
		let mempool = TxMemPool::new_test(api.clone(), usize::MAX, max * LARGE_XT_SIZE);

		let xts = (0..max).map(|x| Arc::from(large_uxt(x))).collect::<Vec<_>>();

		let total_xts_bytes = xts.iter().fold(0, |r, x| r + api.hash_and_length(&x).1);

		let results = mempool.extend_unwatched(TransactionSource::External, 0, &xts).await;
		assert!(results.iter().all(Result::is_ok));
		assert_eq!(mempool.bytes(), total_xts_bytes);

		let xt = Arc::from(large_uxt(98));
		let result = mempool.push_watched(TransactionSource::External, 0, xt).await;
		assert!(matches!(
			result.unwrap_err(),
			sc_transaction_pool_api::error::Error::ImmediatelyDropped
		));

		let xt = Arc::from(large_uxt(99));
		let mut result = mempool.extend_unwatched(TransactionSource::External, 0, &[xt]).await;
		assert!(matches!(
			result.pop().unwrap().unwrap_err(),
			sc_transaction_pool_api::error::Error::ImmediatelyDropped
		));
	}

	#[tokio::test]
	async fn replacing_txs_works_for_same_tx_size() {
		sp_tracing::try_init_simple();
		let max = 10;
		let api = Arc::from(TestApi::default());
		let mempool = TxMemPool::new_test(api.clone(), usize::MAX, max * LARGE_XT_SIZE);

		let xts = (0..max).map(|x| Arc::from(large_uxt(x))).collect::<Vec<_>>();

		let low_prio = 0u64;
		let hi_prio = u64::MAX;

		let total_xts_bytes = xts.iter().fold(0, |r, x| r + api.hash_and_length(&x).1);
		let (submit_outcomes, hashes): (Vec<ViewStoreSubmitOutcome<TestApi>>, Vec<_>) = xts
			.iter()
			.map(|t| {
				let h = api.hash_and_length(t).0;
				(ViewStoreSubmitOutcome::new(h, Some(low_prio)), h)
			})
			.unzip();

		let results = mempool.extend_unwatched(TransactionSource::External, 0, &xts).await;
		assert!(results.iter().all(Result::is_ok));
		assert_eq!(mempool.bytes(), total_xts_bytes);

		for o in submit_outcomes {
			mempool.update_transaction_priority(o.hash(), o.priority()).await;
		}

		let xt = Arc::from(large_uxt(98));
		let hash = api.hash_and_length(&xt).0;
		let result = mempool
			.try_insert_with_replacement(xt, hi_prio, TransactionSource::External, 0, false)
			.await
			.unwrap();

		assert_eq!(result.hash, hash);
		assert_eq!(result.removed, hashes[0..1]);
	}

	#[tokio::test]
	async fn replacing_txs_removes_proper_size_of_txs() {
		sp_tracing::try_init_simple();
		let max = 10;
		let api = Arc::from(TestApi::default());
		let mempool = TxMemPool::new_test(api.clone(), usize::MAX, max * LARGE_XT_SIZE);

		let xts = (0..max).map(|x| Arc::from(large_uxt(x))).collect::<Vec<_>>();

		let low_prio = 0u64;
		let hi_prio = u64::MAX;

		let total_xts_bytes = xts.iter().fold(0, |r, x| r + api.hash_and_length(&x).1);
		let (submit_outcomes, hashes): (Vec<ViewStoreSubmitOutcome<TestApi>>, Vec<_>) = xts
			.iter()
			.map(|t| {
				let h = api.hash_and_length(t).0;
				(ViewStoreSubmitOutcome::new(h, Some(low_prio)), h)
			})
			.unzip();

		let results = mempool.extend_unwatched(TransactionSource::External, 0, &xts).await;
		assert!(results.iter().all(Result::is_ok));
		assert_eq!(mempool.bytes(), total_xts_bytes);
		assert_eq!(total_xts_bytes, max * LARGE_XT_SIZE);

		for o in submit_outcomes {
			mempool.update_transaction_priority(o.hash(), o.priority()).await;
		}

		// this one should drop 2 xts (size: 1130):
		let xt = Arc::from(ExtrinsicBuilder::new_include_data(vec![98 as u8; 1025]).build());
		let (hash, length) = api.hash_and_length(&xt);
		assert_eq!(length, 1130);
		let result = mempool
			.try_insert_with_replacement(xt, hi_prio, TransactionSource::External, 0, false)
			.await
			.unwrap();

		assert_eq!(result.hash, hash);
		assert_eq!(result.removed, hashes[0..2]);
	}

	#[tokio::test]
	async fn replacing_txs_removes_proper_size_and_prios() {
		sp_tracing::try_init_simple();
		const COUNT: usize = 10;
		let api = Arc::from(TestApi::default());
		let mempool = TxMemPool::new_test(api.clone(), usize::MAX, COUNT * LARGE_XT_SIZE);

		let xts = (0..COUNT).map(|x| Arc::from(large_uxt(x))).collect::<Vec<_>>();

		let hi_prio = u64::MAX;

		let total_xts_bytes = xts.iter().fold(0, |r, x| r + api.hash_and_length(&x).1);
		let (submit_outcomes, hashes): (Vec<ViewStoreSubmitOutcome<TestApi>>, Vec<_>) = xts
			.iter()
			.enumerate()
			.map(|(prio, t)| {
				let h = api.hash_and_length(t).0;
				(ViewStoreSubmitOutcome::new(h, Some((COUNT - prio).try_into().unwrap())), h)
			})
			.unzip();

		let results = mempool.extend_unwatched(TransactionSource::External, 0, &xts).await;
		assert!(results.iter().all(Result::is_ok));
		assert_eq!(mempool.bytes(), total_xts_bytes);

		for o in submit_outcomes {
			mempool.update_transaction_priority(o.hash(), o.priority()).await;
		}

		// this one should drop 3 xts (each of size 1129)
		let xt = Arc::from(ExtrinsicBuilder::new_include_data(vec![98 as u8; 2154]).build());
		let (hash, length) = api.hash_and_length(&xt);
		// overhead is 105, thus length: 105 + 2154
		assert_eq!(length, 2 * LARGE_XT_SIZE + 1);
		let result = mempool
			.try_insert_with_replacement(xt, hi_prio, TransactionSource::External, 0, false)
			.await
			.unwrap();

		assert_eq!(result.hash, hash);
		assert!(result.removed.iter().eq(hashes[COUNT - 3..COUNT].iter().rev()));
	}

	#[tokio::test]
	async fn replacing_txs_skips_lower_prio_tx() {
		sp_tracing::try_init_simple();
		const COUNT: usize = 10;
		let api = Arc::from(TestApi::default());
		let mempool = TxMemPool::new_test(api.clone(), usize::MAX, COUNT * LARGE_XT_SIZE);

		let xts = (0..COUNT).map(|x| Arc::from(large_uxt(x))).collect::<Vec<_>>();

		let hi_prio = 100u64;
		let low_prio = 10u64;

		let total_xts_bytes = xts.iter().fold(0, |r, x| r + api.hash_and_length(&x).1);
		let submit_outcomes: Vec<ViewStoreSubmitOutcome<TestApi>> = xts
			.iter()
			.map(|t| {
				let h = api.hash_and_length(t).0;
				ViewStoreSubmitOutcome::new(h, Some(hi_prio))
			})
			.collect::<Vec<_>>();

		let results = mempool.extend_unwatched(TransactionSource::External, 0, &xts).await;
		assert!(results.iter().all(Result::is_ok));
		assert_eq!(mempool.bytes(), total_xts_bytes);

		for o in submit_outcomes {
			mempool.update_transaction_priority(o.hash(), o.priority()).await;
		}

		let xt = Arc::from(large_uxt(98));
		let result = mempool
			.try_insert_with_replacement(xt, low_prio, TransactionSource::External, 0, false)
			.await;

		// lower prio tx is rejected immediately
		assert!(matches!(
			result.unwrap_err(),
			sc_transaction_pool_api::error::Error::ImmediatelyDropped
		));
	}

	#[tokio::test]
	async fn replacing_txs_is_skipped_if_prios_are_not_set() {
		sp_tracing::try_init_simple();
		const COUNT: usize = 10;
		let api = Arc::from(TestApi::default());
		let mempool = TxMemPool::new_test(api.clone(), usize::MAX, COUNT * LARGE_XT_SIZE);

		let xts = (0..COUNT).map(|x| Arc::from(large_uxt(x))).collect::<Vec<_>>();

		let hi_prio = u64::MAX;

		let total_xts_bytes = xts.iter().fold(0, |r, x| r + api.hash_and_length(&x).1);

		let results = mempool.extend_unwatched(TransactionSource::External, 0, &xts).await;
		assert!(results.iter().all(Result::is_ok));
		assert_eq!(mempool.bytes(), total_xts_bytes);

		// this one could drop 3 xts (each of size 1129)
		let xt = Arc::from(ExtrinsicBuilder::new_include_data(vec![98 as u8; 2154]).build());
		let length = api.hash_and_length(&xt).1;
		// overhead is 105, thus length: 105 + 2154
		assert_eq!(length, 2 * LARGE_XT_SIZE + 1);

		let result = mempool
			.try_insert_with_replacement(xt, hi_prio, TransactionSource::External, 0, false)
			.await;

		// we did not update priorities (update_transaction_priority was not called):
		assert!(matches!(
			result.unwrap_err(),
			sc_transaction_pool_api::error::Error::ImmediatelyDropped
		));
	}
}
