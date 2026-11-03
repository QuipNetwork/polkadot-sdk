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

use crate::LOG_TARGET;
use codec::{Decode, Encode};
use cumulus_client_consensus_common::old_finalized_hash;
use cumulus_client_proof_size_recording::prepare_proof_size_recording_aux_data;
use cumulus_primitives_core::{BlockBundleInfo, CoreInfo, CumulusDigestItem, RelayBlockIdentifier};
use futures::{stream::FusedStream, StreamExt};
use sc_client_api::{
	backend::AuxStore,
	client::{AuxDataOperations, FinalityNotification, PreCommitActions},
	HeaderBackend,
};
use sc_consensus::{BlockImport, StateAction};
use sc_utils::mpsc::{tracing_unbounded, TracingUnboundedReceiver, TracingUnboundedSender};
use sp_api::{
	ApiExt, CallApiAt, CallContext, Core, ProofRecorder, ProofRecorderIgnoredNodes,
	ProvideRuntimeApi, StorageProof,
};
use sp_blockchain::{Error as ClientError, Result as ClientResult};
use sp_consensus::BlockOrigin;
use sp_runtime::traits::{Block as BlockT, HashingFor, Header as _};
use sp_trie::proof_size_extension::{ProofSizeExt, RecordingProofSizeProvider};
use std::sync::Arc;

/// The aux storage key used to store the ignored nodes for the given block hash.
fn ignored_nodes_key<H: Encode>(block_hash: H) -> Vec<u8> {
	(b"cumulus_slot_based_nodes_to_ignore", block_hash).encode()
}

/// Prepare a transaction to write the ignored nodes to the aux storage.
///
/// Returns the key-value pairs that need to be written to the aux storage.
fn prepare_ignored_nodes_transaction<Block: BlockT>(
	block_hash: Block::Hash,
	ignored_nodes: ProofRecorderIgnoredNodes<Block>,
) -> impl Iterator<Item = (Vec<u8>, Vec<u8>)> {
	let key = ignored_nodes_key(block_hash);
	let encoded_nodes = <ProofRecorderIgnoredNodes<Block> as Encode>::encode(&ignored_nodes);

	[(key, encoded_nodes)].into_iter()
}

/// Load the ignored nodes associated with a block.
fn load_ignored_nodes<Block: BlockT, B: AuxStore>(
	backend: &B,
	block_hash: Block::Hash,
) -> ClientResult<Option<ProofRecorderIgnoredNodes<Block>>> {
	match backend.get_aux(&ignored_nodes_key(block_hash))? {
		None => Ok(None),
		Some(t) => ProofRecorderIgnoredNodes::<Block>::decode(&mut &t[..])
			.map(Some)
			.map_err(|e| ClientError::Backend(format!("Failed to decode ignored nodes: {}", e))),
	}
}

/// Handle for receiving the block and the storage proof from the [`SlotBasedBlockImport`].
///
/// This handle should be passed to [`Params`](super::Params) or can also be dropped if the node is
/// not running as collator.
pub struct SlotBasedBlockImportHandle<Block> {
	receiver: TracingUnboundedReceiver<(Block, Arc<StorageProof>)>,
}

impl<Block> SlotBasedBlockImportHandle<Block> {
	/// Returns the next item.
	///
	/// The future will never return when the internal channel is closed.
	pub async fn next(&mut self) -> (Block, Arc<StorageProof>) {
		loop {
			if self.receiver.is_terminated() {
				futures::pending!()
			} else if let Some(res) = self.receiver.next().await {
				return res;
			}
		}
	}
}

/// Register the clean up method for cleaning ignored nodes from blocks on which no further blocks
/// will be imported.
fn register_ignored_nodes_cleanup<C, Block>(client: Arc<C>)
where
	C: PreCommitActions<Block> + HeaderBackend<Block> + 'static,
	Block: BlockT,
{
	let client_for_closure = client.clone();
	let on_finality = move |notification: &FinalityNotification<Block>| -> AuxDataOperations {
		let old_finalized = old_finalized_hash::<_, Block>(
			&*client_for_closure,
			&notification.tree_route,
			*notification.header.parent_hash(),
		);

		notification
			.stale_blocks
			.iter()
			// Delete the ignored nodes for all stale blocks.
			.map(|b| (ignored_nodes_key(b.hash), None))
			// We can not delete the ignored nodes for the finalized block, because blocks can still
			// be imported on top of this block. However, once multiple blocks are finalized at
			// once, blocks on the route to the finalized parent can no longer become parents
			// either.
			.chain(
				notification
					.tree_route
					.iter()
					.copied()
					.map(|hash| (ignored_nodes_key(hash), None)),
			)
			.chain(std::iter::once((ignored_nodes_key(old_finalized), None)))
			.collect()
	};

	client.register_finality_action(Box::new(on_finality));
}

/// Special block import for the slot based collator.
pub struct SlotBasedBlockImport<Block: BlockT, BI, Client> {
	inner: BI,
	client: Arc<Client>,
	sender: TracingUnboundedSender<(Block, Arc<StorageProof>)>,
}

impl<Block: BlockT, BI, Client> SlotBasedBlockImport<Block, BI, Client> {
	/// Create a new instance.
	///
	/// The returned [`SlotBasedBlockImportHandle`] needs to be passed to the
	/// [`Params`](super::Params), so that this block import instance can communicate with the
	/// collation task. If the node is not running as a collator, just dropping the handle is fine.
	pub fn new(inner: BI, client: Arc<Client>) -> (Self, SlotBasedBlockImportHandle<Block>)
	where
		Client: PreCommitActions<Block> + HeaderBackend<Block> + 'static,
	{
		let (sender, receiver) = tracing_unbounded("SlotBasedBlockImportChannel", 1000);

		register_ignored_nodes_cleanup(client.clone());

		(Self { sender, client, inner }, SlotBasedBlockImportHandle { receiver })
	}

	/// Get the [`ProofRecorderIgnoredNodes`] for `parent`.
	///
	/// If `parent` was not part of the same block bundle, the [`ProofRecorderIgnoredNodes`] are not
	/// required and `None` will be returned.
	fn get_ignored_nodes(
		&self,
		parent: Block::Hash,
		core_info: &CoreInfo,
		bundle_info: &BlockBundleInfo,
		relay_block_identifier: &RelayBlockIdentifier,
	) -> Option<ProofRecorderIgnoredNodes<Block>>
	where
		Client: AuxStore + HeaderBackend<Block> + Send + Sync,
	{
		let parent_header = self.client.header(parent).ok().flatten()?;
		let parent_core_info = CumulusDigestItem::find_core_info(parent_header.digest())?;
		let parent_bundle_info = CumulusDigestItem::find_block_bundle_info(parent_header.digest())?;
		let parent_relay_block_identifier =
			CumulusDigestItem::find_relay_block_identifier(parent_header.digest())?;

		if parent_relay_block_identifier != *relay_block_identifier {
			tracing::trace!(target: LOG_TARGET, ?parent_relay_block_identifier, ?relay_block_identifier, "Relay block identifier doesn't match");
			return None;
		}

		if parent_core_info != *core_info {
			tracing::trace!(target: LOG_TARGET, ?parent_core_info, ?core_info, "Core info doesn't match");
			return None;
		}

		if parent_bundle_info.index.saturating_add(1) != bundle_info.index {
			tracing::trace!(target: LOG_TARGET, ?parent_bundle_info, ?bundle_info, "Block is not a child, based on the index");
			return None;
		}

		match load_ignored_nodes::<Block, _>(&*self.client, parent) {
			Ok(nodes) => nodes,
			Err(error) => {
				tracing::trace!(target: LOG_TARGET, ?parent, ?error, "Failed to load `IgnoredNodes` from aux store");
				None
			},
		}
	}

	/// Execute the given block and collect the storage proof.
	///
	/// We need to execute the block on this level here, because we are collecting the storage
	/// proofs and combining them for blocks on the same core. So, blocks on the same core do not
	/// need to include the same trie nodes multiple times and thus, not wasting storage proof size.
	///
	/// The proof must be recorded in exactly the same manner as during block building, because the
	/// proof size is tracked via `ProofSizeExt` and affects runtime state. Without identical proof
	/// recording, the computed state root would differ and block import would fail.
	async fn execute_block_and_collect_storage_proof(
		&self,
		params: &mut sc_consensus::BlockImportParams<Block>,
	) -> Result<Option<(Block, Arc<StorageProof>)>, sp_consensus::Error>
	where
		Client: ProvideRuntimeApi<Block>
			+ CallApiAt<Block>
			+ AuxStore
			+ HeaderBackend<Block>
			+ Send
			+ Sync,
		Client::StateBackend: Send,
		Client::Api: Core<Block>,
	{
		let core_info = CumulusDigestItem::find_core_info(params.header.digest());
		let bundle_info = CumulusDigestItem::find_block_bundle_info(params.header.digest());
		let relay_block_identifier =
			CumulusDigestItem::find_relay_block_identifier(params.header.digest());

		let (Some(core_info), Some(bundle_info), Some(relay_block_identifier)) =
			(core_info, bundle_info, relay_block_identifier)
		else {
			tracing::debug!(
				target: LOG_TARGET,
				number = ?params.header.number(),
				"no bundle digests, skipping execute_block_and_collect_storage_proof",
			);
			return Ok(None);
		};

		let parent_hash = *params.header.parent_hash();

		let mut nodes_to_ignore = self
			.get_ignored_nodes(parent_hash, &core_info, &bundle_info, &relay_block_identifier)
			.unwrap_or_default();

		let recorder = ProofRecorder::<Block>::with_ignored_nodes(nodes_to_ignore.clone());
		let proof_size_recorder = RecordingProofSizeProvider::new(recorder.clone());

		let mut runtime_api = self.client.runtime_api();

		// `record_proof_with_recorder` captures trie accesses, while `ProofSizeExt` replays the
		// proof-size estimations in the same order they were observed during block building.
		runtime_api.set_call_context(CallContext::Onchain { import: true });
		runtime_api.record_proof_with_recorder(recorder.clone());
		runtime_api.register_extension(ProofSizeExt::new(proof_size_recorder.clone()));

		// Only blocks imported at the tip (built by other collators) are forwarded to the
		// resubmission backfill task. Sync/gap/warp imports are historical and never part of our
		// unincluded segment, so recording resubmission data for them would be wasted work (and
		// would buffer their potentially large storage proofs).
		let collect_for_resubmission = matches!(
			params.origin,
			BlockOrigin::NetworkBroadcast | BlockOrigin::ConsensusBroadcast
		);

		let body = params.body.clone().unwrap_or_default();
		let resubmission_body = collect_for_resubmission.then(|| body.clone());
		let block = Block::new(params.header.clone(), body);

		tracing::debug!(
			target: LOG_TARGET,
			?parent_hash,
			number = ?params.header.number(),
			?core_info,
			?bundle_info,
			"execute_block_and_collect_storage_proof: calling runtime_api.execute_block",
		);

		runtime_api
			.execute_block(parent_hash, block.into())
			.map_err(|e| Box::new(e) as Box<_>)?;

		let storage_proof =
			runtime_api.extract_proof().expect("Proof recording was enabled above; qed");

		let state = self.client.state_at(parent_hash).map_err(|e| Box::new(e) as Box<_>)?;
		let gen_storage_changes = runtime_api
			.into_storage_changes(&state, parent_hash)
			.map_err(sp_consensus::Error::ChainLookup)?;

		if params.header.state_root() != &gen_storage_changes.transaction_storage_root {
			return Err(sp_consensus::Error::Other(Box::new(
				sp_blockchain::Error::InvalidStateRoot,
			)));
		}

		// Extend the ignored nodes with the nodes from the storage proof and the generated
		// storage changes. This ensures that subsequent blocks in the same bundle don't
		// redundantly include the same trie nodes in their proof.
		nodes_to_ignore.extend(ProofRecorderIgnoredNodes::<Block>::from_storage_proof::<
			HashingFor<Block>,
		>(&storage_proof));
		nodes_to_ignore.extend(ProofRecorderIgnoredNodes::<Block>::from_memory_db(
			gen_storage_changes.transaction.clone(),
		));

		let block_hash = params.post_hash();
		prepare_ignored_nodes_transaction::<Block>(block_hash, nodes_to_ignore).for_each(
			|(k, v)| {
				params.auxiliary.push((k, Some(v)));
			},
		);

		// Extract and store proof size recordings
		let recorded_sizes = proof_size_recorder
			.recorded_estimations()
			.into_iter()
			.map(|size| size as u32)
			.collect::<Vec<u32>>();

		if !recorded_sizes.is_empty() {
			prepare_proof_size_recording_aux_data(block_hash, recorded_sizes).for_each(|(k, v)| {
				params.auxiliary.push((k, Some(v)));
			});
		}

		params.state_action =
			StateAction::ApplyChanges(sc_consensus::StorageChanges::Changes(gen_storage_changes));

		let Some(resubmission_body) = resubmission_body else { return Ok(None) };

		let block = Block::new(params.post_header(), resubmission_body);
		Ok(Some((block, Arc::new(storage_proof))))
	}
}

impl<Block: BlockT, BI: Clone, Client> Clone for SlotBasedBlockImport<Block, BI, Client> {
	fn clone(&self) -> Self {
		Self { inner: self.inner.clone(), client: self.client.clone(), sender: self.sender.clone() }
	}
}

#[async_trait::async_trait]
impl<Block, BI, Client> BlockImport<Block> for SlotBasedBlockImport<Block, BI, Client>
where
	Block: BlockT,
	BI: BlockImport<Block> + Send + Sync,
	BI::Error: Into<sp_consensus::Error>,
	Client:
		ProvideRuntimeApi<Block> + CallApiAt<Block> + AuxStore + HeaderBackend<Block> + Send + Sync,
	Client::StateBackend: Send,
	Client::Api: Core<Block>,
{
	type Error = sp_consensus::Error;

	async fn check_block(
		&self,
		block: sc_consensus::BlockCheckParams<Block>,
	) -> Result<sc_consensus::ImportResult, Self::Error> {
		self.inner.check_block(block).await.map_err(Into::into)
	}

	async fn import_block(
		&self,
		mut params: sc_consensus::BlockImportParams<Block>,
	) -> Result<sc_consensus::ImportResult, Self::Error> {
		let origin = params.origin;

		let imported = if !(origin == BlockOrigin::Own ||
			params.with_state() ||
			params.state_action.skip_execution_checks())
		{
			self.execute_block_and_collect_storage_proof(&mut params).await?
		} else {
			None
		};

		let result = self.inner.import_block(params).await.map_err(Into::into)?;

		if let Some((block, proof)) = imported {
			let _ = self.sender.unbounded_send((block, proof));
		}

		Ok(result)
	}
}
