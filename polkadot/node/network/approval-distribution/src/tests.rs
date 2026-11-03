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

use super::*;
use assert_matches::assert_matches;
use futures::{channel::oneshot, executor, future, Future};
use polkadot_node_network_protocol::{
	grid_topology::{SessionGridTopology, TopologyPeerInfo},
	our_view,
	peer_set::ValidationVersion,
	view, ObservedRole,
};
use polkadot_node_primitives::approval::{
	criteria,
	v1::{VrfPreOutput, VrfProof, VrfSignature},
	v2::{
		AssignmentCertKindV2, AssignmentCertV2, CoreBitfield, IndirectAssignmentCertV2,
		RELAY_VRF_MODULO_CONTEXT,
	},
};
use polkadot_node_subsystem::messages::{
	network_bridge_event, AllMessages, ReportPeerMessage, RuntimeApiRequest,
};
use polkadot_node_subsystem_util::{reputation::add_reputation, TimeoutExt as _};
use polkadot_primitives::{
	ApprovalVoteMultipleCandidates, ApprovalVotingParams, AuthorityDiscoveryId, BlakeTwo256,
	CoreIndex, HashT, NodeFeatures, SessionInfo, ValidatorId, MAX_COALESCE_APPROVALS,
};
use polkadot_primitives_test_helpers::dummy_signature;
use rand::SeedableRng;
use sc_keystore::{Keystore, LocalKeystore};
use sp_application_crypto::AppCrypto;
use sp_authority_discovery::AuthorityPair as AuthorityDiscoveryPair;
use sp_core::crypto::Pair as PairT;
use std::time::Duration;
type VirtualOverseer =
	polkadot_node_subsystem_test_helpers::TestSubsystemContextHandle<ApprovalDistributionMessage>;

fn test_harness<T: Future<Output = VirtualOverseer>>(
	assignment_criteria: Arc<dyn AssignmentCriteria + Send + Sync>,
	clock: Arc<dyn Clock + Send + Sync>,
	mut state: State,
	test_fn: impl FnOnce(VirtualOverseer) -> T,
) -> State {
	sp_tracing::init_for_tests();

	let pool = sp_core::testing::TaskExecutor::new();
	let (context, virtual_overseer) =
		polkadot_node_subsystem_test_helpers::make_subsystem_context(pool.clone());

	let subsystem = ApprovalDistribution::new_with_clock(
		Metrics::default(),
		Default::default(),
		clock,
		assignment_criteria,
	);
	{
		let mut rng = rand_chacha::ChaCha12Rng::seed_from_u64(12345);
		let mut session_info_provider = RuntimeInfo::new_with_config(RuntimeInfoConfig {
			keystore: None,
			session_cache_lru_size: DISPUTE_WINDOW.get(),
		});

		let (tx, rx) = oneshot::channel();
		let subsystem = async {
			subsystem
				.run_inner(
					context,
					&mut state,
					REPUTATION_CHANGE_TEST_INTERVAL,
					&mut rng,
					&mut session_info_provider,
				)
				.await;
			tx.send(()).expect("Fail to notify subystem is done");
		};

		let test_fut = test_fn(virtual_overseer);

		futures::pin_mut!(test_fut);
		futures::pin_mut!(subsystem);

		executor::block_on(future::join(
			async move {
				let mut overseer = test_fut.await;
				overseer
					.send(FromOrchestra::Signal(OverseerSignal::Conclude))
					.timeout(TIMEOUT)
					.await
					.expect("Conclude send timeout");
				let _ =
					rx.timeout(Duration::from_secs(2)).await.expect("Subsystem did not conclude");
			},
			subsystem,
		));
	}

	state
}

const TIMEOUT: Duration = Duration::from_millis(200);
const REPUTATION_CHANGE_TEST_INTERVAL: Duration = Duration::from_millis(1);

async fn overseer_send(overseer: &mut VirtualOverseer, msg: ApprovalDistributionMessage) {
	gum::trace!(msg = ?msg, "Sending message");
	overseer
		.send(FromOrchestra::Communication { msg })
		.timeout(TIMEOUT)
		.await
		.expect("msg send timeout");
}

async fn overseer_signal_block_finalized(overseer: &mut VirtualOverseer, number: BlockNumber) {
	gum::trace!(?number, "Sending a finalized signal");
	// we don't care about the block hash
	overseer
		.send(FromOrchestra::Signal(OverseerSignal::BlockFinalized(Hash::zero(), number)))
		.timeout(TIMEOUT)
		.await
		.expect("signal send timeout");
}

async fn overseer_recv(overseer: &mut VirtualOverseer) -> AllMessages {
	gum::trace!("Waiting for a message");
	let msg = overseer.recv().timeout(TIMEOUT).await.expect("msg recv timeout");

	gum::trace!(msg = ?msg, "Received message");

	msg
}

async fn provide_session(virtual_overseer: &mut VirtualOverseer, session_info: SessionInfo) {
	provide_session_with_coalesce_count(virtual_overseer, session_info, MAX_COALESCE_APPROVALS)
		.await;
}

async fn provide_session_with_coalesce_count(
	virtual_overseer: &mut VirtualOverseer,
	session_info: SessionInfo,
	max_approval_coalesce_count: u32,
) {
	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::RuntimeApi(
			RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::SessionInfo(_, si_tx),
			)
		) => {
			si_tx.send(Ok(Some(session_info.clone()))).unwrap();
		}
	);
	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::RuntimeApi(
			RuntimeApiMessage::Request(_, RuntimeApiRequest::NodeFeatures(_, si_tx), )
		) => {
			si_tx.send(Ok(NodeFeatures::EMPTY)).unwrap();
		}
	);
	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::RuntimeApi(
			RuntimeApiMessage::Request(_, RuntimeApiRequest::ApprovalVotingParams(_, si_tx))
		) => {
			si_tx
				.send(Ok(ApprovalVotingParams { max_approval_coalesce_count }))
				.unwrap();
		}
	);
}

fn make_peers_and_authority_ids(n: usize) -> Vec<(PeerId, AuthorityDiscoveryId)> {
	(0..n)
		.map(|_| {
			let peer_id = PeerId::random();
			let authority_id = AuthorityDiscoveryPair::generate().0.public();

			(peer_id, authority_id)
		})
		.collect()
}

fn make_gossip_topology(
	session: SessionIndex,
	all_peers: &[(Option<PeerId>, AuthorityDiscoveryId)],
	neighbors_x: &[usize],
	neighbors_y: &[usize],
	local_index: usize,
) -> network_bridge_event::NewGossipTopology {
	// This builds a grid topology which is a square matrix.
	// The local validator occupies the top left-hand corner.
	// The X peers occupy the same row and the Y peers occupy
	// the same column.

	assert_eq!(
		neighbors_x.len(),
		neighbors_y.len(),
		"mocking grid topology only implemented for squares",
	);

	let d = neighbors_x.len() + 1;

	let grid_size = d * d;
	assert!(grid_size > 0);
	assert!(all_peers.len() >= grid_size);

	let peer_info = |i: usize| TopologyPeerInfo {
		peer_ids: all_peers[i].0.into_iter().collect_vec(),
		validator_index: ValidatorIndex::from(i as u32),
		discovery_id: all_peers[i].1.clone(),
	};

	let mut canonical_shuffling: Vec<_> = (0..)
		.filter(|i| local_index != *i)
		.filter(|i| !neighbors_x.contains(i))
		.filter(|i| !neighbors_y.contains(i))
		.take(grid_size)
		.map(peer_info)
		.collect();

	// filled with junk except for own.
	let mut shuffled_indices = vec![d + 1; grid_size];
	shuffled_indices[local_index] = 0;
	canonical_shuffling[0] = peer_info(local_index);

	for (x_pos, v) in neighbors_x.iter().enumerate() {
		let pos = 1 + x_pos;
		canonical_shuffling[pos] = peer_info(*v);
	}

	for (y_pos, v) in neighbors_y.iter().enumerate() {
		let pos = d * (1 + y_pos);
		canonical_shuffling[pos] = peer_info(*v);
	}

	let topology = SessionGridTopology::new(shuffled_indices, canonical_shuffling);

	// sanity check.
	{
		let g_n = topology
			.compute_grid_neighbors_for(ValidatorIndex(local_index as _))
			.expect("topology just constructed with this validator index");

		assert_eq!(g_n.validator_indices_x.len(), neighbors_x.len());
		assert_eq!(g_n.validator_indices_y.len(), neighbors_y.len());

		for i in neighbors_x {
			assert!(g_n.validator_indices_x.contains(&ValidatorIndex(*i as _)));
		}

		for i in neighbors_y {
			assert!(g_n.validator_indices_y.contains(&ValidatorIndex(*i as _)));
		}
	}

	network_bridge_event::NewGossipTopology {
		session,
		topology,
		local_index: Some(ValidatorIndex(local_index as _)),
	}
}

async fn setup_gossip_topology(
	virtual_overseer: &mut VirtualOverseer,
	gossip_topology: network_bridge_event::NewGossipTopology,
) {
	overseer_send(
		virtual_overseer,
		ApprovalDistributionMessage::NetworkBridgeUpdate(NetworkBridgeEvent::NewGossipTopology(
			gossip_topology,
		)),
	)
	.await;
}

async fn setup_peer_with_view(
	virtual_overseer: &mut VirtualOverseer,
	peer_id: &PeerId,
	view: View,
	version: ValidationVersion,
) {
	overseer_send(
		virtual_overseer,
		ApprovalDistributionMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerConnected(
			*peer_id,
			ObservedRole::Full,
			version.into(),
			None,
		)),
	)
	.await;
	overseer_send(
		virtual_overseer,
		ApprovalDistributionMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerViewChange(
			*peer_id, view,
		)),
	)
	.await;
}

async fn send_message_from_peer_v3(
	virtual_overseer: &mut VirtualOverseer,
	peer_id: &PeerId,
	msg: protocol_v3::ApprovalDistributionMessage,
) {
	overseer_send(
		virtual_overseer,
		ApprovalDistributionMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerMessage(
			*peer_id,
			ValidationProtocols::V3(msg),
		)),
	)
	.await;
}

fn fake_assignment_cert_v2(
	block_hash: Hash,
	validator: ValidatorIndex,
	core_bitfield: CoreBitfield,
) -> IndirectAssignmentCertV2 {
	let ctx = schnorrkel::signing_context(RELAY_VRF_MODULO_CONTEXT);
	let msg = b"WhenParachains?";
	let mut prng = rand_core::OsRng;
	let keypair = schnorrkel::Keypair::generate_with(&mut prng);
	let (inout, proof, _) = keypair.vrf_sign(ctx.bytes(msg));
	let preout = inout.to_preout();

	IndirectAssignmentCertV2 {
		block_hash,
		validator,
		cert: AssignmentCertV2 {
			kind: AssignmentCertKindV2::RelayVRFModuloCompact { core_bitfield },
			vrf: VrfSignature { pre_output: VrfPreOutput(preout), proof: VrfProof(proof) },
		},
	}
}

fn fake_assignment_cert_delay(
	block_hash: Hash,
	validator: ValidatorIndex,
	core_bitfield: CoreBitfield,
) -> IndirectAssignmentCertV2 {
	let ctx = schnorrkel::signing_context(RELAY_VRF_MODULO_CONTEXT);
	let msg = b"WhenParachains?";
	let mut prng = rand_core::OsRng;
	let keypair = schnorrkel::Keypair::generate_with(&mut prng);
	let (inout, proof, _) = keypair.vrf_sign(ctx.bytes(msg));
	let preout = inout.to_preout();

	IndirectAssignmentCertV2 {
		block_hash,
		validator,
		cert: AssignmentCertV2 {
			kind: AssignmentCertKindV2::RelayVRFDelay {
				core_index: CoreIndex(core_bitfield.iter_ones().next().unwrap() as u32),
			},
			vrf: VrfSignature { pre_output: VrfPreOutput(preout), proof: VrfProof(proof) },
		},
	}
}

async fn expect_reputation_change(
	virtual_overseer: &mut VirtualOverseer,
	peer_id: &PeerId,
	rep: Rep,
) {
	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(
			ReportPeerMessage::Single(p, r),
		)) => {
			assert_eq!(p, *peer_id);
			assert_eq!(r, rep.into());
		}
	);
}

async fn expect_reputation_changes(
	virtual_overseer: &mut VirtualOverseer,
	peer_id: &PeerId,
	reps: Vec<Rep>,
) {
	let mut acc = HashMap::new();
	for rep in reps {
		add_reputation(&mut acc, *peer_id, rep);
	}
	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(
			ReportPeerMessage::Batch(v),
		)) => {
			assert_eq!(v, acc);
		}
	);
}

fn state_without_reputation_delay() -> State {
	State { reputation: ReputationAggregator::new(|_| true), ..Default::default() }
}

fn state_with_reputation_delay() -> State {
	State { reputation: ReputationAggregator::new(|_| false), ..Default::default() }
}

fn dummy_session_info_valid(
	index: SessionIndex,
	keystore: &mut LocalKeystore,
	num_validators: usize,
) -> SessionInfo {
	let keys = (0..num_validators)
		.map(|_| {
			keystore
				.sr25519_generate_new(ValidatorId::ID, Some("//Node"))
				.expect("Insert key into keystore")
		})
		.collect_vec();

	SessionInfo {
		validators: keys.clone().into_iter().map(|key| key.into()).collect(),
		discovery_keys: keys.clone().into_iter().map(|key| key.into()).collect(),
		assignment_keys: keys.clone().into_iter().map(|key| key.into()).collect(),
		validator_groups: Default::default(),
		n_cores: 20,
		zeroth_delay_tranche_width: index as _,
		relay_vrf_modulo_samples: index as _,
		n_delay_tranches: index as _,
		no_show_slots: index as _,
		needed_approvals: index as _,
		active_validator_indices: Vec::new(),
		dispute_period: 6,
		random_seed: [0u8; 32],
	}
}

fn signature_for(
	keystore: &LocalKeystore,
	session: &SessionInfo,
	candidate_hashes: Vec<CandidateHash>,
	validator_index: ValidatorIndex,
) -> ValidatorSignature {
	let payload = ApprovalVoteMultipleCandidates(&candidate_hashes).signing_payload(1);
	let sign_key = session.validators.get(validator_index).unwrap().clone();
	let signature = keystore
		.sr25519_sign(ValidatorId::ID, &sign_key.into(), &payload[..])
		.unwrap()
		.unwrap();
	signature.into()
}

struct MockAssignmentCriteria {
	tranche:
		Result<polkadot_node_primitives::approval::v1::DelayTranche, criteria::InvalidAssignment>,
}

impl AssignmentCriteria for MockAssignmentCriteria {
	fn compute_assignments(
		&self,
		_keystore: &LocalKeystore,
		_relay_vrf_story: polkadot_node_primitives::approval::v1::RelayVRFStory,
		_config: &criteria::Config,
		_leaving_cores: Vec<(
			CandidateHash,
			polkadot_primitives::CoreIndex,
			polkadot_primitives::GroupIndex,
		)>,
	) -> HashMap<polkadot_primitives::CoreIndex, criteria::OurAssignment> {
		HashMap::new()
	}

	fn check_assignment_cert(
		&self,
		_claimed_core_bitfield: polkadot_node_primitives::approval::v2::CoreBitfield,
		_validator_index: polkadot_primitives::ValidatorIndex,
		_config: &criteria::Config,
		_relay_vrf_story: polkadot_node_primitives::approval::v1::RelayVRFStory,
		_assignment: &polkadot_node_primitives::approval::v2::AssignmentCertV2,
		_backing_groups: Vec<polkadot_primitives::GroupIndex>,
	) -> Result<polkadot_node_primitives::approval::v1::DelayTranche, criteria::InvalidAssignment>
	{
		self.tranche
	}
}

/// import an assignment
/// connect a new peer
/// the new peer sends us the same assignment
/// use `VRFModuloCompact` assignments for multiple cores
#[test]
fn try_import_the_same_assignment_v2() {
	let peers = make_peers_and_authority_ids(15);
	let peer_a = peers.get(0).unwrap().0;
	let peer_b = peers.get(1).unwrap().0;
	let peer_c = peers.get(2).unwrap().0;
	let peer_d = peers.get(4).unwrap().0;
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// setup peers
			setup_peer_with_view(overseer, &peer_a, view![], ValidationVersion::V3).await;
			setup_peer_with_view(overseer, &peer_b, view![hash], ValidationVersion::V3).await;
			setup_peer_with_view(overseer, &peer_c, view![hash], ValidationVersion::V3).await;

			// Set up a gossip topology, where a, b, c and d are topology neighbors to the node
			// under testing.
			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0, 1], &[2, 4], 3),
			)
			.await;

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 2,
				candidates: vec![Default::default(); 5],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			// send the assignment related to `hash`
			let validator_index = ValidatorIndex(0);
			let cores = vec![1, 2, 3, 4];
			let core_bitfield: CoreBitfield = cores
				.iter()
				.map(|index| CoreIndex(*index))
				.collect::<Vec<_>>()
				.try_into()
				.unwrap();

			let cert = fake_assignment_cert_v2(hash, validator_index, core_bitfield.clone());
			let assignments = vec![(cert.clone(), cores.clone().try_into().unwrap())];

			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());
			send_message_from_peer_v3(overseer, &peer_a, msg).await;

			expect_reputation_change(overseer, &peer_a, COST_UNEXPECTED_MESSAGE).await;
			provide_session(
				overseer,
				dummy_session_info_valid(1, &mut LocalKeystore::in_memory(), 1),
			)
			.await;
			// send an `Accept` message from the Approval Voting subsystem
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment,
					_,
				)) => {
					assert_eq!(assignment.candidate_indices(), &cores.try_into().unwrap());
					assert_eq!(assignment.assignment(), &cert.into());
					assert_eq!(assignment.tranche(), 0);
				}
			);

			expect_reputation_change(overseer, &peer_a, BENEFIT_VALID_MESSAGE_FIRST).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert_eq!(peers.len(), 2);
					assert_eq!(assignments.len(), 1);
				}
			);

			// setup new peer
			setup_peer_with_view(overseer, &peer_d, view![], ValidationVersion::V3).await;

			// send the same assignment from peer_d
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments);
			send_message_from_peer_v3(overseer, &peer_d, msg).await;

			expect_reputation_change(overseer, &peer_d, COST_UNEXPECTED_MESSAGE).await;
			expect_reputation_change(overseer, &peer_d, BENEFIT_VALID_MESSAGE).await;

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none(), "no message should be sent");
			virtual_overseer
		},
	);
}

/// import an assignment
/// connect a new peer
/// state sends aggregated reputation change
#[test]
fn delay_reputation_change() {
	let peer = PeerId::random();
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state_with_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;

			// Setup peers
			setup_peer_with_view(overseer, &peer, view![], ValidationVersion::V3).await;

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 2,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			// send the assignment related to `hash`
			let validator_index = ValidatorIndex(0);
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
			let assignments = vec![(cert.clone(), 0.into())];

			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());
			send_message_from_peer_v3(overseer, &peer, msg).await;
			provide_session(
				overseer,
				dummy_session_info_valid(1, &mut LocalKeystore::in_memory(), 1),
			)
			.await;

			// send an `Accept` message from the Approval Voting subsystem
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment,
					_,
				)) => {
					assert_eq!(assignment.assignment().cert, cert.cert.into());
					assert_eq!(assignment.candidate_indices(), &vec![0u32].try_into().unwrap());
					assert_eq!(assignment.tranche(), 0);
				}
			);
			expect_reputation_changes(
				overseer,
				&peer,
				vec![COST_UNEXPECTED_MESSAGE, BENEFIT_VALID_MESSAGE_FIRST],
			)
			.await;
			assert!(overseer.recv().timeout(TIMEOUT).await.is_none(), "no message should be sent");

			virtual_overseer
		},
	);
}

/// <https://github.com/paritytech/polkadot/pull/2160#discussion_r547594835>
///
/// 1. Send a view update that removes block B from their view.
/// 2. Send a message from B that they incur `COST_UNEXPECTED_MESSAGE` for, but then they receive
///    `BENEFIT_VALID_MESSAGE`.
/// 3. Send all other messages related to B.
#[test]
fn spam_attack_results_in_negative_reputation_change() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let peer_a = PeerId::random();
	let hash_b = Hash::repeat_byte(0xBB);

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			let peer = &peer_a;
			setup_peer_with_view(overseer, peer, view![], ValidationVersion::V3).await;

			// new block `hash_b` with 20 candidates
			let candidates_count = 20;
			let meta = BlockApprovalMeta {
				hash: hash_b,
				parent_hash,
				number: 2,
				candidates: vec![Default::default(); candidates_count],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};

			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			// send 20 assignments related to `hash_b`
			// to populate our knowledge
			let assignments: Vec<_> = (0..candidates_count)
				.map(|candidate_index| {
					let validator_index = ValidatorIndex(candidate_index as u32);
					let cert = fake_assignment_cert_v2(
						hash_b,
						validator_index,
						CoreIndex(candidate_index as u32).into(),
					);
					(cert, (candidate_index as u32).into())
				})
				.collect();

			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());
			send_message_from_peer_v3(overseer, peer, msg.clone()).await;

			for i in 0..candidates_count {
				expect_reputation_change(overseer, peer, COST_UNEXPECTED_MESSAGE).await;
				if i == 0 {
					provide_session(
						overseer,
						dummy_session_info_valid(1, &mut LocalKeystore::in_memory(), 1),
					)
					.await;
				}
				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
						assignment,
						_,
					)) => {
						assert_eq!(assignment.assignment(), &assignments[i].0.clone().into());
						assert_eq!(assignment.candidate_indices(), &assignments[i].1.clone().into());
						assert_eq!(assignment.tranche(), 0);
					}
				);

				expect_reputation_change(overseer, peer, BENEFIT_VALID_MESSAGE_FIRST).await;
			}

			// send a view update that removes block B from peer's view by bumping the
			// finalized_number
			overseer_send(
				overseer,
				ApprovalDistributionMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::PeerViewChange(*peer, View::with_finalized(2)),
				),
			)
			.await;

			// send the assignments again
			send_message_from_peer_v3(overseer, peer, msg.clone()).await;

			// each of them will incur `COST_UNEXPECTED_MESSAGE`, not only the first one
			for _ in 0..candidates_count {
				expect_reputation_change(overseer, peer, COST_UNEXPECTED_MESSAGE).await;
				expect_reputation_change(overseer, peer, BENEFIT_VALID_MESSAGE).await;
			}
			virtual_overseer
		},
	);
}

/// Imagine we send a message to peer A and peer B.
/// Upon receiving them, they both will try to send the message each other.
/// This test makes sure they will not punish each other for such duplicate messages.
///
/// See <https://github.com/paritytech/polkadot/issues/2499>.
#[test]
fn peer_sending_us_the_same_we_just_sent_them_is_ok() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let peers = make_peers_and_authority_ids(8);
	let peer_a = peers.first().unwrap().0;

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			let peer = &peer_a;
			setup_peer_with_view(overseer, peer, view![], ValidationVersion::V3).await;

			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			// Setup a topology where peer_a is neighbor to current node.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0], &[2], 1),
			)
			.await;

			// new block `hash` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			// import an assignment related to `hash` locally
			let validator_index = ValidatorIndex(0);
			let candidate_index = 0u32;
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(
					cert.clone().into(),
					candidate_index.into(),
				),
			)
			.await;

			// update peer view to include the hash
			overseer_send(
				overseer,
				ApprovalDistributionMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::PeerViewChange(*peer, view![hash]),
				),
			)
			.await;

			// we should send them the assignment
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert_eq!(peers.len(), 1);
					assert_eq!(assignments.len(), 1);
				}
			);

			// but if someone else is sending it the same assignment
			// the peer could send us it as well
			let assignments = vec![(cert, candidate_index.into())];
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments);
			send_message_from_peer_v3(overseer, peer, msg.clone()).await;

			assert!(
				overseer.recv().timeout(TIMEOUT).await.is_none(),
				"we should not punish the peer"
			);

			// send the assignments again
			send_message_from_peer_v3(overseer, peer, msg).await;

			// now we should
			expect_reputation_change(overseer, peer, COST_DUPLICATE_MESSAGE).await;
			virtual_overseer
		},
	);
}

#[test]
fn peer_sending_us_duplicates_while_aggression_enabled_is_ok() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let peers = make_peers_and_authority_ids(8);
	let peer_a = peers.first().unwrap().0;

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			let peer = &peer_a;
			setup_peer_with_view(overseer, peer, view![], ValidationVersion::V3).await;

			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			// Setup a topology where peer_a is neighbor to current node.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0], &[2], 1),
			)
			.await;

			// new block `hash` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			// import an assignment related to `hash` locally
			let validator_index = ValidatorIndex(0);
			let candidate_indices: CandidateBitfield =
				vec![0 as CandidateIndex].try_into().unwrap();
			let candidate_bitfields = vec![CoreIndex(0)].try_into().unwrap();
			let cert = fake_assignment_cert_v2(hash, validator_index, candidate_bitfields);
			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(
					cert.clone().into(),
					candidate_indices.clone(),
				),
			)
			.await;

			// update peer view to include the hash
			overseer_send(
				overseer,
				ApprovalDistributionMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::PeerViewChange(*peer, view![hash]),
				),
			)
			.await;

			// we should send them the assignment
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert_eq!(peers.len(), 1);
					assert_eq!(assignments.len(), 1);
				}
			);

			// but if someone else is sending it the same assignment
			// the peer could send us it as well
			let assignments = vec![(cert, candidate_indices)];
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments);
			send_message_from_peer_v3(overseer, peer, msg.clone()).await;

			assert!(
				overseer.recv().timeout(TIMEOUT).await.is_none(),
				"we should not punish the peer"
			);

			// send the assignments again
			send_message_from_peer_v3(overseer, peer, msg.clone()).await;

			// now we should
			expect_reputation_change(overseer, peer, COST_DUPLICATE_MESSAGE).await;

			// Peers will be continously punished for sending duplicates until approval-distribution
			// aggression kicks, at which point they aren't anymore.
			let mut parent_hash = hash;
			for level in 0..16 {
				// As long as the lag is bellow l1 aggression, punish peers for duplicates.
				send_message_from_peer_v3(overseer, peer, msg.clone()).await;
				expect_reputation_change(overseer, peer, COST_DUPLICATE_MESSAGE).await;

				let number = 1 + level + 1; // first block had number 1
				let hash = BlakeTwo256::hash_of(&(parent_hash, number));
				let meta = BlockApprovalMeta {
					hash,
					parent_hash,
					number,
					candidates: vec![],
					slot: (level as u64).into(),
					session: 1,
					vrf_story: RelayVRFStory(Default::default()),
				};

				let msg = ApprovalDistributionMessage::ApprovalCheckingLagUpdate(level + 1);
				overseer_send(overseer, msg).await;

				let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
				overseer_send(overseer, msg).await;

				parent_hash = hash;
			}

			// send the assignments again, we should not punish the peer because aggression is
			// enabled.
			send_message_from_peer_v3(overseer, peer, msg).await;

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none(), "no message should be sent");
			virtual_overseer
		},
	);
}

// Test a v2 approval that signs multiple candidate is correctly processed.
#[test]
fn import_approval_happy_path_v2() {
	let peers = make_peers_and_authority_ids(15);

	let peer_a = peers.get(0).unwrap().0;
	let peer_b = peers.get(1).unwrap().0;
	let peer_c = peers.get(2).unwrap().0;
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);
	let candidate_hash_first = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xBB));
	let candidate_hash_second = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xCC));
	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// setup peers with  V3 protocol versions
			setup_peer_with_view(overseer, &peer_a, view![], ValidationVersion::V3).await;
			setup_peer_with_view(overseer, &peer_b, view![hash], ValidationVersion::V3).await;
			setup_peer_with_view(overseer, &peer_c, view![hash], ValidationVersion::V3).await;
			let mut keystore = LocalKeystore::in_memory();
			let session = dummy_session_info_valid(1, &mut keystore, 1);

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![
					(candidate_hash_first, 0.into(), 0.into()),
					(candidate_hash_second, 1.into(), 1.into()),
				],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			// Set up a gossip topology, where a, b, and c are topology neighbors to the node.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0, 1], &[2, 4], 3),
			)
			.await;

			// import an assignment related to `hash` locally
			let validator_index = ValidatorIndex(0);
			let candidate_indices: CandidateBitfield =
				vec![0 as CandidateIndex, 1 as CandidateIndex].try_into().unwrap();
			let candidate_bitfields = vec![CoreIndex(0), CoreIndex(1)].try_into().unwrap();
			let cert = fake_assignment_cert_v2(hash, validator_index, candidate_bitfields);
			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(
					cert.clone().into(),
					candidate_indices.clone(),
				),
			)
			.await;

			// 1 peer is v2
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert_eq!(peers.len(), 2);
					assert_eq!(assignments.len(), 1);
				}
			);

			// send an approval from peer_b
			let approval = IndirectSignedApprovalVoteV2 {
				block_hash: hash,
				candidate_indices,
				validator: validator_index,
				signature: signature_for(
					&keystore,
					&session,
					vec![candidate_hash_first, candidate_hash_second],
					validator_index,
				),
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Approvals(vec![approval.clone()]);
			send_message_from_peer_v3(overseer, &peer_b, msg).await;
			provide_session(
				overseer,
				dummy_session_info_valid(1, &mut LocalKeystore::in_memory(), 1),
			)
			.await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportApproval(
					vote, _,
				)) => {
					assert_eq!(Into::<IndirectSignedApprovalVoteV2>::into(vote), approval);
				}
			);

			expect_reputation_change(overseer, &peer_b, BENEFIT_VALID_MESSAGE_FIRST).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Approvals(approvals)
					))
				)) => {
					assert_eq!(peers.len(), 1);
					assert_eq!(approvals.len(), 1);
				}
			);
			virtual_overseer
		},
	);
}

// Tests that votes that cover multiple assignments candidates are correctly processed on importing
#[test]
fn multiple_assignments_covered_with_one_approval_vote() {
	let peers = make_peers_and_authority_ids(15);

	let peer_a = peers.get(0).unwrap().0;
	let peer_b = peers.get(1).unwrap().0;
	let peer_c = peers.get(2).unwrap().0;
	let peer_d = peers.get(4).unwrap().0;
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);
	let candidate_hash_first = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xBB));
	let candidate_hash_second = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xCC));

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// setup peers with  V3 protocol versions
			setup_peer_with_view(overseer, &peer_a, view![hash], ValidationVersion::V3).await;
			setup_peer_with_view(overseer, &peer_b, view![hash], ValidationVersion::V3).await;
			setup_peer_with_view(overseer, &peer_c, view![hash], ValidationVersion::V3).await;
			setup_peer_with_view(overseer, &peer_d, view![hash], ValidationVersion::V3).await;

			let mut keystore = LocalKeystore::in_memory();
			let session = dummy_session_info_valid(1, &mut keystore, 5);
			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![
					(candidate_hash_first, 0.into(), 0.into()),
					(candidate_hash_second, 1.into(), 1.into()),
				],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			// Set up a gossip topology, where a, b, and c, d are topology neighbors to the node.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0, 1], &[2, 4], 3),
			)
			.await;

			// import an assignment related to `hash` locally
			let validator_index = ValidatorIndex(2); // peer_c is the originator
			let candidate_indices: CandidateBitfield =
				vec![0 as CandidateIndex, 1 as CandidateIndex].try_into().unwrap();

			let core_bitfields = vec![CoreIndex(0)].try_into().unwrap();
			let cert = fake_assignment_cert_v2(hash, validator_index, core_bitfields);

			// send the candidate 0 assignment from peer_b
			let assignment = IndirectAssignmentCertV2 {
				block_hash: hash,
				validator: validator_index,
				cert: cert.cert,
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(vec![(
				assignment,
				(0 as CandidateIndex).into(),
			)]);
			send_message_from_peer_v3(overseer, &peer_d, msg).await;
			provide_session(overseer, session.clone()).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment,
					_,
				)) => {
					assert_eq!(assignment.tranche(), 0);
				}
			);
			expect_reputation_change(overseer, &peer_d, BENEFIT_VALID_MESSAGE_FIRST).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert!(peers.len() >= 2);
					assert!(peers.contains(&peer_a));
					assert!(peers.contains(&peer_b));
					assert_eq!(assignments.len(), 1);
				}
			);

			let candidate_bitfields = vec![CoreIndex(1)].try_into().unwrap();
			let cert = fake_assignment_cert_v2(hash, validator_index, candidate_bitfields);

			// send the candidate 1 assignment from peer_c
			let assignment = IndirectAssignmentCertV2 {
				block_hash: hash,
				validator: validator_index,
				cert: cert.cert,
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(vec![(
				assignment,
				(1 as CandidateIndex).into(),
			)]);

			send_message_from_peer_v3(overseer, &peer_c, msg).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment, _,
				)) => {
					assert_eq!(assignment.tranche(), 0);
				}
			);
			expect_reputation_change(overseer, &peer_c, BENEFIT_VALID_MESSAGE_FIRST).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert!(peers.len() >= 2);
					assert!(peers.contains(&peer_b));
					assert!(peers.contains(&peer_a));
					assert_eq!(assignments.len(), 1);
				}
			);

			// send an approval from peer_b
			let approval = IndirectSignedApprovalVoteV2 {
				block_hash: hash,
				candidate_indices,
				validator: validator_index,
				signature: signature_for(
					&keystore,
					&session,
					vec![candidate_hash_first, candidate_hash_second],
					validator_index,
				),
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Approvals(vec![approval.clone()]);
			send_message_from_peer_v3(overseer, &peer_d, msg).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportApproval(
					vote, _,
				)) => {
					assert_eq!(Into::<IndirectSignedApprovalVoteV2>::into(vote), approval);
				}
			);

			expect_reputation_change(overseer, &peer_d, BENEFIT_VALID_MESSAGE_FIRST).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Approvals(approvals)
					))
				)) => {
					assert!(peers.len() >= 2);
					assert!(peers.contains(&peer_b));
					assert!(peers.contains(&peer_a));
					assert_eq!(approvals.len(), 1);
				}
			);
			for candidate_index in 0..1 {
				let (tx_distribution, rx_distribution) = oneshot::channel();
				let mut candidates_requesting_signatures = HashSet::new();
				candidates_requesting_signatures.insert((hash, candidate_index));
				overseer_send(
					overseer,
					ApprovalDistributionMessage::GetApprovalSignatures(
						candidates_requesting_signatures,
						tx_distribution,
					),
				)
				.await;
				let signatures = rx_distribution.await.unwrap();

				assert_eq!(signatures.len(), 1);
				for (signing_validator, signature) in signatures {
					assert_eq!(validator_index, signing_validator);
					assert_eq!(signature.0, hash);
					assert_eq!(signature.2, approval.signature);
					assert_eq!(signature.1, vec![0, 1]);
				}
			}
			virtual_overseer
		},
	);
}

// Tests that an approval coalescing more candidates than the runtime's
// `max_approval_coalesce_count` is rejected and costs the sending peer reputation.
#[test]
fn coalesced_approval_above_coalesce_limit_is_rejected_early() {
	let peers = make_peers_and_authority_ids(15);

	let peer_c = peers.get(2).unwrap().0;
	let peer_d = peers.get(4).unwrap().0;
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);
	let candidate_hash_first = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xBB));
	let candidate_hash_second = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xCC));

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			for (peer, _) in &peers {
				setup_peer_with_view(overseer, peer, view![hash], ValidationVersion::V3).await;
			}

			let mut keystore = LocalKeystore::in_memory();
			let session = dummy_session_info_valid(1, &mut keystore, 5);
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![
					(candidate_hash_first, 0.into(), 0.into()),
					(candidate_hash_second, 1.into(), 1.into()),
				],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0, 1], &[2, 4], 3),
			)
			.await;

			let validator_index = ValidatorIndex(2);
			let candidate_indices: CandidateBitfield =
				vec![0 as CandidateIndex, 1 as CandidateIndex].try_into().unwrap();

			// Import the assignment for candidate 0. The runtime advertises a coalescing limit of
			// 1, i.e. coalescing is disabled.
			let cert = fake_assignment_cert_v2(
				hash,
				validator_index,
				vec![CoreIndex(0)].try_into().unwrap(),
			);
			let assignment = IndirectAssignmentCertV2 {
				block_hash: hash,
				validator: validator_index,
				cert: cert.cert,
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(vec![(
				assignment,
				(0 as CandidateIndex).into(),
			)]);
			send_message_from_peer_v3(overseer, &peer_d, msg).await;
			provide_session_with_coalesce_count(overseer, session.clone(), 1).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment,
					_,
				)) => {
					assert_eq!(assignment.tranche(), 0);
				}
			);
			expect_reputation_change(overseer, &peer_d, BENEFIT_VALID_MESSAGE_FIRST).await;
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					_,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert_eq!(assignments.len(), 1);
				}
			);

			// Import the assignment for candidate 1.
			let cert = fake_assignment_cert_v2(
				hash,
				validator_index,
				vec![CoreIndex(1)].try_into().unwrap(),
			);
			let assignment = IndirectAssignmentCertV2 {
				block_hash: hash,
				validator: validator_index,
				cert: cert.cert,
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(vec![(
				assignment,
				(1 as CandidateIndex).into(),
			)]);
			send_message_from_peer_v3(overseer, &peer_c, msg).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment,
					_,
				)) => {
					assert_eq!(assignment.tranche(), 0);
				}
			);
			expect_reputation_change(overseer, &peer_c, BENEFIT_VALID_MESSAGE_FIRST).await;
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					_,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert_eq!(assignments.len(), 1);
				}
			);

			// Send an approval coalescing both candidates. With a coalescing limit of 1 this
			// exceeds the allowed count and must be rejected without being imported or circulated.
			let approval = IndirectSignedApprovalVoteV2 {
				block_hash: hash,
				candidate_indices,
				validator: validator_index,
				signature: signature_for(
					&keystore,
					&session,
					vec![candidate_hash_first, candidate_hash_second],
					validator_index,
				),
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Approvals(vec![approval]);
			send_message_from_peer_v3(overseer, &peer_d, msg).await;

			expect_reputation_change(overseer, &peer_d, COST_INVALID_MESSAGE).await;
			virtual_overseer
		},
	);
}

// Tests that votes that cover multiple assignments candidates are correctly processed when unify
// with peer view
#[test]
fn unify_with_peer_multiple_assignments_covered_with_one_approval_vote() {
	let peers = make_peers_and_authority_ids(15);

	let peer_a = peers.get(0).unwrap().0;
	let peer_b = peers.get(1).unwrap().0;
	let peer_d = peers.get(4).unwrap().0;
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);
	let candidate_hash_first = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xBB));
	let candidate_hash_second = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xCC));

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			setup_peer_with_view(overseer, &peer_d, view![hash], ValidationVersion::V3).await;
			let mut keystore = LocalKeystore::in_memory();
			let session = dummy_session_info_valid(1, &mut keystore, 5);
			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![
					(candidate_hash_first, 0.into(), 0.into()),
					(candidate_hash_second, 1.into(), 1.into()),
				],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			// Set up a gossip topology, where a, b, and c, d are topology neighbors to the node.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0, 1], &[2, 4], 3),
			)
			.await;

			// import an assignment related to `hash` locally
			let validator_index = ValidatorIndex(2); // peer_c is the originator
			let candidate_indices: CandidateBitfield =
				vec![0 as CandidateIndex, 1 as CandidateIndex].try_into().unwrap();

			let core_bitfields = vec![CoreIndex(0)].try_into().unwrap();
			let cert = fake_assignment_cert_v2(hash, validator_index, core_bitfields);

			// send the candidate 0 assignment from peer_b
			let assignment = IndirectAssignmentCertV2 {
				block_hash: hash,
				validator: validator_index,
				cert: cert.cert,
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(vec![(
				assignment,
				(0 as CandidateIndex).into(),
			)]);
			send_message_from_peer_v3(overseer, &peer_d, msg).await;
			provide_session(overseer, session.clone()).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment, _,
				)) => {
					assert_eq!(assignment.tranche(), 0);
				}
			);
			expect_reputation_change(overseer, &peer_d, BENEFIT_VALID_MESSAGE_FIRST).await;

			let candidate_bitfields = vec![CoreIndex(1)].try_into().unwrap();
			let cert = fake_assignment_cert_v2(hash, validator_index, candidate_bitfields);

			// send the candidate 1 assignment from peer_c
			let assignment = IndirectAssignmentCertV2 {
				block_hash: hash,
				validator: validator_index,
				cert: cert.cert,
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(vec![(
				assignment,
				(1 as CandidateIndex).into(),
			)]);

			send_message_from_peer_v3(overseer, &peer_d, msg).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment, _,
				)) => {
					assert_eq!(assignment.tranche(), 0);
				}
			);
			expect_reputation_change(overseer, &peer_d, BENEFIT_VALID_MESSAGE_FIRST).await;

			// send an approval from peer_b
			let approval = IndirectSignedApprovalVoteV2 {
				block_hash: hash,
				candidate_indices,
				validator: validator_index,
				signature: signature_for(
					&keystore,
					&session,
					vec![candidate_hash_first, candidate_hash_second],
					validator_index,
				),
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Approvals(vec![approval.clone()]);
			send_message_from_peer_v3(overseer, &peer_d, msg).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportApproval(
					vote, _,
				)) => {
					assert_eq!(Into::<IndirectSignedApprovalVoteV2>::into(vote), approval);
				}
			);

			expect_reputation_change(overseer, &peer_d, BENEFIT_VALID_MESSAGE_FIRST).await;

			// setup peers with  V3 protocol versions
			setup_peer_with_view(overseer, &peer_a, view![hash], ValidationVersion::V3).await;
			setup_peer_with_view(overseer, &peer_b, view![hash], ValidationVersion::V3).await;
			let mut expected_peers_assignments = vec![peer_a, peer_b];
			let mut expected_peers_approvals = vec![peer_a, peer_b];
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert!(peers.len() == 1);
					assert!(expected_peers_assignments.contains(peers.first().unwrap()));
					expected_peers_assignments.retain(|peer| peer != peers.first().unwrap());
					assert_eq!(assignments.len(), 2);
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Approvals(approvals)
					))
				)) => {
					assert!(peers.len() == 1);
					assert!(expected_peers_approvals.contains(peers.first().unwrap()));
					expected_peers_approvals.retain(|peer| peer != peers.first().unwrap());
					assert_eq!(approvals.len(), 1);
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert!(peers.len() == 1);
					assert!(expected_peers_assignments.contains(peers.first().unwrap()));
					expected_peers_assignments.retain(|peer| peer != peers.first().unwrap());
					assert_eq!(assignments.len(), 2);
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Approvals(approvals)
					))
				)) => {
					assert!(peers.len() == 1);
					assert!(expected_peers_approvals.contains(peers.first().unwrap()));
					expected_peers_approvals.retain(|peer| peer != peers.first().unwrap());
					assert_eq!(approvals.len(), 1);
				}
			);

			virtual_overseer
		},
	);
}

#[test]
fn import_approval_bad() {
	let peer_a = PeerId::random();
	let peer_b = PeerId::random();
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);
	let candidate_hash = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xBB));

	let diff_candidate_hash = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xCC));

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// setup peers
			setup_peer_with_view(overseer, &peer_a, view![], ValidationVersion::V3).await;
			setup_peer_with_view(overseer, &peer_b, view![hash], ValidationVersion::V3).await;
			let mut keystore = LocalKeystore::in_memory();
			let session = dummy_session_info_valid(1, &mut keystore, 1);
			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![(candidate_hash, 0.into(), 0.into()); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			let validator_index = ValidatorIndex(0);
			let candidate_index = 0u32;
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());

			// Sign a different candidate hash.
			let payload =
				ApprovalVoteMultipleCandidates(&vec![diff_candidate_hash]).signing_payload(1);
			let sign_key = session.validators.get(ValidatorIndex(0)).unwrap().clone();
			let signature = keystore
				.sr25519_sign(ValidatorId::ID, &sign_key.into(), &payload[..])
				.unwrap()
				.unwrap();

			// send an approval from peer_b, we don't have an assignment yet
			let approval = IndirectSignedApprovalVoteV2 {
				block_hash: hash,
				candidate_indices: candidate_index.into(),
				validator: validator_index,
				signature: signature.into(),
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Approvals(vec![approval.clone()]);
			send_message_from_peer_v3(overseer, &peer_b, msg).await;

			expect_reputation_change(overseer, &peer_b, COST_UNEXPECTED_MESSAGE).await;

			// now import an assignment from peer_b
			let assignments = vec![(cert.clone(), candidate_index.into())];
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments);
			send_message_from_peer_v3(overseer, &peer_b, msg).await;
			provide_session(overseer, session.clone()).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment,
					_,
				)) => {
					assert_eq!(assignment.assignment(), &cert.into());
					assert_eq!(assignment.candidate_indices(), &candidate_index.into());
					assert_eq!(assignment.tranche(), 0);
				}
			);

			expect_reputation_change(overseer, &peer_b, BENEFIT_VALID_MESSAGE_FIRST).await;

			// and try again
			let msg = protocol_v3::ApprovalDistributionMessage::Approvals(vec![approval.clone()]);
			send_message_from_peer_v3(overseer, &peer_b, msg).await;

			expect_reputation_change(overseer, &peer_b, COST_INVALID_MESSAGE).await;
			virtual_overseer
		},
	);
}

/// make sure we clean up the state on block finalized
#[test]
fn update_our_view() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash_a = Hash::repeat_byte(0xAA);
	let hash_b = Hash::repeat_byte(0xBB);
	let hash_c = Hash::repeat_byte(0xCC);

	let state = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		State::default(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// new block `hash_a` with 1 candidates
			let meta_a = BlockApprovalMeta {
				hash: hash_a,
				parent_hash,
				number: 1,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let meta_b = BlockApprovalMeta {
				hash: hash_b,
				parent_hash: hash_a,
				number: 2,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let meta_c = BlockApprovalMeta {
				hash: hash_c,
				parent_hash: hash_b,
				number: 3,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};

			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta_a, meta_b, meta_c]);
			overseer_send(overseer, msg).await;
			virtual_overseer
		},
	);

	assert!(state.blocks_by_number.get(&1).is_some());
	assert!(state.blocks_by_number.get(&2).is_some());
	assert!(state.blocks_by_number.get(&3).is_some());
	assert!(state.blocks.get(&hash_a).is_some());
	assert!(state.blocks.get(&hash_b).is_some());
	assert!(state.blocks.get(&hash_c).is_some());

	let state = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state,
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// finalize a block
			overseer_signal_block_finalized(overseer, 2).await;
			virtual_overseer
		},
	);

	assert!(state.blocks_by_number.get(&1).is_none());
	assert!(state.blocks_by_number.get(&2).is_none());
	assert!(state.blocks_by_number.get(&3).is_some());
	assert!(state.blocks.get(&hash_a).is_none());
	assert!(state.blocks.get(&hash_b).is_none());
	assert!(state.blocks.get(&hash_c).is_some());

	let state = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state,
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// finalize a very high block
			overseer_signal_block_finalized(overseer, 4_000_000_000).await;
			virtual_overseer
		},
	);

	assert!(state.blocks_by_number.get(&3).is_none());
	assert!(state.blocks.get(&hash_c).is_none());
}

/// make sure we unify with peers and clean up the state
#[test]
fn update_peer_view() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash_a = Hash::repeat_byte(0xAA);
	let hash_b = Hash::repeat_byte(0xBB);
	let hash_c = Hash::repeat_byte(0xCC);
	let hash_d = Hash::repeat_byte(0xDD);
	let peers = make_peers_and_authority_ids(8);
	let peer_a = peers.first().unwrap().0;
	let peer = &peer_a;

	let state = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		State::default(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// new block `hash_a` with 1 candidates
			let meta_a = BlockApprovalMeta {
				hash: hash_a,
				parent_hash,
				number: 1,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let meta_b = BlockApprovalMeta {
				hash: hash_b,
				parent_hash: hash_a,
				number: 2,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let meta_c = BlockApprovalMeta {
				hash: hash_c,
				parent_hash: hash_b,
				number: 3,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};

			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta_a, meta_b, meta_c]);
			overseer_send(overseer, msg).await;

			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			// Setup a topology where peer_a is neighbor to current node.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0], &[2], 1),
			)
			.await;

			let cert_a = fake_assignment_cert_v2(hash_a, ValidatorIndex(0), CoreIndex(0).into());
			let cert_b = fake_assignment_cert_v2(hash_b, ValidatorIndex(0), CoreIndex(0).into());

			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(cert_a.into(), 0.into()),
			)
			.await;

			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(cert_b.into(), 0.into()),
			)
			.await;

			// connect a peer
			setup_peer_with_view(overseer, peer, view![hash_a], ValidationVersion::V3).await;

			// we should send relevant assignments to the peer
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert_eq!(peers.len(), 1);
					assert_eq!(assignments.len(), 1);
				}
			);
			virtual_overseer
		},
	);

	assert_eq!(state.peer_views.get(peer).map(|v| v.view.finalized_number), Some(0));
	assert_eq!(
		state
			.blocks
			.get(&hash_a)
			.unwrap()
			.known_by
			.get(peer)
			.unwrap()
			.sent
			.known_messages
			.len(),
		1,
	);

	let state = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state,
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// update peer's view
			overseer_send(
				overseer,
				ApprovalDistributionMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::PeerViewChange(
						*peer,
						View::new(vec![hash_b, hash_c, hash_d], 2),
					),
				),
			)
			.await;

			let cert_c = fake_assignment_cert_v2(hash_c, ValidatorIndex(0), CoreIndex(0).into());

			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(cert_c.clone().into(), 0.into()),
			)
			.await;

			// we should send relevant assignments to the peer
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert_eq!(peers.len(), 1);
					assert_eq!(assignments.len(), 1);
					assert_eq!(assignments[0].0, cert_c);
				}
			);
			virtual_overseer
		},
	);

	assert_eq!(state.peer_views.get(peer).map(|v| v.view.finalized_number), Some(2));
	assert_eq!(
		state
			.blocks
			.get(&hash_c)
			.unwrap()
			.known_by
			.get(peer)
			.unwrap()
			.sent
			.known_messages
			.len(),
		1,
	);

	let finalized_number = 4_000_000_000;
	let state = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state,
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// update peer's view
			overseer_send(
				overseer,
				ApprovalDistributionMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::PeerViewChange(
						*peer,
						View::with_finalized(finalized_number),
					),
				),
			)
			.await;
			virtual_overseer
		},
	);

	assert_eq!(state.peer_views.get(peer).map(|v| v.view.finalized_number), Some(finalized_number));
	assert!(state.blocks.get(&hash_c).unwrap().known_by.get(peer).is_none());
}

// Tests that updating the known peer_id for a given authority updates the topology
// and sends the required messages
#[test]
fn update_peer_authority_id() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash_a = Hash::repeat_byte(0xAA);
	let hash_b = Hash::repeat_byte(0xBB);
	let hash_c = Hash::repeat_byte(0xCC);
	let peers = make_peers_and_authority_ids(8);
	let neighbour_x_index = 0;
	let neighbour_y_index = 2;
	let local_index = 1;
	// X neighbour, we simulate that PeerId is not known in the beginning.
	let neighbour_x = peers.get(neighbour_x_index).unwrap().0;
	// Y neighbour, we simulate that PeerId is not known in the beginning.
	let neighbour_y = peers.get(neighbour_y_index).unwrap().0;

	let _state = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		State::default(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// new block `hash_a` with 1 candidates
			let meta_a = BlockApprovalMeta {
				hash: hash_a,
				parent_hash,
				number: 1,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let meta_b = BlockApprovalMeta {
				hash: hash_b,
				parent_hash: hash_a,
				number: 2,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let meta_c = BlockApprovalMeta {
				hash: hash_c,
				parent_hash: hash_b,
				number: 3,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};

			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta_a, meta_b, meta_c]);
			overseer_send(overseer, msg).await;

			let peers_with_optional_peer_id = peers
				.iter()
				.enumerate()
				.map(|(index, (peer_id, authority))| {
					(if index == 0 { None } else { Some(*peer_id) }, authority.clone())
				})
				.collect_vec();

			// Setup a topology where peer_a is neighbor to current node.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(
					1,
					&peers_with_optional_peer_id,
					&[neighbour_x_index],
					&[neighbour_y_index],
					local_index,
				),
			)
			.await;

			let cert_a = fake_assignment_cert_v2(
				hash_a,
				ValidatorIndex(local_index as u32),
				CoreIndex(local_index as u32).into(),
			);
			let cert_b = fake_assignment_cert_v2(
				hash_b,
				ValidatorIndex(local_index as u32),
				CoreIndex(local_index as u32).into(),
			);

			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(cert_a.into(), 0.into()),
			)
			.await;

			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(cert_b.into(), 0.into()),
			)
			.await;

			// connect a peer
			setup_peer_with_view(overseer, &neighbour_x, view![hash_a], ValidationVersion::V3)
				.await;
			setup_peer_with_view(overseer, &neighbour_y, view![hash_a], ValidationVersion::V3)
				.await;

			setup_peer_with_view(overseer, &neighbour_x, view![hash_b], ValidationVersion::V3)
				.await;
			setup_peer_with_view(overseer, &neighbour_y, view![hash_b], ValidationVersion::V3)
				.await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert_eq!(peers.len(), 1);
					assert_eq!(assignments.len(), 1);
					assert_eq!(peers.get(0), Some(&neighbour_y));
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert_eq!(peers.len(), 1);
					assert_eq!(assignments.len(), 1);
					assert_eq!(peers.get(0), Some(&neighbour_y));
				}
			);

			overseer_send(
				overseer,
				ApprovalDistributionMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::UpdatedAuthorityIds(
						peers[neighbour_x_index].0,
						[peers[neighbour_x_index].1.clone()].into_iter().collect(),
					),
				),
			)
			.await;

			// we should send relevant assignments to the peer, after we found it's peer id.
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					gum::info!(target: LOG_TARGET, ?peers, ?assignments);
					assert_eq!(peers.len(), 1);
					assert_eq!(assignments.len(), 2);
					assert_eq!(assignments.get(0).unwrap().0.block_hash, hash_a);
					assert_eq!(assignments.get(1).unwrap().0.block_hash, hash_b);
					assert_eq!(peers.get(0), Some(&neighbour_x));
				}
			);

			overseer_send(
				overseer,
				ApprovalDistributionMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::UpdatedAuthorityIds(
						peers[neighbour_y_index].0,
						[peers[neighbour_y_index].1.clone()].into_iter().collect(),
					),
				),
			)
			.await;
			overseer_send(
				overseer,
				ApprovalDistributionMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::UpdatedAuthorityIds(
						peers[neighbour_x_index].0,
						[peers[neighbour_x_index].1.clone()].into_iter().collect(),
					),
				),
			)
			.await;
			assert!(
				overseer.recv().timeout(TIMEOUT).await.is_none(),
				"no message should be sent peers are already known"
			);

			virtual_overseer
		},
	);
}

/// E.g. if someone copies the keys...
#[test]
fn import_remotely_then_locally() {
	let peer_a = PeerId::random();
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);
	let candidate_hash = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xBB));
	let peer = &peer_a;

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// setup the peer
			setup_peer_with_view(overseer, peer, view![hash], ValidationVersion::V3).await;
			let mut keystore = LocalKeystore::in_memory();

			let session = dummy_session_info_valid(1, &mut keystore, 1);
			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![(candidate_hash, 0.into(), 0.into()); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};

			let payload = ApprovalVoteMultipleCandidates(&vec![candidate_hash]).signing_payload(1);
			let sign_key = session.validators.get(ValidatorIndex(0)).unwrap().clone();
			let signature = keystore
				.sr25519_sign(ValidatorId::ID, &sign_key.into(), &payload[..])
				.unwrap()
				.unwrap();

			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			// import the assignment remotely first
			let validator_index = ValidatorIndex(0);
			let candidate_index = 0u32;
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
			let assignments = vec![(cert.clone(), candidate_index.into())];
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());
			send_message_from_peer_v3(overseer, peer, msg).await;
			provide_session(overseer, session.clone()).await;

			// send an `Accept` message from the Approval Voting subsystem
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment,
					 _,
				)) => {
					assert_eq!(assignment.assignment(), &cert.clone().into());
					assert_eq!(assignment.candidate_indices(), &candidate_index.into());
					assert_eq!(assignment.tranche(), 0);
				}
			);

			expect_reputation_change(overseer, peer, BENEFIT_VALID_MESSAGE_FIRST).await;

			// import the same assignment locally
			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(
					cert.clone().into(),
					candidate_index.into(),
				),
			)
			.await;

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none(), "no message should be sent");

			// send the approval remotely
			let approval = IndirectSignedApprovalVoteV2 {
				block_hash: hash,
				candidate_indices: candidate_index.into(),
				validator: validator_index,
				signature: signature.into(),
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Approvals(vec![approval.clone()]);
			send_message_from_peer_v3(overseer, peer, msg).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportApproval(
					vote, _,
				)) => {
					assert_eq!(Into::<IndirectSignedApprovalVoteV2>::into(vote), approval);
				}
			);
			expect_reputation_change(overseer, peer, BENEFIT_VALID_MESSAGE_FIRST).await;

			// import the same approval locally
			overseer_send(overseer, ApprovalDistributionMessage::DistributeApproval(approval))
				.await;

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none(), "no message should be sent");
			virtual_overseer
		},
	);
}

/// With `VRFModuloCompact` assignments.
#[test]
fn sends_assignments_even_when_state_is_approved_v2() {
	let peers = make_peers_and_authority_ids(8);
	let peer_a = peers.first().unwrap().0;
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);
	let peer = &peer_a;

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		State::default(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![Default::default(); 4],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			// Setup a topology where peer_a is neighbor to current node.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0], &[2], 1),
			)
			.await;

			let validator_index = ValidatorIndex(0);
			let cores = vec![0, 1, 2, 3];
			let candidate_bitfield: CandidateBitfield = cores.clone().try_into().unwrap();

			let core_bitfield: CoreBitfield = cores
				.iter()
				.map(|index| CoreIndex(*index))
				.collect::<Vec<_>>()
				.try_into()
				.unwrap();

			let cert = fake_assignment_cert_v2(hash, validator_index, core_bitfield.clone());

			// Assumes candidate index == core index.
			let approvals = cores
				.iter()
				.map(|core| IndirectSignedApprovalVoteV2 {
					block_hash: hash,
					candidate_indices: (*core).into(),
					validator: validator_index,
					signature: dummy_signature(),
				})
				.collect::<Vec<_>>();

			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(
					cert.clone().into(),
					candidate_bitfield.clone(),
				),
			)
			.await;

			for approval in &approvals {
				overseer_send(
					overseer,
					ApprovalDistributionMessage::DistributeApproval(approval.clone()),
				)
				.await;
			}

			// connect the peer.
			setup_peer_with_view(overseer, peer, view![hash], ValidationVersion::V3).await;

			let assignments = vec![(cert.clone(), candidate_bitfield.clone())];

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(sent_assignments)
					))
				)) => {
					assert_eq!(peers, vec![*peer]);
					assert_eq!(sent_assignments, assignments);
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Approvals(sent_approvals)
					))
				)) => {
					// Construct a hashmaps of approvals for comparison. Approval distribution reorders messages because they are kept in a
					// hashmap as well.
					let sent_approvals = sent_approvals.into_iter().map(|approval| (approval.candidate_indices.clone(), approval)).collect::<HashMap<_,_>>();
					let approvals = approvals.into_iter().map(|approval| (approval.candidate_indices.clone(), approval)).collect::<HashMap<_,_>>();

					assert_eq!(peers, vec![*peer]);
					assert_eq!(sent_approvals, approvals);
				}
			);

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none(), "no message should be sent");
			virtual_overseer
		},
	);
}

/// <https://github.com/paritytech/polkadot/pull/5089>
///
/// 1. Receive remote peer view update with an unknown head
/// 2. Receive assignments for that unknown head
/// 3. Update our view and import the new block
/// 4. Expect that no reputation with `COST_UNEXPECTED_MESSAGE` is applied
#[test]
fn race_condition_in_local_vs_remote_view_update() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let peer_a = PeerId::random();
	let hash_b = Hash::repeat_byte(0xBB);

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			let peer = &peer_a;

			// Test a small number of candidates
			let candidates_count = 1;
			let meta = BlockApprovalMeta {
				hash: hash_b,
				parent_hash,
				number: 2,
				candidates: vec![Default::default(); candidates_count],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};

			// This will send a peer view that is ahead of our view
			setup_peer_with_view(overseer, peer, view![hash_b], ValidationVersion::V3).await;

			// Send our view update to include a new head
			overseer_send(
				overseer,
				ApprovalDistributionMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::OurViewChange(our_view![hash_b]),
				),
			)
			.await;

			// send assignments related to `hash_b` but they will come to the MessagesPending
			let assignments: Vec<_> = (0..candidates_count)
				.map(|candidate_index| {
					let validator_index = ValidatorIndex(candidate_index as u32);
					let cert = fake_assignment_cert_v2(
						hash_b,
						validator_index,
						CoreIndex(candidate_index as u32).into(),
					);
					(cert, (candidate_index as u32).into())
				})
				.collect();

			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());
			send_message_from_peer_v3(overseer, peer, msg.clone()).await;

			// This will handle pending messages being processed
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			provide_session(
				overseer,
				dummy_session_info_valid(1, &mut LocalKeystore::in_memory(), 1),
			)
			.await;

			for i in 0..candidates_count {
				// Previously, this has caused out-of-view assignments/approvals
				// expect_reputation_change(overseer, peer, COST_UNEXPECTED_MESSAGE).await;

				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
						assignment,
						_,
					)) => {
						assert_eq!(assignment.assignment(), &assignments[i].0.clone().into());
						assert_eq!(assignment.candidate_indices(), &assignments[i].1.clone().into());
						assert_eq!(assignment.tranche(), 0);
					}
				);

				// Since we have a valid statement pending, this should always occur
				expect_reputation_change(overseer, peer, BENEFIT_VALID_MESSAGE_FIRST).await;
			}
			virtual_overseer
		},
	);
}

// Tests that local messages propagate to both dimensions.
#[test]
fn propagates_locally_generated_assignment_to_both_dimensions() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let peers = make_peers_and_authority_ids(100);

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		State::default(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;

			// Connect all peers.
			for (peer, _) in &peers {
				setup_peer_with_view(overseer, peer, view![hash], ValidationVersion::V3).await;
			}

			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();

			// Set up a gossip topology.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(
					1,
					&peers_with_optional_peer_id,
					&[0, 10, 20, 30, 40, 60, 70, 80],
					&[50, 51, 52, 53, 54, 55, 56, 57],
					1,
				),
			)
			.await;

			let expected_indices = [
				// Both dimensions in the gossip topology
				0, 10, 20, 30, 40, 60, 70, 80, 50, 51, 52, 53, 54, 55, 56, 57,
			];

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};

			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			let validator_index = ValidatorIndex(0);
			let candidate_index = 0u32;

			// import an assignment and approval locally.
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
			let approval = IndirectSignedApprovalVoteV2 {
				block_hash: hash,
				candidate_indices: candidate_index.into(),
				validator: validator_index,
				signature: dummy_signature(),
			};

			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(
					cert.clone().into(),
					candidate_index.into(),
				),
			)
			.await;

			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeApproval(approval.clone().into()),
			)
			.await;

			let assignments = vec![(cert.clone(), candidate_index.into())];
			let approvals = vec![approval.clone()];

			let mut assignment_sent_peers = assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					sent_peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(sent_assignments)
					))
				)) => {
					assert_eq!(sent_peers.len(), expected_indices.len() + 4);
					for &i in &expected_indices {
						assert!(
							sent_peers.contains(&peers[i].0),
							"Message not sent to expected peer {}",
							i,
						);
					}
					assert_eq!(sent_assignments, assignments);
					sent_peers
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					mut sent_peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Approvals(sent_approvals)
					))
				)) => {
					// Random sampling is reused from the assignment.
					sent_peers.sort();
					assignment_sent_peers.sort();
					assert_eq!(sent_peers, assignment_sent_peers);
					assert_eq!(sent_approvals, approvals);
				}
			);

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none(), "no message should be sent");
			virtual_overseer
		},
	);
}

// Tests that messages propagate to the unshared dimension.
#[test]
fn propagates_assignments_along_unshared_dimension() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let peers = make_peers_and_authority_ids(100);

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;

			// Connect all peers.
			for (peer, _) in &peers {
				setup_peer_with_view(overseer, peer, view![hash], ValidationVersion::V3).await;
			}

			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();

			// Set up a gossip topology.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(
					1,
					&peers_with_optional_peer_id,
					&[0, 10, 20, 30],
					&[50, 51, 52, 53],
					1,
				),
			)
			.await;

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};

			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			// Test messages from X direction go to Y peers
			{
				let validator_index = ValidatorIndex(0);
				let candidate_index = 0u32;

				// import an assignment and approval locally.
				let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
				let assignments = vec![(cert.clone(), candidate_index.into())];

				let msg =
					protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());

				// Issuer of the message is important, not the peer we receive from.
				// 99 deliberately chosen because it's not in X or Y.
				send_message_from_peer_v3(overseer, &peers[99].0, msg).await;
				provide_session(
					overseer,
					dummy_session_info_valid(1, &mut LocalKeystore::in_memory(), 1),
				)
				.await;
				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
						assignment, _,
					)) => {
						assert_eq!(assignment.tranche(), 0);
					}
				);
				expect_reputation_change(overseer, &peers[99].0, BENEFIT_VALID_MESSAGE_FIRST).await;

				let expected_y = [50, 51, 52, 53];

				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
						sent_peers,
						ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
							protocol_v3::ApprovalDistributionMessage::Assignments(sent_assignments)
						))
					)) => {
						assert_eq!(sent_peers.len(), expected_y.len() + 4);
						for &i in &expected_y {
							assert!(
								sent_peers.contains(&peers[i].0),
								"Message not sent to expected peer {}",
								i,
							);
						}
						assert_eq!(sent_assignments, assignments);
					}
				);
			};

			// Test messages from X direction go to Y peers
			{
				let validator_index = ValidatorIndex(50);
				let candidate_index = 0u32;

				// import an assignment and approval locally.
				let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(50).into());
				let assignments = vec![(cert.clone(), candidate_index.into())];

				let msg =
					protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());

				// Issuer of the message is important, not the peer we receive from.
				// 99 deliberately chosen because it's not in X or Y.
				send_message_from_peer_v3(overseer, &peers[99].0, msg).await;
				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
						assignment, _,
					)) => {
						assert_eq!(assignment.tranche(), 0);
					}
				);
				expect_reputation_change(overseer, &peers[99].0, BENEFIT_VALID_MESSAGE_FIRST).await;

				let expected_x = [0, 10, 20, 30];

				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
						sent_peers,
						ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
							protocol_v3::ApprovalDistributionMessage::Assignments(sent_assignments)
						))
					)) => {
						assert_eq!(sent_peers.len(), expected_x.len() + 4);
						for &i in &expected_x {
							assert!(
								sent_peers.contains(&peers[i].0),
								"Message not sent to expected peer {}",
								i,
							);
						}
						assert_eq!(sent_assignments, assignments);
					}
				);
			};

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none(), "no message should be sent");
			virtual_overseer
		},
	);
}

// tests that messages are propagated to necessary peers after they connect
#[test]
fn propagates_to_required_after_connect() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let peers = make_peers_and_authority_ids(100);

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		State::default(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;

			let omitted = [0, 10, 50, 51];

			// Connect all peers except omitted.
			for (i, (peer, _)) in peers.iter().enumerate() {
				if !omitted.contains(&i) {
					setup_peer_with_view(overseer, peer, view![hash], ValidationVersion::V3).await;
				}
			}
			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			// Set up a gossip topology.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(
					1,
					&peers_with_optional_peer_id,
					&[0, 10, 20, 30, 40, 60, 70, 80],
					&[50, 51, 52, 53, 54, 55, 56, 57],
					1,
				),
			)
			.await;

			let expected_indices = [
				// Both dimensions in the gossip topology, minus omitted.
				20, 30, 40, 60, 70, 80, 52, 53, 54, 55, 56, 57,
			];

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};

			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			let validator_index = ValidatorIndex(0);
			let candidate_index = 0u32;

			// import an assignment and approval locally.
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
			let approval = IndirectSignedApprovalVoteV2 {
				block_hash: hash,
				candidate_indices: candidate_index.into(),
				validator: validator_index,
				signature: dummy_signature(),
			};

			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(
					cert.clone().into(),
					candidate_index.into(),
				),
			)
			.await;

			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeApproval(approval.clone().into()),
			)
			.await;

			let assignments = vec![(cert.clone(), candidate_index.into())];
			let approvals = vec![approval.clone()];

			let mut assignment_sent_peers = assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					sent_peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(sent_assignments)
					))
				)) => {
					assert_eq!(sent_peers.len(), expected_indices.len() + 4);
					for &i in &expected_indices {
						assert!(
							sent_peers.contains(&peers[i].0),
							"Message not sent to expected peer {}",
							i,
						);
					}
					assert_eq!(sent_assignments, assignments);
					sent_peers
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					mut sent_peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Approvals(sent_approvals)
					))
				)) => {
					// Random sampling is reused from the assignment.
					sent_peers.sort();
					assignment_sent_peers.sort();
					assert_eq!(sent_peers, assignment_sent_peers);
					assert_eq!(sent_approvals, approvals);
				}
			);

			for i in omitted.iter().copied() {
				setup_peer_with_view(overseer, &peers[i].0, view![hash], ValidationVersion::V3)
					.await;

				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
						sent_peers,
						ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
							protocol_v3::ApprovalDistributionMessage::Assignments(sent_assignments)
						))
					)) => {
						assert_eq!(sent_peers.len(), 1);
						assert_eq!(&sent_peers[0], &peers[i].0);
						assert_eq!(sent_assignments, assignments);
					}
				);

				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
						sent_peers,
						ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
							protocol_v3::ApprovalDistributionMessage::Approvals(sent_approvals)
						))
					)) => {
						assert_eq!(sent_peers.len(), 1);
						assert_eq!(&sent_peers[0], &peers[i].0);
						assert_eq!(sent_approvals, approvals);
					}
				);
			}

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none(), "no message should be sent");
			virtual_overseer
		},
	);
}

// test that new gossip topology triggers send of messages.
#[test]
fn sends_to_more_peers_after_getting_topology() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let peers = make_peers_and_authority_ids(100);

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		State::default(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;

			// Connect all peers except omitted.
			for (peer, _) in &peers {
				setup_peer_with_view(overseer, peer, view![hash], ValidationVersion::V3).await;
			}

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};

			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			let validator_index = ValidatorIndex(0);
			let candidate_index = 0u32;

			// import an assignment and approval locally.
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
			let approval = IndirectSignedApprovalVoteV2 {
				block_hash: hash,
				candidate_indices: candidate_index.into(),
				validator: validator_index,
				signature: dummy_signature(),
			};

			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(
					cert.clone().into(),
					candidate_index.into(),
				),
			)
			.await;

			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeApproval(approval.clone().into()),
			)
			.await;

			let assignments = vec![(cert.clone(), candidate_index.into())];
			let approvals = vec![approval.clone()];

			let expected_indices = vec![0, 10, 20, 30, 50, 51, 52, 53];
			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			// Set up a gossip topology.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(
					1,
					&peers_with_optional_peer_id,
					&[0, 10, 20, 30],
					&[50, 51, 52, 53],
					1,
				),
			)
			.await;

			let mut expected_indices_assignments = expected_indices.clone();
			let mut expected_indices_approvals = expected_indices.clone();

			for _ in 0..expected_indices_assignments.len() {
				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
						sent_peers,
						ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
							protocol_v3::ApprovalDistributionMessage::Assignments(sent_assignments)
						))
					)) => {
						// Sends to all expected peers.
						assert_eq!(sent_peers.len(), 1);
						assert_eq!(sent_assignments, assignments);

						let pos = expected_indices_assignments.iter()
							.position(|i| &peers[*i].0 == &sent_peers[0])
							.unwrap();
						expected_indices_assignments.remove(pos);
					}
				);
			}

			for _ in 0..expected_indices_approvals.len() {
				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
						sent_peers,
						ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
							protocol_v3::ApprovalDistributionMessage::Approvals(sent_approvals)
						))
					)) => {
						// Sends to all expected peers.
						assert_eq!(sent_peers.len(), 1);
						assert_eq!(sent_approvals, approvals);

						let pos = expected_indices_approvals.iter()
							.position(|i| &peers[*i].0 == &sent_peers[0])
							.unwrap();

						expected_indices_approvals.remove(pos);
					}
				);
			}

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none(), "no message should be sent");
			virtual_overseer
		},
	);
}

// test aggression L1
#[test]
fn originator_aggression_l1() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let peers = make_peers_and_authority_ids(100);

	let mut state = State::default();
	state.aggression_config.resend_unfinalized_period = None;
	let aggression_l1_threshold = state.aggression_config.l1_threshold.unwrap();

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state,
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;

			// Connect all peers except omitted.
			for (peer, _) in &peers {
				setup_peer_with_view(overseer, peer, view![hash], ValidationVersion::V3).await;
			}

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};

			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			let validator_index = ValidatorIndex(0);
			let candidate_index = 0u32;

			// import an assignment and approval locally.
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
			let approval = IndirectSignedApprovalVoteV2 {
				block_hash: hash,
				candidate_indices: candidate_index.into(),
				validator: validator_index,
				signature: dummy_signature(),
			};
			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			// Set up a gossip topology.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(
					1,
					&peers_with_optional_peer_id,
					&[0, 10, 20, 30],
					&[50, 51, 52, 53],
					1,
				),
			)
			.await;

			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(
					cert.clone().into(),
					candidate_index.into(),
				),
			)
			.await;

			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeApproval(approval.clone().into()),
			)
			.await;

			let assignments = vec![(cert.clone(), candidate_index.into())];
			let approvals = vec![approval.clone()];

			let prev_sent_indices = assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					sent_peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(_)
					))
				)) => {
					sent_peers.into_iter()
						.filter_map(|sp| peers.iter().position(|p| &p.0 == &sp))
						.collect::<Vec<_>>()
				}
			);

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					_,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Approvals(_)
					))
				)) => { }
			);

			// Add blocks until aggression L1 is triggered.
			{
				let mut parent_hash = hash;
				for level in 0..aggression_l1_threshold {
					let number = 1 + level + 1; // first block had number 1
					let hash = BlakeTwo256::hash_of(&(parent_hash, number));
					let meta = BlockApprovalMeta {
						hash,
						parent_hash,
						number,
						candidates: vec![],
						slot: (level as u64).into(),
						session: 1,
						vrf_story: RelayVRFStory(Default::default()),
					};

					let msg = ApprovalDistributionMessage::ApprovalCheckingLagUpdate(level + 1);
					overseer_send(overseer, msg).await;

					let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
					overseer_send(overseer, msg).await;

					parent_hash = hash;
				}
			}

			let unsent_indices =
				(0..peers.len()).filter(|i| !prev_sent_indices.contains(&i)).collect::<Vec<_>>();

			for _ in 0..unsent_indices.len() {
				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
						sent_peers,
						ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
							protocol_v3::ApprovalDistributionMessage::Assignments(sent_assignments)
						))
					)) => {
						// Sends to all expected peers.
						assert_eq!(sent_peers.len(), 1);
						assert_eq!(sent_assignments, assignments);

						assert!(unsent_indices.iter()
							.any(|i| &peers[*i].0 == &sent_peers[0]));
					}
				);
			}

			for _ in 0..unsent_indices.len() {
				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
						sent_peers,
						ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
							protocol_v3::ApprovalDistributionMessage::Approvals(sent_approvals)
						))
					)) => {
						// Sends to all expected peers.
						assert_eq!(sent_peers.len(), 1);
						assert_eq!(sent_approvals, approvals);

						assert!(unsent_indices.iter()
							.any(|i| &peers[*i].0 == &sent_peers[0]));
					}
				);
			}

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none(), "no message should be sent");
			virtual_overseer
		},
	);
}

// test aggression L1
#[test]
fn non_originator_aggression_l1() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let peers = make_peers_and_authority_ids(100);

	let mut state = state_without_reputation_delay();
	state.aggression_config.resend_unfinalized_period = None;
	let aggression_l1_threshold = state.aggression_config.l1_threshold.unwrap();

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state,
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;

			// Connect all peers except omitted.
			for (peer, _) in &peers {
				setup_peer_with_view(overseer, peer, view![hash], ValidationVersion::V3).await;
			}

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};

			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			let validator_index = ValidatorIndex(0);
			let candidate_index = 0u32;

			// import an assignment and approval locally.
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			// Set up a gossip topology.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(
					1,
					&peers_with_optional_peer_id,
					&[0, 10, 20, 30],
					&[50, 51, 52, 53],
					1,
				),
			)
			.await;

			let assignments = vec![(cert.clone().into(), candidate_index.into())];
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());

			// Issuer of the message is important, not the peer we receive from.
			// 99 deliberately chosen because it's not in X or Y.
			send_message_from_peer_v3(overseer, &peers[99].0, msg).await;
			provide_session(
				overseer,
				dummy_session_info_valid(1, &mut LocalKeystore::in_memory(), 1),
			)
			.await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment, _,
				)) => {
					assert_eq!(assignment.tranche(), 0);
				}
			);

			expect_reputation_change(overseer, &peers[99].0, BENEFIT_VALID_MESSAGE_FIRST).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					_,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(_)
					))
				)) => { }
			);

			// Add blocks until aggression L1 is triggered.
			{
				let mut parent_hash = hash;
				for level in 0..aggression_l1_threshold {
					let number = 1 + level + 1; // first block had number 1
					let hash = BlakeTwo256::hash_of(&(parent_hash, number));
					let meta = BlockApprovalMeta {
						hash,
						parent_hash,
						number,
						candidates: vec![],
						slot: (level as u64).into(),
						session: 1,
						vrf_story: RelayVRFStory(Default::default()),
					};

					let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
					overseer_send(overseer, msg).await;

					parent_hash = hash;
				}
			}

			// No-op on non-originator

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none(), "no message should be sent");
			virtual_overseer
		},
	);
}

// test aggression L2 on non-originator
#[test]
fn non_originator_aggression_l2() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let peers = make_peers_and_authority_ids(100);

	let mut state = state_without_reputation_delay();
	state.aggression_config.resend_unfinalized_period = None;

	let aggression_l1_threshold = state.aggression_config.l1_threshold.unwrap();
	let aggression_l2_threshold = state.aggression_config.l2_threshold.unwrap();
	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state,
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;

			// Connect all peers except omitted.
			for (peer, _) in &peers {
				setup_peer_with_view(overseer, peer, view![hash], ValidationVersion::V3).await;
			}

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};

			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			let validator_index = ValidatorIndex(0);
			let candidate_index = 0u32;

			// import an assignment and approval locally.
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			// Set up a gossip topology.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(
					1,
					&peers_with_optional_peer_id,
					&[0, 10, 20, 30],
					&[50, 51, 52, 53],
					1,
				),
			)
			.await;

			let assignments = vec![(cert.clone(), candidate_index.into())];
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());

			// Issuer of the message is important, not the peer we receive from.
			// 99 deliberately chosen because it's not in X or Y.
			send_message_from_peer_v3(overseer, &peers[99].0, msg).await;
			provide_session(
				overseer,
				dummy_session_info_valid(1, &mut LocalKeystore::in_memory(), 1),
			)
			.await;
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment, _,
				)) => {
					assert_eq!(assignment.tranche(), 0);
				}
			);

			expect_reputation_change(overseer, &peers[99].0, BENEFIT_VALID_MESSAGE_FIRST).await;

			let prev_sent_indices = assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					sent_peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(_)
					))
				)) => {
					sent_peers.into_iter()
						.filter_map(|sp| peers.iter().position(|p| &p.0 == &sp))
						.collect::<Vec<_>>()
				}
			);

			// Add blocks until aggression L1 is triggered.
			let chain_head = {
				let mut parent_hash = hash;
				for level in 0..aggression_l1_threshold {
					let number = 1 + level + 1; // first block had number 1
					let hash = BlakeTwo256::hash_of(&(parent_hash, number));
					let meta = BlockApprovalMeta {
						hash,
						parent_hash,
						number,
						candidates: vec![],
						slot: (level as u64).into(),
						session: 1,
						vrf_story: RelayVRFStory(Default::default()),
					};

					let msg = ApprovalDistributionMessage::ApprovalCheckingLagUpdate(level + 1);
					overseer_send(overseer, msg).await;

					let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
					overseer_send(overseer, msg).await;

					parent_hash = hash;
				}

				parent_hash
			};

			// No-op on non-originator

			// Add blocks until aggression L2 is triggered.
			{
				let mut parent_hash = chain_head;
				for level in 0..aggression_l2_threshold - aggression_l1_threshold {
					let number = aggression_l1_threshold + level + 1 + 1; // first block had number 1
					let hash = BlakeTwo256::hash_of(&(parent_hash, number));
					let meta = BlockApprovalMeta {
						hash,
						parent_hash,
						number,
						candidates: vec![],
						slot: (level as u64).into(),
						session: 1,
						vrf_story: RelayVRFStory(Default::default()),
					};

					let msg = ApprovalDistributionMessage::ApprovalCheckingLagUpdate(
						aggression_l1_threshold + level + 1,
					);
					overseer_send(overseer, msg).await;
					let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
					overseer_send(overseer, msg).await;

					parent_hash = hash;
				}
			}

			// XY dimension - previously sent.
			let unsent_indices = [0, 10, 20, 30, 50, 51, 52, 53]
				.iter()
				.cloned()
				.filter(|i| !prev_sent_indices.contains(&i))
				.collect::<Vec<_>>();

			for _ in 0..unsent_indices.len() {
				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
						sent_peers,
						ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
							protocol_v3::ApprovalDistributionMessage::Assignments(sent_assignments)
						))
					)) => {
						// Sends to all expected peers.
						assert_eq!(sent_peers.len(), 1);
						assert_eq!(sent_assignments, assignments);

						assert!(unsent_indices.iter()
							.any(|i| &peers[*i].0 == &sent_peers[0]));
					}
				);
			}

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none(), "no message should be sent");
			virtual_overseer
		},
	);
}

// Tests that messages propagate to the unshared dimension.
#[test]
fn resends_messages_periodically() {
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let peers = make_peers_and_authority_ids(100);

	let mut state = state_without_reputation_delay();
	state.aggression_config.l1_threshold = None;
	state.aggression_config.l2_threshold = None;
	state.aggression_config.resend_unfinalized_period = Some(2);
	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state,
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;

			// Connect all peers.
			for (peer, _) in &peers {
				setup_peer_with_view(overseer, peer, view![hash], ValidationVersion::V3).await;
			}
			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			// Set up a gossip topology.
			setup_gossip_topology(
				overseer,
				make_gossip_topology(
					1,
					&peers_with_optional_peer_id,
					&[0, 10, 20, 30],
					&[50, 51, 52, 53],
					1,
				),
			)
			.await;

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};

			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			let validator_index = ValidatorIndex(0);
			let candidate_index = 0u32;

			// import an assignment and approval locally.
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
			let assignments = vec![(cert.clone(), candidate_index.into())];

			{
				let msg =
					protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());

				// Issuer of the message is important, not the peer we receive from.
				// 99 deliberately chosen because it's not in X or Y.
				send_message_from_peer_v3(overseer, &peers[99].0, msg).await;
				provide_session(
					overseer,
					dummy_session_info_valid(1, &mut LocalKeystore::in_memory(), 1),
				)
				.await;

				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
						assignment, _,
					)) => {
						assert_eq!(assignment.tranche(), 0);
					}
				);
				expect_reputation_change(overseer, &peers[99].0, BENEFIT_VALID_MESSAGE_FIRST).await;

				let expected_y = [50, 51, 52, 53];

				assert_matches!(
					overseer_recv(overseer).await,
					AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
						sent_peers,
						ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
							protocol_v3::ApprovalDistributionMessage::Assignments(sent_assignments)
						))
					)) => {
						assert_eq!(sent_peers.len(), expected_y.len() + 4);
						for &i in &expected_y {
							assert!(
								sent_peers.contains(&peers[i].0),
								"Message not sent to expected peer {}",
								i,
							);
						}
						assert_eq!(sent_assignments, assignments);
					}
				);
			};

			let mut number = 1;
			for _ in 0..10 {
				// Add blocks until resend is done.
				{
					let mut parent_hash = hash;
					for level in 0..4 {
						number = number + 1;
						let hash = BlakeTwo256::hash_of(&(parent_hash, number));
						let meta = BlockApprovalMeta {
							hash,
							parent_hash,
							number,
							candidates: vec![],
							slot: (level as u64).into(),
							session: 1,
							vrf_story: RelayVRFStory(Default::default()),
						};

						let msg = ApprovalDistributionMessage::ApprovalCheckingLagUpdate(2);
						overseer_send(overseer, msg).await;
						let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
						overseer_send(overseer, msg).await;

						parent_hash = hash;
					}
				}

				let mut expected_y = vec![50, 51, 52, 53];

				// Expect messages sent only to topology peers, one by one.
				for _ in 0..expected_y.len() {
					assert_matches!(
						overseer_recv(overseer).await,
						AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
							sent_peers,
							ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
								protocol_v3::ApprovalDistributionMessage::Assignments(sent_assignments)
							))
						)) => {
							assert_eq!(sent_peers.len(), 1);
							let expected_pos = expected_y.iter()
								.position(|&i| &peers[i].0 == &sent_peers[0])
								.unwrap();

							expected_y.remove(expected_pos);
							assert_eq!(sent_assignments, assignments);
						}
					);
				}
			}

			assert!(overseer.recv().timeout(TIMEOUT).await.is_none(), "no message should be sent");
			virtual_overseer
		},
	);
}

/// Tests that peers correctly receive versioned messages.
#[test]
fn import_versioned_approval() {
	let peers = make_peers_and_authority_ids(15);
	let peer_a = peers.get(0).unwrap().0;
	let peer_b = peers.get(1).unwrap().0;
	let peer_c = peers.get(2).unwrap().0;

	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);
	let state = state_without_reputation_delay();
	let candidate_hash = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xBB));
	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state,
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// All peers are aware of relay parent.
			setup_peer_with_view(overseer, &peer_a, view![hash], ValidationVersion::V3).await;
			setup_peer_with_view(overseer, &peer_b, view![hash], ValidationVersion::V3).await;
			setup_peer_with_view(overseer, &peer_c, view![hash], ValidationVersion::V3).await;

			// Set up a gossip topology, where a, b, c and d are topology neighbors to the node
			// under testing.
			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0, 1], &[2, 4], 3),
			)
			.await;

			let mut keystore = LocalKeystore::in_memory();
			let session = dummy_session_info_valid(1, &mut keystore, 1);

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![(candidate_hash, 0.into(), 0.into()); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			// import an assignment related to `hash` locally
			let validator_index = ValidatorIndex(0);
			let candidate_index = 0u32;
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
			overseer_send(
				overseer,
				ApprovalDistributionMessage::DistributeAssignment(
					cert.into(),
					candidate_index.into(),
				),
			)
			.await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert_eq!(peers.len(), 3);
					assert!(peers.contains(&peer_a));
					assert!(peers.contains(&peer_b));
					assert!(peers.contains(&peer_c));

					assert_eq!(assignments.len(), 1);
				}
			);

			// send an approval from peer_a
			let approval = IndirectSignedApprovalVoteV2 {
				block_hash: hash,
				candidate_indices: candidate_index.into(),
				validator: validator_index,
				signature: signature_for(
					&keystore,
					&session,
					vec![candidate_hash],
					validator_index,
				),
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Approvals(vec![approval.clone()]);
			send_message_from_peer_v3(overseer, &peer_a, msg).await;
			provide_session(overseer, session).await;
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportApproval(
					vote, _,
				)) => {
					assert_eq!(Into::<IndirectSignedApprovalVoteV2>::into(vote), approval.into());
				}
			);

			expect_reputation_change(overseer, &peer_a, BENEFIT_VALID_MESSAGE_FIRST).await;

			// Peers b and c receive versioned approval messages.
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Approvals(approvals)
					))
				)) => {
					assert!(peers.contains(&peer_b));
					assert!(peers.contains(&peer_c));
					assert_eq!(approvals.len(), 1);
				}
			);

			// send an obviously invalid approval
			let approval = IndirectSignedApprovalVoteV2 {
				block_hash: hash,
				// Invalid candidate index, should not pass sanitization.
				candidate_indices: 16777284.into(),
				validator: validator_index,
				signature: dummy_signature(),
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Approvals(vec![approval.clone()]);
			send_message_from_peer_v3(overseer, &peer_a, msg).await;

			expect_reputation_change(overseer, &peer_a, COST_OVERSIZED_BITFIELD).await;

			// send an obviously invalid approval
			let approval = IndirectSignedApprovalVoteV2 {
				block_hash: hash,
				// Invalid candidates len, should not pass sanitization.
				candidate_indices: 16777284.into(),
				validator: validator_index,
				signature: dummy_signature(),
			};
			let msg = protocol_v3::ApprovalDistributionMessage::Approvals(vec![approval.clone()]);
			send_message_from_peer_v3(overseer, &peer_a, msg).await;

			expect_reputation_change(overseer, &peer_a, COST_OVERSIZED_BITFIELD).await;

			virtual_overseer
		},
	);
}

fn batch_test_round(message_count: usize) {
	use polkadot_node_subsystem::SubsystemContext;
	let pool = sp_core::testing::TaskExecutor::new();
	let mut state = State::default();

	let (mut context, mut virtual_overseer) =
		polkadot_node_subsystem_test_helpers::make_subsystem_context(pool.clone());
	let subsystem = ApprovalDistribution::new_with_clock(
		Default::default(),
		Default::default(),
		Arc::new(SystemClock {}),
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
	);
	let mut rng = rand_chacha::ChaCha12Rng::seed_from_u64(12345);
	let mut sender = context.sender().clone();
	let mut session_info_provider = RuntimeInfo::new_with_config(RuntimeInfoConfig {
		keystore: None,
		session_cache_lru_size: DISPUTE_WINDOW.get(),
	});

	let subsystem = subsystem.run_inner(
		context,
		&mut state,
		REPUTATION_CHANGE_TEST_INTERVAL,
		&mut rng,
		&mut session_info_provider,
	);

	let test_fut = async move {
		let overseer = &mut virtual_overseer;
		let validators = 0..message_count;
		let assignments: Vec<_> = validators
			.clone()
			.map(|index| {
				(
					fake_assignment_cert_v2(
						Hash::zero(),
						ValidatorIndex(index as u32),
						CoreIndex(index as u32).into(),
					)
					.into(),
					0.into(),
				)
			})
			.collect();

		let approvals: Vec<_> = validators
			.map(|index| IndirectSignedApprovalVoteV2 {
				block_hash: Hash::zero(),
				candidate_indices: 0u32.into(),
				validator: ValidatorIndex(index as u32),
				signature: dummy_signature(),
			})
			.collect();

		let peer = PeerId::random();
		send_assignments_batched(
			&mut sender,
			assignments.clone(),
			&vec![(peer, ValidationVersion::V3.into())],
		)
		.await;
		send_approvals_batched(
			&mut sender,
			approvals.clone(),
			&vec![(peer, ValidationVersion::V3.into())],
		)
		.await;

		// Check expected assignments batches.
		for assignment_index in (0..assignments.len()).step_by(super::MAX_ASSIGNMENT_BATCH_SIZE) {
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(sent_assignments)
					))
				)) => {
					// Last batch should cover all remaining messages.
					if sent_assignments.len() < super::MAX_ASSIGNMENT_BATCH_SIZE {
						assert_eq!(sent_assignments.len() + assignment_index, assignments.len());
					} else {
						assert_eq!(sent_assignments.len(), super::MAX_ASSIGNMENT_BATCH_SIZE);
					}

					assert_eq!(peers.len(), 1);

					for (message_index,  assignment) in sent_assignments.iter().enumerate() {
						assert_eq!(assignment.0, assignments[assignment_index + message_index].0.clone().try_into().unwrap());
						assert_eq!(assignment.1, 0.into());
					}
				}
			);
		}

		// Check approval vote batching.
		for approval_index in (0..approvals.len()).step_by(super::MAX_APPROVAL_BATCH_SIZE) {
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Approvals(sent_approvals)
					))
				)) => {
					// Last batch should cover all remaining messages.
					if sent_approvals.len() < super::MAX_APPROVAL_BATCH_SIZE {
						assert_eq!(sent_approvals.len() + approval_index, approvals.len());
					} else {
						assert_eq!(sent_approvals.len(), super::MAX_APPROVAL_BATCH_SIZE);
					}

					assert_eq!(peers.len(), 1);

					for (message_index,  approval) in sent_approvals.iter().enumerate() {
						assert_eq!(approval, &approvals[approval_index + message_index].clone().try_into().unwrap());
					}
				}
			);
		}
		virtual_overseer
	};

	futures::pin_mut!(test_fut);
	futures::pin_mut!(subsystem);

	executor::block_on(future::join(
		async move {
			let mut overseer = test_fut.await;
			overseer
				.send(FromOrchestra::Signal(OverseerSignal::Conclude))
				.timeout(TIMEOUT)
				.await
				.expect("Conclude send timeout");
		},
		subsystem,
	));
}

#[test]
fn batch_sending_1_msg() {
	batch_test_round(1);
}

#[test]
fn batch_sending_exactly_one_batch() {
	batch_test_round(super::MAX_APPROVAL_BATCH_SIZE);
	batch_test_round(super::MAX_ASSIGNMENT_BATCH_SIZE);
}

#[test]
fn batch_sending_partial_batch() {
	batch_test_round(super::MAX_APPROVAL_BATCH_SIZE * 2 + 4);
	batch_test_round(super::MAX_ASSIGNMENT_BATCH_SIZE * 2 + 4);
}

#[test]
fn batch_sending_multiple_same_len() {
	batch_test_round(super::MAX_APPROVAL_BATCH_SIZE * 10);
	batch_test_round(super::MAX_ASSIGNMENT_BATCH_SIZE * 10);
}

#[test]
fn batch_sending_half_batch() {
	batch_test_round(super::MAX_APPROVAL_BATCH_SIZE / 2);
	batch_test_round(super::MAX_ASSIGNMENT_BATCH_SIZE / 2);
}

#[test]
#[should_panic]
fn const_batch_size_panics_if_zero() {
	crate::ensure_size_not_zero(0);
}

#[test]
fn const_ensure_size_not_zero() {
	crate::ensure_size_not_zero(super::MAX_ASSIGNMENT_BATCH_SIZE);
	crate::ensure_size_not_zero(super::MAX_APPROVAL_BATCH_SIZE);
}

struct DummyClock;
impl Clock for DummyClock {
	fn tick_now(&self) -> polkadot_node_primitives::approval::time::Tick {
		0
	}

	fn wait(
		&self,
		_tick: polkadot_node_primitives::approval::time::Tick,
	) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
		todo!()
	}
}

/// Subsystem rejects assignments too far into the future.
#[test]
fn subsystem_rejects_assignment_in_future() {
	let peers = make_peers_and_authority_ids(15);
	let peer_a = peers.get(0).unwrap().0;
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(89) }),
		Arc::new(DummyClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// setup peers
			setup_peer_with_view(overseer, &peer_a, view![], ValidationVersion::V3).await;

			// Set up a gossip topology, where a, b, c and d are topology neighbors to the node
			// under testing.
			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0, 1], &[2, 4], 3),
			)
			.await;

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 2,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			// send the assignment related to `hash`
			let validator_index = ValidatorIndex(0);
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
			let assignments = vec![(cert.clone(), 0.into())];
			setup_peer_with_view(overseer, &peer_a, view![hash], ValidationVersion::V3).await;

			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());
			send_message_from_peer_v3(overseer, &peer_a, msg).await;
			provide_session(
				overseer,
				dummy_session_info_valid(1, &mut LocalKeystore::in_memory(), 1),
			)
			.await;

			expect_reputation_change(overseer, &peer_a, COST_ASSIGNMENT_TOO_FAR_IN_THE_FUTURE)
				.await;

			virtual_overseer
		},
	);
}

/// Subsystem rejects bad vrf assignments.
#[test]
fn subsystem_rejects_bad_assignments() {
	let peers = make_peers_and_authority_ids(15);
	let peer_a = peers.get(0).unwrap().0;
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria {
			tranche: Err(InvalidAssignment(criteria::InvalidAssignmentReason::NullAssignment)),
		}),
		Arc::new(DummyClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// setup peers
			setup_peer_with_view(overseer, &peer_a, view![], ValidationVersion::V3).await;

			// Set up a gossip topology, where a, b, c and d are topology neighbors to the node
			// under testing.
			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0, 1], &[2, 4], 3),
			)
			.await;

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 2,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			// send the assignment related to `hash`
			let validator_index = ValidatorIndex(0);
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
			let assignments = vec![(cert.clone(), 0.into())];
			setup_peer_with_view(overseer, &peer_a, view![hash], ValidationVersion::V3).await;

			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());
			send_message_from_peer_v3(overseer, &peer_a, msg).await;
			provide_session(
				overseer,
				dummy_session_info_valid(1, &mut LocalKeystore::in_memory(), 1),
			)
			.await;

			expect_reputation_change(overseer, &peer_a, COST_INVALID_MESSAGE).await;

			virtual_overseer
		},
	);
}

/// Subsystem rejects assignments that have invalid claimed candidates.
#[test]
fn subsystem_rejects_wrong_claimed_assignments() {
	let peers = make_peers_and_authority_ids(15);
	let peer_a = peers.get(0).unwrap().0;
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(DummyClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// setup peers
			setup_peer_with_view(overseer, &peer_a, view![], ValidationVersion::V3).await;

			// Set up a gossip topology, where a, b, c and d are topology neighbors to the node
			// under testing.
			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0, 1], &[2, 4], 3),
			)
			.await;

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 2,
				candidates: vec![Default::default(); 1],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			// send the assignment related to `hash`
			let validator_index = ValidatorIndex(0);

			// Claimed core 1 which does not have a candidate included on it, so the assignment
			// should be rejected.
			let cores = vec![1];
			let core_bitfield: CoreBitfield = cores
				.iter()
				.map(|index| CoreIndex(*index))
				.collect::<Vec<_>>()
				.try_into()
				.unwrap();
			let cert = fake_assignment_cert_v2(hash, validator_index, core_bitfield);
			let assignments: Vec<(IndirectAssignmentCertV2, CandidateBitfield)> =
				vec![(cert.clone(), cores.try_into().unwrap())];
			setup_peer_with_view(overseer, &peer_a, view![hash], ValidationVersion::V3).await;

			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());
			send_message_from_peer_v3(overseer, &peer_a, msg).await;
			provide_session(
				overseer,
				dummy_session_info_valid(1, &mut LocalKeystore::in_memory(), 1),
			)
			.await;

			expect_reputation_change(overseer, &peer_a, COST_INVALID_MESSAGE).await;

			virtual_overseer
		},
	);
}

/// Subsystem accepts tranche0 duplicate assignments, sometimes on validator Compact tranche0
/// assignment and Delay tranche assignments land on the same candidate. The delay tranche0 can be
/// safely ignored and we don't need to gossip it however, the compact tranche0 assignment should be
/// gossiped, because other candidates are included in it, this test makes sure this invariant is
/// upheld, see  https://github.com/paritytech/polkadot/pull/2160#discussion_r557628699, for
/// this edge case.
#[test]
fn subsystem_accepts_tranche0_duplicate_assignments() {
	let peers = make_peers_and_authority_ids(15);
	let peer_a = peers.get(0).unwrap().0;
	let peer_b = peers.get(1).unwrap().0;
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);
	let candidate_hash_first = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xBB));
	let candidate_hash_second = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xCC));
	let candidate_hash_third = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xBB));
	let candidate_hash_fourth = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xBB));

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(DummyClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;
			// setup peers
			setup_peer_with_view(overseer, &peer_a, view![], ValidationVersion::V3).await;
			setup_peer_with_view(overseer, &peer_b, view![], ValidationVersion::V3).await;

			// Set up a gossip topology, where a, b, c and d are topology neighbors to the node
			// under testing.
			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0, 1], &[2, 4], 3),
			)
			.await;

			// new block `hash_a` with 1 candidates
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 2,
				candidates: vec![
					(candidate_hash_first, 0.into(), 0.into()),
					(candidate_hash_second, 1.into(), 1.into()),
					(candidate_hash_third, 2.into(), 2.into()),
					(candidate_hash_fourth, 3.into(), 3.into()),
				],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			let msg = ApprovalDistributionMessage::NewBlocks(vec![meta]);
			overseer_send(overseer, msg).await;

			// send the assignment related to `hash`
			let validator_index = ValidatorIndex(0);

			setup_peer_with_view(overseer, &peer_a, view![hash], ValidationVersion::V3).await;
			setup_peer_with_view(overseer, &peer_b, view![hash], ValidationVersion::V3).await;

			// 1. Compact assignment with multiple candidates, coming after delay assignment which
			//    covered just one of the candidate is still imported and gossiped.
			let candidate_indices: CandidateBitfield =
				vec![1 as CandidateIndex].try_into().unwrap();
			let core_bitfield = vec![CoreIndex(1)].try_into().unwrap();
			let cert = fake_assignment_cert_delay(hash, validator_index, core_bitfield);
			let assignments: Vec<(IndirectAssignmentCertV2, CandidateBitfield)> =
				vec![(cert.clone(), candidate_indices.clone())];
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());
			send_message_from_peer_v3(overseer, &peer_a, msg).await;
			provide_session(
				overseer,
				dummy_session_info_valid(1, &mut LocalKeystore::in_memory(), 1),
			)
			.await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment,
					_,
				)) => {
					assert_eq!(assignment.candidate_indices(), &candidate_indices);
					assert_eq!(assignment.assignment(), &cert.into());
					assert_eq!(assignment.tranche(), 0);
				}
			);

			expect_reputation_change(overseer, &peer_a, BENEFIT_VALID_MESSAGE_FIRST).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert_eq!(peers.len(), 1);
					assert_eq!(assignments.len(), 1);
				}
			);

			let candidate_indices: CandidateBitfield =
				vec![0 as CandidateIndex, 1 as CandidateIndex].try_into().unwrap();
			let core_bitfield = vec![CoreIndex(0), CoreIndex(1)].try_into().unwrap();

			let cert = fake_assignment_cert_v2(hash, validator_index, core_bitfield);

			let assignments: Vec<(IndirectAssignmentCertV2, CandidateBitfield)> =
				vec![(cert.clone(), candidate_indices.clone())];
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());
			send_message_from_peer_v3(overseer, &peer_a, msg).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment,
					_,
				)) => {
					assert_eq!(assignment.candidate_indices(), &candidate_indices);
					assert_eq!(assignment.assignment(), &cert.into());
					assert_eq!(assignment.tranche(), 0);
				}
			);

			expect_reputation_change(overseer, &peer_a, BENEFIT_VALID_MESSAGE_FIRST).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert_eq!(peers.len(), 1);
					assert_eq!(assignments.len(), 1);
				}
			);

			// 2. Delay assignment coming after compact assignment that already covered the
			//    candidate is not gossiped anymore.
			let candidate_indices: CandidateBitfield =
				vec![2 as CandidateIndex, 3 as CandidateIndex].try_into().unwrap();
			let core_bitfield = vec![CoreIndex(2), CoreIndex(3)].try_into().unwrap();
			let cert = fake_assignment_cert_v2(hash, validator_index, core_bitfield);
			let assignments: Vec<(IndirectAssignmentCertV2, CandidateBitfield)> =
				vec![(cert.clone(), candidate_indices.clone())];
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());
			send_message_from_peer_v3(overseer, &peer_a, msg).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(
					assignment,
					_,
				)) => {
					assert_eq!(assignment.candidate_indices(), &candidate_indices);
					assert_eq!(assignment.assignment(), &cert.into());
					assert_eq!(assignment.tranche(), 0);
				}
			);

			expect_reputation_change(overseer, &peer_a, BENEFIT_VALID_MESSAGE_FIRST).await;

			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					ValidationProtocols::V3(protocol_v3::ValidationProtocol::ApprovalDistribution(
						protocol_v3::ApprovalDistributionMessage::Assignments(assignments)
					))
				)) => {
					assert_eq!(peers.len(), 1);
					assert_eq!(assignments.len(), 1);
				}
			);

			let candidate_indices: CandidateBitfield = vec![3].try_into().unwrap();
			let core_bitfield = vec![CoreIndex(3)].try_into().unwrap();

			let cert = fake_assignment_cert_delay(hash, validator_index, core_bitfield);

			let assignments: Vec<(IndirectAssignmentCertV2, CandidateBitfield)> =
				vec![(cert.clone(), candidate_indices.clone())];
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments.clone());
			send_message_from_peer_v3(overseer, &peer_a, msg).await;

			expect_reputation_change(overseer, &peer_a, COST_DUPLICATE_MESSAGE).await;

			virtual_overseer
		},
	);
}

#[test]
fn test_empty_bitfield_gets_rejected_early() {
	let peers = make_peers_and_authority_ids(15);
	let peer_a = peers.get(0).unwrap().0;
	let parent_hash = Hash::repeat_byte(0xFF);
	let hash = Hash::repeat_byte(0xAA);
	let candidate_hash = polkadot_primitives::CandidateHash(Hash::repeat_byte(0xBB));

	let _ = test_harness(
		Arc::new(MockAssignmentCriteria { tranche: Ok(0) }),
		Arc::new(SystemClock {}),
		state_without_reputation_delay(),
		|mut virtual_overseer| async move {
			let overseer = &mut virtual_overseer;

			// Setup peer
			setup_peer_with_view(overseer, &peer_a, view![hash], ValidationVersion::V3).await;

			let mut keystore = LocalKeystore::in_memory();
			let session = dummy_session_info_valid(1, &mut keystore, 1);

			// Setup block with one candidate
			let meta = BlockApprovalMeta {
				hash,
				parent_hash,
				number: 1,
				candidates: vec![(candidate_hash, 0.into(), 0.into())],
				slot: 1.into(),
				session: 1,
				vrf_story: RelayVRFStory(Default::default()),
			};
			overseer_send(overseer, ApprovalDistributionMessage::NewBlocks(vec![meta])).await;

			// Setup gossip topology
			let peers_with_optional_peer_id = peers
				.iter()
				.map(|(peer_id, authority)| (Some(*peer_id), authority.clone()))
				.collect_vec();
			setup_gossip_topology(
				overseer,
				make_gossip_topology(1, &peers_with_optional_peer_id, &[0], &[2], 1),
			)
			.await;

			// Send assignment first
			let validator_index = ValidatorIndex(0);
			let candidate_index = 0u32;
			let cert = fake_assignment_cert_v2(hash, validator_index, CoreIndex(0).into());
			let assignments = vec![(cert.clone(), candidate_index.into())];
			let msg = protocol_v3::ApprovalDistributionMessage::Assignments(assignments);
			send_message_from_peer_v3(overseer, &peer_a, msg).await;
			provide_session(overseer, session.clone()).await;

			// Should receive the assignment
			assert_matches!(
				overseer_recv(overseer).await,
				AllMessages::ApprovalVoting(ApprovalVotingMessage::ImportAssignment(_, _))
			);
			expect_reputation_change(overseer, &peer_a, BENEFIT_VALID_MESSAGE_FIRST).await;

			// Create an approval with empty candidate_indices is rejected early
			let mut candidate_indices: CandidateBitfield = vec![0].try_into().unwrap();
			candidate_indices.inner_mut().clear();

			let normal_approval = IndirectSignedApprovalVoteV2 {
				block_hash: hash,
				candidate_indices: candidate_indices.clone(),
				validator: validator_index,
				signature: signature_for(
					&keystore,
					&session,
					vec![candidate_hash],
					validator_index,
				),
			};

			let approval_to_send = normal_approval;

			// Send the approval
			let msg =
				protocol_v3::ApprovalDistributionMessage::Approvals(vec![approval_to_send.clone()]);
			send_message_from_peer_v3(overseer, &peer_a, msg).await;

			// Expect rejection due to invalid message
			expect_reputation_change(overseer, &peer_a, COST_INVALID_MESSAGE).await;

			virtual_overseer
		},
	);
}
