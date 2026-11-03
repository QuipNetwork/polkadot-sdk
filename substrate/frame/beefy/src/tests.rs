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

use codec::Encode;
use std::{
	ops::{Mul, Sub},
	vec,
};

use frame_support::{
	assert_err, assert_ok,
	dispatch::{DispatchResultWithPostInfo, Pays},
	traits::{Currency, KeyOwnerProofSystem, OnInitialize},
};
use sp_consensus_beefy::{
	check_double_voting_proof, ecdsa_crypto,
	known_payloads::MMR_ROOT_ID,
	test_utils::{
		generate_double_voting_proof, generate_fork_voting_proof,
		generate_future_block_voting_proof, Keyring as BeefyKeyring,
	},
	Payload, ValidatorSet, ValidatorSetId, KEY_TYPE as BEEFY_KEY_TYPE,
};
use sp_runtime::{DigestItem, Perbill};
use sp_session::MembershipProof;

use crate::{self as beefy, mock::*, Call, Config, Error, WeightInfoExt};

fn init_block(block: u64) {
	System::set_block_number(block);
	// Staking has to also be initialized, and be the first, to have the new validator set ready.
	Staking::on_initialize(block);
	Session::on_initialize(block);
}

pub fn beefy_log(log: ConsensusLog<BeefyId>) -> DigestItem {
	DigestItem::Consensus(BEEFY_ENGINE_ID, log.encode())
}

#[test]
fn genesis_session_initializes_authorities() {
	let authorities = mock_authorities(vec![1, 2, 3, 4]);
	let want = authorities.clone();

	ExtBuilder::default().add_authorities(authorities).build_and_execute(|| {
		let authorities = beefy::Authorities::<Test>::get();

		assert_eq!(authorities.len(), 4);
		assert_eq!(want[0], authorities[0]);
		assert_eq!(want[1], authorities[1]);

		assert!(beefy::ValidatorSetId::<Test>::get() == 0);

		let next_authorities = beefy::NextAuthorities::<Test>::get();

		assert_eq!(next_authorities.len(), 4);
		assert_eq!(want[0], next_authorities[0]);
		assert_eq!(want[1], next_authorities[1]);
	});
}

#[test]
fn session_change_updates_authorities() {
	let authorities = mock_authorities(vec![1, 2, 3, 4]);
	let want_validators = authorities.clone();

	ExtBuilder::default()
		.add_authorities(mock_authorities(vec![1, 2, 3, 4]))
		.build_and_execute(|| {
			assert!(0 == beefy::ValidatorSetId::<Test>::get());

			init_block(1);

			assert!(1 == beefy::ValidatorSetId::<Test>::get());

			let want = beefy_log(ConsensusLog::AuthoritiesChange(
				ValidatorSet::new(want_validators, 1).unwrap(),
			));

			let log = System::digest().logs[0].clone();
			assert_eq!(want, log);

			init_block(2);

			assert!(2 == beefy::ValidatorSetId::<Test>::get());

			let want = beefy_log(ConsensusLog::AuthoritiesChange(
				ValidatorSet::new(vec![mock_beefy_id(2), mock_beefy_id(4)], 2).unwrap(),
			));

			let log = System::digest().logs[1].clone();
			assert_eq!(want, log);
		});
}

#[test]
fn session_change_updates_next_authorities() {
	let want = vec![mock_beefy_id(1), mock_beefy_id(2), mock_beefy_id(3), mock_beefy_id(4)];

	ExtBuilder::default()
		.add_authorities(mock_authorities(vec![1, 2, 3, 4]))
		.build_and_execute(|| {
			let next_authorities = beefy::NextAuthorities::<Test>::get();

			assert_eq!(next_authorities.len(), 4);
			assert_eq!(want[0], next_authorities[0]);
			assert_eq!(want[1], next_authorities[1]);
			assert_eq!(want[2], next_authorities[2]);
			assert_eq!(want[3], next_authorities[3]);

			init_block(1);

			let next_authorities = beefy::NextAuthorities::<Test>::get();

			assert_eq!(next_authorities.len(), 2);
			assert_eq!(want[1], next_authorities[0]);
			assert_eq!(want[3], next_authorities[1]);
		});
}

#[test]
fn validator_set_at_genesis() {
	let want = vec![mock_beefy_id(1), mock_beefy_id(2)];

	ExtBuilder::default()
		.add_authorities(mock_authorities(vec![1, 2, 3, 4]))
		.build_and_execute(|| {
			let vs = Beefy::validator_set().unwrap();

			assert_eq!(vs.id(), 0u64);
			assert_eq!(vs.validators()[0], want[0]);
			assert_eq!(vs.validators()[1], want[1]);
		});
}

#[test]
fn validator_set_updates_work() {
	let want = vec![mock_beefy_id(1), mock_beefy_id(2), mock_beefy_id(3), mock_beefy_id(4)];

	ExtBuilder::default()
		.add_authorities(mock_authorities(vec![1, 2, 3, 4]))
		.build_and_execute(|| {
			let vs = Beefy::validator_set().unwrap();
			assert_eq!(vs.id(), 0u64);
			assert_eq!(want[0], vs.validators()[0]);
			assert_eq!(want[1], vs.validators()[1]);
			assert_eq!(want[2], vs.validators()[2]);
			assert_eq!(want[3], vs.validators()[3]);

			init_block(1);

			let vs = Beefy::validator_set().unwrap();

			assert_eq!(vs.id(), 1u64);
			assert_eq!(want[0], vs.validators()[0]);
			assert_eq!(want[1], vs.validators()[1]);

			init_block(2);

			let vs = Beefy::validator_set().unwrap();

			assert_eq!(vs.id(), 2u64);
			assert_eq!(want[1], vs.validators()[0]);
			assert_eq!(want[3], vs.validators()[1]);
		});
}

#[test]
fn cleans_up_old_set_id_session_mappings() {
	ExtBuilder::default()
		.add_authorities(mock_authorities(vec![1, 2, 3, 4]))
		.build_and_execute(|| {
			let max_set_id_session_entries = MaxSetIdSessionEntries::get();

			// we have 3 sessions per era
			let era_limit = max_set_id_session_entries / 3;
			// sanity check against division precision loss
			assert_eq!(0, max_set_id_session_entries % 3);
			// go through `max_set_id_session_entries` sessions
			start_era(era_limit);

			// we should have a session id mapping for all the set ids from
			// `max_set_id_session_entries` eras we have observed
			for i in 1..=max_set_id_session_entries {
				assert!(beefy::SetIdSession::<Test>::get(i as u64).is_some());
			}

			// go through another `max_set_id_session_entries` sessions
			start_era(era_limit * 2);

			// we should keep tracking the new mappings for new sessions
			for i in max_set_id_session_entries + 1..=max_set_id_session_entries * 2 {
				assert!(beefy::SetIdSession::<Test>::get(i as u64).is_some());
			}

			// but the old ones should have been pruned by now
			for i in 1..=max_set_id_session_entries {
				assert!(beefy::SetIdSession::<Test>::get(i as u64).is_none());
			}
		});
}

/// Returns a list with 3 authorities with known keys:
/// Alice, Bob and Charlie.
pub fn test_authorities() -> Vec<BeefyId> {
	let authorities = vec![BeefyKeyring::Alice, BeefyKeyring::Bob, BeefyKeyring::Charlie];
	authorities.into_iter().map(|id| id.public()).collect()
}

#[test]
fn should_sign_and_verify() {
	let set_id = 3;
	let payload1 = Payload::from_single_entry(MMR_ROOT_ID, vec![42]);
	let payload2 = Payload::from_single_entry(MMR_ROOT_ID, vec![128]);

	// generate an equivocation proof, with two votes in the same round for
	// same payload signed by the same key
	let equivocation_proof = generate_double_voting_proof(
		(1, payload1.clone(), set_id, &BeefyKeyring::Bob),
		(1, payload1.clone(), set_id, &BeefyKeyring::Bob),
	);
	// expect invalid equivocation proof
	assert!(!check_double_voting_proof::<_, _>(&equivocation_proof));

	// generate an equivocation proof, with two votes in different rounds for
	// different payloads signed by the same key
	let equivocation_proof = generate_double_voting_proof(
		(1, payload1.clone(), set_id, &BeefyKeyring::Bob),
		(2, payload2.clone(), set_id, &BeefyKeyring::Bob),
	);
	// expect invalid equivocation proof
	assert!(!check_double_voting_proof::<_, _>(&equivocation_proof));

	// generate an equivocation proof, with two votes by different authorities
	let equivocation_proof = generate_double_voting_proof(
		(1, payload1.clone(), set_id, &BeefyKeyring::Alice),
		(1, payload2.clone(), set_id, &BeefyKeyring::Bob),
	);
	// expect invalid equivocation proof
	assert!(!check_double_voting_proof::<_, _>(&equivocation_proof));

	// generate an equivocation proof, with two votes in different set ids
	let equivocation_proof = generate_double_voting_proof(
		(1, payload1.clone(), set_id, &BeefyKeyring::Bob),
		(1, payload2.clone(), set_id + 1, &BeefyKeyring::Bob),
	);
	// expect invalid equivocation proof
	assert!(!check_double_voting_proof::<_, _>(&equivocation_proof));

	// generate an equivocation proof, with two votes in the same round for
	// different payloads signed by the same key
	let payload2 = Payload::from_single_entry(MMR_ROOT_ID, vec![128]);
	let equivocation_proof = generate_double_voting_proof(
		(1, payload1, set_id, &BeefyKeyring::Bob),
		(1, payload2, set_id, &BeefyKeyring::Bob),
	);
	// expect valid equivocation proof
	assert!(check_double_voting_proof::<_, _>(&equivocation_proof));
}

trait ReportEquivocationFn:
	FnMut(
	u64,
	ValidatorSetId,
	&BeefyKeyring<ecdsa_crypto::AuthorityId>,
	MembershipProof,
) -> DispatchResultWithPostInfo
{
}

impl<F> ReportEquivocationFn for F where
	F: FnMut(
		u64,
		ValidatorSetId,
		&BeefyKeyring<ecdsa_crypto::AuthorityId>,
		MembershipProof,
	) -> DispatchResultWithPostInfo
{
}

fn report_double_voting(
	block_num: u64,
	set_id: ValidatorSetId,
	equivocation_keyring: &BeefyKeyring<ecdsa_crypto::AuthorityId>,
	key_owner_proof: MembershipProof,
) -> DispatchResultWithPostInfo {
	let payload1 = Payload::from_single_entry(MMR_ROOT_ID, vec![42]);
	let payload2 = Payload::from_single_entry(MMR_ROOT_ID, vec![128]);
	let equivocation_proof = generate_double_voting_proof(
		(block_num, payload1, set_id, &equivocation_keyring),
		(block_num, payload2, set_id, &equivocation_keyring),
	);

	Beefy::report_double_voting_unsigned(
		RuntimeOrigin::none(),
		Box::new(equivocation_proof),
		key_owner_proof,
	)
}

fn report_equivocation_current_set_works(
	mut f: impl ReportEquivocationFn,
	hardcoded_slash_fraction: Option<Perbill>,
) {
	let authorities = test_authorities();
	let initial_balance = 10_000_000;
	let initial_slashable_balance = 10_000;

	ExtBuilder::default().add_authorities(authorities).build_and_execute(|| {
		assert_eq!(pallet_staking::CurrentEra::<Test>::get(), Some(0));
		assert_eq!(Session::current_index(), 0);

		start_era(1);

		let block_num = System::block_number();
		let validator_set = Beefy::validator_set().unwrap();
		let authorities = validator_set.validators();
		let set_id = validator_set.id();
		let validators = Session::validators();

		// make sure that all validators have the same balance
		for validator in &validators {
			assert_eq!(Balances::total_balance(validator), initial_balance);
			assert_eq!(Staking::slashable_balance_of(validator), initial_slashable_balance);

			assert_eq!(
				Staking::eras_stakers(1, &validator),
				pallet_staking::Exposure {
					total: initial_slashable_balance,
					own: initial_slashable_balance,
					others: vec![]
				},
			);
		}

		assert_eq!(authorities.len(), 2);
		let equivocation_authority_index = 1;
		let equivocation_key = &authorities[equivocation_authority_index];
		let equivocation_keyring = BeefyKeyring::from_public(equivocation_key).unwrap();

		// create the key ownership proof
		let key_owner_proof = Historical::prove((BEEFY_KEY_TYPE, &equivocation_key)).unwrap();

		// report the equivocation and the tx should be dispatched successfully
		assert_ok!(f(block_num, set_id, &equivocation_keyring, key_owner_proof));

		start_era(2);

		// check that the balance of 0-th validator is slashed 100%.
		let equivocation_validator_id = validators[equivocation_authority_index];

		if let Some(slash_fraction) = hardcoded_slash_fraction {
			assert_eq!(
				Balances::total_balance(&equivocation_validator_id),
				initial_balance - slash_fraction.mul(initial_slashable_balance)
			);
			assert_eq!(
				Staking::slashable_balance_of(&equivocation_validator_id),
				Perbill::from_percent(100).sub(slash_fraction).mul(initial_slashable_balance)
			);
		} else {
			assert_eq!(
				Balances::total_balance(&equivocation_validator_id),
				initial_balance - initial_slashable_balance
			);
			assert_eq!(Staking::slashable_balance_of(&equivocation_validator_id), 0);
		}
		assert_eq!(
			Staking::eras_stakers(2, &equivocation_validator_id),
			pallet_staking::Exposure { total: 0, own: 0, others: vec![] },
		);

		// check that the balances of all other validators are left intact.
		for validator in &validators {
			if *validator == equivocation_validator_id {
				continue;
			}

			assert_eq!(Balances::total_balance(validator), initial_balance);
			assert_eq!(Staking::slashable_balance_of(validator), initial_slashable_balance);

			assert_eq!(
				Staking::eras_stakers(2, &validator),
				pallet_staking::Exposure {
					total: initial_slashable_balance,
					own: initial_slashable_balance,
					others: vec![]
				},
			);
		}
	});
}

fn report_equivocation_old_set_works(
	mut f: impl ReportEquivocationFn,
	hardcoded_slash_fraction: Option<Perbill>,
) {
	let authorities = test_authorities();
	let initial_balance = 10_000_000;
	let initial_slashable_balance = 10_000;

	ExtBuilder::default().add_authorities(authorities).build_and_execute(|| {
		start_era(1);

		let block_num = System::block_number();
		let validator_set = Beefy::validator_set().unwrap();
		let authorities = validator_set.validators();
		let validators = Session::validators();
		let old_set_id = validator_set.id();

		assert_eq!(authorities.len(), 2);
		let equivocation_authority_index = 0;
		let equivocation_key = &authorities[equivocation_authority_index];

		// create the key ownership proof in the "old" set
		let key_owner_proof = Historical::prove((BEEFY_KEY_TYPE, &equivocation_key)).unwrap();

		start_era(2);

		// make sure that all authorities have the same balance
		for validator in &validators {
			assert_eq!(Balances::total_balance(validator), initial_balance);
			assert_eq!(Staking::slashable_balance_of(validator), initial_slashable_balance);

			assert_eq!(
				Staking::eras_stakers(2, &validator),
				pallet_staking::Exposure {
					total: initial_slashable_balance,
					own: initial_slashable_balance,
					others: vec![]
				},
			);
		}

		let validator_set = Beefy::validator_set().unwrap();
		let new_set_id = validator_set.id();
		assert_eq!(old_set_id + 3, new_set_id);

		let equivocation_keyring = BeefyKeyring::from_public(equivocation_key).unwrap();

		// report the equivocation and the tx should be dispatched successfully
		assert_ok!(f(block_num, old_set_id, &equivocation_keyring, key_owner_proof));

		start_era(3);

		// check that the balance of 0-th validator is slashed 100%.
		let equivocation_validator_id = validators[equivocation_authority_index];

		if let Some(slash_fraction) = hardcoded_slash_fraction {
			assert_eq!(
				Balances::total_balance(&equivocation_validator_id),
				initial_balance - slash_fraction.mul(initial_slashable_balance)
			);
			assert_eq!(
				Staking::slashable_balance_of(&equivocation_validator_id),
				Perbill::from_percent(100).sub(slash_fraction).mul(initial_slashable_balance)
			);
		} else {
			assert_eq!(
				Balances::total_balance(&equivocation_validator_id),
				initial_balance - initial_slashable_balance
			);
			assert_eq!(Staking::slashable_balance_of(&equivocation_validator_id), 0);
		}
		assert_eq!(
			Staking::eras_stakers(3, &equivocation_validator_id),
			pallet_staking::Exposure { total: 0, own: 0, others: vec![] },
		);

		// check that the balances of all other validators are left intact.
		for validator in &validators {
			if *validator == equivocation_validator_id {
				continue;
			}

			assert_eq!(Balances::total_balance(validator), initial_balance);
			assert_eq!(Staking::slashable_balance_of(validator), initial_slashable_balance);

			assert_eq!(
				Staking::eras_stakers(3, &validator),
				pallet_staking::Exposure {
					total: initial_slashable_balance,
					own: initial_slashable_balance,
					others: vec![]
				},
			);
		}
	});
}

fn report_equivocation_invalid_set_id(mut f: impl ReportEquivocationFn) {
	let authorities = test_authorities();

	ExtBuilder::default().add_authorities(authorities).build_and_execute(|| {
		start_era(1);

		let block_num = System::block_number();
		let validator_set = Beefy::validator_set().unwrap();
		let authorities = validator_set.validators();
		let set_id = validator_set.id();

		let equivocation_authority_index = 0;
		let equivocation_key = &authorities[equivocation_authority_index];
		let equivocation_keyring = BeefyKeyring::from_public(equivocation_key).unwrap();

		let key_owner_proof = Historical::prove((BEEFY_KEY_TYPE, &equivocation_key)).unwrap();

		// the call for reporting the equivocation should error
		assert_err!(
			f(block_num, set_id + 1, &equivocation_keyring, key_owner_proof),
			Error::<Test>::InvalidEquivocationProofSessionMember,
		);
	});
}

fn report_equivocation_invalid_session(mut f: impl ReportEquivocationFn) {
	let authorities = test_authorities();

	ExtBuilder::default().add_authorities(authorities).build_and_execute(|| {
		start_era(1);

		let block_num = System::block_number();
		let validator_set = Beefy::validator_set().unwrap();
		let authorities = validator_set.validators();

		let equivocation_authority_index = 0;
		let equivocation_key = &authorities[equivocation_authority_index];
		let equivocation_keyring = BeefyKeyring::from_public(equivocation_key).unwrap();

		// generate a key ownership proof at current era set id
		let key_owner_proof = Historical::prove((BEEFY_KEY_TYPE, &equivocation_key)).unwrap();

		start_era(2);

		let set_id = Beefy::validator_set().unwrap().id();

		// report an equivocation for the current set using an key ownership
		// proof from the previous set, the session should be invalid.
		assert_err!(
			f(block_num, set_id + 1, &equivocation_keyring, key_owner_proof),
			Error::<Test>::InvalidEquivocationProofSessionMember,
		);
	});
}

fn report_equivocation_invalid_key_owner_proof(mut f: impl ReportEquivocationFn) {
	let authorities = test_authorities();

	ExtBuilder::default().add_authorities(authorities).build_and_execute(|| {
		start_era(1);

		let block_num = System::block_number();
		let validator_set = Beefy::validator_set().unwrap();
		let authorities = validator_set.validators();
		let set_id = validator_set.id();

		let invalid_owner_authority_index = 1;
		let invalid_owner_key = &authorities[invalid_owner_authority_index];

		// generate a key ownership proof for the authority at index 1
		let invalid_key_owner_proof =
			Historical::prove((BEEFY_KEY_TYPE, &invalid_owner_key)).unwrap();

		let equivocation_authority_index = 0;
		let equivocation_key = &authorities[equivocation_authority_index];
		let equivocation_keyring = BeefyKeyring::from_public(equivocation_key).unwrap();

		// we need to start a new era otherwise the key ownership proof won't be
		// checked since the authorities are part of the current session
		start_era(2);

		// report an equivocation for the current set using a key ownership
		// proof for a different key than the one in the equivocation proof.
		assert_err!(
			f(block_num, set_id, &equivocation_keyring, invalid_key_owner_proof),
			Error::<Test>::InvalidKeyOwnershipProof,
		);
	});
}

fn valid_equivocation_reports_dont_pay_fees(mut f: impl ReportEquivocationFn) {
	let authorities = test_authorities();

	ExtBuilder::default().add_authorities(authorities).build_and_execute(|| {
		start_era(1);

		let block_num = System::block_number();
		let validator_set = Beefy::validator_set().unwrap();
		let authorities = validator_set.validators();
		let set_id = validator_set.id();

		let equivocation_authority_index = 0;
		let equivocation_key = &authorities[equivocation_authority_index];
		let equivocation_keyring = BeefyKeyring::from_public(equivocation_key).unwrap();

		// create the key ownership proof.
		let key_owner_proof = Historical::prove((BEEFY_KEY_TYPE, &equivocation_key)).unwrap();

		// report the equivocation.
		let post_info =
			f(block_num, set_id, &equivocation_keyring, key_owner_proof.clone()).unwrap();

		// the original weight should be kept, but given that the report
		// is valid the fee is waived.
		assert!(post_info.actual_weight.is_none());
		assert_eq!(post_info.pays_fee, Pays::No);

		// report the equivocation again which is invalid now since it is
		// duplicate.
		let post_info = f(block_num, set_id, &equivocation_keyring, key_owner_proof)
			.err()
			.unwrap()
			.post_info;

		// the fee is not waived and the original weight is kept.
		assert!(post_info.actual_weight.is_none());
		assert_eq!(post_info.pays_fee, Pays::Yes);
	})
}

// Test double voting reporting logic.

#[test]
fn report_double_voting_current_set_works() {
	report_equivocation_current_set_works(report_double_voting, None);
}

#[test]
fn report_double_voting_old_set_works() {
	report_equivocation_old_set_works(report_double_voting, None);
}

#[test]
fn report_double_voting_invalid_set_id() {
	report_equivocation_invalid_set_id(report_double_voting);
}

#[test]
fn report_double_voting_invalid_session() {
	report_equivocation_invalid_session(report_double_voting);
}

#[test]
fn report_double_voting_invalid_key_owner_proof() {
	report_equivocation_invalid_key_owner_proof(report_double_voting);
}

#[test]
fn report_double_voting_invalid_equivocation_proof() {
	let authorities = test_authorities();

	ExtBuilder::default().add_authorities(authorities).build_and_execute(|| {
		start_era(1);

		let block_num = System::block_number();
		let validator_set = Beefy::validator_set().unwrap();
		let authorities = validator_set.validators();
		let set_id = validator_set.id();

		let equivocation_authority_index = 0;
		let equivocation_key = &authorities[equivocation_authority_index];
		let equivocation_keyring = BeefyKeyring::from_public(equivocation_key).unwrap();

		// generate a key ownership proof at set id in era 1
		let key_owner_proof = Historical::prove((BEEFY_KEY_TYPE, &equivocation_key)).unwrap();

		let assert_invalid_equivocation_proof = |equivocation_proof| {
			assert_err!(
				Beefy::report_double_voting_unsigned(
					RuntimeOrigin::none(),
					Box::new(equivocation_proof),
					key_owner_proof.clone(),
				),
				Error::<Test>::InvalidDoubleVotingProof,
			);
		};

		start_era(2);

		let payload1 = Payload::from_single_entry(MMR_ROOT_ID, vec![42]);
		let payload2 = Payload::from_single_entry(MMR_ROOT_ID, vec![128]);

		// both votes target the same block number and payload,
		// there is no equivocation.
		assert_invalid_equivocation_proof(generate_double_voting_proof(
			(block_num, payload1.clone(), set_id, &equivocation_keyring),
			(block_num, payload1.clone(), set_id, &equivocation_keyring),
		));

		// votes targeting different rounds, there is no equivocation.
		assert_invalid_equivocation_proof(generate_double_voting_proof(
			(block_num, payload1.clone(), set_id, &equivocation_keyring),
			(block_num + 1, payload2.clone(), set_id, &equivocation_keyring),
		));

		// votes signed with different authority keys
		assert_invalid_equivocation_proof(generate_double_voting_proof(
			(block_num, payload1.clone(), set_id, &equivocation_keyring),
			(block_num, payload1.clone(), set_id, &BeefyKeyring::Charlie),
		));

		// votes signed with a key that isn't part of the authority set
		assert_invalid_equivocation_proof(generate_double_voting_proof(
			(block_num, payload1.clone(), set_id, &equivocation_keyring),
			(block_num, payload1.clone(), set_id, &BeefyKeyring::Dave),
		));

		// votes targeting different set ids
		assert_invalid_equivocation_proof(generate_double_voting_proof(
			(block_num, payload1, set_id, &equivocation_keyring),
			(block_num, payload2, set_id + 1, &equivocation_keyring),
		));
	});
}

#[test]
#[allow(deprecated)]
fn report_double_voting_validate_unsigned_prevents_duplicates() {
	use sp_runtime::transaction_validity::{
		InvalidTransaction, TransactionPriority, TransactionSource, TransactionValidity,
		ValidTransaction,
	};

	let authorities = test_authorities();

	ExtBuilder::default().add_authorities(authorities).build_and_execute(|| {
		start_era(1);

		let block_num = System::block_number();
		let validator_set = Beefy::validator_set().unwrap();
		let authorities = validator_set.validators();
		let set_id = validator_set.id();

		// generate and report an equivocation for the validator at index 0
		let equivocation_authority_index = 0;
		let equivocation_key = &authorities[equivocation_authority_index];
		let equivocation_keyring = BeefyKeyring::from_public(equivocation_key).unwrap();

		let payload1 = Payload::from_single_entry(MMR_ROOT_ID, vec![42]);
		let payload2 = Payload::from_single_entry(MMR_ROOT_ID, vec![128]);
		let equivocation_proof = generate_double_voting_proof(
			(block_num, payload1, set_id, &equivocation_keyring),
			(block_num, payload2, set_id, &equivocation_keyring),
		);

		let key_owner_proof = Historical::prove((BEEFY_KEY_TYPE, &equivocation_key)).unwrap();

		let call = Call::report_double_voting_unsigned {
			equivocation_proof: Box::new(equivocation_proof.clone()),
			key_owner_proof: key_owner_proof.clone(),
		};

		// only local/inblock reports are allowed
		assert_eq!(
			<Beefy as sp_runtime::traits::ValidateUnsigned>::validate_unsigned(
				TransactionSource::External,
				&call,
			),
			InvalidTransaction::Call.into(),
		);

		// the transaction is valid when passed as local
		let tx_tag = (equivocation_key, set_id, 3u64);

		assert_eq!(
			<Beefy as sp_runtime::traits::ValidateUnsigned>::validate_unsigned(
				TransactionSource::Local,
				&call,
			),
			TransactionValidity::Ok(ValidTransaction {
				priority: TransactionPriority::max_value(),
				requires: vec![],
				provides: vec![("BeefyEquivocation", tx_tag).encode()],
				longevity: ReportLongevity::get(),
				propagate: false,
			})
		);

		// the pre dispatch checks should also pass
		assert_ok!(<Beefy as sp_runtime::traits::ValidateUnsigned>::pre_dispatch(&call));

		// we submit the report
		Beefy::report_double_voting_unsigned(
			RuntimeOrigin::none(),
			Box::new(equivocation_proof),
			key_owner_proof,
		)
		.unwrap();

		// the report should now be considered stale and the transaction is invalid
		// the check for staleness should be done on both `validate_unsigned` and on `pre_dispatch`
		assert_err!(
			<Beefy as sp_runtime::traits::ValidateUnsigned>::validate_unsigned(
				TransactionSource::Local,
				&call,
			),
			InvalidTransaction::Stale,
		);

		assert_err!(
			<Beefy as sp_runtime::traits::ValidateUnsigned>::pre_dispatch(&call),
			InvalidTransaction::Stale,
		);
	});
}

#[test]
fn report_double_voting_has_valid_weight() {
	// the weight depends on the size of the validator set,
	// but there's a lower bound of 100 validators.
	assert!((1..=100)
		.map(|validators| <<Test as Config>::WeightInfo as WeightInfoExt>::report_double_voting(
			validators, 1000
		))
		.collect::<Vec<_>>()
		.windows(2)
		.all(|w| w[0] == w[1]));

	// after 100 validators the weight should keep increasing
	// with every extra validator.
	assert!((100..=1000)
		.map(|validators| <<Test as Config>::WeightInfo as WeightInfoExt>::report_double_voting(
			validators, 1000
		))
		.collect::<Vec<_>>()
		.windows(2)
		.all(|w| w[0].ref_time() < w[1].ref_time()));
}

#[test]
fn valid_double_voting_reports_dont_pay_fees() {
	valid_equivocation_reports_dont_pay_fees(report_double_voting)
}

// Test fork voting reporting logic.

fn report_fork_voting(
	block_num: u64,
	set_id: ValidatorSetId,
	equivocation_keyring: &BeefyKeyring<ecdsa_crypto::AuthorityId>,
	key_owner_proof: MembershipProof,
) -> DispatchResultWithPostInfo {
	let payload = Payload::from_single_entry(MMR_ROOT_ID, vec![42]);
	let equivocation_proof = generate_fork_voting_proof(
		(block_num, payload, set_id, &equivocation_keyring),
		MockAncestryProof { is_optimal: true, is_non_canonical: true },
		System::finalize(),
	);

	Beefy::report_fork_voting_unsigned(
		RuntimeOrigin::none(),
		Box::new(equivocation_proof),
		key_owner_proof,
	)
}

#[test]
fn report_fork_voting_current_set_works() {
	report_equivocation_current_set_works(report_fork_voting, Some(Perbill::from_percent(50)));
}

#[test]
fn report_fork_voting_old_set_works() {
	report_equivocation_old_set_works(report_fork_voting, Some(Perbill::from_percent(50)));
}

#[test]
fn report_fork_voting_invalid_set_id() {
	report_equivocation_invalid_set_id(report_fork_voting);
}

#[test]
fn report_fork_voting_invalid_session() {
	report_equivocation_invalid_session(report_fork_voting);
}

#[test]
fn report_fork_voting_invalid_key_owner_proof() {
	report_equivocation_invalid_key_owner_proof(report_fork_voting);
}

#[test]
fn report_fork_voting_non_optimal_equivocation_proof() {
	let authorities = test_authorities();

	let mut ext = ExtBuilder::default().add_authorities(authorities).build();

	let mut era = 1;
	let (block_num, set_id, equivocation_keyring, key_owner_proof) = ext.execute_with(|| {
		start_era(era);
		let block_num = System::block_number();

		let validator_set = Beefy::validator_set().unwrap();
		let authorities = validator_set.validators();
		let set_id = validator_set.id();

		let equivocation_authority_index = 0;
		let equivocation_key = &authorities[equivocation_authority_index];
		let equivocation_keyring = BeefyKeyring::from_public(equivocation_key).unwrap();

		// generate a key ownership proof at set id in era 1
		let key_owner_proof = Historical::prove((BEEFY_KEY_TYPE, &equivocation_key)).unwrap();

		era += 1;
		start_era(era);
		(block_num, set_id, equivocation_keyring, key_owner_proof)
	});
	ext.persist_offchain_overlay();

	ext.execute_with(|| {
		let payload = Payload::from_single_entry(MMR_ROOT_ID, vec![42]);

		// Simulate non optimal equivocation proof.
		let equivocation_proof = generate_fork_voting_proof(
			(block_num + 1, payload.clone(), set_id, &equivocation_keyring),
			MockAncestryProof { is_optimal: false, is_non_canonical: true },
			System::finalize(),
		);
		assert_err!(
			Beefy::report_fork_voting_unsigned(
				RuntimeOrigin::none(),
				Box::new(equivocation_proof),
				key_owner_proof.clone(),
			),
			Error::<Test>::InvalidForkVotingProof,
		);
	});
}

#[test]
fn report_fork_voting_invalid_equivocation_proof() {
	let authorities = test_authorities();

	let mut ext = ExtBuilder::default().add_authorities(authorities).build();

	let mut era = 1;
	let (block_num, set_id, equivocation_keyring, key_owner_proof) = ext.execute_with(|| {
		start_era(era);
		let block_num = System::block_number();

		let validator_set = Beefy::validator_set().unwrap();
		let authorities = validator_set.validators();
		let set_id = validator_set.id();

		let equivocation_authority_index = 0;
		let equivocation_key = &authorities[equivocation_authority_index];
		let equivocation_keyring = BeefyKeyring::from_public(equivocation_key).unwrap();

		// generate a key ownership proof at set id in era 1
		let key_owner_proof = Historical::prove((BEEFY_KEY_TYPE, &equivocation_key)).unwrap();

		era += 1;
		start_era(era);
		(block_num, set_id, equivocation_keyring, key_owner_proof)
	});
	ext.persist_offchain_overlay();

	ext.execute_with(|| {
		let payload = Payload::from_single_entry(MMR_ROOT_ID, vec![42]);

		// vote signed with a key that isn't part of the authority set
		let equivocation_proof = generate_fork_voting_proof(
			(block_num, payload.clone(), set_id, &BeefyKeyring::Dave),
			MockAncestryProof { is_optimal: true, is_non_canonical: true },
			System::finalize(),
		);
		assert_err!(
			Beefy::report_fork_voting_unsigned(
				RuntimeOrigin::none(),
				Box::new(equivocation_proof),
				key_owner_proof.clone(),
			),
			Error::<Test>::InvalidKeyOwnershipProof,
		);

		// Simulate InvalidForkVotingProof error.
		let equivocation_proof = generate_fork_voting_proof(
			(block_num + 1, payload.clone(), set_id, &equivocation_keyring),
			MockAncestryProof { is_optimal: true, is_non_canonical: false },
			System::finalize(),
		);
		assert_err!(
			Beefy::report_fork_voting_unsigned(
				RuntimeOrigin::none(),
				Box::new(equivocation_proof),
				key_owner_proof.clone(),
			),
			Error::<Test>::InvalidForkVotingProof,
		);
	});
}

#[test]
fn report_fork_voting_invalid_context() {
	let authorities = test_authorities();

	let mut ext = ExtBuilder::default().add_authorities(authorities).build();

	let mut era = 1;
	let block_num = ext.execute_with(|| {
		assert_eq!(pallet_staking::CurrentEra::<Test>::get(), Some(0));
		assert_eq!(Session::current_index(), 0);
		start_era(era);

		let block_num = System::block_number();
		era += 1;
		start_era(era);
		block_num
	});
	ext.persist_offchain_overlay();

	ext.execute_with(|| {
		let validator_set = Beefy::validator_set().unwrap();
		let authorities = validator_set.validators();
		let set_id = validator_set.id();
		let validators = Session::validators();

		// make sure that all validators have the same balance
		for validator in &validators {
			assert_eq!(Balances::total_balance(validator), 10_000_000);
			assert_eq!(Staking::slashable_balance_of(validator), 10_000);

			assert_eq!(
				Staking::eras_stakers(era, validator),
				pallet_staking::Exposure { total: 10_000, own: 10_000, others: vec![] },
			);
		}

		assert_eq!(authorities.len(), 2);
		let equivocation_authority_index = 1;
		let equivocation_key = &authorities[equivocation_authority_index];
		let equivocation_keyring = BeefyKeyring::from_public(equivocation_key).unwrap();

		let payload = Payload::from_single_entry(MMR_ROOT_ID, vec![42]);

		// generate a fork equivocation proof, with a vote in the same round for a
		// different payload than finalized
		let equivocation_proof = generate_fork_voting_proof(
			(block_num, payload, set_id, &equivocation_keyring),
			MockAncestryProof { is_optimal: true, is_non_canonical: true },
			System::finalize(),
		);

		// create the key ownership proof
		let key_owner_proof = Historical::prove((BEEFY_KEY_TYPE, &equivocation_key)).unwrap();

		// report an equivocation for the current set. Simulate a failure of
		// `extract_validation_context`
		AncestryProofContext::set(&None);
		assert_err!(
			Beefy::report_fork_voting_unsigned(
				RuntimeOrigin::none(),
				Box::new(equivocation_proof.clone()),
				key_owner_proof.clone(),
			),
			Error::<Test>::InvalidForkVotingProof,
		);

		// report an equivocation for the current set. Simulate an invalid context.
		AncestryProofContext::set(&Some(MockAncestryProofContext { is_valid: false }));
		assert_err!(
			Beefy::report_fork_voting_unsigned(
				RuntimeOrigin::none(),
				Box::new(equivocation_proof),
				key_owner_proof,
			),
			Error::<Test>::InvalidForkVotingProof,
		);
	});
}

#[test]
fn valid_fork_voting_reports_dont_pay_fees() {
	valid_equivocation_reports_dont_pay_fees(report_fork_voting)
}

// Test future block voting reporting logic.

fn report_future_block_voting(
	block_num: u64,
	set_id: ValidatorSetId,
	equivocation_keyring: &BeefyKeyring<ecdsa_crypto::AuthorityId>,
	key_owner_proof: MembershipProof,
) -> DispatchResultWithPostInfo {
	let payload = Payload::from_single_entry(MMR_ROOT_ID, vec![42]);
	let equivocation_proof = generate_future_block_voting_proof((
		block_num + 100,
		payload,
		set_id,
		&equivocation_keyring,
	));

	Beefy::report_future_block_voting_unsigned(
		RuntimeOrigin::none(),
		Box::new(equivocation_proof),
		key_owner_proof,
	)
}

#[test]
fn report_future_block_voting_current_set_works() {
	report_equivocation_current_set_works(
		report_future_block_voting,
		Some(Perbill::from_percent(50)),
	);
}

#[test]
fn report_future_block_voting_old_set_works() {
	report_equivocation_old_set_works(report_future_block_voting, Some(Perbill::from_percent(50)));
}

#[test]
fn report_future_block_voting_invalid_set_id() {
	report_equivocation_invalid_set_id(report_future_block_voting);
}

#[test]
fn report_future_block_voting_invalid_session() {
	report_equivocation_invalid_session(report_future_block_voting);
}

#[test]
fn report_future_block_voting_invalid_key_owner_proof() {
	report_equivocation_invalid_key_owner_proof(report_future_block_voting);
}

#[test]
fn report_future_block_voting_invalid_equivocation_proof() {
	let authorities = test_authorities();

	ExtBuilder::default().add_authorities(authorities).build_and_execute(|| {
		start_era(1);

		let validator_set = Beefy::validator_set().unwrap();
		let authorities = validator_set.validators();
		let set_id = validator_set.id();

		let equivocation_authority_index = 0;
		let equivocation_key = &authorities[equivocation_authority_index];
		let equivocation_keyring = BeefyKeyring::from_public(equivocation_key).unwrap();

		// create the key ownership proof
		let key_owner_proof = Historical::prove((BEEFY_KEY_TYPE, &equivocation_key)).unwrap();

		start_era(2);

		let payload = Payload::from_single_entry(MMR_ROOT_ID, vec![42]);

		// vote targeting old block
		assert_err!(
			Beefy::report_future_block_voting_unsigned(
				RuntimeOrigin::none(),
				Box::new(generate_future_block_voting_proof((
					1,
					payload.clone(),
					set_id,
					&equivocation_keyring,
				))),
				key_owner_proof.clone(),
			),
			Error::<Test>::InvalidFutureBlockVotingProof,
		);
	});
}

#[test]
fn report_future_block_voting_invalid_signature() {
	let authorities = test_authorities();

	ExtBuilder::default().add_authorities(authorities).build_and_execute(|| {
		start_era(1);

		let block_num = System::block_number();
		let validator_set = Beefy::validator_set().expect(
			"starting era 1 with configured authorities initializes the BEEFY validator set; qed",
		);
		let equivocation_key = &validator_set.validators()[0];
		let equivocation_keyring = BeefyKeyring::from_public(equivocation_key)
			.expect("test_authorities returns keys from BeefyKeyring; qed");
		let key_owner_proof = Historical::prove((BEEFY_KEY_TYPE, equivocation_key))
			.expect("the current validator key is registered in historical session storage; qed");

		let mut equivocation_proof = generate_future_block_voting_proof((
			block_num + 100,
			Payload::from_single_entry(MMR_ROOT_ID, vec![42]),
			validator_set.id(),
			&equivocation_keyring,
		));
		// Alter the signed payload without updating the signature.
		equivocation_proof.vote.commitment.payload =
			Payload::from_single_entry(MMR_ROOT_ID, vec![200]);

		assert_err!(
			Beefy::report_future_block_voting_unsigned(
				RuntimeOrigin::none(),
				Box::new(equivocation_proof),
				key_owner_proof,
			),
			Error::<Test>::InvalidFutureBlockVotingProof,
		);
	});
}

#[test]
fn valid_future_block_voting_reports_dont_pay_fees() {
	valid_equivocation_reports_dont_pay_fees(report_future_block_voting)
}

#[test]
fn set_new_genesis_works() {
	let authorities = test_authorities();

	ExtBuilder::default().add_authorities(authorities).build_and_execute(|| {
		start_era(1);

		let new_genesis_delay = 10u64;
		// the call for setting new genesis should work
		assert_ok!(Beefy::set_new_genesis(RuntimeOrigin::root(), new_genesis_delay,));
		let expected = System::block_number() + new_genesis_delay;
		// verify new genesis was set
		assert_eq!(beefy::GenesisBlock::<Test>::get(), Some(expected));

		// setting delay < 1 should fail
		assert_err!(
			Beefy::set_new_genesis(RuntimeOrigin::root(), 0u64,),
			Error::<Test>::InvalidConfiguration,
		);
	});
}
