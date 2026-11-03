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

//! Implements the `CandidateBackingSubsystem`.
//!
//! This subsystem maintains the entire responsibility of tracking parachain
//! candidates which can be backed, as well as the issuance of statements
//! about candidates when run on a validator node.
//!
//! There are two types of statements: `Seconded` and `Valid`.
//! `Seconded` implies `Valid`, and nothing should be stated as
//! `Valid` unless its already been `Seconded`.
//!
//! Validators may only second candidates which fall under their own group
//! assignment, and they may only second one candidate per depth per active leaf.
//! Candidates which are stated as either `Second` or `Valid` by a majority of the
//! assigned group of validators may be backed on-chain and proceed to the availability
//! stage.
//!
//! Depth is a concept relating to asynchronous backing, by which
//! short sub-chains of candidates are backed and extended off-chain, and then placed
//! asynchronously into blocks of the relay chain as those are authored and as the
//! relay-chain state becomes ready for them. Asynchronous backing allows parachains to
//! grow mostly independently from the state of the relay chain, which gives more time for
//! parachains to be validated and thereby increases performance.
//!
//! Most of the work of asynchronous backing is handled by the Prospective Parachains
//! subsystem. The 'depth' of a parachain block with respect to a relay chain block is
//! a measure of how many parachain blocks are between the most recent included parachain block
//! in the post-state of the relay-chain block and the candidate. For instance,
//! a candidate that descends directly from the most recent parachain block in the relay-chain
//! state has depth 0. The child of that candidate would have depth 1. And so on.
//!
//! The candidate backing subsystem keeps track of a set of 'active leaves' which are the
//! most recent blocks in the relay-chain (which is in fact a tree) which could be built
//! upon. Depth is always measured against active leaves, and the valid relay-parent that
//! each candidate can have is determined by the active leaves. The Prospective Parachains
//! subsystem enforces that the relay-parent increases monotonically, so that logic
//! is not handled here. By communicating with the Prospective Parachains subsystem,
//! this subsystem extrapolates an "implicit view" from the set of currently active leaves,
//! which determines the set of all recent relay-chain block hashes which could be relay-parents
//! for candidates backed in children of the active leaves.
//!
//! In fact, this subsystem relies on the Statement Distribution subsystem to prevent spam
//! by enforcing the rule that each validator may second at most one candidate per depth per
//! active leaf. This bounds the number of candidates that the system needs to consider and
//! is not handled within this subsystem, except for candidates seconded locally.
//!
//! This subsystem also handles relay-chain heads which don't support asynchronous backing.
//! For such active leaves, the only valid relay-parent is the leaf hash itself and the only
//! allowed depth is 0.

#![deny(unused_crate_dependencies)]

use std::{
	collections::{HashMap, HashSet},
	sync::Arc,
};

use bitvec::vec::BitVec;
use futures::{
	channel::{mpsc, oneshot},
	future::BoxFuture,
	stream::FuturesOrdered,
	FutureExt, SinkExt, StreamExt, TryFutureExt,
};
use schnellru::{ByLength, LruMap};

use error::{Error, FatalResult};
use polkadot_node_primitives::{
	AvailableData, InvalidCandidate, PoV, SignedFullStatementWithPVD, StatementWithPVD,
	ValidationResult, DISPUTE_WINDOW,
};
use polkadot_node_subsystem::{
	messages::{
		AvailabilityDistributionMessage, AvailabilityStoreMessage, BackableCandidateRef,
		CanSecondRequest, CandidateBackingMessage, CandidateValidationMessage,
		CollatorProtocolMessage, HypotheticalCandidate, HypotheticalMembershipRequest,
		IntroduceSecondedCandidateRequest, ProspectiveParachainsMessage, ProvisionableData,
		ProvisionerMessage, PvfExecKind, RuntimeApiMessage, RuntimeApiRequest,
		StatementDistributionMessage, StoreAvailableDataError,
	},
	overseer, ActiveLeavesUpdate, FromOrchestra, OverseerSignal, RuntimeApiError, SpawnedSubsystem,
	SubsystemError,
};
use polkadot_node_subsystem_util::{
	self as util,
	backing_implicit_view::View as ImplicitView,
	request_claim_queue, request_disabled_validators, request_min_backing_votes,
	request_node_features, request_session_index_for_child, request_validator_groups,
	request_validators,
	runtime::{self, ClaimQueueSnapshot},
	Validator,
};
use polkadot_parachain_primitives::primitives::IsSystem;
use polkadot_primitives::{
	node_features::FeatureIndex, BackedCandidate, CandidateCommitments, CandidateHash,
	CandidateReceiptV2 as CandidateReceipt,
	CommittedCandidateReceiptV2 as CommittedCandidateReceipt, CoreIndex, GroupIndex,
	GroupRotationInfo, Hash, Id as ParaId, IndexedVec, NodeFeatures, PersistedValidationData,
	SessionIndex, SigningContext, ValidationCode, ValidatorId, ValidatorIndex, ValidatorSignature,
	ValidityAttestation,
};
use polkadot_statement_table::{
	generic::AttestedCandidate as TableAttestedCandidate,
	v2::{
		SignedStatement as TableSignedStatement, Statement as TableStatement,
		Summary as TableSummary,
	},
	Context as TableContextTrait, Table,
};
use sp_keystore::KeystorePtr;

mod error;

mod metrics;
use self::metrics::Metrics;

#[cfg(test)]
mod tests;

const LOG_TARGET: &str = "parachain::candidate-backing";

/// PoV data to validate.
enum PoVData {
	/// Already available (from candidate selection).
	Ready(Arc<PoV>),
	/// Needs to be fetched from validator (we are checking a signed statement).
	FetchFromValidator {
		from_validator: ValidatorIndex,
		candidate_hash: CandidateHash,
		pov_hash: Hash,
	},
}

enum ValidatedCandidateCommand {
	// We were instructed to second the candidate that has been already validated.
	Second(BackgroundValidationResult),
	// We were instructed to validate the candidate.
	Attest(BackgroundValidationResult),
	// We were not able to `Attest` because backing validator did not send us the PoV.
	AttestNoPoV(CandidateHash),
}

impl std::fmt::Debug for ValidatedCandidateCommand {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		let candidate_hash = self.candidate_hash();
		match *self {
			ValidatedCandidateCommand::Second(_) => write!(f, "Second({})", candidate_hash),
			ValidatedCandidateCommand::Attest(_) => write!(f, "Attest({})", candidate_hash),
			ValidatedCandidateCommand::AttestNoPoV(_) => write!(f, "Attest({})", candidate_hash),
		}
	}
}

impl ValidatedCandidateCommand {
	fn candidate_hash(&self) -> CandidateHash {
		match *self {
			ValidatedCandidateCommand::Second(Ok(ref outputs)) => outputs.candidate.hash(),
			ValidatedCandidateCommand::Second(Err(ref candidate)) => candidate.hash(),
			ValidatedCandidateCommand::Attest(Ok(ref outputs)) => outputs.candidate.hash(),
			ValidatedCandidateCommand::Attest(Err(ref candidate)) => candidate.hash(),
			ValidatedCandidateCommand::AttestNoPoV(candidate_hash) => candidate_hash,
		}
	}
}

/// The candidate backing subsystem.
pub struct CandidateBackingSubsystem {
	keystore: KeystorePtr,
	metrics: Metrics,
}

impl CandidateBackingSubsystem {
	/// Create a new instance of the `CandidateBackingSubsystem`.
	pub fn new(keystore: KeystorePtr, metrics: Metrics) -> Self {
		Self { keystore, metrics }
	}
}

#[overseer::subsystem(CandidateBacking, error = SubsystemError, prefix = self::overseer)]
impl<Context> CandidateBackingSubsystem
where
	Context: Send + Sync,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = async move {
			run(ctx, self.keystore, self.metrics)
				.await
				.map_err(|e| SubsystemError::with_origin("candidate-backing", e))
		}
		.boxed();

		SpawnedSubsystem { name: "candidate-backing-subsystem", future }
	}
}

struct PerSchedulingParentState {
	/// The hash of the scheduling parent on top of which this job is doing it's work.
	parent: Hash,
	/// The node features.
	node_features: NodeFeatures,
	/// The `CoreIndex` assigned to the local validator at this scheduling parent.
	assigned_core: Option<CoreIndex>,
	/// The candidates that are backed by enough validators in their group, by hash.
	backed: HashSet<CandidateHash>,
	/// The table of candidates and statements under this relay-parent.
	table: Table<TableContext>,
	/// The table context, including groups.
	table_context: TableContext,
	/// We issued `Seconded` or `Valid` statements on about these candidates.
	issued_statements: HashSet<CandidateHash>,
	/// These candidates are undergoing validation in the background.
	awaiting_validation: HashSet<CandidateHash>,
	/// Data needed for retrying in case of `ValidatedCandidateCommand::AttestNoPoV`.
	fallbacks: HashMap<CandidateHash, AttestingData>,
	/// The minimum backing votes threshold.
	minimum_backing_votes: u32,
	/// The number of cores.
	n_cores: u32,
	/// Claim queue state. If the runtime API is not available, it'll be populated with info from
	/// availability cores.
	claim_queue: ClaimQueueSnapshot,
	/// The validator index -> group mapping at this scheduling parent.
	validator_to_group: Arc<IndexedVec<ValidatorIndex, Option<GroupIndex>>>,
	/// Session index for this scheduling parent. Passed to candidate-validation so it
	/// can fetch session-scoped params without a runtime call for V1 descriptors.
	session_index: SessionIndex,
	/// The associated group rotation information.
	group_rotation_info: GroupRotationInfo,
}

struct PerCandidateState {
	persisted_validation_data: PersistedValidationData,
	seconded_locally: bool,
	scheduling_parent: Hash,
}

/// A cache for storing data per-session to reduce repeated
/// runtime API calls and avoid redundant computations.
struct PerSessionCache {
	/// Cache for storing validators list, retrieved from the runtime.
	validators_cache: LruMap<SessionIndex, Arc<Vec<ValidatorId>>>,
	/// Cache for storing node features, retrieved from the runtime.
	node_features_cache: LruMap<SessionIndex, NodeFeatures>,
	/// Cache for storing the minimum backing votes threshold, retrieved from the runtime.
	minimum_backing_votes_cache: LruMap<SessionIndex, u32>,
	/// Cache for storing validator-to-group mappings, computed from validator groups.
	validator_to_group_cache:
		LruMap<SessionIndex, Arc<IndexedVec<ValidatorIndex, Option<GroupIndex>>>>,
}

impl Default for PerSessionCache {
	/// Creates a new `PerSessionCache` with a default capacity.
	// TODO: Use the actual session window for allowed relay parents once
	// https://github.com/paritytech/polkadot-sdk/issues/11207 is available,
	// instead of DISPUTE_WINDOW.
	fn default() -> Self {
		Self::new(DISPUTE_WINDOW.get())
	}
}

impl PerSessionCache {
	/// Creates a new `PerSessionCache` with a given capacity.
	fn new(capacity: u32) -> Self {
		PerSessionCache {
			validators_cache: LruMap::new(ByLength::new(capacity)),
			node_features_cache: LruMap::new(ByLength::new(capacity)),
			minimum_backing_votes_cache: LruMap::new(ByLength::new(capacity)),
			validator_to_group_cache: LruMap::new(ByLength::new(capacity)),
		}
	}

	/// Gets validators from the cache or fetches them from the runtime if not present.
	async fn validators(
		&mut self,
		session_index: SessionIndex,
		parent: Hash,
		sender: &mut impl overseer::SubsystemSender<RuntimeApiMessage>,
	) -> Result<Arc<Vec<ValidatorId>>, RuntimeApiError> {
		// Try to get the validators list from the cache.
		if let Some(validators) = self.validators_cache.get(&session_index) {
			return Ok(Arc::clone(validators));
		}

		// Fetch the validators list from the runtime since it was not in the cache.
		let validators: Vec<ValidatorId> =
			request_validators(parent, sender).await.await.map_err(|err| {
				RuntimeApiError::Execution { runtime_api_name: "Validators", source: Arc::new(err) }
			})??;

		// Wrap the validators list in an Arc to avoid a deep copy when storing it in the cache.
		let validators = Arc::new(validators);

		// Cache the fetched validators list for future use.
		self.validators_cache.insert(session_index, Arc::clone(&validators));

		Ok(validators)
	}

	/// Gets the node features from the cache or fetches it from the runtime if not present.
	async fn node_features(
		&mut self,
		session_index: SessionIndex,
		parent: Hash,
		sender: &mut impl overseer::SubsystemSender<RuntimeApiMessage>,
	) -> Result<NodeFeatures, RuntimeApiError> {
		// Try to get the node features from the cache.
		if let Some(node_features) = self.node_features_cache.get(&session_index) {
			return Ok(node_features.clone());
		}

		// Fetch the node features from the runtime since it was not in the cache.
		let node_features = request_node_features(parent, session_index, sender)
			.await
			.await
			.map_err(|err| RuntimeApiError::Execution {
				runtime_api_name: "NodeFeatures",
				source: Arc::new(err),
			})??;

		// Cache the fetched node features for future use.
		self.node_features_cache.insert(session_index, node_features.clone());

		Ok(node_features)
	}

	/// Gets the minimum backing votes threshold from the
	/// cache or fetches it from the runtime if not present.
	async fn minimum_backing_votes(
		&mut self,
		session_index: SessionIndex,
		parent: Hash,
		sender: &mut impl overseer::SubsystemSender<RuntimeApiMessage>,
	) -> Result<u32, RuntimeApiError> {
		// Try to get the value from the cache.
		if let Some(minimum_backing_votes) = self.minimum_backing_votes_cache.get(&session_index) {
			return Ok(*minimum_backing_votes);
		}

		// Fetch the value from the runtime since it was not in the cache.
		let minimum_backing_votes = request_min_backing_votes(parent, session_index, sender)
			.await
			.await
			.map_err(|err| RuntimeApiError::Execution {
				runtime_api_name: "MinimumBackingVotes",
				source: Arc::new(err),
			})??;

		// Cache the fetched value for future use.
		self.minimum_backing_votes_cache.insert(session_index, minimum_backing_votes);

		Ok(minimum_backing_votes)
	}

	/// Gets or computes the validator-to-group mapping for a session.
	fn validator_to_group(
		&mut self,
		session_index: SessionIndex,
		validators: &[ValidatorId],
		validator_groups: &[Vec<ValidatorIndex>],
	) -> Arc<IndexedVec<ValidatorIndex, Option<GroupIndex>>> {
		let validator_to_group = self
			.validator_to_group_cache
			.get_or_insert(session_index, || {
				let mut vector = vec![None; validators.len()];

				for (group_idx, validator_group) in validator_groups.iter().enumerate() {
					for validator in validator_group {
						vector[validator.0 as usize] = Some(GroupIndex(group_idx as u32));
					}
				}

				Arc::new(IndexedVec::<_, _>::from(vector))
			})
			.expect("Just inserted");

		Arc::clone(validator_to_group)
	}
}

/// The state of the subsystem.
struct State {
	/// The utility for managing the implicit and explicit views in a consistent way.
	implicit_view: ImplicitView,
	/// State tracked for all scheduling-parents backing work is ongoing for. This includes
	/// all active leaves.
	per_scheduling_parent: HashMap<Hash, PerSchedulingParentState>,
	/// State tracked for all candidates relevant to the implicit view.
	///
	/// This is guaranteed to have an entry for each candidate with a scheduling parent in the
	/// implicit or explicit view for which a `Seconded` statement has been successfully imported.
	per_candidate: HashMap<CandidateHash, PerCandidateState>,
	/// A local cache for storing per-session data. This cache helps to
	/// reduce repeated calls to the runtime and avoid redundant computations.
	per_session_cache: PerSessionCache,
	/// A clonable sender which is dispatched to background candidate validation tasks to inform
	/// the main task of the result.
	background_validation_tx: mpsc::Sender<(Hash, ValidatedCandidateCommand)>,
	/// The handle to the keystore used for signing.
	keystore: KeystorePtr,
	/// Monotonic flag: set to `true` once a **finalized** block is observed whose
	/// session has the `CandidateReceiptV3` node feature enabled. Once set, never
	/// unset. Used for V3 gating checks in backing — if V3 was never seen, reject
	/// V3 candidates and candidates where old/new version detection disagrees.
	///
	/// Backing uses finalized blocks (rather than any active leaf) to ensure that
	/// new backers do not start producing V3 candidates before a supermajority of
	/// validators (including approval checkers) are aware of the feature. Approval
	/// and dispute validation use active leaves instead, so they always transition
	/// *before* backing does — the safe direction.
	v3_ever_seen: bool,
}

impl State {
	fn new(
		background_validation_tx: mpsc::Sender<(Hash, ValidatedCandidateCommand)>,
		keystore: KeystorePtr,
	) -> Self {
		State {
			implicit_view: ImplicitView::default(),
			per_scheduling_parent: HashMap::default(),
			per_candidate: HashMap::new(),
			per_session_cache: PerSessionCache::default(),
			background_validation_tx,
			keystore,
			v3_ever_seen: false,
		}
	}
}

#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn run<Context>(
	mut ctx: Context,
	keystore: KeystorePtr,
	metrics: Metrics,
) -> FatalResult<()> {
	let (background_validation_tx, mut background_validation_rx) = mpsc::channel(16);
	let mut state = State::new(background_validation_tx, keystore);

	loop {
		let res =
			run_iteration(&mut ctx, &mut state, &metrics, &mut background_validation_rx).await;

		match res {
			Ok(()) => break,
			Err(e) => crate::error::log_error(Err(e))?,
		}
	}

	Ok(())
}

#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn run_iteration<Context>(
	ctx: &mut Context,
	state: &mut State,
	metrics: &Metrics,
	background_validation_rx: &mut mpsc::Receiver<(Hash, ValidatedCandidateCommand)>,
) -> Result<(), Error> {
	loop {
		futures::select!(
			validated_command = background_validation_rx.next().fuse() => {
				if let Some((scheduling_parent, command)) = validated_command {
					handle_validated_candidate_command(
						&mut *ctx,
						state,
						scheduling_parent,
						command,
						metrics,
					).await?;
				} else {
					panic!("background_validation_tx always alive at this point; qed");
				}
			}
			from_overseer = ctx.recv().fuse() => {
				match from_overseer.map_err(Error::OverseerExited)? {
					FromOrchestra::Signal(OverseerSignal::ActiveLeaves(update)) => {
						handle_active_leaves_update(
							&mut *ctx,
							update,
							state,
						).await?;
					}
					FromOrchestra::Signal(OverseerSignal::BlockFinalized(hash, _number)) => {
						if !state.v3_ever_seen {
							check_v3_on_finalized(&mut *ctx, state, hash).await?;
						}
					}
					FromOrchestra::Signal(OverseerSignal::Conclude) => return Ok(()),
					FromOrchestra::Communication { msg } => {
						handle_communication(&mut *ctx, state, msg, metrics).await?;
					}
				}
			}
		)
	}
}

/// In case a backing validator does not provide a PoV, we need to retry with other backing
/// validators.
///
/// This is the data needed to accomplish this. Basically all the data needed for spawning a
/// validation job and a list of backing validators, we can try.
#[derive(Clone)]
struct AttestingData {
	/// The candidate to attest.
	candidate: CandidateReceipt,
	/// Hash of the PoV we need to fetch.
	pov_hash: Hash,
	/// Validator we are currently trying to get the PoV from.
	from_validator: ValidatorIndex,
	/// Other backing validators we can try in case `from_validator` failed.
	backing: Vec<ValidatorIndex>,
}

#[derive(Default, Debug)]
struct TableContext {
	validator: Option<Validator>,
	groups: HashMap<CoreIndex, Vec<ValidatorIndex>>,
	validators: Vec<ValidatorId>,
	disabled_validators: Vec<ValidatorIndex>,
}

impl TableContext {
	// Returns `true` if the provided `ValidatorIndex` is in the disabled validators list
	pub fn validator_is_disabled(&self, validator_idx: &ValidatorIndex) -> bool {
		self.disabled_validators
			.iter()
			.any(|disabled_val_idx| *disabled_val_idx == *validator_idx)
	}

	// Returns `true` if the local validator is in the disabled validators list
	pub fn local_validator_is_disabled(&self) -> Option<bool> {
		self.validator.as_ref().map(|v| v.disabled())
	}
}

impl TableContextTrait for TableContext {
	type AuthorityId = ValidatorIndex;
	type Digest = CandidateHash;
	type GroupId = CoreIndex;
	type Signature = ValidatorSignature;
	type Candidate = CommittedCandidateReceipt;

	fn candidate_digest(candidate: &CommittedCandidateReceipt) -> CandidateHash {
		candidate.hash()
	}

	fn is_member_of(&self, authority: &ValidatorIndex, core: &CoreIndex) -> bool {
		self.groups.get(core).map_or(false, |g| g.iter().any(|a| a == authority))
	}

	fn get_group_size(&self, group: &CoreIndex) -> Option<usize> {
		self.groups.get(group).map(|g| g.len())
	}
}

// It looks like it's not possible to do an `impl From` given the current state of
// the code. So this does the necessary conversion.
fn primitive_statement_to_table(s: &SignedFullStatementWithPVD) -> TableSignedStatement {
	let statement = match s.payload() {
		StatementWithPVD::Seconded(c, _) => TableStatement::Seconded(c.clone()),
		StatementWithPVD::Valid(h) => TableStatement::Valid(*h),
	};

	TableSignedStatement {
		statement,
		signature: s.signature().clone(),
		sender: s.validator_index(),
	}
}

fn table_attested_to_backed(
	attested: TableAttestedCandidate<
		CoreIndex,
		CommittedCandidateReceipt,
		ValidatorIndex,
		ValidatorSignature,
	>,
	table_context: &TableContext,
) -> Option<BackedCandidate> {
	let TableAttestedCandidate { candidate, validity_votes, group_id: core_index } = attested;

	let (ids, validity_votes): (Vec<_>, Vec<ValidityAttestation>) =
		validity_votes.into_iter().map(|(id, vote)| (id, vote.into())).unzip();

	let group = table_context.groups.get(&core_index)?;

	let mut validator_indices = BitVec::with_capacity(group.len());

	validator_indices.resize(group.len(), false);

	// The order of the validity votes in the backed candidate must match
	// the order of bits set in the bitfield, which is not necessarily
	// the order of the `validity_votes` we got from the table.
	let mut vote_positions = Vec::with_capacity(validity_votes.len());
	for (orig_idx, id) in ids.iter().enumerate() {
		if let Some(position) = group.iter().position(|x| x == id) {
			validator_indices.set(position, true);
			vote_positions.push((orig_idx, position));
		} else {
			gum::warn!(
				target: LOG_TARGET,
				"Logic error: Validity vote from table does not correspond to group",
			);

			return None;
		}
	}
	vote_positions.sort_by_key(|(_orig, pos_in_group)| *pos_in_group);

	Some(BackedCandidate::new(
		candidate,
		vote_positions
			.into_iter()
			.map(|(pos_in_votes, _pos_in_group)| validity_votes[pos_in_votes].clone())
			.collect(),
		validator_indices,
		core_index,
	))
}

async fn store_available_data(
	sender: &mut impl overseer::CandidateBackingSenderTrait,
	n_validators: u32,
	candidate_hash: CandidateHash,
	available_data: AvailableData,
	expected_erasure_root: Hash,
	core_index: CoreIndex,
	node_features: NodeFeatures,
) -> Result<(), Error> {
	let (tx, rx) = oneshot::channel();
	// Important: the `av-store` subsystem will check if the erasure root of the `available_data`
	// matches `expected_erasure_root` which was provided by the collator in the `CandidateReceipt`.
	// This check is consensus critical and the `backing` subsystem relies on it for ensuring
	// candidate validity.
	sender
		.send_message(AvailabilityStoreMessage::StoreAvailableData {
			candidate_hash,
			n_validators,
			available_data,
			expected_erasure_root,
			core_index,
			node_features,
			tx,
		})
		.await;

	rx.await
		.map_err(Error::StoreAvailableDataChannel)?
		.map_err(Error::StoreAvailableData)
}

// Make a `PoV` available.
//
// This calls the AV store to write the available data to storage. The AV store also checks the
// erasure root matches the `expected_erasure_root`.
// This returns `Err()` on erasure root mismatch or due to any AV store subsystem error.
//
// Otherwise, it returns `Ok(())`.
async fn make_pov_available(
	sender: &mut impl overseer::CandidateBackingSenderTrait,
	n_validators: usize,
	pov: Arc<PoV>,
	candidate_hash: CandidateHash,
	validation_data: PersistedValidationData,
	expected_erasure_root: Hash,
	core_index: CoreIndex,
	node_features: NodeFeatures,
) -> Result<(), Error> {
	store_available_data(
		sender,
		n_validators as u32,
		candidate_hash,
		AvailableData { pov, validation_data },
		expected_erasure_root,
		core_index,
		node_features,
	)
	.await
}

async fn request_pov(
	sender: &mut impl overseer::CandidateBackingSenderTrait,
	relay_parent: Hash,
	from_validator: ValidatorIndex,
	para_id: ParaId,
	candidate_hash: CandidateHash,
	pov_hash: Hash,
) -> Result<Arc<PoV>, Error> {
	let (tx, rx) = oneshot::channel();
	sender
		.send_message(AvailabilityDistributionMessage::FetchPoV {
			relay_parent,
			from_validator,
			para_id,
			candidate_hash,
			pov_hash,
			tx,
		})
		.await;

	let pov = rx.await.map_err(|_| Error::FetchPoV)?;
	Ok(Arc::new(pov))
}

async fn request_candidate_validation(
	sender: &mut impl overseer::CandidateBackingSenderTrait,
	validation_data: PersistedValidationData,
	validation_code: ValidationCode,
	candidate_receipt: CandidateReceipt,
	pov: Arc<PoV>,
	session_index: SessionIndex,
) -> Result<ValidationResult, Error> {
	let (tx, rx) = oneshot::channel();
	let is_system = candidate_receipt.descriptor.para_id().is_system();
	// PVF execution uses this for pruning obsolete jobs - we need the scheduling parent here:
	let scheduling_parent = candidate_receipt.descriptor.scheduling_parent();

	sender
		.send_message(CandidateValidationMessage::ValidateFromExhaustive {
			validation_data,
			validation_code,
			candidate_receipt,
			pov,
			scheduling_session_index: session_index,
			exec_kind: if is_system {
				PvfExecKind::BackingSystemParas(scheduling_parent)
			} else {
				PvfExecKind::Backing(scheduling_parent)
			},
			response_sender: tx,
		})
		.await;

	match rx.await {
		Ok(Ok(validation_result)) => Ok(validation_result),
		Ok(Err(err)) => Err(Error::ValidationFailed(err)),
		Err(err) => Err(Error::ValidateFromExhaustive(err)),
	}
}

struct BackgroundValidationOutputs {
	candidate: CandidateReceipt,
	commitments: CandidateCommitments,
	persisted_validation_data: PersistedValidationData,
}

type BackgroundValidationResult = Result<BackgroundValidationOutputs, CandidateReceipt>;

struct BackgroundValidationParams<S: overseer::CandidateBackingSenderTrait, F> {
	sender: S,
	tx_command: mpsc::Sender<(Hash, ValidatedCandidateCommand)>,
	candidate: CandidateReceipt,
	/// The scheduling parent hash. Used as context for runtime API queries and as
	/// the key for `per_scheduling_parent` lookup when sending results back.
	/// For V1/V2, this equals the candidate's relay_parent.
	scheduling_parent: Hash,
	session_index: SessionIndex,
	node_features: NodeFeatures,
	persisted_validation_data: PersistedValidationData,
	pov: PoVData,
	n_validators: usize,
	make_command: F,
}

async fn validate_and_make_available(
	params: BackgroundValidationParams<
		impl overseer::CandidateBackingSenderTrait,
		impl Fn(BackgroundValidationResult) -> ValidatedCandidateCommand + Sync,
	>,
	core_index: CoreIndex,
) -> Result<(), Error> {
	let BackgroundValidationParams {
		mut sender,
		mut tx_command,
		candidate,
		scheduling_parent,
		session_index,
		node_features,
		persisted_validation_data,
		pov,
		n_validators,
		make_command,
	} = params;

	let validation_code = {
		let validation_code_hash = candidate.descriptor().validation_code_hash();
		let (tx, rx) = oneshot::channel();
		sender
			.send_message(RuntimeApiMessage::Request(
				scheduling_parent,
				RuntimeApiRequest::ValidationCodeByHash(validation_code_hash, tx),
			))
			.await;

		let code = rx.await.map_err(Error::RuntimeApiUnavailable)?;
		match code {
			Err(e) => return Err(Error::FetchValidationCode(validation_code_hash, e)),
			Ok(None) => return Err(Error::NoValidationCode(validation_code_hash)),
			Ok(Some(c)) => c,
		}
	};

	let pov = match pov {
		PoVData::Ready(pov) => pov,
		PoVData::FetchFromValidator { from_validator, candidate_hash, pov_hash } => {
			match request_pov(
				&mut sender,
				scheduling_parent,
				from_validator,
				candidate.descriptor.para_id(),
				candidate_hash,
				pov_hash,
			)
			.await
			{
				Err(Error::FetchPoV) => {
					tx_command
						.send((
							scheduling_parent,
							ValidatedCandidateCommand::AttestNoPoV(candidate.hash()),
						))
						.await
						.map_err(Error::BackgroundValidationMpsc)?;
					return Ok(());
				},
				Err(err) => return Err(err),
				Ok(pov) => pov,
			}
		},
	};

	let v = {
		request_candidate_validation(
			&mut sender,
			persisted_validation_data,
			validation_code,
			candidate.clone(),
			pov.clone(),
			session_index,
		)
		.await?
	};

	let res = match v {
		ValidationResult::Valid(commitments, validation_data) => {
			gum::debug!(
				target: LOG_TARGET,
				candidate_hash = ?candidate.hash(),
				"Validation successful",
			);

			let erasure_valid = make_pov_available(
				&mut sender,
				n_validators,
				pov.clone(),
				candidate.hash(),
				validation_data.clone(),
				candidate.descriptor.erasure_root(),
				core_index,
				node_features,
			)
			.await;

			match erasure_valid {
				Ok(()) => Ok(BackgroundValidationOutputs {
					candidate,
					commitments,
					persisted_validation_data: validation_data,
				}),
				Err(Error::StoreAvailableData(StoreAvailableDataError::InvalidErasureRoot)) => {
					gum::debug!(
						target: LOG_TARGET,
						candidate_hash = ?candidate.hash(),
						actual_commitments = ?commitments,
						"Erasure root doesn't match the announced by the candidate receipt",
					);
					Err(candidate)
				},
				// Bubble up any other error.
				Err(e) => return Err(e),
			}
		},
		ValidationResult::Invalid(InvalidCandidate::CommitmentsHashMismatch) => {
			// If validation produces a new set of commitments, we vote the candidate as invalid.
			gum::warn!(
				target: LOG_TARGET,
				candidate_hash = ?candidate.hash(),
				"Validation yielded different commitments",
			);
			Err(candidate)
		},
		ValidationResult::Invalid(reason) => {
			gum::warn!(
				target: LOG_TARGET,
				candidate_hash = ?candidate.hash(),
				reason = ?reason,
				"Validation yielded an invalid candidate",
			);
			Err(candidate)
		},
	};

	tx_command
		.send((scheduling_parent, make_command(res)))
		.await
		.map_err(Into::into)
}

#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn handle_communication<Context>(
	ctx: &mut Context,
	state: &mut State,
	message: CandidateBackingMessage,
	metrics: &Metrics,
) -> Result<(), Error> {
	match message {
		CandidateBackingMessage::Second { scheduling_parent: _, candidate, pvd, pov } => {
			handle_second_message(ctx, state, candidate, pvd, pov, metrics).await?;
		},
		CandidateBackingMessage::Statement { scheduling_parent, statement } => {
			handle_statement_message(ctx, state, scheduling_parent, statement, metrics).await?;
		},
		CandidateBackingMessage::GetBackableCandidates { candidates, sender } => {
			handle_get_backable_candidates_message(state, candidates, sender, metrics)?
		},
		CandidateBackingMessage::CanSecond(request, tx) => {
			handle_can_second_request(ctx, state, request, tx).await
		},
	}

	Ok(())
}

#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn handle_active_leaves_update<Context>(
	ctx: &mut Context,
	update: ActiveLeavesUpdate,
	state: &mut State,
) -> Result<(), Error> {
	// Activate in implicit view before deactivate, per the docs
	// on ImplicitView, this is more efficient.
	let res = if let Some(leaf) = update.activated {
		let leaf_hash = leaf.hash;
		Some((leaf, state.implicit_view.activate_leaf(ctx.sender(), leaf_hash).await.map(|_| ())))
	} else {
		None
	};

	for deactivated in update.deactivated {
		state.implicit_view.deactivate_leaf(deactivated);
	}

	// clean up `per_scheduling_parent` according to ancestry
	// of leaves. we do this so we can clean up candidates right after
	// as a result.
	{
		let remaining: HashSet<_> = state.implicit_view.all_allowed_relay_parents().collect();

		state.per_scheduling_parent.retain(|r, _| remaining.contains(&r));
	}

	// clean up `per_candidate` according to which relay-parents
	// are known.
	state
		.per_candidate
		.retain(|_, pc| state.per_scheduling_parent.contains_key(&pc.scheduling_parent));

	// Get relay parents which might be fresh but might be known already
	// that are explicit or implicit from the new active leaf.
	let fresh_relay_parents = match res {
		None => return Ok(()),
		Some((leaf, Ok(_))) => {
			let fresh_relay_parents =
				state.implicit_view.known_allowed_relay_parents_under(&leaf.hash);

			let fresh_relay_parent = match fresh_relay_parents {
				Some(f) => f.to_vec(),
				None => {
					gum::warn!(
						target: LOG_TARGET,
						leaf_hash = ?leaf.hash,
						"Implicit view gave no relay-parents"
					);

					vec![leaf.hash]
				},
			};
			fresh_relay_parent
		},
		Some((leaf, Err(e))) => {
			gum::debug!(
				target: LOG_TARGET,
				leaf_hash = ?leaf.hash,
				err = ?e,
				"Failed to load implicit view for leaf."
			);

			return Ok(());
		},
	};

	// add entries in `per_scheduling_parent`. for all new relay-parents.
	for maybe_new in fresh_relay_parents {
		if state.per_scheduling_parent.contains_key(&maybe_new) {
			continue;
		}

		// construct a `PerSchedulingParent` from the runtime API
		// and insert it.
		let per = construct_per_scheduling_parent_state(
			ctx,
			maybe_new,
			&state.keystore,
			&mut state.per_session_cache,
		)
		.await?;

		if let Some(per) = per {
			state.per_scheduling_parent.insert(maybe_new, per);
		}
	}

	Ok(())
}

/// Check whether the `CandidateReceiptV3` node feature is enabled at the session
/// of the given finalized block. Sets `state.v3_ever_seen` if so.
#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn check_v3_on_finalized<Context>(
	ctx: &mut Context,
	state: &mut State,
	finalized_hash: Hash,
) -> Result<(), Error> {
	let session_index = request_session_index_for_child(finalized_hash, ctx.sender())
		.await
		.await
		.map_err(runtime::Error::from)?
		.map_err(runtime::Error::from)?;

	let node_features = state
		.per_session_cache
		.node_features(session_index, finalized_hash, ctx.sender())
		.await
		.map_err(runtime::Error::from)?;

	if FeatureIndex::CandidateReceiptV3.is_set(&node_features) {
		gum::info!(
			target: LOG_TARGET,
			?session_index,
			"CandidateReceiptV3 node feature detected in finalized block, \
		 enabling V3 candidate support",
		);
		state.v3_ever_seen = true;
	}

	Ok(())
}

macro_rules! try_runtime_api {
	($x: expr) => {
		match $x {
			Ok(x) => x,
			Err(err) => {
				// Only bubble up fatal errors.
				error::log_error(Err(Into::<runtime::Error>::into(err).into()))?;

				// We can't do candidate validation work if we don't have the
				// requisite runtime API data. But these errors should not take
				// down the node.
				return Ok(None);
			},
		}
	};
}

fn core_index_from_statement(
	sp_state: &PerSchedulingParentState,
	statement: &SignedFullStatementWithPVD,
) -> Option<CoreIndex> {
	let compact_statement = statement.as_unchecked();
	let candidate_hash = CandidateHash(*compact_statement.unchecked_payload().candidate_hash());

	gum::trace!(
		target: LOG_TARGET,
		group_rotation_info = ?sp_state.group_rotation_info,
		?statement,
		validator_to_group = ?sp_state.validator_to_group,
		n_cores = sp_state.n_cores,
		?candidate_hash,
		"Extracting core index from statement"
	);

	let statement_validator_index = statement.validator_index();
	let Some(Some(group_index)) = sp_state.validator_to_group.get(statement_validator_index) else {
		gum::debug!(
			target: LOG_TARGET,
			group_rotation_info = ?sp_state.group_rotation_info,
			?statement,
			validator_to_group = ?sp_state.validator_to_group,
			n_cores = sp_state.n_cores,
			?candidate_hash,
			"Invalid validator index: {:?}",
			statement_validator_index
		);
		return None;
	};

	// First check if the statement para id matches the core assignment.
	let core_index =
		sp_state.group_rotation_info.core_for_group(*group_index, sp_state.n_cores as _);

	if core_index.0 > sp_state.n_cores {
		gum::warn!(target: LOG_TARGET, ?candidate_hash, ?core_index, n_cores = sp_state.n_cores, "Invalid CoreIndex");
		return None;
	}

	if let StatementWithPVD::Seconded(candidate, _pvd) = statement.payload() {
		let candidate_para_id = candidate.descriptor.para_id();
		let mut assigned_paras = sp_state.claim_queue.iter_claims_for_core(&core_index);

		if !assigned_paras.any(|id| id == &candidate_para_id) {
			gum::debug!(
				target: LOG_TARGET,
				?candidate_hash,
				?core_index,
				assigned_paras = ?sp_state.claim_queue.iter_claims_for_core(&core_index).collect::<Vec<_>>(),
				?candidate_para_id,
				"Invalid CoreIndex, core is not assigned to this para_id"
			);
			return None;
		}
		Some(core_index)
	} else {
		Some(core_index)
	}
}

/// Load the data necessary to do backing work on top of a relay-parent.
#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn construct_per_scheduling_parent_state<Context>(
	ctx: &mut Context,
	relay_parent: Hash,
	keystore: &KeystorePtr,
	per_session_cache: &mut PerSessionCache,
) -> Result<Option<PerSchedulingParentState>, Error> {
	let parent = relay_parent;

	let (session_index, groups, claim_queue, disabled_validators) = futures::try_join!(
		request_session_index_for_child(parent, ctx.sender()).await,
		request_validator_groups(parent, ctx.sender()).await,
		request_claim_queue(parent, ctx.sender()).await,
		request_disabled_validators(parent, ctx.sender()).await,
	)
	.map_err(Error::JoinMultiple)?;

	let session_index = try_runtime_api!(session_index);

	let validators = per_session_cache.validators(session_index, parent, ctx.sender()).await;
	let validators = try_runtime_api!(validators);

	let node_features = per_session_cache.node_features(session_index, parent, ctx.sender()).await;
	let node_features = try_runtime_api!(node_features);

	gum::debug!(target: LOG_TARGET, ?parent, "New state");

	let (validator_groups, group_rotation_info) = try_runtime_api!(groups);

	let minimum_backing_votes = per_session_cache
		.minimum_backing_votes(session_index, parent, ctx.sender())
		.await;
	let minimum_backing_votes = try_runtime_api!(minimum_backing_votes);
	let claim_queue = try_runtime_api!(claim_queue);
	let disabled_validators = try_runtime_api!(disabled_validators);

	let signing_context = SigningContext { parent_hash: parent, session_index };
	let validator = match Validator::construct(
		&validators,
		&disabled_validators,
		signing_context.clone(),
		keystore.clone(),
	) {
		Ok(v) => Some(v),
		Err(util::Error::NotAValidator) => None,
		Err(e) => {
			gum::warn!(
				target: LOG_TARGET,
				err = ?e,
				"Cannot participate in candidate backing",
			);

			return Ok(None);
		},
	};

	let n_cores = validator_groups.len();

	let mut groups = HashMap::<CoreIndex, Vec<ValidatorIndex>>::new();
	let mut assigned_core = None;

	for idx in 0..n_cores {
		let core_index = CoreIndex(idx as _);

		if !claim_queue.contains_key(&core_index) {
			continue;
		}

		let group_index = group_rotation_info.group_for_core(core_index, n_cores);
		if let Some(g) = validator_groups.get(group_index.0 as usize) {
			if validator.as_ref().map_or(false, |v| g.contains(&v.index())) {
				assigned_core = Some(core_index);
			}
			groups.insert(core_index, g.clone());
		}
	}
	gum::debug!(target: LOG_TARGET, ?groups, "TableContext");

	let validator_to_group =
		per_session_cache.validator_to_group(session_index, &validators, &validator_groups);

	let table_context =
		TableContext { validator, groups, validators: validators.to_vec(), disabled_validators };

	Ok(Some(PerSchedulingParentState {
		parent,
		node_features,
		assigned_core,
		backed: HashSet::new(),
		table: Table::new(),
		table_context,
		issued_statements: HashSet::new(),
		awaiting_validation: HashSet::new(),
		fallbacks: HashMap::new(),
		minimum_backing_votes,
		n_cores: validator_groups.len() as u32,
		claim_queue: ClaimQueueSnapshot::from(claim_queue),
		validator_to_group,
		session_index,
		group_rotation_info,
	}))
}

enum SecondingAllowed {
	No,
	// On which leaves is seconding allowed.
	Yes(Vec<Hash>),
}

/// Checks whether a candidate can be seconded based on its hypothetical membership in the fragment
/// chain.
#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn seconding_sanity_check<Context>(
	ctx: &mut Context,
	implicit_view: &ImplicitView,
	hypothetical_candidate: HypotheticalCandidate,
) -> SecondingAllowed {
	let mut leaves_for_seconding = Vec::new();
	let mut responses = FuturesOrdered::<BoxFuture<'_, Result<_, oneshot::Canceled>>>::new();

	// Scheduling context: the scheduling parent determines which leaves this candidate
	// can be seconded under (it must be in the allowed ancestry).
	let candidate_scheduling_parent = hypothetical_candidate.scheduling_parent();
	let candidate_hash = hypothetical_candidate.candidate_hash();

	for head in implicit_view.leaves() {
		// Check that the candidate scheduling parent is allowed under this leaf.
		let allowed_parents_for_para = implicit_view.known_allowed_relay_parents_under(head);
		if !allowed_parents_for_para
			.unwrap_or_default()
			.contains(&candidate_scheduling_parent)
		{
			continue;
		}

		let (tx, rx) = oneshot::channel();
		ctx.send_message(ProspectiveParachainsMessage::GetHypotheticalMembership(
			HypotheticalMembershipRequest {
				candidates: vec![hypothetical_candidate.clone()],
				fragment_chain_relay_parent: Some(*head),
			},
			tx,
		))
		.await;
		let response = rx.map_ok(move |candidate_memberships| {
			let is_member_or_potential = candidate_memberships
				.into_iter()
				.find_map(|(candidate, leaves)| {
					(candidate.candidate_hash() == candidate_hash).then_some(leaves)
				})
				.and_then(|leaves| leaves.into_iter().find(|leaf| leaf == head))
				.is_some();

			(is_member_or_potential, head)
		});
		responses.push_back(response.boxed());
	}

	if responses.is_empty() {
		return SecondingAllowed::No;
	}

	while let Some(response) = responses.next().await {
		match response {
			Err(oneshot::Canceled) => {
				gum::warn!(
					target: LOG_TARGET,
					"Failed to reach prospective parachains subsystem for hypothetical membership",
				);

				return SecondingAllowed::No;
			},
			Ok((is_member_or_potential, head)) => match is_member_or_potential {
				false => {
					gum::debug!(
						target: LOG_TARGET,
						?candidate_hash,
						leaf_hash = ?head,
						"Refusing to second candidate at leaf. Is not a potential member.",
					);
				},
				true => {
					leaves_for_seconding.push(*head);
				},
			},
		}
	}

	if leaves_for_seconding.is_empty() {
		SecondingAllowed::No
	} else {
		SecondingAllowed::Yes(leaves_for_seconding)
	}
}

/// Performs seconding sanity check for an advertisement.
#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn handle_can_second_request<Context>(
	ctx: &mut Context,
	state: &State,
	request: CanSecondRequest,
	tx: oneshot::Sender<bool>,
) {
	let scheduling_parent = request.candidate_scheduling_parent;
	let response = if state.per_scheduling_parent.get(&scheduling_parent).is_some() {
		let hypothetical_candidate = HypotheticalCandidate::Incomplete {
			candidate_hash: request.candidate_hash,
			candidate_para: request.candidate_para_id,
			parent_head_data_hash: request.parent_head_data_hash,
			candidate_scheduling_parent: scheduling_parent,
		};

		let result =
			seconding_sanity_check(ctx, &state.implicit_view, hypothetical_candidate).await;

		match result {
			SecondingAllowed::No => false,
			SecondingAllowed::Yes(leaves) => !leaves.is_empty(),
		}
	} else {
		// Relay parent is unknown or async backing is disabled.
		false
	};

	let _ = tx.send(response);
}

#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn handle_validated_candidate_command<Context>(
	ctx: &mut Context,
	state: &mut State,
	scheduling_parent: Hash,
	command: ValidatedCandidateCommand,
	metrics: &Metrics,
) -> Result<(), Error> {
	match state.per_scheduling_parent.get_mut(&scheduling_parent) {
		Some(sp_state) => {
			let candidate_hash = command.candidate_hash();
			sp_state.awaiting_validation.remove(&candidate_hash);

			match command {
				ValidatedCandidateCommand::Second(res) => match res {
					Ok(outputs) => {
						let BackgroundValidationOutputs {
							candidate,
							commitments,
							persisted_validation_data,
						} = outputs;

						if sp_state.issued_statements.contains(&candidate_hash) {
							return Ok(());
						}

						let receipt = CommittedCandidateReceipt {
							descriptor: candidate.descriptor.clone(),
							commitments,
						};

						let hypothetical_candidate = HypotheticalCandidate::Complete {
							candidate_hash,
							receipt: Arc::new(receipt.clone()),
							persisted_validation_data: persisted_validation_data.clone(),
						};
						// sanity check that we're allowed to second the candidate
						// and that it doesn't conflict with other candidates we've
						// seconded.
						if let SecondingAllowed::No = seconding_sanity_check(
							ctx,
							&state.implicit_view,
							hypothetical_candidate,
						)
						.await
						{
							return Ok(());
						};

						let statement =
							StatementWithPVD::Seconded(receipt, persisted_validation_data);

						// If we get an Error::RejectedByProspectiveParachains,
						// then the statement has not been distributed or imported into
						// the table.
						let res = sign_import_and_distribute_statement(
							ctx,
							sp_state,
							&mut state.per_candidate,
							statement,
							state.keystore.clone(),
							metrics,
						)
						.await;

						if let Err(Error::RejectedByProspectiveParachains) = res {
							let candidate_hash = candidate.hash();
							gum::debug!(
								target: LOG_TARGET,
								scheduling_parent = ?candidate.descriptor().scheduling_parent(),
								?candidate_hash,
								"Attempted to second candidate but was rejected by prospective parachains",
							);

							ctx.send_message(CollatorProtocolMessage::Invalid(
								candidate.descriptor().scheduling_parent(),
								candidate,
							))
							.await;

							return Ok(());
						}

						if let Some(stmt) = res? {
							match state.per_candidate.get_mut(&candidate_hash) {
								None => {
									gum::warn!(
										target: LOG_TARGET,
										?candidate_hash,
										"Missing `per_candidate` for seconded candidate.",
									);
								},
								Some(p) => p.seconded_locally = true,
							}

							sp_state.issued_statements.insert(candidate_hash);

							metrics.on_candidate_seconded();
							ctx.send_message(CollatorProtocolMessage::Seconded(
								sp_state.parent,
								StatementWithPVD::drop_pvd_from_signed(stmt),
							))
							.await;
						}
					},
					Err(candidate) => {
						ctx.send_message(CollatorProtocolMessage::Invalid(
							sp_state.parent,
							candidate,
						))
						.await;
					},
				},
				ValidatedCandidateCommand::Attest(res) => {
					// We are done - avoid new validation spawns:
					sp_state.fallbacks.remove(&candidate_hash);
					// sanity check.
					if !sp_state.issued_statements.contains(&candidate_hash) {
						if res.is_ok() {
							let statement = StatementWithPVD::Valid(candidate_hash);

							sign_import_and_distribute_statement(
								ctx,
								sp_state,
								&mut state.per_candidate,
								statement,
								state.keystore.clone(),
								metrics,
							)
							.await?;
						}
						sp_state.issued_statements.insert(candidate_hash);
					}
				},
				ValidatedCandidateCommand::AttestNoPoV(candidate_hash) => {
					if let Some(attesting) = sp_state.fallbacks.get_mut(&candidate_hash) {
						if let Some(index) = attesting.backing.pop() {
							attesting.from_validator = index;
							let attesting = attesting.clone();

							// The candidate state should be available because we've
							// validated it before, the relay-parent is still around,
							// and candidates are pruned on the basis of relay-parents.
							//
							// If it is not, then no point in validating it anyway.
							if let Some(pvd) = state
								.per_candidate
								.get(&candidate_hash)
								.map(|pc| pc.persisted_validation_data.clone())
							{
								kick_off_validation_work(
									ctx,
									sp_state,
									pvd,
									&state.background_validation_tx,
									attesting,
								)
								.await?;
							}
						}
					} else {
						gum::warn!(
							target: LOG_TARGET,
							"AttestNoPoV was triggered without fallback being available."
						);
						debug_assert!(false);
					}
				},
			}
		},
		None => {
			// simple race condition; can be ignored = this relay-parent
			// is no longer relevant.
		},
	}

	Ok(())
}

fn sign_statement(
	sp_state: &PerSchedulingParentState,
	statement: StatementWithPVD,
	keystore: KeystorePtr,
	metrics: &Metrics,
) -> Option<SignedFullStatementWithPVD> {
	let signed = sp_state
		.table_context
		.validator
		.as_ref()?
		.sign(keystore, statement)
		.ok()
		.flatten()?;
	metrics.on_statement_signed();
	Some(signed)
}

/// Import a statement into the statement table and return the summary of the import.
///
/// This will fail with `Error::RejectedByProspectiveParachains` if the message type is seconded,
/// the candidate is fresh, and any of the following are true:
/// 1. There is no `PersistedValidationData` attached.
/// 2. Prospective parachains subsystem returned an empty `HypotheticalMembership` i.e. did not
///    recognize the candidate as being applicable to any of the active leaves.
#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn import_statement<Context>(
	ctx: &mut Context,
	sp_state: &mut PerSchedulingParentState,
	per_candidate: &mut HashMap<CandidateHash, PerCandidateState>,
	statement: &SignedFullStatementWithPVD,
) -> Result<Option<TableSummary>, Error> {
	let candidate_hash = statement.payload().candidate_hash();

	gum::debug!(
		target: LOG_TARGET,
		statement = ?statement.payload().to_compact(),
		validator_index = statement.validator_index().0,
		?candidate_hash,
		"Importing statement",
	);

	// If this is a new candidate (statement is 'seconded' and candidate is unknown),
	// we need to create an entry in the `PerCandidateState` map.
	//
	// We also need to inform the prospective parachains subsystem of the seconded candidate.
	// If `ProspectiveParachainsMessage::Second` fails, then we return
	// Error::RejectedByProspectiveParachains.
	//
	// Persisted Validation Data should be available - it may already be available
	// if this is a candidate we are seconding.
	//
	// We should also not accept any candidates which have no valid depths under any of
	// our active leaves.
	if let StatementWithPVD::Seconded(candidate, pvd) = statement.payload() {
		if !per_candidate.contains_key(&candidate_hash) {
			let (tx, rx) = oneshot::channel();
			ctx.send_message(ProspectiveParachainsMessage::IntroduceSecondedCandidate(
				IntroduceSecondedCandidateRequest {
					candidate_para: candidate.descriptor.para_id(),
					candidate_receipt: candidate.clone(),
					persisted_validation_data: pvd.clone(),
				},
				tx,
			))
			.await;

			match rx.await {
				Err(oneshot::Canceled) => {
					gum::warn!(
						target: LOG_TARGET,
						"Could not reach the Prospective Parachains subsystem."
					);

					return Err(Error::RejectedByProspectiveParachains);
				},
				Ok(false) => return Err(Error::RejectedByProspectiveParachains),
				Ok(true) => {},
			}

			// Only save the candidate if it was approved by prospective parachains.
			per_candidate.insert(
				candidate_hash,
				PerCandidateState {
					persisted_validation_data: pvd.clone(),
					// This is set after importing when seconding locally.
					seconded_locally: false,
					// Scheduling context: used for cleanup against per_scheduling_parent.
					scheduling_parent: candidate.descriptor.scheduling_parent(),
				},
			);
		}
	}

	let stmt = primitive_statement_to_table(statement);

	let core = core_index_from_statement(sp_state, statement).ok_or(Error::CoreIndexUnavailable)?;

	Ok(sp_state.table.import_statement(&sp_state.table_context, core, stmt))
}

/// Handles a summary received from [`import_statement`] and dispatches `Backed` notifications and
/// misbehaviors as a result of importing a statement.
#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn post_import_statement_actions<Context>(
	ctx: &mut Context,
	sp_state: &mut PerSchedulingParentState,
	summary: Option<&TableSummary>,
) {
	if let Some(attested) = summary.as_ref().and_then(|s| {
		sp_state.table.attested_candidate(
			&s.candidate,
			&sp_state.table_context,
			sp_state.minimum_backing_votes,
		)
	}) {
		let candidate_hash = attested.candidate.hash();

		// `HashSet::insert` returns true if the thing wasn't in there already.
		if sp_state.backed.insert(candidate_hash) {
			if let Some(backed) = table_attested_to_backed(attested, &sp_state.table_context) {
				let para_id = backed.candidate().descriptor.para_id();
				gum::debug!(
					target: LOG_TARGET,
					candidate_hash = ?candidate_hash,
					scheduling_parent = ?sp_state.parent,
					%para_id,
					"Candidate backed",
				);

				// Inform the prospective parachains subsystem
				// that the candidate is now backed.
				ctx.send_message(ProspectiveParachainsMessage::CandidateBacked(
					para_id,
					candidate_hash,
				))
				.await;
				// Notify statement distribution of backed candidate.
				ctx.send_message(StatementDistributionMessage::Backed(candidate_hash)).await;
			} else {
				gum::debug!(target: LOG_TARGET, ?candidate_hash, "Cannot get BackedCandidate");
			}
		} else {
			gum::debug!(target: LOG_TARGET, ?candidate_hash, "Candidate already known");
		}
	} else {
		gum::debug!(target: LOG_TARGET, "No attested candidate");
	}

	issue_new_misbehaviors(ctx, sp_state.parent, &mut sp_state.table);
}

/// Check if there have happened any new misbehaviors and issue necessary messages.
#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
fn issue_new_misbehaviors<Context>(
	ctx: &mut Context,
	relay_parent: Hash,
	table: &mut Table<TableContext>,
) {
	// collect the misbehaviors to avoid double mutable self borrow issues
	let misbehaviors: Vec<_> = table.drain_misbehaviors().collect();
	for (validator_id, report) in misbehaviors {
		// The provisioner waits on candidate-backing, which means
		// that we need to send unbounded messages to avoid cycles.
		//
		// Misbehaviors are bounded by the number of validators and
		// the block production protocol.
		ctx.send_unbounded_message(ProvisionerMessage::ProvisionableData(
			relay_parent,
			ProvisionableData::MisbehaviorReport(relay_parent, validator_id, report),
		));
	}
}

/// Sign, import, and distribute a statement.
#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn sign_import_and_distribute_statement<Context>(
	ctx: &mut Context,
	sp_state: &mut PerSchedulingParentState,
	per_candidate: &mut HashMap<CandidateHash, PerCandidateState>,
	statement: StatementWithPVD,
	keystore: KeystorePtr,
	metrics: &Metrics,
) -> Result<Option<SignedFullStatementWithPVD>, Error> {
	if let Some(signed_statement) = sign_statement(&*sp_state, statement, keystore, metrics) {
		let summary = import_statement(ctx, sp_state, per_candidate, &signed_statement).await?;

		// `Share` must always be sent before `Backed`. We send the latter in
		// `post_import_statement_action` below.
		let smsg = StatementDistributionMessage::Share(sp_state.parent, signed_statement.clone());
		ctx.send_unbounded_message(smsg);

		post_import_statement_actions(ctx, sp_state, summary.as_ref()).await;

		Ok(Some(signed_statement))
	} else {
		Ok(None)
	}
}

#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn background_validate_and_make_available<Context>(
	ctx: &mut Context,
	sp_state: &mut PerSchedulingParentState,
	params: BackgroundValidationParams<
		impl overseer::CandidateBackingSenderTrait,
		impl Fn(BackgroundValidationResult) -> ValidatedCandidateCommand + Send + 'static + Sync,
	>,
) -> Result<(), Error> {
	let candidate_hash = params.candidate.hash();
	let Some(core_index) = sp_state.assigned_core else { return Ok(()) };
	if sp_state.awaiting_validation.insert(candidate_hash) {
		// spawn background task.
		let bg = async move {
			if let Err(error) = validate_and_make_available(params, core_index).await {
				if let Error::BackgroundValidationMpsc(error) = error {
					gum::debug!(
						target: LOG_TARGET,
						?candidate_hash,
						?error,
						"Mpsc background validation mpsc died during validation- leaf no longer active?"
					);
				} else {
					gum::error!(
						target: LOG_TARGET,
						?candidate_hash,
						?error,
						"Failed to validate and make available",
					);
				}
			}
		};

		ctx.spawn("backing-validation", bg.boxed())
			.map_err(|_| Error::FailedToSpawnBackgroundTask)?;
	}

	Ok(())
}

/// Kick off validation work and distribute the result as a signed statement.
#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn kick_off_validation_work<Context>(
	ctx: &mut Context,
	sp_state: &mut PerSchedulingParentState,
	persisted_validation_data: PersistedValidationData,
	background_validation_tx: &mpsc::Sender<(Hash, ValidatedCandidateCommand)>,
	attesting: AttestingData,
) -> Result<(), Error> {
	// Do nothing if the local validator is disabled or not a validator at all
	match sp_state.table_context.local_validator_is_disabled() {
		Some(true) => {
			gum::info!(target: LOG_TARGET, "We are disabled - don't kick off validation");
			return Ok(());
		},
		Some(false) => {}, // we are not disabled - move on
		None => {
			gum::debug!(target: LOG_TARGET, "We are not a validator - don't kick off validation");
			return Ok(());
		},
	}

	let candidate_hash = attesting.candidate.hash();
	if sp_state.issued_statements.contains(&candidate_hash) {
		return Ok(());
	}

	gum::debug!(
		target: LOG_TARGET,
		candidate_hash = ?candidate_hash,
		candidate_receipt = ?attesting.candidate,
		"Kicking off validation",
	);

	let bg_sender = ctx.sender().clone();
	let pov = PoVData::FetchFromValidator {
		from_validator: attesting.from_validator,
		candidate_hash,
		pov_hash: attesting.pov_hash,
	};
	let scheduling_parent = attesting.candidate.descriptor().scheduling_parent();

	background_validate_and_make_available(
		ctx,
		sp_state,
		BackgroundValidationParams {
			sender: bg_sender,
			tx_command: background_validation_tx.clone(),
			candidate: attesting.candidate,
			scheduling_parent,
			session_index: sp_state.session_index,
			node_features: sp_state.node_features.clone(),
			persisted_validation_data,
			pov,
			n_validators: sp_state.table_context.validators.len(),
			make_command: ValidatedCandidateCommand::Attest,
		},
	)
	.await
}

/// Import the statement and kick off validation work if it is a part of our assignment.
#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn maybe_validate_and_import<Context>(
	ctx: &mut Context,
	state: &mut State,
	scheduling_parent: Hash,
	statement: SignedFullStatementWithPVD,
) -> Result<(), Error> {
	let sp_state = match state.per_scheduling_parent.get_mut(&scheduling_parent) {
		Some(r) => r,
		None => {
			gum::trace!(
				target: LOG_TARGET,
				?scheduling_parent,
				"Received statement for unknown scheduling parent"
			);

			return Ok(());
		},
	};

	// Don't import statement if the sender is disabled
	if sp_state.table_context.validator_is_disabled(&statement.validator_index()) {
		gum::debug!(
			target: LOG_TARGET,
			sender_validator_idx = ?statement.validator_index(),
			"Not importing statement because the sender is disabled"
		);
		return Ok(());
	}

	// Version consistency + V3 gating for Seconded statements (shared logic).
	if let StatementWithPVD::Seconded(receipt, _) = statement.payload() {
		if let Err(reason) = receipt.descriptor.check_version_acceptance(state.v3_ever_seen) {
			gum::debug!(
				target: LOG_TARGET,
				?scheduling_parent,
				"Not importing Seconded statement: {}",
				reason,
			);
			return Ok(());
		}
	}

	let res = import_statement(ctx, sp_state, &mut state.per_candidate, &statement).await;

	// if we get an Error::RejectedByProspectiveParachains,
	// we will do nothing.
	if let Err(Error::RejectedByProspectiveParachains) = res {
		gum::debug!(
			target: LOG_TARGET,
			?scheduling_parent,
			"Statement rejected by prospective parachains."
		);

		return Ok(());
	}

	let summary = res?;
	post_import_statement_actions(ctx, sp_state, summary.as_ref()).await;

	if let Some(summary) = summary {
		// import_statement already takes care of communicating with the
		// prospective parachains subsystem. At this point, the candidate
		// has already been accepted by the subsystem.

		let candidate_hash = summary.candidate;

		if Some(summary.group_id) != sp_state.assigned_core {
			return Ok(());
		}

		let attesting = match statement.payload() {
			StatementWithPVD::Seconded(receipt, _) => {
				let attesting = AttestingData {
					candidate: sp_state
						.table
						.get_candidate(&candidate_hash)
						.ok_or(Error::CandidateNotFound)?
						.to_plain(),
					pov_hash: receipt.descriptor.pov_hash(),
					from_validator: statement.validator_index(),
					backing: Vec::new(),
				};
				sp_state.fallbacks.insert(summary.candidate, attesting.clone());
				attesting
			},
			StatementWithPVD::Valid(candidate_hash) => {
				if let Some(attesting) = sp_state.fallbacks.get_mut(candidate_hash) {
					let our_index = sp_state.table_context.validator.as_ref().map(|v| v.index());
					if our_index == Some(statement.validator_index()) {
						return Ok(());
					}

					if sp_state.awaiting_validation.contains(candidate_hash) {
						// Job already running:
						attesting.backing.push(statement.validator_index());
						return Ok(());
					} else {
						// No job, so start another with current validator:
						attesting.from_validator = statement.validator_index();
						attesting.clone()
					}
				} else {
					return Ok(());
				}
			},
		};

		// Skip validation if local validator is disabled or not a validator.
		match sp_state.table_context.local_validator_is_disabled() {
			Some(true) => return Ok(()),
			None => return Ok(()),
			Some(false) => {},
		}

		// After `import_statement` succeeds, the candidate entry is guaranteed
		// to exist.
		if let Some(pvd) = state
			.per_candidate
			.get(&candidate_hash)
			.map(|pc| pc.persisted_validation_data.clone())
		{
			kick_off_validation_work(
				ctx,
				sp_state,
				pvd,
				&state.background_validation_tx,
				attesting,
			)
			.await?;
		}
	}
	Ok(())
}

/// Kick off background validation with intent to second.
#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn validate_and_second<Context>(
	ctx: &mut Context,
	sp_state: &mut PerSchedulingParentState,
	persisted_validation_data: PersistedValidationData,
	candidate: &CandidateReceipt,
	pov: Arc<PoV>,
	background_validation_tx: &mpsc::Sender<(Hash, ValidatedCandidateCommand)>,
) -> Result<(), Error> {
	let candidate_hash = candidate.hash();

	gum::debug!(
		target: LOG_TARGET,
		candidate_hash = ?candidate_hash,
		candidate_receipt = ?candidate,
		"Validate and second candidate",
	);

	let bg_sender = ctx.sender().clone();
	let scheduling_parent = candidate.descriptor.scheduling_parent();
	background_validate_and_make_available(
		ctx,
		sp_state,
		BackgroundValidationParams {
			sender: bg_sender,
			tx_command: background_validation_tx.clone(),
			candidate: candidate.clone(),
			scheduling_parent,
			session_index: sp_state.session_index,
			node_features: sp_state.node_features.clone(),
			persisted_validation_data,
			pov: PoVData::Ready(pov),
			n_validators: sp_state.table_context.validators.len(),
			make_command: ValidatedCandidateCommand::Second,
		},
	)
	.await?;

	Ok(())
}

#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn handle_second_message<Context>(
	ctx: &mut Context,
	state: &mut State,
	candidate: CandidateReceipt,
	persisted_validation_data: PersistedValidationData,
	pov: PoV,
	metrics: &Metrics,
) -> Result<(), Error> {
	let _timer = metrics.time_process_second();

	let candidate_hash = candidate.hash();

	if candidate.descriptor().persisted_validation_data_hash() != persisted_validation_data.hash() {
		gum::warn!(
			target: LOG_TARGET,
			?candidate_hash,
			"Candidate backing was asked to second candidate with wrong PVD",
		);

		return Ok(());
	}

	// Version consistency + V3 gating (shared logic from primitives).
	if let Err(reason) = candidate.descriptor().check_version_acceptance(state.v3_ever_seen) {
		gum::debug!(
			target: LOG_TARGET,
			?candidate_hash,
			"Not seconding candidate: {}",
			reason,
		);
		ctx.send_message(CollatorProtocolMessage::Invalid(
			candidate.descriptor().scheduling_parent(),
			candidate,
		))
		.await;
		return Ok(());
	}

	// The signing context should use scheduling_parent (for V1/V2, this equals relay_parent)
	let scheduling_parent = candidate.descriptor().scheduling_parent();

	// Look up the PerSchedulingParentState using scheduling_parent - this is where we'll sign
	let sp_state = match state.per_scheduling_parent.get_mut(&scheduling_parent) {
		None => {
			gum::trace!(
				target: LOG_TARGET,
				?scheduling_parent,
				?candidate_hash,
				"Candidate has scheduling_parent outside of our view."
			);

			return Ok(());
		},
		Some(r) => r,
	};

	// Get the scheduling info (assigned_core and claim_queue) from the scheduling_parent state
	let assigned_core = sp_state.assigned_core;
	let claim_queue = &sp_state.claim_queue;

	// Just return if the local validator is disabled. If we are here the local node should be a
	// validator but defensively use `unwrap_or(false)` to continue processing in this case.
	if sp_state.table_context.local_validator_is_disabled().unwrap_or(false) {
		gum::warn!(target: LOG_TARGET, "Local validator is disabled. Don't validate and second");
		return Ok(());
	}

	// For V3, use scheduling info from scheduling_parent (claim queue determines assignments)
	let assigned_paras = assigned_core.and_then(|core| claim_queue.0.get(&core));

	// Sanity check that candidate is from our assignment.
	if !matches!(assigned_paras, Some(paras) if paras.contains(&candidate.descriptor().para_id())) {
		gum::debug!(
			target: LOG_TARGET,
			our_assignment_core = ?assigned_core,
			our_assignment_paras = ?assigned_paras,
			collation = ?candidate.descriptor().para_id(),
			"Subsystem asked to second for para outside of our assignment",
		);
		return Ok(());
	}

	gum::debug!(
		target: LOG_TARGET,
		our_assignment_core = ?assigned_core,
		our_assignment_paras = ?assigned_paras,
		collation = ?candidate.descriptor().para_id(),
		"Current assignments vs collation",
	);

	// If the message is a `CandidateBackingMessage::Second`, sign and dispatch a
	// Seconded statement only if we have not signed a Valid statement for the requested candidate.
	//
	// The actual logic of issuing the signed statement checks that this isn't
	// conflicting with other seconded candidates. Not doing that check here
	// gives other subsystems the ability to get us to execute arbitrary candidates,
	// but no more.
	if !sp_state.issued_statements.contains(&candidate_hash) {
		let pov = Arc::new(pov);

		validate_and_second(
			ctx,
			sp_state,
			persisted_validation_data,
			&candidate,
			pov,
			&state.background_validation_tx,
		)
		.await?;
	}

	Ok(())
}

#[overseer::contextbounds(CandidateBacking, prefix = self::overseer)]
async fn handle_statement_message<Context>(
	ctx: &mut Context,
	state: &mut State,
	scheduling_parent: Hash,
	statement: SignedFullStatementWithPVD,
	metrics: &Metrics,
) -> Result<(), Error> {
	let _timer = metrics.time_process_statement();

	// Validator disabling is handled in `maybe_validate_and_import`
	match maybe_validate_and_import(ctx, state, scheduling_parent, statement).await {
		Err(Error::ValidationFailed(_)) => Ok(()),
		Err(e) => Err(e),
		Ok(()) => Ok(()),
	}
}

fn handle_get_backable_candidates_message(
	state: &State,
	requested_candidates: HashMap<ParaId, Vec<BackableCandidateRef>>,
	tx: oneshot::Sender<HashMap<ParaId, Vec<BackedCandidate>>>,
	metrics: &Metrics,
) -> Result<(), Error> {
	let _timer = metrics.time_get_backed_candidates();

	let mut backed = HashMap::with_capacity(requested_candidates.len());

	for (para_id, para_candidates) in requested_candidates {
		for candidate_ref in para_candidates.iter() {
			let candidate_hash = candidate_ref.candidate_hash;
			let scheduling_parent = candidate_ref.scheduling_parent;

			let sp_state = match state.per_scheduling_parent.get(&scheduling_parent) {
				Some(sp_state) => sp_state,
				None => {
					gum::debug!(
						target: LOG_TARGET,
						?scheduling_parent,
						?candidate_hash,
						"Requested candidate's scheduling parent is out of view",
					);
					break;
				},
			};
			let maybe_backed_candidate = sp_state
				.table
				.attested_candidate(
					&candidate_hash,
					&sp_state.table_context,
					sp_state.minimum_backing_votes,
				)
				.and_then(|attested| table_attested_to_backed(attested, &sp_state.table_context));

			if let Some(backed_candidate) = maybe_backed_candidate {
				backed
					.entry(para_id)
					.or_insert_with(|| Vec::with_capacity(para_candidates.len()))
					.push(backed_candidate);
			} else {
				break;
			}
		}
	}

	tx.send(backed).map_err(|data| Error::Send(data))?;
	Ok(())
}
