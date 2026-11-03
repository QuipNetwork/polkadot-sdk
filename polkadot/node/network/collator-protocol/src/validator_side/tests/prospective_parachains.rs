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

//! Tests for the validator side with enabled prospective parachains.

use super::*;

use polkadot_node_subsystem::messages::ChainApiMessage;
use polkadot_primitives::{
	BlockNumber, CandidateCommitments, CandidateDescriptorVersion, CollatorId, CollatorSignature,
	CommittedCandidateReceiptV2 as CommittedCandidateReceipt, Header, MutateDescriptorV2,
	SigningContext, ValidatorId, RELAY_CHAIN_SLOT_DURATION_MILLIS,
};
use polkadot_primitives_test_helpers::{
	dummy_committed_candidate_receipt_v2, dummy_committed_candidate_receipt_v3,
	make_valid_candidate_descriptor_v3, CandidateDescriptor,
};
use rstest::rstest;
use sp_consensus_babe::digests::{CompatibleDigestItem, PreDigest, SecondaryPlainPreDigest};
use sp_consensus_slots::Slot;
use sp_runtime::DigestItem;

fn get_parent_hash(hash: Hash) -> Hash {
	Hash::from_low_u64_be(hash.to_low_u64_be() + 1)
}

async fn assert_construct_per_relay_parent(
	virtual_overseer: &mut VirtualOverseer,
	test_state: &TestState,
	hash: Hash,
	number: BlockNumber,
	next_msg: &mut Option<AllMessages>,
) {
	let msg = match next_msg.take() {
		Some(msg) => msg,
		None => overseer_recv(virtual_overseer).await,
	};
	assert_matches!(
		msg,
		AllMessages::RuntimeApi(
			RuntimeApiMessage::Request(parent, RuntimeApiRequest::Validators(tx))
		) => {
			assert_eq!(parent, hash);
			tx.send(Ok(test_state.validator_public.clone())).unwrap();
		}
	);

	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::RuntimeApi(
			RuntimeApiMessage::Request(parent, RuntimeApiRequest::ValidatorGroups(tx))
		) if parent == hash => {
			let validator_groups = test_state.validator_groups.clone();
			let mut group_rotation_info = test_state.group_rotation_info.clone();
			group_rotation_info.now = number;
			tx.send(Ok((validator_groups, group_rotation_info))).unwrap();
		}
	);
}

/// Handle a view update.
pub(super) async fn update_view(
	virtual_overseer: &mut VirtualOverseer,
	test_state: &mut TestState,
	new_view: Vec<(Hash, u32)>, // Hash and block number.
) -> Option<AllMessages> {
	update_view_with_slot(virtual_overseer, test_state, new_view, None).await
}

/// Handle a view update, optionally overriding the BABE slot for the leaf header.
/// When `leaf_slot` is `None`, uses the current timestamp to derive the slot.
pub(super) async fn update_view_with_slot(
	virtual_overseer: &mut VirtualOverseer,
	test_state: &mut TestState,
	new_view: Vec<(Hash, u32)>,
	leaf_slot: Option<Slot>,
) -> Option<AllMessages> {
	let last_block_from_view = new_view.last().map(|t| t.1);
	let new_view: HashMap<Hash, u32> = HashMap::from_iter(new_view);
	let our_view = OurView::new(new_view.keys().map(|hash| *hash), 0);

	overseer_send(
		virtual_overseer,
		CollatorProtocolMessage::NetworkBridgeUpdate(NetworkBridgeEvent::OurViewChange(our_view)),
	)
	.await;

	let mut next_overseer_message = None;
	for _ in 0..new_view.len() {
		let msg = match next_overseer_message.take() {
			Some(msg) => msg,
			None => overseer_recv(virtual_overseer).await,
		};

		let (leaf_hash, leaf_number) = assert_matches!(
			msg,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				parent,
				RuntimeApiRequest::SessionIndexForChild(tx)
			)) => {
				tx.send(Ok(test_state.session_index)).unwrap();
				(parent, new_view.get(&parent).copied().expect("Unknown parent requested"))
			}
		);

		// handle_our_view_change fetches the leaf's scheduling lookahead and claim queue together
		// via `LeafClaimQueues::fetch` — the lookahead request comes first.
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				parent,
				RuntimeApiRequest::SchedulingLookahead(_, tx),
			)) if parent == leaf_hash => {
				tx.send(Ok(test_state.scheduling_lookahead)).unwrap();
			}
		);

		// handle_our_view_change fetches claim queue for the leaf
		// (stored in leaf_claim_queues for the new offset-based validation)
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				parent,
				RuntimeApiRequest::ClaimQueue(tx),
			)) if parent == leaf_hash => {
				let _ = tx.send(Ok(test_state.claim_queue.clone()));
			}
		);

		assert_construct_per_relay_parent(
			virtual_overseer,
			test_state,
			leaf_hash,
			leaf_number,
			&mut next_overseer_message,
		)
		.await;

		// Handle BlockHeader request for the leaf (used to extract BABE slot for V3
		// scheduling parent validation).
		let leaf_parent_hash = get_parent_hash(leaf_hash);
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::BlockHeader(hash, tx)) if hash == leaf_hash => {
				let slot = leaf_slot.unwrap_or_else(|| Slot::from_timestamp(
					sp_timestamp::Timestamp::current(),
					sp_consensus_slots::SlotDuration::from_millis(RELAY_CHAIN_SLOT_DURATION_MILLIS),
				));
				let pre_digest = PreDigest::SecondaryPlain(SecondaryPlainPreDigest {
					authority_index: 0,
					slot,
				});
				let header = Header {
					parent_hash: leaf_parent_hash,
					number: leaf_number,
					state_root: Hash::zero(),
					extrinsics_root: Hash::zero(),
					digest: sp_runtime::Digest {
						logs: vec![DigestItem::babe_pre_digest(pre_digest)],
					},
				};
				tx.send(Ok(Some(header))).unwrap();
			}
		);

		// activate_leaf calls fetch_ancestors
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::SessionIndexForChild(tx)
			)) => {
				tx.send(Ok(test_state.session_index)).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::SchedulingLookahead(_, tx)
			)) => {
				tx.send(Ok(test_state.scheduling_lookahead)).unwrap();
			}
		);

		let min_number = leaf_number.saturating_sub(test_state.scheduling_lookahead);
		let ancestry_len = leaf_number + 1 - min_number;
		let ancestry_hashes = std::iter::successors(Some(leaf_hash), |h| Some(get_parent_hash(*h)))
			.take(ancestry_len as usize);

		let returned_ancestors = assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::ChainApi(ChainApiMessage::Ancestors {
				k,
				response_channel: tx,
				..
			}) => {
				assert_eq!(k, test_state.scheduling_lookahead.saturating_sub(1) as usize);
				let hashes: Vec<_> = ancestry_hashes.clone().skip(1).collect();
				let returned = hashes.clone();
				tx.send(Ok(hashes)).unwrap();
				returned
			}
		);

		// fetch_ancestors checks session for each ancestor that was returned
		for _ in 0..returned_ancestors.len() {
			assert_matches!(
				overseer_recv(virtual_overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_,
					RuntimeApiRequest::SessionIndexForChild(tx)
				)) => {
					tx.send(Ok(test_state.session_index)).unwrap();
				}
			);
		}

		let ancestry_numbers = (min_number..=leaf_number).rev();
		let ancestry_iter = ancestry_hashes.clone().zip(ancestry_numbers).peekable();

		// How many blocks were actually requested.
		let mut requested_len: usize = 0;
		{
			let mut ancestry_iter = ancestry_iter.clone();
			while let Some((hash, number)) = ancestry_iter.next() {
				if Some(number) == test_state.last_known_block {
					break;
				}

				// May be `None` for the last element.
				let parent_hash =
					ancestry_iter.peek().map(|(h, _)| *h).unwrap_or_else(|| get_parent_hash(hash));

				let msg = match next_overseer_message.take() {
					Some(msg) => msg,
					None => match overseer_recv_with_timeout(
						virtual_overseer,
						Duration::from_millis(50),
					)
					.await
					{
						Some(msg) => msg,
						None => {
							// No message arrived - ancestry is cached
							break;
						},
					},
				};

				if !matches!(&msg, AllMessages::ChainApi(ChainApiMessage::BlockHeader(..))) {
					// Ancestry has already been cached for this leaf.
					next_overseer_message.replace(msg);
					break;
				}

				assert_matches!(
					msg,
					AllMessages::ChainApi(ChainApiMessage::BlockHeader(.., tx)) => {
						let header = Header {
							parent_hash,
							number,
							state_root: Hash::zero(),
							extrinsics_root: Hash::zero(),
							digest: Default::default(),
						};

						tx.send(Ok(Some(header))).unwrap();
					}
				);

				requested_len += 1;
			}
		}

		// Skip the leaf.
		for (hash, number) in ancestry_iter.skip(1).take(requested_len.saturating_sub(1)) {
			if Some(number) == test_state.last_known_block {
				break;
			}
			assert_construct_per_relay_parent(
				virtual_overseer,
				test_state,
				hash,
				number,
				&mut next_overseer_message,
			)
			.await;
		}
	}

	test_state.last_known_block = last_block_from_view;

	next_overseer_message
}

async fn send_seconded_statement(
	virtual_overseer: &mut VirtualOverseer,
	keystore: KeystorePtr,
	candidate: &CommittedCandidateReceipt,
) {
	let signing_context = SigningContext { session_index: 0, parent_hash: Hash::zero() };
	let stmt = SignedFullStatement::sign(
		&keystore,
		Statement::Seconded(candidate.clone()),
		&signing_context,
		ValidatorIndex(0),
		&ValidatorId::from(Sr25519Keyring::Alice.public()),
	)
	.ok()
	.flatten()
	.expect("should be signed");

	overseer_send(
		virtual_overseer,
		CollatorProtocolMessage::Seconded(candidate.descriptor.relay_parent(), stmt),
	)
	.await;
}

async fn assert_collation_seconded(
	virtual_overseer: &mut VirtualOverseer,
	relay_parent: Hash,
	peer_id: PeerId,
	version: CollationVersion,
) {
	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(
			ReportPeerMessage::Single(peer, rep)
		)) => {
			assert_eq!(peer_id, peer);
			assert_eq!(rep.value, BENEFIT_NOTIFY_GOOD.cost_or_benefit());
		}
	);

	match version {
		CollationVersion::V1 => {
			assert_matches!(
				overseer_recv(virtual_overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendCollationMessage(
					peers,
					CollationProtocols::V1(protocol_v1::CollationProtocol::CollatorProtocol(
						protocol_v1::CollatorProtocolMessage::CollationSeconded(
							_relay_parent,
							..,
						),
					)),
				)) => {
					assert_eq!(peers, vec![peer_id]);
					assert_eq!(relay_parent, _relay_parent);
				}
			);
		},
		CollationVersion::V2 => {
			assert_matches!(
				overseer_recv(virtual_overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendCollationMessage(
					peers,
					CollationProtocols::V2(protocol_v2::CollationProtocol::CollatorProtocol(
						protocol_v2::CollatorProtocolMessage::CollationSeconded(
							_relay_parent,
							..,
						),
					)),
				)) => {
					assert_eq!(peers, vec![peer_id]);
					assert_eq!(relay_parent, _relay_parent);
				}
			);
		},
		CollationVersion::V3 => {
			assert_matches!(
				overseer_recv(virtual_overseer).await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendCollationMessage(
					peers,
					CollationProtocols::V3(protocol_v3::CollationProtocol::CollatorProtocol(
						protocol_v3::CollatorProtocolMessage::CollationSeconded(
							_relay_parent,
							..,
						),
					)),
				)) => {
					assert_eq!(peers, vec![peer_id]);
					assert_eq!(relay_parent, _relay_parent);
				}
			);
		},
		// V4 sends no `CollationSeconded`, so there is nothing to assert.
		CollationVersion::V4 => {},
	}
}

/// Assert that the next message is a persisted validation data request and respond with the
/// supplied PVD.
async fn assert_persisted_validation_data(
	virtual_overseer: &mut VirtualOverseer,
	version: CollationVersion,
	expected_relay_parent: Hash,
	expected_para_id: ParaId,
	expected_parent_head_data_hash: Option<Hash>,
	pvd: Option<PersistedValidationData>,
) {
	// Depending on relay parent mode pvd will be either requested
	// from the Runtime API or Prospective Parachains.
	let msg = overseer_recv(virtual_overseer).await;
	match version {
		CollationVersion::V1 => assert_matches!(
			msg,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				hash,
				RuntimeApiRequest::PersistedValidationData(para_id, assumption, tx),
			)) => {
				assert_eq!(expected_relay_parent, hash);
				assert_eq!(expected_para_id, para_id);
				assert_eq!(OccupiedCoreAssumption::Free, assumption);
				tx.send(Ok(pvd)).unwrap();
			}
		),
		CollationVersion::V2 | CollationVersion::V3 | CollationVersion::V4 => assert_matches!(
			msg,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::GetProspectiveValidationData(request, tx),
			) => {
				assert_eq!(expected_relay_parent, request.candidate_relay_parent);
				assert_eq!(expected_para_id, request.para_id);
				if let Some(expected_parent_head_data_hash) = expected_parent_head_data_hash {
					assert_eq!(expected_parent_head_data_hash, request.parent_head_data.hash());
				}
				tx.send(pvd).unwrap();
			}
		),
	}
}

// Combines dummy candidate creation, advertisement and fetching in a single call
async fn submit_second_and_assert(
	virtual_overseer: &mut VirtualOverseer,
	keystore: KeystorePtr,
	para_id: ParaId,
	relay_parent: Hash,
	collator: PeerId,
	candidate_head_data: HeadData,
) {
	let (candidate, commitments) =
		create_dummy_candidate_and_commitments(para_id, candidate_head_data, relay_parent);

	let candidate_hash = candidate.hash();
	let parent_head_data_hash = Hash::zero();

	assert_advertise_collation(
		virtual_overseer,
		collator,
		para_id,
		AdvertisementPayload::v2(relay_parent, candidate_hash, parent_head_data_hash),
	)
	.await;

	let response_channel = assert_fetch_collation_request(
		virtual_overseer,
		relay_parent,
		para_id,
		Some(candidate_hash),
	)
	.await;

	let pov = PoV { block_data: BlockData(vec![1]) };

	send_collation_and_assert_processing(
		virtual_overseer,
		keystore,
		relay_parent,
		para_id,
		collator,
		response_channel,
		candidate,
		commitments,
		pov,
	)
	.await;
}

fn create_dummy_candidate_and_commitments(
	para_id: ParaId,
	candidate_head_data: HeadData,
	relay_parent: Hash,
) -> (CandidateReceipt, CandidateCommitments) {
	let mut candidate = dummy_candidate_receipt_bad_sig(relay_parent, Some(Default::default()));
	candidate.descriptor.para_id = para_id;
	candidate.descriptor.persisted_validation_data_hash = dummy_pvd().hash();
	let commitments = CandidateCommitments {
		head_data: candidate_head_data,
		horizontal_messages: Default::default(),
		upward_messages: Default::default(),
		new_validation_code: None,
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};
	candidate.commitments_hash = commitments.hash();

	(candidate.into(), commitments)
}

async fn assert_advertise_collation(
	virtual_overseer: &mut VirtualOverseer,
	peer: PeerId,
	expected_para_id: ParaId,
	payload: AdvertisementPayload,
) {
	let (expected_candidate_hash, expected_parent_head_data_hash) = match &payload {
		AdvertisementPayload::V2 { candidate_hash, parent_head_data_hash, .. } => {
			(*candidate_hash, *parent_head_data_hash)
		},
		AdvertisementPayload::V3 { candidate_hash, parent_head_data_hash, .. } => {
			(*candidate_hash, *parent_head_data_hash)
		},
		AdvertisementPayload::V1 { .. } => {
			panic!("V1 advertisements have no candidate to assert on")
		},
	};
	advertise_collation(virtual_overseer, peer, payload).await;
	assert_matches!(
		overseer_recv(virtual_overseer).await,
		AllMessages::CandidateBacking(
			CandidateBackingMessage::CanSecond(request, tx),
		) => {
			assert_eq!(request.candidate_hash, expected_candidate_hash);
			assert_eq!(request.candidate_para_id, expected_para_id);
			assert_eq!(request.parent_head_data_hash, expected_parent_head_data_hash);
			tx.send(true).expect("receiving side should be alive");
		}
	);
}

async fn send_collation_and_assert_processing(
	virtual_overseer: &mut VirtualOverseer,
	keystore: KeystorePtr,
	relay_parent: Hash,
	expected_para_id: ParaId,
	expected_peer_id: PeerId,
	response_channel: ResponseSender,
	candidate: CandidateReceipt,
	commitments: CandidateCommitments,
	pov: PoV,
) {
	response_channel
		.send(Ok((
			request_v2::CollationFetchingResponse::Collation(candidate.clone(), pov.clone())
				.encode(),
			ProtocolName::from(""),
		)))
		.expect("Sending response should succeed");

	assert_candidate_backing_second(
		virtual_overseer,
		relay_parent,
		relay_parent,
		expected_para_id,
		&pov,
		CollationVersion::V2,
	)
	.await;

	let candidate = CommittedCandidateReceipt { descriptor: candidate.descriptor, commitments };

	send_seconded_statement(virtual_overseer, keystore.clone(), &candidate).await;

	assert_collation_seconded(
		virtual_overseer,
		relay_parent,
		expected_peer_id,
		CollationVersion::V2,
	)
	.await;
}

#[test]
fn v1_advertisement_accepted_and_seconded() {
	let mut test_state = TestState::default();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, keystore } = test_harness;

		let pair_a = CollatorPair::generate().0;

		let head_b = Hash::from_low_u64_be(128);
		let head_b_num: u32 = 0;

		update_view(&mut virtual_overseer, &mut test_state, vec![(head_b, head_b_num)]).await;

		let peer_a = PeerId::random();

		// Accept both collators from the implicit view.
		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[0],
			CollationVersion::V1,
		)
		.await;

		advertise_collation(&mut virtual_overseer, peer_a, AdvertisementPayload::v1(head_b)).await;

		let response_channel = assert_fetch_collation_request(
			&mut virtual_overseer,
			head_b,
			test_state.chain_ids[0],
			None,
		)
		.await;

		let mut candidate = dummy_candidate_receipt_bad_sig(head_b, Some(Default::default()));
		candidate.descriptor.para_id = test_state.chain_ids[0];
		candidate.descriptor.persisted_validation_data_hash = dummy_pvd().hash();
		let commitments = CandidateCommitments {
			head_data: HeadData(vec![1u8]),
			horizontal_messages: Default::default(),
			upward_messages: Default::default(),
			new_validation_code: None,
			processed_downward_messages: 0,
			hrmp_watermark: 0,
		};
		candidate.commitments_hash = commitments.hash();
		let candidate: CandidateReceipt = candidate.into();
		let pov = PoV { block_data: BlockData(vec![1]) };

		response_channel
			.send(Ok((
				request_v2::CollationFetchingResponse::Collation(candidate.clone(), pov.clone())
					.encode(),
				ProtocolName::from(""),
			)))
			.expect("Sending response should succeed");

		assert_candidate_backing_second(
			&mut virtual_overseer,
			head_b,
			head_b,
			test_state.chain_ids[0],
			&pov,
			CollationVersion::V1,
		)
		.await;

		let candidate = CommittedCandidateReceipt { descriptor: candidate.descriptor, commitments };

		send_seconded_statement(&mut virtual_overseer, keystore.clone(), &candidate).await;

		assert_collation_seconded(&mut virtual_overseer, head_b, peer_a, CollationVersion::V1)
			.await;

		virtual_overseer
	});
}

/// Regression test: obsolete claim queue positions are rejected.
///
/// With the leaf-based offset model, `is_slot_available` computes
/// `valid_len = lookahead - offset` for each relay parent. Only the first `valid_len`
/// positions in the leaf's claim queue are considered. Para A at position 2 with
/// `valid_len = 2` (offset=1) falls outside the checked range and is correctly rejected.
#[test]
fn obsolete_positions_rejected() {
	let mut test_state = TestState::with_one_scheduled_para();

	// Leaf CQ: [B, B, A]. Path: [R, L] → offset=1, valid_len=2, checks positions [0,1].
	// Para A only at position 2 → outside valid range → rejected.
	let mut claim_queue = BTreeMap::new();
	claim_queue.insert(
		CoreIndex(0),
		VecDeque::from_iter(
			[
				ParaId::from(999),       // Position 0: Para B (dummy)
				ParaId::from(999),       // Position 1: Para B (dummy)
				test_state.chain_ids[0], // Position 2: Para A (beyond valid range at offset=1)
			]
			.into_iter(),
		),
	);
	test_state.claim_queue = claim_queue;

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair = CollatorPair::generate().0;

		// R is the ancestor, L is the leaf (child of R)
		let head_l = Hash::from_low_u64_be(128);
		let head_l_num: u32 = 5;
		let head_r = get_parent_hash(head_l); // R is parent of L

		// Activate leaf L. This creates a view where R has child L.
		update_view(&mut virtual_overseer, &mut test_state, vec![(head_l, head_l_num)]).await;

		let peer = PeerId::random();
		connect_and_declare_collator(
			&mut virtual_overseer,
			peer,
			pair.clone(),
			test_state.chain_ids[0],
			CollationVersion::V2,
		)
		.await;

		// Advertise collation for Para A at relay_parent R (ancestor of L).
		// R has offset=1, valid_len=2: only CQ positions [0,1] are checked.
		// Para A sits at position 2 → not found → rejected.
		let candidate_hash = CandidateHash(Hash::repeat_byte(0xAA));
		advertise_collation(
			&mut virtual_overseer,
			peer,
			AdvertisementPayload::v2(head_r, candidate_hash, Hash::zero()),
		)
		.await;

		// No CanSecond: rejected by is_slot_available before reaching CandidateBacking.
		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		virtual_overseer
	});
}

/// Regression test: non-obsolete positions are still accepted.
///
/// CQ: [A, B, A]. Path [R, L] → R at offset=1, valid_len=2, checks positions [0,1].
/// Para A at position 0 is within the valid range → accepted.
#[test]
fn non_obsolete_position_accepted() {
	let mut test_state = TestState::with_one_scheduled_para();

	// CQ: [A, B, A]. R at offset=1 → valid_len=2 → positions [0,1] checked.
	// Para A found at position 0 → accepted.
	let mut claim_queue = BTreeMap::new();
	claim_queue.insert(
		CoreIndex(0),
		VecDeque::from_iter(
			[
				test_state.chain_ids[0], // Position 0: Para A (within valid range)
				ParaId::from(999),       // Position 1: Para B
				test_state.chain_ids[0], // Position 2: Para A (outside valid range, not checked)
			]
			.into_iter(),
		),
	);
	test_state.claim_queue = claim_queue;

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair = CollatorPair::generate().0;

		let head_l = Hash::from_low_u64_be(128);
		let head_l_num: u32 = 5;
		let head_r = get_parent_hash(head_l);

		update_view(&mut virtual_overseer, &mut test_state, vec![(head_l, head_l_num)]).await;

		let peer = PeerId::random();
		connect_and_declare_collator(
			&mut virtual_overseer,
			peer,
			pair.clone(),
			test_state.chain_ids[0],
			CollationVersion::V2,
		)
		.await;

		// Advertise collation for Para A at relay_parent R.
		// Para A found at position 0 (within valid_len=2) → accepted.
		let candidate_hash = CandidateHash(Hash::repeat_byte(0xCC));
		advertise_collation(
			&mut virtual_overseer,
			peer,
			AdvertisementPayload::v2(head_r, candidate_hash, Hash::zero()),
		)
		.await;

		// Should trigger CanSecond (accepted)
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::CanSecond(request, tx),
			) => {
				assert_eq!(request.candidate_hash, candidate_hash);
				assert_eq!(request.candidate_para_id, test_state.chain_ids[0]);
				tx.send(true).expect("receiving side should be alive");
			}
		);

		// Should proceed to fetch
		assert_fetch_collation_request(
			&mut virtual_overseer,
			head_r,
			test_state.chain_ids[0],
			Some(candidate_hash),
		)
		.await;

		virtual_overseer
	});
}

/// The last claim queue position is considered for a leaf-based collation.
///
/// CQ: [B, B, A]. Leaf at offset=0 → valid_len=3 → all positions checked.
/// Para A at the last position (2) is within the valid range → accepted.
#[test]
fn last_claim_queue_position_accepted_at_leaf() {
	let mut test_state = TestState::with_one_scheduled_para();

	// CQ: [B, B, A]. Leaf at offset=0 → valid_len=3 → all positions checked.
	let mut claim_queue = BTreeMap::new();
	claim_queue.insert(
		CoreIndex(0),
		VecDeque::from_iter(
			[
				ParaId::from(999),
				ParaId::from(999),
				test_state.chain_ids[0], // Position 2: Para A (last position)
			]
			.into_iter(),
		),
	);
	test_state.claim_queue = claim_queue;

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair = CollatorPair::generate().0;

		let head_r = Hash::from_low_u64_be(128);
		let head_r_num: u32 = 5;

		// R is the leaf itself (no children)
		update_view(&mut virtual_overseer, &mut test_state, vec![(head_r, head_r_num)]).await;

		let peer = PeerId::random();
		connect_and_declare_collator(
			&mut virtual_overseer,
			peer,
			pair.clone(),
			test_state.chain_ids[0],
			CollationVersion::V2,
		)
		.await;

		// Advertise at the leaf itself: offset=0, valid_len=3 → Para A at pos 2 accepted.
		let candidate_hash = CandidateHash(Hash::repeat_byte(0xDD));
		advertise_collation(
			&mut virtual_overseer,
			peer,
			AdvertisementPayload::v2(head_r, candidate_hash, Hash::zero()),
		)
		.await;

		// Should be accepted
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::CanSecond(request, tx),
			) => {
				assert_eq!(request.candidate_hash, candidate_hash);
				tx.send(true).expect("receiving side should be alive");
			}
		);

		assert_fetch_collation_request(
			&mut virtual_overseer,
			head_r,
			test_state.chain_ids[0],
			Some(candidate_hash),
		)
		.await;

		virtual_overseer
	});
}

/// Lookahead-truncation regression (the bug this PR fixes): the per-relay-parent valid range must
/// be bounded by the runtime scheduling lookahead, NOT by the claim-queue length.
///
/// The runtime CQ can be shorter than the scheduling lookahead (near genesis, after a session
/// change, or for on-demand cores). `is_slot_available` computes `valid_len = lookahead - offset`
/// per relay parent; pre-fix it used `lookahead = leaf_cq.len()`, which underestimates the valid
/// range for ancestor SPs and wrongly rejects otherwise-fetchable advertisements.
///
/// Setup: lookahead 3, but leaf CQ for our core is the short `[B, A]` (length 2). Para A sits at
/// position 1. Advertise A at the ancestor R (offset 1):
/// * Pre-fix: `valid_len = cq.len() - offset = 2 - 1 = 1` → only CQ position 0 checked → A at
///   position 1 is rejected.
/// * Fixed: `valid_len = lookahead - offset = 3 - 1 = 2` → positions [0, 1] checked → A accepted.
#[test]
fn short_claim_queue_does_not_truncate_ancestor_valid_range() {
	let mut test_state = TestState::with_one_scheduled_para();

	// Lookahead 3, but a CQ of length 2 (shorter than the lookahead).
	test_state.scheduling_lookahead = 3;
	let mut claim_queue = BTreeMap::new();
	claim_queue.insert(
		CoreIndex(0),
		VecDeque::from_iter(
			[
				ParaId::from(999),       // Position 0: Para B (dummy)
				test_state.chain_ids[0], // Position 1: Para A
			]
			.into_iter(),
		),
	);
	test_state.claim_queue = claim_queue;

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair = CollatorPair::generate().0;

		// R is the ancestor, L is the leaf (child of R).
		let head_l = Hash::from_low_u64_be(128);
		let head_l_num: u32 = 5;
		let head_r = get_parent_hash(head_l);

		update_view(&mut virtual_overseer, &mut test_state, vec![(head_l, head_l_num)]).await;

		let peer = PeerId::random();
		connect_and_declare_collator(
			&mut virtual_overseer,
			peer,
			pair.clone(),
			test_state.chain_ids[0],
			CollationVersion::V2,
		)
		.await;

		// Advertise Para A at the ancestor R (offset 1). A is at CQ position 1, reachable only
		// when the valid range is bounded by the lookahead (2 positions), not the CQ length
		// truncated by the offset (1 position).
		let candidate_hash = CandidateHash(Hash::repeat_byte(0xEE));
		advertise_collation(
			&mut virtual_overseer,
			peer,
			AdvertisementPayload::v2(head_r, candidate_hash, Hash::zero()),
		)
		.await;

		// Must be accepted (pre-fix this was rejected by the CQ-length-truncated valid range).
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::CanSecond(request, tx),
			) => {
				assert_eq!(request.candidate_hash, candidate_hash);
				assert_eq!(request.candidate_para_id, test_state.chain_ids[0]);
				tx.send(true).expect("receiving side should be alive");
			}
		);

		assert_fetch_collation_request(
			&mut virtual_overseer,
			head_r,
			test_state.chain_ids[0],
			Some(candidate_hash),
		)
		.await;

		virtual_overseer
	});
}

/// Test group rotation handling: verify that per-relay-parent core assignment works correctly.
/// When a validator rotates between cores across blocks in the implicit view, each relay parent
/// should use its own correct core assignment (not confused with other blocks' assignments).
///
/// Setup: With rotation_frequency=1 and 3 cores:
/// - Block 0: Group 0 → Core 0 (Para 1)
/// - Block 1: Group 0 → Core 2 (Para 2)
/// - Block 3: Group 0 → Core 0 (Para 1) again
///
/// Test verifies that advertisements at each relay parent are validated against that specific
/// relay parent's core assignment, not the leaf's assignment.
#[test]
fn group_rotation_uses_correct_core_per_relay_parent() {
	let mut test_state = TestState::default();

	// Default: rotation_frequency=1, 3 validator groups, 3 cores
	// Core 0 → Para 1, Core 2 → Para 2
	// Group 0 rotation: block 0→Core 0, block 1→Core 2, block 2→Core 1, block 3→Core 0...

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair_a = CollatorPair::generate().0;
		let pair_b = CollatorPair::generate().0;

		// Choose blocks where validator (group 0) is assigned to cores with paras
		// Block 0: Group 0 at Core 0 (Para 1)
		// Block 1: Group 0 at Core 2 (Para 2)
		let head_block_0 = Hash::from_low_u64_be(130); // Will be block number 0
		let head_block_1 = Hash::from_low_u64_be(129); // Will be block number 1

		// Activate both blocks in the view
		update_view(
			&mut virtual_overseer,
			&mut test_state,
			vec![(head_block_0, 0), (head_block_1, 1)],
		)
		.await;

		let peer_a = PeerId::random();
		let peer_b = PeerId::random();

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[0], // Para 1
			CollationVersion::V2,
		)
		.await;

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_b,
			pair_b.clone(),
			test_state.chain_ids[1], // Para 2
			CollationVersion::V2,
		)
		.await;

		// Advertise for Para 1 at block 0 (where validator is on Core 0 with Para 1)
		let candidate_hash_a = CandidateHash(Hash::repeat_byte(0xAA));
		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v2(head_block_0, candidate_hash_a, Hash::zero()),
		)
		.await;

		// Should be accepted - validator is assigned to Para 1's core at block 0
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::CanSecond(request, tx),
			) => {
				assert_eq!(request.candidate_hash, candidate_hash_a);
				assert_eq!(request.candidate_para_id, test_state.chain_ids[0]);
				tx.send(true).expect("receiving side should be alive");
			}
		);

		assert_fetch_collation_request(
			&mut virtual_overseer,
			head_block_0,
			test_state.chain_ids[0],
			Some(candidate_hash_a),
		)
		.await;

		// Advertise for Para 2 at block 1 (where validator is on Core 2 with Para 2)
		let candidate_hash_b = CandidateHash(Hash::repeat_byte(0xBB));
		advertise_collation(
			&mut virtual_overseer,
			peer_b,
			AdvertisementPayload::v2(head_block_1, candidate_hash_b, Hash::zero()),
		)
		.await;

		// Should be accepted - validator is assigned to Para 2's core at block 1
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::CanSecond(request, tx),
			) => {
				assert_eq!(request.candidate_hash, candidate_hash_b);
				assert_eq!(request.candidate_para_id, test_state.chain_ids[1]);
				tx.send(true).expect("receiving side should be alive");
			}
		);

		assert_fetch_collation_request(
			&mut virtual_overseer,
			head_block_1,
			test_state.chain_ids[1],
			Some(candidate_hash_b),
		)
		.await;

		virtual_overseer
	});
}

#[test]
fn v1_advertisement_rejected_on_non_active_leaf() {
	let mut test_state = TestState::default();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair_a = CollatorPair::generate().0;

		let head_b = Hash::from_low_u64_be(128);
		let head_b_num: u32 = 5;

		update_view(&mut virtual_overseer, &mut test_state, vec![(head_b, head_b_num)]).await;

		let peer_a = PeerId::random();

		// Accept both collators from the implicit view.
		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[0],
			CollationVersion::V1,
		)
		.await;

		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v1(get_parent_hash(head_b)),
		)
		.await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::NetworkBridgeTx(
				NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(peer, rep)),
			) => {
				assert_eq!(peer, peer_a);
				assert_eq!(rep.value, COST_PROTOCOL_MISUSE.cost_or_benefit());
			}
		);

		virtual_overseer
	});
}

#[test]
fn accept_advertisements_from_implicit_view() {
	let mut test_state = TestState::default();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair_a = CollatorPair::generate().0;
		let pair_b = CollatorPair::generate().0;

		let head_b = Hash::from_low_u64_be(128);
		let head_b_num: u32 = 2;

		let head_c = get_parent_hash(head_b);
		// Grandparent of head `b`.
		// Group rotation frequency is 1 by default, at `d` we're assigned
		// to the first para.
		let head_d = get_parent_hash(head_c);

		// Activated leaf is `b`, but the collation will be based on `c`.
		update_view(&mut virtual_overseer, &mut test_state, vec![(head_b, head_b_num)]).await;

		let peer_a = PeerId::random();
		let peer_b = PeerId::random();

		// Accept both collators from the implicit view.
		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[0],
			CollationVersion::V2,
		)
		.await;
		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_b,
			pair_b.clone(),
			test_state.chain_ids[1],
			CollationVersion::V2,
		)
		.await;

		let candidate_hash = CandidateHash::default();
		let parent_head_data_hash = Hash::zero();
		advertise_collation(
			&mut virtual_overseer,
			peer_b,
			AdvertisementPayload::v2(head_c, candidate_hash, parent_head_data_hash),
		)
		.await;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::CanSecond(request, tx),
			) => {
				assert_eq!(request.candidate_hash, candidate_hash);
				assert_eq!(request.candidate_para_id, test_state.chain_ids[1]);
				assert_eq!(request.parent_head_data_hash, parent_head_data_hash);
				tx.send(true).expect("receiving side should be alive");
			}
		);

		assert_fetch_collation_request(
			&mut virtual_overseer,
			head_c,
			test_state.chain_ids[1],
			Some(candidate_hash),
		)
		.await;
		// Advertise with different para.
		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v2(head_d, candidate_hash, parent_head_data_hash), // Note different relay parent.
		)
		.await;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::CanSecond(request, tx),
			) => {
				assert_eq!(request.candidate_hash, candidate_hash);
				assert_eq!(request.candidate_para_id, test_state.chain_ids[0]);
				assert_eq!(request.parent_head_data_hash, parent_head_data_hash);
				tx.send(true).expect("receiving side should be alive");
			}
		);

		assert_fetch_collation_request(
			&mut virtual_overseer,
			head_d,
			test_state.chain_ids[0],
			Some(candidate_hash),
		)
		.await;

		virtual_overseer
	});
}

#[test]
fn second_multiple_candidates_per_relay_parent() {
	let mut test_state = TestState::with_one_scheduled_para();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, keystore } = test_harness;

		let pair = CollatorPair::generate().0;

		// head_a must NOT be an ancestor of head_b, otherwise non-deterministic
		// activation order can leave head_a without allowed_relay_parents.
		// head_b's ancestors are 0x81, 0x82, ... so we pick a disjoint chain.
		let head_a = Hash::from_low_u64_be(5);
		let head_a_num: u32 = 0;

		let head_b = Hash::from_low_u64_be(128);
		let head_b_num: u32 = 2;

		// Activated leaf is `a` and `b`. The collation will be based on `b`.
		update_view(
			&mut virtual_overseer,
			&mut test_state,
			vec![(head_a, head_a_num), (head_b, head_b_num)],
		)
		.await;

		let peer_a = PeerId::random();

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair.clone(),
			test_state.chain_ids[0],
			CollationVersion::V2,
		)
		.await;

		for i in 0..test_state.scheduling_lookahead {
			submit_second_and_assert(
				&mut virtual_overseer,
				keystore.clone(),
				test_state.chain_ids[0],
				head_a,
				peer_a,
				HeadData(vec![i as u8]),
			)
			.await;
		}

		// No more advertisements can be made for this relay parent.
		let candidate_hash = CandidateHash(Hash::repeat_byte(0xAA));
		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v2(head_a, candidate_hash, Hash::zero()),
		)
		.await;

		// Rejected but not reported because reached the limit of advertisements for the para_id
		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		// By different peer too (not reported).
		let pair_b = CollatorPair::generate().0;
		let peer_b = PeerId::random();

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_b,
			pair_b.clone(),
			test_state.chain_ids[0],
			CollationVersion::V2,
		)
		.await;

		let candidate_hash = CandidateHash(Hash::repeat_byte(0xFF));
		advertise_collation(
			&mut virtual_overseer,
			peer_b,
			AdvertisementPayload::v2(head_a, candidate_hash, Hash::zero()),
		)
		.await;

		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		virtual_overseer
	});
}

#[test]
fn fetched_collation_sanity_check() {
	let mut test_state = TestState::default();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair = CollatorPair::generate().0;

		// Grandparent of head `a`.
		let head_b = Hash::from_low_u64_be(128);
		let head_b_num: u32 = 2;

		// Grandparent of head `b`.
		// Group rotation frequency is 1 by default, at `c` we're assigned
		// to the first para.
		let head_c = Hash::from_low_u64_be(130);

		// Activated leaf is `b`, but the collation will be based on `c`.
		update_view(&mut virtual_overseer, &mut test_state, vec![(head_b, head_b_num)]).await;

		let peer_a = PeerId::random();

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair.clone(),
			test_state.chain_ids[0],
			CollationVersion::V2,
		)
		.await;

		let mut candidate = dummy_candidate_receipt_bad_sig(head_c, Some(Default::default()));
		candidate.descriptor.para_id = test_state.chain_ids[0];
		let commitments = CandidateCommitments {
			head_data: HeadData(vec![1, 2, 3]),
			horizontal_messages: Default::default(),
			upward_messages: Default::default(),
			new_validation_code: None,
			processed_downward_messages: 0,
			hrmp_watermark: 0,
		};
		candidate.commitments_hash = commitments.hash();
		let candidate: CandidateReceipt = candidate.into();
		let candidate_hash = CandidateHash(Hash::zero());
		let parent_head_data_hash = Hash::zero();

		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v2(head_c, candidate_hash, parent_head_data_hash),
		)
		.await;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::CanSecond(request, tx),
			) => {
				assert_eq!(request.candidate_hash, candidate_hash);
				assert_eq!(request.candidate_para_id, test_state.chain_ids[0]);
				assert_eq!(request.parent_head_data_hash, parent_head_data_hash);
				tx.send(true).expect("receiving side should be alive");
			}
		);

		let response_channel = assert_fetch_collation_request(
			&mut virtual_overseer,
			head_c,
			test_state.chain_ids[0],
			Some(candidate_hash),
		)
		.await;

		let pov = PoV { block_data: BlockData(vec![1]) };

		response_channel
			.send(Ok((
				request_v2::CollationFetchingResponse::Collation(candidate.clone(), pov.clone())
					.encode(),
				ProtocolName::from(""),
			)))
			.expect("Sending response should succeed");

		// PVD request.
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::GetProspectiveValidationData(request, tx),
			) => {
				assert_eq!(head_c, request.candidate_relay_parent);
				assert_eq!(test_state.chain_ids[0], request.para_id);
				tx.send(Some(dummy_pvd())).unwrap();
			}
		);

		// Reported malicious.
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::NetworkBridgeTx(
				NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(peer_id, rep)),
			) => {
				assert_eq!(peer_a, peer_id);
				assert_eq!(rep.value, COST_REPORT_BAD.cost_or_benefit());
			}
		);

		virtual_overseer
	});
}

#[test]
fn sanity_check_invalid_parent_head_data() {
	let mut test_state = TestState::default();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair = CollatorPair::generate().0;

		let head_c = Hash::from_low_u64_be(130);
		let head_c_num = 3;

		update_view(&mut virtual_overseer, &mut test_state, vec![(head_c, head_c_num)]).await;

		let peer_a = PeerId::random();

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair.clone(),
			test_state.chain_ids[0],
			CollationVersion::V2,
		)
		.await;

		let mut candidate = dummy_candidate_receipt_bad_sig(head_c, Some(Default::default()));
		candidate.descriptor.para_id = test_state.chain_ids[0];
		let commitments = CandidateCommitments {
			head_data: HeadData(vec![1, 2, 3]),
			horizontal_messages: Default::default(),
			upward_messages: Default::default(),
			new_validation_code: None,
			processed_downward_messages: 0,
			hrmp_watermark: 0,
		};
		candidate.commitments_hash = commitments.hash();

		let parent_head_data = HeadData(vec![4, 2, 0]);
		let parent_head_data_hash = parent_head_data.hash();
		let wrong_parent_head_data = HeadData(vec![4, 2]);

		let mut pvd = dummy_pvd();
		pvd.parent_head = parent_head_data;

		candidate.descriptor.persisted_validation_data_hash = pvd.hash();
		let candidate: CandidateReceipt = candidate.into();

		let candidate_hash = candidate.hash();

		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v2(head_c, candidate_hash, parent_head_data_hash),
		)
		.await;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::CanSecond(request, tx),
			) => {
				assert_eq!(request.candidate_hash, candidate_hash);
				assert_eq!(request.candidate_para_id, test_state.chain_ids[0]);
				assert_eq!(request.parent_head_data_hash, parent_head_data_hash);
				tx.send(true).expect("receiving side should be alive");
			}
		);

		let response_channel = assert_fetch_collation_request(
			&mut virtual_overseer,
			head_c,
			test_state.chain_ids[0],
			Some(candidate_hash),
		)
		.await;

		let pov = PoV { block_data: BlockData(vec![1]) };

		response_channel
			.send(Ok((
				request_v2::CollationFetchingResponse::CollationWithParentHeadData {
					receipt: candidate.clone(),
					pov: pov.clone(),
					parent_head_data: wrong_parent_head_data,
				}
				.encode(),
				ProtocolName::from(""),
			)))
			.expect("Sending response should succeed");

		// PVD request.
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::GetProspectiveValidationData(request, tx),
			) => {
				assert_eq!(head_c, request.candidate_relay_parent);
				assert_eq!(test_state.chain_ids[0], request.para_id);
				tx.send(Some(pvd)).unwrap();
			}
		);

		// Reported malicious.
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::NetworkBridgeTx(
				NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(peer_id, rep)),
			) => {
				assert_eq!(peer_a, peer_id);
				assert_eq!(rep.value, COST_REPORT_BAD.cost_or_benefit());
			}
		);

		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		virtual_overseer
	});
}

#[test]
fn advertisement_spam_protection() {
	let mut test_state = TestState::default();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair_a = CollatorPair::generate().0;

		let head_b = Hash::from_low_u64_be(128);
		let head_b_num: u32 = 2;

		let head_c = get_parent_hash(head_b);

		// Activated leaf is `b`, but the collation will be based on `c`.
		update_view(&mut virtual_overseer, &mut test_state, vec![(head_b, head_b_num)]).await;

		let peer_a = PeerId::random();
		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[1],
			CollationVersion::V2,
		)
		.await;

		let candidate_hash = CandidateHash::default();
		let parent_head_data_hash = Hash::zero();
		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v2(head_c, candidate_hash, parent_head_data_hash),
		)
		.await;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::CanSecond(request, tx),
			) => {
				assert_eq!(request.candidate_hash, candidate_hash);
				assert_eq!(request.candidate_para_id, test_state.chain_ids[1]);
				assert_eq!(request.parent_head_data_hash, parent_head_data_hash);
				// Reject it.
				tx.send(false).expect("receiving side should be alive");
			}
		);

		// Send the same advertisement again.
		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v2(head_c, candidate_hash, parent_head_data_hash),
		)
		.await;
		// Reported.
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::NetworkBridgeTx(
				NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(peer_id, rep)),
			) => {
				assert_eq!(peer_a, peer_id);
				assert_eq!(rep.value, COST_UNEXPECTED_MESSAGE.cost_or_benefit());
			}
		);

		virtual_overseer
	});
}

#[rstest]
#[case(true)]
#[case(false)]
fn child_blocked_from_seconding_by_parent(#[case] valid_parent: bool) {
	let mut test_state = TestState::with_one_scheduled_para();

	// CQ length 4 needed: head_c at offset=2 from leaf gets valid_len = 4 - 2 = 2,
	// which allows exactly 2 advertisements at head_c.
	let mut claim_queue = BTreeMap::new();
	claim_queue.insert(
		CoreIndex(0),
		VecDeque::from_iter(
			[
				ParaId::from(test_state.chain_ids[0]),
				ParaId::from(test_state.chain_ids[0]),
				ParaId::from(test_state.chain_ids[0]),
				ParaId::from(test_state.chain_ids[0]),
			]
			.into_iter(),
		),
	);
	test_state.claim_queue = claim_queue;
	test_state.scheduling_lookahead = 4;

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, keystore } = test_harness;

		let pair = CollatorPair::generate().0;

		// Grandparent of head `a`.
		let head_b = Hash::from_low_u64_be(128);
		let head_b_num: u32 = 2;

		// Grandparent of head `b`.
		// Group rotation frequency is 1 by default, at `c` we're assigned
		// to the first para.
		let head_c = Hash::from_low_u64_be(130);

		// Activated leaf is `b`, but the collation will be based on `c`.
		update_view(&mut virtual_overseer, &mut test_state, vec![(head_b, head_b_num)]).await;

		let peer_a = PeerId::random();

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair.clone(),
			test_state.chain_ids[0],
			CollationVersion::V2,
		)
		.await;

		// Candidate A transitions from head data 0 to 1.
		// Candidate B transitions from head data 1 to 2.

		// Candidate B is advertised and fetched before candidate A.

		let mut candidate_b = dummy_candidate_receipt_bad_sig(head_c, Some(Default::default()));
		candidate_b.descriptor.para_id = test_state.chain_ids[0];
		candidate_b.descriptor.para_head = HeadData(vec![2]).hash();
		candidate_b.descriptor.persisted_validation_data_hash =
			PersistedValidationData::<Hash, BlockNumber> {
				parent_head: HeadData(vec![1]),
				relay_parent_number: 5,
				max_pov_size: 1024,
				relay_parent_storage_root: Default::default(),
			}
			.hash();
		let candidate_b_commitments = CandidateCommitments {
			head_data: HeadData(vec![2]),
			horizontal_messages: Default::default(),
			upward_messages: Default::default(),
			new_validation_code: None,
			processed_downward_messages: 0,
			hrmp_watermark: 0,
		};
		let mut candidate_b: CandidateReceipt = candidate_b.into();
		candidate_b.commitments_hash = candidate_b_commitments.hash();

		let candidate_b_hash = candidate_b.hash();

		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v2(head_c, candidate_b_hash, HeadData(vec![1]).hash()),
		)
		.await;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::CanSecond(request, tx),
			) => {
				assert_eq!(request.candidate_hash, candidate_b_hash);
				assert_eq!(request.candidate_para_id, test_state.chain_ids[0]);
				assert_eq!(request.parent_head_data_hash, HeadData(vec![1]).hash());
				tx.send(true).expect("receiving side should be alive");
			}
		);

		let response_channel = assert_fetch_collation_request(
			&mut virtual_overseer,
			head_c,
			test_state.chain_ids[0],
			Some(candidate_b_hash),
		)
		.await;

		response_channel
			.send(Ok((
				request_v2::CollationFetchingResponse::Collation(
					candidate_b.clone(),
					PoV { block_data: BlockData(vec![1]) },
				)
				.encode(),
				ProtocolName::from(""),
			)))
			.expect("Sending response should succeed");

		// Persisted validation data of candidate B is not found.
		assert_persisted_validation_data(
			&mut virtual_overseer,
			CollationVersion::V2,
			head_c,
			test_state.chain_ids[0],
			Some(HeadData(vec![1]).hash()),
			None,
		)
		.await;

		// Now advertise, fetch and validate candidate A, which is the parent of B.

		let mut candidate_a = dummy_candidate_receipt_bad_sig(head_c, Some(Default::default()));
		candidate_a.descriptor.para_id = test_state.chain_ids[0];
		candidate_a.descriptor.para_head = HeadData(vec![1]).hash();
		candidate_a.descriptor.persisted_validation_data_hash =
			PersistedValidationData::<Hash, BlockNumber> {
				parent_head: HeadData(vec![0]),
				relay_parent_number: 5,
				max_pov_size: 1024,
				relay_parent_storage_root: Default::default(),
			}
			.hash();
		let mut candidate_a: CandidateReceipt = candidate_a.into();
		let candidate_a_commitments = CandidateCommitments {
			head_data: HeadData(vec![1]),
			horizontal_messages: Default::default(),
			upward_messages: Default::default(),
			new_validation_code: None,
			processed_downward_messages: 0,
			hrmp_watermark: 0,
		};
		candidate_a.commitments_hash = candidate_a_commitments.hash();

		let candidate_a: CandidateReceipt = candidate_a.into();
		let candidate_a_hash = candidate_a.hash();

		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v2(head_c, candidate_a_hash, HeadData(vec![0]).hash()),
		)
		.await;
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::CanSecond(request, tx),
			) => {
				assert_eq!(request.candidate_hash, candidate_a_hash);
				assert_eq!(request.candidate_para_id, test_state.chain_ids[0]);
				assert_eq!(request.parent_head_data_hash, HeadData(vec![0]).hash());
				tx.send(true).expect("receiving side should be alive");
			}
		);

		let response_channel = assert_fetch_collation_request(
			&mut virtual_overseer,
			head_c,
			test_state.chain_ids[0],
			Some(candidate_a_hash),
		)
		.await;

		response_channel
			.send(Ok((
				request_v2::CollationFetchingResponse::Collation(
					candidate_a.clone(),
					PoV { block_data: BlockData(vec![2]) },
				)
				.encode(),
				ProtocolName::from(""),
			)))
			.expect("Sending response should succeed");

		assert_persisted_validation_data(
			&mut virtual_overseer,
			CollationVersion::V2,
			head_c,
			test_state.chain_ids[0],
			Some(HeadData(vec![0]).hash()),
			Some(PersistedValidationData::<Hash, BlockNumber> {
				parent_head: HeadData(vec![0]),
				relay_parent_number: 5,
				max_pov_size: 1024,
				relay_parent_storage_root: Default::default(),
			}),
		)
		.await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(CandidateBackingMessage::Second {
			scheduling_parent: relay_parent,
			candidate: candidate_receipt,
			pvd: received_pvd,
			pov: incoming_pov,
		}) => {
				assert_eq!(head_c, relay_parent);
				assert_eq!(test_state.chain_ids[0], candidate_receipt.descriptor.para_id());
				assert_eq!(PoV { block_data: BlockData(vec![2]) }, incoming_pov);
				assert_eq!(PersistedValidationData::<Hash, BlockNumber> {
					parent_head: HeadData(vec![0]),
					relay_parent_number: 5,
					max_pov_size: 1024,
					relay_parent_storage_root: Default::default(),
				}, received_pvd);
				candidate_receipt
			}
		);

		// If candidate A is valid, proceed with seconding B.
		if valid_parent {
			send_seconded_statement(
				&mut virtual_overseer,
				keystore.clone(),
				&CommittedCandidateReceipt {
					descriptor: candidate_a.descriptor,
					commitments: candidate_a_commitments,
				},
			)
			.await;

			assert_collation_seconded(&mut virtual_overseer, head_c, peer_a, CollationVersion::V2)
				.await;

			// Now that candidate A has been seconded, candidate B can be seconded as well.

			assert_persisted_validation_data(
				&mut virtual_overseer,
				CollationVersion::V2,
				head_c,
				test_state.chain_ids[0],
				Some(HeadData(vec![1]).hash()),
				Some(PersistedValidationData::<Hash, BlockNumber> {
					parent_head: HeadData(vec![1]),
					relay_parent_number: 5,
					max_pov_size: 1024,
					relay_parent_storage_root: Default::default(),
				}),
			)
			.await;

			assert_matches!(
					overseer_recv(&mut virtual_overseer).await,
					AllMessages::CandidateBacking(CandidateBackingMessage::Second {
				scheduling_parent: relay_parent,
				candidate: candidate_receipt,
				pvd: received_pvd,
				pov: incoming_pov,
			}) => {
						assert_eq!(head_c, relay_parent);
						assert_eq!(test_state.chain_ids[0], candidate_receipt.descriptor.para_id());
						assert_eq!(PoV { block_data: BlockData(vec![1]) }, incoming_pov);
						assert_eq!(PersistedValidationData::<Hash, BlockNumber> {
							parent_head: HeadData(vec![1]),
							relay_parent_number: 5,
							max_pov_size: 1024,
							relay_parent_storage_root: Default::default(),
						}, received_pvd);
						candidate_receipt
					}
				);

			send_seconded_statement(
				&mut virtual_overseer,
				keystore.clone(),
				&CommittedCandidateReceipt {
					descriptor: candidate_b.descriptor,
					commitments: candidate_b_commitments,
				},
			)
			.await;

			assert_collation_seconded(&mut virtual_overseer, head_c, peer_a, CollationVersion::V2)
				.await;
		} else {
			// If candidate A is invalid, B won't be seconded.
			overseer_send(
				&mut virtual_overseer,
				CollatorProtocolMessage::Invalid(head_c, candidate_a),
			)
			.await;

			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::NetworkBridgeTx(
					NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(peer, rep)),
				) => {
					assert_eq!(peer, peer_a);
					assert_eq!(rep.value, COST_REPORT_BAD.cost_or_benefit());
				}
			);
		}

		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		virtual_overseer
	});
}

#[rstest]
#[case(false, CollationVersion::V1)] // V3 descriptor via V1 protocol → rejected (wrong protocol)
#[case(false, CollationVersion::V2)] // V3 descriptor via V2 protocol → rejected (wrong protocol)
#[case(true, CollationVersion::V1)] // Crafted unknown descriptor via V1 → rejected
#[case(true, CollationVersion::V2)] // Crafted unknown descriptor via V2 → rejected
fn v3_descriptor_rejected_on_v1_v2_collation_protocols(
	#[case] crafted_unknown: bool,
	#[case] collation_version: CollationVersion,
) {
	let mut test_state = TestState::default();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair_a = CollatorPair::generate().0;

		let head_b = Hash::from_low_u64_be(128);
		let head_b_num: u32 = 0;

		update_view(&mut virtual_overseer, &mut test_state, vec![(head_b, head_b_num)]).await;

		let peer_a = PeerId::random();

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[0],
			collation_version,
		)
		.await;

		// Create a V3 descriptor
		let mut committed_candidate = dummy_committed_candidate_receipt_v3(head_b, head_b);
		committed_candidate.descriptor.set_para_id(test_state.chain_ids[0]);
		committed_candidate
			.descriptor
			.set_persisted_validation_data_hash(dummy_pvd().hash());
		committed_candidate.descriptor.set_core_index(CoreIndex(0));
		committed_candidate.descriptor.set_session_index(test_state.session_index);

		if crafted_unknown {
			// Create a descriptor with an unrecognized version field (version=2).
			// version=0 is V2, version=1 is V3, anything else is Unknown.
			committed_candidate.descriptor.set_version(2);
		}

		let candidate: CandidateReceipt = committed_candidate.clone().to_plain();
		let pov = PoV { block_data: BlockData(vec![1]) };

		let candidate_hash = candidate.hash();
		let parent_head_data_hash = Hash::zero();

		// V1 advertisement has no candidate hash; V2 includes it
		let payload = match collation_version {
			CollationVersion::V1 => AdvertisementPayload::v1(head_b),
			CollationVersion::V2 => {
				AdvertisementPayload::v2(head_b, candidate_hash, parent_head_data_hash)
			},
			_ => {
				panic!("unhandled collation version for advertisement")
			},
		};

		advertise_collation(&mut virtual_overseer, peer_a, payload).await;

		// V2 advertisements trigger CanSecond check
		if collation_version != CollationVersion::V1 {
			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::CandidateBacking(
					CandidateBackingMessage::CanSecond(request, tx),
				) => {
					assert_eq!(request.candidate_hash, candidate_hash);
					assert_eq!(request.candidate_para_id, test_state.chain_ids[0]);
					assert_eq!(request.parent_head_data_hash, parent_head_data_hash);
					tx.send(true).expect("receiving side should be alive");
				}
			);
		}

		let response_channel = assert_fetch_collation_request(
			&mut virtual_overseer,
			head_b,
			test_state.chain_ids[0],
			match collation_version {
				CollationVersion::V1 => None,
				_ => Some(candidate_hash),
			},
		)
		.await;

		// V1 uses request_v1, V2 uses request_v2
		let encoded_response = match collation_version {
			CollationVersion::V1 => {
				request_v1::CollationFetchingResponse::Collation(candidate.clone(), pov.clone())
					.encode()
			},
			CollationVersion::V2 => {
				request_v2::CollationFetchingResponse::Collation(candidate.clone(), pov.clone())
					.encode()
			},
			_ => panic!("unhandled collation version for collation fetching"),
		};
		response_channel
			.send(Ok((encoded_response, ProtocolName::from(""))))
			.expect("Sending response should succeed");

		// Crafted unknown version or V3 descriptor via wrong protocol → rejected
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::NetworkBridgeTx(
				NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(peer_id, rep)),
			) => {
				assert_eq!(peer_a, peer_id);
				assert_eq!(rep.value, COST_REPORT_BAD.cost_or_benefit());
			}
		);

		virtual_overseer
	});
}

/// When the relay chain stalls, the active leaf's slot can be many slots behind
/// the current slot. V3 candidates using that stale leaf as scheduling_parent
/// must be rejected because its slot is not `current_slot - 1`.
#[test]
fn v3_scheduling_parent_rejected_on_stalled_relay_chain() {
	let mut test_state = TestState::default();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair_a = CollatorPair::generate().0;

		let head_b = Hash::from_low_u64_be(128);
		let head_b_num: u32 = 0;

		// Use a slot far behind the current slot to simulate a stalled relay chain
		// where no new block has been produced for a long time.
		let stale_slot = Slot::from(1);

		update_view_with_slot(
			&mut virtual_overseer,
			&mut test_state,
			vec![(head_b, head_b_num)],
			Some(stale_slot),
		)
		.await;

		let peer_a = PeerId::random();

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[0],
			CollationVersion::V3,
		)
		.await;

		// Create a V3 descriptor with scheduling_parent == leaf == relay_parent (head_b).
		let mut committed_candidate = dummy_committed_candidate_receipt_v3(head_b, head_b);
		committed_candidate.descriptor.set_para_id(test_state.chain_ids[0]);
		committed_candidate
			.descriptor
			.set_persisted_validation_data_hash(dummy_pvd().hash());
		committed_candidate.descriptor.set_core_index(CoreIndex(0));
		committed_candidate.descriptor.set_session_index(test_state.session_index);
		committed_candidate.descriptor.set_version(1);

		let candidate: CandidateReceipt = committed_candidate.clone().to_plain();

		let candidate_hash = candidate.hash();
		let parent_head_data_hash = Hash::zero();

		// V3 advertisement with a stale scheduling_parent — should be rejected
		// with a minor reputation penalty.
		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v3(
				head_b,
				candidate_hash,
				parent_head_data_hash,
				CandidateDescriptorVersion::V3,
				head_b,
			),
		)
		.await;

		// Expect minor reputation penalty for invalid scheduling parent.
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::NetworkBridgeTx(
				NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(peer_id, rep)),
			) => {
				assert_eq!(peer_a, peer_id);
				assert_eq!(rep.value, COST_INVALID_SCHEDULING_PARENT.cost_or_benefit());
			}
		);

		virtual_overseer
	});
}

/// V3 scheduling parent validation: when the leaf's slot equals the current slot
/// (still in progress), the scheduling parent must be the leaf's parent.
#[test]
fn v3_scheduling_parent_in_progress_slot_accepts_leaf_parent() {
	let mut test_state = TestState::default();

	// Prevent core rotation so group 0 stays on core 0 across all ancestry blocks.
	test_state.group_rotation_info.group_rotation_frequency = 100;

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, keystore } = test_harness;

		let pair_a = CollatorPair::generate().0;

		let head_b = Hash::from_low_u64_be(128);
		// Use block 2 so the parent (block 1) is the scheduling_parent and the
		// grandparent (block 0) serves as relay_parent.
		let head_b_num: u32 = 2;
		let head_b_parent = get_parent_hash(head_b);
		let head_b_grandparent = get_parent_hash(head_b_parent);

		// Use the current slot so leaf.slot == current_slot (slot in progress).
		let current_slot = Slot::from_timestamp(
			sp_timestamp::Timestamp::current(),
			sp_consensus_slots::SlotDuration::from_millis(RELAY_CHAIN_SLOT_DURATION_MILLIS),
		);

		update_view_with_slot(
			&mut virtual_overseer,
			&mut test_state,
			vec![(head_b, head_b_num)],
			Some(current_slot),
		)
		.await;

		let peer_a = PeerId::random();

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[0],
			CollationVersion::V3,
		)
		.await;

		// relay_parent is the grandparent, scheduling_parent is the leaf's parent.
		let mut committed_candidate =
			dummy_committed_candidate_receipt_v3(head_b_grandparent, head_b_parent);
		committed_candidate.descriptor.set_para_id(test_state.chain_ids[0]);
		committed_candidate
			.descriptor
			.set_persisted_validation_data_hash(dummy_pvd().hash());
		committed_candidate.descriptor.set_core_index(CoreIndex(0));
		committed_candidate.descriptor.set_session_index(test_state.session_index);
		committed_candidate.descriptor.set_version(1);

		let candidate: CandidateReceipt = committed_candidate.clone().to_plain();
		let pov = PoV { block_data: BlockData(vec![1]) };

		let candidate_hash = candidate.hash();
		let parent_head_data_hash = Hash::zero();

		// V3 advertisement with scheduling_parent == leaf's parent. Since
		// leaf.slot == current_slot, the leaf's parent is the valid scheduling parent.
		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v3(
				head_b_parent,
				candidate_hash,
				parent_head_data_hash,
				CandidateDescriptorVersion::V3,
				head_b_grandparent,
			),
		)
		.await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::CanSecond(request, tx),
			) => {
				assert_eq!(request.candidate_hash, candidate_hash);
				assert_eq!(request.candidate_para_id, test_state.chain_ids[0]);
				tx.send(true).expect("receiving side should be alive");
			}
		);

		let response_channel = assert_fetch_collation_request(
			&mut virtual_overseer,
			head_b_parent,
			test_state.chain_ids[0],
			Some(candidate_hash),
		)
		.await;

		response_channel
			.send(Ok((
				request_v2::CollationFetchingResponse::Collation(candidate.clone(), pov.clone())
					.encode(),
				ProtocolName::from(""),
			)))
			.expect("Sending response should succeed");

		assert_candidate_backing_second(
			&mut virtual_overseer,
			head_b_parent,
			head_b_grandparent,
			test_state.chain_ids[0],
			&pov,
			CollationVersion::V3,
		)
		.await;

		send_seconded_statement(&mut virtual_overseer, keystore.clone(), &committed_candidate)
			.await;

		assert_collation_seconded(
			&mut virtual_overseer,
			head_b_parent,
			peer_a,
			CollationVersion::V3,
		)
		.await;

		virtual_overseer
	});
}

/// V3 scheduling parent validation: when the leaf's slot is `current_slot - 1`
/// (just finished), the scheduling parent must be the leaf itself.
#[test]
fn v3_scheduling_parent_finished_slot_accepts_leaf() {
	let mut test_state = TestState::default();

	// Prevent core rotation so group 0 stays on core 0 across all ancestry blocks.
	test_state.group_rotation_info.group_rotation_frequency = 100;

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, keystore } = test_harness;

		let pair_a = CollatorPair::generate().0;

		let head_b = Hash::from_low_u64_be(128);
		// Use block 1 so the parent (block 0) can serve as relay_parent while
		// the leaf itself (block 1) is the scheduling_parent.
		let head_b_num: u32 = 1;
		let head_b_parent = get_parent_hash(head_b);

		// Use current_slot - 1 so leaf.slot == current_slot - 1 (just finished).
		let finished_slot = Slot::from_timestamp(
			sp_timestamp::Timestamp::new(
				*sp_timestamp::Timestamp::current() - RELAY_CHAIN_SLOT_DURATION_MILLIS,
			),
			sp_consensus_slots::SlotDuration::from_millis(RELAY_CHAIN_SLOT_DURATION_MILLIS),
		);

		update_view_with_slot(
			&mut virtual_overseer,
			&mut test_state,
			vec![(head_b, head_b_num)],
			Some(finished_slot),
		)
		.await;

		let peer_a = PeerId::random();

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[0],
			CollationVersion::V3,
		)
		.await;

		// relay_parent is the parent, scheduling_parent is the leaf (slot just finished).
		let mut committed_candidate = dummy_committed_candidate_receipt_v3(head_b_parent, head_b);
		committed_candidate.descriptor.set_para_id(test_state.chain_ids[0]);
		committed_candidate
			.descriptor
			.set_persisted_validation_data_hash(dummy_pvd().hash());
		committed_candidate.descriptor.set_core_index(CoreIndex(0));
		committed_candidate.descriptor.set_session_index(test_state.session_index);

		let candidate: CandidateReceipt = committed_candidate.clone().to_plain();
		let pov = PoV { block_data: BlockData(vec![1]) };

		let candidate_hash = candidate.hash();
		let parent_head_data_hash = Hash::zero();

		// V3 advertisement with scheduling_parent == leaf. Since
		// leaf.slot == current_slot - 1, the leaf itself is the valid scheduling parent.
		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v3(
				head_b,
				candidate_hash,
				parent_head_data_hash,
				CandidateDescriptorVersion::V3,
				head_b_parent,
			),
		)
		.await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::CanSecond(request, tx),
			) => {
				assert_eq!(request.candidate_hash, candidate_hash);
				assert_eq!(request.candidate_para_id, test_state.chain_ids[0]);
				tx.send(true).expect("receiving side should be alive");
			}
		);

		let response_channel = assert_fetch_collation_request(
			&mut virtual_overseer,
			head_b,
			test_state.chain_ids[0],
			Some(candidate_hash),
		)
		.await;

		response_channel
			.send(Ok((
				request_v2::CollationFetchingResponse::Collation(candidate.clone(), pov.clone())
					.encode(),
				ProtocolName::from(""),
			)))
			.expect("Sending response should succeed");

		assert_candidate_backing_second(
			&mut virtual_overseer,
			head_b,
			head_b_parent,
			test_state.chain_ids[0],
			&pov,
			CollationVersion::V3,
		)
		.await;

		send_seconded_statement(&mut virtual_overseer, keystore.clone(), &committed_candidate)
			.await;

		assert_collation_seconded(&mut virtual_overseer, head_b, peer_a, CollationVersion::V3)
			.await;

		virtual_overseer
	});
}

/// V3 scheduling parent validation: when `leaf.slot == current_slot` (in progress),
/// using the leaf itself as scheduling_parent (instead of the leaf's parent) must be
/// rejected. This proves the slot check is the gating logic, since the leaf IS in
/// per_scheduling_parent.
#[test]
fn v3_scheduling_parent_in_progress_slot_rejects_leaf() {
	let mut test_state = TestState::default();

	// Prevent core rotation so group 0 stays on core 0 across all ancestry blocks.
	test_state.group_rotation_info.group_rotation_frequency = 100;

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair_a = CollatorPair::generate().0;

		let head_b = Hash::from_low_u64_be(128);
		let head_b_num: u32 = 1;
		let head_b_parent = get_parent_hash(head_b);

		// Use the current slot so leaf.slot == current_slot (slot in progress).
		let current_slot = Slot::from_timestamp(
			sp_timestamp::Timestamp::current(),
			sp_consensus_slots::SlotDuration::from_millis(RELAY_CHAIN_SLOT_DURATION_MILLIS),
		);

		update_view_with_slot(
			&mut virtual_overseer,
			&mut test_state,
			vec![(head_b, head_b_num)],
			Some(current_slot),
		)
		.await;

		let peer_a = PeerId::random();

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[0],
			CollationVersion::V3,
		)
		.await;

		// Use the leaf itself as scheduling_parent — wrong when leaf.slot == current_slot
		// (should be leaf's parent).
		let mut committed_candidate = dummy_committed_candidate_receipt_v3(head_b_parent, head_b);
		committed_candidate.descriptor.set_para_id(test_state.chain_ids[0]);
		committed_candidate
			.descriptor
			.set_persisted_validation_data_hash(dummy_pvd().hash());
		committed_candidate.descriptor.set_core_index(CoreIndex(0));
		committed_candidate.descriptor.set_session_index(test_state.session_index);

		let candidate: CandidateReceipt = committed_candidate.clone().to_plain();
		let candidate_hash = candidate.hash();
		let parent_head_data_hash = Hash::zero();

		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v3(
				head_b,
				candidate_hash,
				parent_head_data_hash,
				CandidateDescriptorVersion::V3,
				head_b_parent,
			),
		)
		.await;

		// Rejected by slot check — expect minor reputation penalty.
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::NetworkBridgeTx(
				NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(peer_id, rep)),
			) => {
				assert_eq!(peer_a, peer_id);
				assert_eq!(rep.value, COST_INVALID_SCHEDULING_PARENT.cost_or_benefit());
			}
		);

		virtual_overseer
	});
}

/// V3 scheduling parent validation: when `leaf.slot == current_slot - 1` (just finished),
/// using the leaf's parent as scheduling_parent (instead of the leaf itself) must be
/// rejected. This proves the slot check is the gating logic, since the parent IS in
/// per_scheduling_parent.
#[test]
fn v3_scheduling_parent_finished_slot_rejects_parent() {
	let mut test_state = TestState::default();

	// Prevent core rotation so group 0 stays on core 0 across all ancestry blocks.
	test_state.group_rotation_info.group_rotation_frequency = 100;

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair_a = CollatorPair::generate().0;

		let head_b = Hash::from_low_u64_be(128);
		// Use block 2 so the parent (block 1) can be used as the (wrong) scheduling_parent
		// and the grandparent (block 0) serves as relay_parent.
		let head_b_num: u32 = 2;
		let head_b_parent = get_parent_hash(head_b);
		let head_b_grandparent = get_parent_hash(head_b_parent);

		// Use current_slot - 1 so leaf.slot == current_slot - 1 (just finished).
		let finished_slot = Slot::from_timestamp(
			sp_timestamp::Timestamp::new(
				*sp_timestamp::Timestamp::current() - RELAY_CHAIN_SLOT_DURATION_MILLIS,
			),
			sp_consensus_slots::SlotDuration::from_millis(RELAY_CHAIN_SLOT_DURATION_MILLIS),
		);

		update_view_with_slot(
			&mut virtual_overseer,
			&mut test_state,
			vec![(head_b, head_b_num)],
			Some(finished_slot),
		)
		.await;

		let peer_a = PeerId::random();

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[0],
			CollationVersion::V3,
		)
		.await;

		// Use the leaf's parent as scheduling_parent — wrong when leaf.slot == current_slot - 1
		// (should be the leaf itself).
		let mut committed_candidate =
			dummy_committed_candidate_receipt_v3(head_b_grandparent, head_b_parent);
		committed_candidate.descriptor.set_para_id(test_state.chain_ids[0]);
		committed_candidate
			.descriptor
			.set_persisted_validation_data_hash(dummy_pvd().hash());
		committed_candidate.descriptor.set_core_index(CoreIndex(0));
		committed_candidate.descriptor.set_session_index(test_state.session_index);

		let candidate: CandidateReceipt = committed_candidate.clone().to_plain();
		let candidate_hash = candidate.hash();
		let parent_head_data_hash = Hash::zero();

		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v3(
				head_b_parent,
				candidate_hash,
				parent_head_data_hash,
				CandidateDescriptorVersion::V3,
				head_b_grandparent,
			),
		)
		.await;

		// Rejected by slot check — expect minor reputation penalty.
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::NetworkBridgeTx(
				NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(peer_id, rep)),
			) => {
				assert_eq!(peer_a, peer_id);
				assert_eq!(rep.value, COST_INVALID_SCHEDULING_PARENT.cost_or_benefit());
			}
		);

		virtual_overseer
	});
}

/// V3 advertisement with a scheduling parent outside the allowed ancestry must be rejected
/// at the `per_scheduling_parent` lookup (`SchedulingParentUnknown`).
#[test]
fn v3_scheduling_parent_outside_allowed_ancestry_rejected() {
	let mut test_state = TestState::default();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair_a = CollatorPair::generate().0;

		let head_b = Hash::from_low_u64_be(128);
		let head_b_num: u32 = 0;

		update_view(&mut virtual_overseer, &mut test_state, vec![(head_b, head_b_num)]).await;

		let peer_a = PeerId::random();

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[0],
			CollationVersion::V3,
		)
		.await;

		// A scheduling parent not in the implicit view's allowed ancestry.
		let unknown_scheduling_parent = Hash::from_low_u64_be(999);

		let mut committed_candidate = dummy_committed_candidate_receipt_v2(head_b);
		committed_candidate.descriptor.set_para_id(test_state.chain_ids[0]);
		committed_candidate
			.descriptor
			.set_persisted_validation_data_hash(dummy_pvd().hash());
		committed_candidate.descriptor.set_core_index(CoreIndex(0));
		committed_candidate.descriptor.set_session_index(test_state.session_index);
		committed_candidate.descriptor.set_scheduling_parent(unknown_scheduling_parent);

		let candidate: CandidateReceipt = committed_candidate.clone().to_plain();
		let candidate_hash = candidate.hash();
		let parent_head_data_hash = Hash::zero();

		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v3(
				unknown_scheduling_parent,
				candidate_hash,
				parent_head_data_hash,
				CandidateDescriptorVersion::V3,
				head_b,
			),
		)
		.await;

		// Rejected by per_scheduling_parent lookup (SchedulingParentUnknown) — expect
		// a reputation report with COST_UNEXPECTED_MESSAGE.
		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::NetworkBridgeTx(
				NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(peer_id, rep)),
			) => {
				assert_eq!(peer_a, peer_id);
				assert_eq!(rep.value, COST_UNEXPECTED_MESSAGE.cost_or_benefit());
			}
		);

		virtual_overseer
	});
}

/// Test that a V3-capable validator (V3 node feature enabled) correctly identifies a V1 descriptor
#[test]
fn v1_descriptor_version_detection_with_v3_enabled() {
	let mut test_state = TestState::default();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, keystore } = test_harness;

		let pair_a = CollatorPair::generate().0;

		let head_b = Hash::from_low_u64_be(128);
		let head_b_num: u32 = 0;

		update_view(&mut virtual_overseer, &mut test_state, vec![(head_b, head_b_num)]).await;

		let peer_a = PeerId::random();

		// Collator connects using the legacy V1 wire protocol.
		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[0],
			CollationVersion::V1,
		)
		.await;

		advertise_collation(&mut virtual_overseer, peer_a, AdvertisementPayload::v1(head_b)).await;

		let response_channel = assert_fetch_collation_request(
			&mut virtual_overseer,
			head_b,
			test_state.chain_ids[0],
			None,
		)
		.await;

		// Build a V1 descriptor directly with non-zero collator (bytes 0..31) so that
		// collator[8..24] maps to `reserved1[0..16]` in the V2 layout, triggering V1
		// detection under the relaxed V3 check.
		let commitments = CandidateCommitments {
			head_data: HeadData(vec![1u8]),
			horizontal_messages: Default::default(),
			upward_messages: Default::default(),
			new_validation_code: None,
			processed_downward_messages: 0,
			hrmp_watermark: 0,
		};

		// Non-zero collator: bytes 8..23 map to `reserved1[0..16]` in V2 layout,
		// triggering V1 detection under the relaxed V3 check.
		let mut collator_bytes = [0u8; 32];
		collator_bytes.iter_mut().enumerate().for_each(|(i, b)| *b = i as u8);
		let mut signature_bytes = [0u8; 64];
		signature_bytes.iter_mut().enumerate().for_each(|(i, b)| *b = i as u8);

		let v1_descriptor = CandidateDescriptor {
			para_id: test_state.chain_ids[0],
			relay_parent: head_b,
			collator: CollatorId::from(sp_core::sr25519::Public::from_raw(collator_bytes)),
			persisted_validation_data_hash: dummy_pvd().hash(),
			pov_hash: Hash::zero(),
			erasure_root: Hash::zero(),
			signature: CollatorSignature::from(sp_core::sr25519::Signature::from_raw(
				signature_bytes,
			)),
			para_head: Hash::zero(),
			validation_code_hash: Hash::zero().into(),
		};

		let candidate = CandidateReceipt {
			descriptor: v1_descriptor.into(),
			commitments_hash: commitments.hash(),
		};

		assert_eq!(
			candidate.descriptor.version(),
			CandidateDescriptorVersion::V1,
			"non-zero reserved1 bytes must be detected as V1 even when v3_enabled=true"
		);

		let pov = PoV { block_data: BlockData(vec![1]) };

		response_channel
			.send(Ok((
				request_v2::CollationFetchingResponse::Collation(candidate.clone(), pov.clone())
					.encode(),
				ProtocolName::from(""),
			)))
			.expect("Sending response should succeed");

		// The subsystem must take the legacy path: RuntimeApi::PersistedValidationData,
		// NOT ProspectiveParachains::GetProspectiveValidationData.
		// assert_candidate_backing_second with CollationVersion::V1 asserts exactly that.
		assert_candidate_backing_second(
			&mut virtual_overseer,
			head_b,
			head_b,
			test_state.chain_ids[0],
			&pov,
			CollationVersion::V1,
		)
		.await;

		let committed = CommittedCandidateReceipt { descriptor: candidate.descriptor, commitments };
		send_seconded_statement(&mut virtual_overseer, keystore.clone(), &committed).await;

		assert_collation_seconded(&mut virtual_overseer, head_b, peer_a, CollationVersion::V1)
			.await;

		virtual_overseer
	});
}

#[test]
fn invalid_v2_descriptor() {
	let mut test_state = TestState::default();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, .. } = test_harness;

		let pair_a = CollatorPair::generate().0;

		let head_b = Hash::from_low_u64_be(128);
		let head_b_num: u32 = 0;

		update_view(&mut virtual_overseer, &mut test_state, vec![(head_b, head_b_num)]).await;

		let peer_a = PeerId::random();

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[0],
			CollationVersion::V2,
		)
		.await;

		let mut candidates = vec![];

		let mut committed_candidate = dummy_committed_candidate_receipt_v2(head_b);
		committed_candidate.descriptor.set_para_id(test_state.chain_ids[0]);
		committed_candidate
			.descriptor
			.set_persisted_validation_data_hash(dummy_pvd().hash());
		// First para is assigned to core 0, set an invalid core index.
		committed_candidate.descriptor.set_core_index(CoreIndex(10));
		committed_candidate.descriptor.set_session_index(test_state.session_index);

		candidates.push(committed_candidate.clone());

		// Invalid session index.
		committed_candidate.descriptor.set_core_index(CoreIndex(0));
		committed_candidate.descriptor.set_session_index(10);

		candidates.push(committed_candidate);

		for committed_candidate in candidates {
			let candidate: CandidateReceipt = committed_candidate.clone().to_plain();
			let pov = PoV { block_data: BlockData(vec![1]) };

			let candidate_hash = candidate.hash();
			let parent_head_data_hash = Hash::zero();

			advertise_collation(
				&mut virtual_overseer,
				peer_a,
				AdvertisementPayload::v2(head_b, candidate_hash, parent_head_data_hash),
			)
			.await;

			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::CandidateBacking(
					CandidateBackingMessage::CanSecond(request, tx),
				) => {
					assert_eq!(request.candidate_hash, candidate_hash);
					assert_eq!(request.candidate_para_id, test_state.chain_ids[0]);
					assert_eq!(request.parent_head_data_hash, parent_head_data_hash);
					tx.send(true).expect("receiving side should be alive");
				}
			);

			let response_channel = assert_fetch_collation_request(
				&mut virtual_overseer,
				head_b,
				test_state.chain_ids[0],
				Some(candidate_hash),
			)
			.await;

			response_channel
				.send(Ok((
					request_v2::CollationFetchingResponse::Collation(
						candidate.clone(),
						pov.clone(),
					)
					.encode(),
					ProtocolName::from(""),
				)))
				.expect("Sending response should succeed");

			// Reported malicious. Invalid core index
			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::NetworkBridgeTx(
					NetworkBridgeTxMessage::ReportPeer(ReportPeerMessage::Single(peer_id, rep)),
				) => {
					assert_eq!(peer_a, peer_id);
					assert_eq!(rep.value, COST_REPORT_BAD.cost_or_benefit());
				}
			);
		}

		virtual_overseer
	});
}

#[test]
fn fair_collation_fetches() {
	let mut test_state = TestState::with_shared_core();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, keystore } = test_harness;

		let head_b = Hash::from_low_u64_be(128);
		let head_b_num: u32 = 2;

		update_view(&mut virtual_overseer, &mut test_state, vec![(head_b, head_b_num)]).await;

		let peer_a = PeerId::random();
		let pair_a = CollatorPair::generate().0;

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_a,
			pair_a.clone(),
			test_state.chain_ids[0],
			CollationVersion::V2,
		)
		.await;

		let peer_b = PeerId::random();
		let pair_b = CollatorPair::generate().0;

		connect_and_declare_collator(
			&mut virtual_overseer,
			peer_b,
			pair_b.clone(),
			test_state.chain_ids[1],
			CollationVersion::V2,
		)
		.await;

		// `peer_a` sends two advertisements (its claim queue limit)
		for i in 0..2u8 {
			submit_second_and_assert(
				&mut virtual_overseer,
				keystore.clone(),
				ParaId::from(test_state.chain_ids[0]),
				head_b,
				peer_a,
				HeadData(vec![i]),
			)
			.await;
		}

		// `peer_a` sends another advertisement and it is ignored
		let candidate_hash = CandidateHash(Hash::repeat_byte(0xAA));
		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v2(head_b, candidate_hash, Hash::zero()),
		)
		.await;
		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		// `peer_b` should still be able to advertise its collation
		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			ParaId::from(test_state.chain_ids[1]),
			head_b,
			peer_b,
			HeadData(vec![0u8]),
		)
		.await;

		// And no more advertisements can be made for this relay parent.

		// verify for peer_a
		let candidate_hash = CandidateHash(Hash::repeat_byte(0xBB));
		advertise_collation(
			&mut virtual_overseer,
			peer_a,
			AdvertisementPayload::v2(head_b, candidate_hash, Hash::zero()),
		)
		.await;
		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		// verify for peer_b
		let candidate_hash = CandidateHash(Hash::repeat_byte(0xCC));
		advertise_collation(
			&mut virtual_overseer,
			peer_b,
			AdvertisementPayload::v2(head_b, candidate_hash, Hash::zero()),
		)
		.await;
		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		virtual_overseer
	});
}

#[test]
fn collation_fetching_prefer_entries_earlier_in_claim_queue() {
	let mut test_state = TestState::with_shared_core();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, keystore } = test_harness;

		let pair_a = CollatorPair::generate().0;
		let collator_a = PeerId::random();
		let para_id_a = test_state.chain_ids[0];

		let pair_b = CollatorPair::generate().0;
		let collator_b = PeerId::random();
		let para_id_b = test_state.chain_ids[1];

		let head = Hash::from_low_u64_be(128);
		let head_num: u32 = 2;

		update_view(&mut virtual_overseer, &mut test_state, vec![(head, head_num)]).await;

		connect_and_declare_collator(
			&mut virtual_overseer,
			collator_a,
			pair_a.clone(),
			para_id_a,
			CollationVersion::V2,
		)
		.await;

		connect_and_declare_collator(
			&mut virtual_overseer,
			collator_b,
			pair_b.clone(),
			para_id_b,
			CollationVersion::V2,
		)
		.await;

		let (candidate_a1, commitments_a1) =
			create_dummy_candidate_and_commitments(para_id_a, HeadData(vec![0u8]), head);
		let (candidate_b1, commitments_b1) =
			create_dummy_candidate_and_commitments(para_id_b, HeadData(vec![1u8]), head);
		let (candidate_a2, commitments_a2) =
			create_dummy_candidate_and_commitments(para_id_a, HeadData(vec![2u8]), head);
		let (candidate_a3, _) =
			create_dummy_candidate_and_commitments(para_id_a, HeadData(vec![3u8]), head);
		let parent_head_data_a1 = HeadData(vec![0u8]);
		let parent_head_data_b1 = HeadData(vec![1u8]);
		let parent_head_data_a2 = HeadData(vec![2u8]);
		let parent_head_data_a3 = HeadData(vec![3u8]);

		// advertise a collation for `para_id_a` but don't send the collation. This will be a
		// pending fetch.
		assert_advertise_collation(
			&mut virtual_overseer,
			collator_a,
			para_id_a,
			AdvertisementPayload::v2(head, candidate_a1.hash(), parent_head_data_a1.hash()),
		)
		.await;

		let response_channel_a1 = assert_fetch_collation_request(
			&mut virtual_overseer,
			head,
			para_id_a,
			Some(candidate_a1.hash()),
		)
		.await;

		// advertise another collation for `para_id_a`. This one should be fetched last.
		assert_advertise_collation(
			&mut virtual_overseer,
			collator_a,
			para_id_a,
			AdvertisementPayload::v2(head, candidate_a2.hash(), parent_head_data_a2.hash()),
		)
		.await;

		// There is a pending collation so nothing should be fetched
		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		// Advertise a collation for `para_id_b`. This should be fetched second
		assert_advertise_collation(
			&mut virtual_overseer,
			collator_b,
			para_id_b,
			AdvertisementPayload::v2(head, candidate_b1.hash(), parent_head_data_b1.hash()),
		)
		.await;

		// Again - no fetch because of the pending collation
		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		// Now send a response for the first fetch and examine the second fetch
		send_collation_and_assert_processing(
			&mut virtual_overseer,
			keystore.clone(),
			head,
			para_id_a,
			collator_a,
			response_channel_a1,
			candidate_a1,
			commitments_a1,
			PoV { block_data: BlockData(vec![1]) },
		)
		.await;

		// The next fetch should be for `para_id_b`
		let response_channel_b = assert_fetch_collation_request(
			&mut virtual_overseer,
			head,
			para_id_b,
			Some(candidate_b1.hash()),
		)
		.await;

		send_collation_and_assert_processing(
			&mut virtual_overseer,
			keystore.clone(),
			head,
			para_id_b,
			collator_b,
			response_channel_b,
			candidate_b1,
			commitments_b1,
			PoV { block_data: BlockData(vec![2]) },
		)
		.await;

		// and the final one for `para_id_a`
		let response_channel_a2 = assert_fetch_collation_request(
			&mut virtual_overseer,
			head,
			para_id_a,
			Some(candidate_a2.hash()),
		)
		.await;

		// Advertise another collation for `para_id_a`. This should be rejected as there is no slot
		// in the claim queue for it. One is fetched and one is pending.
		advertise_collation(
			&mut virtual_overseer,
			collator_a,
			AdvertisementPayload::v2(head, candidate_a3.hash(), parent_head_data_a3.hash()),
		)
		.await;

		// `CanSecond` shouldn't be sent as the advertisement should be ignored
		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		// Fetch the pending collation
		send_collation_and_assert_processing(
			&mut virtual_overseer,
			keystore.clone(),
			head,
			para_id_a,
			collator_a,
			response_channel_a2,
			candidate_a2,
			commitments_a2,
			PoV { block_data: BlockData(vec![3]) },
		)
		.await;

		virtual_overseer
	});
}

#[test]
fn collation_fetching_considers_advertisements_from_the_whole_view() {
	let mut test_state = TestState::with_shared_core();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, keystore } = test_harness;

		let pair_a = CollatorPair::generate().0;
		let collator_a = PeerId::random();
		let para_id_a = test_state.chain_ids[0];

		let pair_b = CollatorPair::generate().0;
		let collator_b = PeerId::random();
		let para_id_b = test_state.chain_ids[1];

		let relay_parent_2 = Hash::from_low_u64_be(test_state.relay_parent.to_low_u64_be() - 1);

		assert_eq!(
			*test_state.claim_queue.get(&CoreIndex(0)).unwrap(),
			VecDeque::from([para_id_b, para_id_a, para_id_a])
		);

		update_view(&mut virtual_overseer, &mut test_state, vec![(relay_parent_2, 2)]).await;

		connect_and_declare_collator(
			&mut virtual_overseer,
			collator_a,
			pair_a.clone(),
			para_id_a,
			CollationVersion::V2,
		)
		.await;

		connect_and_declare_collator(
			&mut virtual_overseer,
			collator_b,
			pair_b.clone(),
			para_id_b,
			CollationVersion::V2,
		)
		.await;

		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			para_id_a,
			relay_parent_2,
			collator_a,
			HeadData(vec![0u8]),
		)
		.await;

		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			para_id_b,
			relay_parent_2,
			collator_b,
			HeadData(vec![1u8]),
		)
		.await;

		let relay_parent_3 = Hash::from_low_u64_be(relay_parent_2.to_low_u64_be() - 1);
		*test_state.claim_queue.get_mut(&CoreIndex(0)).unwrap() =
			VecDeque::from([para_id_a, para_id_a, para_id_b]);
		update_view(&mut virtual_overseer, &mut test_state, vec![(relay_parent_3, 3)]).await;

		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			para_id_b,
			relay_parent_3,
			collator_b,
			HeadData(vec![3u8]),
		)
		.await;
		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			para_id_a,
			relay_parent_3,
			collator_a,
			HeadData(vec![3u8]),
		)
		.await;

		// At this point the claim queue is satisfied and any advertisement at `relay_parent_4`
		// must be ignored

		let (candidate_a, _) =
			create_dummy_candidate_and_commitments(para_id_a, HeadData(vec![5u8]), relay_parent_3);
		let parent_head_data_a = HeadData(vec![5u8]);

		advertise_collation(
			&mut virtual_overseer,
			collator_a,
			AdvertisementPayload::v2(relay_parent_3, candidate_a.hash(), parent_head_data_a.hash()),
		)
		.await;

		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		let (candidate_b, _) =
			create_dummy_candidate_and_commitments(para_id_b, HeadData(vec![6u8]), relay_parent_3);
		let parent_head_data_b = HeadData(vec![6u8]);

		advertise_collation(
			&mut virtual_overseer,
			collator_b,
			AdvertisementPayload::v2(relay_parent_3, candidate_b.hash(), parent_head_data_b.hash()),
		)
		.await;

		// `CanSecond` shouldn't be sent as the advertisement should be ignored
		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		// At `relay_parent_6` the advertisement for `para_id_b` falls out of the view so a new one
		// can be accepted
		let relay_parent_6 = Hash::from_low_u64_be(relay_parent_3.to_low_u64_be() - 2);
		update_view(&mut virtual_overseer, &mut test_state, vec![(relay_parent_6, 6)]).await;

		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			para_id_a,
			relay_parent_6,
			collator_a,
			HeadData(vec![3u8]),
		)
		.await;

		virtual_overseer
	});
}

#[test]
fn collation_fetching_fairness_handles_old_claims() {
	let mut test_state = TestState::with_shared_core();

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, keystore } = test_harness;

		let pair_a = CollatorPair::generate().0;
		let collator_a = PeerId::random();
		let para_id_a = test_state.chain_ids[0];

		let pair_b = CollatorPair::generate().0;
		let collator_b = PeerId::random();
		let para_id_b = test_state.chain_ids[1];

		let relay_parent_2 = Hash::from_low_u64_be(test_state.relay_parent.to_low_u64_be() - 1);

		*test_state.claim_queue.get_mut(&CoreIndex(0)).unwrap() =
			VecDeque::from([para_id_a, para_id_b, para_id_a]);

		update_view(&mut virtual_overseer, &mut test_state, vec![(relay_parent_2, 2)]).await;

		connect_and_declare_collator(
			&mut virtual_overseer,
			collator_a,
			pair_a.clone(),
			para_id_a,
			CollationVersion::V2,
		)
		.await;

		connect_and_declare_collator(
			&mut virtual_overseer,
			collator_b,
			pair_b.clone(),
			para_id_b,
			CollationVersion::V2,
		)
		.await;

		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			para_id_a,
			relay_parent_2,
			collator_a,
			HeadData(vec![0u8]),
		)
		.await;

		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			para_id_b,
			relay_parent_2,
			collator_b,
			HeadData(vec![1u8]),
		)
		.await;

		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			para_id_a,
			relay_parent_2,
			collator_a,
			HeadData(vec![2u8]),
		)
		.await;

		let relay_parent_3 = Hash::from_low_u64_be(relay_parent_2.to_low_u64_be() - 1);

		*test_state.claim_queue.get_mut(&CoreIndex(0)).unwrap() =
			VecDeque::from([para_id_b, para_id_a, para_id_b]);
		update_view(&mut virtual_overseer, &mut test_state, vec![(relay_parent_3, 3)]).await;

		// nothing is advertised here
		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		let relay_parent_4 = Hash::from_low_u64_be(relay_parent_3.to_low_u64_be() - 1);

		*test_state.claim_queue.get_mut(&CoreIndex(0)).unwrap() =
			VecDeque::from([para_id_a, para_id_b, para_id_a]);
		update_view(&mut virtual_overseer, &mut test_state, vec![(relay_parent_4, 4)]).await;

		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			para_id_b,
			relay_parent_4,
			collator_b,
			HeadData(vec![3u8]),
		)
		.await;

		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			para_id_a,
			relay_parent_4,
			collator_a,
			HeadData(vec![4u8]),
		)
		.await;

		// At this point the claim queue is satisfied and any advertisement at `relay_parent_4`
		// must be ignored

		// Advertisement for `para_id_a` at `relay_parent_4` which must be ignored
		let (candidate_a, _) =
			create_dummy_candidate_and_commitments(para_id_a, HeadData(vec![5u8]), relay_parent_4);
		let parent_head_data_a = HeadData(vec![5u8]);

		advertise_collation(
			&mut virtual_overseer,
			collator_a,
			AdvertisementPayload::v2(relay_parent_4, candidate_a.hash(), parent_head_data_a.hash()),
		)
		.await;

		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		// Advertisement for `para_id_b` at `relay_parent_4` which must be ignored
		let (candidate_b, _) =
			create_dummy_candidate_and_commitments(para_id_b, HeadData(vec![6u8]), relay_parent_4);
		let parent_head_data_b = HeadData(vec![6u8]);

		advertise_collation(
			&mut virtual_overseer,
			collator_b,
			AdvertisementPayload::v2(relay_parent_4, candidate_b.hash(), parent_head_data_b.hash()),
		)
		.await;

		// `CanSecond` shouldn't be sent as the advertisement should be ignored
		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		virtual_overseer
	});
}

#[test]
fn claims_below_are_counted_correctly() {
	let mut test_state = TestState::with_one_scheduled_para();

	// CQ length 3 with 2-block ancestry: hash_a (block 0) → hash_b (block 1, leaf).
	// hash_a at offset=1 gets valid_len=2, hash_b at offset=0 gets valid_len=3.
	// Total capacity = 3. We do 2 ads at hash_a + 1 at hash_b = 3, then 4th rejected.
	let mut claim_queue = BTreeMap::new();
	claim_queue.insert(
		CoreIndex(0),
		VecDeque::from_iter(
			[
				ParaId::from(test_state.chain_ids[0]),
				ParaId::from(test_state.chain_ids[0]),
				ParaId::from(test_state.chain_ids[0]),
			]
			.into_iter(),
		),
	);
	test_state.claim_queue = claim_queue;
	test_state.scheduling_lookahead = 3;

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, keystore } = test_harness;

		let hash_a = Hash::from_low_u64_be(test_state.relay_parent.to_low_u64_be() - 1); // block 0
		let hash_b = Hash::from_low_u64_be(hash_a.to_low_u64_be() - 1); // block 1 (leaf)

		let pair_a = CollatorPair::generate().0;
		let collator_a = PeerId::random();
		let para_id_a = test_state.chain_ids[0];

		update_view(&mut virtual_overseer, &mut test_state, vec![(hash_b, 1)]).await;

		connect_and_declare_collator(
			&mut virtual_overseer,
			collator_a,
			pair_a.clone(),
			para_id_a,
			CollationVersion::V2,
		)
		.await;

		// Two collations at hash_a claim 2 of 3 CQ slots
		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			ParaId::from(test_state.chain_ids[0]),
			hash_a,
			collator_a,
			HeadData(vec![0u8]),
		)
		.await;

		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			ParaId::from(test_state.chain_ids[0]),
			hash_a,
			collator_a,
			HeadData(vec![1u8]),
		)
		.await;

		// Collation at hash_b (leaf) claims the last slot
		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			ParaId::from(test_state.chain_ids[0]),
			hash_b,
			collator_a,
			HeadData(vec![2u8]),
		)
		.await;

		// 4th collation at hash_b should be ignored because the claim queue is full
		let (ignored_candidate, _) =
			create_dummy_candidate_and_commitments(para_id_a, HeadData(vec![3u8]), hash_b);

		advertise_collation(
			&mut virtual_overseer,
			collator_a,
			AdvertisementPayload::v2(hash_b, ignored_candidate.hash(), Hash::random()),
		)
		.await;

		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		virtual_overseer
	});
}

#[test]
fn claims_above_are_counted_correctly() {
	let mut test_state = TestState::with_one_scheduled_para();

	// CQ length 3 with 2-block ancestry: hash_a (block 0) → hash_b (block 1, leaf).
	// hash_a at offset=1 gets valid_len=2, hash_b at offset=0 gets valid_len=3.
	// Total capacity = 3. We do 2 ads at hash_b + 1 at hash_a = 3, then 4th rejected.
	let mut claim_queue = BTreeMap::new();
	claim_queue.insert(
		CoreIndex(0),
		VecDeque::from_iter(
			[
				ParaId::from(test_state.chain_ids[0]),
				ParaId::from(test_state.chain_ids[0]),
				ParaId::from(test_state.chain_ids[0]),
			]
			.into_iter(),
		),
	);
	test_state.claim_queue = claim_queue;
	test_state.scheduling_lookahead = 3;

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, keystore } = test_harness;

		let hash_a = Hash::from_low_u64_be(test_state.relay_parent.to_low_u64_be() - 1); // block 0
		let hash_b = Hash::from_low_u64_be(hash_a.to_low_u64_be() - 1); // block 1 (leaf)

		let pair_a = CollatorPair::generate().0;
		let collator_a = PeerId::random();
		let para_id_a = test_state.chain_ids[0];

		update_view(&mut virtual_overseer, &mut test_state, vec![(hash_b, 1)]).await;

		connect_and_declare_collator(
			&mut virtual_overseer,
			collator_a,
			pair_a.clone(),
			para_id_a,
			CollationVersion::V2,
		)
		.await;

		// Two collations at hash_b (leaf) claim 2 of 3 CQ slots
		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			ParaId::from(test_state.chain_ids[0]),
			hash_b,
			collator_a,
			HeadData(vec![0u8]),
		)
		.await;

		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			ParaId::from(test_state.chain_ids[0]),
			hash_b,
			collator_a,
			HeadData(vec![1u8]),
		)
		.await;

		// Collation at hash_a claims the last slot
		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			ParaId::from(test_state.chain_ids[0]),
			hash_a,
			collator_a,
			HeadData(vec![0u8]),
		)
		.await;

		// Another collation at hash_a should be ignored because the claim queue is full
		let (ignored_candidate, _) =
			create_dummy_candidate_and_commitments(para_id_a, HeadData(vec![2u8]), hash_a);

		advertise_collation(
			&mut virtual_overseer,
			collator_a,
			AdvertisementPayload::v2(hash_a, ignored_candidate.hash(), Hash::random()),
		)
		.await;

		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		// Same for hash_b
		let (ignored_candidate, _) =
			create_dummy_candidate_and_commitments(para_id_a, HeadData(vec![3u8]), hash_b);

		advertise_collation(
			&mut virtual_overseer,
			collator_a,
			AdvertisementPayload::v2(hash_b, ignored_candidate.hash(), Hash::random()),
		)
		.await;

		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		virtual_overseer
	});
}

#[test]
fn claim_fills_last_free_slot() {
	let mut test_state = TestState::with_one_scheduled_para();

	// CQ length 3 to cover the depth of the ancestry.
	// Path: hash_a(0) → hash_b(1) → hash_c(2, leaf). One ad per relay parent = 3 = capacity.
	// hash_a: offset=2, valid_len=1; hash_b: offset=1, valid_len=2; hash_c: offset=0, valid_len=3.
	let mut claim_queue = BTreeMap::new();
	claim_queue.insert(
		CoreIndex(0),
		VecDeque::from_iter(
			[
				ParaId::from(test_state.chain_ids[0]),
				ParaId::from(test_state.chain_ids[0]),
				ParaId::from(test_state.chain_ids[0]),
			]
			.into_iter(),
		),
	);
	test_state.claim_queue = claim_queue;
	test_state.scheduling_lookahead = 3;

	test_harness(ReputationAggregator::new(|_| true), HashSet::new(), |test_harness| async move {
		let TestHarness { mut virtual_overseer, keystore } = test_harness;

		let hash_a = Hash::from_low_u64_be(test_state.relay_parent.to_low_u64_be() - 1); // block 0
		let hash_b = Hash::from_low_u64_be(hash_a.to_low_u64_be() - 1); // block 1
		let hash_c = Hash::from_low_u64_be(hash_b.to_low_u64_be() - 1); // block 2

		let pair_a = CollatorPair::generate().0;
		let collator_a = PeerId::random();
		let para_id_a = test_state.chain_ids[0];

		update_view(&mut virtual_overseer, &mut test_state, vec![(hash_c, 2)]).await;

		connect_and_declare_collator(
			&mut virtual_overseer,
			collator_a,
			pair_a.clone(),
			para_id_a,
			CollationVersion::V2,
		)
		.await;

		// A collation at hash_a claims its spot
		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			ParaId::from(test_state.chain_ids[0]),
			hash_a,
			collator_a,
			HeadData(vec![0u8]),
		)
		.await;

		// Collation at hash_b claims its own spot
		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			ParaId::from(test_state.chain_ids[0]),
			hash_b,
			collator_a,
			HeadData(vec![3u8]),
		)
		.await;

		// Collation at hash_c claims its own spot
		submit_second_and_assert(
			&mut virtual_overseer,
			keystore.clone(),
			ParaId::from(test_state.chain_ids[0]),
			hash_c,
			collator_a,
			HeadData(vec![2u8]),
		)
		.await;

		// Another Collation at hash_a should be ignored because the claim queue is satisfied
		let (ignored_candidate, _) =
			create_dummy_candidate_and_commitments(para_id_a, HeadData(vec![3u8]), hash_a);

		advertise_collation(
			&mut virtual_overseer,
			collator_a,
			AdvertisementPayload::v2(hash_a, ignored_candidate.hash(), Hash::random()),
		)
		.await;

		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		// Same for hash_b
		let (ignored_candidate, _) =
			create_dummy_candidate_and_commitments(para_id_a, HeadData(vec![4u8]), hash_b);

		advertise_collation(
			&mut virtual_overseer,
			collator_a,
			AdvertisementPayload::v2(hash_b, ignored_candidate.hash(), Hash::random()),
		)
		.await;

		test_helpers::Yield::new().await;
		assert_matches!(virtual_overseer.recv().now_or_never(), None);

		virtual_overseer
	});
}

mod ah_stop_gap {
	use super::*;

	#[test]
	fn permissionless_collators_are_rejected_when_connection_limit_is_hit() {
		let mut test_state = TestState::with_one_scheduled_para();
		let invulnerable_collator = PeerId::random();
		let invulnerables = HashSet::from_iter([invulnerable_collator]);
		let invulnerables_len = invulnerables.len();

		let mut claim_queue = BTreeMap::new();
		claim_queue.insert(
			CoreIndex(0),
			VecDeque::from_iter(
				[ASSET_HUB_PARA_ID, ASSET_HUB_PARA_ID, ASSET_HUB_PARA_ID].into_iter(),
			),
		);
		test_state.claim_queue = claim_queue;

		test_harness(
			ReputationAggregator::new(|_| true),
			invulnerables,
			|test_harness| async move {
				let TestHarness { mut virtual_overseer, keystore: _ } = test_harness;

				let head = Hash::from_low_u64_be(test_state.relay_parent.to_low_u64_be() - 1);
				update_view(&mut virtual_overseer, &mut test_state, vec![(head, 2)]).await;

				// collators are accepted up to the connection limit AND a slot for the
				// invulnerables is kept
				let connection_limit =
					MAX_AUTHORITY_INCOMING_STREAMS - 10 - invulnerables_len as u32;
				for _ in 0..connection_limit {
					let pair = CollatorPair::generate().0;
					let collator = PeerId::random();

					connect_and_declare_collator(
						&mut virtual_overseer,
						collator,
						pair.clone(),
						ASSET_HUB_PARA_ID,
						CollationVersion::V2,
					)
					.await;
				}

				// connecting one more permissionless collator should be rejected
				{
					let collator = PeerId::random();
					overseer_send(
						&mut virtual_overseer,
						CollatorProtocolMessage::NetworkBridgeUpdate(
							NetworkBridgeEvent::PeerConnected(
								collator,
								ObservedRole::Full,
								CollationVersion::V2.into(),
								None,
							),
						),
					)
					.await;
					assert_matches!(
						overseer_recv(&mut virtual_overseer).await,
						AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::DisconnectPeers(peers, peer_set)
						) => {
							assert_eq!(peers, vec![collator]);
							assert_eq!(peer_set, PeerSet::Collation);
						}
					);
				}

				// connecting an invulnerable collator should succeed
				connect_and_declare_collator(
					&mut virtual_overseer,
					invulnerable_collator,
					CollatorPair::generate().0,
					ASSET_HUB_PARA_ID,
					CollationVersion::V2,
				)
				.await;

				test_helpers::Yield::new().await;
				assert_matches!(virtual_overseer.recv().now_or_never(), None);

				virtual_overseer
			},
		);
	}

	#[test]
	fn invulnerable_collations_are_preferred_over_permissionless_ones() {
		let mut test_state = TestState::with_one_scheduled_para();
		let invulnerable_collator = PeerId::random();
		let invulnerables = HashSet::from_iter([invulnerable_collator]);

		let mut claim_queue = BTreeMap::new();
		claim_queue.insert(
			CoreIndex(0),
			VecDeque::from_iter(
				[ASSET_HUB_PARA_ID, ASSET_HUB_PARA_ID, ASSET_HUB_PARA_ID].into_iter(),
			),
		);
		test_state.claim_queue = claim_queue;
		test_state.chain_ids = vec![ASSET_HUB_PARA_ID];

		test_harness(
			ReputationAggregator::new(|_| true),
			invulnerables,
			|test_harness| async move {
				let TestHarness { mut virtual_overseer, keystore } = test_harness;

				let head = test_state.relay_parent;

				update_view(&mut virtual_overseer, &mut test_state, vec![(head, 1)]).await;

				let permissionless_collator = PeerId::random();
				connect_and_declare_collator(
					&mut virtual_overseer,
					permissionless_collator,
					CollatorPair::generate().0,
					ASSET_HUB_PARA_ID,
					CollationVersion::V2,
				)
				.await;

				// connecting an invulnerable collator should succeed
				connect_and_declare_collator(
					&mut virtual_overseer,
					invulnerable_collator,
					CollatorPair::generate().0,
					ASSET_HUB_PARA_ID,
					CollationVersion::V2,
				)
				.await;

				// permissionless makes an advertisement
				let permissionless_head_data = HeadData(vec![0u8]);
				let permissionless_head_data_hash = permissionless_head_data.hash();
				let (permissionless_candidate, permissionless_commitments) =
					create_dummy_candidate_and_commitments(
						ASSET_HUB_PARA_ID,
						permissionless_head_data,
						head,
					);
				advertise_collation(
					&mut virtual_overseer,
					permissionless_collator,
					AdvertisementPayload::v2(
						head,
						permissionless_candidate.hash(),
						permissionless_head_data_hash,
					),
				)
				.await;

				// invulnerable makes an advertisement and it's fetched
				submit_second_and_assert(
					&mut virtual_overseer,
					keystore.clone(),
					ASSET_HUB_PARA_ID,
					head,
					invulnerable_collator,
					HeadData(vec![1 as u8]),
				)
				.await;

				// sleep, to kick off held off processing
				std::thread::sleep(HOLD_OFF_DURATION_DEFAULT_VALUE * 2);

				// the one from the permissionless is also fetched
				assert_matches!(
					overseer_recv(&mut virtual_overseer).await,
					AllMessages::CandidateBacking(
						CandidateBackingMessage::CanSecond(request, tx),
					) => {
						assert_eq!(request.candidate_hash, permissionless_candidate.hash());
						assert_eq!(request.candidate_para_id, ASSET_HUB_PARA_ID);
						assert_eq!(request.parent_head_data_hash, permissionless_head_data_hash);
						tx.send(true).expect("receiving side should be alive");
					}
				);

				let response_channel = assert_fetch_collation_request(
					&mut virtual_overseer,
					head,
					ASSET_HUB_PARA_ID,
					Some(permissionless_candidate.hash()),
				)
				.await;

				let pov = PoV { block_data: BlockData(vec![1]) };

				send_collation_and_assert_processing(
					&mut virtual_overseer,
					keystore,
					head,
					ASSET_HUB_PARA_ID,
					permissionless_collator,
					response_channel,
					permissionless_candidate,
					permissionless_commitments,
					pov,
				)
				.await;

				test_helpers::Yield::new().await;
				assert_matches!(virtual_overseer.recv().now_or_never(), None);

				virtual_overseer
			},
		);
	}

	#[test]
	fn permissionless_claims_the_whole_cq() {
		let mut test_state = TestState::with_one_scheduled_para();
		let permissionless_collator = PeerId::random();
		let invulnerable_collator = PeerId::random();
		let invulnerables = HashSet::from_iter([invulnerable_collator]);

		let mut claim_queue = BTreeMap::new();
		claim_queue.insert(
			CoreIndex(0),
			VecDeque::from_iter(
				[ASSET_HUB_PARA_ID, ASSET_HUB_PARA_ID, ASSET_HUB_PARA_ID].into_iter(),
			),
		);
		test_state.claim_queue = claim_queue;
		test_state.chain_ids = vec![ASSET_HUB_PARA_ID];

		test_harness(
			ReputationAggregator::new(|_| true),
			invulnerables,
			|test_harness| async move {
				let TestHarness { mut virtual_overseer, keystore } = test_harness;

				let head = test_state.relay_parent;
				update_view(&mut virtual_overseer, &mut test_state, vec![(head, 1)]).await;

				connect_and_declare_collator(
					&mut virtual_overseer,
					permissionless_collator,
					CollatorPair::generate().0,
					ASSET_HUB_PARA_ID,
					CollationVersion::V2,
				)
				.await;

				connect_and_declare_collator(
					&mut virtual_overseer,
					invulnerable_collator,
					CollatorPair::generate().0,
					ASSET_HUB_PARA_ID,
					CollationVersion::V2,
				)
				.await;

				let mut permissionless_candidates = VecDeque::new();

				// permissionless makes three advertisements, aiming to claim all the slots from CQ
				for i in 0..3 {
					let permissionless_head_data = HeadData(vec![i as u8]);
					let permissionless_head_data_hash = permissionless_head_data.hash();
					let (permissionless_candidate, permissionless_commitments) =
						create_dummy_candidate_and_commitments(
							ASSET_HUB_PARA_ID,
							permissionless_head_data,
							head,
						);

					advertise_collation(
						&mut virtual_overseer,
						permissionless_collator,
						AdvertisementPayload::v2(
							head,
							permissionless_candidate.hash(),
							permissionless_head_data_hash,
						),
					)
					.await;

					permissionless_candidates.push_back((
						permissionless_candidate.hash(),
						permissionless_candidate,
						permissionless_head_data_hash,
						permissionless_commitments,
						head,
					));
				}

				// nothing happens because the advertisements are held off
				test_helpers::Yield::new().await;
				assert_matches!(virtual_overseer.recv().now_or_never(), None);

				// invulnerable makes an advertisement and it's fetched
				submit_second_and_assert(
					&mut virtual_overseer,
					keystore.clone(),
					ASSET_HUB_PARA_ID,
					head,
					invulnerable_collator,
					HeadData(vec![3 as u8]),
				)
				.await;

				// sleep to kick off held off processing
				std::thread::sleep(HOLD_OFF_DURATION_DEFAULT_VALUE * 2);

				let mut can_second_count = 0;
				let mut collation_fetching_count = 0;
				let mut response_chan = None;
				// We should get two `CanSecond` (for the first and second candidates) messages and
				// one `CollationFetchingV2` (for the first candidate). Note that there is only one
				// fetch request because the fetches are sequential. The third candidate is ignored
				// because there is no free slot in the claim queue.
				loop {
					match overseer_recv(&mut virtual_overseer).await {
						AllMessages::CandidateBacking(CandidateBackingMessage::CanSecond(
							request,
							tx,
						)) => {
							let (_, _, permissionless_head_data_hash, _, _) =
								permissionless_candidates
									.get(can_second_count)
									.expect("there are three candidates")
									.clone();
							assert_eq!(request.candidate_para_id, ASSET_HUB_PARA_ID);
							assert_eq!(
								request.parent_head_data_hash,
								permissionless_head_data_hash
							);
							assert_eq!(
								request.candidate_hash,
								permissionless_candidates
									.get(can_second_count)
									.cloned()
									.map(|(candidate_hash, _, _, _, _)| candidate_hash)
									.expect("there should be three candidates")
							);
							tx.send(true).expect("receiving side should be alive");
							can_second_count += 1;
						},
						AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendRequests(
							reqs,
							IfDisconnected::ImmediateError,
						)) => {
							let scheduling_parent = head;
							let req = reqs
								.into_iter()
								.next()
								.expect("there should be exactly one request");

							assert_matches!(
								req,
								Requests::CollationFetchingV2(req) => {
									let payload = req.payload;
									assert_eq!(payload.scheduling_parent, scheduling_parent);
									assert_eq!(payload.para_id, ASSET_HUB_PARA_ID);
									assert_eq!(
										payload.candidate_hash,
										permissionless_candidates
										.get(collation_fetching_count)
										.cloned()
										.map(|(candidate_hash, _, _, _, _)| candidate_hash)
										.expect("there should be three candidates")
									);
									response_chan = Some(req.pending_response);
									collation_fetching_count += 1;
								}
							);
						},
						msg => {
							assert!(false, "Unexpected message received: {:?}", msg);
						},
					}

					// sanity check the messages count
					assert!(collation_fetching_count <= 1);
					assert!(can_second_count <= 2);

					if collation_fetching_count == 1 && can_second_count == 2 {
						break;
					}
				}

				// Respond to the `CollationFetchingV2` made in the loop above
				{
					let response_channel =
						response_chan.expect("there should be a response channel");
					let pov = PoV { block_data: BlockData(vec![1]) };
					let (_, permissionless_candidate, _, permissionless_commitments, head) =
						permissionless_candidates.pop_front().expect("supe").clone();

					send_collation_and_assert_processing(
						&mut virtual_overseer,
						keystore.clone(),
						head,
						ASSET_HUB_PARA_ID,
						permissionless_collator,
						response_channel,
						permissionless_candidate,
						permissionless_commitments,
						pov,
					)
					.await;
				}

				// Since the first fetch request is complete, the second candidate will be fetched
				// now. Handle it.
				{
					let (
						permissionless_candidate_hash,
						permissionless_candidate,
						_,
						permissionless_commitments,
						head,
					) = permissionless_candidates.pop_front().expect("supe").clone();

					let response_channel = assert_fetch_collation_request(
						&mut virtual_overseer,
						head,
						ASSET_HUB_PARA_ID,
						Some(permissionless_candidate_hash),
					)
					.await;

					let pov = PoV { block_data: BlockData(vec![1]) };

					send_collation_and_assert_processing(
						&mut virtual_overseer,
						keystore.clone(),
						head,
						ASSET_HUB_PARA_ID,
						permissionless_collator,
						response_channel,
						permissionless_candidate,
						permissionless_commitments,
						pov,
					)
					.await;
				}

				// activate new relay parent
				let head = Hash::from_low_u64_be(head.to_low_u64_be() - 1);
				update_view(&mut virtual_overseer, &mut test_state, vec![(head, 2)]).await;

				// The race begins again. The permissionless sends another advertisement, which
				// will get held off. Sending more advertisements is pointless since there is
				// just one free slot in CQ.
				{
					let permissionless_head_data = HeadData(vec![4u8]);
					let permissionless_head_data_hash = permissionless_head_data.hash();
					let (permissionless_candidate, _) = create_dummy_candidate_and_commitments(
						ASSET_HUB_PARA_ID,
						permissionless_head_data,
						head,
					);

					advertise_collation(
						&mut virtual_overseer,
						permissionless_collator,
						AdvertisementPayload::v2(
							head,
							permissionless_candidate.hash(),
							permissionless_head_data_hash,
						),
					)
					.await;
				}

				// nothing happens because the advertisements are heldoff
				test_helpers::Yield::new().await;
				assert_matches!(virtual_overseer.recv().now_or_never(), None);

				// invulnerable makes an advertisement and it's fetched
				submit_second_and_assert(
					&mut virtual_overseer,
					keystore.clone(),
					ASSET_HUB_PARA_ID,
					head,
					invulnerable_collator,
					HeadData(vec![5 as u8]),
				)
				.await;

				// This pattern can continue indefinitely but for brevity let's stop here. The
				// takeaway is that initially a permissionless collator might manage to fill in the
				// claim queue but after that invulnerable one will be preferred thanks to the hold
				// off mechanism. For claim queue with size 3 this is harmless.

				test_helpers::Yield::new().await;
				assert_matches!(virtual_overseer.recv().now_or_never(), None);

				virtual_overseer
			},
		);
	}

	#[test]
	fn permissionless_are_held_off_only_once() {
		let mut test_state = TestState::with_one_scheduled_para();
		let invulnerable_collator = PeerId::random();
		let invulnerables = HashSet::from_iter([invulnerable_collator]);

		let mut claim_queue = BTreeMap::new();
		claim_queue.insert(
			CoreIndex(0),
			VecDeque::from_iter(
				[ASSET_HUB_PARA_ID, ASSET_HUB_PARA_ID, ASSET_HUB_PARA_ID].into_iter(),
			),
		);
		test_state.claim_queue = claim_queue;
		test_state.chain_ids = vec![ASSET_HUB_PARA_ID];

		test_harness(
			ReputationAggregator::new(|_| true),
			invulnerables,
			|test_harness| async move {
				let TestHarness { mut virtual_overseer, keystore } = test_harness;

				let head = test_state.relay_parent;

				update_view(&mut virtual_overseer, &mut test_state, vec![(head, 1)]).await;

				let permissionless_collator = PeerId::random();
				connect_and_declare_collator(
					&mut virtual_overseer,
					permissionless_collator,
					CollatorPair::generate().0,
					ASSET_HUB_PARA_ID,
					CollationVersion::V2,
				)
				.await;

				// permissionless makes an advertisement
				let permissionless_head_data = HeadData(vec![0u8]);
				let permissionless_head_data_hash = permissionless_head_data.hash();
				let (permissionless_candidate, permissionless_commitments) =
					create_dummy_candidate_and_commitments(
						ASSET_HUB_PARA_ID,
						permissionless_head_data,
						head,
					);
				advertise_collation(
					&mut virtual_overseer,
					permissionless_collator,
					AdvertisementPayload::v2(
						head,
						permissionless_candidate.hash(),
						permissionless_head_data_hash,
					),
				)
				.await;

				// nothing happens yet
				test_helpers::Yield::new().await;
				assert_matches!(virtual_overseer.recv().now_or_never(), None);

				// sleep, to kick off held off processing
				std::thread::sleep(HOLD_OFF_DURATION_DEFAULT_VALUE * 2);

				// the one from the permissionless is also fetched
				assert_matches!(
					overseer_recv(&mut virtual_overseer).await,
					AllMessages::CandidateBacking(
						CandidateBackingMessage::CanSecond(request, tx),
					) => {
						assert_eq!(request.candidate_hash, permissionless_candidate.hash());
						assert_eq!(request.candidate_para_id, ASSET_HUB_PARA_ID);
						assert_eq!(request.parent_head_data_hash, permissionless_head_data_hash);
						tx.send(true).expect("receiving side should be alive");
					}
				);

				let response_channel = assert_fetch_collation_request(
					&mut virtual_overseer,
					head,
					ASSET_HUB_PARA_ID,
					Some(permissionless_candidate.hash()),
				)
				.await;

				let pov = PoV { block_data: BlockData(vec![1]) };

				send_collation_and_assert_processing(
					&mut virtual_overseer,
					keystore.clone(),
					head,
					ASSET_HUB_PARA_ID,
					permissionless_collator,
					response_channel,
					permissionless_candidate,
					permissionless_commitments,
					pov,
				)
				.await;

				// the rest of the permissionless advertisements for this RP should be processed
				// immediately
				submit_second_and_assert(
					&mut virtual_overseer,
					keystore.clone(),
					ASSET_HUB_PARA_ID,
					head,
					permissionless_collator,
					HeadData(vec![1 as u8]),
				)
				.await;

				submit_second_and_assert(
					&mut virtual_overseer,
					keystore,
					ASSET_HUB_PARA_ID,
					head,
					permissionless_collator,
					HeadData(vec![2 as u8]),
				)
				.await;

				test_helpers::Yield::new().await;
				assert_matches!(virtual_overseer.recv().now_or_never(), None);

				virtual_overseer
			},
		);
	}
}

/// Verify that `descriptor_version_sanity_check_with_params` checks the
/// scheduling session (not the relay-parent session) for V3 descriptors
/// where the two sessions differ (cross-session relay parent).
#[test]
fn v3_sanity_check_uses_scheduling_session_not_relay_parent_session() {
	let relay_parent = Hash::repeat_byte(1);
	let scheduling_parent = Hash::repeat_byte(2);

	let relay_parent_session: SessionIndex = 4;
	let scheduling_session_offset: u8 = 1;
	// scheduling_session = relay_parent_session + offset = 5
	let scheduling_session = relay_parent_session + scheduling_session_offset as SessionIndex;

	let core = CoreIndex(0);

	let mut descriptor = make_valid_candidate_descriptor_v3(
		1.into(),
		relay_parent,
		core,
		relay_parent_session,
		relay_parent_session,
		Hash::zero(),
		Hash::zero(),
		Hash::zero(),
		Hash::zero(),
		Hash::zero(),
		scheduling_parent,
	);
	descriptor.set_scheduling_session_offset(scheduling_session_offset);

	// Sanity: verify the descriptor is V3 and sessions are as expected.
	assert_eq!(descriptor.version(), CandidateDescriptorVersion::V3);
	assert_eq!(descriptor.session_index(), Some(relay_parent_session));
	assert_eq!(descriptor.scheduling_session(), Some(scheduling_session));

	// The check must pass when expected_session matches the scheduling session.
	assert!(descriptor_version_sanity_check_with_params(
		&descriptor,
		core,
		scheduling_session,
		CollationVersion::V3,
	)
	.is_ok());

	// The check must fail when expected_session is the relay-parent session
	// (which differs from the scheduling session for cross-session V3 candidates).
	assert_matches!(
		descriptor_version_sanity_check_with_params(
			&descriptor,
			core,
			relay_parent_session,
			CollationVersion::V3,
		),
		Err(SecondingError::InvalidSessionIndex(got, expected)) => {
			assert_eq!(got, scheduling_session);
			assert_eq!(expected, relay_parent_session);
		}
	);
}

/// Verify that V2 descriptors still check session_index correctly (V2 has no
/// scheduling_session_offset, so session_index == scheduling_session).
#[test]
fn v2_sanity_check_session_index_unchanged() {
	let relay_parent = Hash::repeat_byte(1);
	let core = CoreIndex(0);
	let session: SessionIndex = 5;

	let mut descriptor = dummy_committed_candidate_receipt_v2(relay_parent);
	descriptor.descriptor.set_core_index(core);
	descriptor.descriptor.set_session_index(session);

	assert_eq!(descriptor.descriptor.version(), CandidateDescriptorVersion::V2);

	// Passes with matching session.
	assert!(descriptor_version_sanity_check_with_params(
		&descriptor.descriptor,
		core,
		session,
		CollationVersion::V2,
	)
	.is_ok());

	// Fails with wrong session.
	assert_matches!(
		descriptor_version_sanity_check_with_params(
			&descriptor.descriptor,
			core,
			session + 1,
			CollationVersion::V2,
		),
		Err(SecondingError::InvalidSessionIndex(..))
	);
}
