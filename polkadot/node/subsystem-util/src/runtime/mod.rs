// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Convenient interface to runtime information.

use polkadot_node_primitives::MAX_FINALITY_LAG;
use schnellru::{ByLength, LruMap};

use codec::Encode;
use sp_application_crypto::AppCrypto;
use sp_core::crypto::ByteArray;
use sp_keystore::{Keystore, KeystorePtr};

use polkadot_node_subsystem::{
	errors::RuntimeApiError,
	messages::{RuntimeApiMessage, RuntimeApiRequest},
	overseer, SubsystemSender,
};
use polkadot_node_subsystem_types::UnpinHandle;
use polkadot_primitives::{
	node_features::FeatureIndex, slashing, ApprovalVotingParams, CandidateEvent, CandidateHash,
	CoreIndex, CoreState, EncodeAs, GroupIndex, GroupRotationInfo, Hash, Id as ParaId, IndexedVec,
	NodeFeatures, OccupiedCore, ScrapedOnChainVotes, SessionIndex, SessionInfo, Signed,
	SigningContext, UncheckedSigned, ValidationCode, ValidationCodeHash, ValidatorId,
	ValidatorIndex, DEFAULT_SCHEDULING_LOOKAHEAD,
};

use std::collections::{BTreeMap, VecDeque};

use crate::{
	request_approval_voting_params, request_availability_cores, request_candidate_events,
	request_claim_queue, request_disabled_validators, request_from_runtime,
	request_key_ownership_proof, request_node_features, request_on_chain_votes,
	request_session_index_for_child, request_session_info, request_submit_report_dispute_lost,
	request_unapplied_slashes, request_unapplied_slashes_v2, request_validation_code_by_hash,
	request_validator_groups,
};

/// Errors that can happen on runtime fetches.
mod error;

use error::Result;
pub use error::{recv_runtime, Error, FatalError, JfyiError};

const LOG_TARGET: &'static str = "parachain::runtime-info";

/// Configuration for construction a `RuntimeInfo`.
pub struct Config {
	/// Needed for retrieval of `ValidatorInfo`
	///
	/// Pass `None` if you are not interested.
	pub keystore: Option<KeystorePtr>,

	/// How many sessions should we keep in the cache?
	pub session_cache_lru_size: u32,
}

/// Caching of session info.
///
/// It should be ensured that a cached session stays live in the cache as long as we might need it.
pub struct RuntimeInfo {
	/// Get the session index for a given relay parent.
	///
	/// We query this up to a 100 times per block, so caching it here without roundtrips over the
	/// overseer seems sensible.
	session_index_cache: LruMap<Hash, SessionIndex>,

	/// In the happy case, we do not query disabled validators at all. In the worst case, we can
	/// query it order of `n_cores` times `n_validators` per block, so caching it here seems
	/// sensible.
	disabled_validators_cache: LruMap<Hash, Vec<ValidatorIndex>>,

	/// Look up cached sessions by `SessionIndex`.
	session_info_cache: LruMap<SessionIndex, ExtendedSessionInfo>,

	/// Unpin handle of *some* block in the session.
	/// Only blocks pinned explicitly by `pin_block` are stored here.
	pinned_blocks: LruMap<SessionIndex, UnpinHandle>,

	/// Key store for determining whether we are a validator and what `ValidatorIndex` we have.
	keystore: Option<KeystorePtr>,
}

/// `SessionInfo` with additional useful data for validator nodes.
pub struct ExtendedSessionInfo {
	/// Actual session info as fetched from the runtime.
	pub session_info: SessionInfo,
	/// Contains useful information about ourselves, in case this node is a validator.
	pub validator_info: ValidatorInfo,
	/// Node features
	pub node_features: NodeFeatures,
	/// Approval-voting parameters.
	pub approval_voting_params: ApprovalVotingParams,
}

/// Information about ourselves, in case we are an `Authority`.
///
/// This data is derived from the `SessionInfo` and our key as found in the keystore.
pub struct ValidatorInfo {
	/// The index this very validator has in `SessionInfo` vectors, if any.
	pub our_index: Option<ValidatorIndex>,
	/// The group we belong to, if any.
	pub our_group: Option<GroupIndex>,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			keystore: None,
			// Usually we need to cache the current and the last session.
			session_cache_lru_size: 2,
		}
	}
}

impl RuntimeInfo {
	/// Create a new `RuntimeInfo` for convenient runtime fetches.
	pub fn new(keystore: Option<KeystorePtr>) -> Self {
		Self::new_with_config(Config { keystore, ..Default::default() })
	}

	/// Create with more elaborate configuration options.
	pub fn new_with_config(cfg: Config) -> Self {
		Self {
			// Usually messages are processed for blocks pointing to hashes from last finalized
			// block to to best, so make this cache large enough to hold at least this amount of
			// hashes, so that we get the benefit of caching even when finality lag is large.
			session_index_cache: LruMap::new(ByLength::new(
				cfg.session_cache_lru_size.max(2 * MAX_FINALITY_LAG),
			)),
			session_info_cache: LruMap::new(ByLength::new(cfg.session_cache_lru_size)),
			disabled_validators_cache: LruMap::new(ByLength::new(100)),
			pinned_blocks: LruMap::new(ByLength::new(cfg.session_cache_lru_size)),
			keystore: cfg.keystore,
		}
	}

	/// Returns the session index expected at any child of the `parent` block.
	/// This does not return the session index for the `parent` block.
	pub async fn get_session_index_for_child<Sender>(
		&mut self,
		sender: &mut Sender,
		parent: Hash,
	) -> Result<SessionIndex>
	where
		Sender: SubsystemSender<RuntimeApiMessage>,
	{
		match self.session_index_cache.get(&parent) {
			Some(index) => Ok(*index),
			None => {
				let index =
					recv_runtime(request_session_index_for_child(parent, sender).await).await?;
				self.session_index_cache.insert(parent, index);
				Ok(index)
			},
		}
	}

	/// Pin a given block in the given session if none are pinned in that session.
	/// Unpinning will happen automatically when LRU cache grows over the limit.
	pub fn pin_block(&mut self, session_index: SessionIndex, unpin_handle: UnpinHandle) {
		self.pinned_blocks.get_or_insert(session_index, || unpin_handle);
	}

	/// Get the hash of a pinned block for the given session index, if any.
	pub fn get_block_in_session(&self, session_index: SessionIndex) -> Option<Hash> {
		self.pinned_blocks.peek(&session_index).map(|h| h.hash())
	}

	/// Get `ExtendedSessionInfo` by relay parent hash.
	pub async fn get_session_info<'a, Sender>(
		&'a mut self,
		sender: &mut Sender,
		relay_parent: Hash,
	) -> Result<&'a ExtendedSessionInfo>
	where
		Sender: SubsystemSender<RuntimeApiMessage>,
	{
		let session_index = self.get_session_index_for_child(sender, relay_parent).await?;

		self.get_session_info_by_index(sender, relay_parent, session_index).await
	}

	/// Get the list of disabled validators at the relay parent.
	pub async fn get_disabled_validators<Sender>(
		&mut self,
		sender: &mut Sender,
		relay_parent: Hash,
	) -> Result<Vec<ValidatorIndex>>
	where
		Sender: SubsystemSender<RuntimeApiMessage>,
	{
		match self.disabled_validators_cache.get(&relay_parent).cloned() {
			Some(result) => Ok(result),
			None => {
				let disabled_validators =
					request_disabled_validators(relay_parent, sender).await.await??;
				self.disabled_validators_cache.insert(relay_parent, disabled_validators.clone());
				Ok(disabled_validators)
			},
		}
	}

	/// Get `ExtendedSessionInfo` by session index.
	///
	/// `request_session_info` still requires the parent to be passed in, so we take the parent
	/// in addition to the `SessionIndex`.
	pub async fn get_session_info_by_index<'a, Sender>(
		&'a mut self,
		sender: &mut Sender,
		parent: Hash,
		session_index: SessionIndex,
	) -> Result<&'a ExtendedSessionInfo>
	where
		Sender: SubsystemSender<RuntimeApiMessage>,
	{
		if self.session_info_cache.get(&session_index).is_none() {
			let session_info =
				recv_runtime(request_session_info(parent, session_index, sender).await)
					.await?
					.ok_or(JfyiError::NoSuchSession(session_index))?;

			let validator_info = self.get_validator_info(&session_info)?;

			let node_features =
				request_node_features(parent, session_index, sender).await.await??;
			let last_set_index = node_features.iter_ones().last().unwrap_or_default();
			if last_set_index >= FeatureIndex::FirstUnassigned as usize {
				gum::warn!(target: LOG_TARGET, "Runtime requires feature bit {} that node doesn't support, please upgrade node version", last_set_index);
			}

			let approval_voting_params =
				request_approval_voting_params(parent, session_index, sender).await.await??;

			let full_info = ExtendedSessionInfo {
				session_info,
				validator_info,
				node_features,
				approval_voting_params,
			};

			self.session_info_cache.insert(session_index, full_info);
		}
		Ok(self
			.session_info_cache
			.get(&session_index)
			.expect("We just put the value there. qed."))
	}

	/// Convenience function for checking the signature of something signed.
	pub async fn check_signature<Sender, Payload, RealPayload>(
		&mut self,
		sender: &mut Sender,
		relay_parent: Hash,
		signed: UncheckedSigned<Payload, RealPayload>,
	) -> Result<
		std::result::Result<Signed<Payload, RealPayload>, UncheckedSigned<Payload, RealPayload>>,
	>
	where
		Sender: SubsystemSender<RuntimeApiMessage>,
		Payload: EncodeAs<RealPayload> + Clone,
		RealPayload: Encode + Clone,
	{
		let session_index = self.get_session_index_for_child(sender, relay_parent).await?;
		let info = self.get_session_info_by_index(sender, relay_parent, session_index).await?;
		Ok(check_signature(session_index, &info.session_info, relay_parent, signed))
	}

	/// Build `ValidatorInfo` for the current session.
	///
	///
	/// Returns: `None` if not a parachain validator.
	fn get_validator_info(&self, session_info: &SessionInfo) -> Result<ValidatorInfo> {
		if let Some(our_index) = self.get_our_index(&session_info.validators) {
			// Get our group index:
			let our_group =
				session_info.validator_groups.iter().enumerate().find_map(|(i, g)| {
					g.iter().find_map(|v| {
						if *v == our_index {
							Some(GroupIndex(i as u32))
						} else {
							None
						}
					})
				});
			let info = ValidatorInfo { our_index: Some(our_index), our_group };
			return Ok(info);
		}
		return Ok(ValidatorInfo { our_index: None, our_group: None });
	}

	/// Get our `ValidatorIndex`.
	///
	/// Returns: None if we are not a validator.
	fn get_our_index(
		&self,
		validators: &IndexedVec<ValidatorIndex, ValidatorId>,
	) -> Option<ValidatorIndex> {
		let keystore = self.keystore.as_ref()?;
		for (i, v) in validators.iter().enumerate() {
			if Keystore::has_keys(&**keystore, &[(v.to_raw_vec(), ValidatorId::ID)]) {
				return Some(ValidatorIndex(i as u32));
			}
		}
		None
	}
}

/// Convenience function for quickly checking the signature on signed data.
pub fn check_signature<Payload, RealPayload>(
	session_index: SessionIndex,
	session_info: &SessionInfo,
	relay_parent: Hash,
	signed: UncheckedSigned<Payload, RealPayload>,
) -> std::result::Result<Signed<Payload, RealPayload>, UncheckedSigned<Payload, RealPayload>>
where
	Payload: EncodeAs<RealPayload> + Clone,
	RealPayload: Encode + Clone,
{
	let signing_context = SigningContext { session_index, parent_hash: relay_parent };

	session_info
		.validators
		.get(signed.unchecked_validator_index())
		.ok_or_else(|| signed.clone())
		.and_then(|v| signed.try_into_checked(&signing_context, v))
}

/// Request availability cores from the runtime.
pub async fn get_availability_cores<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
) -> Result<Vec<CoreState>>
where
	Sender: overseer::SubsystemSender<RuntimeApiMessage>,
{
	recv_runtime(request_availability_cores(relay_parent, sender).await).await
}

/// Variant of `request_availability_cores` that only returns occupied ones.
pub async fn get_occupied_cores<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
) -> Result<Vec<(CoreIndex, OccupiedCore)>>
where
	Sender: overseer::SubsystemSender<RuntimeApiMessage>,
{
	let cores = get_availability_cores(sender, relay_parent).await?;

	Ok(cores
		.into_iter()
		.enumerate()
		.filter_map(|(core_index, core_state)| {
			if let CoreState::Occupied(occupied) = core_state {
				Some((CoreIndex(core_index as u32), occupied))
			} else {
				None
			}
		})
		.collect())
}

/// Get group rotation info based on the given `relay_parent`.
pub async fn get_group_rotation_info<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
) -> Result<GroupRotationInfo>
where
	Sender: overseer::SubsystemSender<RuntimeApiMessage>,
{
	// We drop `groups` here as we don't need them, because of `RuntimeInfo`. Ideally we would not
	// fetch them in the first place.
	let (_, info) = recv_runtime(request_validator_groups(relay_parent, sender).await).await?;
	Ok(info)
}

/// Get `CandidateEvent`s for the given `relay_parent`.
pub async fn get_candidate_events<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
) -> Result<Vec<CandidateEvent>>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	recv_runtime(request_candidate_events(relay_parent, sender).await).await
}

/// Get on chain votes.
pub async fn get_on_chain_votes<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
) -> Result<Option<ScrapedOnChainVotes>>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	recv_runtime(request_on_chain_votes(relay_parent, sender).await).await
}

/// Fetch `ValidationCode` by hash from the runtime.
pub async fn get_validation_code_by_hash<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
	validation_code_hash: ValidationCodeHash,
) -> Result<Option<ValidationCode>>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	recv_runtime(request_validation_code_by_hash(relay_parent, validation_code_hash, sender).await)
		.await
}

/// Fetch a list of `PendingSlashes` from the runtime.
/// Will fallback to `unapplied_slashes` if the runtime does not
/// support `unapplied_slashes_v2`.
pub async fn get_unapplied_slashes<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
) -> Result<Vec<(SessionIndex, CandidateHash, slashing::PendingSlashes)>>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	match recv_runtime(request_unapplied_slashes_v2(relay_parent, sender).await).await {
		Ok(v2) => Ok(v2),
		Err(Error::RuntimeRequest(RuntimeApiError::NotSupported { .. })) => {
			// Fallback to legacy unapplied_slashes
			let legacy =
				recv_runtime(request_unapplied_slashes(relay_parent, sender).await).await?;
			// Convert legacy slashes to PendingSlashes
			Ok(legacy
				.into_iter()
				.map(|(session, candidate_hash, legacy_slash)| {
					(
						session,
						candidate_hash,
						slashing::PendingSlashes {
							keys: legacy_slash.keys,
							kind: legacy_slash.kind.into(),
						},
					)
				})
				.collect())
		},
		Err(e) => Err(e),
	}
}

/// Generate validator key ownership proof.
///
/// Note: The choice of `relay_parent` is important here, it needs to match
/// the desired session index of the validator set in question.
pub async fn key_ownership_proof<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
	validator_id: ValidatorId,
) -> Result<Option<slashing::OpaqueKeyOwnershipProof>>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	recv_runtime(request_key_ownership_proof(relay_parent, validator_id, sender).await).await
}

/// Submit a past-session dispute slashing report.
pub async fn submit_report_dispute_lost<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
	dispute_proof: slashing::DisputeProof,
	key_ownership_proof: slashing::OpaqueKeyOwnershipProof,
) -> Result<Option<()>>
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	recv_runtime(
		request_submit_report_dispute_lost(
			relay_parent,
			dispute_proof,
			key_ownership_proof,
			sender,
		)
		.await,
	)
	.await
}

/// A snapshot of the runtime claim queue at an arbitrary relay chain block.
#[derive(Default, Clone, Debug)]
pub struct ClaimQueueSnapshot(pub BTreeMap<CoreIndex, VecDeque<ParaId>>);

impl From<BTreeMap<CoreIndex, VecDeque<ParaId>>> for ClaimQueueSnapshot {
	fn from(claim_queue_snapshot: BTreeMap<CoreIndex, VecDeque<ParaId>>) -> Self {
		ClaimQueueSnapshot(claim_queue_snapshot)
	}
}

impl ClaimQueueSnapshot {
	/// Returns the `ParaId` that has a claim for `core_index` at the specified `depth` in the
	/// claim queue. A depth of `0` means the very next block.
	pub fn get_claim_for(&self, core_index: CoreIndex, depth: usize) -> Option<ParaId> {
		self.0.get(&core_index)?.get(depth).copied()
	}

	/// Returns an iterator over all claimed cores and the claiming `ParaId` at the specified
	/// `depth` in the claim queue.
	pub fn iter_claims_at_depth(
		&self,
		depth: usize,
	) -> impl Iterator<Item = (CoreIndex, ParaId)> + '_ {
		self.0
			.iter()
			.filter_map(move |(core_index, paras)| Some((*core_index, *paras.get(depth)?)))
	}

	/// Returns an iterator over all claims on the given core.
	pub fn iter_claims_for_core(
		&self,
		core_index: &CoreIndex,
	) -> impl Iterator<Item = &ParaId> + '_ {
		self.0.get(core_index).map(|c| c.iter()).into_iter().flatten()
	}

	/// Returns an iterator over the whole claim queue.
	pub fn iter_all_claims(&self) -> impl Iterator<Item = (&CoreIndex, &VecDeque<ParaId>)> + '_ {
		self.0.iter()
	}

	/// Get all claimed cores for the given `para_id` at the specified depth.
	pub fn iter_claims_at_depth_for_para(
		&self,
		depth: usize,
		para_id: ParaId,
	) -> impl Iterator<Item = CoreIndex> + '_ {
		self.0.iter().filter_map(move |(core_index, ids)| {
			ids.get(depth).filter(|id| **id == para_id).map(|_| *core_index)
		})
	}
}

/// Fetch the claim queue and wrap it into a helpful `ClaimQueueSnapshot`
pub async fn fetch_claim_queue(
	sender: &mut impl SubsystemSender<RuntimeApiMessage>,
	relay_parent: Hash,
) -> Result<ClaimQueueSnapshot> {
	let cq = request_claim_queue(relay_parent, sender)
		.await
		.await
		.map_err(Error::RuntimeRequestCanceled)??;

	Ok(cq.into())
}

/// Returns the lookahead from the scheduler params if the runtime supports it,
/// or default value if scheduling lookahead API is not supported by runtime.
pub async fn fetch_scheduling_lookahead(
	parent: Hash,
	session_index: SessionIndex,
	sender: &mut impl overseer::SubsystemSender<RuntimeApiMessage>,
) -> Result<u32> {
	let res = recv_runtime(
		request_from_runtime(parent, sender, |tx| {
			RuntimeApiRequest::SchedulingLookahead(session_index, tx)
		})
		.await,
	)
	.await;

	if let Err(Error::RuntimeRequest(RuntimeApiError::NotSupported { .. })) = res {
		gum::trace!(
			target: LOG_TARGET,
			?parent,
			"Querying the scheduling lookahead from the runtime is not supported by the current Runtime API, falling back to default value of {}",
			DEFAULT_SCHEDULING_LOOKAHEAD
		);

		Ok(DEFAULT_SCHEDULING_LOOKAHEAD)
	} else {
		res
	}
}

/// Fetch the validation code bomb limit from the runtime.
pub async fn fetch_validation_code_bomb_limit(
	parent: Hash,
	session_index: SessionIndex,
	sender: &mut impl overseer::SubsystemSender<RuntimeApiMessage>,
) -> Result<u32> {
	let res = recv_runtime(
		request_from_runtime(parent, sender, |tx| {
			RuntimeApiRequest::ValidationCodeBombLimit(session_index, tx)
		})
		.await,
	)
	.await;

	if let Err(Error::RuntimeRequest(RuntimeApiError::NotSupported { .. })) = res {
		gum::trace!(
			target: LOG_TARGET,
			?parent,
			"Querying the validation code bomb limit from the runtime is not supported by the current Runtime API",
		);

		// TODO: Remove this once runtime API version 12 is released.
		#[allow(deprecated)]
		Ok(polkadot_node_primitives::VALIDATION_CODE_BOMB_LIMIT as u32)
	} else {
		res
	}
}
