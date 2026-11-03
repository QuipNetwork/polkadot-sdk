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

//! Transaction pool client facing API.
#![warn(missing_docs)]

pub mod error;

use async_trait::async_trait;
use codec::Codec;
use futures::Stream;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sp_core::offchain::TransactionPoolExt;
use sp_runtime::traits::{Block as BlockT, Member};
use std::{collections::HashMap, hash::Hash, marker::PhantomData, pin::Pin, sync::Arc};

const LOG_TARGET: &str = "txpool::api";

pub use sp_runtime::transaction_validity::{
	TransactionLongevity, TransactionPriority, TransactionSource, TransactionTag,
	TransactionValidityError,
};

/// Transaction pool status.
#[derive(Debug, Clone)]
pub struct PoolStatus {
	/// Number of transactions in the ready queue.
	pub ready: usize,
	/// Sum of bytes of ready transaction encodings.
	pub ready_bytes: usize,
	/// Number of transactions in the future queue.
	pub future: usize,
	/// Sum of bytes of ready transaction encodings.
	pub future_bytes: usize,
}

impl PoolStatus {
	/// Returns true if there are no transactions in the pool.
	pub fn is_empty(&self) -> bool {
		self.ready == 0 && self.future == 0
	}
}

/// Possible transaction status events.
///
/// These events are being emitted by `TransactionPool` watchers,
/// which are also exposed over RPC.
///
/// The status events can be grouped based on their kinds as:
/// 1. Entering/Moving within the pool:
/// 		- [Future](TransactionStatus::Future)
/// 		- [Ready](TransactionStatus::Ready)
/// 2. Inside `Ready` queue:
/// 		- [Broadcast](TransactionStatus::Broadcast)
/// 3. Leaving the pool:
/// 		- [InBlock](TransactionStatus::InBlock)
/// 		- [Invalid](TransactionStatus::Invalid)
/// 		- [Usurped](TransactionStatus::Usurped)
/// 		- [Dropped](TransactionStatus::Dropped)
/// 	4. Re-entering the pool:
/// 		- [Retracted](TransactionStatus::Retracted)
/// 	5. Block finalized:
/// 		- [Finalized](TransactionStatus::Finalized)
/// 		- [FinalityTimeout](TransactionStatus::FinalityTimeout)
///
/// Transactions are first placed in either the `Ready` or `Future` queues of the transaction pool.
/// Substrate validates the transaction before it enters the pool.
///
/// A transaction is placed in the `Future` queue if it will become valid at a future time.
/// For example, submitting a transaction with a higher account nonce than the current
/// expected nonce will place the transaction in the `Future` queue.
///
/// The events will always be received in the order described above, however
/// there might be cases where transactions alternate between `Future` and `Ready`
/// pool, and are `Broadcast` in the meantime.
///
/// There is also only single event causing the transaction to leave the pool.
/// I.e. only one of the listed ones should be triggered.
///
/// Note that there are conditions that may cause transactions to reappear in the pool.
/// 1. Due to possible forks, the transaction that ends up being in included
/// in one block, may later re-enter the pool or be marked as invalid.
/// 2. Transaction `Dropped` at one point, may later re-enter the pool if some other
/// transactions are removed. A `Dropped` transaction may re-enter the pool only if it is
/// resubmitted.
/// 3. `Invalid` transaction may become valid at some point in the future.
/// (Note that runtimes are encouraged to use `UnknownValidity` to inform the pool about
/// such case). An `Invalid` transaction may re-enter the pool only if it is resubmitted.
/// 4. `Retracted` transactions might be included in some next block.
///
/// The `FinalityTimeout` event will be emitted when the block did not reach finality
/// within 512 blocks. This either indicates that finality is not available for your chain,
/// or that finality gadget is lagging behind. If you choose to wait for finality longer, you can
/// re-subscribe for a particular transaction hash manually again.
///
/// ### Last Event
///
/// The stream is considered finished when one of the following events happen:
/// - [Finalized](TransactionStatus::Finalized)
/// - [FinalityTimeout](TransactionStatus::FinalityTimeout)
/// - [Usurped](TransactionStatus::Usurped)
/// - [Invalid](TransactionStatus::Invalid)
/// - [Dropped](TransactionStatus::Dropped)
///
/// See [`TransactionStatus::is_final`] for more details.
///
/// ### Resubmit Transactions
///
/// Users might resubmit the transaction at a later time for the following events:
/// - [FinalityTimeout](TransactionStatus::FinalityTimeout)
/// - [Invalid](TransactionStatus::Invalid)
/// - [Dropped](TransactionStatus::Dropped)
///
/// See [`TransactionStatus::is_retriable`] for more details.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransactionStatus<Hash, BlockHash> {
	/// Transaction is part of the future queue.
	Future,
	/// Transaction is part of the ready queue.
	Ready,
	/// The transaction has been broadcast to the given peers.
	Broadcast(Vec<String>),
	/// Transaction has been included in block with given hash
	/// at the given position.
	#[serde(with = "v1_compatible")]
	InBlock((BlockHash, TxIndex)),
	/// The block this transaction was included in has been retracted.
	Retracted(BlockHash),
	/// Maximum number of finality watchers has been reached,
	/// old watchers are being removed.
	FinalityTimeout(BlockHash),
	/// Transaction has been finalized by a finality-gadget, e.g. GRANDPA.
	#[serde(with = "v1_compatible")]
	Finalized((BlockHash, TxIndex)),
	/// Transaction has been replaced in the pool, by another transaction
	/// that provides the same tags. (e.g. same (sender, nonce)).
	Usurped(Hash),
	/// Transaction has been dropped from the pool because of the limit.
	Dropped,
	/// Transaction is no longer valid in the current state.
	Invalid,
}

impl<Hash, BlockHash> TransactionStatus<Hash, BlockHash> {
	/// Returns true if this is the last event emitted by [`TransactionStatusStream`].
	pub fn is_final(&self) -> bool {
		// The state must be kept in sync with `crate::graph::Sender`.
		match self {
			Self::Usurped(_) |
			Self::Finalized(_) |
			Self::FinalityTimeout(_) |
			Self::Invalid |
			Self::Dropped => true,
			_ => false,
		}
	}

	/// Returns true if the transaction could be re-submitted to the pool in the future.
	///
	/// For example, `TransactionStatus::Dropped` is retriable, because the transaction
	/// may enter the pool if there is space for it in the future.
	pub fn is_retriable(&self) -> bool {
		match self {
			// The number of finality watchers has been reached.
			Self::FinalityTimeout(_) |
			// An invalid transaction might be valid at a later time.
			Self::Invalid |
			// The transaction was dropped because of the limits of the pool.
			// It can reenter the pool when other transactions are removed / finalized.
			Self::Dropped => true,
			_ => false,
		}
	}
}

/// The stream of transaction events.
pub type TransactionStatusStream<Hash, BlockHash> =
	dyn Stream<Item = TransactionStatus<Hash, BlockHash>> + Send;

/// The import notification event stream.
pub type ImportNotificationStream<H> = futures::channel::mpsc::Receiver<H>;

/// Transaction hash type for a pool.
pub type TxHash<P> = <P as TransactionPool>::Hash;
/// Block hash type for a pool.
pub type BlockHash<P> = <<P as TransactionPool>::Block as BlockT>::Hash;
/// Transaction type for a pool.
pub type TransactionFor<P> = <<P as TransactionPool>::Block as BlockT>::Extrinsic;
/// Type of transactions event stream for a pool.
pub type TransactionStatusStreamFor<P> = TransactionStatusStream<TxHash<P>, BlockHash<P>>;
/// Transaction type for a local pool.
pub type LocalTransactionFor<P> = <<P as LocalTransactionPool>::Block as BlockT>::Extrinsic;
/// Transaction's index within the block in which it was included.
pub type TxIndex = usize;
/// Map containing validity errors associated with transaction hashes. Used to report invalid
/// transactions to the pool.
pub type TxInvalidityReportMap<H> = indexmap::IndexMap<H, Option<TransactionValidityError>>;

/// In-pool transaction interface.
///
/// The pool is container of transactions that are implementing this trait.
/// See `sp_runtime::ValidTransaction` for details about every field.
pub trait InPoolTransaction {
	/// Transaction type.
	type Transaction;
	/// Transaction hash type.
	type Hash;

	/// Get the reference to the transaction data.
	fn data(&self) -> &Self::Transaction;
	/// Get hash of the transaction.
	fn hash(&self) -> &Self::Hash;
	/// Get priority of the transaction.
	fn priority(&self) -> &TransactionPriority;
	/// Get longevity of the transaction.
	fn longevity(&self) -> &TransactionLongevity;
	/// Get transaction dependencies.
	fn requires(&self) -> &[TransactionTag];
	/// Get tags that transaction provides.
	fn provides(&self) -> &[TransactionTag];
	/// Return a flag indicating if the transaction should be propagated to other peers.
	fn is_propagable(&self) -> bool;
}

/// Transaction pool interface.
#[async_trait]
pub trait TransactionPool: Send + Sync {
	/// Block type.
	type Block: BlockT;
	/// Transaction hash type.
	type Hash: Hash + Eq + Member + Serialize + DeserializeOwned + Codec;
	/// In-pool transaction type.
	type InPoolTransaction: InPoolTransaction<
		Transaction = Arc<TransactionFor<Self>>,
		Hash = TxHash<Self>,
	>;
	/// Error type.
	type Error: From<crate::error::Error> + crate::error::IntoPoolError;

	// *** RPC

	/// Asynchronously imports a bunch of unverified transactions to the pool.
	async fn submit_at(
		&self,
		at: <Self::Block as BlockT>::Hash,
		source: TransactionSource,
		xts: Vec<TransactionFor<Self>>,
	) -> Result<Vec<Result<TxHash<Self>, Self::Error>>, Self::Error>;

	/// Asynchronously imports one unverified transaction to the pool.
	async fn submit_one(
		&self,
		at: <Self::Block as BlockT>::Hash,
		source: TransactionSource,
		xt: TransactionFor<Self>,
	) -> Result<TxHash<Self>, Self::Error>;

	/// Asynchronously imports a single transaction and starts to watch their progress in the
	/// pool.
	async fn submit_and_watch(
		&self,
		at: <Self::Block as BlockT>::Hash,
		source: TransactionSource,
		xt: TransactionFor<Self>,
	) -> Result<Pin<Box<TransactionStatusStreamFor<Self>>>, Self::Error>;

	// *** Block production / Networking
	/// Get an iterator for ready transactions ordered by priority.
	///
	/// Guaranteed to resolve only when transaction pool got updated at `at` block.
	/// Guaranteed to resolve immediately when `None` is passed.
	async fn ready_at(
		&self,
		at: <Self::Block as BlockT>::Hash,
	) -> Box<dyn ReadyTransactions<Item = Arc<Self::InPoolTransaction>> + Send>;

	/// Get an iterator for ready transactions ordered by priority.
	fn ready(&self) -> Box<dyn ReadyTransactions<Item = Arc<Self::InPoolTransaction>> + Send>;

	// *** Block production
	/// Reports invalid transactions to the transaction pool.
	///
	/// This function takes a map where the key is a transaction hash and the value is an
	/// optional error encountered during the transaction execution, possibly within a specific
	/// block.
	///
	/// The transaction pool implementation decides which transactions to remove. Transactions
	/// removed from the pool will be notified with `TransactionStatus::Invalid` event (if
	/// `submit_and_watch` was used for submission).
	///
	/// If the error associated to transaction is `None`, the transaction will be forcibly removed
	/// from the pool.
	///
	/// The optional `at` parameter provides additional context regarding the block where the error
	/// occurred.
	///
	/// Function returns the transactions actually removed from the pool.
	async fn report_invalid(
		&self,
		at: Option<<Self::Block as BlockT>::Hash>,
		invalid_tx_errors: TxInvalidityReportMap<TxHash<Self>>,
	) -> Vec<Arc<Self::InPoolTransaction>>;

	// *** logging
	/// Get futures transaction list.
	fn futures(&self) -> Vec<Self::InPoolTransaction>;

	/// Returns pool status.
	fn status(&self) -> PoolStatus;

	// *** logging / RPC / networking
	/// Return an event stream of transactions imported to the pool.
	fn import_notification_stream(&self) -> ImportNotificationStream<TxHash<Self>>;

	// *** networking
	/// Notify the pool about transactions broadcast.
	fn on_broadcasted(&self, propagations: HashMap<TxHash<Self>, Vec<String>>);

	/// Returns transaction hash
	fn hash_of(&self, xt: &TransactionFor<Self>) -> TxHash<Self>;

	/// Return specific ready transaction by hash, if there is one.
	fn ready_transaction(&self, hash: &TxHash<Self>) -> Option<Arc<Self::InPoolTransaction>>;

	/// Asynchronously returns a set of ready transaction at given block within given timeout.
	///
	/// If the timeout is hit during method execution, then the best effort (without executing full
	/// maintain process) set of ready transactions for given block is returned.
	async fn ready_at_with_timeout(
		&self,
		at: <Self::Block as BlockT>::Hash,
		timeout: std::time::Duration,
	) -> Box<dyn ReadyTransactions<Item = Arc<Self::InPoolTransaction>> + Send>;
}

/// An iterator of ready transactions.
///
/// The trait extends regular [`std::iter::Iterator`] trait and allows reporting
/// last-returned element as invalid.
///
/// The implementation is then allowed, for performance reasons, to change the elements
/// returned next, by e.g.  skipping elements that are known to depend on the reported
/// transaction, which yields them invalid as well.
pub trait ReadyTransactions: Iterator {
	/// Report given transaction as invalid.
	///
	/// This might affect subsequent elements returned by the iterator, so dependent transactions
	/// are skipped for performance reasons.
	fn report_invalid(&mut self, _tx: &Self::Item);
}

/// A no-op implementation for an empty iterator.
impl<T> ReadyTransactions for std::iter::Empty<T> {
	fn report_invalid(&mut self, _tx: &T) {}
}

/// Events that the transaction pool listens for.
#[derive(Debug)]
pub enum ChainEvent<B: BlockT> {
	/// A new block (which is not the new best block) was added to the chain.
	///
	/// It allows the transaction pool to maintain views for all forks, not only the best
	/// chain. Pools that only care about the best chain may ignore this event.
	NewBlock {
		/// Hash of the block.
		hash: B::Hash,
	},
	/// New best block have been added to the chain.
	NewBestBlock {
		/// Hash of the block.
		hash: B::Hash,
		/// Tree route from old best to new best parent that was calculated on import.
		///
		/// If `None`, no re-org happened on import.
		tree_route: Option<Arc<sp_blockchain::TreeRoute<B>>>,
	},
	/// An existing block has been finalized.
	Finalized {
		/// Hash of just finalized block.
		hash: B::Hash,
		/// Path from old finalized to new finalized parent.
		tree_route: Arc<[B::Hash]>,
	},
}

impl<B: BlockT> ChainEvent<B> {
	/// Returns the block hash associated to the event.
	pub fn hash(&self) -> B::Hash {
		match self {
			Self::NewBlock { hash } |
			Self::NewBestBlock { hash, .. } |
			Self::Finalized { hash, .. } => *hash,
		}
	}

	/// Is `self == Self::Finalized`?
	pub fn is_finalized(&self) -> bool {
		matches!(self, Self::Finalized { .. })
	}
}

/// Trait for transaction pool maintenance.
#[async_trait]
pub trait MaintainedTransactionPool: TransactionPool {
	/// Perform maintenance
	async fn maintain(&self, event: ChainEvent<Self::Block>);
}

/// Transaction pool interface for submitting local transactions that exposes a
/// blocking interface for submission.
pub trait LocalTransactionPool: Send + Sync {
	/// Block type.
	type Block: BlockT;
	/// Transaction hash type.
	type Hash: Hash + Eq + Member + Serialize;
	/// Error type.
	type Error: From<crate::error::Error> + crate::error::IntoPoolError;

	/// Submits the given local unverified transaction to the pool blocking the
	/// current thread for any necessary pre-verification.
	/// NOTE: It MUST NOT be used for transactions that originate from the
	/// network or RPC, since the validation is performed with
	/// `TransactionSource::Local`.
	fn submit_local(
		&self,
		at: <Self::Block as BlockT>::Hash,
		xt: LocalTransactionFor<Self>,
	) -> Result<Self::Hash, Self::Error>;
}

impl<T: LocalTransactionPool> LocalTransactionPool for Arc<T> {
	type Block = T::Block;

	type Hash = T::Hash;

	type Error = T::Error;

	fn submit_local(
		&self,
		at: <Self::Block as BlockT>::Hash,
		xt: LocalTransactionFor<Self>,
	) -> Result<Self::Hash, Self::Error> {
		(**self).submit_local(at, xt)
	}
}

/// An abstraction for [`LocalTransactionPool`]
///
/// We want to use a transaction pool in [`OffchainTransactionPoolFactory`] in a `Arc` without
/// bleeding the associated types besides the `Block`. Thus, this abstraction here exists to achieve
/// the wrapping in a `Arc`.
trait OffchainSubmitTransaction<Block: BlockT>: Send + Sync {
	/// Submit transaction.
	///
	/// The transaction will end up in the pool and be propagated to others.
	fn submit_at(&self, at: Block::Hash, extrinsic: Block::Extrinsic) -> Result<(), ()>;
}

impl<TPool: LocalTransactionPool> OffchainSubmitTransaction<TPool::Block> for TPool {
	fn submit_at(
		&self,
		at: <TPool::Block as BlockT>::Hash,
		extrinsic: <TPool::Block as BlockT>::Extrinsic,
	) -> Result<(), ()> {
		log::trace!(
			target: LOG_TARGET,
			"(offchain call) Submitting a transaction to the pool: {:?}",
			extrinsic
		);

		let result = self.submit_local(at, extrinsic);

		result.map(|_| ()).map_err(|e| {
			log::warn!(
				target: LOG_TARGET,
				"(offchain call) Error submitting a transaction to the pool: {}",
				e
			)
		})
	}
}

/// Factory for creating [`TransactionPoolExt`]s.
///
/// This provides an easy way for creating [`TransactionPoolExt`] extensions for registering them in
/// the wasm execution environment to send transactions from an offchain call to the  runtime.
#[derive(Clone)]
pub struct OffchainTransactionPoolFactory<Block: BlockT> {
	pool: Arc<dyn OffchainSubmitTransaction<Block>>,
}

impl<Block: BlockT> OffchainTransactionPoolFactory<Block> {
	/// Creates a new instance using the given `tx_pool`.
	pub fn new<T: LocalTransactionPool<Block = Block> + 'static>(tx_pool: T) -> Self {
		Self { pool: Arc::new(tx_pool) as Arc<_> }
	}

	/// Returns an instance of [`TransactionPoolExt`] bound to the given `block_hash`.
	///
	/// Transactions that are being submitted by this instance will be submitted with `block_hash`
	/// as context for validation.
	pub fn offchain_transaction_pool(&self, block_hash: Block::Hash) -> TransactionPoolExt {
		TransactionPoolExt::new(OffchainTransactionPool { pool: self.pool.clone(), block_hash })
	}
}

/// Wraps a `pool` and `block_hash` to implement [`sp_core::offchain::TransactionPool`].
struct OffchainTransactionPool<Block: BlockT> {
	block_hash: Block::Hash,
	pool: Arc<dyn OffchainSubmitTransaction<Block>>,
}

impl<Block: BlockT> sp_core::offchain::TransactionPool for OffchainTransactionPool<Block> {
	fn submit_transaction(&mut self, extrinsic: Vec<u8>) -> Result<(), ()> {
		let extrinsic = match codec::Decode::decode(&mut &extrinsic[..]) {
			Ok(t) => t,
			Err(e) => {
				log::error!(
					target: LOG_TARGET,
					"Failed to decode extrinsic in `OffchainTransactionPool::submit_transaction`: {e:?}"
				);

				return Err(());
			},
		};

		self.pool.submit_at(self.block_hash, extrinsic)
	}
}

/// Wrapper functions to keep the API backwards compatible over the wire for the old RPC spec.
mod v1_compatible {
	use serde::{Deserialize, Deserializer, Serialize, Serializer};

	pub fn serialize<S, H>(data: &(H, usize), serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
		H: Serialize,
	{
		let (hash, _) = data;
		serde::Serialize::serialize(&hash, serializer)
	}

	pub fn deserialize<'de, D, H>(deserializer: D) -> Result<(H, usize), D::Error>
	where
		D: Deserializer<'de>,
		H: Deserialize<'de>,
	{
		let hash: H = serde::Deserialize::deserialize(deserializer)?;
		Ok((hash, 0))
	}
}

/// Transaction pool that rejects all submitted transactions.
///
/// Could be used for example in tests.
pub struct RejectAllTxPool<Block>(PhantomData<Block>);

impl<Block> Default for RejectAllTxPool<Block> {
	fn default() -> Self {
		Self(PhantomData)
	}
}

impl<Block: BlockT> LocalTransactionPool for RejectAllTxPool<Block> {
	type Block = Block;

	type Hash = Block::Hash;

	type Error = error::Error;

	fn submit_local(&self, _: Block::Hash, _: Block::Extrinsic) -> Result<Self::Hash, Self::Error> {
		Err(error::Error::ImmediatelyDropped)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn tx_status_compatibility() {
		let event: TransactionStatus<u8, u8> = TransactionStatus::InBlock((1, 2));
		let ser = serde_json::to_string(&event).unwrap();

		let exp = r#"{"inBlock":1}"#;
		assert_eq!(ser, exp);

		let event_dec: TransactionStatus<u8, u8> = serde_json::from_str(exp).unwrap();
		assert_eq!(event_dec, TransactionStatus::InBlock((1, 0)));

		let event: TransactionStatus<u8, u8> = TransactionStatus::Finalized((1, 2));
		let ser = serde_json::to_string(&event).unwrap();

		let exp = r#"{"finalized":1}"#;
		assert_eq!(ser, exp);

		let event_dec: TransactionStatus<u8, u8> = serde_json::from_str(exp).unwrap();
		assert_eq!(event_dec, TransactionStatus::Finalized((1, 0)));
	}
}
