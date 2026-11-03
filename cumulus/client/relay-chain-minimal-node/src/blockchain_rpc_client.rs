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

use std::{
	collections::{BTreeMap, VecDeque},
	pin::Pin,
};

use cumulus_primitives_core::{InboundDownwardMessage, ParaId, PersistedValidationData};
use cumulus_relay_chain_interface::{RelayChainError, RelayChainResult};
use cumulus_relay_chain_rpc_interface::RelayChainRpcClient;
use futures::{Stream, StreamExt};
use polkadot_core_primitives::{Block, BlockNumber, Hash, Header};
use polkadot_overseer::{ChainApiBackend, RuntimeApiSubsystemClient};
use polkadot_primitives::{
	async_backing::{AsyncBackingParams, BackingState, Constraints},
	slashing, ApprovalVotingParams, CoreIndex, NodeFeatures,
};
use sc_authority_discovery::{AuthorityDiscovery, Error as AuthorityDiscoveryError};
use sc_client_api::AuxStore;
use sp_api::{ApiError, RuntimeApiInfo};
use sp_blockchain::Info;
use sp_runtime::traits::{Block as BlockT, Header as HeaderT, NumberFor};

#[derive(Clone)]
pub struct BlockChainRpcClient {
	rpc_client: RelayChainRpcClient,
}

impl BlockChainRpcClient {
	pub fn new(rpc_client: RelayChainRpcClient) -> Self {
		Self { rpc_client }
	}

	pub async fn chain_get_header(
		&self,
		hash: Option<Hash>,
	) -> Result<Option<Header>, RelayChainError> {
		self.rpc_client.chain_get_header(hash).await
	}

	pub async fn block_get_hash(
		&self,
		number: Option<BlockNumber>,
	) -> Result<Option<Hash>, RelayChainError> {
		self.rpc_client.chain_get_block_hash(number).await
	}
}

#[async_trait::async_trait]
impl ChainApiBackend for BlockChainRpcClient {
	async fn header(
		&self,
		hash: <Block as BlockT>::Hash,
	) -> sp_blockchain::Result<Option<<Block as BlockT>::Header>> {
		Ok(self.rpc_client.chain_get_header(Some(hash)).await?)
	}

	async fn info(&self) -> sp_blockchain::Result<Info<Block>> {
		let (best_header_opt, genesis_hash, finalized_head) = futures::try_join!(
			self.rpc_client.chain_get_header(None),
			self.rpc_client.chain_get_head(Some(0)),
			self.rpc_client.chain_get_finalized_head()
		)?;
		let best_header = best_header_opt.ok_or_else(|| {
			RelayChainError::GenericError(
				"Unable to retrieve best header from relay chain.".to_string(),
			)
		})?;

		let finalized_header =
			self.rpc_client.chain_get_header(Some(finalized_head)).await?.ok_or_else(|| {
				RelayChainError::GenericError(
					"Unable to retrieve finalized header from relay chain.".to_string(),
				)
			})?;
		Ok(Info {
			best_hash: best_header.hash(),
			best_number: best_header.number,
			genesis_hash,
			finalized_hash: finalized_head,
			finalized_number: finalized_header.number,
			finalized_state: Some((finalized_header.hash(), finalized_header.number)),
			number_leaves: 1,
			block_gap: None,
		})
	}

	async fn number(
		&self,
		hash: <Block as BlockT>::Hash,
	) -> sp_blockchain::Result<Option<<<Block as BlockT>::Header as HeaderT>::Number>> {
		Ok(self
			.rpc_client
			.chain_get_header(Some(hash))
			.await?
			.map(|maybe_header| maybe_header.number))
	}

	async fn hash(
		&self,
		number: NumberFor<Block>,
	) -> sp_blockchain::Result<Option<<Block as BlockT>::Hash>> {
		Ok(self.rpc_client.chain_get_block_hash(number.into()).await?)
	}
}

#[async_trait::async_trait]
impl RuntimeApiSubsystemClient for BlockChainRpcClient {
	async fn validators(
		&self,
		at: Hash,
	) -> Result<Vec<polkadot_primitives::ValidatorId>, sp_api::ApiError> {
		Ok(self.rpc_client.parachain_host_validators(at).await?)
	}

	async fn validator_groups(
		&self,
		at: Hash,
	) -> Result<
		(
			Vec<Vec<polkadot_primitives::ValidatorIndex>>,
			polkadot_primitives::GroupRotationInfo<BlockNumber>,
		),
		sp_api::ApiError,
	> {
		Ok(self.rpc_client.parachain_host_validator_groups(at).await?)
	}

	async fn availability_cores(
		&self,
		at: Hash,
	) -> Result<
		Vec<polkadot_primitives::CoreState<Hash, polkadot_core_primitives::BlockNumber>>,
		sp_api::ApiError,
	> {
		Ok(self.rpc_client.parachain_host_availability_cores(at).await?)
	}

	async fn persisted_validation_data(
		&self,
		at: Hash,
		para_id: ParaId,
		assumption: polkadot_primitives::OccupiedCoreAssumption,
	) -> Result<Option<PersistedValidationData<Hash, BlockNumber>>, sp_api::ApiError> {
		Ok(self
			.rpc_client
			.parachain_host_persisted_validation_data(at, para_id, assumption)
			.await?)
	}

	async fn assumed_validation_data(
		&self,
		at: Hash,
		para_id: ParaId,
		expected_persisted_validation_data_hash: Hash,
	) -> Result<
		Option<(
			PersistedValidationData<Hash, BlockNumber>,
			polkadot_primitives::ValidationCodeHash,
		)>,
		sp_api::ApiError,
	> {
		Ok(self
			.rpc_client
			.parachain_host_assumed_validation_data(
				at,
				para_id,
				expected_persisted_validation_data_hash,
			)
			.await?)
	}

	async fn check_validation_outputs(
		&self,
		at: Hash,
		para_id: ParaId,
		outputs: polkadot_primitives::CandidateCommitments,
	) -> Result<bool, sp_api::ApiError> {
		Ok(self
			.rpc_client
			.parachain_host_check_validation_outputs(at, para_id, outputs)
			.await?)
	}

	async fn session_index_for_child(
		&self,
		at: Hash,
	) -> Result<polkadot_primitives::SessionIndex, sp_api::ApiError> {
		Ok(self.rpc_client.parachain_host_session_index_for_child(at).await?)
	}

	async fn validation_code(
		&self,
		at: Hash,
		para_id: ParaId,
		assumption: polkadot_primitives::OccupiedCoreAssumption,
	) -> Result<Option<polkadot_primitives::ValidationCode>, sp_api::ApiError> {
		Ok(self.rpc_client.parachain_host_validation_code(at, para_id, assumption).await?)
	}

	async fn candidate_pending_availability(
		&self,
		at: Hash,
		para_id: cumulus_primitives_core::ParaId,
	) -> Result<Option<polkadot_primitives::CommittedCandidateReceiptV2<Hash>>, sp_api::ApiError> {
		Ok(self
			.rpc_client
			.parachain_host_candidate_pending_availability(at, para_id)
			.await?)
	}

	async fn candidate_events(
		&self,
		at: Hash,
	) -> Result<Vec<polkadot_primitives::CandidateEvent<Hash>>, sp_api::ApiError> {
		Ok(self.rpc_client.parachain_host_candidate_events(at).await?)
	}

	async fn dmq_contents(
		&self,
		at: Hash,
		recipient: ParaId,
	) -> Result<Vec<InboundDownwardMessage<BlockNumber>>, sp_api::ApiError> {
		Ok(self.rpc_client.parachain_host_dmq_contents(recipient, at).await?)
	}

	async fn inbound_hrmp_channels_contents(
		&self,
		at: Hash,
		recipient: ParaId,
	) -> Result<
		std::collections::BTreeMap<
			ParaId,
			Vec<polkadot_core_primitives::InboundHrmpMessage<BlockNumber>>,
		>,
		sp_api::ApiError,
	> {
		Ok(self
			.rpc_client
			.parachain_host_inbound_hrmp_channels_contents(recipient, at)
			.await?)
	}

	async fn validation_code_by_hash(
		&self,
		at: Hash,
		validation_code_hash: polkadot_primitives::ValidationCodeHash,
	) -> Result<Option<polkadot_primitives::ValidationCode>, sp_api::ApiError> {
		Ok(self
			.rpc_client
			.parachain_host_validation_code_by_hash(at, validation_code_hash)
			.await?)
	}

	async fn on_chain_votes(
		&self,
		at: Hash,
	) -> Result<Option<polkadot_primitives::ScrapedOnChainVotes<Hash>>, sp_api::ApiError> {
		Ok(self.rpc_client.parachain_host_on_chain_votes(at).await?)
	}

	async fn session_info(
		&self,
		at: Hash,
		index: polkadot_primitives::SessionIndex,
	) -> Result<Option<polkadot_primitives::SessionInfo>, sp_api::ApiError> {
		Ok(self.rpc_client.parachain_host_session_info(at, index).await?)
	}

	async fn session_executor_params(
		&self,
		at: Hash,
		session_index: polkadot_primitives::SessionIndex,
	) -> Result<Option<polkadot_primitives::ExecutorParams>, sp_api::ApiError> {
		Ok(self
			.rpc_client
			.parachain_host_session_executor_params(at, session_index)
			.await?)
	}

	async fn submit_pvf_check_statement(
		&self,
		at: Hash,
		stmt: polkadot_primitives::PvfCheckStatement,
		signature: polkadot_primitives::ValidatorSignature,
	) -> Result<(), sp_api::ApiError> {
		Ok(self
			.rpc_client
			.parachain_host_submit_pvf_check_statement(at, stmt, signature)
			.await?)
	}

	async fn pvfs_require_precheck(
		&self,
		at: Hash,
	) -> Result<Vec<polkadot_primitives::ValidationCodeHash>, sp_api::ApiError> {
		Ok(self.rpc_client.parachain_host_pvfs_require_precheck(at).await?)
	}

	async fn validation_code_hash(
		&self,
		at: Hash,
		para_id: ParaId,
		assumption: polkadot_primitives::OccupiedCoreAssumption,
	) -> Result<Option<polkadot_primitives::ValidationCodeHash>, sp_api::ApiError> {
		Ok(self
			.rpc_client
			.parachain_host_validation_code_hash(at, para_id, assumption)
			.await?)
	}

	async fn current_epoch(&self, at: Hash) -> Result<sp_consensus_babe::Epoch, sp_api::ApiError> {
		Ok(self.rpc_client.babe_api_current_epoch(at).await?)
	}

	async fn authorities(
		&self,
		at: Hash,
	) -> std::result::Result<Vec<polkadot_primitives::AuthorityDiscoveryId>, sp_api::ApiError> {
		Ok(self.rpc_client.authority_discovery_authorities(at).await?)
	}

	async fn api_version_parachain_host(&self, at: Hash) -> Result<Option<u32>, sp_api::ApiError> {
		let api_id = <dyn polkadot_primitives::runtime_api::ParachainHost<Block>>::ID;
		Ok(self.rpc_client.runtime_version(at).await.map(|v| v.api_version(&api_id))?)
	}

	async fn disputes(
		&self,
		at: Hash,
	) -> Result<
		Vec<(
			polkadot_primitives::SessionIndex,
			polkadot_primitives::CandidateHash,
			polkadot_primitives::DisputeState<polkadot_primitives::BlockNumber>,
		)>,
		ApiError,
	> {
		Ok(self.rpc_client.parachain_host_disputes(at).await?)
	}

	async fn unapplied_slashes(
		&self,
		at: Hash,
	) -> Result<
		Vec<(
			polkadot_primitives::SessionIndex,
			polkadot_primitives::CandidateHash,
			slashing::LegacyPendingSlashes,
		)>,
		ApiError,
	> {
		Ok(self.rpc_client.parachain_host_unapplied_slashes(at).await?)
	}

	async fn unapplied_slashes_v2(
		&self,
		at: Hash,
	) -> Result<
		Vec<(
			polkadot_primitives::SessionIndex,
			polkadot_primitives::CandidateHash,
			slashing::PendingSlashes,
		)>,
		ApiError,
	> {
		Ok(self.rpc_client.parachain_host_unapplied_slashes_v2(at).await?)
	}

	async fn key_ownership_proof(
		&self,
		at: Hash,
		validator_id: polkadot_primitives::ValidatorId,
	) -> Result<Option<slashing::OpaqueKeyOwnershipProof>, ApiError> {
		Ok(self.rpc_client.parachain_host_key_ownership_proof(at, validator_id).await?)
	}

	async fn submit_report_dispute_lost(
		&self,
		at: Hash,
		dispute_proof: slashing::DisputeProof,
		key_ownership_proof: slashing::OpaqueKeyOwnershipProof,
	) -> Result<Option<()>, ApiError> {
		Ok(self
			.rpc_client
			.parachain_host_submit_report_dispute_lost(at, dispute_proof, key_ownership_proof)
			.await?)
	}

	async fn minimum_backing_votes(
		&self,
		at: Hash,
		session_index: polkadot_primitives::SessionIndex,
	) -> Result<u32, ApiError> {
		Ok(self.rpc_client.parachain_host_minimum_backing_votes(at, session_index).await?)
	}

	async fn disabled_validators(
		&self,
		at: Hash,
	) -> Result<Vec<polkadot_primitives::ValidatorIndex>, ApiError> {
		Ok(self.rpc_client.parachain_host_disabled_validators(at).await?)
	}

	async fn async_backing_params(&self, at: Hash) -> Result<AsyncBackingParams, ApiError> {
		Ok(self.rpc_client.parachain_host_async_backing_params(at).await?)
	}

	async fn para_backing_state(
		&self,
		at: Hash,
		para_id: ParaId,
	) -> Result<Option<BackingState>, ApiError> {
		Ok(self.rpc_client.parachain_host_para_backing_state(at, para_id).await?)
	}

	/// Approval voting configuration parameters
	async fn approval_voting_params(
		&self,
		at: Hash,
		session_index: polkadot_primitives::SessionIndex,
	) -> Result<ApprovalVotingParams, ApiError> {
		Ok(self.rpc_client.parachain_host_approval_voting_params(at, session_index).await?)
	}

	async fn node_features(&self, at: Hash) -> Result<NodeFeatures, ApiError> {
		Ok(self.rpc_client.parachain_host_node_features(at).await?)
	}

	async fn claim_queue(
		&self,
		at: Hash,
	) -> Result<BTreeMap<CoreIndex, VecDeque<ParaId>>, ApiError> {
		Ok(self.rpc_client.parachain_host_claim_queue(at).await?)
	}

	async fn candidates_pending_availability(
		&self,
		at: Hash,
		para_id: cumulus_primitives_core::ParaId,
	) -> Result<Vec<polkadot_primitives::CommittedCandidateReceiptV2<Hash>>, sp_api::ApiError> {
		Ok(self
			.rpc_client
			.parachain_host_candidates_pending_availability(at, para_id)
			.await?)
	}

	async fn backing_constraints(
		&self,
		at: Hash,
		para_id: ParaId,
	) -> Result<Option<Constraints>, ApiError> {
		Ok(self.rpc_client.parachain_host_backing_constraints(at, para_id).await?)
	}

	async fn scheduling_lookahead(&self, at: Hash) -> Result<u32, sp_api::ApiError> {
		Ok(self.rpc_client.parachain_host_scheduling_lookahead(at).await?)
	}

	async fn validation_code_bomb_limit(&self, at: Hash) -> Result<u32, sp_api::ApiError> {
		Ok(self.rpc_client.parachain_host_validation_code_bomb_limit(at).await?)
	}

	async fn para_ids(&self, at: Hash) -> Result<Vec<ParaId>, sp_api::ApiError> {
		Ok(self.rpc_client.parachain_host_para_ids(at).await?)
	}

	async fn max_relay_parent_session_age(&self, at: Hash) -> Result<u32, sp_api::ApiError> {
		Ok(self.rpc_client.parachain_host_max_relay_parent_session_age(at).await?)
	}

	async fn ancestor_relay_parent_info(
		&self,
		at: Hash,
		session_index: polkadot_primitives::SessionIndex,
		relay_parent: Hash,
	) -> Result<
		Option<polkadot_primitives::vstaging::RelayParentInfo<Hash, BlockNumber>>,
		sp_api::ApiError,
	> {
		Ok(self
			.rpc_client
			.parachain_host_ancestor_relay_parent_info(at, session_index, relay_parent)
			.await?)
	}
}

#[async_trait::async_trait]
impl AuthorityDiscovery<Block> for BlockChainRpcClient {
	async fn authorities(
		&self,
		at: Hash,
	) -> std::result::Result<Vec<polkadot_primitives::AuthorityDiscoveryId>, sp_api::ApiError> {
		let result = self.rpc_client.authority_discovery_authorities(at).await?;
		Ok(result)
	}

	async fn best_hash(&self) -> std::result::Result<Hash, AuthorityDiscoveryError> {
		self.block_get_hash(None)
			.await
			.ok()
			.flatten()
			.ok_or_else(|| AuthorityDiscoveryError::BestBlockFetchingError)
	}
}

impl BlockChainRpcClient {
	pub async fn import_notification_stream(
		&self,
	) -> RelayChainResult<Pin<Box<dyn Stream<Item = Header> + Send>>> {
		Ok(self.rpc_client.get_imported_heads_stream()?.boxed())
	}

	pub async fn finality_notification_stream(
		&self,
	) -> RelayChainResult<Pin<Box<dyn Stream<Item = Header> + Send>>> {
		Ok(self.rpc_client.get_finalized_heads_stream()?.boxed())
	}
}

// Implementation required by ChainApiSubsystem
// but never called in our case.
impl AuxStore for BlockChainRpcClient {
	fn insert_aux<
		'a,
		'b: 'a,
		'c: 'a,
		I: IntoIterator<Item = &'a (&'c [u8], &'c [u8])>,
		D: IntoIterator<Item = &'a &'b [u8]>,
	>(
		&self,
		_insert: I,
		_delete: D,
	) -> sp_blockchain::Result<()> {
		unimplemented!("Not supported on the RPC collator")
	}

	fn get_aux(&self, _key: &[u8]) -> sp_blockchain::Result<Option<Vec<u8>>> {
		unimplemented!("Not supported on the RPC collator")
	}
}
