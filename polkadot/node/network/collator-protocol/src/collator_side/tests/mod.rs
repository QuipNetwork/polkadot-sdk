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

use std::{
	collections::{BTreeMap, HashSet, VecDeque},
	time::Duration,
};

use assert_matches::assert_matches;
use futures::{executor, future, Future};
use futures_timer::Delay;

use codec::{Decode, Encode};

use sc_network::config::IncomingRequest as RawIncomingRequest;
use sp_core::crypto::Pair;
use sp_keyring::Sr25519Keyring;
use sp_runtime::traits::AppVerify;

use itertools::Itertools;
use polkadot_node_network_protocol::{
	peer_set::CollationVersion,
	request_response::{
		v2::{CollationFetchingRequest, CollationFetchingResponse},
		v3 as request_v3, IncomingRequest, ReqProtocolNames,
	},
	view, ObservedRole,
};
use polkadot_node_primitives::BlockData;
use polkadot_node_subsystem::{
	messages::{AllMessages, ReportPeerMessage, RuntimeApiMessage, RuntimeApiRequest},
	ActivatedLeaf, ActiveLeavesUpdate,
};
use polkadot_node_subsystem_test_helpers as test_helpers;
use polkadot_node_subsystem_util::{reputation::add_reputation, TimeoutExt};
use polkadot_primitives::{
	ApprovalVotingParams, AuthorityDiscoveryId, Block, CollatorPair, GroupIndex, GroupRotationInfo,
	IndexedVec, NodeFeatures, SessionIndex, SessionInfo, ValidatorId, ValidatorIndex,
};
use polkadot_primitives_test_helpers::TestCandidateBuilder;
use rstest::rstest;

mod prospective_parachains;
use prospective_parachains::{expect_declare_msg, update_view};

const REPUTATION_CHANGE_TEST_INTERVAL: Duration = Duration::from_millis(10);

const SCHEDULING_LOOKAHEAD: usize = 4;

#[derive(Clone)]
struct TestState {
	para_id: ParaId,
	session_info: SessionInfo,
	group_rotation_info: GroupRotationInfo,
	validator_peer_id: Vec<PeerId>,
	scheduling_parent: Hash,
	claim_queue: BTreeMap<CoreIndex, VecDeque<ParaId>>,
	local_peer_id: PeerId,
	collator_pair: CollatorPair,
	session_index: SessionIndex,
}

fn validator_pubkeys(val_ids: &[Sr25519Keyring]) -> IndexedVec<ValidatorIndex, ValidatorId> {
	val_ids.iter().map(|v| v.public().into()).collect()
}

fn validator_authority_id(val_ids: &[Sr25519Keyring]) -> Vec<AuthorityDiscoveryId> {
	val_ids.iter().map(|v| v.public().into()).collect()
}

impl Default for TestState {
	fn default() -> Self {
		let para_id = ParaId::from(1);

		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Ferdie,
		];

		let validator_public = validator_pubkeys(&validators);
		let discovery_keys = validator_authority_id(&validators);

		let validator_peer_id =
			std::iter::repeat_with(|| PeerId::random()).take(discovery_keys.len()).collect();

		let validator_groups = vec![vec![2, 0, 4], vec![1, 3], vec![], vec![]]
			.into_iter()
			.map(|g| g.into_iter().map(ValidatorIndex).collect())
			.collect();
		let group_rotation_info =
			GroupRotationInfo { session_start_block: 0, group_rotation_frequency: 100, now: 1 };

		let mut claim_queue = BTreeMap::new();
		claim_queue
			.insert(CoreIndex(0), std::iter::repeat(para_id).take(SCHEDULING_LOOKAHEAD).collect());
		claim_queue.insert(CoreIndex(1), VecDeque::new());

		let scheduling_parent = Hash::random();

		let local_peer_id = PeerId::random();
		let collator_pair = CollatorPair::generate().0;

		Self {
			para_id,
			session_info: SessionInfo {
				validators: validator_public,
				discovery_keys,
				validator_groups,
				assignment_keys: vec![],
				n_cores: 0,
				zeroth_delay_tranche_width: 0,
				relay_vrf_modulo_samples: 0,
				n_delay_tranches: 0,
				no_show_slots: 0,
				needed_approvals: 0,
				active_validator_indices: vec![],
				dispute_period: 6,
				random_seed: [0u8; 32],
			},
			group_rotation_info,
			validator_peer_id,
			scheduling_parent,
			claim_queue,
			local_peer_id,
			collator_pair,
			session_index: 1,
		}
	}
}

impl TestState {
	/// Adds a few more scheduled cores to the state for the same para id
	/// compared to the default.
	pub fn with_elastic_scaling() -> Self {
		let mut state = Self::default();
		let para_id = state.para_id;

		state.claim_queue.insert(CoreIndex(2), [para_id].into_iter().collect());
		state.claim_queue.insert(CoreIndex(3), [para_id].into_iter().collect());
		state
	}

	fn current_group_validator_indices(&self) -> &[ValidatorIndex] {
		let core_num = self.claim_queue.len();
		let GroupIndex(group_idx) = self.group_rotation_info.group_for_core(CoreIndex(0), core_num);
		&self.session_info.validator_groups.get(GroupIndex::from(group_idx)).unwrap()
	}

	fn validator_indices_for_core(&self, core: CoreIndex) -> &[ValidatorIndex] {
		let core_num = self.claim_queue.len();
		let GroupIndex(group_idx) = self.group_rotation_info.group_for_core(core, core_num);
		&self.session_info.validator_groups.get(GroupIndex::from(group_idx)).unwrap()
	}

	fn current_session_index(&self) -> SessionIndex {
		self.session_index
	}

	fn current_group_validator_peer_ids(&self) -> Vec<PeerId> {
		self.current_group_validator_indices()
			.iter()
			.map(|i| self.validator_peer_id[i.0 as usize])
			.collect()
	}

	fn current_group_validator_authority_ids(&self) -> Vec<AuthorityDiscoveryId> {
		self.current_group_validator_indices()
			.iter()
			.map(|i| self.session_info.discovery_keys[i.0 as usize].clone())
			.collect()
	}

	fn validator_authority_ids_for_core(&self, core: CoreIndex) -> Vec<AuthorityDiscoveryId> {
		self.validator_indices_for_core(core)
			.iter()
			.map(|i| self.session_info.discovery_keys[i.0 as usize].clone())
			.collect()
	}
}

type VirtualOverseer =
	polkadot_node_subsystem_test_helpers::TestSubsystemContextHandle<CollatorProtocolMessage>;

struct TestHarness {
	virtual_overseer: VirtualOverseer,
	req_v2_cfg: sc_network::config::RequestResponseConfig,
	req_v3_cfg: sc_network::config::RequestResponseConfig,
}

fn test_harness<T: Future<Output = TestHarness>>(
	local_peer_id: PeerId,
	collator_pair: CollatorPair,
	reputation: ReputationAggregator,
	test: impl FnOnce(TestHarness) -> T,
) {
	let _ = sp_tracing::init_for_tests();

	let pool = sp_core::testing::TaskExecutor::new();

	let (context, virtual_overseer) =
		polkadot_node_subsystem_test_helpers::make_subsystem_context(pool.clone());

	let genesis_hash = Hash::repeat_byte(0xff);
	let req_protocol_names = ReqProtocolNames::new(&genesis_hash, None);

	let (collation_req_v2_receiver, req_v2_cfg) = IncomingRequest::get_config_receiver::<
		Block,
		sc_network::NetworkWorker<Block, Hash>,
	>(&req_protocol_names);
	let (collation_req_v3_receiver, req_v3_cfg) = IncomingRequest::get_config_receiver::<
		Block,
		sc_network::NetworkWorker<Block, Hash>,
	>(&req_protocol_names);
	let subsystem = async {
		run_inner(
			context,
			local_peer_id,
			collator_pair,
			collation_req_v2_receiver,
			collation_req_v3_receiver,
			Default::default(),
			reputation,
			REPUTATION_CHANGE_TEST_INTERVAL,
			polkadot_node_clock::system_clock(),
		)
		.await
		.unwrap();
	};

	let test_fut = test(TestHarness { virtual_overseer, req_v2_cfg, req_v3_cfg });

	futures::pin_mut!(test_fut);
	futures::pin_mut!(subsystem);

	executor::block_on(future::join(
		async move {
			let mut test_harness = test_fut.await;
			overseer_signal(&mut test_harness.virtual_overseer, OverseerSignal::Conclude).await;
		},
		subsystem,
	))
	.1
}

const TIMEOUT: Duration = Duration::from_millis(100);

async fn overseer_send(overseer: &mut VirtualOverseer, msg: CollatorProtocolMessage) {
	gum::trace!(?msg, "sending message");
	overseer
		.send(FromOrchestra::Communication { msg })
		.timeout(TIMEOUT)
		.await
		.expect(&format!("{:?} is more than enough for sending messages.", TIMEOUT));
}

async fn overseer_recv(overseer: &mut VirtualOverseer) -> AllMessages {
	overseer_recv_with_timeout(overseer, TIMEOUT)
		.await
		.expect(&format!("{:?} is more than enough to receive messages", TIMEOUT))
}

async fn overseer_recv_with_timeout(
	overseer: &mut VirtualOverseer,
	timeout: Duration,
) -> Option<AllMessages> {
	gum::trace!("waiting for message...");
	let msg = overseer.recv().timeout(timeout).await;

	gum::trace!(?msg, "received message");

	msg
}

async fn overseer_peek_with_timeout(
	overseer: &mut VirtualOverseer,
	timeout: Duration,
) -> Option<&AllMessages> {
	gum::trace!("peeking for message...");
	let msg = overseer.peek().timeout(timeout).await;

	gum::trace!(?msg, "received message");

	msg.flatten()
}

async fn overseer_signal(overseer: &mut VirtualOverseer, signal: OverseerSignal) {
	overseer
		.send(FromOrchestra::Signal(signal))
		.timeout(TIMEOUT)
		.await
		.expect(&format!("{:?} is more than enough for sending signals.", TIMEOUT));
}

/// Deconstruct a test receipt into the `SegmentEntry` the new `DistributeSegment`
/// carries. The receiver reassembles an identical receipt from these fields.
fn segment_entry_from_receipt(
	receipt: &CandidateReceipt,
	pov: &PoV,
	parent_head_data: HeadData,
) -> SegmentEntry {
	SegmentEntry {
		relay_parent: receipt.descriptor.relay_parent(),
		session_index: receipt
			.descriptor
			.session_index()
			.expect("test receipts use V2/V3 descriptors which carry a session index"),
		validation_code_hash: receipt.descriptor.validation_code_hash(),
		persisted_validation_data_hash: receipt.descriptor.persisted_validation_data_hash(),
		erasure_root: receipt.descriptor.erasure_root(),
		commitments_hash: receipt.commitments_hash,
		output_head_data_hash: receipt.descriptor.para_head(),
		pov: pov.clone(),
		parent_head_data,
	}
}

/// Wrap built entries into the `Segment` arm matching the descriptor version.
/// Test-only: panics on shape violations instead of returning errors.
fn segment_from_entries(
	entries: Vec<SegmentEntry>,
	candidates_descriptor_version: CandidateDescriptorVersion,
	scheduling_parent: Hash,
	scheduling_session: SessionIndex,
) -> Segment {
	match candidates_descriptor_version {
		CandidateDescriptorVersion::V2 => {
			let [entry] = entries.try_into().expect("V2 test segments have exactly one entry");
			Segment::V2(entry)
		},
		_ => Segment::V3 {
			scheduling_parent,
			scheduling_session,
			candidates: BoundedVec::try_from(entries).expect("test segments fit MAX_SEGMENT_LEN"),
		},
	}
}

/// Result of [`distribute_collation`]
struct DistributedCollation {
	candidate: CandidateReceipt,
	pov_block: PoV,
}

// Check that the next received message is a connection request to validators.
async fn check_connected_to_validators(
	virtual_overseer: &mut VirtualOverseer,
	expected_connected: Vec<AuthorityDiscoveryId>,
) {
	// First drain any pending runtime API requests (like NodeFeatures, CandidateEvents, ClaimQueue)
	loop {
		match overseer_recv(virtual_overseer).await {
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::NodeFeatures(_, tx),
			)) => {
				tx.send(Ok(NodeFeatures::EMPTY)).unwrap();
			},
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::CandidateEvents(tx),
			)) => {
				tx.send(Ok(Vec::new())).unwrap();
			},
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::ClaimQueue(tx),
			)) => {
				tx.send(Ok(Default::default())).unwrap();
			},
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::SessionIndexForChild(tx),
			)) => {
				tx.send(Ok(Default::default())).unwrap();
			},
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ConnectToValidators {
				validator_ids,
				peer_set: _,
				failed: _,
			}) => {
				assert_eq!(validator_ids.len(), expected_connected.len());
				for validator in expected_connected.iter() {
					assert!(validator_ids.contains(validator));
				}
				break;
			},
			other => panic!("Unexpected message received: {:?}", other),
		}
	}
}

// Expect that the next received messages are the ones necessary to determine the validator group.
async fn expect_determine_validator_group(
	virtual_overseer: &mut VirtualOverseer,
	test_state: &TestState,
) {
	// We don't know precisely what is going to come as session info might be cached:
	loop {
		match overseer_recv(virtual_overseer).await {
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				relay_parent,
				RuntimeApiRequest::SessionIndexForChild(tx),
			)) => {
				assert_eq!(relay_parent, relay_parent);
				tx.send(Ok(test_state.current_session_index())).unwrap();
			},

			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				relay_parent,
				RuntimeApiRequest::SessionInfo(index, tx),
			)) => {
				assert_eq!(relay_parent, relay_parent);
				assert_eq!(index, test_state.current_session_index());

				tx.send(Ok(Some(test_state.session_info.clone()))).unwrap();
			},

			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::NodeFeatures(_, si_tx),
			)) => {
				si_tx.send(Ok(NodeFeatures::EMPTY)).unwrap();
			},
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::ApprovalVotingParams(_, tx),
			)) => {
				tx.send(Ok(ApprovalVotingParams::default())).unwrap();
			},
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_relay_parent,
				RuntimeApiRequest::ValidatorGroups(tx),
			)) => {
				tx.send(Ok((
					test_state.session_info.validator_groups.to_vec(),
					test_state.group_rotation_info.clone(),
				)))
				.unwrap();
				// This call is mandatory - we are done:
				break;
			},
			other => panic!("Unexpected message received: {:?}", other),
		}
	}
}

/// Send a `DistributeSegment` message from pre-built receipts.
///
/// Returns the collations that validators are expected to fetch.
async fn distribute_segment_with_receipts(
	virtual_overseer: &mut VirtualOverseer,
	expected_connected: Vec<AuthorityDiscoveryId>,
	items: Vec<(CandidateReceipt, PoV, Hash)>,
	scheduling_parent: Hash,
	core_index: CoreIndex,
	candidates_descriptor_version: CandidateDescriptorVersion,
	para_id: ParaId,
	scheduling_session: SessionIndex,
) -> Vec<DistributedCollation> {
	let candidates: Vec<SegmentEntry> = items
		.iter()
		.map(|(receipt, pov, _)| segment_entry_from_receipt(receipt, pov, HeadData(vec![1, 2, 3])))
		.collect();
	let segment = segment_from_entries(
		candidates,
		candidates_descriptor_version,
		scheduling_parent,
		scheduling_session,
	);
	overseer_send(
		virtual_overseer,
		CollatorProtocolMessage::DistributeSegment { core_index, para_id, segment },
	)
	.await;
	check_connected_to_validators(virtual_overseer, expected_connected).await;
	items
		.into_iter()
		.map(|(candidate, pov_block, _)| DistributedCollation { candidate, pov_block })
		.collect()
}

/// Build and distribute a V3 segment for the given scheduling parent and core.
async fn distribute_segment(
	virtual_overseer: &mut VirtualOverseer,
	expected_connected: Vec<AuthorityDiscoveryId>,
	test_state: &TestState,
	relay_parent: Hash,
	scheduling_parent: Hash,
	core_index: CoreIndex,
	len: usize,
) -> Vec<DistributedCollation> {
	let mut items = vec![];
	for i in 0..len {
		let increaser = i as u8;
		let pov_block = PoV { block_data: BlockData(vec![increaser, 42, increaser + 3]) };

		let pov_hash = pov_block.hash();
		let parent_head_data_hash = Hash::zero();
		let candidate = TestCandidateBuilder {
			para_id: test_state.para_id,
			pov_hash,
			relay_parent,
			core_index,
			scheduling_parent,
			para_head: Hash::from_low_u64_be(((core_index.0 as u64) << 32) | (i as u64 + 1)),
			..Default::default()
		}
		.build_v3();
		items.push((candidate, pov_block, parent_head_data_hash));
	}
	let scheduling_session = items[0].0.descriptor().scheduling_session().unwrap();
	distribute_segment_with_receipts(
		virtual_overseer,
		expected_connected,
		items,
		scheduling_parent,
		core_index,
		CandidateDescriptorVersion::V3,
		test_state.para_id,
		scheduling_session,
	)
	.await
}

/// Connect a peer
async fn connect_peer(
	virtual_overseer: &mut VirtualOverseer,
	peer: PeerId,
	version: CollationVersion,
	authority_id: Option<AuthorityDiscoveryId>,
) {
	overseer_send(
		virtual_overseer,
		CollatorProtocolMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerConnected(
			peer,
			polkadot_node_network_protocol::ObservedRole::Authority,
			version.into(),
			authority_id.map(|v| HashSet::from([v])),
		)),
	)
	.await;

	if version != CollationVersion::V1 {
		overseer_send(
			virtual_overseer,
			CollatorProtocolMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerViewChange(
				peer,
				view![],
			)),
		)
		.await;
	}
}

/// Disconnect a peer
async fn disconnect_peer(virtual_overseer: &mut VirtualOverseer, peer: PeerId) {
	overseer_send(
		virtual_overseer,
		CollatorProtocolMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerDisconnected(peer)),
	)
	.await;
}

/// Check that the next received message is a collation advertisement message.
async fn expect_advertise_collation_msg(
	virtual_overseer: &mut VirtualOverseer,
	any_peers: &[PeerId],
	expected_scheduling_parent: Hash,
	mut expected_candidate_hashes: Vec<CandidateHash>,
) {
	let iter_num = expected_candidate_hashes.len();

	for _ in 0..iter_num {
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::NetworkBridgeTx(
				NetworkBridgeTxMessage::SendCollationMessage(
					to,
					wire_message,
				)
			) => {
				assert!(any_peers.iter().any(|p| to.contains(p)));
				match wire_message {
					CollationProtocols::V2(protocol_v2::CollationProtocol::CollatorProtocol(
						wire_message,
					)) => {
						assert_matches!(
							wire_message,
							protocol_v2::CollatorProtocolMessage::AdvertiseCollation {
								scheduling_parent,
								candidate_hash,
								..
							} => {
								assert_eq!(scheduling_parent, expected_scheduling_parent);
								assert!(expected_candidate_hashes.contains(&candidate_hash));

								// Drop the hash we've already seen.
								if let Some(pos) = expected_candidate_hashes.iter().position(|h| h == &candidate_hash) {
									expected_candidate_hashes.remove(pos);
								}
							}
						);
					},
					_ => panic!("Invalid advertisement"),
				}
			}
		);
	}
}

// Check that the next received message is a segment advertisement message.
async fn expect_advertise_segment_msg(
	virtual_overseer: &mut VirtualOverseer,
	any_peers: &[PeerId],
	expected_scheduling_parent: Hash,
	expected_output_heads: Vec<Hash>,
) {
	assert_matches!(overseer_recv(virtual_overseer).await, AllMessages::NetworkBridgeTx(
		NetworkBridgeTxMessage::SendCollationMessage(to, wire_message)
	) => {
		assert!(any_peers.iter().any(|p| to.contains(p)));
		match wire_message {
			CollationProtocols::V4(message) => {
				assert_matches!(message, protocol_v4::AdvertiseSegment { scheduling_parent, candidates, .. } => {
					assert_eq!(scheduling_parent, expected_scheduling_parent);
					assert_eq!(candidates.len(), expected_output_heads.len());
					for (fingerprint, expected) in candidates.iter().zip(expected_output_heads.iter()) {
						assert_eq!(fingerprint.output_head_data_hash, *expected);
					}
				})
			},
			_ => panic!("Invalid advertisement"),
		}
	});
}

/// Send a message that the given peer's view changed.
async fn send_peer_view_change(
	virtual_overseer: &mut VirtualOverseer,
	peer: &PeerId,
	hashes: Vec<Hash>,
) {
	overseer_send(
		virtual_overseer,
		CollatorProtocolMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerViewChange(
			*peer,
			View::new(hashes, 0),
		)),
	)
	.await;
}

fn decode_collation_response(bytes: &[u8]) -> (CandidateReceipt, PoV) {
	let response: CollationFetchingResponse =
		CollationFetchingResponse::decode(&mut &bytes[..]).expect("Decoding should work");
	match response {
		CollationFetchingResponse::Collation(_, _) => {
			panic!("Expected to always receive CollationWithParentHeadData")
		},
		CollationFetchingResponse::CollationWithParentHeadData { receipt, pov, .. } => {
			(receipt, pov)
		},
	}
}

fn decode_collation_response_v3(bytes: &[u8]) -> (CandidateReceipt, PoV, HeadData) {
	let response = request_v3::CollationFetchingResponse::decode(&mut &bytes[..])
		.expect("Decoding should work");
	match response {
		request_v3::CollationFetchingResponse::Collation { receipt, pov, parent_head_data } => {
			(receipt, pov, parent_head_data)
		},
	}
}

// Test that connecting on v1 results in disconnect.
#[test]
fn v1_protocol_rejected() {
	let test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|mut test_harness| async move {
			let virtual_overseer = &mut test_harness.virtual_overseer;
			overseer_send(virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups).await;

			overseer_send(virtual_overseer, CollatorProtocolMessage::CollateOn(test_state.para_id))
				.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			let _ = distribute_segment(
				virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;

			for (val, peer) in test_state
				.current_group_validator_authority_ids()
				.into_iter()
				.zip(test_state.current_group_validator_peer_ids())
			{
				connect_peer(virtual_overseer, peer, CollationVersion::V1, Some(val.clone())).await;

				assert_matches!(
					overseer_recv(virtual_overseer).await,
					AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::DisconnectPeers(bad_peers, peer_set)) => {
						assert_eq!(peer_set, PeerSet::Collation);
						assert_eq!(bad_peers, vec![peer]);
					}
				);
			}

			test_harness
		},
	);
}

/// A V4 peer is advertised a segment and fetches a NON-TIP entry by output head
/// via the v3 request/response protocol. Non-tip entries are unreachable via the
/// hash-keyed v2 protocol once candidates are built on demand, so this is the
/// core capability the v3 protocol exists for.
#[test]
fn v3_fetch_by_output_head_is_served() {
	let test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|test_harness| async move {
			let mut virtual_overseer = test_harness.virtual_overseer;
			let req_v2_cfg = test_harness.req_v2_cfg;
			let mut req_v3_cfg = test_harness.req_v3_cfg;

			overseer_send(&mut virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups)
				.await;

			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::CollateOn(test_state.para_id),
			)
			.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				&mut virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			let distributed_collations = distribute_segment(
				&mut virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				3,
			)
			.await;

			for (val, peer) in test_state
				.current_group_validator_authority_ids()
				.into_iter()
				.zip(test_state.current_group_validator_peer_ids())
			{
				connect_peer(&mut virtual_overseer, peer, CollationVersion::V4, Some(val.clone()))
					.await;
			}

			// V4 collators don't send a `Declare` message: the para is carried in the
			// segment advertisement itself.

			let peer = test_state.current_group_validator_peer_ids()[0];

			send_peer_view_change(&mut virtual_overseer, &peer, vec![test_state.scheduling_parent])
				.await;

			let expected_output_heads: Vec<Hash> = distributed_collations
				.iter()
				.map(|d| d.candidate.descriptor.para_head())
				.collect();
			expect_advertise_segment_msg(
				&mut virtual_overseer,
				&[peer],
				test_state.scheduling_parent,
				expected_output_heads,
			)
			.await;

			// Fetch the OLDEST (non-tip) entry of the segment by its output head.
			let candidate = &distributed_collations[0].candidate;
			let pov_block = &distributed_collations[0].pov_block;

			let (pending_response, rx) = oneshot::channel();
			req_v3_cfg
				.inbound_queue
				.as_mut()
				.unwrap()
				.send(RawIncomingRequest {
					peer,
					payload: request_v3::CollationFetchingRequest {
						scheduling_parent: test_state.scheduling_parent,
						para_id: test_state.para_id,
						output_head_data_hash: candidate.descriptor.para_head(),
					}
					.encode(),
					pending_response,
				})
				.await
				.unwrap();

			assert_matches!(
					rx.await,
					Ok(full_response) => {
							let (receipt, pov, parent_head_data) = decode_collation_response_v3(
									full_response.result.expect("We should have a proper answer").as_ref(),
							);
							assert_eq!(receipt, *candidate);
							assert_eq!(pov, *pov_block);
							assert_eq!(parent_head_data, HeadData(vec![1, 2, 3]));
					}
			);

			TestHarness { virtual_overseer, req_v2_cfg, req_v3_cfg }
		},
	);
}

/// A v3 fetch request for an output head we never distributed is dropped without
/// a response (no decline variant by design): the requester observes the request
/// channel closing, times out, and applies its slash on the requesting side.
#[test]
fn v3_fetch_unknown_output_head_is_not_answered() {
	let test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|test_harness| async move {
			let mut virtual_overseer = test_harness.virtual_overseer;
			let req_v2_cfg = test_harness.req_v2_cfg;
			let mut req_v3_cfg = test_harness.req_v3_cfg;

			overseer_send(&mut virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups)
				.await;

			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::CollateOn(test_state.para_id),
			)
			.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				&mut virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			// Distribute something so the scheduling parent is known and populated —
			// the request must fail on the unknown output head, not on an unknown SP.
			let _distributed_collations = distribute_segment(
				&mut virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;

			let peer = test_state.current_group_validator_peer_ids()[0];
			let (pending_response, rx) = oneshot::channel();
			req_v3_cfg
				.inbound_queue
				.as_mut()
				.unwrap()
				.send(RawIncomingRequest {
					peer,
					payload: request_v3::CollationFetchingRequest {
						scheduling_parent: test_state.scheduling_parent,
						para_id: test_state.para_id,
						output_head_data_hash: Hash::random(),
					}
					.encode(),
					pending_response,
				})
				.await
				.unwrap();

			// The request is dropped without a response.
			rx.await.unwrap_err();

			TestHarness { virtual_overseer, req_v2_cfg, req_v3_cfg }
		},
	);
}

#[test]
fn advertise_and_send_collation() {
	let mut test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|test_harness| async move {
			let mut virtual_overseer = test_harness.virtual_overseer;
			let mut req_v2_cfg = test_harness.req_v2_cfg;
			let req_v3_cfg = test_harness.req_v3_cfg;

			overseer_send(&mut virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups)
				.await;

			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::CollateOn(test_state.para_id),
			)
			.await;
			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				&mut virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			let distributed_collations = distribute_segment(
				&mut virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;
			let candidate = &distributed_collations[0].candidate;
			let pov_block = &distributed_collations[0].pov_block;

			for (val, peer) in test_state
				.current_group_validator_authority_ids()
				.into_iter()
				.zip(test_state.current_group_validator_peer_ids())
			{
				connect_peer(&mut virtual_overseer, peer, CollationVersion::V2, Some(val.clone()))
					.await;
			}

			// We declare to the connected validators that we are a collator.
			// We need to catch all `Declare` messages to the validators we've
			// previously connected to.
			for peer_id in test_state.current_group_validator_peer_ids() {
				expect_declare_msg(&mut virtual_overseer, &test_state, &peer_id).await;
			}

			let peer = test_state.current_group_validator_peer_ids()[0];

			// Send info about peer's view.
			send_peer_view_change(&mut virtual_overseer, &peer, vec![test_state.scheduling_parent])
				.await;

			// The peer is interested in a leaf that we have a collation for;
			// advertise it.
			expect_advertise_collation_msg(
				&mut virtual_overseer,
				&[peer],
				test_state.scheduling_parent,
				vec![candidate.hash()],
			)
			.await;

			// Request a collation.
			let (pending_response, rx) = oneshot::channel();
			req_v2_cfg
				.inbound_queue
				.as_mut()
				.unwrap()
				.send(RawIncomingRequest {
					peer,
					payload: CollationFetchingRequest {
						scheduling_parent: test_state.scheduling_parent,
						candidate_hash: candidate.hash(),
						para_id: test_state.para_id,
					}
					.encode(),
					pending_response,
				})
				.await
				.unwrap();
			// Second request by same validator should get dropped and peer reported:
			{
				let (pending_response, rx) = oneshot::channel();

				req_v2_cfg
					.inbound_queue
					.as_mut()
					.unwrap()
					.send(RawIncomingRequest {
						peer,
						payload: CollationFetchingRequest {
							scheduling_parent: test_state.scheduling_parent,
							candidate_hash: candidate.hash(),
							para_id: test_state.para_id,
						}
						.encode(),
						pending_response,
					})
					.await
					.unwrap();
				assert_matches!(
					overseer_recv(&mut virtual_overseer).await,
					AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(bad_peer, _))) => {
						assert_eq!(bad_peer, peer);
					}
				);
				assert_matches!(
					rx.await,
					Err(_),
					"Multiple concurrent requests by the same validator should get dropped."
				);
			}

			assert_matches!(
				rx.await,
				Ok(full_response) => {
					let (receipt, pov) = decode_collation_response(
						full_response.result
						.expect("We should have a proper answer").as_ref()
					);
					assert_eq!(receipt, *candidate);
					assert_eq!(pov, *pov_block);
				}
			);

			let old_scheduling_parent = test_state.scheduling_parent;
			test_state.scheduling_parent.randomize();

			// Update our view, making the old relay parent go out of the implicit view.
			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				&mut virtual_overseer,
				vec![(test_state.scheduling_parent, 20)],
				1,
			)
			.await;

			let peer = test_state.validator_peer_id[2];

			// Re-request the collation.
			let (pending_response, rx) = oneshot::channel();

			req_v2_cfg
				.inbound_queue
				.as_mut()
				.unwrap()
				.send(RawIncomingRequest {
					peer,
					payload: CollationFetchingRequest {
						scheduling_parent: old_scheduling_parent,
						candidate_hash: candidate.hash(),
						para_id: test_state.para_id,
					}
					.encode(),
					pending_response,
				})
				.await
				.unwrap();
			// Re-requesting collation should fail, becasue the relay parent is out of the view.
			rx.await.unwrap_err();

			assert!(overseer_recv_with_timeout(&mut virtual_overseer, TIMEOUT).await.is_none());

			let distributed_collations = distribute_segment(
				&mut virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;
			let candidate = &distributed_collations[0].candidate;

			// Send info about peer's view.
			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerViewChange(
					peer,
					view![test_state.scheduling_parent],
				)),
			)
			.await;

			expect_advertise_collation_msg(
				&mut virtual_overseer,
				&[peer],
				test_state.scheduling_parent,
				vec![candidate.hash()],
			)
			.await;
			TestHarness { virtual_overseer, req_v2_cfg, req_v3_cfg }
		},
	);
}

#[test]
fn delay_reputation_change() {
	let test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| false),
		|test_harness| async move {
			let mut virtual_overseer = test_harness.virtual_overseer;
			let mut req_v2_cfg = test_harness.req_v2_cfg;
			let req_v3_cfg = test_harness.req_v3_cfg;

			overseer_send(&mut virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups)
				.await;

			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::CollateOn(test_state.para_id),
			)
			.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				&mut virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			let distributed_collations = distribute_segment(
				&mut virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;
			let candidate = &distributed_collations[0].candidate;

			for (val, peer) in test_state
				.current_group_validator_authority_ids()
				.into_iter()
				.zip(test_state.current_group_validator_peer_ids())
			{
				connect_peer(&mut virtual_overseer, peer, CollationVersion::V2, Some(val.clone()))
					.await;
			}

			// We declare to the connected validators that we are a collator.
			// We need to catch all `Declare` messages to the validators we've
			// previously connected to.
			for peer_id in test_state.current_group_validator_peer_ids() {
				expect_declare_msg(&mut virtual_overseer, &test_state, &peer_id).await;
			}

			let peer = test_state.current_group_validator_peer_ids()[0];

			// Send info about peer's view.
			send_peer_view_change(&mut virtual_overseer, &peer, vec![test_state.scheduling_parent])
				.await;

			// The peer is interested in a leaf that we have a collation for;
			// advertise it.
			expect_advertise_collation_msg(
				&mut virtual_overseer,
				&[peer],
				test_state.scheduling_parent,
				vec![candidate.hash()],
			)
			.await;

			// Request a collation.
			let (pending_response, _rx) = oneshot::channel();
			req_v2_cfg
				.inbound_queue
				.as_mut()
				.unwrap()
				.send(RawIncomingRequest {
					peer,
					payload: CollationFetchingRequest {
						scheduling_parent: test_state.scheduling_parent,
						para_id: test_state.para_id,
						candidate_hash: candidate.hash(),
					}
					.encode(),
					pending_response,
				})
				.await
				.unwrap();
			// Second request by same validator should get dropped and peer reported:
			{
				let (pending_response, _rx) = oneshot::channel();

				req_v2_cfg
					.inbound_queue
					.as_mut()
					.unwrap()
					.send(RawIncomingRequest {
						peer,
						payload: CollationFetchingRequest {
							scheduling_parent: test_state.scheduling_parent,
							para_id: test_state.para_id,
							candidate_hash: candidate.hash(),
						}
						.encode(),
						pending_response,
					})
					.await
					.unwrap();

				// Wait enough to fire reputation delay
				futures_timer::Delay::new(REPUTATION_CHANGE_TEST_INTERVAL).await;

				assert_matches!(
					overseer_recv(&mut virtual_overseer).await,
					AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Batch(v))) => {
						let mut expected_change = HashMap::new();
						for rep in vec![COST_APPARENT_FLOOD] {
							add_reputation(&mut expected_change, peer, rep);
						}
						assert_eq!(v, expected_change);
					}
				);
			}

			TestHarness { virtual_overseer, req_v2_cfg, req_v3_cfg }
		},
	);
}

#[test]
#[allow(clippy::async_yields_async)]
fn send_only_one_collation_per_scheduling_parent_at_a_time() {
	test_validator_send_sequence(|mut second_response_receiver, feedback_first_tx| async move {
		Delay::new(Duration::from_millis(100)).await;
		assert!(
			second_response_receiver.try_recv().unwrap().is_none(),
			"We should not have send the collation yet to the second validator",
		);

		// Signal that the collation fetch is finished
		feedback_first_tx.send(()).expect("Sending collation fetch finished");
		second_response_receiver
	});
}

#[test]
#[allow(clippy::async_yields_async)]
fn send_next_collation_after_max_unshared_upload_time() {
	test_validator_send_sequence(|second_response_receiver, _| async move {
		Delay::new(MAX_UNSHARED_UPLOAD_TIME + Duration::from_millis(50)).await;
		second_response_receiver
	});
}

#[test]
fn collators_declare_to_connected_peers() {
	let test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|mut test_harness| async move {
			let peer = test_state.validator_peer_id[0];
			let validator_id = test_state.current_group_validator_authority_ids()[0].clone();

			overseer_send(
				&mut test_harness.virtual_overseer,
				CollatorProtocolMessage::ConnectToBackingGroups,
			)
			.await;

			overseer_send(
				&mut test_harness.virtual_overseer,
				CollatorProtocolMessage::CollateOn(test_state.para_id),
			)
			.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				&mut test_harness.virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			// A validator connected to us
			connect_peer(
				&mut test_harness.virtual_overseer,
				peer,
				CollationVersion::V2,
				Some(validator_id),
			)
			.await;
			expect_declare_msg(&mut test_harness.virtual_overseer, &test_state, &peer).await;
			test_harness
		},
	)
}

#[test]
fn collations_are_only_advertised_to_validators_with_correct_view() {
	let test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|mut test_harness| async move {
			let virtual_overseer = &mut test_harness.virtual_overseer;

			let peer = test_state.current_group_validator_peer_ids()[0];
			let validator_id = test_state.current_group_validator_authority_ids()[0].clone();

			let peer2 = test_state.current_group_validator_peer_ids()[1];
			let validator_id2 = test_state.current_group_validator_authority_ids()[1].clone();

			overseer_send(virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups).await;

			overseer_send(virtual_overseer, CollatorProtocolMessage::CollateOn(test_state.para_id))
				.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			// A validator connected to us
			connect_peer(virtual_overseer, peer, CollationVersion::V2, Some(validator_id)).await;

			// Connect the second validator
			connect_peer(virtual_overseer, peer2, CollationVersion::V2, Some(validator_id2)).await;

			expect_declare_msg(virtual_overseer, &test_state, &peer).await;
			expect_declare_msg(virtual_overseer, &test_state, &peer2).await;

			// And let it tell us that it is has the same view.
			send_peer_view_change(virtual_overseer, &peer2, vec![test_state.scheduling_parent])
				.await;

			let distributed_collations = distribute_segment(
				virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;
			let candidate = &distributed_collations[0].candidate;

			expect_advertise_collation_msg(
				virtual_overseer,
				&[peer2],
				test_state.scheduling_parent,
				vec![candidate.hash()],
			)
			.await;

			// The other validator announces that it changed its view.
			send_peer_view_change(virtual_overseer, &peer, vec![test_state.scheduling_parent])
				.await;

			// After changing the view we should receive the advertisement
			expect_advertise_collation_msg(
				virtual_overseer,
				&[peer],
				test_state.scheduling_parent,
				vec![candidate.hash()],
			)
			.await;
			test_harness
		},
	)
}

#[test]
fn collate_on_two_different_relay_chain_blocks() {
	let mut test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|mut test_harness| async move {
			let virtual_overseer = &mut test_harness.virtual_overseer;

			let peer = test_state.current_group_validator_peer_ids()[0];
			let validator_id = test_state.current_group_validator_authority_ids()[0].clone();

			let peer2 = test_state.current_group_validator_peer_ids()[1];
			let validator_id2 = test_state.current_group_validator_authority_ids()[1].clone();

			overseer_send(virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups).await;

			overseer_send(virtual_overseer, CollatorProtocolMessage::CollateOn(test_state.para_id))
				.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			// A validator connected to us
			connect_peer(virtual_overseer, peer, CollationVersion::V2, Some(validator_id)).await;

			// Connect the second validator
			connect_peer(virtual_overseer, peer2, CollationVersion::V2, Some(validator_id2)).await;

			expect_declare_msg(virtual_overseer, &test_state, &peer).await;
			expect_declare_msg(virtual_overseer, &test_state, &peer2).await;

			let distributed_collations = distribute_segment(
				virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;
			let old_candidate = &distributed_collations[0].candidate;

			let old_scheduling_parent = test_state.scheduling_parent;

			// Update our view, informing the subsystem that the old and the new relay
			// parent are active.
			test_state.scheduling_parent.randomize();
			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				virtual_overseer,
				vec![(old_scheduling_parent, 10), (test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			let distributed_collations = distribute_segment(
				virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;
			let new_candidate = &distributed_collations[0].candidate;

			send_peer_view_change(virtual_overseer, &peer, vec![old_scheduling_parent]).await;
			expect_advertise_collation_msg(
				virtual_overseer,
				&[peer],
				old_scheduling_parent,
				vec![old_candidate.hash()],
			)
			.await;

			send_peer_view_change(virtual_overseer, &peer2, vec![test_state.scheduling_parent])
				.await;

			expect_advertise_collation_msg(
				virtual_overseer,
				&[peer2],
				test_state.scheduling_parent,
				vec![new_candidate.hash()],
			)
			.await;
			test_harness
		},
	)
}

/// Test that when a validator disconnects and reconnects, we re-advertise collations to them.
///
/// This handles the race condition where we send an advertisement, but the peer disconnects
/// before receiving it. When they reconnect, we need to re-advertise so they can fetch
/// the collation.
#[rstest]
#[case::v2_len_one(CollationVersion::V2, 1)]
#[case::v4_len_three(CollationVersion::V4, 3)]
#[case::v4_len_three(CollationVersion::V4, 1)]
fn validator_reconnect_readvertises_segment(
	#[case] peer_version: CollationVersion,
	#[case] segment_len: usize,
) {
	let test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|mut test_harness| async move {
			let virtual_overseer = &mut test_harness.virtual_overseer;

			let peer = test_state.current_group_validator_peer_ids()[0];
			let validator_id = test_state.current_group_validator_authority_ids()[0].clone();

			overseer_send(virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups).await;

			overseer_send(virtual_overseer, CollatorProtocolMessage::CollateOn(test_state.para_id))
				.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			// A validator connected to us
			connect_peer(virtual_overseer, peer, peer_version, Some(validator_id.clone())).await;
			if peer_version != CollationVersion::V4 {
				// V4 has no `Declare` handshake.
				expect_declare_msg(virtual_overseer, &test_state, &peer).await;
			}

			let distributed = distribute_segment(
				virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				segment_len,
			)
			.await;
			let candidate_hashes: Vec<_> = distributed.iter().map(|d| d.candidate.hash()).collect();
			let output_heads: Vec<_> =
				distributed.iter().map(|d| d.candidate.descriptor.para_head()).collect();

			send_peer_view_change(virtual_overseer, &peer, vec![test_state.scheduling_parent])
				.await;
			match peer_version {
				CollationVersion::V2 => {
					expect_advertise_collation_msg(
						virtual_overseer,
						&[peer],
						test_state.scheduling_parent,
						candidate_hashes.clone(),
					)
					.await;
				},
				CollationVersion::V4 => {
					expect_advertise_segment_msg(
						virtual_overseer,
						&[peer],
						test_state.scheduling_parent,
						output_heads.clone(),
					)
					.await;
				},
				_ => panic!("unsupported version in test"),
			}

			// Disconnect - this resets the advertised_to bit for this peer
			disconnect_peer(virtual_overseer, peer).await;

			// Reconnect the same validator
			connect_peer(virtual_overseer, peer, peer_version, Some(validator_id)).await;
			if peer_version != CollationVersion::V4 {
				// V4 has no `Declare` handshake.
				expect_declare_msg(virtual_overseer, &test_state, &peer).await;
			}

			// Send view change - since the advertised_to bit was reset on disconnect,
			// the collation should be re-advertised
			send_peer_view_change(virtual_overseer, &peer, vec![test_state.scheduling_parent])
				.await;

			// We should receive the advertisement again after reconnect
			match peer_version {
				CollationVersion::V2 => {
					expect_advertise_collation_msg(
						virtual_overseer,
						&[peer],
						test_state.scheduling_parent,
						candidate_hashes.clone(),
					)
					.await;
				},
				CollationVersion::V4 => {
					expect_advertise_segment_msg(
						virtual_overseer,
						&[peer],
						test_state.scheduling_parent,
						output_heads.clone(),
					)
					.await;
				},
				_ => panic!("unsupported version in test"),
			}

			test_harness
		},
	)
}

/// A collator serving a mixed-version validator group: the V2 peer gets a `Declare`
/// handshake and a per-candidate advertisement, the V4 peer gets no `Declare` and a
/// segment advertisement carrying the para id. This is the steady state of a rollout
/// network where legacy validators cap negotiation at V3 while experimental ones
/// speak V4.
#[test]
fn mixed_version_peers_declare_and_advertise_per_version() {
	let test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|mut test_harness| async move {
			let virtual_overseer = &mut test_harness.virtual_overseer;

			let peer_v2 = test_state.current_group_validator_peer_ids()[0];
			let peer_v4 = test_state.current_group_validator_peer_ids()[1];
			let validator_id_v2 = test_state.current_group_validator_authority_ids()[0].clone();
			let validator_id_v4 = test_state.current_group_validator_authority_ids()[1].clone();

			overseer_send(virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups).await;

			overseer_send(virtual_overseer, CollatorProtocolMessage::CollateOn(test_state.para_id))
				.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			// The V2 peer is greeted with a `Declare`.
			connect_peer(virtual_overseer, peer_v2, CollationVersion::V2, Some(validator_id_v2))
				.await;
			expect_declare_msg(virtual_overseer, &test_state, &peer_v2).await;

			// The V4 peer gets no `Declare`: its first advertisement carries the para.
			connect_peer(virtual_overseer, peer_v4, CollationVersion::V4, Some(validator_id_v4))
				.await;
			assert!(
				overseer_recv_with_timeout(virtual_overseer, TIMEOUT).await.is_none(),
				"no message should be sent when a V4 validator connects"
			);

			let distributed = distribute_segment(
				virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;
			let candidate_hashes: Vec<_> = distributed.iter().map(|d| d.candidate.hash()).collect();
			let output_heads: Vec<_> =
				distributed.iter().map(|d| d.candidate.descriptor.para_head()).collect();

			// The same distributed segment is advertised per-candidate to the V2 peer...
			send_peer_view_change(virtual_overseer, &peer_v2, vec![test_state.scheduling_parent])
				.await;
			expect_advertise_collation_msg(
				virtual_overseer,
				&[peer_v2],
				test_state.scheduling_parent,
				candidate_hashes,
			)
			.await;

			// ...and as a segment to the V4 peer.
			send_peer_view_change(virtual_overseer, &peer_v4, vec![test_state.scheduling_parent])
				.await;
			expect_advertise_segment_msg(
				virtual_overseer,
				&[peer_v4],
				test_state.scheduling_parent,
				output_heads,
			)
			.await;

			test_harness
		},
	)
}

#[test]
fn collators_reject_declare_messages() {
	let test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();
	let collator_pair2 = CollatorPair::generate().0;

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|mut test_harness| async move {
			let virtual_overseer = &mut test_harness.virtual_overseer;

			let peer = test_state.current_group_validator_peer_ids()[0];
			let validator_id = test_state.current_group_validator_authority_ids()[0].clone();

			overseer_send(virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups).await;

			overseer_send(virtual_overseer, CollatorProtocolMessage::CollateOn(test_state.para_id))
				.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			// A validator connected to us
			connect_peer(virtual_overseer, peer, CollationVersion::V2, Some(validator_id)).await;
			expect_declare_msg(virtual_overseer, &test_state, &peer).await;

			overseer_send(
				virtual_overseer,
				CollatorProtocolMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerMessage(
					peer,
					CollationProtocols::V1(protocol_v1::CollatorProtocolMessage::Declare(
						collator_pair2.public(),
						ParaId::from(5),
						collator_pair2.sign(b"garbage"),
					)),
				)),
			)
			.await;

			assert_matches!(
				overseer_recv(virtual_overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::DisconnectPeers(
					p,
					PeerSet::Collation,
				)) if p == vec![peer]
			);
			test_harness
		},
	)
}

/// Run tests on validator response sequence.
///
/// After the first response is done, the passed in lambda will be called with the receiver for the
/// next response and a sender for giving feedback on the response of the first transmission. After
/// the lambda has passed it is assumed that the second response is sent, which is checked by this
/// function.
///
/// The lambda can trigger occasions on which the second response should be sent, like timeouts,
/// successful completion.
fn test_validator_send_sequence<T, F>(handle_first_response: T)
where
	T: FnOnce(oneshot::Receiver<sc_network::config::OutgoingResponse>, oneshot::Sender<()>) -> F,
	F: Future<Output = oneshot::Receiver<sc_network::config::OutgoingResponse>>,
{
	let test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|mut test_harness| async move {
			let virtual_overseer = &mut test_harness.virtual_overseer;
			let req_cfg = &mut test_harness.req_v2_cfg;

			overseer_send(virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups).await;

			overseer_send(virtual_overseer, CollatorProtocolMessage::CollateOn(test_state.para_id))
				.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			let distributed_collations = distribute_segment(
				virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;
			let candidate = &distributed_collations[0].candidate;
			let pov_block = &distributed_collations[0].pov_block;

			for (val, peer) in test_state
				.current_group_validator_authority_ids()
				.into_iter()
				.zip(test_state.current_group_validator_peer_ids())
			{
				connect_peer(virtual_overseer, peer, CollationVersion::V2, Some(val.clone())).await;
			}

			// We declare to the connected validators that we are a collator.
			// We need to catch all `Declare` messages to the validators we've
			// previously connected to.
			for peer_id in test_state.current_group_validator_peer_ids() {
				expect_declare_msg(virtual_overseer, &test_state, &peer_id).await;
			}

			let validator_0 = test_state.current_group_validator_peer_ids()[0];
			let validator_1 = test_state.current_group_validator_peer_ids()[1];

			// Send info about peer's view.
			send_peer_view_change(
				virtual_overseer,
				&validator_0,
				vec![test_state.scheduling_parent],
			)
			.await;
			send_peer_view_change(
				virtual_overseer,
				&validator_1,
				vec![test_state.scheduling_parent],
			)
			.await;

			// The peer is interested in a leaf that we have a collation for;
			// advertise it.
			expect_advertise_collation_msg(
				virtual_overseer,
				&[validator_0],
				test_state.scheduling_parent,
				vec![candidate.hash()],
			)
			.await;
			expect_advertise_collation_msg(
				virtual_overseer,
				&[validator_1],
				test_state.scheduling_parent,
				vec![candidate.hash()],
			)
			.await;

			// Request a collation.
			let (pending_response, rx) = oneshot::channel();
			req_cfg
				.inbound_queue
				.as_mut()
				.unwrap()
				.send(RawIncomingRequest {
					peer: validator_0,
					payload: CollationFetchingRequest {
						scheduling_parent: test_state.scheduling_parent,
						para_id: test_state.para_id,
						candidate_hash: candidate.hash(),
					}
					.encode(),
					pending_response,
				})
				.await
				.unwrap();

			// Keep the feedback channel alive because we need to use it to inform about the
			// finished transfer.
			let feedback_tx = assert_matches!(
				rx.await,
				Ok(full_response) => {
					let (receipt, pov) = decode_collation_response(
						full_response.result
						.expect("We should have a proper answer").as_ref()
					);
					assert_eq!(receipt, *candidate);
					assert_eq!(pov, *pov_block);

					full_response.sent_feedback.expect("Feedback channel is always set")
				}
			);

			// Let the second validator request the collation.
			let (pending_response, rx) = oneshot::channel();
			req_cfg
				.inbound_queue
				.as_mut()
				.unwrap()
				.send(RawIncomingRequest {
					peer: validator_1,
					payload: CollationFetchingRequest {
						scheduling_parent: test_state.scheduling_parent,
						para_id: test_state.para_id,
						candidate_hash: candidate.hash(),
					}
					.encode(),
					pending_response,
				})
				.await
				.unwrap();

			let rx = handle_first_response(rx, feedback_tx).await;

			// Now we should send it to the second validator
			assert_matches!(
				rx.await,
				Ok(full_response) => {
					let (receipt, pov) = decode_collation_response(
						full_response.result
						.expect("We should have a proper answer").as_ref()
					);
					assert_eq!(receipt, *candidate);
					assert_eq!(pov, *pov_block);

					full_response.sent_feedback.expect("Feedback channel is always set")
				}
			);

			test_harness
		},
	);
}

#[test]
fn connect_to_group_in_view() {
	let mut test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|test_harness| async move {
			let mut virtual_overseer = test_harness.virtual_overseer;
			let mut req_cfg = test_harness.req_v2_cfg;
			let req_cfg_3 = test_harness.req_v3_cfg;

			overseer_send(&mut virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups)
				.await;

			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::CollateOn(test_state.para_id),
			)
			.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				&mut virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			let group_a = test_state.current_group_validator_authority_ids();
			let peers_a = test_state.current_group_validator_peer_ids();
			assert!(group_a.len() > 1);

			let distributed_collations = distribute_segment(
				&mut virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;
			let candidate = &distributed_collations[0].candidate;

			let head_a = test_state.scheduling_parent;

			for (val, peer) in group_a.iter().zip(&peers_a) {
				connect_peer(&mut virtual_overseer, *peer, CollationVersion::V2, Some(val.clone()))
					.await;
			}

			for peer_id in &peers_a {
				expect_declare_msg(&mut virtual_overseer, &test_state, peer_id).await;
			}

			// Update views.
			for peer_id in &peers_a {
				send_peer_view_change(&mut virtual_overseer, peer_id, vec![head_a]).await;
				expect_advertise_collation_msg(
					&mut virtual_overseer,
					&[*peer_id],
					head_a,
					vec![candidate.hash()],
				)
				.await;
			}

			let peer = peers_a[0];
			// Peer from the group fetches the collation.
			let (pending_response, rx) = oneshot::channel();
			req_cfg
				.inbound_queue
				.as_mut()
				.unwrap()
				.send(RawIncomingRequest {
					peer,
					payload: CollationFetchingRequest {
						scheduling_parent: head_a,
						para_id: test_state.para_id,
						candidate_hash: candidate.hash(),
					}
					.encode(),
					pending_response,
				})
				.await
				.unwrap();
			assert_matches!(
				rx.await,
				Ok(full_response) => {
					let _ = decode_collation_response(
						full_response.result
						.expect("We should have a proper answer").as_ref()
					);
				}
			);

			// Let the subsystem process process the collation event.
			test_helpers::Yield::new().await;

			let old_scheduling_parent = test_state.scheduling_parent;
			test_state.scheduling_parent.randomize();

			test_state.group_rotation_info = test_state.group_rotation_info.bump_rotation();

			// Update our view.
			let mut expected_group = group_a.clone();
			expected_group.extend(test_state.current_group_validator_authority_ids());
			update_view(
				Some(expected_group.clone()),
				&test_state,
				&mut virtual_overseer,
				vec![(old_scheduling_parent, 10), (test_state.scheduling_parent, 20)],
				1,
			)
			.await;

			let head_b = test_state.scheduling_parent;
			let group_b = test_state.current_group_validator_authority_ids();
			assert_ne!(head_a, head_b);
			assert_ne!(group_a, group_b);

			let _ = distribute_segment(
				&mut virtual_overseer,
				expected_group,
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;

			TestHarness { virtual_overseer, req_v2_cfg: req_cfg, req_v3_cfg: req_cfg_3 }
		},
	);
}

#[test]
fn connect_with_no_cores_assigned() {
	let mut test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|test_harness| async move {
			let mut virtual_overseer = test_harness.virtual_overseer;
			let req_cfg = test_harness.req_v2_cfg;
			let req_cfg_3 = test_harness.req_v3_cfg;

			overseer_send(&mut virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups)
				.await;

			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::CollateOn(test_state.para_id),
			)
			.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				&mut virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			let group_a = test_state.current_group_validator_authority_ids();
			assert!(group_a.len() > 1);

			let _ = distribute_segment(
				&mut virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;

			// Create a new relay parent and remove the core assignments.
			test_state.scheduling_parent.randomize();
			test_state.claim_queue.clear();

			update_view(
				Some(vec![]),
				&test_state,
				&mut virtual_overseer,
				vec![(test_state.scheduling_parent, 20)],
				1,
			)
			.await;

			// Send the ActiveLeaves signal to trigger the reconnect timeout.
			overseer_signal(
				&mut virtual_overseer,
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(ActivatedLeaf {
					hash: test_state.scheduling_parent,
					number: 20,
					unpin_handle: polkadot_node_subsystem_test_helpers::mock::dummy_unpin_handle(
						test_state.scheduling_parent,
					),
				})),
			)
			.await;

			// The collator should attempt to connect to an empty validator list.
			let timeout = Duration::from_secs(5);
			assert_matches!(
				overseer_recv_with_timeout(&mut virtual_overseer, timeout)
					.await
					.expect(&format!("{:?} is more than `RECONNECT_AFTER_LEAF_TIMEOUT`, which is enough to receive messages", timeout)),
				AllMessages::NetworkBridgeTx(
					NetworkBridgeTxMessage::ConnectToValidators { validator_ids, .. }
				) => {
					assert_eq!(Vec::<AuthorityDiscoveryId>::new(), validator_ids);
				}
			);

			TestHarness { virtual_overseer, req_v2_cfg: req_cfg, req_v3_cfg: req_cfg_3 }
		},
	);
}

#[test]
fn no_connection_without_preconnect_message() {
	let test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|test_harness| async move {
			let mut virtual_overseer = test_harness.virtual_overseer;
			let req_cfg = test_harness.req_v2_cfg;
			let req_cfg_3 = test_harness.req_v3_cfg;

			// NOTE: We intentionally DO NOT send ConnectToBackingGroups here
			// to verify that connections are not made without the pre-connect message.

			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::CollateOn(test_state.para_id),
			)
			.await;

			// Update view without expecting any connections (None parameter)
			update_view(
				None, // No connections should be made
				&test_state,
				&mut virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			// Drain any runtime API requests (like NodeFeatures) but verify no ConnectToValidators
			let timeout = Duration::from_millis(250);
			loop {
				match overseer_recv_with_timeout(&mut virtual_overseer, timeout).await {
					None => {
						// Timeout is fine - no more messages
						break;
					},
					Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
						_,
						RuntimeApiRequest::NodeFeatures(_, tx),
					))) => {
						tx.send(Ok(NodeFeatures::EMPTY)).unwrap();
					},
					Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
						_,
						RuntimeApiRequest::SessionIndexForChild(tx),
					))) => {
						tx.send(Ok(Default::default())).unwrap();
					},
					Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
						_,
						RuntimeApiRequest::CandidateEvents(tx),
					))) => {
						tx.send(Ok(Vec::new())).unwrap();
					},
					Some(AllMessages::NetworkBridgeTx(
						NetworkBridgeTxMessage::ConnectToValidators { .. },
					)) => {
						panic!("Unexpected ConnectToValidators message was sent");
					},
					Some(msg) => {
						panic!("Unexpected message was sent by subsystem: {:?}", msg);
					},
				}
			}

			TestHarness { virtual_overseer, req_v2_cfg: req_cfg, req_v3_cfg: req_cfg_3 }
		},
	);
}

#[test]
fn distribute_collation_forces_connect() {
	let test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|test_harness| async move {
			let mut virtual_overseer = test_harness.virtual_overseer;
			let req_cfg = test_harness.req_v2_cfg;
			let req_cfg_3 = test_harness.req_v3_cfg;

			// NOTE: We intentionally DO NOT send ConnectToBackingGroups here
			// to verify that connections are not made without the pre-connect message.

			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::CollateOn(test_state.para_id),
			)
			.await;

			// Update view without expecting any connections (None parameter)
			update_view(
				None, // No connections should be made
				&test_state,
				&mut virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			// Drain any runtime API requests (like NodeFeatures) but verify no ConnectToValidators
			let timeout = Duration::from_millis(250);
			loop {
				match overseer_recv_with_timeout(&mut virtual_overseer, timeout).await {
					None => {
						// Timeout is fine - no more messages
						break;
					},
					Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
						_,
						RuntimeApiRequest::NodeFeatures(_, tx),
					))) => {
						tx.send(Ok(NodeFeatures::EMPTY)).unwrap();
					},
					Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
						_,
						RuntimeApiRequest::SessionIndexForChild(tx),
					))) => {
						tx.send(Ok(Default::default())).unwrap();
					},
					Some(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
						_,
						RuntimeApiRequest::CandidateEvents(tx),
					))) => {
						tx.send(Ok(Vec::new())).unwrap();
					},
					Some(AllMessages::NetworkBridgeTx(
						NetworkBridgeTxMessage::ConnectToValidators { .. },
					)) => {
						panic!("Unexpected ConnectToValidators message was sent");
					},
					Some(msg) => {
						panic!("Unexpected message was sent by subsystem: {:?}", msg);
					},
				}
			}

			// Distribute a collation
			let _ = distribute_segment(
				&mut virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;

			for (val, peer) in test_state
				.current_group_validator_authority_ids()
				.into_iter()
				.zip(test_state.current_group_validator_peer_ids())
			{
				connect_peer(&mut virtual_overseer, peer, CollationVersion::V2, Some(val.clone()))
					.await;
			}

			// Expect advertisement for the candidate
			expect_declare_msg(
				&mut virtual_overseer,
				&test_state,
				&test_state.current_group_validator_peer_ids()[0],
			)
			.await;

			TestHarness { virtual_overseer, req_v2_cfg: req_cfg, req_v3_cfg: req_cfg_3 }
		},
	);
}

#[test]
fn connect_advertise_disconnect_three_backing_groups() {
	// Create a test state with 3 non-empty backing groups
	let mut test_state = TestState::default();
	let para_id = test_state.para_id;

	// We have 5 validators total (indices 0-4)
	// Group 0: validators [0, 1]
	// Group 1: validators [2, 3]
	// Group 2: validators [4]
	test_state.session_info.validator_groups = vec![
		vec![ValidatorIndex(0), ValidatorIndex(1)],
		vec![ValidatorIndex(2), ValidatorIndex(3)],
		vec![ValidatorIndex(4)],
		vec![],
	]
	.into_iter()
	.collect();

	// Assign our para_id to 3 cores (0, 1, 2) which will map to 3 groups
	test_state.claim_queue.clear();
	test_state.claim_queue.insert(CoreIndex(0), [para_id].into_iter().collect());
	test_state.claim_queue.insert(CoreIndex(1), [para_id].into_iter().collect());
	test_state.claim_queue.insert(CoreIndex(2), [para_id].into_iter().collect());
	test_state.claim_queue.insert(CoreIndex(3), VecDeque::new());

	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|test_harness| async move {
			let mut virtual_overseer = test_harness.virtual_overseer;
			let req_cfg = test_harness.req_v2_cfg;
			let req_cfg_3 = test_harness.req_v3_cfg;

			// Send the pre-connect message
			overseer_send(&mut virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups)
				.await;

			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::CollateOn(test_state.para_id),
			)
			.await;

			// Get validators from all 3 backing groups
			let mut expected_validators = Vec::new();
			for core_idx in [0, 1, 2] {
				let validators = test_state.validator_authority_ids_for_core(CoreIndex(core_idx));
				expected_validators.extend(validators);
			}

			// Remove duplicates while preserving order
			let mut seen = std::collections::HashSet::new();
			expected_validators.retain(|v| seen.insert(v.clone()));

			// Verify we're connecting to all 5 validators (from 3 groups)
			// Group 0 (Core 0): 2 validators
			// Group 1 (Core 1): 2 validators
			// Group 2 (Core 2): 1 validator
			assert_eq!(
				expected_validators.len(),
				5,
				"Expected 5 unique validators from 3 backing groups"
			);

			// Update view and expect connections to all validators from all 3 backing groups
			update_view(
				Some(expected_validators.clone()),
				&test_state,
				&mut virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			// Generate NetworkBridgeEvent::PeerConnected messages for each expected validator peer
			// Use some random peer ids
			let validator_peer_ids: Vec<_> =
				(0..expected_validators.len()).map(|_| PeerId::random()).sorted().collect();

			for (peer_id, auth_id) in validator_peer_ids.iter().zip(expected_validators.iter()) {
				overseer_send(
					&mut virtual_overseer,
					CollatorProtocolMessage::NetworkBridgeUpdate(
						NetworkBridgeEvent::PeerConnected(
							*peer_id,
							ObservedRole::Authority,
							CollationVersion::V2.into(),
							Some(HashSet::from([auth_id.clone()])),
						),
					),
				)
				.await;
			}

			// Expect declare messages for each validator
			for peer_id in validator_peer_ids.iter() {
				expect_declare_msg(&mut virtual_overseer, &test_state, peer_id).await;
			}

			// Distribute collations for first 2 cores
			let mut candidate_hashes = HashMap::new();
			for core_idx in [0, 1] {
				let distributed_collations = distribute_segment(
					&mut virtual_overseer,
					expected_validators.clone(),
					&test_state,
					test_state.scheduling_parent,
					test_state.scheduling_parent,
					CoreIndex(core_idx),
					1,
				)
				.await;
				let candidate = &distributed_collations[0].candidate;

				// Add the same candidate hash twice we remove them once per validator.
				candidate_hashes.insert(core_idx as usize, vec![candidate.hash(); 2]);
			}

			// Send peer view changes for all validators to trigger advertisements
			for peer_id in validator_peer_ids.iter() {
				send_peer_view_change(
					&mut virtual_overseer,
					peer_id,
					vec![test_state.scheduling_parent],
				)
				.await;
			}

			// Expect advertisements for 2 collations to each validator
			for (idx, peer_ids) in
				validator_peer_ids.iter().take(4).chunks(2).into_iter().enumerate()
			{
				let peer_ids_vec: Vec<PeerId> = peer_ids.copied().collect();
				expect_advertise_collation_msg(
					&mut virtual_overseer,
					&peer_ids_vec,
					test_state.scheduling_parent,
					candidate_hashes[&idx].clone(),
				)
				.await;
			}

			// Send the disconnect message
			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::DisconnectFromBackingGroups,
			)
			.await;

			// We should disconnect from validator of core 2, but keep the other validators
			// connected
			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ConnectToValidators{
					mut validator_ids,
					peer_set,
					failed: _,
				}) => {
					let mut expected: Vec<_> = expected_validators.into_iter().take(4).collect();
					validator_ids.sort();
					expected.sort();
					assert_eq!(validator_ids, expected, "Expected to disconnect validator assigned to core 2");
					assert_eq!(peer_set, PeerSet::Collation);
				}
			);

			// Update view and expect connections to all validators to be dropped.
			update_view(
				Some(vec![]),
				&test_state,
				&mut virtual_overseer,
				vec![(Hash::random(), 11)],
				1,
			)
			.await;

			TestHarness { virtual_overseer, req_v2_cfg: req_cfg, req_v3_cfg: req_cfg_3 }
		},
	);
}

/// Test that collations are re-advertised when a peer's authority IDs are updated
/// to match the validator group (when they previously didn't match).
///
/// This tests the fix for a race condition where:
/// 1. A peer connects with authority IDs that don't match the validator group
/// 2. A collation is distributed but can't be advertised (no authority ID match)
/// 3. The peer's view includes the relay parent
/// 4. UpdatedAuthorityIds arrives with new authority IDs that DO match
/// 5. The collation should now be advertised
#[test]
fn readvertise_collation_on_authority_id_update() {
	let test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|mut test_harness| async move {
			let virtual_overseer = &mut test_harness.virtual_overseer;

			let peer = test_state.current_group_validator_peer_ids()[0];
			// Use an unrelated authority ID that doesn't match the validator group
			let unrelated_authority_id: AuthorityDiscoveryId = Sr25519Keyring::Eve.public().into();
			// The actual authority ID that matches the validator group
			let matching_authority_id =
				test_state.current_group_validator_authority_ids()[0].clone();

			overseer_send(virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups).await;

			overseer_send(virtual_overseer, CollatorProtocolMessage::CollateOn(test_state.para_id))
				.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			// Connect peer with an authority ID that does NOT match the validator group
			connect_peer(
				virtual_overseer,
				peer,
				CollationVersion::V2,
				Some(unrelated_authority_id),
			)
			.await;

			// We still declare to the peer (declaration doesn't depend on validator group match)
			expect_declare_msg(virtual_overseer, &test_state, &peer).await;

			// Distribute a collation
			let distributed_collations = distribute_segment(
				virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				1,
			)
			.await;
			let candidate = &distributed_collations[0].candidate;

			// Send peer view change - peer is interested in the relay parent
			send_peer_view_change(virtual_overseer, &peer, vec![test_state.scheduling_parent])
				.await;

			// No advertisement should happen because the peer's authority ID (Eve)
			// doesn't match any validator in the group for this collation
			assert!(
				overseer_recv_with_timeout(virtual_overseer, TIMEOUT).await.is_none(),
				"Should not advertise to peer with non-matching authority ID"
			);

			// Now send UpdatedAuthorityIds with the correct authority ID
			overseer_send(
				virtual_overseer,
				CollatorProtocolMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::UpdatedAuthorityIds(
						peer,
						HashSet::from([matching_authority_id]),
					),
				),
			)
			.await;

			// Now the collation should be advertised because the authority ID matches
			expect_advertise_collation_msg(
				virtual_overseer,
				&[peer],
				test_state.scheduling_parent,
				vec![candidate.hash()],
			)
			.await;

			test_harness
		},
	)
}

/// Test that UpdatedAuthorityIds for existing peers with unchanged matching authority IDs
/// doesn't cause duplicate advertisements (idempotency check).
#[rstest]
#[case::v2_len_one(CollationVersion::V2, 1)]
#[case::v4_len_three(CollationVersion::V4, 3)]
fn no_duplicate_advertisement_on_authority_id_update(
	#[case] peer_version: CollationVersion,
	#[case] segment_len: usize,
) {
	let test_state = TestState::default();
	let local_peer_id = test_state.local_peer_id;
	let collator_pair = test_state.collator_pair.clone();

	test_harness(
		local_peer_id,
		collator_pair,
		ReputationAggregator::new(|_| true),
		|mut test_harness| async move {
			let virtual_overseer = &mut test_harness.virtual_overseer;

			let peer = test_state.current_group_validator_peer_ids()[0];
			let validator_id = test_state.current_group_validator_authority_ids()[0].clone();

			overseer_send(virtual_overseer, CollatorProtocolMessage::ConnectToBackingGroups).await;

			overseer_send(virtual_overseer, CollatorProtocolMessage::CollateOn(test_state.para_id))
				.await;

			update_view(
				Some(test_state.current_group_validator_authority_ids()),
				&test_state,
				virtual_overseer,
				vec![(test_state.scheduling_parent, 10)],
				1,
			)
			.await;

			// Connect peer with matching authority ID
			connect_peer(virtual_overseer, peer, peer_version, Some(validator_id.clone())).await;
			if peer_version != CollationVersion::V4 {
				// V4 has no `Declare` handshake.
				expect_declare_msg(virtual_overseer, &test_state, &peer).await;
			}

			// Distribute a collation
			let distributed = distribute_segment(
				virtual_overseer,
				test_state.current_group_validator_authority_ids(),
				&test_state,
				test_state.scheduling_parent,
				test_state.scheduling_parent,
				CoreIndex(0),
				segment_len,
			)
			.await;
			let candidate_hashes: Vec<_> = distributed.iter().map(|d| d.candidate.hash()).collect();
			let output_heads: Vec<_> =
				distributed.iter().map(|d| d.candidate.descriptor.para_head()).collect();

			// Peer view change triggers advertisement
			send_peer_view_change(virtual_overseer, &peer, vec![test_state.scheduling_parent])
				.await;

			// First advertisement
			match peer_version {
				CollationVersion::V2 => {
					expect_advertise_collation_msg(
						virtual_overseer,
						&[peer],
						test_state.scheduling_parent,
						candidate_hashes.clone(),
					)
					.await;
				},
				CollationVersion::V4 => {
					expect_advertise_segment_msg(
						virtual_overseer,
						&[peer],
						test_state.scheduling_parent,
						output_heads.clone(),
					)
					.await;
				},
				_ => panic!("unsupported version in test"),
			}

			// Send UpdatedAuthorityIds with the same authority ID
			overseer_send(
				virtual_overseer,
				CollatorProtocolMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::UpdatedAuthorityIds(peer, HashSet::from([validator_id])),
				),
			)
			.await;

			// No duplicate advertisement should happen (already advertised to this validator)
			assert!(
				overseer_recv_with_timeout(virtual_overseer, TIMEOUT).await.is_none(),
				"Should not re-advertise to peer that was already advertised to"
			);

			test_harness
		},
	)
}
