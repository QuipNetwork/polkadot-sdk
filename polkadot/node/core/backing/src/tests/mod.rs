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
use futures::{future, Future};
use polkadot_node_primitives::{BlockData, InvalidCandidate, SignedFullStatement, Statement};
use polkadot_node_subsystem::{
	messages::{
		AllMessages, BackableCandidateRef, ChainApiMessage, CollatorProtocolMessage,
		HypotheticalMembership, PvfExecKind, RuntimeApiMessage, RuntimeApiRequest,
		ValidationFailed,
	},
	ActivatedLeaf, ActiveLeavesUpdate, FromOrchestra, OverseerSignal, TimeoutExt,
};
use polkadot_node_subsystem_test_helpers::mock::new_leaf;
use polkadot_primitives::{
	node_features, BlockNumber, CandidateDescriptorV2, CandidateDescriptorVersion, CollatorId,
	CollatorSignature, CoreState, GroupRotationInfo, HeadData, Header, MutateDescriptorV2,
	OccupiedCore, PersistedValidationData, ScheduledCore, SessionIndex, LEGACY_MIN_BACKING_VOTES,
};
use polkadot_primitives_test_helpers::{
	dummy_candidate_receipt_bad_sig, dummy_committed_candidate_receipt_v2, dummy_hash,
	validator_pubkeys, CandidateDescriptor,
};
use polkadot_statement_table::v2::Misbehavior;
use sp_application_crypto::{AppCrypto, ByteArray};
use sp_keyring::Sr25519Keyring;
use sp_keystore::Keystore;
use sp_tracing as _;
use std::{
	collections::{BTreeMap, HashMap, VecDeque},
	time::Duration,
};

struct TestLeaf {
	activated: ActivatedLeaf,
	min_relay_parents: Vec<(ParaId, u32)>,
}

fn table_statement_to_primitive(statement: TableStatement) -> Statement {
	match statement {
		TableStatement::Seconded(committed_candidate_receipt) => {
			Statement::Seconded(committed_candidate_receipt)
		},
		TableStatement::Valid(candidate_hash) => Statement::Valid(candidate_hash),
	}
}

fn dummy_pvd() -> PersistedValidationData {
	PersistedValidationData {
		parent_head: HeadData(vec![7, 8, 9]),
		relay_parent_number: 0_u32.into(),
		max_pov_size: 1024,
		relay_parent_storage_root: dummy_hash(),
	}
}

#[derive(Default)]
struct PerSessionCacheState {
	has_cached_validators: bool,
	has_cached_node_features: bool,
	has_cached_minimum_backing_votes: bool,
}

pub(crate) struct TestState {
	chain_ids: Vec<ParaId>,
	keystore: KeystorePtr,
	validators: Vec<Sr25519Keyring>,
	validator_public: Vec<ValidatorId>,
	validation_data: PersistedValidationData,
	validator_groups: (Vec<Vec<ValidatorIndex>>, GroupRotationInfo),
	validator_to_group: IndexedVec<ValidatorIndex, Option<GroupIndex>>,
	availability_cores: Vec<CoreState>,
	claim_queue: BTreeMap<CoreIndex, VecDeque<ParaId>>,
	head_data: HashMap<ParaId, HeadData>,
	signing_context: SigningContext,
	relay_parent: Hash,
	minimum_backing_votes: u32,
	disabled_validators: Vec<ValidatorIndex>,
	node_features: NodeFeatures,
	per_session_cache_state: PerSessionCacheState,
}

impl TestState {
	fn session(&self) -> SessionIndex {
		self.signing_context.session_index
	}
}

impl Default for TestState {
	fn default() -> Self {
		let chain_a = ParaId::from(1);
		let chain_b = ParaId::from(2);

		let chain_ids = vec![chain_a, chain_b];

		let validators = vec![
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Ferdie,
			Sr25519Keyring::One,
		];

		let keystore = Arc::new(sc_keystore::LocalKeystore::in_memory());
		// Make sure `Alice` key is in the keystore, so this mocked node will be a parachain
		// validator.
		Keystore::sr25519_generate_new(&*keystore, ValidatorId::ID, Some(&validators[0].to_seed()))
			.expect("Insert key into keystore");

		let validator_public = validator_pubkeys(&validators);

		let validator_groups = vec![vec![2, 0, 3, 5], vec![1]]
			.into_iter()
			.map(|g| g.into_iter().map(ValidatorIndex).collect())
			.collect();
		let validator_to_group: IndexedVec<_, _> =
			vec![Some(0), Some(1), Some(0), Some(0), None, Some(0)]
				.into_iter()
				.map(|x| x.map(|x| GroupIndex(x)))
				.collect();
		let group_rotation_info =
			GroupRotationInfo { session_start_block: 0, group_rotation_frequency: 100, now: 1 };

		let availability_cores = vec![
			CoreState::Scheduled(ScheduledCore { para_id: chain_a, collator: None }),
			CoreState::Scheduled(ScheduledCore { para_id: chain_b, collator: None }),
		];

		let mut claim_queue = BTreeMap::new();
		claim_queue.insert(CoreIndex(0), [chain_a].into_iter().collect());
		claim_queue.insert(CoreIndex(1), [chain_b].into_iter().collect());

		let mut head_data = HashMap::new();
		head_data.insert(chain_a, HeadData(vec![4, 5, 6]));
		head_data.insert(chain_b, HeadData(vec![5, 6, 7]));

		let relay_parent = Hash::repeat_byte(5);

		let signing_context = SigningContext { session_index: 1, parent_hash: relay_parent };

		let validation_data = PersistedValidationData {
			parent_head: HeadData(vec![7, 8, 9]),
			relay_parent_number: 0_u32.into(),
			max_pov_size: 1024,
			relay_parent_storage_root: dummy_hash(),
		};

		Self {
			chain_ids,
			keystore,
			validators,
			per_session_cache_state: PerSessionCacheState::default(),
			validator_public,
			validator_groups: (validator_groups, group_rotation_info),
			validator_to_group,
			availability_cores,
			claim_queue,
			head_data,
			validation_data,
			signing_context,
			relay_parent,
			minimum_backing_votes: LEGACY_MIN_BACKING_VOTES,
			disabled_validators: Vec::new(),
			node_features: Default::default(),
		}
	}
}

type VirtualOverseer =
	polkadot_node_subsystem_test_helpers::TestSubsystemContextHandle<CandidateBackingMessage>;

fn test_harness<T: Future<Output = VirtualOverseer>>(
	keystore: KeystorePtr,
	test: impl FnOnce(VirtualOverseer) -> T,
) {
	sp_tracing::init_for_tests();

	let pool = sp_core::testing::TaskExecutor::new();

	let (context, virtual_overseer) =
		polkadot_node_subsystem_test_helpers::make_subsystem_context(pool.clone());

	let subsystem = async move {
		if let Err(e) = super::run(context, keystore, Metrics(None)).await {
			panic!("{:?}", e);
		}
	};

	let test_fut = test(virtual_overseer);

	futures::pin_mut!(test_fut);
	futures::pin_mut!(subsystem);
	futures::executor::block_on(future::join(
		async move {
			let mut virtual_overseer = test_fut.await;
			virtual_overseer.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
		},
		subsystem,
	));
}

fn make_erasure_root(test: &TestState, pov: PoV, validation_data: PersistedValidationData) -> Hash {
	let available_data = AvailableData { validation_data, pov: Arc::new(pov) };

	let chunks =
		polkadot_erasure_coding::obtain_chunks_v1(test.validators.len(), &available_data).unwrap();
	polkadot_erasure_coding::branches(&chunks).root()
}

#[derive(Default, Clone)]
struct TestCandidateBuilder {
	para_id: ParaId,
	head_data: HeadData,
	pov_hash: Hash,
	relay_parent: Hash,
	erasure_root: Hash,
	persisted_validation_data_hash: Hash,
	validation_code: Vec<u8>,
}

impl TestCandidateBuilder {
	fn build(self) -> CommittedCandidateReceipt {
		CommittedCandidateReceipt {
			descriptor: polkadot_primitives::CandidateDescriptorV2::new(
				self.para_id,
				self.relay_parent,
				CoreIndex(0),
				1, // session_index (matches TestState default)
				self.persisted_validation_data_hash,
				self.pov_hash,
				self.erasure_root,
				self.head_data.hash(),
				ValidationCode(self.validation_code).hash(),
			),
			commitments: CandidateCommitments {
				head_data: self.head_data,
				upward_messages: Default::default(),
				horizontal_messages: Default::default(),
				new_validation_code: None,
				processed_downward_messages: 0,
				hrmp_watermark: 0_u32,
			},
		}
	}
}

async fn assert_validation_request(
	virtual_overseer: &mut VirtualOverseer,
	validation_code: ValidationCode,
) {
	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::RuntimeApi(
			RuntimeApiMessage::Request(_, RuntimeApiRequest::ValidationCodeByHash(hash, tx))
		) if hash == validation_code.hash() => {
			tx.send(Ok(Some(validation_code))).unwrap();
		}
	);
}

async fn assert_validate_from_exhaustive(
	virtual_overseer: &mut VirtualOverseer,
	assert_pvd: &PersistedValidationData,
	assert_pov: &PoV,
	assert_validation_code: &ValidationCode,
	assert_candidate: &CommittedCandidateReceipt,
	expected_head_data: &HeadData,
	result_validation_data: PersistedValidationData,
) {
	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::CandidateValidation(
			CandidateValidationMessage::ValidateFromExhaustive {
				pov,
				validation_data,
				validation_code,
				candidate_receipt,
				exec_kind,
				response_sender,
				..
			},
		) if validation_data == *assert_pvd &&
			validation_code == *assert_validation_code &&
			*pov == *assert_pov && candidate_receipt.descriptor == assert_candidate.descriptor &&
			matches!(exec_kind, PvfExecKind::BackingSystemParas(_)) &&
			candidate_receipt.commitments_hash == assert_candidate.commitments.hash() =>
		{
			response_sender.send(Ok(ValidationResult::Valid(
				CandidateCommitments {
					head_data: expected_head_data.clone(),
					horizontal_messages: Default::default(),
					upward_messages: Default::default(),
					new_validation_code: None,
					processed_downward_messages: 0,
					hrmp_watermark: 0,
				},
				result_validation_data,
			)))
			.unwrap();
		}
	);
}

// Activates the initial leaf and returns the `ParaId` used. This function is a prerequisite for all
// tests.
async fn activate_initial_leaf(
	virtual_overseer: &mut VirtualOverseer,
	test_state: &mut TestState,
) -> ParaId {
	const LEAF_A_BLOCK_NUMBER: BlockNumber = 100;
	const LEAF_A_ANCESTRY_LEN: BlockNumber = 3;
	let para_id = test_state.chain_ids[0];

	let activated = new_leaf(test_state.relay_parent, LEAF_A_BLOCK_NUMBER - 1);
	let min_relay_parents = vec![(para_id, LEAF_A_BLOCK_NUMBER - LEAF_A_ANCESTRY_LEN)];
	let test_leaf_a = TestLeaf { activated, min_relay_parents };

	activate_leaf(virtual_overseer, test_leaf_a, test_state).await;
	para_id
}

async fn assert_candidate_is_shared_and_seconded(
	virtual_overseer: &mut VirtualOverseer,
	relay_parent: &Hash,
) {
	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::StatementDistribution(
			StatementDistributionMessage::Share(
				parent_hash,
				_signed_statement,
			)
		) if parent_hash == *relay_parent => {}
	);

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::CollatorProtocol(CollatorProtocolMessage::Seconded(hash, statement)) => {
			assert_eq!(*relay_parent, hash);
			assert_matches!(statement.payload(), Statement::Seconded(_));
		}
	);
}

async fn assert_candidate_is_shared_and_backed(
	virtual_overseer: &mut VirtualOverseer,
	relay_parent: &Hash,
	expected_para_id: &ParaId,
	expected_candidate_hash: &CandidateHash,
) {
	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::StatementDistribution(
			StatementDistributionMessage::Share(hash, _stmt)
		) => {
			assert_eq!(*relay_parent, hash);
		}
	);

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::ProspectiveParachains(
			ProspectiveParachainsMessage::CandidateBacked(
				candidate_para_id, candidate_hash
			),
		) if *expected_candidate_hash == candidate_hash && candidate_para_id == *expected_para_id
	);

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::StatementDistribution(StatementDistributionMessage::Backed (
			candidate_hash
		)) if *expected_candidate_hash == candidate_hash
	);
}

fn get_parent_hash(hash: Hash) -> Hash {
	Hash::from_low_u64_be(hash.to_low_u64_be() + 1)
}

async fn activate_leaf(
	virtual_overseer: &mut VirtualOverseer,
	leaf: TestLeaf,
	test_state: &mut TestState,
) {
	let TestLeaf { activated, min_relay_parents } = leaf;
	let leaf_hash = activated.hash;
	let leaf_number = activated.number;
	// Start work on some new parent.
	virtual_overseer
		.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(
			activated,
		))))
		.await;

	let min_min = *min_relay_parents
		.iter()
		.map(|(_, block_num)| block_num)
		.min()
		.unwrap_or(&leaf_number);

	let ancestry_len = leaf_number + 1 - min_min;

	// 1. SessionIndexForChild for the leaf
	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::RuntimeApi(
			RuntimeApiMessage::Request(parent, RuntimeApiRequest::SessionIndexForChild(tx))
		) if parent == leaf_hash => {
			tx.send(Ok(test_state.signing_context.session_index)).unwrap();
		}
	);

	// 2. SchedulingLookahead to determine how many ancestors to fetch
	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::RuntimeApi(
			RuntimeApiMessage::Request(parent, RuntimeApiRequest::SchedulingLookahead(session_index, tx))
		) if parent == leaf_hash && session_index == test_state.signing_context.session_index => {
			tx.send(Ok(ancestry_len)).unwrap();
		}
	);

	// 3. Bulk Ancestors request from Chain API
	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::ChainApi(ChainApiMessage::Ancestors {
			hash,
			k,
			response_channel: tx,
		}) if hash == leaf_hash && k == (ancestry_len - 1) as usize => {
			// Return ancestor hashes in descending order (excluding leaf)
			let ancestors: Vec<Hash> = std::iter::successors(
				Some(leaf_hash),
				|h| Some(get_parent_hash(*h))
			)
			.skip(1)
			.take(k)
			.collect();
			tx.send(Ok(ancestors)).unwrap();
		}
	);

	// 4. SessionIndexForChild for each ancestor (to detect session boundaries)
	for i in 0..(ancestry_len - 1) {
		let ancestor_hash = std::iter::successors(Some(leaf_hash), |h| Some(get_parent_hash(*h)))
			.nth((i + 1) as usize)
			.unwrap();
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::SessionIndexForChild(tx))
			) if parent == ancestor_hash => {
				tx.send(Ok(test_state.signing_context.session_index)).unwrap();
			}
		);
	}

	// 5. Now handle BlockHeader requests for uncached blocks (existing flow)
	let ancestry_hashes = std::iter::successors(Some(leaf_hash), |h| Some(get_parent_hash(*h)))
		.take(ancestry_len as usize);
	let ancestry_numbers = (min_min..=leaf_number).rev();
	let ancestry_iter = ancestry_hashes.zip(ancestry_numbers).peekable();

	let mut next_overseer_message = None;
	// How many blocks were actually requested.
	let mut requested_len = 0;
	{
		let mut ancestry_iter = ancestry_iter.clone();
		while let Some((hash, number)) = ancestry_iter.next() {
			// May be `None` for the last element.
			let parent_hash =
				ancestry_iter.peek().map(|(h, _)| *h).unwrap_or_else(|| get_parent_hash(hash));

			let msg = virtual_overseer.recv().await;
			// It may happen that some blocks were cached by implicit view,
			// reuse the message.
			if !matches!(&msg, AllMessages::ChainApi(ChainApiMessage::BlockHeader(..))) {
				next_overseer_message.replace(msg);
				break;
			}

			assert_matches!(
				msg,
				AllMessages::ChainApi(
					ChainApiMessage::BlockHeader(_hash, tx)
				) if _hash == hash => {
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

	for (hash, number) in ancestry_iter.take(requested_len) {
		let msg = match next_overseer_message.take() {
			Some(msg) => msg,
			None => virtual_overseer.recv().await,
		};

		// Check that subsystem job issues a request for the session index for child.
		assert_matches!(
			msg,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::SessionIndexForChild(tx))
			) if parent == hash => {
				tx.send(Ok(test_state.signing_context.session_index)).unwrap();
			}
		);

		// Check that subsystem job issues a request for the validator groups.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::ValidatorGroups(tx))
			) if parent == hash => {
				let (validator_groups, mut group_rotation_info) = test_state.validator_groups.clone();
				group_rotation_info.now = number;
				tx.send(Ok((validator_groups, group_rotation_info))).unwrap();
			}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::ClaimQueue(tx))
			) if parent == hash => {
				tx.send(Ok(
					test_state.claim_queue.clone()
				)).unwrap();
			}
		);

		// Check that the subsystem job issues a request for the disabled validators.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::DisabledValidators(tx))
			) if parent == hash => {
				tx.send(Ok(test_state.disabled_validators.clone())).unwrap();
			}
		);

		if !test_state.per_session_cache_state.has_cached_validators {
			// Check that subsystem job issues a request for a validator set.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::RuntimeApi(
					RuntimeApiMessage::Request(parent, RuntimeApiRequest::Validators(tx))
				) if parent == hash => {
					tx.send(Ok(test_state.validator_public.clone())).unwrap();
				}
			);
			test_state.per_session_cache_state.has_cached_validators = true;
		}

		if !test_state.per_session_cache_state.has_cached_node_features {
			// Node features request from runtime: all features are disabled.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::RuntimeApi(
					RuntimeApiMessage::Request(parent, RuntimeApiRequest::NodeFeatures(_session_index, tx))
				) if parent == hash => {
					tx.send(Ok(test_state.node_features.clone())).unwrap();
				}
			);
			test_state.per_session_cache_state.has_cached_node_features = true;
		}

		if !test_state.per_session_cache_state.has_cached_minimum_backing_votes {
			// Check if subsystem job issues a request for the minimum backing votes.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					parent,
					RuntimeApiRequest::MinimumBackingVotes(session_index, tx),
				)) if parent == hash && session_index == test_state.signing_context.session_index => {
					tx.send(Ok(test_state.minimum_backing_votes)).unwrap();
				}
			);
			test_state.per_session_cache_state.has_cached_minimum_backing_votes = true;
		}
	}
}

async fn assert_validate_seconded_candidate(
	virtual_overseer: &mut VirtualOverseer,
	relay_parent: Hash,
	candidate: &CommittedCandidateReceipt,
	assert_pov: &PoV,
	assert_pvd: &PersistedValidationData,
	assert_validation_code: &ValidationCode,
	expected_head_data: &HeadData,
	fetch_pov: bool,
) {
	assert_validation_request(virtual_overseer, assert_validation_code.clone()).await;

	if fetch_pov {
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::AvailabilityDistribution(
				AvailabilityDistributionMessage::FetchPoV {
					relay_parent: hash,
					tx,
					..
				}
			) if hash == relay_parent => {
				tx.send(assert_pov.clone()).unwrap();
			}
		);
	}

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::CandidateValidation(
			CandidateValidationMessage::ValidateFromExhaustive {
				pov,
				validation_data,
				validation_code,
				candidate_receipt,
				exec_kind,
				response_sender,
				..
			},
		) if validation_data == *assert_pvd &&
			validation_code == *assert_validation_code &&
			*pov == *assert_pov && candidate_receipt.descriptor == candidate.descriptor &&
			matches!(exec_kind, PvfExecKind::BackingSystemParas(_)) &&
			candidate_receipt.commitments_hash == candidate.commitments.hash() =>
		{
			response_sender.send(Ok(ValidationResult::Valid(
				CandidateCommitments {
					head_data: expected_head_data.clone(),
					horizontal_messages: Default::default(),
					upward_messages: Default::default(),
					new_validation_code: None,
					processed_downward_messages: 0,
					hrmp_watermark: 0,
				},
				assert_pvd.clone(),
			)))
			.unwrap();
		}
	);

	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::AvailabilityStore(
			AvailabilityStoreMessage::StoreAvailableData { candidate_hash, tx, .. }
		) if candidate_hash == candidate.hash() => {
			tx.send(Ok(())).unwrap();
		}
	);
}

pub(crate) async fn assert_hypothetical_membership_requests(
	virtual_overseer: &mut VirtualOverseer,
	mut expected_requests: Vec<(
		HypotheticalMembershipRequest,
		Vec<(HypotheticalCandidate, HypotheticalMembership)>,
	)>,
) {
	// Requests come with no particular order.
	let requests_num = expected_requests.len();

	for _ in 0..requests_num {
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::GetHypotheticalMembership(request, tx),
			) => {
				let idx = match expected_requests.iter().position(|r| r.0 == request) {
					Some(idx) => idx,
					None =>
						panic!(
						"unexpected hypothetical membership request, no match found for {:?}",
						request
						),
				};
				let resp = std::mem::take(&mut expected_requests[idx].1);
				tx.send(resp).unwrap();

				expected_requests.remove(idx);
			}
		);
	}
}

pub(crate) fn make_hypothetical_membership_response(
	hypothetical_candidate: HypotheticalCandidate,
	relay_parent_hash: Hash,
) -> Vec<(HypotheticalCandidate, HypotheticalMembership)> {
	vec![(hypothetical_candidate, vec![relay_parent_hash])]
}

// Test that a `CandidateBackingMessage::Second` issues validation work and in case validation is
// successful issues correct messages.
#[test]
fn backing_second_works() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		let para_id = activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id,
			relay_parent: test_state.relay_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second {
			scheduling_parent: test_state.relay_parent,
			candidate: candidate.to_plain(),
			pvd: pvd.clone(),
			pov: pov.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			test_state.relay_parent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			false,
		)
		.await;

		let hypothetical_candidate = HypotheticalCandidate::Complete {
			candidate_hash: candidate.hash(),
			receipt: Arc::new(candidate.clone()),
			persisted_validation_data: pvd.clone(),
		};
		let expected_request = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(test_state.relay_parent),
		};
		let expected_response =
			make_hypothetical_membership_response(hypothetical_candidate, test_state.relay_parent);
		assert_hypothetical_membership_requests(
			&mut virtual_overseer,
			vec![(expected_request, expected_response)],
		)
		.await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_candidate_is_shared_and_seconded(&mut virtual_overseer, &test_state.relay_parent)
			.await;

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::stop_work(test_state.relay_parent),
			)))
			.await;
		virtual_overseer
	});
}

// Test that the candidate reaches quorum successfully.
#[test]
fn backing_works() {
	let mut test_state = TestState::default();

	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		let para_id = activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov_ab = PoV { block_data: BlockData(vec![1, 2, 3]) };
		let pvd_ab = dummy_pvd();
		let validation_code_ab = ValidationCode(vec![1, 2, 3]);

		let pov_hash = pov_ab.hash();

		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let candidate_a = TestCandidateBuilder {
			para_id,
			relay_parent: test_state.relay_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov_ab.clone(), pvd_ab.clone()),
			validation_code: validation_code_ab.0.clone(),
			..Default::default()
		}
		.build();

		let candidate_a_hash = candidate_a.hash();

		let public1 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[5].to_seed()),
		)
		.expect("Insert key into keystore");
		let public2 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[2].to_seed()),
		)
		.expect("Insert key into keystore");

		let signed_a = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate_a.clone(), pvd_ab.clone()),
			&test_state.signing_context,
			ValidatorIndex(2),
			&public2.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let signed_b = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Valid(candidate_a_hash),
			&test_state.signing_context,
			ValidatorIndex(5),
			&public1.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_a.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate_a
				&& req.candidate_para == para_id
				&& pvd_ab == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			candidate_a.descriptor.relay_parent(),
			&candidate_a,
			&pov_ab,
			&pvd_ab,
			&validation_code_ab,
			expected_head_data,
			true,
		)
		.await;

		assert_candidate_is_shared_and_backed(
			&mut virtual_overseer,
			&test_state.relay_parent,
			&para_id,
			&candidate_a_hash,
		)
		.await;

		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_b.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		let (tx, rx) = oneshot::channel();
		let msg = CandidateBackingMessage::GetBackableCandidates {
			candidates: std::iter::once((
				test_state.chain_ids[0],
				vec![BackableCandidateRef {
					candidate_hash: candidate_a_hash,
					scheduling_parent: test_state.relay_parent,
				}],
			))
			.collect(),
			sender: tx,
		};

		virtual_overseer.send(FromOrchestra::Communication { msg }).await;

		let mut candidates = rx.await.unwrap();
		assert_eq!(1, candidates.len());
		let candidates = candidates.remove(&test_state.chain_ids[0]).unwrap();
		assert_eq!(1, candidates.len());
		assert_eq!(candidates[0].validity_votes().len(), 3);

		let (validator_indices, maybe_core_index) =
			candidates[0].validator_indices_and_core_index();

		assert_eq!(maybe_core_index.unwrap(), CoreIndex(0));

		assert_eq!(
			validator_indices,
			bitvec::bitvec![u8, bitvec::order::Lsb0; 1, 1, 0, 1].as_bitslice()
		);

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::stop_work(test_state.relay_parent),
			)))
			.await;
		virtual_overseer
	});
}

#[test]
fn get_backed_candidate_preserves_order() {
	let mut test_state = TestState::default();
	// Set a single validator as the first validator group. It simplifies the test.
	test_state.validator_groups.0[0] = vec![ValidatorIndex(2)];
	// Add another validator group for the third core.
	test_state.validator_groups.0.push(vec![ValidatorIndex(3)]);
	// Assign the second core to the same para as the first one.
	test_state.availability_cores[1] =
		CoreState::Scheduled(ScheduledCore { para_id: test_state.chain_ids[0], collator: None });
	*test_state.claim_queue.get_mut(&CoreIndex(1)).unwrap() =
		[test_state.chain_ids[0]].into_iter().collect();
	// Add another availability core for paraid 2.
	test_state.availability_cores.push(CoreState::Scheduled(ScheduledCore {
		para_id: test_state.chain_ids[1],
		collator: None,
	}));
	test_state
		.claim_queue
		.insert(CoreIndex(2), [test_state.chain_ids[1]].into_iter().collect());

	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov_a = PoV { block_data: BlockData(vec![1, 2, 3]) };
		let pov_b = PoV { block_data: BlockData(vec![3, 4, 5]) };
		let pov_c = PoV { block_data: BlockData(vec![5, 6, 7]) };
		let validation_code_ab = ValidationCode(vec![1, 2, 3]);
		let validation_code_c = ValidationCode(vec![4, 5, 6]);

		let parent_head_data_a = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();
		let parent_head_data_b = {
			let mut head = parent_head_data_a.clone();
			head.0[0] = 98;
			head
		};
		let output_head_data_b = {
			let mut head = parent_head_data_a.clone();
			head.0[0] = 99;
			head
		};
		let parent_head_data_c = test_state.head_data.get(&test_state.chain_ids[1]).unwrap();
		let output_head_data_c = {
			let mut head = parent_head_data_c.clone();
			head.0[0] = 97;
			head
		};

		let pvd_a = PersistedValidationData {
			parent_head: parent_head_data_a.clone(),
			relay_parent_number: 0_u32.into(),
			max_pov_size: 1024,
			relay_parent_storage_root: dummy_hash(),
		};
		let pvd_b = PersistedValidationData {
			parent_head: parent_head_data_b.clone(),
			relay_parent_number: 0_u32.into(),
			max_pov_size: 1024,
			relay_parent_storage_root: dummy_hash(),
		};
		let pvd_c = PersistedValidationData {
			parent_head: parent_head_data_c.clone(),
			relay_parent_number: 0_u32.into(),
			max_pov_size: 1024,
			relay_parent_storage_root: dummy_hash(),
		};

		let candidate_a = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash: pov_a.hash(),
			head_data: parent_head_data_b.clone(),
			erasure_root: make_erasure_root(&test_state, pov_a.clone(), pvd_a.clone()),
			validation_code: validation_code_ab.0.clone(),
			persisted_validation_data_hash: pvd_a.hash(),
		}
		.build();
		let candidate_b = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash: pov_b.hash(),
			head_data: output_head_data_b.clone(),
			erasure_root: make_erasure_root(&test_state, pov_b.clone(), pvd_b.clone()),
			validation_code: validation_code_ab.0.clone(),
			persisted_validation_data_hash: pvd_b.hash(),
		}
		.build();
		let candidate_c = TestCandidateBuilder {
			para_id: test_state.chain_ids[1],
			relay_parent: test_state.relay_parent,
			pov_hash: pov_c.hash(),
			head_data: output_head_data_c.clone(),
			erasure_root: make_erasure_root(&test_state, pov_b.clone(), pvd_c.clone()),
			validation_code: validation_code_c.0.clone(),
			persisted_validation_data_hash: pvd_c.hash(),
		}
		.build();
		let candidate_a_hash = candidate_a.hash();
		let candidate_b_hash = candidate_b.hash();
		let candidate_c_hash = candidate_c.hash();

		// Back a chain of two candidates for the first paraid. Back one candidate for the second
		// paraid.
		for (candidate, pvd, validator_index) in [
			(candidate_a, pvd_a, ValidatorIndex(2)),
			(candidate_b, pvd_b, ValidatorIndex(1)),
			(candidate_c, pvd_c, ValidatorIndex(3)),
		] {
			let public = Keystore::sr25519_generate_new(
				&*test_state.keystore,
				ValidatorId::ID,
				Some(&test_state.validators[validator_index.0 as usize].to_seed()),
			)
			.expect("Insert key into keystore");

			let signed = SignedFullStatementWithPVD::sign(
				&test_state.keystore,
				StatementWithPVD::Seconded(candidate.clone(), pvd.clone()),
				&test_state.signing_context,
				validator_index,
				&public.into(),
			)
			.ok()
			.flatten()
			.expect("should be signed");

			let statement = CandidateBackingMessage::Statement {
				scheduling_parent: test_state.relay_parent,
				statement: signed.clone(),
			};

			virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

			// Prospective parachains are notified about candidate seconded first.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::ProspectiveParachains(
					ProspectiveParachainsMessage::IntroduceSecondedCandidate(
						req,
						tx,
					),
				) if
					req.candidate_receipt == candidate
					&& req.candidate_para == candidate.descriptor.para_id()
					&& pvd == req.persisted_validation_data => {
					tx.send(true).unwrap();
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::ProspectiveParachains(
					ProspectiveParachainsMessage::CandidateBacked(
						candidate_para_id, candidate_hash
					),
				) if candidate.hash() == candidate_hash && candidate_para_id == candidate.descriptor.para_id()
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::StatementDistribution(StatementDistributionMessage::Backed (
					candidate_hash
				)) if candidate.hash() == candidate_hash
			);
		}

		// Happy case, all candidates should be present.
		let (tx, rx) = oneshot::channel();
		let msg = CandidateBackingMessage::GetBackableCandidates {
			candidates: [
				(
					test_state.chain_ids[0],
					vec![
						BackableCandidateRef {
							candidate_hash: candidate_a_hash,
							scheduling_parent: test_state.relay_parent,
						},
						BackableCandidateRef {
							candidate_hash: candidate_b_hash,
							scheduling_parent: test_state.relay_parent,
						},
					],
				),
				(
					test_state.chain_ids[1],
					vec![BackableCandidateRef {
						candidate_hash: candidate_c_hash,
						scheduling_parent: test_state.relay_parent,
					}],
				),
			]
			.into_iter()
			.collect(),
			sender: tx,
		};
		virtual_overseer.send(FromOrchestra::Communication { msg }).await;
		let mut candidates = rx.await.unwrap();
		assert_eq!(2, candidates.len());
		assert_eq!(
			candidates
				.remove(&test_state.chain_ids[0])
				.unwrap()
				.iter()
				.map(|c| c.hash())
				.collect::<Vec<_>>(),
			vec![candidate_a_hash, candidate_b_hash]
		);
		assert_eq!(
			candidates
				.remove(&test_state.chain_ids[1])
				.unwrap()
				.iter()
				.map(|c| c.hash())
				.collect::<Vec<_>>(),
			vec![candidate_c_hash]
		);

		// The first candidate of the first para is invalid (we supply the wrong relay parent or a
		// wrong candidate hash). No candidates should be returned for paraid 1. ParaId 2 should be
		// fine.
		for candidates in [
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_a_hash,
					scheduling_parent: Hash::repeat_byte(9),
				},
				BackableCandidateRef {
					candidate_hash: candidate_b_hash,
					scheduling_parent: test_state.relay_parent,
				},
			],
			vec![
				BackableCandidateRef {
					candidate_hash: CandidateHash(Hash::repeat_byte(9)),
					scheduling_parent: test_state.relay_parent,
				},
				BackableCandidateRef {
					candidate_hash: candidate_b_hash,
					scheduling_parent: test_state.relay_parent,
				},
			],
		] {
			let (tx, rx) = oneshot::channel();
			let msg = CandidateBackingMessage::GetBackableCandidates {
				candidates: [
					(test_state.chain_ids[0], candidates),
					(
						test_state.chain_ids[1],
						vec![BackableCandidateRef {
							candidate_hash: candidate_c_hash,
							scheduling_parent: test_state.relay_parent,
						}],
					),
				]
				.into_iter()
				.collect(),
				sender: tx,
			};
			virtual_overseer.send(FromOrchestra::Communication { msg }).await;
			let mut candidates = rx.await.unwrap();
			assert_eq!(candidates.len(), 1);

			assert!(candidates.remove(&test_state.chain_ids[0]).is_none());
			assert_eq!(
				candidates
					.remove(&test_state.chain_ids[1])
					.unwrap()
					.iter()
					.map(|c| c.hash())
					.collect::<Vec<_>>(),
				vec![candidate_c_hash]
			);
		}

		// The second candidate of the first para is invalid (we supply the wrong relay parent or a
		// wrong candidate hash). The first candidate of the first para should still be present.
		// ParaId 2 is fine.
		for candidates in [
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_a_hash,
					scheduling_parent: test_state.relay_parent,
				},
				BackableCandidateRef {
					candidate_hash: candidate_b_hash,
					scheduling_parent: Hash::repeat_byte(9),
				},
			],
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_a_hash,
					scheduling_parent: test_state.relay_parent,
				},
				BackableCandidateRef {
					candidate_hash: CandidateHash(Hash::repeat_byte(9)),
					scheduling_parent: test_state.relay_parent,
				},
			],
		] {
			let (tx, rx) = oneshot::channel();
			let msg = CandidateBackingMessage::GetBackableCandidates {
				candidates: [
					(test_state.chain_ids[0], candidates),
					(
						test_state.chain_ids[1],
						vec![BackableCandidateRef {
							candidate_hash: candidate_c_hash,
							scheduling_parent: test_state.relay_parent,
						}],
					),
				]
				.into_iter()
				.collect(),
				sender: tx,
			};
			virtual_overseer.send(FromOrchestra::Communication { msg }).await;
			let mut candidates = rx.await.unwrap();
			assert_eq!(2, candidates.len());
			assert_eq!(
				candidates
					.remove(&test_state.chain_ids[0])
					.unwrap()
					.iter()
					.map(|c| c.hash())
					.collect::<Vec<_>>(),
				vec![candidate_a_hash]
			);
			assert_eq!(
				candidates
					.remove(&test_state.chain_ids[1])
					.unwrap()
					.iter()
					.map(|c| c.hash())
					.collect::<Vec<_>>(),
				vec![candidate_c_hash]
			);
		}

		// Both candidates of para id 1 are invalid (we supply the wrong relay parent or a wrong
		// candidate hash). No candidates should be returned for para id 1. Para Id 2 is fine.
		for candidates in [
			vec![
				BackableCandidateRef {
					candidate_hash: CandidateHash(Hash::repeat_byte(9)),
					scheduling_parent: test_state.relay_parent,
				},
				BackableCandidateRef {
					candidate_hash: CandidateHash(Hash::repeat_byte(10)),
					scheduling_parent: test_state.relay_parent,
				},
			],
			vec![
				BackableCandidateRef {
					candidate_hash: candidate_a_hash,
					scheduling_parent: Hash::repeat_byte(9),
				},
				BackableCandidateRef {
					candidate_hash: candidate_b_hash,
					scheduling_parent: Hash::repeat_byte(10),
				},
			],
		] {
			let (tx, rx) = oneshot::channel();
			let msg = CandidateBackingMessage::GetBackableCandidates {
				candidates: [
					(test_state.chain_ids[0], candidates),
					(
						test_state.chain_ids[1],
						vec![BackableCandidateRef {
							candidate_hash: candidate_c_hash,
							scheduling_parent: test_state.relay_parent,
						}],
					),
				]
				.into_iter()
				.collect(),
				sender: tx,
			};
			virtual_overseer.send(FromOrchestra::Communication { msg }).await;
			let mut candidates = rx.await.unwrap();
			assert_eq!(candidates.len(), 1);

			assert!(candidates.remove(&test_state.chain_ids[0]).is_none());
			assert_eq!(
				candidates
					.remove(&test_state.chain_ids[1])
					.unwrap()
					.iter()
					.map(|c| c.hash())
					.collect::<Vec<_>>(),
				vec![candidate_c_hash]
			);
		}

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::stop_work(test_state.relay_parent),
			)))
			.await;
		virtual_overseer
	});
}

#[test]
fn extract_core_index_from_statement_works() {
	let test_state = TestState::default();

	let pov_a = PoV { block_data: BlockData(vec![42, 43, 44]) };
	let pvd_a = dummy_pvd();
	let validation_code_a = ValidationCode(vec![1, 2, 3]);

	let pov_hash = pov_a.hash();

	let mut candidate = TestCandidateBuilder {
		para_id: test_state.chain_ids[0],
		relay_parent: test_state.relay_parent,
		pov_hash,
		erasure_root: make_erasure_root(&test_state, pov_a.clone(), pvd_a.clone()),
		persisted_validation_data_hash: pvd_a.hash(),
		validation_code: validation_code_a.0.clone(),
		..Default::default()
	}
	.build();

	let public2 = Keystore::sr25519_generate_new(
		&*test_state.keystore,
		ValidatorId::ID,
		Some(&test_state.validators[2].to_seed()),
	)
	.expect("Insert key into keystore");

	let signed_statement_1 = SignedFullStatementWithPVD::sign(
		&test_state.keystore,
		StatementWithPVD::Seconded(candidate.clone(), pvd_a.clone()),
		&test_state.signing_context,
		ValidatorIndex(2),
		&public2.into(),
	)
	.ok()
	.flatten()
	.expect("should be signed");

	let public1 = Keystore::sr25519_generate_new(
		&*test_state.keystore,
		ValidatorId::ID,
		Some(&test_state.validators[1].to_seed()),
	)
	.expect("Insert key into keystore");

	let signed_statement_2 = SignedFullStatementWithPVD::sign(
		&test_state.keystore,
		StatementWithPVD::Seconded(candidate.clone(), pvd_a.clone()),
		&test_state.signing_context,
		ValidatorIndex(1),
		&public1.into(),
	)
	.ok()
	.flatten()
	.expect("should be signed");

	candidate.descriptor.set_para_id(test_state.chain_ids[1]);

	let signed_statement_3 = SignedFullStatementWithPVD::sign(
		&test_state.keystore,
		StatementWithPVD::Seconded(candidate, pvd_a.clone()),
		&test_state.signing_context,
		ValidatorIndex(1),
		&public1.into(),
	)
	.ok()
	.flatten()
	.expect("should be signed");

	// Build a minimal PerSchedulingParentState for the test
	let groups: HashMap<CoreIndex, Vec<ValidatorIndex>> = test_state
		.validator_groups
		.0
		.iter()
		.enumerate()
		.map(|(i, g)| (CoreIndex(i as u32), g.clone()))
		.collect();

	let sp_state = PerSchedulingParentState {
		parent: test_state.relay_parent,
		node_features: test_state.node_features.clone(),
		assigned_core: None,
		backed: HashSet::new(),
		table: Table::new(),
		table_context: TableContext {
			validator: None,
			disabled_validators: Default::default(),
			groups,
			validators: test_state.validator_public.clone(),
		},
		issued_statements: HashSet::new(),
		awaiting_validation: HashSet::new(),
		fallbacks: HashMap::new(),
		minimum_backing_votes: test_state.minimum_backing_votes,
		n_cores: test_state.availability_cores.len() as u32,
		claim_queue: test_state.claim_queue.clone().into(),
		validator_to_group: Arc::new(test_state.validator_to_group.clone()),
		session_index: test_state.session(),
		group_rotation_info: test_state.validator_groups.1.clone(),
	};

	let core_index_1 = core_index_from_statement(&sp_state, &signed_statement_1).unwrap();

	assert_eq!(core_index_1, CoreIndex(0));

	let core_index_2 = core_index_from_statement(&sp_state, &signed_statement_2);

	// Must be none, para_id in descriptor is different than para assigned to core
	assert_eq!(core_index_2, None);

	let core_index_3 = core_index_from_statement(&sp_state, &signed_statement_3).unwrap();

	assert_eq!(core_index_3, CoreIndex(1));
}

#[test]
fn backing_works_while_validation_ongoing() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		let para_id = activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov_abc = PoV { block_data: BlockData(vec![1, 2, 3]) };
		let pvd_abc = dummy_pvd();
		let validation_code_abc = ValidationCode(vec![1, 2, 3]);

		let pov_hash = pov_abc.hash();

		let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

		let candidate_a = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov_abc.clone(), pvd_abc.clone()),
			validation_code: validation_code_abc.0.clone(),
			..Default::default()
		}
		.build();

		let candidate_a_hash = candidate_a.hash();
		let candidate_a_commitments_hash = candidate_a.commitments.hash();

		let public1 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[5].to_seed()),
		)
		.expect("Insert key into keystore");
		let public2 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[2].to_seed()),
		)
		.expect("Insert key into keystore");
		let public3 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[3].to_seed()),
		)
		.expect("Insert key into keystore");

		let signed_a = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate_a.clone(), pvd_abc.clone()),
			&test_state.signing_context,
			ValidatorIndex(2),
			&public2.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let signed_b = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Valid(candidate_a_hash),
			&test_state.signing_context,
			ValidatorIndex(5),
			&public1.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let signed_c = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Valid(candidate_a_hash),
			&test_state.signing_context,
			ValidatorIndex(3),
			&public3.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_a.clone(),
		};
		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate_a
				&& req.candidate_para == para_id
				&& pvd_abc == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_validation_request(&mut virtual_overseer, validation_code_abc.clone()).await;

		// Sending a `Statement::Seconded` for our assignment will start
		// validation process. The first thing requested is PoV from the
		// `PoVDistribution`.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::AvailabilityDistribution(
				AvailabilityDistributionMessage::FetchPoV {
					relay_parent,
					tx,
					..
				}
			) if relay_parent == test_state.relay_parent => {
				tx.send(pov_abc.clone()).unwrap();
			}
		);

		// The next step is the actual request to Validation subsystem
		// to validate the `Seconded` candidate.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CandidateValidation(
				CandidateValidationMessage::ValidateFromExhaustive {
					validation_data,
					validation_code,
					candidate_receipt,
					pov,
					exec_kind,
					response_sender,
					..
				},
			) if validation_data == pvd_abc &&
				validation_code == validation_code_abc &&
				*pov == pov_abc && candidate_receipt.descriptor == candidate_a.descriptor &&
				matches!(exec_kind, PvfExecKind::BackingSystemParas(_)) &&
				candidate_a_commitments_hash == candidate_receipt.commitments_hash =>
			{
				// we never validate the candidate. our local node
				// shouldn't issue any statements.
				std::mem::forget(response_sender);
			}
		);

		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_b.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		// Candidate gets backed entirely by other votes.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::CandidateBacked(
					candidate_para_id, candidate_hash
				),
			) if candidate_a_hash == candidate_hash && candidate_para_id == para_id
		);

		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_c.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		let (tx, rx) = oneshot::channel();
		let msg = CandidateBackingMessage::GetBackableCandidates {
			candidates: std::iter::once((
				test_state.chain_ids[0],
				vec![BackableCandidateRef {
					candidate_hash: candidate_a.hash(),
					scheduling_parent: test_state.relay_parent,
				}],
			))
			.collect(),
			sender: tx,
		};

		virtual_overseer.send(FromOrchestra::Communication { msg }).await;

		let mut candidates = rx.await.unwrap();
		assert_eq!(candidates.len(), 1);
		let candidates = candidates.remove(&test_state.chain_ids[0]).unwrap();
		assert_eq!(1, candidates.len());
		assert_eq!(candidates[0].validity_votes().len(), 3);

		assert!(candidates[0]
			.validity_votes()
			.contains(&ValidityAttestation::Implicit(signed_a.signature().clone())));
		assert!(candidates[0]
			.validity_votes()
			.contains(&ValidityAttestation::Explicit(signed_b.signature().clone())));
		assert!(candidates[0]
			.validity_votes()
			.contains(&ValidityAttestation::Explicit(signed_c.signature().clone())));
		assert_eq!(
			candidates[0].validator_indices_and_core_index(),
			(
				bitvec::bitvec![u8, bitvec::order::Lsb0; 1, 0, 1, 1].as_bitslice(),
				Some(CoreIndex(0))
			)
		);

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::stop_work(test_state.relay_parent),
			)))
			.await;
		virtual_overseer
	});
}

// Issuing conflicting statements on the same candidate should be a misbehavior.
#[test]
fn backing_misbehavior_works() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		let para_id = activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov_a = PoV { block_data: BlockData(vec![1, 2, 3]) };

		let pov_hash = pov_a.hash();
		let pvd_a = dummy_pvd();
		let validation_code_a = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

		let candidate_a = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			erasure_root: make_erasure_root(&test_state, pov_a.clone(), pvd_a.clone()),
			head_data: expected_head_data.clone(),
			validation_code: validation_code_a.0.clone(),
			..Default::default()
		}
		.build();

		let candidate_a_hash = candidate_a.hash();
		let public2 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[2].to_seed()),
		)
		.expect("Insert key into keystore");
		let seconded_2 = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate_a.clone(), pvd_a.clone()),
			&test_state.signing_context,
			ValidatorIndex(2),
			&public2.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let valid_2 = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Valid(candidate_a_hash),
			&test_state.signing_context,
			ValidatorIndex(2),
			&public2.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: seconded_2.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		// Prospective parachains are notified about candidate seconded first.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate_a
				&& req.candidate_para == para_id
				&& pvd_a == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			test_state.relay_parent,
			&candidate_a,
			&pov_a,
			&pvd_a,
			&validation_code_a,
			expected_head_data,
			true,
		)
		.await;

		assert_candidate_is_shared_and_backed(
			&mut virtual_overseer,
			&test_state.relay_parent,
			&para_id,
			&candidate_a_hash,
		)
		.await;

		// This `Valid` statement is redundant after the `Seconded` statement already sent.
		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: valid_2.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::Provisioner(
				ProvisionerMessage::ProvisionableData(
					_,
					ProvisionableData::MisbehaviorReport(
						relay_parent,
						validator_index,
						Misbehavior::ValidityDoubleVote(vdv),
					)
				)
			) if relay_parent == test_state.relay_parent => {
				let ((t1, s1), (t2, s2)) = vdv.deconstruct::<TableContext>();
				let t1 = table_statement_to_primitive(t1);
				let t2 = table_statement_to_primitive(t2);

				SignedFullStatement::new(
					t1,
					validator_index,
					s1,
					&test_state.signing_context,
					&test_state.validator_public[validator_index.0 as usize],
				).expect("signature must be valid");

				SignedFullStatement::new(
					t2,
					validator_index,
					s2,
					&test_state.signing_context,
					&test_state.validator_public[validator_index.0 as usize],
				).expect("signature must be valid");
			}
		);
		virtual_overseer
	});
}

// Test that if we are asked to second an invalid candidate we can still second a valid one
// afterwards.
#[test]
fn backing_doesnt_second_invalid() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		let para_id = activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov_block_a = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd_a = dummy_pvd();
		let validation_code_a = ValidationCode(vec![1, 2, 3]);

		let pov_block_b = PoV { block_data: BlockData(vec![45, 46, 47]) };
		let pvd_b = {
			let mut pvd_b = pvd_a.clone();
			pvd_b.parent_head = HeadData(vec![14, 15, 16]);
			pvd_b.max_pov_size = pvd_a.max_pov_size / 2;
			pvd_b
		};
		let validation_code_b = ValidationCode(vec![4, 5, 6]);

		let pov_hash_a = pov_block_a.hash();
		let pov_hash_b = pov_block_b.hash();

		let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

		let candidate_a = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash: pov_hash_a,
			erasure_root: make_erasure_root(&test_state, pov_block_a.clone(), pvd_a.clone()),
			persisted_validation_data_hash: pvd_a.hash(),
			validation_code: validation_code_a.0.clone(),
			..Default::default()
		}
		.build();

		let candidate_b = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash: pov_hash_b,
			erasure_root: make_erasure_root(&test_state, pov_block_b.clone(), pvd_b.clone()),
			head_data: expected_head_data.clone(),
			persisted_validation_data_hash: pvd_b.hash(),
			validation_code: validation_code_b.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second {
			scheduling_parent: test_state.relay_parent,
			candidate: candidate_a.to_plain(),
			pvd: pvd_a.clone(),
			pov: pov_block_a.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validation_request(&mut virtual_overseer, validation_code_a.clone()).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CandidateValidation(
				CandidateValidationMessage::ValidateFromExhaustive {
					validation_data,
					validation_code,
					candidate_receipt,
					pov,
					exec_kind,
					response_sender,
					..
				},
			) if validation_data == pvd_a &&
				validation_code == validation_code_a &&
				*pov == pov_block_a && candidate_receipt.descriptor == candidate_a.descriptor &&
				matches!(exec_kind, PvfExecKind::BackingSystemParas(_)) &&
				candidate_a.commitments.hash() == candidate_receipt.commitments_hash =>
			{
				response_sender.send(Ok(ValidationResult::Invalid(InvalidCandidate::BadReturn))).unwrap();
			}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CollatorProtocol(
				CollatorProtocolMessage::Invalid(parent_hash, c)
			) if parent_hash == test_state.relay_parent && c == candidate_a.to_plain()
		);

		let second = CandidateBackingMessage::Second {
			scheduling_parent: test_state.relay_parent,
			candidate: candidate_b.to_plain(),
			pvd: pvd_b.clone(),
			pov: pov_block_b.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validation_request(&mut virtual_overseer, validation_code_b.clone()).await;

		assert_validate_from_exhaustive(
			&mut virtual_overseer,
			&pvd_b,
			&pov_block_b,
			&validation_code_b,
			&candidate_b,
			expected_head_data,
			test_state.validation_data.clone(),
		)
		.await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::AvailabilityStore(
				AvailabilityStoreMessage::StoreAvailableData { candidate_hash, tx, .. }
			) if candidate_hash == candidate_b.hash() => {
				tx.send(Ok(())).unwrap();
			}
		);

		let hypothetical_candidate_b = HypotheticalCandidate::Complete {
			candidate_hash: candidate_b.hash(),
			receipt: Arc::new(candidate_b.clone()),
			persisted_validation_data: pvd_a.clone(), // ???
		};
		let expected_request_b = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate_b.clone()],
			fragment_chain_relay_parent: Some(test_state.relay_parent),
		};
		let expected_response_b = make_hypothetical_membership_response(
			hypothetical_candidate_b.clone(),
			test_state.relay_parent,
		);

		assert_hypothetical_membership_requests(
			&mut virtual_overseer,
			vec![
				// (expected_request_a, expected_response_a),
				(expected_request_b, expected_response_b),
			],
		)
		.await;

		// Prospective parachains are notified.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) => {
				assert_eq!(req.candidate_receipt, candidate_b);
				assert_eq!(req.candidate_para, para_id);
				assert_eq!(pvd_a, req.persisted_validation_data); // ???
				tx.send(true).unwrap();
			}
		);

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::stop_work(test_state.relay_parent),
			)))
			.await;
		virtual_overseer
	});
}

// Test that if we have already issued a statement (in this case `Invalid`) about a candidate we
// will not be issuing a `Seconded` statement on it.
#[test]
fn backing_second_after_first_fails_works() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		let para_id = activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov_a = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd_a = dummy_pvd();
		let validation_code_a = ValidationCode(vec![1, 2, 3]);

		let pov_hash = pov_a.hash();

		let candidate = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			erasure_root: make_erasure_root(&test_state, pov_a.clone(), pvd_a.clone()),
			persisted_validation_data_hash: pvd_a.hash(),
			validation_code: validation_code_a.0.clone(),
			..Default::default()
		}
		.build();

		let validator2 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[2].to_seed()),
		)
		.expect("Insert key into keystore");

		let signed_a = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate.clone(), pvd_a.clone()),
			&test_state.signing_context,
			ValidatorIndex(2),
			&validator2.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		// Send in a `Statement` with a candidate.
		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_a.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd_a == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_validation_request(&mut virtual_overseer, validation_code_a.clone()).await;

		// Subsystem requests PoV and requests validation.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::AvailabilityDistribution(
				AvailabilityDistributionMessage::FetchPoV {
					relay_parent,
					tx,
					..
				}
			) if relay_parent == test_state.relay_parent => {
				tx.send(pov_a.clone()).unwrap();
			}
		);

		// Tell subsystem that this candidate is invalid.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CandidateValidation(
				CandidateValidationMessage::ValidateFromExhaustive {
					validation_data,
					validation_code,
					candidate_receipt,
					pov,
					exec_kind,
					response_sender,
					..
				},
			) if validation_data == pvd_a &&
				validation_code == validation_code_a &&
				*pov == pov_a && candidate_receipt.descriptor == candidate.descriptor &&
				matches!(exec_kind, PvfExecKind::BackingSystemParas(_)) &&
				candidate.commitments.hash() == candidate_receipt.commitments_hash =>
			{
				response_sender.send(Ok(ValidationResult::Invalid(InvalidCandidate::BadReturn))).unwrap();
			}
		);

		// Ask subsystem to `Second` a candidate that already has a statement issued about.
		// This should emit no actions from subsystem.
		let second = CandidateBackingMessage::Second {
			scheduling_parent: test_state.relay_parent,
			candidate: candidate.to_plain(),
			pvd: pvd_a.clone(),
			pov: pov_a.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		let pov_to_second = PoV { block_data: BlockData(vec![3, 2, 1]) };
		let pvd_to_second = dummy_pvd();
		let validation_code_to_second = ValidationCode(vec![5, 6, 7]);

		let pov_hash = pov_to_second.hash();

		let candidate_to_second = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			erasure_root: make_erasure_root(
				&test_state,
				pov_to_second.clone(),
				pvd_to_second.clone(),
			),
			persisted_validation_data_hash: pvd_to_second.hash(),
			validation_code: validation_code_to_second.0.clone(),
			..Default::default()
		}
		.build();

		let second = CandidateBackingMessage::Second {
			scheduling_parent: test_state.relay_parent,
			candidate: candidate_to_second.to_plain(),
			pvd: pvd_to_second.clone(),
			pov: pov_to_second.clone(),
		};

		// In order to trigger _some_ actions from subsystem ask it to second another
		// candidate. The only reason to do so is to make sure that no actions were
		// triggered on the prev step.
		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validation_request(&mut virtual_overseer, validation_code_to_second.clone()).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CandidateValidation(
				CandidateValidationMessage::ValidateFromExhaustive { pov, .. },
			) => {
				assert_eq!(&*pov, &pov_to_second);
			}
		);
		virtual_overseer
	});
}

// Test that if the validation of the candidate has failed this does not stop the work of this
// subsystem and so it is not fatal to the node.
#[test]
fn backing_works_after_failed_validation() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		let para_id = activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov_a = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd_a = dummy_pvd();
		let validation_code_a = ValidationCode(vec![1, 2, 3]);

		let pov_hash = pov_a.hash();

		let candidate = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			erasure_root: make_erasure_root(&test_state, pov_a.clone(), pvd_a.clone()),
			validation_code: validation_code_a.0.clone(),
			..Default::default()
		}
		.build();

		let public2 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[2].to_seed()),
		)
		.expect("Insert key into keystore");
		let signed_a = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate.clone(), pvd_a.clone()),
			&test_state.signing_context,
			ValidatorIndex(2),
			&public2.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		// Send in a `Statement` with a candidate.
		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_a.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd_a == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_validation_request(&mut virtual_overseer, validation_code_a.clone()).await;

		// Subsystem requests PoV and requests validation.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::AvailabilityDistribution(
				AvailabilityDistributionMessage::FetchPoV {
					relay_parent,
					tx,
					..
				}
			) if relay_parent == test_state.relay_parent => {
				tx.send(pov_a.clone()).unwrap();
			}
		);

		// Tell subsystem that this candidate is invalid.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CandidateValidation(
				CandidateValidationMessage::ValidateFromExhaustive {
					validation_data,
					validation_code,
					candidate_receipt,
					pov,
					exec_kind,
					response_sender,
					..
				},
			) if validation_data == pvd_a &&
				validation_code == validation_code_a &&
				*pov == pov_a && candidate_receipt.descriptor == candidate.descriptor &&
				matches!(exec_kind, PvfExecKind::BackingSystemParas(_)) &&
				candidate.commitments.hash() == candidate_receipt.commitments_hash =>
			{
				response_sender.send(Err(ValidationFailed("Internal test error".into()))).unwrap();
			}
		);

		// Try to get a set of backable candidates to trigger _some_ action in the subsystem
		// and check that it is still alive.
		let (tx, rx) = oneshot::channel();
		let msg = CandidateBackingMessage::GetBackableCandidates {
			candidates: std::iter::once((
				test_state.chain_ids[0],
				vec![BackableCandidateRef {
					candidate_hash: candidate.hash(),
					scheduling_parent: test_state.relay_parent,
				}],
			))
			.collect(),
			sender: tx,
		};

		virtual_overseer.send(FromOrchestra::Communication { msg }).await;
		assert_eq!(rx.await.unwrap().len(), 0);
		virtual_overseer
	});
}

#[test]
fn candidate_backing_reorders_votes() {
	use sp_core::Encode;

	let core_idx = CoreIndex(10);
	let validators = vec![
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Ferdie,
		Sr25519Keyring::One,
	];

	let validator_public = validator_pubkeys(&validators);
	let validator_groups = {
		let mut validator_groups = HashMap::new();
		validator_groups
			.insert(core_idx, vec![0, 1, 2, 3, 4, 5].into_iter().map(ValidatorIndex).collect());
		validator_groups
	};

	let table_context = TableContext {
		validator: None,
		disabled_validators: Vec::new(),
		groups: validator_groups,
		validators: validator_public.clone(),
	};

	let fake_attestation = |idx: u32| {
		let candidate =
			dummy_candidate_receipt_bad_sig(Default::default(), Some(Default::default()));
		let hash = candidate.hash();
		let mut data = vec![0; 64];
		data[0..32].copy_from_slice(hash.0.as_bytes());
		data[32..36].copy_from_slice(idx.encode().as_slice());

		let sig = ValidatorSignature::try_from(data).unwrap();
		polkadot_statement_table::generic::ValidityAttestation::Implicit(sig)
	};

	let attested = TableAttestedCandidate {
		candidate: dummy_committed_candidate_receipt_v2(dummy_hash()),
		validity_votes: vec![
			(ValidatorIndex(5), fake_attestation(5)),
			(ValidatorIndex(3), fake_attestation(3)),
			(ValidatorIndex(1), fake_attestation(1)),
		],
		group_id: core_idx,
	};

	let backed = table_attested_to_backed(attested, &table_context).unwrap();

	let expected_bitvec = {
		let mut validator_indices = BitVec::<u8, bitvec::order::Lsb0>::with_capacity(6);
		validator_indices.resize(6, false);

		validator_indices.set(1, true);
		validator_indices.set(3, true);
		validator_indices.set(5, true);

		validator_indices
	};

	// Should be in bitfield order, which is opposite to the order provided to the function.
	let expected_attestations =
		vec![fake_attestation(1).into(), fake_attestation(3).into(), fake_attestation(5).into()];

	assert_eq!(
		backed.validator_indices_and_core_index(),
		(expected_bitvec.as_bitslice(), Some(CoreIndex(10)))
	);
	assert_eq!(backed.validity_votes(), expected_attestations);
}

// Test whether we retry on failed PoV fetching.
#[test]
fn retry_works() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		let para_id = activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov_a = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd_a = dummy_pvd();
		let validation_code_a = ValidationCode(vec![1, 2, 3]);

		let pov_hash = pov_a.hash();
		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let candidate = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			erasure_root: make_erasure_root(&test_state, pov_a.clone(), pvd_a.clone()),
			persisted_validation_data_hash: pvd_a.hash(),
			validation_code: validation_code_a.0.clone(),
			head_data: expected_head_data.clone(),
		}
		.build();

		let public2 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[2].to_seed()),
		)
		.expect("Insert key into keystore");
		let public3 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[3].to_seed()),
		)
		.expect("Insert key into keystore");
		let public5 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[5].to_seed()),
		)
		.expect("Insert key into keystore");
		let signed_a = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate.clone(), pvd_a.clone()),
			&test_state.signing_context,
			ValidatorIndex(2),
			&public2.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");
		let signed_b = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Valid(candidate.hash()),
			&test_state.signing_context,
			ValidatorIndex(3),
			&public3.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");
		let signed_c = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Valid(candidate.hash()),
			&test_state.signing_context,
			ValidatorIndex(5),
			&public5.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		// Send in a `Statement` with a candidate.
		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_a.clone(),
		};
		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd_a == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_validation_request(&mut virtual_overseer, validation_code_a.clone()).await;

		// Subsystem requests PoV and requests validation.
		// We cancel - should mean retry on next backing statement.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::AvailabilityDistribution(
				AvailabilityDistributionMessage::FetchPoV {
					relay_parent,
					tx,
					..
				}
			) if relay_parent == test_state.relay_parent => {
				std::mem::drop(tx);
			}
		);

		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_b.clone(),
		};
		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		// Not deterministic which message comes first:
		for _ in 0u32..3 {
			match virtual_overseer.recv().await {
				AllMessages::ProspectiveParachains(
					ProspectiveParachainsMessage::CandidateBacked(
						candidate_para_id,
						candidate_hash,
					),
				) if candidate_hash == candidate_hash && candidate_para_id == para_id => {
					assert_eq!(candidate_para_id, para_id);
					assert_eq!(candidate_hash, candidate.hash());
				},
				AllMessages::AvailabilityDistribution(
					AvailabilityDistributionMessage::FetchPoV { relay_parent, tx, .. },
				) if relay_parent == test_state.relay_parent => {
					std::mem::drop(tx);
				},
				AllMessages::StatementDistribution(StatementDistributionMessage::Backed(
					candidate_hash,
				)) if candidate_hash == candidate.hash() => {},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_,
					RuntimeApiRequest::ValidationCodeByHash(hash, tx),
				)) if hash == validation_code_a.hash() => {
					tx.send(Ok(Some(validation_code_a.clone()))).unwrap();
				},
				msg => {
					assert!(false, "Unexpected message: {:?}", msg);
				},
			}
		}

		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_c.clone(),
		};
		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::AvailabilityDistribution(
				AvailabilityDistributionMessage::FetchPoV {
					relay_parent,
					tx,
					..
				}
				// Subsystem requests PoV and requests validation.
				// Now we pass.
				) if relay_parent == test_state.relay_parent => {
					tx.send(pov_a.clone()).unwrap();
				}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CandidateValidation(
				CandidateValidationMessage::ValidateFromExhaustive {
					validation_data,
					validation_code,
					candidate_receipt,
					pov,
					exec_kind,
					..
				},
			) if validation_data == pvd_a &&
				validation_code == validation_code_a &&
				*pov == pov_a && candidate_receipt.descriptor == candidate.descriptor &&
				matches!(exec_kind, PvfExecKind::BackingSystemParas(_)) &&
				candidate.commitments.hash() == candidate_receipt.commitments_hash
		);
		virtual_overseer
	});
}

#[test]
fn observes_backing_even_if_not_validator() {
	let mut test_state = TestState::default();
	let empty_keystore = Arc::new(sc_keystore::LocalKeystore::in_memory());
	test_harness(empty_keystore, |mut virtual_overseer| async move {
		let para_id = activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![1, 2, 3]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let pov_hash = pov.hash();

		let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

		let candidate_a = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let candidate_a_hash = candidate_a.hash();
		let public0 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[0].to_seed()),
		)
		.expect("Insert key into keystore");
		let public1 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[5].to_seed()),
		)
		.expect("Insert key into keystore");
		let public2 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[2].to_seed()),
		)
		.expect("Insert key into keystore");

		// Produce a 3-of-5 quorum on the candidate.

		let signed_a = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate_a.clone(), pvd.clone()),
			&test_state.signing_context,
			ValidatorIndex(0),
			&public0.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let signed_b = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Valid(candidate_a_hash),
			&test_state.signing_context,
			ValidatorIndex(5),
			&public1.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let signed_c = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Valid(candidate_a_hash),
			&test_state.signing_context,
			ValidatorIndex(2),
			&public2.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_a.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		// Prospective parachains are notified about candidate seconded first.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate_a
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_b.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::CandidateBacked(
					candidate_para_id, candidate_hash
				),
			) if candidate_a_hash == candidate_hash && candidate_para_id == para_id
		);

		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_c.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::stop_work(test_state.relay_parent),
			)))
			.await;
		virtual_overseer
	});
}

#[test]
fn new_leaf_view_doesnt_clobber_old() {
	let mut test_state = TestState::default();
	let relay_parent_2 = Hash::repeat_byte(1);
	assert_ne!(test_state.relay_parent, relay_parent_2);
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		// New leaf that doesn't clobber old.
		{
			let old_relay_parent = test_state.relay_parent;
			test_state.relay_parent = relay_parent_2;

			const LEAF_B_BLOCK_NUMBER: BlockNumber = 101;
			const LEAF_B_ANCESTRY_LEN: BlockNumber = 3;
			let para_id = test_state.chain_ids[0];
			let activated = new_leaf(test_state.relay_parent, LEAF_B_BLOCK_NUMBER - 1);
			let min_relay_parents = vec![(para_id, LEAF_B_BLOCK_NUMBER - LEAF_B_ANCESTRY_LEN)];
			let test_leaf_b = TestLeaf { activated, min_relay_parents };

			activate_leaf(&mut virtual_overseer, test_leaf_b, &mut test_state).await;
			test_state.relay_parent = old_relay_parent;
		}

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second {
			scheduling_parent: test_state.relay_parent,
			candidate: candidate.to_plain(),
			pvd: pvd.clone(),
			pov: pov.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		// If the old leaf was clobbered by the first, the seconded candidate
		// would be ignored.
		assert!(
			virtual_overseer
				.recv()
				.timeout(std::time::Duration::from_millis(500))
				.await
				.is_some(),
			"first leaf appears to be inactive"
		);

		virtual_overseer
	});
}

// Test that a disabled local validator doesn't do any work on `CandidateBackingMessage::Second`
#[test]
fn disabled_validator_doesnt_distribute_statement_on_receiving_second() {
	let mut test_state = TestState::default();
	test_state.disabled_validators.push(ValidatorIndex(0));

	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second {
			scheduling_parent: test_state.relay_parent,
			candidate: candidate.to_plain(),
			pvd: pvd.clone(),
			pov: pov.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		// Ensure backing subsystem is not doing any work
		assert_matches!(virtual_overseer.recv().timeout(Duration::from_secs(1)).await, None);

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::stop_work(test_state.relay_parent),
			)))
			.await;
		virtual_overseer
	});
}

// Test that a disabled local validator doesn't do any work on `CandidateBackingMessage::Statement`
#[test]
fn disabled_validator_doesnt_distribute_statement_on_receiving_statement() {
	let mut test_state = TestState::default();
	test_state.disabled_validators.push(ValidatorIndex(0));

	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		let para_id = activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let public2 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[2].to_seed()),
		)
		.expect("Insert key into keystore");

		let signed = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate.clone(), pvd.clone()),
			&test_state.signing_context,
			ValidatorIndex(2),
			&public2.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		// Ensure backing subsystem is not doing any work
		assert_matches!(virtual_overseer.recv().timeout(Duration::from_secs(1)).await, None);

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::stop_work(test_state.relay_parent),
			)))
			.await;
		virtual_overseer
	});
}

// Test that a validator doesn't do any work on receiving a `CandidateBackingMessage::Statement`
// from a disabled validator
#[test]
fn validator_ignores_statements_from_disabled_validators() {
	let mut test_state = TestState::default();
	test_state.disabled_validators.push(ValidatorIndex(2));

	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		let para_id = activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let public2 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[2].to_seed()),
		)
		.expect("Insert key into keystore");

		let signed_2 = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate.clone(), pvd.clone()),
			&test_state.signing_context,
			ValidatorIndex(2),
			&public2.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let statement_2 = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_2.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement_2 }).await;

		// Ensure backing subsystem is not doing any work
		assert_matches!(virtual_overseer.recv().timeout(Duration::from_secs(1)).await, None);

		// Now send a statement from a honest validator and make sure it gets processed
		let public3 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[3].to_seed()),
		)
		.expect("Insert key into keystore");

		let signed_3 = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate.clone(), pvd.clone()),
			&test_state.signing_context,
			ValidatorIndex(3),
			&public3.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let statement_3 = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_3.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement_3 }).await;

		// Prospective parachains are notified about candidate seconded first.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			test_state.relay_parent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			true,
		)
		.await;

		assert_candidate_is_shared_and_backed(
			&mut virtual_overseer,
			&test_state.relay_parent,
			&para_id,
			&candidate.hash(),
		)
		.await;

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::stop_work(test_state.relay_parent),
			)))
			.await;
		virtual_overseer
	});
}

// Test that `seconding_sanity_check` works when a candidate is allowed
// for all leaves.
#[test]
fn seconding_sanity_check_allowed_on_all() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate is seconded in a parent of the activated `leaf_a`.
		const LEAF_A_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_A_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];

		// `a` is grandparent of `b`.
		let leaf_a_hash = Hash::from_low_u64_be(130);
		let leaf_a_parent = get_parent_hash(leaf_a_hash);
		let activated = new_leaf(leaf_a_hash, LEAF_A_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_A_BLOCK_NUMBER - LEAF_A_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		const LEAF_B_BLOCK_NUMBER: BlockNumber = LEAF_A_BLOCK_NUMBER + 2;
		const LEAF_B_ANCESTRY_LEN: BlockNumber = 4;

		let leaf_b_hash = Hash::from_low_u64_be(128);
		let activated = new_leaf(leaf_b_hash, LEAF_B_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_B_BLOCK_NUMBER - LEAF_B_ANCESTRY_LEN)];
		let test_leaf_b = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;
		activate_leaf(&mut virtual_overseer, test_leaf_b, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_a_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second {
			scheduling_parent: leaf_a_hash,
			candidate: candidate.to_plain(),
			pvd: pvd.clone(),
			pov: pov.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			leaf_a_parent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			false,
		)
		.await;

		// `seconding_sanity_check`
		let hypothetical_candidate = HypotheticalCandidate::Complete {
			candidate_hash: candidate.hash(),
			receipt: Arc::new(candidate.clone()),
			persisted_validation_data: pvd.clone(),
		};
		let expected_request_a = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(leaf_a_hash),
		};
		let expected_response_a =
			make_hypothetical_membership_response(hypothetical_candidate.clone(), leaf_a_hash);
		let expected_request_b = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(leaf_b_hash),
		};
		let expected_response_b =
			make_hypothetical_membership_response(hypothetical_candidate, leaf_b_hash);
		assert_hypothetical_membership_requests(
			&mut virtual_overseer,
			vec![
				(expected_request_a, expected_response_a),
				(expected_request_b, expected_response_b),
			],
		)
		.await;
		// Prospective parachains are notified.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_candidate_is_shared_and_seconded(&mut virtual_overseer, &leaf_a_parent).await;

		virtual_overseer
	});
}

// Test that `seconding_sanity_check` disallows seconding when a candidate is disallowed
// for all leaves.
#[test]
fn seconding_sanity_check_disallowed() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate is seconded in a parent of the activated `leaf_a`.
		const LEAF_A_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_A_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];

		let leaf_b_hash = Hash::from_low_u64_be(128);
		// `a` is grandparent of `b`.
		let leaf_a_hash = Hash::from_low_u64_be(130);
		let leaf_a_parent = get_parent_hash(leaf_a_hash);
		let activated = new_leaf(leaf_a_hash, LEAF_A_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_A_BLOCK_NUMBER - LEAF_A_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		const LEAF_B_BLOCK_NUMBER: BlockNumber = LEAF_A_BLOCK_NUMBER + 2;
		const LEAF_B_ANCESTRY_LEN: BlockNumber = 4;

		let activated = new_leaf(leaf_b_hash, LEAF_B_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_B_BLOCK_NUMBER - LEAF_B_ANCESTRY_LEN)];
		let test_leaf_b = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&para_id).unwrap().clone();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_a_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second {
			scheduling_parent: leaf_a_hash,
			candidate: candidate.to_plain(),
			pvd: pvd.clone(),
			pov: pov.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			leaf_a_parent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			&expected_head_data,
			false,
		)
		.await;

		// `seconding_sanity_check`
		let hypothetical_candidate = HypotheticalCandidate::Complete {
			candidate_hash: candidate.hash(),
			receipt: Arc::new(candidate.clone()),
			persisted_validation_data: pvd.clone(),
		};
		let expected_request_a = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(leaf_a_hash),
		};
		let expected_response_a =
			make_hypothetical_membership_response(hypothetical_candidate, leaf_a_hash);
		assert_hypothetical_membership_requests(
			&mut virtual_overseer,
			vec![(expected_request_a, expected_response_a)],
		)
		.await;
		// Prospective parachains are notified.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_candidate_is_shared_and_seconded(&mut virtual_overseer, &leaf_a_parent).await;

		activate_leaf(&mut virtual_overseer, test_leaf_b, &mut test_state).await;
		let leaf_a_grandparent = get_parent_hash(leaf_a_parent);
		let candidate = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_a_grandparent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second {
			scheduling_parent: leaf_a_hash,
			candidate: candidate.to_plain(),
			pvd: pvd.clone(),
			pov: pov.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			leaf_a_grandparent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			&expected_head_data,
			false,
		)
		.await;

		// `seconding_sanity_check`

		let hypothetical_candidate = HypotheticalCandidate::Complete {
			candidate_hash: candidate.hash(),
			receipt: Arc::new(candidate),
			persisted_validation_data: pvd,
		};
		let expected_request_a = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(leaf_a_hash),
		};
		let expected_empty_response = vec![(hypothetical_candidate.clone(), vec![])];
		let expected_request_b = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(leaf_b_hash),
		};
		assert_hypothetical_membership_requests(
			&mut virtual_overseer,
			vec![
				(expected_request_a, expected_empty_response.clone()),
				(expected_request_b, expected_empty_response),
			],
		)
		.await;

		assert!(virtual_overseer
			.recv()
			.timeout(std::time::Duration::from_millis(50))
			.await
			.is_none());

		virtual_overseer
	});
}

// Test that `seconding_sanity_check` allows seconding a candidate when it's allowed on at least one
// leaf.
#[test]
fn seconding_sanity_check_allowed_on_at_least_one_leaf() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate is seconded in a parent of the activated `leaf_a`.
		const LEAF_A_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_A_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];

		// `a` is grandparent of `b`.
		let leaf_a_hash = Hash::from_low_u64_be(130);
		let leaf_a_parent = get_parent_hash(leaf_a_hash);
		let activated = new_leaf(leaf_a_hash, LEAF_A_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_A_BLOCK_NUMBER - LEAF_A_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		const LEAF_B_BLOCK_NUMBER: BlockNumber = LEAF_A_BLOCK_NUMBER + 2;
		const LEAF_B_ANCESTRY_LEN: BlockNumber = 4;

		let leaf_b_hash = Hash::from_low_u64_be(128);
		let activated = new_leaf(leaf_b_hash, LEAF_B_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_B_BLOCK_NUMBER - LEAF_B_ANCESTRY_LEN)];
		let test_leaf_b = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;
		activate_leaf(&mut virtual_overseer, test_leaf_b, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_a_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second {
			scheduling_parent: leaf_a_hash,
			candidate: candidate.to_plain(),
			pvd: pvd.clone(),
			pov: pov.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			leaf_a_parent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			false,
		)
		.await;

		// `seconding_sanity_check`
		let hypothetical_candidate = HypotheticalCandidate::Complete {
			candidate_hash: candidate.hash(),
			receipt: Arc::new(candidate.clone()),
			persisted_validation_data: pvd.clone(),
		};
		let expected_request_a = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(leaf_a_hash),
		};
		let expected_response_a =
			make_hypothetical_membership_response(hypothetical_candidate.clone(), leaf_a_hash);
		let expected_request_b = HypotheticalMembershipRequest {
			candidates: vec![hypothetical_candidate.clone()],
			fragment_chain_relay_parent: Some(leaf_b_hash),
		};
		let expected_response_b = vec![(hypothetical_candidate.clone(), vec![])];
		assert_hypothetical_membership_requests(
			&mut virtual_overseer,
			vec![
				(expected_request_a, expected_response_a),
				(expected_request_b, expected_response_b),
			],
		)
		.await;
		// Prospective parachains are notified.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_candidate_is_shared_and_seconded(&mut virtual_overseer, &leaf_a_parent).await;

		virtual_overseer
	});
}

// Test that a seconded candidate which is not approved by prospective parachains
// subsystem doesn't change the view.
#[test]
fn prospective_parachains_reject_candidate() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate is seconded in a parent of the activated `leaf_a`.
		const LEAF_A_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_A_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];

		let leaf_a_hash = Hash::from_low_u64_be(130);
		let leaf_a_parent = get_parent_hash(leaf_a_hash);
		let activated = new_leaf(leaf_a_hash, LEAF_A_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_A_BLOCK_NUMBER - LEAF_A_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_a_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second {
			scheduling_parent: leaf_a_hash,
			candidate: candidate.to_plain(),
			pvd: pvd.clone(),
			pov: pov.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			leaf_a_parent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			false,
		)
		.await;

		// `seconding_sanity_check`
		let hypothetical_candidate = HypotheticalCandidate::Complete {
			candidate_hash: candidate.hash(),
			receipt: Arc::new(candidate.clone()),
			persisted_validation_data: pvd.clone(),
		};
		let expected_request_a = vec![(
			HypotheticalMembershipRequest {
				candidates: vec![hypothetical_candidate.clone()],
				fragment_chain_relay_parent: Some(leaf_a_hash),
			},
			make_hypothetical_membership_response(hypothetical_candidate, leaf_a_hash),
		)];
		assert_hypothetical_membership_requests(&mut virtual_overseer, expected_request_a.clone())
			.await;

		// Prospective parachains are notified.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				// Reject it.
				tx.send(false).unwrap();
			}
		);

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CollatorProtocol(CollatorProtocolMessage::Invalid(
				relay_parent,
				candidate_receipt,
			)) if candidate_receipt.descriptor() == &candidate.descriptor &&
				candidate_receipt.commitments_hash == candidate.commitments.hash() &&
				relay_parent == leaf_a_parent
		);

		// Try seconding the same candidate.

		let second = CandidateBackingMessage::Second {
			scheduling_parent: leaf_a_hash,
			candidate: candidate.to_plain(),
			pvd: pvd.clone(),
			pov: pov.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			leaf_a_parent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			false,
		)
		.await;

		// `seconding_sanity_check`
		assert_hypothetical_membership_requests(&mut virtual_overseer, expected_request_a).await;
		// Prospective parachains are notified.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_candidate_is_shared_and_seconded(&mut virtual_overseer, &leaf_a_parent).await;

		virtual_overseer
	});
}

// Test that a validator can second multiple candidates per single relay parent.
#[test]
fn second_multiple_candidates_per_relay_parent() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate `a` is seconded in a parent of the activated `leaf`.
		const LEAF_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];

		let leaf_hash = Hash::from_low_u64_be(130);
		let leaf_parent = get_parent_hash(leaf_hash);
		let leaf_grandparent = get_parent_hash(leaf_parent);
		let activated = new_leaf(leaf_hash, LEAF_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_BLOCK_NUMBER - LEAF_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let pov_hash = pov.hash();
		let candidate_a = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		};
		let mut candidate_b = candidate_a.clone();
		candidate_b.relay_parent = leaf_grandparent;

		let candidate_a = candidate_a.build();
		let candidate_b = candidate_b.build();

		for candidate in &[candidate_a, candidate_b] {
			let second = CandidateBackingMessage::Second {
				scheduling_parent: leaf_hash,
				candidate: candidate.to_plain(),
				pvd: pvd.clone(),
				pov: pov.clone(),
			};

			virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

			assert_validate_seconded_candidate(
				&mut virtual_overseer,
				candidate.descriptor.relay_parent(),
				&candidate,
				&pov,
				&pvd,
				&validation_code,
				expected_head_data,
				false,
			)
			.await;

			// `seconding_sanity_check`
			let hypothetical_candidate = HypotheticalCandidate::Complete {
				candidate_hash: candidate.hash(),
				receipt: Arc::new(candidate.clone()),
				persisted_validation_data: pvd.clone(),
			};
			let expected_request_a = vec![(
				HypotheticalMembershipRequest {
					candidates: vec![hypothetical_candidate.clone()],
					fragment_chain_relay_parent: Some(leaf_hash),
				},
				make_hypothetical_membership_response(hypothetical_candidate, leaf_hash),
			)];
			assert_hypothetical_membership_requests(
				&mut virtual_overseer,
				expected_request_a.clone(),
			)
			.await;

			// Prospective parachains are notified.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::ProspectiveParachains(
					ProspectiveParachainsMessage::IntroduceSecondedCandidate(
						req,
						tx,
					),
				) if
					&req.candidate_receipt == candidate
					&& req.candidate_para == para_id
					&& pvd == req.persisted_validation_data
				=> {
					tx.send(true).unwrap();
				}
			);

			assert_candidate_is_shared_and_seconded(
				&mut virtual_overseer,
				&candidate.descriptor.relay_parent(),
			)
			.await;
		}

		virtual_overseer
	});
}

// Tests that validators start work on consecutive prospective parachain blocks.
#[test]
fn concurrent_dependent_candidates() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate `a` is seconded in a grandparent of the activated `leaf`,
		// candidate `b` -- in parent.
		const LEAF_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];

		let leaf_hash = Hash::from_low_u64_be(130);
		let leaf_parent = get_parent_hash(leaf_hash);
		let leaf_grandparent = get_parent_hash(leaf_parent);
		let activated = new_leaf(leaf_hash, LEAF_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_BLOCK_NUMBER - LEAF_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;

		let head_data = &[
			HeadData(vec![10, 20, 30]), // Before `a`.
			HeadData(vec![11, 21, 31]), // After `a`.
			HeadData(vec![12, 22]),     // After `b`.
		];

		let pov_a = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd_a = PersistedValidationData {
			parent_head: head_data[0].clone(),
			relay_parent_number: LEAF_BLOCK_NUMBER - 2,
			relay_parent_storage_root: Hash::zero(),
			max_pov_size: 1024,
		};

		let pov_b = PoV { block_data: BlockData(vec![22, 14, 100]) };
		let pvd_b = PersistedValidationData {
			parent_head: head_data[1].clone(),
			relay_parent_number: LEAF_BLOCK_NUMBER - 1,
			relay_parent_storage_root: Hash::zero(),
			max_pov_size: 1024,
		};
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let candidate_a = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_grandparent,
			pov_hash: pov_a.hash(),
			head_data: head_data[1].clone(),
			erasure_root: make_erasure_root(&test_state, pov_a.clone(), pvd_a.clone()),
			persisted_validation_data_hash: pvd_a.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();
		let candidate_b = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_parent,
			pov_hash: pov_b.hash(),
			head_data: head_data[2].clone(),
			erasure_root: make_erasure_root(&test_state, pov_b.clone(), pvd_b.clone()),
			persisted_validation_data_hash: pvd_b.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();
		let candidate_a_hash = candidate_a.hash();
		let candidate_b_hash = candidate_b.hash();

		let public1 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[5].to_seed()),
		)
		.expect("Insert key into keystore");
		let public2 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[2].to_seed()),
		)
		.expect("Insert key into keystore");

		// Signing context should have a parent hash candidate is based on.
		let signing_context =
			SigningContext { parent_hash: leaf_grandparent, session_index: test_state.session() };
		let signed_a = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate_a.clone(), pvd_a.clone()),
			&signing_context,
			ValidatorIndex(2),
			&public2.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let signing_context =
			SigningContext { parent_hash: leaf_parent, session_index: test_state.session() };
		let signed_b = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate_b.clone(), pvd_b.clone()),
			&signing_context,
			ValidatorIndex(5),
			&public1.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let statement_a = CandidateBackingMessage::Statement {
			scheduling_parent: leaf_grandparent,
			statement: signed_a.clone(),
		};
		let statement_b = CandidateBackingMessage::Statement {
			scheduling_parent: leaf_parent,
			statement: signed_b.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement_a }).await;

		// At this point the subsystem waits for response, the previous message is received,
		// send a second one without blocking.
		let _ = virtual_overseer
			.tx
			.start_send_unpin(FromOrchestra::Communication { msg: statement_b });

		let mut valid_statements = HashSet::new();
		let mut backed_statements = HashSet::new();

		loop {
			let msg = virtual_overseer
				.recv()
				.timeout(std::time::Duration::from_secs(1))
				.await
				.expect("overseer recv timed out");

			// Order is not guaranteed since we have 2 statements being handled concurrently.
			match msg {
				AllMessages::ProspectiveParachains(
					ProspectiveParachainsMessage::IntroduceSecondedCandidate(_, tx),
				) => {
					tx.send(true).unwrap();
				},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_,
					RuntimeApiRequest::ValidationCodeByHash(_, tx),
				)) => {
					tx.send(Ok(Some(validation_code.clone()))).unwrap();
				},
				AllMessages::AvailabilityDistribution(
					AvailabilityDistributionMessage::FetchPoV { candidate_hash, tx, .. },
				) => {
					let pov = if candidate_hash == candidate_a_hash {
						&pov_a
					} else if candidate_hash == candidate_b_hash {
						&pov_b
					} else {
						panic!("unknown candidate hash")
					};
					tx.send(pov.clone()).unwrap();
				},
				AllMessages::CandidateValidation(
					CandidateValidationMessage::ValidateFromExhaustive {
						candidate_receipt,
						response_sender,
						..
					},
				) => {
					let candidate_hash = candidate_receipt.hash();
					let (head_data, pvd) = if candidate_hash == candidate_a_hash {
						(&head_data[1], &pvd_a)
					} else if candidate_hash == candidate_b_hash {
						(&head_data[2], &pvd_b)
					} else {
						panic!("unknown candidate hash")
					};
					response_sender
						.send(Ok(ValidationResult::Valid(
							CandidateCommitments {
								head_data: head_data.clone(),
								horizontal_messages: Default::default(),
								upward_messages: Default::default(),
								new_validation_code: None,
								processed_downward_messages: 0,
								hrmp_watermark: 0,
							},
							pvd.clone(),
						)))
						.unwrap();
				},
				AllMessages::AvailabilityStore(AvailabilityStoreMessage::StoreAvailableData {
					tx,
					..
				}) => {
					tx.send(Ok(())).unwrap();
				},
				AllMessages::ProspectiveParachains(
					ProspectiveParachainsMessage::CandidateBacked(..),
				) => {},
				AllMessages::StatementDistribution(StatementDistributionMessage::Share(
					_,
					statement,
				)) => {
					assert_eq!(statement.validator_index(), ValidatorIndex(0));
					let payload = statement.payload();
					assert_matches!(
						payload.clone(),
						StatementWithPVD::Valid(hash)
							if hash == candidate_a_hash || hash == candidate_b_hash =>
						{
							assert!(valid_statements.insert(hash));
						}
					);
				},
				AllMessages::StatementDistribution(StatementDistributionMessage::Backed(hash)) => {
					// Ensure that `Share` was received first for the candidate.
					assert!(valid_statements.contains(&hash));
					backed_statements.insert(hash);

					if backed_statements.len() == 2 {
						break;
					}
				},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_,
					RuntimeApiRequest::SessionIndexForChild(tx),
				)) => {
					tx.send(Ok(1u32.into())).unwrap();
				},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_parent,
					RuntimeApiRequest::ValidatorGroups(tx),
				)) => {
					tx.send(Ok(test_state.validator_groups.clone())).unwrap();
				},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_,
					RuntimeApiRequest::NodeFeatures(sess_idx, tx),
				)) => {
					assert_eq!(sess_idx, 1);
					tx.send(Ok(NodeFeatures::EMPTY)).unwrap();
				},
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_parent,
					RuntimeApiRequest::AvailabilityCores(tx),
				)) => {
					tx.send(Ok(test_state.availability_cores.clone())).unwrap();
				},
				_ => panic!("unexpected message received from overseer: {:?}", msg),
			}
		}

		assert!(valid_statements.contains(&candidate_a_hash));
		assert!(valid_statements.contains(&candidate_b_hash));
		assert!(backed_statements.contains(&candidate_a_hash));
		assert!(backed_statements.contains(&candidate_b_hash));

		virtual_overseer
	});
}

// Test that multiple candidates from different paras can occupy the same depth
// in a given relay parent.
#[test]
fn seconding_sanity_check_occupy_same_depth() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate `a` is seconded in a parent of the activated `leaf`.
		const LEAF_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_ANCESTRY_LEN: BlockNumber = 3;

		let para_id_a = test_state.chain_ids[0];
		let para_id_b = test_state.chain_ids[1];

		let leaf_hash = Hash::from_low_u64_be(130);
		let leaf_parent = get_parent_hash(leaf_hash);

		let activated = new_leaf(leaf_hash, LEAF_BLOCK_NUMBER);
		let min_block_number = LEAF_BLOCK_NUMBER - LEAF_ANCESTRY_LEN;
		let min_relay_parents = vec![(para_id_a, min_block_number), (para_id_b, min_block_number)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data_a = test_state.head_data.get(&para_id_a).unwrap();
		let expected_head_data_b = test_state.head_data.get(&para_id_b).unwrap();

		let pov_hash = pov.hash();
		let candidate_a = TestCandidateBuilder {
			para_id: para_id_a,
			relay_parent: leaf_parent,
			pov_hash,
			head_data: expected_head_data_a.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		};

		let mut candidate_b = candidate_a.clone();
		candidate_b.para_id = para_id_b;
		candidate_b.head_data = expected_head_data_b.clone();
		// A rotation happens, test validator is assigned to second para here.
		candidate_b.relay_parent = leaf_hash;

		let candidate_a = (candidate_a.build(), expected_head_data_a, para_id_a);
		let candidate_b = (candidate_b.build(), expected_head_data_b, para_id_b);

		for candidate in &[candidate_a, candidate_b] {
			let (candidate, expected_head_data, para_id) = candidate;
			let second = CandidateBackingMessage::Second {
				scheduling_parent: leaf_hash,
				candidate: candidate.to_plain(),
				pvd: pvd.clone(),
				pov: pov.clone(),
			};

			virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

			assert_validate_seconded_candidate(
				&mut virtual_overseer,
				candidate.descriptor.relay_parent(),
				&candidate,
				&pov,
				&pvd,
				&validation_code,
				expected_head_data,
				false,
			)
			.await;

			// `seconding_sanity_check`
			let hypothetical_candidate = HypotheticalCandidate::Complete {
				candidate_hash: candidate.hash(),
				receipt: Arc::new(candidate.clone()),
				persisted_validation_data: pvd.clone(),
			};
			let expected_request_a = vec![(
				HypotheticalMembershipRequest {
					candidates: vec![hypothetical_candidate.clone()],
					fragment_chain_relay_parent: Some(leaf_hash),
				},
				// Send the same membership for both candidates.
				make_hypothetical_membership_response(hypothetical_candidate, leaf_hash),
			)];

			assert_hypothetical_membership_requests(
				&mut virtual_overseer,
				expected_request_a.clone(),
			)
			.await;

			// Prospective parachains are notified.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::ProspectiveParachains(
					ProspectiveParachainsMessage::IntroduceSecondedCandidate(
						req,
						tx,
					),
				) if
					&req.candidate_receipt == candidate
					&& &req.candidate_para == para_id
					&& pvd == req.persisted_validation_data
				=> {
					tx.send(true).unwrap();
				}
			);

			assert_candidate_is_shared_and_seconded(
				&mut virtual_overseer,
				&candidate.descriptor.relay_parent(),
			)
			.await;
		}

		virtual_overseer
	});
}

// V3-capable validator successfully backs a V1 candidate descriptor.
// `TestCandidateBuilder` produces non-zero collator/signature fields which overlap
// the reserved fields in the V2 layout, causing the descriptor to be detected as V1.
#[test]
fn v3_capable_validator_backs_v1_descriptor() {
	let mut test_state = TestState::default();

	// Enable V3 feature
	test_state
		.node_features
		.resize(node_features::FeatureIndex::CandidateReceiptV3 as usize + 1, false);
	test_state
		.node_features
		.set(node_features::FeatureIndex::CandidateReceiptV3 as u8 as usize, true);

	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		let para_id = activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![1, 2, 3]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);
		let pov_hash = pov.hash();
		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		// Build the V1 descriptor
		let v1_descriptor = CandidateDescriptor {
			para_id,
			relay_parent: test_state.relay_parent,
			collator: CollatorId::from_slice(&(0u8..32).collect::<Vec<_>>())
				.expect("32 bytes; qed"),
			persisted_validation_data_hash: pvd.hash(),
			pov_hash,
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			signature: CollatorSignature::from_slice(&(0u8..64).collect::<Vec<_>>())
				.expect("64 bytes; qed"),
			para_head: expected_head_data.hash(),
			validation_code_hash: ValidationCode(validation_code.0.clone()).hash(),
		};

		let candidate = CommittedCandidateReceipt {
			descriptor: v1_descriptor.into(),
			commitments: CandidateCommitments {
				head_data: expected_head_data.clone(),
				horizontal_messages: Default::default(),
				upward_messages: Default::default(),
				new_validation_code: None,
				processed_downward_messages: 0,
				hrmp_watermark: 0_u32,
			},
		};

		// The descriptor must be detected as V1
		assert_eq!(candidate.descriptor.version(), CandidateDescriptorVersion::V1);

		let candidate_hash = candidate.hash();

		let public2 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[2].to_seed()),
		)
		.expect("Insert key into keystore");

		let signed_seconded = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate.clone(), pvd.clone()),
			&test_state.signing_context,
			ValidatorIndex(2),
			&public2.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let public5 = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[5].to_seed()),
		)
		.expect("Insert key into keystore");

		let signed_valid = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Valid(candidate_hash),
			&test_state.signing_context,
			ValidatorIndex(5),
			&public5.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CandidateBackingMessage::Statement {
					scheduling_parent: test_state.relay_parent,
					statement: signed_seconded,
				},
			})
			.await;

		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(req, tx),
			) if req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data => {
				tx.send(true).unwrap();
			}
		);

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			candidate.descriptor.relay_parent(),
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			true,
		)
		.await;

		assert_candidate_is_shared_and_backed(
			&mut virtual_overseer,
			&test_state.relay_parent,
			&para_id,
			&candidate_hash,
		)
		.await;

		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CandidateBackingMessage::Statement {
					scheduling_parent: test_state.relay_parent,
					statement: signed_valid,
				},
			})
			.await;

		let (tx, rx) = oneshot::channel();
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CandidateBackingMessage::GetBackableCandidates {
					candidates: std::iter::once((
						para_id,
						vec![BackableCandidateRef {
							candidate_hash,
							scheduling_parent: test_state.relay_parent,
						}],
					))
					.collect(),
					sender: tx,
				},
			})
			.await;

		let mut candidates = rx.await.unwrap();
		assert_eq!(candidates.len(), 1);
		let candidates = candidates.remove(&para_id).unwrap();
		assert_eq!(candidates.len(), 1);
		assert_eq!(candidates[0].validity_votes().len(), 3);

		virtual_overseer
			.send(FromOrchestra::Signal(OverseerSignal::ActiveLeaves(
				ActiveLeavesUpdate::stop_work(test_state.relay_parent),
			)))
			.await;
		virtual_overseer
	});
}

// Test that the subsystem doesn't skip occupied cores assignments.
#[test]
fn occupied_core_assignment() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		// Candidate is seconded in a parent of the activated `leaf_a`.
		const LEAF_A_BLOCK_NUMBER: BlockNumber = 100;
		const LEAF_A_ANCESTRY_LEN: BlockNumber = 3;
		let para_id = test_state.chain_ids[0];
		let previous_para_id = test_state.chain_ids[1];

		// Set the core state to occupied.
		let mut candidate_descriptor =
			polkadot_primitives_test_helpers::dummy_candidate_descriptor(Hash::zero());
		candidate_descriptor.para_id = previous_para_id;
		test_state.availability_cores[0] = CoreState::Occupied(OccupiedCore {
			group_responsible: Default::default(),
			next_up_on_available: Some(ScheduledCore { para_id, collator: None }),
			occupied_since: 100_u32,
			time_out_at: 200_u32,
			next_up_on_time_out: None,
			availability: Default::default(),
			candidate_descriptor: candidate_descriptor.into(),
			candidate_hash: Default::default(),
		});

		let leaf_a_hash = Hash::from_low_u64_be(130);
		let leaf_a_parent = get_parent_hash(leaf_a_hash);
		let activated = new_leaf(leaf_a_hash, LEAF_A_BLOCK_NUMBER);
		let min_relay_parents = vec![(para_id, LEAF_A_BLOCK_NUMBER - LEAF_A_ANCESTRY_LEN)];
		let test_leaf_a = TestLeaf { activated, min_relay_parents };

		activate_leaf(&mut virtual_overseer, test_leaf_a, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&para_id).unwrap();

		let pov_hash = pov.hash();
		let candidate = TestCandidateBuilder {
			para_id,
			relay_parent: leaf_a_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		let second = CandidateBackingMessage::Second {
			scheduling_parent: leaf_a_hash,
			candidate: candidate.to_plain(),
			pvd: pvd.clone(),
			pov: pov.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		assert_validate_seconded_candidate(
			&mut virtual_overseer,
			leaf_a_parent,
			&candidate,
			&pov,
			&pvd,
			&validation_code,
			expected_head_data,
			false,
		)
		.await;

		// `seconding_sanity_check`
		let hypothetical_candidate = HypotheticalCandidate::Complete {
			candidate_hash: candidate.hash(),
			receipt: Arc::new(candidate.clone()),
			persisted_validation_data: pvd.clone(),
		};
		let expected_request = vec![(
			HypotheticalMembershipRequest {
				candidates: vec![hypothetical_candidate.clone()],
				fragment_chain_relay_parent: Some(leaf_a_hash),
			},
			make_hypothetical_membership_response(hypothetical_candidate, leaf_a_hash),
		)];
		assert_hypothetical_membership_requests(&mut virtual_overseer, expected_request).await;
		// Prospective parachains are notified.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(
					req,
					tx,
				),
			) if
				req.candidate_receipt == candidate
				&& req.candidate_para == para_id
				&& pvd == req.persisted_validation_data
			=> {
				tx.send(true).unwrap();
			}
		);

		assert_candidate_is_shared_and_seconded(&mut virtual_overseer, &leaf_a_parent).await;

		virtual_overseer
	});
}

// Test that an ambiguous candidate (version consistency check fails) is silently rejected
// when sent via CandidateBackingMessage::Second.
#[test]
fn ambiguous_candidate_rejected_on_second() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

		let pov_hash = pov.hash();
		let mut candidate = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		// Make the candidate ambiguous: set scheduling_parent to non-zero while keeping
		// version=0. Old rules see V1 (non-zero scheduling_parent), new rules see V2.
		candidate.descriptor.set_scheduling_parent(Hash::repeat_byte(0xAB));

		let second = CandidateBackingMessage::Second {
			scheduling_parent: test_state.relay_parent,
			candidate: candidate.to_plain(),
			pvd: pvd.clone(),
			pov: pov.clone(),
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: second }).await;

		// The candidate should be rejected and reported as invalid to collator protocol.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CollatorProtocol(CollatorProtocolMessage::Invalid(_, _))
		);

		virtual_overseer
	});
}

// Test that an ambiguous candidate (version consistency check fails) is silently rejected
// when received as a Seconded statement from another validator via
// CandidateBackingMessage::Statement.
#[test]
fn ambiguous_candidate_rejected_on_statement() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);

		let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

		let pov_hash = pov.hash();
		let mut candidate = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();

		// Make the candidate ambiguous: set scheduling_parent to non-zero while keeping
		// version=0. Old rules see V1 (non-zero scheduling_parent), new rules see V2.
		candidate.descriptor.set_scheduling_parent(Hash::repeat_byte(0xAB));

		// Sign as a Seconded statement from another validator.
		let public = Keystore::sr25519_generate_new(
			&*test_state.keystore,
			ValidatorId::ID,
			Some(&test_state.validators[2].to_seed()),
		)
		.expect("Insert key into keystore");

		let signed_statement = SignedFullStatementWithPVD::sign(
			&test_state.keystore,
			StatementWithPVD::Seconded(candidate.clone(), pvd.clone()),
			&test_state.signing_context,
			ValidatorIndex(2),
			&public.into(),
		)
		.ok()
		.flatten()
		.expect("should be signed");

		let statement = CandidateBackingMessage::Statement {
			scheduling_parent: test_state.relay_parent,
			statement: signed_statement,
		};

		virtual_overseer.send(FromOrchestra::Communication { msg: statement }).await;

		// The candidate should be silently rejected — no validation work issued.
		assert_matches!(virtual_overseer.recv().timeout(Duration::from_secs(1)).await, None);

		virtual_overseer
	});
}

/// Helper: send a BlockFinalized signal that activates V3 node features.
///
/// Uses session 2 (uncached) so the subsystem fetches fresh NodeFeatures with V3 enabled.
async fn activate_v3_via_block_finalized(virtual_overseer: &mut VirtualOverseer) {
	let finalized_hash = Hash::repeat_byte(0xFF);
	virtual_overseer
		.send(FromOrchestra::Signal(OverseerSignal::BlockFinalized(finalized_hash, 1)))
		.await;

	// check_v3_on_finalized: SessionIndexForChild → session 2
	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::RuntimeApi(RuntimeApiMessage::Request(
			parent,
			RuntimeApiRequest::SessionIndexForChild(tx),
		)) if parent == finalized_hash => {
			tx.send(Ok(2)).unwrap();
		}
	);

	// NodeFeatures for session 2 → V3 enabled
	assert_matches!(
		virtual_overseer.recv().await,
		AllMessages::RuntimeApi(RuntimeApiMessage::Request(
			parent,
			RuntimeApiRequest::NodeFeatures(session_index, tx),
		)) if parent == finalized_hash && session_index == 2 => {
			let mut features = NodeFeatures::new();
			features.resize(FeatureIndex::CandidateReceiptV3 as usize + 1, false);
			features.set(FeatureIndex::CandidateReceiptV3 as usize, true);
			tx.send(Ok(features)).unwrap();
		}
	);
}

/// Helper: sign a Seconded statement as validator 2 for use with
/// CandidateBackingMessage::Statement.
fn sign_seconded_statement(
	test_state: &TestState,
	candidate: &CommittedCandidateReceipt,
	pvd: &PersistedValidationData,
) -> SignedFullStatementWithPVD {
	let public = Keystore::sr25519_generate_new(
		&*test_state.keystore,
		ValidatorId::ID,
		Some(&test_state.validators[2].to_seed()),
	)
	.expect("Insert key into keystore");

	SignedFullStatementWithPVD::sign(
		&test_state.keystore,
		StatementWithPVD::Seconded(candidate.clone(), pvd.clone()),
		&test_state.signing_context,
		ValidatorIndex(2),
		&public.into(),
	)
	.ok()
	.flatten()
	.expect("should be signed")
}

// Test that version acceptance filtering behaves correctly before and after V3 activation
// via the CandidateBackingMessage::Second path (local seconding).
// - Ambiguous candidates (old rules ≠ new rules in an unexpected way) are always rejected.
// - V3 candidates are rejected before activation but accepted after.
#[test]
fn version_acceptance_before_and_after_v3_activation_on_second() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);
		let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();
		let pov_hash = pov.hash();

		// --- Before V3 activation ---

		// 1. Ambiguous candidate: rejected (Inconsistency).
		// scheduling_parent non-zero + version=0 → old rules see V1, new rules see V2.
		let mut ambiguous = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();
		ambiguous.descriptor.set_scheduling_parent(Hash::repeat_byte(0xAB));

		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CandidateBackingMessage::Second {
					scheduling_parent: test_state.relay_parent,
					candidate: ambiguous.to_plain(),
					pvd: pvd.clone(),
					pov: pov.clone(),
				},
			})
			.await;
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CollatorProtocol(CollatorProtocolMessage::Invalid(_, _))
		);

		// 2. V3 candidate: rejected (V3NotEnabled).
		let v3_candidate = CommittedCandidateReceipt {
			descriptor: CandidateDescriptorV2::new_v3(
				test_state.chain_ids[0],
				test_state.relay_parent,
				CoreIndex(0),
				1, // session_index
				1, // scheduling_session_index
				pvd.hash(),
				pov_hash,
				make_erasure_root(&test_state, pov.clone(), pvd.clone()),
				expected_head_data.hash(),
				ValidationCode(validation_code.0.clone()).hash(),
				test_state.relay_parent,
			),
			commitments: CandidateCommitments {
				head_data: expected_head_data.clone(),
				..Default::default()
			},
		};

		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CandidateBackingMessage::Second {
					scheduling_parent: test_state.relay_parent,
					candidate: v3_candidate.to_plain(),
					pvd: pvd.clone(),
					pov: pov.clone(),
				},
			})
			.await;
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CollatorProtocol(CollatorProtocolMessage::Invalid(_, _))
		);

		// --- Activate V3 ---
		activate_v3_via_block_finalized(&mut virtual_overseer).await;

		// --- After V3 activation ---

		// 3. Ambiguous candidate: STILL rejected (Inconsistency is always fatal).
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CandidateBackingMessage::Second {
					scheduling_parent: test_state.relay_parent,
					candidate: ambiguous.to_plain(),
					pvd: pvd.clone(),
					pov: pov.clone(),
				},
			})
			.await;
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::CollatorProtocol(CollatorProtocolMessage::Invalid(_, _))
		);

		// 4. V3 candidate: NOW ACCEPTED — passes check_version_acceptance.
		// It proceeds past the version check into the normal seconding flow.
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CandidateBackingMessage::Second {
					scheduling_parent: test_state.relay_parent,
					candidate: v3_candidate.to_plain(),
					pvd: pvd.clone(),
					pov: pov.clone(),
				},
			})
			.await;

		// The candidate passed version acceptance and proceeds to validation —
		// validation code is fetched as part of the validation setup.
		// (Before V3 activation, the same candidate produced a timeout above.)
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::ValidationCodeByHash(_, _),
			))
		);

		virtual_overseer
	});
}

// Same test as above but via the CandidateBackingMessage::Statement path
// (receiving a Seconded statement from another validator).
#[test]
fn version_acceptance_before_and_after_v3_activation_on_statement() {
	let mut test_state = TestState::default();
	test_harness(test_state.keystore.clone(), |mut virtual_overseer| async move {
		activate_initial_leaf(&mut virtual_overseer, &mut test_state).await;

		let pov = PoV { block_data: BlockData(vec![42, 43, 44]) };
		let pvd = dummy_pvd();
		let validation_code = ValidationCode(vec![1, 2, 3]);
		let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();
		let pov_hash = pov.hash();

		// Ambiguous candidate
		let mut ambiguous = TestCandidateBuilder {
			para_id: test_state.chain_ids[0],
			relay_parent: test_state.relay_parent,
			pov_hash,
			head_data: expected_head_data.clone(),
			erasure_root: make_erasure_root(&test_state, pov.clone(), pvd.clone()),
			persisted_validation_data_hash: pvd.hash(),
			validation_code: validation_code.0.clone(),
		}
		.build();
		ambiguous.descriptor.set_scheduling_parent(Hash::repeat_byte(0xAB));

		// V3 candidate
		let v3_candidate = CommittedCandidateReceipt {
			descriptor: CandidateDescriptorV2::new_v3(
				test_state.chain_ids[0],
				test_state.relay_parent,
				CoreIndex(0),
				1, // session_index
				1, // scheduling_session_index
				pvd.hash(),
				pov_hash,
				make_erasure_root(&test_state, pov.clone(), pvd.clone()),
				expected_head_data.hash(),
				ValidationCode(validation_code.0.clone()).hash(),
				test_state.relay_parent,
			),
			commitments: CandidateCommitments {
				head_data: expected_head_data.clone(),
				..Default::default()
			},
		};

		// --- Before V3 activation ---

		// 1. Ambiguous candidate via Statement: rejected.
		let stmt = sign_seconded_statement(&test_state, &ambiguous, &pvd);
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CandidateBackingMessage::Statement {
					scheduling_parent: test_state.relay_parent,
					statement: stmt,
				},
			})
			.await;
		assert_matches!(virtual_overseer.recv().timeout(Duration::from_secs(1)).await, None);

		// 2. V3 candidate via Statement: rejected (V3NotEnabled).
		let stmt = sign_seconded_statement(&test_state, &v3_candidate, &pvd);
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CandidateBackingMessage::Statement {
					scheduling_parent: test_state.relay_parent,
					statement: stmt,
				},
			})
			.await;
		assert_matches!(virtual_overseer.recv().timeout(Duration::from_secs(1)).await, None);

		// --- Activate V3 ---
		activate_v3_via_block_finalized(&mut virtual_overseer).await;

		// --- After V3 activation ---

		// 3. Ambiguous candidate via Statement: STILL rejected.
		let stmt = sign_seconded_statement(&test_state, &ambiguous, &pvd);
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CandidateBackingMessage::Statement {
					scheduling_parent: test_state.relay_parent,
					statement: stmt,
				},
			})
			.await;
		assert_matches!(virtual_overseer.recv().timeout(Duration::from_secs(1)).await, None);

		// 4. V3 candidate via Statement: NOW ACCEPTED.
		let stmt = sign_seconded_statement(&test_state, &v3_candidate, &pvd);
		virtual_overseer
			.send(FromOrchestra::Communication {
				msg: CandidateBackingMessage::Statement {
					scheduling_parent: test_state.relay_parent,
					statement: stmt,
				},
			})
			.await;

		// The candidate passed version acceptance and is forwarded to
		// prospective parachains for import — this is the first step of the
		// Statement path after the version check.
		// (Before V3 activation, the same candidate produced a timeout above.)
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::ProspectiveParachains(
				ProspectiveParachainsMessage::IntroduceSecondedCandidate(_, _),
			)
		);

		virtual_overseer
	});
}
