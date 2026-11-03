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

//! Utility module for subsystems
//!
//! Many subsystems have common interests such as canceling a bunch of spawned jobs,
//! or determining what their validator ID is. These common interests are factored into
//! this module.
//!
//! This crate also reexports Prometheus metric types which are expected to be implemented by
//! subsystems.

#![warn(missing_docs)]

pub use overseer::{
	gen::{OrchestraError as OverseerError, Timeout},
	Subsystem, TimeoutExt,
};
use polkadot_node_subsystem::{
	errors::{RuntimeApiError, SubsystemError},
	messages::{ChainApiMessage, RuntimeApiMessage, RuntimeApiRequest, RuntimeApiSender},
	overseer, SubsystemSender,
};

pub use polkadot_node_metrics::{metrics, Metronome};

use codec::Encode;
use futures::channel::{mpsc, oneshot};

use polkadot_primitives::{
	async_backing::{BackingState, Constraints},
	slashing,
	vstaging::RelayParentInfo,
	ApprovalVotingParams, AsyncBackingParams, AuthorityDiscoveryId, BlockNumber, CandidateEvent,
	CandidateHash, CommittedCandidateReceiptV2 as CommittedCandidateReceipt, CoreIndex, CoreState,
	EncodeAs, ExecutorParams, GroupIndex, GroupRotationInfo, Hash, Id as ParaId, NodeFeatures,
	OccupiedCoreAssumption, PersistedValidationData, ScrapedOnChainVotes, SessionIndex,
	SessionInfo, Signed, SigningContext, ValidationCode, ValidationCodeHash, ValidatorId,
	ValidatorIndex, ValidatorSignature,
};
pub use rand;
use sp_application_crypto::AppCrypto;
use sp_core::ByteArray;
use sp_keystore::{Error as KeystoreError, KeystorePtr};
use std::{
	collections::{BTreeMap, VecDeque},
	time::Duration,
};
use thiserror::Error;

pub use determine_new_blocks::determine_new_blocks;
pub use metered;
pub use polkadot_node_network_protocol::MIN_GOSSIP_PEERS;

/// These reexports are required so that external crates can use the `delegated_subsystem` macro
/// properly.
pub mod reexports {
	pub use polkadot_overseer::gen::{SpawnedSubsystem, Spawner, Subsystem, SubsystemContext};
}

/// Helpers for the validator->chunk index mapping.
pub mod availability_chunks;
/// A utility for managing the implicit view of the relay-chain derived from active
/// leaves and the minimum allowed relay-parents that parachain candidates can have
/// and be backed in those leaves' children.
pub mod backing_implicit_view;
/// Database trait for subsystem.
pub mod database;
/// An emulator for node-side code to predict the results of on-chain parachain inclusion
/// and predict future constraints.
pub mod inclusion_emulator;
/// Convenient and efficient runtime info access.
pub mod runtime;

/// Helpers for working with unreleased runtime calls
pub mod vstaging;

/// Nested message sending
///
/// Useful for having mostly synchronous code, with submodules spawning short lived asynchronous
/// tasks, sending messages back.
pub mod nesting_sender;

pub mod reputation;

mod determine_new_blocks;

mod controlled_validator_indices;
pub use controlled_validator_indices::ControlledValidatorIndices;

#[cfg(test)]
mod tests;

const LOG_TARGET: &'static str = "parachain::subsystem-util";

/// Duration a job will wait after sending a stop signal before hard-aborting.
pub const JOB_GRACEFUL_STOP_DURATION: Duration = Duration::from_secs(1);
/// Capacity of channels to and from individual jobs
pub const JOB_CHANNEL_CAPACITY: usize = 64;

/// Utility errors
#[derive(Debug, Error)]
pub enum Error {
	/// Attempted to send or receive on a oneshot channel which had been canceled
	#[error(transparent)]
	Oneshot(#[from] oneshot::Canceled),
	/// Attempted to send on a MPSC channel which has been canceled
	#[error(transparent)]
	Mpsc(#[from] mpsc::SendError),
	/// A subsystem error
	#[error(transparent)]
	Subsystem(#[from] SubsystemError),
	/// An error in the Runtime API.
	#[error(transparent)]
	RuntimeApi(#[from] RuntimeApiError),
	/// The type system wants this even though it doesn't make sense
	#[error(transparent)]
	Infallible(#[from] std::convert::Infallible),
	/// Attempted to convert from an `AllMessages` to a `FromJob`, and failed.
	#[error("AllMessage not relevant to Job")]
	SenderConversion(String),
	/// The local node is not a validator.
	#[error("Node is not a validator")]
	NotAValidator,
	/// Already forwarding errors to another sender
	#[error("AlreadyForwarding")]
	AlreadyForwarding,
	/// Data that are supposed to be there a not there
	#[error("Data are not available")]
	DataNotAvailable,
}

impl From<OverseerError> for Error {
	fn from(e: OverseerError) -> Self {
		Self::from(SubsystemError::from(e))
	}
}

impl TryFrom<crate::runtime::Error> for Error {
	type Error = ();

	fn try_from(e: crate::runtime::Error) -> Result<Self, ()> {
		use crate::runtime::Error;

		match e {
			Error::RuntimeRequestCanceled(e) => Ok(Self::Oneshot(e)),
			Error::RuntimeRequest(e) => Ok(Self::RuntimeApi(e)),
			Error::NoSuchSession(_) => Err(()),
		}
	}
}

/// A type alias for Runtime API receivers.
pub type RuntimeApiReceiver<T> = oneshot::Receiver<Result<T, RuntimeApiError>>;

/// Request some data from the `RuntimeApi`.
pub async fn request_from_runtime<RequestBuilder, Response, Sender>(
	parent: Hash,
	sender: &mut Sender,
	request_builder: RequestBuilder,
) -> RuntimeApiReceiver<Response>
where
	RequestBuilder: FnOnce(RuntimeApiSender<Response>) -> RuntimeApiRequest,
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	let (tx, rx) = oneshot::channel();

	sender
		.send_message(RuntimeApiMessage::Request(parent, request_builder(tx)).into())
		.await;

	rx
}

/// Verifies if `ParachainHost` runtime api is at least at version `required_runtime_version`. This
/// method is used to determine if a given runtime call is supported by the runtime.
pub async fn has_required_runtime<Sender>(
	sender: &mut Sender,
	relay_parent: Hash,
	required_runtime_version: u32,
) -> bool
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	gum::trace!(target: LOG_TARGET, ?relay_parent, "Fetching ParachainHost runtime api version");

	let (tx, rx) = oneshot::channel();
	sender
		.send_message(RuntimeApiMessage::Request(relay_parent, RuntimeApiRequest::Version(tx)))
		.await;

	match rx.await {
		Result::Ok(Ok(runtime_version)) => {
			gum::trace!(
				target: LOG_TARGET,
				?relay_parent,
				?runtime_version,
				?required_runtime_version,
				"Fetched  ParachainHost runtime api version"
			);
			runtime_version >= required_runtime_version
		},
		Result::Ok(Err(RuntimeApiError::Execution { source: error, .. })) => {
			gum::trace!(
				target: LOG_TARGET,
				?relay_parent,
				?error,
				"Execution error while fetching ParachainHost runtime api version"
			);
			false
		},
		Result::Ok(Err(RuntimeApiError::NotSupported { .. })) => {
			gum::trace!(
				target: LOG_TARGET,
				?relay_parent,
				"NotSupported error while fetching ParachainHost runtime api version"
			);
			false
		},
		Result::Err(_) => {
			gum::trace!(
				target: LOG_TARGET,
				?relay_parent,
				"Cancelled error while fetching ParachainHost runtime api version"
			);
			false
		},
	}
}

/// Construct specialized request functions for the runtime.
///
/// These would otherwise get pretty repetitive.
macro_rules! specialize_requests {
	// expand return type name for documentation purposes
	(fn $func_name:ident( $( $param_name:ident : $param_ty:ty ),* ) -> $return_ty:ty ; $request_variant:ident;) => {
		specialize_requests!{
			named stringify!($request_variant) ; fn $func_name( $( $param_name : $param_ty ),* ) -> $return_ty ; $request_variant;
		}
	};

	// create a single specialized request function
	(named $doc_name:expr ; fn $func_name:ident( $( $param_name:ident : $param_ty:ty ),* ) -> $return_ty:ty ; $request_variant:ident;) => {
		#[doc = "Request `"]
		#[doc = $doc_name]
		#[doc = "` from the runtime"]
		pub async fn $func_name (
			parent: Hash,
			$(
				$param_name: $param_ty,
			)*
			sender: &mut impl overseer::SubsystemSender<RuntimeApiMessage>,
		) -> RuntimeApiReceiver<$return_ty>
		{
			request_from_runtime(parent, sender, |tx| RuntimeApiRequest::$request_variant(
				$( $param_name, )* tx
			)).await
		}
	};

	// recursive decompose
	(
		fn $func_name:ident( $( $param_name:ident : $param_ty:ty ),* ) -> $return_ty:ty ; $request_variant:ident;
		$(
			fn $t_func_name:ident( $( $t_param_name:ident : $t_param_ty:ty ),* ) -> $t_return_ty:ty ; $t_request_variant:ident;
		)+
	) => {
		specialize_requests!{
			fn $func_name( $( $param_name : $param_ty ),* ) -> $return_ty ; $request_variant ;
		}
		specialize_requests!{
			$(
				fn $t_func_name( $( $t_param_name : $t_param_ty ),* ) -> $t_return_ty ; $t_request_variant ;
			)+
		}
	};
}

specialize_requests! {
	fn request_runtime_api_version() -> u32; Version;
	fn request_authorities() -> Vec<AuthorityDiscoveryId>; Authorities;
	fn request_validators() -> Vec<ValidatorId>; Validators;
	fn request_validator_groups() -> (Vec<Vec<ValidatorIndex>>, GroupRotationInfo); ValidatorGroups;
	fn request_availability_cores() -> Vec<CoreState>; AvailabilityCores;
	fn request_persisted_validation_data(para_id: ParaId, assumption: OccupiedCoreAssumption) -> Option<PersistedValidationData>; PersistedValidationData;
	fn request_assumed_validation_data(para_id: ParaId, expected_persisted_validation_data_hash: Hash) -> Option<(PersistedValidationData, ValidationCodeHash)>; AssumedValidationData;
	fn request_session_index_for_child() -> SessionIndex; SessionIndexForChild;
	fn request_validation_code(para_id: ParaId, assumption: OccupiedCoreAssumption) -> Option<ValidationCode>; ValidationCode;
	fn request_validation_code_by_hash(validation_code_hash: ValidationCodeHash) -> Option<ValidationCode>; ValidationCodeByHash;
	fn request_candidate_pending_availability(para_id: ParaId) -> Option<CommittedCandidateReceipt>; CandidatePendingAvailability;
	fn request_candidates_pending_availability(para_id: ParaId) -> Vec<CommittedCandidateReceipt>; CandidatesPendingAvailability;
	fn request_candidate_events() -> Vec<CandidateEvent>; CandidateEvents;
	fn request_session_info(index: SessionIndex) -> Option<SessionInfo>; SessionInfo;
	fn request_validation_code_hash(para_id: ParaId, assumption: OccupiedCoreAssumption)
		-> Option<ValidationCodeHash>; ValidationCodeHash;
	fn request_on_chain_votes() -> Option<ScrapedOnChainVotes>; FetchOnChainVotes;
	fn request_session_executor_params(session_index: SessionIndex) -> Option<ExecutorParams>;SessionExecutorParams;
	fn request_unapplied_slashes() -> Vec<(SessionIndex, CandidateHash, slashing::LegacyPendingSlashes)>; UnappliedSlashes;
	fn request_unapplied_slashes_v2() -> Vec<(SessionIndex, CandidateHash, slashing::PendingSlashes)>; UnappliedSlashesV2;
	fn request_key_ownership_proof(validator_id: ValidatorId) -> Option<slashing::OpaqueKeyOwnershipProof>; KeyOwnershipProof;
	fn request_submit_report_dispute_lost(dp: slashing::DisputeProof, okop: slashing::OpaqueKeyOwnershipProof) -> Option<()>; SubmitReportDisputeLost;
	fn request_disabled_validators() -> Vec<ValidatorIndex>; DisabledValidators;
	fn request_async_backing_params() -> AsyncBackingParams; AsyncBackingParams;
	fn request_claim_queue() -> BTreeMap<CoreIndex, VecDeque<ParaId>>; ClaimQueue;
	fn request_para_backing_state(para_id: ParaId) -> Option<BackingState>; ParaBackingState;
	fn request_backing_constraints(para_id: ParaId) -> Option<Constraints>; BackingConstraints;
	fn request_min_backing_votes(session_index: SessionIndex) -> u32; MinimumBackingVotes;
	fn request_node_features(session_index: SessionIndex) -> NodeFeatures; NodeFeatures;
	fn request_approval_voting_params(session_index: SessionIndex) -> ApprovalVotingParams; ApprovalVotingParams;
	fn request_para_ids(session_index: SessionIndex) -> Vec<ParaId>; ParaIds;

}

/// Result of [`check_relay_parent_session`].
pub enum CheckRelayParentSessionResult {
	/// The relay parent is valid in the given session.
	Valid,
	/// The relay parent was not found in the given session (or session mismatch
	/// in the self-query case).
	NotFound,
	/// The `ancestor_relay_parent_info` runtime API is not supported. Safe to
	/// skip on old runtimes where cross-session relay parents don't exist.
	NotSupported,
	/// A runtime API or communication error occurred.
	RuntimeError(String),
}

/// Check whether a relay parent is valid in a given session.
///
/// Works for all blocks within the `max_relay_parent_session_age` window,
/// including the block being queried at (the "self" case where
/// `query_at == relay_parent`). The `ancestor_relay_parent_info` runtime API
/// only works for ancestors (a block is not in its own `AllowedRelayParents`).
/// This utility handles the self case by verifying the session directly via
/// `session_index_for_child`.
pub async fn check_relay_parent_session(
	sender: &mut impl overseer::SubsystemSender<RuntimeApiMessage>,
	query_at: Hash,
	session_index: SessionIndex,
	relay_parent: Hash,
) -> CheckRelayParentSessionResult {
	if query_at == relay_parent {
		// Self-query: the runtime API can't answer (block not in its own
		// AllowedRelayParents). Verify the session directly.
		return match request_session_index_for_child(relay_parent, sender).await.await {
			Ok(Ok(session)) if session == session_index => CheckRelayParentSessionResult::Valid,
			Ok(Ok(_)) => CheckRelayParentSessionResult::NotFound,
			Ok(Err(err)) => CheckRelayParentSessionResult::RuntimeError(format!(
				"SessionIndexForChild error: {err}"
			)),
			Err(_) => CheckRelayParentSessionResult::RuntimeError(
				"SessionIndexForChild request cancelled".into(),
			),
		};
	}

	// Ancestor query: use the runtime API.
	match request_from_runtime(query_at, sender, |tx| {
		RuntimeApiRequest::AncestorRelayParentInfo(session_index, relay_parent, tx)
	})
	.await
	.await
	{
		Ok(Ok(Some(_))) => CheckRelayParentSessionResult::Valid,
		Ok(Ok(None)) => CheckRelayParentSessionResult::NotFound,
		Ok(Err(RuntimeApiError::NotSupported { .. })) => {
			CheckRelayParentSessionResult::NotSupported
		},
		Ok(Err(err)) => CheckRelayParentSessionResult::RuntimeError(format!(
			"AncestorRelayParentInfo error: {err}"
		)),
		Err(_) => CheckRelayParentSessionResult::RuntimeError(
			"AncestorRelayParentInfo request cancelled".into(),
		),
	}
}

/// Fetch relay parent info for a block, including the block being queried at.
///
/// Works for all blocks within the `max_relay_parent_session_age` window,
/// including the self-query case (`query_at == relay_parent`). For ancestors,
/// uses the `ancestor_relay_parent_info` runtime API. For self, constructs the
/// answer from the block header and session check.
///
/// Requires both `RuntimeApiMessage` and `ChainApiMessage` senders.
pub async fn fetch_relay_parent_info<Sender>(
	sender: &mut Sender,
	query_at: Hash,
	session_index: SessionIndex,
	relay_parent: Hash,
) -> Result<Option<RelayParentInfo<Hash, BlockNumber>>, runtime::Error>
where
	Sender: SubsystemSender<RuntimeApiMessage> + SubsystemSender<ChainApiMessage>,
{
	if query_at == relay_parent {
		// Self-query: the ancestor runtime API can't answer (a block is not in
		// its own AllowedRelayParents). Verify session and construct from header.

		return get_scheduling_parent_info(sender, session_index, relay_parent).await;
	}

	// Ancestor query: use the runtime API.
	match request_from_runtime(query_at, sender, |tx| {
		RuntimeApiRequest::AncestorRelayParentInfo(session_index, relay_parent, tx)
	})
	.await
	.await?
	{
		Ok(info) => Ok(info),
		Err(RuntimeApiError::NotSupported { .. }) => {
			// The runtime API is not existent, this means the v3 descriptor node feature was not
			// enabled. Fallback to querying the chain API.
			get_scheduling_parent_info(sender, session_index, relay_parent).await
		},
		Err(err) => Err(runtime::Error::RuntimeRequest(err)),
	}
}

async fn get_scheduling_parent_info<Sender>(
	sender: &mut Sender,
	session_index: SessionIndex,
	hash: Hash,
) -> Result<Option<RelayParentInfo<Hash, BlockNumber>>, runtime::Error>
where
	Sender: SubsystemSender<RuntimeApiMessage> + SubsystemSender<ChainApiMessage>,
{
	let session_ok = request_session_index_for_child(hash, sender).await.await?? == session_index;
	if !session_ok {
		return Ok(None);
	}

	let (tx, rx) = oneshot::channel();
	sender.send_message(ChainApiMessage::BlockHeader(hash, tx)).await;
	match rx.await? {
		Ok(Some(header)) => {
			Ok(Some(RelayParentInfo { number: header.number, state_root: header.state_root }))
		},
		_ => Ok(None),
	}
}

/// Requests executor parameters from the runtime effective at given relay-parent. First obtains
/// session index at the relay-parent, relying on the fact that it should be cached by the runtime
/// API caching layer even if the block itself has already been pruned. Then requests executor
/// parameters by session index.
/// Returns an error if failed to communicate to the runtime, or the parameters are not in the
/// storage, which should never happen.
/// Returns default execution parameters if the runtime doesn't yet support `SessionExecutorParams`
/// API call.
/// Otherwise, returns execution parameters returned by the runtime.
pub async fn executor_params_at_relay_parent(
	relay_parent: Hash,
	sender: &mut impl overseer::SubsystemSender<RuntimeApiMessage>,
) -> Result<ExecutorParams, Error> {
	match request_session_index_for_child(relay_parent, sender).await.await {
		Err(err) => {
			// Failed to communicate with the runtime
			Err(Error::Oneshot(err))
		},
		Ok(Err(err)) => {
			// Runtime has failed to obtain a session index at the relay-parent.
			Err(Error::RuntimeApi(err))
		},
		Ok(Ok(session_index)) => {
			match request_session_executor_params(relay_parent, session_index, sender).await.await {
				Err(err) => {
					// Failed to communicate with the runtime
					Err(Error::Oneshot(err))
				},
				Ok(Err(RuntimeApiError::NotSupported { .. })) => {
					// Runtime doesn't yet support the api requested, should execute anyway
					// with default set of parameters
					Ok(ExecutorParams::default())
				},
				Ok(Err(err)) => {
					// Runtime failed to execute the request
					Err(Error::RuntimeApi(err))
				},
				Ok(Ok(None)) => {
					// Storage doesn't contain a parameter set for the given session; should
					// never happen
					Err(Error::DataNotAvailable)
				},
				Ok(Ok(Some(executor_params))) => Ok(executor_params),
			}
		},
	}
}

/// From the given set of validators, find the first key we can sign with, if any.
pub fn signing_key<'a>(
	validators: impl IntoIterator<Item = &'a ValidatorId>,
	keystore: &KeystorePtr,
) -> Option<ValidatorId> {
	signing_key_and_index(validators, keystore).map(|(k, _)| k)
}

/// From the given set of validators, find the first key we can sign with, if any, and return it
/// along with the validator index.
pub fn signing_key_and_index<'a>(
	validators: impl IntoIterator<Item = &'a ValidatorId>,
	keystore: &KeystorePtr,
) -> Option<(ValidatorId, ValidatorIndex)> {
	for (i, v) in validators.into_iter().enumerate() {
		if keystore.has_keys(&[(v.to_raw_vec(), ValidatorId::ID)]) {
			return Some((v.clone(), ValidatorIndex(i as _)));
		}
	}
	None
}

/// Sign the given data with the given validator ID.
///
/// Returns `Ok(None)` if the private key that corresponds to that validator ID is not found in the
/// given keystore. Returns an error if the key could not be used for signing.
pub fn sign(
	keystore: &KeystorePtr,
	key: &ValidatorId,
	data: &[u8],
) -> Result<Option<ValidatorSignature>, KeystoreError> {
	let signature = keystore
		.sr25519_sign(ValidatorId::ID, key.as_ref(), data)?
		.map(|sig| sig.into());
	Ok(signature)
}

/// Find the validator group the given validator index belongs to.
pub fn find_validator_group(
	groups: &[Vec<ValidatorIndex>],
	index: ValidatorIndex,
) -> Option<GroupIndex> {
	groups.iter().enumerate().find_map(|(i, g)| {
		if g.contains(&index) {
			Some(GroupIndex(i as _))
		} else {
			None
		}
	})
}

/// Choose a random subset of `min` elements.
/// But always include `is_priority` elements.
pub fn choose_random_subset<T, F: FnMut(&T) -> bool>(is_priority: F, v: &mut Vec<T>, min: usize) {
	choose_random_subset_with_rng(is_priority, v, &mut rand::thread_rng(), min)
}

/// Choose a random subset of `min` elements using a specific Random Generator `Rng`
/// But always include `is_priority` elements.
pub fn choose_random_subset_with_rng<T, F: FnMut(&T) -> bool, R: rand::Rng>(
	is_priority: F,
	v: &mut Vec<T>,
	rng: &mut R,
	min: usize,
) {
	use rand::seq::SliceRandom as _;

	// partition the elements into priority first
	// the returned index is when non_priority elements start
	let i = itertools::partition(v.iter_mut(), is_priority);

	if i >= min || v.len() <= i {
		v.truncate(i);
		return;
	}

	v[i..].shuffle(rng);

	v.truncate(min);
}

/// Returns a `bool` with a probability of `a / b` of being true.
pub fn gen_ratio(a: usize, b: usize) -> bool {
	gen_ratio_rng(a, b, &mut rand::thread_rng())
}

/// Returns a `bool` with a probability of `a / b` of being true.
pub fn gen_ratio_rng<R: rand::Rng>(a: usize, b: usize, rng: &mut R) -> bool {
	rng.gen_ratio(a as u32, b as u32)
}

/// Local validator information
///
/// It can be created if the local node is a validator in the context of a particular
/// relay chain block.
#[derive(Debug)]
pub struct Validator {
	signing_context: SigningContext,
	key: ValidatorId,
	index: ValidatorIndex,
	disabled: bool,
}

impl Validator {
	/// Get a struct representing this node's validator if this node is in fact a validator in the
	/// context of the given block.
	pub async fn new<S>(parent: Hash, keystore: KeystorePtr, sender: &mut S) -> Result<Self, Error>
	where
		S: SubsystemSender<RuntimeApiMessage>,
	{
		// Note: request_validators, request_disabled_validators and request_session_index_for_child
		// do not and cannot run concurrently: they both have a mutable handle to the same sender.
		// However, each of them returns a oneshot::Receiver, and those are resolved concurrently.
		let (validators, disabled_validators, session_index) = futures::try_join!(
			request_validators(parent, sender).await,
			request_disabled_validators(parent, sender).await,
			request_session_index_for_child(parent, sender).await,
		)?;

		let signing_context = SigningContext { session_index: session_index?, parent_hash: parent };

		let validators = validators?;

		let disabled_validators = disabled_validators?;

		Self::construct(&validators, &disabled_validators, signing_context, keystore)
	}

	/// Construct a validator instance without performing runtime fetches.
	///
	/// This can be useful if external code also needs the same data.
	pub fn construct(
		validators: &[ValidatorId],
		disabled_validators: &[ValidatorIndex],
		signing_context: SigningContext,
		keystore: KeystorePtr,
	) -> Result<Self, Error> {
		let (key, index) =
			signing_key_and_index(validators, &keystore).ok_or(Error::NotAValidator)?;

		let disabled = disabled_validators.iter().any(|d: &ValidatorIndex| *d == index);

		Ok(Validator { signing_context, key, index, disabled })
	}

	/// Get this validator's id.
	pub fn id(&self) -> ValidatorId {
		self.key.clone()
	}

	/// Get this validator's local index.
	pub fn index(&self) -> ValidatorIndex {
		self.index
	}

	/// Get the enabled/disabled state of this validator
	pub fn disabled(&self) -> bool {
		self.disabled
	}

	/// Get the current signing context.
	pub fn signing_context(&self) -> &SigningContext {
		&self.signing_context
	}

	/// Sign a payload with this validator
	pub fn sign<Payload: EncodeAs<RealPayload>, RealPayload: Encode>(
		&self,
		keystore: KeystorePtr,
		payload: Payload,
	) -> Result<Option<Signed<Payload, RealPayload>>, KeystoreError> {
		Signed::sign(&keystore, payload, &self.signing_context, self.index, &self.key)
	}
}
