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
use futures::{self, Future, StreamExt};
use polkadot_node_primitives::{
	BlockData, Collation, CollationResult, CollatorFn, MaybeCompressedPoV, PoV,
};
use polkadot_node_subsystem::{
	messages::{AllMessages, RuntimeApiMessage, RuntimeApiRequest},
	ActivatedLeaf,
};
use polkadot_node_subsystem_test_helpers::TestSubsystemContextHandle;
use polkadot_node_subsystem_util::TimeoutExt;
use polkadot_primitives::{
	CandidateDescriptorVersion, ClaimQueueOffset, CollatorPair, CoreSelector,
	PersistedValidationData, UMPSignal, ValidationCodeHash, UMP_SEPARATOR,
};
use polkadot_primitives_test_helpers::dummy_head_data;
use rstest::rstest;
use sp_core::Pair;
use sp_keyring::sr25519::Keyring as Sr25519Keyring;
use std::{
	collections::{BTreeMap, VecDeque},
	sync::Mutex,
};

type VirtualOverseer = TestSubsystemContextHandle<CollationGenerationMessage>;

fn test_harness<T: Future<Output = VirtualOverseer>>(test: impl FnOnce(VirtualOverseer) -> T) {
	let pool = sp_core::testing::TaskExecutor::new();
	let (context, virtual_overseer) =
		polkadot_node_subsystem_test_helpers::make_subsystem_context(pool);
	let subsystem = async move {
		let subsystem = crate::CollationGenerationSubsystem::new(Metrics::default());

		subsystem.run(context).await;
	};

	let test_fut = test(virtual_overseer);

	futures::pin_mut!(test_fut);
	futures::executor::block_on(futures::future::join(
		async move {
			let mut virtual_overseer = test_fut.await;
			// Ensure we have handled all responses.
			if let Some(msg) = virtual_overseer.rx.next().timeout(TIMEOUT).await {
				panic!("Did not handle all responses: {:?}", msg);
			}
			// Conclude.
			virtual_overseer.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
		},
		subsystem,
	));
}

fn test_collation() -> Collation {
	Collation {
		upward_messages: Default::default(),
		horizontal_messages: Default::default(),
		new_validation_code: None,
		head_data: dummy_head_data(),
		proof_of_validity: MaybeCompressedPoV::Raw(PoV { block_data: BlockData(Vec::new()) }),
		processed_downward_messages: 0_u32,
		hrmp_watermark: 0_u32.into(),
	}
}

struct CoreSelectorData {
	// The core selector index.
	index: u8,
	// The increment value for the core selector index. Normally 1, but can be set to 0 or another
	// value for testing scenarios where a parachain repeatedly selects the same core index.
	increment_index_by: u8,
	// The claim queue offset.
	cq_offset: u8,
}

impl CoreSelectorData {
	fn new(index: u8, increment_index_by: u8, cq_offset: u8) -> Self {
		Self { index, increment_index_by, cq_offset }
	}
}

struct State {
	core_selector_data: Option<CoreSelectorData>,
}

impl State {
	fn new(core_selector_data: Option<CoreSelectorData>) -> Self {
		Self { core_selector_data }
	}
}

struct TestCollator {
	state: Arc<Mutex<State>>,
}

impl TestCollator {
	fn new(core_selector_data: Option<CoreSelectorData>) -> Self {
		Self { state: Arc::new(Mutex::new(State::new(core_selector_data))) }
	}

	pub fn create_collation_function(&self) -> CollatorFn {
		let state = Arc::clone(&self.state);

		Box::new(move |_relay_parent: Hash, _validation_data: &PersistedValidationData| {
			let mut collation = test_collation();
			let mut state_guard = state.lock().unwrap();

			if let Some(core_selector_data) = &mut state_guard.core_selector_data {
				collation.upward_messages.force_push(UMP_SEPARATOR);
				collation.upward_messages.force_push(
					UMPSignal::SelectCore(
						CoreSelector(core_selector_data.index),
						ClaimQueueOffset(core_selector_data.cq_offset),
					)
					.encode(),
				);
				core_selector_data.index += core_selector_data.increment_index_by;
			}

			async move { Some(CollationResult { collation }) }.boxed()
		})
	}
}

const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2000);

async fn overseer_recv(overseer: &mut VirtualOverseer) -> AllMessages {
	overseer
		.recv()
		.timeout(TIMEOUT)
		.await
		.expect(&format!("{:?} is long enough to receive messages", TIMEOUT))
}

fn test_config<Id: Into<ParaId>>(
	para_id: Id,
	core_selector_data: Option<CoreSelectorData>,
) -> CollationGenerationConfig {
	let test_collator = TestCollator::new(core_selector_data);
	CollationGenerationConfig {
		key: CollatorPair::generate().0,
		collator: Some(test_collator.create_collation_function()),
		para_id: para_id.into(),
	}
}

fn test_config_no_collator<Id: Into<ParaId>>(para_id: Id) -> CollationGenerationConfig {
	CollationGenerationConfig {
		key: CollatorPair::generate().0,
		collator: None,
		para_id: para_id.into(),
	}
}

#[test]
fn submit_segment_is_no_op_before_initialization() {
	test_harness(|mut virtual_overseer| async move {
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::SubmitSegment(SubmitSegmentParams {
					scheduling_parent: Hash::repeat_byte(0),
					core_index: CoreIndex(0),
					candidates_descriptor_version: CandidateDescriptorVersion::V2,
					collations: BoundedVec::try_from(vec![SegmentCollation {
						relay_parent: Hash::repeat_byte(0),
						collation: test_collation(),
						validation_code_hash: Hash::repeat_byte(1).into(),
						session_index: 1,
						validation_data: PersistedValidationData {
							parent_head: vec![1, 2, 3].into(),
							relay_parent_number: 10,
							relay_parent_storage_root: Hash::repeat_byte(1),
							max_pov_size: 1024,
						},
					}])
					.expect("One element segment should fit;qed!"),
				}),
			})
			.await;

		virtual_overseer
	});
}

#[test]
fn submit_segment_leads_to_distribution() {
	let relay_parent = Hash::repeat_byte(0);
	let validation_code_hash = ValidationCodeHash::from(Hash::repeat_byte(42));
	let parent_head = dummy_head_data();
	let para_id = ParaId::from(5);
	let expected_pvd = PersistedValidationData {
		parent_head: parent_head.clone(),
		relay_parent_number: 10,
		relay_parent_storage_root: Hash::repeat_byte(1),
		max_pov_size: 1024,
	};

	test_harness(|mut virtual_overseer| async move {
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::Initialize(test_config_no_collator(para_id)),
			})
			.await;

		let mut collation = test_collation();
		collation.upward_messages.force_push(UMP_SEPARATOR);
		// Add a core selector signal to make this V3-compatible
		collation
			.upward_messages
			.force_push(UMPSignal::SelectCore(CoreSelector(0), ClaimQueueOffset(0)).encode());

		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::SubmitSegment(SubmitSegmentParams {
					scheduling_parent: relay_parent,
					core_index: CoreIndex(0),
					candidates_descriptor_version: CandidateDescriptorVersion::V2,
					collations: BoundedVec::try_from(vec![SegmentCollation {
						relay_parent,
						collation,
						validation_code_hash,
						session_index: 1,
						validation_data: expected_pvd.clone(),
					}])
					.expect("One element segment should fit;qed!"),
				}),
			})
			.await;

		helpers::handle_runtime_calls_on_submit_segment(
			&mut virtual_overseer,
			relay_parent,
			[(CoreIndex(0), VecDeque::from([para_id]))].into(),
		)
		.await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CollatorProtocol(CollatorProtocolMessage::DistributeSegment {segment: Segment::V2(entry),..}) => {
				assert_eq!(entry.parent_head_data.hash(), parent_head.hash());
				assert_eq!(entry.persisted_validation_data_hash, expected_pvd.hash());
				assert_eq!(entry.output_head_data_hash, dummy_head_data().hash());
				assert_eq!(entry.validation_code_hash, validation_code_hash);
			}
		);

		virtual_overseer
	});
}

#[test]
fn submit_segment_v3_runtime_calls_use_scheduling_parent() {
	let relay_parent = Hash::repeat_byte(0xAA);
	let scheduling_parent = Hash::repeat_byte(0xBB);
	let validation_code_hash = ValidationCodeHash::from(Hash::repeat_byte(42));
	let parent_head = dummy_head_data();
	let para_id = ParaId::from(5);
	let expected_pvd = PersistedValidationData {
		parent_head: parent_head.clone(),
		relay_parent_number: 10,
		relay_parent_storage_root: Hash::repeat_byte(1),
		max_pov_size: 1024,
	};

	test_harness(|mut virtual_overseer| async move {
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::Initialize(test_config_no_collator(para_id)),
			})
			.await;

		let mut collation = test_collation();
		collation.upward_messages.force_push(UMP_SEPARATOR);
		collation
			.upward_messages
			.force_push(UMPSignal::SelectCore(CoreSelector(0), ClaimQueueOffset(0)).encode());

		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::SubmitSegment(SubmitSegmentParams {
					scheduling_parent,
					core_index: CoreIndex(0),
					candidates_descriptor_version: CandidateDescriptorVersion::V3,
					collations: BoundedVec::try_from(vec![SegmentCollation {
						relay_parent,
						collation,
						validation_code_hash,
						session_index: 1,
						validation_data: expected_pvd.clone(),
					}])
					.expect("One element segment should fit;qed!"),
				}),
			})
			.await;

		// All runtime API calls must be against the scheduling_parent, not relay_parent.
		helpers::handle_runtime_calls_on_submit_segment(
			&mut virtual_overseer,
			scheduling_parent,
			[(CoreIndex(0), VecDeque::from([para_id]))].into(),
		)
		.await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CollatorProtocol(CollatorProtocolMessage::DistributeSegment {segment: Segment::V3 { scheduling_parent: sp, candidates,.. },.. }) => {
				let [entry] = &candidates[..] else {
					panic!("expected exactly one built entry")
				};
				assert_eq!(entry.parent_head_data.hash(), parent_head.hash());
				assert_eq!(entry.persisted_validation_data_hash, expected_pvd.hash());
				// relay_parent in the descriptor is the execution context
				assert_eq!(entry.relay_parent, relay_parent);
				// scheduling_parent in the descriptor is the scheduling context
				assert_eq!(sp, scheduling_parent);
			}
		);

		virtual_overseer
	});
}

#[test]
fn distribute_collation_only_for_assigned_para_id_at_offset_0() {
	let activated_hash: Hash = [1; 32].into();
	let para_id = ParaId::from(5);

	let claim_queue = (0..=5)
		.into_iter()
		// Set all cores assigned to para_id 5 at the second and third depths. This shouldn't
		// matter.
		.map(|idx| (CoreIndex(idx), VecDeque::from([ParaId::from(idx), para_id, para_id])))
		.collect::<BTreeMap<_, _>>();

	test_harness(|mut virtual_overseer| async move {
		helpers::initialize_collator(&mut virtual_overseer, para_id, None).await;
		helpers::activate_new_head(&mut virtual_overseer, activated_hash).await;
		helpers::handle_runtime_calls_on_new_head_activation(
			&mut virtual_overseer,
			activated_hash,
			claim_queue,
		)
		.await;

		helpers::handle_cores_processing_for_a_leaf(
			&mut virtual_overseer,
			activated_hash,
			para_id,
			vec![5], // Only core 5 is assigned to paraid 5.
		)
		.await;

		virtual_overseer
	});
}

// There are variable number of cores assigned to the paraid.
// On new head activation `CollationGeneration` should produce and distribute the right number of
// new collations with proper assumption about the para candidate chain availability at next block.
#[rstest]
#[case(0)]
#[case(1)]
#[case(2)]
#[case(3)]
fn distribute_collation_with_elastic_scaling(#[case] total_cores: u32) {
	let activated_hash: Hash = [1; 32].into();
	let para_id = ParaId::from(5);

	let claim_queue = (0..total_cores)
		.into_iter()
		.map(|idx| (CoreIndex(idx), VecDeque::from([para_id])))
		.collect::<BTreeMap<_, _>>();

	test_harness(|mut virtual_overseer| async move {
		helpers::initialize_collator(&mut virtual_overseer, para_id, None).await;
		helpers::activate_new_head(&mut virtual_overseer, activated_hash).await;
		helpers::handle_runtime_calls_on_new_head_activation(
			&mut virtual_overseer,
			activated_hash,
			claim_queue,
		)
		.await;

		helpers::handle_cores_processing_for_a_leaf(
			&mut virtual_overseer,
			activated_hash,
			para_id,
			(0..total_cores).collect(),
		)
		.await;

		virtual_overseer
	});
}

// Tests when submission core indexes need to be selected using the core selectors provided in the
// UMP signals. The core selector index is an increasing number that can start with a non-negative
// value (even greater than the core index), but the collation generation protocol uses the
// remainder to select the core. UMP signals may also contain a claim queue offset, based on which
// we need to select the assigned core indexes for the para from that offset in the claim queue.
#[rstest]
#[case(1, 0, 0)]
#[case(2, 0, 1)]
fn distribute_collation_with_core_selectors(
	#[case] total_cores: u32,
	// The core selector index that will be obtained from the first collation.
	#[case] init_cs_index: u8,
	// Claim queue offset where the assigned cores will be stored.
	#[case] cq_offset: u8,
) {
	let activated_hash: Hash = [1; 32].into();
	let para_id = ParaId::from(5);
	let other_para_id = ParaId::from(10);

	let claim_queue = (0..total_cores)
		.into_iter()
		.map(|idx| {
			// Set all cores assigned to para_id 5 at the cq_offset depth.
			let mut vec = VecDeque::from(vec![other_para_id; cq_offset as usize]);
			vec.push_back(para_id);
			(CoreIndex(idx), vec)
		})
		.collect::<BTreeMap<_, _>>();

	test_harness(|mut virtual_overseer| async move {
		helpers::initialize_collator(
			&mut virtual_overseer,
			para_id,
			Some(CoreSelectorData::new(init_cs_index, 1, cq_offset)),
		)
		.await;
		helpers::activate_new_head(&mut virtual_overseer, activated_hash).await;
		helpers::handle_runtime_calls_on_new_head_activation(
			&mut virtual_overseer,
			activated_hash,
			claim_queue,
		)
		.await;

		let mut cores_assigned = (0..total_cores).collect::<Vec<_>>();
		if total_cores > 1 && init_cs_index > 0 {
			// We need to rotate the list of cores because the first core selector index was
			// non-zero, which should change the sequence of submissions. However, collations should
			// still be submitted on all cores.
			cores_assigned.rotate_left((init_cs_index as u32 % total_cores) as usize);
		}
		helpers::handle_cores_processing_for_a_leaf(
			&mut virtual_overseer,
			activated_hash,
			para_id,
			cores_assigned,
		)
		.await;

		virtual_overseer
	});
}

// Tests the behavior when a parachain repeatedly selects the same core index.
// Ensures that the system handles this behavior correctly while maintaining expected functionality.
#[rstest]
#[case(3, 0, vec![0])]
#[case(3, 1, vec![0, 1, 2])]
#[case(3, 2, vec![0, 2, 1])]
#[case(3, 3, vec![0])]
#[case(3, 4, vec![0, 1, 2])]
fn distribute_collation_with_repeated_core_selector_index(
	#[case] total_cores: u32,
	#[case] increment_cs_index_by: u8,
	#[case] expected_selected_cores: Vec<u32>,
) {
	let activated_hash: Hash = [1; 32].into();
	let para_id = ParaId::from(5);

	let claim_queue = (0..total_cores)
		.into_iter()
		.map(|idx| (CoreIndex(idx), VecDeque::from([para_id])))
		.collect::<BTreeMap<_, _>>();

	test_harness(|mut virtual_overseer| async move {
		helpers::initialize_collator(
			&mut virtual_overseer,
			para_id,
			Some(CoreSelectorData::new(0, increment_cs_index_by, 0)),
		)
		.await;
		helpers::activate_new_head(&mut virtual_overseer, activated_hash).await;
		helpers::handle_runtime_calls_on_new_head_activation(
			&mut virtual_overseer,
			activated_hash,
			claim_queue,
		)
		.await;

		helpers::handle_cores_processing_for_a_leaf(
			&mut virtual_overseer,
			activated_hash,
			para_id,
			expected_selected_cores,
		)
		.await;

		virtual_overseer
	});
}

#[test]
fn v2_receipts_failed_core_index_check() {
	let relay_parent = Hash::repeat_byte(0);
	let validation_code_hash = ValidationCodeHash::from(Hash::repeat_byte(42));
	let parent_head = dummy_head_data();
	let para_id = ParaId::from(5);
	let expected_pvd = PersistedValidationData {
		parent_head: parent_head.clone(),
		relay_parent_number: 10,
		relay_parent_storage_root: Hash::repeat_byte(1),
		max_pov_size: 1024,
	};

	test_harness(|mut virtual_overseer| async move {
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::Initialize(test_config_no_collator(para_id)),
			})
			.await;

		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::SubmitSegment(SubmitSegmentParams {
					scheduling_parent: relay_parent,
					core_index: CoreIndex(0),
					candidates_descriptor_version: CandidateDescriptorVersion::V2,
					collations: BoundedVec::try_from(vec![SegmentCollation {
						relay_parent,
						collation: test_collation(),
						validation_code_hash,
						session_index: 1,
						validation_data: expected_pvd.clone(),
					}])
					.expect("One element segment should fit;qed!"),
				}),
			})
			.await;

		helpers::handle_runtime_calls_on_submit_segment(
			&mut virtual_overseer,
			relay_parent,
			// Core index commitment is on core 0 but don't add any assignment for core 0.
			[(CoreIndex(1), [para_id].into_iter().collect())].into_iter().collect(),
		)
		.await;

		// No collation is distributed.

		virtual_overseer
	});
}

#[test]
// An empty segment is rejected with `Error::InvalidSegmentSize`.
fn submit_empty_segment_is_rejected() {
	let para_id = ParaId::from(5);

	test_harness(|mut virtual_overseer| async move {
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::Initialize(test_config_no_collator(para_id)),
			})
			.await;

		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::SubmitSegment(SubmitSegmentParams {
					scheduling_parent: Hash::repeat_byte(0),
					core_index: CoreIndex(0),
					candidates_descriptor_version: CandidateDescriptorVersion::V3,
					collations: BoundedVec::try_from(vec![])
						.expect("Empty segment should fit;qed!"),
				}),
			})
			.await;

		virtual_overseer
	});
}

#[test]
// A V2 segment with more than one collation is rejected with `Error::V2InvalidSegmentLength`.
fn submit_v2_segment_with_multiple_collations_is_rejected() {
	let relay_parent = Hash::repeat_byte(0);
	let validation_code_hash = ValidationCodeHash::from(Hash::repeat_byte(42));
	let para_id = ParaId::from(5);
	let pvd = PersistedValidationData {
		parent_head: dummy_head_data(),
		relay_parent_number: 10,
		relay_parent_storage_root: Hash::repeat_byte(1),
		max_pov_size: 1024,
	};

	test_harness(|mut virtual_overseer| async move {
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::Initialize(test_config_no_collator(para_id)),
			})
			.await;

		let collations = (0..2)
			.map(|_| SegmentCollation {
				relay_parent,
				collation: test_collation(),
				validation_code_hash,
				session_index: 1,
				validation_data: pvd.clone(),
			})
			.collect::<Vec<_>>();

		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::SubmitSegment(SubmitSegmentParams {
					scheduling_parent: relay_parent,
					core_index: CoreIndex(0),
					candidates_descriptor_version: CandidateDescriptorVersion::V2,
					collations: BoundedVec::try_from(collations)
						.expect("Two element segment should fit;qed!"),
				}),
			})
			.await;

		virtual_overseer
	});
}

#[rstest]
#[case(CandidateDescriptorVersion::V1)]
#[case(CandidateDescriptorVersion::Unknown(42))]
// Only V2 and V3 descriptor versions can be submitted.
fn submit_segment_with_unsupported_descriptor_version_is_rejected(
	#[case] version: CandidateDescriptorVersion,
) {
	let relay_parent = Hash::repeat_byte(0);
	let validation_code_hash = ValidationCodeHash::from(Hash::repeat_byte(42));
	let para_id = ParaId::from(5);
	let pvd = PersistedValidationData {
		parent_head: dummy_head_data(),
		relay_parent_number: 10,
		relay_parent_storage_root: Hash::repeat_byte(1),
		max_pov_size: 1024,
	};

	test_harness(|mut virtual_overseer| async move {
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::Initialize(test_config_no_collator(para_id)),
			})
			.await;

		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::SubmitSegment(SubmitSegmentParams {
					scheduling_parent: relay_parent,
					core_index: CoreIndex(0),
					candidates_descriptor_version: version,
					// A plain collation without UMP signals: with a `SelectCore` signal a
					// `V1` submission would already fail the UMP signal checks instead of
					// reaching the descriptor version check.
					collations: BoundedVec::try_from(vec![SegmentCollation {
						relay_parent,
						collation: test_collation(),
						validation_code_hash,
						session_index: 1,
						validation_data: pvd,
					}])
					.expect("One element segment should fit;qed!"),
				}),
			})
			.await;

		// The runtime calls precede the descriptor version checks.
		helpers::handle_runtime_calls_on_submit_segment(
			&mut virtual_overseer,
			relay_parent,
			[(CoreIndex(0), VecDeque::from([para_id]))].into(),
		)
		.await;

		// No segment is distributed.

		virtual_overseer
	});
}

#[test]
// Verify that an ApprovedPeer UMP signal does not break the subsystem (DistributedSegment is
// sent), assuming CandidateReceiptV2 node feature is enabled.
fn approved_peer_signal() {
	let relay_parent = Hash::repeat_byte(0);
	let validation_code_hash = ValidationCodeHash::from(Hash::repeat_byte(42));
	let parent_head = dummy_head_data();
	let para_id = ParaId::from(5);
	let expected_pvd = PersistedValidationData {
		parent_head: parent_head.clone(),
		relay_parent_number: 10,
		relay_parent_storage_root: Hash::repeat_byte(1),
		max_pov_size: 1024,
	};

	test_harness(|mut virtual_overseer| async move {
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::Initialize(test_config_no_collator(para_id)),
			})
			.await;

		let mut collation = test_collation();
		collation.upward_messages.force_push(UMP_SEPARATOR);
		collation
			.upward_messages
			.force_push(UMPSignal::ApprovedPeer(vec![1, 2, 3, 4, 5].try_into().unwrap()).encode());

		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::SubmitSegment(SubmitSegmentParams {
					scheduling_parent: relay_parent,
					core_index: CoreIndex(0),
					candidates_descriptor_version: CandidateDescriptorVersion::V3,
					collations: BoundedVec::try_from(vec![SegmentCollation {
						relay_parent,
						collation,
						validation_code_hash,
						session_index: 1,
						validation_data: expected_pvd.clone(),
					}])
					.expect("One element segment should fit;qed!"),
				}),
			})
			.await;

		helpers::handle_runtime_calls_on_submit_segment(
			&mut virtual_overseer,
			relay_parent,
			[(CoreIndex(0), [para_id].into_iter().collect())].into_iter().collect(),
		)
		.await;

		assert_matches!(
			overseer_recv(&mut virtual_overseer).await,
			AllMessages::CollatorProtocol(CollatorProtocolMessage::DistributeSegment { segment: Segment::V3 { candidates,.. }, .. }) => {
				let [entry] = &candidates[..] else {
					panic!("expected exactly one built entry")
				};
				assert_eq!(entry.parent_head_data.hash(), parent_head.hash());
				assert_eq!(entry.persisted_validation_data_hash, expected_pvd.hash());
				assert_eq!(entry.output_head_data_hash, dummy_head_data().hash());
				assert_eq!(entry.validation_code_hash, validation_code_hash);
			}
		);

		virtual_overseer
	});
}

mod helpers {
	use super::*;
	use std::collections::{BTreeMap, VecDeque};

	// Sends `Initialize` with a collator config
	pub async fn initialize_collator(
		virtual_overseer: &mut VirtualOverseer,
		para_id: ParaId,
		core_selector_data: Option<CoreSelectorData>,
	) {
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CollationGenerationMessage::Initialize(test_config(
					para_id,
					core_selector_data,
				)),
			})
			.await;
	}

	// Sends `ActiveLeaves` for a single leaf with the specified hash. Block number is hardcoded.
	pub async fn activate_new_head(virtual_overseer: &mut VirtualOverseer, activated_hash: Hash) {
		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
				activated: Some(ActivatedLeaf {
					hash: activated_hash,
					number: 10,
					unpin_handle: polkadot_node_subsystem_test_helpers::mock::dummy_unpin_handle(
						activated_hash,
					),
				}),
				..Default::default()
			})))
			.await;
	}

	// Handle all runtime calls performed in `handle_new_activation`.
	pub async fn handle_runtime_calls_on_new_head_activation(
		virtual_overseer: &mut VirtualOverseer,
		activated_hash: Hash,
		claim_queue: BTreeMap<CoreIndex, VecDeque<ParaId>>,
	) {
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(hash, RuntimeApiRequest::SessionIndexForChild(tx))) => {
				assert_eq!(hash, activated_hash);
				tx.send(Ok(1)).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(hash, RuntimeApiRequest::Validators(tx))) => {
				assert_eq!(hash, activated_hash);
				tx.send(Ok(vec![
					Sr25519Keyring::Alice.public().into(),
					Sr25519Keyring::Bob.public().into(),
					Sr25519Keyring::Charlie.public().into(),
				])).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(hash, RuntimeApiRequest::ClaimQueue(tx))) => {
				assert_eq!(hash, activated_hash);
				tx.send(Ok(claim_queue)).unwrap();
			}
		);
	}

	// Handles all runtime requests performed in `handle_new_activation` for the case when a
	// collation should be prepared for the new leaf
	pub async fn handle_cores_processing_for_a_leaf(
		virtual_overseer: &mut VirtualOverseer,
		activated_hash: Hash,
		para_id: ParaId,
		cores_assigned: Vec<u32>,
	) {
		// Expect no messages if no cores is assigned to the para
		if cores_assigned.is_empty() {
			return;
		}

		// Some hardcoded data - if needed, extract to parameters
		let validation_code_hash = ValidationCodeHash::from(Hash::repeat_byte(42));
		let parent_head = dummy_head_data();
		let pvd = PersistedValidationData {
			parent_head: parent_head.clone(),
			relay_parent_number: 10,
			relay_parent_storage_root: Hash::repeat_byte(1),
			max_pov_size: 1024,
		};

		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(hash, RuntimeApiRequest::PersistedValidationData(id, a, tx))) => {
				assert_eq!(hash, activated_hash);
				assert_eq!(id, para_id);
				assert_eq!(a, OccupiedCoreAssumption::Included);

				let _ = tx.send(Ok(Some(pvd.clone())));
			}
		);

		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				hash,
				RuntimeApiRequest::ValidationCodeHash(
					id,
					assumption,
					tx,
				),
			)) => {
				assert_eq!(hash, activated_hash);
				assert_eq!(id, para_id);
				assert_eq!(assumption, OccupiedCoreAssumption::Included);

				let _ = tx.send(Ok(Some(validation_code_hash)));
			}
		);

		for core in cores_assigned {
			assert_matches!(
				overseer_recv(virtual_overseer).await,
				AllMessages::CollatorProtocol(CollatorProtocolMessage::DistributeSegment{ segment: Segment::V2(entry), core_index, ..}) => {
					assert_eq!(CoreIndex(core), core_index);
					assert_eq!(entry.parent_head_data.hash(), parent_head.hash());
					assert_eq!(entry.persisted_validation_data_hash, pvd.hash());
					assert_eq!(entry.output_head_data_hash, dummy_head_data().hash());
					assert_eq!(entry.validation_code_hash, validation_code_hash);
				}
			);
		}
	}

	// Handles all runtime requests performed in `handle_submit_segment`.
	// All requests are made against the scheduling parent (or relay_parent for V2).
	pub async fn handle_runtime_calls_on_submit_segment(
		virtual_overseer: &mut VirtualOverseer,
		scheduling_parent: Hash,
		claim_queue: BTreeMap<CoreIndex, VecDeque<ParaId>>,
	) {
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::ClaimQueue(tx),
			)) => {
				assert_eq!(rp, scheduling_parent);
				tx.send(Ok(claim_queue)).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(rp, RuntimeApiRequest::SessionIndexForChild(tx))) => {
				assert_eq!(rp, scheduling_parent);
				tx.send(Ok(1)).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(rp, RuntimeApiRequest::Validators(tx))) => {
				assert_eq!(rp, scheduling_parent);
				tx.send(Ok(vec![
					Sr25519Keyring::Alice.public().into(),
					Sr25519Keyring::Bob.public().into(),
					Sr25519Keyring::Charlie.public().into(),
				])).unwrap();
			}
		);
	}
}
