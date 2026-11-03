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

use clap::{Args, ValueEnum};
use sc_transaction_pool::TransactionPoolOptions;

/// Type of transaction pool to be used
#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum TransactionPoolType {
	/// Uses a legacy, single-state transaction pool.
	SingleState,
	/// Uses a fork-aware transaction pool.
	ForkAware,
}

impl Into<sc_transaction_pool::TransactionPoolType> for TransactionPoolType {
	fn into(self) -> sc_transaction_pool::TransactionPoolType {
		match self {
			TransactionPoolType::SingleState => {
				sc_transaction_pool::TransactionPoolType::SingleState
			},
			TransactionPoolType::ForkAware => sc_transaction_pool::TransactionPoolType::ForkAware,
		}
	}
}

/// Parameters used to create the pool configuration.
#[derive(Debug, Clone, Args)]
pub struct TransactionPoolParams {
	/// Maximum number of transactions in the transaction pool.
	#[arg(long, value_name = "COUNT", default_value_t = 8192)]
	pub pool_limit: usize,

	/// Maximum number of kilobytes of all transactions stored in the pool.
	#[arg(long, value_name = "COUNT", default_value_t = 20480)]
	pub pool_kbytes: usize,

	/// How long a transaction is banned for.
	///
	/// If it is considered invalid. Defaults to 1800s.
	#[arg(long, value_name = "SECONDS")]
	pub tx_ban_seconds: Option<u64>,

	/// The type of transaction pool to be instantiated.
	#[arg(long, value_enum, default_value_t = TransactionPoolType::ForkAware)]
	pub pool_type: TransactionPoolType,

	/// Maintain the transaction pool only on best blocks, ignoring non-best forks.
	///
	/// Escape hatch restoring the legacy best-blocks-only behavior (the pool is maintained on
	/// every imported block by default). Expected to degrade performance under forking; only use
	/// it as a fallback if you know what you are doing. No effect on the single-state pool.
	#[arg(long, default_value_t = false)]
	pub pool_best_blocks_only: bool,
}

impl TransactionPoolParams {
	/// Fill the given `PoolConfiguration` by looking at the cli parameters.
	pub fn transaction_pool(&self, is_dev: bool) -> TransactionPoolOptions {
		TransactionPoolOptions::new_with_params(
			self.pool_limit,
			self.pool_kbytes * 1024,
			self.tx_ban_seconds,
			self.pool_type.into(),
			is_dev,
			self.pool_best_blocks_only,
		)
	}
}
