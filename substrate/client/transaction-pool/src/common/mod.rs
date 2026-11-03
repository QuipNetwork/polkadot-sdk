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

//! Common components re-used across different txpool implementations.

pub(crate) mod api;
pub(crate) mod enactment_state;
pub(crate) mod error;
pub(crate) mod metrics;
pub(crate) mod sliding_stat;
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod tracing_log_xt;

use futures::StreamExt;
use std::sync::Arc;

/// Stat sliding window, in seconds for per-transaction activities.
pub(crate) const STAT_SLIDING_WINDOW: u64 = 3;

/// Inform the transaction pool about imported and finalized blocks.
///
/// If `all_block_notifications` is `true`, the pool is informed about every imported block (all
/// forks); otherwise it is only informed about blocks imported as the new best.
pub async fn notification_future<Client, Pool, Block>(
	client: Arc<Client>,
	txpool: Arc<Pool>,
	all_block_notifications: bool,
) where
	Block: sp_runtime::traits::Block,
	Client: sc_client_api::BlockchainEvents<Block>,
	Pool: sc_transaction_pool_api::MaintainedTransactionPool<Block = Block>,
{
	let import_stream = client
		.import_notification_stream()
		.filter_map(move |n| {
			futures::future::ready((all_block_notifications || n.is_new_best).then(|| n.into()))
		})
		.fuse();
	let finality_stream = client.finality_notification_stream().map(Into::into).fuse();

	futures::stream::select(import_stream, finality_stream)
		.for_each(|evt| txpool.maintain(evt))
		.await
}
