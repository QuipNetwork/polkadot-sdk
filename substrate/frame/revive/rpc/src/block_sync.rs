// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Historic block syncing logic for the Ethereum JSON-RPC server.

use crate::{
	BlockInfoProvider,
	client::{Client, ClientError, GapFillRequest, SubstrateBlockNumber},
};
use pallet_revive::evm::H256;
use tokio::sync::mpsc;

const LOG_TARGET: &str = "eth-rpc::block-sync";

/// Trait for types that can be used as keys in the `sync_state` table.
pub trait SyncStateKey: std::fmt::Display {}

/// Labels used to track sync progress in the `sync_state` table.
#[derive(Debug, Clone, Copy, derive_more::Display)]
pub enum SyncLabel {
	/// Lowest synced block. Only decreases.
	#[display(fmt = "sync-tail")]
	Tail,
	/// Highest synced block. Absent means no sync has started.
	/// During backfill: upper boundary being filled.
	/// After backfill: advanced by the finalized-block subscription.
	#[display(fmt = "sync-head")]
	Head,
}

/// Chain metadata stored in the `sync_state` table.
#[derive(Debug, Clone, Copy, derive_more::Display)]
pub enum ChainMetadata {
	/// Genesis block hash — used for chain identity verification.
	#[display(fmt = "chain-genesis")]
	Genesis,
	/// Auto-discovered first EVM block on the chain.
	#[display(fmt = "chain-first-evm-block")]
	FirstEvmBlock,
}

impl SyncStateKey for SyncLabel {}
impl SyncStateKey for ChainMetadata {}

/// Sync checkpoint persisted in the `sync_state` table to allow resuming after a restart.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct SyncCheckpoint {
	pub block_number: SubstrateBlockNumber,
	pub block_hash: Option<H256>,
}

impl SyncCheckpoint {
	/// Create a checkpoint with a known block hash.
	pub fn new(block_number: SubstrateBlockNumber, block_hash: H256) -> Self {
		Self { block_number, block_hash: Some(block_hash) }
	}

	/// Create a checkpoint with only a block number (no hash).
	pub fn from_number(block_number: SubstrateBlockNumber) -> Self {
		Self { block_number, block_hash: None }
	}
}

/// How often (in blocks) the backward sync checkpoints are persisted to the database.
const BLOCK_INTERVAL: u32 = 128;

/// Options for [`Client::sync_backward_range`].
struct BackwardSyncRange {
	from: SubstrateBlockNumber,
	to: SubstrateBlockNumber,
	/// Set `Head` label after syncing the first block.
	set_head: bool,
	/// Checkpoint `Tail` label periodically and at end.
	checkpoint_tail: bool,
	/// When true, persist the first EVM block boundary if a non-EVM block is encountered.
	persist_first_evm_block: bool,
}

impl Client {
	/// Verify that the stored genesis hash matches the connected chain.
	async fn validate_chain_identity(&self) -> Result<H256, ClientError> {
		let genesis_hash: H256 = self.api().genesis_hash();

		if let Some(checkpoint) =
			self.receipt_provider().get_sync_label(ChainMetadata::Genesis).await?
		{
			if let Some(stored) = checkpoint.block_hash {
				if stored != genesis_hash {
					return Err(ClientError::ChainMismatch);
				}
			}
		}

		Ok(genesis_hash)
	}

	/// Verify that a stored boundary block still exists on the finalized chain.
	async fn verify_boundary(&self, checkpoint: &SyncCheckpoint) -> Result<(), ClientError> {
		let num = checkpoint.block_number;
		let hash = checkpoint.block_hash;
		match (num, hash) {
			(_, None) => {
				log::error!(target: LOG_TARGET,
					"Boundary #{num}: missing stored hash");
				Err(ClientError::SyncBoundaryMismatch)
			},
			(_, Some(stored_hash)) => {
				let block = self.block_provider().block_by_number(num).await?.ok_or_else(|| {
					log::error!(target: LOG_TARGET,
						"Boundary #{num}: block not found on chain \
						 (node may have pruned it — use an archive node with --eth-pruning archive)");
					ClientError::SyncBoundaryMismatch
				})?;
				if block.block_hash() != stored_hash {
					log::error!(target: LOG_TARGET,
						"Boundary #{num}: hash mismatch — stored {stored_hash:?}, \
						 chain {:?}", block.block_hash());
					return Err(ClientError::SyncBoundaryMismatch);
				}
				Ok(())
			},
		}
	}

	/// Checkpoint the given sync label to the DB.
	async fn checkpoint_sync_label(&self, label: SyncLabel, num: SubstrateBlockNumber, hash: H256) {
		let cp = SyncCheckpoint::new(num, hash);
		let result = match label {
			SyncLabel::Head => self.receipt_provider().advance_sync_label(label, cp).await,
			SyncLabel::Tail => self.receipt_provider().recede_sync_label(label, cp).await,
		};
		if let Err(err) = result {
			log::warn!(target: LOG_TARGET, "Failed to update sync_label[{label}]: {err:?}");
		}
	}

	/// Backward sync historical blocks from the latest finalized block to the first EVM block.
	/// Resumes from the last checkpoint if a previous sync was interrupted.
	/// Fatal errors (chain/DB mismatch) are propagated; transient errors are swallowed
	/// to avoid taking down the RPC server.
	pub async fn sync_backward(&self) -> Result<(), ClientError> {
		log::info!(target: LOG_TARGET,
			"🔄 Historical block sync enabled. \
			 For a complete sync, the connected node should be an archive node.");
		match self.sync_backward_inner().await {
			Ok(()) => Ok(()),
			Err(err) if err.is_chain_validation_error() => Err(err),
			Err(err) => {
				log::error!(target: LOG_TARGET, "🗄️ Sync stopped due to {err}.");
				Ok(())
			},
		}
	}

	async fn sync_backward_inner(&self) -> Result<(), ClientError> {
		let genesis_hash = self.validate_chain_identity().await?;
		let latest_finalized_block = self.latest_finalized_block().await;
		let latest_finalized = SyncCheckpoint::new(
			latest_finalized_block.block_number(),
			latest_finalized_block.block_hash(),
		);

		// Store genesis (idempotent).
		self.receipt_provider()
			.set_sync_label(ChainMetadata::Genesis, SyncCheckpoint::new(0, genesis_hash))
			.await?;

		let (head, tail) = tokio::try_join!(
			self.receipt_provider().get_sync_label(SyncLabel::Head),
			self.receipt_provider().get_sync_label(SyncLabel::Tail),
		)?;

		match (tail, head) {
			(Some(tail), Some(head)) => {
				// Verify boundary hashes still match the finalized chain.
				tokio::try_join!(self.verify_boundary(&tail), self.verify_boundary(&head),)?;
				self.sync_backward_resume(tail, head, latest_finalized).await?;
			},
			(Some(_), None) => {
				log::warn!(target: LOG_TARGET,
					"🗄️ Tail exists without Head — possible partial corruption, \
					 starting fresh sync from #{}", latest_finalized.block_number);
				self.sync_backward_fresh(latest_finalized.block_number).await?;
			},
			_ => {
				log::info!(target: LOG_TARGET,
					"🗄️ Fresh sync: syncing backward from #{}", latest_finalized.block_number);
				self.sync_backward_fresh(latest_finalized.block_number).await?;
			},
		}

		self.mark_backfill_complete();

		log::info!(target: LOG_TARGET, "🗄️ Historic sync complete");
		Ok(())
	}

	/// Backward sync from `latest_finalized` down to the first EVM block.
	async fn sync_backward_fresh(
		&self,
		latest_finalized: SubstrateBlockNumber,
	) -> Result<(), ClientError> {
		let first_evm = self.receipt_provider().first_evm_block().unwrap_or(0);
		self.sync_backward_range(BackwardSyncRange {
			from: latest_finalized,
			to: first_evm,
			set_head: true,
			checkpoint_tail: true,
			persist_first_evm_block: true,
		})
		.await
	}

	/// Resume backward sync by filling the top gap (new blocks) and bottom gap (backfill).
	async fn sync_backward_resume(
		&self,
		tail: SyncCheckpoint,
		head: SyncCheckpoint,
		latest_finalized: SyncCheckpoint,
	) -> Result<(), ClientError> {
		log::info!(target: LOG_TARGET,
			"🗄️ Resuming sync: DB has blocks #{}..#{}, chain head is #{}",
			tail.block_number, head.block_number, latest_finalized.block_number);

		let top_gap = async {
			// Top gap: sync from latest_finalized down to head + 1.
			if head.block_number < latest_finalized.block_number {
				self.sync_backward_range(BackwardSyncRange {
					from: latest_finalized.block_number,
					to: head.block_number.saturating_add(1),
					set_head: false,
					checkpoint_tail: false,
					persist_first_evm_block: false,
				})
				.await?;

				// Mark top gap complete so a restart won't redo it.
				self.receipt_provider()
					.advance_sync_label(SyncLabel::Head, latest_finalized)
					.await?;
			}
			Ok::<_, ClientError>(())
		};

		let bottom_gap = async {
			// Bottom gap: sync from tail - 1 down to the first EVM block.
			let first_evm = self.receipt_provider().first_evm_block().unwrap_or(0);
			if tail.block_number > first_evm {
				self.sync_backward_range(BackwardSyncRange {
					from: tail.block_number.saturating_sub(1),
					to: first_evm,
					set_head: false,
					checkpoint_tail: true,
					persist_first_evm_block: true,
				})
				.await?;
			} else {
				log::debug!(target: LOG_TARGET, "🗄️ No backward gap to fill");
			}
			Ok::<_, ClientError>(())
		};

		tokio::try_join!(top_gap, bottom_gap)?;

		Ok(())
	}

	/// Backward sync from block `from` down to block `to` (inclusive).
	/// Stops early if a non-EVM block is discovered (auto-discovery of first EVM block).
	async fn sync_backward_range(
		&self,
		BackwardSyncRange {
			from,
			to,
			set_head,
			checkpoint_tail,
			persist_first_evm_block,
		}: BackwardSyncRange,
	) -> Result<(), ClientError> {
		if from < to {
			log::debug!(target: LOG_TARGET,	"⬇️ Backward sync: nothing to sync (#{from}..#{to})");
			return Ok(());
		}

		log::info!(target: LOG_TARGET, "⬇️ Backward sync: #{from} down to #{to}");

		let mut block = self
			.block_provider()
			.block_by_number(from)
			.await?
			.ok_or(ClientError::BlockNotFound)?;

		let mut blocks_synced = 0u64;
		let mut last_synced: Option<(SubstrateBlockNumber, H256)> = None;
		let at_checkpoint =
			|synced: u64| synced <= 1 || synced.is_multiple_of(u64::from(BLOCK_INTERVAL));

		let loop_result: Result<(), ClientError> = loop {
			let block_number = block.block_number();
			let block_hash = block.block_hash();

			// A block whose runtime does not expose `eth_block_hash` predates pallet-revive and
			// is treated exactly like a block without an EVM hash: it marks the end of the
			// backward sync.
			let ethereum_hash = match self.runtime_api(block_hash).await {
				Ok(runtime_api) => {
					match runtime_api.eth_block_hash(pallet_revive::evm::U256::from(block_number)) {
						Some(future) => match future.await {
							Ok(hash) => hash,
							Err(err) => {
								log::error!(target: LOG_TARGET,	"⚠️ eth_block_hash failed for #{block_number}: {err:?}, stopping");
								break Err(err);
							},
						},
						None => None,
					}
				},
				Err(err) => {
					log::error!(target: LOG_TARGET,	"⚠️ eth_block_hash failed for #{block_number}: {err:?}, stopping");
					break Err(err);
				},
			};

			match ethereum_hash {
				Some(hash) => {
					if let Err(err) =
						self.receipt_provider().insert_block_receipts_past(&block, &hash).await
					{
						log::error!(target: LOG_TARGET,
							"⚠️ Insert failed for #{block_number}: {err:?}, stopping");
						break Err(err);
					}

					last_synced = Some((block_number, block_hash));
					blocks_synced += 1;

					if blocks_synced == 1 && set_head {
						self.checkpoint_sync_label(SyncLabel::Head, block_number, block_hash).await;
					}

					if at_checkpoint(blocks_synced) {
						log::debug!(target: LOG_TARGET,
							"⬇️ Backward sync progress: #{block_number} ({blocks_synced} blocks synced)");
						if checkpoint_tail {
							self.checkpoint_sync_label(SyncLabel::Tail, block_number, block_hash)
								.await;
						}
					}
				},
				None => {
					if persist_first_evm_block {
						let first_evm_block = block_number.saturating_add(1);
						log::debug!(target: LOG_TARGET,
							"🔍 No EVM hash at #{block_number}, setting first_evm_block to #{first_evm_block}");
						if let Err(err) =
							self.receipt_provider().set_first_evm_block(first_evm_block).await
						{
							log::warn!(target: LOG_TARGET, "Failed to persist first-evm-block: {err:?}");
						}
					} else {
						log::debug!(target: LOG_TARGET,
							"🔍 No EVM hash at #{block_number}, skipping first EVM block update");
					}

					break Ok(());
				},
			}

			if block_number > to {
				let parent_hash = block.block_header().await?.parent_hash;
				match self
					.block_provider()
					.block_by_hash(&parent_hash)
					.await
					.map_err(Into::into)
					.and_then(|opt| opt.ok_or(ClientError::BlockNotFound))
				{
					Ok(b) => block = b,
					Err(err) => {
						log::error!(target: LOG_TARGET,
							"⚠️ Could not fetch parent of #{block_number}: {err:?}, stopping");
						break Err(err);
					},
				}
			} else {
				break Ok(());
			}
		};

		// Checkpoint the last synced block if it wasn't already at a checkpoint interval.
		if loop_result.is_ok() && checkpoint_tail && !at_checkpoint(blocks_synced) {
			if let Some((num, hash)) = last_synced {
				self.checkpoint_sync_label(SyncLabel::Tail, num, hash).await;
			}
		}

		log::info!(target: LOG_TARGET,
			"⬇️ Backward sync: {blocks_synced} blocks synced \
			 (requested #{from}..#{to})");

		loop_result
	}

	/// Run the background subscription gap filler, processing requests sequentially.
	pub(crate) async fn run_subscription_gap_filler(&self, mut rx: mpsc::Receiver<GapFillRequest>) {
		log::info!(target: LOG_TARGET, "🔄 Subscription gap filler started");

		while let Some(GapFillRequest { from_inclusive, to_inclusive }) = rx.recv().await {
			log::info!(target: LOG_TARGET, "🔄 Subscription gap filler: processing #{from_inclusive} down to #{to_inclusive}");
			if let Err(err) = self
				.sync_backward_range(BackwardSyncRange {
					from: from_inclusive,
					to: to_inclusive,
					set_head: false,
					checkpoint_tail: false,
					persist_first_evm_block: false,
				})
				.await
			{
				log::error!(target: LOG_TARGET, "🔄 Subscription gap fill failed for #{from_inclusive}..#{to_inclusive}: {err:?}");
			} else {
				log::info!(target: LOG_TARGET, "🔄 Subscription gap filler: done with #{from_inclusive}..#{to_inclusive}");
			}
			// Mark done unconditionally — mirrors how subscribe_new_blocks handles
			// callback errors: log and move on.
			self.subscription_gap_queue().mark_done();
		}

		log::info!(target: LOG_TARGET, "🔄 Subscription gap filler stopped");
	}
}
