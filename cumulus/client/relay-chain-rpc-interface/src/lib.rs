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

use async_trait::async_trait;
use core::time::Duration;
use cumulus_primitives_core::{
	relay_chain::{
		CandidateEvent, CommittedCandidateReceiptV2 as CommittedCandidateReceipt,
		Hash as RelayHash, Header as RelayHeader, InboundHrmpMessage, OccupiedCoreAssumption,
		SessionIndex, ValidationCodeHash, ValidatorId,
	},
	InboundDownwardMessage, ParaId, PersistedValidationData,
};
use cumulus_relay_chain_interface::{
	BlockNumber, ChildInfo, CoreIndex, CoreState, PHeader, RelayChainError, RelayChainInterface,
	RelayChainResult,
};
use futures::{FutureExt, Stream, StreamExt};
use polkadot_overseer::Handle;

use sc_client_api::StorageProof;
use sp_state_machine::StorageValue;
use sp_storage::StorageKey;
use sp_version::RuntimeVersion;
use std::{collections::btree_map::BTreeMap, pin::Pin};

use cumulus_primitives_core::relay_chain::{vstaging::RelayParentInfo, BlockId, NodeFeatures};
pub use url::Url;

mod metrics;
mod reconnecting_ws_client;
mod rpc_client;

pub use rpc_client::{create_client_and_start_worker, RelayChainRpcClient};

const TIMEOUT_IN_SECONDS: u64 = 6;

/// RelayChainRpcInterface is used to interact with a full node that is running locally
/// in the same process.
#[derive(Clone)]
pub struct RelayChainRpcInterface {
	rpc_client: RelayChainRpcClient,
	overseer_handle: Handle,
}

impl RelayChainRpcInterface {
	pub fn new(rpc_client: RelayChainRpcClient, overseer_handle: Handle) -> Self {
		Self { rpc_client, overseer_handle }
	}
}

#[async_trait]
impl RelayChainInterface for RelayChainRpcInterface {
	async fn retrieve_dmq_contents(
		&self,
		para_id: ParaId,
		relay_parent: RelayHash,
	) -> RelayChainResult<Vec<InboundDownwardMessage>> {
		self.rpc_client.parachain_host_dmq_contents(para_id, relay_parent).await
	}

	async fn retrieve_all_inbound_hrmp_channel_contents(
		&self,
		para_id: ParaId,
		relay_parent: RelayHash,
	) -> RelayChainResult<BTreeMap<ParaId, Vec<InboundHrmpMessage>>> {
		self.rpc_client
			.parachain_host_inbound_hrmp_channels_contents(para_id, relay_parent)
			.await
	}

	async fn header(&self, block_id: BlockId) -> RelayChainResult<Option<PHeader>> {
		let hash = match block_id {
			BlockId::Hash(hash) => hash,
			BlockId::Number(num) => {
				if let Some(hash) = self.rpc_client.chain_get_block_hash(Some(num)).await? {
					hash
				} else {
					return Ok(None);
				}
			},
		};
		let header = self.rpc_client.chain_get_header(Some(hash)).await?;

		Ok(header)
	}

	async fn persisted_validation_data(
		&self,
		hash: RelayHash,
		para_id: ParaId,
		occupied_core_assumption: OccupiedCoreAssumption,
	) -> RelayChainResult<Option<PersistedValidationData>> {
		self.rpc_client
			.parachain_host_persisted_validation_data(hash, para_id, occupied_core_assumption)
			.await
	}

	async fn validation_code_hash(
		&self,
		hash: RelayHash,
		para_id: ParaId,
		occupied_core_assumption: OccupiedCoreAssumption,
	) -> RelayChainResult<Option<ValidationCodeHash>> {
		self.rpc_client
			.validation_code_hash(hash, para_id, occupied_core_assumption)
			.await
	}

	async fn candidate_pending_availability(
		&self,
		hash: RelayHash,
		para_id: ParaId,
	) -> RelayChainResult<Option<CommittedCandidateReceipt>> {
		self.rpc_client
			.parachain_host_candidate_pending_availability(hash, para_id)
			.await
	}

	async fn session_index_for_child(&self, hash: RelayHash) -> RelayChainResult<SessionIndex> {
		self.rpc_client.parachain_host_session_index_for_child(hash).await
	}

	async fn validators(&self, block_id: RelayHash) -> RelayChainResult<Vec<ValidatorId>> {
		self.rpc_client.parachain_host_validators(block_id).await
	}

	async fn import_notification_stream(
		&self,
	) -> RelayChainResult<Pin<Box<dyn Stream<Item = RelayHeader> + Send>>> {
		let imported_headers_stream = self.rpc_client.get_imported_heads_stream()?;

		Ok(imported_headers_stream.boxed())
	}

	async fn finality_notification_stream(
		&self,
	) -> RelayChainResult<Pin<Box<dyn Stream<Item = RelayHeader> + Send>>> {
		let imported_headers_stream = self.rpc_client.get_finalized_heads_stream()?;

		Ok(imported_headers_stream.boxed())
	}

	async fn best_block_hash(&self) -> RelayChainResult<RelayHash> {
		self.rpc_client.chain_get_head(None).await
	}

	async fn finalized_block_hash(&self) -> RelayChainResult<RelayHash> {
		self.rpc_client.chain_get_finalized_head().await
	}

	async fn call_runtime_api(
		&self,
		method_name: &'static str,
		hash: RelayHash,
		payload: &[u8],
	) -> RelayChainResult<Vec<u8>> {
		self.rpc_client
			.call_remote_runtime_function_encoded(method_name, hash, payload)
			.await
			.map(|bytes| bytes.to_vec())
	}

	async fn is_major_syncing(&self) -> RelayChainResult<bool> {
		self.rpc_client.system_health().await.map(|h| h.is_syncing)
	}

	fn overseer_handle(&self) -> RelayChainResult<Handle> {
		Ok(self.overseer_handle.clone())
	}

	async fn get_storage_by_key(
		&self,
		relay_parent: RelayHash,
		key: &[u8],
	) -> RelayChainResult<Option<StorageValue>> {
		let storage_key = StorageKey(key.to_vec());
		self.rpc_client
			.state_get_storage(storage_key, Some(relay_parent))
			.await
			.map(|storage_data| storage_data.map(|sv| sv.0))
	}

	async fn prove_read(
		&self,
		relay_parent: RelayHash,
		relevant_keys: &Vec<Vec<u8>>,
	) -> RelayChainResult<StorageProof> {
		let cloned = relevant_keys.clone();
		let storage_keys: Vec<StorageKey> = cloned.into_iter().map(StorageKey).collect();

		self.rpc_client
			.state_get_read_proof(storage_keys, Some(relay_parent))
			.await
			.map(|read_proof| {
				StorageProof::new(read_proof.proof.into_iter().map(|bytes| bytes.to_vec()))
			})
	}

	async fn prove_child_read(
		&self,
		relay_parent: RelayHash,
		child_info: &ChildInfo,
		child_keys: &[Vec<u8>],
	) -> RelayChainResult<StorageProof> {
		let child_storage_key = child_info.prefixed_storage_key();
		let storage_keys: Vec<StorageKey> =
			child_keys.iter().map(|key| StorageKey(key.clone())).collect();

		self.rpc_client
			.state_get_child_read_proof(child_storage_key, storage_keys, Some(relay_parent))
			.await
			.map(|read_proof| {
				StorageProof::new(read_proof.proof.into_iter().map(|bytes| bytes.to_vec()))
			})
	}

	/// Wait for a given relay chain block
	///
	/// The hash of the block to wait for is passed. We wait for the block to arrive or return after
	/// a timeout.
	///
	/// Implementation:
	/// 1. Register a listener to all new blocks.
	/// 2. Check if the block is already in chain. If yes, succeed early.
	/// 3. Wait for the block to be imported via subscription.
	/// 4. If timeout is reached, we return an error.
	async fn wait_for_block(&self, wait_for_hash: RelayHash) -> RelayChainResult<()> {
		let mut head_stream = self.rpc_client.get_imported_heads_stream()?;

		if self.rpc_client.chain_get_header(Some(wait_for_hash)).await?.is_some() {
			return Ok(());
		}

		let mut timeout = futures_timer::Delay::new(Duration::from_secs(TIMEOUT_IN_SECONDS)).fuse();

		loop {
			futures::select! {
				_ = timeout => return Err(RelayChainError::WaitTimeout(wait_for_hash)),
				evt = head_stream.next().fuse() => match evt {
					Some(evt) if evt.hash() == wait_for_hash => return Ok(()),
					// Not the event we waited on.
					Some(_) => continue,
					None => return Err(RelayChainError::ImportListenerClosed(wait_for_hash)),
				}
			}
		}
	}

	async fn new_best_notification_stream(
		&self,
	) -> RelayChainResult<Pin<Box<dyn Stream<Item = RelayHeader> + Send>>> {
		let imported_headers_stream = self.rpc_client.get_best_heads_stream()?;
		Ok(imported_headers_stream.boxed())
	}

	async fn candidates_pending_availability(
		&self,
		hash: RelayHash,
		para_id: ParaId,
	) -> RelayChainResult<Vec<CommittedCandidateReceipt>> {
		self.rpc_client
			.parachain_host_candidates_pending_availability(hash, para_id)
			.await
	}

	async fn version(&self, relay_parent: RelayHash) -> RelayChainResult<RuntimeVersion> {
		self.rpc_client.runtime_version(relay_parent).await
	}

	async fn availability_cores(
		&self,
		relay_parent: RelayHash,
	) -> RelayChainResult<Vec<CoreState<RelayHash, BlockNumber>>> {
		self.rpc_client.parachain_host_availability_cores(relay_parent).await
	}

	async fn claim_queue(
		&self,
		relay_parent: RelayHash,
	) -> RelayChainResult<BTreeMap<CoreIndex, std::collections::VecDeque<ParaId>>> {
		self.rpc_client.parachain_host_claim_queue(relay_parent).await
	}

	async fn scheduling_lookahead(&self, relay_parent: RelayHash) -> RelayChainResult<u32> {
		self.rpc_client.parachain_host_scheduling_lookahead(relay_parent).await
	}

	async fn candidate_events(
		&self,
		relay_parent: RelayHash,
	) -> RelayChainResult<Vec<CandidateEvent>> {
		self.rpc_client.parachain_host_candidate_events(relay_parent).await
	}

	async fn max_relay_parent_session_age(&self, at: RelayHash) -> RelayChainResult<u32> {
		self.rpc_client.parachain_host_max_relay_parent_session_age(at).await
	}

	async fn node_features(&self, at: RelayHash) -> RelayChainResult<NodeFeatures> {
		self.rpc_client.parachain_host_node_features(at).await
	}

	async fn ancestor_relay_parent_info(
		&self,
		at: RelayHash,
		session_index: SessionIndex,
		relay_parent: RelayHash,
	) -> RelayChainResult<Option<RelayParentInfo<RelayHash, BlockNumber>>> {
		self.rpc_client
			.parachain_host_ancestor_relay_parent_info(at, session_index, relay_parent)
			.await
	}
}
