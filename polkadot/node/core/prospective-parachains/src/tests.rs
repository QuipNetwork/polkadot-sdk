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
use futures::FutureExt;
use polkadot_node_subsystem::{
	messages::{
		AllMessages, HypotheticalMembershipRequest, ParentHeadData, ProspectiveParachainsMessage,
		ProspectiveValidationDataRequest, RuntimeApiMessage, RuntimeApiRequest,
	},
	RuntimeApiError,
};
use polkadot_node_subsystem_test_helpers as test_helpers;
use polkadot_primitives::{
	async_backing::{CandidatePendingAvailability, Constraints, InboundHrmpLimitations},
	vstaging::RelayParentInfo,
	BlockNumber, CommittedCandidateReceiptV2 as CommittedCandidateReceipt, CoreIndex, HeadData,
	Header, MutateDescriptorV2, PersistedValidationData, ValidationCodeHash,
	DEFAULT_SCHEDULING_LOOKAHEAD,
};
use polkadot_primitives_test_helpers::{make_candidate, make_candidate_v3};
use rstest::rstest;
use std::{
	cell::RefCell,
	collections::{BTreeMap, HashSet, VecDeque},
	sync::Arc,
};
use test_helpers::mock::new_leaf;

const RUNTIME_API_NOT_SUPPORTED: RuntimeApiError =
	RuntimeApiError::NotSupported { runtime_api_name: "test-runtime" };

const MAX_POV_SIZE: u32 = 1_000_000;

type VirtualOverseer =
	polkadot_node_subsystem_test_helpers::TestSubsystemContextHandle<ProspectiveParachainsMessage>;

fn dummy_constraints(
	min_relay_parent_number: BlockNumber,
	valid_watermarks: Vec<BlockNumber>,
	required_parent: HeadData,
	validation_code_hash: ValidationCodeHash,
) -> Constraints {
	Constraints {
		min_relay_parent_number,
		max_pov_size: MAX_POV_SIZE,
		max_head_data_size: 20480,
		max_code_size: 1_000_000,
		ump_remaining: 10,
		ump_remaining_bytes: 1_000,
		max_ump_num_per_candidate: 10,
		dmp_remaining_messages: vec![],
		hrmp_inbound: InboundHrmpLimitations { valid_watermarks },
		hrmp_channels_out: vec![],
		max_hrmp_num_per_candidate: 0,
		required_parent,
		validation_code_hash,
		upgrade_restriction: None,
		future_validation_code: None,
	}
}

struct TestState {
	claim_queue: BTreeMap<CoreIndex, VecDeque<ParaId>>,
	runtime_api_version: u32,
	validation_code_hash: ValidationCodeHash,
	/// If set, overrides the `min_relay_parent_number` returned in the backing constraints.
	/// Otherwise, defaults to `leaf.number - (scheduling_lookahead - 1)`.
	min_relay_parent_number_override: Option<BlockNumber>,
	/// Mirrors the production LRU cache populated by `fetch_relay_parent_info_cached`. When a
	/// relay parent is already cached, the subsystem won't send a runtime API request for it, so
	/// the test harness must not block on `recv`. Tests use a single session (1), so we key by
	/// relay parent only. `RefCell` allows interior mutability while helpers still take
	/// `&TestState`.
	cached_relay_parents: RefCell<HashSet<Hash>>,
}

impl Default for TestState {
	fn default() -> Self {
		let chain_a = ParaId::from(1);
		let chain_b = ParaId::from(2);

		let mut claim_queue = BTreeMap::new();
		claim_queue.insert(
			CoreIndex(0),
			std::iter::repeat(chain_a).take(DEFAULT_SCHEDULING_LOOKAHEAD as _).collect(),
		);
		claim_queue.insert(
			CoreIndex(1),
			std::iter::repeat(chain_b).take(DEFAULT_SCHEDULING_LOOKAHEAD as _).collect(),
		);

		let validation_code_hash = Hash::repeat_byte(42).into();

		Self {
			validation_code_hash,
			claim_queue,
			runtime_api_version: RuntimeApiRequest::ANCESTOR_RELAY_PARENT_INFO_RUNTIME_REQUIREMENT,
			min_relay_parent_number_override: None,
			cached_relay_parents: RefCell::new(HashSet::new()),
		}
	}
}

impl TestState {
	fn set_runtime_api_version(&mut self, version: u32) {
		self.runtime_api_version = version;
	}

	fn set_min_relay_parent_number(&mut self, n: BlockNumber) {
		self.min_relay_parent_number_override = Some(n);
	}
}

fn get_parent_hash(hash: Hash) -> Hash {
	Hash::from_low_u64_be(hash.to_low_u64_be() + 1)
}

fn test_harness<T: Future<Output = VirtualOverseer>>(
	test: impl FnOnce(VirtualOverseer) -> T,
) -> View {
	sp_tracing::init_for_tests();

	let pool = sp_core::testing::TaskExecutor::new();

	let (mut context, virtual_overseer) =
		polkadot_node_subsystem_test_helpers::make_subsystem_context(pool.clone());

	let mut view = View::new();
	let subsystem = async move {
		if let Err(e) = run_iteration(&mut context, &mut view, &Metrics(None)).await {
			panic!("{:?}", e);
		}

		view
	};

	let test_fut = test(virtual_overseer);

	futures::pin_mut!(test_fut);
	futures::pin_mut!(subsystem);
	let (_, view) = futures::executor::block_on(future::join(
		async move {
			let mut virtual_overseer = test_fut.await;
			virtual_overseer.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
		},
		subsystem,
	));

	view
}

#[derive(Debug, Clone)]
struct PerParaData {
	head_data: HeadData,
	pending_availability: Vec<CandidatePendingAvailability>,
}

impl PerParaData {
	pub fn new(head_data: HeadData) -> Self {
		Self { head_data, pending_availability: Vec::new() }
	}

	pub fn new_with_pending(
		head_data: HeadData,
		pending: Vec<CandidatePendingAvailability>,
	) -> Self {
		Self { head_data, pending_availability: pending }
	}
}

struct TestLeaf {
	number: BlockNumber,
	hash: Hash,
	para_data: Vec<(ParaId, PerParaData)>,
}

impl TestLeaf {
	pub fn para_data(&self, para_id: ParaId) -> &PerParaData {
		self.para_data
			.iter()
			.find_map(|(p_id, data)| if *p_id == para_id { Some(data) } else { None })
			.unwrap()
	}
}

/// Handle messages sent by `fetch_relay_parent_info`. Paths taken depend on the runtime API
/// version:
/// - Self-query (query_at == relay_parent): SessionIndexForChild + BlockHeader (regardless of
///   version).
/// - Ancestor query when `runtime_api_version` >= `ANCESTOR_RELAY_PARENT_INFO_RUNTIME_REQUIREMENT`:
///   answer the `AncestorRelayParentInfo` runtime API directly with a successful response.
/// - Ancestor query when `runtime_api_version` is older: answer `AncestorRelayParentInfo` with
///   `NotSupported`, causing production to fall back to SessionIndexForChild + BlockHeader.
async fn handle_fetch_relay_parent_info(
	virtual_overseer: &mut VirtualOverseer,
	test_state: &TestState,
	relay_parent: Hash,
	relay_parent_number: BlockNumber,
) {
	let msg = virtual_overseer.recv().await;
	handle_fetch_relay_parent_info_message(
		virtual_overseer,
		msg,
		test_state,
		relay_parent,
		relay_parent_number,
	)
	.await;
}

async fn handle_fetch_relay_parent_info_message(
	virtual_overseer: &mut VirtualOverseer,
	msg: AllMessages,
	test_state: &TestState,
	relay_parent: Hash,
	relay_parent_number: BlockNumber,
) {
	let ancestor_api_supported = test_state.runtime_api_version >=
		RuntimeApiRequest::ANCESTOR_RELAY_PARENT_INFO_RUNTIME_REQUIREMENT;

	match msg {
		AllMessages::RuntimeApi(RuntimeApiMessage::Request(
			_parent,
			RuntimeApiRequest::AncestorRelayParentInfo(_, rp, tx),
		)) => {
			assert_eq!(rp, relay_parent);
			if ancestor_api_supported {
				// Runtime API supported: answer directly.
				tx.send(Ok(Some(RelayParentInfo {
					number: relay_parent_number,
					state_root: Hash::zero(),
				})))
				.unwrap();
			} else {
				// Runtime API not supported: trigger the fallback.
				tx.send(Err(RUNTIME_API_NOT_SUPPORTED)).unwrap();

				assert_matches!(
					virtual_overseer.recv().await,
					AllMessages::RuntimeApi(
						RuntimeApiMessage::Request(parent, RuntimeApiRequest::SessionIndexForChild(tx))
					) if parent == relay_parent => {
						tx.send(Ok(1)).unwrap();
					}
				);
				send_block_header(virtual_overseer, relay_parent, relay_parent_number).await;
			}
		},
		AllMessages::RuntimeApi(RuntimeApiMessage::Request(
			parent,
			RuntimeApiRequest::SessionIndexForChild(tx),
		)) if parent == relay_parent => {
			tx.send(Ok(1)).unwrap();
			send_block_header(virtual_overseer, relay_parent, relay_parent_number).await;
		},
		other => panic!("Unexpected message in handle_fetch_relay_parent_info: {:?}", other),
	}

	// Production just populated its LRU cache for this relay parent; mirror that here so the
	// harness can skip subsequent expected messages that won't arrive.
	test_state.cached_relay_parents.borrow_mut().insert(relay_parent);
}

async fn send_block_header(virtual_overseer: &mut VirtualOverseer, hash: Hash, number: u32) {
	let header = Header {
		parent_hash: get_parent_hash(hash),
		number,
		state_root: Hash::zero(),
		extrinsics_root: Hash::zero(),
		digest: Default::default(),
	};

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::ChainApi(
			ChainApiMessage::BlockHeader(parent, tx)
		) if parent == hash => {
			tx.send(Ok(Some(header))).unwrap();
		}
	);
}

async fn activate_leaf(
	virtual_overseer: &mut VirtualOverseer,
	leaf: &TestLeaf,
	test_state: &TestState,
) {
	let TestLeaf { number, hash, .. } = leaf;

	let activated = new_leaf(*hash, *number);

	virtual_overseer
		.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(
			activated,
		))))
		.await;

	handle_leaf_activation(virtual_overseer, leaf, test_state, get_parent_hash).await;
}

async fn activate_leaf_with_parent_hash_fn(
	virtual_overseer: &mut VirtualOverseer,
	leaf: &TestLeaf,
	test_state: &TestState,
	parent_hash_fn: impl Fn(Hash) -> Hash,
) {
	let TestLeaf { number, hash, .. } = leaf;

	let activated = new_leaf(*hash, *number);

	virtual_overseer
		.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(
			activated,
		))))
		.await;

	handle_leaf_activation(virtual_overseer, leaf, test_state, parent_hash_fn).await;
}

async fn handle_leaf_activation(
	virtual_overseer: &mut VirtualOverseer,
	leaf: &TestLeaf,
	test_state: &TestState,
	parent_hash_fn: impl Fn(Hash) -> Hash,
) {
	let TestLeaf { number, hash, para_data: _ } = leaf;

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::RuntimeApi(
			RuntimeApiMessage::Request(parent, RuntimeApiRequest::ClaimQueue(tx))
		) if parent == *hash => {
			tx.send(Ok(test_state.claim_queue.clone())).unwrap();
		}
	);

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::RuntimeApi(
			RuntimeApiMessage::Request(parent, RuntimeApiRequest::SessionIndexForChild(tx))
		) if parent == *hash => {
			tx.send(Ok(1)).unwrap();
		}
	);

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::RuntimeApi(
			RuntimeApiMessage::Request(parent, RuntimeApiRequest::SchedulingLookahead(session_index, tx))
		) if parent == *hash && session_index == 1 => {
			tx.send(Ok(DEFAULT_SCHEDULING_LOOKAHEAD)).unwrap();
		}
	);

	// Check that subsystem job issues a request for ancestors.
	// ancestry_len is (lookahead - 1), which determines how many ancestors to fetch.
	let ancestry_len = (DEFAULT_SCHEDULING_LOOKAHEAD - 1) as usize;
	let ancestry_hashes: Vec<Hash> =
		std::iter::successors(Some(*hash), |h| Some(parent_hash_fn(*h)))
			.skip(1)
			.take(ancestry_len)
			.collect();
	let min_relay_parent_number = number.saturating_sub(ancestry_len as u32);
	let ancestry_numbers = (min_relay_parent_number..*number).rev();
	let ancestry_iter = ancestry_hashes.clone().into_iter().zip(ancestry_numbers).peekable();
	if ancestry_len > 0 {
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ChainApi(
				ChainApiMessage::Ancestors{hash: block_hash, k, response_channel: tx}
			) if block_hash == *hash && k == (DEFAULT_SCHEDULING_LOOKAHEAD - 1) as usize => {
				tx.send(Ok(ancestry_hashes.clone())).unwrap();
			}
		);
	}

	let mut used_relay_parents = HashSet::new();
	for (hash, _number) in ancestry_iter {
		if !used_relay_parents.contains(&hash) {
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::RuntimeApi(
					RuntimeApiMessage::Request(parent, RuntimeApiRequest::SessionIndexForChild(tx))
				) if parent == hash => {
					tx.send(Ok(1)).unwrap();
				}
			);
			used_relay_parents.insert(hash);
		}
	}

	let paras: HashSet<_> = test_state.claim_queue.values().flatten().collect();

	// We expect two messages per parachain block.
	for _ in 0..paras.len() * 2 {
		let message = virtual_overseer.recv().await;
		let para_id = match message {
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				parent,
				RuntimeApiRequest::BackingConstraints(p_id, tx),
			)) if parent == *hash => {
				let PerParaData { head_data, pending_availability: _ } = leaf.para_data(p_id);

				let min_relay_parent_number = test_state
					.min_relay_parent_number_override
					.unwrap_or_else(|| number.saturating_sub(ancestry_len as u32));

				let constraints = dummy_constraints(
					min_relay_parent_number,
					vec![*number],
					head_data.clone(),
					test_state.validation_code_hash,
				);

				tx.send(Ok(Some(constraints))).unwrap();
				None
			},
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				parent,
				RuntimeApiRequest::CandidatesPendingAvailability(p_id, tx),
			)) if parent == *hash => {
				tx.send(Ok(leaf
					.para_data(p_id)
					.pending_availability
					.clone()
					.into_iter()
					.map(|c| CommittedCandidateReceipt {
						descriptor: c.descriptor,
						commitments: c.commitments,
					})
					.collect()))
					.unwrap();
				Some(p_id)
			},
			_ => panic!("received unexpected message {:?}", message),
		};

		if let Some(para_id) = para_id {
			for pending in leaf.para_data(para_id).pending_availability.clone() {
				// Production calls `fetch_relay_parent_info_cached`: on a cache hit no runtime API
				// request is sent, so we must not call `recv`. The tracker mirrors which relay
				// parents have already been cached by earlier operations (including prior pending
				// availability candidates within this same activation).
				if test_state
					.cached_relay_parents
					.borrow()
					.contains(&pending.descriptor.relay_parent())
				{
					continue;
				}
				handle_fetch_relay_parent_info(
					virtual_overseer,
					test_state,
					pending.descriptor.relay_parent(),
					pending.relay_parent_number,
				)
				.await;
			}
		}
	}
}

async fn deactivate_leaf(virtual_overseer: &mut VirtualOverseer, hash: Hash) {
	virtual_overseer
		.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::stop_work(
			hash,
		))))
		.await;
}

// The subsystem verifies the relay parent once per entry in `per_scheduling_parent` during
// candidate introduction and hypothetical membership checks. We don't know the count ahead of
// time, so this helper races the response channel against overseer messages: whenever a
// verification runtime-api request arrives we handle it; whenever the response arrives we
// return.
async fn handle_potential_relay_parent_info_calls<T>(
	virtual_overseer: &mut VirtualOverseer,
	rx: oneshot::Receiver<T>,
	test_state: &TestState,
	relay_parent: Hash,
	relay_parent_number: BlockNumber,
) -> T {
	let mut rx = rx.fuse();
	loop {
		futures::select! {
			response = &mut rx => {
				return response.expect("oneshot sender dropped unexpectedly");
			}
			msg = virtual_overseer.recv().fuse() => {
				handle_fetch_relay_parent_info_message(
					virtual_overseer,
					msg,
					test_state,
					relay_parent,
					relay_parent_number,
				)
				.await;
			}
		}
	}
}

async fn introduce_seconded_candidate(
	virtual_overseer: &mut VirtualOverseer,
	test_state: &TestState,
	candidate: CommittedCandidateReceipt,
	pvd: PersistedValidationData,
) {
	let relay_parent = candidate.descriptor.relay_parent();
	let relay_parent_number = pvd.relay_parent_number;
	let req = IntroduceSecondedCandidateRequest {
		candidate_para: candidate.descriptor.para_id(),
		candidate_receipt: candidate,
		persisted_validation_data: pvd,
	};
	let (tx, rx) = oneshot::channel();
	virtual_overseer
		.send(overseer::FromOrchestra::Communication {
			msg: ProspectiveParachainsMessage::IntroduceSecondedCandidate(req, tx),
		})
		.await;
	let response = handle_potential_relay_parent_info_calls(
		virtual_overseer,
		rx,
		test_state,
		relay_parent,
		relay_parent_number,
	)
	.await;
	assert!(response);
}

async fn introduce_seconded_candidate_failed(
	virtual_overseer: &mut VirtualOverseer,
	test_state: &TestState,
	candidate: CommittedCandidateReceipt,
	pvd: PersistedValidationData,
) {
	let relay_parent = candidate.descriptor.relay_parent();
	let relay_parent_number = pvd.relay_parent_number;
	let req = IntroduceSecondedCandidateRequest {
		candidate_para: candidate.descriptor.para_id(),
		candidate_receipt: candidate,
		persisted_validation_data: pvd,
	};
	let (tx, rx) = oneshot::channel();
	virtual_overseer
		.send(overseer::FromOrchestra::Communication {
			msg: ProspectiveParachainsMessage::IntroduceSecondedCandidate(req, tx),
		})
		.await;
	let response = handle_potential_relay_parent_info_calls(
		virtual_overseer,
		rx,
		test_state,
		relay_parent,
		relay_parent_number,
	)
	.await;
	assert!(!response);
}

async fn back_candidate(
	virtual_overseer: &mut VirtualOverseer,
	candidate: &CommittedCandidateReceipt,
	candidate_hash: CandidateHash,
) {
	virtual_overseer
		.send(overseer::FromOrchestra::Communication {
			msg: ProspectiveParachainsMessage::CandidateBacked(
				candidate.descriptor.para_id(),
				candidate_hash,
			),
		})
		.await;
}

async fn get_backable_candidates(
	virtual_overseer: &mut VirtualOverseer,
	leaf: &TestLeaf,
	para_id: ParaId,
	ancestors: Ancestors,
	count: u32,
	expected_result: Vec<BackableCandidateRef>,
) {
	let (tx, rx) = oneshot::channel();
	virtual_overseer
		.send(overseer::FromOrchestra::Communication {
			msg: ProspectiveParachainsMessage::GetBackableCandidates {
				leaf: leaf.hash,
				para_id,
				count,
				ancestors,
				sender: tx,
			},
		})
		.await;
	let resp = rx.await.unwrap();
	assert_eq!(resp, expected_result);
}

async fn get_hypothetical_membership(
	virtual_overseer: &mut VirtualOverseer,
	test_state: &TestState,
	candidate_hash: CandidateHash,
	receipt: CommittedCandidateReceipt,
	persisted_validation_data: PersistedValidationData,
	expected_membership: Vec<Hash>,
) {
	let relay_parent = receipt.descriptor.relay_parent();
	let relay_parent_number = persisted_validation_data.relay_parent_number;
	let hypothetical_candidate = HypotheticalCandidate::Complete {
		candidate_hash,
		receipt: Arc::new(receipt),
		persisted_validation_data,
	};
	let request = HypotheticalMembershipRequest {
		candidates: vec![hypothetical_candidate.clone()],
		fragment_chain_relay_parent: None,
	};
	let (tx, rx) = oneshot::channel();
	virtual_overseer
		.send(overseer::FromOrchestra::Communication {
			msg: ProspectiveParachainsMessage::GetHypotheticalMembership(request, tx),
		})
		.await;
	let mut resp = handle_potential_relay_parent_info_calls(
		virtual_overseer,
		rx,
		test_state,
		relay_parent,
		relay_parent_number,
	)
	.await;
	assert_eq!(resp.len(), 1);
	let (candidate, membership) = resp.remove(0);
	assert_eq!(candidate, hypothetical_candidate);
	assert_eq!(
		membership.into_iter().collect::<HashSet<_>>(),
		expected_membership.into_iter().collect::<HashSet<_>>()
	);
}

async fn get_pvd(
	virtual_overseer: &mut VirtualOverseer,
	test_state: &TestState,
	para_id: ParaId,
	candidate_relay_parent: Hash,
	parent_head_data: HeadData,
	expected_pvd: Option<PersistedValidationData>,
) {
	let request = ProspectiveValidationDataRequest {
		para_id,
		candidate_relay_parent,
		session_index: 1,
		parent_head_data: ParentHeadData::OnlyHash(parent_head_data.hash()),
	};
	let (tx, rx) = oneshot::channel();
	virtual_overseer
		.send(overseer::FromOrchestra::Communication {
			msg: ProspectiveParachainsMessage::GetProspectiveValidationData(request, tx),
		})
		.await;

	// answer_prospective_validation_data_request calls fetch_relay_parent_info_cached. On a
	// cache hit, no runtime API message is sent. `handle_potential_relay_parent_info_calls` races
	// the response channel against incoming messages so we handle any verification query that does
	// fire and return as soon as the response arrives.
	let resp = handle_potential_relay_parent_info_calls(
		virtual_overseer,
		rx,
		test_state,
		candidate_relay_parent,
		expected_pvd.as_ref().map_or(0, |p| p.relay_parent_number),
	)
	.await;
	assert_eq!(resp, expected_pvd);
}

macro_rules! make_and_back_candidate {
	($test_state:ident, $virtual_overseer:ident, $leaf:ident, $parent:expr, $index:expr) => {{
		let (mut candidate, pvd) = make_candidate(
			$leaf.hash,
			$leaf.number,
			1.into(),
			$parent.commitments.head_data.clone(),
			HeadData(vec![$index]),
			$test_state.validation_code_hash,
		);
		// Set a field to make this candidate unique.
		candidate.descriptor.set_para_head(Hash::from_low_u64_le($index));
		let candidate_hash = candidate.hash();
		introduce_seconded_candidate(&mut $virtual_overseer, &$test_state, candidate.clone(), pvd)
			.await;
		back_candidate(&mut $virtual_overseer, &candidate, candidate_hash).await;

		(candidate, candidate_hash)
	}};
}

// Send some candidates and make sure all are found:
// - Two for the same leaf A (one for parachain 1 and one for parachain 2)
// - One for leaf B on parachain 1
// - One for leaf C on parachain 2
// Also tests a claim queue size larger than 1.
#[test]
fn introduce_candidates_basic() {
	let mut test_state = TestState::default();

	let chain_a = ParaId::from(1);
	let chain_b = ParaId::from(2);
	let mut claim_queue = BTreeMap::new();
	claim_queue.insert(CoreIndex(0), [chain_a, chain_b].into_iter().collect());

	test_state.claim_queue = claim_queue;

	let view = test_harness(|mut virtual_overseer| async move {
		// Leaf A
		let leaf_a = TestLeaf {
			number: 100,
			hash: Hash::from_low_u64_be(130),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};
		// Leaf B
		let leaf_b = TestLeaf {
			number: 101,
			hash: Hash::from_low_u64_be(131),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![3, 4, 5]))),
				(2.into(), PerParaData::new(HeadData(vec![4, 5, 6]))),
			],
		};
		// Leaf C
		let leaf_c = TestLeaf {
			number: 102,
			hash: Hash::from_low_u64_be(132),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![5, 6, 7]))),
				(2.into(), PerParaData::new(HeadData(vec![6, 7, 8]))),
			],
		};

		// Activate leaves.
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;
		activate_leaf(&mut virtual_overseer, &leaf_b, &test_state).await;
		activate_leaf(&mut virtual_overseer, &leaf_c, &test_state).await;

		// Candidate A1
		let (candidate_a1, pvd_a1) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);
		let candidate_hash_a1 = candidate_a1.hash();
		let response_a1 = vec![BackableCandidateRef {
			candidate_hash: candidate_hash_a1,
			scheduling_parent: leaf_a.hash,
		}];

		// Candidate A2
		let (candidate_a2, pvd_a2) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			2.into(),
			HeadData(vec![2, 3, 4]),
			HeadData(vec![2]),
			test_state.validation_code_hash,
		);
		let candidate_hash_a2 = candidate_a2.hash();
		let response_a2 = vec![BackableCandidateRef {
			candidate_hash: candidate_hash_a2,
			scheduling_parent: leaf_a.hash,
		}];

		// Candidate B
		let (candidate_b, pvd_b) = make_candidate(
			leaf_b.hash,
			leaf_b.number,
			1.into(),
			HeadData(vec![3, 4, 5]),
			HeadData(vec![3]),
			test_state.validation_code_hash,
		);
		let candidate_hash_b = candidate_b.hash();
		let response_b = vec![BackableCandidateRef {
			candidate_hash: candidate_hash_b,
			scheduling_parent: leaf_b.hash,
		}];

		// Candidate C
		let (candidate_c, pvd_c) = make_candidate(
			leaf_c.hash,
			leaf_c.number,
			2.into(),
			HeadData(vec![6, 7, 8]),
			HeadData(vec![4]),
			test_state.validation_code_hash,
		);
		let candidate_hash_c = candidate_c.hash();
		let response_c = vec![BackableCandidateRef {
			candidate_hash: candidate_hash_c,
			scheduling_parent: leaf_c.hash,
		}];

		// Introduce candidates.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a1.clone(),
			pvd_a1,
		)
		.await;
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a2.clone(),
			pvd_a2,
		)
		.await;
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_b.clone(),
			pvd_b,
		)
		.await;
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_c.clone(),
			pvd_c,
		)
		.await;

		// Back candidates. Otherwise, we cannot check membership with GetBackableCandidates.
		back_candidate(&mut virtual_overseer, &candidate_a1, candidate_hash_a1).await;
		back_candidate(&mut virtual_overseer, &candidate_a2, candidate_hash_a2).await;
		back_candidate(&mut virtual_overseer, &candidate_b, candidate_hash_b).await;
		back_candidate(&mut virtual_overseer, &candidate_c, candidate_hash_c).await;

		// Check candidate tree membership.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::default(),
			5,
			response_a1,
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			2.into(),
			Ancestors::default(),
			5,
			response_a2,
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_b,
			1.into(),
			Ancestors::default(),
			5,
			response_b,
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_c,
			2.into(),
			Ancestors::default(),
			5,
			response_c,
		)
		.await;

		// Check membership on other leaves.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_b,
			2.into(),
			Ancestors::default(),
			5,
			vec![],
		)
		.await;

		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_c,
			1.into(),
			Ancestors::default(),
			5,
			vec![],
		)
		.await;

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 3);
}

// Check if candidates are not backed if they fail constraint checks
#[test]
fn introduce_candidates_error() {
	let mut test_state = TestState::default();
	test_state.claim_queue.insert(
		CoreIndex(2),
		std::iter::repeat(1.into()).take(DEFAULT_SCHEDULING_LOOKAHEAD as _).collect(),
	);

	let view = test_harness(|mut virtual_overseer| async move {
		// Leaf A
		let leaf_a = TestLeaf {
			number: 100,
			hash: Default::default(),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};

		// Activate leaves.
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;

		// Candidate A.
		let (candidate_a, pvd_a) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);

		// Candidate B.
		let (candidate_b, pvd_b) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1]),
			HeadData(vec![1; 20480]),
			test_state.validation_code_hash,
		);

		// Candidate C commits to oversized head data.
		let (candidate_c, pvd_c) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1; 20480]),
			HeadData(vec![0; 20485]),
			test_state.validation_code_hash,
		);

		// Get hypothetical membership of candidates before adding candidate A.
		// Candidate A can be added directly, candidates B and C are potential candidates.
		for (candidate, pvd) in
			[(candidate_a.clone(), pvd_a.clone()), (candidate_b.clone(), pvd_b.clone())]
		{
			get_hypothetical_membership(
				&mut virtual_overseer,
				&test_state,
				candidate.hash(),
				candidate,
				pvd,
				vec![leaf_a.hash],
			)
			.await;
		}

		// Fails constraints check
		get_hypothetical_membership(
			&mut virtual_overseer,
			&test_state,
			candidate_c.hash(),
			candidate_c.clone(),
			pvd_c.clone(),
			Vec::new(),
		)
		.await;

		// Add candidates
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a.clone(),
			pvd_a.clone(),
		)
		.await;
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_b.clone(),
			pvd_b.clone(),
		)
		.await;
		// Fails constraints check
		introduce_seconded_candidate_failed(
			&mut virtual_overseer,
			&test_state,
			candidate_c.clone(),
			pvd_c.clone(),
		)
		.await;

		back_candidate(&mut virtual_overseer, &candidate_a, candidate_a.hash()).await;
		back_candidate(&mut virtual_overseer, &candidate_b, candidate_b.hash()).await;
		// This one will not be backed.
		back_candidate(&mut virtual_overseer, &candidate_c, candidate_c.hash()).await;

		// Expect only A and B to be backable
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::default(),
			5,
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_a.hash(),
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_b.hash(),
					scheduling_parent: leaf_a.hash,
				},
			],
		)
		.await;
		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 1);
}

#[test]
fn introduce_candidate_multiple_times() {
	let test_state = TestState::default();
	let view = test_harness(|mut virtual_overseer| async move {
		// Leaf A
		let leaf_a = TestLeaf {
			number: 100,
			hash: Hash::from_low_u64_be(130),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};
		// Activate leaves.
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;

		// Candidate A.
		let (candidate_a, pvd_a) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);
		let candidate_hash_a = candidate_a.hash();
		let response_a = vec![BackableCandidateRef {
			candidate_hash: candidate_hash_a,
			scheduling_parent: leaf_a.hash,
		}];

		// Introduce candidates.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a.clone(),
			pvd_a.clone(),
		)
		.await;

		// Back candidates. Otherwise, we cannot check membership with GetBackableCandidates.
		back_candidate(&mut virtual_overseer, &candidate_a, candidate_hash_a).await;

		// Check candidate tree membership.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::default(),
			5,
			response_a.clone(),
		)
		.await;

		// Introduce the same candidate multiple times. It'll return true but it will only be added
		// once.
		for _ in 0..5 {
			introduce_seconded_candidate(
				&mut virtual_overseer,
				&test_state,
				candidate_a.clone(),
				pvd_a.clone(),
			)
			.await;
		}

		// Check candidate tree membership.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::default(),
			5,
			response_a,
		)
		.await;

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 1);
}

#[test]
fn fragment_chain_chain_length_is_bounded() {
	let mut test_state = TestState::default();
	test_state.claim_queue.insert(
		CoreIndex(2),
		std::iter::repeat(1.into()).take(DEFAULT_SCHEDULING_LOOKAHEAD as _).collect(),
	);
	let view = test_harness(|mut virtual_overseer| async move {
		// Leaf A
		let leaf_a = TestLeaf {
			number: 100,
			hash: Hash::from_low_u64_be(130),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};
		// Activate leaves.
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;

		// Candidates A, B and C form a chain.
		let (candidate_a, pvd_a) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);
		let (candidate_b, pvd_b) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1]),
			HeadData(vec![2]),
			test_state.validation_code_hash,
		);
		let (candidate_c, pvd_c) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![2]),
			HeadData(vec![3]),
			test_state.validation_code_hash,
		);

		// Introduce candidates A and B. Since max depth is 2, only these two will be allowed.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a.clone(),
			pvd_a,
		)
		.await;
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_b.clone(),
			pvd_b,
		)
		.await;

		// Back candidates. Otherwise, we cannot check membership with GetBackableCandidates and
		// they won't be part of the chain.
		back_candidate(&mut virtual_overseer, &candidate_a, candidate_a.hash()).await;
		back_candidate(&mut virtual_overseer, &candidate_b, candidate_b.hash()).await;

		// Check candidate tree membership.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::default(),
			5,
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_a.hash(),
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_b.hash(),
					scheduling_parent: leaf_a.hash,
				},
			],
		)
		.await;

		// Introducing C will not fail. It will be kept as unconnected storage.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_c.clone(),
			pvd_c,
		)
		.await;
		// When being backed, candidate C will be dropped.
		back_candidate(&mut virtual_overseer, &candidate_c, candidate_c.hash()).await;

		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::default(),
			5,
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_a.hash(),
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_b.hash(),
					scheduling_parent: leaf_a.hash,
				},
			],
		)
		.await;

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 1);
}

// Send some candidates, check if the candidate won't be found once its relay parent leaves the
// view.
#[test]
fn introduce_candidate_parent_leaving_view() {
	let test_state = TestState::default();
	let view = test_harness(|mut virtual_overseer| async move {
		// Leaf A
		let leaf_a = TestLeaf {
			number: 100,
			hash: Hash::from_low_u64_be(130),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};
		// Leaf B
		let leaf_b = TestLeaf {
			number: 101,
			hash: Hash::from_low_u64_be(131),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![3, 4, 5]))),
				(2.into(), PerParaData::new(HeadData(vec![4, 5, 6]))),
			],
		};
		// Leaf C
		let leaf_c = TestLeaf {
			number: 102,
			hash: Hash::from_low_u64_be(132),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![5, 6, 7]))),
				(2.into(), PerParaData::new(HeadData(vec![6, 7, 8]))),
			],
		};

		// Activate leaves.
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;
		activate_leaf(&mut virtual_overseer, &leaf_b, &test_state).await;
		activate_leaf(&mut virtual_overseer, &leaf_c, &test_state).await;

		// Candidate A1
		let (candidate_a1, pvd_a1) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);
		let candidate_hash_a1 = candidate_a1.hash();

		// Candidate A2
		let (candidate_a2, pvd_a2) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			2.into(),
			HeadData(vec![2, 3, 4]),
			HeadData(vec![2]),
			test_state.validation_code_hash,
		);
		let candidate_hash_a2 = candidate_a2.hash();

		// Candidate B
		let (candidate_b, pvd_b) = make_candidate(
			leaf_b.hash,
			leaf_b.number,
			1.into(),
			HeadData(vec![3, 4, 5]),
			HeadData(vec![3]),
			test_state.validation_code_hash,
		);
		let candidate_hash_b = candidate_b.hash();
		let response_b = vec![BackableCandidateRef {
			candidate_hash: candidate_hash_b,
			scheduling_parent: leaf_b.hash,
		}];

		// Candidate C
		let (candidate_c, pvd_c) = make_candidate(
			leaf_c.hash,
			leaf_c.number,
			2.into(),
			HeadData(vec![6, 7, 8]),
			HeadData(vec![4]),
			test_state.validation_code_hash,
		);
		let candidate_hash_c = candidate_c.hash();
		let response_c = vec![BackableCandidateRef {
			candidate_hash: candidate_hash_c,
			scheduling_parent: leaf_c.hash,
		}];

		// Introduce candidates.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a1.clone(),
			pvd_a1,
		)
		.await;
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a2.clone(),
			pvd_a2,
		)
		.await;
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_b.clone(),
			pvd_b,
		)
		.await;
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_c.clone(),
			pvd_c,
		)
		.await;

		// Back candidates. Otherwise, we cannot check membership with GetBackableCandidates.
		back_candidate(&mut virtual_overseer, &candidate_a1, candidate_hash_a1).await;
		back_candidate(&mut virtual_overseer, &candidate_a2, candidate_hash_a2).await;
		back_candidate(&mut virtual_overseer, &candidate_b, candidate_hash_b).await;
		back_candidate(&mut virtual_overseer, &candidate_c, candidate_hash_c).await;

		// Deactivate leaf A.
		deactivate_leaf(&mut virtual_overseer, leaf_a.hash).await;

		// Candidates A1 and A2 should be gone. Candidates B and C should remain.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::default(),
			5,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			2.into(),
			Ancestors::default(),
			5,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_b,
			1.into(),
			Ancestors::default(),
			5,
			response_b,
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_c,
			2.into(),
			Ancestors::default(),
			5,
			response_c.clone(),
		)
		.await;

		// Deactivate leaf B.
		deactivate_leaf(&mut virtual_overseer, leaf_b.hash).await;

		// Candidate B should be gone, C should remain.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::default(),
			5,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			2.into(),
			Ancestors::default(),
			5,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_b,
			1.into(),
			Ancestors::default(),
			5,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_c,
			2.into(),
			Ancestors::default(),
			5,
			response_c,
		)
		.await;

		// Deactivate leaf C.
		deactivate_leaf(&mut virtual_overseer, leaf_c.hash).await;

		// Candidate C should be gone.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::default(),
			5,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			2.into(),
			Ancestors::default(),
			5,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_b,
			1.into(),
			Ancestors::default(),
			5,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_c,
			2.into(),
			Ancestors::default(),
			5,
			vec![],
		)
		.await;

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 0);
}

// Introduce a candidate to multiple forks, see how the membership is returned.
#[test]
fn introduce_candidate_on_multiple_forks() {
	let test_state = TestState::default();
	let view = test_harness(|mut virtual_overseer| async move {
		// Leaf B
		let leaf_b = TestLeaf {
			number: 101,
			hash: Hash::from_low_u64_be(131),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![4, 5, 6]))),
			],
		};
		// Leaf A
		let leaf_a = TestLeaf {
			number: 100,
			hash: get_parent_hash(leaf_b.hash),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};

		// Activate leaves.
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;
		activate_leaf(&mut virtual_overseer, &leaf_b, &test_state).await;

		// Candidate built on leaf A.
		let (candidate_a, pvd_a) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);
		let candidate_hash_a = candidate_a.hash();
		let response_a = vec![BackableCandidateRef {
			candidate_hash: candidate_hash_a,
			scheduling_parent: leaf_a.hash,
		}];

		// Introduce candidate. Should be present on leaves B and C.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a.clone(),
			pvd_a,
		)
		.await;
		back_candidate(&mut virtual_overseer, &candidate_a, candidate_hash_a).await;

		// Check candidate tree membership.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::default(),
			5,
			response_a.clone(),
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_b,
			1.into(),
			Ancestors::default(),
			5,
			response_a.clone(),
		)
		.await;

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 2);
}

#[test]
fn unconnected_candidates_become_connected() {
	// This doesn't test all the complicated cases with many unconnected candidates, as it's more
	// extensively tested in the `fragment_chain::tests` module.
	let mut test_state = TestState::default();
	for i in 2..=4 {
		test_state.claim_queue.insert(
			CoreIndex(i),
			std::iter::repeat(1.into()).take(DEFAULT_SCHEDULING_LOOKAHEAD as _).collect(),
		);
	}

	let view = test_harness(|mut virtual_overseer| async move {
		// Leaf A
		let leaf_a = TestLeaf {
			number: 100,
			hash: Hash::from_low_u64_be(130),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};
		// Activate leaves.
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;

		// Candidates A, B, C and D all form a chain, but we'll first introduce A, C and D.
		let (candidate_a, pvd_a) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);
		let (candidate_b, pvd_b) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1]),
			HeadData(vec![2]),
			test_state.validation_code_hash,
		);
		let (candidate_c, pvd_c) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![2]),
			HeadData(vec![3]),
			test_state.validation_code_hash,
		);
		let (candidate_d, pvd_d) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![3]),
			HeadData(vec![4]),
			test_state.validation_code_hash,
		);

		// Introduce candidates A, C and D.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a.clone(),
			pvd_a.clone(),
		)
		.await;
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_c.clone(),
			pvd_c.clone(),
		)
		.await;
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_d.clone(),
			pvd_d.clone(),
		)
		.await;

		// Back candidates. Otherwise, we cannot check membership with GetBackableCandidates.
		back_candidate(&mut virtual_overseer, &candidate_a, candidate_a.hash()).await;
		back_candidate(&mut virtual_overseer, &candidate_c, candidate_c.hash()).await;
		back_candidate(&mut virtual_overseer, &candidate_d, candidate_d.hash()).await;

		// Check candidate tree membership. Only A should be returned.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::default(),
			5,
			vec![BackableCandidateRef {
				candidate_hash: candidate_a.hash(),
				scheduling_parent: leaf_a.hash,
			}],
		)
		.await;

		// Introduce C and check membership. Full chain should be returned.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_b.clone(),
			pvd_b.clone(),
		)
		.await;
		back_candidate(&mut virtual_overseer, &candidate_b, candidate_b.hash()).await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::default(),
			5,
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_a.hash(),
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_b.hash(),
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_c.hash(),
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_d.hash(),
					scheduling_parent: leaf_a.hash,
				},
			],
		)
		.await;

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 1);
}

// Backs some candidates and tests `GetBackableCandidates` when requesting a single candidate.
#[test]
fn check_backable_query_single_candidate() {
	let mut test_state = TestState::default();
	test_state.claim_queue.insert(
		CoreIndex(2),
		std::iter::repeat(1.into()).take(DEFAULT_SCHEDULING_LOOKAHEAD as _).collect(),
	);
	let view = test_harness(|mut virtual_overseer| async move {
		// Leaf A
		let leaf_a = TestLeaf {
			number: 100,
			hash: Hash::from_low_u64_be(130),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};

		// Activate leaves.
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;

		// Candidate A
		let (candidate_a, pvd_a) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);
		let candidate_hash_a = candidate_a.hash();

		// Candidate B
		let (mut candidate_b, pvd_b) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1]),
			HeadData(vec![2]),
			test_state.validation_code_hash,
		);
		// Set a field to make this candidate unique.
		candidate_b.descriptor.set_para_head(Hash::from_low_u64_le(1000));
		let candidate_hash_b = candidate_b.hash();

		// Introduce candidates.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a.clone(),
			pvd_a,
		)
		.await;
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_b.clone(),
			pvd_b,
		)
		.await;

		// Should not get any backable candidates.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			vec![candidate_hash_a].into_iter().collect(),
			1,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			vec![candidate_hash_a].into_iter().collect(),
			0,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::new(),
			0,
			vec![],
		)
		.await;

		// Back candidates.
		back_candidate(&mut virtual_overseer, &candidate_a, candidate_hash_a).await;
		back_candidate(&mut virtual_overseer, &candidate_b, candidate_hash_b).await;

		// Back an unknown candidate. It doesn't return anything but it's ignored. Will not have any
		// effect on the backable candidates.
		back_candidate(&mut virtual_overseer, &candidate_b, CandidateHash(Hash::random())).await;

		// Should not get any backable candidates for the other para.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			2.into(),
			Ancestors::new(),
			1,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			2.into(),
			vec![candidate_hash_a].into_iter().collect(),
			1,
			vec![],
		)
		.await;

		// Get backable candidate.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::new(),
			1,
			vec![BackableCandidateRef {
				candidate_hash: candidate_hash_a,
				scheduling_parent: leaf_a.hash,
			}],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			vec![candidate_hash_a].into_iter().collect(),
			1,
			vec![BackableCandidateRef {
				candidate_hash: candidate_hash_b,
				scheduling_parent: leaf_a.hash,
			}],
		)
		.await;

		// Wrong path
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			vec![candidate_hash_b].into_iter().collect(),
			1,
			vec![BackableCandidateRef {
				candidate_hash: candidate_hash_a,
				scheduling_parent: leaf_a.hash,
			}],
		)
		.await;

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 1);
}

// Backs some candidates and tests `GetBackableCandidates` when requesting a multiple candidates.
#[test]

fn check_backable_query_multiple_candidates() {
	// This doesn't test all the complicated cases with many unconnected candidates, as it's more
	// extensively tested in the `fragment_chain::tests` module.
	let mut test_state = TestState::default();
	// Add three more cores for para A, so that we can get a chain of max length 4
	for i in 2..=4 {
		test_state.claim_queue.insert(
			CoreIndex(i),
			std::iter::repeat(1.into()).take(DEFAULT_SCHEDULING_LOOKAHEAD as _).collect(),
		);
	}

	let view = test_harness(|mut virtual_overseer| async move {
		// Leaf A
		let leaf_a = TestLeaf {
			number: 100,
			hash: Hash::from_low_u64_be(130),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};

		// Activate leaves.
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;

		// Candidate A
		let (candidate_a, pvd_a) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);
		let candidate_hash_a = candidate_a.hash();
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a.clone(),
			pvd_a,
		)
		.await;
		back_candidate(&mut virtual_overseer, &candidate_a, candidate_hash_a).await;

		let (candidate_b, candidate_hash_b) =
			make_and_back_candidate!(test_state, virtual_overseer, leaf_a, &candidate_a, 2);
		let (candidate_c, candidate_hash_c) =
			make_and_back_candidate!(test_state, virtual_overseer, leaf_a, &candidate_b, 3);
		let (_candidate_d, candidate_hash_d) =
			make_and_back_candidate!(test_state, virtual_overseer, leaf_a, &candidate_c, 4);

		// Should not get any backable candidates for the other para.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			2.into(),
			Ancestors::new(),
			1,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			2.into(),
			Ancestors::new(),
			5,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			2.into(),
			vec![candidate_hash_a].into_iter().collect(),
			1,
			vec![],
		)
		.await;

		// Test various scenarios with various counts.

		// empty ancestors
		{
			get_backable_candidates(
				&mut virtual_overseer,
				&leaf_a,
				1.into(),
				Ancestors::new(),
				1,
				vec![BackableCandidateRef {
					candidate_hash: candidate_hash_a,
					scheduling_parent: leaf_a.hash,
				}],
			)
			.await;
			for count in 4..10 {
				get_backable_candidates(
					&mut virtual_overseer,
					&leaf_a,
					1.into(),
					Ancestors::new(),
					count,
					vec![
						BackableCandidateRef {
							candidate_hash: candidate_hash_a,
							scheduling_parent: leaf_a.hash,
						},
						BackableCandidateRef {
							candidate_hash: candidate_hash_b,
							scheduling_parent: leaf_a.hash,
						},
						BackableCandidateRef {
							candidate_hash: candidate_hash_c,
							scheduling_parent: leaf_a.hash,
						},
						BackableCandidateRef {
							candidate_hash: candidate_hash_d,
							scheduling_parent: leaf_a.hash,
						},
					],
				)
				.await;
			}
		}

		// ancestors of size 1
		{
			get_backable_candidates(
				&mut virtual_overseer,
				&leaf_a,
				1.into(),
				vec![candidate_hash_a].into_iter().collect(),
				1,
				vec![BackableCandidateRef {
					candidate_hash: candidate_hash_b,
					scheduling_parent: leaf_a.hash,
				}],
			)
			.await;
			get_backable_candidates(
				&mut virtual_overseer,
				&leaf_a,
				1.into(),
				vec![candidate_hash_a].into_iter().collect(),
				2,
				vec![
					BackableCandidateRef {
						candidate_hash: candidate_hash_b,
						scheduling_parent: leaf_a.hash,
					},
					BackableCandidateRef {
						candidate_hash: candidate_hash_c,
						scheduling_parent: leaf_a.hash,
					},
				],
			)
			.await;

			// If the requested count exceeds the largest chain, return the longest
			// chain we can get.
			for count in 3..10 {
				get_backable_candidates(
					&mut virtual_overseer,
					&leaf_a,
					1.into(),
					vec![candidate_hash_a].into_iter().collect(),
					count,
					vec![
						BackableCandidateRef {
							candidate_hash: candidate_hash_b,
							scheduling_parent: leaf_a.hash,
						},
						BackableCandidateRef {
							candidate_hash: candidate_hash_c,
							scheduling_parent: leaf_a.hash,
						},
						BackableCandidateRef {
							candidate_hash: candidate_hash_d,
							scheduling_parent: leaf_a.hash,
						},
					],
				)
				.await;
			}
		}

		// ancestor count 2 and higher
		{
			get_backable_candidates(
				&mut virtual_overseer,
				&leaf_a,
				1.into(),
				vec![candidate_hash_a, candidate_hash_b, candidate_hash_c].into_iter().collect(),
				1,
				vec![BackableCandidateRef {
					candidate_hash: candidate_hash_d,
					scheduling_parent: leaf_a.hash,
				}],
			)
			.await;

			get_backable_candidates(
				&mut virtual_overseer,
				&leaf_a,
				1.into(),
				vec![candidate_hash_a, candidate_hash_b].into_iter().collect(),
				1,
				vec![BackableCandidateRef {
					candidate_hash: candidate_hash_c,
					scheduling_parent: leaf_a.hash,
				}],
			)
			.await;

			// If the requested count exceeds the largest chain, return the longest
			// chain we can get.
			for count in 3..10 {
				get_backable_candidates(
					&mut virtual_overseer,
					&leaf_a,
					1.into(),
					vec![candidate_hash_a, candidate_hash_b].into_iter().collect(),
					count,
					vec![
						BackableCandidateRef {
							candidate_hash: candidate_hash_c,
							scheduling_parent: leaf_a.hash,
						},
						BackableCandidateRef {
							candidate_hash: candidate_hash_d,
							scheduling_parent: leaf_a.hash,
						},
					],
				)
				.await;
			}
		}

		// No more candidates in the chain.
		for count in 1..4 {
			get_backable_candidates(
				&mut virtual_overseer,
				&leaf_a,
				1.into(),
				vec![candidate_hash_a, candidate_hash_b, candidate_hash_c, candidate_hash_d]
					.into_iter()
					.collect(),
				count,
				vec![],
			)
			.await;
		}

		// Wrong paths.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			vec![candidate_hash_b].into_iter().collect(),
			1,
			vec![BackableCandidateRef {
				candidate_hash: candidate_hash_a,
				scheduling_parent: leaf_a.hash,
			}],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			vec![candidate_hash_b, candidate_hash_c].into_iter().collect(),
			3,
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_hash_a,
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_hash_b,
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_hash_c,
					scheduling_parent: leaf_a.hash,
				},
			],
		)
		.await;

		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			vec![candidate_hash_a, candidate_hash_c, candidate_hash_d].into_iter().collect(),
			2,
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_hash_b,
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_hash_c,
					scheduling_parent: leaf_a.hash,
				},
			],
		)
		.await;

		// Non-existent candidate.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			vec![candidate_hash_a, CandidateHash(Hash::from_low_u64_be(100))]
				.into_iter()
				.collect(),
			2,
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_hash_b,
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_hash_c,
					scheduling_parent: leaf_a.hash,
				},
			],
		)
		.await;

		// Requested count is zero.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			Ancestors::new(),
			0,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			vec![candidate_hash_a].into_iter().collect(),
			0,
			vec![],
		)
		.await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			1.into(),
			vec![candidate_hash_a, candidate_hash_b].into_iter().collect(),
			0,
			vec![],
		)
		.await;

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 1);
}

// Test hypothetical membership query.
#[test]
fn check_hypothetical_membership_query() {
	let test_state = TestState::default();

	let view = test_harness(|mut virtual_overseer| async move {
		// Leaf B
		let leaf_b = TestLeaf {
			number: 101,
			hash: Hash::from_low_u64_be(131),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};
		// Leaf A
		let leaf_a = TestLeaf {
			number: 100,
			hash: get_parent_hash(leaf_b.hash),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};

		// Activate leaves.
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;
		activate_leaf(&mut virtual_overseer, &leaf_b, &test_state).await;

		// Candidates will be valid on both leaves.

		// Candidate A.
		let (candidate_a, pvd_a) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);

		// Candidate B.
		let (candidate_b, pvd_b) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1]),
			HeadData(vec![2]),
			test_state.validation_code_hash,
		);

		// Candidate C.
		let (candidate_c, pvd_c) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![2]),
			HeadData(vec![3]),
			test_state.validation_code_hash,
		);

		// Get hypothetical membership of candidates before adding candidate A.
		// Candidate A can be added directly, candidates B and C are potential candidates.
		for (candidate, pvd) in [
			(candidate_a.clone(), pvd_a.clone()),
			(candidate_b.clone(), pvd_b.clone()),
			(candidate_c.clone(), pvd_c.clone()),
		] {
			get_hypothetical_membership(
				&mut virtual_overseer,
				&test_state,
				candidate.hash(),
				candidate,
				pvd,
				vec![leaf_a.hash, leaf_b.hash],
			)
			.await;
		}

		// Add candidate A.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a.clone(),
			pvd_a.clone(),
		)
		.await;

		// Get membership of candidates after adding A. They all are still unconnected candidates
		// (not part of the backable chain).
		for (candidate, pvd) in [
			(candidate_a.clone(), pvd_a.clone()),
			(candidate_b.clone(), pvd_b.clone()),
			(candidate_c.clone(), pvd_c.clone()),
		] {
			get_hypothetical_membership(
				&mut virtual_overseer,
				&test_state,
				candidate.hash(),
				candidate,
				pvd,
				vec![leaf_a.hash, leaf_b.hash],
			)
			.await;
		}

		// Back A. Now A is part of the chain, the rest can be added as unconnected.

		back_candidate(&mut virtual_overseer, &candidate_a, candidate_a.hash()).await;

		for (candidate, pvd) in [
			(candidate_a.clone(), pvd_a.clone()),
			(candidate_b.clone(), pvd_b.clone()),
			(candidate_c.clone(), pvd_c.clone()),
		] {
			get_hypothetical_membership(
				&mut virtual_overseer,
				&test_state,
				candidate.hash(),
				candidate,
				pvd,
				vec![leaf_a.hash, leaf_b.hash],
			)
			.await;
		}

		// Candidate D has invalid relay parent.
		let (candidate_d, pvd_d) = make_candidate(
			Hash::from_low_u64_be(200),
			leaf_a.number,
			1.into(),
			HeadData(vec![1]),
			HeadData(vec![2]),
			test_state.validation_code_hash,
		);
		introduce_seconded_candidate_failed(&mut virtual_overseer, &test_state, candidate_d, pvd_d)
			.await;

		// Candidate E has invalid head data.
		let (candidate_e, pvd_e) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![2]),
			HeadData(vec![0; 20481]),
			test_state.validation_code_hash,
		);
		introduce_seconded_candidate_failed(&mut virtual_overseer, &test_state, candidate_e, pvd_e)
			.await;

		// Add candidate B and back it.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_b.clone(),
			pvd_b.clone(),
		)
		.await;
		back_candidate(&mut virtual_overseer, &candidate_b, candidate_b.hash()).await;

		// Get membership of candidates after adding B.
		for (candidate, pvd) in [
			(candidate_a.clone(), pvd_a.clone()),
			(candidate_b.clone(), pvd_b.clone()),
			(candidate_c.clone(), pvd_c.clone()),
		] {
			get_hypothetical_membership(
				&mut virtual_overseer,
				&test_state,
				candidate.hash(),
				candidate,
				pvd,
				vec![leaf_a.hash, leaf_b.hash],
			)
			.await;
		}

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 2);
}

#[test]
fn check_pvd_query() {
	// This doesn't test all the complicated cases with many unconnected candidates, as it's more
	// extensively tested in the `fragment_chain::tests` module.
	let test_state = TestState::default();
	let view = test_harness(|mut virtual_overseer| async move {
		// Leaf A
		let leaf_a = TestLeaf {
			number: 100,
			hash: Hash::from_low_u64_be(130),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};

		// Activate leaves.
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;

		// Candidate A.
		let (candidate_a, pvd_a) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);

		// Candidate B.
		let (candidate_b, pvd_b) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![1]),
			HeadData(vec![2]),
			test_state.validation_code_hash,
		);

		// Candidate C.
		let (candidate_c, pvd_c) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![2]),
			HeadData(vec![3]),
			test_state.validation_code_hash,
		);

		// Candidate E.
		let (candidate_e, pvd_e) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			1.into(),
			HeadData(vec![5]),
			HeadData(vec![6]),
			test_state.validation_code_hash,
		);

		// Get pvd of candidate A before adding it.
		get_pvd(
			&mut virtual_overseer,
			&test_state,
			1.into(),
			leaf_a.hash,
			HeadData(vec![1, 2, 3]),
			Some(pvd_a.clone()),
		)
		.await;

		// Add candidate A.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a.clone(),
			pvd_a.clone(),
		)
		.await;
		back_candidate(&mut virtual_overseer, &candidate_a, candidate_a.hash()).await;

		// Get pvd of candidate A after adding it.
		get_pvd(
			&mut virtual_overseer,
			&test_state,
			1.into(),
			leaf_a.hash,
			HeadData(vec![1, 2, 3]),
			Some(pvd_a.clone()),
		)
		.await;

		// Get pvd of candidate B before adding it.
		get_pvd(
			&mut virtual_overseer,
			&test_state,
			1.into(),
			leaf_a.hash,
			HeadData(vec![1]),
			Some(pvd_b.clone()),
		)
		.await;

		// Add candidate B.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_b,
			pvd_b.clone(),
		)
		.await;

		// Get pvd of candidate B after adding it.
		get_pvd(
			&mut virtual_overseer,
			&test_state,
			1.into(),
			leaf_a.hash,
			HeadData(vec![1]),
			Some(pvd_b.clone()),
		)
		.await;

		// Get pvd of candidate C before adding it.
		get_pvd(
			&mut virtual_overseer,
			&test_state,
			1.into(),
			leaf_a.hash,
			HeadData(vec![2]),
			Some(pvd_c.clone()),
		)
		.await;

		// Add candidate C.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_c,
			pvd_c.clone(),
		)
		.await;

		// Get pvd of candidate C after adding it.
		get_pvd(
			&mut virtual_overseer,
			&test_state,
			1.into(),
			leaf_a.hash,
			HeadData(vec![2]),
			Some(pvd_c),
		)
		.await;

		// Get pvd of candidate E before adding it. It won't be found, as we don't have its parent.
		get_pvd(&mut virtual_overseer, &test_state, 1.into(), leaf_a.hash, HeadData(vec![5]), None)
			.await;

		// Add candidate E and check again. Should succeed this time.
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_e,
			pvd_e.clone(),
		)
		.await;

		get_pvd(
			&mut virtual_overseer,
			&test_state,
			1.into(),
			leaf_a.hash,
			HeadData(vec![5]),
			Some(pvd_e),
		)
		.await;

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 1);
}

// Test simultaneously activating and deactivating leaves, and simultaneously deactivating
// multiple leaves.
#[test]
fn correctly_updates_leaves() {
	let test_state = TestState::default();

	let view = test_harness(|mut virtual_overseer| async move {
		// Leaf A
		let leaf_a = TestLeaf {
			number: 100,
			hash: Hash::from_low_u64_be(130),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};
		// Leaf B
		let leaf_b = TestLeaf {
			number: 101,
			hash: Hash::from_low_u64_be(131),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![3, 4, 5]))),
				(2.into(), PerParaData::new(HeadData(vec![4, 5, 6]))),
			],
		};
		// Leaf C
		let leaf_c = TestLeaf {
			number: 102,
			hash: Hash::from_low_u64_be(132),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![5, 6, 7]))),
				(2.into(), PerParaData::new(HeadData(vec![6, 7, 8]))),
			],
		};

		// Activate leaves.
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;

		activate_leaf(&mut virtual_overseer, &leaf_b, &test_state).await;

		// Try activating a duplicate leaf.
		activate_leaf(&mut virtual_overseer, &leaf_b, &test_state).await;

		// Pass in an empty update.
		let update = ActiveLeavesUpdate::default();
		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(update)))
			.await;

		// Activate a leaf and remove one at the same time.
		let activated = new_leaf(leaf_c.hash, leaf_c.number);
		let update = ActiveLeavesUpdate {
			activated: Some(activated),
			deactivated: [leaf_b.hash][..].into(),
		};
		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(update)))
			.await;
		handle_leaf_activation(&mut virtual_overseer, &leaf_c, &test_state, get_parent_hash).await;

		// Remove all remaining leaves.
		let update = ActiveLeavesUpdate {
			deactivated: [leaf_a.hash, leaf_c.hash][..].into(),
			..Default::default()
		};
		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(update)))
			.await;

		// Activate and deactivate the same leaf.
		let activated = new_leaf(leaf_a.hash, leaf_a.number);
		let update = ActiveLeavesUpdate {
			activated: Some(activated),
			deactivated: [leaf_a.hash][..].into(),
		};
		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(update)))
			.await;

		// Remove the leaf again. Send some unnecessary hashes.
		let update = ActiveLeavesUpdate {
			deactivated: [leaf_a.hash, leaf_b.hash, leaf_c.hash][..].into(),
			..Default::default()
		};
		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(update)))
			.await;

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 0);
}

#[test]
fn handle_active_leaves_update_gets_candidates_from_parent() {
	let para_id = ParaId::from(1);

	// This doesn't test all the complicated cases with many unconnected candidates, as it's more
	// extensively tested in the `fragment_chain::tests` module.
	let mut test_state = TestState::default();

	test_state.claim_queue = BTreeMap::new();
	for i in 0..=4 {
		test_state.claim_queue.insert(
			CoreIndex(i),
			std::iter::repeat(para_id).take(DEFAULT_SCHEDULING_LOOKAHEAD as _).collect(),
		);
	}

	let view = test_harness(|mut virtual_overseer| async move {
		// Leaf A
		let leaf_a = TestLeaf {
			number: 100,
			hash: Hash::from_low_u64_be(130),
			para_data: vec![(para_id, PerParaData::new(HeadData(vec![1, 2, 3])))],
		};
		// Activate leaf A.
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;

		// Candidates A, B, C and D all form a chain
		let (candidate_a, pvd_a) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			para_id,
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);
		let candidate_hash_a = candidate_a.hash();
		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a.clone(),
			pvd_a,
		)
		.await;
		back_candidate(&mut virtual_overseer, &candidate_a, candidate_hash_a).await;

		let (candidate_b, candidate_hash_b) =
			make_and_back_candidate!(test_state, virtual_overseer, leaf_a, &candidate_a, 2);
		let (candidate_c, candidate_hash_c) =
			make_and_back_candidate!(test_state, virtual_overseer, leaf_a, &candidate_b, 3);
		let (candidate_d, candidate_hash_d) =
			make_and_back_candidate!(test_state, virtual_overseer, leaf_a, &candidate_c, 4);

		let mut all_candidates_resp = vec![
			BackableCandidateRef {
				candidate_hash: candidate_hash_a,
				scheduling_parent: leaf_a.hash,
			},
			BackableCandidateRef {
				candidate_hash: candidate_hash_b,
				scheduling_parent: leaf_a.hash,
			},
			BackableCandidateRef {
				candidate_hash: candidate_hash_c,
				scheduling_parent: leaf_a.hash,
			},
			BackableCandidateRef {
				candidate_hash: candidate_hash_d,
				scheduling_parent: leaf_a.hash,
			},
		];

		// Check candidate tree membership.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			para_id,
			Ancestors::default(),
			5,
			all_candidates_resp.clone(),
		)
		.await;

		// Activate leaf B, which makes candidates A and B pending availability.
		// Leaf B
		let leaf_b = TestLeaf {
			number: 101,
			hash: Hash::from_low_u64_be(129),
			para_data: vec![(
				para_id,
				PerParaData::new_with_pending(
					HeadData(vec![1, 2, 3]),
					vec![
						CandidatePendingAvailability {
							candidate_hash: candidate_a.hash(),
							descriptor: candidate_a.descriptor.clone(),
							commitments: candidate_a.commitments.clone(),
							relay_parent_number: leaf_a.number,
							max_pov_size: MAX_POV_SIZE,
						},
						CandidatePendingAvailability {
							candidate_hash: candidate_b.hash(),
							descriptor: candidate_b.descriptor.clone(),
							commitments: candidate_b.commitments.clone(),
							relay_parent_number: leaf_a.number,
							max_pov_size: MAX_POV_SIZE,
						},
					],
				),
			)],
		};
		// Activate leaf B.
		activate_leaf(&mut virtual_overseer, &leaf_b, &test_state).await;
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_b,
			para_id,
			Ancestors::default(),
			5,
			vec![],
		)
		.await;

		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_b,
			para_id,
			[candidate_a.hash(), candidate_b.hash()].into_iter().collect(),
			5,
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_c.hash(),
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_d.hash(),
					scheduling_parent: leaf_a.hash,
				},
			],
		)
		.await;

		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_b,
			para_id,
			Ancestors::default(),
			5,
			vec![],
		)
		.await;

		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			para_id,
			Ancestors::default(),
			5,
			all_candidates_resp.clone(),
		)
		.await;

		// Now deactivate leaf A.
		deactivate_leaf(&mut virtual_overseer, leaf_a.hash).await;

		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_b,
			para_id,
			Ancestors::default(),
			5,
			vec![],
		)
		.await;

		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_b,
			para_id,
			[candidate_a.hash(), candidate_b.hash()].into_iter().collect(),
			5,
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_c.hash(),
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_d.hash(),
					scheduling_parent: leaf_a.hash,
				},
			],
		)
		.await;

		// Now add leaf C, which will be a sibling (fork) of leaf B. It should also inherit the
		// candidates of leaf A (their common parent).
		let leaf_c = TestLeaf {
			number: 101,
			hash: Hash::from_low_u64_be(12),
			para_data: vec![(
				para_id,
				PerParaData::new_with_pending(HeadData(vec![1, 2, 3]), vec![]),
			)],
		};

		activate_leaf_with_parent_hash_fn(&mut virtual_overseer, &leaf_c, &test_state, |hash| {
			if hash == leaf_c.hash {
				leaf_a.hash
			} else {
				get_parent_hash(hash)
			}
		})
		.await;

		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_b,
			para_id,
			[candidate_a.hash(), candidate_b.hash()].into_iter().collect(),
			5,
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_c.hash(),
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_d.hash(),
					scheduling_parent: leaf_a.hash,
				},
			],
		)
		.await;

		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_c,
			para_id,
			Ancestors::new(),
			5,
			all_candidates_resp.clone(),
		)
		.await;

		// Deactivate C and add another candidate that will be present on the deactivated parent A.
		// When activating C again it should also get the new candidate. Deactivated leaves are
		// still updated with new candidates.
		deactivate_leaf(&mut virtual_overseer, leaf_c.hash).await;

		let (candidate_e, _) =
			make_and_back_candidate!(test_state, virtual_overseer, leaf_a, &candidate_d, 5);
		activate_leaf_with_parent_hash_fn(&mut virtual_overseer, &leaf_c, &test_state, |hash| {
			if hash == leaf_c.hash {
				leaf_a.hash
			} else {
				get_parent_hash(hash)
			}
		})
		.await;

		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_b,
			para_id,
			[candidate_a.hash(), candidate_b.hash()].into_iter().collect(),
			5,
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_c.hash(),
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_d.hash(),
					scheduling_parent: leaf_a.hash,
				},
				BackableCandidateRef {
					candidate_hash: candidate_e.hash(),
					scheduling_parent: leaf_a.hash,
				},
			],
		)
		.await;

		all_candidates_resp.push(BackableCandidateRef {
			candidate_hash: candidate_e.hash(),
			scheduling_parent: leaf_a.hash,
		});
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_c,
			para_id,
			Ancestors::new(),
			5,
			all_candidates_resp,
		)
		.await;

		// Querying the backable candidates for deactivated leaf won't work.
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			para_id,
			Ancestors::new(),
			5,
			vec![],
		)
		.await;

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 2);
	assert_eq!(view.per_scheduling_parent.len(), 3);
}

#[test]
fn handle_active_leaves_update_bounded_implicit_view() {
	let para_id = ParaId::from(1);
	let mut test_state = TestState::default();
	test_state.claim_queue = test_state
		.claim_queue
		.into_iter()
		.filter(|(_, paras)| matches!(paras.front(), Some(para) if para == &para_id))
		.collect();
	assert_eq!(test_state.claim_queue.len(), 1);

	let scheduling_lookahead = DEFAULT_SCHEDULING_LOOKAHEAD;

	let mut leaves = vec![TestLeaf {
		number: 100,
		hash: Hash::from_low_u64_be(130),
		para_data: vec![(para_id, PerParaData::new(HeadData(vec![1, 2, 3])))],
	}];

	for index in 1..10 {
		let prev_leaf = &leaves[index - 1];
		leaves.push(TestLeaf {
			number: prev_leaf.number - 1,
			hash: get_parent_hash(prev_leaf.hash),
			para_data: vec![(para_id, PerParaData::new(HeadData(vec![1, 2, 3])))],
		});
	}
	leaves.reverse();

	let view = test_harness(|mut virtual_overseer| async {
		// Activate first 10 leaves.
		for leaf in &leaves[0..10] {
			activate_leaf(&mut virtual_overseer, leaf, &test_state).await;
		}

		// Now deactivate first 9 leaves.
		for leaf in &leaves[0..9] {
			deactivate_leaf(&mut virtual_overseer, leaf.hash).await;
		}

		virtual_overseer
	});

	// Only latest leaf is active.
	assert_eq!(view.active_leaves.len(), 1);
	// We keep scheduling_lookahead - 1 implicit leaves. The latest leaf is also present here.
	assert_eq!(view.per_scheduling_parent.len() as u32, scheduling_lookahead);

	assert_eq!(view.active_leaves, [leaves[9].hash].into_iter().collect());
	assert_eq!(
		view.per_scheduling_parent.into_keys().collect::<HashSet<_>>(),
		leaves[7..].into_iter().map(|l| l.hash).collect::<HashSet<_>>()
	);
}

#[test]
fn persists_pending_availability_candidate() {
	// This doesn't test all the complicated cases with many unconnected candidates, as it's more
	// extensively tested in the `fragment_chain::tests` module.
	let mut test_state = TestState::default();
	let para_id = ParaId::from(1);
	test_state.claim_queue = test_state
		.claim_queue
		.into_iter()
		.filter(|(_, paras)| matches!(paras.front(), Some(para) if para == &para_id))
		.collect();
	assert_eq!(test_state.claim_queue.len(), 1);

	test_harness(|mut virtual_overseer| async move {
		let para_head = HeadData(vec![1, 2, 3]);

		// Min allowed relay parent for leaf `a` which goes out of scope in the test.
		// Block 97 will have hash 0x04 given the ancestry chain from leaf_a (hash 0x02).
		let candidate_relay_parent = Hash::from_low_u64_be(4);
		let candidate_relay_parent_number = 97;

		let leaf_a = TestLeaf {
			number: candidate_relay_parent_number + (DEFAULT_SCHEDULING_LOOKAHEAD - 1),
			hash: Hash::from_low_u64_be(2),
			para_data: vec![(para_id, PerParaData::new(para_head.clone()))],
		};

		let leaf_b_hash = Hash::from_low_u64_be(1);
		let leaf_b_number = leaf_a.number + 1;

		// Activate leaf.
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;

		// Candidate A
		let (candidate_a, pvd_a) = make_candidate(
			candidate_relay_parent,
			candidate_relay_parent_number,
			para_id,
			para_head.clone(),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);
		let candidate_hash_a = candidate_a.hash();

		// Candidate B, built on top of the candidate which is out of scope but pending
		// availability.
		let (candidate_b, pvd_b) = make_candidate(
			leaf_b_hash,
			leaf_b_number,
			para_id,
			HeadData(vec![1]),
			HeadData(vec![2]),
			test_state.validation_code_hash,
		);
		let candidate_hash_b = candidate_b.hash();

		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a.clone(),
			pvd_a.clone(),
		)
		.await;
		back_candidate(&mut virtual_overseer, &candidate_a, candidate_hash_a).await;

		let candidate_a_pending_av = CandidatePendingAvailability {
			candidate_hash: candidate_hash_a,
			descriptor: candidate_a.descriptor.clone(),
			commitments: candidate_a.commitments.clone(),
			relay_parent_number: candidate_relay_parent_number,
			max_pov_size: MAX_POV_SIZE,
		};
		let leaf_b = TestLeaf {
			number: leaf_b_number,
			hash: leaf_b_hash,
			para_data: vec![(
				1.into(),
				PerParaData::new_with_pending(para_head.clone(), vec![candidate_a_pending_av]),
			)],
		};
		activate_leaf(&mut virtual_overseer, &leaf_b, &test_state).await;

		get_hypothetical_membership(
			&mut virtual_overseer,
			&test_state,
			candidate_hash_a,
			candidate_a,
			pvd_a,
			vec![leaf_a.hash, leaf_b.hash],
		)
		.await;

		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_b.clone(),
			pvd_b,
		)
		.await;
		back_candidate(&mut virtual_overseer, &candidate_b, candidate_hash_b).await;

		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_b,
			para_id,
			vec![candidate_hash_a].into_iter().collect(),
			1,
			vec![BackableCandidateRef {
				candidate_hash: candidate_hash_b,
				scheduling_parent: leaf_b_hash,
			}],
		)
		.await;

		virtual_overseer
	});
}

#[test]
fn uses_ancestry_only_within_session() {
	test_harness(|mut virtual_overseer| async move {
		let number = 5;
		let hash = Hash::repeat_byte(5);
		let scheduling_lookahead = DEFAULT_SCHEDULING_LOOKAHEAD;
		let session = 2;

		let ancestry_hashes =
			vec![Hash::repeat_byte(4), Hash::repeat_byte(3), Hash::repeat_byte(2)];
		let session_change_hash = Hash::repeat_byte(3);

		let activated = new_leaf(hash, number);

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::start_work(activated),
			)))
			.await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::ClaimQueue(tx))
			) if parent == hash => {
				tx.send(Ok(BTreeMap::new())).unwrap();
			}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::SessionIndexForChild(tx))
			) if parent == hash => {
				tx.send(Ok(session)).unwrap();
			}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::SchedulingLookahead(session_index, tx))
			) if parent == hash && session_index == session => {
				tx.send(Ok(scheduling_lookahead)).unwrap();
			}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ChainApi(
				ChainApiMessage::Ancestors{hash: block_hash, k, response_channel: tx}
			) if block_hash == hash && k == (scheduling_lookahead - 1) as usize => {
				tx.send(Ok(ancestry_hashes.clone())).unwrap();
			}
		);

		for (_i, hash) in ancestry_hashes.into_iter().enumerate() {
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::RuntimeApi(
					RuntimeApiMessage::Request(parent, RuntimeApiRequest::SessionIndexForChild(tx))
				) if parent == hash => {
					if hash == session_change_hash {
						tx.send(Ok(session - 1)).unwrap();
						break
					} else {
						tx.send(Ok(session)).unwrap();
					}
				}
			);
		}

		virtual_overseer
	});
}

// Test that a V3 candidate whose `relay_parent` is MUCH OLDER than its `scheduling_parent`
// (older than the scheduling lookahead — out of the scheduling scope, but within
// `min_relay_parent_number` allowed by the runtime) is accepted end-to-end (introduced, backed,
// and returned as backable). This is the core new behavior enabled by this PR.
#[test]
fn introduce_v3_candidate_with_older_relay_parent() {
	const LEAF_NUMBER: BlockNumber = 100;
	// Pick a relay parent far older than the scheduling lookahead so this exercises the
	// behavior that wasn't possible pre-PR.
	const OLDER_RELAY_PARENT_NUMBER: BlockNumber = LEAF_NUMBER - 4 * DEFAULT_SCHEDULING_LOOKAHEAD;

	let para_id = ParaId::from(1);
	let mut test_state = TestState::default();
	// Configure the runtime to allow relay parents down to the much older block.
	test_state.set_min_relay_parent_number(OLDER_RELAY_PARENT_NUMBER);

	let view = test_harness(|mut virtual_overseer| async move {
		let leaf_a = TestLeaf {
			number: LEAF_NUMBER,
			hash: Hash::from_low_u64_be(130),
			para_data: vec![
				(para_id, PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;

		// Relay parent well outside the scheduling scope (scope is
		// `[LEAF_NUMBER - (lookahead-1), LEAF_NUMBER]`).
		let older_relay_parent = Hash::from_low_u64_be(9999);

		// V3 candidate: relay_parent = older block, scheduling_parent = leaf_a.
		let (candidate_a, pvd_a) = make_candidate_v3(
			older_relay_parent,
			OLDER_RELAY_PARENT_NUMBER,
			leaf_a.hash,
			para_id,
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);
		let candidate_hash_a = candidate_a.hash();

		// Sanity: confirm the descriptor is V3 and parents are decoupled.
		assert_eq!(candidate_a.descriptor.relay_parent(), older_relay_parent);
		assert_eq!(candidate_a.descriptor.scheduling_parent(), leaf_a.hash);

		introduce_seconded_candidate(
			&mut virtual_overseer,
			&test_state,
			candidate_a.clone(),
			pvd_a.clone(),
		)
		.await;
		back_candidate(&mut virtual_overseer, &candidate_a, candidate_hash_a).await;

		// Verify it's returned as backable anchored at the scheduling parent (leaf_a.hash).
		get_backable_candidates(
			&mut virtual_overseer,
			&leaf_a,
			para_id,
			Ancestors::default(),
			1,
			vec![BackableCandidateRef {
				candidate_hash: candidate_hash_a,
				scheduling_parent: leaf_a.hash,
			}],
		)
		.await;

		// And that hypothetical membership treats the leaf as a valid anchor.
		get_hypothetical_membership(
			&mut virtual_overseer,
			&test_state,
			candidate_hash_a,
			candidate_a,
			pvd_a,
			vec![leaf_a.hash],
		)
		.await;

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 1);
}

// Test that `GetProspectiveValidationData` for a candidate with a MUCH OLDER `relay_parent`
// (older than the scheduling lookahead) returns a PVD that uses the old relay parent's
// number/storage_root, but the SCHEDULING session's `max_pov_size`. This locks in the
// load-bearing assumption documented in `answer_prospective_validation_data_request`.
//
// Parameterized over `runtime_api_version`:
// - `case_1` uses the default runtime version (< `ANCESTOR_RELAY_PARENT_INFO_RUNTIME_REQUIREMENT`),
//   exercising the chain-header fallback path.
// - `case_2` uses a runtime version that supports the `AncestorRelayParentInfo` API, exercising the
//   runtime API path — the production scenario for relay parents beyond recent chain storage
//   (potentially from a previous session).
#[rstest]
#[case(RuntimeApiRequest::CONSTRAINTS_RUNTIME_REQUIREMENT)]
#[case(RuntimeApiRequest::ANCESTOR_RELAY_PARENT_INFO_RUNTIME_REQUIREMENT)]
fn get_pvd_for_candidate_with_older_relay_parent(#[case] runtime_api_version: u32) {
	const LEAF_NUMBER: BlockNumber = 100;
	const OLDER_RELAY_PARENT_NUMBER: BlockNumber = LEAF_NUMBER - 4 * DEFAULT_SCHEDULING_LOOKAHEAD;

	let para_id = ParaId::from(1);
	let mut test_state = TestState::default();
	test_state.set_runtime_api_version(runtime_api_version);
	test_state.set_min_relay_parent_number(OLDER_RELAY_PARENT_NUMBER);

	test_harness(|mut virtual_overseer| async move {
		let leaf_a = TestLeaf {
			number: LEAF_NUMBER,
			hash: Hash::from_low_u64_be(130),
			para_data: vec![
				(para_id, PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};
		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;

		// V3 candidate with relay_parent far outside the scheduling scope.
		let older_relay_parent = Hash::from_low_u64_be(9999);
		let (candidate_a, pvd_a) = make_candidate_v3(
			older_relay_parent,
			OLDER_RELAY_PARENT_NUMBER,
			leaf_a.hash,
			para_id,
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);
		introduce_seconded_candidate(&mut virtual_overseer, &test_state, candidate_a, pvd_a).await;

		// Ask for PVD of a *child* candidate whose parent_head is candidate A's output and
		// whose relay_parent is the OLDER block. The returned PVD should:
		//   - use the old block's number (OLDER_RELAY_PARENT_NUMBER),
		//   - use the old block's storage_root (Hash::zero() in test fixtures),
		//   - use the max_pov_size from the scheduling session's constraints (MAX_POV_SIZE).
		// The version-aware `handle_fetch_relay_parent_info` routes through the API path or
		// the chain-header fallback based on `test_state.runtime_api_version`.
		get_pvd(
			&mut virtual_overseer,
			&test_state,
			para_id,
			older_relay_parent,
			HeadData(vec![1]),
			Some(PersistedValidationData {
				parent_head: HeadData(vec![1]),
				relay_parent_number: OLDER_RELAY_PARENT_NUMBER,
				relay_parent_storage_root: Hash::zero(),
				max_pov_size: MAX_POV_SIZE,
			}),
		)
		.await;

		virtual_overseer
	});
}

async fn get_known_output_heads(
	virtual_overseer: &mut VirtualOverseer,
	para_ids: Vec<ParaId>,
) -> KnownOutputHeads {
	let (tx, rx) = oneshot::channel();
	virtual_overseer
		.send(overseer::FromOrchestra::Communication {
			msg: ProspectiveParachainsMessage::GetKnownOutputHeads(para_ids, tx),
		})
		.await;
	rx.await.unwrap()
}

// Leaves: A #100, B #101 and C #101 both children of A. Y (SP A, head [1]) is reported under A,
// B and C; X (SP B, head [2]) under B only. Three keys in the answer.
#[test]
fn known_output_heads_are_reported_per_leaf() {
	let test_state = TestState::default();
	let para_id = ParaId::from(1);
	let view = test_harness(|mut virtual_overseer| async move {
		// Leaf B (#101) and its parent A (#100).
		let leaf_b = TestLeaf {
			number: 101,
			hash: Hash::from_low_u64_be(131),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![4, 5, 6]))),
			],
		};
		let leaf_a = TestLeaf {
			number: 100,
			hash: get_parent_hash(leaf_b.hash),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};
		// Leaf C (#101): a sibling of B, also a child of A.
		let leaf_c = TestLeaf {
			number: 101,
			hash: Hash::from_low_u64_be(12),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![4, 5, 6]))),
			],
		};

		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;
		activate_leaf(&mut virtual_overseer, &leaf_b, &test_state).await;
		activate_leaf_with_parent_hash_fn(&mut virtual_overseer, &leaf_c, &test_state, |hash| {
			if hash == leaf_c.hash {
				leaf_a.hash
			} else {
				get_parent_hash(hash)
			}
		})
		.await;

		// Y: built on the common ancestor A. Known at A, B and C.
		let (candidate_y, pvd_y) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			para_id,
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);
		introduce_seconded_candidate(&mut virtual_overseer, &test_state, candidate_y, pvd_y).await;

		// X: child of Y, built on leaf B. Known at B only.
		let (candidate_x, pvd_x) = make_candidate(
			leaf_b.hash,
			leaf_b.number,
			para_id,
			HeadData(vec![1]),
			HeadData(vec![2]),
			test_state.validation_code_hash,
		);
		introduce_seconded_candidate(&mut virtual_overseer, &test_state, candidate_x, pvd_x).await;

		let y_head = HeadData(vec![1]).hash();
		let x_head = HeadData(vec![2]).hash();

		let known = get_known_output_heads(&mut virtual_overseer, vec![para_id]).await;
		let at = |leaf: Hash| {
			known
				.get(&leaf)
				.and_then(|per_para| per_para.get(&para_id))
				.cloned()
				.unwrap_or_default()
		};

		assert!(at(leaf_a.hash).contains(&y_head));
		assert!(at(leaf_b.hash).contains(&y_head));
		assert!(at(leaf_c.hash).contains(&y_head));
		assert!(at(leaf_b.hash).contains(&x_head));
		assert!(!at(leaf_a.hash).contains(&x_head));
		assert!(!at(leaf_c.hash).contains(&x_head));
		assert_eq!(known.len(), 3);

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 3);
}

// The answer is keyed by every scheduling parent PP still holds, not only by active leaves.
//
// Phase 1: activate A #100 and its child B #101, then deactivate A. A is no longer a leaf
// but stays in `per_scheduling_parent` as an ancestor of B. Introduce Y (relay parent A)
// and X (relay parent B, child of Y). Expect: A is still a key and reports Y but not X;
// B reports both.
//
// Phase 2: activate D #101, child of A's sibling at #100, then deactivate B. Nothing
// references A or B any more. Expect: D is the only key.
#[test]
fn known_output_heads_include_retained_ancestors() {
	let test_state = TestState::default();
	let para_id = ParaId::from(1);
	let view = test_harness(|mut virtual_overseer| async move {
		let leaf_b = TestLeaf {
			number: 101,
			hash: Hash::from_low_u64_be(131),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![4, 5, 6]))),
			],
		};
		let leaf_a = TestLeaf {
			number: 100,
			hash: get_parent_hash(leaf_b.hash),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![2, 3, 4]))),
			],
		};

		activate_leaf(&mut virtual_overseer, &leaf_a, &test_state).await;
		activate_leaf(&mut virtual_overseer, &leaf_b, &test_state).await;
		deactivate_leaf(&mut virtual_overseer, leaf_a.hash).await;

		// Y: built on A, introduced after A stopped being a leaf. Known at A and B.
		let (candidate_y, pvd_y) = make_candidate(
			leaf_a.hash,
			leaf_a.number,
			para_id,
			HeadData(vec![1, 2, 3]),
			HeadData(vec![1]),
			test_state.validation_code_hash,
		);
		introduce_seconded_candidate(&mut virtual_overseer, &test_state, candidate_y, pvd_y).await;

		// X: child of Y, built on B. Known at B only.
		let (candidate_x, pvd_x) = make_candidate(
			leaf_b.hash,
			leaf_b.number,
			para_id,
			HeadData(vec![1]),
			HeadData(vec![2]),
			test_state.validation_code_hash,
		);
		introduce_seconded_candidate(&mut virtual_overseer, &test_state, candidate_x, pvd_x).await;

		let y_head = HeadData(vec![1]).hash();
		let x_head = HeadData(vec![2]).hash();

		let known = get_known_output_heads(&mut virtual_overseer, vec![para_id]).await;
		let at = |leaf: Hash| {
			known
				.get(&leaf)
				.and_then(|per_para| per_para.get(&para_id))
				.cloned()
				.unwrap_or_default()
		};

		// A is no longer a leaf but is still keyed, as an ancestor of B.
		assert!(at(leaf_a.hash).contains(&y_head));
		assert!(!at(leaf_a.hash).contains(&x_head));
		assert!(at(leaf_b.hash).contains(&y_head));
		assert!(at(leaf_b.hash).contains(&x_head));
		assert_eq!(known.len(), 2);

		// D's parent is a sibling of A. After B goes, A and B are unreferenced and pruned.
		let leaf_d = TestLeaf {
			number: 101,
			hash: Hash::from_low_u64_be(200),
			para_data: vec![
				(1.into(), PerParaData::new(HeadData(vec![1, 2, 3]))),
				(2.into(), PerParaData::new(HeadData(vec![4, 5, 6]))),
			],
		};
		let fork_parent = Hash::from_low_u64_be(300);
		activate_leaf_with_parent_hash_fn(&mut virtual_overseer, &leaf_d, &test_state, |hash| {
			if hash == leaf_d.hash {
				fork_parent
			} else if hash == fork_parent {
				get_parent_hash(leaf_a.hash)
			} else {
				get_parent_hash(hash)
			}
		})
		.await;
		deactivate_leaf(&mut virtual_overseer, leaf_b.hash).await;

		let known_after = get_known_output_heads(&mut virtual_overseer, vec![para_id]).await;
		assert!(!known_after.contains_key(&leaf_a.hash));
		assert!(!known_after.contains_key(&leaf_b.hash));
		assert!(known_after.contains_key(&leaf_d.hash));
		assert_eq!(known_after.len(), 1);

		virtual_overseer
	});

	assert_eq!(view.active_leaves.len(), 1);
	assert_eq!(view.per_scheduling_parent.len(), 1);
}
