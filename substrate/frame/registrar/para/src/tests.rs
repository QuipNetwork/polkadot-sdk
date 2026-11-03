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

//! Tests for `pallet-registrar-para`.

use crate::{mock::*, Error, Event, HoldReason, Paras, RegistrationState};
use frame_support::{assert_noop, assert_ok, traits::fungible::InspectHold};
use registrar_primitives::{
	FailureReason, MessageToPara, MessageToParaV1, MessageToRelay, MessageToRelayV1, Outcome,
};
use sp_runtime::{
	traits::{BlakeTwo256, Hash},
	DispatchError, TokenError,
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

/// Total on hold for `who` across both of this pallet's reasons.
fn held(who: AccountId) -> Balance {
	Balances::balance_on_hold(&HoldReason::ParaIdReservation.into(), &who) +
		Balances::balance_on_hold(&HoldReason::Registration.into(), &who)
}

/// Reserve a para id for `who` and return it.
fn reserve_for(who: AccountId) -> u32 {
	assert_ok!(Registrar::reserve(RuntimeOrigin::signed(who)));
	let para_id = crate::NextFreeParaId::<Test>::get() - 1;
	let _ = registrar_events();
	para_id
}

/// Put `para_id` into `Pending` with a code of `code_len` bytes, and return that code.
fn request_registration(who: AccountId, para_id: u32, head_len: usize, code_len: usize) -> Vec<u8> {
	let blob = code(code_len);
	assert_ok!(Registrar::register(
		RuntimeOrigin::signed(who),
		para_id,
		head(head_len),
		code_len as u32,
		hash_of(&blob),
	));
	blob
}

mod reserve {
	use super::*;

	#[test]
	fn allocates_ids_from_the_first_public_id_upwards() {
		new_test_ext().execute_with(|| {
			assert_ok!(Registrar::reserve(RuntimeOrigin::signed(ALICE)));
			assert_ok!(Registrar::reserve(RuntimeOrigin::signed(BOB)));

			assert_eq!(
				registrar_events(),
				vec![
					Event::Reserved { para_id: FIRST_PARA_ID, who: ALICE },
					Event::Reserved { para_id: FIRST_PARA_ID + 1, who: BOB },
				]
			);
			assert_eq!(crate::NextFreeParaId::<Test>::get(), FIRST_PARA_ID + 2);
		});
	}

	#[test]
	fn holds_the_para_deposit_and_records_the_manager() {
		new_test_ext().execute_with(|| {
			let before = Balances::free_balance(ALICE);
			let para_id = reserve_for(ALICE);

			let info = Paras::<Test>::get(para_id).unwrap();
			assert_eq!(info.manager, ALICE);
			assert_eq!(info.state, RegistrationState::Reserved);
			assert_eq!(held(ALICE), PARA_DEPOSIT);
			assert_eq!(Balances::free_balance(ALICE), before - PARA_DEPOSIT);
		});
	}

	#[test]
	fn fails_without_the_deposit() {
		new_test_ext().execute_with(|| {
			// Alice keeps only enough to stay alive, well short of the deposit.
			let keep = 10;
			assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), ALICE, keep));

			assert_noop!(
				Registrar::reserve(RuntimeOrigin::signed(ALICE)),
				DispatchError::Token(TokenError::FundsUnavailable)
			);
			// Nothing was handed out.
			assert_eq!(crate::NextFreeParaId::<Test>::get(), 0);
			assert!(Paras::<Test>::iter().next().is_none());
		});
	}
}

mod register {
	use super::*;

	#[test]
	fn holds_the_data_deposit_and_asks_the_relay_chain() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			let head_len = 20;
			let code_len = 300;

			let blob = request_registration(ALICE, para_id, head_len, code_len);

			// Deposit covers head data plus the *declared* code length.
			let expected = PER_BYTE * (head_len as Balance + code_len as Balance);
			assert_eq!(held(ALICE), PARA_DEPOSIT + expected);

			let expected_at = System::block_number() + PENDING_DEADLINE;
			assert!(matches!(
				Paras::<Test>::get(para_id).unwrap().state,
				RegistrationState::Pending { cancellable_at, .. } if cancellable_at == expected_at
			));
			assert_eq!(
				registrar_events(),
				vec![Event::RegisterRequested { para_id, message_id: 0, manager: ALICE }]
			);
			assert_eq!(
				take_sent(),
				vec![MessageToRelay::V1(MessageToRelayV1::Register {
					para_id,
					message_id: 0,
					manager: ALICE,
					genesis_head: head(head_len),
					code_hash: hash_of(&blob),
					code_len: code_len as u32,
				})]
			);
			assert_eq!(crate::NextMessageId::<Test>::get(), 1);
		});
	}

	#[test]
	fn rejects_an_unreserved_id() {
		new_test_ext().execute_with(|| {
			assert_noop!(
				Registrar::register(
					RuntimeOrigin::signed(ALICE),
					FIRST_PARA_ID,
					head(10),
					100,
					hash_of(&code(100)),
				),
				Error::<Test>::NotReserved
			);
		});
	}

	#[test]
	fn rejects_someone_elses_id() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			assert_noop!(
				Registrar::register(
					RuntimeOrigin::signed(BOB),
					para_id,
					head(10),
					100,
					hash_of(&code(100)),
				),
				Error::<Test>::NotOwner
			);
		});
	}

	#[test]
	fn rejects_a_second_request_while_one_is_in_flight() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			request_registration(ALICE, para_id, 10, 100);

			assert_noop!(
				Registrar::register(
					RuntimeOrigin::signed(ALICE),
					para_id,
					head(10),
					100,
					hash_of(&code(100)),
				),
				Error::<Test>::AlreadyRegistered
			);
		});
	}

	#[test]
	fn enforces_the_relay_chains_size_bounds() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			assert_noop!(
				Registrar::register(
					RuntimeOrigin::signed(ALICE),
					para_id,
					head(MAX_HEAD_SIZE as usize + 1),
					100,
					hash_of(&code(100)),
				),
				Error::<Test>::HeadDataTooLarge
			);
			assert_noop!(
				Registrar::register(
					RuntimeOrigin::signed(ALICE),
					para_id,
					head(10),
					MAX_CODE_SIZE + 1,
					hash_of(&code(10)),
				),
				Error::<Test>::CodeTooLarge
			);
			assert_noop!(
				Registrar::register(
					RuntimeOrigin::signed(ALICE),
					para_id,
					head(10),
					MIN_CODE_SIZE - 1,
					hash_of(&code(10)),
				),
				Error::<Test>::CodeTooSmall
			);
		});
	}

	#[test]
	fn a_transport_failure_rolls_the_whole_call_back() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			SendFails::set(true);

			assert_noop!(
				Registrar::register(
					RuntimeOrigin::signed(ALICE),
					para_id,
					head(10),
					100,
					hash_of(&code(100)),
				),
				Error::<Test>::SendFailed
			);

			// The hold, the state change and the message id went with it.
			assert_eq!(held(ALICE), PARA_DEPOSIT);
			assert_eq!(Paras::<Test>::get(para_id).unwrap().state, RegistrationState::Reserved);
			assert_eq!(crate::NextMessageId::<Test>::get(), 0);
		});
	}
}

mod receive {
	use super::*;

	fn result_message(para_id: u32, message_id: u64, outcome: Outcome) -> MessageToPara {
		MessageToPara::V1(MessageToParaV1::RegisterResponse { para_id, message_id, outcome })
	}

	fn cancel_message(para_id: u32, message_id: u64, outcome: Outcome) -> MessageToPara {
		MessageToPara::V1(MessageToParaV1::CancelResponse { para_id, message_id, outcome })
	}

	#[test]
	fn success_finalises_the_registration_and_keeps_the_deposit() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			request_registration(ALICE, para_id, 20, 300);
			let _ = registrar_events();
			let deposit = PER_BYTE * (20 + 300);

			assert_ok!(Registrar::receive(
				RuntimeOrigin::root(),
				result_message(para_id, 0, Ok(())),
			));

			assert!(matches!(
				Paras::<Test>::get(para_id).unwrap().state,
				RegistrationState::Registered { .. }
			));
			assert_eq!(held(ALICE), PARA_DEPOSIT + deposit);
			assert_eq!(
				registrar_events(),
				vec![Event::Registered { para_id, message_id: 0, manager: ALICE }]
			);
		});
	}

	#[test]
	fn failure_releases_the_deposit_and_keeps_the_id_reserved() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			request_registration(ALICE, para_id, 20, 300);
			let _ = registrar_events();

			assert_ok!(Registrar::receive(
				RuntimeOrigin::root(),
				result_message(para_id, 0, Err(FailureReason::AlreadyRegistered)),
			));

			// Back to square one, para id still ours, only the para deposit still held.
			assert_eq!(Paras::<Test>::get(para_id).unwrap().state, RegistrationState::Reserved);
			assert_eq!(held(ALICE), PARA_DEPOSIT);
			assert_eq!(
				registrar_events(),
				vec![Event::RegistrationFailed {
					para_id,
					message_id: 0,
					manager: ALICE,
					reason: FailureReason::AlreadyRegistered,
				}]
			);
		});
	}

	#[test]
	fn a_user_cannot_forge_a_relay_chain_report() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			request_registration(ALICE, para_id, 20, 300);

			assert_noop!(
				Registrar::receive(
					RuntimeOrigin::signed(ALICE),
					result_message(para_id, 0, Ok(())),
				),
				DispatchError::BadOrigin
			);
		});
	}

	#[test]
	#[cfg(debug_assertions)]
	#[should_panic(expected = "register response for unknown para, dropping")]
	fn a_report_for_an_unknown_para_is_defensive() {
		new_test_ext().execute_with(|| {
			let _ = Registrar::receive(RuntimeOrigin::root(), result_message(4242, 0, Ok(())));
		});
	}

	#[test]
	#[cfg(debug_assertions)]
	#[should_panic(expected = "register response for para which is not pending, dropping")]
	fn a_report_for_a_non_pending_para_is_defensive() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			let _ = Registrar::receive(RuntimeOrigin::root(), result_message(para_id, 0, Ok(())));
		});
	}

	#[test]
	fn a_refused_cancellation_records_the_registration_and_keeps_the_deposit() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			request_registration(ALICE, para_id, 20, 300);
			let _ = registrar_events();
			let deposit = PER_BYTE * (20 + 300);

			// The code landed on the relay chain after all and the success report was lost, so the
			// cancellation comes back refused and the deposit is owed. The event echoes the
			// cancellation's message id, not the registration's.
			assert_ok!(Registrar::receive(
				RuntimeOrigin::root(),
				cancel_message(para_id, 1, Err(FailureReason::AlreadyRegistered)),
			));

			assert!(matches!(
				Paras::<Test>::get(para_id).unwrap().state,
				RegistrationState::Registered { .. }
			));
			assert_eq!(held(ALICE), PARA_DEPOSIT + deposit);
			assert_eq!(
				registrar_events(),
				vec![Event::Registered { para_id, message_id: 1, manager: ALICE }]
			);
		});
	}

	#[test]
	fn a_cancel_response_that_lost_the_race_to_a_report_is_dropped_quietly() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			request_registration(ALICE, para_id, 20, 300);
			let deposit = PER_BYTE * (20 + 300);

			// A verdict already in flight settles the registration first.
			assert_ok!(Registrar::receive(
				RuntimeOrigin::root(),
				result_message(para_id, 0, Ok(()))
			));
			let _ = registrar_events();

			// The answer to the cancellation then has nothing left to do. Unlike a stray register
			// response this is expected, so it is not defensive.
			assert_ok!(Registrar::receive(
				RuntimeOrigin::root(),
				cancel_message(para_id, 1, Ok(()))
			));

			assert!(matches!(
				Paras::<Test>::get(para_id).unwrap().state,
				RegistrationState::Registered { .. }
			));
			assert_eq!(held(ALICE), PARA_DEPOSIT + deposit);
			assert!(registrar_events().is_empty());
		});
	}

	#[test]
	#[cfg(debug_assertions)]
	#[should_panic(expected = "cancel response for unknown para, dropping")]
	fn a_cancel_response_for_an_unknown_para_is_defensive() {
		new_test_ext().execute_with(|| {
			let _ = Registrar::receive(RuntimeOrigin::root(), cancel_message(4242, 0, Ok(())));
		});
	}
}

mod cancel_registration {
	use super::*;

	fn cancel_request(para_id: u32, message_id: u64) -> MessageToRelay<AccountId> {
		MessageToRelay::V1(MessageToRelayV1::CancelRegistration { para_id, message_id })
	}

	fn cancel_confirmation(para_id: u32, message_id: u64) -> MessageToPara {
		MessageToPara::V1(MessageToParaV1::CancelResponse { para_id, message_id, outcome: Ok(()) })
	}

	#[test]
	fn is_refused_before_the_deadline() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			request_registration(ALICE, para_id, 20, 300);

			run_to_block(System::block_number() + PENDING_DEADLINE - 1);
			assert_noop!(
				Registrar::cancel_registration(RuntimeOrigin::signed(ALICE), para_id),
				Error::<Test>::CannotCancelYet
			);
		});
	}

	#[test]
	fn asks_the_relay_chain_and_only_the_answer_releases_the_deposit() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			request_registration(ALICE, para_id, 20, 300);
			let _ = registrar_events();
			let _ = take_sent();
			let deposit = PER_BYTE * (20 + 300);

			run_to_block(System::block_number() + PENDING_DEADLINE);
			assert_ok!(Registrar::cancel_registration(RuntimeOrigin::signed(ALICE), para_id));

			// The relay chain has been asked, and until it answers nothing is given back: it is the
			// only side that knows whether the code landed.
			let expected_at = System::block_number() + PENDING_DEADLINE;
			assert!(matches!(
				Paras::<Test>::get(para_id).unwrap().state,
				RegistrationState::Pending { cancellable_at, .. } if cancellable_at == expected_at
			));
			assert_eq!(held(ALICE), PARA_DEPOSIT + deposit);
			// The registration took message id 0, so the cancellation is message 1.
			assert_eq!(take_sent(), vec![cancel_request(para_id, 1)]);
			assert_eq!(
				registrar_events(),
				vec![Event::CancelRequested { para_id, message_id: 1, manager: ALICE }]
			);

			assert_ok!(Registrar::receive(RuntimeOrigin::root(), cancel_confirmation(para_id, 1)));

			assert_eq!(Paras::<Test>::get(para_id).unwrap().state, RegistrationState::Reserved);
			assert_eq!(held(ALICE), PARA_DEPOSIT);
			assert_eq!(
				registrar_events(),
				vec![Event::RegistrationCancelled { para_id, message_id: 1, manager: ALICE }]
			);

			// And the manager can simply try again on the same id.
			request_registration(ALICE, para_id, 20, 300);
		});
	}

	#[test]
	fn cannot_be_asked_again_until_another_deadline_passes() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			request_registration(ALICE, para_id, 20, 300);
			run_to_block(System::block_number() + PENDING_DEADLINE);
			assert_ok!(Registrar::cancel_registration(RuntimeOrigin::signed(ALICE), para_id));
			let _ = take_sent();

			// One request per deadline, so a cancellation that goes missing can be retried without
			// the relay chain being asked once per block.
			run_to_block(System::block_number() + PENDING_DEADLINE - 1);
			assert_noop!(
				Registrar::cancel_registration(RuntimeOrigin::signed(ALICE), para_id),
				Error::<Test>::CannotCancelYet
			);

			run_to_block(System::block_number() + 1);
			assert_ok!(Registrar::cancel_registration(RuntimeOrigin::signed(ALICE), para_id));
			// Register was 0, the first cancellation 1, so the retry carries 2.
			assert_eq!(take_sent(), vec![cancel_request(para_id, 2)]);
		});
	}

	#[test]
	fn a_transport_failure_rolls_the_whole_call_back() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);
			request_registration(ALICE, para_id, 20, 300);
			let cancellable_at = System::block_number() + PENDING_DEADLINE;
			run_to_block(cancellable_at);
			SendFails::set(true);

			assert_noop!(
				Registrar::cancel_registration(RuntimeOrigin::signed(ALICE), para_id),
				Error::<Test>::SendFailed
			);

			// The pushed-out deadline went with it, so the manager can retry immediately.
			assert!(matches!(
				Paras::<Test>::get(para_id).unwrap().state,
				RegistrationState::Pending { cancellable_at: at, .. } if at == cancellable_at
			));
			assert_eq!(held(ALICE), PARA_DEPOSIT + PER_BYTE * (20 + 300));
			SendFails::set(false);
			assert_ok!(Registrar::cancel_registration(RuntimeOrigin::signed(ALICE), para_id));
		});
	}

	#[test]
	fn rejects_a_non_manager_and_a_para_that_is_not_pending() {
		new_test_ext().execute_with(|| {
			let para_id = reserve_for(ALICE);

			// Reserved, not pending.
			assert_noop!(
				Registrar::cancel_registration(RuntimeOrigin::signed(ALICE), para_id),
				Error::<Test>::NotPending
			);

			request_registration(ALICE, para_id, 20, 300);
			run_to_block(System::block_number() + PENDING_DEADLINE);
			assert_noop!(
				Registrar::cancel_registration(RuntimeOrigin::signed(BOB), para_id),
				Error::<Test>::NotOwner
			);
		});
	}
}
