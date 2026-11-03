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

//! Registration end to end, over real XCM.

use crate::*;
use frame_support::{
	assert_ok,
	traits::{fungible::InspectHold, EnsureOrigin},
};
use pallet_registrar_para::{HoldReason, RegistrationState};
use para::{Balances, Runtime};
use polkadot_primitives::ValidationCode;
use registrar_primitives::{FailureReason, MessageToRelay, MessageToRelayV1};
use sp_runtime::traits::{BlakeTwo256, Hash};
use xcm_simulator::TestExt;

/// A validation code that is at least `MIN_CODE_SIZE` and hashes to something predictable.
fn code(len: usize) -> Vec<u8> {
	assert!(len >= MIN_CODE_SIZE as usize);
	(0..len).map(|i| (i % 251) as u8).collect()
}

fn head(len: usize) -> Vec<u8> {
	vec![7u8; len]
}

fn hash_of(code: &[u8]) -> sp_core::H256 {
	BlakeTwo256::hash(code)
}

/// Total held on the parachain for `who`, across both registrar reasons.
fn para_held(who: &AccountId32) -> u128 {
	<Balances as InspectHold<_>>::balance_on_hold(
		&para::RuntimeHoldReason::from(HoldReason::ParaIdReservation),
		who,
	) + <Balances as InspectHold<_>>::balance_on_hold(
		&para::RuntimeHoldReason::from(HoldReason::Registration),
		who,
	)
}

/// Reserve a para id on the parachain for `who`, returning it.
fn reserve(who: AccountId32) -> u32 {
	let mut para_id = 0;
	RegistrarPara::execute_with(|| {
		assert_ok!(para::Registrar::reserve(para::RuntimeOrigin::signed(who)));
		para_id = pallet_registrar_para::NextFreeParaId::<para::Runtime>::get() - 1;
	});
	para_id
}

/// Ask the parachain to register `para_id`, letting the request travel to the relay chain.
///
/// Returns the validation code the relay chain will now be waiting for.
fn request_registration(
	who: AccountId32,
	para_id: u32,
	head_len: usize,
	code_len: usize,
) -> Vec<u8> {
	let blob = code(code_len);
	let hash = hash_of(&blob);
	RegistrarPara::execute_with(|| {
		assert_ok!(para::Registrar::register(
			para::RuntimeOrigin::signed(who),
			para_id,
			head(head_len),
			code_len as u32,
			hash,
		));
	});
	blob
}

/// Submit the validation code to the relay chain the way an unsigned transaction would: authorize
/// first, then dispatch under the authorized origin.
fn submit_code(para_id: u32, blob: Vec<u8>) -> sp_runtime::DispatchResult {
	use pallet_registrar_relay::Pallet as RelayRegistrar;
	use sp_runtime::transaction_validity::TransactionSource;

	RelayRegistrar::<relay::Runtime>::authorize_apply_authorized_code(
		TransactionSource::External,
		&para_id,
		&blob,
	)
	.map_err(|_| sp_runtime::DispatchError::Other("not authorized"))?;

	RelayRegistrar::<relay::Runtime>::apply_authorized_code(
		frame_system::RawOrigin::Authorized.into(),
		para_id,
		blob,
	)
	.map(|_| ())
	.map_err(|e| e.error)
}

type RegistrationTicket = <Runtime as pallet_registrar_para::Config>::RegistrationConsideration;

fn para_state(para_id: u32) -> Option<RegistrationState<RegistrationTicket, u64>> {
	pallet_registrar_para::Paras::<para::Runtime>::get(para_id).map(|info| info.state)
}

#[test]
fn a_registration_travels_from_the_parachain_to_the_relay_chain_and_onboards_a_para() {
	MockNet::reset();

	// Get the relay chain into a session with active validators, otherwise there is nobody to
	// approve the PVF and onboarding never completes.
	Relay::execute_with(|| relay::run_to_session(1));

	let head_len = 32;
	let code_len = 64;
	let para_id = reserve(ALICE);
	assert_eq!(para_id, para::FIRST_PARA_ID);

	// Only the para id deposit so far.
	RegistrarPara::execute_with(|| {
		assert_eq!(para_held(&ALICE), para::PARA_DEPOSIT);
		assert_eq!(para_state(para_id), Some(RegistrationState::Reserved));
	});

	let blob = request_registration(ALICE, para_id, head_len, code_len);
	let expected_deposit = para::PER_BYTE * (head_len as u128 + code_len as u128);

	// The parachain is holding for head data plus the declared code length, and is waiting.
	RegistrarPara::execute_with(|| {
		assert_eq!(para_held(&ALICE), para::PARA_DEPOSIT + expected_deposit);
		assert!(matches!(para_state(para_id), Some(RegistrationState::Pending { .. })));
	});

	// The relay chain received the request over XCM and parked it.
	Relay::execute_with(|| {
		let pending =
			pallet_registrar_relay::PendingRegistrations::<relay::Runtime>::get(para_id).unwrap();
		assert_eq!(pending.manager, ALICE);
		assert_eq!(pending.code_hash, hash_of(&blob));
		assert_eq!(pending.code_len, code_len as u32);
		assert_eq!(pending.genesis_head.clone().into_inner(), head(head_len));

		// Nothing has been onboarded yet: the code is still missing.
		assert!(polkadot_runtime_parachains::paras::Pallet::<relay::Runtime>::lifecycle(
			para_id.into()
		)
		.is_none());
	});

	// Anybody uploads the blob. The para onboards and the relay chain reports back.
	Relay::execute_with(|| {
		assert_ok!(submit_code(para_id, blob.clone()));

		// Onboarding is scheduled; it lands after the validators approve the PVF and a session
		// rotates.
		assert!(
			pallet_registrar_relay::PendingRegistrations::<relay::Runtime>::get(para_id).is_none()
		);

		let session =
			polkadot_runtime_parachains::shared::CurrentSessionIndex::<relay::Runtime>::get();
		relay::conclude_pvf_checking(&ValidationCode(blob.clone()), session);
		relay::run_to_session(3);

		assert!(polkadot_runtime_parachains::paras::Pallet::<relay::Runtime>::is_parathread(
			para_id.into()
		));

		// The deposit lives on the parachain, so the relay chain took nothing.
		let id = polkadot_primitives::Id::from(para_id);
		let info =
			polkadot_runtime_common::paras_registrar::Paras::<relay::Runtime>::get(id).unwrap();
		assert_eq!(info.manager, ALICE);
		assert_eq!(info.deposit, 0);
		assert_eq!(pallet_balances::Pallet::<relay::Runtime>::reserved_balance(ALICE), 0);
	});

	// And the parachain heard about it.
	RegistrarPara::execute_with(|| {
		assert!(matches!(para_state(para_id), Some(RegistrationState::Registered { .. })));
		// Both deposits stay held for as long as the para is registered.
		assert_eq!(para_held(&ALICE), para::PARA_DEPOSIT + expected_deposit);
	});
}

#[test]
fn a_para_the_relay_chain_already_knows_is_refused_and_the_deposit_comes_back() {
	MockNet::reset();

	let para_id = reserve(ALICE);

	// Someone registers that same id directly on the relay chain first, the old way.
	Relay::execute_with(|| {
		assert_ok!(
			polkadot_runtime_common::paras_registrar::Pallet::<relay::Runtime>::force_register(
				relay::RuntimeOrigin::root(),
				BOB,
				100,
				para_id.into(),
				polkadot_primitives::HeadData(head(32)),
				ValidationCode(code(64)),
			)
		);
	});

	let _ = request_registration(ALICE, para_id, 32, 64);

	Relay::execute_with(|| {
		assert!(
			pallet_registrar_relay::PendingRegistrations::<relay::Runtime>::get(para_id).is_none()
		);
	});

	// The rejection made it back, the registration deposit was released, and the para id is still
	// Alice's to retry with.
	RegistrarPara::execute_with(|| {
		assert_eq!(para_state(para_id), Some(RegistrationState::Reserved));
		assert_eq!(para_held(&ALICE), para::PARA_DEPOSIT);

		let events = para::System::events();
		assert!(events.iter().any(|e| matches!(
			&e.event,
			para::RuntimeEvent::Registrar(pallet_registrar_para::Event::RegistrationFailed {
				reason: FailureReason::AlreadyRegistered,
				..
			})
		)));
	});
}

#[test]
fn a_manager_who_gives_up_drives_the_cancellation_and_gets_the_deposit_back() {
	MockNet::reset();

	let para_id = reserve(ALICE);
	let blob = request_registration(ALICE, para_id, 32, 64);

	// Nothing on the relay chain abandons the request: it waits for the code indefinitely.
	Relay::execute_with(|| {
		relay::run_to_block(relay::BLOCKS_PER_SESSION * 20);
		assert!(
			pallet_registrar_relay::PendingRegistrations::<relay::Runtime>::get(para_id).is_some()
		);
	});
	RegistrarPara::execute_with(|| {
		assert!(matches!(para_state(para_id), Some(RegistrationState::Pending { .. })));
	});

	// So the manager gives up, and pays for the round trip that ends it.
	RegistrarPara::execute_with(|| {
		para::System::set_block_number(para::PENDING_DEADLINE + 1);
		assert_ok!(para::Registrar::cancel_registration(
			para::RuntimeOrigin::signed(ALICE),
			para_id
		));

		// The deposit is still held: the relay chain has not answered yet.
		assert_eq!(para_held(&ALICE), para::PARA_DEPOSIT + para::PER_BYTE * (32 + 64));
	});

	// The relay chain drops the authorization, so the code can no longer be pushed through.
	Relay::execute_with(|| {
		assert!(
			pallet_registrar_relay::PendingRegistrations::<relay::Runtime>::get(para_id).is_none()
		);
		assert!(submit_code(para_id, blob).is_err());
	});

	// And its confirmation is what frees the deposit, leaving the para id reserved to retry with.
	RegistrarPara::execute_with(|| {
		assert_eq!(para_state(para_id), Some(RegistrationState::Reserved));
		assert_eq!(para_held(&ALICE), para::PARA_DEPOSIT);

		let events = para::System::events();
		assert!(events.iter().any(|e| matches!(
			&e.event,
			para::RuntimeEvent::Registrar(
				pallet_registrar_para::Event::RegistrationCancelled { .. }
			)
		)));
	});
}

#[test]
fn the_relay_chain_refuses_a_blob_that_is_not_the_one_that_was_paid_for() {
	MockNet::reset();

	let code_len = 64;
	let para_id = reserve(ALICE);
	let blob = request_registration(ALICE, para_id, 32, code_len);

	Relay::execute_with(|| {
		// Right length, wrong bytes: only the hash check catches this.
		let mut impostor = blob.clone();
		*impostor.last_mut().unwrap() ^= 0xff;
		assert_eq!(impostor.len(), blob.len());
		assert_ne!(hash_of(&impostor), hash_of(&blob));
		assert!(submit_code(para_id, impostor).is_err());

		// Right bytes but truncated: the manager would have underpaid.
		assert!(submit_code(para_id, blob[..code_len - 1].to_vec()).is_err());

		// Still waiting for the real thing, and nothing was onboarded.
		assert!(
			pallet_registrar_relay::PendingRegistrations::<relay::Runtime>::get(para_id).is_some()
		);
		assert!(polkadot_runtime_parachains::paras::Pallet::<relay::Runtime>::lifecycle(
			para_id.into()
		)
		.is_none());

		// The genuine blob still works.
		assert_ok!(submit_code(para_id, blob));
	});

	RegistrarPara::execute_with(|| {
		assert!(matches!(para_state(para_id), Some(RegistrationState::Registered { .. })));
	});
}

#[test]
fn only_the_registrar_parachain_may_drive_registrations() {
	MockNet::reset();

	Relay::execute_with(|| {
		use polkadot_runtime_parachains::Origin as ParachainsOrigin;

		let message = MessageToRelay::V1(MessageToRelayV1::Register {
			para_id: 3000,
			message_id: 0,
			manager: BOB,
			genesis_head: head(32),
			code_hash: hash_of(&code(64)),
			code_len: 64,
		});

		// A different parachain's origin is not accepted...
		let other_para: relay::RuntimeOrigin =
			ParachainsOrigin::Parachain((PARA_ID + 1).into()).into();
		assert!(senders::EnsureRegistrarPara::try_origin(other_para.clone()).is_err());
		assert!(relay::Registrar::authorize_code(other_para, message.clone()).is_err());

		// ...nor is a plain signed account.
		assert!(relay::Registrar::authorize_code(
			relay::RuntimeOrigin::signed(BOB),
			message.clone()
		)
		.is_err());

		// The configured parachain is.
		let ours: relay::RuntimeOrigin = ParachainsOrigin::Parachain(PARA_ID.into()).into();
		assert_ok!(relay::Registrar::authorize_code(ours, message));
		assert!(pallet_registrar_relay::PendingRegistrations::<relay::Runtime>::get(3000).is_some());
	});
}
