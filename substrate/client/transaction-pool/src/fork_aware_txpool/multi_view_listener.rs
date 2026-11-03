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

//! `MultiViewListener` and `ExternalWatcherContext` manage view streams and status updates for
//! transactions, providing control commands to manage transaction states, and create external
//! aggregated streams of transaction events.

use crate::{
	common::tracing_log_xt::log_xt_trace,
	fork_aware_txpool::{stream_map_util::next_event, view::TransactionStatusEvent},
	graph::{self, BlockHash, ExtrinsicHash},
	LOG_TARGET,
};
use futures::{Future, FutureExt, Stream, StreamExt};
use parking_lot::RwLock;
use sc_transaction_pool_api::{TransactionStatus, TransactionStatusStream, TxIndex};
use sc_utils::mpsc;
use sp_runtime::traits::Block as BlockT;
use std::{
	collections::{hash_map::Entry, HashMap, HashSet},
	pin::Pin,
	sync::Arc,
};
use tokio_stream::StreamMap;
use tracing::trace;

use super::{
	dropped_watcher::{DroppedReason, DroppedTransaction},
	metrics::EventsMetricsCollector,
};

/// A side channel allowing to control the external stream instance (one per transaction) with
/// [`ControllerCommand`].
///
/// Set of instances of [`Controller`] lives within the [`MultiViewListener`].
type Controller<T> = mpsc::TracingUnboundedSender<T>;

/// A receiver of [`ControllerCommand`] instances allowing to control the external stream.
///
/// Lives within the [`ExternalWatcherContext`] instance.
type CommandReceiver<T> = mpsc::TracingUnboundedReceiver<T>;

/// The stream of the transaction events.
///
/// It can represent both a single view's stream and an external watcher stream.
pub type TxStatusStream<T> = Pin<Box<TransactionStatusStream<ExtrinsicHash<T>, BlockHash<T>>>>;

/// An aggregated stream providing events for all transactions from the view.
///
/// This stream delivers updates for all transactions in the view, rather than for individual
/// transactions.
pub type ViewStatusStream<T> =
	Pin<Box<dyn Stream<Item = TransactionStatusEvent<ExtrinsicHash<T>, BlockHash<T>>> + Send>>;

/// Commands to control / drive the task of the multi view listener.
enum ControllerCommand<ChainApi: graph::ChainApi> {
	/// Requests transaction status updated. Sent by transaction pool implementation.
	TransactionStatusRequest(TransactionStatusUpdate<ChainApi>),
	/// Adds a new (aggregated) stream of transactions statuses originating in the view associated
	/// with a specific block hash.
	AddViewStream(BlockHash<ChainApi>, ViewStatusStream<ChainApi>),
	/// Removes an existing view's stream associated with a specific block hash.
	RemoveViewStream(BlockHash<ChainApi>),
}

/// Represents the transaction status update performed by transaction pool state machine. The
/// corresponding statuses coming from the view would typically be ignored in the external watcher.
enum TransactionStatusUpdate<ChainApi: graph::ChainApi> {
	/// Marks a transaction as invalidated.
	///
	/// If all pre-conditions are met, an external invalid event will be sent out.
	Invalidated(ExtrinsicHash<ChainApi>),

	/// Notifies that a transaction was finalized in a specific block hash and transaction index.
	///
	/// Send out an external finalized event.
	Finalized(ExtrinsicHash<ChainApi>, BlockHash<ChainApi>, TxIndex),

	/// Notifies that a transaction was broadcasted with a list of peer addresses.
	///
	/// Sends out an external broadcasted event.
	Broadcasted(ExtrinsicHash<ChainApi>, Vec<String>),

	/// Notifies that a transaction was dropped from the pool.
	///
	/// If all preconditions are met, an external dropped event will be sent out.
	Dropped(ExtrinsicHash<ChainApi>, DroppedReason<ExtrinsicHash<ChainApi>>),

	/// Notifies that a finality watcher timed out.
	///
	/// An external finality timed out event will be sent out.
	FinalityTimeout(ExtrinsicHash<ChainApi>, BlockHash<ChainApi>),
}

impl<ChainApi> TransactionStatusUpdate<ChainApi>
where
	ChainApi: graph::ChainApi,
{
	fn hash(&self) -> ExtrinsicHash<ChainApi> {
		match self {
			Self::Invalidated(hash) |
			Self::Finalized(hash, _, _) |
			Self::Broadcasted(hash, _) |
			Self::Dropped(hash, _) => *hash,
			Self::FinalityTimeout(hash, _) => *hash,
		}
	}
}

impl<ChainApi> Into<TransactionStatus<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>>
	for &TransactionStatusUpdate<ChainApi>
where
	ChainApi: graph::ChainApi,
{
	fn into(self) -> TransactionStatus<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>> {
		match self {
			TransactionStatusUpdate::Invalidated(_) => TransactionStatus::Invalid,
			TransactionStatusUpdate::Finalized(_, hash, index) => {
				TransactionStatus::Finalized((*hash, *index))
			},
			TransactionStatusUpdate::Broadcasted(_, peers) => {
				TransactionStatus::Broadcast(peers.clone())
			},
			TransactionStatusUpdate::Dropped(_, DroppedReason::Usurped(by)) => {
				TransactionStatus::Usurped(*by)
			},
			TransactionStatusUpdate::Dropped(_, DroppedReason::LimitsEnforced) => {
				TransactionStatus::Dropped
			},
			TransactionStatusUpdate::Dropped(_, DroppedReason::Invalid) => {
				TransactionStatus::Invalid
			},
			TransactionStatusUpdate::Dropped(_, DroppedReason::Viewless) => {
				// Viewless events are handled by dropped_monitor_task and should never
				// reach the listener. If they do, treat as dropped.
				tracing::warn!(
					target: crate::LOG_TARGET,
					"Unexpected Viewless event reached the transaction status listener"
				);
				TransactionStatus::Dropped
			},
			TransactionStatusUpdate::FinalityTimeout(_, block_hash) => {
				TransactionStatus::FinalityTimeout(*block_hash)
			},
		}
	}
}

impl<ChainApi> std::fmt::Debug for TransactionStatusUpdate<ChainApi>
where
	ChainApi: graph::ChainApi,
{
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Invalidated(h) => {
				write!(f, "Invalidated({h})")
			},
			Self::Finalized(h, b, i) => {
				write!(f, "Finalized({h},{b},{i})")
			},
			Self::Broadcasted(h, _) => {
				write!(f, "Broadcasted({h})")
			},
			Self::Dropped(h, r) => {
				write!(f, "Dropped({h},{r:?})")
			},
			Self::FinalityTimeout(h, b) => {
				write!(f, "FinalityTimeout({h},{b:?})")
			},
		}
	}
}

impl<ChainApi> std::fmt::Debug for ControllerCommand<ChainApi>
where
	ChainApi: graph::ChainApi,
{
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			ControllerCommand::AddViewStream(h, _) => write!(f, "AddView({h})"),
			ControllerCommand::RemoveViewStream(h) => write!(f, "RemoveView({h})"),
			ControllerCommand::TransactionStatusRequest(c) => {
				write!(f, "TransactionStatusRequest({c:?})")
			},
		}
	}
}

impl<ChainApi> ControllerCommand<ChainApi>
where
	ChainApi: graph::ChainApi,
{
	/// Creates new instance of a command requesting [`TransactionStatus::Invalid`] transaction
	/// status.
	fn new_invalidated(tx_hash: ExtrinsicHash<ChainApi>) -> Self {
		ControllerCommand::TransactionStatusRequest(TransactionStatusUpdate::Invalidated(tx_hash))
	}
	/// Creates new instance of a command requesting [`TransactionStatus::Broadcast`] transaction
	/// status.
	fn new_broadcasted(tx_hash: ExtrinsicHash<ChainApi>, peers: Vec<String>) -> Self {
		ControllerCommand::TransactionStatusRequest(TransactionStatusUpdate::Broadcasted(
			tx_hash, peers,
		))
	}
	/// Creates new instance of a command requesting [`TransactionStatus::Finalized`] transaction
	/// status.
	fn new_finalized(
		tx_hash: ExtrinsicHash<ChainApi>,
		block_hash: BlockHash<ChainApi>,
		index: TxIndex,
	) -> Self {
		ControllerCommand::TransactionStatusRequest(TransactionStatusUpdate::Finalized(
			tx_hash, block_hash, index,
		))
	}
	/// Creates new instance of a command requesting [`TransactionStatus::Dropped`] transaction
	/// status.
	fn new_dropped(
		tx_hash: ExtrinsicHash<ChainApi>,
		reason: DroppedReason<ExtrinsicHash<ChainApi>>,
	) -> Self {
		ControllerCommand::TransactionStatusRequest(TransactionStatusUpdate::Dropped(
			tx_hash, reason,
		))
	}
	/// Creates new instance of a command requesting [`TransactionStatus::FinalityTimeout`]
	/// transaction status.
	fn new_finality_timeout(
		tx_hash: ExtrinsicHash<ChainApi>,
		block_hash: BlockHash<ChainApi>,
	) -> Self {
		ControllerCommand::TransactionStatusRequest(TransactionStatusUpdate::FinalityTimeout(
			tx_hash, block_hash,
		))
	}
}

/// This struct allows to create and control listener for multiple transactions.
///
/// For every view, an aggregated stream of transactions events can be added. The events are
/// flattened and sent out to the external listener for individual transactions. (The *external*
/// term here means that it can be exposed to [`sc_transaction_pool_api::TransactionPool`] API
/// client e.g. over RPC.)
///
/// The listener allows to add and remove view's stream.
///
/// The listener provides a side channel that allows triggering specific events (finalized, dropped,
/// invalid, broadcast) independently of the view's stream.
pub struct MultiViewListener<ChainApi: graph::ChainApi> {
	/// Provides the controller for sending control commands to the listener's task.
	controller: Controller<ControllerCommand<ChainApi>>,

	/// The map containing the sinks of the streams representing the external listeners of
	/// the individual transactions. Hash of the transaction is used as a map's key. A map is
	/// shared with listener's task.
	external_controllers:
		Arc<RwLock<HashMap<ExtrinsicHash<ChainApi>, Controller<ExternalWatcherCommand<ChainApi>>>>>,
}

/// A type representing a `MultiViewListener` task. For more details refer to
/// [`MultiViewListener::task`].
pub type MultiViewListenerTask = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The external stream unfolding context.
///
/// This context is used to unfold the external events stream for a individual transaction, it
/// facilitates the logic of converting events incoming from numerous views into the external events
/// stream.
struct ExternalWatcherContext<ChainApi: graph::ChainApi> {
	/// The hash of the transaction being monitored within this context.
	tx_hash: ExtrinsicHash<ChainApi>,
	/// A receiver for controller commands sent by [`MultiViewListener`]'s task.
	command_receiver: CommandReceiver<ExternalWatcherCommand<ChainApi>>,
	/// A flag indicating whether the context should terminate.
	terminate: bool,
	/// A flag indicating if a `Future` status has been encountered.
	future_seen: bool,
	/// A flag indicating if a `Ready` status has been encountered.
	ready_seen: bool,
	/// A hash set of block hashes from views that consider the transaction valid.
	views_keeping_tx_valid: HashSet<BlockHash<ChainApi>>,
	/// The set of views (represented by block hashes) currently maintained by the transaction
	/// pool.
	known_views: HashSet<BlockHash<ChainApi>>,
}

/// Commands to control the single external stream living within the multi view listener. These
/// commands are sent from listener's task to [`ExternalWatcherContext`].
enum ExternalWatcherCommand<ChainApi: graph::ChainApi> {
	/// Command for triggering some of the transaction states, that are decided by the pool logic.
	PoolTransactionStatus(TransactionStatusUpdate<ChainApi>),
	/// Transaction status updates coming from the individual views.
	ViewTransactionStatus(
		BlockHash<ChainApi>,
		TransactionStatus<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>,
	),
	/// Notification about new view being added.
	AddView(BlockHash<ChainApi>),
	/// Notification about view being removed.
	RemoveView(BlockHash<ChainApi>),
}

impl<ChainApi: graph::ChainApi> ExternalWatcherContext<ChainApi>
where
	<<ChainApi as graph::ChainApi>::Block as BlockT>::Hash: Unpin,
{
	/// Creates new `ExternalWatcherContext` for particular transaction identified by `tx_hash`
	///
	/// The `command_receiver` is a side channel for receiving controller's
	/// [commands][`ExternalWatcherCommand`].
	fn new(
		tx_hash: ExtrinsicHash<ChainApi>,
		command_receiver: CommandReceiver<ExternalWatcherCommand<ChainApi>>,
	) -> Self {
		Self {
			tx_hash,
			command_receiver,
			terminate: false,
			future_seen: false,
			ready_seen: false,
			views_keeping_tx_valid: Default::default(),
			known_views: Default::default(),
		}
	}

	/// Handles transaction status updates from the pool and manages internal states based on the
	/// input value.
	///
	/// Function may set the context termination flag, which will close the stream.
	///
	/// Returns `Some` with the `event` to be sent out or `None`.
	fn handle_pool_transaction_status(
		&mut self,
		request: TransactionStatusUpdate<ChainApi>,
	) -> Option<TransactionStatus<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>> {
		let status = Into::<TransactionStatus<_, _>>::into(&request);
		status.is_final().then(|| self.terminate = true);
		return Some(status);
	}

	/// Handles various transaction status updates from individual views and manages internal states
	/// based on the input value.
	///
	/// Function may set the context termination flag, which will close the stream.
	///
	/// Returns `Some` with the `event` to be sent out or `None`.
	fn handle_view_transaction_status(
		&mut self,
		block_hash: BlockHash<ChainApi>,
		status: TransactionStatus<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>,
	) -> Option<TransactionStatus<ExtrinsicHash<ChainApi>, BlockHash<ChainApi>>> {
		trace!(
			target: LOG_TARGET,
			tx_hash = ?self.tx_hash,
			?block_hash,
			?status,
			views = ?self.known_views.iter().collect::<Vec<_>>(),
			"mvl handle event"
		);

		match status {
			TransactionStatus::Future => {
				self.views_keeping_tx_valid.insert(block_hash);
				if self.ready_seen || self.future_seen {
					None
				} else {
					self.future_seen = true;
					Some(status)
				}
			},
			TransactionStatus::Ready => {
				self.views_keeping_tx_valid.insert(block_hash);
				if self.ready_seen {
					None
				} else {
					self.ready_seen = true;
					Some(status)
				}
			},
			TransactionStatus::InBlock((..)) => {
				self.views_keeping_tx_valid.insert(block_hash);
				if !(self.ready_seen || self.future_seen) {
					self.ready_seen = true;
					Some(status)
				} else {
					Some(status)
				}
			},
			TransactionStatus::Finalized(_) => {
				self.terminate = true;
				Some(status)
			},
			TransactionStatus::FinalityTimeout(_) |
			TransactionStatus::Retracted(_) |
			TransactionStatus::Broadcast(_) |
			TransactionStatus::Usurped(_) |
			TransactionStatus::Dropped |
			TransactionStatus::Invalid => None,
		}
	}

	/// Adds a new aggragted transaction status stream.
	///
	/// Inserts a new view's transaction status stream into the stream map. The view is represented
	/// by `block_hash`.
	fn add_view(&mut self, block_hash: BlockHash<ChainApi>) {
		trace!(
			target: LOG_TARGET,
			tx_hash = ?self.tx_hash,
			?block_hash,
			views = ?self.known_views.iter().collect::<Vec<_>>(),
			"AddView view"
		);
		self.known_views.insert(block_hash);
	}

	/// Removes an existing aggreagated transaction status stream.
	///
	/// Removes an aggregated transaction status stream associated with a specific block hash from
	/// the stream map.
	fn remove_view(&mut self, block_hash: BlockHash<ChainApi>) {
		self.known_views.remove(&block_hash);
		self.views_keeping_tx_valid.remove(&block_hash);
		trace!(
			target: LOG_TARGET,
			tx_hash = ?self.tx_hash,
			?block_hash,
			views = ?self.known_views.iter().collect::<Vec<_>>(),
			"RemoveView view"
		);
	}
}

impl<ChainApi> MultiViewListener<ChainApi>
where
	ChainApi: graph::ChainApi + 'static,
	<<ChainApi as graph::ChainApi>::Block as BlockT>::Hash: Unpin,
{
	/// A worker task associated with `MultiViewListener` instance.
	///
	/// An asynchronous listener's task responsible for dispatching:
	/// - stream_map containing aggregated transaction status streams from multiple views,
	/// - view add/remove requests,
	/// - transaction commands,
	/// to multiple individual per-transaction external watcher contexts.
	///
	/// It also reports transactions statuses updates to the provided `events_metrics_collector`.
	///
	/// The returned future shall be polled by instantiator of `MultiViewListener`.
	async fn task(
		external_watchers_tx_hash_map: Arc<
			RwLock<HashMap<ExtrinsicHash<ChainApi>, Controller<ExternalWatcherCommand<ChainApi>>>>,
		>,
		mut command_receiver: CommandReceiver<ControllerCommand<ChainApi>>,
		events_metrics_collector: EventsMetricsCollector<ChainApi>,
	) {
		let mut aggregated_streams_map: StreamMap<BlockHash<ChainApi>, ViewStatusStream<ChainApi>> =
			Default::default();

		loop {
			tokio::select! {
				biased;
				Some((view_hash, (tx_hash, status))) =  next_event(&mut aggregated_streams_map) => {
					events_metrics_collector.report_status(tx_hash, status.clone());
					if let Entry::Occupied(mut ctrl) = external_watchers_tx_hash_map.write().entry(tx_hash) {
						trace!(
							target: LOG_TARGET,
							?tx_hash,
							?view_hash,
							?status,
							"aggregated_stream_map event",
						);
						if let Err(error) = ctrl
							.get_mut()
							.unbounded_send(ExternalWatcherCommand::ViewTransactionStatus(view_hash, status))
						{
							trace!(target: LOG_TARGET, ?tx_hash, ?error, "send status failed");
							ctrl.remove();
						}
					}
				},
				cmd = command_receiver.next() => {
					match cmd {
						Some(ControllerCommand::AddViewStream(h,stream)) => {
							aggregated_streams_map.insert(h,stream);
							// //todo: aysnc and join all?
							external_watchers_tx_hash_map.write().retain(|tx_hash, ctrl| {
								ctrl.unbounded_send(ExternalWatcherCommand::AddView(h))
									.inspect_err(|error| {
										trace!(target: LOG_TARGET, ?tx_hash, ?error, "add_view: send message failed");
									})
									.is_ok()
							})
						},
						Some(ControllerCommand::RemoveViewStream(h)) => {
							aggregated_streams_map.remove(&h);
							//todo: aysnc and join all?
							external_watchers_tx_hash_map.write().retain(|tx_hash, ctrl| {
								ctrl.unbounded_send(ExternalWatcherCommand::RemoveView(h))
									.inspect_err(|error| {
										trace!(target: LOG_TARGET, ?tx_hash, ?error, "remove_view: send message failed");
									})
									.is_ok()
							})
						},

						Some(ControllerCommand::TransactionStatusRequest(request)) => {
							let tx_hash = request.hash();
							events_metrics_collector.report_status(tx_hash, (&request).into());
							if let Entry::Occupied(mut ctrl) = external_watchers_tx_hash_map.write().entry(tx_hash) {
								if let Err(error) = ctrl
									.get_mut()
									.unbounded_send(ExternalWatcherCommand::PoolTransactionStatus(request))
								{
									trace!(target: LOG_TARGET, ?tx_hash, ?error, "send message failed");
									ctrl.remove();
								}
							}
						},
						None =>  {}
					}
				},
			};
		}
	}

	/// Creates a new [`MultiViewListener`] instance along with its associated worker task.
	///
	/// This function instantiates the new `MultiViewListener` and provides the worker task that
	/// relays messages to the external transactions listeners. The task shall be polled by caller.
	///
	/// The `events_metrics_collector` is an instance of `EventsMetricsCollector` that is
	/// responsible for collecting and managing metrics related to transaction events. Newly
	/// created instance of `MultiViewListener` will report transaction status updates and its
	/// timestamps to the given metrics collector.
	///
	/// Returns a tuple containing the [`MultiViewListener`] and the
	/// [`MultiViewListenerTask`].
	pub fn new_with_worker(
		events_metrics_collector: EventsMetricsCollector<ChainApi>,
	) -> (Self, MultiViewListenerTask) {
		let external_controllers = Arc::from(RwLock::from(HashMap::<
			ExtrinsicHash<ChainApi>,
			Controller<ExternalWatcherCommand<ChainApi>>,
		>::default()));

		const CONTROLLER_QUEUE_WARN_SIZE: usize = 100_000;
		let (tx, rx) = mpsc::tracing_unbounded(
			"txpool-multi-view-listener-task-controller",
			CONTROLLER_QUEUE_WARN_SIZE,
		);
		let task = Self::task(external_controllers.clone(), rx, events_metrics_collector);

		(Self { external_controllers, controller: tx }, task.boxed())
	}

	/// Creates an external tstream of events for given transaction.
	///
	/// This method initializes an `ExternalWatcherContext` for the provided transaction hash, sets
	/// up the necessary communication channel with listener's task, and unfolds an external
	/// (meaning that it can be exposed to [`sc_transaction_pool_api::TransactionPool`] API client
	/// e.g. rpc) stream of transaction status events. If an external watcher is already present for
	/// the given transaction, it returns `None`.
	///
	/// The `submit_timestamp` indicates the time at which a transaction is submitted.
	/// It is primarily used to calculate event timings for metric collection.
	pub(crate) fn create_external_watcher_for_tx(
		&self,
		tx_hash: ExtrinsicHash<ChainApi>,
	) -> Option<TxStatusStream<ChainApi>> {
		let external_ctx = match self.external_controllers.write().entry(tx_hash) {
			Entry::Occupied(_) => return None,
			Entry::Vacant(entry) => {
				const EXT_CONTROLLER_QUEUE_WARN_THRESHOLD: usize = 128;
				let (tx, rx) = mpsc::tracing_unbounded(
					"txpool-multi-view-listener",
					EXT_CONTROLLER_QUEUE_WARN_THRESHOLD,
				);
				entry.insert(tx);
				ExternalWatcherContext::new(tx_hash, rx)
			},
		};

		trace!(
			target: LOG_TARGET,
			?tx_hash,
			"create_external_watcher_for_tx"
		);

		Some(
			futures::stream::unfold(external_ctx, |mut ctx| async move {
				if ctx.terminate {
					trace!(target: LOG_TARGET, tx_hash = ?ctx.tx_hash, "terminate");
					return None;
				}
				loop {
					tokio::select! {
						cmd = ctx.command_receiver.next() => {
							match cmd? {
								ExternalWatcherCommand::ViewTransactionStatus(view_hash, status) => {
									if let Some(new_status) = ctx.handle_view_transaction_status(view_hash, status) {
										trace!(
											target: LOG_TARGET,
											tx_hash = ?ctx.tx_hash,
											?new_status,
											"mvl sending out"
										);
										return Some((new_status, ctx))
									}
								},
								ExternalWatcherCommand::PoolTransactionStatus(request) => {
									if let Some(new_status) = ctx.handle_pool_transaction_status(request) {
										trace!(
											target: LOG_TARGET,
											tx_hash = ?ctx.tx_hash,
											?new_status,
											"mvl sending out"
										);
										return Some((new_status, ctx))
									}
								}
								ExternalWatcherCommand::AddView(h) => {
									ctx.add_view(h);
								},
								ExternalWatcherCommand::RemoveView(h) => {
									ctx.remove_view(h);
								},
							}
						},
					};
				}
			})
			.boxed(),
		)
	}

	/// Adds an aggregated view's transaction status stream.
	///
	/// This method sends a `AddViewStream` command to the task, from where it is further dispatched
	/// to the external watcher context for every watched transaction.
	///
	/// The stream is associated with a view represented by `block_hash`.
	pub(crate) fn add_view_aggregated_stream(
		&self,
		block_hash: BlockHash<ChainApi>,
		stream: ViewStatusStream<ChainApi>,
	) {
		trace!(target: LOG_TARGET, ?block_hash, "mvl::add_view_aggregated_stream");
		if let Err(error) = self
			.controller
			.unbounded_send(ControllerCommand::AddViewStream(block_hash, stream))
		{
			trace!(
				target: LOG_TARGET,
				?block_hash,
				%error,
				"add_view_aggregated_stream: send message failed"
			);
		}
	}

	/// Removes a view's stream associated with a specific view hash.
	///
	/// This method sends a `RemoveViewStream` command to the listener's task, from where is further
	/// dispatched to the external watcher context for every watched transaction.
	pub(crate) fn remove_view(&self, block_hash: BlockHash<ChainApi>) {
		trace!(target: LOG_TARGET, ?block_hash, "mvl::remove_view");
		if let Err(error) =
			self.controller.unbounded_send(ControllerCommand::RemoveViewStream(block_hash))
		{
			trace!(
				target: LOG_TARGET,
				?block_hash,
				%error,
				"remove_view: send message failed"
			);
		}
	}

	/// Invalidate given transaction.
	///
	/// This method sends a `TransactionInvalidated` command to the task's controller of each
	/// transaction provided to process the invalidation request.
	///
	/// The external event will be sent if no view is referencing the transaction as `Ready` or
	/// `Future`.
	pub(crate) fn transactions_invalidated(&self, invalid_hashes: &[ExtrinsicHash<ChainApi>]) {
		log_xt_trace!(target: LOG_TARGET, invalid_hashes, "transactions_invalidated");
		for tx_hash in invalid_hashes {
			if let Err(error) =
				self.controller.unbounded_send(ControllerCommand::new_invalidated(*tx_hash))
			{
				trace!(
					target: LOG_TARGET,
					?tx_hash,
					%error,
					"transactions_invalidated: send message failed"
				);
			}
		}
	}

	/// Send `Broadcasted` event to listeners of all transactions.
	///
	/// This method sends a `TransactionBroadcasted` command to the task's controller for each
	/// transaction provided. It will prompt the external `Broadcasted` event.
	pub(crate) fn transactions_broadcasted(
		&self,
		propagated: HashMap<ExtrinsicHash<ChainApi>, Vec<String>>,
	) {
		for (tx_hash, peers) in propagated {
			if let Err(error) = self
				.controller
				.unbounded_send(ControllerCommand::new_broadcasted(tx_hash, peers))
			{
				trace!(
					target: LOG_TARGET,
					?tx_hash,
					%error,
					"transactions_broadcasted: send message failed"
				);
			}
		}
	}

	/// Send `Dropped` event to listeners of transactions.
	///
	/// This method sends a `TransactionDropped` command to the task's controller. It will prompt
	/// the external `Broadcasted` event.
	pub(crate) fn transaction_dropped(&self, dropped: DroppedTransaction<ExtrinsicHash<ChainApi>>) {
		let DroppedTransaction { tx_hash, reason } = dropped;
		trace!(target: LOG_TARGET, ?tx_hash, ?reason, "transaction_dropped");
		if let Err(error) =
			self.controller.unbounded_send(ControllerCommand::new_dropped(tx_hash, reason))
		{
			trace!(
				target: LOG_TARGET,
				?tx_hash,
				%error,
				"transaction_dropped: send message failed"
			);
		}
	}

	/// Send `Finalized` event for given transaction at given block.
	///
	/// This will trigger `Finalized` event to the external watcher.
	pub(crate) fn transaction_finalized(
		&self,
		tx_hash: ExtrinsicHash<ChainApi>,
		block: BlockHash<ChainApi>,
		idx: TxIndex,
	) {
		trace!(target: LOG_TARGET, ?tx_hash, "transaction_finalized");
		if let Err(error) = self
			.controller
			.unbounded_send(ControllerCommand::new_finalized(tx_hash, block, idx))
		{
			trace!(
				target: LOG_TARGET,
				?tx_hash,
				%error,
				"transaction_finalized: send message failed"
			);
		};
	}

	/// Send `FinalityTimeout` event for given transactions at given block.
	///
	/// This will trigger `FinalityTimeout` event to the external watcher.
	pub(crate) fn transactions_finality_timeout(
		&self,
		tx_hashes: &[ExtrinsicHash<ChainApi>],
		block: BlockHash<ChainApi>,
	) {
		for tx_hash in tx_hashes {
			trace!(target: LOG_TARGET, ?tx_hash, "transaction_finality_timeout");
			if let Err(error) = self
				.controller
				.unbounded_send(ControllerCommand::new_finality_timeout(*tx_hash, block))
			{
				trace!(
					target: LOG_TARGET,
					?tx_hash,
					%error,
					"transaction_finality_timeout: send message failed"
				);
			};
		}
	}

	/// Removes stale controllers.
	pub(crate) fn remove_stale_controllers(&self) {
		self.external_controllers.write().retain(|_, c| !c.is_closed());
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::common::tests::TestApi;
	use futures::{stream, StreamExt};
	use sp_core::H256;
	use tokio::{select, task::JoinHandle};
	use tracing::debug;

	type MultiViewListener = super::MultiViewListener<TestApi>;

	fn create_multi_view_listener(
	) -> (MultiViewListener, tokio::sync::oneshot::Sender<()>, JoinHandle<()>) {
		let (listener, listener_task) = MultiViewListener::new_with_worker(Default::default());

		let (tx, rx) = tokio::sync::oneshot::channel();

		let listener_handle = tokio::spawn(async move {
			select! {
				_ = listener_task => {},
				_ = rx => { return; }
			}
		});

		(listener, tx, listener_handle)
	}

	#[tokio::test]
	async fn test01() {
		sp_tracing::try_init_simple();
		let (listener, terminate_listener, listener_task) = create_multi_view_listener();

		let block_hash = H256::repeat_byte(0x01);
		let tx_hash = H256::repeat_byte(0x0a);
		let events = vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((block_hash, 0)),
			TransactionStatus::Finalized((block_hash, 0)),
		];

		let external_watcher = listener.create_external_watcher_for_tx(tx_hash).unwrap();
		let handle = tokio::spawn(async move { external_watcher.collect::<Vec<_>>().await });

		let view_stream =
			futures::stream::iter(std::iter::repeat(tx_hash).zip(events.clone().into_iter()));

		listener.add_view_aggregated_stream(block_hash, view_stream.boxed());

		let out = handle.await.unwrap();
		assert_eq!(out, events);
		debug!("out: {:#?}", out);

		let _ = terminate_listener.send(());
		let _ = listener_task.await.unwrap();
	}

	#[tokio::test]
	async fn test02() {
		sp_tracing::try_init_simple();
		let (listener, terminate_listener, listener_task) = create_multi_view_listener();

		let block_hash0 = H256::repeat_byte(0x01);
		let events0 = vec![
			TransactionStatus::Future,
			TransactionStatus::Ready,
			TransactionStatus::InBlock((block_hash0, 0)),
		];

		let block_hash1 = H256::repeat_byte(0x02);
		let events1 = vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((block_hash1, 0)),
			TransactionStatus::Finalized((block_hash1, 0)),
		];

		let tx_hash = H256::repeat_byte(0x0a);
		let external_watcher = listener.create_external_watcher_for_tx(tx_hash).unwrap();

		let view_stream0 =
			futures::stream::iter(std::iter::repeat(tx_hash).zip(events0.clone().into_iter()));
		let view_stream1 =
			futures::stream::iter(std::iter::repeat(tx_hash).zip(events1.clone().into_iter()));

		let handle = tokio::spawn(async move { external_watcher.collect::<Vec<_>>().await });

		listener.add_view_aggregated_stream(block_hash0, view_stream0.boxed());
		listener.add_view_aggregated_stream(block_hash1, view_stream1.boxed());

		let out = handle.await.unwrap();

		debug!("out: {:#?}", out);
		assert!(out.iter().all(|v| vec![
			TransactionStatus::Future,
			TransactionStatus::Ready,
			TransactionStatus::InBlock((block_hash0, 0)),
			TransactionStatus::InBlock((block_hash1, 0)),
			TransactionStatus::Finalized((block_hash1, 0)),
		]
		.contains(v)));
		assert_eq!(out.len(), 5);

		let _ = terminate_listener.send(());
		let _ = listener_task.await.unwrap();
	}

	#[tokio::test]
	async fn test03() {
		sp_tracing::try_init_simple();
		let (listener, terminate_listener, listener_task) = create_multi_view_listener();

		let block_hash0 = H256::repeat_byte(0x01);
		let events0 = vec![
			TransactionStatus::Future,
			TransactionStatus::Ready,
			TransactionStatus::InBlock((block_hash0, 0)),
		];

		let block_hash1 = H256::repeat_byte(0x02);
		let events1 = vec![TransactionStatus::Future];

		let tx_hash = H256::repeat_byte(0x0a);
		let external_watcher = listener.create_external_watcher_for_tx(tx_hash).unwrap();
		let handle = tokio::spawn(async move { external_watcher.collect::<Vec<_>>().await });

		let view_stream0 =
			futures::stream::iter(std::iter::repeat(tx_hash).zip(events0.clone().into_iter()));
		let view_stream1 =
			futures::stream::iter(std::iter::repeat(tx_hash).zip(events1.clone().into_iter()));

		listener.add_view_aggregated_stream(block_hash0, view_stream0.boxed());
		listener.add_view_aggregated_stream(block_hash1, view_stream1.boxed());

		listener.remove_view(block_hash0);
		listener.remove_view(block_hash1);

		listener.transactions_invalidated(&[tx_hash]);

		let out = handle.await.unwrap();
		debug!("out: {:#?}", out);
		assert!(out.iter().all(|v| vec![
			TransactionStatus::Future,
			TransactionStatus::Ready,
			TransactionStatus::InBlock((block_hash0, 0)),
			TransactionStatus::Invalid
		]
		.contains(v)));
		assert_eq!(out.len(), 4);

		let _ = terminate_listener.send(());
		let _ = listener_task.await.unwrap();
	}
	//
	#[tokio::test]
	async fn test032() {
		sp_tracing::try_init_simple();
		let (listener, terminate_listener, listener_task) = create_multi_view_listener();

		let block_hash0 = H256::repeat_byte(0x01);
		let events0_tx0 = vec![TransactionStatus::Future];
		let events0_tx1 = vec![TransactionStatus::Ready];

		let block_hash1 = H256::repeat_byte(0x02);
		let events1_tx0 =
			vec![TransactionStatus::Ready, TransactionStatus::InBlock((block_hash1, 0))];
		let events1_tx1 = vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((block_hash1, 1)),
			TransactionStatus::Finalized((block_hash1, 1)),
		];

		let tx0_hash = H256::repeat_byte(0x0a);
		let tx1_hash = H256::repeat_byte(0x0b);
		let external_watcher_tx0 = listener.create_external_watcher_for_tx(tx0_hash).unwrap();
		let external_watcher_tx1 = listener.create_external_watcher_for_tx(tx1_hash).unwrap();

		let handle0 = tokio::spawn(async move { external_watcher_tx0.collect::<Vec<_>>().await });
		let handle1 = tokio::spawn(async move { external_watcher_tx1.collect::<Vec<_>>().await });

		let view0_tx0_stream =
			futures::stream::iter(std::iter::repeat(tx0_hash).zip(events0_tx0.clone()));
		let view0_tx1_stream =
			futures::stream::iter(std::iter::repeat(tx1_hash).zip(events0_tx1.clone()));

		let view1_tx0_stream =
			futures::stream::iter(std::iter::repeat(tx0_hash).zip(events1_tx0.clone()));
		let view1_tx1_stream =
			futures::stream::iter(std::iter::repeat(tx1_hash).zip(events1_tx1.clone()));

		listener.add_view_aggregated_stream(block_hash0, view0_tx0_stream.boxed());
		listener.add_view_aggregated_stream(block_hash1, view1_tx0_stream.boxed());
		listener.add_view_aggregated_stream(block_hash0, view0_tx1_stream.boxed());
		listener.add_view_aggregated_stream(block_hash1, view1_tx1_stream.boxed());

		listener.remove_view(block_hash0);
		listener.remove_view(block_hash1);

		listener.transactions_invalidated(&[tx0_hash]);
		listener.transactions_invalidated(&[tx1_hash]);

		let out_tx0 = handle0.await.unwrap();
		let out_tx1 = handle1.await.unwrap();

		debug!("out_tx0: {:#?}", out_tx0);
		debug!("out_tx1: {:#?}", out_tx1);
		assert!(out_tx0.iter().all(|v| vec![
			TransactionStatus::Future,
			TransactionStatus::Ready,
			TransactionStatus::InBlock((block_hash1, 0)),
			TransactionStatus::Invalid
		]
		.contains(v)));

		assert!(out_tx1.iter().all(|v| vec![
			TransactionStatus::Ready,
			TransactionStatus::InBlock((block_hash1, 1)),
			TransactionStatus::Finalized((block_hash1, 1))
		]
		.contains(v)));
		assert_eq!(out_tx0.len(), 4);
		assert_eq!(out_tx1.len(), 3);

		let _ = terminate_listener.send(());
		let _ = listener_task.await.unwrap();
	}

	#[tokio::test]
	async fn test04() {
		sp_tracing::try_init_simple();
		let (listener, terminate_listener, listener_task) = create_multi_view_listener();

		let block_hash0 = H256::repeat_byte(0x01);
		let events0 = vec![
			TransactionStatus::Future,
			TransactionStatus::Ready,
			TransactionStatus::InBlock((block_hash0, 0)),
		];

		let block_hash1 = H256::repeat_byte(0x02);
		let events1 = vec![TransactionStatus::Future];

		let tx_hash = H256::repeat_byte(0x0a);
		let external_watcher = listener.create_external_watcher_for_tx(tx_hash).unwrap();

		// views will keep transaction valid, invalidation shall not happen
		let view_stream0 = futures::stream::iter(std::iter::repeat(tx_hash).zip(events0.clone()))
			.chain(stream::pending().boxed());
		let view_stream1 = futures::stream::iter(std::iter::repeat(tx_hash).zip(events1.clone()))
			.chain(stream::pending().boxed());

		let handle = tokio::spawn(async move {
			// views are still there, we need to fetch 3 events
			external_watcher.take(3).collect::<Vec<_>>().await
		});

		listener.add_view_aggregated_stream(block_hash0, view_stream0.boxed());
		listener.add_view_aggregated_stream(block_hash1, view_stream1.boxed());

		listener.transactions_invalidated(&[tx_hash]);

		let out = handle.await.unwrap();
		debug!("out: {:#?}", out);

		// invalid shall not be sent
		assert!(out.iter().all(|v| vec![
			TransactionStatus::Future,
			TransactionStatus::Ready,
			TransactionStatus::InBlock((block_hash0, 0)),
		]
		.contains(v)));
		assert_eq!(out.len(), 3);
		let _ = terminate_listener.send(());
		let _ = listener_task.await.unwrap();
	}

	#[tokio::test]
	async fn test05() {
		sp_tracing::try_init_simple();
		let (listener, terminate_listener, listener_task) = create_multi_view_listener();

		let block_hash0 = H256::repeat_byte(0x01);
		let events0 = vec![TransactionStatus::Invalid];

		let tx_hash = H256::repeat_byte(0x0a);
		let external_watcher = listener.create_external_watcher_for_tx(tx_hash).unwrap();
		let handle = tokio::spawn(async move { external_watcher.collect::<Vec<_>>().await });

		let view_stream0 = futures::stream::iter(std::iter::repeat(tx_hash).zip(events0.clone()))
			.chain(stream::pending().boxed());

		// Note: this generates actual Invalid event.
		// Invalid event from View's stream is intentionally ignored .
		// we need to explicitely remove the view
		listener.remove_view(block_hash0);
		listener.transactions_invalidated(&[tx_hash]);

		listener.add_view_aggregated_stream(block_hash0, view_stream0.boxed());

		let out = handle.await.unwrap();
		debug!("out: {:#?}", out);

		assert!(out.iter().all(|v| vec![TransactionStatus::Invalid].contains(v)));
		assert_eq!(out.len(), 1);

		let _ = terminate_listener.send(());
		let _ = listener_task.await.unwrap();
	}
}
