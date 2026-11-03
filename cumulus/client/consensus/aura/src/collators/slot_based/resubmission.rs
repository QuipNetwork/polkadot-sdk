// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus. If not, see <https://www.gnu.org/licenses/>.

//! Resolving relay-chain data needed to record resubmission entries.
//!
//! [`resolve_session`] is shared by the build path (block_builder_task) and the resubmission
//! backfill task. [`run_resubmission_backfill`] is the task that records resubmission entries for
//! blocks imported at the tip (built by other collators), doing the relay-chain queries off the
//! block-import path.

use super::SlotBasedBlockImportHandle;
use cumulus_client_resubmission_store::{
	prepare_resubmission_aux_data, prune_finalized_entries, prune_missed_finalized_entries,
};
use cumulus_primitives_core::{
	relay_chain::{
		BlockId, BlockNumber as RelayBlockNumber, Hash as RelayHash, Header as RelayHeader,
		SessionIndex,
	},
	CumulusDigestItem, RelayBlockIdentifier,
};
use cumulus_relay_chain_interface::{RelayChainError, RelayChainInterface};
use futures::{FutureExt, StreamExt};
use sc_client_api::{backend::AuxStore, BlockchainEvents};
use sp_api::StorageProof;
use sp_blockchain::HeaderBackend;
use sp_runtime::traits::{Block as BlockT, Header as HeaderT};
use std::sync::Arc;

const LOG_TARGET: &str = "aura::resubmission";

#[derive(Debug, thiserror::Error)]
pub(crate) enum ResubmissionError {
	#[error("relay parent header unavailable for {0:?}")]
	HeaderUnavailable(RelayBlockIdentifier),

	#[error("relay storage root mismatch at number {block_number}")]
	StorageRootMismatch { block_number: RelayBlockNumber },

	#[error("failed to fetch relay parent header: {0}")]
	RelayParentHeader(RelayChainError),

	#[error("failed to fetch relay-parent session: {0}")]
	Session(RelayChainError),
}

/// Fetch the session index for the given relay parent.
pub(crate) async fn resolve_session<R: RelayChainInterface + ?Sized>(
	relay_client: &R,
	relay_parent: RelayHash,
) -> Result<SessionIndex, ResubmissionError> {
	relay_client
		.session_index_for_child(relay_parent)
		.await
		.map_err(ResubmissionError::Session)
}

/// Resolve the relay-parent header from a [`RelayBlockIdentifier`] found in a parablock's digest.
///
/// Production slot-based blocks carry the relay parent as [`RelayBlockIdentifier::ByStorageRoot`]
/// (via the relay-parent-storage-root digest), so we look the relay block up by number and confirm
/// its state root matches — guarding against resolving the wrong fork. [`RelayBlockIdentifier::
/// ByHash`] is resolved directly.
async fn resolve_relay_parent<R: RelayChainInterface + ?Sized>(
	relay_client: &R,
	identifier: &RelayBlockIdentifier,
) -> Result<RelayHeader, ResubmissionError> {
	match identifier {
		RelayBlockIdentifier::ByHash(relay_parent) => relay_client
			.header(BlockId::Hash(*relay_parent))
			.await
			.map_err(ResubmissionError::RelayParentHeader)?
			.ok_or_else(|| ResubmissionError::HeaderUnavailable(identifier.clone())),
		RelayBlockIdentifier::ByStorageRoot { storage_root, block_number } => {
			let header = relay_client
				.header(BlockId::Number(*block_number))
				.await
				.map_err(ResubmissionError::RelayParentHeader)?
				.ok_or_else(|| ResubmissionError::HeaderUnavailable(identifier.clone()))?;

			// The canonical relay block at this number must match the storage root recorded in the
			// digest, otherwise we resolved a different fork.
			if header.state_root != *storage_root {
				return Err(ResubmissionError::StorageRootMismatch { block_number: *block_number });
			}

			Ok(header)
		},
	}
}

/// Maintain the resubmission store for blocks imported at the tip.
///
/// Receives imported `(block, storage proof)` pairs from the [`SlotBasedBlockImportHandle`] and
/// records a resubmission entry for each via [`backfill_resubmission_entry`], and prunes entries as
/// blocks are finalized.
pub(crate) async fn run_resubmission_backfill<Block, RClient, Client>(
	mut block_import_handle: SlotBasedBlockImportHandle<Block>,
	relay_client: RClient,
	para_client: Arc<Client>,
) where
	Block: BlockT,
	RClient: RelayChainInterface,
	Client: AuxStore + HeaderBackend<Block> + BlockchainEvents<Block>,
{
	// Reclaim entries for blocks that were finalized without their prune being observed (e.g. while
	// the node was down). The notification stream below only covers finalizations from now on.
	if let Err(err) = prune_missed_finalized_entries::<Block, _>(&*para_client) {
		tracing::warn!(
			target: LOG_TARGET,
			?err,
			"Failed to prune missed finalized resubmission entries at startup.",
		);
	}

	let mut finality_notifications = para_client.finality_notification_stream();

	loop {
		let import_fut = block_import_handle.next().fuse();
		let notification_fut = finality_notifications.next().fuse();
		futures::pin_mut!(import_fut, notification_fut);

		futures::select! {
			maybe_notification = notification_fut => {
				let Some(notification) = maybe_notification else {
					// The finality stream ended; nothing left to prune.
					break;
				};

				if let Err(err) = prune_finalized_entries(&*para_client, &notification) {
					tracing::warn!(
						target: LOG_TARGET,
						?err,
						"Failed to prune finalized resubmission entries.",
					);
				}
			},
			(block, proof) = import_fut => {
				backfill_resubmission_entry(
					&relay_client,
					&*para_client,
					block.header(),
					proof,
				)
				.await;
			},
		}
	}
}

/// Resolve the relay-chain data an entry needs and write it to the aux store.
///
/// Finalized blocks are skipped
async fn backfill_resubmission_entry<Block, R, Client>(
	relay_client: &R,
	para_client: &Client,
	header: &Block::Header,
	proof: Arc<StorageProof>,
) where
	Block: BlockT,
	R: RelayChainInterface + ?Sized,
	Client: AuxStore + HeaderBackend<Block>,
{
	let block_hash = header.hash();
	let number = *header.number();

	if number <= para_client.info().finalized_number {
		return;
	}

	let Some(relay_block_identifier) =
		CumulusDigestItem::find_relay_block_identifier(header.digest())
	else {
		tracing::trace!(target: LOG_TARGET, ?block_hash, "Imported block has no relay block identifier; skipping.");
		return;
	};

	let relay_parent_header =
		match resolve_relay_parent(relay_client, &relay_block_identifier).await {
			Ok(header) => header,
			Err(err) => {
				tracing::debug!(
					target: LOG_TARGET,
					?block_hash,
					?err,
					"Could not resolve relay parent; skipping resubmission entry.",
				);
				return;
			},
		};
	let relay_parent = relay_parent_header.hash();

	let relay_parent_session = match resolve_session(relay_client, relay_parent).await {
		Ok(session) => session,
		Err(err) => {
			tracing::debug!(
				target: LOG_TARGET,
				?block_hash,
				?err,
				"Could not resolve relay-parent session; skipping resubmission entry.",
			);
			return;
		},
	};
	if number <= para_client.info().finalized_number {
		return;
	}

	let pairs: Vec<_> = prepare_resubmission_aux_data::<Block>(
		block_hash,
		proof,
		relay_parent_header,
		relay_parent_session,
	)
	.collect();
	let refs: Vec<_> = pairs.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect();

	match para_client.insert_aux(&refs, &[]) {
		Ok(()) => tracing::trace!(
			target: LOG_TARGET,
			?block_hash,
			"Stored resubmission entry for imported block.",
		),
		Err(err) => tracing::warn!(
			target: LOG_TARGET,
			?block_hash,
			?err,
			"Failed to store resubmission entry for imported block.",
		),
	}
}
