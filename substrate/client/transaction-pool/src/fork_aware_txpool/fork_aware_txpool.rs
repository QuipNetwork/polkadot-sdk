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

//! Substrate fork-aware transaction pool implementation.

use super::{
	dropped_watcher::{MultiViewDroppedWatcherController, StreamOfDropped},
	import_notification_sink::MultiViewImportNotificationSink,
	metrics::{EventsMetricsCollector, MetricsLink as PrometheusMetrics},
	multi_view_listener::MultiViewListener,
	tx_mem_pool::{InsertionInfo, TxMemPool},
	view::{ImportedStatus, View},
	view_store::ViewStore,
};
use crate::{
	api::FullChainApi,
	common::{
		sliding_stat::DurationSlidingStats,
		tracing_log_xt::{log_xt_debug, log_xt_trace},
		STAT_SLIDING_WINDOW,
	},
	enactment_state::{EnactmentAction, EnactmentState},
	fork_aware_txpool::{
		dropped_watcher::{DroppedReason, DroppedTransaction},
		revalidation_worker,
	},
	graph::{
		self,
		base_pool::{TimedTransactionSource, Transaction},
		BlockHash, ExtrinsicFor, ExtrinsicHash, IsValidator, Options, RawExtrinsicFor,
	},
	insert_and_log_throttled, ReadyIteratorFor, ValidateTransactionPriority, LOG_TARGET,
	LOG_TARGET_STAT,
};
use async_trait::async_trait;
use futures::{
	channel::oneshot,
	future::{self},
	prelude::*,
	FutureExt,
};
use parking_lot::Mutex;
use prometheus_endpoint::Registry as PrometheusRegistry;
use sc_transaction_pool_api::{
	error::Error as TxPoolApiError, ChainEvent, ImportNotificationStream,
	MaintainedTransactionPool, PoolStatus, TransactionFor, TransactionPool, TransactionSource,
	TransactionStatusStreamFor, TxHash, TxInvalidityReportMap,
};
use sp_blockchain::{HashAndNumber, TreeRoute};
use sp_core::traits::SpawnEssentialNamed;
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, NumberFor},
	transaction_validity::{TransactionTag as Tag, TransactionValidityError, ValidTransaction},
	Saturating,
};
use std::{
	collections::{BTreeMap, HashMap, HashSet},
	pin::Pin,
	sync::Arc,
	time::{Duration, Instant},
};
use tokio::select;
use tracing::{debug, instrument, trace, warn, Level};

/// The maximum block height difference before considering a view or transaction as timed-out
/// due to a finality stall. When the difference exceeds this threshold, elements are treated
/// as stale and are subject to cleanup.
const FINALITY_TIMEOUT_THRESHOLD: usize = 128;

/// The number of transactions that will be sent from the mempool to the newly created view during
/// the maintain process.
// todo [#8835]: better approach is needed - maybe time-budget approach?
// note: yap parachain block size.
const MEMPOOL_TO_VIEW_BATCH_SIZE: usize = 7_000;

/// Fork aware transaction pool task, that needs to be polled.
pub type ForkAwareTxPoolTask = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A structure that maintains a collection of pollers associated with specific block hashes
/// (views).
struct ReadyPoll<T, Block>
where
	Block: BlockT,
{
	pollers: HashMap<Block::Hash, Vec<oneshot::Sender<T>>>,
}

impl<T, Block> ReadyPoll<T, Block>
where
	Block: BlockT,
{
	/// Creates a new `ReadyPoll` instance with an empty collection of pollers.
	fn new() -> Self {
		Self { pollers: Default::default() }
	}

	/// Adds a new poller for a specific block hash and returns the `Receiver` end of the created
	/// oneshot channel which will be used to deliver polled result.
	fn add(&mut self, at: <Block as BlockT>::Hash) -> oneshot::Receiver<T> {
		let (s, r) = oneshot::channel();
		self.pollers.entry(at).or_default().push(s);
		r
	}

	/// Triggers all pollers associated with a specific block by sending the polled result through
	/// each oneshot channel.
	///
	/// `ready_iterator` is a closure that generates the result data to be sent to the pollers.
	fn trigger(&mut self, at: Block::Hash, ready_iterator: impl Fn() -> T) {
		debug!(target: LOG_TARGET, ?at, keys = ?self.pollers.keys(), "fatp::trigger");
		let Some(pollers) = self.pollers.remove(&at) else { return };
		pollers.into_iter().for_each(|p| {
			debug!(target: LOG_TARGET, "fatp::trigger trigger ready signal at block {}", at);
			let _ = p.send(ready_iterator());
		});
	}

	/// Removes pollers that have their oneshot channels cancelled.
	fn remove_cancelled(&mut self) {
		self.pollers.retain(|_, v| v.iter().any(|sender| !sender.is_canceled()));
	}
}

/// The fork-aware transaction pool.
///
/// It keeps track of every fork and provides the set of transactions that is valid for every fork.
pub struct ForkAwareTxPool<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: graph::ChainApi<Block = Block> + 'static,
{
	/// The reference to the `ChainApi` provided by client/backend.
	api: Arc<ChainApi>,

	/// Intermediate buffer for the incoming transaction.
	mempool: Arc<TxMemPool<ChainApi, Block>>,

	/// The store for all the views.
	view_store: Arc<ViewStore<ChainApi, Block>>,

	/// Utility for managing pollers of `ready_at` future.
	ready_poll: Arc<Mutex<ReadyPoll<ReadyIteratorFor<ChainApi>, Block>>>,

	/// Prometheus's metrics endpoint.
	metrics: PrometheusMetrics,

	/// Collector of transaction statuses updates, reports transaction events metrics.
	events_metrics_collector: EventsMetricsCollector<ChainApi>,

	/// Util tracking best and finalized block.
	enactment_state: Arc<Mutex<EnactmentState<Block>>>,

	/// The channel allowing to send revalidation jobs to the background thread.
	revalidation_queue: Arc<revalidation_worker::RevalidationQueue<ChainApi, Block>>,

	/// Util providing an aggregated stream of transactions that were imported to ready queue in
	/// any view.
	import_notification_sink: MultiViewImportNotificationSink<Block::Hash, ExtrinsicHash<ChainApi>>,

	/// Externally provided pool options.
	options: Options,

	/// Is node the validator.
	is_validator: IsValidator,

	/// Finality timeout threshold.
	///
	/// Sets the maximum permissible block height difference between the latest block
	/// and the oldest transactions or views in the pool. Beyond this difference,
	/// transactions/views are considered timed out and eligible for cleanup.
	finality_timeout_threshold: usize,

	/// Transactions included in blocks since the most recently finalized block (including this
	/// block).
	///
	/// Holds a mapping of block hash and number to their corresponding transaction hashes.
	///
	/// Intended to be used in the finality stall cleanups and also as a cache for all in-block
	/// transactions.
	included_transactions: Mutex<BTreeMap<HashAndNumber<Block>, Vec<ExtrinsicHash<ChainApi>>>>,

	/// Stats for submit call durations
	submit_stats: DurationSlidingStats,

	/// Stats for submit_and_watch call durations
	submit_and_watch_stats: DurationSlidingStats,
}

impl<ChainApi, Block> ForkAwareTxPool<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: graph::ChainApi<Block = Block> + 'static,
	<Block as BlockT>::Hash: Unpin,
{
	// Injects a view for the given block to self.
	//
	// Helper for the pool new methods.
	fn inject_initial_view(self, initial_view_hash: Block::Hash) -> Self {
		if let Some(block_number) =
			self.api.block_id_to_number(&BlockId::Hash(initial_view_hash)).ok().flatten()
		{
			let at_best = HashAndNumber { number: block_number, hash: initial_view_hash };
			let tree_route =
				&TreeRoute::new(vec![at_best.clone()], 0).expect("tree route is correct; qed");
			let view = self.build_and_plug_view(None, &at_best, &tree_route);
			self.view_store.insert_new_view_sync(view.into(), &tree_route);
			trace!(target: LOG_TARGET, ?block_number, ?initial_view_hash, "fatp::injected initial view");
		};
		self
	}

	/// Create new fork aware transaction pool with provided shared instance of `ChainApi` intended
	/// for tests.
	pub fn new_test(
		pool_api: Arc<ChainApi>,
		best_block_hash: Block::Hash,
		finalized_hash: Block::Hash,
		finality_timeout_threshold: Option<usize>,
	) -> (Self, [ForkAwareTxPoolTask; 2]) {
		Self::new_test_with_limits(
			pool_api,
			best_block_hash,
			finalized_hash,
			Options::default().ready,
			Options::default().future,
			usize::MAX,
			finality_timeout_threshold,
		)
	}

	/// Create new fork aware transaction pool with given limits and with provided shared instance
	/// of `ChainApi` intended for tests.
	pub fn new_test_with_limits(
		pool_api: Arc<ChainApi>,
		best_block_hash: Block::Hash,
		finalized_hash: Block::Hash,
		ready_limits: crate::PoolLimit,
		future_limits: crate::PoolLimit,
		mempool_max_transactions_count: usize,
		finality_timeout_threshold: Option<usize>,
	) -> (Self, [ForkAwareTxPoolTask; 2]) {
		let (listener, listener_task) = MultiViewListener::new_with_worker(Default::default());
		let listener = Arc::new(listener);

		let (import_notification_sink, import_notification_sink_task) =
			MultiViewImportNotificationSink::new_with_worker();

		let (mempool, mempool_task) = TxMemPool::new(
			pool_api.clone(),
			listener.clone(),
			Default::default(),
			mempool_max_transactions_count,
			ready_limits.total_bytes + future_limits.total_bytes,
		);
		let mempool = Arc::from(mempool);

		let (dropped_stream_controller, dropped_stream) =
			MultiViewDroppedWatcherController::<ChainApi>::new();

		let view_store = Arc::new(ViewStore::new(
			pool_api.clone(),
			listener,
			dropped_stream_controller,
			import_notification_sink.clone(),
		));

		let dropped_monitor_task = Self::dropped_monitor_task(
			dropped_stream,
			mempool.clone(),
			view_store.clone(),
			import_notification_sink.clone(),
		);

		let combined_tasks = async move {
			tokio::select! {
				_ = listener_task => {},
				_ = import_notification_sink_task => {},
				_ = dropped_monitor_task => {}
			}
		}
		.boxed();

		let options = Options { ready: ready_limits, future: future_limits, ..Default::default() };

		(
			Self {
				mempool,
				api: pool_api,
				view_store,
				ready_poll: Arc::from(Mutex::from(ReadyPoll::new())),
				enactment_state: Arc::new(Mutex::new(EnactmentState::new(
					best_block_hash,
					finalized_hash,
				))),
				revalidation_queue: Arc::from(revalidation_worker::RevalidationQueue::new()),
				import_notification_sink,
				options,
				is_validator: false.into(),
				metrics: Default::default(),
				events_metrics_collector: EventsMetricsCollector::default(),
				finality_timeout_threshold: finality_timeout_threshold
					.unwrap_or(FINALITY_TIMEOUT_THRESHOLD),
				included_transactions: Default::default(),
				submit_stats: DurationSlidingStats::new(Duration::from_secs(STAT_SLIDING_WINDOW)),
				submit_and_watch_stats: DurationSlidingStats::new(Duration::from_secs(
					STAT_SLIDING_WINDOW,
				)),
			}
			.inject_initial_view(best_block_hash),
			[combined_tasks, mempool_task],
		)
	}

	/// Monitors the stream of dropped transactions and removes them from the mempool and
	/// view_store.
	///
	/// This asynchronous task continuously listens for dropped transaction notifications provided
	/// within `dropped_stream` and ensures that these transactions are removed from the `mempool`
	/// and `import_notification_sink` instances. For Usurped events, the transaction is also
	/// removed from the view_store.
	async fn dropped_monitor_task(
		mut dropped_stream: StreamOfDropped<ChainApi>,
		mempool: Arc<TxMemPool<ChainApi, Block>>,
		view_store: Arc<ViewStore<ChainApi, Block>>,
		import_notification_sink: MultiViewImportNotificationSink<
			Block::Hash,
			ExtrinsicHash<ChainApi>,
		>,
	) {
		let dropped_stats = DurationSlidingStats::new(Duration::from_secs(STAT_SLIDING_WINDOW));
		loop {
			let Some(dropped) = dropped_stream.next().await else {
				debug!(target: LOG_TARGET, "fatp::dropped_monitor_task: terminated...");
				break;
			};
			let start = Instant::now();
			let tx_hash = dropped.tx_hash;
			trace!(
				target: LOG_TARGET,
				?tx_hash,
				reason = ?dropped.reason,
				"fatp::dropped notification, removing"
			);
			match dropped.reason {
				DroppedReason::Usurped(new_tx_hash) => {
					if let Some(new_tx) = mempool.get_by_hash(new_tx_hash).await {
						view_store.replace_transaction(new_tx.source(), new_tx.tx(), tx_hash).await;
					} else {
						trace!(
							target: LOG_TARGET,
							tx_hash = ?new_tx_hash,
							"error: dropped_monitor_task: no entry in mempool for new transaction"
						);
					};
				},
				DroppedReason::LimitsEnforced | DroppedReason::Invalid => {
					view_store.remove_transaction_subtree(tx_hash, |_, _| {});
				},
				DroppedReason::Viewless => {
					if let Some(tx) = mempool.get_by_hash(tx_hash).await {
						trace!(
							target: LOG_TARGET,
							?tx_hash,
							"dropped_monitor_task: transaction became viewless, marking for unban"
						);
						tx.set_needs_unban();
					}
					continue;
				},
			};

			mempool.remove_transactions(&[tx_hash]).await;
			import_notification_sink.clean_notified_items(&[tx_hash]);
			view_store.listener.transaction_dropped(dropped);
			insert_and_log_throttled!(
				Level::DEBUG,
				target:LOG_TARGET_STAT,
				prefix:"dropped_stats",
				dropped_stats,
				start.elapsed().into()
			);
		}
	}

	/// Creates new fork aware transaction pool with the background revalidation worker.
	///
	/// The txpool essential tasks (including a revalidation worker) are spawned using provided
	/// spawner.
	pub fn new_with_background_worker(
		options: Options,
		is_validator: IsValidator,
		pool_api: Arc<ChainApi>,
		prometheus: Option<&PrometheusRegistry>,
		spawner: impl SpawnEssentialNamed,
		best_block_hash: Block::Hash,
		finalized_hash: Block::Hash,
	) -> Self {
		let metrics = PrometheusMetrics::new(prometheus);
		let (events_metrics_collector, event_metrics_task) =
			EventsMetricsCollector::<ChainApi>::new_with_worker(metrics.clone());

		let (listener, listener_task) =
			MultiViewListener::new_with_worker(events_metrics_collector.clone());
		let listener = Arc::new(listener);

		let (revalidation_queue, revalidation_task) =
			revalidation_worker::RevalidationQueue::new_with_worker();

		let (import_notification_sink, import_notification_sink_task) =
			MultiViewImportNotificationSink::new_with_worker();

		let (mempool, blocking_mempool_task) = TxMemPool::new(
			pool_api.clone(),
			listener.clone(),
			metrics.clone(),
			options.total_count(),
			options.ready.total_bytes + options.future.total_bytes,
		);
		let mempool = Arc::from(mempool);

		let (dropped_stream_controller, dropped_stream) =
			MultiViewDroppedWatcherController::<ChainApi>::new();

		let view_store = Arc::new(ViewStore::new(
			pool_api.clone(),
			listener,
			dropped_stream_controller,
			import_notification_sink.clone(),
		));

		let dropped_monitor_task = Self::dropped_monitor_task(
			dropped_stream,
			mempool.clone(),
			view_store.clone(),
			import_notification_sink.clone(),
		);

		let combined_tasks = async move {
			tokio::select! {
				_ = listener_task => {}
				_ = revalidation_task => {},
				_ = import_notification_sink_task => {},
				_ = dropped_monitor_task => {}
				_ = event_metrics_task => {},
			}
		}
		.boxed();
		spawner.spawn_essential("txpool-background", Some("transaction-pool"), combined_tasks);
		spawner.spawn_essential_blocking(
			"txpool-background",
			Some("transaction-pool"),
			blocking_mempool_task,
		);

		Self {
			mempool,
			api: pool_api,
			view_store,
			ready_poll: Arc::from(Mutex::from(ReadyPoll::new())),
			enactment_state: Arc::new(Mutex::new(EnactmentState::new(
				best_block_hash,
				finalized_hash,
			))),
			revalidation_queue: Arc::from(revalidation_queue),
			import_notification_sink,
			options,
			metrics,
			events_metrics_collector,
			is_validator,
			finality_timeout_threshold: FINALITY_TIMEOUT_THRESHOLD,
			included_transactions: Default::default(),
			submit_stats: DurationSlidingStats::new(Duration::from_secs(STAT_SLIDING_WINDOW)),
			submit_and_watch_stats: DurationSlidingStats::new(Duration::from_secs(
				STAT_SLIDING_WINDOW,
			)),
		}
		.inject_initial_view(best_block_hash)
	}

	/// Get access to the underlying api
	pub fn api(&self) -> &ChainApi {
		&self.api
	}

	/// Provides a status for all views at the tips of the forks.
	pub fn status_all(&self) -> HashMap<Block::Hash, PoolStatus> {
		self.view_store.status()
	}

	/// Provides a number of views at the tips of the forks.
	pub fn active_views_count(&self) -> usize {
		self.view_store.active_views.read().len()
	}

	/// Provides a number of views at the tips of the forks.
	pub fn inactive_views_count(&self) -> usize {
		self.view_store.inactive_views.read().len()
	}

	/// Provides internal views statistics.
	///
	/// Provides block number, count of ready, count of future transactions for every view. It is
	/// suitable for printing log information.
	fn views_stats(&self) -> Vec<(NumberFor<Block>, usize, usize)> {
		self.view_store
			.active_views
			.read()
			.iter()
			.map(|v| (v.1.at.number, v.1.status().ready, v.1.status().future))
			.collect()
	}

	/// Checks if there is a view at the tip of the fork with given hash.
	pub fn has_view(&self, hash: &Block::Hash) -> bool {
		self.view_store.active_views.read().contains_key(hash)
	}

	/// Returns a number of unwatched and watched transactions in internal mempool.
	///
	/// Intended for use in unit tests.
	pub async fn mempool_len(&self) -> (usize, usize) {
		self.mempool.unwatched_and_watched_count().await
	}

	/// Returns a set of future transactions for given block hash.
	///
	/// Intended for logging / tests.
	pub fn futures_at(
		&self,
		at: Block::Hash,
	) -> Option<Vec<Transaction<ExtrinsicHash<ChainApi>, ExtrinsicFor<ChainApi>>>> {
		self.view_store.futures_at(at)
	}

	/// Returns a best-effort set of ready transactions for a given block, without executing full
	/// maintain process.
	///
	/// The method attempts to build a temporary view and create an iterator of ready transactions
	/// for a specific `at` hash. If a valid view is found, it collects and prunes
	/// transactions already included in the blocks and returns the valid set. Not finding a view
	/// returns with the ready transaction set found in the most recent view processed by the
	/// fork-aware txpool. Not being able to query for block number for the provided `at` block hash
	/// results in returning an empty transaction set.
	///
	/// Pruning is just rebuilding the underlying transactions graph, no validations are executed,
	/// so this process shall be fast.
	pub async fn ready_at_light(&self, at: Block::Hash) -> ReadyIteratorFor<ChainApi> {
		let start = Instant::now();
		let api = self.api.clone();
		debug!(
			target: LOG_TARGET,
			?at,
			"fatp::ready_at_light"
		);

		let at_number = self.api.resolve_block_number(at).ok();
		let finalized_number = self
			.api
			.resolve_block_number(self.enactment_state.lock().recent_finalized_block())
			.ok();

		// Prune all txs from the best view found, considering the extrinsics part of the blocks
		// that are more recent than the view itself.
		if let Some((view, enacted_blocks, at_hn)) = at_number.and_then(|at_number| {
			let at_hn = HashAndNumber { hash: at, number: at_number };
			finalized_number.and_then(|finalized_number| {
				self.view_store
					.find_view_descendent_up_to_number(&at_hn, finalized_number)
					.map(|(view, enacted_blocks)| (view, enacted_blocks, at_hn))
			})
		}) {
			let (tmp_view, _, _): (View<ChainApi>, _, _) = View::new_from_other(&view, &at_hn);
			let mut all_extrinsics = vec![];
			for h in enacted_blocks {
				let extrinsics = api
					.block_body(h)
					.await
					.unwrap_or_else(|error| {
						warn!(
							target: LOG_TARGET,
							%error,
							"Compute ready light transactions: error request"
						);
						None
					})
					.unwrap_or_default()
					.into_iter()
					.map(|t| api.hash_and_length(&t).0);
				all_extrinsics.extend(extrinsics);
			}

			let before_count = tmp_view.pool.validated_pool().status().ready;
			let tags = tmp_view
				.pool
				.validated_pool()
				.extrinsics_tags(&all_extrinsics)
				.into_iter()
				.flatten()
				.flatten()
				.collect::<Vec<_>>();
			let _ = tmp_view.pool.validated_pool().prune_tags(tags);

			let after_count = tmp_view.pool.validated_pool().status().ready;
			debug!(
				target: LOG_TARGET,
				?at,
				best_view_hash = ?view.at.hash,
				before_count,
				to_be_removed = all_extrinsics.len(),
				after_count,
				duration = ?start.elapsed(),
				"fatp::ready_at_light -> light"
			);
			Box::new(tmp_view.pool.validated_pool().ready())
		} else if let Some(most_recent_view) = self.view_store.most_recent_view.read().clone() {
			// Fallback for the case when `at` is not on the already known fork.
			// Falls back to the most recent view, which may include txs which
			// are invalid or already included in the blocks but can still yield a
			// partially valid ready set, which is still better than including nothing.
			debug!(
				target: LOG_TARGET,
				?at,
				duration = ?start.elapsed(),
				"fatp::ready_at_light -> most_recent_view"
			);
			Box::new(most_recent_view.pool.validated_pool().ready())
		} else {
			let empty: ReadyIteratorFor<ChainApi> = Box::new(std::iter::empty());
			debug!(
				target: LOG_TARGET,
				?at,
				duration = ?start.elapsed(),
				"fatp::ready_at_light -> empty"
			);
			empty
		}
	}

	/// Waits for the set of ready transactions for a given block up to a specified timeout.
	///
	/// This method combines two futures:
	/// - The `ready_at` future, which waits for the ready transactions resulting from the full
	/// maintenance process to be available.
	/// - The `ready_at_light` future, used as a fallback if the timeout expires before `ready_at`
	/// completes. This provides a best-effort, ready set of transactions as a result light
	/// maintain.
	///
	/// Returns a future resolving to a ready iterator of transactions.
	async fn ready_at_with_timeout_internal(
		&self,
		at: Block::Hash,
		timeout: std::time::Duration,
	) -> ReadyIteratorFor<ChainApi> {
		debug!(
			target: LOG_TARGET,
			?at,
			?timeout,
			"fatp::ready_at_with_timeout"
		);
		let timeout = futures_timer::Delay::new(timeout);
		let (view_already_exists, ready_at) = self.ready_at_internal(at);

		if view_already_exists {
			return ready_at.await;
		}

		let maybe_ready = async move {
			select! {
				ready = ready_at => Some(ready),
				_ = timeout => {
					debug!(
						target: LOG_TARGET,
						?at,
						"Timeout fired waiting for transaction pool at block. Proceeding with production."
					);
					None
				}
			}
		};

		let fall_back_ready = self.ready_at_light(at);
		let (maybe_ready, fall_back_ready) =
			futures::future::join(maybe_ready, fall_back_ready).await;
		maybe_ready.unwrap_or(fall_back_ready)
	}

	fn ready_at_internal(
		&self,
		at: Block::Hash,
	) -> (bool, Pin<Box<dyn Future<Output = ReadyIteratorFor<ChainApi>> + Send>>) {
		let mut ready_poll = self.ready_poll.lock();

		if let Some((view, inactive)) = self.view_store.get_view_at(at, true) {
			debug!(
				target: LOG_TARGET,
				?at,
				?inactive,
				"fatp::ready_at_internal"
			);
			let iterator: ReadyIteratorFor<ChainApi> = Box::new(view.pool.validated_pool().ready());
			return (true, async move { iterator }.boxed());
		}

		let pending = ready_poll
			.add(at)
			.map(|received| {
				received.unwrap_or_else(|error| {
					warn!(
						target: LOG_TARGET,
						%error,
						"Error receiving ready-set iterator"
					);
					Box::new(std::iter::empty())
				})
			})
			.boxed();
		debug!(
			target: LOG_TARGET,
			?at,
			pending_keys = ?ready_poll.pollers.keys(),
			"fatp::ready_at_internal"
		);
		(false, pending)
	}

	/// Refer to [`Self::submit_and_watch`]
	async fn submit_and_watch_inner(
		&self,
		at: Block::Hash,
		source: TransactionSource,
		xt: TransactionFor<Self>,
	) -> Result<Pin<Box<TransactionStatusStreamFor<Self>>>, ChainApi::Error> {
		let xt = Arc::from(xt);

		let at_number = self
			.api
			.block_id_to_number(&BlockId::Hash(at))
			.ok()
			.flatten()
			.unwrap_or_default()
			.into()
			.as_u64();

		let insertion = match self.mempool.push_watched(source, at_number, xt.clone()).await {
			Ok(result) => result,
			Err(TxPoolApiError::ImmediatelyDropped) => {
				self.attempt_transaction_replacement(source, at_number, true, xt.clone())
					.await?
			},
			Err(e) => return Err(e.into()),
		};

		self.metrics.report(|metrics| metrics.submitted_transactions.inc());
		self.events_metrics_collector.report_submitted(&insertion);

		match self.view_store.submit_and_watch(at, insertion.source, xt).await {
			Err(e) => {
				self.mempool.remove_transactions(&[insertion.hash]).await;
				Err(e.into())
			},
			Ok(mut outcome) => {
				self.mempool
					.update_transaction_priority(outcome.hash(), outcome.priority())
					.await;
				Ok(outcome.expect_watcher())
			},
		}
	}

	/// Refer to [`Self::submit_at`]
	async fn submit_at_inner(
		&self,
		at: Block::Hash,
		source: TransactionSource,
		xts: Vec<TransactionFor<Self>>,
	) -> Result<Vec<Result<TxHash<Self>, ChainApi::Error>>, ChainApi::Error> {
		let at_number = self
			.api
			.block_id_to_number(&BlockId::Hash(at))
			.ok()
			.flatten()
			.unwrap_or_default()
			.into()
			.as_u64();
		let view_store = self.view_store.clone();
		let xts = xts.into_iter().map(Arc::from).collect::<Vec<_>>();
		let mempool_results = self.mempool.extend_unwatched(source, at_number, &xts).await;

		if view_store.is_empty() {
			return Ok(mempool_results
				.into_iter()
				.map(|r| r.map(|r| r.hash).map_err(Into::into))
				.collect::<Vec<_>>());
		}

		// Submit all the transactions to the mempool
		let retries = mempool_results
			.into_iter()
			.zip(xts.clone())
			.map(|(result, xt)| async move {
				match result {
					Err(TxPoolApiError::ImmediatelyDropped) => {
						self.attempt_transaction_replacement(source, at_number, false, xt).await
					},
					_ => result,
				}
			})
			.collect::<Vec<_>>();

		let mempool_results = futures::future::join_all(retries).await;

		// Collect transactions that were successfully submitted to the mempool...
		let to_be_submitted = mempool_results
			.iter()
			.zip(xts)
			.filter_map(|(result, xt)| {
				result.as_ref().ok().map(|insertion| {
					self.events_metrics_collector.report_submitted(&insertion);
					(insertion.source.clone(), xt)
				})
			})
			.collect::<Vec<_>>();

		self.metrics
			.report(|metrics| metrics.submitted_transactions.inc_by(to_be_submitted.len() as _));

		// ... and submit them to the view_store. Please note that transactions rejected by mempool
		// are not sent here.
		let mempool = self.mempool.clone();
		let results_map = view_store.submit(to_be_submitted.into_iter()).await;
		let mut submission_results = reduce_multiview_result(results_map).into_iter();

		// Note for composing final result:
		//
		// For each failed insertion into the mempool, the mempool result should be placed into
		// the returned vector.
		//
		// For each successful insertion into the mempool, the corresponding
		// view_store submission result needs to be examined (merged_results):
		// - If there is an error during view_store submission, the transaction is removed from
		// the mempool, and the final result recorded in the vector for this transaction is the
		// view_store submission error.
		//
		// - If the view_store submission is successful, the transaction priority is updated in the
		// mempool.
		//
		// Finally, it collects the hashes of updated transactions or submission errors (either
		// from the mempool or view_store) into a returned vector (final_results).
		const RESULTS_ASSUMPTION : &str =
			"The number of Ok results in mempool is exactly the same as the size of view_store submission result. qed.";
		let merged_results = mempool_results.into_iter().map(|result| {
			result.map_err(Into::into).and_then(|insertion| {
				Ok((insertion.hash, submission_results.next().expect(RESULTS_ASSUMPTION)))
			})
		});

		let mut final_results = vec![];
		for r in merged_results {
			match r {
				Ok((hash, submission_result)) => match submission_result {
					Ok(r) => {
						mempool.update_transaction_priority(r.hash(), r.priority()).await;
						final_results.push(Ok(r.hash()));
					},
					Err(e) => {
						mempool.remove_transactions(&[hash]).await;
						final_results.push(Err(e));
					},
				},
				Err(e) => final_results.push(Err(e)),
			}
		}

		Ok(final_results)
	}

	/// Number of notified items in import_notification_sink.
	///
	/// Internal detail, exposed only for testing.
	pub fn import_notification_sink_len(&self) -> usize {
		self.import_notification_sink.notified_items_len()
	}
}

/// Converts the input view-to-statuses map into the output vector of statuses.
///
/// The result of importing a bunch of transactions into a single view is the vector of statuses.
/// Every item represents a status for single transaction. The input is the map that associates
/// hash-views with vectors indicating the statuses of transactions imports.
///
/// Import to multiple views result in two-dimensional array of statuses, which is provided as
/// input map.
///
/// This function converts the map into the vec of results, according to the following rules:
/// - for given transaction if at least one status is success, then output vector contains success,
/// - if given transaction status is error for every view, then output vector contains error.
///
/// The results for transactions are in the same order for every view. An output vector preserves
/// this order.
///
/// ```skip
/// in:
/// view  |   xt0 status | xt1 status | xt2 status
/// h1   -> [ Ok(xth0),    Ok(xth1),    Err       ]
/// h2   -> [ Ok(xth0),    Err,         Err       ]
/// h3   -> [ Ok(xth0),    Ok(xth1),    Err       ]
///
/// out:
/// [ Ok(xth0), Ok(xth1), Err ]
/// ```
fn reduce_multiview_result<H, D, E>(input: HashMap<H, Vec<Result<D, E>>>) -> Vec<Result<D, E>> {
	let mut values = input.values();
	let Some(first) = values.next() else {
		return Default::default();
	};
	let length = first.len();
	debug_assert!(values.all(|x| length == x.len()));

	input
		.into_values()
		.reduce(|mut agg_results, results| {
			agg_results.iter_mut().zip(results.into_iter()).for_each(|(agg_r, r)| {
				if agg_r.is_err() {
					*agg_r = r;
				}
			});
			agg_results
		})
		.unwrap_or_default()
}

#[async_trait]
impl<ChainApi, Block> TransactionPool for ForkAwareTxPool<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: 'static + graph::ChainApi<Block = Block>,
	<Block as BlockT>::Hash: Unpin,
{
	type Block = ChainApi::Block;
	type Hash = ExtrinsicHash<ChainApi>;
	type InPoolTransaction = Transaction<ExtrinsicHash<ChainApi>, ExtrinsicFor<ChainApi>>;
	type Error = ChainApi::Error;

	/// Submits multiple transactions and returns a future resolving to the submission results.
	///
	/// Actual transactions submission process is delegated to the `ViewStore` internal instance.
	///
	/// The internal limits of the pool are checked. The results of submissions to individual views
	/// are reduced to single result. Refer to `reduce_multiview_result` for more details.
	async fn submit_at(
		&self,
		at: <Self::Block as BlockT>::Hash,
		source: TransactionSource,
		xts: Vec<TransactionFor<Self>>,
	) -> Result<Vec<Result<TxHash<Self>, Self::Error>>, Self::Error> {
		let start = Instant::now();
		trace!(
			target: LOG_TARGET,
			count = xts.len(),
			active_views_count = self.active_views_count(),
			"fatp::submit_at"
		);
		log_xt_trace!(target: LOG_TARGET, xts.iter().map(|xt| self.tx_hash(xt)), "fatp::submit_at");
		let result = self.submit_at_inner(at, source, xts).await;
		insert_and_log_throttled!(
			Level::DEBUG,
			target:LOG_TARGET_STAT,
			prefix:"submit_stats",
			self.submit_stats,
			start.elapsed().into()
		);
		result
	}

	/// Submits a single transaction and returns a future resolving to the submission results.
	///
	/// Actual transaction submission process is delegated to the `submit_at` function.
	async fn submit_one(
		&self,
		_at: <Self::Block as BlockT>::Hash,
		source: TransactionSource,
		xt: TransactionFor<Self>,
	) -> Result<TxHash<Self>, Self::Error> {
		trace!(
			target: LOG_TARGET,
			tx_hash = ?self.tx_hash(&xt),
			active_views_count = self.active_views_count(),
			"fatp::submit_one"
		);
		match self.submit_at(_at, source, vec![xt]).await {
			Ok(mut v) => {
				v.pop().expect("There is exactly one element in result of submit_at. qed.")
			},
			Err(e) => Err(e),
		}
	}

	/// Submits a transaction and starts to watch its progress in the pool, returning a stream of
	/// status updates.
	///
	/// Actual transaction submission process is delegated to the `ViewStore` internal instance.
	#[instrument(level = Level::TRACE, skip_all, target = "txpool", name = "fatp::submit_and_watch")]
	async fn submit_and_watch(
		&self,
		at: <Self::Block as BlockT>::Hash,
		source: TransactionSource,
		xt: TransactionFor<Self>,
	) -> Result<Pin<Box<TransactionStatusStreamFor<Self>>>, Self::Error> {
		let start = Instant::now();
		trace!(
			target: LOG_TARGET,
			tx_hash = ?self.tx_hash(&xt),
			views = self.active_views_count(),
			"fatp::submit_and_watch"
		);
		let result = self.submit_and_watch_inner(at, source, xt).await;
		insert_and_log_throttled!(
			Level::DEBUG,
			target:LOG_TARGET_STAT,
			prefix:"submit_and_watch_stats",
			self.submit_and_watch_stats,
			start.elapsed().into()
		);
		result
	}

	/// Reports invalid transactions to the transaction pool.
	///
	/// This function takes an array of tuples, each consisting of a transaction hash and the
	/// corresponding error that occurred during transaction execution at given block.
	///
	/// The transaction pool implementation will determine which transactions should be
	/// removed from the pool. Transactions that depend on invalid transactions will also
	/// be removed.
	async fn report_invalid(
		&self,
		at: Option<<Self::Block as BlockT>::Hash>,
		invalid_tx_errors: TxInvalidityReportMap<TxHash<Self>>,
	) -> Vec<Arc<Self::InPoolTransaction>> {
		debug!(target: LOG_TARGET, len = ?invalid_tx_errors.len(), "fatp::report_invalid");
		log_xt_debug!(data: tuple, target:LOG_TARGET, invalid_tx_errors.iter(), "fatp::report_invalid {:?}");
		self.metrics
			.report(|metrics| metrics.reported_invalid_txs.inc_by(invalid_tx_errors.len() as _));

		let removed = self.view_store.report_invalid(at, invalid_tx_errors);

		let removed_hashes = removed.iter().map(|tx| tx.hash).collect::<Vec<_>>();
		self.mempool.remove_transactions(&removed_hashes).await;
		self.import_notification_sink.clean_notified_items(&removed_hashes);

		self.metrics
			.report(|metrics| metrics.removed_invalid_txs.inc_by(removed_hashes.len() as _));

		removed
	}

	// todo [#5491]: api change?
	// status(Hash) -> Option<PoolStatus>
	/// Returns the pool status which includes information like the number of ready and future
	/// transactions.
	///
	/// Currently the status for the most recently notified block is returned (for which
	/// maintain process was accomplished).
	fn status(&self) -> PoolStatus {
		self.view_store
			.most_recent_view
			.read()
			.as_ref()
			.map(|v| v.status())
			.unwrap_or(PoolStatus { ready: 0, ready_bytes: 0, future: 0, future_bytes: 0 })
	}

	/// Return an event stream of notifications when transactions are imported to the pool.
	///
	/// Consumers of this stream should use the `ready` method to actually get the
	/// pending transactions in the right order.
	fn import_notification_stream(&self) -> ImportNotificationStream<ExtrinsicHash<ChainApi>> {
		self.import_notification_sink.event_stream()
	}

	/// Returns the hash of a given transaction.
	fn hash_of(&self, xt: &TransactionFor<Self>) -> TxHash<Self> {
		self.api().hash_and_length(xt).0
	}

	/// Notifies the pool about the broadcasting status of transactions.
	fn on_broadcasted(&self, propagations: HashMap<TxHash<Self>, Vec<String>>) {
		self.view_store.listener.transactions_broadcasted(propagations);
	}

	/// Return specific ready transaction by hash, if there is one.
	///
	/// Currently the ready transaction is returned if it exists for the most recently notified
	/// block (for which maintain process was accomplished).
	// todo [#5491]: api change: we probably should have at here?
	fn ready_transaction(&self, tx_hash: &TxHash<Self>) -> Option<Arc<Self::InPoolTransaction>> {
		let most_recent_view_hash =
			self.view_store.most_recent_view.read().as_ref().map(|v| v.at.hash);
		let result = most_recent_view_hash
			.and_then(|block_hash| self.view_store.ready_transaction(block_hash, tx_hash));
		trace!(
			target: LOG_TARGET,
			?tx_hash,
			is_ready = result.is_some(),
			most_recent_view = ?most_recent_view_hash,
			"ready_transaction"
		);
		result
	}

	/// Returns an iterator for ready transactions at a specific block, ordered by priority.
	async fn ready_at(&self, at: <Self::Block as BlockT>::Hash) -> ReadyIteratorFor<ChainApi> {
		let (_, result) = self.ready_at_internal(at);
		result.await
	}

	/// Returns an iterator for ready transactions, ordered by priority.
	///
	/// Currently the set of ready transactions is returned if it exists for the most recently
	/// notified best block (for which maintain process was accomplished).
	fn ready(&self) -> ReadyIteratorFor<ChainApi> {
		self.view_store.ready()
	}

	/// Returns a list of future transactions in the pool.
	///
	/// Currently the set of future transactions is returned if it exists for the most recently
	/// notified best block (for which maintain process was accomplished).
	fn futures(&self) -> Vec<Self::InPoolTransaction> {
		self.view_store.futures()
	}

	/// Returns a set of ready transactions at a given block within the specified timeout.
	///
	/// If the timeout expires before the maintain process is accomplished, a best-effort
	/// set of transactions is returned (refer to `ready_at_light`).
	async fn ready_at_with_timeout(
		&self,
		at: <Self::Block as BlockT>::Hash,
		timeout: std::time::Duration,
	) -> ReadyIteratorFor<ChainApi> {
		self.ready_at_with_timeout_internal(at, timeout).await
	}
}

impl<ChainApi, Block> sc_transaction_pool_api::LocalTransactionPool
	for ForkAwareTxPool<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: 'static + graph::ChainApi<Block = Block>,
	<Block as BlockT>::Hash: Unpin,
{
	type Block = Block;
	type Hash = ExtrinsicHash<ChainApi>;
	type Error = ChainApi::Error;

	fn submit_local(
		&self,
		at: Block::Hash,
		xt: sc_transaction_pool_api::LocalTransactionFor<Self>,
	) -> Result<Self::Hash, Self::Error> {
		trace!(
			target: LOG_TARGET,
			active_views_count = self.active_views_count(),
			"fatp::submit_local"
		);
		let xt = Arc::from(xt);
		let at_number = self
			.api
			.block_id_to_number(&BlockId::Hash(at))
			.ok()
			.flatten()
			.unwrap_or_default()
			.into()
			.as_u64();

		// note: would be nice to get rid of sync methods one day. See: #8912
		let result = self
			.mempool
			.clone()
			.extend_unwatched_sync(TransactionSource::Local, at_number, vec![xt.clone()])
			.remove(0);

		let insertion = match result {
			Err(TxPoolApiError::ImmediatelyDropped) => self.attempt_transaction_replacement_sync(
				TransactionSource::Local,
				false,
				xt.clone(),
			),
			_ => result,
		}?;

		self.view_store
			.submit_local(xt)
			.inspect_err(|_| {
				self.mempool.clone().remove_transactions_sync(vec![insertion.hash]);
			})
			.map(|outcome| {
				self.mempool
					.clone()
					.update_transaction_priority_sync(outcome.hash(), outcome.priority());
				outcome.hash()
			})
			.or_else(|_| Ok(insertion.hash))
	}
}

impl<ChainApi, Block> ForkAwareTxPool<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: graph::ChainApi<Block = Block> + 'static,
	<Block as BlockT>::Hash: Unpin,
{
	/// Handles a new block notification.
	///
	/// It is responsible for handling a newly notified block. It executes some sanity checks, find
	/// the best view to clone from and executes the new view build procedure for the notified
	/// block.
	///
	/// If the view is correctly created, `ready_at` pollers for this block will be triggered.
	#[instrument(level = Level::TRACE, skip_all, target = "txpool", name = "fatp::handle_new_block")]
	async fn handle_new_block(&self, tree_route: &TreeRoute<Block>) {
		let hash_and_number = match tree_route.last() {
			Some(hash_and_number) => hash_and_number,
			None => {
				warn!(
					target: LOG_TARGET,
					?tree_route,
					"Skipping ChainEvent - no last block in tree route"
				);
				return;
			},
		};

		if self.has_view(&hash_and_number.hash) {
			debug!(
				target: LOG_TARGET,
				?hash_and_number,
				"view already exists for block"
			);
			return;
		}

		let best_view = self.view_store.find_best_view(tree_route);
		let new_view = self.build_and_update_view(best_view, hash_and_number, tree_route).await;

		if let Some(view) = new_view {
			{
				let view = view.clone();
				self.ready_poll.lock().trigger(hash_and_number.hash, move || {
					Box::from(view.pool.validated_pool().ready())
				});
			}

			View::start_background_revalidation(view, self.revalidation_queue.clone()).await;
		}

		self.finality_stall_cleanup(hash_and_number).await;
	}

	/// Cleans up transactions and views outdated by potential finality stalls.
	///
	/// This function removes transactions from the pool that were included in blocks but not
	/// finalized within a pre-defined block height threshold. Transactions not meeting finality
	/// within this threshold are notified with finality timed out event. The threshold is based on
	/// the current block number, 'at'.
	///
	/// Additionally, this method triggers the view store to handle and remove stale views caused by
	/// the finality stall.
	async fn finality_stall_cleanup(&self, at: &HashAndNumber<Block>) {
		let (oldest_block_number, finality_timedout_blocks) = {
			let mut included_transactions = self.included_transactions.lock();

			let Some(oldest_block_number) =
				included_transactions.first_key_value().map(|(k, _)| k.number)
			else {
				return;
			};

			if at.number.saturating_sub(oldest_block_number).into() <=
				self.finality_timeout_threshold.into()
			{
				return;
			}

			let mut finality_timedout_blocks =
				indexmap::IndexMap::<BlockHash<ChainApi>, Vec<ExtrinsicHash<ChainApi>>>::default();

			included_transactions.retain(
				|HashAndNumber { number: view_number, hash: view_hash }, tx_hashes| {
					let diff = at.number.saturating_sub(*view_number);
					if diff.into() > self.finality_timeout_threshold.into() {
						finality_timedout_blocks.insert(*view_hash, std::mem::take(tx_hashes));
						false
					} else {
						true
					}
				},
			);

			(oldest_block_number, finality_timedout_blocks)
		};

		if !finality_timedout_blocks.is_empty() {
			self.ready_poll.lock().remove_cancelled();
			self.view_store.listener.remove_stale_controllers();
		}

		let finality_timedout_blocks_len = finality_timedout_blocks.len();

		for (block_hash, tx_hashes) in finality_timedout_blocks {
			self.view_store.listener.transactions_finality_timeout(&tx_hashes, block_hash);

			self.mempool.remove_transactions(&tx_hashes).await;
			self.import_notification_sink.clean_notified_items(&tx_hashes);
			self.view_store.dropped_stream_controller.remove_transactions(tx_hashes.clone());
		}

		self.view_store.finality_stall_view_cleanup(at, self.finality_timeout_threshold);

		debug!(
			target: LOG_TARGET,
			?at,
			included_transactions_len = ?self.included_transactions.lock().len(),
			finality_timedout_blocks_len,
			?oldest_block_number,
			"finality_stall_cleanup"
		);
	}

	/// Builds a new view.
	///
	/// If `origin_view` is provided, the new view will be cloned from it. Otherwise an empty view
	/// will be created.
	///
	/// This method will also update multi-view listeners with newly created view.
	///
	/// The new view will not be inserted into the view store.
	fn build_and_plug_view(
		&self,
		origin_view: Option<Arc<View<ChainApi>>>,
		at: &HashAndNumber<Block>,
		tree_route: &TreeRoute<Block>,
	) -> View<ChainApi> {
		let enter = Instant::now();
		let (view, view_dropped_stream, view_aggregated_stream) =
			if let Some(origin_view) = origin_view {
				let (mut view, view_dropped_stream, view_aggragated_stream) =
					View::new_from_other(&origin_view, at);
				if !tree_route.retracted().is_empty() {
					view.pool.clear_recently_pruned();
				}
				(view, view_dropped_stream, view_aggragated_stream)
			} else {
				debug!(
					target: LOG_TARGET,
					?at,
					"creating non-cloned view"
				);
				View::new(
					self.api.clone(),
					at.clone(),
					self.options.clone(),
					self.metrics.clone(),
					self.is_validator.clone(),
				)
			};
		debug!(
			target: LOG_TARGET,
			?at,
			duration = ?enter.elapsed(),
			"build_new_view::clone_view"
		);

		// 1. Capture all import notification from the very beginning, so first register all
		// the listeners.
		self.import_notification_sink.add_view(
			view.at.hash,
			view.pool.validated_pool().import_notification_stream().boxed(),
		);

		self.view_store
			.dropped_stream_controller
			.add_view(view.at.hash, view_dropped_stream.boxed());

		self.view_store
			.listener
			.add_view_aggregated_stream(view.at.hash, view_aggregated_stream.boxed());

		view
	}

	/// Builds and updates a new view.
	///
	/// This functio uses [`Self::build_new_view`] to create or clone new view.
	///
	/// The new view will be updated with transactions from the tree_route and the mempool, all
	/// required events will be triggered, it will be inserted to the view store (respecting all
	/// pre-insertion actions).
	async fn build_and_update_view(
		&self,
		origin_view: Option<Arc<View<ChainApi>>>,
		at: &HashAndNumber<Block>,
		tree_route: &TreeRoute<Block>,
	) -> Option<Arc<View<ChainApi>>> {
		let start = Instant::now();
		debug!(
			target: LOG_TARGET,
			?at,
			origin_view_at = ?origin_view.as_ref().map(|v| v.at.clone()),
			?tree_route,
			"build_new_view"
		);

		let mut view = self.build_and_plug_view(origin_view, at, tree_route);

		// sync the transactions statuses and referencing views in all the listeners with newly
		// cloned view.
		view.pool.validated_pool().retrigger_notifications();
		debug!(
			target: LOG_TARGET,
			?at,
			duration = ?start.elapsed(),
			"register_listeners"
		);

		// 2. Handle transactions from the tree route. Pruning transactions from the view first
		// will make some space for mempool transactions in case we are at the view's limits.
		let start = Instant::now();
		self.update_view_with_fork(&view, tree_route, at.clone()).await;
		debug!(
			target: LOG_TARGET,
			?at,
			duration = ?start.elapsed(),
			"update_view_with_fork"
		);

		// 3. Finally, submit transactions from the mempool.
		let start = Instant::now();
		self.update_view_with_mempool(&mut view).await;
		debug!(
			target: LOG_TARGET,
			?at,
			duration= ?start.elapsed(),
			"update_view_with_mempool"
		);
		let view = Arc::from(view);
		self.view_store.insert_new_view(view.clone(), tree_route).await;

		debug!(
			target: LOG_TARGET,
			duration = ?start.elapsed(),
			?at,
			"build_new_view"
		);
		Some(view)
	}

	/// Retrieves transactions hashes from a `included_transactions` cache or, if not present,
	/// fetches them from the blockchain API using the block's hash `at`.
	///
	/// Returns a `Vec` of transactions hashes
	async fn fetch_block_transactions(&self, at: &HashAndNumber<Block>) -> Vec<TxHash<Self>> {
		if let Some(txs) = self.included_transactions.lock().get(at) {
			return txs.clone();
		};

		debug!(
			target: LOG_TARGET,
			?at,
			"fetch_block_transactions from api"
		);

		self.api
			.block_body(at.hash)
			.await
			.unwrap_or_else(|error| {
				warn!(
					target: LOG_TARGET,
					%error,
					"fetch_block_transactions: error request"
				);
				None
			})
			.unwrap_or_default()
			.into_iter()
			.map(|t| self.hash_of(&t))
			.collect::<Vec<_>>()
	}

	/// Returns the list of xts included in all block's ancestors up to recently finalized block (or
	/// up finality timeout threshold), including the block itself.
	///
	/// Example: for the following chain `F<-B1<-B2<-B3` xts from `B1,B2,B3` will be returned.
	async fn txs_included_since_finalized(
		&self,
		at: &HashAndNumber<Block>,
	) -> HashSet<TxHash<Self>> {
		let start = Instant::now();
		let recent_finalized_block = self.enactment_state.lock().recent_finalized_block();

		let Ok(tree_route) = self.api.tree_route(recent_finalized_block, at.hash) else {
			return Default::default();
		};

		let mut all_txs = HashSet::new();

		for block in tree_route.enacted().iter() {
			// note: There is no point to fetch the transactions from blocks older than threshold.
			// All transactions included in these blocks, were already removed from pool
			// with FinalityTimeout event.
			if at.number.saturating_sub(block.number).into() <=
				self.finality_timeout_threshold.into()
			{
				all_txs.extend(self.fetch_block_transactions(block).await);
			}
		}

		debug!(
			target: LOG_TARGET,
			?at,
			?recent_finalized_block,
			extrinsics_count = all_txs.len(),
			duration = ?start.elapsed(),
			"fatp::txs_included_since_finalized"
		);
		all_txs
	}

	/// Updates the given view with the transactions from the internal mempol.
	///
	/// All transactions from the mempool (excluding those which are either already imported or
	/// already included in blocks since recently finalized block) are submitted to the
	/// view.
	///
	/// If there are no views, and mempool transaction is reported as invalid for the given view,
	/// the transaction is notified as invalid and removed from the mempool.
	async fn update_view_with_mempool(&self, view: &View<ChainApi>) {
		let xts_count = self.mempool.unwatched_and_watched_count().await;
		debug!(
			target: LOG_TARGET,
			view_at = ?view.at,
			?xts_count,
			active_views_count = self.active_views_count(),
			"update_view_with_mempool"
		);
		let included_xts = self.txs_included_since_finalized(&view.at).await;

		let view_hash = view.at.hash;
		let (hashes, xts_filtered): (Vec<_>, Vec<_>) = self
			.mempool
			.with_transactions(|iter| {
				iter.filter(|(hash, _)| {
					match view.imported_status(hash) {
						ImportedStatus::Banned => {
							trace!(
								target: LOG_TARGET,
								?hash,
								?view_hash,
								"update_view_with_mempool: skipped (temporarily banned)"
							);
							return false;
						},
						ImportedStatus::Imported => return false,
						ImportedStatus::NotImported => {},
					}
					!included_xts.contains(hash)
				})
				.map(|(k, v)| (*k, v.clone()))
				// todo [#8835]: better approach is needed - maybe time-budget approach?
				.take(MEMPOOL_TO_VIEW_BATCH_SIZE)
				.collect::<HashMap<_, _>>()
			})
			.await
			.into_iter()
			.map(|(tx_hash, tx)| (tx_hash, (tx.source(), tx.tx())))
			.unzip();

		let results = view
			.submit_many(xts_filtered, ValidateTransactionPriority::Maintained)
			.await
			.into_iter()
			.zip(hashes)
			.map(|(result, tx_hash)| async move {
				match result {
					Ok(outcome) => Ok(self
						.mempool
						.update_transaction_priority(outcome.hash(), outcome.priority())
						.await),
					Err(error) => {
						trace!(
							target: LOG_TARGET,
							?tx_hash,
							?error,
							"update_view_with_mempool: tx rejected from view"
						);
						Err(tx_hash)
					},
				}
			})
			.collect::<Vec<_>>();

		let results = futures::future::join_all(results).await;

		let submitted_count = results.len();

		debug!(
			target: LOG_TARGET,
			view_at_hash = ?view.at.hash,
			submitted_count,
			mempool_len = self.mempool.len(),
			"update_view_with_mempool"
		);

		self.metrics
			.report(|metrics| metrics.submitted_from_mempool_txs.inc_by(submitted_count as _));

		// if there are no views yet, and a single newly created view is reporting error, just send
		// out the invalid event, and remove transaction.
		if self.view_store.is_empty() {
			for result in results {
				if let Err(tx_hash) = result {
					self.view_store.listener.transactions_invalidated(&[tx_hash]);
					self.mempool.remove_transactions(&[tx_hash]).await;
				}
			}
		}
	}

	/// Attempts to search the view store for the `provides` tags of enacted
	/// transactions associated with the specified `tree_route`.
	///
	/// The 'provides' tags of transactions from enacted blocks are searched
	/// in inactive views. Found `provide` tags are intended to serve as cache,
	/// helping to avoid unnecessary revalidations during pruning.
	async fn collect_provides_tags_from_view_store(
		&self,
		tree_route: &TreeRoute<Block>,
		xts_hashes: Vec<ExtrinsicHash<ChainApi>>,
	) -> HashMap<ExtrinsicHash<ChainApi>, Vec<Tag>> {
		let blocks_hashes = tree_route
			.retracted()
			.iter()
			// Skip the tip of the retracted fork, since it has an active view.
			.skip(1)
			// Skip also the tip of the enacted fork, since it has an active view too.
			.chain(
				std::iter::once(tree_route.common_block())
					.chain(tree_route.enacted().iter().rev().skip(1)),
			)
			.collect::<Vec<&HashAndNumber<Block>>>();

		self.view_store.provides_tags_from_inactive_views(blocks_hashes, xts_hashes)
	}

	/// Build a map from blocks to their extrinsics.
	pub async fn collect_extrinsics(
		&self,
		blocks: &[HashAndNumber<Block>],
	) -> HashMap<Block::Hash, Vec<RawExtrinsicFor<ChainApi>>> {
		future::join_all(blocks.iter().map(|hn| async move {
			(
				hn.hash,
				self.api
					.block_body(hn.hash)
					.await
					.unwrap_or_else(|e| {
						warn!(target: LOG_TARGET, %e, ": block_body error request");
						None
					})
					.unwrap_or_default(),
			)
		}))
		.await
		.into_iter()
		.collect()
	}

	/// Updates the view with the transactions from the given tree route.
	///
	/// Transactions from the retracted blocks are resubmitted to the given view. Tags for
	/// transactions included in blocks on enacted fork are pruned from the provided view.
	async fn update_view_with_fork(
		&self,
		view: &View<ChainApi>,
		tree_route: &TreeRoute<Block>,
		hash_and_number: HashAndNumber<Block>,
	) {
		debug!(
			target: LOG_TARGET,
			?tree_route,
			at = ?view.at,
			"update_view_with_fork"
		);
		let api = self.api.clone();

		// Collect extrinsics on the enacted path in a map from block hn -> extrinsics.
		let mut extrinsics = self.collect_extrinsics(tree_route.enacted()).await;

		// Create a map from enacted blocks' extrinsics to their `provides`
		// tags based on inactive views.
		let known_provides_tags = Arc::new(
			self.collect_provides_tags_from_view_store(
				tree_route,
				extrinsics.values().flatten().map(|tx| view.pool.hash_of(tx)).collect(),
			)
			.await,
		);

		debug!(target: LOG_TARGET, "update_view_with_fork: txs to tags map length: {}", known_provides_tags.len());

		// We keep track of everything we prune so that later we won't add
		// transactions with those hashes from the retracted blocks.
		let mut pruned_log = HashSet::<ExtrinsicHash<ChainApi>>::new();
		future::join_all(tree_route.enacted().iter().map(|hn| {
			let api = api.clone();
			let xts = extrinsics.remove(&hn.hash).unwrap_or_default();
			let known_provides_tags = known_provides_tags.clone();
			async move {
				(
					hn,
					crate::prune_known_txs_for_block(
						hn,
						&*api,
						&view.pool,
						Some(xts),
						Some(known_provides_tags),
					)
					.await,
				)
			}
		}))
		.await
		.into_iter()
		.for_each(|(key, enacted_log)| {
			pruned_log.extend(enacted_log.clone());
			self.included_transactions.lock().insert(key.clone(), enacted_log);
		});

		let unknown_count = self.mempool.count_unknown_transactions(pruned_log.iter()).await;
		self.metrics
			.report(|metrics| metrics.unknown_from_block_import_txs.inc_by(unknown_count as _));

		// resubmit
		{
			let mut resubmit_transactions = Vec::new();

			for retracted in tree_route.retracted().iter().rev() {
				let hash = retracted.hash;

				let block_transactions = api
					.block_body(hash)
					.await
					.unwrap_or_else(|error| {
						warn!(
							target: LOG_TARGET,
							%error,
							"Failed to fetch block body"
						);
						None
					})
					.unwrap_or_default()
					.into_iter();

				let mut resubmitted_to_report = 0;

				let txs = block_transactions.into_iter().map(|tx| (self.hash_of(&tx), tx)).filter(
					|(tx_hash, _)| {
						let contains = pruned_log.contains(&tx_hash);

						// need to count all transactions, not just filtered, here
						resubmitted_to_report += 1;

						if !contains {
							trace!(
								target: LOG_TARGET,
								?tx_hash,
								?hash,
								"Resubmitting from retracted block"
							);
						}
						!contains
					},
				);
				let mut result = vec![];
				for (tx_hash, tx) in txs {
					result.push(
						// find arc if tx is known
						self.mempool
							.get_by_hash(tx_hash)
							.await
							.map(|tx| (tx.source(), tx.tx()))
							.unwrap_or_else(|| {
								// These transactions are coming from retracted blocks, we
								// should simply consider them external.
								(TimedTransactionSource::new_external(true), Arc::from(tx))
							}),
					);
				}
				resubmit_transactions.extend(result);

				self.metrics.report(|metrics| {
					metrics.resubmitted_retracted_txs.inc_by(resubmitted_to_report)
				});
			}

			let _ = view
				.pool
				.resubmit_at(
					&hash_and_number,
					resubmit_transactions,
					ValidateTransactionPriority::Maintained,
				)
				.await;
		}
	}

	/// Executes the maintainance for the finalized event.
	///
	/// Performs a house-keeping required for finalized event. This includes:
	/// - executing the on finalized procedure for the view store,
	/// - purging finalized transactions from the mempool and triggering mempool revalidation,
	async fn handle_finalized(&self, finalized_hash: Block::Hash, tree_route: &[Block::Hash]) {
		let start = Instant::now();
		let finalized_number = self.api.block_id_to_number(&BlockId::Hash(finalized_hash));
		debug!(
			target: LOG_TARGET,
			?finalized_number,
			?tree_route,
			active_views_count = self.active_views_count(),
			"handle_finalized"
		);
		let finalized_xts = self.view_store.handle_finalized(finalized_hash, tree_route).await;

		self.mempool.purge_finalized_transactions(&finalized_xts).await;
		self.import_notification_sink.clean_notified_items(&finalized_xts);

		self.metrics
			.report(|metrics| metrics.finalized_txs.inc_by(finalized_xts.len() as _));

		if let Ok(Some(finalized_number)) = finalized_number {
			self.included_transactions
				.lock()
				.retain(|cached_block, _| finalized_number < cached_block.number);
			self.revalidation_queue
				.revalidate_mempool(
					self.mempool.clone(),
					self.view_store.clone(),
					HashAndNumber { hash: finalized_hash, number: finalized_number },
				)
				.await;
		} else {
			debug!(
				target: LOG_TARGET,
				?finalized_number,
				"handle_finalized: revalidation/cleanup skipped: could not resolve finalized block number"
			);
		}

		self.ready_poll.lock().remove_cancelled();

		debug!(
			target: LOG_TARGET,
			active_views_count = self.active_views_count(),
			included_transactions_len = ?self.included_transactions.lock().len(),
			duration = ?start.elapsed(),
			"handle_finalized after"
		);
	}

	/// Computes a hash of the provided transaction
	fn tx_hash(&self, xt: &TransactionFor<Self>) -> TxHash<Self> {
		self.api.hash_and_length(xt).0
	}

	/// Attempts to find and replace a lower-priority transaction in the transaction pool with a new
	/// one.
	///
	/// This asynchronous function verifies the new transaction against the most recent view. If a
	/// transaction with a lower priority exists in the transaction pool, it is replaced with the
	/// new transaction.
	///
	/// If no lower-priority transaction is found, the function returns an error indicating the
	/// transaction was dropped immediately.
	#[instrument(level = Level::TRACE, skip_all, target = "txpool", name = "fatp::attempt_transaction_replacement")]
	async fn attempt_transaction_replacement(
		&self,
		source: TransactionSource,
		at_number: u64,
		watched: bool,
		xt: ExtrinsicFor<ChainApi>,
	) -> Result<InsertionInfo<ExtrinsicHash<ChainApi>>, TxPoolApiError> {
		let best_view = self
			.view_store
			.most_recent_view
			.read()
			.as_ref()
			.ok_or(TxPoolApiError::ImmediatelyDropped)?
			.clone();

		let (xt_hash, validated_tx) = best_view
			.pool
			.verify_one(
				best_view.at.hash,
				best_view.at.number,
				TimedTransactionSource::from_transaction_source(source, false),
				xt.clone(),
				crate::graph::CheckBannedBeforeVerify::Yes,
				ValidateTransactionPriority::Submitted,
			)
			.await;

		let Some(priority) = validated_tx.priority() else {
			return Err(TxPoolApiError::ImmediatelyDropped);
		};

		let insertion_info = self
			.mempool
			.try_insert_with_replacement(xt, priority, source, at_number, watched)
			.await?;
		self.post_attempt_transaction_replacement(xt_hash, insertion_info)
	}

	/// Sync version of [`Self::attempt_transaction_replacement`].
	fn attempt_transaction_replacement_sync(
		&self,
		source: TransactionSource,
		watched: bool,
		xt: ExtrinsicFor<ChainApi>,
	) -> Result<InsertionInfo<ExtrinsicHash<ChainApi>>, TxPoolApiError> {
		let HashAndNumber { number: at_number, hash: at_hash } = self
			.view_store
			.most_recent_view
			.read()
			.as_ref()
			.ok_or(TxPoolApiError::ImmediatelyDropped)?
			.at;

		let ValidTransaction { priority, .. } = self
			.api
			.validate_transaction_blocking(at_hash, TransactionSource::Local, Arc::from(xt.clone()))
			.map_err(|_| TxPoolApiError::ImmediatelyDropped)?
			.map_err(|e| match e {
				TransactionValidityError::Invalid(i) => TxPoolApiError::InvalidTransaction(i),
				TransactionValidityError::Unknown(u) => TxPoolApiError::UnknownTransaction(u),
			})?;
		let xt_hash = self.hash_of(&xt);

		let insertion_info = self.mempool.clone().try_insert_with_replacement_sync(
			xt,
			priority,
			source,
			at_number.into().as_u64(),
			watched,
		)?;
		self.post_attempt_transaction_replacement(xt_hash, insertion_info)
	}

	fn post_attempt_transaction_replacement(
		&self,
		tx_hash: ExtrinsicHash<ChainApi>,
		insertion_info: InsertionInfo<ExtrinsicHash<ChainApi>>,
	) -> Result<InsertionInfo<ExtrinsicHash<ChainApi>>, TxPoolApiError> {
		for worst_hash in &insertion_info.removed {
			trace!(
				target: LOG_TARGET,
				tx_hash = ?worst_hash,
				new_tx_hash = ?tx_hash,
				"removed: replaced by"
			);
			self.view_store
				.listener
				.transaction_dropped(DroppedTransaction::new_enforced_by_limts(*worst_hash));

			self.view_store
				.remove_transaction_subtree(*worst_hash, |listener, removed_tx_hash| {
					listener.limits_enforced(&removed_tx_hash);
				});
		}

		return Ok(insertion_info);
	}
}

#[async_trait]
impl<ChainApi, Block> MaintainedTransactionPool for ForkAwareTxPool<ChainApi, Block>
where
	Block: BlockT,
	ChainApi: 'static + graph::ChainApi<Block = Block>,
	<Block as BlockT>::Hash: Unpin,
{
	/// Executes the maintainance for the given chain event.
	async fn maintain(&self, event: ChainEvent<Self::Block>) {
		let start = Instant::now();
		debug!(
			target: LOG_TARGET,
			?event,
			"processing event"
		);

		self.view_store.finish_background_revalidations().await;

		let prev_finalized_block = self.enactment_state.lock().recent_finalized_block();

		let compute_tree_route = |from, to| -> Result<TreeRoute<Block>, String> {
			match self.api.tree_route(from, to) {
				Ok(tree_route) => Ok(tree_route),
				Err(e) => {
					return Err(format!(
						"Error occurred while computing tree_route from {from:?} to {to:?}: {e}"
					))
				},
			}
		};
		let block_id_to_number =
			|hash| self.api.block_id_to_number(&BlockId::Hash(hash)).map_err(|e| format!("{}", e));

		let result =
			self.enactment_state
				.lock()
				.update(&event, &compute_tree_route, &block_id_to_number);

		match result {
			Err(error) => {
				debug!(
					target: LOG_TARGET,
					%error,
					"enactment_state::update error"
				);
				self.enactment_state.lock().force_update(&event);
			},
			Ok(EnactmentAction::Skip) => return,
			Ok(EnactmentAction::HandleFinalization) => {
				// todo [#5492]: in some cases handle_new_block is actually needed (new_num >
				// tips_of_forks) let hash = event.hash();
				// if !self.has_view(hash) {
				// 	if let Ok(tree_route) = compute_tree_route(prev_finalized_block, hash) {
				// 		self.handle_new_block(&tree_route).await;
				// 	}
				// }
			},
			Ok(EnactmentAction::HandleEnactment(tree_route)) => {
				self.handle_new_block(&tree_route).await;
			},
		};

		match event {
			ChainEvent::NewBlock { .. } | ChainEvent::NewBestBlock { .. } => {},
			ChainEvent::Finalized { hash, ref tree_route } => {
				self.handle_finalized(hash, tree_route).await;

				debug!(
					target: LOG_TARGET,
					?tree_route,
					?prev_finalized_block,
					"on-finalized enacted"
				);
			},
		}

		let duration = start.elapsed();
		let mempool_len = self.mempool_len().await;
		debug!(
			target: LOG_TARGET,
			txs = ?mempool_len,
			a = self.active_views_count(),
			i = self.inactive_views_count(),
			views = ?self.views_stats(),
			?event,
			?duration,
			"maintain"
		);

		self.metrics.report(|metrics| {
			let (unwatched, watched) = mempool_len;
			let _ = (
				self.active_views_count().try_into().map(|v| metrics.active_views.set(v)),
				self.inactive_views_count().try_into().map(|v| metrics.inactive_views.set(v)),
				watched.try_into().map(|v| metrics.watched_txs.set(v)),
				unwatched.try_into().map(|v| metrics.unwatched_txs.set(v)),
			);
			metrics.maintain_duration.observe(duration.as_secs_f64());
		});
	}
}

impl<Block, Client> ForkAwareTxPool<FullChainApi<Client, Block>, Block>
where
	Block: BlockT,
	Client: sp_api::ProvideRuntimeApi<Block>
		+ sc_client_api::BlockBackend<Block>
		+ sc_client_api::blockchain::HeaderBackend<Block>
		+ sp_runtime::traits::BlockIdTo<Block>
		+ sc_client_api::ExecutorProvider<Block>
		+ sc_client_api::UsageProvider<Block>
		+ sp_blockchain::HeaderMetadata<Block, Error = sp_blockchain::Error>
		+ Send
		+ Sync
		+ 'static,
	Client::Api: sp_transaction_pool::runtime_api::TaggedTransactionQueue<Block>,
	<Block as BlockT>::Hash: std::marker::Unpin,
{
	/// Create new fork aware transaction pool for a full node with the provided api.
	pub fn new_full(
		options: Options,
		is_validator: IsValidator,
		prometheus: Option<&PrometheusRegistry>,
		spawner: impl SpawnEssentialNamed,
		client: Arc<Client>,
	) -> Self {
		let pool_api = Arc::new(FullChainApi::new(client.clone(), prometheus, &spawner));
		let pool = Self::new_with_background_worker(
			options,
			is_validator,
			pool_api,
			prometheus,
			spawner,
			client.usage_info().chain.best_hash,
			client.usage_info().chain.finalized_hash,
		);

		pool
	}
}

#[cfg(test)]
mod reduce_multiview_result_tests {
	use super::*;
	use sp_core::H256;
	#[derive(Debug, PartialEq, Clone)]
	enum Error {
		Custom(u8),
	}

	#[test]
	fn empty() {
		sp_tracing::try_init_simple();
		let input = HashMap::default();
		let r = reduce_multiview_result::<H256, H256, Error>(input);
		assert!(r.is_empty());
	}

	#[test]
	fn errors_only() {
		sp_tracing::try_init_simple();
		let v: Vec<(H256, Vec<Result<H256, Error>>)> = vec![
			(
				H256::repeat_byte(0x13),
				vec![
					Err(Error::Custom(10)),
					Err(Error::Custom(11)),
					Err(Error::Custom(12)),
					Err(Error::Custom(13)),
				],
			),
			(
				H256::repeat_byte(0x14),
				vec![
					Err(Error::Custom(20)),
					Err(Error::Custom(21)),
					Err(Error::Custom(22)),
					Err(Error::Custom(23)),
				],
			),
			(
				H256::repeat_byte(0x15),
				vec![
					Err(Error::Custom(30)),
					Err(Error::Custom(31)),
					Err(Error::Custom(32)),
					Err(Error::Custom(33)),
				],
			),
		];
		let input = HashMap::from_iter(v.clone());
		let r = reduce_multiview_result(input);

		// order in HashMap is random, the result shall be one of:
		assert!(r == v[0].1 || r == v[1].1 || r == v[2].1);
	}

	#[test]
	#[should_panic]
	#[cfg(debug_assertions)]
	fn invalid_lengths() {
		sp_tracing::try_init_simple();
		let v: Vec<(H256, Vec<Result<H256, Error>>)> = vec![
			(H256::repeat_byte(0x13), vec![Err(Error::Custom(12)), Err(Error::Custom(13))]),
			(H256::repeat_byte(0x14), vec![Err(Error::Custom(23))]),
		];
		let input = HashMap::from_iter(v);
		let _ = reduce_multiview_result(input);
	}

	#[test]
	fn only_hashes() {
		sp_tracing::try_init_simple();

		let v: Vec<(H256, Vec<Result<H256, Error>>)> = vec![
			(
				H256::repeat_byte(0x13),
				vec![Ok(H256::repeat_byte(0x13)), Ok(H256::repeat_byte(0x14))],
			),
			(
				H256::repeat_byte(0x14),
				vec![Ok(H256::repeat_byte(0x13)), Ok(H256::repeat_byte(0x14))],
			),
		];
		let input = HashMap::from_iter(v);
		let r = reduce_multiview_result(input);

		assert_eq!(r, vec![Ok(H256::repeat_byte(0x13)), Ok(H256::repeat_byte(0x14))]);
	}

	#[test]
	fn one_view() {
		sp_tracing::try_init_simple();
		let v: Vec<(H256, Vec<Result<H256, Error>>)> = vec![(
			H256::repeat_byte(0x13),
			vec![Ok(H256::repeat_byte(0x10)), Err(Error::Custom(11))],
		)];
		let input = HashMap::from_iter(v);
		let r = reduce_multiview_result(input);

		assert_eq!(r, vec![Ok(H256::repeat_byte(0x10)), Err(Error::Custom(11))]);
	}

	#[test]
	fn mix() {
		sp_tracing::try_init_simple();
		let v: Vec<(H256, Vec<Result<H256, Error>>)> = vec![
			(
				H256::repeat_byte(0x13),
				vec![
					Ok(H256::repeat_byte(0x10)),
					Err(Error::Custom(11)),
					Err(Error::Custom(12)),
					Err(Error::Custom(33)),
				],
			),
			(
				H256::repeat_byte(0x14),
				vec![
					Err(Error::Custom(20)),
					Ok(H256::repeat_byte(0x21)),
					Err(Error::Custom(22)),
					Err(Error::Custom(33)),
				],
			),
			(
				H256::repeat_byte(0x15),
				vec![
					Err(Error::Custom(30)),
					Err(Error::Custom(31)),
					Ok(H256::repeat_byte(0x32)),
					Err(Error::Custom(33)),
				],
			),
		];
		let input = HashMap::from_iter(v);
		let r = reduce_multiview_result(input);

		assert_eq!(
			r,
			vec![
				Ok(H256::repeat_byte(0x10)),
				Ok(H256::repeat_byte(0x21)),
				Ok(H256::repeat_byte(0x32)),
				Err(Error::Custom(33))
			]
		);
	}
}
