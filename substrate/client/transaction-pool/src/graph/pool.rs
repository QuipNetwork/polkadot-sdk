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

use crate::{common::tracing_log_xt::log_xt_trace, LOG_TARGET};
use async_trait::async_trait;
use futures::channel::mpsc::Receiver;
use indexmap::IndexMap;
use sc_transaction_pool_api::error;
use sp_blockchain::{HashAndNumber, TreeRoute};
use sp_runtime::{
	generic::BlockId,
	traits::{self, Block as BlockT, SaturatedConversion},
	transaction_validity::{
		TransactionSource, TransactionTag as Tag, TransactionValidity, TransactionValidityError,
	},
};
use std::{
	collections::HashMap,
	sync::Arc,
	time::{Duration, Instant},
};
use tracing::{debug, instrument, trace, Level};

use super::{
	base_pool as base,
	validated_pool::{IsValidator, ValidatedPool, ValidatedTransaction},
	EventHandler, ValidatedPoolSubmitOutcome,
};

/// Modification notification event stream type;
pub type EventStream<H> = Receiver<H>;

/// Block hash type for a pool.
pub type BlockHash<A> = <<A as ChainApi>::Block as traits::Block>::Hash;
/// Extrinsic hash type for a pool.
pub type ExtrinsicHash<A> = <<A as ChainApi>::Block as traits::Block>::Hash;
/// Extrinsic type for a pool (reference counted).
pub type ExtrinsicFor<A> = Arc<<<A as ChainApi>::Block as traits::Block>::Extrinsic>;
/// Extrinsic type for a pool (raw data).
pub type RawExtrinsicFor<A> = <<A as ChainApi>::Block as traits::Block>::Extrinsic;
/// Block number type for the ChainApi
pub type NumberFor<A> = traits::NumberFor<<A as ChainApi>::Block>;
/// A type of transaction stored in the pool
pub type TransactionFor<A> = Arc<base::Transaction<ExtrinsicHash<A>, ExtrinsicFor<A>>>;
/// A type of validated transaction stored in the pool.
pub type ValidatedTransactionFor<A> =
	ValidatedTransaction<ExtrinsicHash<A>, ExtrinsicFor<A>, <A as ChainApi>::Error>;

/// The priority of request to validate the transaction.
#[derive(PartialEq, Copy, Clone)]
pub enum ValidateTransactionPriority {
	/// Validate the newly submitted transactions
	///
	/// Validation will be done with lower priority.
	Submitted,
	/// Validate the transaction during maintainance process,
	///
	/// Validation will be performed with higher priority.
	Maintained,
}

/// Concrete extrinsic validation and query logic.
#[async_trait]
pub trait ChainApi: Send + Sync {
	/// Block type.
	type Block: BlockT;
	/// Error type.
	type Error: From<error::Error> + error::IntoPoolError + error::IntoMetricsLabel;

	/// Asynchronously verify extrinsic at given block.
	async fn validate_transaction(
		&self,
		at: <Self::Block as BlockT>::Hash,
		source: TransactionSource,
		uxt: ExtrinsicFor<Self>,
		validation_priority: ValidateTransactionPriority,
	) -> Result<TransactionValidity, Self::Error>;

	/// Synchronously verify given extrinsic at given block.
	///
	/// Validates a transaction by calling into the runtime. Same as `validate_transaction` but
	/// blocks the current thread when performing validation.
	fn validate_transaction_blocking(
		&self,
		at: <Self::Block as BlockT>::Hash,
		source: TransactionSource,
		uxt: ExtrinsicFor<Self>,
	) -> Result<TransactionValidity, Self::Error>;

	/// Returns a block number given the block id.
	fn block_id_to_number(
		&self,
		at: &BlockId<Self::Block>,
	) -> Result<Option<NumberFor<Self>>, Self::Error>;

	/// Returns a block hash given the block id.
	fn block_id_to_hash(
		&self,
		at: &BlockId<Self::Block>,
	) -> Result<Option<<Self::Block as BlockT>::Hash>, Self::Error>;

	/// Returns hash and encoding length of the extrinsic.
	fn hash_and_length(&self, uxt: &RawExtrinsicFor<Self>) -> (ExtrinsicHash<Self>, usize);

	/// Returns a block body given the block.
	async fn block_body(
		&self,
		at: <Self::Block as BlockT>::Hash,
	) -> Result<Option<Vec<<Self::Block as traits::Block>::Extrinsic>>, Self::Error>;

	/// Returns a block header given the block id.
	fn block_header(
		&self,
		at: <Self::Block as BlockT>::Hash,
	) -> Result<Option<<Self::Block as BlockT>::Header>, Self::Error>;

	/// Compute a tree-route between two blocks. See [`TreeRoute`] for more details.
	fn tree_route(
		&self,
		from: <Self::Block as BlockT>::Hash,
		to: <Self::Block as BlockT>::Hash,
	) -> Result<TreeRoute<Self::Block>, Self::Error>;

	/// Resolves block number by id.
	fn resolve_block_number(
		&self,
		at: <Self::Block as BlockT>::Hash,
	) -> Result<NumberFor<Self>, Self::Error> {
		self.block_id_to_number(&BlockId::Hash(at)).and_then(|number| {
			number.ok_or_else(|| error::Error::InvalidBlockId(format!("{:?}", at)).into())
		})
	}
}

/// Pool configuration options.
#[derive(Debug, Clone)]
pub struct Options {
	/// Ready queue limits.
	pub ready: base::Limit,
	/// Future queue limits.
	pub future: base::Limit,
	/// Reject future transactions.
	pub reject_future_transactions: bool,
	/// How long the extrinsic is banned for.
	pub ban_time: Duration,
}

impl Default for Options {
	fn default() -> Self {
		Self {
			ready: base::Limit { count: 8192, total_bytes: 20 * 1024 * 1024 },
			future: base::Limit { count: 512, total_bytes: 1 * 1024 * 1024 },
			reject_future_transactions: false,
			ban_time: Duration::from_secs(60 * 30),
		}
	}
}

impl Options {
	/// Total (ready+future) maximal number of transactions in the pool.
	pub fn total_count(&self) -> usize {
		self.ready.count + self.future.count
	}
}

/// Should we check that the transaction is banned
/// in the pool, before we verify it?
#[derive(Copy, Clone)]
pub(crate) enum CheckBannedBeforeVerify {
	Yes,
	No,
}

/// Extrinsics pool that performs validation.
pub struct Pool<B: ChainApi, L: EventHandler<B>> {
	validated_pool: Arc<ValidatedPool<B, L>>,
}

impl<B: ChainApi, L: EventHandler<B>> Pool<B, L> {
	/// Create a new transaction pool with statically sized rotator.
	pub fn new_with_staticly_sized_rotator(
		options: Options,
		is_validator: IsValidator,
		api: Arc<B>,
	) -> Self {
		Self {
			validated_pool: Arc::new(ValidatedPool::new_with_staticly_sized_rotator(
				options,
				is_validator,
				api,
			)),
		}
	}

	/// Create a new transaction pool.
	pub fn new(options: Options, is_validator: IsValidator, api: Arc<B>) -> Self {
		Self { validated_pool: Arc::new(ValidatedPool::new(options, is_validator, api)) }
	}

	/// Create a new transaction pool.
	pub fn new_with_event_handler(
		options: Options,
		is_validator: IsValidator,
		api: Arc<B>,
		event_handler: L,
	) -> Self {
		Self {
			validated_pool: Arc::new(ValidatedPool::new_with_event_handler(
				options,
				is_validator,
				api,
				event_handler,
			)),
		}
	}

	/// Imports a bunch of unverified extrinsics to the pool
	#[instrument(level = Level::TRACE, skip_all, target="txpool", name = "pool::submit_at")]
	pub async fn submit_at(
		&self,
		at: &HashAndNumber<B::Block>,
		xts: impl IntoIterator<Item = (base::TimedTransactionSource, ExtrinsicFor<B>)>,
		validation_priority: ValidateTransactionPriority,
	) -> Vec<Result<ValidatedPoolSubmitOutcome<B>, B::Error>> {
		let validated_transactions =
			self.verify(at, xts, CheckBannedBeforeVerify::Yes, validation_priority).await;
		self.validated_pool.submit(validated_transactions.into_values())
	}

	/// Resubmit the given extrinsics to the pool.
	///
	/// This does not check if a transaction is banned, before we verify it again.
	pub async fn resubmit_at(
		&self,
		at: &HashAndNumber<B::Block>,
		xts: impl IntoIterator<Item = (base::TimedTransactionSource, ExtrinsicFor<B>)>,
		validation_priority: ValidateTransactionPriority,
	) -> Vec<Result<ValidatedPoolSubmitOutcome<B>, B::Error>> {
		let validated_transactions =
			self.verify(at, xts, CheckBannedBeforeVerify::No, validation_priority).await;
		self.validated_pool.submit(validated_transactions.into_values())
	}

	/// Imports one unverified extrinsic to the pool
	pub async fn submit_one(
		&self,
		at: &HashAndNumber<B::Block>,
		source: base::TimedTransactionSource,
		xt: ExtrinsicFor<B>,
	) -> Result<ValidatedPoolSubmitOutcome<B>, B::Error> {
		let res = self
			.submit_at(at, std::iter::once((source, xt)), ValidateTransactionPriority::Submitted)
			.await
			.pop();
		res.expect("One extrinsic passed; one result returned; qed")
	}

	/// Import a single extrinsic and starts to watch its progress in the pool.
	pub async fn submit_and_watch(
		&self,
		at: &HashAndNumber<B::Block>,
		source: base::TimedTransactionSource,
		xt: ExtrinsicFor<B>,
	) -> Result<ValidatedPoolSubmitOutcome<B>, B::Error> {
		let (_, tx) = self
			.verify_one(
				at.hash,
				at.number,
				source,
				xt,
				CheckBannedBeforeVerify::Yes,
				ValidateTransactionPriority::Submitted,
			)
			.await;
		self.validated_pool.submit_and_watch(tx)
	}

	/// Resubmit some transaction that were validated elsewhere.
	pub fn resubmit(
		&self,
		revalidated_transactions: IndexMap<ExtrinsicHash<B>, ValidatedTransactionFor<B>>,
	) {
		let now = Instant::now();
		self.validated_pool.resubmit(revalidated_transactions);
		trace!(
			target: LOG_TARGET,
			duration = ?now.elapsed(),
			status = ?self.validated_pool.status(),
			"Resubmitted transaction."
		);
	}

	/// Prunes known ready transactions.
	///
	/// Used to clear the pool from transactions that were part of recently imported block.
	/// The main difference from the `prune` is that we do not revalidate any transactions
	/// and ignore unknown passed hashes.
	pub fn prune_known(&self, at: &HashAndNumber<B::Block>, hashes: &[ExtrinsicHash<B>]) {
		// Get details of all extrinsics that are already in the pool
		let in_pool_tags =
			self.validated_pool.extrinsics_tags(hashes).into_iter().flatten().flatten();

		// Prune all transactions that provide given tags
		let prune_status = self.validated_pool.prune_tags(in_pool_tags);
		let pruned_transactions =
			hashes.iter().cloned().chain(prune_status.pruned.iter().map(|tx| tx.hash));
		self.validated_pool.fire_pruned(at, pruned_transactions);
	}

	/// Prunes ready transactions.
	///
	/// Used to clear the pool from transactions that were part of recently imported block.
	/// To perform pruning we need the tags that each extrinsic provides and to avoid calling
	/// into runtime too often we first look up all extrinsics that are in the pool and get
	/// their provided tags from there. Otherwise we query the runtime at the `parent` block.
	pub async fn prune(
		&self,
		at: &HashAndNumber<B::Block>,
		parent: <B::Block as BlockT>::Hash,
		extrinsics: &[RawExtrinsicFor<B>],
		known_provides_tags: Option<Arc<HashMap<ExtrinsicHash<B>, Vec<Tag>>>>,
	) {
		debug!(
			target: LOG_TARGET,
			?at,
			extrinsics_count = extrinsics.len(),
			"Starting pruning of block."
		);
		// Get details of all extrinsics that are already in the pool
		let in_pool_hashes =
			extrinsics.iter().map(|extrinsic| self.hash_of(extrinsic)).collect::<Vec<_>>();
		let in_pool_tags = self.validated_pool.extrinsics_tags(&in_pool_hashes);
		// Fill unknown tags based on the known tags given in `known_provides_tags`.
		let mut unknown_txs_count = 0usize;
		let mut reused_txs_count = 0usize;
		let tags = in_pool_hashes.iter().zip(in_pool_tags).map(|(tx_hash, tags)| {
			tags.or_else(|| {
				unknown_txs_count += 1;
				known_provides_tags.as_ref().and_then(|inner| {
					inner.get(&tx_hash).map(|found_tags| {
						reused_txs_count += 1;
						found_tags.clone()
					})
				})
			})
		});

		// Zip the ones from the pool with the full list (we get pairs `(Extrinsic,
		// Option<Vec<Tag>>)`)
		let all = extrinsics.iter().zip(tags);
		let mut validated_counter: usize = 0;
		let mut future_tags = Vec::new();
		let now = Instant::now();
		for (extrinsic, in_pool_tags) in all {
			match in_pool_tags {
				// reuse the tags for extrinsics that were found in the pool or given in
				// `known_provides_tags` cache.
				Some(tags) => future_tags.extend(tags),
				// if it's not found in the pool query the runtime at parent block
				// to get validity info and tags that the extrinsic provides.
				None => {
					// Avoid validating block txs if the pool is empty
					if !self.validated_pool.status().is_empty() {
						validated_counter = validated_counter + 1;
						let validity = self
							.validated_pool
							.api()
							.validate_transaction(
								parent,
								TransactionSource::InBlock,
								Arc::from(extrinsic.clone()),
								ValidateTransactionPriority::Maintained,
							)
							.await;

						trace!(
							target: LOG_TARGET,
							tx_hash = ?self.validated_pool.api().hash_and_length(&extrinsic.clone()).0,
							?validity,
							"prune::revalidated"
						);
						if let Ok(Ok(validity)) = validity {
							future_tags.extend(validity.provides);
						}
					} else {
						trace!(
							target: LOG_TARGET,
							?at,
							"txpool is empty, skipping validation for block",
						);
					}
				},
			}
		}

		let known_provides_tags_len = known_provides_tags.map(|inner| inner.len()).unwrap_or(0);
		debug!(
			target: LOG_TARGET,
			validated_counter,
			known_provides_tags_len,
			unknown_txs_count,
			reused_txs_count,
			duration = ?now.elapsed(),
			"prune"
		);
		self.prune_tags(at, future_tags, in_pool_hashes).await
	}

	/// Prunes ready transactions that provide given list of tags.
	///
	/// Given tags are assumed to be always provided now, so all transactions
	/// in the Future Queue that require that particular tag (and have other
	/// requirements satisfied) are promoted to Ready Queue.
	///
	/// Moreover for each provided tag we remove transactions in the pool that:
	/// 1. Provide that tag directly
	/// 2. Are a dependency of pruned transaction.
	///
	/// Returns transactions that have been removed from the pool and must be reverified
	/// before reinserting to the pool.
	///
	/// By removing predecessor transactions as well we might actually end up
	/// pruning too much, so all removed transactions are reverified against
	/// the runtime (`validate_transaction`) to make sure they are invalid.
	///
	/// However we avoid revalidating transactions that are contained within
	/// the second parameter of `known_imported_hashes`. These transactions
	/// (if pruned) are not revalidated and become temporarily banned to
	/// prevent importing them in the (near) future.
	pub async fn prune_tags(
		&self,
		at: &HashAndNumber<B::Block>,
		tags: impl IntoIterator<Item = Tag>,
		known_imported_hashes: impl IntoIterator<Item = ExtrinsicHash<B>> + Clone,
	) {
		let now = Instant::now();
		trace!(target: LOG_TARGET, ?at, "Pruning tags.");
		// Prune all transactions that provide given tags
		let prune_status = self.validated_pool.prune_tags(tags);

		// Make sure that we don't revalidate extrinsics that were part of the recently
		// imported block. This is especially important for UTXO-like chains cause the
		// inputs are pruned so such transaction would go to future again.
		self.validated_pool.ban(
			&Instant::now(),
			known_imported_hashes.clone().into_iter(),
			crate::graph::rotator::BanReason::Validation,
		);

		// Try to re-validate pruned transactions since some of them might be still valid.
		// note that `known_imported_hashes` will be rejected here due to temporary ban.
		let pruned_transactions =
			prune_status.pruned.into_iter().map(|tx| (tx.source.clone(), tx.data.clone()));

		let reverified_transactions = self
			.verify(
				at,
				pruned_transactions,
				CheckBannedBeforeVerify::Yes,
				ValidateTransactionPriority::Maintained,
			)
			.await;

		let pruned_hashes = reverified_transactions.keys().map(Clone::clone).collect::<Vec<_>>();
		debug!(
			target: LOG_TARGET,
			?at,
			reverified_transactions = reverified_transactions.len(),
			duration = ?now.elapsed(),
			"Pruned. Resubmitting transactions."
		);
		log_xt_trace!(data: tuple, target: LOG_TARGET, &reverified_transactions, "Resubmitting transaction: {:?}");

		// And finally - submit reverified transactions back to the pool
		self.validated_pool.resubmit_pruned(
			&at,
			known_imported_hashes,
			pruned_hashes,
			reverified_transactions.into_values().collect(),
		)
	}

	/// Returns transaction hash
	pub fn hash_of(&self, xt: &RawExtrinsicFor<B>) -> ExtrinsicHash<B> {
		self.validated_pool.api().hash_and_length(xt).0
	}

	/// Returns future that validates a bunch of transactions at given block.
	#[instrument(level = Level::TRACE, skip_all, target = "txpool",name = "pool::verify")]
	async fn verify(
		&self,
		at: &HashAndNumber<B::Block>,
		xts: impl IntoIterator<Item = (base::TimedTransactionSource, ExtrinsicFor<B>)>,
		check: CheckBannedBeforeVerify,
		validation_priority: ValidateTransactionPriority,
	) -> IndexMap<ExtrinsicHash<B>, ValidatedTransactionFor<B>> {
		let HashAndNumber { number, hash } = *at;

		let res = futures::future::join_all(xts.into_iter().map(|(source, xt)| {
			self.verify_one(hash, number, source, xt, check, validation_priority)
		}))
		.await
		.into_iter()
		.collect::<IndexMap<_, _>>();

		res
	}

	/// Returns future that validates single transaction at given block.
	#[instrument(level = Level::TRACE, skip_all, target = "txpool",name = "pool::verify_one")]
	pub(crate) async fn verify_one(
		&self,
		block_hash: <B::Block as BlockT>::Hash,
		block_number: NumberFor<B>,
		source: base::TimedTransactionSource,
		xt: ExtrinsicFor<B>,
		check: CheckBannedBeforeVerify,
		validation_priority: ValidateTransactionPriority,
	) -> (ExtrinsicHash<B>, ValidatedTransactionFor<B>) {
		let (hash, bytes) = self.validated_pool.api().hash_and_length(&xt);

		let ignore_banned = matches!(check, CheckBannedBeforeVerify::No);
		if let Err(err) = self.validated_pool.check_is_known(&hash, ignore_banned) {
			return (hash, ValidatedTransaction::Invalid(hash, err));
		}

		let validation_result = self
			.validated_pool
			.api()
			.validate_transaction(
				block_hash,
				source.clone().into(),
				xt.clone(),
				validation_priority,
			)
			.await;

		let status = match validation_result {
			Ok(status) => status,
			Err(e) => return (hash, ValidatedTransaction::Invalid(hash, e)),
		};

		let validity = match status {
			Ok(validity) => {
				if validity.provides.is_empty() {
					ValidatedTransaction::Invalid(hash, error::Error::NoTagsProvided.into())
				} else {
					ValidatedTransaction::valid_at(
						block_number.saturated_into::<u64>(),
						hash,
						source,
						xt,
						bytes,
						validity,
					)
				}
			},
			Err(TransactionValidityError::Invalid(e)) => {
				ValidatedTransaction::Invalid(hash, error::Error::InvalidTransaction(e).into())
			},
			Err(TransactionValidityError::Unknown(e)) => {
				ValidatedTransaction::Unknown(hash, error::Error::UnknownTransaction(e).into())
			},
		};

		(hash, validity)
	}

	/// Get a reference to the underlying validated pool.
	pub fn validated_pool(&self) -> &ValidatedPool<B, L> {
		&self.validated_pool
	}

	/// Clears the recently pruned transactions in validated pool.
	pub fn clear_recently_pruned(&mut self) {
		self.validated_pool.pool.write().clear_recently_pruned();
	}
}

impl<B: ChainApi, L: EventHandler<B>> Pool<B, L> {
	/// Deep clones the pool.
	///
	/// Must be called on purpose: it duplicates all the internal structures.
	pub fn deep_clone_with_event_handler(&self, event_handler: L) -> Self {
		let other: ValidatedPool<B, L> =
			self.validated_pool().deep_clone_with_event_handler(event_handler);
		Self { validated_pool: Arc::from(other) }
	}
}

#[cfg(test)]
mod tests {
	use super::{super::base_pool::Limit, *};
	use crate::common::tests::{pool, uxt, TestApi, INVALID_NONCE};
	use assert_matches::assert_matches;
	use base::TimedTransactionSource;
	use codec::Encode;
	use futures::executor::block_on;
	use parking_lot::Mutex;
	use sc_transaction_pool_api::TransactionStatus;
	use sp_runtime::transaction_validity::TransactionSource;
	use std::{collections::HashMap, time::Instant};
	use substrate_test_runtime::{AccountId, ExtrinsicBuilder, Transfer, H256};
	use substrate_test_runtime_client::Sr25519Keyring::{Alice, Bob};

	const SOURCE: TimedTransactionSource =
		TimedTransactionSource { source: TransactionSource::External, timestamp: None };

	type Pool<Api> = super::Pool<Api, ()>;

	#[test]
	fn should_validate_and_import_transaction() {
		// given
		let (pool, api) = pool();

		// when
		let hash = block_on(
			pool.submit_one(
				&api.expect_hash_and_number(0),
				SOURCE,
				uxt(Transfer {
					from: Alice.into(),
					to: AccountId::from_h256(H256::from_low_u64_be(2)),
					amount: 5,
					nonce: 0,
				})
				.into(),
			),
		)
		.map(|outcome| outcome.hash())
		.unwrap();

		// then
		assert_eq!(pool.validated_pool().ready().map(|v| v.hash).collect::<Vec<_>>(), vec![hash]);
	}

	#[test]
	fn submit_at_preserves_order() {
		sp_tracing::try_init_simple();
		// given
		let (pool, api) = pool();

		let txs = (0..10)
			.map(|i| {
				uxt(Transfer {
					from: Alice.into(),
					to: AccountId::from_h256(H256::from_low_u64_be(i)),
					amount: 5,
					nonce: i,
				})
				.into()
			})
			.collect::<Vec<_>>();

		let initial_hashes = txs.iter().map(|t| api.hash_and_length(t).0).collect::<Vec<_>>();

		// when
		let txs = txs.into_iter().map(|x| (SOURCE, Arc::from(x))).collect::<Vec<_>>();
		let hashes = block_on(pool.submit_at(
			&api.expect_hash_and_number(0),
			txs,
			ValidateTransactionPriority::Submitted,
		))
		.into_iter()
		.map(|r| r.map(|o| o.hash()))
		.collect::<Vec<_>>();
		debug!(hashes = ?hashes, "-->");

		// then
		hashes.into_iter().zip(initial_hashes.into_iter()).for_each(
			|(result_hash, initial_hash)| {
				assert_eq!(result_hash.unwrap(), initial_hash);
			},
		);
	}

	#[test]
	fn should_reject_if_temporarily_banned() {
		// given
		let (pool, api) = pool();
		let uxt = uxt(Transfer {
			from: Alice.into(),
			to: AccountId::from_h256(H256::from_low_u64_be(2)),
			amount: 5,
			nonce: 0,
		});

		// when
		pool.validated_pool.ban(
			&Instant::now(),
			vec![pool.hash_of(&uxt)],
			crate::graph::rotator::BanReason::Validation,
		);
		let res = block_on(pool.submit_one(&api.expect_hash_and_number(0), SOURCE, uxt.into()))
			.map(|o| o.hash());
		assert_eq!(pool.validated_pool().status().ready, 0);
		assert_eq!(pool.validated_pool().status().future, 0);

		// then
		assert_matches!(res.unwrap_err(), error::Error::TemporarilyBanned);
	}

	#[test]
	fn should_reject_unactionable_transactions() {
		// given
		let api = Arc::new(TestApi::default());
		let pool = Pool::new_with_staticly_sized_rotator(
			Default::default(),
			// the node does not author blocks
			false.into(),
			api.clone(),
		);

		// after validation `IncludeData` will be set to non-propagable (validate_transaction mock)
		let uxt = ExtrinsicBuilder::new_include_data(vec![42]).build();

		// when
		let res = block_on(pool.submit_one(&api.expect_hash_and_number(0), SOURCE, uxt.into()))
			.map(|o| o.hash());

		// then
		assert_matches!(res.unwrap_err(), error::Error::Unactionable);
	}

	#[test]
	fn should_notify_about_pool_events() {
		let (stream, hash0, hash1) = {
			// given
			let (pool, api) = pool();
			let han_of_block0 = api.expect_hash_and_number(0);
			let stream = pool.validated_pool().import_notification_stream();

			// when
			let hash0 = block_on(
				pool.submit_one(
					&han_of_block0,
					SOURCE,
					uxt(Transfer {
						from: Alice.into(),
						to: AccountId::from_h256(H256::from_low_u64_be(2)),
						amount: 5,
						nonce: 0,
					})
					.into(),
				),
			)
			.unwrap()
			.hash();
			let hash1 = block_on(
				pool.submit_one(
					&han_of_block0,
					SOURCE,
					uxt(Transfer {
						from: Alice.into(),
						to: AccountId::from_h256(H256::from_low_u64_be(2)),
						amount: 5,
						nonce: 1,
					})
					.into(),
				),
			)
			.unwrap()
			.hash();
			// future doesn't count
			let _hash = block_on(
				pool.submit_one(
					&han_of_block0,
					SOURCE,
					uxt(Transfer {
						from: Alice.into(),
						to: AccountId::from_h256(H256::from_low_u64_be(2)),
						amount: 5,
						nonce: 3,
					})
					.into(),
				),
			)
			.unwrap()
			.hash();

			assert_eq!(pool.validated_pool().status().ready, 2);
			assert_eq!(pool.validated_pool().status().future, 1);

			(stream, hash0, hash1)
		};

		// then
		let mut it = futures::executor::block_on_stream(stream);
		assert_eq!(it.next(), Some(hash0));
		assert_eq!(it.next(), Some(hash1));
		assert_eq!(it.next(), None);
	}

	#[test]
	fn should_clear_stale_transactions() {
		// given
		let (pool, api) = pool();
		let han_of_block0 = api.expect_hash_and_number(0);
		let hash1 = block_on(
			pool.submit_one(
				&han_of_block0,
				SOURCE,
				uxt(Transfer {
					from: Alice.into(),
					to: AccountId::from_h256(H256::from_low_u64_be(2)),
					amount: 5,
					nonce: 0,
				})
				.into(),
			),
		)
		.unwrap()
		.hash();
		let hash2 = block_on(
			pool.submit_one(
				&han_of_block0,
				SOURCE,
				uxt(Transfer {
					from: Alice.into(),
					to: AccountId::from_h256(H256::from_low_u64_be(2)),
					amount: 5,
					nonce: 1,
				})
				.into(),
			),
		)
		.unwrap()
		.hash();
		let hash3 = block_on(
			pool.submit_one(
				&han_of_block0,
				SOURCE,
				uxt(Transfer {
					from: Alice.into(),
					to: AccountId::from_h256(H256::from_low_u64_be(2)),
					amount: 5,
					nonce: 3,
				})
				.into(),
			),
		)
		.unwrap()
		.hash();

		// when
		pool.validated_pool.clear_stale(&api.expect_hash_and_number(5));

		// then
		assert_eq!(pool.validated_pool().ready().count(), 0);
		assert_eq!(pool.validated_pool().status().future, 0);
		assert_eq!(pool.validated_pool().status().ready, 0);
		// make sure they are temporarily banned as well
		assert!(pool.validated_pool.is_banned(&hash1));
		assert!(pool.validated_pool.is_banned(&hash2));
		assert!(pool.validated_pool.is_banned(&hash3));
	}

	#[test]
	fn should_ban_mined_transactions() {
		// given
		let (pool, api) = pool();
		let hash1 = block_on(
			pool.submit_one(
				&api.expect_hash_and_number(0),
				SOURCE,
				uxt(Transfer {
					from: Alice.into(),
					to: AccountId::from_h256(H256::from_low_u64_be(2)),
					amount: 5,
					nonce: 0,
				})
				.into(),
			),
		)
		.unwrap()
		.hash();

		// when
		block_on(pool.prune_tags(&api.expect_hash_and_number(1), vec![vec![0]], vec![hash1]));

		// then
		assert!(pool.validated_pool.is_banned(&hash1));
	}

	#[test]
	fn should_limit_futures() {
		sp_tracing::try_init_simple();

		let xt = uxt(Transfer {
			from: Alice.into(),
			to: AccountId::from_h256(H256::from_low_u64_be(2)),
			amount: 5,
			nonce: 1,
		});

		// given
		let limit = Limit { count: 100, total_bytes: xt.encoded_size() };

		let options = Options { ready: limit.clone(), future: limit.clone(), ..Default::default() };

		let api = Arc::new(TestApi::default());
		let pool = Pool::new_with_staticly_sized_rotator(options, true.into(), api.clone());

		let hash1 = block_on(pool.submit_one(&api.expect_hash_and_number(0), SOURCE, xt.into()))
			.unwrap()
			.hash();
		assert_eq!(pool.validated_pool().status().future, 1);

		// when
		let hash2 = block_on(
			pool.submit_one(
				&api.expect_hash_and_number(0),
				SOURCE,
				uxt(Transfer {
					from: Bob.into(),
					to: AccountId::from_h256(H256::from_low_u64_be(2)),
					amount: 5,
					nonce: 10,
				})
				.into(),
			),
		)
		.unwrap()
		.hash();

		// then
		assert_eq!(pool.validated_pool().status().future, 1);
		assert!(pool.validated_pool.is_banned(&hash1));
		assert!(!pool.validated_pool.is_banned(&hash2));
	}

	#[test]
	fn should_error_if_reject_immediately() {
		// given
		let limit = Limit { count: 100, total_bytes: 10 };

		let options = Options { ready: limit.clone(), future: limit.clone(), ..Default::default() };

		let api = Arc::new(TestApi::default());
		let pool = Pool::new_with_staticly_sized_rotator(options, true.into(), api.clone());

		// when
		block_on(
			pool.submit_one(
				&api.expect_hash_and_number(0),
				SOURCE,
				uxt(Transfer {
					from: Alice.into(),
					to: AccountId::from_h256(H256::from_low_u64_be(2)),
					amount: 5,
					nonce: 1,
				})
				.into(),
			),
		)
		.map(|o| o.hash())
		.unwrap_err();

		// then
		assert_eq!(pool.validated_pool().status().ready, 0);
		assert_eq!(pool.validated_pool().status().future, 0);
	}

	#[test]
	fn should_reject_transactions_with_no_provides() {
		// given
		let (pool, api) = pool();

		// when
		let err = block_on(
			pool.submit_one(
				&api.expect_hash_and_number(0),
				SOURCE,
				uxt(Transfer {
					from: Alice.into(),
					to: AccountId::from_h256(H256::from_low_u64_be(2)),
					amount: 5,
					nonce: INVALID_NONCE,
				})
				.into(),
			),
		)
		.map(|o| o.hash())
		.unwrap_err();

		// then
		assert_eq!(pool.validated_pool().status().ready, 0);
		assert_eq!(pool.validated_pool().status().future, 0);
		assert_matches!(err, error::Error::NoTagsProvided);
	}

	mod listener {
		use super::*;

		#[test]
		fn should_trigger_ready_and_in_block_when_pruning_via_hash() {
			// given
			let (pool, api) = pool();
			let watcher = block_on(
				pool.submit_and_watch(
					&api.expect_hash_and_number(0),
					SOURCE,
					uxt(Transfer {
						from: Alice.into(),
						to: AccountId::from_h256(H256::from_low_u64_be(2)),
						amount: 5,
						nonce: 0,
					})
					.into(),
				),
			)
			.unwrap()
			.expect_watcher();
			assert_eq!(pool.validated_pool().status().ready, 1);
			assert_eq!(pool.validated_pool().status().future, 0);

			let han_of_block2 = api.expect_hash_and_number(2);

			// when
			block_on(pool.prune_tags(&han_of_block2, vec![vec![0u8]], vec![*watcher.hash()]));
			assert_eq!(pool.validated_pool().status().ready, 0);
			assert_eq!(pool.validated_pool().status().future, 0);

			// then
			let mut stream = futures::executor::block_on_stream(watcher.into_stream());
			assert_eq!(stream.next(), Some(TransactionStatus::Ready));
			assert_eq!(
				stream.next(),
				Some(TransactionStatus::InBlock((han_of_block2.hash.into(), 0))),
			);
		}

		#[test]
		fn should_trigger_future_and_ready_after_promoted() {
			// given
			let (pool, api) = pool();
			let han_of_block0 = api.expect_hash_and_number(0);

			let watcher = block_on(
				pool.submit_and_watch(
					&han_of_block0,
					SOURCE,
					uxt(Transfer {
						from: Alice.into(),
						to: AccountId::from_h256(H256::from_low_u64_be(2)),
						amount: 5,
						nonce: 1,
					})
					.into(),
				),
			)
			.unwrap()
			.expect_watcher();
			assert_eq!(pool.validated_pool().status().ready, 0);
			assert_eq!(pool.validated_pool().status().future, 1);

			// when
			block_on(
				pool.submit_one(
					&han_of_block0,
					SOURCE,
					uxt(Transfer {
						from: Alice.into(),
						to: AccountId::from_h256(H256::from_low_u64_be(2)),
						amount: 5,
						nonce: 0,
					})
					.into(),
				),
			)
			.unwrap();
			assert_eq!(pool.validated_pool().status().ready, 2);

			// then
			let mut stream = futures::executor::block_on_stream(watcher.into_stream());
			assert_eq!(stream.next(), Some(TransactionStatus::Future));
			assert_eq!(stream.next(), Some(TransactionStatus::Ready));
		}

		#[test]
		fn should_trigger_invalid_and_ban() {
			// given
			let (pool, api) = pool();
			let uxt = uxt(Transfer {
				from: Alice.into(),
				to: AccountId::from_h256(H256::from_low_u64_be(2)),
				amount: 5,
				nonce: 0,
			});
			let watcher =
				block_on(pool.submit_and_watch(&api.expect_hash_and_number(0), SOURCE, uxt.into()))
					.unwrap()
					.expect_watcher();
			assert_eq!(pool.validated_pool().status().ready, 1);

			// when
			pool.validated_pool.remove_invalid(&[*watcher.hash()]);

			// then
			let mut stream = futures::executor::block_on_stream(watcher.into_stream());
			assert_eq!(stream.next(), Some(TransactionStatus::Ready));
			assert_eq!(stream.next(), Some(TransactionStatus::Invalid));
			assert_eq!(stream.next(), None);
		}

		#[test]
		fn should_trigger_broadcasted() {
			// given
			let (pool, api) = pool();
			let uxt = uxt(Transfer {
				from: Alice.into(),
				to: AccountId::from_h256(H256::from_low_u64_be(2)),
				amount: 5,
				nonce: 0,
			});
			let watcher =
				block_on(pool.submit_and_watch(&api.expect_hash_and_number(0), SOURCE, uxt.into()))
					.unwrap()
					.expect_watcher();
			assert_eq!(pool.validated_pool().status().ready, 1);

			// when
			let mut map = HashMap::new();
			let peers = vec!["a".into(), "b".into(), "c".into()];
			map.insert(*watcher.hash(), peers.clone());
			pool.validated_pool().on_broadcasted(map);

			// then
			let mut stream = futures::executor::block_on_stream(watcher.into_stream());
			assert_eq!(stream.next(), Some(TransactionStatus::Ready));
			assert_eq!(stream.next(), Some(TransactionStatus::Broadcast(peers)));
		}

		#[test]
		fn should_trigger_dropped_older() {
			// given
			let limit = Limit { count: 1, total_bytes: 1000 };
			let options =
				Options { ready: limit.clone(), future: limit.clone(), ..Default::default() };

			let api = Arc::new(TestApi::default());
			let pool = Pool::new_with_staticly_sized_rotator(options, true.into(), api.clone());

			let xt = uxt(Transfer {
				from: Alice.into(),
				to: AccountId::from_h256(H256::from_low_u64_be(2)),
				amount: 5,
				nonce: 0,
			});
			let watcher =
				block_on(pool.submit_and_watch(&api.expect_hash_and_number(0), SOURCE, xt.into()))
					.unwrap()
					.expect_watcher();
			assert_eq!(pool.validated_pool().status().ready, 1);

			// when
			let xt = uxt(Transfer {
				from: Bob.into(),
				to: AccountId::from_h256(H256::from_low_u64_be(1)),
				amount: 4,
				nonce: 1,
			});
			block_on(pool.submit_one(&api.expect_hash_and_number(1), SOURCE, xt.into())).unwrap();
			assert_eq!(pool.validated_pool().status().ready, 1);

			// then
			let mut stream = futures::executor::block_on_stream(watcher.into_stream());
			assert_eq!(stream.next(), Some(TransactionStatus::Ready));
			assert_eq!(stream.next(), Some(TransactionStatus::Dropped));
		}

		#[test]
		fn should_trigger_dropped_lower_priority() {
			{
				// given
				let limit = Limit { count: 1, total_bytes: 1000 };
				let options =
					Options { ready: limit.clone(), future: limit.clone(), ..Default::default() };

				let api = Arc::new(TestApi::default());
				let pool = Pool::new_with_staticly_sized_rotator(options, true.into(), api.clone());

				// after validation `IncludeData` will have priority set to 9001
				// (validate_transaction mock)
				let xt = ExtrinsicBuilder::new_include_data(Vec::new()).build();
				block_on(pool.submit_one(&api.expect_hash_and_number(0), SOURCE, xt.into()))
					.unwrap();
				assert_eq!(pool.validated_pool().status().ready, 1);

				// then
				// after validation `Transfer` will have priority set to 4 (validate_transaction
				// mock)
				let xt = uxt(Transfer {
					from: Bob.into(),
					to: AccountId::from_h256(H256::from_low_u64_be(1)),
					amount: 4,
					nonce: 1,
				});
				let result =
					block_on(pool.submit_one(&api.expect_hash_and_number(1), SOURCE, xt.into()));
				assert!(matches!(
					result,
					Err(sc_transaction_pool_api::error::Error::ImmediatelyDropped)
				));
			}
			{
				// given
				let limit = Limit { count: 2, total_bytes: 1000 };
				let options =
					Options { ready: limit.clone(), future: limit.clone(), ..Default::default() };

				let api = Arc::new(TestApi::default());
				let pool = Pool::new_with_staticly_sized_rotator(options, true.into(), api.clone());

				let han_of_block0 = api.expect_hash_and_number(0);

				// after validation `IncludeData` will have priority set to 9001
				// (validate_transaction mock)
				let xt = ExtrinsicBuilder::new_include_data(Vec::new()).build();
				block_on(pool.submit_and_watch(&han_of_block0, SOURCE, xt.into()))
					.unwrap()
					.expect_watcher();
				assert_eq!(pool.validated_pool().status().ready, 1);

				// after validation `Transfer` will have priority set to 4 (validate_transaction
				// mock)
				let xt = uxt(Transfer {
					from: Alice.into(),
					to: AccountId::from_h256(H256::from_low_u64_be(2)),
					amount: 5,
					nonce: 0,
				});
				let watcher = block_on(pool.submit_and_watch(&han_of_block0, SOURCE, xt.into()))
					.unwrap()
					.expect_watcher();
				assert_eq!(pool.validated_pool().status().ready, 2);

				// when
				// after validation `Store` will have priority set to 9001 (validate_transaction
				// mock)
				let xt = ExtrinsicBuilder::new_indexed_call(Vec::new()).build();
				block_on(pool.submit_one(&api.expect_hash_and_number(1), SOURCE, xt.into()))
					.unwrap();
				assert_eq!(pool.validated_pool().status().ready, 2);

				// then
				let mut stream = futures::executor::block_on_stream(watcher.into_stream());
				assert_eq!(stream.next(), Some(TransactionStatus::Ready));
				assert_eq!(stream.next(), Some(TransactionStatus::Dropped));
			}
		}

		#[test]
		fn should_handle_pruning_in_the_middle_of_import() {
			// given
			let (ready, is_ready) = std::sync::mpsc::sync_channel(0);
			let (tx, rx) = std::sync::mpsc::sync_channel(1);
			let mut api = TestApi::default();
			api.delay = Arc::new(Mutex::new(rx.into()));
			let api = Arc::new(api);
			let pool = Arc::new(Pool::new_with_staticly_sized_rotator(
				Default::default(),
				true.into(),
				api.clone(),
			));

			let han_of_block0 = api.expect_hash_and_number(0);

			// when
			let xt = uxt(Transfer {
				from: Alice.into(),
				to: AccountId::from_h256(H256::from_low_u64_be(2)),
				amount: 5,
				nonce: 1,
			});

			// This transaction should go to future, since we use `nonce: 1`
			let pool2 = pool.clone();
			std::thread::spawn({
				let hash_of_block0 = han_of_block0.clone();
				move || {
					block_on(pool2.submit_one(&hash_of_block0, SOURCE, xt.into())).unwrap();
					ready.send(()).unwrap();
				}
			});

			// But now before the previous one is imported we import
			// the one that it depends on.
			let xt = uxt(Transfer {
				from: Alice.into(),
				to: AccountId::from_h256(H256::from_low_u64_be(2)),
				amount: 4,
				nonce: 0,
			});
			// The tag the above transaction provides (TestApi is using just nonce as u8)
			let provides = vec![0_u8];
			block_on(pool.submit_one(&han_of_block0, SOURCE, xt.into())).unwrap();
			assert_eq!(pool.validated_pool().status().ready, 1);

			// Now block import happens before the second transaction is able to finish
			// verification.
			block_on(pool.prune_tags(&api.expect_hash_and_number(1), vec![provides], vec![]));
			assert_eq!(pool.validated_pool().status().ready, 0);

			// so when we release the verification of the previous one it will have
			// something in `requires`, but should go to ready directly, since the previous
			// transaction was imported correctly.
			tx.send(()).unwrap();

			// then
			is_ready.recv().unwrap(); // wait for finish
			assert_eq!(pool.validated_pool().status().ready, 1);
			assert_eq!(pool.validated_pool().status().future, 0);
		}
	}
}
