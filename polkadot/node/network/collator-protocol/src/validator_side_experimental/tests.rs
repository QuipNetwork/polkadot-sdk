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

use crate::validator_side_experimental::{
	common::{
		Advertisement, CollationFetchError, Score, CONNECTED_PEERS_PARA_LIMIT, FAILED_FETCH_SLASH,
		INVALID_COLLATION_SLASH, MAX_STARTUP_ANCESTRY_LOOKBACK, VALID_INCLUDED_CANDIDATE_BUMP,
	},
	peer_manager::{Backend, ReputationUpdate},
};
use rstest::rstest;

use super::*;
use assert_matches::assert_matches;
use async_trait::async_trait;
use codec::Encode;
use futures::channel::mpsc::UnboundedReceiver;
use polkadot_node_clock::{Clock as _, MockClock};
use polkadot_node_network_protocol::{
	peer_set::{CollationVersion, PeerSet},
	request_response::{
		outgoing::RequestError, v1::CollationFetchingResponse, Recipient, Requests, ResponseSender,
	},
	v4_collation::CandidateFingerprint,
	OurView,
};
use polkadot_node_primitives::{
	BlockData, PoV, SignedFullStatement, Statement, UncheckedSignedFullStatement,
};
use polkadot_node_subsystem::messages::{
	AllMessages, CanSecondRequest, CandidateBackingMessage, ChainApiMessage, IfDisconnected,
	KnownOutputHeads, NetworkBridgeTxMessage, ParentHeadData, ProspectiveParachainsMessage,
	ProspectiveValidationDataRequest, RuntimeApiMessage, RuntimeApiRequest,
};
use polkadot_node_subsystem_test_helpers::{mock::new_leaf, sender_receiver, TestSubsystemSender};
use polkadot_node_subsystem_util::TimeoutExt;
use polkadot_primitives::{
	node_features::{self, FeatureIndex},
	ApprovedPeerId, BlockNumber, CandidateDescriptorVersion,
	CandidateReceiptV2 as CandidateReceipt,
	CommittedCandidateReceiptV2 as CommittedCandidateReceipt, CoreIndex, GroupRotationInfo, Hash,
	HeadData, Header, Id as ParaId, MutateDescriptorV2, NodeFeatures, OccupiedCoreAssumption,
	PersistedValidationData, SessionIndex, SigningContext, UMPSignal, ValidatorId, ValidatorIndex,
	UMP_SEPARATOR,
};
use polkadot_primitives_test_helpers::{
	dummy_candidate_commitments, dummy_committed_candidate_receipt,
	dummy_committed_candidate_receipt_v2, make_valid_candidate_descriptor_v2,
};
use sc_network::{OutboundFailure, RequestFailure};
use sc_network_types::multihash::Multihash;
use sp_consensus_babe::digests::{PreDigest, SecondaryPlainPreDigest};
use sp_keyring::Sr25519Keyring;
use sp_keystore::Keystore;
use std::{
	collections::{BTreeMap, BTreeSet, HashMap, HashSet},
	ops::DerefMut,
	sync::{Arc, Mutex},
	time::Duration,
};

const TIMEOUT: Duration = Duration::from_millis(100);

fn peer_id(i: u8) -> PeerId {
	let data = [i; 32];

	PeerId::from_multihash(Multihash::wrap(0x0, &data).unwrap()).unwrap()
}

fn dummy_pvd() -> PersistedValidationData {
	PersistedValidationData {
		parent_head: HeadData(vec![7, 8, 9]),
		relay_parent_number: 5,
		max_pov_size: 1024,
		relay_parent_storage_root: Default::default(),
	}
}

fn dummy_pov() -> PoV {
	PoV { block_data: BlockData(vec![1]) }
}

fn dummy_candidate(
	relay_parent: Hash,
	para_id: ParaId,
	peer_id: PeerId,
	core: CoreIndex,
	session: SessionIndex,
	pvd_hash: Hash,
) -> (CommittedCandidateReceipt, Advertisement) {
	let mut ccr = dummy_committed_candidate_receipt_v2(relay_parent);
	ccr.descriptor.set_para_id(para_id);
	ccr.descriptor.set_persisted_validation_data_hash(pvd_hash);
	ccr.descriptor.set_core_index(core);
	ccr.descriptor.set_session_index(session);

	let receipt = ccr.to_plain();
	let prospective_candidate = Some(ProspectiveCandidate::ByHash {
		candidate_hash: receipt.hash(),
		parent_head_data_hash: dummy_pvd().parent_head.hash(),
	});

	(
		ccr,
		Advertisement {
			peer_id,
			para_id,
			scheduling_parent: relay_parent,
			prospective_candidate,
			advertised_descriptor_version: None,
		},
	)
}

fn dummy_candidate_v3(
	relay_parent: Hash,
	scheduling_parent: Hash,
	para_id: ParaId,
	peer_id: PeerId,
	core: CoreIndex,
	session: SessionIndex,
	pvd_hash: Hash,
) -> (CommittedCandidateReceipt, Advertisement) {
	let mut ccr = dummy_committed_candidate_receipt_v2(relay_parent);
	ccr.descriptor.set_para_id(para_id);
	ccr.descriptor.set_persisted_validation_data_hash(pvd_hash);
	ccr.descriptor.set_core_index(core);
	ccr.descriptor.set_session_index(session);
	ccr.descriptor.set_version(1);
	ccr.descriptor.set_scheduling_parent(scheduling_parent);

	let receipt = ccr.to_plain();
	let prospective_candidate = Some(ProspectiveCandidate::ByHash {
		candidate_hash: receipt.hash(),
		parent_head_data_hash: dummy_pvd().parent_head.hash(),
	});
	(
		ccr,
		Advertisement {
			peer_id,
			para_id,
			scheduling_parent,
			prospective_candidate,
			advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
		},
	)
}

/// A distinct V4 wire fingerprint per seed. The hashes only need to be unique and stable —
/// nothing in the accept path interprets them.
fn v4_fingerprint(seed: u8) -> protocol_v4::CandidateFingerprint {
	protocol_v4::CandidateFingerprint {
		output_head_data_hash: Hash::repeat_byte(seed),
		parent_head_data_hash: Hash::repeat_byte(seed.wrapping_add(0x80)),
		claim_queue_offset: 0,
	}
}

/// The stored-entry twin of a wire fingerprint.
fn v4_entry(fp: &CandidateFingerprint) -> ProspectiveCandidate {
	ProspectiveCandidate::ByOutputHead {
		output_head_data_hash: fp.output_head_data_hash,
		parent_head_data_hash: fp.parent_head_data_hash,
	}
}

#[derive(Clone)]
struct RelayParentInfo {
	number: BlockNumber,
	parent: Hash,
	session_index: SessionIndex,
	claim_queue: BTreeMap<CoreIndex, Vec<ParaId>>,
	assigned_core: CoreIndex,
}

#[derive(Clone)]
struct SessionInfo {
	validators: Vec<ValidatorId>,
	validator_groups: Vec<Vec<ValidatorIndex>>,
	group_rotation_info: GroupRotationInfo,
	scheduling_lookahead: u32,
	paras: Vec<ParaId>,
}

struct TestState {
	sender: TestSubsystemSender,
	recv: UnboundedReceiver<AllMessages>,
	rp_info: HashMap<Hash, RelayParentInfo>,
	session_info: HashMap<SessionIndex, SessionInfo>,
	buffered_msg: Option<AllMessages>,
	finalized_block: BlockNumber,
	// The key is the block at which it is included.
	candidates_pending_availability: HashMap<Hash, Vec<CommittedCandidateReceipt>>,
	candidate_nonce: u64,
	keystore: KeystorePtr,
	node_features: NodeFeatures,
	slot_overrides: HashMap<Hash, sp_consensus_slots::Slot>,
	pp_known_output_heads: KnownOutputHeads,
	// Shared by the subsystem state and the mock header responder: a single, frozen time
	// source so V3 scheduling-parent slot validation can't race a wall-clock slot boundary.
	clock: Arc<MockClock>,
}

impl Default for TestState {
	fn default() -> Self {
		sp_tracing::init_for_tests();

		let keystore = Arc::new(sc_keystore::LocalKeystore::in_memory());
		Keystore::sr25519_generate_new(
			&*keystore,
			polkadot_primitives::PARACHAIN_KEY_TYPE_ID,
			Some(&Sr25519Keyring::Alice.to_seed()),
		)
		.expect("Insert key into keystore");

		let mut rp_info = HashMap::new();

		let cq: BTreeMap<CoreIndex, Vec<ParaId>> =
			(1..3).map(|i| (CoreIndex::from(i), vec![600.into(), 600.into()])).collect();

		rp_info.insert(
			get_hash(10),
			RelayParentInfo {
				number: 10,
				parent: get_parent_hash(10),
				session_index: 1,
				claim_queue: {
					let mut cq = cq.clone();
					cq.insert(CoreIndex(0), vec![100.into(), 200.into(), 100.into()]);
					cq
				},
				assigned_core: CoreIndex(0),
			},
		);
		rp_info.insert(
			get_hash(9),
			RelayParentInfo {
				number: 9,
				parent: get_parent_hash(9),
				session_index: 1,
				claim_queue: {
					let mut cq = cq.clone();
					cq.insert(CoreIndex(0), vec![200.into(), 100.into(), 200.into()]);
					cq
				},
				assigned_core: CoreIndex(0),
			},
		);
		rp_info.insert(
			get_hash(8),
			RelayParentInfo {
				number: 8,
				parent: get_parent_hash(8),
				session_index: 1,
				claim_queue: {
					let mut cq = cq.clone();
					cq.insert(CoreIndex(0), vec![100.into(), 200.into(), 100.into()]);
					cq
				},
				assigned_core: CoreIndex(0),
			},
		);

		let mut session_info = HashMap::new();

		let validators = [
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
		]
		.iter()
		.map(|k| k.public().into())
		.collect();
		let validator_groups = vec![
			vec![ValidatorIndex(0), ValidatorIndex(1)],
			vec![ValidatorIndex(2), ValidatorIndex(3)],
			vec![ValidatorIndex(4)],
		];

		let group_rotation_info =
			GroupRotationInfo { session_start_block: 0, group_rotation_frequency: 100, now: 0 };
		session_info.insert(
			1,
			SessionInfo {
				validators,
				validator_groups,
				group_rotation_info,
				scheduling_lookahead: 3,
				paras: vec![100.into(), 200.into(), 600.into()],
			},
		);

		let (sender, recv) = sender_receiver();

		let mut node_features = NodeFeatures::EMPTY;
		node_features.resize(FeatureIndex::FirstUnassigned as usize, false);

		// Seed away from slot 0 so tests can reference the previous slot without underflow.
		let clock = Arc::new(MockClock::default());
		clock.advance_secs(60);

		Self {
			session_info,
			rp_info,
			buffered_msg: None,
			sender,
			recv,
			finalized_block: 0,
			candidates_pending_availability: HashMap::new(),
			candidate_nonce: 0,
			keystore,
			node_features,
			slot_overrides: HashMap::default(),
			pp_known_output_heads: HashMap::default(),
			clock,
		}
	}
}

impl TestState {
	fn set_candidates_pending_availability(
		&mut self,
		pending_candidates_info: HashMap<Hash, Vec<(ParaId, PeerId)>>,
	) {
		self.candidates_pending_availability = pending_candidates_info
			.into_iter()
			.map(|(key, info)| {
				(
					key,
					info.into_iter()
						.map(|(para_id, peer_id)| {
							let mut ccr = dummy_committed_candidate_receipt_v2(Hash::zero());
							ccr.descriptor.set_para_id(para_id);
							ccr.descriptor
								.set_pov_hash(Hash::from_low_u64_be(self.candidate_nonce));
							ccr.commitments.upward_messages.force_push(UMP_SEPARATOR);
							ccr.commitments.upward_messages.force_push(
								UMPSignal::ApprovedPeer(
									ApprovedPeerId::try_from(peer_id.to_bytes()).unwrap(),
								)
								.encode(),
							);
							self.candidate_nonce += 1;
							ccr
						})
						.collect(),
				)
			})
			.collect();
	}

	async fn assert_no_messages(&mut self) {
		assert!(self.buffered_msg.is_none());
		// Use a small timeout here because we expect this to be called after the future we're
		// testing resolved.
		assert!(self.recv.next().timeout(Duration::from_millis(10)).await.is_none());
	}

	async fn assert_peers_disconnected(
		&mut self,
		expected_peers: impl IntoIterator<Item = PeerId>,
	) {
		let msg = match self.buffered_msg.take() {
			Some(msg) => msg,
			None => self.timeout_recv().await,
		};
		assert_matches!(
			msg,
			AllMessages::NetworkBridgeTx(
				NetworkBridgeTxMessage::DisconnectPeers(peers, PeerSet::Collation)
			) if peers.clone().into_iter().collect::<BTreeSet<_>>() == expected_peers.into_iter().collect::<BTreeSet<_>>()
		);
	}

	async fn timeout_recv(&mut self) -> AllMessages {
		self.recv
			.next()
			.timeout(TIMEOUT)
			.await
			.expect("Receiver timed out")
			.expect("Sender dropped")
	}

	async fn handle_view_update(&mut self, active_leaves: Vec<Hash>) {
		if active_leaves.is_empty() {
			return;
		}

		for active in active_leaves.iter() {
			assert!(self.rp_info.contains_key(active));
		}

		let extra_msg = loop {
			let had_buffered_msg = self.buffered_msg.is_some();
			let msg = match self.buffered_msg.take() {
				Some(msg) => msg,
				None => {
					if let Some(Some(msg)) = self.recv.next().timeout(TIMEOUT).await {
						msg
					} else {
						break None;
					}
				},
			};

			match msg {
				AllMessages::ChainApi(ChainApiMessage::BlockHeader(rp, tx)) => {
					let slot = self.slot_overrides.get(&rp).copied().unwrap_or_else(|| {
						sp_consensus_slots::Slot::from_timestamp(
							sp_timestamp::Timestamp::new(
								self.clock.duration_since_epoch().as_millis() as u64,
							),
							sp_consensus_slots::SlotDuration::from_millis(
								polkadot_primitives::RELAY_CHAIN_SLOT_DURATION_MILLIS,
							),
						)
					});
					let pre_digest =
						sp_consensus_babe::digests::CompatibleDigestItem::babe_pre_digest(
							PreDigest::SecondaryPlain(SecondaryPlainPreDigest {
								authority_index: 0,
								slot,
							}),
						);
					tx.send(Ok(Some(
						self.rp_info
							.get(&rp)
							.map(|info| Header {
								parent_hash: info.parent,
								number: info.number,
								state_root: Hash::zero(),
								extrinsics_root: Hash::zero(),
								digest: sp_runtime::Digest { logs: vec![pre_digest] },
							})
							.unwrap(),
					)))
					.unwrap();
				},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_rp,
					RuntimeApiRequest::SchedulingLookahead(session_index, tx),
				)) => {
					let session_info = self.session_info.get(&session_index).unwrap();
					tx.send(Ok(session_info.scheduling_lookahead)).unwrap();
				},
				AllMessages::ChainApi(ChainApiMessage::Ancestors { hash, k, response_channel }) => {
					let rp_info = self.rp_info.get(&hash).unwrap();
					let mut current = Some(hash);
					let ancestors: Vec<Hash> = (1..=k as u32)
						.map(|i| rp_info.number.saturating_sub(i))
						.take_while(|n| *n > 0)
						.flat_map(|n| {
							let via_parent = current
								.and_then(|c| self.rp_info.get(&c))
								.map(|info| info.parent)
								.filter(|p| self.rp_info.get(&p).is_some_and(|pi| pi.number == n));
							current = via_parent.or_else(|| {
								self.rp_info
									.iter()
									.find(|(_, info)| info.number == n)
									.map(|(h, _)| *h)
							});
							current
						})
						.collect();
					response_channel.send(Ok(ancestors)).unwrap();
				},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					tx.send(Ok(self.rp_info.get(&rp).unwrap().session_index)).unwrap();
				},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::Validators(tx),
				)) => {
					let session_index = self.rp_info.get(&rp).unwrap().session_index;
					let session_info = self.session_info.get(&session_index).unwrap();
					tx.send(Ok(session_info.validators.clone())).unwrap();
				},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::ValidatorGroups(tx),
				)) => {
					let session_index = self.rp_info.get(&rp).unwrap().session_index;
					let session_info = self.session_info.get(&session_index).unwrap();
					tx.send(Ok((
						session_info.validator_groups.clone(),
						session_info.group_rotation_info.clone(),
					)))
					.unwrap();
				},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::ClaimQueue(tx),
				)) => {
					let rp_info = self.rp_info.get(&rp).unwrap();

					tx.send(Ok(rp_info
						.claim_queue
						.clone()
						.into_iter()
						.map(|(i, cq)| (i, cq.into_iter().collect()))
						.collect()))
						.unwrap();
				},
				other => {
					if had_buffered_msg {
						panic!("Unexpected message: {:?}", other);
					} else {
						break Some(other);
					}
				},
			};
		};

		self.buffered_msg = extra_msg;
	}

	async fn activate_leaf<B: Backend>(&mut self, state: &mut State<B>, height: BlockNumber) {
		let mut sender = self.sender.clone();
		futures::join!(self.handle_view_update(vec![get_hash(height)]), async {
			state
				.handle_our_view_change(&mut sender, OurView::new([get_hash(height)], 0))
				.await
				.unwrap()
		});
	}

	fn pp_knows(&mut self, leaf: Hash, para_id: ParaId, known_output_heads: HashSet<Hash>) {
		self.pp_known_output_heads
			.entry(leaf)
			.or_default()
			.insert(para_id, known_output_heads.into());
	}

	async fn activate_leaves<B: Backend>(&mut self, state: &mut State<B>, leaves: Vec<Hash>) {
		let mut sender = self.sender.clone();
		futures::join!(self.handle_view_update(leaves.clone()), async {
			state
				.handle_our_view_change(&mut sender, OurView::new(leaves, 0))
				.await
				.unwrap()
		});
	}

	async fn handle_finalized_block_lookback(
		&mut self,
		processed: BlockNumber,
		finalized: BlockNumber,
	) {
		self.finalized_block = finalized;

		let diff =
			std::cmp::min(finalized.checked_sub(processed).unwrap(), MAX_STARTUP_ANCESTRY_LOOKBACK);
		if diff == 0 {
			return;
		}

		let msg = match self.buffered_msg.take() {
			Some(msg) => msg,
			None => self.timeout_recv().await,
		};

		let ancestors =
			((finalized - diff)..finalized).map(|n| get_hash(n)).rev().collect::<Vec<_>>();

		assert_matches!(
			msg,
			AllMessages::ChainApi(
				ChainApiMessage::Ancestors {
					hash,
					k,
					response_channel
				}
			) => {
				assert_eq!(hash, get_hash(finalized));
				assert_eq!(k as u32, diff);
				assert_eq!(ancestors.len() as u32, diff);
				response_channel.send(Ok(ancestors.clone())).unwrap();
			}
		);

		let extra_msg = loop {
			let had_buffered_msg = self.buffered_msg.is_some();
			let msg = match self.buffered_msg.take() {
				Some(msg) => msg,
				None => {
					if let Some(Some(msg)) = self.recv.next().timeout(TIMEOUT).await {
						msg
					} else {
						break None;
					}
				},
			};

			match msg {
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::CandidateEvents(tx),
				)) => {
					assert!(ancestors.contains(&rp) || rp == get_hash(finalized));
					let events = self
						.candidates_pending_availability
						.get(&rp)
						.cloned()
						.unwrap_or_default()
						.iter()
						.map(|ccr| {
							polkadot_primitives::CandidateEvent::CandidateIncluded(
								ccr.to_plain(),
								Default::default(),
								Default::default(),
								Default::default(),
							)
						})
						.collect();
					tx.send(Ok(events)).unwrap()
				},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::CandidatesPendingAvailability(para_id, tx),
				)) => {
					assert!(ancestors.contains(&rp));
					let included_at = (rp.to_low_u64_be() as u32) + 1;
					let candidates = self
						.candidates_pending_availability
						.get(&get_hash(included_at))
						.cloned()
						.unwrap_or_default()
						.into_iter()
						.filter(|ccr| ccr.descriptor.para_id() == para_id)
						.collect();
					tx.send(Ok(candidates)).unwrap();
				},
				other => {
					if had_buffered_msg {
						panic!("Unexpected message: {:?}", other);
					} else {
						break Some(other);
					}
				},
			};
		};

		self.buffered_msg = extra_msg;
	}

	async fn handle_finalized_block(&mut self, processed: BlockNumber, finalized: BlockNumber) {
		self.handle_finalized_block_lookback(processed, finalized).await;

		// Finality notifications additionally check for changes in the registered paras
		// (once per session).
		let msg = match self.buffered_msg.take() {
			Some(msg) => msg,
			None => self.timeout_recv().await,
		};

		let session_index = match msg {
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::SessionIndexForChild(tx),
			)) => {
				let index = self.rp_info.get(&rp).map(|info| info.session_index).unwrap_or(1);
				tx.send(Ok(index)).unwrap();
				index
			},
			other => panic!("Unexpected message: {:?}", other),
		};

		let msg = if let Some(Some(msg)) = self.recv.next().timeout(TIMEOUT).await {
			msg
		} else {
			return;
		};

		assert_matches!(
			msg,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_rp,
				RuntimeApiRequest::ParaIds(index, tx),
			)) => {
				assert_eq!(index, session_index);
				let session_info = self.session_info.get(&index).unwrap();
				tx.send(Ok(session_info.paras.clone())).unwrap();
			}
		);
	}

	async fn handle_advertisement<B: Backend>(&mut self, state: &mut State<B>, adv: Advertisement) {
		let mut sender = self.sender.clone();
		futures::join!(
			state.handle_advertisement(
				&mut sender,
				adv.peer_id,
				adv.scheduling_parent,
				adv.prospective_candidate.into_iter().collect(),
				adv.advertised_descriptor_version,
				None
			),
			async move {
				// Only a candidate-hash claim triggers a backing pre-check; V1 (no claim) and
				// V4 (no candidate hash) advertisements are accepted without one.
				if adv.candidate_hash().is_some() {
					self.assert_can_second_request(adv, true).await
				}
			}
		);
	}

	/// Deliver a V4 segment advertisement over the wire path (V3 candidate descriptors).
	async fn send_v4_segment<B: Backend>(
		&mut self,
		state: &mut State<B>,
		peer_id: PeerId,
		scheduling_parent: Hash,
		fingerprints: Vec<CandidateFingerprint>,
		para_id: ParaId,
	) {
		let mut sender = self.sender.clone();
		process_incoming_peer_message(
			&mut sender,
			state,
			peer_id,
			CollationProtocols::V4(protocol_v4::AdvertiseSegment {
				scheduling_parent,
				candidates_descriptor_version: CandidateDescriptorVersion::V3,
				candidates: fingerprints.try_into().unwrap(),
				para_id,
			}),
		)
		.await;
	}

	async fn assert_collation_request(&mut self, adv: Advertisement) -> ResponseSender {
		let mut res = self.assert_collation_requests([adv].into()).await;

		assert_eq!(res.len(), 1);
		res.pop_first().unwrap().1
	}

	async fn assert_collation_requests(
		&mut self,
		mut advertisements: BTreeSet<Advertisement>,
	) -> BTreeMap<Advertisement, ResponseSender> {
		let msg = match self.buffered_msg.take() {
			Some(msg) => msg,
			None => self.timeout_recv().await,
		};

		let mut res = BTreeMap::new();

		assert_matches!(
			msg,
			AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendRequests(
				reqs,
				IfDisconnected::ImmediateError
			)) => {
				assert_eq!(reqs.len(), advertisements.len());
				for req in reqs {
					match req {
						Requests::CollationFetchingV2(req) => {
							assert!(req.fallback_request.is_none());

							let adv = advertisements.iter().find(|adv| {
								if let Some(ProspectiveCandidate::ByHash { candidate_hash, .. }) = adv.prospective_candidate {
									matches!(req.peer, Recipient::Peer(peer) if peer == adv.peer_id) &&
										req.payload.scheduling_parent == adv.scheduling_parent &&
										req.payload.para_id == adv.para_id &&
										req.payload.candidate_hash == candidate_hash
								} else {
									false
								}
							}).copied().unwrap();

							res.insert(adv, req.pending_response);

							advertisements.remove(&adv);
						},
						Requests::CollationFetchingV1(req) => {
							assert!(req.fallback_request.is_none());

							let adv = advertisements.iter().find(|adv| {
								adv.prospective_candidate.is_none() &&
									matches!(req.peer, Recipient::Peer(peer) if peer == adv.peer_id) &&
									req.payload.scheduling_parent == adv.scheduling_parent &&
									req.payload.para_id == adv.para_id

							}).copied().unwrap();

							res.insert(adv, req.pending_response);

							advertisements.remove(&adv);
						}
						Requests::CollationFetchingV3(req) => {
							assert!(req.fallback_request.is_none());

							let adv = advertisements.iter().find(|adv| {
								adv.prospective_candidate
									.and_then(|pc| pc.output_head_data_hash())
									.is_some_and(|output_head| {
										matches!(req.peer, Recipient::Peer(peer) if peer == adv.peer_id) &&
											req.payload.scheduling_parent == adv.scheduling_parent &&
											req.payload.para_id == adv.para_id &&
											req.payload.output_head_data_hash == output_head
									})
							}).copied().unwrap();

							res.insert(adv, req.pending_response);

							advertisements.remove(&adv);
						}
						_ => panic!("Unexpected request")
					}
				}
			}
		);

		res
	}

	async fn assert_can_second_request(&mut self, adv: Advertisement, response: bool) {
		let msg = match self.buffered_msg.take() {
			Some(msg) => msg,
			None => self.timeout_recv().await,
		};

		if let Some(ProspectiveCandidate::ByHash { candidate_hash, parent_head_data_hash }) =
			adv.prospective_candidate
		{
			let expected_req = CanSecondRequest {
				candidate_para_id: adv.para_id,
				candidate_scheduling_parent: adv.scheduling_parent,
				candidate_hash,
				parent_head_data_hash,
			};

			assert_matches!(
				msg,
				AllMessages::CandidateBacking(CandidateBackingMessage::CanSecond(req, tx)) => {
					assert_eq!(
						expected_req,
						req
					);

					tx.send(response).unwrap();
				}
			);
		} else {
			panic!(
				"Didn't expect a CanSecond request for a claim without a candidate hash {:?}",
				msg
			);
		}
	}

	async fn assert_pvd_request(
		&mut self,
		adv: Advertisement,
		pvd: Option<PersistedValidationData>,
		expected_relay_parent: Hash,
	) {
		let msg = match self.buffered_msg.take() {
			Some(msg) => msg,
			None => self.timeout_recv().await,
		};

		// Both claim shapes carry a parent head hash; V1 (no claim) takes the runtime-API path.
		if let Some(parent_head_data_hash) =
			adv.prospective_candidate.map(|pc| pc.parent_head_data_hash())
		{
			assert_matches!(
				msg,
				AllMessages::ProspectiveParachains(
					ProspectiveParachainsMessage::GetProspectiveValidationData(ProspectiveValidationDataRequest {
						para_id,
						candidate_relay_parent,
						parent_head_data,
						session_index: _,
					}, tx)
				) => {
					assert_eq!(para_id, adv.para_id);
					assert_eq!(candidate_relay_parent, expected_relay_parent);

					assert!(
						matches!(
							parent_head_data,
							ParentHeadData::OnlyHash(head_data_hash) if head_data_hash == parent_head_data_hash
						) ||
						matches!(
							parent_head_data,
							ParentHeadData::WithData {head_data, ..} if head_data == pvd.as_ref().unwrap().parent_head
						)
					);

					tx.send(pvd).unwrap();
				}
			);
		} else {
			assert_matches!(
				msg,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::PersistedValidationData(
						para_id,
						OccupiedCoreAssumption::Free,
						tx
					)
				)) => {
					assert_eq!(para_id, adv.para_id);
					assert_eq!(rp, adv.scheduling_parent);
					tx.send(Ok(pvd)).unwrap();
				}
			);
		}
	}

	async fn assert_seconding_kickoff(
		&mut self,
		expected_receipt: CandidateReceipt,
		expected_pvd: PersistedValidationData,
		expected_pov: PoV,
		expected_scheduling_parent: Hash,
	) {
		let msg = match self.buffered_msg.take() {
			Some(msg) => msg,
			None => self.timeout_recv().await,
		};

		assert_matches!(
			msg,
			AllMessages::CandidateBacking(
				CandidateBackingMessage::Second { scheduling_parent, candidate, pvd, pov }
			) => {
				assert_eq!(scheduling_parent, expected_scheduling_parent);
				assert_eq!(candidate, expected_receipt);
				assert_eq!(pvd, expected_pvd);
				assert_eq!(pov, expected_pov);
			}
		);
	}

	async fn handle_fetched_collation<B: Backend>(
		&mut self,
		state: &mut State<B>,
		adv: Advertisement,
		receipt: CandidateReceipt,
		maybe_pvd: Option<PersistedValidationData>,
		expected_relay_parent: Hash,
	) {
		let mut sender = self.sender.clone();
		let pvd = maybe_pvd.unwrap_or_else(|| dummy_pvd());

		futures::join!(
			state.handle_fetched_collation(
				&mut sender,
				(adv, Ok(CollationFetchingResponse::Collation(receipt.clone(), dummy_pov())))
			),
			self.assert_pvd_request(adv, Some(pvd.clone()), expected_relay_parent)
		);

		self.assert_seconding_kickoff(receipt, pvd, dummy_pov(), adv.scheduling_parent)
			.await;
	}

	async fn assert_collation_seconded_notification(
		&mut self,
		peer_id: PeerId,
		version: CollationVersion,
		statement: UncheckedSignedFullStatement,
	) {
		// V4 has no `CollationSeconded` message; the validator sends nothing to ack.
		if version == CollationVersion::V4 {
			return;
		}

		let msg = match self.buffered_msg.take() {
			Some(msg) => msg,
			None => self.timeout_recv().await,
		};

		assert_matches!(
			msg,
			AllMessages::NetworkBridgeTx(
				NetworkBridgeTxMessage::SendCollationMessage(peers, msg)
			) => {
				assert_eq!(peers, vec![peer_id]);

				match version {
					CollationVersion::V1 => {
						assert_matches!(
							msg,
							CollationProtocols::V1(protocol_v1::CollationProtocol::CollatorProtocol(
								protocol_v1::CollatorProtocolMessage::CollationSeconded(
									_rp,
									stmt
								),
							)) => {
								assert_eq!(statement, stmt);
							}
						);
					},
					CollationVersion::V2 => {
						assert_matches!(
							msg,
							CollationProtocols::V2(protocol_v2::CollationProtocol::CollatorProtocol(
								protocol_v2::CollatorProtocolMessage::CollationSeconded(
									_rp,
									stmt
								),
							)) => {
								assert_eq!(statement, stmt);
							}
						);
					},
					CollationVersion::V3 => {
						assert_matches!(
							msg,
							CollationProtocols::V3(protocol_v3::CollationProtocol::CollatorProtocol(
								protocol_v3::CollatorProtocolMessage::CollationSeconded(_rp, stmt)
							)) => {
								assert_eq!(statement, stmt);
							}
						)
					},
					// V4 sends no `CollationSeconded`, so there is nothing to assert.
					CollationVersion::V4 => {}
				};
			}
		);
	}

	async fn second_collation<B: Backend>(
		&mut self,
		state: &mut State<B>,
		peer_id: PeerId,
		version: CollationVersion,
		ccr: CommittedCandidateReceipt,
		scheduling_parent: Hash,
	) {
		let statement = make_seconded_statement(&self.keystore, ccr);
		state
			.handle_seconded_collation(&mut self.sender, statement.clone(), scheduling_parent)
			.await;
		self.assert_collation_seconded_notification(peer_id, version, statement.into())
			.await;
	}
}

fn make_seconded_statement(
	keystore: &KeystorePtr,
	ccr: CommittedCandidateReceipt,
) -> SignedFullStatement {
	let signing_context =
		SigningContext { parent_hash: ccr.descriptor.relay_parent(), session_index: 1 };

	SignedFullStatement::sign(
		keystore,
		Statement::Seconded(ccr),
		&signing_context,
		ValidatorIndex(0),
		&ValidatorId::from(Sr25519Keyring::Alice.public()),
	)
	.ok()
	.flatten()
	.expect("should be signed")
}

fn get_parent_hash(number: u32) -> Hash {
	get_hash(number - 1)
}

fn get_hash(number: u32) -> Hash {
	Hash::from_low_u64_be(number as u64)
}

async fn make_state<B: Backend>(
	db: B,
	test_state: &mut TestState,
	initial_leaf_hash: Hash,
) -> State<B> {
	let initial_leaf_number = test_state.rp_info.get(&initial_leaf_hash).unwrap().number;

	let keystore = test_state.keystore.clone();
	let clock = test_state.clock.clone();

	let processed_block_number = db.processed_finalized_block_number().await.unwrap_or_default();

	let mut sender = test_state.sender.clone();

	let responder = async move {
		test_state.handle_view_update(vec![initial_leaf_hash]).await;

		let msg = match test_state.buffered_msg.take() {
			Some(msg) => msg,
			None => test_state.timeout_recv().await,
		};

		let finalized_block_number = test_state.finalized_block;
		let finalized_block_hash = get_hash(finalized_block_number);

		assert_matches!(
			msg,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockNumber(tx)) => {
				tx.send(Ok(finalized_block_number)).unwrap();
			}
		);

		assert_matches!(
			test_state.timeout_recv().await,
			AllMessages::ChainApi(ChainApiMessage::FinalizedBlockHash(number, tx)) => {
				assert_eq!(number, finalized_block_number);
				tx.send(Ok(Some(finalized_block_hash))).unwrap();
			}
		);

		// Startup only extracts reputation bumps from the finalized chain, it doesn't prune the
		// registered paras.
		if finalized_block_number > processed_block_number {
			test_state
				.handle_finalized_block_lookback(processed_block_number, finalized_block_number)
				.await;
		}

		// No more messages are expected
		test_state.assert_no_messages().await;
	};

	let initializer = async move {
		let collation_manager = CollationManager::new(
			&mut sender,
			keystore,
			new_leaf(initial_leaf_hash, initial_leaf_number),
			clock.clone(),
		)
		.await
		.unwrap();

		let peer_manager = PeerManager::startup(
			db,
			&mut sender,
			collation_manager.assignments(),
			clock,
			Metrics::default(),
		)
		.await
		.unwrap();

		State::new(peer_manager, collation_manager, Metrics::default())
	};

	let (state, ..) = futures::join!(initializer, responder);

	state
}

#[derive(Clone)]
struct MockDb {
	finalized: Arc<Mutex<BlockNumber>>,
	// Use BTreeMaps to ensure ordering when asserting.
	witnessed_bumps: Arc<Mutex<BTreeMap<ParaId, BTreeMap<PeerId, Score>>>>,
	witnessed_slash: Arc<Mutex<Option<(PeerId, ParaId, Score)>>>,
	witnessed_prunes: Arc<Mutex<Vec<BTreeSet<ParaId>>>>,
	query_fn: Arc<Mutex<dyn Fn(PeerId, ParaId) -> Option<Score> + Send>>,
}

impl Default for MockDb {
	/// Create an instance where all peers have a score that allows instant fetching.
	fn default() -> Self {
		let query_fn = |_peer_id, _para_id| Some(Score::new(VALID_INCLUDED_CANDIDATE_BUMP));
		Self::new(Arc::new(Mutex::new(query_fn)))
	}
}

impl MockDb {
	fn new(query_fn: Arc<Mutex<dyn Fn(PeerId, ParaId) -> Option<Score> + Send>>) -> Self {
		Self {
			finalized: Default::default(),
			witnessed_bumps: Default::default(),
			witnessed_slash: Default::default(),
			witnessed_prunes: Default::default(),
			query_fn,
		}
	}

	fn witnessed_bumps(&self) -> BTreeMap<ParaId, BTreeMap<PeerId, Score>> {
		std::mem::take(self.witnessed_bumps.lock().unwrap().deref_mut())
	}

	fn witnessed_slash(&self) -> Option<(PeerId, ParaId, Score)> {
		std::mem::take(self.witnessed_slash.lock().unwrap().deref_mut())
	}

	fn witnessed_prunes(&self) -> Vec<BTreeSet<ParaId>> {
		std::mem::take(self.witnessed_prunes.lock().unwrap().deref_mut())
	}

	fn set_processed_finalized_block_number(&self, number: BlockNumber) {
		*self.finalized.lock().unwrap() = number;
	}
}

#[async_trait]
impl Backend for MockDb {
	async fn processed_finalized_block_number(&self) -> Option<BlockNumber> {
		Some(*(self.finalized.lock().unwrap()))
	}

	async fn query(&self, peer_id: &PeerId, para_id: &ParaId) -> Option<Score> {
		let query_fn = self.query_fn.lock().unwrap();

		query_fn(*peer_id, *para_id)
	}

	async fn slash(&mut self, peer_id: &PeerId, para_id: &ParaId, value: Score) {
		let old_slash = std::mem::replace(
			self.witnessed_slash.lock().unwrap().deref_mut(),
			Some((*peer_id, *para_id, value)),
		);

		assert!(old_slash.is_none());
	}

	async fn prune_paras(&mut self, registered_paras: BTreeSet<ParaId>) {
		self.witnessed_prunes.lock().unwrap().push(registered_paras);
	}

	async fn process_bumps(
		&mut self,
		leaf_number: BlockNumber,
		bumps: BTreeMap<ParaId, HashMap<PeerId, Score>>,
		_decay_value: Option<Score>,
		_now: std::time::Duration,
	) -> Vec<ReputationUpdate> {
		// Mirror the real backend: never move the processed finalized block backwards.
		if *(self.finalized.lock().unwrap()) >= leaf_number {
			return vec![];
		}

		let old_bumps = std::mem::replace(
			self.witnessed_bumps.lock().unwrap().deref_mut(),
			bumps.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect(),
		);

		assert!(old_bumps.is_empty());

		*(self.finalized.lock().unwrap()) = leaf_number;

		vec![]
	}

	async fn max_scores_for_paras(&self, _paras: BTreeSet<ParaId>) -> HashMap<ParaId, Score> {
		HashMap::new()
	}
}

impl Drop for MockDb {
	fn drop(&mut self) {
		assert!(self.witnessed_bumps().is_empty());
		assert!(self.witnessed_slash().is_none());
		assert!(self.witnessed_prunes().is_empty());
	}
}

// Test scenarios concerning connects/disconnects and declares.
// More fine grained tests are in the `ConnectedPeers` unit tests.
#[tokio::test]
async fn test_connection_flow() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let db = Db::new(MAX_STORED_SCORES_PER_PARA).await;
	let mut state = make_state(db, &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();
	assert!(state.take_replan());

	let first_peer = PeerId::random();
	state.handle_peer_connected(&mut sender, first_peer, CollationVersion::V2).await;
	// If we don't get a disconnect message, it was accepted.
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), [first_peer].into_iter().collect());

	// Reconnecting is a no-op. We should have first received a disconnect.
	state.handle_peer_connected(&mut sender, first_peer, CollationVersion::V1).await;
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), [first_peer].into_iter().collect());

	// Disconnect the peer.
	state.handle_peer_disconnected(first_peer).await;
	assert_eq!(state.connected_peers(), Default::default());

	// Fill up the connection slots. For each para (ids 100 and 200) we should have 100 slots.
	let peer_ids = (0..(CONNECTED_PEERS_PARA_LIMIT.get() as u8))
		.map(|i| peer_id(i))
		.collect::<Vec<_>>();

	for id in peer_ids.iter() {
		state.handle_peer_connected(&mut sender, *id, CollationVersion::V2).await;
	}
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), peer_ids.clone().into_iter().collect());

	// Now all 100 peers were accepted on both paras (since they're not declared).
	// A new connection from a peer with the same score will be rejected.
	let new_peer = PeerId::random();
	state.handle_peer_connected(&mut sender, new_peer, CollationVersion::V2).await;
	test_state.assert_peers_disconnected([new_peer]).await;
	assert_eq!(state.connected_peers(), peer_ids.clone().into_iter().collect());

	// Bump the reputations of all peers except for the first one.
	// The ith peer will have it's reputation bumped i times.
	let mut pending = vec![];
	for (i, peer) in peer_ids.iter().enumerate().skip(1) {
		for _ in 0..i {
			pending.push((ParaId::from(100), *peer));
		}
	}

	test_state.set_candidates_pending_availability(
		[(get_hash(1), pending), (get_hash(2), vec![(ParaId::from(100), new_peer)])]
			.into_iter()
			.collect(),
	);

	// Reputations are bumped on finalized block notifications.
	let processed = state.processed_finalized_block_number().await.unwrap_or_default();
	futures::join!(test_state.handle_finalized_block(processed, 2), async {
		state.handle_finalized_block(&mut sender, get_hash(2), 2).await.unwrap()
	});
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), peer_ids.clone().into_iter().collect());

	state.handle_peer_connected(&mut sender, new_peer, CollationVersion::V2).await;
	// The new peer took the spot of the first one, but that other one remains connected for the
	// other para (200).
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), peer_ids.clone().into_iter().chain([new_peer]).collect());

	// If that first peer then declares for another para, it will get disconnected.
	state.handle_declare(&mut sender, peer_ids[0], 100.into()).await;
	test_state.assert_peers_disconnected([peer_ids[0]]).await;
	assert_eq!(
		state.connected_peers(),
		peer_ids.clone().into_iter().skip(1).chain([new_peer]).collect()
	);

	// Make all peers declare for para 100.
	state.handle_declare(&mut sender, new_peer, 100.into()).await;
	for peer in peer_ids.iter().skip(1) {
		state.handle_declare(&mut sender, *peer, 100.into()).await;
	}
	test_state.assert_no_messages().await;

	// A subsequent declare is idempotent.
	state.handle_declare(&mut sender, new_peer, 100.into()).await;

	test_state.assert_no_messages().await;

	assert_eq!(
		state.connected_peers(),
		peer_ids.clone().into_iter().skip(1).chain([new_peer]).collect()
	);

	// The first peer can attempt to reconnect and declare for the other para.
	state
		.handle_peer_connected(&mut sender, peer_ids[0], CollationVersion::V2)
		.await;
	state.handle_declare(&mut sender, peer_ids[0], 200.into()).await;
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), peer_ids.clone().into_iter().chain([new_peer]).collect());
	state.handle_peer_disconnected(peer_ids[0]).await;

	// Will be disconnected if declared to collate for an unscheduled para.
	state
		.handle_peer_connected(&mut sender, peer_ids[0], CollationVersion::V2)
		.await;
	state.handle_declare(&mut sender, peer_ids[0], 600.into()).await;
	test_state.assert_peers_disconnected([peer_ids[0]]).await;

	assert_eq!(
		state.connected_peers(),
		peer_ids.clone().into_iter().skip(1).chain([new_peer]).collect()
	);

	// The new peer will be disconnected if it switches the paraid.
	state.handle_declare(&mut sender, new_peer, 200.into()).await;
	test_state.assert_peers_disconnected([new_peer]).await;
	assert!(!state.take_replan());
	assert_eq!(state.connected_peers(), peer_ids.clone().into_iter().skip(1).collect());
}

// Test peer disconnects before the collation is successfully fetched.
#[tokio::test]
async fn test_peer_disconnects_before_fetch() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();

	let first_peer = peer_id(1);
	let second_peer = peer_id(2);

	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	// Para 100 has 2 assignments in the claim queue at the active leaf.

	// Let's build two advertisements from the first peer and one advertisement from the second
	// peer.
	state.handle_peer_connected(&mut sender, first_peer, CollationVersion::V2).await;
	state.handle_declare(&mut sender, first_peer, 100.into()).await;

	let (_, first_adv) = dummy_candidate(
		active_leaf,
		100.into(),
		first_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		Hash::from_low_u64_be(0),
	);

	let (_, second_adv) = dummy_candidate(
		active_leaf,
		100.into(),
		first_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		Hash::from_low_u64_be(1),
	);

	let (_, third_adv) = dummy_candidate(
		active_leaf,
		100.into(),
		second_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		Hash::from_low_u64_be(2),
	);

	// Advertise a collation from the first peer, it will be launched, leaving only one free
	// slot.
	test_state.handle_advertisement(&mut state, first_adv).await;

	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(first_adv).await;
	test_state.assert_no_messages().await;

	// Now let's add the second peer and make it advertise the third candidate.
	// Disconnect before the fetch is launched.
	{
		state
			.handle_peer_connected(&mut sender, second_peer, CollationVersion::V2)
			.await;
		state.handle_declare(&mut sender, second_peer, 100.into()).await;

		test_state.handle_advertisement(&mut state, third_adv).await;
		assert_eq!(state.advertisements(), [third_adv].into());

		state.handle_peer_disconnected(second_peer).await;

		// Was disconnected, so no new requests.
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_no_messages().await;

		assert!(state.advertisements().is_empty());
	}

	// Peer disconnected while the request was fetching.
	{
		state
			.handle_peer_connected(&mut sender, second_peer, CollationVersion::V2)
			.await;
		state.handle_declare(&mut sender, second_peer, 100.into()).await;

		// Let's add the third advertisement.
		test_state.handle_advertisement(&mut state, third_adv).await;

		assert_eq!(state.advertisements(), [third_adv].into());

		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_collation_request(third_adv).await;
		test_state.handle_advertisement(&mut state, second_adv).await;
		assert_eq!(state.advertisements(), [second_adv].into());
		test_state.assert_no_messages().await;

		// Second advertisement is not launched since the third one already occupied the other
		// slot.
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_no_messages().await;

		// Second peer disconnects, which will free up the claim queue slot.
		state.handle_peer_disconnected(second_peer).await;

		assert_eq!(state.advertisements(), [second_adv].into());
		state
			.handle_fetched_collation(
				&mut sender,
				(
					third_adv,
					Err(CollationFetchError::Request(RequestError::NetworkError(
						RequestFailure::NotConnected,
					))),
				),
			)
			.await;
		test_state.assert_no_messages().await;
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		// Since it freed up the slot, second adv can now be launched.
		test_state.assert_collation_request(second_adv).await;

		test_state.assert_no_messages().await;
	}
}

// Test peer disconnects after the collation is successfully fetched.
#[tokio::test]
async fn test_peer_disconnects_after_fetch() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();

	let first_peer = peer_id(1);
	let second_peer = peer_id(2);

	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	// Para 100 has 2 assignments in the claim queue at the active leaf.

	// Let's build two advertisements from the first peer and one advertisement from the second
	// peer.
	state.handle_peer_connected(&mut sender, first_peer, CollationVersion::V2).await;
	state.handle_declare(&mut sender, first_peer, 100.into()).await;

	let (_, first_adv) = dummy_candidate(
		active_leaf,
		100.into(),
		first_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		Hash::from_low_u64_be(0),
	);

	let (_, second_adv) = dummy_candidate(
		active_leaf,
		100.into(),
		first_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		Hash::from_low_u64_be(1),
	);

	let (third_ccr, third_adv) = dummy_candidate(
		active_leaf,
		100.into(),
		second_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);

	// Advertise a collation from the first peer, it will be launched, leaving only one free
	// slot.
	test_state.handle_advertisement(&mut state, first_adv).await;

	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(first_adv).await;
	test_state.assert_no_messages().await;

	// Test a peer disconnect after the collation was successfully fetched. In this case, we
	// won't free up the slot.

	state
		.handle_peer_connected(&mut sender, second_peer, CollationVersion::V2)
		.await;
	state.handle_declare(&mut sender, second_peer, 100.into()).await;

	// Let's add the third advertisement.
	test_state.handle_advertisement(&mut state, third_adv).await;

	assert_eq!(state.advertisements(), [third_adv].into());

	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(third_adv).await;
	test_state.handle_advertisement(&mut state, second_adv).await;
	assert_eq!(state.advertisements(), [second_adv].into());
	test_state.assert_no_messages().await;

	// Second advertisement is not launched since the third one already occupied the other slot.
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;

	// Send a successful response to the third advertisement and start seconding it.
	test_state
		.handle_fetched_collation(
			&mut state,
			third_adv,
			third_ccr.to_plain(),
			None,
			third_adv.scheduling_parent,
		)
		.await;

	// Second peer disconnects, which will not free up the claim queue slot since the collation
	// was already fetched.
	state.handle_peer_disconnected(second_peer).await;

	assert_eq!(state.advertisements(), [second_adv].into());

	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;

	// The collation was seconded, the claim will still not be freed but we won't be able to
	// send back a notification to the collator.

	let parent = third_ccr.descriptor.scheduling_parent();
	let statement = make_seconded_statement(&test_state.keystore, third_ccr);

	state.handle_seconded_collation(&mut sender, statement, parent).await;
	test_state.assert_no_messages().await;

	assert_eq!(state.advertisements(), [second_adv].into());

	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;
}

// Test that peer connections are rejected if we have no assignments.
#[tokio::test]
async fn test_no_assignments() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let active_leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	let assigned_core = active_leaf_info.assigned_core;

	for info in test_state.rp_info.values_mut() {
		info.claim_queue.get_mut(&assigned_core).unwrap().clear();
	}

	let db = MockDb::default();
	let mut state = make_state(db, &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	let peer = peer_id(1);

	state.handle_peer_connected(&mut sender, peer, CollationVersion::V2).await;
	test_state.assert_peers_disconnected([peer]).await;
	assert!(state.connected_peers().is_empty());
	test_state.assert_no_messages().await;

	// Now add some assignments and connected peers. We want to check what happens to already
	// connected peers when we run out of assignments
	test_state.rp_info.insert(
		get_hash(11),
		RelayParentInfo {
			number: 11,
			parent: get_parent_hash(11),
			session_index: active_leaf_info.session_index,
			claim_queue: [(assigned_core, vec![600.into()])].into_iter().collect(),
			assigned_core,
		},
	);

	test_state.activate_leaf(&mut state, 11).await;

	let first_peer = peer;
	let second_peer = peer_id(2);

	state.handle_peer_connected(&mut sender, first_peer, CollationVersion::V2).await;
	state
		.handle_peer_connected(&mut sender, second_peer, CollationVersion::V2)
		.await;
	state.handle_declare(&mut sender, first_peer, 600.into()).await;
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), [first_peer, second_peer].into());

	for height in 12..=14 {
		test_state.rp_info.insert(
			get_hash(height),
			RelayParentInfo {
				number: height,
				parent: get_parent_hash(height),
				session_index: active_leaf_info.session_index,
				claim_queue: [(assigned_core, vec![])].into(),
				assigned_core,
			},
		);
	}

	for _ in 12..=13 {
		test_state.activate_leaf(&mut state, 11).await;
		test_state.assert_no_messages().await;
	}
	assert_eq!(state.connected_peers(), [first_peer, second_peer].into());

	// When 14th leaf comes in, we're left with no assignments. Peers will be disconnected.
	test_state.activate_leaf(&mut state, 14).await;
	test_state.assert_peers_disconnected([first_peer, second_peer]).await;
	assert!(state.connected_peers().is_empty());
}

// Test peer connection inheritance across scheduled para change.
#[tokio::test]
async fn test_peer_connections_across_schedule_change() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);

	let mut state = make_state(MockDb::default(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	// Leaf 8 has 100, 200, 100.
	// Leaf 9 has 200, 100, 200.
	// Leaf 10 has 100, 200, 100.

	// First 5 peers will be declared for para 100.
	// Next 5 peers will be declared for para 200.
	// Last 5 peers undeclared.
	let peer_ids = (0..15).map(|i| peer_id(i)).collect::<Vec<_>>();

	for id in peer_ids.iter() {
		state.handle_peer_connected(&mut sender, *id, CollationVersion::V2).await;
	}
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), peer_ids.clone().into_iter().collect());

	for id in &peer_ids[..5] {
		state.handle_declare(&mut sender, *id, 100.into()).await;
	}
	for id in &peer_ids[5..10] {
		state.handle_declare(&mut sender, *id, 200.into()).await;
	}
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), peer_ids.clone().into_iter().collect());

	let all_100: Vec<ParaId> = std::iter::repeat(100.into()).take(3).collect();
	let all_600: Vec<ParaId> = std::iter::repeat(600.into()).take(3).collect();

	let prev_leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	for (height, assignments) in [
		(11, vec![200.into(), 100.into(), 100.into()]),
		(12, all_100.clone()),
		(13, all_100.clone()),
		(14, all_100),
		(15, all_600.clone()),
		(16, all_600.clone()),
		(17, all_600),
	] {
		let mut cq = prev_leaf_info.claim_queue.clone();
		cq.insert(prev_leaf_info.assigned_core, assignments);

		test_state.rp_info.insert(
			get_hash(height),
			RelayParentInfo {
				number: height,
				parent: get_parent_hash(height),
				session_index: prev_leaf_info.session_index,
				claim_queue: cq.clone(),
				assigned_core: prev_leaf_info.assigned_core,
			},
		);
	}

	// Activating leaf 11 keeps 200 in the visible window: the leaf's CQ for our core is
	// [200, 100, 100], so position 0 is still 200 and every SP in view can see it.
	// Repeating the update is a no-op.
	for _ in 0..2 {
		test_state.activate_leaf(&mut state, 11).await;
		test_state.assert_no_messages().await;
		assert_eq!(state.connected_peers(), peer_ids.clone().into_iter().collect());
	}

	// Activating leaf 12 makes the leaf's CQ [100, 100, 100] — no SP in view can see 200
	// anymore (offset windows are slices of the leaf's CQ, all of which is now para 100).
	// Peers declared for 200 are disconnected.
	test_state.activate_leaf(&mut state, 12).await;
	test_state.assert_peers_disconnected((&peer_ids[5..10]).to_vec()).await;
	let expected_connected_peers = (&peer_ids[..5])
		.into_iter()
		.cloned()
		.chain((&peer_ids[10..15]).into_iter().cloned())
		.collect();
	assert_eq!(state.connected_peers(), expected_connected_peers);
	test_state.assert_no_messages().await;

	// Activating leaves 13 and 14 keeps the assignment set (still all-100). No churn.
	for height in 13..=14 {
		test_state.activate_leaf(&mut state, height).await;
		test_state.assert_no_messages().await;
		assert_eq!(state.connected_peers(), expected_connected_peers);
	}

	// Activating leaf 15 makes the leaf's CQ [600, 600, 600] — para 100 is gone from every
	// SP's visible window. Peers declared for 100 are disconnected.
	test_state.activate_leaf(&mut state, 15).await;

	test_state.assert_peers_disconnected((&peer_ids[0..5]).to_vec()).await;
	let expected_connected_peers = (&peer_ids[10..]).into_iter().cloned().collect();
	assert_eq!(state.connected_peers(), expected_connected_peers);
	test_state.assert_no_messages().await;

	// Subsequent same-CQ leaves don't disconnect anyone else.
	for height in 16..=17 {
		test_state.activate_leaf(&mut state, height).await;
		test_state.assert_no_messages().await;
		assert_eq!(state.connected_peers(), expected_connected_peers);
	}

	// Add a parallel fork at block 17 which schedules para 200, alongside the existing
	// 600-only leaf. Test that assignments are the union across forks: peers can declare for
	// either para and both should be accepted.
	let mut cq = prev_leaf_info.claim_queue.clone();
	cq.insert(prev_leaf_info.assigned_core, vec![200.into()]);

	let fork_hash = Hash::random();
	test_state.rp_info.insert(
		fork_hash,
		RelayParentInfo {
			number: 17,
			parent: get_parent_hash(17),
			session_index: prev_leaf_info.session_index,
			claim_queue: cq.clone(),
			assigned_core: prev_leaf_info.assigned_core,
		},
	);
	futures::join!(test_state.handle_view_update(vec![get_hash(17), fork_hash]), async {
		state
			.handle_our_view_change(&mut sender, OurView::new([get_hash(17), fork_hash], 0))
			.await
			.unwrap()
	});

	assert_eq!(state.connected_peers(), expected_connected_peers);
	test_state.assert_no_messages().await;

	// With both leaves in view, assignments = {600, 200}. Declarations for either para are
	// accepted.
	let peer_200 = peer_ids[10];
	let peer_600 = peer_ids[11];
	state.handle_declare(&mut sender, peer_200, 200.into()).await;
	test_state.assert_no_messages().await;

	state.handle_declare(&mut sender, peer_600, 600.into()).await;
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), expected_connected_peers);
}

// Test peer connection inheritance across group rotations.
#[tokio::test]
async fn test_peer_connections_across_group_rotations() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let active_leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	let initial_core = active_leaf_info.assigned_core;
	assert_eq!(initial_core, CoreIndex(0));
	let next_core = CoreIndex(1);

	// Set the rotation frequency to 12, so that the core is switched to core 1 on block 11.
	test_state
		.session_info
		.get_mut(&active_leaf_info.session_index)
		.unwrap()
		.group_rotation_info
		.group_rotation_frequency = 12;

	let mut state = make_state(MockDb::default(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	let mut cq = active_leaf_info.claim_queue.clone();
	cq.insert(next_core, std::iter::repeat(600.into()).take(3).collect());

	for rp_info in test_state.rp_info.values_mut() {
		rp_info.claim_queue = cq.clone();
	}

	// Leaf 8 has 100, 200, 100.
	// Leaf 9 has 200, 100, 200.
	// Leaf 10 has 100, 200, 100.
	// Leaves 11-13 switch to 600, 600, 600

	// First 5 peers will be declared for para 100.
	// Last 5 peers undeclared.
	let peer_ids = (0..10).map(|i| peer_id(i)).collect::<Vec<_>>();

	for id in peer_ids.iter() {
		state.handle_peer_connected(&mut sender, *id, CollationVersion::V2).await;
	}
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), peer_ids.clone().into_iter().collect());

	for id in &peer_ids[..5] {
		state.handle_declare(&mut sender, *id, 100.into()).await;
	}
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), peer_ids.clone().into_iter().collect());

	for height in 11..=13 {
		test_state.rp_info.insert(
			get_hash(height),
			RelayParentInfo {
				number: height,
				parent: get_parent_hash(height),
				session_index: active_leaf_info.session_index,
				claim_queue: cq.clone(),
				assigned_core: next_core,
			},
		);
	}

	for height in 11..=12 {
		test_state.activate_leaf(&mut state, height).await;
		test_state.assert_no_messages().await;
		assert_eq!(state.connected_peers(), peer_ids.clone().into_iter().collect());
	}

	test_state.activate_leaf(&mut state, 13).await;
	test_state.assert_peers_disconnected((&peer_ids[0..5]).to_vec()).await;
	assert_eq!(state.connected_peers(), (&peer_ids[5..]).into_iter().cloned().collect());

	// Declare the yet undeclared peers for para 600.
	for id in &peer_ids[5..] {
		state.handle_declare(&mut sender, *id, 600.into()).await;
	}
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), (&peer_ids[5..]).into_iter().cloned().collect());
}

// Test reputation bumps on finalized block notification.
#[tokio::test]
async fn finalized_block_notification() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);

	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	// Add 3 peers and connect them
	let first_peer = peer_id(1);
	let second_peer = peer_id(2);
	let third_peer = peer_id(3);
	let peers = vec![first_peer, second_peer, third_peer];

	for peer in peers.iter() {
		state.handle_peer_connected(&mut sender, *peer, CollationVersion::V2).await;
	}
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), peers.clone().into_iter().collect());

	// Finalize block 5, no expected bumps, because there are no included candidates.
	let processed = state.processed_finalized_block_number().await.unwrap_or_default();
	futures::join!(test_state.handle_finalized_block(processed, 5), async {
		state.handle_finalized_block(&mut sender, get_hash(5), 5).await.unwrap()
	});
	test_state.assert_no_messages().await;
	// The first finalized block notification queries and prunes the registered paras.
	assert_eq!(
		db.witnessed_prunes(),
		vec![[100.into(), 200.into(), 600.into()].into_iter().collect::<BTreeSet<ParaId>>()]
	);

	// Add one included candidate at block 6 for first peer and para 100.
	test_state.set_candidates_pending_availability(
		[(get_hash(6), vec![(ParaId::from(100), first_peer)])].into_iter().collect(),
	);

	let mut expected_bumps = BTreeMap::new();
	expected_bumps.insert(
		ParaId::new(100),
		[(first_peer, Score::new(VALID_INCLUDED_CANDIDATE_BUMP))].into_iter().collect(),
	);

	let processed = state.processed_finalized_block_number().await.unwrap_or_default();
	futures::join!(test_state.handle_finalized_block(processed, 6), async {
		state.handle_finalized_block(&mut sender, get_hash(6), 6).await.unwrap()
	});
	test_state.assert_no_messages().await;

	assert_eq!(db.witnessed_bumps(), expected_bumps);

	// This peer is not even connected, but we should process its reputation bumps.
	let fourth_peer = peer_id(4);

	test_state.set_candidates_pending_availability(
		[
			// Keep this one to ensure that we don't end up processing it again.
			(get_hash(6), vec![(ParaId::from(100), first_peer)]),
			(
				get_hash(7),
				vec![
					(ParaId::from(200), first_peer),
					(ParaId::from(200), first_peer),
					(ParaId::from(200), second_peer),
				],
			),
			(get_hash(8), vec![(ParaId::from(100), fourth_peer)]),
			(get_hash(10), vec![(ParaId::from(100), first_peer)]),
		]
		.into_iter()
		.collect(),
	);

	let mut expected_bumps = BTreeMap::new();
	expected_bumps.insert(
		ParaId::new(100),
		[
			(first_peer, Score::new(VALID_INCLUDED_CANDIDATE_BUMP)),
			(fourth_peer, Score::new(VALID_INCLUDED_CANDIDATE_BUMP)),
		]
		.into_iter()
		.collect(),
	);
	expected_bumps.insert(
		ParaId::new(200),
		[
			(first_peer, Score::new(2 * VALID_INCLUDED_CANDIDATE_BUMP)),
			(second_peer, Score::new(VALID_INCLUDED_CANDIDATE_BUMP)),
		]
		.into_iter()
		.collect(),
	);

	// Add multiple included candidates at different block heights and check that they are processed
	// accordingly.
	let processed = state.processed_finalized_block_number().await.unwrap_or_default();
	futures::join!(test_state.handle_finalized_block(processed, 10), async {
		state.handle_finalized_block(&mut sender, get_hash(10), 10).await.unwrap()
	});
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), peers.clone().into_iter().collect());

	assert_eq!(db.witnessed_bumps(), expected_bumps);
}

// Test various scenarios for rejecting advertisements.
#[tokio::test]
async fn test_advertisement_rejections() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();

	let mut state = make_state(MockDb::default(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	assert!(state.take_replan());
	let peer_id = PeerId::random();

	let (ccr, adv) = dummy_candidate(
		active_leaf,
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);

	let prospective_candidate = adv.prospective_candidate;

	// Send advertisement from a peer that is not connected. Will be dropped.
	let pc = vec![];
	state
		.handle_advertisement(
			&mut sender,
			peer_id,
			active_leaf,
			pc.clone().into_iter().collect(),
			None,
			None,
		)
		.await;
	assert!(state.advertisements().is_empty());
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;

	// Send advertisement from a peer that is connected but not declared. Will be dropped.
	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;

	state
		.handle_advertisement(
			&mut sender,
			peer_id,
			active_leaf,
			prospective_candidate.into_iter().collect(),
			None,
			None,
		)
		.await;
	assert!(state.advertisements().is_empty());
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;

	// Now declare. Still, the old advertisement was dropped.
	state.handle_declare(&mut sender, peer_id, 100.into()).await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;
	assert!(state.advertisements().is_empty());

	// Relay parent outside view.
	state
		.handle_advertisement(
			&mut sender,
			peer_id,
			get_hash(11),
			prospective_candidate.into_iter().collect(),
			None,
			None,
		)
		.await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;
	assert!(state.advertisements().is_empty());

	// Blocked by the backing subsystem. Do this twice, so that we reach the per-rp candidate limit
	// of this peer (which is 2).
	for _ in 0..2 {
		futures::join!(
			state.handle_advertisement(
				&mut sender,
				peer_id,
				active_leaf,
				prospective_candidate.into_iter().collect(),
				None,
				None
			),
			test_state.assert_can_second_request(adv, false)
		);
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_no_messages().await;
		assert!(state.advertisements().is_empty());
	}

	// Here comes a valid advertisement, will be rejected because we reached the limit.
	state
		.handle_advertisement(
			&mut sender,
			peer_id,
			active_leaf,
			prospective_candidate.into_iter().collect(),
			None,
			None,
		)
		.await;
	assert!(!state.take_replan());

	// Let's add a new peerid then.
	let peer_id = PeerId::random();
	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	// Add a valid advertisement.
	let adv = Advertisement {
		peer_id,
		para_id: 100.into(),
		scheduling_parent: active_leaf,
		prospective_candidate,
		advertised_descriptor_version: None,
	};
	test_state.handle_advertisement(&mut state, adv).await;
	assert_eq!(state.advertisements(), [adv].into());
	assert!(state.take_replan());

	// Duplicate advertisement. Only one fetch request will be launched.
	state
		.handle_advertisement(
			&mut sender,
			peer_id,
			active_leaf,
			prospective_candidate.into_iter().collect(),
			None,
			None,
		)
		.await;
	assert_eq!(state.advertisements(), [adv].into());
	assert!(!state.take_replan());
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(adv).await;
	// Segment entitlements are spent at launch.
	assert!(state.advertisements().is_empty());
	test_state.assert_no_messages().await;

	// We still detect the duplicate advertisement with a fetching collation.
	state
		.handle_advertisement(
			&mut sender,
			peer_id,
			active_leaf,
			prospective_candidate.into_iter().collect(),
			None,
			None,
		)
		.await;
	assert!(state.advertisements().is_empty());
	assert!(!state.take_replan());
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;

	// We still detect the duplicate advertisement with the fetched collation.
	test_state
		.handle_fetched_collation(&mut state, adv, ccr.to_plain(), None, adv.scheduling_parent)
		.await;
	test_state.assert_no_messages().await;
	assert!(state.advertisements().is_empty());
	assert!(state.take_replan());
	state
		.handle_advertisement(
			&mut sender,
			peer_id,
			active_leaf,
			prospective_candidate.into_iter().collect(),
			None,
			None,
		)
		.await;

	assert!(!state.take_replan());
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;

	// We still detect the duplicate advertisement with a seconded collation.
	let parent = ccr.descriptor.relay_parent();
	test_state
		.second_collation(&mut state, peer_id, CollationVersion::V2, ccr, parent)
		.await;
	test_state.assert_no_messages().await;
	assert!(state.advertisements().is_empty());
	state
		.handle_advertisement(
			&mut sender,
			peer_id,
			active_leaf,
			prospective_candidate.into_iter().collect(),
			None,
			None,
		)
		.await;
	assert!(!state.take_replan());
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	assert!(state.advertisements().is_empty());
	test_state.assert_no_messages().await;

	// V1 advertisement rejected on non-active leaf.
	let peer_id = PeerId::random();
	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;
	state
		.handle_advertisement(
			&mut sender,
			peer_id,
			get_hash(9),
			pc.into_iter().collect(),
			None,
			None,
		)
		.await;
	assert!(state.advertisements().is_empty());
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;
	assert!(!state.take_replan());
}

#[tokio::test]
async fn test_collation_fetch_failure() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();

	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	let mut ccr = dummy_committed_candidate_receipt_v2(active_leaf);
	ccr.descriptor.set_para_id(100.into());
	ccr.descriptor.set_persisted_validation_data_hash(dummy_pvd().hash());
	ccr.descriptor.set_core_index(leaf_info.assigned_core);
	ccr.descriptor.set_session_index(leaf_info.session_index);

	let receipt = ccr.to_plain();
	let prospective_candidate = Some(ProspectiveCandidate::ByHash {
		candidate_hash: receipt.hash(),
		parent_head_data_hash: dummy_pvd().parent_head.hash(),
	});

	// Different network errors.
	for (err, maybe_slash) in [
		// Cancelled by us. No slash.
		(Err(CollationFetchError::Cancelled), None),
		// Network error. No slash as it could be caused by us.
		(
			Err(CollationFetchError::Request(RequestError::NetworkError(
				RequestFailure::NotConnected,
			))),
			None,
		),
		(
			Err(CollationFetchError::Request(RequestError::NetworkError(RequestFailure::Network(
				OutboundFailure::DialFailure,
			)))),
			None,
		),
		// Timeout. Slash.
		(
			Err(CollationFetchError::Request(RequestError::NetworkError(RequestFailure::Network(
				OutboundFailure::Timeout,
			)))),
			Some(FAILED_FETCH_SLASH),
		),
		// Invalid response. Slash.
		(
			Err(CollationFetchError::Request(RequestError::InvalidResponse("Invalid".into()))),
			Some(FAILED_FETCH_SLASH),
		),
	] {
		let peer_id = PeerId::random();

		// We reuse the same advertisement, to test that if a fetch fails, another peer can
		// advertise the same collation.
		let adv = Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent: active_leaf,
			prospective_candidate,
			advertised_descriptor_version: None,
		};

		state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;
		state.handle_declare(&mut sender, peer_id, 100.into()).await;

		test_state.handle_advertisement(&mut state, adv).await;
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_collation_request(adv).await;
		// The accepted advertisement armed the planner; drain before the conclusion check.
		assert!(state.take_replan());

		state.handle_fetched_collation(&mut sender, (adv, err)).await;
		// Every fetch conclusion re-arms the planner.
		assert!(state.take_replan());
		// Once it failed, we no longer retry it.
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		assert_eq!(db.witnessed_slash(), maybe_slash.map(|score| (peer_id, adv.para_id, score)));
		test_state.assert_no_messages().await;
	}

	// Received paraid is different than the advertised one. Try for both network protocol
	// versions. This implies a check on the candidate hash as well for v2 advertisements.
	for version in [CollationVersion::V1, CollationVersion::V2] {
		let peer_id = PeerId::random();

		let adv = Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent: active_leaf,
			prospective_candidate: if version == CollationVersion::V2 {
				prospective_candidate
			} else {
				None
			},
			advertised_descriptor_version: None,
		};

		state.handle_peer_connected(&mut sender, peer_id, version).await;
		state.handle_declare(&mut sender, peer_id, 100.into()).await;

		test_state.handle_advertisement(&mut state, adv).await;

		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_collation_request(adv).await;

		let mut receipt = receipt.clone();
		// Modify the paraid.
		receipt.descriptor.set_para_id(200.into());
		let res = Ok(CollationFetchingResponse::Collation(receipt, dummy_pov()));
		state.handle_fetched_collation(&mut sender, (adv, res)).await;
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		assert_eq!(db.witnessed_slash(), Some((peer_id, adv.para_id, FAILED_FETCH_SLASH)));
		test_state.assert_no_messages().await;
	}

	// Relay parent mismatch.
	{
		let peer_id = PeerId::random();

		let mut adv = Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent: active_leaf,
			prospective_candidate,
			advertised_descriptor_version: None,
		};

		state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;
		state.handle_declare(&mut sender, peer_id, 100.into()).await;

		test_state.handle_advertisement(&mut state, adv).await;

		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_collation_request(adv).await;

		// Modify the relay parent.
		adv.scheduling_parent = get_hash(8);
		let res = Ok(CollationFetchingResponse::Collation(receipt.clone(), dummy_pov()));
		state.handle_fetched_collation(&mut sender, (adv, res)).await;
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		assert_eq!(db.witnessed_slash(), Some((peer_id, adv.para_id, FAILED_FETCH_SLASH)));
		test_state.assert_no_messages().await;
	}

	// Invalid core index on the v2 descriptor.
	{
		let peer_id = PeerId::random();

		let mut receipt = receipt.clone();
		// Set a different core index.
		receipt.descriptor.set_core_index(CoreIndex(5));

		let prospective_candidate = Some(ProspectiveCandidate::ByHash {
			candidate_hash: receipt.hash(),
			parent_head_data_hash: dummy_pvd().parent_head.hash(),
		});
		let adv = Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent: active_leaf,
			prospective_candidate,
			advertised_descriptor_version: None,
		};

		state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;
		state.handle_declare(&mut sender, peer_id, 100.into()).await;

		test_state.handle_advertisement(&mut state, adv).await;

		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_collation_request(adv).await;

		let res = Ok(CollationFetchingResponse::Collation(receipt, dummy_pov()));
		state.handle_fetched_collation(&mut sender, (adv, res)).await;
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		assert_eq!(db.witnessed_slash(), Some((peer_id, adv.para_id, FAILED_FETCH_SLASH)));
		test_state.assert_no_messages().await;
	}

	// Invalid session index on the v2 descriptor.
	{
		let peer_id = PeerId::random();

		let mut receipt = receipt.clone();
		// Set a different session index.
		receipt.descriptor.set_session_index(5);

		let prospective_candidate = Some(ProspectiveCandidate::ByHash {
			candidate_hash: receipt.hash(),
			parent_head_data_hash: dummy_pvd().parent_head.hash(),
		});
		let adv = Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent: active_leaf,
			prospective_candidate,
			advertised_descriptor_version: None,
		};

		state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;
		state.handle_declare(&mut sender, peer_id, 100.into()).await;

		test_state.handle_advertisement(&mut state, adv).await;

		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_collation_request(adv).await;

		let res = Ok(CollationFetchingResponse::Collation(receipt, dummy_pov()));
		state.handle_fetched_collation(&mut sender, (adv, res)).await;
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		assert_eq!(db.witnessed_slash(), Some((peer_id, adv.para_id, FAILED_FETCH_SLASH)));
		test_state.assert_no_messages().await;
	}

	// PVD not found. Only check for v1 advertisement, which will end up querying the runtime.
	// For v2 advertisement, an unknown PVD could just make the candidate end up being blocked from
	// seconding (and we have other tests for it)
	{
		let peer_id = PeerId::random();

		let adv = Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent: active_leaf,
			prospective_candidate: None,
			advertised_descriptor_version: None,
		};

		state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V1).await;
		state.handle_declare(&mut sender, peer_id, 100.into()).await;

		test_state.handle_advertisement(&mut state, adv).await;

		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_collation_request(adv).await;

		let res = Ok(CollationFetchingResponse::Collation(receipt.clone(), dummy_pov()));
		futures::join!(
			state.handle_fetched_collation(&mut sender, (adv, res)),
			test_state.assert_pvd_request(adv, None, adv.scheduling_parent)
		);
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		// No slash, as it's not the collator's fault.
		assert_eq!(db.witnessed_slash(), None);
		test_state.assert_no_messages().await;
	}

	// PVD Mismatch.
	{
		let peer_id = PeerId::random();

		let mut receipt = receipt.clone();
		// Modify some random thing in the receipt so that we get a different candidate.
		receipt.commitments_hash = get_hash(10);

		let prospective_candidate = Some(ProspectiveCandidate::ByHash {
			candidate_hash: receipt.hash(),
			parent_head_data_hash: dummy_pvd().parent_head.hash(),
		});

		let adv = Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent: active_leaf,
			prospective_candidate,
			advertised_descriptor_version: None,
		};

		state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;
		state.handle_declare(&mut sender, peer_id, 100.into()).await;

		test_state.handle_advertisement(&mut state, adv).await;

		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_collation_request(adv).await;

		// Modify the PVD.
		let mut pvd: PersistedValidationData = dummy_pvd();
		pvd.relay_parent_number = 100;

		let res = Ok(CollationFetchingResponse::Collation(receipt, dummy_pov()));
		futures::join!(
			state.handle_fetched_collation(&mut sender, (adv, res)),
			test_state.assert_pvd_request(adv, Some(pvd), adv.scheduling_parent)
		);
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		assert_eq!(db.witnessed_slash(), Some((peer_id, adv.para_id, FAILED_FETCH_SLASH)));
		test_state.assert_no_messages().await;
	}

	// Parent head data mismatch.
	{
		let peer_id = PeerId::random();

		let mut receipt = receipt.clone();
		// Modify some random thing in the receipt so that we get a different candidate.
		receipt.commitments_hash = get_hash(11);

		let prospective_candidate = Some(ProspectiveCandidate::ByHash {
			candidate_hash: receipt.hash(),
			// Randomly modify the parent head data hash in the advertisement.
			parent_head_data_hash: get_hash(11),
		});

		let adv = Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent: active_leaf,
			prospective_candidate,
			advertised_descriptor_version: None,
		};

		state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;
		state.handle_declare(&mut sender, peer_id, 100.into()).await;

		test_state.handle_advertisement(&mut state, adv).await;

		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_collation_request(adv).await;

		let res = Ok(CollationFetchingResponse::Collation(receipt, dummy_pov()));
		futures::join!(
			state.handle_fetched_collation(&mut sender, (adv, res)),
			test_state.assert_pvd_request(adv, Some(dummy_pvd()), adv.scheduling_parent)
		);
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		assert_eq!(db.witnessed_slash(), Some((peer_id, adv.para_id, FAILED_FETCH_SLASH)));
		test_state.assert_no_messages().await;
	}

	// Parent head data mismatch with full parent head present in response.
	{
		let peer_id = PeerId::random();

		let mut receipt = receipt.clone();
		// Modify some random thing in the receipt so that we get a different candidate.
		receipt.commitments_hash = get_hash(12);

		let prospective_candidate = Some(ProspectiveCandidate::ByHash {
			candidate_hash: receipt.hash(),
			parent_head_data_hash: dummy_pvd().parent_head.hash(),
		});

		let adv = Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent: active_leaf,
			prospective_candidate,
			advertised_descriptor_version: None,
		};

		state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;
		state.handle_declare(&mut sender, peer_id, 100.into()).await;

		test_state.handle_advertisement(&mut state, adv).await;

		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_collation_request(adv).await;

		let res = Ok(CollationFetchingResponse::CollationWithParentHeadData {
			receipt,
			pov: dummy_pov(),
			// Add a random head data here.
			parent_head_data: HeadData(vec![1, 2, 3]),
		});
		let mut pvd = dummy_pvd();
		pvd.parent_head = HeadData(vec![1, 2, 3]);

		futures::join!(
			state.handle_fetched_collation(&mut sender, (adv, res)),
			test_state.assert_pvd_request(adv, Some(pvd), adv.scheduling_parent)
		);
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		assert_eq!(db.witnessed_slash(), Some((peer_id, adv.para_id, FAILED_FETCH_SLASH)));
		test_state.assert_no_messages().await;
	}
}

// Test a successful collation fetch that went out of the view in the meantime.
#[tokio::test]
async fn test_collation_response_out_of_view() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();

	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	let peer_id = PeerId::random();

	let (_, adv) = dummy_candidate(
		active_leaf,
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);

	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	test_state.handle_advertisement(&mut state, adv).await;

	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	let _resp_sender = test_state.assert_collation_request(adv).await;

	// While the request is pending, handle some new leaf updates which remove the 10th relay parent
	// from the allowed ancestry.
	for height in 11..=13 {
		test_state.rp_info.insert(
			get_hash(height),
			RelayParentInfo {
				number: height,
				parent: get_parent_hash(height),
				session_index: leaf_info.session_index,
				claim_queue: leaf_info.claim_queue.clone(),
				assigned_core: leaf_info.assigned_core,
			},
		);
	}

	test_state.activate_leaf(&mut state, 11).await;

	assert!(state
		.collation_response_stream()
		.select_next_some()
		.timeout(TIMEOUT)
		.await
		.is_none());

	test_state.activate_leaf(&mut state, 12).await;

	assert!(state
		.collation_response_stream()
		.select_next_some()
		.timeout(TIMEOUT)
		.await
		.is_none());

	test_state.activate_leaf(&mut state, 13).await;

	let resp = state
		.collation_response_stream()
		.select_next_some()
		.timeout(TIMEOUT)
		.await
		.unwrap();

	assert_eq!(resp.0, adv);
	assert_matches!(resp.1, Err(CollationFetchError::Cancelled));

	assert!(db.witnessed_slash().is_none());
}

// TODO(https://github.com/paritytech/polkadot-sdk/issues/10883?issue=paritytech%7Cpolkadot-sdk%7C11084): Add
// test for V3 descriptor validation. The previous test_v2_descriptor_without_feature_enabled was
// removed because V2 is now always enabled. The v3_enabled parameter has been removed from
// descriptor methods.

// Test that we accept v1 candidates.
#[rstest]
#[tokio::test]
async fn v1_descriptor_compatibility() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);

	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	// Build a v1 candidate.
	let mut ccr = dummy_committed_candidate_receipt(active_leaf);
	ccr.descriptor.para_id = 100.into();
	ccr.descriptor.persisted_validation_data_hash = dummy_pvd().hash();

	let receipt = ccr.to_plain();
	let prospective_candidate = Some(ProspectiveCandidate::ByHash {
		candidate_hash: receipt.hash(),
		parent_head_data_hash: dummy_pvd().parent_head.hash(),
	});

	let peer_id = PeerId::random();

	let adv = Advertisement {
		peer_id,
		para_id: 100.into(),
		scheduling_parent: active_leaf,
		prospective_candidate,
		advertised_descriptor_version: None,
	};

	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	test_state.handle_advertisement(&mut state, adv).await;

	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(adv).await;

	test_state
		.handle_fetched_collation(&mut state, adv, receipt.into(), None, adv.scheduling_parent)
		.await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;
	let parent = ccr.descriptor.relay_parent;
	test_state
		.second_collation(&mut state, peer_id, CollationVersion::V2, ccr.into(), parent)
		.await;
	test_state.assert_no_messages().await;
}

// Test a collation that is deemed invalid by candidate-backing. It will lead to a slash and the
// slot will be freed.
#[tokio::test]
async fn test_invalid_collation() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();

	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	let good_peer = peer_id(1);
	let bad_peer = peer_id(2);

	// Add two collations from this good peer.
	let (_, first_adv) = dummy_candidate(
		active_leaf,
		100.into(),
		good_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		Hash::from_low_u64_be(0),
	);
	let (_, second_adv) = dummy_candidate(
		active_leaf,
		100.into(),
		good_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		Hash::from_low_u64_be(1),
	);

	// Add one advertisement from the bad peer.
	let (bad_ccr, bad_adv) = dummy_candidate(
		active_leaf,
		100.into(),
		bad_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);
	let bad_receipt = bad_ccr.to_plain();

	state.handle_peer_connected(&mut sender, good_peer, CollationVersion::V2).await;
	state.handle_declare(&mut sender, good_peer, 100.into()).await;
	state.handle_peer_connected(&mut sender, bad_peer, CollationVersion::V2).await;
	state.handle_declare(&mut sender, bad_peer, 100.into()).await;

	test_state.handle_advertisement(&mut state, first_adv).await;
	test_state.handle_advertisement(&mut state, bad_adv).await;

	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_requests([first_adv, bad_adv].into()).await;
	test_state.assert_no_messages().await;

	test_state.handle_advertisement(&mut state, second_adv).await;
	// Second advertisement is not fetched yet, because all claim queue slots are occupied.
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;

	// The bad collation was fetched and it's invalid.
	test_state
		.handle_fetched_collation(
			&mut state,
			bad_adv,
			bad_receipt.clone(),
			None,
			bad_adv.scheduling_parent,
		)
		.await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;
	assert!(state.take_replan());

	let parent = bad_receipt.descriptor.scheduling_parent();
	state.handle_invalid_collation(bad_receipt, parent).await;
	assert!(state.take_replan());

	// Bad peer was slashed.
	assert_eq!(db.witnessed_slash().unwrap(), (bad_peer, 100.into(), INVALID_COLLATION_SLASH));

	// Good peer now gets a chance of sending the second collation, since the slot was freed.
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(second_adv).await;
	test_state.assert_no_messages().await;
}

// Test that we can block the seconding of a candidate on its parent being seconded. If the parent
// is later seconded, the child is unblocked as well. If it's invalid, it will be dropped.
#[rstest]
#[case(true)]
#[case(false)]
#[tokio::test]
async fn test_blocked_from_seconding_by_parent(#[case] valid_parent: bool) {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	let para_id = ParaId::from(100);

	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	let first_peer = peer_id(1);
	let second_peer = peer_id(2);

	let (first_pvd, first_ccr, first_adv) = {
		let pvd = PersistedValidationData {
			parent_head: HeadData(vec![0]),
			relay_parent_number: 10,
			..dummy_pvd()
		};
		let ccr = CommittedCandidateReceipt {
			descriptor: make_valid_candidate_descriptor_v2(
				para_id,
				active_leaf,
				leaf_info.assigned_core,
				leaf_info.session_index,
				pvd.hash(),
				dummy_pov().hash(),
				Hash::zero(),
				HeadData(vec![1]).hash(),
				Hash::zero(),
			),
			commitments: dummy_candidate_commitments(HeadData(vec![1])),
		};

		let receipt = ccr.to_plain();
		let prospective_candidate = Some(ProspectiveCandidate::ByHash {
			candidate_hash: receipt.hash(),
			parent_head_data_hash: pvd.parent_head.hash(),
		});

		(
			pvd,
			ccr,
			Advertisement {
				peer_id: first_peer,
				para_id,
				scheduling_parent: active_leaf,
				prospective_candidate,
				advertised_descriptor_version: None,
			},
		)
	};

	let (second_pvd, second_ccr, second_adv) = {
		let pvd = PersistedValidationData {
			parent_head: HeadData(vec![1]),
			relay_parent_number: 10,
			..dummy_pvd()
		};
		let ccr = CommittedCandidateReceipt {
			descriptor: make_valid_candidate_descriptor_v2(
				para_id,
				active_leaf,
				leaf_info.assigned_core,
				leaf_info.session_index,
				pvd.hash(),
				dummy_pov().hash(),
				Hash::zero(),
				HeadData(vec![2]).hash(),
				Hash::zero(),
			),
			commitments: dummy_candidate_commitments(HeadData(vec![2])),
		};

		let receipt = ccr.to_plain();
		let prospective_candidate = Some(ProspectiveCandidate::ByHash {
			candidate_hash: receipt.hash(),
			parent_head_data_hash: pvd.parent_head.hash(),
		});

		(
			pvd,
			ccr,
			Advertisement {
				peer_id: second_peer,
				para_id,
				scheduling_parent: active_leaf,
				prospective_candidate,
				advertised_descriptor_version: None,
			},
		)
	};

	state.handle_peer_connected(&mut sender, first_peer, CollationVersion::V2).await;
	state
		.handle_peer_connected(&mut sender, second_peer, CollationVersion::V2)
		.await;
	state.handle_declare(&mut sender, first_peer, para_id).await;
	state.handle_declare(&mut sender, second_peer, para_id).await;

	test_state.handle_advertisement(&mut state, first_adv).await;
	test_state.handle_advertisement(&mut state, second_adv).await;

	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_requests([first_adv, second_adv].into()).await;

	test_state.assert_no_messages().await;

	// First collation is fetched and seconding kicks off.
	test_state
		.handle_fetched_collation(
			&mut state,
			first_adv,
			first_ccr.to_plain(),
			Some(first_pvd),
			first_adv.scheduling_parent,
		)
		.await;

	// Then, second collation is fetched and seconding kicks off (but the parent header is unknown
	// so it get's blocked)
	let res = Ok(CollationFetchingResponse::Collation(second_ccr.to_plain(), dummy_pov()));
	futures::join!(
		state.handle_fetched_collation(&mut sender, (second_adv, res)),
		// We don't know it's pvd so it gets blocked from seconding
		test_state.assert_pvd_request(second_adv, None, second_adv.scheduling_parent)
	);
	test_state.assert_no_messages().await;

	// Add two new advertisements. They won't be fetched right now because we already have two
	// claims (one blocked and one seconding).
	let third_peer = peer_id(3);
	state.handle_peer_connected(&mut sender, third_peer, CollationVersion::V2).await;
	state.handle_declare(&mut sender, third_peer, para_id).await;
	let (_, pending_adv_1) = dummy_candidate(
		active_leaf,
		para_id,
		third_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		Hash::from_low_u64_be(0),
	);
	let (_, pending_adv_2) = dummy_candidate(
		active_leaf,
		para_id,
		third_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		Hash::from_low_u64_be(1),
	);

	test_state.handle_advertisement(&mut state, pending_adv_1).await;
	test_state.handle_advertisement(&mut state, pending_adv_2).await;

	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;

	test_state.assert_no_messages().await;
	assert!(state.take_replan());

	if valid_parent {
		let parent = first_ccr.descriptor.relay_parent();
		let statement = make_seconded_statement(&test_state.keystore, first_ccr);

		futures::join!(
			async {
				state.handle_seconded_collation(&mut sender, statement.clone(), parent).await;
			},
			test_state.assert_pvd_request(
				second_adv,
				Some(second_pvd.clone()),
				second_adv.scheduling_parent
			)
		);

		test_state
			.assert_collation_seconded_notification(
				first_peer,
				CollationVersion::V2,
				statement.into(),
			)
			.await;

		// Second collation was unblocked and it began being seconded.
		let parent = second_ccr.descriptor.relay_parent();
		test_state
			.assert_seconding_kickoff(second_ccr.to_plain(), second_pvd, dummy_pov(), parent)
			.await;

		test_state.assert_no_messages().await;
		let parent = second_ccr.descriptor.relay_parent();
		test_state
			.second_collation(&mut state, second_peer, CollationVersion::V2, second_ccr, parent)
			.await;
		test_state.assert_no_messages().await;
		assert!(!state.take_replan());

		// These claims are not getting freed, since the collations were valid, so we can't launch
		// more collation requests.
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_no_messages().await;
	} else {
		let parent = first_ccr.descriptor.relay_parent();
		state.handle_invalid_collation(first_ccr.to_plain(), parent).await;
		assert!(state.take_replan());
		assert_eq!(
			db.witnessed_slash().unwrap(),
			(first_peer, 100.into(), INVALID_COLLATION_SLASH)
		);
		test_state.assert_no_messages().await;

		// Both claims were freed, we can now launch two new requests.
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state
			.assert_collation_requests([pending_adv_1, pending_adv_2].into())
			.await;
	}
}

// A blocked-from-seconding collation gets pruned once its scheduling parent goes out of view.
//
// We park a fetched collation in `blocked_from_seconding` (by withholding its PVD), then activate
// new leaves until the SP falls out of the implicit view. Asserting "the parked entry is gone"
// requires an *observable* signal: a parked entry would re-attempt seconding when its parent
// gets seconded (`note_seconded` walks `blocked_from_seconding[(para, parent_head)]` and re-runs
// `can_begin_seconding`, sending a fresh PVD request). So after the SP exits view we send a
// seconded statement matching the parked candidate's parent and assert no PVD request is sent —
// the entry must already be gone.
#[tokio::test]
async fn test_outdated_blocked_collations_are_pruned() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	let para_id = ParaId::from(100);

	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	test_state.rp_info.insert(
		get_hash(11),
		RelayParentInfo {
			number: 11,
			parent: get_parent_hash(11),
			session_index: leaf_info.session_index,
			claim_queue: [(leaf_info.assigned_core, vec![200.into(), 100.into(), 200.into()])]
				.into_iter()
				.collect(),
			assigned_core: leaf_info.assigned_core,
		},
	);
	test_state.rp_info.insert(
		get_hash(12),
		RelayParentInfo {
			number: 12,
			parent: get_parent_hash(12),
			session_index: leaf_info.session_index,
			claim_queue: [(leaf_info.assigned_core, vec![100.into(), 200.into(), 100.into()])]
				.into_iter()
				.collect(),
			assigned_core: leaf_info.assigned_core,
		},
	);
	test_state.rp_info.insert(
		get_hash(13),
		RelayParentInfo {
			number: 13,
			parent: get_parent_hash(13),
			session_index: leaf_info.session_index,
			claim_queue: [(leaf_info.assigned_core, vec![200.into(), 100.into(), 200.into()])]
				.into_iter()
				.collect(),
			assigned_core: leaf_info.assigned_core,
		},
	);

	let peer = peer_id(1);

	let pvd = PersistedValidationData {
		parent_head: HeadData(vec![0]),
		relay_parent_number: 10,
		..dummy_pvd()
	};
	let ccr = CommittedCandidateReceipt {
		descriptor: make_valid_candidate_descriptor_v2(
			para_id,
			active_leaf,
			leaf_info.assigned_core,
			leaf_info.session_index,
			pvd.hash(),
			dummy_pov().hash(),
			Hash::zero(),
			HeadData(vec![1]).hash(),
			Hash::zero(),
		),
		commitments: dummy_candidate_commitments(HeadData(vec![1])),
	};
	let prospective_candidate = Some(ProspectiveCandidate::ByHash {
		candidate_hash: ccr.to_plain().hash(),
		parent_head_data_hash: pvd.parent_head.hash(),
	});
	let adv = Advertisement {
		peer_id: peer,
		para_id,
		scheduling_parent: active_leaf,
		prospective_candidate,
		advertised_descriptor_version: None,
	};

	state.handle_peer_connected(&mut sender, peer, CollationVersion::V2).await;
	state.handle_declare(&mut sender, peer, para_id).await;

	test_state.handle_advertisement(&mut state, adv).await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(adv).await;
	test_state.assert_no_messages().await;

	// Fetch the candidate; PVD lookup is left unanswered so seconding gets blocked-on-parent.
	let res = Ok(CollationFetchingResponse::Collation(ccr.to_plain(), dummy_pov()));
	futures::join!(
		state.handle_fetched_collation(&mut sender, (adv, res)),
		test_state.assert_pvd_request(adv, None, adv.scheduling_parent),
	);
	test_state.assert_no_messages().await;

	// Advance the leaves so sp=10 falls out of view; the parked entry must be pruned along the
	// way.
	test_state.activate_leaf(&mut state, 11).await;
	test_state.activate_leaf(&mut state, 12).await;
	test_state.activate_leaf(&mut state, 13).await;

	// If the parked entry were still there, this seconded statement (whose `output_head`
	// matches the candidate's parent head) would unblock it and trigger a new PVD request.
	let parent = ccr.descriptor.relay_parent();
	let statement = make_seconded_statement(&test_state.keystore, ccr);
	state.handle_seconded_collation(&mut sender, statement, parent).await;

	test_state.assert_no_messages().await;
}

// Test that collation request is cancelled once the relay parent goes out of scope and the claim is
// freed.
#[tokio::test]
async fn test_outdated_fetching_collations_are_pruned() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();

	// Use a uniform CQ entirely for para 100 so capacity reasoning is straightforward — the
	// purpose of this test is pruning of outdated fetches, not multi-para or rotating CQs.
	let same_cq: BTreeMap<CoreIndex, Vec<ParaId>> =
		[(leaf_info.assigned_core, vec![100.into(), 100.into(), 100.into()])]
			.into_iter()
			.collect();
	test_state
		.rp_info
		.get_mut(&get_hash(10))
		.expect("inserted by TestState::default")
		.claim_queue = same_cq.clone();
	test_state.rp_info.insert(
		get_hash(11),
		RelayParentInfo {
			number: 11,
			parent: get_parent_hash(11),
			session_index: leaf_info.session_index,
			claim_queue: same_cq.clone(),
			assigned_core: leaf_info.assigned_core,
		},
	);
	test_state.rp_info.insert(
		get_hash(12),
		RelayParentInfo {
			number: 12,
			parent: get_parent_hash(12),
			session_index: leaf_info.session_index,
			claim_queue: same_cq.clone(),
			assigned_core: leaf_info.assigned_core,
		},
	);
	test_state.rp_info.insert(
		get_hash(13),
		RelayParentInfo {
			number: 13,
			parent: get_parent_hash(13),
			session_index: leaf_info.session_index,
			claim_queue: same_cq,
			assigned_core: leaf_info.assigned_core,
		},
	);

	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	let peer_id = PeerId::random();

	// `first_adv` is at sp=10, the relay parent that will go out of scope when leaf 13
	// activates. We launch its fetch and then move the leaf forward; once sp=10 falls out of
	// view the fetch must get cancelled.
	let (_, first_adv) = dummy_candidate(
		get_hash(10),
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		Hash::from_low_u64_be(0),
	);

	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	test_state.handle_advertisement(&mut state, first_adv).await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	let _pending_first = test_state.assert_collation_request(first_adv).await;

	test_state.activate_leaf(&mut state, 11).await;

	assert!(state
		.collation_response_stream()
		.select_next_some()
		.timeout(TIMEOUT)
		.await
		.is_none());

	test_state.activate_leaf(&mut state, 12).await;

	assert!(state
		.collation_response_stream()
		.select_next_some()
		.timeout(TIMEOUT)
		.await
		.is_none());

	// Activating leaf 13 takes sp=10 out of view; the in-flight fetch must get cancelled.
	test_state.activate_leaf(&mut state, 13).await;
	let resp = state
		.collation_response_stream()
		.select_next_some()
		.timeout(TIMEOUT)
		.await
		.expect("cancellation response should arrive");
	assert_eq!(resp.0, first_adv);
	assert_matches!(resp.1, Err(CollationFetchError::Cancelled));
	state.handle_fetched_collation(&mut sender, resp).await;

	test_state.assert_no_messages().await;
}

// Test that a v1 advertisement cannot claim a future slot.
#[tokio::test]
async fn test_single_collation_per_rp_for_v1_advertisement() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();

	let first_peer = peer_id(1);
	let second_peer = peer_id(2);

	let db = MockDb::new(Arc::new(Mutex::new(move |peer_id, _para_id| {
		if peer_id == first_peer {
			Some(Score::new(100))
		} else {
			None
		}
	})));
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	// Make two v1 advertisements on the same RP. They will both try to claim the same slot, which
	// is not possible. Only one will make it.
	let (first_ccr, mut first_adv) = dummy_candidate(
		get_hash(10),
		100.into(),
		first_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);

	let (_, mut second_adv) = dummy_candidate(
		get_hash(10),
		100.into(),
		second_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		Hash::from_low_u64_be(1),
	);

	// Make them v1 advertisements.
	second_adv.prospective_candidate = None;
	first_adv.prospective_candidate = None;

	state.handle_peer_connected(&mut sender, first_peer, CollationVersion::V1).await;
	state.handle_declare(&mut sender, first_peer, 100.into()).await;
	state
		.handle_peer_connected(&mut sender, second_peer, CollationVersion::V1)
		.await;
	state.handle_declare(&mut sender, second_peer, 100.into()).await;

	test_state.handle_advertisement(&mut state, first_adv).await;

	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(first_adv).await;

	test_state.handle_advertisement(&mut state, second_adv).await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;

	test_state
		.handle_fetched_collation(
			&mut state,
			first_adv,
			first_ccr.to_plain(),
			None,
			first_adv.scheduling_parent,
		)
		.await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;
	let parent = first_ccr.descriptor.relay_parent();
	test_state
		.second_collation(&mut state, first_peer, CollationVersion::V1, first_ccr, parent)
		.await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;

	// Still, adding a v2 advertisement would work.
	let (_, third_adv) = dummy_candidate(
		get_hash(10),
		100.into(),
		first_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		Hash::from_low_u64_be(2),
	);
	test_state.handle_advertisement(&mut state, third_adv).await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(third_adv).await;
}

// Test that activating a new leaf on top of an existing one doesn't overwrite the `PerRelayParent`
// state.
#[tokio::test]
async fn test_view_update_preserves_relay_parent_state() {
	let mut test_state = TestState::default();
	let leaf_a = get_hash(10);
	let leaf_a_info = test_state.rp_info.get(&leaf_a).unwrap().clone();

	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, leaf_a).await;
	let mut sender = test_state.sender.clone();
	assert!(state.take_replan());

	let peer = peer_id(1);

	let (_, adv_a) = dummy_candidate(
		leaf_a,
		100.into(),
		peer,
		leaf_a_info.assigned_core,
		leaf_a_info.session_index,
		dummy_pvd().hash(),
	);

	state.handle_peer_connected(&mut sender, peer, CollationVersion::V2).await;
	state.handle_declare(&mut sender, peer, 100.into()).await;

	test_state.handle_advertisement(&mut state, adv_a).await;

	assert_eq!(state.advertisements(), [adv_a].into());
	assert!(state.take_replan());

	// Now activate a new leaf B which has A in its allowed ancestry
	test_state.rp_info.insert(
		get_hash(11),
		RelayParentInfo {
			number: 11,
			parent: get_parent_hash(11),
			session_index: leaf_a_info.session_index,
			claim_queue: leaf_a_info.claim_queue.clone(),
			assigned_core: leaf_a_info.assigned_core,
		},
	);

	test_state.activate_leaf(&mut state, 11).await;
	assert!(state.take_replan());

	// Advertisement A should still be there
	assert_eq!(state.advertisements(), [adv_a].into());
}

#[tokio::test]
async fn v4_advertise_segment_len_one_is_accepted() {
	let mut test_state = TestState::default();
	test_state
		.node_features
		.resize(node_features::FeatureIndex::CandidateReceiptV3 as usize + 1, false);
	test_state
		.node_features
		.set(node_features::FeatureIndex::CandidateReceiptV3 as u8 as usize, true);

	let active_leaf = get_hash(9);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	let mut state = make_state(MockDb::default(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();
	let peer_id = PeerId::random();

	test_state.activate_leaf(&mut state, 10).await;

	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	let relay_parent = get_hash(8);
	let scheduling_parent = get_hash(9);
	let (ccr, _) = dummy_candidate_v3(
		relay_parent,
		scheduling_parent,
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);
	let output_head_data_hash = ccr.descriptor.para_head();
	let parent_head_data_hash = dummy_pvd().parent_head.hash();
	let candidates = vec![protocol_v4::CandidateFingerprint {
		output_head_data_hash,
		parent_head_data_hash,
		claim_queue_offset: 0,
	}]
	.try_into()
	.unwrap();
	// The advertisement the subsystem is expected to construct from the segment.
	let adv = Advertisement {
		peer_id,
		para_id: 100.into(),
		scheduling_parent,
		prospective_candidate: Some(ProspectiveCandidate::ByOutputHead {
			output_head_data_hash,
			parent_head_data_hash,
		}),
		advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
	};
	process_incoming_peer_message(
		&mut sender,
		&mut state,
		peer_id,
		CollationProtocols::V4(protocol_v4::AdvertiseSegment {
			scheduling_parent,
			candidates_descriptor_version: CandidateDescriptorVersion::V3,
			candidates,
			para_id: 100.into(),
		}),
	)
	.await;
	// No CanSecond request for a V4 advertisement: the claim has no candidate hash to
	// pre-check; bad advertisements self-heal post-fetch.
	test_state.assert_no_messages().await;

	// The fetch goes out over the V3 request-response protocol, keyed by output head.
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(adv).await;

	test_state
		.handle_fetched_collation(&mut state, adv, ccr.to_plain(), None, relay_parent)
		.await;
}

// V4 has no `Declare` message: a peer's first `AdvertiseSegment` binds it to the advertised
// para. Covers the accept, para-switch and unscheduled-para outcomes of the implicit
// declaration, plus the unconnected-peer no-op.
#[tokio::test]
async fn v4_first_advertisement_implicitly_declares() {
	let mut test_state = TestState::default();
	test_state
		.node_features
		.resize(node_features::FeatureIndex::CandidateReceiptV3 as usize + 1, false);
	test_state
		.node_features
		.set(node_features::FeatureIndex::CandidateReceiptV3 as u8 as usize, true);

	let active_leaf = get_hash(9);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	let mut state = make_state(MockDb::default(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();
	let peer_id = PeerId::random();

	test_state.activate_leaf(&mut state, 10).await;

	let relay_parent = get_hash(8);
	let scheduling_parent = get_hash(9);
	let (ccr, _) = dummy_candidate_v3(
		relay_parent,
		scheduling_parent,
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);
	let output_head_data_hash = ccr.descriptor.para_head();
	let parent_head_data_hash = dummy_pvd().parent_head.hash();
	let fingerprints = vec![protocol_v4::CandidateFingerprint {
		output_head_data_hash,
		parent_head_data_hash,
		claim_queue_offset: 0,
	}];

	// An advertisement from an unconnected peer cannot declare anything and is dropped.
	test_state
		.send_v4_segment(&mut state, peer_id, scheduling_parent, fingerprints.clone(), 100.into())
		.await;
	test_state.assert_no_messages().await;
	assert!(state.advertisements().is_empty());

	// First advertisement from a connected peer declares it for the advertised para and the
	// advertisement itself is accepted. No `Declare` message is ever sent.
	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
	test_state
		.send_v4_segment(&mut state, peer_id, scheduling_parent, fingerprints.clone(), 100.into())
		.await;
	test_state.assert_no_messages().await;
	let adv = Advertisement {
		peer_id,
		para_id: 100.into(),
		scheduling_parent,
		prospective_candidate: Some(ProspectiveCandidate::ByOutputHead {
			output_head_data_hash,
			parent_head_data_hash,
		}),
		advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
	};
	assert_eq!(state.advertisements(), [adv].into());

	// Advertising a different para is a para switch: the peer is disconnected and its stored
	// advertisements are removed.
	test_state
		.send_v4_segment(&mut state, peer_id, scheduling_parent, fingerprints.clone(), 200.into())
		.await;
	test_state.assert_peers_disconnected([peer_id]).await;
	assert!(state.advertisements().is_empty());
	assert_eq!(state.connected_peers(), Default::default());

	// An advertisement for an unscheduled para gets the peer disconnected, same as an explicit
	// `Declare` for an unscheduled para would on older protocol versions.
	let second_peer = PeerId::random();
	state
		.handle_peer_connected(&mut sender, second_peer, CollationVersion::V4)
		.await;
	test_state
		.send_v4_segment(&mut state, second_peer, scheduling_parent, fingerprints, 600.into())
		.await;
	test_state.assert_peers_disconnected([second_peer]).await;
	assert!(state.advertisements().is_empty());
}

// A fetched collation must match the selected fingerprint: output head, relay parent and the
// declared para are each verified against the receipt (there is no candidate hash to check).
// Every mismatch is malicious and slashed.
#[tokio::test]
async fn v4_fetched_collation_mismatches_are_slashed() {
	let mut test_state = TestState::default();
	test_state
		.node_features
		.resize(node_features::FeatureIndex::CandidateReceiptV3 as usize + 1, false);
	test_state
		.node_features
		.set(node_features::FeatureIndex::CandidateReceiptV3 as u8 as usize, true);

	let active_leaf = get_hash(9);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	test_state.activate_leaf(&mut state, 10).await;

	let relay_parent = get_hash(8);
	let scheduling_parent = get_hash(9);
	let (ccr, _) = dummy_candidate_v3(
		relay_parent,
		scheduling_parent,
		100.into(),
		PeerId::random(),
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);
	let parent_head_data_hash = dummy_pvd().parent_head.hash();

	let advertise_and_fetch = |output_head_data_hash: Hash| {
		(
			PeerId::random(),
			ProspectiveCandidate::ByOutputHead { output_head_data_hash, parent_head_data_hash },
		)
	};

	// The advertised output head doesn't match the fetched receipt.
	{
		let (peer_id, claim) = advertise_and_fetch(Hash::repeat_byte(0xaa));
		let adv = Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent,
			prospective_candidate: Some(claim),
			advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
		};

		state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
		state.handle_declare(&mut sender, peer_id, 100.into()).await;
		test_state.handle_advertisement(&mut state, adv).await;

		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_collation_request(adv).await;

		let res = Ok(CollationFetchingResponse::Collation(ccr.to_plain(), dummy_pov()));
		state.handle_fetched_collation(&mut sender, (adv, res)).await;
		assert_eq!(db.witnessed_slash(), Some((peer_id, adv.para_id, FAILED_FETCH_SLASH)));
		test_state.assert_no_messages().await;
	}

	// An output head doesn't pin the para the way a candidate hash did: the receipt's para
	// differs from the declared one, everything else matches.
	{
		let (peer_id, claim) = advertise_and_fetch(ccr.descriptor.para_head());
		let adv = Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent,
			prospective_candidate: Some(claim),
			advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
		};

		state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
		state.handle_declare(&mut sender, peer_id, 100.into()).await;
		test_state.handle_advertisement(&mut state, adv).await;

		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_collation_request(adv).await;

		let mut receipt = ccr.to_plain();
		receipt.descriptor.set_para_id(200.into());
		let res = Ok(CollationFetchingResponse::Collation(receipt, dummy_pov()));
		state.handle_fetched_collation(&mut sender, (adv, res)).await;
		assert_eq!(db.witnessed_slash(), Some((peer_id, adv.para_id, FAILED_FETCH_SLASH)));
		test_state.assert_no_messages().await;
	}

	// The advertised descriptor version doesn't match the fetched receipt: the segment
	// declared V3 descriptors, the fetched candidate carries a V2 one.
	{
		let mut ccr_v2 = dummy_committed_candidate_receipt_v2(scheduling_parent);
		ccr_v2.descriptor.set_para_id(100.into());
		ccr_v2.descriptor.set_persisted_validation_data_hash(dummy_pvd().hash());
		ccr_v2.descriptor.set_core_index(leaf_info.assigned_core);
		ccr_v2.descriptor.set_session_index(leaf_info.session_index);

		// For V2 descriptors the scheduling parent is the relay parent.
		let (peer_id, claim) = advertise_and_fetch(ccr_v2.descriptor.para_head());
		let adv = Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent,
			prospective_candidate: Some(claim),
			advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
		};

		state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
		state.handle_declare(&mut sender, peer_id, 100.into()).await;
		test_state.handle_advertisement(&mut state, adv).await;

		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_collation_request(adv).await;

		let res = Ok(CollationFetchingResponse::Collation(ccr_v2.to_plain(), dummy_pov()));
		state.handle_fetched_collation(&mut sender, (adv, res)).await;
		assert_eq!(db.witnessed_slash(), Some((peer_id, adv.para_id, FAILED_FETCH_SLASH)));
		test_state.assert_no_messages().await;
	}
}

// Shared fixture for the segment-lifecycle tests: leaf 11 gets a claim queue with THREE
// para-100 positions so the window visible from scheduling parent `get_hash(10)` holds two —
// enough capacity that cap never masks the behavior under test.
async fn v4_two_slot_fixture(test_state: &mut TestState) -> (State<MockDb>, MockDb, Hash) {
	test_state
		.node_features
		.resize(node_features::FeatureIndex::CandidateReceiptV3 as usize + 1, false);
	test_state
		.node_features
		.set(node_features::FeatureIndex::CandidateReceiptV3 as u8 as usize, true);

	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	test_state.rp_info.insert(
		get_hash(11),
		RelayParentInfo {
			number: 11,
			parent: get_parent_hash(11),
			session_index: leaf_info.session_index,
			claim_queue: {
				let mut cq = leaf_info.claim_queue.clone();
				cq.insert(leaf_info.assigned_core, vec![100.into(), 100.into(), 100.into()]);
				cq
			},
			assigned_core: leaf_info.assigned_core,
		},
	);

	let db = MockDb::default();
	let mut state = make_state(db.clone(), test_state, active_leaf).await;
	test_state.activate_leaf(&mut state, 11).await;

	// SP = parent of the active leaf: passes the V3 slot validation like every V4 test here.
	(state, db, get_hash(10))
}

#[tokio::test]
// The whole segment is stored, not just the tip, and duplicate detection is byte-exact:
// an identical re-advertisement is rejected while the segment is stored, a byte-different
// one (here: a prefix of the same parablocks) is a fresh segment.
async fn v4_whole_segment_stored_and_byte_deduped() {
	let mut test_state = TestState::default();
	let (mut state, _db, scheduling_parent) = v4_two_slot_fixture(&mut test_state).await;
	let mut sender = test_state.sender.clone();

	let peer_a = PeerId::random();
	let peer_b = PeerId::random();
	for peer in [peer_a, peer_b] {
		state.handle_peer_connected(&mut sender, peer, CollationVersion::V4).await;
		state.handle_declare(&mut sender, peer, 100.into()).await;
	}
	assert!(state.take_replan());

	let fps: Vec<_> = (0xa1..=0xa3).map(|s| v4_fingerprint(s)).collect();
	let entries: Vec<_> = fps.iter().map(v4_entry).collect();

	test_state
		.send_v4_segment(&mut state, peer_a, scheduling_parent, fps.clone(), 100.into())
		.await;
	// No CanSecond for a V4 advertisement, and all three entries are stored.
	test_state.assert_no_messages().await;
	assert!(state.take_replan());
	assert_eq!(state.segments(), [(scheduling_parent, peer_a, entries.clone())].into());

	// Byte-identical re-advertisement while stored: Duplicate, nothing changes.
	test_state
		.send_v4_segment(&mut state, peer_a, scheduling_parent, fps.clone(), 100.into())
		.await;
	assert!(!state.take_replan());
	assert_eq!(state.segments(), [(scheduling_parent, peer_a, entries.clone())].into());

	// A prefix shares every parablock but differs in bytes: not a duplicate.
	let prefix = fps[..2].to_vec();
	test_state
		.send_v4_segment(&mut state, peer_b, scheduling_parent, prefix.clone(), 100.into())
		.await;
	assert_eq!(
		state.segments(),
		[
			(scheduling_parent, peer_a, entries),
			(scheduling_parent, peer_b, prefix.iter().map(v4_entry).collect())
		]
		.into()
	);
	assert!(state.take_replan());
	test_state.assert_no_messages().await;
}

#[tokio::test]
// A segment is ONE fetch entitlement: two free claim-queue positions and two stored entries
// still produce exactly one fetch (the tip — interim resolution), and launching consumes the
// whole segment.
async fn v4_one_fetch_per_segment() {
	let mut test_state = TestState::default();
	let (mut state, _db, scheduling_parent) = v4_two_slot_fixture(&mut test_state).await;
	let mut sender = test_state.sender.clone();

	let peer_id = PeerId::random();
	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	let fps: Vec<_> = (0xb1..=0xb2).map(|s| v4_fingerprint(s)).collect();
	test_state
		.send_v4_segment(&mut state, peer_id, scheduling_parent, fps.clone(), 100.into())
		.await;

	let tip_adv = Advertisement {
		peer_id,
		para_id: 100.into(),
		scheduling_parent,
		prospective_candidate: Some(v4_entry(fps.first().unwrap())),
		advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
	};

	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(tip_adv).await;
	test_state.assert_no_messages().await;
	// Consumed at launch: nothing stored, and the still-free second position stays empty.
	assert!(state.segments().is_empty());
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;
}

#[tokio::test]
// A byte-identical re-advertisement AFTER launch is a fresh entitlement (the launched segment
// left storage, so byte-dedup has nothing to compare against). It cannot double-launch while
// the first fetch is in flight, and it is the same-SP retry path once that fetch fails.
async fn v4_re_advertisement_after_launch_is_fresh_entitlement() {
	let mut test_state = TestState::default();
	let (mut state, _db, scheduling_parent) = v4_two_slot_fixture(&mut test_state).await;
	let mut sender = test_state.sender.clone();

	let peer_id = PeerId::random();
	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	let fps: Vec<_> = (0xc1..=0xc2).map(|s| v4_fingerprint(s)).collect();
	let entries: Vec<_> = fps.iter().map(v4_entry).collect();
	let mut tip_adv = Advertisement {
		peer_id,
		para_id: 100.into(),
		scheduling_parent,
		prospective_candidate: Some(v4_entry(fps.first().unwrap())),
		advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
	};
	let second_adv = Advertisement { prospective_candidate: Some(v4_entry(&fps[1])), ..tip_adv };

	test_state
		.send_v4_segment(&mut state, peer_id, scheduling_parent, fps.clone(), 100.into())
		.await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(tip_adv).await;
	assert!(state.segments().is_empty());

	// Re-advertise the same bytes: accepted — a fresh entitlement.
	test_state
		.send_v4_segment(&mut state, peer_id, scheduling_parent, fps.clone(), 100.into())
		.await;
	assert_eq!(state.segments(), [(scheduling_parent, peer_id, entries.clone())].into());

	// 0xc1 is in flight: the walk advances past it and launches 0xc2 - the same
	// entry is never double-launched, but the segment IS spent.
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(second_adv).await;
	assert!(state.segments().is_empty());

	// The 0xc1 fetch fails. Nothing stored remains, so nothing retries on its own
	state
		.handle_fetched_collation(
			&mut sender,
			(
				tip_adv,
				Err(CollationFetchError::Request(RequestError::NetworkError(
					RequestFailure::NotConnected,
				))),
			),
		)
		.await;
	test_state.assert_no_messages().await;
	assert!(state.segments().is_empty());

	// Activate new leaf so we can submit more segments
	let leaf_info = test_state.rp_info.get(&get_hash(11)).unwrap().clone();
	test_state.rp_info.insert(
		get_hash(12),
		RelayParentInfo {
			number: 12,
			parent: get_parent_hash(12),
			session_index: leaf_info.session_index,
			claim_queue: {
				let mut cq = leaf_info.claim_queue.clone();
				cq.insert(leaf_info.assigned_core, vec![100.into(), 100.into(), 100.into()]);
				cq
			},
			assigned_core: leaf_info.assigned_core,
		},
	);
	test_state.activate_leaf(&mut state, 12).await;

	// Recovery: a re-advertisement AFTER the failure. 0xc1 is no longer in flight
	// nor fetched, so the walk resolves it again — the retry.
	test_state
		.send_v4_segment(&mut state, peer_id, get_hash(11), fps.clone(), 100.into())
		.await;
	assert_eq!(state.segments(), [(get_hash(11), peer_id, entries)].into());

	tip_adv.scheduling_parent = get_hash(11);
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(tip_adv).await;
	assert!(state.segments().is_empty());
	test_state.assert_no_messages().await;
}

#[tokio::test]
// The advertisement cap counts SEGMENTS, not entries: at a scheduling parent whose window
// holds a single para-100 position (cap 1), one three-entry segment is accepted whole, and
// the peer's next segment is rejected by the cap.
async fn v4_cap_counts_segments_not_entries() {
	let mut test_state = TestState::default();
	test_state
		.node_features
		.resize(node_features::FeatureIndex::CandidateReceiptV3 as usize + 1, false);
	test_state
		.node_features
		.set(node_features::FeatureIndex::CandidateReceiptV3 as u8 as usize, true);

	let active_leaf = get_hash(9);
	let mut state = make_state(MockDb::default(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();
	test_state.activate_leaf(&mut state, 10).await;
	// Leaf 10's window from SP 9 shows one para-100 position: cap = 1 segment.
	let scheduling_parent = get_hash(9);

	let peer_id = PeerId::random();
	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	let fps: Vec<_> = (0xd1..=0xd3).map(|s| v4_fingerprint(s)).collect();
	test_state
		.send_v4_segment(&mut state, peer_id, scheduling_parent, fps.clone(), 100.into())
		.await;
	let expected: BTreeSet<_> =
		[(scheduling_parent, peer_id, fps.iter().map(v4_entry).collect::<Vec<_>>())].into();
	// Three entries stored against a cap of one SEGMENT.
	assert_eq!(state.segments(), expected);

	// The second segment (byte-different) trips the cap.
	let more: Vec<_> = (0xe1..=0xe2).map(|s| v4_fingerprint(s)).collect();
	test_state
		.send_v4_segment(&mut state, peer_id, scheduling_parent, more, 100.into())
		.await;
	assert_eq!(state.segments(), expected);
	test_state.assert_no_messages().await;
}

// Test that a V3 candidate descriptor is correctly accepted and
// seconded when the CandidateReceiptV3 node feature is enabled.
#[tokio::test]
async fn v3_descriptor_accepted_when_v3_enabled() {
	let mut test_state = TestState::default();
	test_state
		.node_features
		.resize(node_features::FeatureIndex::CandidateReceiptV3 as usize + 1, false);
	test_state
		.node_features
		.set(node_features::FeatureIndex::CandidateReceiptV3 as u8 as usize, true);

	let active_leaf = get_hash(9);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	let mut state = make_state(MockDb::default(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();
	let peer_id = PeerId::random();

	test_state.rp_info.insert(
		get_hash(10),
		RelayParentInfo {
			number: 10,
			parent: get_parent_hash(10),
			session_index: leaf_info.session_index,
			claim_queue: leaf_info.claim_queue.clone(),
			assigned_core: leaf_info.assigned_core,
		},
	);

	test_state.activate_leaf(&mut state, 10).await;

	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V3).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	// V3: relay_parent (execution context) is older than scheduling_parent.
	let relay_parent = get_hash(8);
	let scheduling_parent = get_hash(9);
	let (ccr, adv) = dummy_candidate_v3(
		relay_parent,
		scheduling_parent,
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);

	// Advertise the v3 candidate
	test_state.handle_advertisement(&mut state, adv).await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(adv).await;
	test_state
		.handle_fetched_collation(&mut state, adv, ccr.to_plain(), None, relay_parent)
		.await;
	test_state
		.second_collation(&mut state, peer_id, CollationVersion::V3, ccr, active_leaf)
		.await;
}

// V3 advertisement is accepted when the scheduling parent is a leaf
// whose slot has already finished (current_slot == leaf_slot + 1).
#[tokio::test]
async fn v3_advertisement_accepted_when_sp_is_finished_slot_leaf() {
	let mut test_state = TestState::default();
	test_state
		.node_features
		.resize(node_features::FeatureIndex::CandidateReceiptV3 as usize + 1, false);
	test_state
		.node_features
		.set(node_features::FeatureIndex::CandidateReceiptV3 as u8 as usize, true);

	let slot_duration = sp_consensus_slots::SlotDuration::from_millis(
		polkadot_primitives::RELAY_CHAIN_SLOT_DURATION_MILLIS,
	);
	let current_slot = sp_consensus_slots::Slot::from_timestamp(
		sp_timestamp::Timestamp::new(test_state.clock.duration_since_epoch().as_millis() as u64),
		slot_duration,
	);
	test_state
		.slot_overrides
		.insert(get_hash(10), sp_consensus_slots::Slot::from(*current_slot - 1));

	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	let mut state = make_state(MockDb::default(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();
	let peer_id = PeerId::random();

	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V3).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	let relay_parent = get_hash(9);
	let scheduling_parent = get_hash(10);
	let (ccr, adv) = dummy_candidate_v3(
		relay_parent,
		scheduling_parent,
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);

	// Advertise the v3 candidate
	test_state.handle_advertisement(&mut state, adv).await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(adv).await;
	test_state
		.handle_fetched_collation(&mut state, adv, ccr.to_plain(), None, relay_parent)
		.await;
	test_state
		.second_collation(&mut state, peer_id, CollationVersion::V3, ccr, active_leaf)
		.await;
}

// V3 advertisements require the scheduling parent to be a RC block from
// the last finished slot. Check that the leaf of an active slot and its
// grand parent are rejected
#[tokio::test]
async fn v3_advertisement_rejected_when_sp_not_last_finished_slot() {
	let mut test_state = TestState::default();
	test_state
		.node_features
		.resize(node_features::FeatureIndex::CandidateReceiptV3 as usize + 1, false);
	test_state
		.node_features
		.set(node_features::FeatureIndex::CandidateReceiptV3 as u8 as usize, true);

	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	let mut state = make_state(MockDb::default(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();
	let peer_id = PeerId::random();

	test_state.rp_info.insert(
		get_hash(11),
		RelayParentInfo {
			number: 11,
			parent: get_parent_hash(11),
			session_index: leaf_info.session_index,
			claim_queue: leaf_info.claim_queue.clone(),
			assigned_core: leaf_info.assigned_core,
		},
	);

	test_state.activate_leaf(&mut state, 11).await;

	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V3).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	// Test that the grand parent of a block from the current, unfinished slot is rejected
	let (_ccr, adv) = dummy_candidate_v3(
		get_hash(9),
		get_hash(9),
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);

	state
		.handle_advertisement(
			&mut sender,
			peer_id,
			adv.scheduling_parent,
			adv.prospective_candidate.into_iter().collect(),
			Some(CandidateDescriptorVersion::V3),
			None,
		)
		.await;
	assert!(state.advertisements().is_empty());
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;

	// Test that the current leaf is rejected as the slot is not yet finished.
	let (_ccr, adv) = dummy_candidate_v3(
		get_hash(8),
		get_hash(11),
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);
	state
		.handle_advertisement(
			&mut sender,
			peer_id,
			adv.scheduling_parent,
			adv.prospective_candidate.into_iter().collect(),
			Some(CandidateDescriptorVersion::V3),
			None,
		)
		.await;
	assert!(state.advertisements().is_empty());
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;

	// Test that the parent of the current leaf (last finished slot) is accepted.
	let relay_parent = get_hash(9);
	let scheduling_parent = get_hash(10);
	let (ccr, adv) = dummy_candidate_v3(
		relay_parent,
		scheduling_parent,
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);

	test_state.handle_advertisement(&mut state, adv).await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(adv).await;
	test_state
		.handle_fetched_collation(&mut state, adv, ccr.to_plain(), None, relay_parent)
		.await;
	test_state
		.second_collation(&mut state, peer_id, CollationVersion::V3, ccr, active_leaf)
		.await;
}

// V3 descriptor via V2 protocol → rejected (wrong protocol).
#[tokio::test]
async fn v3_descriptor_rejected_via_v2_protocol() {
	let mut test_state = TestState::default();

	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();
	let peer_id = PeerId::random();

	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	let (ccr, _) = dummy_candidate_v3(
		active_leaf,
		active_leaf,
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);

	let receipt = ccr.to_plain();

	let adv = Advertisement {
		peer_id,
		para_id: 100.into(),
		scheduling_parent: active_leaf,
		prospective_candidate: Some(ProspectiveCandidate::ByHash {
			candidate_hash: receipt.hash(),
			parent_head_data_hash: dummy_pvd().parent_head.hash(),
		}),
		advertised_descriptor_version: None,
	};

	test_state.handle_advertisement(&mut state, adv).await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(adv).await;

	let res = Ok(CollationFetchingResponse::Collation(receipt, dummy_pov()));
	state.handle_fetched_collation(&mut sender, (adv, res)).await;
	assert_eq!(db.witnessed_slash(), Some((peer_id, adv.para_id, FAILED_FETCH_SLASH)));
	test_state.assert_no_messages().await;
}

// Test that a collator advertising a V3 descriptor but fetching a V2 candidate
// is rejected due to descriptor version mismatch and the peer is slashed.
#[tokio::test]
async fn v3_advertised_but_v2_fetched_descriptor_version_mismatch() {
	let mut test_state = TestState::default();
	test_state
		.node_features
		.resize(node_features::FeatureIndex::CandidateReceiptV3 as usize + 1, false);
	test_state
		.node_features
		.set(node_features::FeatureIndex::CandidateReceiptV3 as u8 as usize, true);

	let active_leaf = get_hash(9);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();
	let peer_id = PeerId::random();

	test_state.rp_info.insert(
		get_hash(10),
		RelayParentInfo {
			number: 10,
			parent: get_parent_hash(10),
			session_index: leaf_info.session_index,
			claim_queue: leaf_info.claim_queue.clone(),
			assigned_core: leaf_info.assigned_core,
		},
	);

	test_state.activate_leaf(&mut state, 10).await;

	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V3).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	// Create a V2-style candidate (version=0, no scheduling_parent set).
	let (ccr, _) = dummy_candidate(
		active_leaf,
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);

	let receipt = ccr.to_plain();

	// Advertise as V3 but the actual fetched candidate is V2.
	let adv = Advertisement {
		peer_id,
		para_id: 100.into(),
		scheduling_parent: get_hash(9),
		prospective_candidate: Some(ProspectiveCandidate::ByHash {
			candidate_hash: receipt.hash(),
			parent_head_data_hash: dummy_pvd().parent_head.hash(),
		}),
		advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
	};

	test_state.handle_advertisement(&mut state, adv).await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(adv).await;
	let res = Ok(CollationFetchingResponse::Collation(receipt, dummy_pov()));
	state.handle_fetched_collation(&mut sender, (adv, res)).await;
	assert_eq!(db.witnessed_slash(), Some((peer_id, adv.para_id, FAILED_FETCH_SLASH)));
	test_state.assert_no_messages().await;
}

// Test that a crafted descriptor is rejected as Unknown version when
// CandidateReceiptV3 feature is disabled.
#[tokio::test]
async fn v3_descriptor_unknown_rejected_when_v3_disabled() {
	let mut test_state = TestState::default();

	let active_leaf = get_hash(9);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();
	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();
	let peer_id = PeerId::random();
	test_state.rp_info.insert(
		get_hash(10),
		RelayParentInfo {
			number: 10,
			parent: active_leaf,
			session_index: leaf_info.session_index,
			claim_queue: leaf_info.claim_queue.clone(),
			assigned_core: leaf_info.assigned_core,
		},
	);

	test_state.activate_leaf(&mut state, 10).await;

	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V2).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	let mut ccr = dummy_committed_candidate_receipt_v2(active_leaf);
	ccr.descriptor.set_para_id(100.into());
	ccr.descriptor.set_persisted_validation_data_hash(dummy_pvd().hash());
	ccr.descriptor.set_core_index(leaf_info.assigned_core);
	ccr.descriptor.set_session_index(leaf_info.session_index);
	ccr.descriptor.set_version(1);

	let receipt = ccr.to_plain();
	let adv = Advertisement {
		peer_id,
		para_id: 100.into(),
		scheduling_parent: active_leaf,
		prospective_candidate: Some(ProspectiveCandidate::ByHash {
			candidate_hash: receipt.hash(),
			parent_head_data_hash: dummy_pvd().parent_head.hash(),
		}),
		advertised_descriptor_version: None,
	};
	test_state.handle_advertisement(&mut state, adv).await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(adv).await;
	let res = Ok(CollationFetchingResponse::Collation(receipt, dummy_pov()));
	state.handle_fetched_collation(&mut sender, (adv, res)).await;
	assert_eq!(db.witnessed_slash(), Some((peer_id, adv.para_id, FAILED_FETCH_SLASH)));
	test_state.assert_no_messages().await;
}

// Regression test for checking that the core is computed correctly
// for ancestors.
#[tokio::test]
async fn core_assignment_uses_ancestor_not_leaf() {
	let mut test_state = TestState::default();
	// Rotate groups every block so block 9 and block 10 have different
	// core assignments.
	test_state
		.session_info
		.get_mut(&1)
		.unwrap()
		.group_rotation_info
		.group_rotation_frequency = 1;
	test_state
		.node_features
		.resize(node_features::FeatureIndex::CandidateReceiptV3 as usize + 1, false);
	test_state
		.node_features
		.set(node_features::FeatureIndex::CandidateReceiptV3 as u8 as usize, true);

	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, get_hash(10)).await;
	let mut sender = test_state.sender.clone();

	let peer_id = PeerId::random();
	// Core 2 is the correct one for the SP 10. Given the group rotation
	// math the core index for block 10 is 1. If we were to still compute the CoreIndex
	// for the ancestors based on leaf then this candidate would be rejected and
	// the collator slashed.
	let (ccr, adv) = dummy_candidate_v3(
		get_hash(8),
		get_hash(9),
		600.into(),
		peer_id,
		CoreIndex(2),
		1,
		dummy_pvd().hash(),
	);

	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V3).await;
	state.handle_declare(&mut sender, peer_id, 600.into()).await;

	test_state
		.handle_fetched_collation(&mut state, adv, ccr.to_plain(), None, get_hash(8))
		.await;
	assert!(db.witnessed_slash().is_none());
}

// Regression test for https://github.com/paritytech/polkadot-sdk/issues/11556
// Ensure that after a core rotation, candidates for both the old core's para (at the pre-rotation
// leaf) and the new core's para (at the post-rotation leaf) are accepted.
#[tokio::test]
async fn core_rotation_accepts_candidates_for_both_cores() {
	let mut test_state = TestState::default();

	// With group_rotation_frequency=1, session_start=0, group=0, cores=3 the
	// core_for_group formula: idx = (group + cores - rotations % cores) % cores gives:
	//   block 8: now=9,  rotations=9,  9%3=0 → core = (0+3-0)%3 = 0
	//   block 9: now=10, rotations=10, 10%3=1 → core = (0+3-1)%3 = 2
	//   block 10: now=11, rotations=11, 11%3=2 → core = (0+3-2)%3 = 1
	// So leaf 9 is on core 2 and leaf 10 is on core 1.
	let core_at_9 = CoreIndex(2);
	let core_at_10 = CoreIndex(1);

	let para_a: ParaId = 100.into();
	let para_b: ParaId = 600.into();

	test_state
		.session_info
		.get_mut(&1)
		.unwrap()
		.group_rotation_info
		.group_rotation_frequency = 1;

	// Set up claim queues (length 3, matching scheduling_lookahead) so that:
	//   - At leaf 9 (core 2): CQ[2] = [para_a, para_a, para_a]
	//   - At leaf 10 (core 1): CQ[1] = [para_b, para_b, para_b]
	test_state.rp_info.clear();

	test_state.rp_info.insert(
		get_hash(8),
		RelayParentInfo {
			number: 8,
			parent: get_parent_hash(8),
			session_index: 1,
			claim_queue: BTreeMap::from([
				(CoreIndex(0), vec![para_a, para_a, para_a]),
				(CoreIndex(1), vec![para_b, para_b, para_b]),
				(CoreIndex(2), vec![para_a, para_a, para_a]),
			]),
			assigned_core: CoreIndex(0),
		},
	);

	test_state.rp_info.insert(
		get_hash(9),
		RelayParentInfo {
			number: 9,
			parent: get_parent_hash(9),
			session_index: 1,
			claim_queue: BTreeMap::from([
				(CoreIndex(0), vec![para_a, para_a, para_a]),
				(CoreIndex(1), vec![para_b, para_b, para_b]),
				(CoreIndex(2), vec![para_a, para_a, para_a]),
			]),
			assigned_core: core_at_9,
		},
	);

	test_state.rp_info.insert(
		get_hash(10),
		RelayParentInfo {
			number: 10,
			parent: get_parent_hash(10),
			session_index: 1,
			claim_queue: BTreeMap::from([
				(CoreIndex(0), vec![para_a, para_a, para_a]),
				(CoreIndex(1), vec![para_b, para_b, para_b]),
				(CoreIndex(2), vec![para_a, para_a, para_a]),
			]),
			assigned_core: core_at_10,
		},
	);

	// Start at leaf 9 (core 2, CQ = [para_a])
	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, get_hash(9)).await;
	let mut sender = test_state.sender.clone();

	// Connect peer_a and declare for para_a (available on core 2 at leaf 9)
	let peer_a = peer_id(1);
	state.handle_peer_connected(&mut sender, peer_a, CollationVersion::V2).await;
	state.handle_declare(&mut sender, peer_a, para_a).await;
	test_state.assert_no_messages().await;

	// Advertise a candidate for para_a at scheduling_parent=9 (core 2)
	let (ccr_a, adv_a) =
		dummy_candidate(get_hash(9), para_a, peer_a, core_at_9, 1, dummy_pvd().hash());

	test_state.handle_advertisement(&mut state, adv_a).await;
	assert_eq!(state.advertisements(), [adv_a].into());

	// Activate leaf 10 (rotation to core 1, CQ = [para_b])
	test_state.activate_leaf(&mut state, 10).await;

	// Now para_b (600) becomes part of the assignments. Connect peer_b and declare for para_b.
	let peer_b = peer_id(2);
	state.handle_peer_connected(&mut sender, peer_b, CollationVersion::V2).await;
	state.handle_declare(&mut sender, peer_b, para_b).await;
	test_state.assert_no_messages().await;

	// Advertise a candidate for para_b at scheduling_parent=10 (core 1)
	let (ccr_b, adv_b) =
		dummy_candidate(get_hash(10), para_b, peer_b, core_at_10, 1, dummy_pvd().hash());

	test_state.handle_advertisement(&mut state, adv_b).await;
	assert_eq!(state.advertisements(), [adv_a, adv_b].into());

	// Launch fetch requests - both should be fetched
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_requests([adv_a, adv_b].into()).await;

	// Fetch and second both collations
	test_state
		.handle_fetched_collation(
			&mut state,
			adv_a,
			ccr_a.to_plain(),
			None,
			adv_a.scheduling_parent,
		)
		.await;
	test_state
		.second_collation(&mut state, peer_a, CollationVersion::V2, ccr_a, get_hash(9))
		.await;

	test_state
		.handle_fetched_collation(
			&mut state,
			adv_b,
			ccr_b.to_plain(),
			None,
			adv_b.scheduling_parent,
		)
		.await;
	test_state
		.second_collation(&mut state, peer_b, CollationVersion::V2, ccr_b, get_hash(10))
		.await;

	test_state.assert_no_messages().await;

	// After rotation (leaf 10 active), advertise another candidate for para_a at the old
	// scheduling_parent=9 (core 2) from the same peer. This should still be accepted because
	// the old core's claim queue slots were not overwritten by the rotation.
	let pvd_a2 = PersistedValidationData { parent_head: HeadData(vec![1, 2, 3]), ..dummy_pvd() };
	let ccr_a2 = {
		let mut ccr = dummy_committed_candidate_receipt_v2(get_hash(9));
		ccr.descriptor.set_para_id(para_a);
		ccr.descriptor.set_persisted_validation_data_hash(pvd_a2.hash());
		ccr.descriptor.set_core_index(core_at_9);
		ccr.descriptor.set_session_index(1);
		ccr
	};
	let receipt_a2 = ccr_a2.to_plain();
	let adv_a2 = Advertisement {
		peer_id: peer_a,
		para_id: para_a,
		scheduling_parent: get_hash(9),
		prospective_candidate: Some(ProspectiveCandidate::ByHash {
			candidate_hash: receipt_a2.hash(),
			parent_head_data_hash: pvd_a2.parent_head.hash(),
		}),
		advertised_descriptor_version: None,
	};

	test_state.handle_advertisement(&mut state, adv_a2).await;
	assert_eq!(state.advertisements(), [adv_a2].into());

	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(adv_a2).await;

	test_state
		.handle_fetched_collation(
			&mut state,
			adv_a2,
			receipt_a2,
			Some(pvd_a2),
			adv_a2.scheduling_parent,
		)
		.await;
	test_state
		.second_collation(&mut state, peer_a, CollationVersion::V2, ccr_a2, get_hash(9))
		.await;

	test_state.assert_no_messages().await;
}

// =============================================================================================
// Active-fork tests.
//
// Default topology — two sibling leaves at block 10, both children of a common ancestor at
// block 9. Tests that diverge from this topology say so in their own diagram.
//
//                 fork_a  (block 10)
//                /
//   block 9 (common ancestor)
//                \
//                 fork_b  (block 10)
//
// Each test varies the leaves' CQs to exercise a specific property of the fork-aware logic.
// =============================================================================================

const FORK_HEIGHT: u32 = 10;

fn fork_a_hash() -> Hash {
	Hash::from_low_u64_be(0xa10)
}
fn fork_b_hash() -> Hash {
	Hash::from_low_u64_be(0xb10)
}

/// Set up two sibling leaves at block 10 with the given CQs for our core, both children of
/// block 9 (the default leaf 9 in `TestState`). The shared core is the test_state's default
/// `assigned_core`.
fn setup_fork(
	test_state: &mut TestState,
	leaf_a_cq: Vec<ParaId>,
	leaf_b_cq: Vec<ParaId>,
) -> (Hash, Hash) {
	let common = get_hash(9);
	let common_info = test_state.rp_info.get(&common).unwrap().clone();
	let core = common_info.assigned_core;
	let session = common_info.session_index;

	for (hash, cq) in [(fork_a_hash(), leaf_a_cq), (fork_b_hash(), leaf_b_cq)] {
		let mut cqs = common_info.claim_queue.clone();
		cqs.insert(core, cq);
		test_state.rp_info.insert(
			hash,
			RelayParentInfo {
				number: FORK_HEIGHT,
				parent: common,
				session_index: session,
				claim_queue: cqs,
				assigned_core: core,
			},
		);
	}

	(fork_a_hash(), fork_b_hash())
}

// Fork test (1): peer connections — assignments are the union across leaves.
//
// Leaf A schedules para 100; leaf B schedules para 200. While both are active, peers may
// declare for either. Dropping leaf B disconnects the 200-peers (no leaf schedules 200
// anymore); dropping the rest disconnects the 100-peers.
#[tokio::test]
async fn fork_assignments_are_union_of_leaves() {
	let mut test_state = TestState::default();
	let (fork_a, fork_b) = setup_fork(&mut test_state, vec![100.into(); 3], vec![200.into(); 3]);

	let mut state = make_state(MockDb::default(), &mut test_state, get_hash(9)).await;
	let mut sender = test_state.sender.clone();

	// Activate both forks. Assignments now = {100, 200}.
	test_state.activate_leaves(&mut state, vec![fork_a, fork_b]).await;

	let peer_100 = peer_id(0);
	let peer_200 = peer_id(1);
	state.handle_peer_connected(&mut sender, peer_100, CollationVersion::V2).await;
	state.handle_peer_connected(&mut sender, peer_200, CollationVersion::V2).await;
	state.handle_declare(&mut sender, peer_100, 100.into()).await;
	state.handle_declare(&mut sender, peer_200, 200.into()).await;
	test_state.assert_no_messages().await;
	assert_eq!(state.connected_peers(), [peer_100, peer_200].into_iter().collect());

	// Drop leaf B → 200-peers disconnect.
	test_state.activate_leaves(&mut state, vec![fork_a]).await;
	test_state.assert_peers_disconnected(vec![peer_200]).await;
	assert_eq!(state.connected_peers(), [peer_100].into_iter().collect());
}

// Fork test (2): different-length forks — the longest visible window from a common SP
// determines capacity.
//
// Two sibling forks branch at block 9. fork_a is a one-block leaf at block 10. fork_b
// extends two blocks: fork_b_10 (block 10) → fork_b_11 (block 11, leaf). From sp=9, path
// via fork_a has offset 1 (window 2); path via fork_b_11 has offset 2 (window 1). Both
// paths are real (each ends at an actual leaf with no descendants). Capacity at sp=9 must
// reflect the longer window.
//
//                    ┌──► fork_a (block 10, leaf)
//   block 9 (sp) ────┤
//                    └──► fork_b_10 (block 10) ──► fork_b_11 (block 11, leaf)
//
//                offset 1: 2-slot window         offset 2: 1-slot window
//                ── this one wins for capacity ──
#[tokio::test]
async fn fork_capacity_uses_longest_window_across_paths() {
	let mut test_state = TestState::default();
	let core = test_state.rp_info[&get_hash(9)].assigned_core;
	let session = test_state.rp_info[&get_hash(9)].session_index;
	let common_cq = test_state.rp_info[&get_hash(9)].claim_queue.clone();

	// Build a CQ with three 100s for our core, used by both forks.
	let make_cq = |entry: ParaId| {
		let mut cq = common_cq.clone();
		cq.insert(core, vec![entry, entry, entry]);
		cq
	};

	let fork_a = Hash::from_low_u64_be(0xa10);
	let fork_b_10 = Hash::from_low_u64_be(0xb10);
	let fork_b_11 = Hash::from_low_u64_be(0xb11);

	test_state.rp_info.insert(
		fork_a,
		RelayParentInfo {
			number: 10,
			parent: get_hash(9),
			session_index: session,
			claim_queue: make_cq(100.into()),
			assigned_core: core,
		},
	);
	test_state.rp_info.insert(
		fork_b_10,
		RelayParentInfo {
			number: 10,
			parent: get_hash(9),
			session_index: session,
			claim_queue: make_cq(100.into()),
			assigned_core: core,
		},
	);
	test_state.rp_info.insert(
		fork_b_11,
		RelayParentInfo {
			number: 11,
			parent: fork_b_10,
			session_index: session,
			claim_queue: make_cq(100.into()),
			assigned_core: core,
		},
	);

	let mut state = make_state(MockDb::default(), &mut test_state, get_hash(9)).await;
	let mut sender = test_state.sender.clone();
	// Activate fork_a (length 1 from sp=9) and fork_b_11 (length 2 from sp=9).
	test_state.activate_leaves(&mut state, vec![fork_a, fork_b_11]).await;

	// 2 peers, advertise 2 distinct candidates of para 100 at sp=9. Both should fit
	// (window 2 via fork_a) — if we used the shorter window (1 via fork_b_11) we'd cap
	// at 1.
	let peers: Vec<_> = (0..2).map(peer_id).collect();
	for &p in &peers {
		state.handle_peer_connected(&mut sender, p, CollationVersion::V2).await;
		state.handle_declare(&mut sender, p, 100.into()).await;
	}
	for (i, &p) in peers.iter().enumerate() {
		let (_, adv) = dummy_candidate(
			get_hash(9),
			100.into(),
			p,
			core,
			1,
			Hash::from_low_u64_be(i as u64 + 200),
		);
		test_state.handle_advertisement(&mut state, adv).await;
	}
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	let msg = test_state.timeout_recv().await;
	assert_matches::assert_matches!(
		msg,
		AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendRequests(reqs, _)) => {
			assert_eq!(reqs.len(), 2);
		}
	);
}

// Fork test (3): an in-flight candidate at a shared SP is counted once, not per leaf.
//
// Both leaves share sp=9 in their path. With CQ for our core having two slots for para 100
// at sp=9's window, we should be able to fetch *two* candidates for para 100 at sp=9 — not
// four (which would be the case if each leaf got its own bucket).
#[tokio::test]
async fn fork_shared_sp_capacity_not_double_counted() {
	let mut test_state = TestState::default();
	// Both leaves use the same CQ — sp=9 sees 2 slots for para 100 (window len = lookahead - 1).
	let (fork_a, fork_b) = setup_fork(
		&mut test_state,
		vec![100.into(), 100.into(), 100.into()],
		vec![100.into(), 100.into(), 100.into()],
	);

	let mut state = make_state(MockDb::default(), &mut test_state, get_hash(9)).await;
	let mut sender = test_state.sender.clone();
	test_state.activate_leaves(&mut state, vec![fork_a, fork_b]).await;

	let core = test_state.rp_info[&get_hash(9)].assigned_core;

	// Connect 4 peers, declare for 100, advertise 4 distinct candidates at sp=9.
	let peers: Vec<_> = (0..4).map(peer_id).collect();
	for &p in &peers {
		state.handle_peer_connected(&mut sender, p, CollationVersion::V2).await;
		state.handle_declare(&mut sender, p, 100.into()).await;
	}
	let mut advs = Vec::new();
	for (i, &p) in peers.iter().enumerate() {
		let (_, adv) = dummy_candidate(
			get_hash(9),
			100.into(),
			p,
			core,
			1,
			Hash::from_low_u64_be(i as u64 + 100),
		);
		test_state.handle_advertisement(&mut state, adv).await;
		advs.push(adv);
	}

	// Capacity at sp=9: window len 2, all 4 ads compete for 2 slots → only 2 fetches.
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	let mut launched: BTreeSet<Advertisement> = BTreeSet::new();
	let msg = test_state.timeout_recv().await;
	assert_matches::assert_matches!(
		msg,
		AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendRequests(reqs, _)) => {
			assert_eq!(reqs.len(), 2);
			for r in reqs {
				if let Requests::CollationFetchingV2(req) = r {
					launched.insert(*advs.iter().find(|a| {
						a.candidate_hash() == Some(req.payload.candidate_hash)
					}).unwrap());
				}
			}
		}
	);
	test_state.assert_no_messages().await;
	assert_eq!(launched.len(), 2);
}

// Fork test (4): dropping a leaf reclaims its unique-fork capacity / assignments.
//
// Leaf A schedules 100 at every slot; leaf B schedules 200 at every slot. We exhaust leaf B's
// capacity by advertising 200s at fork_b. Then we drop leaf B — the 200 advertisements'
// fetches must be cancelled, and 200 must leave the assignment set so peers for 200
// disconnect.
#[tokio::test]
async fn fork_drop_reclaims_capacity_and_disconnects_peers() {
	let mut test_state = TestState::default();
	let (fork_a, fork_b) = setup_fork(&mut test_state, vec![100.into(); 3], vec![200.into(); 3]);

	let mut state = make_state(MockDb::default(), &mut test_state, get_hash(9)).await;
	let mut sender = test_state.sender.clone();
	test_state.activate_leaves(&mut state, vec![fork_a, fork_b]).await;

	let peer_200 = peer_id(0);
	state.handle_peer_connected(&mut sender, peer_200, CollationVersion::V2).await;
	state.handle_declare(&mut sender, peer_200, 200.into()).await;

	let core = test_state.rp_info[&get_hash(9)].assigned_core;
	let (_, adv) = dummy_candidate(fork_b, 200.into(), peer_200, core, 1, dummy_pvd().hash());
	test_state.handle_advertisement(&mut state, adv).await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	// Keep the response sender alive so cancellation (not "sender dropped") is what completes
	// the fetch future.
	let _response_sender = test_state.assert_collation_request(adv).await;

	// Drop leaf B. The fetch at fork_b must be cancelled and the 200-peer disconnected
	// (no remaining leaf schedules 200).
	test_state.activate_leaves(&mut state, vec![fork_a]).await;
	let resp = state
		.collation_response_stream()
		.select_next_some()
		.timeout(TIMEOUT)
		.await
		.expect("cancellation should arrive");
	assert_matches::assert_matches!(resp.1, Err(CollationFetchError::Cancelled));
	state.handle_fetched_collation(&mut sender, resp).await;

	test_state.assert_peers_disconnected(vec![peer_200]).await;
	assert!(state.connected_peers().is_empty());
}

// Note: rotation + forks isn't a separate test here.
// `core_rotation_accepts_candidates_for_both_cores` already exercises group rotation, and forks
// (1)-(4) above exercise fork logic with our group's core; the combination doesn't add a new
// property worth the test setup.

// Multi-SP-on-linear-path same-para capacity: candidates at different SPs on the same path
// compete for the same per-core CQ pool. With leaf 10 active and ancestors 8, 9 in the
// implicit view, leaf 10's CQ for our core is [100, 200, 100] — para 100 owns two positions
// total across all SPs on this path. Advertising para 100 at three different SPs (one at each
// of 8, 9, 10) must result in at most two fetches, regardless of which SP the implementation
// walks first.
//
// Pre-fix bug: `consumed_for_para` filters by SP, so each SP independently sees zero
// consumption and fetches for its own quota — producing three fetches total (overrun).
#[tokio::test]
async fn linear_multi_sp_same_para_capacity_not_double_counted() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let core = test_state.rp_info[&active_leaf].assigned_core;
	// Sanity: leaf-10 CQ for our core has para 100 twice.
	assert_eq!(
		test_state.rp_info[&active_leaf].claim_queue[&core],
		vec![100.into(), 200.into(), 100.into()]
	);

	let mut state = make_state(MockDb::default(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	let peers: Vec<_> = (0..3).map(peer_id).collect();
	for &p in &peers {
		state.handle_peer_connected(&mut sender, p, CollationVersion::V2).await;
		state.handle_declare(&mut sender, p, 100.into()).await;
	}

	// One advertisement per SP for para 100. Each advertisement lives at a different SP on
	// the same linear path: 8 (grandparent), 9 (parent), 10 (leaf).
	let sps = [get_hash(8), get_hash(9), get_hash(10)];
	let mut advs = Vec::new();
	for (i, &sp) in sps.iter().enumerate() {
		let (_, adv) = dummy_candidate(
			sp,
			100.into(),
			peers[i],
			core,
			1,
			Hash::from_low_u64_be(i as u64 + 100),
		);
		test_state.handle_advertisement(&mut state, adv).await;
		advs.push(adv);
	}

	// Exactly two fetches: leaf 10's CQ has two slots for para 100, and the matching is
	// optimal regardless of which SP the implementation walks first. >2 = over-fetch
	// (third candidate has nowhere to land on-chain). <2 = under-fetch (a wide-window
	// candidate stole a slot reachable only from a narrower-window SP).
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	let msg = test_state.timeout_recv().await;
	assert_matches::assert_matches!(
		msg,
		AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendRequests(reqs, _)) => {
			assert_eq!(
				reqs.len(), 2,
				"expected exactly 2 fetches, got {}",
				reqs.len()
			);
		}
	);
	test_state.assert_no_messages().await;
}

// Under-fetch regression: a wide-window SP must not "steal" a CQ position reachable only
// from a narrower-window SP. With leaf 10's CQ for our core = [100, 200, 100], SP=8 (window
// [100], offset 2) can only fill position 0; SP=10 (window [100, 200, 100], offset 0) can
// fill positions 0 or 2. If consumption from a fetch at SP=10 is greedily charged to
// position 0, SP=8's only reachable slot is gone and its ad gets wrongly parked.
//
// Both ads must be fetched regardless of which SP `try_launch_new_fetch_requests` visits
// first.
#[tokio::test]
async fn linear_multi_sp_no_under_fetch_when_wide_and_narrow_compete() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let core = test_state.rp_info[&active_leaf].assigned_core;
	assert_eq!(
		test_state.rp_info[&active_leaf].claim_queue[&core],
		vec![100.into(), 200.into(), 100.into()]
	);

	let mut state = make_state(MockDb::default(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	let peer_narrow = peer_id(0); // ad at SP=8 (window len 1)
	let peer_wide = peer_id(1); // ad at SP=10 (window len 3)
	for &p in &[peer_narrow, peer_wide] {
		state.handle_peer_connected(&mut sender, p, CollationVersion::V2).await;
		state.handle_declare(&mut sender, p, 100.into()).await;
	}

	let (_, adv_narrow) =
		dummy_candidate(get_hash(8), 100.into(), peer_narrow, core, 1, Hash::from_low_u64_be(100));
	let (_, adv_wide) =
		dummy_candidate(get_hash(10), 100.into(), peer_wide, core, 1, Hash::from_low_u64_be(101));
	test_state.handle_advertisement(&mut state, adv_narrow).await;
	test_state.handle_advertisement(&mut state, adv_wide).await;

	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	let msg = test_state.timeout_recv().await;
	assert_matches::assert_matches!(
		msg,
		AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendRequests(reqs, _)) => {
			assert_eq!(
				reqs.len(), 2,
				"expected both narrow- and wide-window ads to fetch, got {}",
				reqs.len()
			);
		}
	);
	test_state.assert_no_messages().await;
}

// Short claim queues (e.g. on-demand parachains where not every CQ position is filled) must
// not cause valid advertisements at ancestor SPs to be rejected.
//
// The runtime's CQ length can be less than the scheduling lookahead. An ancestor SP at depth
// `d` from the leaf can still validly host an advertisement whose slot lands at any leaf-CQ
// position `i` where `i + d < lookahead`. The bound is the lookahead, NOT the CQ length —
// using `cq.len() - d` underestimates the valid range when `cq.len() < lookahead` and rejects
// otherwise-valid advertisements.
//
// Setup: lookahead=3 (default), override leaf-10's CQ to `[100]` (single slot, simulating a
// parachain with only the next slot scheduled). Advertise para 100 at sp=8 (depth 2). Slot
// leaf+1 lands at sp+3, just within sp's lookahead window — must be accepted.
#[tokio::test]
async fn short_claim_queue_does_not_reject_ancestor_advertisements() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let core = test_state.rp_info[&active_leaf].assigned_core;

	// Shorten leaf-10's CQ for our core to a single para-100 position.
	test_state
		.rp_info
		.get_mut(&active_leaf)
		.unwrap()
		.claim_queue
		.insert(core, vec![100.into()]);

	let mut state = make_state(MockDb::default(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	let peer = peer_id(0);
	state.handle_peer_connected(&mut sender, peer, CollationVersion::V2).await;
	state.handle_declare(&mut sender, peer, 100.into()).await;

	// Advertise at sp=8 (depth 2 from leaf=10). leaf-CQ position 0 maps to sp-position 3,
	// which is within sp's lookahead-3 window — the ad must be accepted.
	let (_, adv) = dummy_candidate(get_hash(8), 100.into(), peer, core, 1, dummy_pvd().hash());
	test_state.handle_advertisement(&mut state, adv).await;
	assert_eq!(state.advertisements(), [adv].into());
}

// A is fetched SUCCESSFULLY at SP1; the segment [A, B] arrives (resubmitted)
// at SP2 after the next leaf activates. The walk skips A because it is in the
// fetched set.
#[tokio::test]
async fn v4_resubmission_at_new_sp_skips_fetched_entry() {
	let mut test_state = TestState::default();
	let (mut state, _db, sp_1) = v4_two_slot_fixture(&mut test_state).await;
	let mut sender = test_state.sender.clone();
	let leaf_info = test_state.rp_info.get(&get_hash(11)).unwrap().clone();

	let peer_id = PeerId::random();
	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	let relay_parent = get_hash(9);
	let (ccr, _) = dummy_candidate_v3(
		relay_parent,
		sp_1,
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);
	let fp_a = CandidateFingerprint {
		output_head_data_hash: ccr.descriptor.para_head(),
		parent_head_data_hash: dummy_pvd().parent_head.hash(),
		claim_queue_offset: 0,
	};
	let fp_b = CandidateFingerprint {
		output_head_data_hash: Hash::repeat_byte(0xb2),
		parent_head_data_hash: fp_a.output_head_data_hash,
		claim_queue_offset: 0,
	};

	// Fetch A successfully at SP1.
	test_state
		.send_v4_segment(&mut state, peer_id, sp_1, vec![fp_a.clone()], 100.into())
		.await;
	let adv_a = Advertisement {
		peer_id,
		para_id: 100.into(),
		scheduling_parent: sp_1,
		prospective_candidate: Some(v4_entry(&fp_a)),
		advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
	};
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(adv_a).await;
	test_state
		.handle_fetched_collation(&mut state, adv_a, ccr.to_plain(), None, relay_parent)
		.await;

	// Next leaf activates; the resubmitted segment [A, B] arrives at SP2.
	let sp_2 = get_hash(11);
	test_state.rp_info.insert(
		get_hash(12),
		RelayParentInfo {
			number: 12,
			parent: get_parent_hash(12),
			session_index: leaf_info.session_index,
			claim_queue: leaf_info.claim_queue.clone(),
			assigned_core: leaf_info.assigned_core,
		},
	);
	test_state.activate_leaf(&mut state, 12).await;
	test_state
		.send_v4_segment(&mut state, peer_id, sp_2, vec![fp_a.clone(), fp_b.clone()], 100.into())
		.await;

	// Selection skips fetched A and resolves B.
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state
		.assert_collation_request(Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent: sp_2,
			prospective_candidate: Some(v4_entry(&fp_b)),
			advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
		})
		.await;
	assert!(state.segments().is_empty());
	test_state.assert_no_messages().await;
}

#[tokio::test]
// Advance past IN-FLIGHT: peer 1's [A] launches and its fetch stays unconcluded; peer 2's
// [A, B], picked in a later pass, sees in-flight A through the known-set and resolves B.
async fn v4_walk_advances_past_in_flight_fetch() {
	let mut test_state = TestState::default();
	let (mut state, _db, scheduling_parent) = v4_two_slot_fixture(&mut test_state).await;
	let mut sender = test_state.sender.clone();

	let peer_1 = PeerId::random();
	let peer_2 = PeerId::random();
	for peer in [peer_1, peer_2] {
		state.handle_peer_connected(&mut sender, peer, CollationVersion::V4).await;
		state.handle_declare(&mut sender, peer, 100.into()).await;
	}

	let fp_a = v4_fingerprint(0xa1);
	let fp_b = CandidateFingerprint {
		output_head_data_hash: Hash::repeat_byte(0xa2),
		parent_head_data_hash: fp_a.output_head_data_hash,
		claim_queue_offset: 0,
	};

	let fetch_target = |peer_id, fp: &CandidateFingerprint| Advertisement {
		peer_id,
		para_id: 100.into(),
		scheduling_parent,
		prospective_candidate: Some(v4_entry(fp)),
		advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
	};

	// Peer 1's [A] launches; the fetch is left unconcluded — A is in flight.
	test_state
		.send_v4_segment(&mut state, peer_1, scheduling_parent, vec![fp_a.clone()], 100.into())
		.await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(fetch_target(peer_1, &fp_a)).await;

	// Peer 2's [A, B]: the walk advances past in-flight A and resolves B.
	test_state
		.send_v4_segment(
			&mut state,
			peer_2,
			scheduling_parent,
			vec![fp_a.clone(), fp_b.clone()],
			100.into(),
		)
		.await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(fetch_target(peer_2, &fp_b)).await;
	assert!(state.segments().is_empty());
	test_state.assert_no_messages().await;
}

#[tokio::test]
// output heads prospective-parachains reports through the live GetKnownOutputHeads
// answer are skipped by the walk; a segment whose every entry is PP-known is consumed without
// a launch and disappears from the live view.
async fn v4_pp_known_entries_skipped_and_all_known_deleted() {
	let fp_a = v4_fingerprint(0xa1);
	let fp_b = CandidateFingerprint {
		output_head_data_hash: Hash::repeat_byte(0xa2),
		parent_head_data_hash: fp_a.output_head_data_hash,
		claim_queue_offset: 0,
	};

	// PP knows A: the walk skips it and resolves B.
	{
		let mut test_state = TestState::default();
		test_state.pp_knows(get_hash(11), 100.into(), [fp_a.output_head_data_hash].into());
		let (mut state, _db, scheduling_parent) = v4_two_slot_fixture(&mut test_state).await;
		let mut sender = test_state.sender.clone();

		let peer_id = PeerId::random();
		state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
		state.handle_declare(&mut sender, peer_id, 100.into()).await;

		test_state
			.send_v4_segment(
				&mut state,
				peer_id,
				scheduling_parent,
				vec![fp_a.clone(), fp_b.clone()],
				100.into(),
			)
			.await;
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state
			.assert_collation_request(Advertisement {
				peer_id,
				para_id: 100.into(),
				scheduling_parent,
				prospective_candidate: Some(v4_entry(&fp_b)),
				advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
			})
			.await;
	}

	// PP knows both: all-blocked, consumed without a launch.
	{
		let mut test_state = TestState::default();
		test_state.pp_knows(
			get_hash(11),
			100.into(),
			[fp_a.output_head_data_hash, fp_b.output_head_data_hash].into(),
		);
		let (mut state, _db, scheduling_parent) = v4_two_slot_fixture(&mut test_state).await;
		let mut sender = test_state.sender.clone();

		let peer_id = PeerId::random();
		state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
		state.handle_declare(&mut sender, peer_id, 100.into()).await;

		test_state
			.send_v4_segment(
				&mut state,
				peer_id,
				scheduling_parent,
				vec![fp_a.clone(), fp_b.clone()],
				100.into(),
			)
			.await;
		assert_eq!(state.segments().len(), 1);
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_no_messages().await;
		assert!(state.segments().is_empty());
	}
}

#[tokio::test]
// PP knows head 0xa1 under `dead_leaf` only. Segment [0xa1, 0xa2] at SP `live_leaf` resolves
// 0xa1.
async fn v4_pp_knowledge_under_sibling_leaf_does_not_block() {
	let fp_a = v4_fingerprint(0xa1);
	let fp_b = CandidateFingerprint {
		output_head_data_hash: Hash::repeat_byte(0xa2),
		parent_head_data_hash: fp_a.output_head_data_hash,
		claim_queue_offset: 0,
	};

	let mut test_state = TestState::default();
	test_state
		.node_features
		.resize(node_features::FeatureIndex::CandidateReceiptV3 as usize + 1, false);
	test_state
		.node_features
		.set(node_features::FeatureIndex::CandidateReceiptV3 as u8 as usize, true);

	let leaf_info = test_state.rp_info.get(&get_hash(10)).unwrap().clone();
	let mut cq = leaf_info.claim_queue.clone();
	cq.insert(leaf_info.assigned_core, vec![100.into(), 100.into(), 100.into()]);

	let dead_leaf = get_hash(11);
	let live_leaf = Hash::random();
	for leaf in [dead_leaf, live_leaf] {
		test_state.rp_info.insert(
			leaf,
			RelayParentInfo {
				number: 11,
				parent: get_hash(10),
				session_index: leaf_info.session_index,
				claim_queue: cq.clone(),
				assigned_core: leaf_info.assigned_core,
			},
		);
	}

	// `live_leaf`'s slot is over, so it is valid as its own scheduling parent.
	let slot_duration = sp_consensus_slots::SlotDuration::from_millis(
		polkadot_primitives::RELAY_CHAIN_SLOT_DURATION_MILLIS,
	);
	let current_slot = sp_consensus_slots::Slot::from_timestamp(
		sp_timestamp::Timestamp::new(test_state.clock.duration_since_epoch().as_millis() as u64),
		slot_duration,
	);
	test_state
		.slot_overrides
		.insert(live_leaf, sp_consensus_slots::Slot::from(*current_slot - 1));

	let mut state = make_state(MockDb::default(), &mut test_state, get_hash(10)).await;
	let mut sender = test_state.sender.clone();
	test_state.activate_leaves(&mut state, vec![dead_leaf, live_leaf]).await;

	let peer_id = PeerId::random();
	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	// PP knows 0xa1 under `dead_leaf` only.
	test_state.pp_knows(dead_leaf, 100.into(), [fp_a.output_head_data_hash].into());

	test_state
		.send_v4_segment(&mut state, peer_id, live_leaf, vec![fp_a.clone(), fp_b], 100.into())
		.await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state
		.assert_collation_request(Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent: live_leaf,
			prospective_candidate: Some(v4_entry(&fp_a)),
			advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
		})
		.await;
	test_state.assert_no_messages().await;
}

/// Advertise, fetch and second one V4 candidate at `scheduling_parent`. Returns its fingerprint.
async fn v4_fetch_and_second(
	test_state: &mut TestState,
	state: &mut State<MockDb>,
	peer_id: PeerId,
	relay_parent: Hash,
	scheduling_parent: Hash,
	leaf_info: &RelayParentInfo,
) -> CandidateFingerprint {
	let mut sender = test_state.sender.clone();
	let (ccr, _) = dummy_candidate_v3(
		relay_parent,
		scheduling_parent,
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);
	let fp = CandidateFingerprint {
		output_head_data_hash: ccr.descriptor.para_head(),
		parent_head_data_hash: dummy_pvd().parent_head.hash(),
		claim_queue_offset: 0,
	};
	test_state
		.send_v4_segment(state, peer_id, scheduling_parent, vec![fp.clone()], 100.into())
		.await;
	let adv = Advertisement {
		peer_id,
		para_id: 100.into(),
		scheduling_parent,
		prospective_candidate: Some(v4_entry(&fp)),
		advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
	};
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(adv).await;
	test_state
		.handle_fetched_collation(state, adv, ccr.to_plain(), None, relay_parent)
		.await;
	test_state
		.second_collation(state, peer_id, CollationVersion::V4, ccr, scheduling_parent)
		.await;
	fp
}

#[tokio::test]
// Fork under block 10:
//   a11
//   b11 -> b12 -> b13p
//              -> b13pp
// fp_a: fetched and seconded at SP b11, PP-known at b13p and b13pp. 0xb1: PP-known at a11.
// [fp_a, 0xb1, 0xb2] at SP b12 fetches 0xb1. [fp_a, 0xc2] at SP 10 fetches fp_a.
async fn v4_nested_fork_knowledge_stays_on_its_branch() {
	let mut test_state = TestState::default();
	test_state
		.node_features
		.resize(node_features::FeatureIndex::CandidateReceiptV3 as usize + 1, false);
	test_state
		.node_features
		.set(node_features::FeatureIndex::CandidateReceiptV3 as u8 as usize, true);

	let leaf_info = test_state.rp_info.get(&get_hash(10)).unwrap().clone();
	let mut cq = leaf_info.claim_queue.clone();
	cq.insert(leaf_info.assigned_core, vec![100.into(), 100.into(), 100.into()]);

	let a11 = Hash::from_low_u64_be(0xa11);
	let b11 = Hash::from_low_u64_be(0xb11);
	let b12 = Hash::from_low_u64_be(0xb12);
	let b13p = Hash::from_low_u64_be(0xb131);
	let b13pp = Hash::from_low_u64_be(0xb132);
	for (hash, number, parent) in [
		(a11, 11, get_hash(10)),
		(b11, 11, get_hash(10)),
		(b12, 12, b11),
		(b13p, 13, b12),
		(b13pp, 13, b12),
	] {
		test_state.rp_info.insert(
			hash,
			RelayParentInfo {
				number,
				parent,
				session_index: leaf_info.session_index,
				claim_queue: cq.clone(),
				assigned_core: leaf_info.assigned_core,
			},
		);
	}

	let mut state = make_state(MockDb::default(), &mut test_state, get_hash(10)).await;
	let mut sender = test_state.sender.clone();
	let peer_id = PeerId::random();

	test_state.activate_leaves(&mut state, vec![a11]).await;
	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	// Fetch and second fp_a at SP b11, the parent of leaf b12.
	test_state.activate_leaves(&mut state, vec![a11, b12]).await;
	let fp_a =
		v4_fetch_and_second(&mut test_state, &mut state, peer_id, get_hash(10), b11, &leaf_info)
			.await;

	test_state.activate_leaves(&mut state, vec![a11, b13p, b13pp]).await;

	let fp_p = v4_fingerprint(0xb1);
	let fp_q = CandidateFingerprint {
		output_head_data_hash: Hash::repeat_byte(0xb2),
		parent_head_data_hash: fp_p.output_head_data_hash,
		claim_queue_offset: 0,
	};
	let fp_r = CandidateFingerprint {
		output_head_data_hash: Hash::repeat_byte(0xc2),
		parent_head_data_hash: fp_a.output_head_data_hash,
		claim_queue_offset: 0,
	};
	// PP: fp_a at b13p and b13pp (SP b11 is in both scopes), 0xb1 at a11 only.
	test_state.pp_knows(a11, 100.into(), [fp_p.output_head_data_hash].into());
	test_state.pp_knows(b13p, 100.into(), [fp_a.output_head_data_hash].into());
	test_state.pp_knows(b13pp, 100.into(), [fp_a.output_head_data_hash].into());

	// SP b12 is on both b paths: fp_a is known there, 0xb1 is not.
	test_state
		.send_v4_segment(
			&mut state,
			peer_id,
			b12,
			vec![fp_a.clone(), fp_p.clone(), fp_q],
			100.into(),
		)
		.await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state
		.assert_collation_request(Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent: b12,
			prospective_candidate: Some(v4_entry(&fp_p)),
			advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
		})
		.await;
	test_state.assert_no_messages().await;

	// SP 10 is on a11's path only: fp_a (fetched at b11, PP-known at b13p and b13pp) does not
	// block.
	test_state
		.send_v4_segment(&mut state, peer_id, get_hash(10), vec![fp_a.clone(), fp_r], 100.into())
		.await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state
		.assert_collation_request(Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent: get_hash(10),
			prospective_candidate: Some(v4_entry(&fp_a)),
			advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
		})
		.await;
	test_state.assert_no_messages().await;
}

#[tokio::test]
// PP knowledge is read live at each planner pass: a head that becomes known AFTER
// leaf activation, with no further view change, is skipped by the walk.
async fn v4_pp_knowledge_arriving_between_passes_is_seen() {
	let fp_a = v4_fingerprint(0xa1);
	let fp_b = CandidateFingerprint {
		output_head_data_hash: Hash::repeat_byte(0xa2),
		parent_head_data_hash: fp_a.output_head_data_hash,
		claim_queue_offset: 0,
	};

	let mut test_state = TestState::default();
	// The fixture runs every view update with an EMPTY PP map.
	let (mut state, _db, scheduling_parent) = v4_two_slot_fixture(&mut test_state).await;
	let mut sender = test_state.sender.clone();

	let peer_id = PeerId::random();
	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	// A becomes PP-known only now — after activation, before the pass.
	test_state.pp_knows(get_hash(11), 100.into(), [fp_a.output_head_data_hash].into());

	test_state
		.send_v4_segment(
			&mut state,
			peer_id,
			scheduling_parent,
			vec![fp_a.clone(), fp_b.clone()],
			100.into(),
		)
		.await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state
		.assert_collation_request(Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent,
			prospective_candidate: Some(v4_entry(&fp_b)),
			advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
		})
		.await;
}

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
// The PP answer is the knowledge that OUTLIVES the fetched-set: A is fetched and seconded
// at SP1, then enough leaves activate that SP1 leaves the view and A's `fetched_collations`
// entry dies with it.
//
// Control arm (false): PP knowledge unseeded — the seconded candidate is REFETCHED, proving the
// fetched-set knowledge really expired. Treatment arm (true): PP reports A (as production PP
// would, having introduced it at seconding) — the PP answer alone blocks the refetch and the
// walk resolves B.
async fn v4_seconded_head_blocked_after_fetched_entry_expires(#[case] pp_reports_a: bool) {
	let mut test_state = TestState::default();
	let (mut state, _db, sp_1) = v4_two_slot_fixture(&mut test_state).await;
	let mut sender = test_state.sender.clone();
	let leaf_info = test_state.rp_info.get(&get_hash(11)).unwrap().clone();

	let peer_id = PeerId::random();
	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	let relay_parent = get_hash(9);
	let (ccr, _) = dummy_candidate_v3(
		relay_parent,
		sp_1,
		100.into(),
		peer_id,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);
	let fp_a = CandidateFingerprint {
		output_head_data_hash: ccr.descriptor.para_head(),
		parent_head_data_hash: dummy_pvd().parent_head.hash(),
		claim_queue_offset: 0,
	};
	let fp_b = CandidateFingerprint {
		output_head_data_hash: Hash::repeat_byte(0xb2),
		parent_head_data_hash: fp_a.output_head_data_hash,
		claim_queue_offset: 0,
	};

	// Fetch and second A at SP1.
	test_state
		.send_v4_segment(&mut state, peer_id, sp_1, vec![fp_a.clone()], 100.into())
		.await;
	let adv_a = Advertisement {
		peer_id,
		para_id: 100.into(),
		scheduling_parent: sp_1,
		prospective_candidate: Some(v4_entry(&fp_a)),
		advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
	};
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_request(adv_a).await;
	test_state
		.handle_fetched_collation(&mut state, adv_a, ccr.to_plain(), None, relay_parent)
		.await;
	test_state
		.second_collation(&mut state, peer_id, CollationVersion::V4, ccr, sp_1)
		.await;

	if pp_reports_a {
		// As production PP would: A was introduced at seconding, so every
		// subsequent refresh reports it.
		test_state.pp_knows(get_hash(13), 100.into(), [fp_a.output_head_data_hash].into());
	}

	// Two more leaves: SP1 leaves the view and A's fetched entry dies with it.
	// Gates 1-2 are now empty for A; only the (possibly seeded) PP knowledge remains.
	for number in [12u32, 13] {
		test_state.rp_info.insert(
			get_hash(number),
			RelayParentInfo {
				number,
				parent: get_parent_hash(number),
				session_index: leaf_info.session_index,
				claim_queue: leaf_info.claim_queue.clone(),
				assigned_core: leaf_info.assigned_core,
			},
		);
		test_state.activate_leaf(&mut state, number).await;
	}

	let sp_3 = get_hash(12);
	test_state
		.send_v4_segment(&mut state, peer_id, sp_3, vec![fp_a.clone(), fp_b.clone()], 100.into())
		.await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	// With PP reporting A the walk resolves B; without it the knowledge is truly
	// gone and A — an already-seconded candidate — gets refetched. The control
	// arm is what makes the treatment arm's B attributable to the PP answer and
	// nothing else (it proves the fetched-set entry really expired).
	let expected = if pp_reports_a { &fp_b } else { &fp_a };
	test_state
		.assert_collation_request(Advertisement {
			peer_id,
			para_id: 100.into(),
			scheduling_parent: sp_3,
			prospective_candidate: Some(v4_entry(expected)),
			advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
		})
		.await;
	test_state.assert_no_messages().await;
}

#[tokio::test]
// The mixed-version transitional gap, a V3 peer advertises candidate B by hash while a V4
// peer's segment resolves B by output head — the two are mutually blind pre-fetch, so B is
// fetched twice during the overlap. The system tolerates the duplicate, and once the by-hash
// fetch concludes (output heads are recorded for EVERY protocol version), the walk converges:
//  a re-advertised segment resolves C, never B again.
async fn v4_mixed_version_double_fetch_converges() {
	let fp_a = v4_fingerprint(0xa1);

	let mut test_state = TestState::default();
	// A is already PP-known so the V4 segment resolves B on its first pick.
	test_state.pp_knows(get_hash(11), 100.into(), [fp_a.output_head_data_hash].into());
	let (mut state, _db, scheduling_parent) = v4_two_slot_fixture(&mut test_state).await;
	let mut sender = test_state.sender.clone();
	let leaf_info = test_state.rp_info.get(&get_hash(11)).unwrap().clone();

	let v3_peer = PeerId::random();
	let v4_peer = PeerId::random();
	state.handle_peer_connected(&mut sender, v3_peer, CollationVersion::V3).await;
	state.handle_declare(&mut sender, v3_peer, 100.into()).await;
	state.handle_peer_connected(&mut sender, v4_peer, CollationVersion::V4).await;
	state.handle_declare(&mut sender, v4_peer, 100.into()).await;

	// The real para block B, advertised by hash by the V3 peer…
	let relay_parent = get_hash(9);
	let (ccr, _) = dummy_candidate_v3(
		relay_parent,
		scheduling_parent,
		100.into(),
		v3_peer,
		leaf_info.assigned_core,
		leaf_info.session_index,
		dummy_pvd().hash(),
	);
	let by_hash_adv = Advertisement {
		peer_id: v3_peer,
		para_id: 100.into(),
		scheduling_parent,
		prospective_candidate: Some(ProspectiveCandidate::ByHash {
			candidate_hash: ccr.to_plain().hash(),
			parent_head_data_hash: dummy_pvd().parent_head.hash(),
		}),
		advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
	};
	test_state.handle_advertisement(&mut state, by_hash_adv).await;

	// …and by output head inside the V4 peer's segment [A, B, C].
	let fp_b = CandidateFingerprint {
		output_head_data_hash: ccr.descriptor.para_head(),
		parent_head_data_hash: fp_a.output_head_data_hash,
		claim_queue_offset: 0,
	};
	let fp_c = CandidateFingerprint {
		output_head_data_hash: Hash::repeat_byte(0xc3),
		parent_head_data_hash: fp_b.output_head_data_hash,
		claim_queue_offset: 0,
	};
	let segment = vec![fp_a.clone(), fp_b.clone(), fp_c.clone()];
	test_state
		.send_v4_segment(&mut state, v4_peer, scheduling_parent, segment.clone(), 100.into())
		.await;

	// One pass launches BOTH fetches of B: the by-hash ticket carries no output head and
	// the hash-based checks cannot see the V4 ticket — the documented two-sided blindness.
	let v4_b_adv = Advertisement {
		peer_id: v4_peer,
		para_id: 100.into(),
		scheduling_parent,
		prospective_candidate: Some(v4_entry(&fp_b)),
		advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
	};
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_collation_requests([by_hash_adv, v4_b_adv].into()).await;

	// The by-hash fetch concludes successfully; the V4 duplicate fails. B's output head is
	// now durably in the fetched-set.
	test_state
		.handle_fetched_collation(&mut state, by_hash_adv, ccr.to_plain(), None, relay_parent)
		.await;
	state
		.handle_fetched_collation(
			&mut sender,
			(
				v4_b_adv,
				Err(CollationFetchError::Request(RequestError::NetworkError(
					RequestFailure::NotConnected,
				))),
			),
		)
		.await;
	test_state.assert_no_messages().await;

	// Convergence: a re-advertised segment resolves C — A is PP-known, B is fetched.
	test_state
		.send_v4_segment(&mut state, v4_peer, scheduling_parent, segment, 100.into())
		.await;
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state
		.assert_collation_request(Advertisement {
			peer_id: v4_peer,
			para_id: 100.into(),
			scheduling_parent,
			prospective_candidate: Some(v4_entry(&fp_c)),
			advertised_descriptor_version: Some(CandidateDescriptorVersion::V3),
		})
		.await;
	test_state.assert_no_messages().await;
}

#[tokio::test]
// A segment carrying a zero-length cycle (an entry whose parent and output heads are equal)
// is rejected: nothing is stored, and — because rejection happens before the
// rate-limit counter — the peer's cap is untouched, so a subsequent valid segment from the
// same peer is accepted.
async fn v4_zero_length_cycle_segment_rejected() {
	let mut test_state = TestState::default();
	let (mut state, _db, scheduling_parent) = v4_two_slot_fixture(&mut test_state).await;
	let mut sender = test_state.sender.clone();

	let peer_id = PeerId::random();
	state.handle_peer_connected(&mut sender, peer_id, CollationVersion::V4).await;
	state.handle_declare(&mut sender, peer_id, 100.into()).await;

	let self_loop = CandidateFingerprint {
		output_head_data_hash: Hash::repeat_byte(0xaa),
		parent_head_data_hash: Hash::repeat_byte(0xaa),
		claim_queue_offset: 0,
	};
	let fp_ok = v4_fingerprint(0xb1);

	let malformed = [vec![], vec![fp_ok.clone(), self_loop.clone()]];

	for segment in malformed.iter().cloned() {
		test_state
			.send_v4_segment(&mut state, peer_id, scheduling_parent, segment, 100.into())
			.await;
		assert!(state.segments().is_empty());
		state
			.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
			.await;
		test_state.assert_no_messages().await;
	}

	test_state
		.send_v4_segment(
			&mut state,
			peer_id,
			scheduling_parent,
			vec![fp_ok.clone(), self_loop],
			100.into(),
		)
		.await;
	assert!(state.segments().is_empty());
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	test_state.assert_no_messages().await;

	// The rejection consumed no cap: a valid segment from the same peer is accepted.
	test_state
		.send_v4_segment(&mut state, peer_id, scheduling_parent, vec![fp_ok.clone()], 100.into())
		.await;
	assert_eq!(state.segments(), [(scheduling_parent, peer_id, vec![v4_entry(&fp_ok)])].into());
}

// Lookahead-truncation regression (the bug this PR fixes): the leaf-CQ window must be bounded
// by the runtime scheduling lookahead, NOT by the implicit-view ancestry path length.
//
// After a session change the implicit view clamps ancestry at the session boundary (the relay
// chain cannot back candidates from a previous session), so the allowed-ancestry path is
// shorter than the lookahead. Here leaf 10 (session 1) has ancestors 9 and 8 in session 0, so
// the path under the leaf is just `[10]` (length 1) even though the lookahead is 3.
//
// leaf-10's CQ for our core is `[100, 200, 100]` — para 100 owns positions 0 and 2. The leaf
// SP (depth 0) reaches positions `0 .. lookahead = 0..3`, so both 100-slots are fetchable.
//
// Pre-fix the window length was the ancestry path length (1), so only position 0 was reached
// and para 100 got a single fetch; the position-2 slot was silently dropped. With the lookahead
// sourced from the runtime, both para-100 ads must fetch.
#[tokio::test]
async fn lookahead_not_truncated_by_short_ancestry_after_session_change() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let core = test_state.rp_info[&active_leaf].assigned_core;
	// Sanity: leaf-10 CQ for our core has para 100 at positions 0 and 2, lookahead is 3.
	assert_eq!(
		test_state.rp_info[&active_leaf].claim_queue[&core],
		vec![100.into(), 200.into(), 100.into()]
	);
	assert_eq!(test_state.session_info[&1].scheduling_lookahead, 3);

	// Put the ancestors in a previous session so the implicit view truncates ancestry at the
	// session boundary, leaving the leaf as the only relay parent in its own allowed ancestry.
	for h in [get_hash(9), get_hash(8)] {
		test_state.rp_info.get_mut(&h).unwrap().session_index = 0;
	}

	let mut state = make_state(MockDb::default(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	// Two peers, both for para 100, advertising two distinct candidates at the leaf SP.
	let peers: Vec<_> = (0..2).map(peer_id).collect();
	for &p in &peers {
		state.handle_peer_connected(&mut sender, p, CollationVersion::V2).await;
		state.handle_declare(&mut sender, p, 100.into()).await;
	}
	for (i, &p) in peers.iter().enumerate() {
		let (_, adv) = dummy_candidate(
			active_leaf,
			100.into(),
			p,
			core,
			1,
			Hash::from_low_u64_be(i as u64 + 100),
		);
		test_state.handle_advertisement(&mut state, adv).await;
	}

	// Both para-100 slots (positions 0 and 2) are reachable from the leaf SP, so both ads fetch.
	// Pre-fix the truncated window (length 1) would have produced only a single fetch.
	state
		.try_launch_new_fetch_requests(&mut sender, &test_state.pp_known_output_heads)
		.await;
	let msg = test_state.timeout_recv().await;
	assert_matches::assert_matches!(
		msg,
		AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendRequests(reqs, _)) => {
			assert_eq!(
				reqs.len(), 2,
				"both para-100 slots must be fetchable from the leaf; lookahead must not be \
				 truncated to the ancestry path length, got {} fetch(es)",
				reqs.len()
			);
		}
	);
	test_state.assert_no_messages().await;
}

#[tokio::test]
async fn startup_populates_db_from_finalized_chain() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);

	let first_peer = peer_id(1);
	let second_peer = peer_id(2);
	let third_peer = peer_id(3);

	// The node starts with finalized block 6 and a DB which hasn't processed any blocks yet.
	test_state.finalized_block = 6;
	test_state.set_candidates_pending_availability(
		[
			(get_hash(2), vec![(ParaId::from(100), first_peer)]),
			(
				get_hash(5),
				vec![
					(ParaId::from(200), first_peer),
					(ParaId::from(200), first_peer),
					(ParaId::from(200), second_peer),
				],
			),
			(get_hash(6), vec![(ParaId::from(100), third_peer)]),
		]
		.into_iter()
		.collect(),
	);

	let db = MockDb::default();
	let state = make_state(db.clone(), &mut test_state, active_leaf).await;

	let mut expected_bumps = BTreeMap::new();
	expected_bumps.insert(
		ParaId::new(100),
		[
			(first_peer, Score::new(VALID_INCLUDED_CANDIDATE_BUMP)),
			(third_peer, Score::new(VALID_INCLUDED_CANDIDATE_BUMP)),
		]
		.into_iter()
		.collect(),
	);
	expected_bumps.insert(
		ParaId::new(200),
		[
			(first_peer, Score::new(2 * VALID_INCLUDED_CANDIDATE_BUMP)),
			(second_peer, Score::new(VALID_INCLUDED_CANDIDATE_BUMP)),
		]
		.into_iter()
		.collect(),
	);
	assert_eq!(db.witnessed_bumps(), expected_bumps);
	// The DB has caught up with the finalized chain.
	assert_eq!(db.processed_finalized_block_number().await, Some(6));
	// Startup doesn't prune the registered paras.
	assert!(db.witnessed_prunes().is_empty());

	drop(state);

	// Simulate a restart with the same DB: the already processed blocks are not re-processed,
	// so the candidates of the ancestry don't produce bumps again.
	let _state = make_state(db.clone(), &mut test_state, active_leaf).await;

	assert!(db.witnessed_bumps().is_empty());
	assert_eq!(db.processed_finalized_block_number().await, Some(6));
}

#[tokio::test]
async fn startup_lookback_is_capped() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);

	let outside_peer = peer_id(1);
	let oldest_peer = peer_id(2);
	let newest_peer = peer_id(3);

	let finalized = MAX_STARTUP_ANCESTRY_LOOKBACK + 10;
	// The oldest block whose included candidates are within the lookback window.
	let window_start = finalized - MAX_STARTUP_ANCESTRY_LOOKBACK + 1;

	test_state.finalized_block = finalized;
	test_state.set_candidates_pending_availability(
		[
			// Right outside of the lookback window.
			(get_hash(window_start - 1), vec![(ParaId::from(100), outside_peer)]),
			(get_hash(window_start), vec![(ParaId::from(100), oldest_peer)]),
			(get_hash(finalized), vec![(ParaId::from(100), newest_peer)]),
		]
		.into_iter()
		.collect(),
	);

	let db = MockDb::default();
	let _state = make_state(db.clone(), &mut test_state, active_leaf).await;

	let mut expected_bumps = BTreeMap::new();
	expected_bumps.insert(
		ParaId::new(100),
		[
			(oldest_peer, Score::new(VALID_INCLUDED_CANDIDATE_BUMP)),
			(newest_peer, Score::new(VALID_INCLUDED_CANDIDATE_BUMP)),
		]
		.into_iter()
		.collect(),
	);
	assert_eq!(db.witnessed_bumps(), expected_bumps);
	assert_eq!(db.processed_finalized_block_number().await, Some(finalized));
}

// Should never happen but verify that the case is handled gracefully.
#[tokio::test]
async fn startup_with_db_ahead_of_finalized_chain() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);

	test_state.finalized_block = 8;
	let db = MockDb::default();
	db.set_processed_finalized_block_number(15);
	// A candidate on the finalized chain which must not be processed.
	test_state.set_candidates_pending_availability(
		[(get_hash(5), vec![(ParaId::from(100), peer_id(1))])].into_iter().collect(),
	);

	let _state = make_state(db.clone(), &mut test_state, active_leaf).await;

	assert!(db.witnessed_bumps().is_empty());
	assert_eq!(db.processed_finalized_block_number().await, Some(15));
}

// Test the registered paras processing on finalized block notifications: the registered paras
// are queried once per session and the DB is told to prune the paras that are no longer
// registered.
#[tokio::test]
async fn registered_paras_pruned_on_new_session() {
	let mut test_state = TestState::default();
	let active_leaf = get_hash(10);
	let leaf_info = test_state.rp_info.get(&active_leaf).unwrap().clone();

	let db = MockDb::default();
	let mut state = make_state(db.clone(), &mut test_state, active_leaf).await;
	let mut sender = test_state.sender.clone();

	// The first finalized block notification always queries the registered paras.
	let processed = state.processed_finalized_block_number().await.unwrap_or_default();
	futures::join!(test_state.handle_finalized_block(processed, 2), async {
		state.handle_finalized_block(&mut sender, get_hash(2), 2).await.unwrap()
	});
	test_state.assert_no_messages().await;
	assert_eq!(
		db.witnessed_prunes(),
		vec![[100.into(), 200.into(), 600.into()].into_iter().collect::<BTreeSet<ParaId>>()]
	);

	// A subsequent finalized block in the same session doesn't re-query the registered paras
	// (the harness panics on an unexpected `ParaIds` request).
	let processed = state.processed_finalized_block_number().await.unwrap_or_default();
	futures::join!(test_state.handle_finalized_block(processed, 3), async {
		state.handle_finalized_block(&mut sender, get_hash(3), 3).await.unwrap()
	});
	test_state.assert_no_messages().await;
	assert!(db.witnessed_prunes().is_empty());

	// Start session 2, where para 200 is deregistered.
	let mut session_2_info = test_state.session_info.get(&1).unwrap().clone();
	session_2_info.paras = vec![100.into(), 600.into()];
	test_state.session_info.insert(2, session_2_info);
	test_state.rp_info.insert(
		get_hash(4),
		RelayParentInfo {
			number: 4,
			parent: get_parent_hash(4),
			session_index: 2,
			claim_queue: leaf_info.claim_queue.clone(),
			assigned_core: leaf_info.assigned_core,
		},
	);

	let processed = state.processed_finalized_block_number().await.unwrap_or_default();
	futures::join!(test_state.handle_finalized_block(processed, 4), async {
		state.handle_finalized_block(&mut sender, get_hash(4), 4).await.unwrap()
	});
	test_state.assert_no_messages().await;
	assert_eq!(
		db.witnessed_prunes(),
		vec![[100.into(), 600.into()].into_iter().collect::<BTreeSet<ParaId>>()]
	);

	// A finalized block with an older session index doesn't trigger a query.
	// (block 5 is not in `rp_info`, so the harness reports session 1 for it)
	let processed = state.processed_finalized_block_number().await.unwrap_or_default();
	futures::join!(test_state.handle_finalized_block(processed, 5), async {
		state.handle_finalized_block(&mut sender, get_hash(5), 5).await.unwrap()
	});
	test_state.assert_no_messages().await;
	assert!(db.witnessed_prunes().is_empty());

	// Same for an equal session index.
	test_state.rp_info.insert(
		get_hash(6),
		RelayParentInfo {
			number: 6,
			parent: get_parent_hash(6),
			session_index: 2,
			claim_queue: leaf_info.claim_queue.clone(),
			assigned_core: leaf_info.assigned_core,
		},
	);
	let processed = state.processed_finalized_block_number().await.unwrap_or_default();
	futures::join!(test_state.handle_finalized_block(processed, 6), async {
		state.handle_finalized_block(&mut sender, get_hash(6), 6).await.unwrap()
	});
	test_state.assert_no_messages().await;
	assert!(db.witnessed_prunes().is_empty());
}
