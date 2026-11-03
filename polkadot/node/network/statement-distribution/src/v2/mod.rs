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

//! Implementation of the v2 statement distribution protocol,
//! designed for asynchronous backing.

use bitvec::prelude::{BitVec, Lsb0};
use polkadot_node_network_protocol::{
	self as net_protocol, filter_by_peer_version,
	grid_topology::SessionGridTopology,
	peer_set::{ProtocolVersion, ValidationVersion},
	request_response::{
		incoming::OutgoingResponse,
		v2::{AttestedCandidateRequest, AttestedCandidateResponse},
		IncomingRequest, IncomingRequestReceiver, Requests,
		MAX_PARALLEL_ATTESTED_CANDIDATE_REQUESTS,
	},
	v3::{self as protocol_v3, StatementFilter},
	IfDisconnected, PeerId, UnifiedReputationChange as Rep, ValidationProtocols, View,
};
use polkadot_node_primitives::{
	SignedFullStatementWithPVD, StatementWithPVD as FullStatementWithPVD,
};
use polkadot_node_subsystem::{
	messages::{
		network_bridge_event::NewGossipTopology, CandidateBackingMessage, HypotheticalCandidate,
		HypotheticalMembershipRequest, NetworkBridgeEvent, NetworkBridgeTxMessage,
		ProspectiveParachainsMessage,
	},
	overseer, ActivatedLeaf,
};
use polkadot_node_subsystem_util::{
	backing_implicit_view::View as ImplicitView, reputation::ReputationAggregator,
	request_min_backing_votes, runtime::ClaimQueueSnapshot,
};
use polkadot_primitives::{
	transpose_claim_queue, AuthorityDiscoveryId, CandidateHash, CompactStatement, CoreIndex,
	GroupIndex, GroupRotationInfo, Hash, Id as ParaId, IndexedVec, SessionIndex, SessionInfo,
	SignedStatement, SigningContext, TransposedClaimQueue, UncheckedSignedStatement, ValidatorId,
	ValidatorIndex,
};

use sp_keystore::KeystorePtr;

use fatality::Nested;
use futures::{
	channel::{mpsc, oneshot},
	future::FutureExt,
	select,
	stream::FuturesUnordered,
	SinkExt, StreamExt,
};

use std::{
	collections::{
		hash_map::{Entry, HashMap},
		HashSet,
	},
	time::{Duration, Instant},
};

use crate::{
	error::{JfyiError, JfyiErrorResult},
	metrics::Metrics,
	LOG_TARGET,
};
use candidates::{BadAdvertisement, Candidates, PostConfirmation};
use cluster::{Accept as ClusterAccept, ClusterTracker, RejectIncoming as ClusterRejectIncoming};
use grid::GridTracker;
use groups::Groups;
use requests::{CandidateIdentifier, RequestProperties};
use statement_store::{StatementOrigin, StatementStore};

pub use requests::{RequestManager, ResponseManager, UnhandledResponse};

mod candidates;
mod cluster;
mod grid;
mod groups;
mod requests;
mod statement_store;

#[cfg(test)]
mod tests;

const COST_UNEXPECTED_STATEMENT_NOT_VALIDATOR: Rep =
	Rep::CostMinor("Unexpected Statement, not a validator");
const COST_UNEXPECTED_STATEMENT_VALIDATOR_NOT_FOUND: Rep =
	Rep::CostMinor("Unexpected Statement, validator not found");
const COST_UNEXPECTED_STATEMENT_INVALID_SENDER: Rep =
	Rep::CostMinor("Unexpected Statement, invalid sender");
const COST_UNEXPECTED_STATEMENT_BAD_ADVERTISE: Rep =
	Rep::CostMinor("Unexpected Statement, bad advertise");
const COST_UNEXPECTED_STATEMENT_CLUSTER_REJECTED: Rep =
	Rep::CostMinor("Unexpected Statement, cluster rejected");
const COST_UNEXPECTED_STATEMENT_NOT_IN_GROUP: Rep =
	Rep::CostMinor("Unexpected Statement, not in group");

const COST_UNEXPECTED_STATEMENT_MISSING_KNOWLEDGE: Rep =
	Rep::CostMinor("Unexpected Statement, missing knowledge for relay parent");
const COST_EXCESSIVE_SECONDED: Rep = Rep::CostMinor("Sent Excessive `Seconded` Statements");
const COST_DISABLED_VALIDATOR: Rep = Rep::CostMinor("Sent a statement from a disabled validator");

const COST_UNEXPECTED_MANIFEST_MISSING_KNOWLEDGE: Rep =
	Rep::CostMinor("Unexpected Manifest, missing knowledge for relay parent");
const COST_UNEXPECTED_MANIFEST_DISALLOWED: Rep =
	Rep::CostMinor("Unexpected Manifest, Peer Disallowed");
const COST_UNEXPECTED_MANIFEST_PEER_UNKNOWN: Rep =
	Rep::CostMinor("Unexpected Manifest, Peer Unknown");
const COST_CONFLICTING_MANIFEST: Rep = Rep::CostMajor("Manifest conflicts with previous");
const COST_INSUFFICIENT_MANIFEST: Rep =
	Rep::CostMajor("Manifest statements insufficient to back candidate");
const COST_MALFORMED_MANIFEST: Rep = Rep::CostMajor("Manifest is malformed");
const COST_UNEXPECTED_ACKNOWLEDGEMENT_UNKNOWN_CANDIDATE: Rep =
	Rep::CostMinor("Unexpected acknowledgement, unknown candidate");

const COST_INVALID_SIGNATURE: Rep = Rep::CostMajor("Invalid Statement Signature");
const COST_IMPROPERLY_DECODED_RESPONSE: Rep =
	Rep::CostMajor("Improperly Encoded Candidate Response");
const COST_INVALID_RESPONSE: Rep = Rep::CostMajor("Invalid Candidate Response");
const COST_UNREQUESTED_RESPONSE_STATEMENT: Rep =
	Rep::CostMajor("Un-requested Statement In Response");
const COST_INACCURATE_ADVERTISEMENT: Rep =
	Rep::CostMajor("Peer advertised a candidate inaccurately");
const COST_INVALID_UMP_SIGNALS: Rep =
	Rep::CostMajor("Candidate Descriptor contains invalid ump signals");
const COST_INVALID_SESSION_INDEX: Rep =
	Rep::CostMajor("Candidate Descriptor contains an invalid session index");

const COST_INVALID_REQUEST: Rep = Rep::CostMajor("Peer sent unparsable request");
const COST_INVALID_REQUEST_BITFIELD_SIZE: Rep =
	Rep::CostMajor("Attested candidate request bitfields have wrong size");
const COST_UNEXPECTED_REQUEST: Rep = Rep::CostMajor("Unexpected attested candidate request");

const BENEFIT_VALID_RESPONSE: Rep = Rep::BenefitMajor("Peer Answered Candidate Request");
const BENEFIT_VALID_STATEMENT: Rep = Rep::BenefitMajor("Peer provided a valid statement");
const BENEFIT_VALID_STATEMENT_FIRST: Rep =
	Rep::BenefitMajorFirst("Peer was the first to provide a given valid statement");

/// The amount of time to wait before retrying when the node sends a request and it is dropped.
pub(crate) const REQUEST_RETRY_DELAY: Duration = Duration::from_secs(1);

struct PerSchedulingParentState {
	local_validator: Option<LocalValidatorState>,
	statement_store: StatementStore,
	session: SessionIndex,
	transposed_cq: TransposedClaimQueue,
	groups_per_para: HashMap<ParaId, Vec<GroupIndex>>,
	disabled_validators: HashSet<ValidatorIndex>,
	assignments_per_group: HashMap<GroupIndex, Vec<ParaId>>,
}

impl PerSchedulingParentState {
	fn active_validator_state(&self) -> Option<&ActiveValidatorState> {
		self.local_validator.as_ref().and_then(|local| local.active.as_ref())
	}

	fn active_validator_state_mut(&mut self) -> Option<&mut ActiveValidatorState> {
		self.local_validator.as_mut().and_then(|local| local.active.as_mut())
	}

	/// Returns `true` if the given validator is disabled in the context of the scheduling parent.
	pub fn is_disabled(&self, validator_index: &ValidatorIndex) -> bool {
		self.disabled_validators.contains(validator_index)
	}

	/// A convenience function to generate a disabled bitmask for the given backing group.
	/// The output bits are set to `true` for validators that are disabled.
	pub fn disabled_bitmask(&self, group: &[ValidatorIndex]) -> BitVec<u8, Lsb0> {
		BitVec::from_iter(group.iter().map(|v| self.is_disabled(v)))
	}
}

// per-scheduling-parent local validator state.
struct LocalValidatorState {
	// the grid-level communication at this relay-parent.
	grid_tracker: GridTracker,
	// additional fields in case local node is an active validator.
	active: Option<ActiveValidatorState>,
	// local index actually exists in case node is inactive validator, however,
	// it's not needed outside of `build_session_topology`, where it's known.
}

struct ActiveValidatorState {
	// The index of the validator.
	index: ValidatorIndex,
	// our validator group
	group: GroupIndex,
	// the assignments of our validator group, if any.
	assignments: Vec<ParaId>,
	// the 'direct-in-group' communication at this relay-parent.
	cluster_tracker: ClusterTracker,
}

#[derive(Debug, Copy, Clone)]
enum LocalValidatorIndex {
	// Local node is an active validator.
	Active(ValidatorIndex),
	// Local node is not in active validator set.
	Inactive,
}

#[derive(Debug)]
struct PerSessionState {
	session_info: SessionInfo,
	groups: Groups,
	authority_lookup: HashMap<AuthorityDiscoveryId, ValidatorIndex>,
	// is only `None` in the time between seeing a session and
	// getting the topology from the gossip-support subsystem
	grid_view: Option<grid::SessionTopologyView>,
	local_validator: Option<LocalValidatorIndex>,
}

impl PerSessionState {
	fn new(session_info: SessionInfo, keystore: &KeystorePtr, backing_threshold: u32) -> Self {
		let groups = Groups::new(session_info.validator_groups.clone(), backing_threshold);
		let mut authority_lookup = HashMap::new();
		for (i, ad) in session_info.discovery_keys.iter().cloned().enumerate() {
			authority_lookup.insert(ad, ValidatorIndex(i as _));
		}

		let local_validator = polkadot_node_subsystem_util::signing_key_and_index(
			session_info.validators.iter(),
			keystore,
		)
		.map(|(_, index)| LocalValidatorIndex::Active(index));

		PerSessionState { session_info, groups, authority_lookup, grid_view: None, local_validator }
	}

	fn supply_topology(
		&mut self,
		topology: &SessionGridTopology,
		local_index: Option<ValidatorIndex>,
	) {
		// Note: we use the local index rather than the `self.local_validator` as the
		// former may be `Some` when the latter is `None`, due to the set of nodes in
		// discovery being a superset of the active validators for consensus.
		let grid_view = grid::build_session_topology(
			self.session_info.validator_groups.iter(),
			topology,
			local_index,
		);

		self.grid_view = Some(grid_view);
		if local_index.is_some() {
			self.local_validator.get_or_insert(LocalValidatorIndex::Inactive);
		}

		gum::info!(
			target: LOG_TARGET,
			index_in_gossip_topology = ?local_index,
			index_in_parachain_authorities = ?self.local_validator,
			"Node uses the following topology indices"
		);
	}

	/// Returns `true` if local is neither active or inactive validator node.
	///
	/// `false` is also returned if session topology is not known yet.
	fn is_not_validator(&self) -> bool {
		self.grid_view.is_some() && self.local_validator.is_none()
	}
}

pub(crate) struct State {
	/// The utility for managing the implicit and explicit views in a consistent way.
	implicit_view: ImplicitView,
	candidates: Candidates,
	per_scheduling_parent: HashMap<Hash, PerSchedulingParentState>,
	per_session: HashMap<SessionIndex, PerSessionState>,
	// Topology might be received before first leaf update, where we
	// initialize the per_session_state, so cache it here until we
	// are able to use it.
	unused_topologies: HashMap<SessionIndex, NewGossipTopology>,
	peers: HashMap<PeerId, PeerState>,
	keystore: KeystorePtr,
	authorities: HashMap<AuthorityDiscoveryId, PeerId>,
	request_manager: RequestManager,
	response_manager: ResponseManager,
}

impl State {
	/// Create a new state.
	pub(crate) fn new(keystore: KeystorePtr) -> Self {
		State {
			implicit_view: Default::default(),
			candidates: Default::default(),
			per_scheduling_parent: HashMap::new(),
			per_session: HashMap::new(),
			peers: HashMap::new(),
			keystore,
			authorities: HashMap::new(),
			request_manager: RequestManager::new(),
			response_manager: ResponseManager::new(),
			unused_topologies: HashMap::new(),
		}
	}

	pub(crate) fn request_and_response_managers(
		&mut self,
	) -> (&mut RequestManager, &mut ResponseManager) {
		(&mut self.request_manager, &mut self.response_manager)
	}
}

// For the provided validator index, if there is a connected peer controlling the given authority
// ID, then return that peer's `PeerId`.
fn connected_validator_peer(
	authorities: &HashMap<AuthorityDiscoveryId, PeerId>,
	per_session: &PerSessionState,
	validator_index: ValidatorIndex,
) -> Option<PeerId> {
	per_session
		.session_info
		.discovery_keys
		.get(validator_index.0 as usize)
		.and_then(|k| authorities.get(k))
		.map(|p| *p)
}

struct PeerState {
	view: View,
	protocol_version: ValidationVersion,
	implicit_view: HashSet<Hash>,
	discovery_ids: Option<HashSet<AuthorityDiscoveryId>>,
}

impl PeerState {
	// Update the view, returning a vector of implicit scheduling-parents which weren't previously
	// part of the view.
	fn update_view(&mut self, new_view: View, local_implicit: &ImplicitView) -> Vec<Hash> {
		let next_implicit = new_view
			.iter()
			.flat_map(|x| local_implicit.known_allowed_relay_parents_under(x))
			.flatten()
			.cloned()
			.collect::<HashSet<_>>();

		let fresh_implicit = next_implicit
			.iter()
			.filter(|x| !self.implicit_view.contains(x))
			.cloned()
			.collect();

		self.view = new_view;
		self.implicit_view = next_implicit;

		fresh_implicit
	}

	// Attempt to reconcile the view with new information about the implicit scheduling parents
	// under an active leaf.
	fn reconcile_active_leaf(&mut self, leaf_hash: Hash, implicit: &[Hash]) -> Vec<Hash> {
		if !self.view.contains(&leaf_hash) {
			return Vec::new();
		}

		let mut v = Vec::with_capacity(implicit.len());
		for i in implicit {
			if self.implicit_view.insert(*i) {
				v.push(*i);
			}
		}
		v
	}

	// Whether we know that a peer knows a relay-parent.
	// The peer knows the relay-parent if it is either implicit or explicit
	// in their view. However, if it is implicit via an active-leaf we don't
	// recognize, we will not accurately be able to recognize them as 'knowing'
	// the relay-parent.
	fn knows_relay_parent(&self, relay_parent: &Hash) -> bool {
		self.implicit_view.contains(relay_parent) || self.view.contains(relay_parent)
	}

	fn is_authority(&self, authority_id: &AuthorityDiscoveryId) -> bool {
		self.discovery_ids.as_ref().map_or(false, |x| x.contains(authority_id))
	}

	fn iter_known_discovery_ids(&self) -> impl Iterator<Item = &AuthorityDiscoveryId> {
		self.discovery_ids.as_ref().into_iter().flatten()
	}
}

#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
pub(crate) async fn handle_network_update<Context>(
	ctx: &mut Context,
	state: &mut State,
	update: NetworkBridgeEvent<net_protocol::StatementDistributionMessage>,
	reputation: &mut ReputationAggregator,
	metrics: &Metrics,
) {
	match update {
		NetworkBridgeEvent::PeerConnected(peer_id, role, protocol_version, mut authority_ids) => {
			gum::trace!(target: LOG_TARGET, ?peer_id, ?role, ?protocol_version, "Peer connected");

			let versioned_protocol = if protocol_version != ValidationVersion::V3.into() {
				return;
			} else {
				protocol_version.try_into().expect("Qed, we checked above")
			};

			if let Some(ref mut authority_ids) = authority_ids {
				authority_ids.retain(|a| match state.authorities.entry(a.clone()) {
					Entry::Vacant(e) => {
						e.insert(peer_id);
						true
					},
					Entry::Occupied(e) => {
						gum::debug!(
							target: LOG_TARGET,
							authority_id = ?a,
							existing_peer = ?e.get(),
							new_peer = ?peer_id,
							"Ignoring new peer with duplicate authority ID as a bearer of that identity"
						);

						false
					},
				});
			}

			state.peers.insert(
				peer_id,
				PeerState {
					view: View::default(),
					implicit_view: HashSet::new(),
					protocol_version: versioned_protocol,
					discovery_ids: authority_ids,
				},
			);
		},
		NetworkBridgeEvent::PeerDisconnected(peer_id) => {
			if let Some(p) = state.peers.remove(&peer_id) {
				for discovery_key in p.discovery_ids.into_iter().flatten() {
					state.authorities.remove(&discovery_key);
				}
			}
		},
		NetworkBridgeEvent::NewGossipTopology(topology) => {
			let new_session_index = &topology.session;
			let new_topology = &topology.topology;
			let local_index = &topology.local_index;

			if let Some(per_session) = state.per_session.get_mut(new_session_index) {
				per_session.supply_topology(new_topology, *local_index);
			} else {
				state.unused_topologies.insert(*new_session_index, topology);
			}

			// TODO [https://github.com/paritytech/polkadot/issues/6194]
			// technically, we should account for the fact that the session topology might
			// come late, and for all scheduling-parents with this session, send all grid peers
			// any `BackedCandidateInv` messages they might need.
			//
			// in practice, this is a small issue & the API of receiving topologies could
			// be altered to fix it altogether.
		},
		NetworkBridgeEvent::PeerMessage(peer_id, message) => match message {
			net_protocol::StatementDistributionMessage::V3(
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, statement),
			) => {
				handle_incoming_statement(
					ctx,
					state,
					peer_id,
					relay_parent,
					statement,
					reputation,
					metrics,
				)
				.await
			},
			net_protocol::StatementDistributionMessage::V3(
				protocol_v3::StatementDistributionMessage::BackedCandidateManifest(inner),
			) => handle_incoming_manifest(ctx, state, peer_id, inner, reputation, metrics).await,
			net_protocol::StatementDistributionMessage::V3(
				protocol_v3::StatementDistributionMessage::BackedCandidateKnown(inner),
			) => {
				handle_incoming_acknowledgement(ctx, state, peer_id, inner, reputation, metrics)
					.await
			},
		},
		NetworkBridgeEvent::PeerViewChange(peer_id, view) => {
			handle_peer_view_update(ctx, state, peer_id, view, metrics).await
		},
		NetworkBridgeEvent::OurViewChange(_view) => {
			// handled by `handle_activated_leaf`
		},
		NetworkBridgeEvent::UpdatedAuthorityIds(peer_id, authority_ids) => {
			gum::trace!(
				target: LOG_TARGET,
				?peer_id,
				?authority_ids,
				"Updated `AuthorityDiscoveryId`s"
			);

			// defensive: ensure peers are actually connected
			let peer_state = match state.peers.get_mut(&peer_id) {
				None => return,
				Some(p) => p,
			};

			// Remove the authority IDs which were previously mapped to the peer
			// but aren't part of the new set.
			state.authorities.retain(|a, p| p != &peer_id || authority_ids.contains(a));

			// Map the new authority IDs to the peer.
			for a in authority_ids.iter().cloned() {
				state.authorities.insert(a, peer_id);
			}

			peer_state.discovery_ids = Some(authority_ids);
		},
	}
}

#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn handle_active_leaf_update<Context>(
	ctx: &mut Context,
	state: &mut State,
	new_scheduling_parent: Hash,
) -> JfyiErrorResult<()> {
	let disabled_validators: HashSet<_> =
		polkadot_node_subsystem_util::request_disabled_validators(
			new_scheduling_parent,
			ctx.sender(),
		)
		.await
		.await
		.map_err(JfyiError::RuntimeApiUnavailable)?
		.map_err(JfyiError::FetchDisabledValidators)?
		.into_iter()
		.collect();

	let session_index = polkadot_node_subsystem_util::request_session_index_for_child(
		new_scheduling_parent,
		ctx.sender(),
	)
	.await
	.await
	.map_err(JfyiError::RuntimeApiUnavailable)?
	.map_err(JfyiError::FetchSessionIndex)?;

	if !state.per_session.contains_key(&session_index) {
		let session_info = polkadot_node_subsystem_util::request_session_info(
			new_scheduling_parent,
			session_index,
			ctx.sender(),
		)
		.await
		.await
		.map_err(JfyiError::RuntimeApiUnavailable)?
		.map_err(JfyiError::FetchSessionInfo)?;

		let session_info = match session_info {
			None => {
				gum::warn!(
					target: LOG_TARGET,
					scheduling_parent = ?new_scheduling_parent,
					"No session info available for current session"
				);

				return Ok(());
			},
			Some(s) => s,
		};

		let minimum_backing_votes =
			request_min_backing_votes(new_scheduling_parent, session_index, ctx.sender())
				.await
				.await
				.map_err(JfyiError::RuntimeApiUnavailable)?
				.map_err(JfyiError::FetchMinimumBackingVotes)?;
		let mut per_session_state =
			PerSessionState::new(session_info, &state.keystore, minimum_backing_votes);
		if let Some(topology) = state.unused_topologies.remove(&session_index) {
			per_session_state.supply_topology(&topology.topology, topology.local_index);
		}
		state.per_session.insert(session_index, per_session_state);
	}

	let per_session = state
		.per_session
		.get_mut(&session_index)
		.expect("either existed or just inserted; qed");

	if !disabled_validators.is_empty() {
		gum::debug!(
			target: LOG_TARGET,
			scheduling_parent = ?new_scheduling_parent,
			?session_index,
			?disabled_validators,
			"Disabled validators detected"
		);
	}

	let group_rotation_info =
		polkadot_node_subsystem_util::request_validator_groups(new_scheduling_parent, ctx.sender())
			.await
			.await
			.map_err(JfyiError::RuntimeApiUnavailable)?
			.map_err(JfyiError::FetchValidatorGroups)?
			.1;

	let claim_queue = ClaimQueueSnapshot(
		polkadot_node_subsystem_util::request_claim_queue(new_scheduling_parent, ctx.sender())
			.await
			.await
			.map_err(JfyiError::RuntimeApiUnavailable)?
			.map_err(JfyiError::FetchClaimQueue)?,
	);

	let (groups_per_para, assignments_per_group) = determine_group_assignments(
		per_session.groups.all().len(),
		group_rotation_info,
		&claim_queue,
	)
	.await;

	let local_validator = per_session.local_validator.and_then(|v| {
		if let LocalValidatorIndex::Active(idx) = v {
			find_active_validator_state(idx, &per_session.groups, &assignments_per_group)
		} else {
			Some(LocalValidatorState { grid_tracker: GridTracker::default(), active: None })
		}
	});

	let transposed_cq = transpose_claim_queue(claim_queue.0);

	state.per_scheduling_parent.insert(
		new_scheduling_parent,
		PerSchedulingParentState {
			local_validator,
			statement_store: StatementStore::new(&per_session.groups),
			session: session_index,
			groups_per_para,
			disabled_validators,
			transposed_cq,
			assignments_per_group,
		},
	);
	Ok(())
}

/// Called on new leaf updates.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
pub(crate) async fn handle_active_leaves_update<Context>(
	ctx: &mut Context,
	state: &mut State,
	activated: &ActivatedLeaf,
	metrics: &Metrics,
) -> JfyiErrorResult<()> {
	state
		.implicit_view
		.activate_leaf(ctx.sender(), activated.hash)
		.await
		.map_err(JfyiError::ActivateLeafFailure)?;

	let new_scheduling_parents =
		state.implicit_view.all_allowed_relay_parents().cloned().collect::<Vec<_>>();

	for new_scheduling_parent in new_scheduling_parents.iter().cloned() {
		if state.per_scheduling_parent.contains_key(&new_scheduling_parent) {
			continue;
		}

		if let Err(err) = handle_active_leaf_update(ctx, state, new_scheduling_parent).await {
			gum::warn!(
				target: LOG_TARGET,
				scheduling_parent = ?new_scheduling_parent,
				error = ?err,
				"Failed to handle active leaf update"
			);
			continue;
		}
	}

	gum::debug!(
		target: LOG_TARGET,
		"Activated leaves. Now tracking {} scheduling-parents across {} sessions",
		state.per_scheduling_parent.len(),
		state.per_session.len(),
	);

	// Reconcile all peers' views with the active leaf and any scheduling parents
	// it implies. If they learned about the block before we did, this reconciliation will give
	// non-empty results and we should send them messages concerning all activated
	// scheduling-parents.
	{
		let mut update_peers = Vec::new();
		for (peer, peer_state) in state.peers.iter_mut() {
			let fresh = peer_state.reconcile_active_leaf(activated.hash, &new_scheduling_parents);
			if !fresh.is_empty() {
				update_peers.push((*peer, fresh));
			}
		}

		for (peer, fresh) in update_peers {
			for fresh_scheduling_parent in fresh {
				send_peer_messages_for_scheduling_parent(
					ctx,
					state,
					peer,
					fresh_scheduling_parent,
					metrics,
				)
				.await;
			}
		}
	}

	new_leaf_fragment_chain_updates(ctx, state, activated.hash).await;

	Ok(())
}

fn find_active_validator_state(
	validator_index: ValidatorIndex,
	groups: &Groups,
	assignments_per_group: &HashMap<GroupIndex, Vec<ParaId>>,
) -> Option<LocalValidatorState> {
	if groups.all().is_empty() {
		return None;
	}

	let our_group = groups.by_validator_index(validator_index)?;

	let group_validators = groups.get(our_group)?.to_owned();
	let paras_assigned_to_core = assignments_per_group.get(&our_group).cloned().unwrap_or_default();
	let seconding_limit = paras_assigned_to_core.len();

	Some(LocalValidatorState {
		active: Some(ActiveValidatorState {
			index: validator_index,
			group: our_group,
			cluster_tracker: ClusterTracker::new(group_validators, seconding_limit)
				.expect("group is non-empty because we are in it; qed"),
			assignments: paras_assigned_to_core.clone(),
		}),
		grid_tracker: GridTracker::default(),
	})
}

pub(crate) fn handle_deactivate_leaves(state: &mut State, leaves: &[Hash]) {
	// deactivate the leaf in the implicit view.
	for leaf in leaves {
		let pruned = state.implicit_view.deactivate_leaf(*leaf);
		for pruned_rp in pruned {
			// clean up per-scheduling-parent data based on everything removed.
			state
				.per_scheduling_parent
				.remove(&pruned_rp)
				.as_ref()
				.and_then(|pruned| pruned.active_validator_state())
				.map(|active_state| {
					active_state.cluster_tracker.warn_if_too_many_pending_statements(pruned_rp)
				});

			// clean up requests related to this scheduling parent.
			state.request_manager.remove_by_scheduling_parent(pruned_rp);
		}
	}

	state
		.candidates
		.on_deactivate_leaves(&leaves, |h| state.per_scheduling_parent.contains_key(h));

	// clean up sessions based on everything remaining.
	let sessions: HashSet<_> = state.per_scheduling_parent.values().map(|r| r.session).collect();
	state.per_session.retain(|s, _| sessions.contains(s));

	let last_session_index = state.unused_topologies.keys().max().copied();
	// Do not clean-up the last saved toplogy unless we moved to the next session
	// This is needed because handle_deactive_leaves, gets also called when
	// prospective_parachains APIs are not present, so we would actually remove
	// the topology without using it because `per_scheduling_parent` is empty until
	// prospective_parachains gets enabled
	state
		.unused_topologies
		.retain(|s, _| sessions.contains(s) || last_session_index == Some(*s));
}

#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn handle_peer_view_update<Context>(
	ctx: &mut Context,
	state: &mut State,
	peer: PeerId,
	new_view: View,
	metrics: &Metrics,
) {
	let fresh_implicit = {
		let peer_data = match state.peers.get_mut(&peer) {
			None => return,
			Some(p) => p,
		};

		peer_data.update_view(new_view, &state.implicit_view)
	};

	for new_scheduling_parent in fresh_implicit {
		send_peer_messages_for_scheduling_parent(ctx, state, peer, new_scheduling_parent, metrics)
			.await;
	}
}

// Returns an iterator over known validator indices, given an iterator over discovery IDs
// and a mapping from discovery IDs to validator indices.
fn find_validator_ids<'a>(
	known_discovery_ids: impl IntoIterator<Item = &'a AuthorityDiscoveryId>,
	discovery_mapping: impl Fn(&AuthorityDiscoveryId) -> Option<&'a ValidatorIndex>,
) -> impl Iterator<Item = ValidatorIndex> {
	known_discovery_ids.into_iter().filter_map(discovery_mapping).cloned()
}

/// Send a peer, apparently just becoming aware of a relay-parent, all messages
/// concerning that relay-parent.
///
/// In particular, we send all statements pertaining to our common cluster,
/// as well as all manifests, acknowledgements, or other grid statements.
///
/// Note that due to the way we handle views, our knowledge of peers' scheduling parents
/// may "oscillate" with scheduling parents repeatedly leaving and entering the
/// view of a peer based on the implicit view of active leaves.
///
/// This function is designed to be cheap and not to send duplicate messages in repeated
/// cases.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn send_peer_messages_for_scheduling_parent<Context>(
	ctx: &mut Context,
	state: &mut State,
	peer: PeerId,
	scheduling_parent: Hash,
	metrics: &Metrics,
) {
	let peer_data = match state.peers.get_mut(&peer) {
		None => return,
		Some(p) => p,
	};

	let scheduling_parent_state = match state.per_scheduling_parent.get_mut(&scheduling_parent) {
		None => return,
		Some(s) => s,
	};

	let per_session_state = match state.per_session.get(&scheduling_parent_state.session) {
		None => return,
		Some(s) => s,
	};

	for validator_id in find_validator_ids(peer_data.iter_known_discovery_ids(), |a| {
		per_session_state.authority_lookup.get(a)
	}) {
		if let Some(active) = scheduling_parent_state
			.local_validator
			.as_mut()
			.and_then(|local| local.active.as_mut())
		{
			send_pending_cluster_statements(
				ctx,
				scheduling_parent,
				&(peer, peer_data.protocol_version),
				validator_id,
				&mut active.cluster_tracker,
				&state.candidates,
				&scheduling_parent_state.statement_store,
				metrics,
			)
			.await;
		}

		send_pending_grid_messages(
			ctx,
			scheduling_parent,
			&(peer, peer_data.protocol_version),
			validator_id,
			&per_session_state.groups,
			scheduling_parent_state,
			&state.candidates,
			metrics,
		)
		.await;
	}
}

fn pending_statement_network_message(
	statement_store: &StatementStore,
	relay_parent: Hash,
	peer: &(PeerId, ValidationVersion),
	originator: ValidatorIndex,
	compact: CompactStatement,
) -> Option<(Vec<PeerId>, net_protocol::VersionedValidationProtocol)> {
	match peer.1 {
		ValidationVersion::V3 => statement_store
			.validator_statement(originator, compact)
			.map(|s| s.as_unchecked().clone())
			.map(|signed| {
				protocol_v3::StatementDistributionMessage::Statement(relay_parent, signed)
			})
			.map(|msg| (vec![peer.0], ValidationProtocols::V3(msg).into())),
	}
}

/// Send a peer all pending cluster statements for a scheduling parent.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn send_pending_cluster_statements<Context>(
	ctx: &mut Context,
	relay_parent: Hash,
	peer_id: &(PeerId, ValidationVersion),
	peer_validator_id: ValidatorIndex,
	cluster_tracker: &mut ClusterTracker,
	candidates: &Candidates,
	statement_store: &StatementStore,
	metrics: &Metrics,
) {
	let pending_statements = cluster_tracker.pending_statements_for(peer_validator_id);
	let network_messages = pending_statements
		.into_iter()
		.filter_map(|(originator, compact)| {
			if !candidates.is_confirmed(compact.candidate_hash()) {
				return None;
			}

			let res = pending_statement_network_message(
				&statement_store,
				relay_parent,
				peer_id,
				originator,
				compact.clone(),
			);

			if res.is_some() {
				cluster_tracker.note_sent(peer_validator_id, originator, compact);
			}

			res
		})
		.collect::<Vec<_>>();

	if !network_messages.is_empty() {
		let count = network_messages.len();
		ctx.send_message(NetworkBridgeTxMessage::SendValidationMessages(network_messages))
			.await;
		metrics.on_statements_distributed(count);
	}
}

/// Send a peer all pending grid messages / acknowledgements / follow up statements
/// upon learning about a new scheduling parent.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn send_pending_grid_messages<Context>(
	ctx: &mut Context,
	scheduling_parent: Hash,
	peer_id: &(PeerId, ValidationVersion),
	peer_validator_id: ValidatorIndex,
	groups: &Groups,
	scheduling_parent_state: &mut PerSchedulingParentState,
	candidates: &Candidates,
	metrics: &Metrics,
) {
	let pending_manifests = {
		let local_validator = match scheduling_parent_state.local_validator.as_mut() {
			None => return,
			Some(l) => l,
		};

		let grid_tracker = &mut local_validator.grid_tracker;
		grid_tracker.pending_manifests_for(peer_validator_id)
	};

	let mut messages: Vec<(Vec<PeerId>, net_protocol::VersionedValidationProtocol)> = Vec::new();
	let mut statements_count = 0;
	for (candidate_hash, kind) in pending_manifests {
		let confirmed_candidate = match candidates.get_confirmed(&candidate_hash) {
			None => continue, // sanity
			Some(c) => c,
		};

		let group_index = confirmed_candidate.group_index();

		let local_knowledge = {
			let group_size = match groups.get(group_index) {
				None => return, // sanity
				Some(x) => x.len(),
			};

			local_knowledge_filter(
				group_size,
				group_index,
				candidate_hash,
				&scheduling_parent_state.statement_store,
			)
		};

		match kind {
			grid::ManifestKind::Full => {
				let manifest = protocol_v3::BackedCandidateManifest {
					scheduling_parent,
					candidate_hash,
					group_index,
					para_id: confirmed_candidate.para_id(),
					parent_head_data_hash: confirmed_candidate.parent_head_data_hash(),
					statement_knowledge: local_knowledge.clone(),
				};

				let grid = &mut scheduling_parent_state
					.local_validator
					.as_mut()
					.expect("determined to be some earlier in this function; qed")
					.grid_tracker;

				grid.manifest_sent_to(
					groups,
					peer_validator_id,
					candidate_hash,
					local_knowledge.clone(),
				);
				match peer_id.1 {
					ValidationVersion::V3 => messages.push((
						vec![peer_id.0],
						ValidationProtocols::V3(
							protocol_v3::StatementDistributionMessage::BackedCandidateManifest(
								manifest,
							),
						)
						.into(),
					)),
				};
			},
			grid::ManifestKind::Acknowledgement => {
				let (m, c) = acknowledgement_and_statement_messages(
					peer_id,
					peer_validator_id,
					groups,
					scheduling_parent_state,
					scheduling_parent,
					group_index,
					candidate_hash,
					local_knowledge,
				);
				messages.extend(m);
				statements_count += c;
			},
		}
	}

	// Send all remaining pending grid statements for a validator, not just
	// those for the acknowledgements we've sent.
	//
	// otherwise, we might receive statements while the grid peer is "out of view" and then
	// not send them when they get back "in view". problem!
	{
		let grid_tracker = &mut scheduling_parent_state
			.local_validator
			.as_mut()
			.expect("checked earlier; qed")
			.grid_tracker;

		let pending_statements = grid_tracker.all_pending_statements_for(peer_validator_id);

		let extra_statements = pending_statements
			.into_iter()
			.filter_map(|(originator, compact)| {
				let res = pending_statement_network_message(
					&scheduling_parent_state.statement_store,
					scheduling_parent,
					peer_id,
					originator,
					compact.clone(),
				);

				if res.is_some() {
					grid_tracker.sent_or_received_direct_statement(
						groups,
						originator,
						peer_validator_id,
						&compact,
						false,
					);
				}

				res
			})
			.collect::<Vec<_>>();

		statements_count += extra_statements.len();
		messages.extend(extra_statements);
	}

	if !messages.is_empty() {
		ctx.send_message(NetworkBridgeTxMessage::SendValidationMessages(messages)).await;
		metrics.on_statements_distributed(statements_count);
	}
}

// Imports a locally originating statement and distributes it to peers.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
pub(crate) async fn share_local_statement<Context>(
	ctx: &mut Context,
	state: &mut State,
	scheduling_parent: Hash,
	statement: SignedFullStatementWithPVD,
	reputation: &mut ReputationAggregator,
	metrics: &Metrics,
) -> JfyiErrorResult<()> {
	let per_scheduling_parent = match state.per_scheduling_parent.get_mut(&scheduling_parent) {
		None => return Err(JfyiError::InvalidShare),
		Some(x) => x,
	};

	gum::debug!(
		target: LOG_TARGET,
		statement = ?statement.payload().to_compact(),
		"Sharing Statement",
	);

	let per_session = match state.per_session.get(&per_scheduling_parent.session) {
		Some(s) => s,
		None => return Ok(()),
	};

	let (local_index, local_assignments, local_group) =
		match per_scheduling_parent.active_validator_state() {
			None => return Err(JfyiError::InvalidShare),
			Some(l) => (l.index, &l.assignments, l.group),
		};

	// Two possibilities: either the statement is `Seconded` or we already
	// have the candidate. Sanity: check the para-id is valid.
	let expected = match statement.payload() {
		FullStatementWithPVD::Seconded(ref c, _) => {
			Some((c.descriptor.para_id(), c.descriptor.scheduling_parent()))
		},
		FullStatementWithPVD::Valid(hash) => state
			.candidates
			.get_confirmed(&hash)
			.map(|c| (c.para_id(), c.scheduling_parent())),
	};

	let is_seconded = match statement.payload() {
		FullStatementWithPVD::Seconded(_, _) => true,
		FullStatementWithPVD::Valid(_) => false,
	};

	let (expected_para, expected_scheduling_parent) = match expected {
		None => return Err(JfyiError::InvalidShare),
		Some(x) => x,
	};

	if local_index != statement.validator_index() {
		return Err(JfyiError::InvalidShare);
	}

	let seconding_limit = local_assignments.len();

	if is_seconded &&
		per_scheduling_parent.statement_store.seconded_count(&local_index) >= seconding_limit
	{
		gum::warn!(
			target: LOG_TARGET,
			limit = ?seconding_limit,
			"Local node has issued too many `Seconded` statements",
		);
		return Err(JfyiError::InvalidShare);
	}

	if !local_assignments.contains(&expected_para) ||
		scheduling_parent != expected_scheduling_parent
	{
		return Err(JfyiError::InvalidShare);
	}

	let mut post_confirmation = None;

	// Insert candidate if unknown + more sanity checks.
	let compact_statement = {
		let compact_statement = FullStatementWithPVD::signed_to_compact(statement.clone());
		let candidate_hash = CandidateHash(*statement.payload().candidate_hash());

		if let FullStatementWithPVD::Seconded(ref c, ref pvd) = statement.payload() {
			post_confirmation = state.candidates.confirm_candidate(
				candidate_hash,
				c.clone(),
				pvd.clone(),
				local_group,
			);
		};

		match per_scheduling_parent.statement_store.insert(
			&per_session.groups,
			compact_statement.clone(),
			StatementOrigin::Local,
		) {
			Ok(false) | Err(_) => {
				gum::warn!(
					target: LOG_TARGET,
					statement = ?compact_statement.payload(),
					"Candidate backing issued redundant statement?",
				);
				return Err(JfyiError::InvalidShare);
			},
			Ok(true) => {},
		}

		{
			let l = per_scheduling_parent.active_validator_state_mut().expect("checked above; qed");
			l.cluster_tracker.note_issued(local_index, compact_statement.payload().clone());
		}

		if let Some(ref session_topology) = per_session.grid_view {
			let l = per_scheduling_parent.local_validator.as_mut().expect("checked above; qed");
			l.grid_tracker.learned_fresh_statement(
				&per_session.groups,
				session_topology,
				local_index,
				&compact_statement.payload(),
			);
		}

		compact_statement
	};

	// send the compact version of the statement to any peers which need it.
	circulate_statement(
		ctx,
		scheduling_parent,
		per_scheduling_parent,
		per_session,
		&state.candidates,
		&state.authorities,
		&state.peers,
		compact_statement,
		metrics,
	)
	.await;

	if let Some(post_confirmation) = post_confirmation {
		apply_post_confirmation(ctx, state, post_confirmation, reputation, metrics).await;
	}

	Ok(())
}

// two kinds of targets: those in our 'cluster' (currently just those in the same group),
// and those we are propagating to through the grid.
#[derive(Debug)]
enum DirectTargetKind {
	Cluster,
	Grid,
}

// Circulates a compact statement to all peers who need it: those in the current group of the
// local validator and grid peers which have already indicated that they know the candidate as
// backed.
//
// We only circulate statements for which we have the confirmed candidate, even to the local group.
//
// The group index which is _canonically assigned_ to this parachain must be
// specified already. This function should not be used when the candidate receipt and
// therefore the canonical group for the parachain is unknown.
//
// preconditions: the candidate entry exists in the state under the scheduling parent
// and the statement has already been imported into the entry. If this is a `Valid`
// statement, then there must be at least one `Seconded` statement.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn circulate_statement<Context>(
	ctx: &mut Context,
	scheduling_parent: Hash,
	scheduling_parent_state: &mut PerSchedulingParentState,
	per_session: &PerSessionState,
	candidates: &Candidates,
	authorities: &HashMap<AuthorityDiscoveryId, PeerId>,
	peers: &HashMap<PeerId, PeerState>,
	statement: SignedStatement,
	metrics: &Metrics,
) {
	let session_info = &per_session.session_info;

	let candidate_hash = *statement.payload().candidate_hash();

	let compact_statement = statement.payload().clone();
	let is_confirmed = candidates.is_confirmed(&candidate_hash);

	let originator = statement.validator_index();
	let (local_validator, targets) = {
		let local_validator = match scheduling_parent_state.local_validator.as_mut() {
			Some(v) => v,
			None => return, // sanity: nothing to propagate if not a validator.
		};

		let statement_group = per_session.groups.by_validator_index(originator);

		// We're not meant to circulate statements in the cluster until we have the confirmed
		// candidate.
		//
		// Cluster is only relevant if local node is an active validator.
		let (cluster_relevant, cluster_targets, all_cluster_targets) = local_validator
			.active
			.as_mut()
			.map(|active| {
				let cluster_relevant = Some(active.group) == statement_group;
				let cluster_targets = if is_confirmed && cluster_relevant {
					Some(
						active
							.cluster_tracker
							.targets()
							.iter()
							.filter(|&&v| {
								active
									.cluster_tracker
									.can_send(v, originator, compact_statement.clone())
									.is_ok()
							})
							.filter(|&v| v != &active.index)
							.map(|v| (*v, DirectTargetKind::Cluster)),
					)
				} else {
					None
				};
				let all_cluster_targets = active.cluster_tracker.targets();
				(cluster_relevant, cluster_targets, all_cluster_targets)
			})
			.unwrap_or((false, None, &[]));

		let grid_targets = local_validator
			.grid_tracker
			.direct_statement_targets(&per_session.groups, originator, &compact_statement)
			.into_iter()
			.filter(|v| !cluster_relevant || !all_cluster_targets.contains(v))
			.map(|v| (v, DirectTargetKind::Grid));

		let targets = cluster_targets
			.into_iter()
			.flatten()
			.chain(grid_targets)
			.filter_map(|(v, k)| {
				session_info.discovery_keys.get(v.0 as usize).map(|a| (v, a.clone(), k))
			})
			.collect::<Vec<_>>();

		(local_validator, targets)
	};

	let mut statement_to_peers: Vec<(PeerId, ProtocolVersion)> = Vec::new();
	for (target, authority_id, kind) in targets {
		// Find peer ID based on authority ID, and also filter to connected.
		let peer_id: (PeerId, ProtocolVersion) = match authorities.get(&authority_id) {
			Some(p) if peers.get(p).map_or(false, |p| p.knows_relay_parent(&scheduling_parent)) => {
				(
					*p,
					peers
						.get(p)
						.expect("Qed, can't fail because it was checked above")
						.protocol_version
						.into(),
				)
			},
			None | Some(_) => continue,
		};

		match kind {
			DirectTargetKind::Cluster => {
				let active = local_validator
					.active
					.as_mut()
					.expect("cluster target means local is active validator; qed");

				// At this point, all peers in the cluster should 'know'
				// the candidate, so we don't expect for this to fail.
				if let Ok(()) =
					active.cluster_tracker.can_send(target, originator, compact_statement.clone())
				{
					active.cluster_tracker.note_sent(target, originator, compact_statement.clone());
					statement_to_peers.push(peer_id);
				}
			},
			DirectTargetKind::Grid => {
				statement_to_peers.push(peer_id);
				local_validator.grid_tracker.sent_or_received_direct_statement(
					&per_session.groups,
					originator,
					target,
					&compact_statement,
					false,
				);
			},
		}
	}

	let statement_to_v3_peers =
		filter_by_peer_version(&statement_to_peers, ValidationVersion::V3.into());

	// ship off the network messages to the network bridge.
	if !statement_to_v3_peers.is_empty() {
		gum::debug!(
			target: LOG_TARGET,
			?compact_statement,
			n_peers = ?statement_to_peers.len(),
			"Sending statement to v3 peers",
		);

		ctx.send_message(NetworkBridgeTxMessage::SendValidationMessage(
			statement_to_v3_peers,
			ValidationProtocols::V3(protocol_v3::StatementDistributionMessage::Statement(
				scheduling_parent,
				statement.as_unchecked().clone(),
			))
			.into(),
		))
		.await;
		metrics.on_statement_distributed();
	}
}
/// Check a statement signature under this parent hash.
fn check_statement_signature(
	session_index: SessionIndex,
	validators: &IndexedVec<ValidatorIndex, ValidatorId>,
	scheduling_parent: Hash,
	statement: UncheckedSignedStatement,
) -> std::result::Result<SignedStatement, UncheckedSignedStatement> {
	let signing_context = SigningContext { session_index, parent_hash: scheduling_parent };

	validators
		.get(statement.unchecked_validator_index())
		.ok_or_else(|| statement.clone())
		.and_then(|v| statement.try_into_checked(&signing_context, v))
}

/// Modify the reputation of a peer based on its behavior.
async fn modify_reputation(
	reputation: &mut ReputationAggregator,
	sender: &mut impl overseer::StatementDistributionSenderTrait,
	peer: PeerId,
	rep: Rep,
) {
	reputation.modify(sender, peer, rep).await;
}

/// Handle an incoming statement.
///
/// This checks whether the sender is allowed to send the statement,
/// either via the cluster or the grid.
///
/// This also checks the signature of the statement.
/// If the statement is fresh, this function guarantees that after completion
///   - The statement is re-circulated to all relevant peers in both the cluster and the grid
///   - If the candidate is out-of-cluster and is backable and importable, all statements about the
///     candidate have been sent to backing
///   - If the candidate is in-cluster and is importable, the statement has been sent to backing
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn handle_incoming_statement<Context>(
	ctx: &mut Context,
	state: &mut State,
	peer: PeerId,
	scheduling_parent: Hash,
	statement: UncheckedSignedStatement,
	reputation: &mut ReputationAggregator,
	metrics: &Metrics,
) {
	let peer_state = match state.peers.get(&peer) {
		None => {
			// sanity: should be impossible.
			return;
		},
		Some(p) => p,
	};

	// Ensure we know the scheduling parent.
	let per_scheduling_parent = match state.per_scheduling_parent.get_mut(&scheduling_parent) {
		None => {
			modify_reputation(
				reputation,
				ctx.sender(),
				peer,
				COST_UNEXPECTED_STATEMENT_MISSING_KNOWLEDGE,
			)
			.await;
			return;
		},
		Some(p) => p,
	};

	let per_session = match state.per_session.get(&per_scheduling_parent.session) {
		None => {
			gum::warn!(
				target: LOG_TARGET,
				session = ?per_scheduling_parent.session,
				"Missing expected session info.",
			);

			return;
		},
		Some(s) => s,
	};
	let session_info = &per_session.session_info;

	if per_scheduling_parent.is_disabled(&statement.unchecked_validator_index()) {
		gum::debug!(
			target: LOG_TARGET,
			?scheduling_parent,
			validator_index = ?statement.unchecked_validator_index(),
			"Ignoring a statement from disabled validator."
		);
		modify_reputation(reputation, ctx.sender(), peer, COST_DISABLED_VALIDATOR).await;
		return;
	}

	let local_validator = match per_scheduling_parent.local_validator.as_mut() {
		None => {
			// we shouldn't be receiving statements unless we're a validator
			// this session.
			if per_session.is_not_validator() {
				modify_reputation(
					reputation,
					ctx.sender(),
					peer,
					COST_UNEXPECTED_STATEMENT_NOT_VALIDATOR,
				)
				.await;
			}
			return;
		},
		Some(l) => l,
	};

	let originator_group =
		match per_session.groups.by_validator_index(statement.unchecked_validator_index()) {
			Some(g) => g,
			None => {
				modify_reputation(
					reputation,
					ctx.sender(),
					peer,
					COST_UNEXPECTED_STATEMENT_VALIDATOR_NOT_FOUND,
				)
				.await;
				return;
			},
		};

	let (active, cluster_sender_index) = {
		// This block of code only returns `Some` when both the originator and
		// the sending peer are in the cluster.
		let active = local_validator.active.as_mut();

		let allowed_senders = active
			.as_ref()
			.map(|active| {
				active
					.cluster_tracker
					.senders_for_originator(statement.unchecked_validator_index())
			})
			.unwrap_or_default();

		let idx = allowed_senders
			.iter()
			.filter_map(|i| session_info.discovery_keys.get(i.0 as usize).map(|ad| (*i, ad)))
			.filter(|(_, ad)| peer_state.is_authority(ad))
			.map(|(i, _)| i)
			.next();
		(active, idx)
	};

	let checked_statement = if let Some((active, cluster_sender_index)) =
		active.zip(cluster_sender_index)
	{
		match handle_cluster_statement(
			scheduling_parent,
			&mut active.cluster_tracker,
			per_scheduling_parent.session,
			&per_session.session_info,
			statement,
			cluster_sender_index,
		) {
			Ok(Some(s)) => s,
			Ok(None) => return,
			Err(rep) => {
				modify_reputation(reputation, ctx.sender(), peer, rep).await;
				return;
			},
		}
	} else {
		let grid_sender_index = local_validator
			.grid_tracker
			.direct_statement_providers(
				&per_session.groups,
				statement.unchecked_validator_index(),
				statement.unchecked_payload(),
			)
			.into_iter()
			.filter_map(|(i, validator_knows_statement)| {
				session_info
					.discovery_keys
					.get(i.0 as usize)
					.map(|ad| (i, ad, validator_knows_statement))
			})
			.filter(|(_, ad, _)| peer_state.is_authority(ad))
			.map(|(i, _, validator_knows_statement)| (i, validator_knows_statement))
			.next();

		if let Some((grid_sender_index, validator_knows_statement)) = grid_sender_index {
			if !validator_knows_statement {
				match handle_grid_statement(
					scheduling_parent,
					&mut local_validator.grid_tracker,
					per_scheduling_parent.session,
					&per_session,
					statement,
					grid_sender_index,
				) {
					Ok(s) => s,
					Err(rep) => {
						modify_reputation(reputation, ctx.sender(), peer, rep).await;
						return;
					},
				}
			} else {
				// Reward the peer for sending us the statement
				modify_reputation(reputation, ctx.sender(), peer, BENEFIT_VALID_STATEMENT).await;
				return;
			}
		} else {
			// Not a cluster or grid peer.
			modify_reputation(
				reputation,
				ctx.sender(),
				peer,
				COST_UNEXPECTED_STATEMENT_INVALID_SENDER,
			)
			.await;
			return;
		}
	};

	let statement = checked_statement.payload().clone();
	let originator_index = checked_statement.validator_index();
	let candidate_hash = *checked_statement.payload().candidate_hash();

	// Insert an unconfirmed candidate entry if needed. Note that if the candidate is already
	// confirmed, this ensures that the assigned group of the originator matches the expected group
	// of the parachain.
	{
		let res = state.candidates.insert_unconfirmed(
			peer,
			candidate_hash,
			scheduling_parent,
			originator_group,
			None,
		);

		if let Err(BadAdvertisement) = res {
			modify_reputation(
				reputation,
				ctx.sender(),
				peer,
				COST_UNEXPECTED_STATEMENT_BAD_ADVERTISE,
			)
			.await;
			return;
		}
	}

	let confirmed = state.candidates.get_confirmed(&candidate_hash);
	let is_confirmed = state.candidates.is_confirmed(&candidate_hash);
	if !is_confirmed {
		// If the candidate is not confirmed, note that we should attempt
		// to request it from the given peer.
		let mut request_entry = state.request_manager.get_or_insert(
			scheduling_parent,
			candidate_hash,
			originator_group,
		);

		request_entry.add_peer(peer);

		// We only successfully accept statements from the grid on confirmed
		// candidates, therefore this check only passes if the statement is from the cluster
		request_entry.set_cluster_priority();
	}

	let was_fresh = match per_scheduling_parent.statement_store.insert(
		&per_session.groups,
		checked_statement.clone(),
		StatementOrigin::Remote,
	) {
		Err(statement_store::Error::ValidatorUnknown) => {
			// sanity: should never happen.
			gum::warn!(
				target: LOG_TARGET,
				?scheduling_parent,
				validator_index = ?originator_index,
				"Error - accepted message from unknown validator."
			);

			return;
		},
		Ok(known) => known,
	};

	if was_fresh {
		modify_reputation(reputation, ctx.sender(), peer, BENEFIT_VALID_STATEMENT_FIRST).await;
		let is_importable = state.candidates.is_importable(&candidate_hash);

		if let Some(ref session_topology) = per_session.grid_view {
			local_validator.grid_tracker.learned_fresh_statement(
				&per_session.groups,
				session_topology,
				originator_index,
				&statement,
			);
		}

		if let (true, &Some(confirmed)) = (is_importable, &confirmed) {
			send_backing_fresh_statements(
				ctx,
				candidate_hash,
				originator_group,
				&scheduling_parent,
				&mut *per_scheduling_parent,
				confirmed,
				per_session,
			)
			.await;
		}

		// We always circulate statements at this point.
		circulate_statement(
			ctx,
			scheduling_parent,
			per_scheduling_parent,
			per_session,
			&state.candidates,
			&state.authorities,
			&state.peers,
			checked_statement,
			metrics,
		)
		.await;
	} else {
		modify_reputation(reputation, ctx.sender(), peer, BENEFIT_VALID_STATEMENT).await;
	}
}

/// Checks whether a statement is allowed, whether the signature is accurate,
/// and importing into the cluster tracker if successful.
///
/// if successful, this returns a checked signed statement if it should be imported
/// or otherwise an error indicating a reputational fault.
fn handle_cluster_statement(
	scheduling_parent: Hash,
	cluster_tracker: &mut ClusterTracker,
	session: SessionIndex,
	session_info: &SessionInfo,
	statement: UncheckedSignedStatement,
	cluster_sender_index: ValidatorIndex,
) -> Result<Option<SignedStatement>, Rep> {
	// additional cluster checks.
	let should_import = {
		match cluster_tracker.can_receive(
			cluster_sender_index,
			statement.unchecked_validator_index(),
			statement.unchecked_payload().clone(),
		) {
			Ok(ClusterAccept::Ok) => true,
			Ok(ClusterAccept::WithPrejudice) => false,
			Err(ClusterRejectIncoming::ExcessiveSeconded) => return Err(COST_EXCESSIVE_SECONDED),
			Err(ClusterRejectIncoming::CandidateUnknown | ClusterRejectIncoming::Duplicate) => {
				return Err(COST_UNEXPECTED_STATEMENT_CLUSTER_REJECTED)
			},
			Err(ClusterRejectIncoming::NotInGroup) => {
				// sanity: shouldn't be possible; we already filtered this
				// out above.
				return Err(COST_UNEXPECTED_STATEMENT_NOT_IN_GROUP);
			},
		}
	};

	// Ensure the statement is correctly signed.
	let checked_statement = match check_statement_signature(
		session,
		&session_info.validators,
		scheduling_parent,
		statement,
	) {
		Ok(s) => s,
		Err(_) => return Err(COST_INVALID_SIGNATURE),
	};

	cluster_tracker.note_received(
		cluster_sender_index,
		checked_statement.validator_index(),
		checked_statement.payload().clone(),
	);

	Ok(if should_import { Some(checked_statement) } else { None })
}

/// Checks whether the signature is accurate,
/// importing into the grid tracker if successful.
///
/// if successful, this returns a checked signed statement if it should be imported
/// or otherwise an error indicating a reputational fault.
fn handle_grid_statement(
	scheduling_parent: Hash,
	grid_tracker: &mut GridTracker,
	session: SessionIndex,
	per_session: &PerSessionState,
	statement: UncheckedSignedStatement,
	grid_sender_index: ValidatorIndex,
) -> Result<SignedStatement, Rep> {
	// Ensure the statement is correctly signed.
	let checked_statement = match check_statement_signature(
		session,
		&per_session.session_info.validators,
		scheduling_parent,
		statement,
	) {
		Ok(s) => s,
		Err(_) => return Err(COST_INVALID_SIGNATURE),
	};

	grid_tracker.sent_or_received_direct_statement(
		&per_session.groups,
		checked_statement.validator_index(),
		grid_sender_index,
		&checked_statement.payload(),
		true,
	);

	Ok(checked_statement)
}

/// Send backing fresh statements. This should only be performed on importable & confirmed
/// candidates.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn send_backing_fresh_statements<Context>(
	ctx: &mut Context,
	candidate_hash: CandidateHash,
	group_index: GroupIndex,
	scheduling_parent: &Hash,
	scheduling_parent_state: &mut PerSchedulingParentState,
	confirmed: &candidates::ConfirmedCandidate,
	per_session: &PerSessionState,
) {
	let group_validators = per_session.groups.get(group_index).unwrap_or(&[]);
	let mut imported = Vec::new();

	for statement in scheduling_parent_state
		.statement_store
		.fresh_statements_for_backing(group_validators, candidate_hash)
	{
		let v = statement.validator_index();
		let compact = statement.payload().clone();
		imported.push((v, compact));
		let carrying_pvd = statement
			.clone()
			.convert_to_superpayload_with(|statement| match statement {
				CompactStatement::Seconded(_) => FullStatementWithPVD::Seconded(
					(&**confirmed.candidate_receipt()).clone(),
					confirmed.persisted_validation_data().clone(),
				),
				CompactStatement::Valid(c_hash) => FullStatementWithPVD::Valid(c_hash),
			})
			.expect("statements refer to same candidate; qed");

		ctx.send_message(CandidateBackingMessage::Statement {
			scheduling_parent: *scheduling_parent,
			statement: carrying_pvd,
		})
		.await;
	}

	for (v, s) in imported {
		scheduling_parent_state.statement_store.note_known_by_backing(v, s);
	}
}

fn local_knowledge_filter(
	group_size: usize,
	group_index: GroupIndex,
	candidate_hash: CandidateHash,
	statement_store: &StatementStore,
) -> StatementFilter {
	let mut f = StatementFilter::blank(group_size);
	statement_store.fill_statement_filter(group_index, candidate_hash, &mut f);
	f
}

// This provides a backable candidate to the grid and dispatches backable candidate announcements
// and acknowledgements via the grid topology. If the session topology is not yet
// available, this will be a no-op.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn provide_candidate_to_grid<Context>(
	ctx: &mut Context,
	candidate_hash: CandidateHash,
	scheduling_parent_state: &mut PerSchedulingParentState,
	confirmed_candidate: &candidates::ConfirmedCandidate,
	per_session: &PerSessionState,
	authorities: &HashMap<AuthorityDiscoveryId, PeerId>,
	peers: &HashMap<PeerId, PeerState>,
	metrics: &Metrics,
) {
	let local_validator = match scheduling_parent_state.local_validator {
		Some(ref mut v) => v,
		None => return,
	};

	let scheduling_parent = confirmed_candidate.scheduling_parent();
	let group_index = confirmed_candidate.group_index();

	let grid_view = match per_session.grid_view {
		Some(ref t) => t,
		None => {
			gum::debug!(
				target: LOG_TARGET,
				session = scheduling_parent_state.session,
				"Cannot handle backable candidate due to lack of topology",
			);

			return;
		},
	};

	let group_size = match per_session.groups.get(group_index) {
		None => {
			gum::warn!(
				target: LOG_TARGET,
				?candidate_hash,
				?scheduling_parent,
				?group_index,
				session = scheduling_parent_state.session,
				"Handled backed candidate with unknown group?",
			);

			return;
		},
		Some(g) => g.len(),
	};

	let filter = local_knowledge_filter(
		group_size,
		group_index,
		candidate_hash,
		&scheduling_parent_state.statement_store,
	);

	let actions = local_validator.grid_tracker.add_backed_candidate(
		grid_view,
		candidate_hash,
		group_index,
		filter.clone(),
	);

	let manifest = protocol_v3::BackedCandidateManifest {
		scheduling_parent,
		candidate_hash,
		group_index,
		para_id: confirmed_candidate.para_id(),
		parent_head_data_hash: confirmed_candidate.parent_head_data_hash(),
		statement_knowledge: filter.clone(),
	};
	let acknowledgement = protocol_v3::BackedCandidateAcknowledgement {
		candidate_hash,
		statement_knowledge: filter.clone(),
	};

	let mut manifest_peers: Vec<(PeerId, ProtocolVersion)> = Vec::new();
	let mut ack_peers: Vec<(PeerId, ProtocolVersion)> = Vec::new();

	let mut post_statements = Vec::new();
	for (v, action) in actions {
		let p = match connected_validator_peer(authorities, per_session, v) {
			None => continue,
			Some(p) => {
				if peers.get(&p).map_or(false, |d| d.knows_relay_parent(&scheduling_parent)) {
					(p, peers.get(&p).expect("Qed, was checked above").protocol_version.into())
				} else {
					continue;
				}
			},
		};

		match action {
			grid::ManifestKind::Full => manifest_peers.push(p),
			grid::ManifestKind::Acknowledgement => ack_peers.push(p),
		}

		local_validator.grid_tracker.manifest_sent_to(
			&per_session.groups,
			v,
			candidate_hash,
			filter.clone(),
		);
		post_statements.extend(
			post_acknowledgement_statement_messages(
				v,
				scheduling_parent,
				&mut local_validator.grid_tracker,
				&scheduling_parent_state.statement_store,
				&per_session.groups,
				group_index,
				candidate_hash,
				&(p.0, p.1.try_into().expect("Qed, can not fail was checked above")),
			)
			.into_iter()
			.map(|m| (vec![p.0], m)),
		);
	}

	let manifest_peers_v3 = filter_by_peer_version(&manifest_peers, ValidationVersion::V3.into());
	if !manifest_peers_v3.is_empty() {
		gum::debug!(
			target: LOG_TARGET,
			?candidate_hash,
			local_validator = ?per_session.local_validator,
			n_peers = manifest_peers_v3.len(),
			"Sending manifest to v3 peers"
		);

		ctx.send_message(NetworkBridgeTxMessage::SendValidationMessage(
			manifest_peers_v3,
			ValidationProtocols::V3(
				protocol_v3::StatementDistributionMessage::BackedCandidateManifest(manifest),
			)
			.into(),
		))
		.await;
	}

	let ack_peers_v3 = filter_by_peer_version(&ack_peers, ValidationVersion::V3.into());
	if !ack_peers_v3.is_empty() {
		gum::debug!(
			target: LOG_TARGET,
			?candidate_hash,
			local_validator = ?per_session.local_validator,
			n_peers = ack_peers_v3.len(),
			"Sending acknowledgement to v3 peers"
		);

		ctx.send_message(NetworkBridgeTxMessage::SendValidationMessage(
			ack_peers_v3,
			ValidationProtocols::V3(
				protocol_v3::StatementDistributionMessage::BackedCandidateKnown(acknowledgement),
			)
			.into(),
		))
		.await;
	}
	if !post_statements.is_empty() {
		let count = post_statements.len();
		ctx.send_message(NetworkBridgeTxMessage::SendValidationMessages(post_statements))
			.await;
		metrics.on_statements_distributed(count);
	}
}

// Utility function to populate:
// - per scheduling parent `ParaId` to `GroupIndex` mappings.
// - per `GroupIndex` claim queue assignments
async fn determine_group_assignments(
	n_cores: usize,
	group_rotation_info: GroupRotationInfo,
	claim_queue: &ClaimQueueSnapshot,
) -> (HashMap<ParaId, Vec<GroupIndex>>, HashMap<GroupIndex, Vec<ParaId>>) {
	// Determine the core indices occupied by each para at the current scheduling parent. To support
	// on-demand parachains we also consider the core indices at next blocks.
	let schedule: HashMap<CoreIndex, Vec<ParaId>> = claim_queue
		.iter_all_claims()
		.map(|(core_index, paras)| (*core_index, paras.iter().copied().collect()))
		.collect();

	let mut groups_per_para = HashMap::new();
	let mut assignments_per_group = HashMap::with_capacity(schedule.len());

	// Map from `CoreIndex` to `GroupIndex` and collect as `HashMap`.
	for (core_index, paras) in schedule {
		let group_index = group_rotation_info.group_for_core(core_index, n_cores);
		assignments_per_group.insert(group_index, paras.clone());

		for para in paras {
			groups_per_para.entry(para).or_insert_with(Vec::new).push(group_index);
		}
	}

	(groups_per_para, assignments_per_group)
}

#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn fragment_chain_update_inner<Context>(
	ctx: &mut Context,
	state: &mut State,
	active_leaf_hash: Option<Hash>,
	required_parent_info: Option<(Hash, ParaId)>,
	known_hypotheticals: Option<Vec<HypotheticalCandidate>>,
) {
	// 1. get hypothetical candidates
	let hypotheticals = match known_hypotheticals {
		None => state.candidates.frontier_hypotheticals(required_parent_info),
		Some(h) => h,
	};

	// 2. find out which are in the frontier
	gum::debug!(
		target: LOG_TARGET,
		active_leaf_hash = ?active_leaf_hash,
		"Calling getHypotheticalMembership from statement distribution for candidates: {:?}",
		&hypotheticals.iter().map(|hypo| hypo.candidate_hash()).collect::<Vec<_>>()
	);
	let candidate_memberships = {
		let (tx, rx) = oneshot::channel();
		ctx.send_message(ProspectiveParachainsMessage::GetHypotheticalMembership(
			HypotheticalMembershipRequest {
				candidates: hypotheticals,
				fragment_chain_relay_parent: active_leaf_hash,
			},
			tx,
		))
		.await;

		match rx.await {
			Ok(candidate_memberships) => candidate_memberships,
			Err(oneshot::Canceled) => return,
		}
	};
	// 3. note that they are importable under a given leaf hash.
	for (hypo, membership) in candidate_memberships {
		// skip parablocks which aren't potential candidates
		if membership.is_empty() {
			continue;
		}

		for leaf_hash in membership {
			state.candidates.note_importable_under(&hypo, leaf_hash);
		}

		// 4. for confirmed candidates, send all statements which are new to backing.
		if let HypotheticalCandidate::Complete {
			candidate_hash,
			receipt,
			persisted_validation_data: _,
		} = hypo
		{
			let confirmed_candidate = state.candidates.get_confirmed(&candidate_hash);

			let scheduling_parent = receipt.descriptor.scheduling_parent();
			let per_session = state
				.per_scheduling_parent
				.get(&scheduling_parent)
				.and_then(|p| state.per_session.get(&p.session));
			let prs = state.per_scheduling_parent.get_mut(&scheduling_parent);

			if let (Some(confirmed), Some(prs), Some(per_session)) =
				(confirmed_candidate, prs, per_session)
			{
				let group_index = confirmed.group_index();

				// Sanity check if group_index is valid for this para at scheduling parent.
				let Some(expected_groups) = prs.groups_per_para.get(&receipt.descriptor.para_id())
				else {
					continue;
				};
				if !expected_groups.iter().any(|g| *g == group_index) {
					continue;
				}

				send_backing_fresh_statements(
					ctx,
					candidate_hash,
					confirmed.group_index(),
					&scheduling_parent,
					prs,
					confirmed,
					per_session,
				)
				.await;
			}
		}
	}
}

#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn new_leaf_fragment_chain_updates<Context>(
	ctx: &mut Context,
	state: &mut State,
	leaf_hash: Hash,
) {
	fragment_chain_update_inner(ctx, state, Some(leaf_hash), None, None).await
}

#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn prospective_backed_notification_fragment_chain_updates<Context>(
	ctx: &mut Context,
	state: &mut State,
	para_id: ParaId,
	para_head: Hash,
) {
	fragment_chain_update_inner(ctx, state, None, Some((para_head, para_id)), None).await
}

#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn new_confirmed_candidate_fragment_chain_updates<Context>(
	ctx: &mut Context,
	state: &mut State,
	candidate: HypotheticalCandidate,
) {
	fragment_chain_update_inner(ctx, state, None, None, Some(vec![candidate])).await
}

struct ManifestImportSuccess<'a> {
	scheduling_parent_state: &'a mut PerSchedulingParentState,
	per_session: &'a PerSessionState,
	acknowledge: bool,
	sender_index: ValidatorIndex,
}

/// Handles the common part of incoming manifests of both types (full & acknowledgement)
///
/// Basic sanity checks around data, importing the manifest into the grid tracker, finding the
/// sending peer's validator index, reporting the peer for any misbehavior, etc.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn handle_incoming_manifest_common<'a, Context>(
	ctx: &mut Context,
	peer: PeerId,
	peers: &HashMap<PeerId, PeerState>,
	per_scheduling_parent: &'a mut HashMap<Hash, PerSchedulingParentState>,
	per_session: &'a HashMap<SessionIndex, PerSessionState>,
	candidates: &mut Candidates,
	candidate_hash: CandidateHash,
	scheduling_parent: Hash,
	para_id: ParaId,
	mut manifest_summary: grid::ManifestSummary,
	manifest_kind: grid::ManifestKind,
	reputation: &mut ReputationAggregator,
) -> Option<ManifestImportSuccess<'a>> {
	// 1. sanity checks: peer is connected, relay-parent in state, para ID matches group index.
	let peer_state = peers.get(&peer)?;

	let scheduling_parent_state = match per_scheduling_parent.get_mut(&scheduling_parent) {
		None => {
			modify_reputation(
				reputation,
				ctx.sender(),
				peer,
				COST_UNEXPECTED_MANIFEST_MISSING_KNOWLEDGE,
			)
			.await;
			return None;
		},
		Some(s) => s,
	};

	let per_session = per_session.get(&scheduling_parent_state.session)?;

	if scheduling_parent_state.local_validator.is_none() {
		if per_session.is_not_validator() {
			modify_reputation(
				reputation,
				ctx.sender(),
				peer,
				COST_UNEXPECTED_MANIFEST_MISSING_KNOWLEDGE,
			)
			.await;
		}
		return None;
	}

	let Some(expected_groups) = scheduling_parent_state.groups_per_para.get(&para_id) else {
		modify_reputation(reputation, ctx.sender(), peer, COST_MALFORMED_MANIFEST).await;
		return None;
	};

	if !expected_groups.iter().any(|g| g == &manifest_summary.claimed_group_index) {
		modify_reputation(reputation, ctx.sender(), peer, COST_MALFORMED_MANIFEST).await;
		return None;
	}

	let grid_topology = per_session.grid_view.as_ref()?;

	let sender_index = grid_topology
		.iter_sending_for_group(manifest_summary.claimed_group_index, manifest_kind)
		.filter_map(|i| per_session.session_info.discovery_keys.get(i.0 as usize).map(|ad| (i, ad)))
		.filter(|(_, ad)| peer_state.is_authority(ad))
		.map(|(i, _)| i)
		.next();

	let sender_index = match sender_index {
		None => {
			modify_reputation(
				reputation,
				ctx.sender(),
				peer,
				COST_UNEXPECTED_MANIFEST_PEER_UNKNOWN,
			)
			.await;
			return None;
		},
		Some(s) => s,
	};

	// 2. sanity checks: peer is validator, bitvec size, import into grid tracker
	let group_index = manifest_summary.claimed_group_index;
	let claimed_parent_hash = manifest_summary.claimed_parent_hash;

	// Ignore votes from disabled validators when counting towards the threshold.
	let group = per_session.groups.get(group_index).unwrap_or(&[]);
	let disabled_mask = scheduling_parent_state.disabled_bitmask(group);
	manifest_summary.statement_knowledge.mask_seconded(&disabled_mask);
	manifest_summary.statement_knowledge.mask_valid(&disabled_mask);

	let local_validator =
		scheduling_parent_state.local_validator.as_mut().expect("checked above; qed");

	let seconding_limit = scheduling_parent_state
		.assignments_per_group
		.get(&group_index)?
		.iter()
		.filter(|para| para == &&para_id)
		.count();

	let acknowledge = match local_validator.grid_tracker.import_manifest(
		grid_topology,
		&per_session.groups,
		candidate_hash,
		seconding_limit,
		manifest_summary,
		manifest_kind,
		sender_index,
	) {
		Ok(x) => x,
		Err(grid::ManifestImportError::Conflicting) => {
			modify_reputation(reputation, ctx.sender(), peer, COST_CONFLICTING_MANIFEST).await;
			return None;
		},
		Err(grid::ManifestImportError::Overflow) => {
			modify_reputation(reputation, ctx.sender(), peer, COST_EXCESSIVE_SECONDED).await;
			return None;
		},
		Err(grid::ManifestImportError::Insufficient) => {
			modify_reputation(reputation, ctx.sender(), peer, COST_INSUFFICIENT_MANIFEST).await;
			return None;
		},
		Err(grid::ManifestImportError::Malformed) => {
			modify_reputation(reputation, ctx.sender(), peer, COST_MALFORMED_MANIFEST).await;
			return None;
		},
		Err(grid::ManifestImportError::Disallowed) => {
			modify_reputation(reputation, ctx.sender(), peer, COST_UNEXPECTED_MANIFEST_DISALLOWED)
				.await;
			return None;
		},
	};

	// 3. if accepted by grid, insert as unconfirmed.
	if let Err(BadAdvertisement) = candidates.insert_unconfirmed(
		peer,
		candidate_hash,
		scheduling_parent,
		group_index,
		Some((claimed_parent_hash, para_id)),
	) {
		modify_reputation(reputation, ctx.sender(), peer, COST_INACCURATE_ADVERTISEMENT).await;
		return None;
	}

	if acknowledge {
		gum::trace!(
			target: LOG_TARGET,
			?candidate_hash,
			from = ?sender_index,
			local_index = ?per_session.local_validator,
			?manifest_kind,
			"immediate ack, known candidate"
		);
	}

	Some(ManifestImportSuccess { scheduling_parent_state, per_session, acknowledge, sender_index })
}

/// Produce a list of network messages to send to a peer, following acknowledgement of a manifest.
/// This notes the messages as sent within the grid state.
fn post_acknowledgement_statement_messages(
	recipient: ValidatorIndex,
	scheduling_parent: Hash,
	grid_tracker: &mut GridTracker,
	statement_store: &StatementStore,
	groups: &Groups,
	group_index: GroupIndex,
	candidate_hash: CandidateHash,
	peer: &(PeerId, ValidationVersion),
) -> Vec<net_protocol::VersionedValidationProtocol> {
	let sending_filter = match grid_tracker.pending_statements_for(recipient, candidate_hash) {
		None => return Vec::new(),
		Some(f) => f,
	};

	let mut messages = Vec::new();
	for statement in
		statement_store.group_statements(groups, group_index, candidate_hash, &sending_filter)
	{
		grid_tracker.sent_or_received_direct_statement(
			groups,
			statement.validator_index(),
			recipient,
			statement.payload(),
			false,
		);
		match peer.1.into() {
			ValidationVersion::V3 => messages.push(ValidationProtocols::V3(
				protocol_v3::StatementDistributionMessage::Statement(
					scheduling_parent,
					statement.as_unchecked().clone(),
				)
				.into(),
			)),
		};
	}

	messages
}

#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn handle_incoming_manifest<Context>(
	ctx: &mut Context,
	state: &mut State,
	peer: PeerId,
	manifest: net_protocol::v3::BackedCandidateManifest,
	reputation: &mut ReputationAggregator,
	metrics: &Metrics,
) {
	gum::debug!(
		target: LOG_TARGET,
		candidate_hash = ?manifest.candidate_hash,
		?peer,
		"Received incoming manifest",
	);

	let x = match handle_incoming_manifest_common(
		ctx,
		peer,
		&state.peers,
		&mut state.per_scheduling_parent,
		&state.per_session,
		&mut state.candidates,
		manifest.candidate_hash,
		manifest.scheduling_parent,
		manifest.para_id,
		grid::ManifestSummary {
			claimed_parent_hash: manifest.parent_head_data_hash,
			claimed_group_index: manifest.group_index,
			statement_knowledge: manifest.statement_knowledge,
		},
		grid::ManifestKind::Full,
		reputation,
	)
	.await
	{
		Some(x) => x,
		None => return,
	};

	let ManifestImportSuccess { scheduling_parent_state, per_session, acknowledge, sender_index } =
		x;

	if acknowledge {
		// 4. if already known within grid (confirmed & backed), acknowledge candidate
		gum::trace!(
			target: LOG_TARGET,
			candidate_hash = ?manifest.candidate_hash,
			"Known candidate - acknowledging manifest",
		);

		let local_knowledge = {
			let group_size = match per_session.groups.get(manifest.group_index) {
				None => return, // sanity
				Some(x) => x.len(),
			};

			local_knowledge_filter(
				group_size,
				manifest.group_index,
				manifest.candidate_hash,
				&scheduling_parent_state.statement_store,
			)
		};

		let (messages, statements_count) = acknowledgement_and_statement_messages(
			&(
				peer,
				state
					.peers
					.get(&peer)
					.map(|val| val.protocol_version)
					// Assume the latest stable version, if we don't have info about peer version.
					.unwrap_or(ValidationVersion::V3),
			),
			sender_index,
			&per_session.groups,
			scheduling_parent_state,
			manifest.scheduling_parent,
			manifest.group_index,
			manifest.candidate_hash,
			local_knowledge,
		);

		if !messages.is_empty() {
			ctx.send_message(NetworkBridgeTxMessage::SendValidationMessages(messages)).await;
			metrics.on_statements_distributed(statements_count);
		}
	} else if !state.candidates.is_confirmed(&manifest.candidate_hash) {
		// 5. if unconfirmed, add request entry
		gum::trace!(
			target: LOG_TARGET,
			candidate_hash = ?manifest.candidate_hash,
			"Unknown candidate - requesting",
		);

		state
			.request_manager
			.get_or_insert(
				manifest.scheduling_parent,
				manifest.candidate_hash,
				manifest.group_index,
			)
			.add_peer(peer);
	}
}

/// Produces acknowledgement and statement messages to be sent over the network,
/// noting that they have been sent within the grid topology tracker as well.
fn acknowledgement_and_statement_messages(
	peer: &(PeerId, ValidationVersion),
	validator_index: ValidatorIndex,
	groups: &Groups,
	scheduling_parent_state: &mut PerSchedulingParentState,
	scheduling_parent: Hash,
	group_index: GroupIndex,
	candidate_hash: CandidateHash,
	local_knowledge: StatementFilter,
) -> (Vec<(Vec<PeerId>, net_protocol::VersionedValidationProtocol)>, usize) {
	let local_validator = match scheduling_parent_state.local_validator.as_mut() {
		None => return (Vec::new(), 0),
		Some(l) => l,
	};

	let acknowledgement = protocol_v3::BackedCandidateAcknowledgement {
		candidate_hash,
		statement_knowledge: local_knowledge.clone(),
	};

	let mut messages = match peer.1 {
		ValidationVersion::V3 => vec![(
			vec![peer.0],
			ValidationProtocols::V3(
				protocol_v3::StatementDistributionMessage::BackedCandidateKnown(acknowledgement),
			)
			.into(),
		)],
	};

	local_validator.grid_tracker.manifest_sent_to(
		groups,
		validator_index,
		candidate_hash,
		local_knowledge.clone(),
	);

	let statement_messages = post_acknowledgement_statement_messages(
		validator_index,
		scheduling_parent,
		&mut local_validator.grid_tracker,
		&scheduling_parent_state.statement_store,
		&groups,
		group_index,
		candidate_hash,
		peer,
	);
	let statements_count = statement_messages.len();

	messages.extend(statement_messages.into_iter().map(|m| (vec![peer.0], m)));

	(messages, statements_count)
}

#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn handle_incoming_acknowledgement<Context>(
	ctx: &mut Context,
	state: &mut State,
	peer: PeerId,
	acknowledgement: net_protocol::v3::BackedCandidateAcknowledgement,
	reputation: &mut ReputationAggregator,
	metrics: &Metrics,
) {
	// The key difference between acknowledgments and full manifests is that only
	// the candidate hash is included alongside the bitfields, so the candidate
	// must be confirmed for us to even process it.

	gum::debug!(
		target: LOG_TARGET,
		candidate_hash = ?acknowledgement.candidate_hash,
		?peer,
		"Received incoming acknowledgement",
	);

	let candidate_hash = acknowledgement.candidate_hash;
	let (scheduling_parent, parent_head_data_hash, group_index, para_id) = {
		match state.candidates.get_confirmed(&candidate_hash) {
			Some(c) => {
				(c.scheduling_parent(), c.parent_head_data_hash(), c.group_index(), c.para_id())
			},
			None => {
				modify_reputation(
					reputation,
					ctx.sender(),
					peer,
					COST_UNEXPECTED_ACKNOWLEDGEMENT_UNKNOWN_CANDIDATE,
				)
				.await;
				return;
			},
		}
	};

	let x = match handle_incoming_manifest_common(
		ctx,
		peer,
		&state.peers,
		&mut state.per_scheduling_parent,
		&state.per_session,
		&mut state.candidates,
		candidate_hash,
		scheduling_parent,
		para_id,
		grid::ManifestSummary {
			claimed_parent_hash: parent_head_data_hash,
			claimed_group_index: group_index,
			statement_knowledge: acknowledgement.statement_knowledge,
		},
		grid::ManifestKind::Acknowledgement,
		reputation,
	)
	.await
	{
		Some(x) => x,
		None => return,
	};

	let ManifestImportSuccess { scheduling_parent_state, per_session, sender_index, .. } = x;

	let local_validator = match scheduling_parent_state.local_validator.as_mut() {
		None => return,
		Some(l) => l,
	};

	let messages = post_acknowledgement_statement_messages(
		sender_index,
		scheduling_parent,
		&mut local_validator.grid_tracker,
		&scheduling_parent_state.statement_store,
		&per_session.groups,
		group_index,
		candidate_hash,
		&(
			peer,
			state
				.peers
				.get(&peer)
				.map(|val| val.protocol_version)
				// Assume the latest stable version, if we don't have info about peer version.
				.unwrap_or(ValidationVersion::V3),
		),
	);

	if !messages.is_empty() {
		let count = messages.len();
		ctx.send_message(NetworkBridgeTxMessage::SendValidationMessages(
			messages.into_iter().map(|m| (vec![peer], m)).collect(),
		))
		.await;
		metrics.on_statements_distributed(count);
	}
}

/// Handle a notification of a candidate being backed.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
pub(crate) async fn handle_backed_candidate_message<Context>(
	ctx: &mut Context,
	state: &mut State,
	candidate_hash: CandidateHash,
	metrics: &Metrics,
) {
	// If the candidate is unknown or unconfirmed, it's a race (pruned before receiving message)
	// or a bug. Ignore if so
	let confirmed = match state.candidates.get_confirmed(&candidate_hash) {
		None => {
			gum::debug!(
				target: LOG_TARGET,
				?candidate_hash,
				"Received backed candidate notification for unknown or unconfirmed",
			);

			return;
		},
		Some(c) => c,
	};

	let scheduling_parent_state =
		match state.per_scheduling_parent.get_mut(&confirmed.scheduling_parent()) {
			None => return,
			Some(s) => s,
		};

	let per_session = match state.per_session.get(&scheduling_parent_state.session) {
		None => return,
		Some(s) => s,
	};

	gum::debug!(
		target: LOG_TARGET,
		?candidate_hash,
		group_index = ?confirmed.group_index(),
		"Candidate Backed - initiating grid distribution & child fetches"
	);

	provide_candidate_to_grid(
		ctx,
		candidate_hash,
		scheduling_parent_state,
		confirmed,
		per_session,
		&state.authorities,
		&state.peers,
		metrics,
	)
	.await;

	// Search for children of the backed candidate to request.
	prospective_backed_notification_fragment_chain_updates(
		ctx,
		state,
		confirmed.para_id(),
		confirmed.candidate_receipt().descriptor.para_head(),
	)
	.await;
}

/// Sends all messages about a candidate to all peers in the cluster,
/// with `Seconded` statements first.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn send_cluster_candidate_statements<Context>(
	ctx: &mut Context,
	state: &mut State,
	candidate_hash: CandidateHash,
	scheduling_parent: Hash,
	metrics: &Metrics,
) {
	let scheduling_parent_state = match state.per_scheduling_parent.get_mut(&scheduling_parent) {
		None => return,
		Some(s) => s,
	};

	let per_session = match state.per_session.get(&scheduling_parent_state.session) {
		None => return,
		Some(s) => s,
	};

	let local_group = match scheduling_parent_state.active_validator_state_mut() {
		None => return,
		Some(v) => v.group,
	};

	let group_size = match per_session.groups.get(local_group) {
		None => return,
		Some(g) => g.len(),
	};

	let statements: Vec<_> = scheduling_parent_state
		.statement_store
		.group_statements(
			&per_session.groups,
			local_group,
			candidate_hash,
			&StatementFilter::full(group_size),
		)
		.map(|x| x.clone())
		.collect();

	for statement in statements {
		circulate_statement(
			ctx,
			scheduling_parent,
			scheduling_parent_state,
			per_session,
			&state.candidates,
			&state.authorities,
			&state.peers,
			statement,
			metrics,
		)
		.await;
	}
}

/// Applies state & p2p updates as a result of a newly confirmed candidate.
///
/// This punishes peers which advertised the candidate incorrectly, as well as
/// doing an importability analysis of the confirmed candidate and providing
/// statements to the backing subsystem if importable. It also cleans up
/// any pending requests for the candidate.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn apply_post_confirmation<Context>(
	ctx: &mut Context,
	state: &mut State,
	post_confirmation: PostConfirmation,
	reputation: &mut ReputationAggregator,
	metrics: &Metrics,
) {
	for peer in post_confirmation.reckoning.incorrect {
		modify_reputation(reputation, ctx.sender(), peer, COST_INACCURATE_ADVERTISEMENT).await;
	}

	let candidate_hash = post_confirmation.hypothetical.candidate_hash();
	state.request_manager.remove_for(candidate_hash);

	// Scheduling context: look up per_scheduling_parent state for statement circulation.
	send_cluster_candidate_statements(
		ctx,
		state,
		candidate_hash,
		post_confirmation.hypothetical.scheduling_parent(),
		metrics,
	)
	.await;
	new_confirmed_candidate_fragment_chain_updates(ctx, state, post_confirmation.hypothetical)
		.await;
}

/// Dispatch pending requests for candidate data & statements.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
pub(crate) async fn dispatch_requests<Context>(ctx: &mut Context, state: &mut State) {
	if !state.request_manager.has_pending_requests() {
		return;
	}

	let peers = &state.peers;
	let peer_advertised = |identifier: &CandidateIdentifier, peer: &_| {
		let peer_data = peers.get(peer)?;

		let scheduling_parent_state =
			state.per_scheduling_parent.get(&identifier.scheduling_parent)?;
		let per_session = state.per_session.get(&scheduling_parent_state.session)?;

		let local_validator = scheduling_parent_state.local_validator.as_ref()?;

		for validator_id in find_validator_ids(peer_data.iter_known_discovery_ids(), |a| {
			per_session.authority_lookup.get(a)
		}) {
			// For cluster members, they haven't advertised any statements in particular,
			// but have surely sent us some.
			if let Some(active) = local_validator.active.as_ref() {
				if active.cluster_tracker.knows_candidate(validator_id, identifier.candidate_hash) {
					return Some(StatementFilter::blank(active.cluster_tracker.targets().len()));
				}
			}

			let filter = local_validator
				.grid_tracker
				.advertised_statements(validator_id, &identifier.candidate_hash);

			if let Some(f) = filter {
				return Some(f);
			}
		}

		None
	};
	let request_props = |identifier: &CandidateIdentifier| {
		let &CandidateIdentifier { scheduling_parent, group_index, .. } = identifier;

		let scheduling_parent_state = state.per_scheduling_parent.get(&scheduling_parent)?;
		let per_session = state.per_session.get(&scheduling_parent_state.session)?;
		let group = per_session.groups.get(group_index)?;
		let seconding_limit =
			scheduling_parent_state.assignments_per_group.get(&group_index)?.len();

		// Request nothing which would be an 'over-seconded' statement.
		let mut unwanted_mask = StatementFilter::blank(group.len());
		for (i, v) in group.iter().enumerate() {
			if scheduling_parent_state.statement_store.seconded_count(v) >= seconding_limit {
				unwanted_mask.seconded_in_group.set(i, true);
			}
		}

		// Add disabled validators to the unwanted mask.
		let disabled_mask = scheduling_parent_state.disabled_bitmask(group);
		unwanted_mask.seconded_in_group |= &disabled_mask;
		unwanted_mask.validated_in_group |= &disabled_mask;

		// don't require a backing threshold for cluster candidates.
		let local_validator = scheduling_parent_state.local_validator.as_ref()?;
		let require_backing = local_validator
			.active
			.as_ref()
			.map_or(true, |active| active.group != group_index);

		let backing_threshold = if require_backing {
			let threshold = per_session.groups.get_size_and_backing_threshold(group_index)?.1;
			Some(threshold)
		} else {
			None
		};

		Some(RequestProperties { unwanted_mask, backing_threshold })
	};

	while let Some(request) = state.request_manager.next_request(
		&mut state.response_manager,
		request_props,
		peer_advertised,
	) {
		// Peer is supposedly connected.
		ctx.send_message(NetworkBridgeTxMessage::SendRequests(
			vec![Requests::AttestedCandidateV2(request)],
			IfDisconnected::ImmediateError,
		))
		.await;
	}
}

/// Wait on the next incoming response. If there are no requests pending, this
/// future never resolves. It is the responsibility of the user of this API
/// to interrupt the future.
pub(crate) async fn receive_response(response_manager: &mut ResponseManager) -> UnhandledResponse {
	match response_manager.incoming().await {
		Some(r) => r,
		None => futures::future::pending().await,
	}
}

/// Wait on the next soonest retry on a pending request. If there are no retries pending, this
/// future never resolves. Note that this only signals that a request is ready to retry; the user of
/// this API must call `dispatch_requests`.
pub(crate) async fn next_retry(request_manager: &mut RequestManager) {
	match request_manager.next_retry_time() {
		Some(instant) => {
			futures_timer::Delay::new(instant.saturating_duration_since(Instant::now())).await
		},
		None => futures::future::pending().await,
	}
}

/// Handles an incoming response. This does the actual work of validating the response,
/// importing statements, sending acknowledgements, etc.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
pub(crate) async fn handle_response<Context>(
	ctx: &mut Context,
	state: &mut State,
	response: UnhandledResponse,
	reputation: &mut ReputationAggregator,
	metrics: &Metrics,
) {
	let &requests::CandidateIdentifier { scheduling_parent, candidate_hash, group_index } =
		response.candidate_identifier();
	let peer = *response.requested_peer();

	gum::trace!(
		target: LOG_TARGET,
		?candidate_hash,
		?peer,
		"Received response",
	);

	let post_confirmation = {
		let scheduling_parent_state = match state.per_scheduling_parent.get_mut(&scheduling_parent)
		{
			None => return,
			Some(s) => s,
		};

		let per_session = match state.per_session.get(&scheduling_parent_state.session) {
			None => return,
			Some(s) => s,
		};

		let group = match per_session.groups.get(group_index) {
			None => return,
			Some(g) => g,
		};

		let disabled_mask = scheduling_parent_state.disabled_bitmask(group);

		let res = response.validate_response(
			&mut state.request_manager,
			group,
			scheduling_parent_state.session,
			|v| per_session.session_info.validators.get(v).map(|x| x.clone()),
			|para, g_index| {
				let Some(expected_groups) = scheduling_parent_state.groups_per_para.get(&para)
				else {
					return false;
				};

				expected_groups.iter().any(|g| g == &g_index)
			},
			disabled_mask,
			&scheduling_parent_state.transposed_cq,
		);

		for (peer, rep) in res.reputation_changes {
			modify_reputation(reputation, ctx.sender(), peer, rep).await;
		}

		let (candidate, pvd, statements) = match res.request_status {
			requests::CandidateRequestStatus::Outdated => return,
			requests::CandidateRequestStatus::Incomplete => {
				gum::trace!(
					target: LOG_TARGET,
					?candidate_hash,
					"Response incomplete. Retrying"
				);

				return;
			},
			requests::CandidateRequestStatus::Complete {
				candidate,
				persisted_validation_data,
				statements,
			} => {
				gum::trace!(
					target: LOG_TARGET,
					?candidate_hash,
					n_statements = statements.len(),
					"Successfully received candidate"
				);

				(candidate, persisted_validation_data, statements)
			},
		};

		for statement in statements {
			let _ = scheduling_parent_state.statement_store.insert(
				&per_session.groups,
				statement,
				StatementOrigin::Remote,
			);
		}

		if let Some(post_confirmation) =
			state.candidates.confirm_candidate(candidate_hash, candidate, pvd, group_index)
		{
			post_confirmation
		} else {
			gum::warn!(
				target: LOG_TARGET,
				?candidate_hash,
				"Candidate re-confirmed by request/response: logic error",
			);

			return;
		}
	};

	// Note that this implicitly circulates all statements via the cluster.
	apply_post_confirmation(ctx, state, post_confirmation, reputation, metrics).await;

	let confirmed = state.candidates.get_confirmed(&candidate_hash).expect("just confirmed; qed");

	// Although the candidate is confirmed, it isn't yet a
	// hypothetical member of the fragment chain. Later, when it is,
	// we will import statements.
	if !confirmed.is_importable(None) {
		return;
	}

	let scheduling_parent_state = match state.per_scheduling_parent.get_mut(&scheduling_parent) {
		None => return,
		Some(s) => s,
	};

	let per_session = match state.per_session.get(&scheduling_parent_state.session) {
		None => return,
		Some(s) => s,
	};

	send_backing_fresh_statements(
		ctx,
		candidate_hash,
		group_index,
		&scheduling_parent,
		scheduling_parent_state,
		confirmed,
		per_session,
	)
	.await;

	// we don't need to send acknowledgement yet because
	// 1. the candidate is not known yet, so cannot be backed. any previous confirmation is a bug,
	//    because `apply_post_confirmation` is meant to clear requests.
	// 2. providing the statements to backing will lead to 'Backed' message.
	// 3. on 'Backed' we will send acknowledgements/follow up statements when this becomes
	//    includable.
}

/// Returns true if the statement filter meets the backing threshold for grid requests.
pub(crate) fn seconded_and_sufficient(
	filter: &StatementFilter,
	backing_threshold: Option<usize>,
) -> bool {
	backing_threshold.map_or(true, |t| filter.has_seconded() && filter.backing_validators() >= t)
}

/// Answer an incoming request for a candidate.
pub(crate) fn answer_request(state: &mut State, message: ResponderMessage) {
	let ResponderMessage { request, sent_feedback } = message;
	let AttestedCandidateRequest { candidate_hash, ref mask } = &request.payload;

	gum::trace!(
		target: LOG_TARGET,
		?candidate_hash,
		peer = ?request.peer,
		"Received request"
	);

	// Signal to the responder that we started processing this request.
	let _ = sent_feedback.send(());

	let confirmed = match state.candidates.get_confirmed(&candidate_hash) {
		None => return, // drop request, candidate not known.
		Some(c) => c,
	};

	let scheduling_parent_state =
		match state.per_scheduling_parent.get_mut(&confirmed.scheduling_parent()) {
			None => return,
			Some(s) => s,
		};

	let local_validator = match scheduling_parent_state.local_validator.as_ref() {
		None => return,
		Some(s) => s,
	};

	let per_session = match state.per_session.get(&scheduling_parent_state.session) {
		None => return,
		Some(s) => s,
	};

	let peer_data = match state.peers.get(&request.peer) {
		None => return,
		Some(d) => d,
	};

	let group_index = confirmed.group_index();
	let group = per_session
		.groups
		.get(group_index)
		.expect("group from session's candidate always known; qed");

	let group_size = group.len();

	// check request bitfields are right size.
	if mask.seconded_in_group.len() != group_size || mask.validated_in_group.len() != group_size {
		let _ = request.send_outgoing_response(OutgoingResponse {
			result: Err(()),
			reputation_changes: vec![COST_INVALID_REQUEST_BITFIELD_SIZE],
			sent_feedback: None,
		});

		return;
	}

	// check peer is allowed to request the candidate (i.e. they're in the cluster or we've sent
	// them a manifest)
	let (validator_id, is_cluster) = {
		let mut validator_id = None;
		let mut is_cluster = false;
		for v in find_validator_ids(peer_data.iter_known_discovery_ids(), |a| {
			per_session.authority_lookup.get(a)
		}) {
			if local_validator
				.active
				.as_ref()
				.map_or(false, |active| active.cluster_tracker.can_request(v, *candidate_hash))
			{
				validator_id = Some(v);
				is_cluster = true;
				break;
			}

			if local_validator.grid_tracker.can_request(v, *candidate_hash) {
				validator_id = Some(v);
				break;
			}
		}

		match validator_id {
			Some(v) => (v, is_cluster),
			None => {
				let _ = request.send_outgoing_response(OutgoingResponse {
					result: Err(()),
					reputation_changes: vec![COST_UNEXPECTED_REQUEST],
					sent_feedback: None,
				});

				return;
			},
		}
	};

	// Transform mask with 'OR' semantics into one with 'AND' semantics for the API used
	// below.
	let and_mask = StatementFilter {
		seconded_in_group: !mask.seconded_in_group.clone(),
		validated_in_group: !mask.validated_in_group.clone(),
	};

	let local_validator = match scheduling_parent_state.local_validator.as_mut() {
		None => return,
		Some(s) => s,
	};

	let mut sent_filter = StatementFilter::blank(group_size);
	let statements: Vec<_> = scheduling_parent_state
		.statement_store
		.group_statements(&per_session.groups, group_index, *candidate_hash, &and_mask)
		.map(|s| {
			let s = s.as_unchecked().clone();
			let index_in_group = |v: ValidatorIndex| group.iter().position(|x| &v == x);
			let Some(i) = index_in_group(s.unchecked_validator_index()) else { return s };

			match s.unchecked_payload() {
				CompactStatement::Seconded(_) => {
					sent_filter.seconded_in_group.set(i, true);
				},
				CompactStatement::Valid(_) => {
					sent_filter.validated_in_group.set(i, true);
				},
			}
			s
		})
		.collect();

	// There should be no response at all for grid requests when the
	// backing threshold is no longer met as a result of disabled validators.
	if !is_cluster {
		let threshold = per_session
			.groups
			.get_size_and_backing_threshold(group_index)
			.expect("group existence checked above; qed")
			.1;

		if !seconded_and_sufficient(&sent_filter, Some(threshold)) {
			gum::info!(
				target: LOG_TARGET,
				?candidate_hash,
				scheduling_parent = ?confirmed.scheduling_parent(),
				?group_index,
				"Dropping a request from a grid peer because the backing threshold is no longer met."
			);
			return;
		}
	}

	// Update bookkeeping about which statements peers have received.
	for statement in &statements {
		if is_cluster {
			local_validator
				.active
				.as_mut()
				.expect("cluster peer means local is active validator; qed")
				.cluster_tracker
				.note_sent(
					validator_id,
					statement.unchecked_validator_index(),
					statement.unchecked_payload().clone(),
				);
		} else {
			local_validator.grid_tracker.sent_or_received_direct_statement(
				&per_session.groups,
				statement.unchecked_validator_index(),
				validator_id,
				statement.unchecked_payload(),
				false,
			);
		}
	}

	let response = AttestedCandidateResponse {
		candidate_receipt: (&**confirmed.candidate_receipt()).clone(),
		persisted_validation_data: confirmed.persisted_validation_data().clone(),
		statements,
	};

	let _ = request.send_response(response);
}

/// Messages coming from the background respond task.
pub(crate) struct ResponderMessage {
	request: IncomingRequest<AttestedCandidateRequest>,
	sent_feedback: oneshot::Sender<()>,
}

/// A fetching task, taking care of fetching candidates via request/response.
///
/// Runs in a background task and feeds request to [`answer_request`] through [`MuxedMessage`].
pub(crate) async fn respond_task(
	mut receiver: IncomingRequestReceiver<AttestedCandidateRequest>,
	mut sender: mpsc::Sender<ResponderMessage>,
	metrics: Metrics,
) {
	let mut pending_out = FuturesUnordered::new();
	let mut active_peers = HashSet::new();

	loop {
		select! {
			// New request
			request_result = receiver.recv(|| vec![COST_INVALID_REQUEST]).fuse() => {
				let request = match request_result.into_nested() {
					Ok(Ok(v)) => v,
					Err(fatal) => {
						gum::debug!(target: LOG_TARGET, error = ?fatal, "Shutting down request responder");
						return
					},
					Ok(Err(jfyi)) => {
						gum::debug!(target: LOG_TARGET, error = ?jfyi, "Decoding request failed");
						continue
					},
				};

				// If peer currently being served drop request
				if active_peers.contains(&request.peer) {
					gum::trace!(target: LOG_TARGET, "Peer already being served, dropping request");
					metrics.on_request_dropped_peer_rate_limit();
					continue
				}

				// If we are over parallel limit wait for one to finish
				if pending_out.len() >= MAX_PARALLEL_ATTESTED_CANDIDATE_REQUESTS as usize {
					gum::trace!(target: LOG_TARGET, "Over max parallel requests, waiting for one to finish");
					metrics.on_max_parallel_requests_reached();
					let (_, peer) = pending_out.select_next_some().await;
					active_peers.remove(&peer);
				}

				// Start serving the request
				let (pending_sent_tx, pending_sent_rx) = oneshot::channel();
				let peer = request.peer;
				if let Err(err) = sender
					.feed(ResponderMessage { request, sent_feedback: pending_sent_tx })
					.await
				{
					gum::debug!(target: LOG_TARGET, ?err, "Shutting down responder");
					return
				}
				let future_with_peer = pending_sent_rx.map(move |result| (result, peer));
				pending_out.push(future_with_peer);
				active_peers.insert(peer);
			},
			// Request served/finished
			result = pending_out.select_next_some() => {
				let (_, peer) = result;
				active_peers.remove(&peer);
			},
		}
	}
}

#[cfg(test)]
mod deactivation_tests {
	//! Unit tests for [`handle_deactivate_leaves`] covering all cleanup responsibilities:
	//! implicit view pruning, `per_scheduling_parent` cleanup, request-manager cleanup keyed by
	//! pruned scheduling parent (not by leaf), `per_session` retention, and `unused_topologies`
	//! retention including the "keep last session's topology" edge case.
	//!
	//! Tests run without the full subsystem harness: they construct a `State` directly and
	//! drive `ImplicitView::activate_leaf` via a mock subsystem context, which lets us make
	//! precise assertions on private state fields after deactivation.
	use super::*;
	use assert_matches::assert_matches;
	use futures::future::join;
	use polkadot_node_network_protocol::grid_topology::SessionGridTopology;
	use polkadot_node_subsystem::{
		messages::{ChainApiMessage, RuntimeApiMessage, RuntimeApiRequest},
		AllMessages, SubsystemContext,
	};
	use polkadot_node_subsystem_test_helpers::{
		make_subsystem_context, TestSubsystemContextHandle,
	};
	use polkadot_primitives::{BlockNumber, CandidateHash, Header, IndexedVec, SessionInfo};
	use sc_keystore::LocalKeystore;
	use sp_core::testing::TaskExecutor;
	use std::sync::Arc;

	type VirtualOverseer = TestSubsystemContextHandle<AllMessages>;

	const SCHEDULING_LOOKAHEAD: u32 = 3;

	/// Build an empty `State` with an in-memory keystore.
	fn new_state() -> State {
		State::new(Arc::new(LocalKeystore::in_memory()) as KeystorePtr)
	}

	/// Build a minimal `SessionInfo` with a single validator group containing a single validator.
	/// Enough for `PerSessionState::new` to succeed; no other session-dependent logic is exercised
	/// by these tests.
	fn dummy_session_info() -> SessionInfo {
		SessionInfo {
			validators: IndexedVec::from(vec![]),
			discovery_keys: vec![],
			assignment_keys: vec![],
			validator_groups: IndexedVec::from(vec![vec![ValidatorIndex(0)]]),
			n_cores: 1,
			zeroth_delay_tranche_width: 0,
			relay_vrf_modulo_samples: 0,
			n_delay_tranches: 0,
			no_show_slots: 0,
			needed_approvals: 0,
			active_validator_indices: vec![],
			dispute_period: 6,
			random_seed: [0u8; 32],
		}
	}

	/// Build a `PerSchedulingParentState` with no local-validator state. Enough for
	/// `handle_deactivate_leaves` to traverse it without panicking.
	fn dummy_per_sp_state(session: SessionIndex) -> PerSchedulingParentState {
		let groups = Groups::new(
			dummy_session_info().validator_groups.clone(),
			// backing_threshold
			1,
		);
		PerSchedulingParentState {
			local_validator: None,
			statement_store: StatementStore::new(&groups),
			session,
			transposed_cq: TransposedClaimQueue::default(),
			groups_per_para: HashMap::new(),
			disabled_validators: HashSet::new(),
			assignments_per_group: HashMap::new(),
		}
	}

	fn dummy_per_session_state(keystore: &KeystorePtr) -> PerSessionState {
		PerSessionState::new(dummy_session_info(), keystore, /* backing_threshold */ 1)
	}

	fn dummy_topology() -> NewGossipTopology {
		NewGossipTopology {
			session: 0,
			topology: SessionGridTopology::new(Vec::new(), Vec::new()),
			local_index: None,
		}
	}

	/// Answer the sequence of `ChainApi`/`RuntimeApi` messages produced by
	/// `ImplicitView::activate_leaf` for a single leaf with `SCHEDULING_LOOKAHEAD` ancestors,
	/// all in `session`.
	///
	/// `chain` is `[leaf, parent, grandparent, ...]` — leaf first, then ancestors in
	/// descending block-number order. `leaf_number` is the absolute block number of the leaf;
	/// ancestor numbers decrement by 1 going deeper.
	///
	/// `already_cached` tracks blocks that prior activations have already placed in the view's
	/// `block_info_storage`; those don't trigger `BlockHeader` requests. Updated in-place.
	async fn answer_implicit_view_activation(
		ctx_handle: &mut VirtualOverseer,
		chain: &[Hash],
		leaf_number: BlockNumber,
		session: SessionIndex,
		already_cached: &mut HashSet<Hash>,
	) {
		let leaf = chain[0];
		let ancestors_len = (SCHEDULING_LOOKAHEAD as usize).saturating_sub(1).min(chain.len() - 1);
		let ancestors: Vec<Hash> = chain[1..=ancestors_len].to_vec();

		// 1. SessionIndexForChild(leaf).
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				hash,
				RuntimeApiRequest::SessionIndexForChild(tx),
			)) if hash == leaf => {
				tx.send(Ok(session)).unwrap();
			}
		);

		// 2. SchedulingLookahead(session).
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::SchedulingLookahead(_, tx),
			)) => {
				tx.send(Ok(SCHEDULING_LOOKAHEAD)).unwrap();
			}
		);

		// 3. Ancestors(leaf, k = lookahead - 1).
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::ChainApi(ChainApiMessage::Ancestors {
				hash,
				k,
				response_channel,
			}) if hash == leaf && k == ancestors_len => {
				response_channel.send(Ok(ancestors.clone())).unwrap();
			}
		);

		// 4. SessionIndexForChild(ancestor) for each ancestor (same session → no truncation).
		for ancestor in &ancestors {
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					hash,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) if hash == *ancestor => {
					tx.send(Ok(session)).unwrap();
				}
			);
		}

		// 5. BlockHeader for each ancestry block NOT already in the view's block_info_storage.
		//    `fetch_fresh_leaf_and_insert_ancestry` iterates `iter::once(leaf).chain(ancestors)`
		//    and skips `Occupied` entries.
		let full_chain: Vec<Hash> =
			std::iter::once(leaf).chain(ancestors.iter().copied()).collect();
		for (depth, block) in full_chain.iter().enumerate() {
			if already_cached.contains(block) {
				continue;
			}
			let expected_number = leaf_number - depth as BlockNumber;
			let parent_hash = full_chain.get(depth + 1).copied().unwrap_or_default();
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::ChainApi(ChainApiMessage::BlockHeader(hash, tx)) if hash == *block => {
					tx.send(Ok(Some(Header {
						parent_hash,
						number: expected_number,
						state_root: Hash::zero(),
						extrinsics_root: Hash::zero(),
						digest: Default::default(),
					}))).unwrap();
				}
			);
			already_cached.insert(*block);
		}
	}

	/// Drive the full implicit-view activation for each leaf in `leaves_with_chains`, in order.
	/// Each entry is `(chain, leaf_number, session)` where `chain[0]` is the leaf and subsequent
	/// entries are its ancestors, `leaf_number` is the leaf's absolute block number.
	fn activate_leaves_in_view(
		view: &mut ImplicitView,
		leaves_with_chains: &[(&[Hash], BlockNumber, SessionIndex)],
	) {
		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);
		let mut cached = HashSet::new();

		futures::executor::block_on(async {
			for (chain, leaf_number, session) in leaves_with_chains {
				let activation = view.activate_leaf(ctx.sender(), chain[0]);
				let answering = answer_implicit_view_activation(
					&mut ctx_handle,
					chain,
					*leaf_number,
					*session,
					&mut cached,
				);
				let (result, _) = join(activation, answering).await;
				result.expect("activation should succeed");
			}
		});
	}

	// --------------------------------------------------------------------------------------------
	// Scenario A: deactivate the only leaf in the view.
	//
	// After deactivation the implicit view has no leaves, so *all* block info is pruned. Every
	// `per_scheduling_parent` entry (for leaf and ancestors), all requests keyed by any of those
	// scheduling parents, and the corresponding `per_session` entry should disappear.
	// `unused_topologies` for the (former) last session is kept per the inline-documented edge
	// case.
	// --------------------------------------------------------------------------------------------
	#[test]
	fn deactivate_last_leaf_prunes_everything_but_keeps_last_session_topology() {
		const SESSION: SessionIndex = 7;
		let leaf = Hash::repeat_byte(0x0A);
		let parent = Hash::repeat_byte(0x09);
		let grandparent = Hash::repeat_byte(0x08);
		let chain = [leaf, parent, grandparent];
		const LEAF_NUMBER: BlockNumber = 10;

		let mut state = new_state();
		activate_leaves_in_view(&mut state.implicit_view, &[(&chain, LEAF_NUMBER, SESSION)]);

		// Seed `per_scheduling_parent` for every block in the implicit view, `per_session` for
		// the active session, and `unused_topologies` for both an active and an older session
		// so we can see which ones are retained.
		const OLDER_SESSION: SessionIndex = 6;
		for rp in &chain {
			state.per_scheduling_parent.insert(*rp, dummy_per_sp_state(SESSION));
		}
		state
			.per_session
			.insert(SESSION, dummy_per_session_state(&state.keystore.clone()));
		state.unused_topologies.insert(SESSION, dummy_topology());
		state.unused_topologies.insert(OLDER_SESSION, dummy_topology());

		// Seed requests against the leaf, an ancestor, and an unrelated scheduling parent that
		// isn't in the view. The unrelated one should survive — deactivation must only clean up
		// things reachable from the deactivated leaf's implicit view.
		let unrelated_sp = Hash::repeat_byte(0xFF);
		let c_leaf = CandidateHash(Hash::repeat_byte(0xA1));
		let c_parent = CandidateHash(Hash::repeat_byte(0xA2));
		let c_unrelated = CandidateHash(Hash::repeat_byte(0xA3));
		state.request_manager.get_or_insert(leaf, c_leaf, GroupIndex(0));
		state.request_manager.get_or_insert(parent, c_parent, GroupIndex(0));
		state.request_manager.get_or_insert(unrelated_sp, c_unrelated, GroupIndex(0));
		assert_eq!(state.request_manager.total_requests_count(), 3);

		handle_deactivate_leaves(&mut state, &[leaf]);

		// Implicit view is empty: all blocks were pruned.
		assert_eq!(state.implicit_view.leaves().count(), 0);

		// `per_scheduling_parent` lost entries for all blocks in the pruned view.
		for rp in &chain {
			assert!(
				!state.per_scheduling_parent.contains_key(rp),
				"per_scheduling_parent should not contain pruned block {:?}",
				rp,
			);
		}

		// Requests for pruned scheduling parents are gone; unrelated requests survive.
		assert_eq!(state.request_manager.requests_count_by_scheduling_parent(leaf), 0);
		assert_eq!(state.request_manager.requests_count_by_scheduling_parent(parent), 0);
		assert_eq!(state.request_manager.requests_count_by_scheduling_parent(grandparent), 0);
		assert_eq!(state.request_manager.requests_count_by_scheduling_parent(unrelated_sp), 1);
		assert_eq!(state.request_manager.total_requests_count(), 1);

		// `per_session` is dropped because no `per_scheduling_parent` entry still references it.
		assert!(state.per_session.is_empty());

		// `unused_topologies`: the last-seen session's topology is retained (per inline comment
		// at the cleanup site — we might still need it if PP APIs come online later), older
		// topologies are dropped.
		assert!(state.unused_topologies.contains_key(&SESSION));
		assert!(!state.unused_topologies.contains_key(&OLDER_SESSION));
	}

	// --------------------------------------------------------------------------------------------
	// Scenario B: deactivate one of two leaves sharing the same chain + session.
	//
	// The surviving leaf's retain-minimum keeps some ancestors alive in the implicit view, so
	// deactivation only partially prunes. This is the scenario where the pre-fix bug was
	// observable: requests tied to *pruned ancestors* (not the deactivated leaf) must be cleaned
	// up. Before the fix, `remove_by_scheduling_parent(*leaf)` was called once per pruned RP,
	// which is wrong on both axes — wrong key, and called more times than needed.
	// --------------------------------------------------------------------------------------------
	#[test]
	fn deactivate_one_of_two_leaves_prunes_only_unreachable_ancestors() {
		const SESSION: SessionIndex = 5;

		// Chain: leaf_b (n=5) <- leaf_a (n=4) <- a1 (n=3) <- a2 (n=2) <- a3 (n=1).
		// Both leaves share the same chain, so activating leaf_a first, then leaf_b, gives us
		// a view where both leaves are active. Deactivating leaf_a shouldn't prune any blocks
		// that are still inside leaf_b's retain window.
		let leaf_b = Hash::repeat_byte(0x05);
		let leaf_a = Hash::repeat_byte(0x04);
		let a1 = Hash::repeat_byte(0x03);
		let a2 = Hash::repeat_byte(0x02);
		let a3 = Hash::repeat_byte(0x01);

		let chain_a = [leaf_a, a1, a2];
		let chain_b = [leaf_b, leaf_a, a1];
		const LEAF_A_NUMBER: BlockNumber = 4;
		const LEAF_B_NUMBER: BlockNumber = 5;

		let mut state = new_state();
		activate_leaves_in_view(
			&mut state.implicit_view,
			&[(&chain_a, LEAF_A_NUMBER, SESSION), (&chain_b, LEAF_B_NUMBER, SESSION)],
		);

		// Seed per_scheduling_parent for every block that's in the combined view.
		for rp in [leaf_b, leaf_a, a1, a2] {
			state.per_scheduling_parent.insert(rp, dummy_per_sp_state(SESSION));
		}
		state
			.per_session
			.insert(SESSION, dummy_per_session_state(&state.keystore.clone()));
		state.unused_topologies.insert(SESSION, dummy_topology());

		// Seed a request for every scheduling parent and for an unrelated hash.
		let unrelated_sp = Hash::repeat_byte(0xFF);
		let ch = |b: u8| CandidateHash(Hash::repeat_byte(b));
		state.request_manager.get_or_insert(leaf_b, ch(0xB0), GroupIndex(0));
		state.request_manager.get_or_insert(leaf_a, ch(0xA0), GroupIndex(0));
		state.request_manager.get_or_insert(a1, ch(0xA1), GroupIndex(0));
		state.request_manager.get_or_insert(a2, ch(0xA2), GroupIndex(0));
		state.request_manager.get_or_insert(a3, ch(0xA3), GroupIndex(0));
		state.request_manager.get_or_insert(unrelated_sp, ch(0xFE), GroupIndex(0));
		assert_eq!(state.request_manager.total_requests_count(), 6);

		handle_deactivate_leaves(&mut state, &[leaf_a]);

		// leaf_b is still active.
		assert!(state.implicit_view.contains_leaf(&leaf_b));
		assert!(!state.implicit_view.contains_leaf(&leaf_a));

		// Ancestors reachable from leaf_b (leaf_a, a1) survive in the implicit view; older
		// ones (a2, a3) are pruned.
		assert!(
			state.per_scheduling_parent.contains_key(&leaf_b),
			"leaf_b's per-scheduling-parent entry must remain",
		);
		assert!(
			!state.per_scheduling_parent.contains_key(&a2),
			"a2 falls outside leaf_b's retain window and should be pruned",
		);

		// Request cleanup must follow the same "pruned = gone" rule, keyed on the *pruned*
		// scheduling parent, not on the deactivated leaf. In particular: requests tied to
		// ancestors of the deactivated leaf that were pruned (a2) must be removed; requests
		// tied to still-referenced blocks (leaf_b, leaf_a, a1) must survive.
		assert_eq!(state.request_manager.requests_count_by_scheduling_parent(a2), 0);
		assert_eq!(state.request_manager.requests_count_by_scheduling_parent(leaf_b), 1);
		assert_eq!(state.request_manager.requests_count_by_scheduling_parent(unrelated_sp), 1);
		// a3 was never in the implicit view at all — its request survives, because cleanup
		// only targets blocks that were actually pruned from the view.
		assert_eq!(state.request_manager.requests_count_by_scheduling_parent(a3), 1);

		// `per_session` survives because leaf_b still references SESSION.
		assert!(state.per_session.contains_key(&SESSION));
		assert!(state.unused_topologies.contains_key(&SESSION));
	}
}
