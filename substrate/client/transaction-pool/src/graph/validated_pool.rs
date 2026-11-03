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

use crate::{
	common::{
		sliding_stat::SyncDurationSlidingStats, tracing_log_xt::log_xt_trace, STAT_SLIDING_WINDOW,
	},
	insert_and_log_throttled_sync, LOG_TARGET,
};
use futures::channel::mpsc::{channel, Sender};
use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use sc_transaction_pool_api::{error, PoolStatus, ReadyTransactions, TransactionPriority};
use sp_blockchain::HashAndNumber;
use sp_runtime::{
	traits::SaturatedConversion,
	transaction_validity::{TransactionTag as Tag, ValidTransaction},
};
use std::{
	collections::{HashMap, HashSet},
	sync::Arc,
	time::{Duration, Instant},
};
use tracing::{debug, trace, warn, Level};

use super::{
	base_pool::{self as base, PruneStatus},
	listener::EventHandler,
	pool::{
		BlockHash, ChainApi, EventStream, ExtrinsicFor, ExtrinsicHash, Options, TransactionFor,
	},
	rotator::{BanReason, PoolRotator},
	watcher::Watcher,
};

/// Pre-validated transaction. Validated pool only accepts transactions wrapped in this enum.
#[derive(Debug)]
pub enum ValidatedTransaction<Hash, Ex, Error> {
	/// Transaction that has been validated successfully.
	Valid(base::Transaction<Hash, Ex>),
	/// Transaction that is invalid.
	Invalid(Hash, Error),
	/// Transaction which validity can't be determined.
	///
	/// We're notifying watchers about failure, if 'unknown' transaction is submitted.
	Unknown(Hash, Error),
}

impl<Hash, Ex, Error> ValidatedTransaction<Hash, Ex, Error> {
	/// Consume validity result, transaction data and produce ValidTransaction.
	pub fn valid_at(
		at: u64,
		hash: Hash,
		source: base::TimedTransactionSource,
		data: Ex,
		bytes: usize,
		validity: ValidTransaction,
	) -> Self {
		Self::Valid(base::Transaction {
			data,
			bytes,
			hash,
			source,
			priority: validity.priority,
			requires: validity.requires,
			provides: validity.provides,
			propagate: validity.propagate,
			valid_till: at.saturated_into::<u64>().saturating_add(validity.longevity),
		})
	}

	/// Returns priority for valid transaction, None if transaction is not valid.
	pub fn priority(&self) -> Option<TransactionPriority> {
		match self {
			ValidatedTransaction::Valid(base::Transaction { priority, .. }) => Some(*priority),
			_ => None,
		}
	}
}

/// A type of validated transaction stored in the validated pool.
pub type ValidatedTransactionFor<B> =
	ValidatedTransaction<ExtrinsicHash<B>, ExtrinsicFor<B>, <B as ChainApi>::Error>;

/// A type alias representing ValidatedPool event dispatcher for given ChainApi type.
pub type EventDispatcher<B, L> = super::listener::EventDispatcher<ExtrinsicHash<B>, B, L>;

/// A closure that returns true if the local node is a validator that can author blocks.
#[derive(Clone)]
pub struct IsValidator(Arc<Box<dyn Fn() -> bool + Send + Sync>>);

impl From<bool> for IsValidator {
	fn from(is_validator: bool) -> Self {
		Self(Arc::new(Box::new(move || is_validator)))
	}
}

impl From<Box<dyn Fn() -> bool + Send + Sync>> for IsValidator {
	fn from(is_validator: Box<dyn Fn() -> bool + Send + Sync>) -> Self {
		Self(Arc::new(is_validator))
	}
}

/// Represents the result of `submit` or `submit_and_watch` operations.
pub struct BaseSubmitOutcome<B: ChainApi, W> {
	/// The hash of the submitted transaction.
	hash: ExtrinsicHash<B>,
	/// A transaction watcher. This is `Some` for `submit_and_watch` and `None` for `submit`.
	watcher: Option<W>,

	/// The priority of the transaction. Defaults to None if unknown.
	priority: Option<TransactionPriority>,
}

/// Type alias to outcome of submission to `ValidatedPool`.
pub type ValidatedPoolSubmitOutcome<B> =
	BaseSubmitOutcome<B, Watcher<ExtrinsicHash<B>, ExtrinsicHash<B>>>;

impl<B: ChainApi, W> BaseSubmitOutcome<B, W> {
	/// Creates a new instance with given hash and priority.
	pub fn new(hash: ExtrinsicHash<B>, priority: Option<TransactionPriority>) -> Self {
		Self { hash, priority, watcher: None }
	}

	/// Sets the transaction watcher.
	pub fn with_watcher(mut self, watcher: W) -> Self {
		self.watcher = Some(watcher);
		self
	}

	/// Provides priority of submitted transaction.
	pub fn priority(&self) -> Option<TransactionPriority> {
		self.priority
	}

	/// Provides hash of submitted transaction.
	pub fn hash(&self) -> ExtrinsicHash<B> {
		self.hash
	}

	/// Provides a watcher. Should only be called on outcomes of `submit_and_watch`. Otherwise will
	/// panic (that would mean logical error in program).
	pub fn expect_watcher(&mut self) -> W {
		self.watcher.take().expect("watcher was set in submit_and_watch. qed")
	}
}

/// Pool that deals with validated transactions.
pub struct ValidatedPool<B: ChainApi, L: EventHandler<B>> {
	api: Arc<B>,
	is_validator: IsValidator,
	options: Options,
	event_dispatcher: RwLock<EventDispatcher<B, L>>,
	pub(crate) pool: RwLock<base::BasePool<ExtrinsicHash<B>, ExtrinsicFor<B>>>,
	import_notification_sinks: Mutex<Vec<Sender<ExtrinsicHash<B>>>>,
	rotator: PoolRotator<ExtrinsicHash<B>>,
	enforce_limits_stats: SyncDurationSlidingStats,
}

impl<B: ChainApi, L: EventHandler<B>> Clone for ValidatedPool<B, L> {
	fn clone(&self) -> Self {
		Self {
			api: self.api.clone(),
			is_validator: self.is_validator.clone(),
			options: self.options.clone(),
			event_dispatcher: Default::default(),
			pool: RwLock::from(self.pool.read().clone()),
			import_notification_sinks: Default::default(),
			rotator: self.rotator.clone(),
			enforce_limits_stats: self.enforce_limits_stats.clone(),
		}
	}
}

impl<B: ChainApi, L: EventHandler<B>> ValidatedPool<B, L> {
	pub fn deep_clone_with_event_handler(&self, event_handler: L) -> Self {
		Self {
			event_dispatcher: RwLock::new(EventDispatcher::new_with_event_handler(Some(
				event_handler,
			))),
			..self.clone()
		}
	}

	/// Create a new transaction pool with statically sized rotator.
	pub fn new_with_staticly_sized_rotator(
		options: Options,
		is_validator: IsValidator,
		api: Arc<B>,
	) -> Self {
		let ban_time = options.ban_time;
		Self::new_with_rotator(options, is_validator, api, PoolRotator::new(ban_time), None)
	}

	/// Create a new transaction pool.
	pub fn new(options: Options, is_validator: IsValidator, api: Arc<B>) -> Self {
		let ban_time = options.ban_time;
		let total_count = options.total_count();
		Self::new_with_rotator(
			options,
			is_validator,
			api,
			PoolRotator::new_with_expected_size(ban_time, total_count),
			None,
		)
	}

	/// Create a new transaction pool with given event handler.
	pub fn new_with_event_handler(
		options: Options,
		is_validator: IsValidator,
		api: Arc<B>,
		event_handler: L,
	) -> Self {
		let ban_time = options.ban_time;
		let total_count = options.total_count();
		Self::new_with_rotator(
			options,
			is_validator,
			api,
			PoolRotator::new_with_expected_size(ban_time, total_count),
			Some(event_handler),
		)
	}

	fn new_with_rotator(
		options: Options,
		is_validator: IsValidator,
		api: Arc<B>,
		rotator: PoolRotator<ExtrinsicHash<B>>,
		event_handler: Option<L>,
	) -> Self {
		let base_pool = base::BasePool::new(options.reject_future_transactions);
		Self {
			is_validator,
			options,
			event_dispatcher: RwLock::new(EventDispatcher::new_with_event_handler(event_handler)),
			api,
			pool: RwLock::new(base_pool),
			import_notification_sinks: Default::default(),
			rotator,
			enforce_limits_stats: SyncDurationSlidingStats::new(Duration::from_secs(
				STAT_SLIDING_WINDOW,
			)),
		}
	}

	/// Bans given set of hashes with the specified reason.
	pub fn ban(
		&self,
		now: &Instant,
		hashes: impl IntoIterator<Item = ExtrinsicHash<B>>,
		reason: BanReason,
	) {
		self.rotator.ban(now, hashes, reason)
	}

	/// Returns true if transaction with given hash is currently banned from the pool.
	pub fn is_banned(&self, hash: &ExtrinsicHash<B>) -> bool {
		self.rotator.is_banned(hash)
	}

	/// Removes the ban for a transaction, but only if it was banned for
	/// [`BanReason::Validation`].
	///
	/// Returns `true` if the ban was removed.
	pub fn unban_if_validation(&self, hash: &ExtrinsicHash<B>) -> bool {
		self.rotator.unban_if_validation(hash)
	}

	/// A fast check before doing any further processing of a transaction, like validation.
	///
	/// If `ignore_banned` is `true`, it will not check if the transaction is banned.
	///
	/// It checks if the transaction is already imported or banned. If so, it returns an error.
	pub fn check_is_known(
		&self,
		tx_hash: &ExtrinsicHash<B>,
		ignore_banned: bool,
	) -> Result<(), B::Error> {
		if !ignore_banned && self.is_banned(tx_hash) {
			Err(error::Error::TemporarilyBanned.into())
		} else if self.pool.read().is_imported(tx_hash) {
			Err(error::Error::AlreadyImported(Box::new(*tx_hash)).into())
		} else {
			Ok(())
		}
	}

	/// Imports a bunch of pre-validated transactions to the pool.
	pub fn submit(
		&self,
		txs: impl IntoIterator<Item = ValidatedTransactionFor<B>>,
	) -> Vec<Result<ValidatedPoolSubmitOutcome<B>, B::Error>> {
		let results = txs
			.into_iter()
			.map(|validated_tx| self.submit_one(validated_tx))
			.collect::<Vec<_>>();

		// only enforce limits if there is at least one imported transaction
		let removed = if results.iter().any(|res| res.is_ok()) {
			let start = Instant::now();
			let removed = self.enforce_limits();
			insert_and_log_throttled_sync!(
				Level::DEBUG,
				target:"txpool",
				prefix:"enforce_limits_stats",
				self.enforce_limits_stats,
				start.elapsed().into()
			);
			removed
		} else {
			Default::default()
		};

		results
			.into_iter()
			.map(|res| match res {
				Ok(outcome) if removed.contains(&outcome.hash) => {
					Err(error::Error::ImmediatelyDropped.into())
				},
				other => other,
			})
			.collect()
	}

	/// Submit single pre-validated transaction to the pool.
	fn submit_one(
		&self,
		tx: ValidatedTransactionFor<B>,
	) -> Result<ValidatedPoolSubmitOutcome<B>, B::Error> {
		match tx {
			ValidatedTransaction::Valid(tx) => {
				let priority = tx.priority;
				trace!(
					target: LOG_TARGET,
					tx_hash = ?tx.hash,
					"ValidatedPool::submit_one"
				);
				if !tx.propagate && !(self.is_validator.0)() {
					return Err(error::Error::Unactionable.into());
				}

				let imported = self.pool.write().import(tx)?;

				if let base::Imported::Ready { ref hash, .. } = imported {
					let sinks = &mut self.import_notification_sinks.lock();
					sinks.retain_mut(|sink| match sink.try_send(*hash) {
						Ok(()) => true,
						Err(e) => {
							if e.is_full() {
								warn!(
									target: LOG_TARGET,
									tx_hash = ?hash,
									"Trying to notify an import but the channel is full"
								);
								true
							} else {
								false
							}
						},
					});
				}

				let mut event_dispatcher = self.event_dispatcher.write();
				fire_events(&mut *event_dispatcher, &imported);
				Ok(ValidatedPoolSubmitOutcome::new(*imported.hash(), Some(priority)))
			},
			ValidatedTransaction::Invalid(tx_hash, error) => {
				trace!(
					target: LOG_TARGET,
					?tx_hash,
					?error,
					"ValidatedPool::submit_one invalid"
				);
				self.rotator
					.ban(&Instant::now(), std::iter::once(tx_hash), BanReason::Validation);
				Err(error)
			},
			ValidatedTransaction::Unknown(tx_hash, error) => {
				trace!(
					target: LOG_TARGET,
					?tx_hash,
					?error,
					"ValidatedPool::submit_one unknown"
				);
				self.event_dispatcher.write().invalid(&tx_hash);
				Err(error)
			},
		}
	}

	fn enforce_limits(&self) -> HashSet<ExtrinsicHash<B>> {
		let status = self.pool.read().status();
		let ready_limit = &self.options.ready;
		let future_limit = &self.options.future;

		if ready_limit.is_exceeded(status.ready, status.ready_bytes) ||
			future_limit.is_exceeded(status.future, status.future_bytes)
		{
			trace!(
				target: LOG_TARGET,
				ready_count = ready_limit.count,
				ready_kb = ready_limit.total_bytes / 1024,
				future_count = future_limit.count,
				future_kb = future_limit.total_bytes / 1024,
				"Enforcing limits"
			);

			// clean up the pool
			let removed = {
				let mut pool = self.pool.write();
				let removed = pool
					.enforce_limits(ready_limit, future_limit)
					.into_iter()
					.map(|x| x.hash)
					.collect::<HashSet<_>>();
				// ban all removed transactions
				self.rotator.ban(
					&Instant::now(),
					removed.iter().copied(),
					BanReason::LimitsEnforced,
				);
				removed
			};
			if !removed.is_empty() {
				trace!(
					target: LOG_TARGET,
					dropped_count = removed.len(),
					"Enforcing limits"
				);
			}

			// run notifications
			let mut event_dispatcher = self.event_dispatcher.write();
			for h in &removed {
				event_dispatcher.limits_enforced(h);
			}

			removed
		} else {
			Default::default()
		}
	}

	/// Import a single extrinsic and starts to watch their progress in the pool.
	pub fn submit_and_watch(
		&self,
		tx: ValidatedTransactionFor<B>,
	) -> Result<ValidatedPoolSubmitOutcome<B>, B::Error> {
		match tx {
			ValidatedTransaction::Valid(tx) => {
				let hash = self.api.hash_and_length(&tx.data).0;
				let watcher = self.create_watcher(hash);
				self.submit(std::iter::once(ValidatedTransaction::Valid(tx)))
					.pop()
					.expect("One extrinsic passed; one result returned; qed")
					.map(|outcome| outcome.with_watcher(watcher))
			},
			ValidatedTransaction::Invalid(hash, err) => {
				self.rotator.ban(&Instant::now(), std::iter::once(hash), BanReason::Validation);
				Err(err)
			},
			ValidatedTransaction::Unknown(_, err) => Err(err),
		}
	}

	/// Creates a new watcher for given extrinsic.
	pub fn create_watcher(
		&self,
		tx_hash: ExtrinsicHash<B>,
	) -> Watcher<ExtrinsicHash<B>, ExtrinsicHash<B>> {
		self.event_dispatcher.write().create_watcher(tx_hash)
	}

	/// Provides a list of hashes for all watched transactions in the pool.
	pub fn watched_transactions(&self) -> Vec<ExtrinsicHash<B>> {
		self.event_dispatcher.read().watched_transactions().map(Clone::clone).collect()
	}

	/// Resubmits revalidated transactions back to the pool.
	///
	/// Removes and then submits passed transactions and all dependent transactions.
	/// Transactions that are missing from the pool are not submitted.
	pub fn resubmit(
		&self,
		mut updated_transactions: IndexMap<ExtrinsicHash<B>, ValidatedTransactionFor<B>>,
	) {
		#[derive(Debug, Clone, Copy, PartialEq)]
		enum Status {
			Future,
			Ready,
			Failed,
			Dropped,
		}

		let (mut initial_statuses, final_statuses) = {
			let mut pool = self.pool.write();

			// remove all passed transactions from the ready/future queues
			// (this may remove additional transactions as well)
			//
			// for every transaction that has an entry in the `updated_transactions`,
			// we store updated validation result in txs_to_resubmit
			// for every transaction that has no entry in the `updated_transactions`,
			// we store last validation result (i.e. the pool entry) in txs_to_resubmit
			let mut initial_statuses = HashMap::new();
			let mut txs_to_resubmit = Vec::with_capacity(updated_transactions.len());
			while !updated_transactions.is_empty() {
				let hash = updated_transactions
					.keys()
					.next()
					.cloned()
					.expect("transactions is not empty; qed");

				// note we are not considering tx with hash invalid here - we just want
				// to remove it along with dependent transactions and `remove_subtree()`
				// does exactly what we need
				let removed = pool.remove_subtree(&[hash]);
				for removed_tx in removed {
					let removed_hash = removed_tx.hash;
					let updated_transaction = updated_transactions.shift_remove(&removed_hash);
					let tx_to_resubmit = if let Some(updated_tx) = updated_transaction {
						updated_tx
					} else {
						// in most cases we'll end up in successful `try_unwrap`, but if not
						// we still need to reinsert transaction back to the pool => duplicate call
						let transaction = match Arc::try_unwrap(removed_tx) {
							Ok(transaction) => transaction,
							Err(transaction) => transaction.duplicate(),
						};
						ValidatedTransaction::Valid(transaction)
					};

					initial_statuses.insert(removed_hash, Status::Ready);
					txs_to_resubmit.push((removed_hash, tx_to_resubmit));
				}
				// make sure to remove the hash even if it's not present in the pool anymore.
				updated_transactions.shift_remove(&hash);
			}

			// if we're rejecting future transactions, then insertion order matters here:
			// if tx1 depends on tx2, then if tx1 is inserted before tx2, then it goes
			// to the future queue and gets rejected immediately
			// => let's temporary stop rejection and clear future queue before return
			pool.with_futures_enabled(|pool, reject_future_transactions| {
				// now resubmit all removed transactions back to the pool
				let mut final_statuses = HashMap::new();
				for (tx_hash, tx_to_resubmit) in txs_to_resubmit {
					match tx_to_resubmit {
						ValidatedTransaction::Valid(tx) => match pool.import(tx) {
							Ok(imported) => match imported {
								base::Imported::Ready { promoted, failed, removed, .. } => {
									final_statuses.insert(tx_hash, Status::Ready);
									for hash in promoted {
										final_statuses.insert(hash, Status::Ready);
									}
									for hash in failed {
										final_statuses.insert(hash, Status::Failed);
									}
									for tx in removed {
										final_statuses.insert(tx.hash, Status::Dropped);
									}
								},
								base::Imported::Future { .. } => {
									final_statuses.insert(tx_hash, Status::Future);
								},
							},
							Err(error) => {
								// we do not want to fail if single transaction import has failed
								// nor we do want to propagate this error, because it could tx
								// unknown to caller => let's just notify listeners (and issue debug
								// message)
								warn!(
									target: LOG_TARGET,
									?tx_hash,
									%error,
									"Removing invalid transaction from update"
								);
								final_statuses.insert(tx_hash, Status::Failed);
							},
						},
						ValidatedTransaction::Invalid(_, _) |
						ValidatedTransaction::Unknown(_, _) => {
							final_statuses.insert(tx_hash, Status::Failed);
						},
					}
				}

				// if the pool is configured to reject future transactions, let's clear the future
				// queue, updating final statuses as required
				if reject_future_transactions {
					for future_tx in pool.clear_future() {
						final_statuses.insert(future_tx.hash, Status::Dropped);
					}
				}

				(initial_statuses, final_statuses)
			})
		};

		// and now let's notify listeners about status changes
		let mut event_dispatcher = self.event_dispatcher.write();
		for (hash, final_status) in final_statuses {
			let initial_status = initial_statuses.remove(&hash);
			if initial_status.is_none() || Some(final_status) != initial_status {
				match final_status {
					Status::Future => event_dispatcher.future(&hash),
					Status::Ready => event_dispatcher.ready(&hash, None),
					Status::Dropped => event_dispatcher.dropped(&hash),
					Status::Failed => event_dispatcher.invalid(&hash),
				}
			}
		}
	}

	/// For each extrinsic, returns tags that it provides (if known), or None (if it is unknown).
	pub fn extrinsics_tags(&self, hashes: &[ExtrinsicHash<B>]) -> Vec<Option<Vec<Tag>>> {
		self.pool
			.read()
			.by_hashes(hashes)
			.into_iter()
			.map(|existing_in_pool| {
				existing_in_pool.map(|transaction| transaction.provides.to_vec())
			})
			.collect()
	}

	/// Get ready transaction by hash
	pub fn ready_by_hash(&self, hash: &ExtrinsicHash<B>) -> Option<TransactionFor<B>> {
		self.pool.read().ready_by_hash(hash)
	}

	/// Prunes ready transactions that provide given list of tags.
	pub fn prune_tags(
		&self,
		tags: impl IntoIterator<Item = Tag>,
	) -> PruneStatus<ExtrinsicHash<B>, ExtrinsicFor<B>> {
		// Perform tag-based pruning in the base pool
		let status = self.pool.write().prune_tags(tags);
		// Notify event listeners of all transactions
		// that were promoted to `Ready` or were dropped.
		{
			let mut event_dispatcher = self.event_dispatcher.write();
			for promoted in &status.promoted {
				fire_events(&mut *event_dispatcher, promoted);
			}
			for f in &status.failed {
				event_dispatcher.dropped(f);
			}
		}

		status
	}

	/// Resubmit transactions that have been revalidated after prune_tags call.
	pub fn resubmit_pruned(
		&self,
		at: &HashAndNumber<B::Block>,
		known_imported_hashes: impl IntoIterator<Item = ExtrinsicHash<B>> + Clone,
		pruned_hashes: Vec<ExtrinsicHash<B>>,
		pruned_xts: Vec<ValidatedTransactionFor<B>>,
	) {
		debug_assert_eq!(pruned_hashes.len(), pruned_xts.len());

		// Resubmit pruned transactions back to the pool. Tag-based pruning may over-prune
		// (removing dependents in the subtree), so still-valid collateral txs need to be
		// re-added. In the fork-aware pool this is likely redundant — `update_view_with_mempool`
		// resubmits everything from the mempool right after pruning — and could be removed in
		// the future.
		self.submit(pruned_xts);

		// Fire `pruned` (InBlock) notifications only for `known_imported_hashes` — the
		// hashes of extrinsics actually present in the imported block body.
		self.fire_pruned(at, known_imported_hashes.into_iter());

		// perform regular cleanup of old transactions in the pool
		// and update temporary bans.
		self.clear_stale(at);
	}

	/// Fire notifications for pruned transactions.
	pub fn fire_pruned(
		&self,
		at: &HashAndNumber<B::Block>,
		hashes: impl Iterator<Item = ExtrinsicHash<B>>,
	) {
		let mut event_dispatcher = self.event_dispatcher.write();
		let mut set = HashSet::with_capacity(hashes.size_hint().0);
		for h in hashes {
			// `hashes` has possibly duplicate hashes.
			// we'd like to send out the `InBlock` notification only once.
			if !set.contains(&h) {
				event_dispatcher.pruned(at.hash, &h);
				set.insert(h);
			}
		}
	}

	/// Removes stale transactions from the pool.
	///
	/// Stale transactions are transaction beyond their longevity period.
	/// Note this function does not remove transactions that are already included in the chain.
	/// See `prune_tags` if you want this.
	pub fn clear_stale(&self, at: &HashAndNumber<B::Block>) {
		let HashAndNumber { number, .. } = *at;
		let number = number.saturated_into::<u64>();
		let now = Instant::now();
		let to_remove = {
			self.ready()
				.filter(|tx| self.rotator.ban_if_stale(&now, number, tx))
				.map(|tx| tx.hash)
				.collect::<Vec<_>>()
		};
		let futures_to_remove: Vec<ExtrinsicHash<B>> = {
			let p = self.pool.read();
			let mut hashes = Vec::new();
			for tx in p.futures() {
				if self.rotator.ban_if_stale(&now, number, tx) {
					hashes.push(tx.hash);
				}
			}
			hashes
		};
		debug!(
			target:LOG_TARGET,
			to_remove_len=to_remove.len(),
			futures_to_remove_len=futures_to_remove.len(),
			"clear_stale"
		);
		// removing old transactions
		self.remove_invalid(&to_remove);
		self.remove_invalid(&futures_to_remove);
		// clear banned transactions timeouts
		self.rotator.clear_timeouts(&now);
	}

	/// Get api reference.
	pub fn api(&self) -> &B {
		&self.api
	}

	/// Return an event stream of notifications for when transactions are imported to the pool.
	///
	/// Consumers of this stream should use the `ready` method to actually get the
	/// pending transactions in the right order.
	pub fn import_notification_stream(&self) -> EventStream<ExtrinsicHash<B>> {
		const CHANNEL_BUFFER_SIZE: usize = 1024;

		let (sink, stream) = channel(CHANNEL_BUFFER_SIZE);
		self.import_notification_sinks.lock().push(sink);
		stream
	}

	/// Invoked when extrinsics are broadcasted.
	pub fn on_broadcasted(&self, propagated: HashMap<ExtrinsicHash<B>, Vec<String>>) {
		let mut event_dispatcher = self.event_dispatcher.write();
		for (hash, peers) in propagated.into_iter() {
			event_dispatcher.broadcasted(&hash, peers);
		}
	}

	/// Remove a subtree of transactions from the pool and mark them invalid.
	///
	/// The transactions passed as an argument will be additionally banned
	/// to prevent them from entering the pool right away.
	/// Note this is not the case for the dependent transactions - those may
	/// still be valid so we want to be able to re-import them.
	///
	/// For every removed transaction an Invalid event is triggered.
	///
	/// Returns the list of actually removed transactions, which may include transactions dependent
	/// on provided set.
	pub fn remove_invalid(&self, hashes: &[ExtrinsicHash<B>]) -> Vec<TransactionFor<B>> {
		// early exit in case there is no invalid transactions.
		if hashes.is_empty() {
			return vec![];
		}

		let invalid = self.remove_subtree(hashes, true, |listener, removed_tx_hash| {
			listener.invalid(&removed_tx_hash);
		});

		trace!(
			target: LOG_TARGET,
			removed_count = hashes.len(),
			invalid_count = invalid.len(),
			"Removed invalid transactions"
		);
		log_xt_trace!(target: LOG_TARGET, invalid.iter().map(|t| t.hash), "Removed invalid transaction");

		invalid
	}

	/// Get an iterator for ready transactions ordered by priority
	pub fn ready(&self) -> impl ReadyTransactions<Item = TransactionFor<B>> + Send {
		self.pool.read().ready()
	}

	/// Returns a Vec of hashes and extrinsics in the future pool.
	pub fn futures(&self) -> Vec<(ExtrinsicHash<B>, ExtrinsicFor<B>)> {
		self.pool.read().futures().map(|tx| (tx.hash, tx.data.clone())).collect()
	}

	/// Returns pool status.
	pub fn status(&self) -> PoolStatus {
		self.pool.read().status()
	}

	/// Notify all watchers that transactions in the block with hash have been finalized
	pub async fn on_block_finalized(&self, block_hash: BlockHash<B>) -> Result<(), B::Error> {
		trace!(
			target: LOG_TARGET,
			?block_hash,
			"Attempting to notify watchers of finalization"
		);
		self.event_dispatcher.write().finalized(block_hash);
		Ok(())
	}

	/// Notify the event_dispatcher of retracted blocks
	pub fn on_block_retracted(&self, block_hash: BlockHash<B>) {
		self.event_dispatcher.write().retracted(block_hash)
	}

	/// Resends ready and future events for all the ready and future transactions that are already
	/// in the pool.
	///
	/// Intended to be called after cloning the instance of `ValidatedPool`.
	pub fn retrigger_notifications(&self) {
		let pool = self.pool.read();
		let mut event_dispatcher = self.event_dispatcher.write();
		pool.ready().for_each(|r| {
			event_dispatcher.ready(&r.hash, None);
		});
		pool.futures().for_each(|f| {
			event_dispatcher.future(&f.hash);
		});
	}

	/// Removes a transaction subtree from the pool, starting from the given transaction hash.
	///
	/// This function traverses the dependency graph of transactions and removes the specified
	/// transaction along with all its descendant transactions from the pool.
	///
	/// The root transactions will be banned from re-entrering the pool if `ban_transactions` is
	/// true. Descendant transactions may be re-submitted to the pool if required.
	///
	/// A `event_disaptcher_action` callback function is invoked for every transaction that is
	/// removed, providing a reference to the pool's event dispatcher and the hash of the removed
	/// transaction. This allows to trigger the required events.
	///
	/// Returns a vector containing the hashes of all removed transactions, including the root
	/// transaction specified by `tx_hash`.
	pub fn remove_subtree<F>(
		&self,
		hashes: &[ExtrinsicHash<B>],
		ban_transactions: bool,
		event_dispatcher_action: F,
	) -> Vec<TransactionFor<B>>
	where
		F: Fn(&mut EventDispatcher<B, L>, ExtrinsicHash<B>),
	{
		// temporarily ban removed transactions if requested
		if ban_transactions {
			self.rotator.ban(&Instant::now(), hashes.iter().cloned(), BanReason::Validation);
		};
		let removed = self.pool.write().remove_subtree(hashes);

		removed
			.into_iter()
			.map(|tx| {
				let removed_tx_hash = tx.hash;
				let mut event_dispatcher = self.event_dispatcher.write();
				event_dispatcher_action(&mut *event_dispatcher, removed_tx_hash);
				tx.clone()
			})
			.collect::<Vec<_>>()
	}
}

fn fire_events<B, L, Ex>(
	event_dispatcher: &mut EventDispatcher<B, L>,
	imported: &base::Imported<ExtrinsicHash<B>, Ex>,
) where
	B: ChainApi,
	L: EventHandler<B>,
{
	match *imported {
		base::Imported::Ready { ref promoted, ref failed, ref removed, ref hash } => {
			event_dispatcher.ready(hash, None);
			failed.iter().for_each(|f| event_dispatcher.invalid(f));
			removed.iter().for_each(|r| event_dispatcher.usurped(&r.hash, hash));
			promoted.iter().for_each(|p| event_dispatcher.ready(p, None));
		},
		base::Imported::Future { ref hash } => event_dispatcher.future(hash),
	}
}
