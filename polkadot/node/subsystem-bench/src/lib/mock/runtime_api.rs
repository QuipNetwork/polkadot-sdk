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

//! A generic runtime api subsystem mockup suitable to be used in benchmarks.

use crate::configuration::{TestAuthorities, TestConfiguration};
use bitvec::prelude::BitVec;
use futures::FutureExt;
use itertools::Itertools;
use polkadot_node_subsystem::{
	messages::{RuntimeApiMessage, RuntimeApiRequest},
	overseer, SpawnedSubsystem, SubsystemError,
};
use polkadot_node_subsystem_types::OverseerSignal;
use polkadot_primitives::{
	node_features, ApprovalVotingParams, AsyncBackingParams, CandidateEvent,
	CandidateReceiptV2 as CandidateReceipt, CoreIndex, CoreState, GroupIndex, GroupRotationInfo,
	Id as ParaId, IndexedVec, NodeFeatures, OccupiedCore, ScheduledCore, SessionIndex, SessionInfo,
	ValidationCode, ValidatorIndex, MAX_COALESCE_APPROVALS,
};
use sp_consensus_babe::Epoch as BabeEpoch;
use sp_core::H256;
use std::collections::{BTreeMap, HashMap, VecDeque};

const LOG_TARGET: &str = "subsystem-bench::runtime-api-mock";

/// Minimal state to answer requests.
#[derive(Clone)]
pub struct RuntimeApiState {
	// All authorities in the test,
	authorities: TestAuthorities,
	// Node features state in the runtime
	node_features: NodeFeatures,
	// Candidate hashes per block
	candidate_hashes: HashMap<H256, Vec<CandidateReceipt>>,
	// Candidate events per block
	candidate_events: HashMap<H256, Vec<CandidateEvent>>,
	babe_epoch: Option<BabeEpoch>,
	// The session child index,
	session_index: SessionIndex,
	// The claim queue
	claim_queue: BTreeMap<CoreIndex, VecDeque<ParaId>>,
}

#[derive(Clone)]
pub enum MockRuntimeApiCoreState {
	Occupied,
	Scheduled,
	#[allow(dead_code)]
	Free,
}

/// A mocked `runtime-api` subsystem.
#[derive(Clone)]
pub struct MockRuntimeApi {
	state: RuntimeApiState,
	config: TestConfiguration,
	core_state: MockRuntimeApiCoreState,
}

impl MockRuntimeApi {
	pub fn new(
		config: TestConfiguration,
		authorities: TestAuthorities,
		candidate_hashes: HashMap<H256, Vec<CandidateReceipt>>,
		candidate_events: HashMap<H256, Vec<CandidateEvent>>,
		babe_epoch: Option<BabeEpoch>,
		session_index: SessionIndex,
		core_state: MockRuntimeApiCoreState,
	) -> MockRuntimeApi {
		// Enable chunk mapping feature to make systematic av-recovery possible.
		let node_features = default_node_features();
		let validator_group_count =
			session_info_for_peers(&config, &authorities).validator_groups.len();

		// Each para gets one core assigned and there is only one candidate per
		// parachain per relay chain block (no elastic scaling).
		let claim_queue = candidate_hashes
			.iter()
			.next()
			.expect("Candidates are generated at test start")
			.1
			.iter()
			.enumerate()
			.map(|(index, candidate_receipt)| {
				// Ensure test breaks if badly configured.
				assert!(index < validator_group_count);
				(CoreIndex(index as u32), vec![candidate_receipt.descriptor.para_id()].into())
			})
			.collect();

		Self {
			state: RuntimeApiState {
				authorities,
				candidate_hashes,
				candidate_events,
				babe_epoch,
				session_index,
				node_features,
				claim_queue,
			},
			config,
			core_state,
		}
	}

	fn session_info(&self) -> SessionInfo {
		session_info_for_peers(&self.config, &self.state.authorities)
	}
}

/// Generates a test session info with all passed authorities as consensus validators.
pub fn session_info_for_peers(
	configuration: &TestConfiguration,
	authorities: &TestAuthorities,
) -> SessionInfo {
	let all_validators = (0..configuration.n_validators)
		.map(|i| ValidatorIndex(i as _))
		.collect::<Vec<_>>();

	let validator_groups = all_validators
		.chunks(configuration.max_validators_per_core)
		.map(Vec::from)
		.collect::<Vec<_>>();

	SessionInfo {
		validators: authorities.validator_public.iter().cloned().collect(),
		discovery_keys: authorities.validator_authority_id.to_vec(),
		assignment_keys: authorities.validator_assignment_id.to_vec(),
		validator_groups: IndexedVec::<GroupIndex, Vec<ValidatorIndex>>::from(validator_groups),
		n_cores: configuration.n_cores as u32,
		needed_approvals: configuration.needed_approvals as u32,
		zeroth_delay_tranche_width: configuration.zeroth_delay_tranche_width as u32,
		relay_vrf_modulo_samples: configuration.relay_vrf_modulo_samples as u32,
		n_delay_tranches: configuration.n_delay_tranches as u32,
		no_show_slots: configuration.no_show_slots as u32,
		active_validator_indices: (0..authorities.validator_authority_id.len())
			.map(|index| ValidatorIndex(index as u32))
			.collect_vec(),
		dispute_period: 6,
		random_seed: [0u8; 32],
	}
}

#[overseer::subsystem(RuntimeApi, error=SubsystemError, prefix=self::overseer)]
impl<Context> MockRuntimeApi {
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = self.run(ctx).map(|_| Ok(())).boxed();

		SpawnedSubsystem { name: "test-environment", future }
	}
}

#[overseer::contextbounds(RuntimeApi, prefix = self::overseer)]
impl MockRuntimeApi {
	async fn run<Context>(self, mut ctx: Context) {
		let validator_group_count = self.session_info().validator_groups.len();

		loop {
			let msg = ctx.recv().await.expect("Overseer never fails us");

			match msg {
				orchestra::FromOrchestra::Signal(signal) => {
					if signal == OverseerSignal::Conclude {
						return;
					}
				},
				orchestra::FromOrchestra::Communication { msg } => {
					gum::debug!(target: LOG_TARGET, msg=?msg, "recv message");

					match msg {
						RuntimeApiMessage::Request(
							request,
							RuntimeApiRequest::CandidateEvents(sender),
						) => {
							let candidate_events = self.state.candidate_events.get(&request);
							let _ = sender.send(Ok(candidate_events.cloned().unwrap_or_default()));
						},
						RuntimeApiMessage::Request(
							_block_hash,
							RuntimeApiRequest::SessionInfo(_session_index, sender),
						) => {
							let _ = sender.send(Ok(Some(self.session_info())));
						},
						RuntimeApiMessage::Request(
							_block_hash,
							RuntimeApiRequest::NodeFeatures(_session_index, sender),
						) => {
							let _ = sender.send(Ok(self.state.node_features.clone()));
						},
						RuntimeApiMessage::Request(
							_block_hash,
							RuntimeApiRequest::Validators(sender),
						) => {
							let _ =
								sender.send(Ok(self.state.authorities.validator_public.clone()));
						},
						RuntimeApiMessage::Request(
							_block_hash,
							RuntimeApiRequest::SessionIndexForChild(sender),
						) => {
							// Session is always the same.
							let _ = sender.send(Ok(self.state.session_index));
						},
						RuntimeApiMessage::Request(
							block_hash,
							RuntimeApiRequest::AvailabilityCores(sender),
						) => {
							let candidate_hashes = self
								.state
								.candidate_hashes
								.get(&block_hash)
								.expect("Relay chain block hashes are generated at test start");

							// All cores are always occupied.
							let cores = candidate_hashes
								.iter()
								.enumerate()
								.map(|(index, candidate_receipt)| {
									// Ensure test breaks if badly configured.
									assert!(index < validator_group_count);

									use MockRuntimeApiCoreState::*;
									match self.core_state {
										Occupied => CoreState::Occupied(OccupiedCore {
											next_up_on_available: None,
											occupied_since: 0,
											time_out_at: 0,
											next_up_on_time_out: None,
											availability: BitVec::default(),
											group_responsible: GroupIndex(index as u32),
											candidate_hash: candidate_receipt.hash(),
											candidate_descriptor: candidate_receipt
												.descriptor
												.clone(),
										}),
										Scheduled => CoreState::Scheduled(ScheduledCore {
											para_id: (index + 1).into(),
											collator: None,
										}),
										Free => todo!(),
									}
								})
								.collect::<Vec<_>>();

							let _ = sender.send(Ok(cores));
						},
						RuntimeApiMessage::Request(
							_request,
							RuntimeApiRequest::CurrentBabeEpoch(sender),
						) => {
							let _ = sender.send(Ok(self
								.state
								.babe_epoch
								.clone()
								.expect("Babe epoch unpopulated")));
						},
						RuntimeApiMessage::Request(
							_block_hash,
							RuntimeApiRequest::AsyncBackingParams(sender),
						) => {
							let _ = sender.send(Ok(AsyncBackingParams {
								max_candidate_depth: self.config.max_candidate_depth,
								allowed_ancestry_len: self.config.allowed_ancestry_len,
							}));
						},
						RuntimeApiMessage::Request(_parent, RuntimeApiRequest::Version(tx)) => {
							tx.send(Ok(RuntimeApiRequest::DISABLED_VALIDATORS_RUNTIME_REQUIREMENT))
								.unwrap();
						},
						RuntimeApiMessage::Request(
							_parent,
							RuntimeApiRequest::DisabledValidators(tx),
						) => {
							tx.send(Ok(vec![])).unwrap();
						},
						RuntimeApiMessage::Request(
							_parent,
							RuntimeApiRequest::MinimumBackingVotes(_session_index, tx),
						) => {
							tx.send(Ok(self.config.minimum_backing_votes)).unwrap();
						},
						RuntimeApiMessage::Request(
							_parent,
							RuntimeApiRequest::ValidatorGroups(tx),
						) => {
							let groups = self.session_info().validator_groups.to_vec();
							let group_rotation_info = GroupRotationInfo {
								session_start_block: 1,
								group_rotation_frequency: 12,
								now: 1,
							};
							tx.send(Ok((groups, group_rotation_info))).unwrap();
						},
						RuntimeApiMessage::Request(
							_parent,
							RuntimeApiRequest::ValidationCodeByHash(hash, tx),
						) => {
							gum::debug!(target: LOG_TARGET, "ValidationCodeByHash: {:?}", hash);
							let validation_code = ValidationCode(Vec::new());
							if let Err(err) = tx.send(Ok(Some(validation_code))) {
								gum::error!(target: LOG_TARGET, ?err, "validation code wasn't received");
							}
						},
						RuntimeApiMessage::Request(
							_parent,
							RuntimeApiRequest::ApprovalVotingParams(_, tx),
						) => {
							if let Err(err) = tx.send(Ok(ApprovalVotingParams {
								max_approval_coalesce_count: MAX_COALESCE_APPROVALS,
							})) {
								gum::error!(target: LOG_TARGET, ?err, "Voting params weren't received");
							}
						},
						RuntimeApiMessage::Request(_parent, RuntimeApiRequest::ClaimQueue(tx)) => {
							tx.send(Ok(self.state.claim_queue.clone())).unwrap();
						},
						RuntimeApiMessage::Request(
							_parent,
							RuntimeApiRequest::FetchOnChainVotes(tx),
						) => {
							tx.send(Ok(None)).unwrap();
						},
						RuntimeApiMessage::Request(
							_parent,
							RuntimeApiRequest::UnappliedSlashes(tx),
						) => {
							tx.send(Ok(vec![])).unwrap();
						},
						RuntimeApiMessage::Request(
							_parent,
							RuntimeApiRequest::UnappliedSlashesV2(tx),
						) => {
							tx.send(Ok(vec![])).unwrap();
						},
						RuntimeApiMessage::Request(
							_parent,
							RuntimeApiRequest::SchedulingLookahead(_session, tx),
						) => {
							tx.send(Ok(2)).unwrap();
						},
						// Long term TODO: implement more as needed.
						message => {
							unimplemented!("Unexpected runtime-api message: {:?}", message)
						},
					}
				},
			}
		}
	}
}

pub fn default_node_features() -> NodeFeatures {
	let mut node_features = NodeFeatures::new();
	node_features.resize(node_features::FeatureIndex::FirstUnassigned as usize, false);
	node_features.set(node_features::FeatureIndex::AvailabilityChunkMapping as u8 as usize, true);
	node_features.set(node_features::FeatureIndex::ElasticScalingMVP as u8 as usize, true);
	node_features.set(node_features::FeatureIndex::CandidateReceiptV2 as u8 as usize, true);

	node_features
}
