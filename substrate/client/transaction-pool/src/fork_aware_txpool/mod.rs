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

//! Substrate fork aware transaction pool implementation.
//!
//! # Top level overview.
//! This documentation provides high level overview of the main structures and the main flows within
//! the fork-aware transaction pool.
//!
//! ## Structures.
//! ### View.
//! #### Purpose.
//! The main responsibility of the [`View`] is to provide the valid set of ready transactions at
//! the given block. [`ForkAwareTxPool`] keeps the number of recent views for all the blocks
//! notified since recently finalized block.
//!
//! The views associated with blocks at the tips of the forks are actively updated with all newly
//! incoming transactions, while intermediate views are not updated (they still provide transactions
//! ready to be included at that block) due to performance reasons, since every transaction
//! submitted to the view needs to be [validated][runtime_api::validate].
//! Building upon the older blocks happens relatively rare so this does not affect blocks filling.
//!
//! The view is wrapper around [`Pool`] and exposes its functionality, including the ability
//! of [tracking][`Watcher`] the progress of every transaction.
//!
//! #### Views: active, inactive.
//! All the views are stored in [`ViewStore`] structure. In this documentation the views at the tips
//! of the forks are referred as [`active_views`], while the intermediate views as
//! [`inactive_views`].
//!
//!
//! #### The life cycle of the [`View`].
//! Views are created when the new [`ChainEvent`] is notified to the pool. The view that is
//! [closest][find_best_view] to the newly notified block is chosen to clone from. Once built and
//! updated the newly created view is placed in [`active_views`]. Detailed description of view
//! creation is described in [the material to follow](#handling-the-new-best-block). When the view
//! is no longer at the tip of the forks, it is moved to the [`inactive_views`]. When the block
//! number of the view is lower then the finalized block, the view is permanently removed.
//!
//!
//! *Example*:
//! The following chain:
//! ```text
//!    C2 - C3 - C4
//!   /
//! B1
//!   \
//!    B2 - B3 - B4
//! ```
//! and the following set of events:
//! ```text
//! New best block: B1, C3, C4, B4
//! ```
//! will result in the following set of views within the [`ViewStore`]:
//! ```text
//!   active: C4, B4
//! inactive: B1, C3
//! ```
//! Please note that views are only created for the notified blocks.
//!
//! The pool can be notified either about every imported block (all forks), or only about the best
//! blocks. In the former case views are created for all forks, keeping the pool ready to author on
//! or switch to any fork without a cold-start re-validation; in the latter only the best chain is
//! tracked.
//!
//!
//! ### View store.
//! [`ViewStore`] is the helper structure that provides means to perform some actions like
//! [`submit`] or [`submit_and_watch`] on every view. It keeps track of both active and inactive
//! views.
//!
//! It also keeps tracks of the `most_recent_view` which is used to implement some methods of
//! [TransactionPool API], see [API considerations](#api-considerations) section.
//!
//! ### Multi-view listeners
//! There is a number of event streams that are provided by individual views:
//! - aggregated stream of [transactions statuses][`AggregatedStream`] for all the transactions
//!   within the view in the form of `(transaction-hash, status)` tuple,
//! - [ready notification][`vp::import_notification_stream`] (see [networking
//!   section](#networking)),
//! - [dropped notification][`create_dropped_by_limits_stream`].
//!
//! These streams need to be merged into a single stream exposed by transaction pool (or used
//! internally). Those aggregators are often referred as multi-view listeners and they implement
//! stream-specific or event-specific logic.
//!
//! The most important is [`MultiViewListener`] which is owned by view store. Some internal details
//! on events' flow is provided in [transaction status](#monitoring-the-status-of-a-transaction)
//! section.
//!
//! ### Intermediate transactions buffer: [`TxMemPool`]
//! The main purpose of an internal [`TxMemPool`] (referred to as *mempool*) is to prevent a
//! transaction from being lost, e.g. due to race condition when the new transaction submission
//! occurs just before the new view is created. This could also happen when a transaction is invalid
//! on one fork and could be valid on another which is not yet fully processed by the maintain
//! procedure. Additionally, it allows the pool to accept transactions when no blocks have been
//! reported yet.
//!
//! The *mempool* keeps a track on how the transaction was submitted - keeping number of watched and
//! non-watched transactions is useful for testing and metrics. The [transaction
//! source][`TransactionSource`] used to submit transactions also needs to be kept in the *mempool*.
//! The *mempool* transaction is a simple [wrapper][`TxInMemPool`] around the [`Arc`] reference to
//! the actual extrinsic body.
//!
//! Once the view is created, all transactions from *mempool* are submitted to and validated at this
//! view.
//!
//! The *mempool* removes its transactions when they get finalized. The transactions in *mempool*
//! are also periodically verified at every finalized block and removed from the *mempool* if no
//! longer valid. This is process is called [*mempool* revalidation](#mempool-pruningrevalidation).
//!
//! ## Flows
//!
//! The transaction pool internally is executing numerous tasks. This includes handling submitted
//! transactions and tracking their progress, listening to [`ChainEvent`]s and executing the
//! maintain process, which aims to provide the set of ready transactions. On the other side
//! transaction pool provides a [`ready_at`] future that resolves to the iterator of ready
//! transactions. On top of that pool performs background revalidation jobs.
//!
//! This section provides a top level overview of all flows within the fork aware transaction pool.
//!
//! ### Transaction route: [`submit`][`api_submit`]
//! This flow is simple. Transaction is added to the mempool and if it is not rejected by it (due to
//! size limits), it is also [submitted][`submit`] into every view in [`active_views`].
//!
//! When the newly created view does not contain this transaction yet, it is
//! [re-submitted][ForkAwareTxPool::update_view_with_mempool] from [`TxMemPool`] into this view.
//!
//! ### Transaction route: [`submit_and_watch`][`api_submit_and_watch`]
//!
//! The [`submit_and_watch`] function allows to submit the transaction and track its
//! [status][`TransactionStatus`] within the pool.
//!
//! When a watched transaction is submitted to the pool it is added to the *mempool* with the
//! watched flag. The external stream for the transaction is created in a [`MultiViewListener`].
//! Then a transaction is submitted to every active [`View`] (using
//! [`submit_many`][`View::submit_many`]). The view's [aggregated
//! stream][`create_aggregated_stream`] was already connected to the [`MultiViewListener`] when new
//! view was created, so no additional action is required upon the submission. The view will provide
//! the required updates for all the transactions over this single stream.
//!
//!
//! #### Monitoring the status of a transaction
//!
//! Transaction status monitoring and triggering events to [external
//! listener][`TransactionStatusStreamFor`] (e.g. to RPC client) is responsibility of the
//! [`MultiViewListener`].
//!
//! Every view is providing an independent aggreagated [stream][`create_aggregated_stream`] of
//! events for all transactions in this view, which needs to be merged into the single stream
//! exposed to the [external listener][`TransactionStatusStreamFor`] (e.g. to RPC client). For
//! majority of events simple forwarding would not work (e.g. we could get multiple [`Ready`]
//! events, or [`Ready`] / [`Future`] mix). Some additional stateful logic (implemented by
//! [`MultiViewListener`]) is required to filter and process the views' events.
//!
//! It is not possible to trigger some external events (e.g., [`Dropped`], [`Finalized`],
//! [`Invalid`], and [`Broadcast`]) using only the view-aggregated streams. These events require a
//! pool-wide understanding of the transaction state. For example, dropping a transaction from a
//! single view does not mean it was dropped from other views. Broadcast and finalized notifications
//! are sent to the transaction pool API, not at the view level. These events are simply ignored
//! when they originate in the view. The pool uses a dedicated side channel exposed by
//! [`MultiViewListener`] to trigger the beforementioned events.
//!
//! ### Maintain
//! The transaction pool exposes the [task][`notification_future`] that listens to the
//! finalized and block-import streams and executes the [`maintain`] procedure. The block-import
//! stream may carry either every imported block (all forks) or best blocks only, depending on the
//! pool configuration.
//!
//! The [`maintain`] is the main procedure of the transaction pool. It handles incoming
//! [`ChainEvent`]s, as described in the following two sub-sections.
//!
//! #### Handling the new block (best or non-best fork)
//! If the new block actually needs to be handled, the following steps are
//! executed:
//! - [find][find_best_view] the best view and clone it to [create a new
//! view][crate::ForkAwareTxPool::build_new_view],
//! - [update the view][ForkAwareTxPool::update_view_with_mempool] with the transactions from the
//!   *mempool*
//! 	- all transactions from the *mempool* (with some obvious filtering applied) are submitted to
//!    the view,
//! 	- the new [aggregated stream][`create_aggregated_stream`] of all transactions statuses  is
//! 	created for the new view and it is connected to the multi-view-listener,
//! - [update the view][ForkAwareTxPool::update_view_with_fork] with the transactions from the [tree
//!   route][`TreeRoute`] (which is computed from the recent best block to newly notified one by
//!   [enactment state][`EnactmentState`] helper):
//! 	- resubmit the transactions from the retracted blocks,
//! 	- prune extrinsic from the enacted blocks, and trigger [`InBlock`] events,
//! - insert the newly created and updated view into the view store.
//!
//!
//! #### Handling the finalized block
//! The following actions are taken on every finalized block:
//! - send [`Finalized`] events for every transactions on the finalized [tree route][`TreeRoute`],
//! - remove all the views (both active and inactive) that are lower then finalized block from the
//! view store,
//! - removal of finalized transaction from the *mempool*,
//! - trigger [*mempool* background revalidation](#mempool-pruningrevalidation).
//! - clean up of multi-view listeners which is required to avoid ever-growing structures,
//!
//! ### Light maintain
//! The [maintain](#maintain) procedure can sometimes be quite heavy, and it may not be accomplished
//! within the time window expected by the block builder. On top of that block builder may want to
//! build few blocks in the row, not giving the pool enough time to accomplish possible ongoing
//! maintain process.
//!
//! To address this, there is a [light version][`ready_at_light`] of the maintain procedure. It
//! [finds the first descendent view][`find_view_descendent_up_to_number`] up to the recent
//! finalized block, clones it and prunes all the transactions that were included in enacted part of
//! the traversed route, from the base view to the block at which a ready iterator was requested. No
//! new [transaction validations][runtime_api::validate] are required to accomplish it. If no view
//! is found, it will return the ready transactions of the most recent view processed by the
//! transaction pool.
//!
//! ### Providing ready transactions: `ready_at`
//! The asynchronous [`ready_at`] function resolves to the [ready transactions
//! iterator][`ReadyTransactions`]. The block builder shall wait either for the future to be
//! resolved or for timeout to be hit. To avoid building empty blocks in case of timeout, the
//! waiting for timeout functionality was moved into the transaction pool, and new API function was
//! added: [`ready_at_with_timeout`]. This function also provides a fall back ready iterator which
//! is result of [light maintain](#light-maintain).
//!
//! New function internally waits either for [maintain](#maintain) process triggered for requested
//! block to be accomplished or for the timeout. If timeout hits then the result of [light
//! maintain](#light-maintain) is returned. Light maintain is always executed at the beginning of
//! [`ready_at_with_timeout`] to make sure that it is available w/ o additional delay.
//!
//! If the maintain process for the requested block was accomplished before the `ready_at` functions
//! are called both of them immediately provide the ready transactions iterator (which is simply
//! requested on the appropriate instance of the [`View`]).
//!
//! The little [`ReadyPoll`] helper contained within [`ForkAwareTxPool`] as ([`ready_poll`])
//! implements the futures management.
//!
//! ### Background tasks
//! The [maintain](#maintain) procedure shall be as quick as possible, so heavy revalidation job is
//! delegated to the background worker. These includes view and *mempool* revalidation which are
//! both handled by the [`RevalidationQueue`] which simply sends revalidation requests to the
//! background thread.
//!
//! ####  View revalidation
//! View revalidation is performed in the background thread. Revalidation is executed for every
//! view. All the transaction from the view are [revalidated][`view::revalidate`].
//!
//! The fork-aware pool utilizes two threads to execute maintain and revalidation process
//! exclusively, ensuring maintain performance without overlapping with revalidation.
//!
//! The view revalidation process is [triggered][`start_background_revalidation`] at the very end of
//! the [maintain][`maintain`] process, and [stopped][`finish_background_revalidations`] at the
//! very beginning of the next maintenance execution (upon the next [`ChainEvent`] reception). The
//! results from the revalidation are immediately applied once the revalidation is
//! [terminated][crate::fork_aware_txpool::view::View::finish_revalidation].
//! ```text
//!                time: ---------------------->
//!  maintenance thread: M----M------M--M-M---
//! revalidation thread: -RRRR-RR-----RR-R-RRR
//! ```
//!
//! ####  Mempool pruning/revalidation
//! Transactions within *mempool* are constantly revalidated in the background. The
//! [revalidation][`mp::revalidate`] is performed in [batches][`batch_size`], and transactions that
//! were validated as latest, are revalidated first in the next iteration. The revalidation is
//! triggered on every finalized block. If a transaction is found to be invalid, the [`Invalid`]
//! event is sent and transaction is removed from the *mempool*.
//!
//! NOTE: There is one exception: if transaction is referenced by any view as ready, then it is
//! removed from the *mempool*, but not removed from the view. The [`Invalid`] event is not sent.
//! This case is not likely to happen, however it may need some extra attention.
//!
//! ### Networking
//! The pool is exposing [`ImportNotificationStream`][`import_notification_stream`], the dedicated
//! channel over which all ready transactions are notified. Internally this channel needs to merge
//! all ready events from every view. This functionality is implemented by
//! [`MultiViewImportNotificationSink`].
//!
//! The networking module is utilizing this channel to receive info about new ready transactions
//! which later will be propagated over the network. On the other side, when a transaction is
//! received networking submits transaction to the pool using [`submit`][`api_submit`].
//!
//! ### Handling invalid transactions
//! Refer to *mempool* revalidation [section](#mempool-pruningrevalidation).
//!
//! ## Pool limits
//! Every [`View`] has the [limits][`Options`] for the number or size of transactions it can hold.
//! Obviously the number of transactions in every view is not distributed equally, so some views
//! might be fully filled while others not.
//!
//! On the other hand the size of internal *mempool* shall also be capped, but transactions that are
//! still referenced by views should not be removed.
//!
//! When the [`View`] is at its limits, it can either reject the transaction during
//! submission process, or it can accept the transaction and drop different transaction which is
//! already in the pool during the [`enforce_limits`][`vp::enforce_limits`] process.
//!
//! The [`StreamOfDropped`] stream aggregating [per-view][`create_dropped_by_limits_stream`] streams
//! allows to monitor the transactions that were dropped by all the views (or dropped by some views
//! while not referenced by the others), what means that transaction can also be
//! [removed][`dropped_monitor_task`] from the *mempool*.
//!
//!
//! ## API Considerations
//! Refer to github issue: <https://github.com/paritytech/polkadot-sdk/issues/5491>
//!
//! [`View`]: crate::fork_aware_txpool::view::View
//! [`view::revalidate`]: crate::fork_aware_txpool::view::View::revalidate
//! [`start_background_revalidation`]: crate::fork_aware_txpool::view::View::start_background_revalidation
//! [`View::submit_many`]: crate::fork_aware_txpool::view::View::submit_many
//! [`ViewStore`]: crate::fork_aware_txpool::view_store::ViewStore
//! [`finish_background_revalidations`]: crate::fork_aware_txpool::view_store::ViewStore::finish_background_revalidations
//! [find_best_view]: crate::fork_aware_txpool::view_store::ViewStore::find_best_view
//! [`find_view_descendent_up_to_number`]: crate::fork_aware_txpool::view_store::ViewStore::find_view_descendent_up_to_number
//! [`active_views`]: crate::fork_aware_txpool::view_store::ViewStore::active_views
//! [`inactive_views`]: crate::fork_aware_txpool::view_store::ViewStore::inactive_views
//! [`TxMemPool`]: crate::fork_aware_txpool::tx_mem_pool::TxMemPool
//! [`mp::revalidate`]: crate::fork_aware_txpool::tx_mem_pool::TxMemPool::revalidate
//! [`batch_size`]: crate::fork_aware_txpool::tx_mem_pool::TXMEMPOOL_MAX_REVALIDATION_BATCH_SIZE
//! [`TxInMemPool`]: crate::fork_aware_txpool::tx_mem_pool::TxInMemPool
//! [`MultiViewListener`]: crate::fork_aware_txpool::multi_view_listener::MultiViewListener
//! [`Pool`]: crate::graph::Pool
//! [`Watcher`]: crate::graph::watcher::Watcher
//! [`AggregatedStream`]: crate::fork_aware_txpool::view::AggregatedStream
//! [`Options`]: crate::graph::Options
//! [`vp::import_notification_stream`]: ../graph/validated_pool/struct.ValidatedPool.html#method.import_notification_stream
//! [`vp::enforce_limits`]: ../graph/validated_pool/struct.ValidatedPool.html#method.enforce_limits
//! [`create_dropped_by_limits_stream`]: ../graph/validated_pool/struct.ValidatedPool.html#method.create_dropped_by_limits_stream
//! [`create_aggregated_stream`]: ../graph/validated_pool/struct.ValidatedPool.html#method.create_aggregated_stream
//! [`ChainEvent`]: sc_transaction_pool_api::ChainEvent
//! [`TransactionStatusStreamFor`]: sc_transaction_pool_api::TransactionStatusStreamFor
//! [`api_submit`]: sc_transaction_pool_api::TransactionPool::submit_at
//! [`api_submit_and_watch`]: sc_transaction_pool_api::TransactionPool::submit_and_watch
//! [`ready_at_with_timeout`]: sc_transaction_pool_api::TransactionPool::ready_at_with_timeout
//! [`TransactionSource`]: sc_transaction_pool_api::TransactionSource
//! [TransactionPool API]: sc_transaction_pool_api::TransactionPool
//! [`TransactionStatus`]:sc_transaction_pool_api::TransactionStatus
//! [`Ready`]:sc_transaction_pool_api::TransactionStatus::Ready
//! [`Future`]:sc_transaction_pool_api::TransactionStatus::Future
//! [`Broadcast`]:sc_transaction_pool_api::TransactionStatus::Broadcast
//! [`Invalid`]:sc_transaction_pool_api::TransactionStatus::Invalid
//! [`InBlock`]:sc_transaction_pool_api::TransactionStatus::InBlock
//! [`Finalized`]:sc_transaction_pool_api::TransactionStatus::Finalized
//! [`Dropped`]:sc_transaction_pool_api::TransactionStatus::Dropped
//! [`ReadyTransactions`]:sc_transaction_pool_api::ReadyTransactions
//! [`dropped_monitor_task`]: ForkAwareTxPool::dropped_monitor_task
//! [`ready_poll`]: ForkAwareTxPool::ready_poll
//! [`ready_at_light`]: ForkAwareTxPool::ready_at_light
//! [`ready_at`]: ../struct.ForkAwareTxPool.html#method.ready_at
//! [`import_notification_stream`]: ../struct.ForkAwareTxPool.html#method.import_notification_stream
//! [`maintain`]: ../struct.ForkAwareTxPool.html#method.maintain
//! [`submit`]: ../struct.ForkAwareTxPool.html#method.submit_at
//! [`submit_and_watch`]: ../struct.ForkAwareTxPool.html#method.submit_and_watch
//! [`ReadyPoll`]: ../fork_aware_txpool/fork_aware_txpool/struct.ReadyPoll.html
//! [`TreeRoute`]: sp_blockchain::TreeRoute
//! [runtime_api::validate]: sp_transaction_pool::runtime_api::TaggedTransactionQueue::validate_transaction
//! [`notification_future`]: crate::common::notification_future
//! [`EnactmentState`]: crate::common::enactment_state::EnactmentState
//! [`MultiViewImportNotificationSink`]: crate::fork_aware_txpool::import_notification_sink::MultiViewImportNotificationSink
//! [`RevalidationQueue`]: crate::fork_aware_txpool::revalidation_worker::RevalidationQueue
//! [`StreamOfDropped`]: crate::fork_aware_txpool::dropped_watcher::StreamOfDropped
//! [`Arc`]: std::sync::Arc

mod dropped_watcher;
pub(crate) mod fork_aware_txpool;
mod import_notification_sink;
mod metrics;
mod multi_view_listener;
mod revalidation_worker;
mod tx_mem_pool;
mod view;
mod view_store;

pub use fork_aware_txpool::{ForkAwareTxPool, ForkAwareTxPoolTask};

mod stream_map_util {
	use futures::Stream;
	use std::marker::Unpin;
	use tokio_stream::StreamMap;

	pub async fn next_event<K, V>(
		stream_map: &mut StreamMap<K, V>,
	) -> Option<(K, <V as Stream>::Item)>
	where
		K: Clone + Unpin,
		V: Stream + Unpin,
	{
		if stream_map.is_empty() {
			// yield pending to prevent busy-loop on an empty map
			futures::pending!()
		}

		futures::StreamExt::next(stream_map).await
	}
}
