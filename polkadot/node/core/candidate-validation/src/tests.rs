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

use std::{
	collections::BTreeMap,
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc, Mutex,
	},
};

use super::*;
use crate::PvfExecKind;
use assert_matches::assert_matches;
use futures::executor;
use polkadot_node_core_pvf::PrepareError;
use polkadot_node_primitives::BlockData;
use polkadot_node_subsystem::messages::AllMessages;
use polkadot_node_subsystem_test_helpers::{make_subsystem_context, TestSubsystemContextHandle};
use polkadot_node_subsystem_util::reexports::SubsystemContext;
use polkadot_overseer::ActivatedLeaf;
use polkadot_primitives::{
	CandidateDescriptorV2, CandidateDescriptorVersion, ClaimQueueOffset,
	CommittedCandidateReceiptError, CoreIndex, CoreSelector, GroupIndex, HeadData, Id as ParaId,
	MutateDescriptorV2, NodeFeatures, OccupiedCoreAssumption, SessionInfo, UMPSignal,
	UpwardMessage, ValidatorId, DEFAULT_SCHEDULING_LOOKAHEAD, UMP_SEPARATOR,
};
use polkadot_primitives_test_helpers::{
	dummy_collator, dummy_collator_signature, dummy_hash, make_valid_candidate_descriptor,
	make_valid_candidate_descriptor_v2, make_valid_candidate_descriptor_v3, CandidateDescriptor,
};
use rstest::rstest;
use sp_core::{sr25519::Public, testing::TaskExecutor};
use sp_keyring::Sr25519Keyring;
use sp_keystore::{testing::MemoryKeystore, Keystore};

const VALIDATION_CODE_BOMB_LIMIT: u32 = 30 * 1024 * 1024;

#[derive(Debug)]
enum AssumptionCheckOutcome {
	Matches(PersistedValidationData, ValidationCode),
	DoesNotMatch,
	BadRequest,
}

async fn check_assumption_validation_data<Sender>(
	sender: &mut Sender,
	descriptor: &CandidateDescriptor,
	assumption: OccupiedCoreAssumption,
) -> AssumptionCheckOutcome
where
	Sender: SubsystemSender<RuntimeApiMessage>,
{
	let validation_data = {
		let (tx, rx) = oneshot::channel();
		let d = runtime_api_request(
			sender,
			descriptor.relay_parent,
			RuntimeApiRequest::PersistedValidationData(descriptor.para_id, assumption, tx),
			rx,
		)
		.await;

		match d {
			Ok(None) | Err(RuntimeRequestFailed) => return AssumptionCheckOutcome::BadRequest,
			Ok(Some(d)) => d,
		}
	};

	let persisted_validation_data_hash = validation_data.hash();

	if descriptor.persisted_validation_data_hash == persisted_validation_data_hash {
		let (code_tx, code_rx) = oneshot::channel();
		let validation_code = runtime_api_request(
			sender,
			descriptor.relay_parent,
			RuntimeApiRequest::ValidationCode(descriptor.para_id, assumption, code_tx),
			code_rx,
		)
		.await;

		match validation_code {
			Ok(None) | Err(RuntimeRequestFailed) => AssumptionCheckOutcome::BadRequest,
			Ok(Some(v)) => AssumptionCheckOutcome::Matches(validation_data, v),
		}
	} else {
		AssumptionCheckOutcome::DoesNotMatch
	}
}

#[test]
fn correctly_checks_included_assumption() {
	let validation_data: PersistedValidationData = Default::default();
	let validation_code: ValidationCode = vec![1, 2, 3].into();

	let persisted_validation_data_hash = validation_data.hash();
	let relay_parent = [2; 32].into();
	let para_id = ParaId::from(5_u32);

	let descriptor = make_valid_candidate_descriptor(
		para_id,
		relay_parent,
		persisted_validation_data_hash,
		dummy_hash(),
		dummy_hash(),
		dummy_hash(),
		dummy_hash(),
		Sr25519Keyring::Alice,
	)
	.into();

	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());

	let (check_fut, check_result) = check_assumption_validation_data(
		ctx.sender(),
		&descriptor,
		OccupiedCoreAssumption::Included,
	)
	.remote_handle();

	let test_fut = async move {
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::PersistedValidationData(
					p,
					OccupiedCoreAssumption::Included,
					tx
				),
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(Some(validation_data.clone())));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::ValidationCode(p, OccupiedCoreAssumption::Included, tx)
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(Some(validation_code.clone())));
			}
		);

		assert_matches!(check_result.await, AssumptionCheckOutcome::Matches(o, v) => {
			assert_eq!(o, validation_data);
			assert_eq!(v, validation_code);
		});
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);
}

#[test]
fn correctly_checks_timed_out_assumption() {
	let validation_data: PersistedValidationData = Default::default();
	let validation_code: ValidationCode = vec![1, 2, 3].into();

	let persisted_validation_data_hash = validation_data.hash();
	let relay_parent = [2; 32].into();
	let para_id = ParaId::from(5_u32);

	let descriptor = make_valid_candidate_descriptor(
		para_id,
		relay_parent,
		persisted_validation_data_hash,
		dummy_hash(),
		dummy_hash(),
		dummy_hash(),
		dummy_hash(),
		Sr25519Keyring::Alice,
	)
	.into();

	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());

	let (check_fut, check_result) = check_assumption_validation_data(
		ctx.sender(),
		&descriptor,
		OccupiedCoreAssumption::TimedOut,
	)
	.remote_handle();

	let test_fut = async move {
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::PersistedValidationData(
					p,
					OccupiedCoreAssumption::TimedOut,
					tx
				),
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(Some(validation_data.clone())));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::ValidationCode(p, OccupiedCoreAssumption::TimedOut, tx)
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(Some(validation_code.clone())));
			}
		);

		assert_matches!(check_result.await, AssumptionCheckOutcome::Matches(o, v) => {
			assert_eq!(o, validation_data);
			assert_eq!(v, validation_code);
		});
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);
}

#[test]
fn check_is_bad_request_if_no_validation_data() {
	let validation_data: PersistedValidationData = Default::default();
	let persisted_validation_data_hash = validation_data.hash();
	let relay_parent = [2; 32].into();
	let para_id = ParaId::from(5_u32);

	let descriptor = make_valid_candidate_descriptor(
		para_id,
		relay_parent,
		persisted_validation_data_hash,
		dummy_hash(),
		dummy_hash(),
		dummy_hash(),
		dummy_hash(),
		Sr25519Keyring::Alice,
	)
	.into();

	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());

	let (check_fut, check_result) = check_assumption_validation_data(
		ctx.sender(),
		&descriptor,
		OccupiedCoreAssumption::Included,
	)
	.remote_handle();

	let test_fut = async move {
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::PersistedValidationData(
					p,
					OccupiedCoreAssumption::Included,
					tx
				),
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(None));
			}
		);

		assert_matches!(check_result.await, AssumptionCheckOutcome::BadRequest);
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);
}

#[test]
fn check_is_bad_request_if_no_validation_code() {
	let validation_data: PersistedValidationData = Default::default();
	let persisted_validation_data_hash = validation_data.hash();
	let relay_parent = [2; 32].into();
	let para_id = ParaId::from(5_u32);

	let descriptor = make_valid_candidate_descriptor(
		para_id,
		relay_parent,
		persisted_validation_data_hash,
		dummy_hash(),
		dummy_hash(),
		dummy_hash(),
		dummy_hash(),
		Sr25519Keyring::Alice,
	)
	.into();

	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());

	let (check_fut, check_result) = check_assumption_validation_data(
		ctx.sender(),
		&descriptor,
		OccupiedCoreAssumption::TimedOut,
	)
	.remote_handle();

	let test_fut = async move {
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::PersistedValidationData(
					p,
					OccupiedCoreAssumption::TimedOut,
					tx
				),
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(Some(validation_data.clone())));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::ValidationCode(p, OccupiedCoreAssumption::TimedOut, tx)
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(None));
			}
		);

		assert_matches!(check_result.await, AssumptionCheckOutcome::BadRequest);
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);
}

#[test]
fn check_does_not_match() {
	let validation_data: PersistedValidationData = Default::default();
	let relay_parent = Hash::repeat_byte(0x02);
	let para_id = ParaId::from(5_u32);

	let descriptor = make_valid_candidate_descriptor(
		para_id,
		relay_parent,
		Hash::from([3; 32]),
		dummy_hash(),
		dummy_hash(),
		dummy_hash(),
		dummy_hash(),
		Sr25519Keyring::Alice,
	)
	.into();

	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());

	let (check_fut, check_result) = check_assumption_validation_data(
		ctx.sender(),
		&descriptor,
		OccupiedCoreAssumption::Included,
	)
	.remote_handle();

	let test_fut = async move {
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::PersistedValidationData(
					p,
					OccupiedCoreAssumption::Included,
					tx
				),
			)) => {
				assert_eq!(rp, relay_parent);
				assert_eq!(p, para_id);

				let _ = tx.send(Ok(Some(validation_data.clone())));
			}
		);

		assert_matches!(check_result.await, AssumptionCheckOutcome::DoesNotMatch);
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);
}

#[derive(Clone)]
struct MockValidateCandidateBackend {
	inner: Arc<Mutex<MockValidateCandidateBackendInner>>,
}

struct MockValidateCandidateBackendInner {
	result_list: Vec<Result<WasmValidationResult, ValidationError>>,
	num_times_called: usize,
}

impl MockValidateCandidateBackend {
	fn with_hardcoded_result(result: Result<WasmValidationResult, ValidationError>) -> Self {
		Self {
			inner: Arc::new(Mutex::new(MockValidateCandidateBackendInner {
				result_list: vec![result],
				num_times_called: 0,
			})),
		}
	}

	fn with_hardcoded_result_list(
		result_list: Vec<Result<WasmValidationResult, ValidationError>>,
	) -> Self {
		Self {
			inner: Arc::new(Mutex::new(MockValidateCandidateBackendInner {
				result_list,
				num_times_called: 0,
			})),
		}
	}
}

#[async_trait]
impl ValidationBackend for MockValidateCandidateBackend {
	async fn validate_candidate(
		&mut self,
		_pvf: PvfPrepData,
		_validation_context: ValidationContext,
		_exec_kind: PvfExecKind,
	) -> Result<WasmValidationResult, ValidationError> {
		// This is expected to panic if called more times than expected, indicating an error in the
		// test.
		let mut inner = self.inner.lock().unwrap();
		let result = inner.result_list[inner.num_times_called].clone();
		inner.num_times_called += 1;

		result
	}

	async fn precheck_pvf(&mut self, _pvf: PvfPrepData) -> Result<(), PrepareError> {
		unreachable!()
	}

	async fn heads_up(&mut self, _active_pvfs: Vec<PvfPrepData>) -> Result<(), String> {
		unreachable!()
	}

	async fn update_active_leaves(
		&mut self,
		_update: ActiveLeavesUpdate,
		_ancestors: Vec<Hash>,
	) -> Result<(), String> {
		unreachable!()
	}
}

#[rstest]
#[case(true)]
#[case(false)]
fn candidate_validation_ok_is_ok(#[case] v2_descriptor: bool) {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };

	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let head_data = HeadData(vec![1, 1, 1]);
	let validation_code = ValidationCode(vec![2; 16]);

	let descriptor = if v2_descriptor {
		make_valid_candidate_descriptor_v2(
			ParaId::from(1_u32),
			dummy_hash(),
			CoreIndex(1),
			1,
			dummy_hash(),
			pov.hash(),
			validation_code.hash(),
			head_data.hash(),
			dummy_hash(),
		)
	} else {
		make_valid_candidate_descriptor(
			ParaId::from(1_u32),
			dummy_hash(),
			validation_data.hash(),
			pov.hash(),
			validation_code.hash(),
			head_data.hash(),
			dummy_hash(),
			Sr25519Keyring::Alice,
		)
		.into()
	};

	let check = perform_basic_checks(
		&descriptor,
		validation_data.max_pov_size,
		&pov,
		&validation_code.hash(),
	);
	assert!(check.is_ok());

	let mut validation_result = WasmValidationResult {
		head_data,
		new_validation_code: Some(vec![2, 2, 2].into()),
		upward_messages: Default::default(),
		horizontal_messages: Default::default(),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};

	if v2_descriptor {
		validation_result.upward_messages.force_push(UMP_SEPARATOR);
		validation_result
			.upward_messages
			.force_push(UMPSignal::SelectCore(CoreSelector(0), ClaimQueueOffset(1)).encode());
		validation_result
			.upward_messages
			.force_push(UMPSignal::ApprovedPeer(vec![1, 2, 3].try_into().unwrap()).encode());
	}

	let commitments = CandidateCommitments {
		head_data: validation_result.head_data.clone(),
		upward_messages: validation_result.upward_messages.clone(),
		horizontal_messages: validation_result.horizontal_messages.clone(),
		new_validation_code: validation_result.new_validation_code.clone(),
		processed_downward_messages: validation_result.processed_downward_messages,
		hrmp_watermark: validation_result.hrmp_watermark,
	};

	let candidate_receipt = CandidateReceipt { descriptor, commitments_hash: commitments.hash() };
	let mut cq = BTreeMap::new();
	let _ = cq.insert(CoreIndex(0), vec![1.into(), 2.into()].into());
	let _ = cq.insert(CoreIndex(1), vec![1.into(), 1.into()].into());

	let v = executor::block_on(validate_candidate(
		MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result)),
		validation_data.clone(),
		validation_code,
		candidate_receipt,
		Arc::new(pov),
		ExecutorParams::default(),
		PvfExecKind::Backing(dummy_hash()),
		&Default::default(),
		false,
		PreValidationOutput {
			validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
			claim_queue: Some(ClaimQueueSnapshot(cq)),
		},
	))
	.unwrap();

	assert_matches!(v, ValidationResult::Valid(outputs, used_validation_data) => {
		assert_eq!(outputs.head_data, HeadData(vec![1, 1, 1]));
		assert_eq!(outputs.upward_messages, commitments.upward_messages);
		assert_eq!(outputs.horizontal_messages, Vec::new());
		assert_eq!(outputs.new_validation_code, Some(vec![2, 2, 2].into()));
		assert_eq!(outputs.hrmp_watermark, 0);
		assert_eq!(used_validation_data, validation_data);
	});
}

#[test]
// Test v2 receipt validation in the following scenarios:
// - v2 receipt with mismatching session index in descriptor
// - v2 candidate has no assignments but a core selector is present
// - v1 candidate that outputs a UMP signal is invalid.
// - v2 candidate that outputs an approved peer id is valid.
// Also check that the validation of invalid candidates only fail during backing checks.
fn invalid_session_or_ump_signals() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };

	let pov: PoV = PoV { block_data: BlockData(vec![1; 32]) };
	let head_data = HeadData(vec![1, 1, 1]);
	let validation_code = ValidationCode(vec![2; 16]);

	let descriptor = make_valid_candidate_descriptor_v2(
		ParaId::from(1_u32),
		dummy_hash(),
		CoreIndex(1),
		100,
		dummy_hash(),
		pov.hash(),
		validation_code.hash(),
		head_data.hash(),
		dummy_hash(),
	);

	let check = perform_basic_checks(
		&descriptor,
		validation_data.max_pov_size,
		&pov,
		&validation_code.hash(),
	);
	assert!(check.is_ok());

	let mut validation_result = WasmValidationResult {
		head_data: head_data.clone(),
		new_validation_code: Some(vec![2, 2, 2].into()),
		upward_messages: Default::default(),
		horizontal_messages: Default::default(),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};

	validation_result.upward_messages.force_push(UMP_SEPARATOR);
	validation_result
		.upward_messages
		.force_push(UMPSignal::SelectCore(CoreSelector(1), ClaimQueueOffset(0)).encode());

	let commitments = CandidateCommitments {
		head_data: validation_result.head_data.clone(),
		upward_messages: validation_result.upward_messages.clone(),
		horizontal_messages: validation_result.horizontal_messages.clone(),
		new_validation_code: validation_result.new_validation_code.clone(),
		processed_downward_messages: validation_result.processed_downward_messages,
		hrmp_watermark: validation_result.hrmp_watermark,
	};

	let mut candidate_receipt =
		CandidateReceipt { descriptor, commitments_hash: commitments.hash() };

	candidate_receipt.descriptor.set_session_index(1);

	// Candidate has no assignments but a core selector.
	for exec_kind in
		[PvfExecKind::Backing(dummy_hash()), PvfExecKind::BackingSystemParas(dummy_hash())]
	{
		let result = executor::block_on(validate_candidate(
			MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone())),
			validation_data.clone(),
			validation_code.clone(),
			candidate_receipt.clone(),
			Arc::new(pov.clone()),
			ExecutorParams::default(),
			exec_kind,
			&Default::default(),
			false,
			PreValidationOutput {
				validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
				claim_queue: Some(ClaimQueueSnapshot::default()),
			},
		))
		.unwrap();
		assert_matches!(
			result,
			ValidationResult::Invalid(InvalidCandidate::InvalidUMPSignals(
				CommittedCandidateReceiptError::NoAssignment
			))
		);
	}

	// Validation doesn't fail for approvals and disputes, core/session index is not checked.
	for exec_kind in [PvfExecKind::Approval, PvfExecKind::Dispute] {
		let v = executor::block_on(validate_candidate(
			MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone())),
			validation_data.clone(),
			validation_code.clone(),
			candidate_receipt.clone(),
			Arc::new(pov.clone()),
			ExecutorParams::default(),
			exec_kind,
			&Default::default(),
			false,
			PreValidationOutput {
				validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
				claim_queue: None,
			},
		))
		.unwrap();

		assert_matches!(v, ValidationResult::Valid(outputs, used_validation_data) => {
			assert_eq!(outputs.head_data, HeadData(vec![1, 1, 1]));
			assert_eq!(outputs.upward_messages, commitments.upward_messages);
			assert_eq!(outputs.horizontal_messages, Vec::new());
			assert_eq!(outputs.new_validation_code, Some(vec![2, 2, 2].into()));
			assert_eq!(outputs.hrmp_watermark, 0);
			assert_eq!(used_validation_data, validation_data);
		});
	}

	// Populate claim queue.
	let mut cq = BTreeMap::new();
	let _ = cq.insert(CoreIndex(0), vec![1.into(), 2.into()].into());
	let _ = cq.insert(CoreIndex(1), vec![1.into(), 2.into()].into());

	for exec_kind in
		[PvfExecKind::Backing(dummy_hash()), PvfExecKind::BackingSystemParas(dummy_hash())]
	{
		let v = executor::block_on(validate_candidate(
			MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone())),
			validation_data.clone(),
			validation_code.clone(),
			candidate_receipt.clone(),
			Arc::new(pov.clone()),
			ExecutorParams::default(),
			exec_kind,
			&Default::default(),
			false,
			PreValidationOutput {
				validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
				claim_queue: Some(ClaimQueueSnapshot(cq.clone())),
			},
		))
		.unwrap();

		assert_matches!(v, ValidationResult::Valid(outputs, used_validation_data) => {
			assert_eq!(outputs.head_data, HeadData(vec![1, 1, 1]));
			assert_eq!(outputs.upward_messages, commitments.upward_messages);
			assert_eq!(outputs.horizontal_messages, Vec::new());
			assert_eq!(outputs.new_validation_code, Some(vec![2, 2, 2].into()));
			assert_eq!(outputs.hrmp_watermark, 0);
			assert_eq!(used_validation_data, validation_data);
		});
	}

	// Test that a v1 candidate that outputs the core selector UMP signal is invalid.
	let descriptor_v1 = make_valid_candidate_descriptor(
		ParaId::from(1_u32),
		dummy_hash(),
		dummy_hash(),
		pov.hash(),
		validation_code.hash(),
		validation_result.head_data.hash(),
		dummy_hash(),
		sp_keyring::Sr25519Keyring::Ferdie,
	);
	let descriptor: CandidateDescriptorV2 = descriptor_v1.into();

	perform_basic_checks(&descriptor, validation_data.max_pov_size, &pov, &validation_code.hash())
		.unwrap();
	assert_eq!(descriptor.version(), CandidateDescriptorVersion::V1);
	let candidate_receipt = CandidateReceipt { descriptor, commitments_hash: commitments.hash() };

	for exec_kind in
		[PvfExecKind::Backing(dummy_hash()), PvfExecKind::BackingSystemParas(dummy_hash())]
	{
		let result = executor::block_on(validate_candidate(
			MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone())),
			validation_data.clone(),
			validation_code.clone(),
			candidate_receipt.clone(),
			Arc::new(pov.clone()),
			ExecutorParams::default(),
			exec_kind,
			&Default::default(),
			false,
			PreValidationOutput {
				validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
				claim_queue: Some(ClaimQueueSnapshot::default()),
			},
		))
		.unwrap();
		assert_matches!(
			result,
			ValidationResult::Invalid(InvalidCandidate::InvalidUMPSignals(
				CommittedCandidateReceiptError::UMPSignalWithV1Descriptor
			))
		);
	}

	// Validation doesn't fail for approvals and disputes, ump signals are not checked.
	for exec_kind in [PvfExecKind::Approval, PvfExecKind::Dispute] {
		let v = executor::block_on(validate_candidate(
			MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone())),
			validation_data.clone(),
			validation_code.clone(),
			candidate_receipt.clone(),
			Arc::new(pov.clone()),
			ExecutorParams::default(),
			exec_kind,
			&Default::default(),
			false,
			PreValidationOutput {
				validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
				claim_queue: None,
			},
		))
		.unwrap();

		assert_matches!(v, ValidationResult::Valid(outputs, used_validation_data) => {
			assert_eq!(outputs.head_data, HeadData(vec![1, 1, 1]));
			assert_eq!(outputs.upward_messages, commitments.upward_messages);
			assert_eq!(outputs.horizontal_messages, Vec::new());
			assert_eq!(outputs.new_validation_code, Some(vec![2, 2, 2].into()));
			assert_eq!(outputs.hrmp_watermark, 0);
			assert_eq!(used_validation_data, validation_data);
		});
	}

	// Test that a v2 candidate that outputs an approved peer id is valid.
	let descriptor = make_valid_candidate_descriptor_v2(
		ParaId::from(1_u32),
		dummy_hash(),
		CoreIndex(1),
		1,
		dummy_hash(),
		pov.hash(),
		validation_code.hash(),
		head_data.hash(),
		dummy_hash(),
	);
	let mut validation_result = validation_result.clone();

	validation_result
		.upward_messages
		.force_push(UMPSignal::ApprovedPeer(vec![1, 2, 3].try_into().unwrap()).encode());

	let mut commitments = commitments.clone();
	commitments.upward_messages = validation_result.upward_messages.clone();

	let candidate_receipt = CandidateReceipt { descriptor, commitments_hash: commitments.hash() };
	for exec_kind in
		[PvfExecKind::Backing(dummy_hash()), PvfExecKind::BackingSystemParas(dummy_hash())]
	{
		let v = executor::block_on(validate_candidate(
			MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone())),
			validation_data.clone(),
			validation_code.clone(),
			candidate_receipt.clone(),
			Arc::new(pov.clone()),
			ExecutorParams::default(),
			exec_kind,
			&Default::default(),
			false,
			PreValidationOutput {
				validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
				claim_queue: Some(ClaimQueueSnapshot(cq.clone())),
			},
		))
		.unwrap();

		assert_matches!(v, ValidationResult::Valid(outputs, used_validation_data) => {
			assert_eq!(outputs.head_data, HeadData(vec![1, 1, 1]));
			assert_eq!(outputs.upward_messages, commitments.upward_messages);
			assert_eq!(outputs.horizontal_messages, Vec::new());
			assert_eq!(outputs.new_validation_code, Some(vec![2, 2, 2].into()));
			assert_eq!(outputs.hrmp_watermark, 0);
			assert_eq!(used_validation_data, validation_data);
		});
	}

	// Validation also doesn't fail for approvals and disputes.
	for exec_kind in [PvfExecKind::Approval, PvfExecKind::Dispute] {
		let v = executor::block_on(validate_candidate(
			MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone())),
			validation_data.clone(),
			validation_code.clone(),
			candidate_receipt.clone(),
			Arc::new(pov.clone()),
			ExecutorParams::default(),
			exec_kind,
			&Default::default(),
			false,
			PreValidationOutput {
				validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
				claim_queue: None,
			},
		))
		.unwrap();

		assert_matches!(v, ValidationResult::Valid(outputs, used_validation_data) => {
			assert_eq!(outputs.head_data, HeadData(vec![1, 1, 1]));
			assert_eq!(outputs.upward_messages, commitments.upward_messages);
			assert_eq!(outputs.horizontal_messages, Vec::new());
			assert_eq!(outputs.new_validation_code, Some(vec![2, 2, 2].into()));
			assert_eq!(outputs.hrmp_watermark, 0);
			assert_eq!(used_validation_data, validation_data);
		});
	}
}

/// V3 UMP signal enforcement: backing requires UMP signals for V3 candidates,
/// approval/dispute skips the check entirely.
///
/// Loops through all exec kinds × (with signals, without signals) and verifies:
/// - Backing + signals → Valid
/// - Backing + no signals → Invalid(NoUMPSignalWithV3Descriptor)
/// - Approval/Dispute + signals → Valid (check skipped)
/// - Approval/Dispute + no signals → Valid (check skipped)
#[test]
fn v3_ump_signal_enforcement() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };
	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let head_data = HeadData(vec![1, 1, 1]);
	let validation_code = ValidationCode(vec![2; 16]);

	let relay_parent = dummy_hash();
	let scheduling_parent = Hash::repeat_byte(0x42);
	let descriptor = make_valid_candidate_descriptor_v3(
		ParaId::from(1_u32),
		relay_parent,
		CoreIndex(0),
		1,
		1,
		validation_data.hash(),
		pov.hash(),
		validation_code.hash(),
		head_data.hash(),
		dummy_hash(),
		scheduling_parent,
	);

	assert_eq!(descriptor.version(), CandidateDescriptorVersion::V3);

	// Validation result WITH UMP signals (required for V3 in backing)
	let mut result_with_signals = WasmValidationResult {
		head_data: head_data.clone(),
		new_validation_code: None,
		upward_messages: Default::default(),
		horizontal_messages: Default::default(),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};
	result_with_signals.upward_messages.force_push(UMP_SEPARATOR);
	result_with_signals
		.upward_messages
		.force_push(UMPSignal::SelectCore(CoreSelector(0), ClaimQueueOffset(0)).encode());

	// Validation result WITHOUT UMP signals
	let result_no_signals = WasmValidationResult {
		head_data: head_data.clone(),
		new_validation_code: None,
		upward_messages: Default::default(),
		horizontal_messages: Default::default(),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};

	let mut cq = BTreeMap::new();
	let _ = cq.insert(CoreIndex(0), vec![ParaId::from(1_u32)].into());

	let all_exec_kinds = [
		PvfExecKind::Backing(dummy_hash()),
		PvfExecKind::BackingSystemParas(dummy_hash()),
		PvfExecKind::Approval,
		PvfExecKind::Dispute,
	];

	for has_signals in [true, false] {
		let validation_result =
			if has_signals { result_with_signals.clone() } else { result_no_signals.clone() };
		let commitments = CandidateCommitments {
			head_data: validation_result.head_data.clone(),
			upward_messages: validation_result.upward_messages.clone(),
			horizontal_messages: validation_result.horizontal_messages.clone(),
			new_validation_code: validation_result.new_validation_code.clone(),
			processed_downward_messages: validation_result.processed_downward_messages,
			hrmp_watermark: validation_result.hrmp_watermark,
		};
		let candidate_receipt = CandidateReceipt {
			descriptor: descriptor.clone(),
			commitments_hash: commitments.hash(),
		};

		for exec_kind in &all_exec_kinds {
			let is_backing =
				matches!(exec_kind, PvfExecKind::Backing(_) | PvfExecKind::BackingSystemParas(_));

			let result = executor::block_on(validate_candidate(
				MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone())),
				validation_data.clone(),
				validation_code.clone(),
				candidate_receipt.clone(),
				Arc::new(pov.clone()),
				ExecutorParams::default(),
				*exec_kind,
				&Default::default(),
				false,
				PreValidationOutput {
					validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
					claim_queue: if is_backing {
						Some(ClaimQueueSnapshot(cq.clone()))
					} else {
						None
					},
				},
			))
			.unwrap();

			match (is_backing, has_signals) {
				// Backing without signals → V3 requires them.
				(true, false) => assert_matches!(
					result,
					ValidationResult::Invalid(InvalidCandidate::InvalidUMPSignals(
						CommittedCandidateReceiptError::NoUMPSignalWithV3Descriptor
					)),
					"Backing must reject V3 without UMP signals ({exec_kind:?})"
				),
				// All other combinations → valid.
				_ => assert_matches!(
					result,
					ValidationResult::Valid(_, _),
					"Expected Valid for exec_kind={exec_kind:?} has_signals={has_signals}"
				),
			}
		}
	}
}

#[test]
fn candidate_validation_bad_return_is_invalid() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };

	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let validation_code = ValidationCode(vec![2; 16]);

	let descriptor = make_valid_candidate_descriptor(
		ParaId::from(1_u32),
		dummy_hash(),
		validation_data.hash(),
		pov.hash(),
		validation_code.hash(),
		dummy_hash(),
		dummy_hash(),
		Sr25519Keyring::Alice,
	)
	.into();

	let check = perform_basic_checks(
		&descriptor,
		validation_data.max_pov_size,
		&pov,
		&validation_code.hash(),
	);
	assert!(check.is_ok());

	let candidate_receipt = CandidateReceipt { descriptor, commitments_hash: Hash::zero() };

	let v = executor::block_on(validate_candidate(
		MockValidateCandidateBackend::with_hardcoded_result(Err(ValidationError::Invalid(
			WasmInvalidCandidate::HardTimeout,
		))),
		validation_data,
		validation_code,
		candidate_receipt,
		Arc::new(pov),
		ExecutorParams::default(),
		PvfExecKind::Backing(dummy_hash()),
		&Default::default(),
		false,
		PreValidationOutput {
			validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
			claim_queue: None,
		},
	))
	.unwrap();

	assert_matches!(v, ValidationResult::Invalid(InvalidCandidate::Timeout));
}

fn perform_basic_checks_on_valid_candidate(
	pov: &PoV,
	validation_code: &ValidationCode,
	validation_data: &PersistedValidationData,
	head_data_hash: Hash,
) -> CandidateDescriptorV2 {
	let descriptor = make_valid_candidate_descriptor(
		ParaId::from(1_u32),
		dummy_hash(),
		validation_data.hash(),
		pov.hash(),
		validation_code.hash(),
		head_data_hash,
		head_data_hash,
		Sr25519Keyring::Alice,
	)
	.into();

	let check = perform_basic_checks(
		&descriptor,
		validation_data.max_pov_size,
		&pov,
		&validation_code.hash(),
	);
	assert!(check.is_ok());
	descriptor
}

// Test that we vote valid if we get `AmbiguousWorkerDeath`, retry, and then succeed.
#[test]
fn candidate_validation_one_ambiguous_error_is_valid() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };

	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let head_data = HeadData(vec![1, 1, 1]);
	let validation_code = ValidationCode(vec![2; 16]);

	let descriptor = perform_basic_checks_on_valid_candidate(
		&pov,
		&validation_code,
		&validation_data,
		head_data.hash(),
	);

	let validation_result = WasmValidationResult {
		head_data,
		new_validation_code: Some(vec![2, 2, 2].into()),
		upward_messages: Default::default(),
		horizontal_messages: Default::default(),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};

	let commitments = CandidateCommitments {
		head_data: validation_result.head_data.clone(),
		upward_messages: validation_result.upward_messages.clone(),
		horizontal_messages: validation_result.horizontal_messages.clone(),
		new_validation_code: validation_result.new_validation_code.clone(),
		processed_downward_messages: validation_result.processed_downward_messages,
		hrmp_watermark: validation_result.hrmp_watermark,
	};

	let candidate_receipt = CandidateReceipt { descriptor, commitments_hash: commitments.hash() };

	let v = executor::block_on(validate_candidate(
		MockValidateCandidateBackend::with_hardcoded_result_list(vec![
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath)),
			Ok(validation_result),
		]),
		validation_data.clone(),
		validation_code,
		candidate_receipt,
		Arc::new(pov),
		ExecutorParams::default(),
		PvfExecKind::Approval,
		&Default::default(),
		false,
		PreValidationOutput {
			validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
			claim_queue: None,
		},
	))
	.unwrap();

	assert_matches!(v, ValidationResult::Valid(outputs, used_validation_data) => {
		assert_eq!(outputs.head_data, HeadData(vec![1, 1, 1]));
		assert_eq!(outputs.upward_messages, Vec::<UpwardMessage>::new());
		assert_eq!(outputs.horizontal_messages, Vec::new());
		assert_eq!(outputs.new_validation_code, Some(vec![2, 2, 2].into()));
		assert_eq!(outputs.hrmp_watermark, 0);
		assert_eq!(used_validation_data, validation_data);
	});
}

#[test]
fn candidate_validation_multiple_ambiguous_errors_is_invalid() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };

	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let validation_code = ValidationCode(vec![2; 16]);

	let descriptor = perform_basic_checks_on_valid_candidate(
		&pov,
		&validation_code,
		&validation_data,
		dummy_hash(),
	);

	let candidate_receipt = CandidateReceipt { descriptor, commitments_hash: Hash::zero() };

	let v = executor::block_on(validate_candidate(
		MockValidateCandidateBackend::with_hardcoded_result_list(vec![
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath)),
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath)),
		]),
		validation_data,
		validation_code,
		candidate_receipt,
		Arc::new(pov),
		ExecutorParams::default(),
		PvfExecKind::Approval,
		&Default::default(),
		false,
		PreValidationOutput {
			validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
			claim_queue: None,
		},
	))
	.unwrap();

	assert_matches!(v, ValidationResult::Invalid(InvalidCandidate::ExecutionError(_)));
}

// Test that we retry for approval on internal errors.
#[test]
fn candidate_validation_retry_internal_errors() {
	let v = candidate_validation_retry_on_error_helper(
		PvfExecKind::Approval,
		vec![
			Err(InternalValidationError::HostCommunication("foo".into()).into()),
			// Throw an AJD error, we should still retry again.
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousJobDeath(
				"baz".into(),
			))),
			// Throw another internal error.
			Err(InternalValidationError::HostCommunication("bar".into()).into()),
		],
	);
	assert_matches!(v, Err(ValidationFailed(s)) if s.contains("bar"));
}

// Test that we don't retry for backing on internal errors.
#[test]
fn candidate_validation_dont_retry_internal_errors() {
	let v = candidate_validation_retry_on_error_helper(
		PvfExecKind::Backing(dummy_hash()),
		vec![
			Err(InternalValidationError::HostCommunication("foo".into()).into()),
			// Throw an AWD error, we should still retry again.
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath)),
			// Throw another internal error.
			Err(InternalValidationError::HostCommunication("bar".into()).into()),
		],
	);

	assert_matches!(v, Err(ValidationFailed(s)) if s.contains("foo"));
}

// Test that we retry for approval on panic errors.
#[test]
fn candidate_validation_retry_panic_errors() {
	let v = candidate_validation_retry_on_error_helper(
		PvfExecKind::Approval,
		vec![
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::JobError("foo".into()))),
			// Throw an AWD error, we should still retry again.
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath)),
			// Throw another panic error.
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::JobError("bar".into()))),
		],
	);

	assert_matches!(v, Ok(ValidationResult::Invalid(InvalidCandidate::ExecutionError(s))) if s == "bar".to_string());
}

// Test that we don't retry for backing on panic errors.
#[test]
fn candidate_validation_dont_retry_panic_errors() {
	let v = candidate_validation_retry_on_error_helper(
		PvfExecKind::Backing(dummy_hash()),
		vec![
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::JobError("foo".into()))),
			// Throw an AWD error, we should still retry again.
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::AmbiguousWorkerDeath)),
			// Throw another panic error.
			Err(ValidationError::PossiblyInvalid(PossiblyInvalidError::JobError("bar".into()))),
		],
	);

	assert_matches!(v, Ok(ValidationResult::Invalid(InvalidCandidate::ExecutionError(s))) if s == "foo".to_string());
}

fn candidate_validation_retry_on_error_helper(
	exec_kind: PvfExecKind,
	mock_errors: Vec<Result<WasmValidationResult, ValidationError>>,
) -> Result<ValidationResult, ValidationFailed> {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };

	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let validation_code = ValidationCode(vec![2; 16]);

	let descriptor = make_valid_candidate_descriptor(
		ParaId::from(1_u32),
		dummy_hash(),
		validation_data.hash(),
		pov.hash(),
		validation_code.hash(),
		dummy_hash(),
		dummy_hash(),
		Sr25519Keyring::Alice,
	)
	.into();

	let check = perform_basic_checks(
		&descriptor,
		validation_data.max_pov_size,
		&pov,
		&validation_code.hash(),
	);
	assert!(check.is_ok());

	let candidate_receipt = CandidateReceipt { descriptor, commitments_hash: Hash::zero() };

	return executor::block_on(validate_candidate(
		MockValidateCandidateBackend::with_hardcoded_result_list(mock_errors),
		validation_data,
		validation_code,
		candidate_receipt,
		Arc::new(pov),
		ExecutorParams::default(),
		exec_kind,
		&Default::default(),
		false,
		PreValidationOutput {
			validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
			claim_queue: None,
		},
	));
}

#[test]
fn candidate_validation_timeout_is_internal_error() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };

	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let validation_code = ValidationCode(vec![2; 16]);

	let descriptor = make_valid_candidate_descriptor(
		ParaId::from(1_u32),
		dummy_hash(),
		validation_data.hash(),
		pov.hash(),
		validation_code.hash(),
		dummy_hash(),
		dummy_hash(),
		Sr25519Keyring::Alice,
	)
	.into();

	let check = perform_basic_checks(
		&descriptor,
		validation_data.max_pov_size,
		&pov,
		&validation_code.hash(),
	);
	assert!(check.is_ok());

	let candidate_receipt = CandidateReceipt { descriptor, commitments_hash: Hash::zero() };

	let v = executor::block_on(validate_candidate(
		MockValidateCandidateBackend::with_hardcoded_result(Err(ValidationError::Invalid(
			WasmInvalidCandidate::HardTimeout,
		))),
		validation_data,
		validation_code,
		candidate_receipt,
		Arc::new(pov),
		ExecutorParams::default(),
		PvfExecKind::Backing(dummy_hash()),
		&Default::default(),
		false,
		PreValidationOutput {
			validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
			claim_queue: None,
		},
	));

	assert_matches!(v, Ok(ValidationResult::Invalid(InvalidCandidate::Timeout)));
}

#[test]
fn candidate_validation_commitment_hash_mismatch_is_invalid() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };
	let pov = PoV { block_data: BlockData(vec![0xff; 32]) };
	let validation_code = ValidationCode(vec![0xff; 16]);
	let head_data = HeadData(vec![1, 1, 1]);

	let candidate_receipt = CandidateReceipt {
		descriptor: make_valid_candidate_descriptor(
			ParaId::from(1_u32),
			validation_data.parent_head.hash(),
			validation_data.hash(),
			pov.hash(),
			validation_code.hash(),
			head_data.hash(),
			dummy_hash(),
			Sr25519Keyring::Alice,
		)
		.into(),
		commitments_hash: Hash::zero(),
	}
	.into();

	// This will result in different commitments for this candidate.
	let validation_result = WasmValidationResult {
		head_data,
		new_validation_code: None,
		upward_messages: Default::default(),
		horizontal_messages: Default::default(),
		processed_downward_messages: 0,
		hrmp_watermark: 12345,
	};

	let result = executor::block_on(validate_candidate(
		MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result)),
		validation_data,
		validation_code,
		candidate_receipt,
		Arc::new(pov),
		ExecutorParams::default(),
		PvfExecKind::Backing(dummy_hash()),
		&Default::default(),
		false,
		PreValidationOutput {
			validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
			claim_queue: None,
		},
	))
	.unwrap();

	// Ensure `post validation` check on the commitments hash works as expected.
	assert_matches!(result, ValidationResult::Invalid(InvalidCandidate::CommitmentsHashMismatch));
}

#[test]
fn compressed_code_works() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };
	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let head_data = HeadData(vec![1, 1, 1]);

	let raw_code = vec![2u8; 16];
	let validation_code =
		sp_maybe_compressed_blob::compress_strongly(&raw_code, VALIDATION_CODE_BOMB_LIMIT as usize)
			.map(ValidationCode)
			.unwrap();

	let descriptor = make_valid_candidate_descriptor(
		ParaId::from(1_u32),
		dummy_hash(),
		validation_data.hash(),
		pov.hash(),
		validation_code.hash(),
		head_data.hash(),
		dummy_hash(),
		Sr25519Keyring::Alice,
	)
	.into();

	let validation_result = WasmValidationResult {
		head_data,
		new_validation_code: None,
		upward_messages: Default::default(),
		horizontal_messages: Default::default(),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};

	let commitments = CandidateCommitments {
		head_data: validation_result.head_data.clone(),
		upward_messages: validation_result.upward_messages.clone(),
		horizontal_messages: validation_result.horizontal_messages.clone(),
		new_validation_code: validation_result.new_validation_code.clone(),
		processed_downward_messages: validation_result.processed_downward_messages,
		hrmp_watermark: validation_result.hrmp_watermark,
	};

	let candidate_receipt = CandidateReceipt { descriptor, commitments_hash: commitments.hash() };

	let v = executor::block_on(validate_candidate(
		MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result)),
		validation_data,
		validation_code,
		candidate_receipt,
		Arc::new(pov),
		ExecutorParams::default(),
		PvfExecKind::Backing(dummy_hash()),
		&Default::default(),
		false,
		PreValidationOutput {
			validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
			claim_queue: None,
		},
	));

	assert_matches!(v, Ok(ValidationResult::Valid(_, _)));
}

struct MockPreCheckBackend {
	result: Result<(), PrepareError>,
}

impl MockPreCheckBackend {
	fn with_hardcoded_result(result: Result<(), PrepareError>) -> Self {
		Self { result }
	}
}

#[async_trait]
impl ValidationBackend for MockPreCheckBackend {
	async fn validate_candidate(
		&mut self,
		_pvf: PvfPrepData,
		_validation_context: ValidationContext,
		_exec_kind: PvfExecKind,
	) -> Result<WasmValidationResult, ValidationError> {
		unreachable!()
	}

	async fn precheck_pvf(&mut self, _pvf: PvfPrepData) -> Result<(), PrepareError> {
		self.result.clone()
	}

	async fn heads_up(&mut self, _active_pvfs: Vec<PvfPrepData>) -> Result<(), String> {
		unreachable!()
	}

	async fn update_active_leaves(
		&mut self,
		_update: ActiveLeavesUpdate,
		_ancestors: Vec<Hash>,
	) -> Result<(), String> {
		unreachable!()
	}
}

#[test]
fn precheck_works() {
	let relay_parent = [3; 32].into();
	let validation_code = ValidationCode(vec![3; 16]);
	let validation_code_hash = validation_code.hash();

	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());

	let (check_fut, check_result) = precheck_pvf(
		ctx.sender(),
		MockPreCheckBackend::with_hardcoded_result(Ok(())),
		relay_parent,
		validation_code_hash,
		VALIDATION_CODE_BOMB_LIMIT,
	)
	.remote_handle();

	let test_fut = async move {
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				rp,
				RuntimeApiRequest::ValidationCodeByHash(
					vch,
					tx
				),
			)) => {
				assert_eq!(vch, validation_code_hash);
				assert_eq!(rp, relay_parent);

				let _ = tx.send(Ok(Some(validation_code.clone())));
			}
		);
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionIndexForChild(tx))
			) => {
				tx.send(Ok(1u32.into())).unwrap();
			}
		);
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionExecutorParams(_, tx))
			) => {
				tx.send(Ok(Some(ExecutorParams::default()))).unwrap();
			}
		);
		assert_matches!(check_result.await, PreCheckOutcome::Valid);
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);
}

#[test]
fn precheck_properly_classifies_outcomes() {
	let inner = |prepare_result, precheck_outcome| {
		let relay_parent = [3; 32].into();
		let validation_code = ValidationCode(vec![3; 16]);
		let validation_code_hash = validation_code.hash();

		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());

		let (check_fut, check_result) = precheck_pvf(
			ctx.sender(),
			MockPreCheckBackend::with_hardcoded_result(prepare_result),
			relay_parent,
			validation_code_hash,
			VALIDATION_CODE_BOMB_LIMIT,
		)
		.remote_handle();

		let test_fut = async move {
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::ValidationCodeByHash(
						vch,
						tx
					),
				)) => {
					assert_eq!(vch, validation_code_hash);
					assert_eq!(rp, relay_parent);

					let _ = tx.send(Ok(Some(validation_code.clone())));
				}
			);
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(
					RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionIndexForChild(tx))
				) => {
					tx.send(Ok(1u32.into())).unwrap();
				}
			);
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(
					RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionExecutorParams(_, tx))
				) => {
					tx.send(Ok(Some(ExecutorParams::default()))).unwrap();
				}
			);
			assert_eq!(check_result.await, precheck_outcome);
		};

		let test_fut = future::join(test_fut, check_fut);
		executor::block_on(test_fut);
	};

	inner(Err(PrepareError::Prevalidation("foo".to_owned())), PreCheckOutcome::Invalid);
	inner(Err(PrepareError::Preparation("bar".to_owned())), PreCheckOutcome::Invalid);
	inner(Err(PrepareError::JobError("baz".to_owned())), PreCheckOutcome::Invalid);

	inner(Err(PrepareError::TimedOut), PreCheckOutcome::Failed);
	inner(Err(PrepareError::IoErr("fizz".to_owned())), PreCheckOutcome::Failed);
}

#[derive(Default, Clone)]
struct MockHeadsUp {
	heads_up_call_count: Arc<AtomicUsize>,
}

#[async_trait]
impl ValidationBackend for MockHeadsUp {
	async fn validate_candidate(
		&mut self,
		_pvf: PvfPrepData,
		_validation_context: ValidationContext,
		_exec_kind: PvfExecKind,
	) -> Result<WasmValidationResult, ValidationError> {
		unreachable!()
	}

	async fn precheck_pvf(&mut self, _pvf: PvfPrepData) -> Result<(), PrepareError> {
		unreachable!()
	}

	async fn heads_up(&mut self, _active_pvfs: Vec<PvfPrepData>) -> Result<(), String> {
		let _ = self.heads_up_call_count.fetch_add(1, Ordering::SeqCst);
		Ok(())
	}

	async fn update_active_leaves(
		&mut self,
		_update: ActiveLeavesUpdate,
		_ancestors: Vec<Hash>,
	) -> Result<(), String> {
		Ok(())
	}
}

fn alice_keystore() -> KeystorePtr {
	let keystore: KeystorePtr = Arc::new(MemoryKeystore::new());
	let _ = Keystore::sr25519_generate_new(
		&*keystore,
		ValidatorId::ID,
		Some(&Sr25519Keyring::Alice.to_seed()),
	)
	.unwrap();
	let _ = Keystore::sr25519_generate_new(
		&*keystore,
		AuthorityDiscoveryId::ID,
		Some(&Sr25519Keyring::Alice.to_seed()),
	)
	.unwrap();

	keystore
}

fn dummy_active_leaves_update(hash: Hash) -> ActiveLeavesUpdate {
	ActiveLeavesUpdate {
		activated: Some(ActivatedLeaf {
			hash,
			number: 10,
			unpin_handle: polkadot_node_subsystem_test_helpers::mock::dummy_unpin_handle(hash),
		}),
		..Default::default()
	}
}

fn dummy_candidate_backed(
	relay_parent: Hash,
	validation_code_hash: ValidationCodeHash,
) -> CandidateEvent {
	let zeros = dummy_hash();
	let descriptor = CandidateDescriptor {
		para_id: ParaId::from(0_u32),
		relay_parent,
		collator: dummy_collator(),
		persisted_validation_data_hash: zeros,
		pov_hash: zeros,
		erasure_root: zeros,
		signature: dummy_collator_signature(),
		para_head: zeros,
		validation_code_hash,
	}
	.into();

	CandidateEvent::CandidateBacked(
		CandidateReceipt { descriptor, commitments_hash: zeros },
		HeadData(Vec::new()),
		CoreIndex(0),
		GroupIndex(0),
	)
}

fn dummy_session_info(keys: Vec<Public>) -> SessionInfo {
	SessionInfo {
		validators: keys.iter().cloned().map(Into::into).collect(),
		discovery_keys: keys.iter().cloned().map(Into::into).collect(),
		assignment_keys: vec![],
		validator_groups: Default::default(),
		n_cores: 4u32,
		zeroth_delay_tranche_width: 0u32,
		relay_vrf_modulo_samples: 0u32,
		n_delay_tranches: 2u32,
		no_show_slots: 0u32,
		needed_approvals: 1u32,
		active_validator_indices: vec![],
		dispute_period: 6,
		random_seed: [0u8; 32],
	}
}

async fn assert_new_active_leaf_messages(
	recv_handle: &mut TestSubsystemContextHandle<AllMessages>,
	expected_session_index: SessionIndex,
) {
	assert_matches!(
		recv_handle.recv().await,
		AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionIndexForChild(tx))) => {
			let _ = tx.send(Ok(expected_session_index));
		}
	);

	let lookahead_value = DEFAULT_SCHEDULING_LOOKAHEAD;
	assert_matches!(
		recv_handle.recv().await,
		AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SchedulingLookahead(index, tx))) => {
			assert_eq!(index, expected_session_index);
			let _ = tx.send(Ok(lookahead_value));
		}
	);

	assert_matches!(
		recv_handle.recv().await,
		AllMessages::ChainApi(ChainApiMessage::Ancestors {k, response_channel, ..}) => {
			assert_eq!(k as u32, lookahead_value - 1);
			let _ = response_channel.send(Ok((0..(lookahead_value - 1)).into_iter().map(|i| Hash::from_low_u64_be(i as u64)).collect()));
		}
	);

	// Second SessionIndexForChild — from handle_active_leaves_update's own
	// get_session_index call (separate from the one in update_active_leaves_validation_backend).
	assert_matches!(
		recv_handle.recv().await,
		AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionIndexForChild(tx))) => {
			let _ = tx.send(Ok(expected_session_index));
		}
	);
}

#[test]
fn maybe_prepare_validation_golden_path() {
	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

	let keystore = alice_keystore();
	let mut backend = MockHeadsUp::default();
	let activated_hash = Hash::random();
	let update = dummy_active_leaves_update(activated_hash);
	let mut state = State::default();

	let check_fut =
		handle_active_leaves_update(ctx.sender(), keystore, &mut backend, update, &mut state);

	let test_fut = async move {
		assert_new_active_leaf_messages(&mut ctx_handle, 1).await;

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::NodeFeatures(_, tx))) => {
				let _ = tx.send(Ok(NodeFeatures::new()));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::Authorities(tx))) => {
				let _ = tx.send(Ok(vec![Sr25519Keyring::Alice.public().into()]));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionInfo(index, tx))) => {
				assert_eq!(index, 1);
				let _ = tx.send(Ok(Some(dummy_session_info(vec![Sr25519Keyring::Bob.public()]))));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::CandidateEvents(tx))) => {
				let _ = tx.send(Ok(vec![dummy_candidate_backed(activated_hash, dummy_hash().into())]));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionIndexForChild(tx))) => {
				let _ = tx.send(Ok(1));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionExecutorParams(index, tx))) => {
				assert_eq!(index, 1);
				let _ = tx.send(Ok(Some(ExecutorParams::default())));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::ValidationCodeByHash(hash, tx))) => {
				assert_eq!(hash, dummy_hash().into());
				let _ = tx.send(Ok(Some(ValidationCode(Vec::new()))));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionIndexForChild(tx))) => {
				let _ = tx.send(Ok(1));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::ValidationCodeBombLimit(session, tx))) => {
				assert_eq!(session, 1);
				let _ = tx.send(Ok(VALIDATION_CODE_BOMB_LIMIT));
			}
		);
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);

	assert_eq!(backend.heads_up_call_count.load(Ordering::SeqCst), 1);
	assert!(state.session_index.is_some());
	assert!(state.pvf_prep.is_next_session_authority);
}

#[test]
fn maybe_prepare_validation_checkes_authority_once_per_session() {
	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

	let keystore = alice_keystore();
	let mut backend = MockHeadsUp::default();
	let activated_hash = Hash::random();
	let update = dummy_active_leaves_update(activated_hash);
	let mut state = State { session_index: Some(1), ..Default::default() };

	let check_fut =
		handle_active_leaves_update(ctx.sender(), keystore, &mut backend, update, &mut state);

	let test_fut = assert_new_active_leaf_messages(&mut ctx_handle, 1);

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);

	assert_eq!(backend.heads_up_call_count.load(Ordering::SeqCst), 0);
	assert!(state.session_index.is_some());
	assert!(!state.pvf_prep.is_next_session_authority);
}

#[test]
fn maybe_prepare_validation_resets_state_on_a_new_session() {
	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

	let keystore = alice_keystore();
	let mut backend = MockHeadsUp::default();
	let activated_hash = Hash::random();
	let update = dummy_active_leaves_update(activated_hash);
	let mut state = State {
		session_index: Some(1),
		pvf_prep: PvfPrepState {
			is_next_session_authority: true,
			already_prepared_code_hashes: HashSet::from_iter(vec![
				ValidationCode(vec![0; 16]).hash()
			]),
			..Default::default()
		},
		..Default::default()
	};

	let check_fut =
		handle_active_leaves_update(ctx.sender(), keystore, &mut backend, update, &mut state);

	let test_fut = async move {
		assert_new_active_leaf_messages(&mut ctx_handle, 2).await;

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::NodeFeatures(_, tx))) => {
				let _ = tx.send(Ok(NodeFeatures::new()));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::Authorities(tx))) => {
				let _ = tx.send(Ok(vec![Sr25519Keyring::Bob.public().into()]));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionInfo(index, tx))) => {
				assert_eq!(index, 2);
				let _ = tx.send(Ok(Some(dummy_session_info(vec![Sr25519Keyring::Bob.public()]))));
			}
		);
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);

	assert_eq!(backend.heads_up_call_count.load(Ordering::SeqCst), 0);
	assert_eq!(state.session_index.unwrap(), 2);
	assert!(!state.pvf_prep.is_next_session_authority);
	assert!(state.pvf_prep.already_prepared_code_hashes.is_empty());
}

#[test]
fn maybe_prepare_validation_does_not_prepare_pvfs_if_no_new_session_and_not_a_validator() {
	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

	let keystore = alice_keystore();
	let mut backend = MockHeadsUp::default();
	let activated_hash = Hash::random();
	let update = dummy_active_leaves_update(activated_hash);
	let mut state = State { session_index: Some(1), ..Default::default() };

	let check_fut =
		handle_active_leaves_update(ctx.sender(), keystore, &mut backend, update, &mut state);

	let test_fut = assert_new_active_leaf_messages(&mut ctx_handle, 1);

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);

	assert_eq!(backend.heads_up_call_count.load(Ordering::SeqCst), 0);
	assert!(state.session_index.is_some());
	assert!(!state.pvf_prep.is_next_session_authority);
}

#[test]
fn maybe_prepare_validation_does_not_prepare_pvfs_if_no_new_session_but_a_validator() {
	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

	let keystore = alice_keystore();
	let mut backend = MockHeadsUp::default();
	let activated_hash = Hash::random();
	let update = dummy_active_leaves_update(activated_hash);
	let mut state = State {
		session_index: Some(1),
		pvf_prep: PvfPrepState { is_next_session_authority: true, ..Default::default() },
		..Default::default()
	};

	let check_fut =
		handle_active_leaves_update(ctx.sender(), keystore, &mut backend, update, &mut state);

	let test_fut = async move {
		assert_new_active_leaf_messages(&mut ctx_handle, 1).await;

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::CandidateEvents(tx))) => {
				let _ = tx.send(Ok(vec![dummy_candidate_backed(activated_hash, dummy_hash().into())]));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionIndexForChild(tx))) => {
				let _ = tx.send(Ok(1));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionExecutorParams(index, tx))) => {
				assert_eq!(index, 1);
				let _ = tx.send(Ok(Some(ExecutorParams::default())));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::ValidationCodeByHash(hash, tx))) => {
				assert_eq!(hash, dummy_hash().into());
				let _ = tx.send(Ok(Some(ValidationCode(Vec::new()))));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionIndexForChild(tx))) => {
				let _ = tx.send(Ok(1));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::ValidationCodeBombLimit(session, tx))) => {
				assert_eq!(session, 1);
				let _ = tx.send(Ok(VALIDATION_CODE_BOMB_LIMIT));
			}
		);
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);

	assert_eq!(backend.heads_up_call_count.load(Ordering::SeqCst), 1);
	assert!(state.session_index.is_some());
	assert!(state.pvf_prep.is_next_session_authority);
}

#[test]
fn maybe_prepare_validation_does_not_prepare_pvfs_if_not_a_validator_in_the_next_session() {
	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

	let keystore = alice_keystore();
	let mut backend = MockHeadsUp::default();
	let activated_hash = Hash::random();
	let update = dummy_active_leaves_update(activated_hash);
	let mut state = State::default();

	let check_fut =
		handle_active_leaves_update(ctx.sender(), keystore, &mut backend, update, &mut state);

	let test_fut = async move {
		assert_new_active_leaf_messages(&mut ctx_handle, 1).await;

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::NodeFeatures(_, tx))) => {
				let _ = tx.send(Ok(NodeFeatures::new()));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::Authorities(tx))) => {
				let _ = tx.send(Ok(vec![Sr25519Keyring::Bob.public().into()]));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionInfo(index, tx))) => {
				assert_eq!(index, 1);
				let _ = tx.send(Ok(Some(dummy_session_info(vec![Sr25519Keyring::Bob.public()]))));
			}
		);
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);

	assert_eq!(backend.heads_up_call_count.load(Ordering::SeqCst), 0);
	assert!(state.session_index.is_some());
	assert!(!state.pvf_prep.is_next_session_authority);
}

#[test]
fn maybe_prepare_validation_does_not_prepare_pvfs_if_a_validator_in_the_current_session() {
	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

	let keystore = alice_keystore();
	let mut backend = MockHeadsUp::default();
	let activated_hash = Hash::random();
	let update = dummy_active_leaves_update(activated_hash);
	let mut state = State::default();

	let check_fut =
		handle_active_leaves_update(ctx.sender(), keystore, &mut backend, update, &mut state);

	let test_fut = async move {
		assert_new_active_leaf_messages(&mut ctx_handle, 1).await;

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::NodeFeatures(_, tx))) => {
				let _ = tx.send(Ok(NodeFeatures::new()));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::Authorities(tx))) => {
				let _ = tx.send(Ok(vec![Sr25519Keyring::Alice.public().into()]));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionInfo(index, tx))) => {
				assert_eq!(index, 1);
				let _ = tx.send(Ok(Some(dummy_session_info(vec![Sr25519Keyring::Alice.public()]))));
			}
		);
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);

	assert_eq!(backend.heads_up_call_count.load(Ordering::SeqCst), 0);
	assert!(state.session_index.is_some());
	assert!(!state.pvf_prep.is_next_session_authority);
}

#[test]
fn maybe_prepare_validation_prepares_a_limited_number_of_pvfs() {
	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

	let keystore = alice_keystore();
	let mut backend = MockHeadsUp::default();
	let activated_hash = Hash::random();
	let update = dummy_active_leaves_update(activated_hash);
	let mut state = State {
		pvf_prep: PvfPrepState { per_block_limit: 2, ..Default::default() },
		..Default::default()
	};

	let check_fut =
		handle_active_leaves_update(ctx.sender(), keystore, &mut backend, update, &mut state);

	let test_fut = async move {
		assert_new_active_leaf_messages(&mut ctx_handle, 1).await;

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::NodeFeatures(_, tx))) => {
				let _ = tx.send(Ok(NodeFeatures::new()));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::Authorities(tx))) => {
				let _ = tx.send(Ok(vec![Sr25519Keyring::Alice.public().into()]));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionInfo(index, tx))) => {
				assert_eq!(index, 1);
				let _ = tx.send(Ok(Some(dummy_session_info(vec![Sr25519Keyring::Bob.public()]))));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::CandidateEvents(tx))) => {
				let candidates = vec![
					dummy_candidate_backed(activated_hash, ValidationCode(vec![0; 16]).hash()),
					dummy_candidate_backed(activated_hash, ValidationCode(vec![1; 16]).hash()),
					dummy_candidate_backed(activated_hash, ValidationCode(vec![2; 16]).hash()),
				];
				let _ = tx.send(Ok(candidates));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionIndexForChild(tx))) => {
				let _ = tx.send(Ok(1));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionExecutorParams(index, tx))) => {
				assert_eq!(index, 1);
				let _ = tx.send(Ok(Some(ExecutorParams::default())));
			}
		);

		for c in 0..2 {
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::ValidationCodeByHash(hash, tx))) => {
					assert_eq!(hash, ValidationCode(vec![c; 16]).hash());
					let _ = tx.send(Ok(Some(ValidationCode(Vec::new()))));
				}
			);

			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionIndexForChild(tx))) => {
					let _ = tx.send(Ok(1));
				}
			);

			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::ValidationCodeBombLimit(session, tx))) => {
					assert_eq!(session, 1);
					let _ = tx.send(Ok(VALIDATION_CODE_BOMB_LIMIT));
				}
			);
		}
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);

	assert_eq!(backend.heads_up_call_count.load(Ordering::SeqCst), 1);
	assert!(state.session_index.is_some());
	assert!(state.pvf_prep.is_next_session_authority);
	assert_eq!(state.pvf_prep.already_prepared_code_hashes.len(), 2);
}

#[test]
fn maybe_prepare_validation_does_not_prepare_already_prepared_pvfs() {
	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

	let keystore = alice_keystore();
	let mut backend = MockHeadsUp::default();
	let activated_hash = Hash::random();
	let update = dummy_active_leaves_update(activated_hash);
	let mut state = State {
		session_index: Some(1),
		pvf_prep: PvfPrepState {
			is_next_session_authority: true,
			per_block_limit: 2,
			already_prepared_code_hashes: HashSet::from_iter(vec![
				ValidationCode(vec![0; 16]).hash(),
				ValidationCode(vec![1; 16]).hash(),
			]),
		},
		..Default::default()
	};

	let check_fut =
		handle_active_leaves_update(ctx.sender(), keystore, &mut backend, update, &mut state);

	let test_fut = async move {
		assert_new_active_leaf_messages(&mut ctx_handle, 1).await;

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::CandidateEvents(tx))) => {
				let candidates = vec![
					dummy_candidate_backed(activated_hash, ValidationCode(vec![0; 16]).hash()),
					dummy_candidate_backed(activated_hash, ValidationCode(vec![1; 16]).hash()),
					dummy_candidate_backed(activated_hash, ValidationCode(vec![2; 16]).hash()),
				];
				let _ = tx.send(Ok(candidates));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionIndexForChild(tx))) => {
				let _ = tx.send(Ok(1));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionExecutorParams(index, tx))) => {
				assert_eq!(index, 1);
				let _ = tx.send(Ok(Some(ExecutorParams::default())));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::ValidationCodeByHash(hash, tx))) => {
				assert_eq!(hash, ValidationCode(vec![2; 16]).hash());
				let _ = tx.send(Ok(Some(ValidationCode(Vec::new()))));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::SessionIndexForChild(tx))) => {
				let _ = tx.send(Ok(1));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(_, RuntimeApiRequest::ValidationCodeBombLimit(session, tx))) => {
				assert_eq!(session, 1);
				let _ = tx.send(Ok(VALIDATION_CODE_BOMB_LIMIT));
			}
		);
	};

	let test_fut = future::join(test_fut, check_fut);
	executor::block_on(test_fut);

	assert_eq!(backend.heads_up_call_count.load(Ordering::SeqCst), 1);
	assert!(state.session_index.is_some());
	assert!(state.pvf_prep.is_next_session_authority);
	assert_eq!(state.pvf_prep.already_prepared_code_hashes.len(), 3);
}

/// Verify that a V3 descriptor is interpreted differently depending on `v3_ever_seen`.
///
/// Before V3 activation: old rules apply — V3 descriptors appear as V1, so
/// `scheduling_parent` falls back to `relay_parent`.
///
/// After V3 activation: new rules apply — V3 descriptors are correctly identified,
/// so `scheduling_parent` returns the real scheduling parent from the descriptor.
///
/// Verify that `handle_active_leaves_update` correctly detects V3 node features on
/// session changes and sets `v3_ever_seen` accordingly.
///
/// Scenario:
/// 1. First leaf at session 1, V3 OFF → `v3_ever_seen` stays false
/// 2. Second leaf at session 2, V3 ON → `v3_ever_seen` becomes true
/// 3. Third leaf at session 2 (same session) → no re-check (monotonic flag)
#[test]
fn v3_feature_detected_on_session_change() {
	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);

	let keystore = alice_keystore();
	let mut backend = MockHeadsUp::default();
	let mut state = State::default();

	// --- Leaf 1: session 1, V3 feature NOT enabled ---
	let leaf1_hash = Hash::repeat_byte(0x01);
	let update1 = dummy_active_leaves_update(leaf1_hash);

	let check_fut = handle_active_leaves_update(
		ctx.sender(),
		keystore.clone(),
		&mut backend,
		update1,
		&mut state,
	);

	let test_fut = async move {
		// Standard leaf activation messages
		assert_new_active_leaf_messages(&mut ctx_handle, 1).await;

		// NodeFeatures request — return EMPTY (no V3)
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::NodeFeatures(_, tx),
			)) => {
				let _ = tx.send(Ok(NodeFeatures::new()));
			}
		);

		// Authorities (PVF prep) — return empty so we skip PVF prep
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::Authorities(tx),
			)) => {
				let _ = tx.send(Ok(vec![]));
			}
		);

		// SessionInfo (check_next_session_authority always fetches this)
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::SessionInfo(idx, tx),
			)) => {
				assert_eq!(idx, 1);
				let _ = tx.send(Ok(Some(dummy_session_info(vec![]))));
			}
		);

		ctx_handle
	};

	let (test_fut, check_fut) = (test_fut, check_fut);
	let (mut ctx_handle, _) = executor::block_on(future::join(test_fut, check_fut));

	assert_eq!(state.session_index, Some(1));
	assert!(!state.v3_ever_seen, "V3 should not be detected yet");

	// --- Leaf 2: session 2, V3 feature ENABLED ---
	let leaf2_hash = Hash::repeat_byte(0x02);
	let update2 = dummy_active_leaves_update(leaf2_hash);

	let check_fut = handle_active_leaves_update(
		ctx.sender(),
		keystore.clone(),
		&mut backend,
		update2,
		&mut state,
	);

	let test_fut = async move {
		assert_new_active_leaf_messages(&mut ctx_handle, 2).await;

		// NodeFeatures request — return V3 ENABLED
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::NodeFeatures(_, tx),
			)) => {
				let mut features = NodeFeatures::new();
				features.resize(FeatureIndex::CandidateReceiptV3 as usize + 1, false);
				features.set(FeatureIndex::CandidateReceiptV3 as usize, true);
				let _ = tx.send(Ok(features));
			}
		);

		// Authorities + SessionInfo for PVF prep
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::Authorities(tx),
			)) => {
				let _ = tx.send(Ok(vec![]));
			}
		);

		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::SessionInfo(idx, tx),
			)) => {
				assert_eq!(idx, 2);
				let _ = tx.send(Ok(Some(dummy_session_info(vec![]))));
			}
		);

		ctx_handle
	};

	let (mut ctx_handle, _) = executor::block_on(future::join(test_fut, check_fut));

	assert_eq!(state.session_index, Some(2));
	assert!(state.v3_ever_seen, "V3 should be detected now");

	// --- Leaf 3: same session 2, no new session → no V3 re-check ---
	let leaf3_hash = Hash::repeat_byte(0x03);
	let update3 = dummy_active_leaves_update(leaf3_hash);

	let check_fut =
		handle_active_leaves_update(ctx.sender(), keystore, &mut backend, update3, &mut state);

	let test_fut = async move {
		// Same session — only the standard leaf messages, no NodeFeatures query
		assert_new_active_leaf_messages(&mut ctx_handle, 2).await;
	};

	executor::block_on(future::join(test_fut, check_fut));

	assert_eq!(state.session_index, Some(2));
	assert!(state.v3_ever_seen, "V3 flag is monotonic — stays true");
}

// ============================================================================
// Subsystem-level tests: exercise handle_validation_message end-to-end.
//
// These test the real message handling path with mocked runtime API responses,
// ensuring pre-validation, PVF execution, and post-validation are wired
// correctly.
// ============================================================================

/// Default session params for tests.
fn default_session_params() -> SessionParams {
	SessionParams {
		executor_params: ExecutorParams::default(),
		validation_code_bomb_limit: VALIDATION_CODE_BOMB_LIMIT,
	}
}

/// V3 descriptor: executor_params use the execution (relay-parent) session,
/// bomb limit uses the scheduling session. Verify the correct session index is
/// passed to each runtime API call.
///
/// Runs two sub-cases, both with relay parent != scheduling parent:
/// - scheduling session == execution session
/// - scheduling session != execution session
#[test]
fn fetch_params_uses_correct_sessions_for_v3() {
	let relay_parent = Hash::repeat_byte(0xAA);
	let scheduling_parent = Hash::repeat_byte(0xBB);
	let recent_leaf = Hash::repeat_byte(0xCC);
	// Caller fallback — must NOT be used for V3 descriptors.
	let caller_session: SessionIndex = 99;

	let cases: [(SessionIndex, SessionIndex); 2] = [(5, 5), (5, 7)];
	for (execution_session, scheduling_session) in cases {
		let pov = PoV { block_data: BlockData(vec![1]) };
		let validation_code = ValidationCode(vec![2]);
		let descriptor = make_valid_candidate_descriptor_v3(
			ParaId::from(1_u32),
			relay_parent,
			CoreIndex(0),
			execution_session,
			scheduling_session,
			dummy_hash(),
			pov.hash(),
			validation_code.hash(),
			dummy_hash(),
			dummy_hash(),
			scheduling_parent,
		);

		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());

		let test_fut = async move {
			// First call: SessionExecutorParams — must use execution_session.
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					leaf,
					RuntimeApiRequest::SessionExecutorParams(session, tx),
				)) => {
					assert_eq!(leaf, recent_leaf);
					assert_eq!(session, execution_session, "executor_params must use execution session");
					tx.send(Ok(Some(ExecutorParams::default()))).unwrap();
				}
			);

			// Second call: ValidationCodeBombLimit — must use scheduling_session.
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					leaf,
					RuntimeApiRequest::ValidationCodeBombLimit(session, tx),
				)) => {
					assert_eq!(leaf, recent_leaf);
					assert_eq!(session, scheduling_session, "bomb limit must use scheduling session");
					tx.send(Ok(VALIDATION_CODE_BOMB_LIMIT)).unwrap();
				}
			);
		};

		let fetch_fut = async move {
			let mut cache = SessionCache::new();

			let result = cache
				.fetch_params(recent_leaf, caller_session, &descriptor, true, ctx.sender())
				.await;

			assert!(result.is_ok(), "fetch_params should succeed");
		};

		executor::block_on(future::join(test_fut, fetch_fut));
	}
}

/// V1 descriptor: both executor_params and bomb limit fall back to the
/// caller-provided session index.
#[test]
fn fetch_params_uses_fallback_session_for_v1() {
	let recent_leaf = Hash::repeat_byte(0xCC);
	let caller_session: SessionIndex = 42;

	let pov = PoV { block_data: BlockData(vec![1]) };
	let validation_code = ValidationCode(vec![2]);
	// V1-like descriptor within the V2 struct — reserved fields are non-zero so
	// version() returns V1, and session_index_for_candidate_validation() returns None.
	let descriptor = CandidateDescriptorV2::new_v1(
		ParaId::from(1_u32),
		dummy_hash(),
		dummy_hash(),
		pov.hash(),
		dummy_hash(),
		dummy_hash(),
		validation_code.hash(),
	);

	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());

	let test_fut = async move {
		// SessionExecutorParams — must use caller_session (42) as fallback.
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::SessionExecutorParams(session, tx),
			)) => {
				assert_eq!(session, caller_session, "V1 must fall back to caller session for executor_params");
				tx.send(Ok(Some(ExecutorParams::default()))).unwrap();
			}
		);

		// ValidationCodeBombLimit — must also use caller_session (42).
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_,
				RuntimeApiRequest::ValidationCodeBombLimit(session, tx),
			)) => {
				assert_eq!(session, caller_session, "V1 must fall back to caller session for bomb limit");
				tx.send(Ok(VALIDATION_CODE_BOMB_LIMIT)).unwrap();
			}
		);
	};

	let fetch_fut = async move {
		let mut cache = SessionCache::new();

		let result = cache
			.fetch_params(recent_leaf, caller_session, &descriptor, false, ctx.sender())
			.await;

		assert!(result.is_ok(), "fetch_params should succeed");
	};

	executor::block_on(future::join(test_fut, fetch_fut));
}

/// Scheduling session check: backing rejects when the descriptor's session
/// doesn't match the runtime; approval/dispute skips the check entirely.
///
/// Uses a V2 descriptor with a deliberately wrong session_index=100 while the
/// runtime reports session=1. Loops through all exec kinds to verify backing
/// rejects and approval/dispute accepts.
#[test]
fn pre_validation_scheduling_session_check() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };
	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let head_data = HeadData(vec![1, 1, 1]);
	let validation_code = ValidationCode(vec![2; 16]);
	let scheduling_parent = dummy_hash();

	// V2 descriptor with wrong session_index=100 (runtime will report 1).
	let descriptor = make_valid_candidate_descriptor_v2(
		ParaId::from(1_u32),
		scheduling_parent,
		CoreIndex(1),
		100,
		validation_data.hash(),
		pov.hash(),
		validation_code.hash(),
		head_data.hash(),
		dummy_hash(),
	);

	let validation_result = WasmValidationResult {
		head_data: head_data.clone(),
		new_validation_code: None,
		upward_messages: Default::default(),
		horizontal_messages: Default::default(),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};
	let commitments = CandidateCommitments {
		head_data: validation_result.head_data.clone(),
		upward_messages: validation_result.upward_messages.clone(),
		horizontal_messages: validation_result.horizontal_messages.clone(),
		new_validation_code: validation_result.new_validation_code.clone(),
		processed_downward_messages: validation_result.processed_downward_messages,
		hrmp_watermark: validation_result.hrmp_watermark,
	};
	let candidate_receipt = CandidateReceipt { descriptor, commitments_hash: commitments.hash() };

	let all_exec_kinds = [
		PvfExecKind::Backing(dummy_hash()),
		PvfExecKind::BackingSystemParas(dummy_hash()),
		PvfExecKind::Approval,
		PvfExecKind::Dispute,
	];

	for exec_kind in all_exec_kinds {
		let is_backing =
			matches!(exec_kind, PvfExecKind::Backing(_) | PvfExecKind::BackingSystemParas(_));

		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());
		let mock_backend =
			MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone()));

		let (response_tx, response_rx) = oneshot::channel();

		let task = handle_validation_message(
			ctx.sender().clone(),
			mock_backend,
			Metrics::default(),
			false,
			CandidateValidationMessage::ValidateFromExhaustive {
				validation_data: validation_data.clone(),
				validation_code: validation_code.clone(),
				candidate_receipt: candidate_receipt.clone(),
				pov: Arc::new(pov.clone()),
				scheduling_session_index: 100,
				exec_kind,
				response_sender: response_tx,
			},
			Some(default_session_params()),
		);

		let test_fut = async move {
			if is_backing {
				// Backing: SessionIndexForChild returns 1 (mismatch with 100).
				assert_matches!(
					ctx_handle.recv().await,
					AllMessages::RuntimeApi(RuntimeApiMessage::Request(
						_parent,
						RuntimeApiRequest::SessionIndexForChild(tx),
					)) => {
						let _ = tx.send(Ok(1));
					}
				);
				// Rejects before any further calls.
			}
			// Approval/dispute: no session check, PVF runs directly.
		};

		executor::block_on(future::join(test_fut, task));

		let result = executor::block_on(response_rx).unwrap().unwrap();
		if is_backing {
			assert_matches!(
				result,
				ValidationResult::Invalid(InvalidCandidate::InvalidSchedulingSession)
			);
		} else {
			assert_matches!(result, ValidationResult::Valid(_, _));
		}
	}
}

/// V3 scheduling session offset mismatch: backing rejects when the computed scheduling session
/// (session_index + offset) doesn't match the runtime. Uses `v3_ever_seen=true` — backing only
/// sends V3 candidates after V3 is confirmed enabled.
#[test]
fn pre_validation_v3_scheduling_offset_mismatch() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };
	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let validation_code = ValidationCode(vec![2; 16]);
	let scheduling_parent = Hash::repeat_byte(0xAA);

	// V3 descriptor: session_index=1, offset=1 → scheduling_session=2
	let mut descriptor = make_valid_candidate_descriptor_v3(
		ParaId::from(1_u32),
		dummy_hash(), // relay_parent
		CoreIndex(0),
		1, // session_index
		1, // scheduling_session_index
		validation_data.hash(),
		pov.hash(),
		validation_code.hash(),
		dummy_hash(),
		dummy_hash(), // erasure_root
		scheduling_parent,
	);
	descriptor.set_scheduling_session_offset(1);

	let candidate_receipt = CandidateReceipt { descriptor, commitments_hash: Hash::zero() };

	let pool = TaskExecutor::new();
	let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());
	let mock_backend =
		MockValidateCandidateBackend::with_hardcoded_result(Ok(WasmValidationResult {
			head_data: HeadData(vec![1]),
			new_validation_code: None,
			upward_messages: Default::default(),
			horizontal_messages: Default::default(),
			processed_downward_messages: 0,
			hrmp_watermark: 0,
		}));

	let (response_tx, response_rx) = oneshot::channel();

	let task = handle_validation_message(
		ctx.sender().clone(),
		mock_backend,
		Metrics::default(),
		true, // v3_ever_seen=true → V3 descriptor fields are trusted
		CandidateValidationMessage::ValidateFromExhaustive {
			validation_data: validation_data.clone(),
			validation_code: validation_code.clone(),
			candidate_receipt: candidate_receipt.clone(),
			pov: Arc::new(pov.clone()),
			scheduling_session_index: 2,
			exec_kind: PvfExecKind::Backing(dummy_hash()),
			response_sender: response_tx,
		},
		Some(default_session_params()),
	);

	let test_fut = async move {
		// Backing: get_session_index at scheduling_parent returns 1,
		// but descriptor claims scheduling_session=2.
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				parent,
				RuntimeApiRequest::SessionIndexForChild(tx),
			)) => {
				assert_eq!(parent, scheduling_parent);
				let _ = tx.send(Ok(1));
			}
		);
	};

	executor::block_on(future::join(test_fut, task));

	assert_matches!(
		executor::block_on(response_rx).unwrap(),
		Ok(ValidationResult::Invalid(InvalidCandidate::InvalidSchedulingSession))
	);
}

/// Basic checks (PoV hash, code hash, PoV size) are caught during pre-validation
/// before PVF execution, regardless of exec kind.
#[test]
fn pre_validation_basic_checks() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };
	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let validation_code = ValidationCode(vec![2; 16]);

	// Each case: (descriptor, pov_override, expected_error)
	let cases: Vec<(_, Option<PoV>, _)> = vec![
		// Wrong PoV hash
		(
			make_valid_candidate_descriptor_v2(
				ParaId::from(1_u32),
				dummy_hash(),
				CoreIndex(1),
				1,
				validation_data.hash(),
				Hash::repeat_byte(0xFF), // wrong
				validation_code.hash(),
				dummy_hash(),
				dummy_hash(),
			),
			None,
			InvalidCandidate::PoVHashMismatch,
		),
		// Wrong code hash
		(
			make_valid_candidate_descriptor_v2(
				ParaId::from(1_u32),
				dummy_hash(),
				CoreIndex(1),
				1,
				validation_data.hash(),
				pov.hash(),
				ValidationCode(vec![0xFF; 16]).hash(), // wrong
				dummy_hash(),
				dummy_hash(),
			),
			None,
			InvalidCandidate::CodeHashMismatch,
		),
		// PoV too large (max_pov_size=1024 but PoV is 2048 bytes)
		(
			make_valid_candidate_descriptor_v2(
				ParaId::from(1_u32),
				dummy_hash(),
				CoreIndex(1),
				1,
				validation_data.hash(),
				PoV { block_data: BlockData(vec![0; 2048]) }.hash(),
				validation_code.hash(),
				dummy_hash(),
				dummy_hash(),
			),
			Some(PoV { block_data: BlockData(vec![0; 2048]) }),
			InvalidCandidate::ParamsTooLarge(2048),
		),
		// PVD does not match the descriptor's `persisted_validation_data_hash`
		(
			make_valid_candidate_descriptor_v2(
				ParaId::from(1_u32),
				dummy_hash(),
				CoreIndex(1),
				1,
				Hash::repeat_byte(0xFF), // wrong
				pov.hash(),
				validation_code.hash(),
				dummy_hash(),
				dummy_hash(),
			),
			None,
			InvalidCandidate::PersistedValidationDataMismatch,
		),
	];

	let all_exec_kinds = [
		PvfExecKind::Backing(dummy_hash()),
		PvfExecKind::BackingSystemParas(dummy_hash()),
		PvfExecKind::Approval,
		PvfExecKind::Dispute,
	];

	for (descriptor, pov_override, expected_error) in &cases {
		let test_pov = pov_override.as_ref().unwrap_or(&pov);

		for exec_kind in &all_exec_kinds {
			let candidate_receipt =
				CandidateReceipt { descriptor: descriptor.clone(), commitments_hash: Hash::zero() };

			let pool = TaskExecutor::new();
			let (mut ctx, _ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());
			let mock_backend = MockValidateCandidateBackend::with_hardcoded_result(Err(
				ValidationError::Internal(InternalValidationError::HostCommunication(
					"unused".into(),
				)),
			));

			let (response_tx, response_rx) = oneshot::channel();

			let task = handle_validation_message(
				ctx.sender().clone(),
				mock_backend,
				Metrics::default(),
				false,
				CandidateValidationMessage::ValidateFromExhaustive {
					validation_data: validation_data.clone(),
					validation_code: validation_code.clone(),
					candidate_receipt,
					pov: Arc::new(test_pov.clone()),
					scheduling_session_index: 1,
					exec_kind: *exec_kind,
					response_sender: response_tx,
				},
				Some(default_session_params()),
			);

			// Basic checks fail before any runtime calls — no mock interaction needed.
			executor::block_on(task);

			assert_matches!(
				executor::block_on(response_rx).unwrap(),
				Ok(ValidationResult::Invalid(ref e)) => {
					assert_eq!(
						std::mem::discriminant(e),
						std::mem::discriminant(expected_error),
						"Expected {expected_error:?} for exec_kind {exec_kind:?}, got {e:?}"
					);
				}
			);
		}
	}
}

#[test]
fn pvd_not_matching_descriptor_is_invalid() {
	let real_validation_data = PersistedValidationData {
		parent_head: HeadData(vec![1; 4]),
		max_pov_size: 1024,
		..Default::default()
	};
	let forged_validation_data = PersistedValidationData {
		parent_head: HeadData(vec![0xFF; 4]),
		..real_validation_data.clone()
	};

	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let head_data = HeadData(vec![1, 1, 1]);
	let validation_code = ValidationCode(vec![2; 16]);

	let validation_result = WasmValidationResult {
		head_data: head_data.clone(),
		new_validation_code: None,
		upward_messages: Default::default(),
		horizontal_messages: Default::default(),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};
	let commitments = CandidateCommitments {
		head_data: validation_result.head_data.clone(),
		upward_messages: validation_result.upward_messages.clone(),
		horizontal_messages: validation_result.horizontal_messages.clone(),
		new_validation_code: validation_result.new_validation_code.clone(),
		processed_downward_messages: validation_result.processed_downward_messages,
		hrmp_watermark: validation_result.hrmp_watermark,
	};

	let validate = |validation_data: &PersistedValidationData, exec_kind| {
		let descriptor = make_valid_candidate_descriptor_v2(
			ParaId::from(1_u32),
			dummy_hash(),
			CoreIndex(1),
			1,
			real_validation_data.hash(),
			pov.hash(),
			validation_code.hash(),
			head_data.hash(),
			dummy_hash(),
		);
		let candidate_receipt =
			CandidateReceipt { descriptor, commitments_hash: commitments.hash() };

		let pool = TaskExecutor::new();
		let (mut ctx, _ctx_handle) = make_subsystem_context::<AllMessages, _>(pool);
		let (response_tx, response_rx) = oneshot::channel();

		executor::block_on(handle_validation_message(
			ctx.sender().clone(),
			MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone())),
			Metrics::default(),
			false,
			CandidateValidationMessage::ValidateFromExhaustive {
				validation_data: validation_data.clone(),
				validation_code: validation_code.clone(),
				candidate_receipt,
				pov: Arc::new(pov.clone()),
				scheduling_session_index: 1,
				exec_kind,
				response_sender: response_tx,
			},
			Some(default_session_params()),
		));

		executor::block_on(response_rx).unwrap()
	};

	for exec_kind in [PvfExecKind::Approval, PvfExecKind::Dispute] {
		assert_matches!(
			validate(&real_validation_data, exec_kind),
			Ok(ValidationResult::Valid(_, _))
		);
		assert_matches!(
			validate(&forged_validation_data, exec_kind),
			Ok(ValidationResult::Invalid(InvalidCandidate::PersistedValidationDataMismatch))
		);
	}
}

/// Relay parent session check: for V2 candidates (scheduling_parent == relay_parent),
/// the `check_relay_parent_session` utility takes the self-query path, verifying the
/// session via `session_index_for_child` directly.
///
/// Case 1: Session mismatch → InvalidRelayParentSession.
/// Case 2: Session matches → valid, proceeds to PVF execution.
#[test]
fn pre_validation_relay_parent_session_check() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };
	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let head_data = HeadData(vec![1, 1, 1]);
	let validation_code = ValidationCode(vec![2; 16]);
	let scheduling_parent = dummy_hash();

	// V2 descriptor with session_index=1.
	let descriptor = make_valid_candidate_descriptor_v2(
		ParaId::from(1_u32),
		scheduling_parent,
		CoreIndex(1),
		1,
		validation_data.hash(),
		pov.hash(),
		validation_code.hash(),
		head_data.hash(),
		dummy_hash(),
	);

	let validation_result = WasmValidationResult {
		head_data: head_data.clone(),
		new_validation_code: None,
		upward_messages: Default::default(),
		horizontal_messages: Default::default(),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};
	let commitments = CandidateCommitments {
		head_data: validation_result.head_data.clone(),
		upward_messages: validation_result.upward_messages.clone(),
		horizontal_messages: validation_result.horizontal_messages.clone(),
		new_validation_code: validation_result.new_validation_code.clone(),
		processed_downward_messages: validation_result.processed_downward_messages,
		hrmp_watermark: validation_result.hrmp_watermark,
	};
	let candidate_receipt = CandidateReceipt { descriptor, commitments_hash: commitments.hash() };

	// Case 1: Self-query session mismatch → InvalidRelayParentSession.
	// The utility calls session_index_for_child which returns 99 (doesn't match
	// descriptor's session_index=1).
	{
		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());
		let mock_backend =
			MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone()));

		let (response_tx, response_rx) = oneshot::channel();

		let task = handle_validation_message(
			ctx.sender().clone(),
			mock_backend,
			Metrics::default(),
			false,
			CandidateValidationMessage::ValidateFromExhaustive {
				validation_data: validation_data.clone(),
				validation_code: validation_code.clone(),
				candidate_receipt: candidate_receipt.clone(),
				pov: Arc::new(pov.clone()),
				scheduling_session_index: 1,
				exec_kind: PvfExecKind::Backing(dummy_hash()),
				response_sender: response_tx,
			},
			Some(default_session_params()),
		);

		let test_fut = async move {
			// Scheduling session check: matches (session=1).
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_, RuntimeApiRequest::SessionIndexForChild(tx),
				)) => { let _ = tx.send(Ok(1)); }
			);
			// check_relay_parent_session self-query: session_index_for_child returns 99
			// (mismatch with descriptor's session_index=1).
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_, RuntimeApiRequest::SessionIndexForChild(tx),
				)) => { let _ = tx.send(Ok(99)); }
			);
		};

		executor::block_on(future::join(test_fut, task));

		assert_matches!(
			executor::block_on(response_rx).unwrap(),
			Ok(ValidationResult::Invalid(InvalidCandidate::InvalidRelayParentSession))
		);
	}

	// Case 2: Self-query session matches → valid, proceeds to PVF execution.
	{
		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());
		let mock_backend =
			MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone()));

		let (response_tx, response_rx) = oneshot::channel();

		let task = handle_validation_message(
			ctx.sender().clone(),
			mock_backend,
			Metrics::default(),
			false,
			CandidateValidationMessage::ValidateFromExhaustive {
				validation_data: validation_data.clone(),
				validation_code: validation_code.clone(),
				candidate_receipt: candidate_receipt.clone(),
				pov: Arc::new(pov.clone()),
				scheduling_session_index: 1,
				exec_kind: PvfExecKind::Backing(dummy_hash()),
				response_sender: response_tx,
			},
			Some(default_session_params()),
		);

		let test_fut = async move {
			// Scheduling session check: matches.
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_, RuntimeApiRequest::SessionIndexForChild(tx),
				)) => { let _ = tx.send(Ok(1)); }
			);
			// check_relay_parent_session self-query: session matches (session=1).
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_, RuntimeApiRequest::SessionIndexForChild(tx),
				)) => { let _ = tx.send(Ok(1)); }
			);
			// ClaimQueue: proceeds normally.
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_, RuntimeApiRequest::ClaimQueue(tx),
				)) => {
					let mut cq = BTreeMap::new();
					let _ = cq.insert(CoreIndex(1), vec![ParaId::from(1_u32)].into());
					let _ = tx.send(Ok(cq));
				}
			);
		};

		executor::block_on(future::join(test_fut, task));

		assert_matches!(
			executor::block_on(response_rx).unwrap(),
			Ok(ValidationResult::Valid(_, _))
		);
	}
}

/// Relay parent session check for V3 candidates (scheduling_parent != relay_parent):
/// the `check_relay_parent_session` utility takes the ancestor-query path, calling
/// the `AncestorRelayParentInfo` runtime API.
///
/// Case 1: AncestorRelayParentInfo returns None → InvalidRelayParentSession.
/// Case 2: AncestorRelayParentInfo not supported → skipped, proceeds to valid.
/// Case 3: AncestorRelayParentInfo returns Some → valid, proceeds.
#[test]
fn pre_validation_relay_parent_session_check_v3_ancestor_query() {
	let validation_data = PersistedValidationData { max_pov_size: 1024, ..Default::default() };
	let pov = PoV { block_data: BlockData(vec![1; 32]) };
	let head_data = HeadData(vec![1, 1, 1]);
	let validation_code = ValidationCode(vec![2; 16]);
	let relay_parent = dummy_hash();
	let scheduling_parent = Hash::repeat_byte(0x42);

	// V3 descriptor: scheduling_parent != relay_parent, session_index=1.
	let descriptor = make_valid_candidate_descriptor_v3(
		ParaId::from(1_u32),
		relay_parent,
		CoreIndex(1),
		1,
		1,
		validation_data.hash(),
		pov.hash(),
		validation_code.hash(),
		head_data.hash(),
		dummy_hash(),
		scheduling_parent,
	);

	let validation_result = WasmValidationResult {
		head_data: head_data.clone(),
		new_validation_code: None,
		upward_messages: Default::default(),
		horizontal_messages: Default::default(),
		processed_downward_messages: 0,
		hrmp_watermark: 0,
	};
	let commitments = CandidateCommitments {
		head_data: validation_result.head_data.clone(),
		upward_messages: validation_result.upward_messages.clone(),
		horizontal_messages: validation_result.horizontal_messages.clone(),
		new_validation_code: validation_result.new_validation_code.clone(),
		processed_downward_messages: validation_result.processed_downward_messages,
		hrmp_watermark: validation_result.hrmp_watermark,
	};
	let candidate_receipt = CandidateReceipt { descriptor, commitments_hash: commitments.hash() };

	// Helper: mock the V3 backing pre-validation flow up to (but not including)
	// the relay parent session check.
	async fn mock_v3_pre_checks(
		ctx_handle: &mut TestSubsystemContextHandle<AllMessages>,
		session: SessionIndex,
	) {
		// Scheduling session check: SessionIndexForChild at scheduling_parent.
		assert_matches!(
			ctx_handle.recv().await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				_, RuntimeApiRequest::SessionIndexForChild(tx),
			)) => { let _ = tx.send(Ok(session)); }
		);
		// AncestorRelayParentInfo check for relay parent in session (v16+ API).
		// This is only reached for V3 with v3_ever_seen=true, where
		// session_index_for_candidate_validation returns Some.
	}

	// Case 1: AncestorRelayParentInfo returns None → InvalidRelayParentSession.
	{
		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());
		let mock_backend =
			MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone()));

		let (response_tx, response_rx) = oneshot::channel();

		let task = handle_validation_message(
			ctx.sender().clone(),
			mock_backend,
			Metrics::default(),
			true, // v3_ever_seen
			CandidateValidationMessage::ValidateFromExhaustive {
				validation_data: validation_data.clone(),
				validation_code: validation_code.clone(),
				candidate_receipt: candidate_receipt.clone(),
				pov: Arc::new(pov.clone()),
				scheduling_session_index: 1,
				exec_kind: PvfExecKind::Backing(scheduling_parent),
				response_sender: response_tx,
			},
			Some(default_session_params()),
		);

		let test_fut = async move {
			mock_v3_pre_checks(&mut ctx_handle, 1).await;
			// AncestorRelayParentInfo: relay parent NOT found.
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					parent,
					RuntimeApiRequest::AncestorRelayParentInfo(session, rp, tx),
				)) => {
					assert_eq!(parent, scheduling_parent);
					assert_eq!(session, 1);
					assert_eq!(rp, relay_parent);
					let _ = tx.send(Ok(None));
				}
			);
		};

		executor::block_on(future::join(test_fut, task));

		assert_matches!(
			executor::block_on(response_rx).unwrap(),
			Ok(ValidationResult::Invalid(InvalidCandidate::InvalidRelayParentSession))
		);
	}

	// Case 2: AncestorRelayParentInfo not supported → skipped, proceeds past session check.
	// (Candidate then fails UMP signal check since V3 requires signals — this proves
	// the session check was skipped successfully.)
	{
		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());
		let mock_backend =
			MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone()));

		let (response_tx, response_rx) = oneshot::channel();

		let task = handle_validation_message(
			ctx.sender().clone(),
			mock_backend,
			Metrics::default(),
			true,
			CandidateValidationMessage::ValidateFromExhaustive {
				validation_data: validation_data.clone(),
				validation_code: validation_code.clone(),
				candidate_receipt: candidate_receipt.clone(),
				pov: Arc::new(pov.clone()),
				scheduling_session_index: 1,
				exec_kind: PvfExecKind::Backing(scheduling_parent),
				response_sender: response_tx,
			},
			Some(default_session_params()),
		);

		let test_fut = async move {
			mock_v3_pre_checks(&mut ctx_handle, 1).await;
			// AncestorRelayParentInfo: not supported → skipped.
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_, RuntimeApiRequest::AncestorRelayParentInfo(_, _, tx),
				)) => {
					let _ = tx.send(Err(RuntimeApiError::NotSupported {
						runtime_api_name: "AncestorRelayParentInfo",
					}));
				}
			);
			// ClaimQueue: proceeds past session check.
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_, RuntimeApiRequest::ClaimQueue(tx),
				)) => {
					let mut cq = BTreeMap::new();
					let _ = cq.insert(CoreIndex(1), vec![ParaId::from(1_u32)].into());
					let _ = tx.send(Ok(cq));
				}
			);
		};

		executor::block_on(future::join(test_fut, task));

		// V3 requires UMP signals which this candidate doesn't have — but the
		// point is we got past the session check.
		assert_matches!(
			executor::block_on(response_rx).unwrap(),
			Ok(ValidationResult::Invalid(InvalidCandidate::InvalidUMPSignals(_)))
		);
	}

	// Case 3: AncestorRelayParentInfo returns Some → proceeds past session check.
	{
		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = make_subsystem_context::<AllMessages, _>(pool.clone());
		let mock_backend =
			MockValidateCandidateBackend::with_hardcoded_result(Ok(validation_result.clone()));

		let (response_tx, response_rx) = oneshot::channel();

		let task = handle_validation_message(
			ctx.sender().clone(),
			mock_backend,
			Metrics::default(),
			true,
			CandidateValidationMessage::ValidateFromExhaustive {
				validation_data: validation_data.clone(),
				validation_code: validation_code.clone(),
				candidate_receipt: candidate_receipt.clone(),
				pov: Arc::new(pov.clone()),
				scheduling_session_index: 1,
				exec_kind: PvfExecKind::Backing(scheduling_parent),
				response_sender: response_tx,
			},
			Some(default_session_params()),
		);

		let test_fut = async move {
			mock_v3_pre_checks(&mut ctx_handle, 1).await;
			// AncestorRelayParentInfo: found.
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_, RuntimeApiRequest::AncestorRelayParentInfo(_, _, tx),
				)) => { let _ = tx.send(Ok(Some(Default::default()))); }
			);
			// ClaimQueue.
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					_, RuntimeApiRequest::ClaimQueue(tx),
				)) => {
					let mut cq = BTreeMap::new();
					let _ = cq.insert(CoreIndex(1), vec![ParaId::from(1_u32)].into());
					let _ = tx.send(Ok(cq));
				}
			);
		};

		executor::block_on(future::join(test_fut, task));

		// Same as case 2 — V3 UMP signals missing, but we got past session check.
		assert_matches!(
			executor::block_on(response_rx).unwrap(),
			Ok(ValidationResult::Invalid(InvalidCandidate::InvalidUMPSignals(_)))
		);
	}
}
