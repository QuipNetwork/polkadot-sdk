// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Tests for `pallet-registrar-relay`.

use crate::{mock::*, Error, Event, PendingRegistrations};
use frame_support::{assert_noop, assert_ok};
use registrar_primitives::{
	FailureReason, MessageToPara, MessageToParaV1, MessageToRelay, MessageToRelayV1, ParaId,
};
use sp_runtime::{
	traits::{BlakeTwo256, Hash},
	transaction_validity::{InvalidTransaction, TransactionSource},
	DispatchError,
};

fn head(len: usize) -> Vec<u8> {
	vec![7u8; len]
}

fn code(len: usize) -> Vec<u8> {
	vec![3u8; len]
}

fn hash_of(code: &[u8]) -> sp_core::H256 {
	BlakeTwo256::hash(code)
}

/// The message id every test registration request carries. An arbitrary value: this side only
/// echoes what the parachain sent.
const MSG_ID: u64 = 5;

/// The message id every test cancellation carries. Distinct from [`MSG_ID`] so the tests notice
/// if a cancellation were answered with the registration's id.
const CANCEL_ID: u64 = 6;

fn register_msg(
	para_id: ParaId,
	head_len: usize,
	code_len: usize,
) -> (MessageToRelay<AccountId>, Vec<u8>) {
	let blob = code(code_len);
	let msg = MessageToRelay::V1(MessageToRelayV1::Register {
		para_id,
		message_id: MSG_ID,
		manager: ALICE,
		genesis_head: head(head_len),
		code_hash: hash_of(&blob),
		code_len: code_len as u32,
	});
	(msg, blob)
}

/// Push a valid registration request through and return the code that will satisfy it.
fn request(para_id: ParaId, head_len: usize, code_len: usize) -> Vec<u8> {
	let (msg, blob) = register_msg(para_id, head_len, code_len);
	assert_ok!(Registrar::authorize_code(RuntimeOrigin::root(), msg));
	blob
}

fn failure_report(para_id: ParaId, reason: FailureReason) -> MessageToPara {
	MessageToPara::V1(MessageToParaV1::RegisterResponse {
		para_id,
		message_id: MSG_ID,
		outcome: Err(reason),
	})
}

fn success_report(para_id: ParaId) -> MessageToPara {
	MessageToPara::V1(MessageToParaV1::RegisterResponse {
		para_id,
		message_id: MSG_ID,
		outcome: Ok(()),
	})
}

fn cancel_msg(para_id: ParaId) -> MessageToRelay<AccountId> {
	MessageToRelay::V1(MessageToRelayV1::CancelRegistration { para_id, message_id: CANCEL_ID })
}

fn cancel_report(para_id: ParaId, outcome: registrar_primitives::Outcome) -> MessageToPara {
	MessageToPara::V1(MessageToParaV1::CancelResponse { para_id, message_id: CANCEL_ID, outcome })
}

/// Run `authorize_apply_authorized_code` and the dispatch together, the way the node does.
///
/// Returns both verdicts so a test can assert the pool and the block agree.
fn authorize_and_dispatch(
	para_id: ParaId,
	validation_code: Vec<u8>,
) -> (Result<(), InvalidTransaction>, Result<(), DispatchError>) {
	let authorized = Registrar::authorize_apply_authorized_code(
		TransactionSource::External,
		&para_id,
		&validation_code,
	)
	.map(|_| ())
	.map_err(|e| match e {
		sp_runtime::transaction_validity::TransactionValidityError::Invalid(i) => i,
		other => panic!("unexpected validity error: {other:?}"),
	});

	let dispatched = Registrar::apply_authorized_code(
		frame_system::RawOrigin::Authorized.into(),
		para_id,
		validation_code,
	)
	.map(|_| ())
	.map_err(|e| e.error);

	(authorized, dispatched)
}

mod authorize_code {
	use super::*;

	#[test]
	fn parks_a_valid_request_and_says_nothing_to_the_parachain_yet() {
		new_test_ext().execute_with(|| {
			let blob = request(PARA_A, 20, 300);

			let pending = PendingRegistrations::<Test>::get(PARA_A).unwrap();
			assert_eq!(pending.manager, ALICE);
			assert_eq!(pending.message_id, MSG_ID);
			assert_eq!(pending.genesis_head.into_inner(), head(20));
			assert_eq!(pending.code_hash, hash_of(&blob));
			assert_eq!(pending.code_len, 300);
			assert_eq!(PendingRegistrations::<Test>::count(), 1);

			assert_eq!(
				registrar_events(),
				vec![Event::RegistrationPending {
					para_id: PARA_A,
					message_id: MSG_ID,
					code_hash: hash_of(&blob),
				}]
			);
			// Nothing is reported until the code actually lands.
			assert!(take_sent().is_empty());
		});
	}

	#[test]
	fn only_the_parachain_may_send_requests() {
		new_test_ext().execute_with(|| {
			let (msg, _) = register_msg(PARA_A, 20, 300);
			assert_noop!(
				Registrar::authorize_code(RuntimeOrigin::signed(ALICE), msg),
				DispatchError::BadOrigin
			);
		});
	}

	#[test]
	fn a_para_the_relay_chain_already_knows_is_rejected_and_reported() {
		new_test_ext().execute_with(|| {
			AlreadyKnown::set(vec![PARA_A]);
			let (msg, _) = register_msg(PARA_A, 20, 300);

			// A business rejection is not an extrinsic failure: erroring would roll back the
			// report and strand the parachain's deposit.
			assert_ok!(Registrar::authorize_code(RuntimeOrigin::root(), msg));

			assert!(PendingRegistrations::<Test>::get(PARA_A).is_none());
			assert_eq!(take_sent(), vec![failure_report(PARA_A, FailureReason::AlreadyRegistered)]);
			assert_eq!(
				registrar_events(),
				vec![Event::RegistrationRejected {
					para_id: PARA_A,
					message_id: MSG_ID,
					reason: FailureReason::AlreadyRegistered,
				}]
			);
		});
	}

	#[test]
	fn a_duplicate_request_is_rejected_and_reported() {
		new_test_ext().execute_with(|| {
			request(PARA_A, 20, 300);
			let _ = registrar_events();
			let _ = take_sent();

			let (msg, _) = register_msg(PARA_A, 20, 300);
			assert_ok!(Registrar::authorize_code(RuntimeOrigin::root(), msg));

			assert_eq!(PendingRegistrations::<Test>::count(), 1);
			assert_eq!(take_sent(), vec![failure_report(PARA_A, FailureReason::AlreadyRegistered)]);
		});
	}

	#[test]
	fn onboarding_bounds_are_enforced_before_any_code_is_uploaded() {
		new_test_ext().execute_with(|| {
			for (head_len, code_len) in
				[(MAX_HEAD_SIZE as usize + 1, 300), (20, MAX_CODE_SIZE as usize + 1), (20, 1)]
			{
				let (msg, _) = register_msg(PARA_A, head_len, code_len);
				assert_ok!(Registrar::authorize_code(RuntimeOrigin::root(), msg));

				assert!(PendingRegistrations::<Test>::get(PARA_A).is_none());
				assert_eq!(
					take_sent(),
					vec![failure_report(PARA_A, FailureReason::InvalidOnboardingData)]
				);
				let _ = registrar_events();
			}
		});
	}

	#[test]
	fn requests_beyond_the_pending_cap_are_rejected_and_reported() {
		new_test_ext().execute_with(|| {
			for para_id in 0..MAX_PENDING {
				request(PARA_A + para_id, 20, 300);
			}
			let _ = registrar_events();
			let _ = take_sent();

			let overflow = PARA_A + MAX_PENDING;
			let (msg, _) = register_msg(overflow, 20, 300);
			assert_ok!(Registrar::authorize_code(RuntimeOrigin::root(), msg));

			assert!(PendingRegistrations::<Test>::get(overflow).is_none());
			assert_eq!(PendingRegistrations::<Test>::count(), MAX_PENDING);
			assert_eq!(take_sent(), vec![failure_report(overflow, FailureReason::TooManyPending)]);
		});
	}
}

mod apply_authorized_code {
	use super::*;

	#[test]
	fn the_right_blob_onboards_the_para_and_reports_success() {
		new_test_ext().execute_with(|| {
			let blob = request(PARA_A, 20, 300);
			let _ = registrar_events();

			let (authorized, dispatched) = authorize_and_dispatch(PARA_A, blob.clone());
			assert_eq!(authorized, Ok(()));
			assert_eq!(dispatched, Ok(()));

			assert_eq!(Onboarded::get(), vec![(PARA_A, ALICE, head(20), blob)]);
			assert!(PendingRegistrations::<Test>::get(PARA_A).is_none());
			assert_eq!(PendingRegistrations::<Test>::count(), 0);
			assert_eq!(take_sent(), vec![success_report(PARA_A)]);
			assert_eq!(
				registrar_events(),
				vec![Event::Registered { para_id: PARA_A, message_id: MSG_ID, manager: ALICE }]
			);
		});
	}

	#[test]
	fn nothing_pending_is_refused_by_both_the_pool_and_the_block() {
		new_test_ext().execute_with(|| {
			let (authorized, dispatched) = authorize_and_dispatch(PARA_A, code(300));

			assert_eq!(
				authorized,
				Err(InvalidTransaction::Custom(Registrar::err_to_code(Error::NothingPending)))
			);
			assert_eq!(dispatched, Err(Error::<Test>::NothingPending.into()));
			assert!(Onboarded::get().is_empty());
		});
	}

	#[test]
	fn a_blob_with_the_wrong_hash_is_refused_by_both() {
		new_test_ext().execute_with(|| {
			request(PARA_A, 20, 300);
			// Same length, different bytes: only the hash check can catch this.
			let impostor = vec![9u8; 300];

			let (authorized, dispatched) = authorize_and_dispatch(PARA_A, impostor);

			assert_eq!(
				authorized,
				Err(InvalidTransaction::Custom(Registrar::err_to_code(Error::CodeHashMismatch)))
			);
			assert_eq!(dispatched, Err(Error::<Test>::CodeHashMismatch.into()));
			assert!(Onboarded::get().is_empty());
			// Still waiting for the real thing.
			assert!(PendingRegistrations::<Test>::get(PARA_A).is_some());
		});
	}

	#[test]
	fn a_blob_of_the_wrong_length_is_refused_by_both() {
		new_test_ext().execute_with(|| {
			request(PARA_A, 20, 300);
			// Shorter than declared, so the manager underpaid on the parachain.
			let short = code(200);

			let (authorized, dispatched) = authorize_and_dispatch(PARA_A, short);

			assert_eq!(
				authorized,
				Err(InvalidTransaction::Custom(Registrar::err_to_code(Error::CodeLenMismatch)))
			);
			assert_eq!(dispatched, Err(Error::<Test>::CodeLenMismatch.into()));
			assert!(Onboarded::get().is_empty());
		});
	}

	#[test]
	fn an_oversized_blob_is_refused_before_it_is_hashed() {
		new_test_ext().execute_with(|| {
			request(PARA_A, 20, 300);

			let (authorized, dispatched) =
				authorize_and_dispatch(PARA_A, code(MAX_CODE_SIZE as usize + 1));

			assert_eq!(
				authorized,
				Err(InvalidTransaction::Custom(Registrar::err_to_code(Error::CodeTooLarge)))
			);
			assert_eq!(dispatched, Err(Error::<Test>::CodeTooLarge.into()));
		});
	}

	#[test]
	fn an_authorization_does_not_go_stale_with_time() {
		new_test_ext().execute_with(|| {
			let blob = request(PARA_A, 20, 300);

			// Nothing expires here, so however long the code takes, it is still the code that was
			// paid for.
			System::set_block_number(System::block_number() + 100_000);

			let (authorized, dispatched) = authorize_and_dispatch(PARA_A, blob);
			assert_eq!(authorized, Ok(()));
			assert_eq!(dispatched, Ok(()));
		});
	}

	#[test]
	fn it_cannot_be_replayed() {
		new_test_ext().execute_with(|| {
			let blob = request(PARA_A, 20, 300);
			assert_eq!(authorize_and_dispatch(PARA_A, blob.clone()).1, Ok(()));

			let (authorized, dispatched) = authorize_and_dispatch(PARA_A, blob);
			assert_eq!(
				authorized,
				Err(InvalidTransaction::Custom(Registrar::err_to_code(Error::NothingPending)))
			);
			assert_eq!(dispatched, Err(Error::<Test>::NothingPending.into()));
			// Onboarded exactly once.
			assert_eq!(Onboarded::get().len(), 1);
		});
	}

	#[test]
	fn a_registrar_failure_leaves_the_request_pending() {
		new_test_ext().execute_with(|| {
			let blob = request(PARA_A, 20, 300);
			RegisterFails::set(true);
			let _ = take_sent();

			assert!(Registrar::apply_authorized_code(
				frame_system::RawOrigin::Authorized.into(),
				PARA_A,
				blob.clone()
			)
			.is_err());

			// Nothing was consumed and nothing was reported, so the user can retry.
			assert!(PendingRegistrations::<Test>::get(PARA_A).is_some());
			assert_eq!(PendingRegistrations::<Test>::count(), 1);
			assert!(take_sent().is_empty());

			RegisterFails::set(false);
			assert_ok!(Registrar::apply_authorized_code(
				frame_system::RawOrigin::Authorized.into(),
				PARA_A,
				blob
			));
		});
	}

	#[test]
	fn a_signed_origin_is_not_an_authorized_one() {
		new_test_ext().execute_with(|| {
			let blob = request(PARA_A, 20, 300);
			assert_noop!(
				Registrar::apply_authorized_code(RuntimeOrigin::signed(ALICE), PARA_A, blob),
				DispatchError::BadOrigin
			);
		});
	}
}

mod cancel_authorization {
	use super::*;

	#[test]
	fn drops_the_authorization_and_confirms_it() {
		new_test_ext().execute_with(|| {
			let blob = request(PARA_A, 20, 300);
			let _ = registrar_events();
			let _ = take_sent();

			assert_ok!(Registrar::cancel_authorization(RuntimeOrigin::root(), cancel_msg(PARA_A)));

			assert!(PendingRegistrations::<Test>::get(PARA_A).is_none());
			assert_eq!(PendingRegistrations::<Test>::count(), 0);
			assert_eq!(take_sent(), vec![cancel_report(PARA_A, Ok(()))]);
			assert_eq!(
				registrar_events(),
				vec![Event::AuthorizationCancelled { para_id: PARA_A, message_id: CANCEL_ID }]
			);

			// And the code can no longer be pushed through.
			let (authorized, dispatched) = authorize_and_dispatch(PARA_A, blob);
			assert_eq!(
				authorized,
				Err(InvalidTransaction::Custom(Registrar::err_to_code(Error::NothingPending)))
			);
			assert_eq!(dispatched, Err(Error::<Test>::NothingPending.into()));
		});
	}

	#[test]
	fn cancelling_one_leaves_the_others_alone_and_frees_capacity() {
		new_test_ext().execute_with(|| {
			for i in 0..MAX_PENDING {
				request(PARA_A + i, 20, 300);
			}
			let _ = take_sent();

			assert_ok!(Registrar::cancel_authorization(RuntimeOrigin::root(), cancel_msg(PARA_A)));

			assert!(PendingRegistrations::<Test>::get(PARA_A).is_none());
			assert!(PendingRegistrations::<Test>::get(PARA_B).is_some());
			assert_eq!(PendingRegistrations::<Test>::count(), MAX_PENDING - 1);

			// The freed slot is usable again.
			request(PARA_A + MAX_PENDING, 20, 300);
			assert!(PendingRegistrations::<Test>::get(PARA_A + MAX_PENDING).is_some());
		});
	}

	#[test]
	fn a_cancellation_that_lost_the_race_to_the_code_is_refused() {
		new_test_ext().execute_with(|| {
			let blob = request(PARA_A, 20, 300);
			assert_eq!(authorize_and_dispatch(PARA_A, blob).1, Ok(()));
			let _ = registrar_events();
			let _ = take_sent();

			assert_ok!(Registrar::cancel_authorization(RuntimeOrigin::root(), cancel_msg(PARA_A)));

			// The para is registered, so the deposit is owed and the parachain is told so.
			assert_eq!(
				take_sent(),
				vec![cancel_report(PARA_A, Err(FailureReason::AlreadyRegistered))]
			);
			assert_eq!(
				registrar_events(),
				vec![Event::CancellationRefused { para_id: PARA_A, message_id: CANCEL_ID }]
			);
		});
	}

	#[test]
	fn a_refused_cancellation_still_clears_the_dead_authorization() {
		new_test_ext().execute_with(|| {
			request(PARA_A, 20, 300);
			// The id was taken on the relay chain some other way, so this authorization can never
			// be applied and there is no reason to keep holding a slot for it.
			AlreadyKnown::set(vec![PARA_A]);
			let _ = registrar_events();
			let _ = take_sent();

			assert_ok!(Registrar::cancel_authorization(RuntimeOrigin::root(), cancel_msg(PARA_A)));

			assert_eq!(PendingRegistrations::<Test>::count(), 0);
			assert_eq!(
				take_sent(),
				vec![cancel_report(PARA_A, Err(FailureReason::AlreadyRegistered))]
			);
			assert_eq!(
				registrar_events(),
				vec![Event::CancellationRefused { para_id: PARA_A, message_id: CANCEL_ID }]
			);
		});
	}

	#[test]
	fn cancelling_nothing_still_gets_an_answer() {
		new_test_ext().execute_with(|| {
			// The request may have been rejected here and the report lost; the parachain still has
			// a deposit to release, so silence would strand it.
			assert_ok!(Registrar::cancel_authorization(RuntimeOrigin::root(), cancel_msg(PARA_A)));

			assert_eq!(take_sent(), vec![cancel_report(PARA_A, Ok(()))]);
			assert_eq!(
				registrar_events(),
				vec![Event::AuthorizationCancelled { para_id: PARA_A, message_id: CANCEL_ID }]
			);
		});
	}

	#[test]
	fn only_the_parachain_may_cancel() {
		new_test_ext().execute_with(|| {
			request(PARA_A, 20, 300);

			assert_noop!(
				Registrar::cancel_authorization(RuntimeOrigin::signed(ALICE), cancel_msg(PARA_A)),
				DispatchError::BadOrigin
			);
			assert!(PendingRegistrations::<Test>::get(PARA_A).is_some());
		});
	}

	#[test]
	fn each_call_serves_only_its_own_message() {
		new_test_ext().execute_with(|| {
			let (register, _) = register_msg(PARA_A, 20, 300);

			assert_noop!(
				Registrar::cancel_authorization(RuntimeOrigin::root(), register),
				Error::<Test>::UnexpectedMessage
			);
			assert_noop!(
				Registrar::authorize_code(RuntimeOrigin::root(), cancel_msg(PARA_A)),
				Error::<Test>::UnexpectedMessage
			);
		});
	}
}

mod reporting {
	use super::*;

	#[test]
	fn a_bounced_report_does_not_undo_the_onboarding() {
		new_test_ext().execute_with(|| {
			let blob = request(PARA_A, 20, 300);
			let _ = registrar_events();
			SendFails::set(true);

			assert_ok!(Registrar::apply_authorized_code(
				frame_system::RawOrigin::Authorized.into(),
				PARA_A,
				blob
			));

			// The relay chain's own state is correct and stays that way; the parachain is simply
			// told nothing, and the failure is surfaced.
			assert_eq!(Onboarded::get().len(), 1);
			assert!(PendingRegistrations::<Test>::get(PARA_A).is_none());
			assert_eq!(
				registrar_events(),
				vec![
					Event::ReportFailed { para_id: PARA_A, message_id: MSG_ID },
					Event::Registered { para_id: PARA_A, message_id: MSG_ID, manager: ALICE },
				]
			);
		});
	}
}
