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

//! Utility for building substrate transaction pool trait object.

use crate::{
	common::api::FullChainApi,
	fork_aware_txpool::ForkAwareTxPool as ForkAwareFullPool,
	graph::{base_pool::Transaction, ChainApi, ExtrinsicFor, ExtrinsicHash, IsValidator, Options},
	single_state_txpool::BasicPool as SingleStateFullPool,
	TransactionPoolWrapper, LOG_TARGET,
};
use prometheus_endpoint::Registry as PrometheusRegistry;
use sc_transaction_pool_api::{LocalTransactionPool, MaintainedTransactionPool};
use sp_core::traits::SpawnEssentialNamed;
use sp_runtime::traits::Block as BlockT;
use std::{marker::PhantomData, sync::Arc, time::Duration};

/// The type of transaction pool.
#[derive(Debug, Clone)]
pub enum TransactionPoolType {
	/// Single-state transaction pool
	SingleState,
	/// Fork-aware transaction pool
	ForkAware,
}

/// Transaction pool options.
#[derive(Debug, Clone)]
pub struct TransactionPoolOptions {
	txpool_type: TransactionPoolType,
	options: Options,
	/// If `true`, the pool is only maintained on best blocks (legacy behavior).
	///
	/// Only relevant for the fork-aware pool.
	best_blocks_only: bool,
}

impl Default for TransactionPoolOptions {
	fn default() -> Self {
		Self {
			txpool_type: TransactionPoolType::SingleState,
			options: Default::default(),
			best_blocks_only: false,
		}
	}
}

impl TransactionPoolOptions {
	/// Creates the options for the transaction pool using given parameters.
	pub fn new_with_params(
		pool_limit: usize,
		pool_bytes: usize,
		tx_ban_seconds: Option<u64>,
		txpool_type: TransactionPoolType,
		is_dev: bool,
		best_blocks_only: bool,
	) -> TransactionPoolOptions {
		let mut options = Options::default();

		// ready queue
		options.ready.count = pool_limit;
		options.ready.total_bytes = pool_bytes;

		// future queue
		let factor = 10;
		options.future.count = pool_limit / factor;
		options.future.total_bytes = pool_bytes / factor;

		options.ban_time = if let Some(ban_seconds) = tx_ban_seconds {
			Duration::from_secs(ban_seconds)
		} else if is_dev {
			Duration::from_secs(0)
		} else {
			Duration::from_secs(30 * 60)
		};

		TransactionPoolOptions { options, txpool_type, best_blocks_only }
	}

	/// Creates predefined options for benchmarking
	pub fn new_for_benchmarks() -> TransactionPoolOptions {
		TransactionPoolOptions {
			options: Options {
				ready: crate::graph::base_pool::Limit {
					count: 100_000,
					total_bytes: 100 * 1024 * 1024,
				},
				future: crate::graph::base_pool::Limit {
					count: 100_000,
					total_bytes: 100 * 1024 * 1024,
				},
				reject_future_transactions: false,
				ban_time: Duration::from_secs(30 * 60),
			},
			txpool_type: TransactionPoolType::SingleState,
			best_blocks_only: false,
		}
	}

	/// Returns whether the transaction pool should be notified about *every* imported block.
	pub fn use_all_block_notifications(&self) -> bool {
		matches!(self.txpool_type, TransactionPoolType::ForkAware) && !self.best_blocks_only
	}
}

/// `FullClientTransactionPool` is a trait that combines the functionality of
/// `MaintainedTransactionPool` and `LocalTransactionPool` for a given `Client` and `Block`.
///
/// This trait defines the requirements for a full client transaction pool, ensuring
/// that it can handle transactions submission and maintenance.
pub trait FullClientTransactionPool<Block, Client>:
	MaintainedTransactionPool<
		Block = Block,
		Hash = ExtrinsicHash<FullChainApi<Client, Block>>,
		InPoolTransaction = Transaction<
			ExtrinsicHash<FullChainApi<Client, Block>>,
			ExtrinsicFor<FullChainApi<Client, Block>>,
		>,
		Error = <FullChainApi<Client, Block> as ChainApi>::Error,
	> + LocalTransactionPool<
		Block = Block,
		Hash = ExtrinsicHash<FullChainApi<Client, Block>>,
		Error = <FullChainApi<Client, Block> as ChainApi>::Error,
	>
where
	Block: BlockT,
	Client: sp_api::ProvideRuntimeApi<Block>
		+ sc_client_api::BlockBackend<Block>
		+ sc_client_api::blockchain::HeaderBackend<Block>
		+ sp_runtime::traits::BlockIdTo<Block>
		+ sp_blockchain::HeaderMetadata<Block, Error = sp_blockchain::Error>
		+ 'static,
	Client::Api: sp_transaction_pool::runtime_api::TaggedTransactionQueue<Block>,
{
}

impl<Block, Client, P> FullClientTransactionPool<Block, Client> for P
where
	Block: BlockT,
	Client: sp_api::ProvideRuntimeApi<Block>
		+ sc_client_api::BlockBackend<Block>
		+ sc_client_api::blockchain::HeaderBackend<Block>
		+ sp_runtime::traits::BlockIdTo<Block>
		+ sp_blockchain::HeaderMetadata<Block, Error = sp_blockchain::Error>
		+ 'static,
	Client::Api: sp_transaction_pool::runtime_api::TaggedTransactionQueue<Block>,
	P: MaintainedTransactionPool<
			Block = Block,
			Hash = ExtrinsicHash<FullChainApi<Client, Block>>,
			InPoolTransaction = Transaction<
				ExtrinsicHash<FullChainApi<Client, Block>>,
				ExtrinsicFor<FullChainApi<Client, Block>>,
			>,
			Error = <FullChainApi<Client, Block> as ChainApi>::Error,
		> + LocalTransactionPool<
			Block = Block,
			Hash = ExtrinsicHash<FullChainApi<Client, Block>>,
			Error = <FullChainApi<Client, Block> as ChainApi>::Error,
		>,
{
}

/// The public type alias for the actual type providing the implementation of
/// `FullClientTransactionPool` with the given `Client` and `Block` types.
///
/// This handle abstracts away the specific type of the transaction pool. Should be used
/// externally to keep reference to transaction pool.
pub type TransactionPoolHandle<Block, Client> = TransactionPoolWrapper<Block, Client>;

/// Builder allowing to create specific instance of transaction pool.
pub struct Builder<'a, Block, Client> {
	options: TransactionPoolOptions,
	is_validator: IsValidator,
	prometheus: Option<&'a PrometheusRegistry>,
	client: Arc<Client>,
	spawner: Box<dyn SpawnEssentialNamed>,
	_phantom: PhantomData<(Client, Block)>,
}

impl<'a, Client, Block> Builder<'a, Block, Client>
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
	<Block as BlockT>::Hash: std::marker::Unpin,
	Client::Api: sp_transaction_pool::runtime_api::TaggedTransactionQueue<Block>,
{
	/// Creates new instance of `Builder`
	pub fn new(
		spawner: impl SpawnEssentialNamed + 'static,
		client: Arc<Client>,
		is_validator: IsValidator,
	) -> Builder<'a, Block, Client> {
		Builder {
			options: Default::default(),
			_phantom: Default::default(),
			spawner: Box::new(spawner),
			client,
			is_validator,
			prometheus: None,
		}
	}

	/// Sets the options used for creating a transaction pool instance.
	pub fn with_options(mut self, options: TransactionPoolOptions) -> Self {
		self.options = options;
		self
	}

	/// Sets the prometheus endpoint used in a transaction pool instance.
	pub fn with_prometheus(mut self, prometheus: Option<&'a PrometheusRegistry>) -> Self {
		self.prometheus = prometheus;
		self
	}

	/// Creates an instance of transaction pool.
	pub fn build(self) -> TransactionPoolHandle<Block, Client> {
		tracing::info!(
			target: LOG_TARGET,
			txpool_type = ?self.options.txpool_type,
			ready = ?self.options.options.ready,
			future = ?self.options.options.future,
			"Creating transaction pool"
		);
		TransactionPoolWrapper::<Block, Client>(match self.options.txpool_type {
			TransactionPoolType::SingleState => Box::new(SingleStateFullPool::new_full(
				self.options.options,
				self.is_validator,
				self.prometheus,
				self.spawner,
				self.client,
			)),
			TransactionPoolType::ForkAware => Box::new(ForkAwareFullPool::new_full(
				self.options.options,
				self.is_validator,
				self.prometheus,
				self.spawner,
				self.client,
			)),
		})
	}
}
