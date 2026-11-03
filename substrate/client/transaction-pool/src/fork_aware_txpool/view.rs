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

//! Transaction pool view.
//!
//! The View represents the state of the transaction pool at given block. The view is created when
//! new block is notified to transaction pool. Views are removed on finalization.
//!
//! Refer to [*View*](../index.html#view) section for more details.

use super::metrics::MetricsLink as PrometheusMetrics;
use crate::{
	common::tracing_log_xt::log_xt_trace,
	graph::{
		self, base_pool::TimedTransactionSource, BlockHash, ExtrinsicFor, ExtrinsicHash,
		IsValidator, TransactionFor, ValidateTransactionPriority, ValidatedPoolSubmitOutcome,
		ValidatedTransaction, ValidatedTransactionFor,
	},
	LOG_TARGET,
};
use indexmap::IndexMap;
use parking_lot::Mutex;
use sc_transaction_pool_api::{error::Error as TxPoolError, PoolStatus, TransactionStatus};
use sc_utils::mpsc::{tracing_unbounded, TracingUnboundedReceiver, TracingUnboundedSender};
use sp_blockchain::HashAndNumber;
use sp_runtime::{
	generic::BlockId, traits::Block as BlockT, transaction_validity::TransactionValidityError,
	SaturatedConversion,
};
use std::{sync::Arc, time::Instant};
use tracing::{debug, instrument, trace, Level};

/// Status of a transaction with respect to a view.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ImportedStatus {
	/// Transaction is not known to the view and can be submitted.
	NotImported,
	/// Transaction is temporarily banned in the view's rotator.
	Banned,
	/// Transaction is already imported into the view's pool.
	Imported,
}

impl ImportedStatus {
	/// Returns `true` if the transaction is known to the view (either imported or banned).
	pub(super) fn is_imported(&self) -> bool {
		!matches!(self, ImportedStatus::NotImported)
	}
}

pub(super) struct RevalidationResult<ChainApi: graph::ChainApi> {
	revalidated: IndexMap<ExtrinsicHash<ChainApi>, ValidatedTransactionFor<ChainApi>>,
	invalid_hashes: Vec<ExtrinsicHash<ChainApi>>,
}

/// Used to obtain result from RevalidationWorker on View side.
pub(super) type RevalidationResultReceiver<ChainApi> =
	tokio::sync::mpsc::Receiver<RevalidationResult<ChainApi>>;

/// Used to send revalidation result from RevalidationWorker to View.
pub(super) type RevalidationResultSender<ChainApi> =
	tokio::sync::mpsc::Sender<RevalidationResult<ChainApi>>;

/// Used to receive finish-revalidation-request from View on RevalidationWorker side.
pub(super) type FinishRevalidationRequestReceiver = tokio::sync::mpsc::Receiver<()>;

/// Used to send finish-revalidation-request from View to RevalidationWorker.
pub(super) type FinishRevalidationRequestSender = tokio::sync::mpsc::Sender<()>;

/// Endpoints of channels used on View side (maintain thread)
pub(super) struct FinishRevalidationLocalChannels<ChainApi: graph::ChainApi> {
	/// Used to send finish revalidation request.
	finish_revalidation_request_tx: Option<FinishRevalidationRequestSender>,
	/// Used to receive revalidation results.
	revalidation_result_rx: RevalidationResultReceiver<ChainApi>,
}

impl<ChainApi: graph::ChainApi> FinishRevalidationLocalChannels<ChainApi> {
	/// Creates a new instance of endpoints for channels used on View side
	pub fn new(
		finish_revalidation_request_tx: FinishRevalidationRequestSender,
		revalidation_result_rx: RevalidationResultReceiver<ChainApi>,
	) -> Self {
		Self {
			finish_revalidation_request_tx: Some(finish_revalidation_request_tx),
			revalidation_result_rx,
		}
	}

	/// Removes a finish revalidation sender
	///
	/// Should be called when revalidation was already terminated and finish revalidation message is
	/// no longer expected.
	fn remove_sender(&mut self) {
		self.finish_revalidation_request_tx = None;
	}
}

/// Endpoints of channels used on `RevalidationWorker` side (background thread)
pub(super) struct FinishRevalidationWorkerChannels<ChainApi: graph::ChainApi> {
	/// Used to receive finish revalidation request.
	finish_revalidation_request_rx: FinishRevalidationRequestReceiver,
	/// Used to send revalidation results.
	revalidation_result_tx: RevalidationResultSender<ChainApi>,
}

impl<ChainApi: graph::ChainApi> FinishRevalidationWorkerChannels<ChainApi> {
	/// Creates a new instance of endpoints for channels used on `RevalidationWorker` side
	pub fn new(
		finish_revalidation_request_rx: FinishRevalidationRequestReceiver,
		revalidation_result_tx: RevalidationResultSender<ChainApi>,
	) -> Self {
		Self { finish_revalidation_request_rx, revalidation_result_tx }
	}
}

/// Single event used in aggregated stream. Tuple containing hash of transactions and its status.
pub(super) type TransactionStatusEvent<H, BH> = (H, TransactionStatus<H, BH>);
/// Warning threshold for (unbounded) channel used in aggregated view's streams.
const VIEW_STREAM_WARN_THRESHOLD: usize = 100_000;

/// Stream of events providing statuses of all the transactions within the pool.
pub(super) type AggregatedStream<H, BH> = TracingUnboundedReceiver<TransactionStatusEvent<H, BH>>;

/// Type alias for a stream of events intended to track dropped transactions.
type DroppedMonitoringStream<H, BH> = TracingUnboundedReceiver<TransactionStatusEvent<H, BH>>;

/// Notification handler for transactions updates triggered in `ValidatedPool`.
///
/// `ViewPoolObserver` handles transaction status changes notifications coming from an instance of
/// validated pool associated with the `View` and forwards them through specified channels
/// into the View's streams.
pub(super) struct ViewPoolObserver<ChainApi: graph::ChainApi> {
	/// The sink used to notify dropped by enforcing limits or by being usurped, or invalid
	/// transactions.
	///
	/// Note: Ready and future statuses are alse communicated through this channel, enabling the
	/// stream consumer to track views that reference the transaction.
	dropped_stream_sink: TracingUnboundedSender<
		TransactionStatusEvent<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>,
	>,

	/// The sink of the single, merged stream providing updates for all the transactions in the
	/// associated pool.
	///
	/// Note: some of the events which are currently ignored on the other side of this channel
	/// (external watcher) are not relayed.
	aggregated_stream_sink: TracingUnboundedSender<
		TransactionStatusEvent<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>,
	>,
}

impl<C: graph::ChainApi> graph::EventHandler<C> for ViewPoolObserver<C> {
	// note: skipped, notified by ForkAwareTxPool directly to multi view listener.
	fn broadcasted(&self, _: ExtrinsicHash<C>, _: Vec<String>) {}
	fn dropped(&self, _: ExtrinsicHash<C>) {}
	fn finalized(&self, _: ExtrinsicHash<C>, _: BlockHash<C>, _: usize) {}
	fn retracted(&self, _: ExtrinsicHash<C>, _: BlockHash<C>) {
		// note: [#5479], we do not send to aggregated stream.
	}

	fn ready(&self, tx: ExtrinsicHash<C>) {
		let status = TransactionStatus::Ready;
		self.send_to_dropped_stream_sink(tx, status.clone());
		self.send_to_aggregated_stream_sink(tx, status);
	}

	fn future(&self, tx: ExtrinsicHash<C>) {
		let status = TransactionStatus::Future;
		self.send_to_dropped_stream_sink(tx, status.clone());
		self.send_to_aggregated_stream_sink(tx, status);
	}

	fn limits_enforced(&self, tx: ExtrinsicHash<C>) {
		self.send_to_dropped_stream_sink(tx, TransactionStatus::Dropped);
	}

	fn usurped(&self, tx: ExtrinsicHash<C>, by: ExtrinsicHash<C>) {
		self.send_to_dropped_stream_sink(tx, TransactionStatus::Usurped(by));
	}

	fn invalid(&self, tx: ExtrinsicHash<C>) {
		self.send_to_dropped_stream_sink(tx, TransactionStatus::Invalid);
	}

	fn pruned(&self, tx: ExtrinsicHash<C>, block_hash: BlockHash<C>, tx_index: usize) {
		self.send_to_aggregated_stream_sink(tx, TransactionStatus::InBlock((block_hash, tx_index)));
	}

	fn finality_timeout(&self, tx: ExtrinsicHash<C>, hash: BlockHash<C>) {
		// todo: do we need this? [related issue: #5482]
		self.send_to_aggregated_stream_sink(tx, TransactionStatus::FinalityTimeout(hash));
	}
}

impl<ChainApi: graph::ChainApi> ViewPoolObserver<ChainApi> {
	/// Creates an instance of `ViewPoolObserver` together with associated view's streams.
	///
	/// This methods creates an event handler that shall be registered in the `ValidatedPool`
	/// instance associated with the view. It also creates new view's streams:
	/// - a single stream intended to watch dropped transactions only. The stream can be used to
	///   subscribe to events related to dropping of all extrinsics in the pool.
	/// - a single merged stream for all extrinsics in the associated pool. The stream can be used
	/// to subscribe to life-cycle events of all extrinsics in the pool. For fork-aware
	/// pool implementation this approach seems to be more efficient than using individual
	/// streams for every transaction.
	fn new() -> (
		Self,
		DroppedMonitoringStream<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>,
		AggregatedStream<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>,
	) {
		let (dropped_stream_sink, dropped_stream) =
			tracing_unbounded("mpsc_txpool_watcher", VIEW_STREAM_WARN_THRESHOLD);
		let (aggregated_stream_sink, aggregated_stream) =
			tracing_unbounded("mpsc_txpool_aggregated_stream", VIEW_STREAM_WARN_THRESHOLD);

		(Self { dropped_stream_sink, aggregated_stream_sink }, dropped_stream, aggregated_stream)
	}

	/// Sends given event to the `dropped_stream_sink`.
	fn send_to_dropped_stream_sink(
		&self,
		tx: ExtrinsicHash<ChainApi>,
		status: TransactionStatus<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>,
	) {
		if let Err(e) = self.dropped_stream_sink.unbounded_send((tx, status.clone())) {
			trace!(target: LOG_TARGET, "[{:?}] dropped_sink: {:?} send message failed: {:?}", tx, status, e);
		}
	}

	/// Sends given event to the `aggregated_stream_sink`.
	fn send_to_aggregated_stream_sink(
		&self,
		tx: ExtrinsicHash<ChainApi>,
		status: TransactionStatus<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>,
	) {
		if let Err(e) = self.aggregated_stream_sink.unbounded_send((tx, status.clone())) {
			trace!(target: LOG_TARGET, "[{:?}] aggregated_stream {:?} send message failed: {:?}", tx, status, e);
		}
	}
}

/// Represents the state of transaction pool for given block.
///
/// Refer to [*View*](../index.html#view) section for more details on the purpose and life cycle of
/// the `View`.
pub(super) struct View<ChainApi: graph::ChainApi> {
	/// The internal pool keeping the set of ready and future transaction at the given block.
	pub(super) pool: graph::Pool<ChainApi, ViewPoolObserver<ChainApi>>,
	/// The hash and number of the block with which this view is associated.
	pub(super) at: HashAndNumber<ChainApi::Block>,
	/// Endpoints of communication channel with background worker.
	revalidation_worker_channels: Mutex<Option<FinishRevalidationLocalChannels<ChainApi>>>,
	/// Prometheus's metrics endpoint.
	metrics: PrometheusMetrics,
}

impl<ChainApi> View<ChainApi>
where
	ChainApi: graph::ChainApi,
	<ChainApi::Block as BlockT>::Hash: Unpin,
{
	/// Creates a new empty view.
	pub(super) fn new(
		api: Arc<ChainApi>,
		at: HashAndNumber<ChainApi::Block>,
		options: graph::Options,
		metrics: PrometheusMetrics,
		is_validator: IsValidator,
	) -> (
		Self,
		DroppedMonitoringStream<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>,
		AggregatedStream<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>,
	) {
		metrics.report(|metrics| metrics.non_cloned_views.inc());
		let (event_handler, dropped_stream, aggregated_stream) = ViewPoolObserver::new();
		(
			Self {
				pool: graph::Pool::new_with_event_handler(
					options,
					is_validator,
					api,
					event_handler,
				),
				at,
				revalidation_worker_channels: Mutex::from(None),
				metrics,
			},
			dropped_stream,
			aggregated_stream,
		)
	}

	/// Creates a copy of the other view.
	pub(super) fn new_from_other(
		&self,
		at: &HashAndNumber<ChainApi::Block>,
	) -> (
		Self,
		DroppedMonitoringStream<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>,
		AggregatedStream<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>,
	) {
		let (event_handler, dropped_stream, aggregated_stream) = ViewPoolObserver::new();
		(
			View {
				at: at.clone(),
				pool: self.pool.deep_clone_with_event_handler(event_handler),
				revalidation_worker_channels: Mutex::from(None),
				metrics: self.metrics.clone(),
			},
			dropped_stream,
			aggregated_stream,
		)
	}

	/// Imports single unvalidated extrinsic into the view.
	#[instrument(level = Level::TRACE, skip_all, target = "txpool", name = "view::submit_one")]
	pub(super) async fn submit_one(
		&self,
		source: TimedTransactionSource,
		xt: ExtrinsicFor<ChainApi>,
		validation_priority: ValidateTransactionPriority,
	) -> Result<ValidatedPoolSubmitOutcome<ChainApi>, ChainApi::Error> {
		self.submit_many(std::iter::once((source, xt)), validation_priority)
			.await
			.pop()
			.expect("There is exactly one result, qed.")
	}

	/// Imports many unvalidated extrinsics into the view.
	#[instrument(level = Level::TRACE, skip_all, target = "txpool", name = "view::submit_many")]
	pub(super) async fn submit_many(
		&self,
		xts: impl IntoIterator<Item = (TimedTransactionSource, ExtrinsicFor<ChainApi>)>,
		validation_priority: ValidateTransactionPriority,
	) -> Vec<Result<ValidatedPoolSubmitOutcome<ChainApi>, ChainApi::Error>> {
		if tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE) {
			let xts = xts.into_iter().collect::<Vec<_>>();
			log_xt_trace!(
				target: LOG_TARGET,
				xts.iter().map(|(_,xt)| self.pool.validated_pool().api().hash_and_length(xt).0),
				"view::submit_many at:{}",
				self.at.hash
			);
			self.pool.submit_at(&self.at, xts, validation_priority).await
		} else {
			self.pool.submit_at(&self.at, xts, validation_priority).await
		}
	}

	/// Synchronously imports single unvalidated extrinsics into the view.
	pub(super) fn submit_local(
		&self,
		xt: ExtrinsicFor<ChainApi>,
	) -> Result<ValidatedPoolSubmitOutcome<ChainApi>, ChainApi::Error> {
		let (tx_hash, length) = self.pool.validated_pool().api().hash_and_length(&xt);
		trace!(
			target: LOG_TARGET,
			?tx_hash,
			view_at_hash = ?self.at.hash,
			"view::submit_local"
		);
		let validity = self
			.pool
			.validated_pool()
			.api()
			.validate_transaction_blocking(
				self.at.hash,
				sc_transaction_pool_api::TransactionSource::Local,
				Arc::from(xt.clone()),
			)?
			.map_err(|e| {
				match e {
					TransactionValidityError::Invalid(i) => TxPoolError::InvalidTransaction(i),
					TransactionValidityError::Unknown(u) => TxPoolError::UnknownTransaction(u),
				}
				.into()
			})?;

		let block_number = self
			.pool
			.validated_pool()
			.api()
			.block_id_to_number(&BlockId::hash(self.at.hash))?
			.ok_or_else(|| TxPoolError::InvalidBlockId(format!("{:?}", self.at.hash)))?;

		let validated = ValidatedTransaction::valid_at(
			block_number.saturated_into::<u64>(),
			tx_hash,
			TimedTransactionSource::new_local(true),
			Arc::from(xt),
			length,
			validity,
		);

		self.pool.validated_pool().submit(vec![validated]).remove(0)
	}

	/// Status of the pool associated with the view.
	pub(super) fn status(&self) -> PoolStatus {
		self.pool.validated_pool().status()
	}

	/// Revalidates some part of transaction from the internal pool.
	///
	/// Intended to be called from the revalidation worker. The revalidation process can be
	/// terminated by sending a message to the `rx` channel provided within
	/// `finish_revalidation_worker_channels`. Revalidation results are sent back over the `tx`
	/// channels and shall be applied in maintain thread.
	///
	/// View revalidation currently is not throttled, and until not terminated it will revalidate
	/// all the transactions. Note: this can be improved if CPU usage due to revalidation becomes a
	/// problem.
	pub(super) async fn revalidate(
		&self,
		finish_revalidation_worker_channels: FinishRevalidationWorkerChannels<ChainApi>,
	) {
		let FinishRevalidationWorkerChannels {
			mut finish_revalidation_request_rx,
			revalidation_result_tx,
		} = finish_revalidation_worker_channels;

		debug!(
			target: LOG_TARGET,
			at_hash = ?self.at.hash,
			"view::revalidate: at starting"
		);
		let start = Instant::now();
		let validated_pool = self.pool.validated_pool();
		let api = validated_pool.api();

		let batch: Vec<_> = validated_pool.ready().collect();
		let batch_len = batch.len();

		// todo: sort batch by revalidation timestamp | maybe not needed at all? xts will be getting
		// out of the view...
		// todo: revalidate future, remove if invalid [#5496]

		let mut invalid_hashes = Vec::new();
		let mut revalidated = IndexMap::new();

		let mut validation_results = vec![];
		let mut batch_iter = batch.into_iter();
		loop {
			let mut should_break = false;
			tokio::select! {
				_ = finish_revalidation_request_rx.recv() => {
					debug!(
						target: LOG_TARGET,
						at_hash = ?self.at.hash,
						"view::revalidate: finish revalidation request received"
					);
					break
				}
				_ = async {
					if let Some(tx) = batch_iter.next() {
						let validation_result = (
							api.validate_transaction(
								self.at.hash,
								tx.source.clone().into(),
								tx.data.clone(),
								ValidateTransactionPriority::Maintained).await,
							tx.hash,
							tx
						);
						validation_results.push(validation_result);
					} else {
						self.revalidation_worker_channels.lock().as_mut().map(|ch| ch.remove_sender());
						should_break = true;
					}
				} => {}
			}

			if should_break {
				break;
			}
		}

		let revalidation_duration = start.elapsed();
		self.metrics.report(|metrics| {
			metrics.view_revalidation_duration.observe(revalidation_duration.as_secs_f64());
		});
		debug!(
			target: LOG_TARGET,
			at_hash = ?self.at.hash,
			count = validation_results.len(),
			batch_len,
			duration = ?revalidation_duration,
			"view::revalidate"
		);
		log_xt_trace!(
			data:tuple,
			target:LOG_TARGET,
			validation_results.iter().map(|x| (x.1, &x.0)),
			"view::revalidate result: {:?}"
		);
		for (validation_result, tx_hash, tx) in validation_results {
			match validation_result {
				Ok(Err(TransactionValidityError::Invalid(_))) => {
					trace!(
						target: LOG_TARGET,
						?tx_hash,
						?validation_result,
						"Removing. Transaction is invalid"
					);
					invalid_hashes.push(tx_hash);
				},
				Ok(Ok(validity)) => {
					revalidated.insert(
						tx_hash,
						ValidatedTransaction::valid_at(
							self.at.number.saturated_into::<u64>(),
							tx_hash,
							tx.source.clone(),
							tx.data.clone(),
							api.hash_and_length(&tx.data).1,
							validity,
						),
					);
				},
				Ok(Err(TransactionValidityError::Unknown(_))) => {
					trace!(
						target: LOG_TARGET,
						?tx_hash,
						?validation_result,
						"Removing. Cannot determine transaction validity"
					);
					invalid_hashes.push(tx_hash);
				},
				Err(_) => {
					trace!(
						target: LOG_TARGET,
						?tx_hash,
						?validation_result,
						"Removing due to error during revalidation"
					);
					invalid_hashes.push(tx_hash);
				},
			}
		}

		debug!(
			target: LOG_TARGET,
			at_hash = ?self.at.hash,
			"view::revalidate: sending revalidation result"
		);
		if let Err(error) = revalidation_result_tx
			.send(RevalidationResult { invalid_hashes, revalidated })
			.await
		{
			trace!(
				target: LOG_TARGET,
				at_hash = ?self.at.hash,
				?error,
				"view::revalidate: sending revalidation_result failed"
			);
		}
	}

	/// Sends revalidation request to the background worker.
	///
	/// Creates communication channels required to stop revalidation request and receive the
	/// revalidation results and sends the revalidation request to the background worker.
	///
	/// Intended to be called from maintain thread, at the very end of the maintain process.
	///
	/// Refer to [*View revalidation*](../index.html#view-revalidation) for more details.
	pub(super) async fn start_background_revalidation(
		view: Arc<Self>,
		revalidation_queue: Arc<
			super::revalidation_worker::RevalidationQueue<ChainApi, ChainApi::Block>,
		>,
	) {
		debug!(
			target: LOG_TARGET,
			at_hash = ?view.at.hash,
			"view::start_background_revalidation"
		);
		let (finish_revalidation_request_tx, finish_revalidation_request_rx) =
			tokio::sync::mpsc::channel(1);
		let (revalidation_result_tx, revalidation_result_rx) = tokio::sync::mpsc::channel(1);

		let finish_revalidation_worker_channels = FinishRevalidationWorkerChannels::new(
			finish_revalidation_request_rx,
			revalidation_result_tx,
		);

		let finish_revalidation_local_channels = FinishRevalidationLocalChannels::new(
			finish_revalidation_request_tx,
			revalidation_result_rx,
		);

		*view.revalidation_worker_channels.lock() = Some(finish_revalidation_local_channels);
		revalidation_queue
			.revalidate_view(view.clone(), finish_revalidation_worker_channels)
			.await;
	}

	/// Terminates a background view revalidation.
	///
	/// Receives the results from the background worker and applies them to the internal pool.
	/// Intended to be called from the maintain thread, at the very beginning of the maintain
	/// process, before the new view is cloned and updated. Applying results before cloning ensures
	/// that view contains up-to-date set of revalidated transactions.
	///
	/// Refer to [*View revalidation*](../index.html#view-revalidation) for more details.
	pub(super) async fn finish_revalidation(&self) {
		trace!(
			target: LOG_TARGET,
			at_hash = ?self.at.hash,
			"view::finish_revalidation"
		);
		let Some(revalidation_worker_channels) = self.revalidation_worker_channels.lock().take()
		else {
			trace!(target:LOG_TARGET, "view::finish_revalidation: no finish_revalidation_request_tx");
			return;
		};

		let FinishRevalidationLocalChannels {
			finish_revalidation_request_tx,
			mut revalidation_result_rx,
		} = revalidation_worker_channels;

		if let Some(finish_revalidation_request_tx) = finish_revalidation_request_tx {
			if let Err(error) = finish_revalidation_request_tx.send(()).await {
				trace!(
					target: LOG_TARGET,
					at_hash = ?self.at.hash,
					%error,
					"view::finish_revalidation: sending cancellation request failed"
				);
			}
		}

		if let Some(revalidation_result) = revalidation_result_rx.recv().await {
			let start = Instant::now();
			let revalidated_len = revalidation_result.revalidated.len();
			let validated_pool = self.pool.validated_pool();
			validated_pool.remove_invalid(&revalidation_result.invalid_hashes);
			if revalidated_len > 0 {
				self.pool.resubmit(revalidation_result.revalidated);
			}

			self.metrics.report(|metrics| {
				let _ = (
					revalidation_result
						.invalid_hashes
						.len()
						.try_into()
						.map(|v| metrics.view_revalidation_invalid_txs.inc_by(v)),
					revalidated_len
						.try_into()
						.map(|v| metrics.view_revalidation_resubmitted_txs.inc_by(v)),
				);
			});

			debug!(
				target: LOG_TARGET,
				invalid = revalidation_result.invalid_hashes.len(),
				revalidated = revalidated_len,
				at_hash = ?self.at.hash,
				duration = ?start.elapsed(),
				"view::finish_revalidation: applying revalidation result"
			);
		}
	}

	/// Checks if the transaction with given hash is already known to the view.
	pub(super) fn imported_status(&self, tx_hash: &ExtrinsicHash<ChainApi>) -> ImportedStatus {
		const IGNORE_BANNED: bool = false;
		match self.pool.validated_pool().check_is_known(tx_hash, IGNORE_BANNED) {
			Ok(()) => ImportedStatus::NotImported,
			Err(err) => {
				use sc_transaction_pool_api::error::IntoPoolError;
				match err.into_pool_error() {
					Ok(TxPoolError::TemporarilyBanned) => ImportedStatus::Banned,
					_ => ImportedStatus::Imported,
				}
			},
		}
	}

	/// Removes the whole transaction subtree from the inner pool.
	///
	/// Refer to [`crate::graph::ValidatedPool::remove_subtree`] for more details.
	pub fn remove_subtree<F>(
		&self,
		hashes: &[ExtrinsicHash<ChainApi>],
		ban_transactions: bool,
		listener_action: F,
	) -> Vec<TransactionFor<ChainApi>>
	where
		F: Fn(
			&mut crate::graph::EventDispatcher<ChainApi, ViewPoolObserver<ChainApi>>,
			ExtrinsicHash<ChainApi>,
		),
	{
		self.pool
			.validated_pool()
			.remove_subtree(hashes, ban_transactions, listener_action)
	}
}
