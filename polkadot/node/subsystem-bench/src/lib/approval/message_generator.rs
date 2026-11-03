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

use crate::{
	approval::{
		helpers::{generate_babe_epoch, generate_topology},
		test_message::{MessagesBundle, TestMessageInfo},
		ApprovalTestState, ApprovalsOptions, BlockTestData, GeneratedState,
		BUFFER_FOR_GENERATION_MILLIS, LOG_TARGET, SLOT_DURATION_MILLIS,
	},
	configuration::{TestAuthorities, TestConfiguration},
	mock::runtime_api::session_info_for_peers,
	NODE_UNDER_TEST,
};
use codec::Encode;
use futures::SinkExt;
use itertools::Itertools;
use polkadot_node_core_approval_voting::criteria::{compute_assignments, Config};

use polkadot_node_network_protocol::{
	grid_topology::{GridNeighbors, RandomRouting, RequiredRouting, SessionGridTopology},
	v3 as protocol_v3,
};
use polkadot_node_primitives::approval::{
	self,
	time::tranche_to_tick,
	v2::{CoreBitfield, IndirectAssignmentCertV2, IndirectSignedApprovalVoteV2},
};
use polkadot_primitives::{
	ApprovalVoteMultipleCandidates, CandidateEvent, CandidateHash, CandidateIndex, CoreIndex, Hash,
	SessionInfo, Slot, ValidatorId, ValidatorIndex, ASSIGNMENT_KEY_TYPE_ID,
};
use rand::{seq::SliceRandom, RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use rand_distr::{Distribution, Normal};
use sc_keystore::LocalKeystore;
use sc_network_types::PeerId;
use sc_service::SpawnTaskHandle;
use sha1::Digest;
use sp_application_crypto::AppCrypto;
use sp_consensus_babe::SlotDuration;
use sp_keystore::Keystore;
use sp_timestamp::Timestamp;
use std::{
	cmp::max,
	collections::{BTreeMap, HashSet},
	fs,
	io::Write,
	path::{Path, PathBuf},
	time::Duration,
};

/// A generator of messages coming from a given Peer/Validator
pub struct PeerMessagesGenerator {
	/// The grid neighbors of the node under test.
	pub topology_node_under_test: GridNeighbors,
	/// The topology of the network for the epoch under test.
	pub topology: SessionGridTopology,
	/// The validator index for this object generates the messages.
	pub validator_index: ValidatorIndex,
	/// An array of pre-generated random samplings, that is used to determine, which nodes would
	/// send a given assignment, to the node under test because of the random samplings.
	/// As an optimization we generate this sampling at the beginning of the test and just pick
	/// one randomly, because always taking the samples would be too expensive for benchmark.
	pub random_samplings: Vec<Vec<ValidatorIndex>>,
	/// Channel for sending the generated messages to the aggregator
	pub tx_messages: futures::channel::mpsc::UnboundedSender<(Hash, Vec<MessagesBundle>)>,
	/// The list of test authorities
	pub test_authorities: TestAuthorities,
	//// The session info used for the test.
	pub session_info: SessionInfo,
	/// The blocks used for testing
	pub blocks: Vec<BlockTestData>,
	/// Approval options params.
	pub options: ApprovalsOptions,
}

impl PeerMessagesGenerator {
	/// Generates messages by spawning a blocking task in the background which begins creating
	/// the assignments/approvals and peer view changes at the beginning of each block.
	pub fn generate_messages(mut self, spawn_task_handle: &SpawnTaskHandle) {
		spawn_task_handle.spawn("generate-messages", "generate-messages", async move {
			for block_info in &self.blocks {
				let assignments = self.generate_assignments(block_info);

				let bytes = self.validator_index.0.to_be_bytes();
				let seed = [
					bytes[0], bytes[1], bytes[2], bytes[3], 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
					0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
				];

				let mut rand_chacha = ChaCha20Rng::from_seed(seed);
				let approvals = issue_approvals(
					assignments,
					block_info.hash,
					&self.test_authorities.validator_public,
					block_info.candidates.clone(),
					&self.options,
					&mut rand_chacha,
					self.test_authorities.keyring.keystore_ref(),
				);

				self.tx_messages
					.send((block_info.hash, approvals))
					.await
					.expect("Should not fail");
			}
		})
	}

	// Builds the messages finger print corresponding to this configuration.
	// When the finger print exists already on disk the messages are not re-generated.
	fn messages_fingerprint(
		configuration: &TestConfiguration,
		options: &ApprovalsOptions,
	) -> String {
		let mut fingerprint = options.fingerprint();
		let configuration_bytes = bincode::serialize(&configuration).unwrap();
		fingerprint.extend(configuration_bytes);
		let mut sha1 = sha1::Sha1::new();
		sha1.update(fingerprint);
		let result = sha1.finalize();
		hex::encode(result)
	}

	/// Generate all messages(Assignments & Approvals) needed for approving `blocks``.
	pub fn generate_messages_if_needed(
		configuration: &TestConfiguration,
		test_authorities: &TestAuthorities,
		options: &ApprovalsOptions,
		spawn_task_handle: &SpawnTaskHandle,
	) -> PathBuf {
		let path_name = format!(
			"{}/{}",
			options.workdir_prefix,
			Self::messages_fingerprint(configuration, options)
		);

		let path = Path::new(&path_name);
		if path.exists() {
			return path.to_path_buf();
		}

		gum::info!("Generate message because file does not exist");
		let delta_to_first_slot_under_test = Timestamp::new(BUFFER_FOR_GENERATION_MILLIS);
		let initial_slot = Slot::from_timestamp(
			(*Timestamp::current() - *delta_to_first_slot_under_test).into(),
			SlotDuration::from_millis(SLOT_DURATION_MILLIS),
		);

		let babe_epoch = generate_babe_epoch(initial_slot, test_authorities.clone());
		let session_info = session_info_for_peers(configuration, test_authorities);
		let blocks = ApprovalTestState::generate_blocks_information(
			configuration,
			&babe_epoch,
			initial_slot,
		);

		gum::info!(target: LOG_TARGET, "Generate messages");
		let topology = generate_topology(test_authorities);

		let random_samplings = random_samplings_to_node(
			ValidatorIndex(NODE_UNDER_TEST),
			test_authorities.validator_public.len(),
			test_authorities.validator_public.len() * 2,
		);

		let topology_node_under_test =
			topology.compute_grid_neighbors_for(ValidatorIndex(NODE_UNDER_TEST)).unwrap();

		let (tx, mut rx) = futures::channel::mpsc::unbounded();

		// Spawn a thread to generate the messages for each validator, so that we speed up the
		// generation.
		for current_validator_index in 1..test_authorities.validator_public.len() {
			let peer_message_source = PeerMessagesGenerator {
				topology_node_under_test: topology_node_under_test.clone(),
				topology: topology.clone(),
				validator_index: ValidatorIndex(current_validator_index as u32),
				test_authorities: test_authorities.clone(),
				session_info: session_info.clone(),
				blocks: blocks.clone(),
				tx_messages: tx.clone(),
				random_samplings: random_samplings.clone(),
				options: options.clone(),
			};

			peer_message_source.generate_messages(spawn_task_handle);
		}

		std::mem::drop(tx);

		let seed = [0x32; 32];
		let mut rand_chacha = ChaCha20Rng::from_seed(seed);

		let mut all_messages: BTreeMap<u64, Vec<MessagesBundle>> = BTreeMap::new();
		// Receive all messages and sort them by Tick they have to be sent.
		loop {
			match rx.try_next() {
				Ok(Some((block_hash, messages))) => {
					for message in messages {
						let block_info = blocks
							.iter()
							.find(|val| val.hash == block_hash)
							.expect("Should find blocks");
						let tick_to_send = tranche_to_tick(
							SLOT_DURATION_MILLIS,
							block_info.slot,
							message.tranche_to_send(),
						);
						let to_add = all_messages.entry(tick_to_send).or_default();
						to_add.push(message);
					}
				},
				Ok(None) => break,
				Err(_) => {
					std::thread::sleep(Duration::from_millis(50));
				},
			}
		}
		let all_messages = all_messages
			.into_iter()
			.flat_map(|(_, mut messages)| {
				// Shuffle the messages inside the same tick, so that we don't priorities messages
				// for older nodes. we try to simulate the same behaviour as in real world.
				messages.shuffle(&mut rand_chacha);
				messages
			})
			.collect_vec();

		gum::info!("Generated a number of {:} unique messages", all_messages.len());

		let generated_state = GeneratedState { all_messages: Some(all_messages), initial_slot };

		let mut messages_file = fs::OpenOptions::new()
			.write(true)
			.create(true)
			.truncate(true)
			.open(path)
			.unwrap();

		messages_file
			.write_all(&generated_state.encode())
			.expect("Could not update message file");
		path.to_path_buf()
	}

	/// Generates assignments for the given `current_validator_index`
	/// Returns a list of assignments to be sent sorted by tranche.
	fn generate_assignments(&self, block_info: &BlockTestData) -> Vec<TestMessageInfo> {
		let config = Config::from(&self.session_info);

		let leaving_cores = block_info
			.candidates
			.clone()
			.into_iter()
			.map(|candidate_event| {
				if let CandidateEvent::CandidateIncluded(candidate, _, core_index, group_index) =
					candidate_event
				{
					(candidate.hash(), core_index, group_index)
				} else {
					todo!("Variant is never created in this benchmark")
				}
			})
			.collect_vec();

		let mut assignments_by_tranche = BTreeMap::new();

		let bytes = self.validator_index.0.to_be_bytes();
		let seed = [
			bytes[0], bytes[1], bytes[2], bytes[3], 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
			0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
		];
		let mut rand_chacha = ChaCha20Rng::from_seed(seed);

		let to_be_sent_by = neighbours_that_would_sent_message(
			&self.test_authorities.peer_ids,
			self.validator_index.0,
			&self.topology_node_under_test,
			&self.topology,
		);

		let leaving_cores = leaving_cores
			.clone()
			.into_iter()
			.filter(|(_, core_index, _group_index)| core_index.0 != self.validator_index.0)
			.collect_vec();

		let store = LocalKeystore::in_memory();
		let _public = store
			.sr25519_generate_new(
				ASSIGNMENT_KEY_TYPE_ID,
				Some(self.test_authorities.key_seeds[self.validator_index.0 as usize].as_str()),
			)
			.expect("should not fail");
		let assignments = compute_assignments(
			&store,
			block_info.relay_vrf_story.clone(),
			&config,
			leaving_cores.clone(),
		);

		let random_sending_nodes = self
			.random_samplings
			.get(rand_chacha.next_u32() as usize % self.random_samplings.len())
			.unwrap();
		let random_sending_peer_ids = random_sending_nodes
			.iter()
			.map(|validator| (*validator, self.test_authorities.peer_ids[validator.0 as usize]))
			.collect_vec();

		let mut unique_assignments = HashSet::new();
		for (_core_index, assignment) in assignments {
			let assigned_cores = match &assignment.cert().kind {
				approval::v2::AssignmentCertKindV2::RelayVRFModuloCompact { core_bitfield } => {
					core_bitfield.iter_ones().map(|val| CoreIndex::from(val as u32)).collect_vec()
				},
				approval::v2::AssignmentCertKindV2::RelayVRFDelay { core_index } => {
					vec![*core_index]
				},
			};

			let bitfiled: CoreBitfield = assigned_cores.clone().try_into().unwrap();

			// For the cases where tranch0 assignments are in a single certificate we need to make
			// sure we create a single message.
			if unique_assignments.insert(bitfiled) {
				let this_tranche_assignments =
					assignments_by_tranche.entry(assignment.tranche()).or_insert_with(Vec::new);

				this_tranche_assignments.push((
					IndirectAssignmentCertV2 {
						block_hash: block_info.hash,
						validator: self.validator_index,
						cert: assignment.cert().clone(),
					},
					block_info
						.candidates
						.iter()
						.enumerate()
						.filter(|(_index, candidate)| {
							if let CandidateEvent::CandidateIncluded(_, _, core, _) = candidate {
								assigned_cores.contains(core)
							} else {
								panic!("Should not happen");
							}
						})
						.map(|(index, _)| index as u32)
						.collect_vec()
						.try_into()
						.unwrap(),
					to_be_sent_by
						.iter()
						.chain(random_sending_peer_ids.iter())
						.copied()
						.collect::<HashSet<(ValidatorIndex, PeerId)>>(),
					assignment.tranche(),
				));
			}
		}

		assignments_by_tranche
			.into_values()
			.flat_map(|assignments| assignments.into_iter())
			.map(|assignment| {
				let msg = protocol_v3::ApprovalDistributionMessage::Assignments(vec![(
					assignment.0,
					assignment.1,
				)]);
				TestMessageInfo {
					msg,
					sent_by: assignment
						.2
						.into_iter()
						.map(|(validator_index, _)| validator_index)
						.collect_vec(),
					tranche: assignment.3,
					block_hash: block_info.hash,
				}
			})
			.collect_vec()
	}
}

/// A list of random samplings that we use to determine which nodes should send a given message to
/// the node under test.
/// We can not sample every time for all the messages because that would be too expensive to
/// perform, so pre-generate a list of samples for a given network size.
/// - result[i] give us as a list of random nodes that would send a given message to the node under
///   test.
fn random_samplings_to_node(
	node_under_test: ValidatorIndex,
	num_validators: usize,
	num_samplings: usize,
) -> Vec<Vec<ValidatorIndex>> {
	let seed = [7u8; 32];
	let mut rand_chacha = ChaCha20Rng::from_seed(seed);

	(0..num_samplings)
		.map(|_| {
			(0..num_validators)
				.filter(|sending_validator_index| {
					*sending_validator_index != NODE_UNDER_TEST as usize
				})
				.flat_map(|sending_validator_index| {
					let mut validators = (0..num_validators).collect_vec();
					validators.shuffle(&mut rand_chacha);

					let mut random_routing = RandomRouting::default();
					validators
						.into_iter()
						.flat_map(|validator_to_send| {
							if random_routing.sample(num_validators, &mut rand_chacha) {
								random_routing.inc_sent();
								if validator_to_send == node_under_test.0 as usize {
									Some(ValidatorIndex(sending_validator_index as u32))
								} else {
									None
								}
							} else {
								None
							}
						})
						.collect_vec()
				})
				.collect_vec()
		})
		.collect_vec()
}

/// Helper function to randomly determine how many approvals we coalesce together in a single
/// message.
fn coalesce_approvals_len(
	coalesce_mean: f32,
	coalesce_std_dev: f32,
	rand_chacha: &mut ChaCha20Rng,
) -> usize {
	max(
		1,
		Normal::new(coalesce_mean, coalesce_std_dev)
			.expect("normal distribution parameters are good")
			.sample(rand_chacha)
			.round() as i32,
	) as usize
}

/// Helper function to create approvals signatures for all assignments passed as arguments.
/// Returns a list of Approvals messages that need to be sent.
fn issue_approvals(
	assignments: Vec<TestMessageInfo>,
	block_hash: Hash,
	validator_ids: &[ValidatorId],
	candidates: Vec<CandidateEvent>,
	options: &ApprovalsOptions,
	rand_chacha: &mut ChaCha20Rng,
	store: &LocalKeystore,
) -> Vec<MessagesBundle> {
	let mut queued_to_sign: Vec<TestSignInfo> = Vec::new();
	let mut num_coalesce =
		coalesce_approvals_len(options.coalesce_mean, options.coalesce_std_dev, rand_chacha);
	let result = assignments
		.iter()
		.map(|message| match &message.msg {
			protocol_v3::ApprovalDistributionMessage::Assignments(assignments) => {
				let mut approvals_to_create = Vec::new();

				let current_validator_index = queued_to_sign
					.first()
					.map(|msg| msg.validator_index)
					.unwrap_or(ValidatorIndex(99999));

				// Invariant for this benchmark.
				assert_eq!(assignments.len(), 1);

				let assignment = assignments.first().unwrap();

				let earliest_tranche = queued_to_sign
					.first()
					.map(|val| val.assignment.tranche)
					.unwrap_or(message.tranche);

				if queued_to_sign.len() >= num_coalesce ||
					(!queued_to_sign.is_empty() &&
						current_validator_index != assignment.0.validator) ||
					message.tranche - earliest_tranche >= options.coalesce_tranche_diff
				{
					approvals_to_create.push(TestSignInfo::sign_candidates(
						&mut queued_to_sign,
						validator_ids,
						block_hash,
						num_coalesce,
						store,
					));
					num_coalesce = coalesce_approvals_len(
						options.coalesce_mean,
						options.coalesce_std_dev,
						rand_chacha,
					);
				}

				// If more that one candidate was in the assignment queue all of them for issuing
				// approvals
				for candidate_index in assignment.1.iter_ones() {
					let candidate = candidates.get(candidate_index).unwrap();
					if let CandidateEvent::CandidateIncluded(candidate, _, _, _) = candidate {
						queued_to_sign.push(TestSignInfo {
							candidate_hash: candidate.hash(),
							candidate_index: candidate_index as CandidateIndex,
							validator_index: assignment.0.validator,
							assignment: message.clone(),
						});
					} else {
						todo!("Other enum variants are not used in this benchmark");
					}
				}
				approvals_to_create
			},
			_ => {
				todo!("Other enum variants are not used in this benchmark");
			},
		})
		.collect_vec();

	let mut messages = result.into_iter().flatten().collect_vec();

	if !queued_to_sign.is_empty() {
		messages.push(TestSignInfo::sign_candidates(
			&mut queued_to_sign,
			validator_ids,
			block_hash,
			num_coalesce,
			store,
		));
	}
	messages
}

/// Helper struct to gather information about more than one candidate an sign it in a single
/// approval message.
struct TestSignInfo {
	/// The candidate hash
	candidate_hash: CandidateHash,
	/// The candidate index
	candidate_index: CandidateIndex,
	/// The validator sending the assignments
	validator_index: ValidatorIndex,
	/// The assignments covering this candidate
	assignment: TestMessageInfo,
}

impl TestSignInfo {
	/// Helper function to create a signature for all candidates in `to_sign` parameter.
	/// Returns a TestMessage
	fn sign_candidates(
		to_sign: &mut Vec<TestSignInfo>,
		validator_ids: &[ValidatorId],
		block_hash: Hash,
		num_coalesce: usize,
		store: &LocalKeystore,
	) -> MessagesBundle {
		let current_validator_index = to_sign.first().map(|val| val.validator_index).unwrap();
		let tranche_approval_can_be_sent =
			to_sign.iter().map(|val| val.assignment.tranche).max().unwrap();
		let validator_id = validator_ids.get(current_validator_index.0 as usize).unwrap().clone();

		let unique_assignments: HashSet<TestMessageInfo> =
			to_sign.iter().map(|info| info.assignment.clone()).collect();

		let mut to_sign = to_sign
			.drain(..)
			.sorted_by(|val1, val2| val1.candidate_index.cmp(&val2.candidate_index))
			.peekable();

		let mut bundle = MessagesBundle {
			assignments: unique_assignments.into_iter().collect_vec(),
			approvals: Vec::new(),
		};

		while to_sign.peek().is_some() {
			let to_sign = to_sign.by_ref().take(num_coalesce).collect_vec();

			let hashes = to_sign.iter().map(|val| val.candidate_hash).collect_vec();
			let candidate_indices = to_sign.iter().map(|val| val.candidate_index).collect_vec();

			let sent_by = to_sign
				.iter()
				.flat_map(|val| val.assignment.sent_by.iter())
				.copied()
				.collect::<HashSet<ValidatorIndex>>();

			let payload = ApprovalVoteMultipleCandidates(&hashes).signing_payload(1);

			let signature = store
				.sr25519_sign(ValidatorId::ID, &validator_id.clone().into(), &payload[..])
				.unwrap()
				.unwrap()
				.into();
			let indirect = IndirectSignedApprovalVoteV2 {
				block_hash,
				candidate_indices: candidate_indices.try_into().unwrap(),
				validator: current_validator_index,
				signature,
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Approvals(vec![indirect]);

			bundle.approvals.push(TestMessageInfo {
				msg,
				sent_by: sent_by.into_iter().collect_vec(),
				tranche: tranche_approval_can_be_sent,
				block_hash,
			});
		}
		bundle
	}
}

/// Determine what neighbours would send a given message to the node under test.
fn neighbours_that_would_sent_message(
	peer_ids: &[PeerId],
	current_validator_index: u32,
	topology_node_under_test: &GridNeighbors,
	topology: &SessionGridTopology,
) -> Vec<(ValidatorIndex, PeerId)> {
	let topology_originator = topology
		.compute_grid_neighbors_for(ValidatorIndex(current_validator_index))
		.unwrap();

	let originator_y = topology_originator.validator_indices_y.iter().find(|validator| {
		topology_node_under_test.required_routing_by_index(**validator, false) ==
			RequiredRouting::GridY
	});

	assert!(originator_y != Some(&ValidatorIndex(NODE_UNDER_TEST)));

	let originator_x = topology_originator.validator_indices_x.iter().find(|validator| {
		topology_node_under_test.required_routing_by_index(**validator, false) ==
			RequiredRouting::GridX
	});

	assert!(originator_x != Some(&ValidatorIndex(NODE_UNDER_TEST)));

	let is_neighbour = topology_originator
		.validator_indices_x
		.contains(&ValidatorIndex(NODE_UNDER_TEST)) ||
		topology_originator
			.validator_indices_y
			.contains(&ValidatorIndex(NODE_UNDER_TEST));

	let mut to_be_sent_by = originator_y
		.into_iter()
		.chain(originator_x)
		.map(|val| (*val, peer_ids[val.0 as usize]))
		.collect_vec();

	if is_neighbour {
		to_be_sent_by.push((ValidatorIndex(current_validator_index), peer_ids[0]));
	}

	to_be_sent_by
}
