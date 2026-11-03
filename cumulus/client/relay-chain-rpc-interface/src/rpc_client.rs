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

use futures::channel::{
	mpsc::{Receiver, Sender},
	oneshot::Sender as OneshotSender,
};
use jsonrpsee::{
	core::{params::ArrayParams, ClientError as JsonRpseeError},
	rpc_params,
};
use prometheus::Registry;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value as JsonValue;
use std::collections::{btree_map::BTreeMap, VecDeque};
use tokio::sync::mpsc::Sender as TokioSender;

use codec::{Decode, Encode};

use cumulus_primitives_core::{
	relay_chain::{
		async_backing::{AsyncBackingParams, BackingState, Constraints},
		slashing,
		vstaging::RelayParentInfo,
		ApprovalVotingParams, BlockNumber, CandidateCommitments, CandidateEvent, CandidateHash,
		CommittedCandidateReceiptV2 as CommittedCandidateReceipt, CoreIndex, CoreState,
		DisputeState, ExecutorParams, GroupRotationInfo, Hash as RelayHash, Header as RelayHeader,
		InboundHrmpMessage, NodeFeatures, OccupiedCoreAssumption, PvfCheckStatement,
		ScrapedOnChainVotes, SessionIndex, SessionInfo, ValidationCode, ValidationCodeHash,
		ValidatorId, ValidatorIndex, ValidatorSignature,
	},
	InboundDownwardMessage, ParaId, PersistedValidationData,
};
use cumulus_relay_chain_interface::{RelayChainError, RelayChainResult};

use sc_client_api::StorageData;
use sc_rpc_api::{state::ReadProof, system::Health};
use sc_service::TaskManager;
use sp_consensus_babe::Epoch;
use sp_storage::StorageKey;
use sp_version::RuntimeVersion;

use crate::{metrics::RelaychainRpcMetrics, reconnecting_ws_client::ReconnectingWebsocketWorker};
pub use url::Url;

const LOG_TARGET: &str = "relay-chain-rpc-client";
const NOTIFICATION_CHANNEL_SIZE_LIMIT: usize = 20;

/// Messages for communication between [`RelayChainRpcClient`] and the RPC workers.
#[derive(Debug)]
pub enum RpcDispatcherMessage {
	/// Register new listener for the best headers stream. Contains a sender which will be used
	/// to send incoming headers.
	RegisterBestHeadListener(Sender<RelayHeader>),

	/// Register new listener for the import headers stream. Contains a sender which will be used
	/// to send incoming headers.
	RegisterImportListener(Sender<RelayHeader>),

	/// Register new listener for the finalized headers stream. Contains a sender which will be
	/// used to send incoming headers.
	RegisterFinalizationListener(Sender<RelayHeader>),

	/// Register new listener for the finalized headers stream.
	/// Contains the following:
	/// - [`String`] representing the RPC method to be called
	/// - [`ArrayParams`] for the parameters to the RPC call
	/// - [`OneshotSender`] for the return value of the request
	Request(String, ArrayParams, OneshotSender<Result<JsonValue, JsonRpseeError>>),
}

/// Entry point to create [`RelayChainRpcClient`] and start a worker that communicates
/// to JsonRPC servers over the network.
pub async fn create_client_and_start_worker(
	urls: Vec<Url>,
	task_manager: &mut TaskManager,
	prometheus_registry: Option<&Registry>,
) -> RelayChainResult<RelayChainRpcClient> {
	let (worker, sender) = ReconnectingWebsocketWorker::new(urls).await;

	task_manager
		.spawn_essential_handle()
		.spawn("relay-chain-rpc-worker", None, worker.run());

	let client = RelayChainRpcClient::new(sender, prometheus_registry);

	Ok(client)
}

#[derive(Serialize)]
struct PayloadToHex<'a>(#[serde(with = "sp_core::bytes")] &'a [u8]);

/// Client that maps RPC methods and deserializes results
#[derive(Clone)]
pub struct RelayChainRpcClient {
	/// Sender to send messages to the worker.
	worker_channel: TokioSender<RpcDispatcherMessage>,
	metrics: Option<RelaychainRpcMetrics>,
}

impl RelayChainRpcClient {
	/// Initialize new RPC Client.
	///
	/// This client expects a channel connected to a worker that processes
	/// requests sent via this channel.
	pub(crate) fn new(
		worker_channel: TokioSender<RpcDispatcherMessage>,
		prometheus_registry: Option<&Registry>,
	) -> Self {
		RelayChainRpcClient {
			worker_channel,
			metrics: prometheus_registry
				.and_then(|inner| RelaychainRpcMetrics::register(inner).map_err(|err| {
					tracing::warn!(target: LOG_TARGET, error = %err, "Unable to instantiate the RPC client metrics, continuing w/o metrics setup.");
				}).ok()),
		}
	}

	/// Same as `call_remote_runtime_function` but work on encoded data
	pub async fn call_remote_runtime_function_encoded(
		&self,
		method_name: &str,
		hash: RelayHash,
		payload: &[u8],
	) -> RelayChainResult<sp_core::Bytes> {
		let payload = PayloadToHex(payload);

		let params = rpc_params! {
			method_name,
			payload,
			hash
		};

		self.request_tracing::<sp_core::Bytes, _>("state_call", params, |err| {
			tracing::trace!(
				target: LOG_TARGET,
				%method_name,
				%hash,
				error = %err,
				"Error during call to 'state_call'.",
			);
		})
		.await
	}

	/// Call a call to `state_call` rpc method.
	pub async fn call_remote_runtime_function<R: Decode>(
		&self,
		method_name: &str,
		hash: RelayHash,
		payload: Option<impl Encode>,
	) -> RelayChainResult<R> {
		let payload_bytes =
			payload.map_or(sp_core::Bytes(Vec::new()), |v| sp_core::Bytes(v.encode()));
		let res = self
			.call_remote_runtime_function_encoded(method_name, hash, &payload_bytes)
			.await?;
		Decode::decode(&mut &*res.0).map_err(Into::into)
	}

	/// Perform RPC request
	async fn request<'a, R>(
		&self,
		method: &'a str,
		params: ArrayParams,
	) -> Result<R, RelayChainError>
	where
		R: DeserializeOwned + std::fmt::Debug,
	{
		self.request_tracing(
			method,
			params,
			|e| tracing::trace!(target:LOG_TARGET, error = %e, %method, "Unable to complete RPC request"),
		)
		.await
	}

	/// Perform RPC request
	async fn request_tracing<'a, R, OR>(
		&self,
		method: &'a str,
		params: ArrayParams,
		trace_error: OR,
	) -> Result<R, RelayChainError>
	where
		R: DeserializeOwned + std::fmt::Debug,
		OR: Fn(&RelayChainError),
	{
		let _timer = self.metrics.as_ref().map(|inner| inner.start_request_timer(method));

		let (tx, rx) = futures::channel::oneshot::channel();

		let message = RpcDispatcherMessage::Request(method.into(), params, tx);
		self.worker_channel.send(message).await.map_err(|err| {
			RelayChainError::WorkerCommunicationError(format!(
				"Unable to send message to RPC worker: {}",
				err
			))
		})?;

		let value = rx.await.map_err(|err| {
			RelayChainError::WorkerCommunicationError(format!(
				"RPC worker channel closed. This can hint and connectivity issues with the supplied RPC endpoints. Message: {}",
				err
			))
		})??;

		serde_json::from_value(value).map_err(|_| {
			trace_error(&RelayChainError::GenericError("Unable to deserialize value".to_string()));
			RelayChainError::RpcCallError(method.to_string())
		})
	}

	/// Returns information regarding the current epoch.
	pub async fn babe_api_current_epoch(&self, at: RelayHash) -> Result<Epoch, RelayChainError> {
		self.call_remote_runtime_function("BabeApi_current_epoch", at, None::<()>).await
	}

	/// Scrape dispute relevant from on-chain, backing votes and resolved disputes.
	pub async fn parachain_host_on_chain_votes(
		&self,
		at: RelayHash,
	) -> Result<Option<ScrapedOnChainVotes<RelayHash>>, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_on_chain_votes", at, None::<()>)
			.await
	}

	/// Returns code hashes of PVFs that require pre-checking by validators in the active set.
	pub async fn parachain_host_pvfs_require_precheck(
		&self,
		at: RelayHash,
	) -> Result<Vec<ValidationCodeHash>, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_pvfs_require_precheck", at, None::<()>)
			.await
	}

	/// Submits a PVF pre-checking statement into the transaction pool.
	pub async fn parachain_host_submit_pvf_check_statement(
		&self,
		at: RelayHash,
		stmt: PvfCheckStatement,
		signature: ValidatorSignature,
	) -> Result<(), RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_submit_pvf_check_statement",
			at,
			Some((stmt, signature)),
		)
		.await
	}

	/// Get system health information
	pub async fn system_health(&self) -> Result<Health, RelayChainError> {
		self.request("system_health", rpc_params![]).await
	}

	/// Get read proof for `storage_keys`
	pub async fn state_get_read_proof(
		&self,
		storage_keys: Vec<StorageKey>,
		at: Option<RelayHash>,
	) -> Result<ReadProof<RelayHash>, RelayChainError> {
		let params = rpc_params![storage_keys, at];
		self.request("state_getReadProof", params).await
	}

	/// Get child trie read proof for `child_keys`
	pub async fn state_get_child_read_proof(
		&self,
		child_storage_key: sp_core::storage::PrefixedStorageKey,
		child_keys: Vec<StorageKey>,
		at: Option<RelayHash>,
	) -> Result<ReadProof<RelayHash>, RelayChainError> {
		let params = rpc_params![child_storage_key, child_keys, at];
		self.request("state_getChildReadProof", params).await
	}

	/// Retrieve storage item at `storage_key`
	pub async fn state_get_storage(
		&self,
		storage_key: StorageKey,
		at: Option<RelayHash>,
	) -> Result<Option<StorageData>, RelayChainError> {
		let params = rpc_params![storage_key, at];
		self.request("state_getStorage", params).await
	}

	/// Get hash of the n-th block in the canon chain.
	///
	/// By default returns latest block hash.
	pub async fn chain_get_head(&self, at: Option<u64>) -> Result<RelayHash, RelayChainError> {
		let params = rpc_params![at];
		self.request("chain_getHead", params).await
	}

	/// Returns the validator groups and rotation info localized based on the hypothetical child
	///  of a block whose state  this is invoked on. Note that `now` in the `GroupRotationInfo`
	/// should be the successor of the number of the block.
	pub async fn parachain_host_validator_groups(
		&self,
		at: RelayHash,
	) -> Result<(Vec<Vec<ValidatorIndex>>, GroupRotationInfo), RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_validator_groups", at, None::<()>)
			.await
	}

	/// Get a vector of events concerning candidates that occurred within a block.
	pub async fn parachain_host_candidate_events(
		&self,
		at: RelayHash,
	) -> Result<Vec<CandidateEvent>, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_candidate_events", at, None::<()>)
			.await
	}

	/// Checks if the given validation outputs pass the acceptance criteria.
	pub async fn parachain_host_check_validation_outputs(
		&self,
		at: RelayHash,
		para_id: ParaId,
		outputs: CandidateCommitments,
	) -> Result<bool, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_check_validation_outputs",
			at,
			Some((para_id, outputs)),
		)
		.await
	}

	/// Returns the persisted validation data for the given `ParaId` along with the corresponding
	/// validation code hash. Instead of accepting assumption about the para, matches the validation
	/// data hash against an expected one and yields `None` if they're not equal.
	pub async fn parachain_host_assumed_validation_data(
		&self,
		at: RelayHash,
		para_id: ParaId,
		expected_hash: RelayHash,
	) -> Result<Option<(PersistedValidationData, ValidationCodeHash)>, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_persisted_assumed_validation_data",
			at,
			Some((para_id, expected_hash)),
		)
		.await
	}

	/// Get hash of last finalized block.
	pub async fn chain_get_finalized_head(&self) -> Result<RelayHash, RelayChainError> {
		self.request("chain_getFinalizedHead", rpc_params![]).await
	}

	/// Get hash of n-th block.
	pub async fn chain_get_block_hash(
		&self,
		block_number: Option<BlockNumber>,
	) -> Result<Option<RelayHash>, RelayChainError> {
		let params = rpc_params![block_number];
		self.request("chain_getBlockHash", params).await
	}

	/// Yields the persisted validation data for the given `ParaId` along with an assumption that
	/// should be used if the para currently occupies a core.
	///
	/// Returns `None` if either the para is not registered or the assumption is `Freed`
	/// and the para already occupies a core.
	pub async fn parachain_host_persisted_validation_data(
		&self,
		at: RelayHash,
		para_id: ParaId,
		occupied_core_assumption: OccupiedCoreAssumption,
	) -> Result<Option<PersistedValidationData>, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_persisted_validation_data",
			at,
			Some((para_id, occupied_core_assumption)),
		)
		.await
	}

	/// Get the validation code from its hash.
	pub async fn parachain_host_validation_code_by_hash(
		&self,
		at: RelayHash,
		validation_code_hash: ValidationCodeHash,
	) -> Result<Option<ValidationCode>, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_validation_code_by_hash",
			at,
			Some(validation_code_hash),
		)
		.await
	}

	/// Yields information on all availability cores as relevant to the child block.
	/// Cores are either free or occupied. Free cores can have paras assigned to them.
	pub async fn parachain_host_availability_cores(
		&self,
		at: RelayHash,
	) -> Result<Vec<CoreState<RelayHash, BlockNumber>>, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_availability_cores", at, None::<()>)
			.await
	}

	/// Get runtime version
	pub async fn runtime_version(&self, at: RelayHash) -> Result<RuntimeVersion, RelayChainError> {
		let params = rpc_params![at];
		self.request("state_getRuntimeVersion", params).await
	}

	/// Returns all onchain disputes.
	pub async fn parachain_host_disputes(
		&self,
		at: RelayHash,
	) -> Result<Vec<(SessionIndex, CandidateHash, DisputeState<BlockNumber>)>, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_disputes", at, None::<()>)
			.await
	}

	/// Returns a list of validators that lost a past session dispute and need to be slashed.
	///
	/// This is a staging method! Do not use on production runtimes!
	pub async fn parachain_host_unapplied_slashes(
		&self,
		at: RelayHash,
	) -> Result<Vec<(SessionIndex, CandidateHash, slashing::LegacyPendingSlashes)>, RelayChainError>
	{
		self.call_remote_runtime_function("ParachainHost_unapplied_slashes", at, None::<()>)
			.await
	}

	/// Returns a list of validators that lost a past session dispute and need to be slashed.
	pub async fn parachain_host_unapplied_slashes_v2(
		&self,
		at: RelayHash,
	) -> Result<Vec<(SessionIndex, CandidateHash, slashing::PendingSlashes)>, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_unapplied_slashes_v2", at, None::<()>)
			.await
	}

	/// Returns a merkle proof of a validator session key in a past session.
	///
	/// This is a staging method! Do not use on production runtimes!
	pub async fn parachain_host_key_ownership_proof(
		&self,
		at: RelayHash,
		validator_id: ValidatorId,
	) -> Result<Option<slashing::OpaqueKeyOwnershipProof>, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_key_ownership_proof",
			at,
			Some(validator_id),
		)
		.await
	}

	/// Submits an unsigned extrinsic to slash validators who lost a dispute about
	/// a candidate of a past session.
	///
	/// This is a staging method! Do not use on production runtimes!
	pub async fn parachain_host_submit_report_dispute_lost(
		&self,
		at: RelayHash,
		dispute_proof: slashing::DisputeProof,
		key_ownership_proof: slashing::OpaqueKeyOwnershipProof,
	) -> Result<Option<()>, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_submit_report_dispute_lost",
			at,
			Some((dispute_proof, key_ownership_proof)),
		)
		.await
	}

	pub async fn authority_discovery_authorities(
		&self,
		at: RelayHash,
	) -> Result<Vec<sp_authority_discovery::AuthorityId>, RelayChainError> {
		self.call_remote_runtime_function("AuthorityDiscoveryApi_authorities", at, None::<()>)
			.await
	}

	/// Fetch the validation code used by a para, making the given `OccupiedCoreAssumption`.
	///
	/// Returns `None` if either the para is not registered or the assumption is `Freed`
	/// and the para already occupies a core.
	pub async fn parachain_host_validation_code(
		&self,
		at: RelayHash,
		para_id: ParaId,
		occupied_core_assumption: OccupiedCoreAssumption,
	) -> Result<Option<ValidationCode>, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_validation_code",
			at,
			Some((para_id, occupied_core_assumption)),
		)
		.await
	}

	/// Fetch the hash of the validation code used by a para, making the given
	/// `OccupiedCoreAssumption`.
	pub async fn parachain_host_validation_code_hash(
		&self,
		at: RelayHash,
		para_id: ParaId,
		occupied_core_assumption: OccupiedCoreAssumption,
	) -> Result<Option<ValidationCodeHash>, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_validation_code_hash",
			at,
			Some((para_id, occupied_core_assumption)),
		)
		.await
	}

	/// Get the session info for the given session, if stored.
	pub async fn parachain_host_session_info(
		&self,
		at: RelayHash,
		index: SessionIndex,
	) -> Result<Option<SessionInfo>, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_session_info", at, Some(index))
			.await
	}

	/// Get the executor parameters for the given session, if stored
	pub async fn parachain_host_session_executor_params(
		&self,
		at: RelayHash,
		session_index: SessionIndex,
	) -> Result<Option<ExecutorParams>, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_session_executor_params",
			at,
			Some(session_index),
		)
		.await
	}

	/// Get header at specified hash
	pub async fn chain_get_header(
		&self,
		hash: Option<RelayHash>,
	) -> Result<Option<RelayHeader>, RelayChainError> {
		let params = rpc_params![hash];
		self.request("chain_getHeader", params).await
	}

	/// Get the receipt of a candidate pending availability. This returns `Some` for any paras
	/// assigned to occupied cores in `availability_cores` and `None` otherwise.
	pub async fn parachain_host_candidate_pending_availability(
		&self,
		at: RelayHash,
		para_id: ParaId,
	) -> Result<Option<CommittedCandidateReceipt>, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_candidate_pending_availability",
			at,
			Some(para_id),
		)
		.await
	}

	/// Returns the session index expected at a child of the block.
	///
	/// This can be used to instantiate a `SigningContext`.
	pub async fn parachain_host_session_index_for_child(
		&self,
		at: RelayHash,
	) -> Result<SessionIndex, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_session_index_for_child", at, None::<()>)
			.await
	}

	/// Get the current validators.
	pub async fn parachain_host_validators(
		&self,
		at: RelayHash,
	) -> Result<Vec<ValidatorId>, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_validators", at, None::<()>)
			.await
	}

	/// Get the contents of all channels addressed to the given recipient. Channels that have no
	/// messages in them are also included.
	pub async fn parachain_host_inbound_hrmp_channels_contents(
		&self,
		para_id: ParaId,
		at: RelayHash,
	) -> Result<BTreeMap<ParaId, Vec<InboundHrmpMessage>>, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_inbound_hrmp_channels_contents",
			at,
			Some(para_id),
		)
		.await
	}

	/// Get all the pending inbound messages in the downward message queue for a para.
	pub async fn parachain_host_dmq_contents(
		&self,
		para_id: ParaId,
		at: RelayHash,
	) -> Result<Vec<InboundDownwardMessage>, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_dmq_contents", at, Some(para_id))
			.await
	}

	/// Get the minimum number of backing votes for a candidate.
	pub async fn parachain_host_minimum_backing_votes(
		&self,
		at: RelayHash,
		_session_index: SessionIndex,
	) -> Result<u32, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_minimum_backing_votes", at, None::<()>)
			.await
	}

	pub async fn parachain_host_node_features(
		&self,
		at: RelayHash,
	) -> Result<NodeFeatures, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_node_features", at, None::<()>)
			.await
	}

	pub async fn parachain_host_disabled_validators(
		&self,
		at: RelayHash,
	) -> Result<Vec<ValidatorIndex>, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_disabled_validators", at, None::<()>)
			.await
	}

	#[allow(missing_docs)]
	pub async fn parachain_host_async_backing_params(
		&self,
		at: RelayHash,
	) -> Result<AsyncBackingParams, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_async_backing_params", at, None::<()>)
			.await
	}

	#[allow(missing_docs)]
	pub async fn parachain_host_approval_voting_params(
		&self,
		at: RelayHash,
		_session_index: SessionIndex,
	) -> Result<ApprovalVotingParams, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_approval_voting_params", at, None::<()>)
			.await
	}

	pub async fn parachain_host_para_backing_state(
		&self,
		at: RelayHash,
		para_id: ParaId,
	) -> Result<Option<BackingState>, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_para_backing_state", at, Some(para_id))
			.await
	}

	pub async fn parachain_host_claim_queue(
		&self,
		at: RelayHash,
	) -> Result<BTreeMap<CoreIndex, VecDeque<ParaId>>, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_claim_queue", at, None::<()>)
			.await
	}

	/// Get the receipt of all candidates pending availability.
	pub async fn parachain_host_candidates_pending_availability(
		&self,
		at: RelayHash,
		para_id: ParaId,
	) -> Result<Vec<CommittedCandidateReceipt>, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_candidates_pending_availability",
			at,
			Some(para_id),
		)
		.await
	}

	pub async fn parachain_host_scheduling_lookahead(
		&self,
		at: RelayHash,
	) -> Result<u32, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_scheduling_lookahead", at, None::<()>)
			.await
	}

	pub async fn parachain_host_validation_code_bomb_limit(
		&self,
		at: RelayHash,
	) -> Result<u32, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_validation_code_bomb_limit",
			at,
			None::<()>,
		)
		.await
	}

	pub async fn validation_code_hash(
		&self,
		at: RelayHash,
		para_id: ParaId,
		occupied_core_assumption: OccupiedCoreAssumption,
	) -> Result<Option<ValidationCodeHash>, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_validation_code_hash",
			at,
			Some((para_id, occupied_core_assumption)),
		)
		.await
	}

	pub async fn parachain_host_backing_constraints(
		&self,
		at: RelayHash,
		para_id: ParaId,
	) -> Result<Option<Constraints>, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_backing_constraints", at, Some(para_id))
			.await
	}

	fn send_register_message_to_worker(
		&self,
		message: RpcDispatcherMessage,
	) -> Result<(), RelayChainError> {
		self.worker_channel
			.try_send(message)
			.map_err(|e| RelayChainError::WorkerCommunicationError(e.to_string()))
	}

	/// Get a stream of all imported relay chain headers
	pub fn get_imported_heads_stream(&self) -> Result<Receiver<RelayHeader>, RelayChainError> {
		let (tx, rx) =
			futures::channel::mpsc::channel::<RelayHeader>(NOTIFICATION_CHANNEL_SIZE_LIMIT);
		self.send_register_message_to_worker(RpcDispatcherMessage::RegisterImportListener(tx))?;
		Ok(rx)
	}

	/// Get a stream of new best relay chain headers
	pub fn get_best_heads_stream(&self) -> Result<Receiver<RelayHeader>, RelayChainError> {
		let (tx, rx) =
			futures::channel::mpsc::channel::<RelayHeader>(NOTIFICATION_CHANNEL_SIZE_LIMIT);
		self.send_register_message_to_worker(RpcDispatcherMessage::RegisterBestHeadListener(tx))?;
		Ok(rx)
	}

	/// Get a stream of finalized relay chain headers
	pub fn get_finalized_heads_stream(&self) -> Result<Receiver<RelayHeader>, RelayChainError> {
		let (tx, rx) =
			futures::channel::mpsc::channel::<RelayHeader>(NOTIFICATION_CHANNEL_SIZE_LIMIT);
		self.send_register_message_to_worker(RpcDispatcherMessage::RegisterFinalizationListener(
			tx,
		))?;
		Ok(rx)
	}

	pub async fn parachain_host_max_relay_parent_session_age(
		&self,
		at: RelayHash,
	) -> Result<u32, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_max_relay_parent_session_age",
			at,
			None::<()>,
		)
		.await
	}

	pub async fn parachain_host_para_ids(
		&self,
		at: RelayHash,
	) -> Result<Vec<ParaId>, RelayChainError> {
		self.call_remote_runtime_function("ParachainHost_para_ids", at, None::<()>)
			.await
	}

	pub async fn parachain_host_ancestor_relay_parent_info(
		&self,
		at: RelayHash,
		session_index: SessionIndex,
		relay_parent: RelayHash,
	) -> Result<Option<RelayParentInfo<RelayHash, BlockNumber>>, RelayChainError> {
		self.call_remote_runtime_function(
			"ParachainHost_ancestor_relay_parent_info",
			at,
			Some((session_index, relay_parent)),
		)
		.await
	}
}

/// Send `header` through all channels contained in `senders`.
/// If no one is listening to the sender, it is removed from the vector.
pub fn distribute_header(header: RelayHeader, senders: &mut Vec<Sender<RelayHeader>>) {
	senders.retain_mut(|e| {
				match e.try_send(header.clone()) {
					// Receiver has been dropped, remove Sender from list.
					Err(error) if error.is_disconnected() => false,
					// Channel is full. This should not happen.
					// TODO: Improve error handling here
					// https://github.com/paritytech/cumulus/issues/1482
					Err(error) => {
						tracing::error!(target: LOG_TARGET, ?error, "Event distribution channel has reached its limit. This can lead to missed notifications.");
						true
					},
					_ => true,
				}
			});
}
