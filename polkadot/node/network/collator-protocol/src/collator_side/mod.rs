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

use std::{
	collections::{hash_map::Entry, HashMap, HashSet},
	sync::Arc,
	time::Duration,
};

use bitvec::{bitvec, vec::BitVec};
use futures::{channel::oneshot, future::Fuse, pin_mut, select, FutureExt, StreamExt};
use metrics::{CollationStats, CollationTracker};
use schnellru::{ByLength, LruMap};
use sp_core::{ConstU32, Pair};

use polkadot_node_network_protocol::{
	self as net_protocol,
	peer_set::{CollationVersion, PeerSet},
	request_response::{
		incoming::{self},
		v2 as request_v2, v3 as request_v3, IncomingRequestReceiver,
	},
	v1 as protocol_v1, v2 as protocol_v2, v3_collation as protocol_v3,
	v4_collation::{self as protocol_v4, CandidateFingerprint},
	CollationProtocols, OurView, PeerId, UnifiedReputationChange as Rep, View,
};
use polkadot_node_primitives::{PoV, MAX_SEGMENT_LEN};
use polkadot_node_subsystem::{
	messages::{
		ChainApiMessage, CollatorProtocolMessage, NetworkBridgeEvent, NetworkBridgeTxMessage,
		Segment, SegmentEntry,
	},
	overseer, FromOrchestra, OverseerSignal,
};
use polkadot_node_subsystem_util::{
	backing_implicit_view::View as ImplicitView,
	reputation::{ReputationAggregator, REPUTATION_CHANGE_INTERVAL},
	runtime::{
		fetch_claim_queue, get_candidate_events, get_group_rotation_info, ClaimQueueSnapshot,
		RuntimeInfo,
	},
	TimeoutExt,
};
use polkadot_primitives::{
	AuthorityDiscoveryId, BlockNumber, CandidateDescriptorV2, CandidateDescriptorVersion,
	CandidateEvent, CandidateHash, CandidateReceiptV2 as CandidateReceipt, CollatorPair, CoreIndex,
	Hash, HeadData, Id as ParaId, SessionIndex,
};
use sp_runtime::BoundedVec;

use crate::{modify_reputation, LOG_TARGET, LOG_TARGET_STATS};
use polkadot_node_clock::{BoxedDelay, Clock};

mod collation;
mod error;
mod metrics;
#[cfg(test)]
mod tests;

use collation::{
	ActiveCollationFetches, Collation, CollationSendResult, CollationStatus,
	VersionedCollationRequest, WaitingCollationFetches,
};
use error::{log_error, Error, FatalError, Result};

pub use metrics::Metrics;

const COST_INVALID_REQUEST: Rep = Rep::CostMajor("Peer sent unparsable request");
const COST_UNEXPECTED_MESSAGE: Rep = Rep::CostMinor("An unexpected message");
const COST_APPARENT_FLOOD: Rep =
	Rep::CostMinor("Message received when previous one was still being processed");

/// Time after starting an upload to a validator we will start another one to the next validator,
/// even if the upload was not finished yet.
///
/// This is to protect from a single slow validator preventing collations from happening.
///
/// For considerations on this value, see: https://github.com/paritytech/polkadot/issues/4386
const MAX_UNSHARED_UPLOAD_TIME: Duration = Duration::from_millis(150);

/// Ensure that collator updates its connection requests to validators
/// this long after the most recent leaf.
///
/// The timeout is designed for substreams to be properly closed if they need to be
/// reopened shortly after the next leaf.
///
/// Collators also update their connection requests on every new collation.
/// This timeout is mostly about removing stale connections while avoiding races
/// with new collations which may want to reactivate them.
///
/// Validators are obtained from [`ValidatorGroupsBuffer::validators_to_connect`].
const RECONNECT_AFTER_LEAF_TIMEOUT: Duration = Duration::from_secs(4);

/// Maximum number of parallel requests to send to the Chain API.
const MAX_PARALLEL_CHAIN_API_REQUESTS: usize = 10;

/// Future that when resolved indicates that we should update reserved peer-set
/// of validators we want to be connected to.
///
/// `Pending` variant never finishes and should be used when there're no peers
/// connected.
type ReconnectTimeout = Fuse<BoxedDelay>;

#[derive(Debug)]
enum ShouldAdvertiseTo {
	Yes,
	NotAuthority,
	AlreadyAdvertised,
}

/// Info about validators we are currently connected to.
///
/// It keeps track to which validators we advertised our collation.
#[derive(Debug)]
struct ValidatorGroup {
	/// Validators discovery ids.
	validators: Vec<AuthorityDiscoveryId>,

	/// Bits indicating which validators have already seen the announcement
	/// per core. Always the same length as `validators`.
	segment_advertised_to: BitVec,
}

impl ValidatorGroup {
	/// Create a group for `validators`, with nothing advertised yet.
	fn new(validators: Vec<AuthorityDiscoveryId>) -> Self {
		Self { segment_advertised_to: bitvec![0; validators.len()], validators }
	}

	/// Returns `true` if we should advertise the segment to the given peer.
	fn should_advertise_segment(
		&self,
		peer_ids: &HashMap<PeerId, HashSet<AuthorityDiscoveryId>>,
		peer: &PeerId,
	) -> ShouldAdvertiseTo {
		let authority_ids = match peer_ids.get(peer) {
			Some(authority_ids) => authority_ids,
			None => return ShouldAdvertiseTo::NotAuthority,
		};

		for id in authority_ids {
			// One peer id may correspond to different discovery ids across sessions,
			// having a non-empty intersection is sufficient to assume that this peer
			// belongs to this particular validator group.
			let validator_index = match self.validators.iter().position(|v| v == id) {
				Some(idx) => idx,
				None => continue,
			};
			// Either the segment is unseen by this validator group
			// or the corresponding bit is not set.
			if !self.segment_advertised_to.get(validator_index).map(|b| *b).unwrap_or(false) {
				return ShouldAdvertiseTo::Yes;
			} else {
				return ShouldAdvertiseTo::AlreadyAdvertised;
			}
		}
		ShouldAdvertiseTo::NotAuthority
	}

	/// Should be called after we advertised our segment to the given `peer` to keep track of it.
	fn segment_advertised_to_peer(
		&mut self,
		peer_ids: &HashMap<PeerId, HashSet<AuthorityDiscoveryId>>,
		peer: &PeerId,
	) {
		if let Some(authority_ids) = peer_ids.get(peer) {
			for id in authority_ids {
				let validator_index = match self.validators.iter().position(|v| v == id) {
					Some(idx) => idx,
					None => continue,
				};
				self.segment_advertised_to.set(validator_index, true)
			}
		}
	}

	/// Resets the advertised state for the given peer. Should be called when the peer disconnects
	/// so that we re-advertise the segment when they reconnect.
	fn reset_segment_advertised_to_peer(&mut self, authority_ids: &HashSet<AuthorityDiscoveryId>) {
		for id in authority_ids {
			let validator_index = match self.validators.iter().position(|v| v == id) {
				Some(idx) => idx,
				None => continue,
			};
			self.segment_advertised_to.set(validator_index, false);
		}
	}
}

#[derive(Debug)]
struct PeerData {
	/// Peer's view.
	view: View,
	/// Unknown heads in the view.
	///
	/// This can happen when the validator is faster at importing a block and sending out its
	/// `View` than the collator is able to import a block.
	unknown_heads: LruMap<Hash, (), ByLength>,
	/// The collation protocol version the peer is using.
	version: CollationVersion,
}

/// A type wrapping a collation, it's designated core index and stats.
struct CollationData {
	collation: Collation,
	core_index: CoreIndex,
	stats: Option<CollationStats>,
	session_index: SessionIndex,
}

impl CollationData {
	/// Returns inner collation ref.
	pub fn collation(&self) -> &Collation {
		&self.collation
	}

	/// Returns inner collation mut ref.
	pub fn collation_mut(&mut self) -> &mut Collation {
		&mut self.collation
	}

	/// Returns inner core index.
	pub fn core_index(&self) -> &CoreIndex {
		&self.core_index
	}

	/// Takes the stats and returns them.
	pub fn take_stats(&mut self) -> Option<CollationStats> {
		self.stats.take()
	}
}

/// A segment distributed by our collation-generation for a `(scheduling_parent, core)`, kept so
/// we can (re-)advertise it to validators. All candidates in a segment share one descriptor
/// version, hoisted here out of the per-candidate [`CandidateFingerprint`].
struct StoredSegment {
	/// Descriptor version shared by all candidates in the segment.
	descriptor_version: CandidateDescriptorVersion,
	/// Ordered candidate fingerprints advertised to validators.
	fingerprints: BoundedVec<CandidateFingerprint, ConstU32<MAX_SEGMENT_LEN>>,
}

struct PerSchedulingParent {
	/// Per core index validators group responsible for backing candidates built
	/// on top of this relay parent.
	validator_group: HashMap<CoreIndex, ValidatorGroup>,
	/// Distributed collations keyed by output head hash.
	collations: HashMap<Hash, CollationData>,
	/// Reverse index over materialized collations.
	by_candidate_hash: HashMap<CandidateHash, Hash>,
	/// Cores our para is assigned to at this scheduling parent.
	assignments: HashSet<CoreIndex>,
	/// The relay parent block number
	block_number: Option<BlockNumber>,
	/// The session index of this relay parent.
	session_index: SessionIndex,
	/// Current segment per core, used for validator advertisements.
	segments: HashMap<CoreIndex, StoredSegment>,
}

impl PerSchedulingParent {
	#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
	async fn new<Context>(
		ctx: &mut Context,
		runtime: &mut RuntimeInfo,
		para_id: ParaId,
		claim_queue: ClaimQueueSnapshot,
		block_number: Option<BlockNumber>,
		block_hash: Hash,
		session_index: SessionIndex,
	) -> Result<Self> {
		let assignments =
			claim_queue.iter_all_claims().fold(HashSet::new(), |mut acc, (core, claims)| {
				let n_claims = claims.iter().filter(|para| para == &&para_id).count();
				if n_claims > 0 {
					acc.insert(*core);
				}
				acc
			});

		let mut validator_groups = HashMap::default();

		for core in &assignments {
			let GroupValidators { validators } =
				determine_our_validators(ctx, runtime, *core, block_hash).await?;
			validator_groups.insert(*core, ValidatorGroup::new(validators));
		}
		let segments: HashMap<CoreIndex, StoredSegment> = HashMap::new();

		Ok(Self {
			validator_group: validator_groups,
			collations: HashMap::new(),
			by_candidate_hash: HashMap::new(),
			assignments,
			block_number,
			session_index,
			segments,
		})
	}

	/// Resolve a collation by candidate hash via the reverse index.
	fn collation_by_hash(&self, hash: &CandidateHash) -> Option<&CollationData> {
		self.by_candidate_hash.get(hash).and_then(|oh| self.collations.get(oh))
	}

	fn collation_by_hash_mut(&mut self, hash: &CandidateHash) -> Option<&mut CollationData> {
		let output_head = *self.by_candidate_hash.get(hash)?;
		self.collations.get_mut(&output_head)
	}

	fn collation_for_request(&self, req: &VersionedCollationRequest) -> Option<&CollationData> {
		match req {
			VersionedCollationRequest::V2(req) => {
				self.collation_by_hash(&req.payload.candidate_hash)
			},
			VersionedCollationRequest::V3(req) => {
				self.collations.get(&req.payload.output_head_data_hash)
			},
		}
	}

	fn collation_for_request_mut(
		&mut self,
		req: &VersionedCollationRequest,
	) -> Option<&mut CollationData> {
		match req {
			VersionedCollationRequest::V2(req) => {
				self.collation_by_hash_mut(&req.payload.candidate_hash)
			},
			VersionedCollationRequest::V3(req) => {
				self.collations.get_mut(&req.payload.output_head_data_hash)
			},
		}
	}
}

struct State {
	/// Clock used for all time reads. Production passes [`polkadot_node_clock::SystemClock`];
	/// tests inject a mock.
	clock: Arc<dyn Clock>,

	/// Our network peer id.
	local_peer_id: PeerId,

	/// Our collator pair.
	collator_pair: CollatorPair,

	/// The para this collator is collating on.
	/// Starts as `None` and is updated with every `CollateOn` message.
	collating_on: Option<ParaId>,

	/// Track all active peers and their views
	/// to determine what is relevant to them.
	peer_data: HashMap<PeerId, PeerData>,

	/// Leaves along with implicit ancestry.
	///
	/// It's `None` if the collator is not yet collating for a paraid.
	implicit_view: Option<ImplicitView>,

	/// Validators and distributed collations tracked for each scheduling parent from
	/// our view, including both leaves and implicit ancestry.
	per_scheduling_parent: HashMap<Hash, PerSchedulingParent>,

	/// The mapping from [`PeerId`] to [`HashSet<AuthorityDiscoveryId>`]. This is filled over time
	/// as we learn the [`PeerId`]'s by `PeerConnected` events.
	peer_ids: HashMap<PeerId, HashSet<AuthorityDiscoveryId>>,

	/// Timeout-future which is reset after every leaf to [`RECONNECT_AFTER_LEAF_TIMEOUT`] seconds.
	/// When it fires, we update our reserved peers.
	reconnect_timeout: ReconnectTimeout,

	/// Metrics.
	metrics: Metrics,

	/// All collation fetching requests that are still waiting to be answered.
	///
	/// They are stored per relay parent, when our view changes and the relay parent moves out, we
	/// will cancel the fetch request.
	waiting_collation_fetches: HashMap<Hash, WaitingCollationFetches>,

	/// Active collation fetches.
	///
	/// Each future returns the relay parent of the finished collation fetch.
	active_collation_fetches: ActiveCollationFetches,

	/// Aggregated reputation change
	reputation: ReputationAggregator,

	/// An utility for tracking all collations produced by the collator.
	collation_tracker: CollationTracker,

	/// Should we be connected to backers ?
	connect_to_backers: bool,
}

impl State {
	/// Creates a new `State` instance with the given parameters and setting all remaining
	/// state fields to their default values (i.e. empty).
	fn new(
		local_peer_id: PeerId,
		collator_pair: CollatorPair,
		metrics: Metrics,
		reputation: ReputationAggregator,
		clock: Arc<dyn Clock>,
	) -> State {
		State {
			clock,
			local_peer_id,
			collator_pair,
			metrics,
			collating_on: Default::default(),
			peer_data: Default::default(),
			implicit_view: None,
			per_scheduling_parent: Default::default(),
			peer_ids: Default::default(),
			reconnect_timeout: Fuse::terminated(),
			waiting_collation_fetches: Default::default(),
			active_collation_fetches: Default::default(),
			reputation,
			collation_tracker: Default::default(),
			connect_to_backers: false,
		}
	}
}

/// Distribute a segment.
///
/// Figure out the core our para is assgined to and the relevant validators.
/// Issues a connection request to these validators.
/// If the para is not scheduled or next up on any core, at the scheduling-parent,
/// or the scheduling-parent isn't in the implicit ancestry, we ignore the message
/// as it must be invalid in that case - although this indicates a logic error
/// elsewhere in the node.
///
/// A segment is write-once per `(scheduling_parent, core_index)`: the first one we accept
/// wins and any later segment for that key is dropped. Consequently the "already advertised"
/// bookkeeping (`ValidatorGroup::segment_advertised_to`) is a single bitvec per key rather
/// than per candidate — there is never a second segment to advertise, so a validator that
/// has seen this key's segment has seen everything we will ever offer for it.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn distribute_segment<Context>(
	ctx: &mut Context,
	state: &mut State,
	id: ParaId,
	core_index: CoreIndex,
	segment: Segment,
) -> Result<()> {
	// We should already be connected to the validators, but if we aren't, we will try to connect to
	// them now. Do this BEFORE checking if parents are in view to ensure connections are properly
	// updated even when rejecting out-of-view collations.
	update_validator_connections(
		ctx,
		&state.peer_ids,
		&state.implicit_view,
		&state.per_scheduling_parent,
		id,
		true,
	)
	.await;

	let (scheduling_parent, descriptor_version, sp_session, entries) = match segment {
		Segment::V2(entry) => {
			(entry.relay_parent, CandidateDescriptorVersion::V2, None, vec![entry])
		},
		Segment::V3 { scheduling_parent, scheduling_session, candidates } => (
			scheduling_parent,
			CandidateDescriptorVersion::V3,
			Some(scheduling_session),
			candidates.into_inner(),
		),
	};

	let per_scheduling_parent = match state.per_scheduling_parent.get_mut(&scheduling_parent) {
		Some(per_scheduling_parent) => per_scheduling_parent,
		None => {
			gum::debug!(
				target: LOG_TARGET,
				para_id = %id,
				?scheduling_parent,
				"Scheduling parent is out of our view",
			);
			return Ok(());
		},
	};

	if entries.is_empty() {
		gum::warn!(
				target: LOG_TARGET,
				para_id = %id,
				?scheduling_parent,
				?core_index,
				"Received an empty segment, ignoring",
		);
		return Ok(());
	}

	let Some(_) = per_scheduling_parent.assignments.get(&core_index) else {
		gum::warn!(
			target: LOG_TARGET,
			para_id = %id,
			?scheduling_parent,
			cores = ?per_scheduling_parent.assignments,
			?core_index,
			"Attempting to distribute collation for a core we are not assigned to ",
		);

		return Ok(());
	};

	let elastic_scaling = per_scheduling_parent.assignments.len() > 1;
	if elastic_scaling {
		gum::debug!(
			target: LOG_TARGET,
			para_id = %id,
			cores = ?per_scheduling_parent.assignments,
			"{} is assigned to {} cores at {}", id, per_scheduling_parent.assignments.len(), scheduling_parent,
		);
	}

	// The group was determined for every assigned core in `PerSchedulingParent::new`, so a
	// missing or empty one means we have no backers to advertise to.
	let validators = match per_scheduling_parent.validator_group.get(&core_index) {
		Some(group) if !group.validators.is_empty() => &group.validators,
		_ => {
			gum::warn!(
				target: LOG_TARGET,
				core = ?core_index,
				"there are no validators assigned to core",
			);
			return Ok(());
		},
	};

	// Segments are write-once per `(scheduling_parent, core_index)` — see the doc comment above.
	if per_scheduling_parent.segments.contains_key(&core_index) {
		gum::debug!(target: LOG_TARGET, "Received a new segment at core {:?}/sp:{}. Dropping it...", core_index, scheduling_parent);
		return Ok(());
	}

	gum::debug!(
		target: LOG_TARGET,
		para_id = %id,
		scheduling_parent = %scheduling_parent,
		?core_index,
		current_validators = ?validators,
		"Accepted segment, connecting to validators."
	);

	let mut segment_fingerprint = vec![];
	for entry in entries {
		let pov_hash = entry.pov.hash();
		let receipt =
			assemble_receipt(id, core_index, scheduling_parent, sp_session, &entry, pov_hash);
		let candidate_hash = receipt.hash();

		let para_head = entry.output_head_data_hash;
		segment_fingerprint.push(CandidateFingerprint {
			output_head_data_hash: para_head,
			parent_head_data_hash: entry.parent_head_data.hash(),
			claim_queue_offset: 0,
		});
		// We have already seen collation for this scheduling parent.
		if per_scheduling_parent.collations.contains_key(&para_head) {
			if per_scheduling_parent.by_candidate_hash.contains_key(&candidate_hash) {
				gum::warn!(target: LOG_TARGET, ?scheduling_parent, ?candidate_hash, "Already seen this candidate.");
			} else {
				gum::warn!(target: LOG_TARGET, ?scheduling_parent, ?candidate_hash, output_head = ?para_head, "Received a candidate with the same output head at this scheduling parent.");
			}
			continue;
		}
		// Store collation data
		per_scheduling_parent.by_candidate_hash.insert(candidate_hash, para_head);
		per_scheduling_parent.collations.insert(
			para_head,
			CollationData {
				collation: Collation {
					receipt,
					pov: entry.pov,
					parent_head_data: entry.parent_head_data,
					status: CollationStatus::Created,
				},
				core_index,
				stats: per_scheduling_parent.block_number.map(|n| {
					CollationStats::new(
						&*state.clock,
						para_head,
						n,
						scheduling_parent,
						&state.metrics,
						*candidate_hash,
						pov_hash,
					)
				}),
				session_index: per_scheduling_parent.session_index,
			},
		);
	}

	let new_segment = StoredSegment {
		descriptor_version,
		fingerprints: BoundedVec::try_from(segment_fingerprint)
			.expect("exactly one fingerprint per entry, entries bounded by the message; qed"),
	};

	per_scheduling_parent.segments.insert(core_index, new_segment);

	// The leaf should be present in the allowed ancestry of some leaf.
	//
	// It's collation-producer responsibility to verify that there exists
	// a hypothetical membership in a fragment chain for the candidate.
	let interested = state
		.peer_data
		.iter()
		.filter(|(_, PeerData { view: v, .. })| {
			v.iter().any(|block_hash| {
				state.implicit_view.as_ref().map(|implicit_view| {
					implicit_view
						.known_allowed_relay_parents_under(block_hash)
						.unwrap_or_default()
						.contains(&scheduling_parent)
				}) == Some(true)
			})
		})
		.map(|(id, _)| id);

	for peer_id in interested {
		// Get the peer's protocol version. The peer should exist in peer_data
		// since we iterated over it to build `interested`.
		let peer_version =
			state.peer_data.get(peer_id).expect("peer from peer_data should exist").version;
		advertise_segment(
			ctx,
			scheduling_parent,
			core_index,
			per_scheduling_parent,
			peer_id,
			peer_version,
			&state.peer_ids,
			&state.metrics,
			id,
		)
		.await;
	}
	Ok(())
}

/// Build the candidate receipt from the segment commons and a built entry.
/// `sp_session` is `Some` only for V3 segments;
/// `pov_hash` is derived by the caller from the shipped PoV.
fn assemble_receipt(
	para_id: ParaId,
	core_index: CoreIndex,
	scheduling_parent: Hash,
	sp_session: Option<SessionIndex>,
	entry: &SegmentEntry,
	pov_hash: Hash,
) -> CandidateReceipt {
	let descriptor = match sp_session {
		Some(scheduling_session) => CandidateDescriptorV2::new_v3(
			para_id,
			entry.relay_parent,
			core_index,
			entry.session_index,
			scheduling_session,
			entry.persisted_validation_data_hash,
			pov_hash,
			entry.erasure_root,
			entry.output_head_data_hash,
			entry.validation_code_hash,
			scheduling_parent,
		),
		None => CandidateDescriptorV2::new(
			para_id,
			entry.relay_parent,
			core_index,
			entry.session_index,
			entry.persisted_validation_data_hash,
			pov_hash,
			entry.erasure_root,
			entry.output_head_data_hash,
			entry.validation_code_hash,
		),
	};
	CandidateReceipt { descriptor, commitments_hash: entry.commitments_hash }
}

/// Validators of a particular group index.
#[derive(Debug)]
struct GroupValidators {
	/// The validators of above group (their discovery keys).
	validators: Vec<AuthorityDiscoveryId>,
}

/// Figure out current group of validators assigned to the para being collated on.
///
/// Returns [`ValidatorId`]'s of current group as determined based on the `relay_parent`.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn determine_our_validators<Context>(
	ctx: &mut Context,
	runtime: &mut RuntimeInfo,
	core_index: CoreIndex,
	relay_parent: Hash,
) -> Result<GroupValidators> {
	let session_index = runtime.get_session_index_for_child(ctx.sender(), relay_parent).await?;
	let info = &runtime
		.get_session_info_by_index(ctx.sender(), relay_parent, session_index)
		.await?
		.session_info;
	gum::debug!(target: LOG_TARGET, ?session_index, "Received session info");
	let groups = &info.validator_groups;
	let num_cores = groups.len();
	let rotation_info = get_group_rotation_info(ctx.sender(), relay_parent).await?;

	let current_group_index = rotation_info.group_for_core(core_index, num_cores);
	let current_validators =
		groups.get(current_group_index).map(|v| v.as_slice()).unwrap_or_default();

	let validators = &info.discovery_keys;

	let current_validators =
		current_validators.iter().map(|i| validators[i.0 as usize].clone()).collect();

	let current_validators = GroupValidators { validators: current_validators };

	Ok(current_validators)
}

/// Construct the declare message to be sent to validator.
fn declare_message(
	state: &mut State,
	version: CollationVersion,
) -> Option<
	CollationProtocols<
		protocol_v1::CollationProtocol,
		protocol_v2::CollationProtocol,
		protocol_v3::CollationProtocol,
		protocol_v4::CollationProtocol,
	>,
> {
	let para_id = state.collating_on?;
	match version {
		CollationVersion::V2 => {
			let declare_signature_payload =
				protocol_v2::declare_signature_payload(&state.local_peer_id);
			let wire_message = protocol_v2::CollatorProtocolMessage::Declare(
				state.collator_pair.public(),
				para_id,
				state.collator_pair.sign(&declare_signature_payload),
			);
			Some(CollationProtocols::V2(protocol_v2::CollationProtocol::CollatorProtocol(
				wire_message,
			)))
		},
		CollationVersion::V3 => {
			let declare_signature_payload =
				protocol_v3::declare_signature_payload(&state.local_peer_id);
			let wire_message = protocol_v3::CollatorProtocolMessage::Declare(
				state.collator_pair.public(),
				para_id,
				state.collator_pair.sign(&declare_signature_payload),
			);
			Some(CollationProtocols::V3(protocol_v3::CollationProtocol::CollatorProtocol(
				wire_message,
			)))
		},
		// V4 has no Declare; the para is carried in AdvertiseSegment.
		CollationVersion::V4 => None,
		_ => {
			gum::warn!(target: LOG_TARGET, ?version, "Attempting to declare with an unsupported collation version");
			None
		},
	}
}

/// Issue versioned `Declare` collation message to the given `peer`.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn declare<Context>(
	ctx: &mut Context,
	state: &mut State,
	peer: &PeerId,
	version: CollationVersion,
) {
	if let Some(wire_message) = declare_message(state, version) {
		ctx.send_message(NetworkBridgeTxMessage::SendCollationMessage(vec![*peer], wire_message))
			.await;
	}
}

/// Checks whether there are any core assignments for our para on any active relay chain leaves.
fn has_assigned_cores(
	implicit_view: &Option<ImplicitView>,
	per_scheduling_parent: &HashMap<Hash, PerSchedulingParent>,
) -> bool {
	let Some(implicit_view) = implicit_view else { return false };

	for leaf in implicit_view.leaves() {
		if let Some(scheduling_parent) = per_scheduling_parent.get(leaf) {
			if !scheduling_parent.assignments.is_empty() {
				return true;
			}
		}
	}

	false
}

/// Get a list of backing validators at all allowed relay parents.
/// If `pending_collation` is true, we will only return the validators
/// that have a collation pending.
fn list_of_backing_validators_in_view(
	implicit_view: &Option<ImplicitView>,
	per_scheduling_parent: &HashMap<Hash, PerSchedulingParent>,
	pending_collation: bool,
) -> Vec<AuthorityDiscoveryId> {
	let mut backing_validators = HashSet::new();
	let Some(implicit_view) = implicit_view else { return vec![] };

	for leaf in implicit_view.leaves() {
		let allowed_ancestry =
			implicit_view.known_allowed_relay_parents_under(leaf).unwrap_or_default();

		for allowed_scheduling_parent in allowed_ancestry {
			let Some(scheduling_parent) = per_scheduling_parent.get(allowed_scheduling_parent)
			else {
				continue;
			};

			if pending_collation {
				// Check if there is any collation for this scheduling parent.
				for collation_data in scheduling_parent.collations.values() {
					let core_index = collation_data.core_index();
					if let Some(group) = scheduling_parent.validator_group.get(core_index) {
						backing_validators.extend(group.validators.iter().cloned());
					}
				}
			} else {
				for group in scheduling_parent.validator_group.values() {
					backing_validators.extend(group.validators.iter().cloned());
				}
			}
		}
	}

	backing_validators.into_iter().collect()
}

/// Connect or disconnect to/from all backers at all viable relay parents.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn update_validator_connections<Context>(
	ctx: &mut Context,
	peer_ids: &HashMap<PeerId, HashSet<AuthorityDiscoveryId>>,
	implicit_view: &Option<ImplicitView>,
	per_scheduling_parent: &HashMap<Hash, PerSchedulingParent>,
	para_id: ParaId,
	connect: bool,
) {
	gum::trace!(target: LOG_TARGET, ?para_id, ?connect, "update_validator_connections");

	// Ignore address resolution failure, will reissue a new request on new collation.
	let (failed, _) = oneshot::channel();

	let msg = if connect {
		let cores_assigned = has_assigned_cores(implicit_view, per_scheduling_parent);
		// If no cores are assigned to the para, we still need to send a ConnectToValidators request
		// to the network bridge passing an empty list of validator ids. Otherwise, it will keep
		// connecting to the last requested validators until a new request is issued.
		let validator_ids = if cores_assigned {
			list_of_backing_validators_in_view(implicit_view, per_scheduling_parent, false)
		} else {
			Vec::new()
		};

		gum::trace!(
			target: LOG_TARGET,
			?cores_assigned,
			"Sending connection request to validators: {:?}",
			validator_ids,
		);
		NetworkBridgeTxMessage::ConnectToValidators {
			validator_ids,
			peer_set: PeerSet::Collation,
			failed,
		}
	} else {
		if peer_ids.is_empty() {
			return;
		}

		let validator_ids =
			list_of_backing_validators_in_view(implicit_view, per_scheduling_parent, true);

		gum::trace!(
			target: LOG_TARGET,
			"Keeping connections to validators with pending collations: {:?}",
			validator_ids,
		);

		// Disconnect from all validators with no pending collations.
		NetworkBridgeTxMessage::ConnectToValidators {
			validator_ids,
			peer_set: PeerSet::Collation,
			failed,
		}
	};

	ctx.send_message(msg).await;
}

/// Advertise segment to the given `peer`.
///
/// This will only advertise a segment if there exists at least one for the given
/// `scheduling_parent` and `core_index` and the giver `peer` is set as a validator
/// for our para at the given `scheduling_parent`.
///
/// We also make sure not to advertise a segment multiple times to the same validator.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn advertise_segment<Context>(
	ctx: &mut Context,
	scheduling_parent: Hash,
	core_index: CoreIndex,
	per_scheduling_parent: &mut PerSchedulingParent,
	peer: &PeerId,
	peer_version: CollationVersion,
	peer_ids: &HashMap<PeerId, HashSet<AuthorityDiscoveryId>>,
	metrics: &Metrics,
	para_id: ParaId,
) {
	let Some(stored) = per_scheduling_parent.segments.get(&core_index) else {
		gum::debug!(
			target: LOG_TARGET,
			?scheduling_parent,
			?core_index,
			"Skipping advertising, no segments on the sp/core key.",
		);
		return;
	};
	let candidates_descriptor_version = stored.descriptor_version;
	let core_segment = &stored.fingerprints;
	let Some(validator_group) = per_scheduling_parent.validator_group.get_mut(&core_index) else {
		gum::debug!(
			target: LOG_TARGET,
			?scheduling_parent,
			?core_index,
			"Skipping advertising to validator, validator group for core not found",
		);
		return;
	};

	if core_segment.is_empty() {
		gum::debug!(
			target: LOG_TARGET,
			?scheduling_parent,
			?core_index,
			"Skipping advertising to validator, segment is empty.",
		);
		return;
	}

	let should_advertise = validator_group.should_advertise_segment(peer_ids, &peer);
	match should_advertise {
		ShouldAdvertiseTo::Yes => {},
		ShouldAdvertiseTo::NotAuthority | ShouldAdvertiseTo::AlreadyAdvertised => {
			gum::trace!(
				target: LOG_TARGET,
				?scheduling_parent,
				peer_id = %peer,
				reason = ?should_advertise,
				"Not advertising collation"
			);
			return;
		},
	};

	let message = match peer_version {
		CollationVersion::V4 => CollationProtocols::V4(protocol_v4::AdvertiseSegment {
			scheduling_parent,
			candidates_descriptor_version,
			candidates: core_segment.clone(),
			para_id,
		}),
		CollationVersion::V3 => {
			// Send V3 protocol message with the actual descriptor version
			let newest_candidate = core_segment.last().expect("Segment is not empty; qed");
			let Some(tip) =
				per_scheduling_parent.collations.get(&newest_candidate.output_head_data_hash)
			else {
				gum::debug!(
						target: LOG_TARGET,
						?scheduling_parent,
						?core_index,
						"Tip collation not stored, not advertising to old-protocol peer",
				);
				return;
			};
			let tip_candidate_hash = tip.collation().receipt.hash();
			CollationProtocols::V3(protocol_v3::CollationProtocol::CollatorProtocol(
				protocol_v3::CollatorProtocolMessage::AdvertiseCollation {
					scheduling_parent,
					candidate_hash: tip_candidate_hash,
					parent_head_data_hash: newest_candidate.parent_head_data_hash,
					candidate_descriptor_version: candidates_descriptor_version,
					relay_parent: tip.collation().receipt.descriptor.relay_parent(),
				},
			))
		},
		CollationVersion::V2 | CollationVersion::V1 => {
			// Fall back to V2 protocol for older peers
			let newest_candidate = core_segment.last().expect("Segment is not empty; qed");
			let Some(tip) =
				per_scheduling_parent.collations.get(&newest_candidate.output_head_data_hash)
			else {
				gum::debug!(
						target: LOG_TARGET,
						?scheduling_parent,
						?core_index,
						"Tip collation not stored, not advertising to old-protocol peer",
				);
				return;
			};
			let tip_candidate_hash = tip.collation().receipt.hash();
			CollationProtocols::V2(protocol_v2::CollationProtocol::CollatorProtocol(
				protocol_v2::CollatorProtocolMessage::AdvertiseCollation {
					scheduling_parent,
					candidate_hash: tip_candidate_hash,
					parent_head_data_hash: newest_candidate.parent_head_data_hash,
				},
			))
		},
	};

	ctx.send_message(NetworkBridgeTxMessage::SendCollationMessage(vec![*peer], message))
		.await;

	// Only mark the segment's collations advertised once the message is actually out. The
	// per-version arms above can bail without sending; advancing the status before that would
	// report a collation as advertised when nothing was sent, hiding the very bug that made
	// us bail.
	for fingerprint in core_segment {
		let Some(collation_and_core) =
			per_scheduling_parent.collations.get_mut(&fingerprint.output_head_data_hash)
		else {
			// Should not happen
			gum::warn!(
					target: LOG_TARGET,
					?scheduling_parent,
					output_head = ?fingerprint.output_head_data_hash,
					"Segment entry has no stored collation",
			);
			continue;
		};
		let collation = collation_and_core.collation_mut();
		collation.status.advance_to_advertised();
	}

	validator_group.segment_advertised_to_peer(peer_ids, peer);
	metrics.on_advertisement_made();
}

/// The main incoming message dispatching switch.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn process_msg<Context>(
	ctx: &mut Context,
	runtime: &mut RuntimeInfo,
	state: &mut State,
	msg: CollatorProtocolMessage,
) -> Result<()> {
	use CollatorProtocolMessage::*;

	match msg {
		ConnectToBackingGroups => {
			gum::debug!(
				target: LOG_TARGET,
				"Received PreConnectToBackingGroups message."
			);
			state.connect_to_backers = true;

			if let Some(para_id) = state.collating_on {
				update_validator_connections(
					ctx,
					&state.peer_ids,
					&state.implicit_view,
					&state.per_scheduling_parent,
					para_id,
					state.connect_to_backers,
				)
				.await;
			}
		},
		DisconnectFromBackingGroups => {
			gum::debug!(
				target: LOG_TARGET,
				"Received DisconnectFromBackingGroups message."
			);
			state.connect_to_backers = false;

			if let Some(para_id) = state.collating_on {
				update_validator_connections(
					ctx,
					&state.peer_ids,
					&state.implicit_view,
					&state.per_scheduling_parent,
					para_id,
					state.connect_to_backers,
				)
				.await;
			}
		},
		CollateOn(id) => {
			state.collating_on = Some(id);
			state.implicit_view = Some(ImplicitView::new());
		},
		DistributeSegment { core_index, para_id, segment } => {
			match state.collating_on {
				Some(id) if para_id != id => {
					// If the ParaId of a collation requested to be distributed does not match
					// the one we expect, we ignore the message.
					gum::warn!(
						target: LOG_TARGET,
						%para_id,
						collating_on = %id,
						"DistributeSegment for unexpected para_id",
					);
				},
				Some(id) => {
					gum::trace!(target: LOG_TARGET, "DistributeSegment for para_id: {}", id);
					let _ = state.metrics.time_collation_distribution("distribute");
					distribute_segment(ctx, state, id, core_index, segment).await?;
				},
				None => {
					gum::warn!(
						target: LOG_TARGET,
						%para_id,
						"DistributeSegment message while not collating on any",
					);
				},
			}
		},
		NetworkBridgeUpdate(event) => {
			// We should count only this shoulder in the histogram, as other shoulders are just
			// introducing noise
			let _ = state.metrics.time_process_msg();

			if let Err(e) = handle_network_msg(ctx, runtime, state, event).await {
				gum::warn!(
					target: LOG_TARGET,
					err = ?e,
					"Failed to handle incoming network message",
				);
			}
		},
		msg @ (Invalid(..) | Seconded(..)) => {
			gum::warn!(
				target: LOG_TARGET,
				"{:?} message is not expected on the collator side of the protocol",
				msg,
			);
		},
	}

	Ok(())
}

/// Issue a response to a previously requested collation.
async fn send_collation(
	state: &mut State,
	request: VersionedCollationRequest,
	receipt: CandidateReceipt,
	pov: PoV,
	parent_head_data: HeadData,
) {
	let (tx, rx) = oneshot::channel();

	let scheduling_parent = request.scheduling_parent();
	let peer_id = request.peer_id();
	let candidate_hash = receipt.hash();

	if let Err(_) = request.send_collation_response(receipt, pov, parent_head_data, tx) {
		gum::warn!(target: LOG_TARGET, "Sending collation response failed");
	}

	state.active_collation_fetches.push(
		async move {
			let r = rx.timeout(MAX_UNSHARED_UPLOAD_TIME).await;
			let timed_out = r.is_none();

			CollationSendResult { scheduling_parent, candidate_hash, peer_id, timed_out }
		}
		.boxed(),
	);

	state.metrics.on_collation_sent();
}

/// A networking messages switch.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn handle_incoming_peer_message<Context>(
	ctx: &mut Context,
	state: &mut State,
	origin: PeerId,
	msg: CollationProtocols<
		protocol_v1::CollatorProtocolMessage,
		protocol_v2::CollatorProtocolMessage,
		protocol_v3::CollatorProtocolMessage,
		protocol_v4::AdvertiseSegment,
	>,
) -> Result<()> {
	use protocol_v1::CollatorProtocolMessage as V1;
	use protocol_v2::CollatorProtocolMessage as V2;
	use protocol_v3::CollatorProtocolMessage as V3;

	match msg {
		CollationProtocols::V1(V1::Declare(..)) |
		CollationProtocols::V2(V2::Declare(..)) |
		CollationProtocols::V3(V3::Declare(..)) => {
			gum::trace!(
				target: LOG_TARGET,
				?origin,
				"Declare message is not expected on the collator side of the protocol",
			);

			// If we are declared to, this is another collator, and we should disconnect.
			ctx.send_message(NetworkBridgeTxMessage::DisconnectPeers(
				vec![origin],
				PeerSet::Collation,
			))
			.await;
		},
		CollationProtocols::V1(V1::AdvertiseCollation(_)) |
		CollationProtocols::V2(V2::AdvertiseCollation { .. }) |
		CollationProtocols::V3(V3::AdvertiseCollation { .. }) => {
			gum::trace!(
				target: LOG_TARGET,
				?origin,
				"AdvertiseCollation message is not expected on the collator side of the protocol",
			);

			modify_reputation(&mut state.reputation, ctx.sender(), origin, COST_UNEXPECTED_MESSAGE)
				.await;

			// If we are advertised to, this is another collator, and we should disconnect.
			ctx.send_message(NetworkBridgeTxMessage::DisconnectPeers(
				vec![origin],
				PeerSet::Collation,
			))
			.await;
		},
		// `AdvertiseSegment` is the only V4 message, so this arm covers the whole
		// version rather than one variant of it.
		CollationProtocols::V4(_) => {
			gum::trace!(
				target: LOG_TARGET,
				?origin,
				"AdvertiseSegment message is not expected on the collator side of the protocol",
			);

			modify_reputation(&mut state.reputation, ctx.sender(), origin, COST_UNEXPECTED_MESSAGE)
				.await;

			// If we are advertised to, this is another collator, and we should disconnect.
			ctx.send_message(NetworkBridgeTxMessage::DisconnectPeers(
				vec![origin],
				PeerSet::Collation,
			))
			.await;
		},
		CollationProtocols::V1(V1::CollationSeconded(..)) |
		CollationProtocols::V2(V2::CollationSeconded(..)) |
		CollationProtocols::V3(V3::CollationSeconded(..)) => {
			// Validators running older versions still send this. We no longer
			// track seconding, so the message carries no information for us.
			gum::trace!(
					target: LOG_TARGET,
					?origin,
					"Ignoring `CollationSeconded` message",
			);
		},
	}

	Ok(())
}

/// Process an incoming network request for a collation.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn handle_incoming_request<Context>(
	ctx: &mut Context,
	state: &mut State,
	req: std::result::Result<VersionedCollationRequest, incoming::Error>,
) -> Result<()> {
	let req = req?;
	let scheduling_parent = req.scheduling_parent();
	let peer_id = req.peer_id();
	let para_id = req.para_id();

	match state.collating_on {
		Some(our_para_id) if our_para_id == para_id => {
			let per_scheduling_parent =
				match state.per_scheduling_parent.get_mut(&scheduling_parent) {
					Some(per_scheduling_parent) => per_scheduling_parent,
					None => {
						gum::debug!(
							target: LOG_TARGET,
							%scheduling_parent,
							"received a `RequestCollation` for a scheduling parent out of our view",
						);

						return Ok(());
					},
				};

			let collation_with_core = per_scheduling_parent.collation_for_request_mut(&req);
			let (receipt, pov, parent_head_data) =
				if let Some(collation_with_core) = collation_with_core {
					let collation = collation_with_core.collation_mut();
					collation.status.advance_to_requested();
					(
						collation.receipt.clone(),
						collation.pov.clone(),
						collation.parent_head_data.clone(),
					)
				} else {
					gum::warn!(
						target: LOG_TARGET,
						scheduling_parent = %scheduling_parent,
						"received a `RequestCollation` for a relay parent we don't have collation stored.",
					);

					return Ok(());
				};

			state.metrics.on_collation_sent_requested();

			let waiting = state.waiting_collation_fetches.entry(scheduling_parent).or_default();
			let candidate_hash = receipt.hash();

			if !waiting.waiting_peers.insert((peer_id, candidate_hash)) {
				gum::debug!(
					target: LOG_TARGET,
					"Dropping incoming request as peer has a request in flight already."
				);
				modify_reputation(
					&mut state.reputation,
					ctx.sender(),
					peer_id,
					COST_APPARENT_FLOOD.into(),
				)
				.await;
				return Ok(());
			}

			if waiting.collation_fetch_active {
				waiting.req_queue.push_back(req);
			} else {
				waiting.collation_fetch_active = true;
				// Obtain a timer for sending collation
				let _ = state.metrics.time_collation_distribution("send");

				send_collation(state, req, receipt, pov, parent_head_data).await;
			}
		},
		Some(our_para_id) => {
			gum::warn!(
				target: LOG_TARGET,
				for_para_id = %para_id,
				our_para_id = %our_para_id,
				"received a `CollationFetchingRequest` for unexpected para_id",
			);
		},
		None => {
			gum::warn!(
				target: LOG_TARGET,
				for_para_id = %para_id,
				"received a `RequestCollation` while not collating on any para",
			);
		},
	}
	Ok(())
}

/// Advertises collations for the given scheduling parents to the specified peer.
///
/// Returns a list of unknown scheduling parents.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn advertise_collations_for_scheduling_parents<Context>(
	ctx: &mut Context,
	state: &mut State,
	peer_id: &PeerId,
	scheduling_parents: impl IntoIterator<Item = Hash>,
) -> Vec<Hash> {
	let mut unknown_scheduling_parents = Vec::new();

	let peer_version = match state.peer_data.get(peer_id) {
		Some(peer) => peer.version,
		None => return unknown_scheduling_parents,
	};

	let Some(para_id) = state.collating_on else {
		gum::debug!(target: LOG_TARGET, "Not collating on a para, skipping advertisements.");
		return unknown_scheduling_parents;
	};

	for scheduling_parent in scheduling_parents {
		let block_hashes = match state.per_scheduling_parent.contains_key(&scheduling_parent) {
			true => state
				.implicit_view
				.as_ref()
				.and_then(|implicit_view| {
					implicit_view.known_allowed_relay_parents_under(&scheduling_parent)
				})
				.unwrap_or_default(),
			false => {
				unknown_scheduling_parents.push(scheduling_parent);
				continue;
			},
		};

		for block_hash in block_hashes {
			if let Some(per_scheduling_parent) = state.per_scheduling_parent.get_mut(block_hash) {
				let cores: Vec<CoreIndex> =
					per_scheduling_parent.segments.keys().copied().collect();
				for core_index in cores {
					advertise_segment(
						ctx,
						*block_hash,
						core_index,
						per_scheduling_parent,
						peer_id,
						peer_version,
						&state.peer_ids,
						&state.metrics,
						para_id,
					)
					.await;
				}
			}
		}
	}

	unknown_scheduling_parents
}

/// Peer's view has changed. Send advertisements for new scheduling parents
/// if there're any.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn handle_peer_view_change<Context>(
	ctx: &mut Context,
	state: &mut State,
	peer_id: PeerId,
	view: View,
) {
	let Some(added) = state.peer_data.get_mut(&peer_id).map(|peer| {
		let diff: Vec<Hash> = view.difference(&peer.view).cloned().collect();
		peer.view = view;
		diff
	}) else {
		return;
	};

	let unknown_scheduling_parents =
		advertise_collations_for_scheduling_parents(ctx, state, &peer_id, added).await;

	if !unknown_scheduling_parents.is_empty() {
		gum::trace!(
			target: LOG_TARGET,
			?peer_id,
			new_leaves = ?unknown_scheduling_parents,
			"New leaves in peer's view are unknown",
		);

		if let Some(PeerData { unknown_heads, .. }) = state.peer_data.get_mut(&peer_id) {
			for unknown in unknown_scheduling_parents {
				unknown_heads.insert(unknown, ());
			}
		}
	}
}

/// Bridge messages switch.
#[overseer::contextbounds(CollatorProtocol, prefix = self::overseer)]
async fn handle_network_msg<Context>(
	ctx: &mut Context,
	runtime: &mut RuntimeInfo,
	state: &mut State,
	bridge_message: NetworkBridgeEvent<net_protocol::CollatorProtocolMessage>,
) -> Result<()> {
	use NetworkBridgeEvent::*;

	match bridge_message {
		PeerConnected(peer_id, observed_role, protocol_version, maybe_authority) => {
			// If it is possible that a disconnected validator would attempt a reconnect
			// it should be handled here.
			gum::trace!(target: LOG_TARGET, ?peer_id, ?observed_role, ?maybe_authority, "Peer connected");

			let version: CollationVersion = match protocol_version.try_into() {
				Ok(version) => version,
				Err(err) => {
					// Network bridge is expected to handle this.
					gum::error!(
						target: LOG_TARGET,
						?peer_id,
						?observed_role,
						?err,
						"Unsupported protocol version"
					);
					return Ok(());
				},
			};
			if version == CollationVersion::V1 {
				gum::warn!(
					target: LOG_TARGET,
					?peer_id,
					?observed_role,
					"Unsupported protocol version v1"
				);

				// V1 no longer supported, we should disconnect.
				ctx.send_message(NetworkBridgeTxMessage::DisconnectPeers(
					vec![peer_id],
					PeerSet::Collation,
				))
				.await;
				return Ok(());
			}

			state.peer_data.entry(peer_id).or_insert_with(|| PeerData {
				view: View::default(),
				// Unlikely that the collator is falling 10 blocks behind and if so, it probably is
				// not able to keep up any way.
				unknown_heads: LruMap::new(ByLength::new(10)),
				version,
			});

			if let Some(authority_ids) = maybe_authority {
				gum::trace!(
					target: LOG_TARGET,
					?authority_ids,
					?peer_id,
					"Connected to requested validator"
				);
				state.peer_ids.insert(peer_id, authority_ids);

				declare(ctx, state, &peer_id, version).await;
			}
		},
		PeerViewChange(peer_id, view) => {
			gum::trace!(target: LOG_TARGET, ?peer_id, ?view, "Peer view change");
			handle_peer_view_change(ctx, state, peer_id, view).await;
		},
		PeerDisconnected(peer_id) => {
			gum::trace!(target: LOG_TARGET, ?peer_id, "Peer disconnected");

			// Reset advertised_to bits for this peer so that collations will be
			// re-advertised when they reconnect.
			if let Some(authority_ids) = state.peer_ids.get(&peer_id) {
				for per_scheduling_parent in state.per_scheduling_parent.values_mut() {
					for validator_group in per_scheduling_parent.validator_group.values_mut() {
						validator_group.reset_segment_advertised_to_peer(authority_ids);
					}
				}
			}

			state.peer_data.remove(&peer_id);
			state.peer_ids.remove(&peer_id);
		},
		OurViewChange(view) => {
			gum::trace!(target: LOG_TARGET, ?view, "Own view change");
			handle_our_view_change(ctx, runtime, state, view).await?;
			// Connect only if we are collating on a para.
			if let Some(para_id) = state.collating_on {
				update_validator_connections(
					ctx,
					&state.peer_ids,
					&state.implicit_view,
					&state.per_scheduling_parent,
					para_id,
					state.connect_to_backers,
				)
				.await;
			}
		},
		PeerMessage(remote, msg) => {
			handle_incoming_peer_message(ctx, state, remote, msg).await?;
		},
		UpdatedAuthorityIds(peer_id, authority_ids) => {
			gum::trace!(target: LOG_TARGET, ?peer_id, ?authority_ids, "Updated authority ids");
			if let Some(version) = state.peer_data.get(&peer_id).map(|data| data.version) {
				let is_new_peer = state.peer_ids.insert(peer_id, authority_ids).is_none();

				if is_new_peer {
					// The peer connected before its authority ids were known; declare
					// now, using its negotiated protocol version.
					declare(ctx, state, &peer_id, version).await;
				} else {
					// Authority IDs changed for an existing peer. Re-advertise collations
					// for scheduling parents already in their view, as the previous
					// authority IDs may not have matched our validator groups.
					let scheduling_parents_in_view: Vec<_> = state
						.peer_data
						.get(&peer_id)
						.map(|data| data.view.iter().cloned().collect())
						.unwrap_or_default();

					// Unknown scheduling parents are ignored because they were
					// handled when the peer's view was first processed.
					let _ = advertise_collations_for_scheduling_parents(
						ctx,
						state,
						&peer_id,
						scheduling_parents_in_view,
					)
					.await;
				}
			}
		},
		NewGossipTopology { .. } => {
			// impossible!
		},
	}

	Ok(())
}

/// Update collation tracker with the backed and included candidates.
#[overseer::contextbounds(CollatorProtocol, prefix = crate::overseer)]
async fn process_block_events<Context>(
	ctx: &mut Context,
	collation_tracker: &mut CollationTracker,
	leaf: Hash,
	maybe_block_number: Option<BlockNumber>,
	para_id: ParaId,
) {
	if let Ok(events) = get_candidate_events(ctx.sender(), leaf).await {
		let Some(block_number) = maybe_block_number else {
			// This should not happen. If it does this log message explains why
			// metrics and logs are missing for the candidates under this block.
			gum::debug!(
				target: crate::LOG_TARGET_STATS,
				relay_block = ?leaf,
				?para_id,
				"Failed to get relay chain block number",
			);
			return;
		};

		for ev in events {
			match ev {
				CandidateEvent::CandidateIncluded(receipt, _, _, _) => {
					if receipt.descriptor.para_id() != para_id {
						continue;
					}
					collation_tracker.collation_included(block_number, leaf, receipt);
				},
				CandidateEvent::CandidateBacked(receipt, _, _, _) => {
					if receipt.descriptor.para_id() != para_id {
						continue;
					}

					let Some(block_number) = maybe_block_number else { continue };
					collation_tracker.collation_backed(block_number, leaf, receipt);
				},
				_ => {
					// do not care about other events
				},
			}
		}
	}
}

/// Handles our view changes.
#[overseer::contextbounds(CollatorProtocol, prefix = crate::overseer)]
async fn handle_our_view_change<Context>(
	ctx: &mut Context,
	runtime: &mut RuntimeInfo,
	state: &mut State,
	view: OurView,
) -> Result<()> {
	let Some(implicit_view) = &mut state.implicit_view else { return Ok(()) };
	let Some(para_id) = state.collating_on else { return Ok(()) };

	let removed: Vec<_> = implicit_view.leaves().filter(|h| !view.contains(h)).copied().collect();
	let added: Vec<_> = view.iter().filter(|h| !implicit_view.contains_leaf(h)).collect();

	for leaf in added {
		let session_index = runtime.get_session_index_for_child(ctx.sender(), *leaf).await?;

		let claim_queue = fetch_claim_queue(ctx.sender(), *leaf).await?;

		implicit_view
			.activate_leaf(ctx.sender(), *leaf)
			.await
			.map_err(Error::ImplicitViewFetchError)?;

		let block_number = implicit_view.block_number(leaf);

		state.per_scheduling_parent.insert(
			*leaf,
			PerSchedulingParent::new(
				ctx,
				runtime,
				para_id,
				claim_queue,
				block_number,
				*leaf,
				session_index,
			)
			.await?,
		);

		process_block_events(ctx, &mut state.collation_tracker, *leaf, block_number, para_id).await;
		let allowed_ancestry =
			implicit_view.known_allowed_relay_parents_under(leaf).unwrap_or_default();

		// Get the peers that already reported us this head, but we didn't know it at this
		// point.
		let peers = state
			.peer_data
			.iter_mut()
			.filter_map(|(id, data)| data.unknown_heads.remove(leaf).map(|_| *id))
			.collect::<Vec<_>>();

		for block_hash in allowed_ancestry {
			let block_number = implicit_view.block_number(block_hash);

			let per_relay_parent = match state.per_scheduling_parent.entry(*block_hash) {
				Entry::Vacant(entry) => {
					let claim_queue = match fetch_claim_queue(ctx.sender(), *block_hash).await {
						Ok(cq) => cq,
						Err(error) => {
							gum::debug!(
								target: LOG_TARGET,
								?block_hash,
								?error,
								"Failed to fetch claim queue while iterating allowed ancestry",
							);
							continue;
						},
					};
					let session_index =
						match runtime.get_session_index_for_child(ctx.sender(), *leaf).await {
							Ok(si) => si,
							Err(error) => {
								gum::debug!(
									target: LOG_TARGET,
									?block_hash,
									?error,
									"Failed to fetch session index while iterating allowed ancestry",
								);
								continue;
							},
						};

					entry.insert(
						PerSchedulingParent::new(
							ctx,
							runtime,
							para_id,
							claim_queue,
							block_number,
							*block_hash,
							session_index,
						)
						.await?,
					)
				},
				Entry::Occupied(entry) => entry.into_mut(),
			};

			// Announce relevant collations to these peers.
			for peer_id in &peers {
				// Get the peer's protocol version, skip if peer disconnected
				let Some(peer_version) = state.peer_data.get(peer_id).map(|data| data.version)
				else {
					gum::debug!(
						target: LOG_TARGET,
						?peer_id,
						?block_hash,
						"Peer not found in peer_data, likely disconnected. Skipping advertisement.",
					);
					continue;
				};

				let cores: Vec<CoreIndex> = per_relay_parent.segments.keys().copied().collect();
				for core_index in cores {
					advertise_segment(
						ctx,
						*block_hash,
						core_index,
						per_relay_parent,
						peer_id,
						peer_version,
						&state.peer_ids,
						&state.metrics,
						para_id,
					)
					.await;
				}
			}
		}
	}

	let highest_session_index =
		state.per_scheduling_parent.values().map(|pr| pr.session_index).max();

	for leaf in removed {
		// If the leaf is deactivated it still may stay in the view as a part
		// of implicit ancestry. Only update the state after the hash is actually
		// pruned from the block info storage.
		let maybe_block_number = implicit_view.block_number(&leaf);
		let pruned = implicit_view.deactivate_leaf(leaf);

		for removed in &pruned {
			gum::debug!(
				target: LOG_TARGET,
				relay_parent = ?removed,
				"Removing relay parent because our view changed.",
			);

			if let Some(block_number) = maybe_block_number {
				let expired_collations = state.collation_tracker.drain_expired(block_number);
				process_expired_collations(expired_collations, para_id, &state.metrics);
			}

			// Get all the collations built on top of the removed leaf.
			let collations = state
				.per_scheduling_parent
				.remove(removed)
				.map(|per_relay_parent| per_relay_parent.collations)
				.unwrap_or_default();

			for collation_with_core in collations.into_values() {
				process_out_of_view_collation(
					&mut state.collation_tracker,
					collation_with_core,
					highest_session_index,
				);
			}

			state.waiting_collation_fetches.remove(removed);
		}
	}
	Ok(())
}

fn process_out_of_view_collation(
	collation_tracker: &mut CollationTracker,
	mut collation_with_core: CollationData,
	highest_session_index: Option<SessionIndex>,
) {
	let is_same_session =
		highest_session_index.map_or(true, |hs| hs == collation_with_core.session_index);
	let collation = collation_with_core.collation_mut();
	let candidate_hash = collation.receipt.hash();

	match collation.status {
		CollationStatus::Created => {
			if is_same_session {
				gum::warn!(
					target: LOG_TARGET,
					?candidate_hash,
					pov_hash = ?collation.pov.hash(),
					"Collation wasn't advertised to any validator.",
				)
			} else {
				gum::debug!(
					target: LOG_TARGET,
					?candidate_hash,
					pov_hash = ?collation.pov.hash(),
					"Collation wasn't advertised because it was built on a relay chain block that is now part of an old session.",
				)
			}
		},
		CollationStatus::Advertised => gum::debug!(
			target: LOG_TARGET,
			?candidate_hash,
			pov_hash = ?collation.pov.hash(),
			"Collation was advertised but not requested by any validator.",
		),
		CollationStatus::Requested => {
			gum::debug!(
				target: LOG_TARGET,
				?candidate_hash,
				pov_hash = ?collation.pov.hash(),
				"Collation was requested.",
			);
		},
	}

	let collation_status = collation.status.clone();
	let Some(mut stats) = collation_with_core.take_stats() else { return };

	// If the collation stats are still available, it means it was never
	// successfully fetched, even if a fetch request was received, but not succeed.
	//
	// Will expire in it's current state at the next block import.
	stats.set_pre_backing_status(collation_status);
	collation_tracker.track(stats);
}

/// Process collations that were expired
///
/// Collations no more tracked after this call.
fn process_expired_collations(
	expired_collations: Vec<CollationStats>,
	para_id: ParaId,
	metrics: &Metrics,
) {
	for expired_collation in expired_collations {
		let collation_state = expired_collation.expiry_state();
		let age = expired_collation.expired().unwrap_or_default();
		let candidate_hash = expired_collation.candidate_hash();
		let pov_hash = expired_collation.pov_hash();
		let relay_parent = expired_collation.relay_parent();
		gum::debug!(
			target: crate::LOG_TARGET_STATS,
			?age,
			?collation_state,
			?relay_parent,
			?para_id,
			head = ?expired_collation.head(),
			?candidate_hash,
			?pov_hash,
			"Collation expired",
		);

		// Report metrics for collations that were expired after being backed.
		// The only way this can happen is if they time out availability.
		if let Some(latency) = expired_collation.backed() {
			metrics.on_collation_backed(latency as f64);
		}
		metrics.on_collation_expired(age as f64, collation_state);
	}
}

/// Process collations that may have been finalized on the relay chain.
///
/// Collations are no longer tracked after this call.
///
/// ## Limitations
///
/// We only track the inclusion block number, not the inclusion block hash.
/// This means a collation included in a fork that was later dropped could still be counted as
/// finalized. This is acceptable for metrics because such cases are rare.
async fn process_possibly_finalized_collations(
	collations: Vec<CollationStats>,
	last_finalized: (Hash, BlockNumber),
	sender: &mut impl overseer::SubsystemSender<ChainApiMessage>,
	metrics: &Metrics,
) {
	if collations.is_empty() {
		return;
	}

	let (last_hash, last_number) = last_finalized;
	let mut blocks_to_request = collations
		.iter()
		.map(|stats| stats.relay_parent().1)
		.filter(|n| *n != last_number) // No need to request `last_hash` for `last_number`
		.collect::<Vec<_>>();
	blocks_to_request.sort_unstable();
	blocks_to_request.dedup();

	gum::debug!(target: LOG_TARGET_STATS, ?blocks_to_request, "Collations possibly finalized on blocks");

	let mut finalized = vec![(last_hash, last_number)];

	for chunk in blocks_to_request.as_slice().chunks(MAX_PARALLEL_CHAIN_API_REQUESTS) {
		let futures = chunk.iter().map(|&bn| {
				let mut sender = sender.clone();
				async move {
					let (tx, rx) = oneshot::channel();
					sender.send_message(ChainApiMessage::FinalizedBlockHash(bn, tx)).await;
					match rx.await {
						Ok(Ok(Some(bh))) => Some((bh, bn)),
						_ => {
							gum::warn!(target: LOG_TARGET_STATS, block_number = ?bn, "Can't request hash for the finalized block from ChainApi");
							None
						},
					}
				}
			});
		finalized.extend(futures::future::join_all(futures).await.into_iter().flatten());
	}

	for collation in collations {
		if !finalized.contains(&collation.relay_parent()) {
			// Omit collations built on forks: they're dropped but it's expected
			gum::debug!(
				target: crate::LOG_TARGET_STATS,
				relay_parent = ?collation.relay_parent(),
				head = ?collation.head(),
				"Collation is built on a fork, skipping",
			);
			continue;
		}

		// Report metrics for finalized collations
		if let Some(latency) = collation.backed() {
			metrics.on_collation_backed(latency as f64);
		}
		if let Some(latency) = collation.included() {
			metrics.on_collation_included(latency as f64);
		}
		gum::debug!(
			target: crate::LOG_TARGET_STATS,
			relay_parent = ?collation.relay_parent(),
			head = ?collation.head(),
			"Collation finalized, stop tracking",
		);
	}
}

/// The collator protocol collator side main loop.
#[overseer::contextbounds(CollatorProtocol, prefix = crate::overseer)]
pub(crate) async fn run<Context>(
	ctx: Context,
	local_peer_id: PeerId,
	collator_pair: CollatorPair,
	req_v2_receiver: IncomingRequestReceiver<request_v2::CollationFetchingRequest>,
	req_v3_receiver: IncomingRequestReceiver<request_v3::CollationFetchingRequest>,
	metrics: Metrics,
	clock: Arc<dyn Clock>,
) -> std::result::Result<(), FatalError> {
	run_inner(
		ctx,
		local_peer_id,
		collator_pair,
		req_v2_receiver,
		req_v3_receiver,
		metrics,
		ReputationAggregator::default(),
		REPUTATION_CHANGE_INTERVAL,
		clock,
	)
	.await
}

#[overseer::contextbounds(CollatorProtocol, prefix = crate::overseer)]
async fn run_inner<Context>(
	mut ctx: Context,
	local_peer_id: PeerId,
	collator_pair: CollatorPair,
	mut req_v2_receiver: IncomingRequestReceiver<request_v2::CollationFetchingRequest>,
	mut req_v3_receiver: IncomingRequestReceiver<request_v3::CollationFetchingRequest>,
	metrics: Metrics,
	reputation: ReputationAggregator,
	reputation_interval: Duration,
	clock: Arc<dyn Clock>,
) -> std::result::Result<(), FatalError> {
	use OverseerSignal::*;

	let new_reputation_delay = || clock.delay(reputation_interval).fuse();
	let mut reputation_delay = new_reputation_delay();

	let mut state =
		State::new(local_peer_id, collator_pair, metrics.clone(), reputation, clock.clone());
	let mut runtime = RuntimeInfo::new(None);

	loop {
		let reputation_changes = || vec![COST_INVALID_REQUEST];
		let recv_req_v2 = req_v2_receiver.recv(reputation_changes).fuse();
		pin_mut!(recv_req_v2);
		let recv_req_v3 = req_v3_receiver.recv(reputation_changes).fuse();
		pin_mut!(recv_req_v3);

		let mut reconnect_timeout = &mut state.reconnect_timeout;
		select! {
			_ = reputation_delay => {
				state.reputation.send(ctx.sender()).await;
				reputation_delay = new_reputation_delay();
			},
			msg = ctx.recv().fuse() => match msg.map_err(FatalError::SubsystemReceive)? {
				FromOrchestra::Communication { msg } => {
					log_error(
						process_msg(&mut ctx, &mut runtime, &mut state, msg).await,
						"Failed to process message"
					)?;
				},
				FromOrchestra::Signal(ActiveLeaves(update)) => {
					if update.activated.is_some() {
						*reconnect_timeout = clock.delay(RECONNECT_AFTER_LEAF_TIMEOUT).fuse();
					}
				}
				FromOrchestra::Signal(BlockFinalized(hash, number)) => {
					let possibly_finalized = state.collation_tracker.drain_finalized(number);
					process_possibly_finalized_collations(possibly_finalized, (hash, number), ctx.sender(), &metrics).await;
				}
				FromOrchestra::Signal(Conclude) => return Ok(()),
			},
			CollationSendResult { scheduling_parent, candidate_hash, peer_id, timed_out } =
				state.active_collation_fetches.select_next_some() => {

				let next = if let Some(waiting) = state.waiting_collation_fetches.get_mut(&scheduling_parent) {
					if timed_out {
						gum::debug!(
							target: LOG_TARGET_STATS,
							?scheduling_parent,
							?peer_id,
							?candidate_hash,
							"Sending collation to validator timed out, carrying on with next validator."
						);
						// We try to throttle requests per scheduling parent to give validators
						// more bandwidth, but if the collation is not received within the
						// timeout, we simply start processing next request.
						// The request it still alive, it should be kept in a waiting queue.
					} else {
						waiting.waiting_peers.remove(&(peer_id, candidate_hash));

						// Update collation status to fetched.
						if let Some(per_scheduling_parent) =  state.per_scheduling_parent.get_mut(&scheduling_parent) {
							if let Some(collation_with_core) = per_scheduling_parent.collation_by_hash_mut(&candidate_hash) {
								let maybe_stats = collation_with_core.take_stats();
								let our_para_id = collation_with_core.collation().receipt.descriptor.para_id();

								if let Some(mut stats) = maybe_stats {
									// Update the timestamp when collation has been sent (from subsystem perspective)
									stats.set_fetched_at(clock.now());
									gum::debug!(
										target: LOG_TARGET_STATS,
										para_head = ?stats.head(),
										%our_para_id,
										"Collation fetch latency is {}ms",
										stats.fetch_latency().unwrap_or_default().as_millis(),
									);

									// Update the pre-backing status. Should be requested at this point.
									stats.set_pre_backing_status(collation_with_core.collation().status.clone());
									debug_assert_eq!(collation_with_core.collation().status, CollationStatus::Requested);

									// Observe fetch latency metric.
									stats.take_fetch_latency_metric();
									stats.set_backed_latency_metric(metrics.time_collation_backing_latency());

									// Next step is to measure backing latency.
									state.collation_tracker.track(stats);
								}
							}
						}
					}

					if let Some(next) = waiting.req_queue.pop_front() {
						next
					} else {
						waiting.collation_fetch_active = false;
						continue
					}
				} else {
					// No waiting collation fetches means we already removed the relay parent from our view.
					continue
				};

				let next_collation_with_core = {
					let per_relay_parent = match state.per_scheduling_parent.get(&scheduling_parent) {
						Some(per_relay_parent) => per_relay_parent,
						None => continue,
					};

					per_relay_parent.collation_for_request(&next)
				};

				if let Some(collation_with_core) = next_collation_with_core {
					let collation = collation_with_core.collation();
					let receipt = collation.receipt.clone();
					let pov = collation.pov.clone();
					let parent_head_data = collation.parent_head_data.clone();

					send_collation(&mut state, next, receipt, pov, parent_head_data).await;
				}
			},
			_ = reconnect_timeout => {
				// Connect only if we are collating on a para.
				if let Some(para_id) = state.collating_on {
					update_validator_connections(
						&mut ctx,
						&state.peer_ids,
						&state.implicit_view,
						&state.per_scheduling_parent,
						para_id,
						state.connect_to_backers,
					)
					.await;
				}

				gum::trace!(
					target: LOG_TARGET,
					timeout = ?RECONNECT_AFTER_LEAF_TIMEOUT,
					"Peer-set updated due to a timeout"
				);
			},
			in_req = recv_req_v2 => {
				let request = in_req.map(VersionedCollationRequest::from);

				log_error(
					handle_incoming_request(&mut ctx, &mut state, request).await,
					"Handling incoming collation fetch request V2"
				)?;
			},
			in_req3 = recv_req_v3 => {
				let request = in_req3.map(VersionedCollationRequest::from);
				log_error(
					handle_incoming_request(&mut ctx, &mut state, request).await,
					"Handling incoming collation fetch request V3"
				)?;
			}
		}
	}
}
