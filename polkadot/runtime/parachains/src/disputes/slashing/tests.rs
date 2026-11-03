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
use crate::{
	configuration::HostConfiguration,
	disputes::SlashingHandler,
	mock::{
		mock_reported_offences, new_test_ext, register_mock_key_owner_proof,
		set_mock_current_session, set_mock_known_offence, set_mock_report_offence_result,
		AccountId, MockGenesisConfig, MockKeyOwnerProof, MockReportResult, RuntimeOrigin, Slashing,
		Test,
	},
};
use codec::Encode;
use frame_support::{assert_err, assert_noop, assert_ok};
use polkadot_primitives::{
	slashing::{DisputeProof, DisputesTimeSlot, PendingSlashes},
	CandidateHash, DisputeOffenceKind, Hash, SessionIndex, ValidatorId, ValidatorIndex,
};
use sp_core::{sr25519, Pair};
use sp_runtime::transaction_validity::{
	InvalidTransaction, TransactionSource, TransactionValidity,
};

const SESSION_PAST: SessionIndex = 5;
const SESSION_CURRENT: SessionIndex = 10;
const VALIDATOR_SET_COUNT: u32 = 4;

fn candidate_hash(seed: u8) -> CandidateHash {
	CandidateHash(Hash::repeat_byte(seed))
}

fn validator_id(seed: u8) -> ValidatorId {
	let pair = sr25519::Pair::from_seed(&[seed; 32]);
	ValidatorId::from(pair.public())
}

fn time_slot(session: SessionIndex, candidate: CandidateHash) -> DisputesTimeSlot {
	DisputesTimeSlot::new(session, candidate)
}

fn proof(session: SessionIndex, validator_count: u32, tag: u32) -> MockKeyOwnerProof {
	MockKeyOwnerProof { session, validator_count, tag }
}

fn dispute_proof(
	session: SessionIndex,
	candidate: CandidateHash,
	index: ValidatorIndex,
	id: ValidatorId,
	kind: DisputeOffenceKind,
) -> DisputeProof {
	DisputeProof {
		time_slot: time_slot(session, candidate),
		kind,
		validator_index: index,
		validator_id: id,
	}
}

fn insert_pending_slash(
	session: SessionIndex,
	candidate: CandidateHash,
	kind: DisputeOffenceKind,
	entries: Vec<(ValidatorIndex, ValidatorId)>,
) {
	let mut keys = alloc::collections::btree_map::BTreeMap::new();
	for (idx, id) in entries {
		keys.insert(idx, id);
	}
	UnappliedSlashes::<Test>::insert(session, candidate, PendingSlashes { keys, kind });
}

fn insert_session_info(session: SessionIndex, validators: Vec<ValidatorId>) {
	use polkadot_primitives::{AuthorityDiscoveryId, IndexedVec, SessionInfo};
	let n = validators.len();
	let discovery_keys: Vec<AuthorityDiscoveryId> = (0..n)
		.map(|i| {
			let pair = sr25519::Pair::from_seed(&[0xAA + i as u8; 32]);
			AuthorityDiscoveryId::from(pair.public())
		})
		.collect();
	let session_info = SessionInfo {
		active_validator_indices: vec![],
		random_seed: [0u8; 32],
		dispute_period: 6,
		validators: IndexedVec::from(validators),
		discovery_keys,
		assignment_keys: vec![],
		validator_groups: IndexedVec::from(vec![]),
		n_cores: 0,
		zeroth_delay_tranche_width: 0,
		relay_vrf_modulo_samples: 0,
		n_delay_tranches: 0,
		no_show_slots: 0,
		needed_approvals: 0,
	};
	crate::session_info::Sessions::<Test>::insert(session, session_info);
}

fn ext() -> sp_io::TestExternalities {
	let mut ext = new_test_ext(MockGenesisConfig::default());
	ext.execute_with(|| {
		System_initialize();
		set_mock_current_session(SESSION_CURRENT);
		set_mock_report_offence_result(MockReportResult::Ok);
	});
	ext
}

#[allow(non_snake_case)]
fn System_initialize() {
	frame_system::Pallet::<Test>::set_block_number(1);
}

// ---- report_dispute_lost_unsigned: happy paths ----

#[test]
fn report_dispute_lost_for_invalid_backed_succeeds() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		let index = ValidatorIndex(0);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);
		let account: AccountId = 100;

		insert_pending_slash(
			SESSION_PAST,
			candidate,
			DisputeOffenceKind::ForInvalidBacked,
			vec![(index, id.clone())],
		);
		register_mock_key_owner_proof(id.clone(), key_owner_proof.clone(), account);

		let dispute =
			dispute_proof(SESSION_PAST, candidate, index, id, DisputeOffenceKind::ForInvalidBacked);

		assert_ok!(Slashing::report_dispute_lost_unsigned(
			RuntimeOrigin::none(),
			Box::new(dispute),
			key_owner_proof,
		));

		assert!(UnappliedSlashes::<Test>::get(SESSION_PAST, candidate).is_none());
		let reported = mock_reported_offences();
		assert_eq!(reported.len(), 1);
		assert_eq!(reported[0].0, time_slot(SESSION_PAST, candidate));
		assert_eq!(reported[0].1, DisputeOffenceKind::ForInvalidBacked);
		assert_eq!(reported[0].2, vec![(account, ())]);
	});
}

#[test]
fn report_dispute_lost_for_invalid_approved_succeeds() {
	ext().execute_with(|| {
		let candidate = candidate_hash(2);
		let id = validator_id(2);
		let index = ValidatorIndex(1);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 2);
		let account: AccountId = 101;

		insert_pending_slash(
			SESSION_PAST,
			candidate,
			DisputeOffenceKind::ForInvalidApproved,
			vec![(index, id.clone())],
		);
		register_mock_key_owner_proof(id.clone(), key_owner_proof.clone(), account);

		let dispute = dispute_proof(
			SESSION_PAST,
			candidate,
			index,
			id,
			DisputeOffenceKind::ForInvalidApproved,
		);

		assert_ok!(Slashing::report_dispute_lost_unsigned(
			RuntimeOrigin::none(),
			Box::new(dispute),
			key_owner_proof,
		));

		let reported = mock_reported_offences();
		assert_eq!(reported.len(), 1);
		assert_eq!(reported[0].1, DisputeOffenceKind::ForInvalidApproved);
	});
}

#[test]
fn report_dispute_lost_against_valid_succeeds() {
	ext().execute_with(|| {
		let candidate = candidate_hash(3);
		let id = validator_id(3);
		let index = ValidatorIndex(2);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 3);
		let account: AccountId = 102;

		insert_pending_slash(
			SESSION_PAST,
			candidate,
			DisputeOffenceKind::AgainstValid,
			vec![(index, id.clone())],
		);
		register_mock_key_owner_proof(id.clone(), key_owner_proof.clone(), account);

		let dispute =
			dispute_proof(SESSION_PAST, candidate, index, id, DisputeOffenceKind::AgainstValid);

		assert_ok!(Slashing::report_dispute_lost_unsigned(
			RuntimeOrigin::none(),
			Box::new(dispute),
			key_owner_proof,
		));

		let reported = mock_reported_offences();
		assert_eq!(reported.len(), 1);
		assert_eq!(reported[0].1, DisputeOffenceKind::AgainstValid);
	});
}

#[test]
fn report_dispute_lost_only_removes_reported_validator_keeps_others() {
	ext().execute_with(|| {
		let candidate = candidate_hash(4);
		let id_a = validator_id(10);
		let id_b = validator_id(11);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);
		let account: AccountId = 200;

		insert_pending_slash(
			SESSION_PAST,
			candidate,
			DisputeOffenceKind::ForInvalidBacked,
			vec![(ValidatorIndex(0), id_a.clone()), (ValidatorIndex(1), id_b.clone())],
		);
		register_mock_key_owner_proof(id_a.clone(), key_owner_proof.clone(), account);

		let dispute = dispute_proof(
			SESSION_PAST,
			candidate,
			ValidatorIndex(0),
			id_a,
			DisputeOffenceKind::ForInvalidBacked,
		);

		assert_ok!(Slashing::report_dispute_lost_unsigned(
			RuntimeOrigin::none(),
			Box::new(dispute),
			key_owner_proof,
		));

		let remaining = UnappliedSlashes::<Test>::get(SESSION_PAST, candidate).unwrap();
		assert_eq!(remaining.keys.len(), 1);
		assert!(remaining.keys.contains_key(&ValidatorIndex(1)));
		assert_eq!(remaining.keys.get(&ValidatorIndex(1)), Some(&id_b));
	});
}

// ---- report_dispute_lost_unsigned: error paths ----

#[test]
fn report_dispute_lost_rejects_signed_origin() {
	ext().execute_with(|| {
		let id = validator_id(1);
		let dispute = dispute_proof(
			SESSION_PAST,
			candidate_hash(1),
			ValidatorIndex(0),
			id,
			DisputeOffenceKind::ForInvalidBacked,
		);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);

		assert_noop!(
			Slashing::report_dispute_lost_unsigned(
				RuntimeOrigin::signed(1),
				Box::new(dispute),
				key_owner_proof,
			),
			sp_runtime::DispatchError::BadOrigin,
		);
	});
}

#[test]
fn report_dispute_lost_rejects_root_origin() {
	ext().execute_with(|| {
		let id = validator_id(1);
		let dispute = dispute_proof(
			SESSION_PAST,
			candidate_hash(1),
			ValidatorIndex(0),
			id,
			DisputeOffenceKind::ForInvalidBacked,
		);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);

		assert_noop!(
			Slashing::report_dispute_lost_unsigned(
				RuntimeOrigin::root(),
				Box::new(dispute),
				key_owner_proof,
			),
			sp_runtime::DispatchError::BadOrigin,
		);
	});
}

#[test]
fn report_dispute_lost_invalid_key_ownership_proof_is_rejected() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		let index = ValidatorIndex(0);
		let unregistered_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 99);

		insert_pending_slash(
			SESSION_PAST,
			candidate,
			DisputeOffenceKind::ForInvalidBacked,
			vec![(index, id.clone())],
		);

		let dispute =
			dispute_proof(SESSION_PAST, candidate, index, id, DisputeOffenceKind::ForInvalidBacked);

		assert_noop!(
			Slashing::report_dispute_lost_unsigned(
				RuntimeOrigin::none(),
				Box::new(dispute),
				unregistered_proof,
			),
			Error::<Test>::InvalidKeyOwnershipProof,
		);
		assert!(UnappliedSlashes::<Test>::get(SESSION_PAST, candidate).is_some());
		assert!(mock_reported_offences().is_empty());
	});
}

#[test]
fn report_dispute_lost_invalid_candidate_hash_is_rejected() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let other_candidate = candidate_hash(99);
		let id = validator_id(1);
		let index = ValidatorIndex(0);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);
		register_mock_key_owner_proof(id.clone(), key_owner_proof.clone(), 100);

		insert_pending_slash(
			SESSION_PAST,
			candidate,
			DisputeOffenceKind::ForInvalidBacked,
			vec![(index, id.clone())],
		);

		let dispute = dispute_proof(
			SESSION_PAST,
			other_candidate,
			index,
			id,
			DisputeOffenceKind::ForInvalidBacked,
		);

		assert_noop!(
			Slashing::report_dispute_lost_unsigned(
				RuntimeOrigin::none(),
				Box::new(dispute),
				key_owner_proof,
			),
			Error::<Test>::InvalidCandidateHash,
		);
	});
}

#[test]
fn report_dispute_lost_kind_mismatch_is_rejected_as_invalid_candidate_hash() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		let index = ValidatorIndex(0);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);
		register_mock_key_owner_proof(id.clone(), key_owner_proof.clone(), 100);

		insert_pending_slash(
			SESSION_PAST,
			candidate,
			DisputeOffenceKind::ForInvalidBacked,
			vec![(index, id.clone())],
		);

		let dispute =
			dispute_proof(SESSION_PAST, candidate, index, id, DisputeOffenceKind::AgainstValid);

		assert_noop!(
			Slashing::report_dispute_lost_unsigned(
				RuntimeOrigin::none(),
				Box::new(dispute),
				key_owner_proof,
			),
			Error::<Test>::InvalidCandidateHash,
		);
	});
}

#[test]
fn report_dispute_lost_invalid_validator_index_is_rejected() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);
		register_mock_key_owner_proof(id.clone(), key_owner_proof.clone(), 100);

		insert_pending_slash(
			SESSION_PAST,
			candidate,
			DisputeOffenceKind::ForInvalidBacked,
			vec![(ValidatorIndex(0), id.clone())],
		);

		let dispute = dispute_proof(
			SESSION_PAST,
			candidate,
			ValidatorIndex(7),
			id,
			DisputeOffenceKind::ForInvalidBacked,
		);

		assert_noop!(
			Slashing::report_dispute_lost_unsigned(
				RuntimeOrigin::none(),
				Box::new(dispute),
				key_owner_proof,
			),
			Error::<Test>::InvalidValidatorIndex,
		);
	});
}

#[test]
fn report_dispute_lost_validator_index_id_mismatch_is_rejected() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id_stored = validator_id(1);
		let id_reported = validator_id(2);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);
		register_mock_key_owner_proof(id_reported.clone(), key_owner_proof.clone(), 100);

		insert_pending_slash(
			SESSION_PAST,
			candidate,
			DisputeOffenceKind::ForInvalidBacked,
			vec![(ValidatorIndex(0), id_stored)],
		);

		let dispute = dispute_proof(
			SESSION_PAST,
			candidate,
			ValidatorIndex(0),
			id_reported,
			DisputeOffenceKind::ForInvalidBacked,
		);

		assert_noop!(
			Slashing::report_dispute_lost_unsigned(
				RuntimeOrigin::none(),
				Box::new(dispute),
				key_owner_proof,
			),
			Error::<Test>::ValidatorIndexIdMismatch,
		);
	});
}

#[test]
fn report_dispute_lost_duplicate_report_is_rejected() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		let index = ValidatorIndex(0);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);
		register_mock_key_owner_proof(id.clone(), key_owner_proof.clone(), 100);
		set_mock_report_offence_result(MockReportResult::DuplicateReport);

		insert_pending_slash(
			SESSION_PAST,
			candidate,
			DisputeOffenceKind::ForInvalidBacked,
			vec![(index, id.clone())],
		);

		let dispute =
			dispute_proof(SESSION_PAST, candidate, index, id, DisputeOffenceKind::ForInvalidBacked);

		assert_err!(
			Slashing::report_dispute_lost_unsigned(
				RuntimeOrigin::none(),
				Box::new(dispute),
				key_owner_proof,
			),
			Error::<Test>::DuplicateSlashingReport,
		);
	});
}

#[test]
fn report_dispute_lost_rejects_proof_from_different_session() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		let index = ValidatorIndex(0);
		let dispute_session = SESSION_PAST;
		let proof_session = SESSION_PAST + 1;
		let mismatched_proof = proof(proof_session, VALIDATOR_SET_COUNT, 1);
		let wrong_account: AccountId = 999;

		insert_pending_slash(
			dispute_session,
			candidate,
			DisputeOffenceKind::ForInvalidBacked,
			vec![(index, id.clone())],
		);
		register_mock_key_owner_proof(id.clone(), mismatched_proof.clone(), wrong_account);

		let dispute = dispute_proof(
			dispute_session,
			candidate,
			index,
			id,
			DisputeOffenceKind::ForInvalidBacked,
		);

		assert_noop!(
			Slashing::report_dispute_lost_unsigned(
				RuntimeOrigin::none(),
				Box::new(dispute),
				mismatched_proof,
			),
			Error::<Test>::InvalidKeyOwnershipProof,
		);
		assert!(UnappliedSlashes::<Test>::get(dispute_session, candidate).is_some());
		assert!(mock_reported_offences().is_empty());
	});
}

#[test]
fn validate_unsigned_rejects_proof_from_different_session() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		let dispute_session = SESSION_PAST;
		let proof_session = SESSION_PAST + 1;
		let mismatched_proof = proof(proof_session, VALIDATOR_SET_COUNT, 1);
		register_mock_key_owner_proof(id.clone(), mismatched_proof.clone(), 100);

		let dispute = dispute_proof(
			dispute_session,
			candidate,
			ValidatorIndex(0),
			id,
			DisputeOffenceKind::ForInvalidBacked,
		);
		let call = Call::<Test>::report_dispute_lost_unsigned {
			dispute_proof: Box::new(dispute),
			key_owner_proof: mismatched_proof,
		};

		let outcome = Pallet::<Test>::validate_unsigned(TransactionSource::Local, &call);
		assert_eq!(outcome, InvalidTransaction::BadProof.into());
	});
}

// ---- ValidateUnsigned / pre_dispatch ----

#[test]
fn validate_unsigned_rejects_external_source() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);
		register_mock_key_owner_proof(id.clone(), key_owner_proof.clone(), 100);

		let dispute = dispute_proof(
			SESSION_PAST,
			candidate,
			ValidatorIndex(0),
			id,
			DisputeOffenceKind::ForInvalidBacked,
		);
		let call = Call::<Test>::report_dispute_lost_unsigned {
			dispute_proof: Box::new(dispute),
			key_owner_proof,
		};

		let outcome: TransactionValidity =
			Pallet::<Test>::validate_unsigned(TransactionSource::External, &call);
		assert_eq!(outcome, InvalidTransaction::Call.into());
	});
}

#[test]
fn validate_unsigned_accepts_local_source() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);
		register_mock_key_owner_proof(id.clone(), key_owner_proof.clone(), 100);

		let dispute = dispute_proof(
			SESSION_PAST,
			candidate,
			ValidatorIndex(0),
			id,
			DisputeOffenceKind::ForInvalidBacked,
		);
		let call = Call::<Test>::report_dispute_lost_unsigned {
			dispute_proof: Box::new(dispute),
			key_owner_proof,
		};

		let outcome = Pallet::<Test>::validate_unsigned(TransactionSource::Local, &call);
		assert!(outcome.is_ok(), "Local source should be accepted, got {:?}", outcome);
	});
}

#[test]
fn validate_unsigned_accepts_in_block_source() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);
		register_mock_key_owner_proof(id.clone(), key_owner_proof.clone(), 100);

		let dispute = dispute_proof(
			SESSION_PAST,
			candidate,
			ValidatorIndex(0),
			id,
			DisputeOffenceKind::ForInvalidBacked,
		);
		let call = Call::<Test>::report_dispute_lost_unsigned {
			dispute_proof: Box::new(dispute),
			key_owner_proof,
		};

		assert!(Pallet::<Test>::validate_unsigned(TransactionSource::InBlock, &call).is_ok());
	});
}

#[test]
fn validate_unsigned_rejects_bad_proof() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		let unregistered_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 77);

		let dispute = dispute_proof(
			SESSION_PAST,
			candidate,
			ValidatorIndex(0),
			id,
			DisputeOffenceKind::ForInvalidBacked,
		);
		let call = Call::<Test>::report_dispute_lost_unsigned {
			dispute_proof: Box::new(dispute),
			key_owner_proof: unregistered_proof,
		};

		let outcome = Pallet::<Test>::validate_unsigned(TransactionSource::Local, &call);
		assert_eq!(outcome, InvalidTransaction::BadProof.into());
	});
}

#[test]
fn validate_unsigned_rejects_known_offence_as_stale() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);
		let account: AccountId = 100;
		register_mock_key_owner_proof(id.clone(), key_owner_proof.clone(), account);
		set_mock_known_offence(time_slot(SESSION_PAST, candidate), account);

		let dispute = dispute_proof(
			SESSION_PAST,
			candidate,
			ValidatorIndex(0),
			id,
			DisputeOffenceKind::ForInvalidBacked,
		);
		let call = Call::<Test>::report_dispute_lost_unsigned {
			dispute_proof: Box::new(dispute),
			key_owner_proof,
		};

		let outcome = Pallet::<Test>::validate_unsigned(TransactionSource::Local, &call);
		assert_eq!(outcome, InvalidTransaction::Stale.into());
	});
}

#[test]
fn validate_unsigned_uses_kind_specific_tag_prefix() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		let key_owner_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);
		register_mock_key_owner_proof(id.clone(), key_owner_proof.clone(), 100);

		for (kind, expected) in [
			(DisputeOffenceKind::ForInvalidBacked, "DisputeForInvalidBacked"),
			(DisputeOffenceKind::ForInvalidApproved, "DisputeForInvalidApproved"),
			(DisputeOffenceKind::AgainstValid, "DisputeAgainstValid"),
		] {
			let dispute =
				dispute_proof(SESSION_PAST, candidate, ValidatorIndex(0), id.clone(), kind);
			let call = Call::<Test>::report_dispute_lost_unsigned {
				dispute_proof: Box::new(dispute),
				key_owner_proof: key_owner_proof.clone(),
			};
			let valid = Pallet::<Test>::validate_unsigned(TransactionSource::Local, &call)
				.expect("call should validate");
			let provides_tag = (time_slot(SESSION_PAST, candidate), id.clone()).encode();
			let mut full_tag = expected.encode();
			full_tag.extend(provides_tag);
			assert!(
				valid.provides.contains(&full_tag),
				"missing tag prefix {expected} in provides {:?}",
				valid.provides,
			);
		}
	});
}

#[test]
fn pre_dispatch_replays_validation() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		let unregistered_proof = proof(SESSION_PAST, VALIDATOR_SET_COUNT, 1);

		let dispute = dispute_proof(
			SESSION_PAST,
			candidate,
			ValidatorIndex(0),
			id,
			DisputeOffenceKind::ForInvalidBacked,
		);
		let call = Call::<Test>::report_dispute_lost_unsigned {
			dispute_proof: Box::new(dispute),
			key_owner_proof: unregistered_proof,
		};

		assert_eq!(Pallet::<Test>::pre_dispatch(&call), Err(InvalidTransaction::BadProof.into()));
	});
}

// ---- SlashValidatorsForDisputes handler: punish_for_invalid / punish_against_valid ----

type Handler = SlashValidatorsForDisputes<Pallet<Test>>;

#[test]
fn handler_punish_for_invalid_past_session_only_backers_records_for_invalid_backed() {
	ext().execute_with(|| {
		let candidate = candidate_hash(1);
		set_mock_current_session(SESSION_CURRENT);
		let session = SESSION_CURRENT - 2;
		let id = validator_id(20);
		insert_session_info(session, vec![id.clone()]);

		let backer = ValidatorIndex(0);

		<Handler as SlashingHandler<u32>>::punish_for_invalid(
			session,
			candidate,
			vec![backer].into_iter(),
			vec![backer].into_iter(),
		);

		let stored = UnappliedSlashes::<Test>::get(session, candidate)
			.expect("past-session ForInvalid offences must be queued");
		assert_eq!(stored.kind, DisputeOffenceKind::ForInvalidBacked);
		assert_eq!(stored.keys.get(&backer), Some(&id));
		assert!(mock_reported_offences().is_empty());
	});
}

#[test]
fn handler_punish_for_invalid_past_session_only_approvers_records_for_invalid_approved() {
	ext().execute_with(|| {
		let candidate = candidate_hash(2);
		set_mock_current_session(SESSION_CURRENT);
		let session = SESSION_CURRENT - 2;
		let id = validator_id(21);
		insert_session_info(session, vec![id.clone()]);

		let approver = ValidatorIndex(0);

		<Handler as SlashingHandler<u32>>::punish_for_invalid(
			session,
			candidate,
			vec![approver].into_iter(),
			vec![ValidatorIndex(99)].into_iter(),
		);

		let stored = UnappliedSlashes::<Test>::get(session, candidate)
			.expect("past-session ForInvalidApproved offences must be queued");
		assert_eq!(stored.kind, DisputeOffenceKind::ForInvalidApproved);
		assert_eq!(stored.keys.get(&approver), Some(&id));
	});
}

#[test]
fn handler_punish_against_valid_past_session_records_unapplied() {
	ext().execute_with(|| {
		let candidate = candidate_hash(2);
		let session = SESSION_CURRENT - 2;
		let id = validator_id(22);
		let loser = ValidatorIndex(0);
		insert_session_info(session, vec![id.clone()]);

		<Handler as SlashingHandler<u32>>::punish_against_valid(
			session,
			candidate,
			vec![loser].into_iter(),
			vec![].into_iter(),
		);

		let stored = UnappliedSlashes::<Test>::get(session, candidate)
			.expect("past-session AgainstValid offences must be queued");
		assert_eq!(stored.kind, DisputeOffenceKind::AgainstValid);
		assert_eq!(stored.keys.get(&loser), Some(&id));
		assert!(mock_reported_offences().is_empty());
	});
}

#[test]
fn handler_punish_for_invalid_with_no_losers_is_noop() {
	ext().execute_with(|| {
		let candidate = candidate_hash(3);
		let session = SESSION_CURRENT - 2;

		<Handler as SlashingHandler<u32>>::punish_for_invalid(
			session,
			candidate,
			vec![].into_iter(),
			vec![ValidatorIndex(0)].into_iter(),
		);

		assert!(UnappliedSlashes::<Test>::get(session, candidate).is_none());
		assert!(mock_reported_offences().is_empty());
	});
}

#[test]
fn handler_punish_for_invalid_with_no_backers_is_noop() {
	ext().execute_with(|| {
		let candidate = candidate_hash(4);
		let session = SESSION_CURRENT - 2;

		<Handler as SlashingHandler<u32>>::punish_for_invalid(
			session,
			candidate,
			vec![ValidatorIndex(0)].into_iter(),
			vec![].into_iter(),
		);

		assert!(UnappliedSlashes::<Test>::get(session, candidate).is_none());
		assert!(mock_reported_offences().is_empty());
	});
}

#[test]
fn initializer_on_new_session_prunes_old_unapplied_slashes() {
	ext().execute_with(|| {
		let dispute_period = HostConfiguration::<u32>::default().dispute_period;
		let new_session: SessionIndex = dispute_period + 10;
		let pruned_session: SessionIndex = new_session - dispute_period - 1;
		let kept_session: SessionIndex = pruned_session + 1;
		let candidate_pruned = candidate_hash(1);
		let candidate_kept = candidate_hash(2);
		let id = validator_id(1);

		insert_pending_slash(
			pruned_session,
			candidate_pruned,
			DisputeOffenceKind::ForInvalidBacked,
			vec![(ValidatorIndex(0), id.clone())],
		);
		insert_pending_slash(
			kept_session,
			candidate_kept,
			DisputeOffenceKind::ForInvalidBacked,
			vec![(ValidatorIndex(0), id)],
		);

		Pallet::<Test>::initializer_on_new_session(new_session);

		assert!(UnappliedSlashes::<Test>::get(pruned_session, candidate_pruned).is_none());
		assert!(UnappliedSlashes::<Test>::get(kept_session, candidate_kept).is_some());
	});
}

#[test]
fn initializer_on_new_session_is_noop_for_early_sessions() {
	ext().execute_with(|| {
		let dispute_period = HostConfiguration::<u32>::default().dispute_period;
		let candidate = candidate_hash(1);
		let id = validator_id(1);
		insert_pending_slash(
			0,
			candidate,
			DisputeOffenceKind::ForInvalidBacked,
			vec![(ValidatorIndex(0), id)],
		);

		Pallet::<Test>::initializer_on_new_session(dispute_period + 1);
		assert!(UnappliedSlashes::<Test>::get(0, candidate).is_some());
	});
}
