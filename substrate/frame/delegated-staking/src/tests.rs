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

//! Tests for pallet-delegated-staking.

use super::*;
use crate::mock::*;
use frame_support::{assert_noop, assert_ok, traits::fungible::InspectHold};
use pallet_nomination_pools::{Error as PoolsError, Event as PoolsEvent};
use pallet_staking::{Error as StakingError, RewardDestination};
use sp_staking::{Agent, DelegationInterface, Delegator, StakerStatus};

#[test]
fn create_an_agent_with_first_delegator() {
	ExtBuilder::default().build_and_execute(|| {
		let agent: AccountId = 200;
		let reward_account: AccountId = 201;
		let delegator: AccountId = 202;

		// set intention to accept delegation.
		fund(&agent, 1000);
		assert_ok!(DelegatedStaking::register_agent(
			RawOrigin::Signed(agent).into(),
			reward_account
		));

		// delegate to this account
		fund(&delegator, 1000);
		assert_ok!(DelegatedStaking::delegate_to_agent(
			RawOrigin::Signed(delegator).into(),
			agent,
			100
		));

		// verify
		assert!(DelegatedStaking::is_agent(&agent));
		assert_eq!(DelegatedStaking::stakeable_balance(Agent::from(agent)), 100);
		assert_eq!(
			Balances::balance_on_hold(&HoldReason::StakingDelegation.into(), &delegator),
			100
		);
		assert_eq!(DelegatedStaking::held_balance_of(Delegator::from(delegator)), 100);
	});
}

#[test]
fn cannot_become_agent() {
	ExtBuilder::default().build_and_execute(|| {
		// cannot set reward account same as agent account
		assert_noop!(
			DelegatedStaking::register_agent(RawOrigin::Signed(100).into(), 100),
			Error::<T>::InvalidRewardDestination
		);
	});
}

#[test]
fn create_multiple_delegators() {
	ExtBuilder::default().build_and_execute(|| {
		let agent: AccountId = 200;
		let reward_account: AccountId = 201;

		// stakeable balance is 0 for non agent
		fund(&agent, 1000);
		assert!(!DelegatedStaking::is_agent(&agent));
		assert_eq!(DelegatedStaking::stakeable_balance(Agent::from(agent)), 0);

		// set intention to accept delegation.
		assert_ok!(DelegatedStaking::register_agent(
			RawOrigin::Signed(agent).into(),
			reward_account
		));

		// create 100 delegators
		for i in 202..302 {
			fund(&i, 100 + ExistentialDeposit::get());
			assert_ok!(DelegatedStaking::delegate_to_agent(
				RawOrigin::Signed(i).into(),
				agent,
				100
			));
			// Balance of 100 held on delegator account for delegating to the agent.
			assert_eq!(Balances::balance_on_hold(&HoldReason::StakingDelegation.into(), &i), 100);
		}

		// verify
		assert!(DelegatedStaking::is_agent(&agent));
		assert_eq!(DelegatedStaking::stakeable_balance(Agent::from(agent)), 100 * 100);
	});
}

#[test]
fn agent_restrictions() {
	// Similar to creating a nomination pool
	ExtBuilder::default().build_and_execute(|| {
		let agent_one = 200;
		let delegator_one = 210;
		fund(&agent_one, 100);
		assert_ok!(DelegatedStaking::register_agent(
			RawOrigin::Signed(agent_one).into(),
			agent_one + 1
		));
		fund(&delegator_one, 200);
		assert_ok!(DelegatedStaking::delegate_to_agent(
			RawOrigin::Signed(delegator_one).into(),
			agent_one,
			100
		));

		let agent_two = 300;
		let delegator_two = 310;
		fund(&agent_two, 100);
		assert_ok!(DelegatedStaking::register_agent(
			RawOrigin::Signed(agent_two).into(),
			agent_two + 1
		));
		fund(&delegator_two, 200);
		assert_ok!(DelegatedStaking::delegate_to_agent(
			RawOrigin::Signed(delegator_two).into(),
			agent_two,
			100
		));

		// agent one tries to delegate to agent 2
		assert_noop!(
			DelegatedStaking::delegate_to_agent(RawOrigin::Signed(agent_one).into(), agent_two, 10),
			Error::<T>::InvalidDelegation
		);

		// agent one tries to delegate to a delegator
		assert_noop!(
			DelegatedStaking::delegate_to_agent(
				RawOrigin::Signed(agent_one).into(),
				delegator_one,
				10
			),
			Error::<T>::InvalidDelegation
		);
		assert_noop!(
			DelegatedStaking::delegate_to_agent(
				RawOrigin::Signed(agent_one).into(),
				delegator_two,
				10
			),
			Error::<T>::InvalidDelegation
		);

		// delegator one tries to delegate to agent 2 as well (it already delegates to agent
		// 1)
		assert_noop!(
			DelegatedStaking::delegate_to_agent(
				RawOrigin::Signed(delegator_one).into(),
				agent_two,
				10
			),
			Error::<T>::InvalidDelegation
		);

		// cannot delegate to non agents.
		let non_agent = 201;
		// give it some funds
		fund(&non_agent, 200);
		assert_noop!(
			DelegatedStaking::delegate_to_agent(
				RawOrigin::Signed(delegator_one).into(),
				non_agent,
				10
			),
			Error::<T>::InvalidDelegation
		);

		// cannot delegate to a delegator
		assert_noop!(
			DelegatedStaking::delegate_to_agent(
				RawOrigin::Signed(delegator_one).into(),
				delegator_two,
				10
			),
			Error::<T>::InvalidDelegation
		);

		// delegator cannot delegate to self
		assert_noop!(
			DelegatedStaking::delegate_to_agent(
				RawOrigin::Signed(delegator_one).into(),
				delegator_one,
				10
			),
			Error::<T>::InvalidDelegation
		);

		// agent cannot delegate to self
		assert_noop!(
			DelegatedStaking::delegate_to_agent(RawOrigin::Signed(agent_one).into(), agent_one, 10),
			Error::<T>::InvalidDelegation
		);
	});
}

#[test]
fn apply_pending_slash() {
	ExtBuilder::default().build_and_execute(|| {
		start_era(1);
		let agent: AccountId = 200;
		let reward_acc: AccountId = 201;
		let delegators: Vec<AccountId> = (301..=350).collect();
		let reporter: AccountId = 400;

		let total_staked = setup_delegation_stake(agent, reward_acc, delegators.clone(), 10, 10);

		start_era(4);
		// slash half of the stake
		pallet_staking::slashing::do_slash::<T>(
			&agent,
			total_staked / 2,
			&mut Default::default(),
			&mut Default::default(),
			3,
		);

		// agent cannot slash an account that is not its delegator.
		setup_delegation_stake(210, 211, (351..=352).collect(), 100, 0);
		assert_noop!(
			<DelegatedStaking as DelegationInterface>::delegator_slash(
				Agent::from(agent),
				Delegator::from(351),
				1,
				Some(400)
			),
			Error::<T>::NotAgent
		);
		// or a non delegator account
		fund(&353, 100);
		assert_noop!(
			<DelegatedStaking as DelegationInterface>::delegator_slash(
				Agent::from(agent),
				Delegator::from(353),
				1,
				Some(400)
			),
			Error::<T>::NotDelegator
		);

		// ensure bookkept pending slash is correct.
		assert_eq!(get_agent_ledger(&agent).ledger.pending_slash, total_staked / 2);
		let mut old_reporter_balance = Balances::free_balance(reporter);

		// lets apply the pending slash on delegators.
		for i in delegators {
			// balance before slash
			let initial_pending_slash = get_agent_ledger(&agent).ledger.pending_slash;
			assert!(initial_pending_slash > 0);
			let unslashed_balance = DelegatedStaking::held_balance_of(Delegator::from(i));
			let slash = unslashed_balance / 2;
			// slash half of delegator's delegation.
			assert_ok!(<DelegatedStaking as DelegationInterface>::delegator_slash(
				Agent::from(agent),
				Delegator::from(i),
				slash,
				Some(400)
			));

			// balance after slash.
			assert_eq!(
				DelegatedStaking::held_balance_of(Delegator::from(i)),
				unslashed_balance - slash
			);
			// pending slash is reduced by the amount slashed.
			assert_eq!(
				get_agent_ledger(&agent).ledger.pending_slash,
				initial_pending_slash - slash
			);
			// reporter get 10% of the slash amount.
			assert_eq!(
				Balances::free_balance(reporter) - old_reporter_balance,
				<Staking as StakingInterface>::slash_reward_fraction() * slash,
			);
			// update old balance
			old_reporter_balance = Balances::free_balance(reporter);
		}

		// nothing to slash anymore
		assert_eq!(get_agent_ledger(&agent).ledger.pending_slash, 0);

		// cannot slash anymore
		assert_noop!(
			<DelegatedStaking as DelegationInterface>::delegator_slash(
				Agent::from(agent),
				Delegator::from(350),
				1,
				None
			),
			Error::<T>::NothingToSlash
		);
	});
}

#[test]
fn allow_full_amount_to_be_delegated() {
	ExtBuilder::default().build_and_execute(|| {
		let agent: AccountId = 200;
		let reward_acc: AccountId = 201;
		let delegator: AccountId = 300;

		// set intention to accept delegation.
		fund(&agent, 1000);
		assert_ok!(DelegatedStaking::register_agent(RawOrigin::Signed(agent).into(), reward_acc));

		// delegate to this account
		fund(&delegator, 1000);
		assert_ok!(DelegatedStaking::delegate_to_agent(
			RawOrigin::Signed(delegator).into(),
			agent,
			1000
		));

		// verify
		assert!(DelegatedStaking::is_agent(&agent));
		assert_eq!(DelegatedStaking::stakeable_balance(Agent::from(agent)), 1000);
		assert_eq!(
			Balances::balance_on_hold(&HoldReason::StakingDelegation.into(), &delegator),
			1000
		);
		assert_eq!(DelegatedStaking::held_balance_of(Delegator::from(delegator)), 1000);
	});
}

#[test]
fn zero_amount_new_delegation_is_a_noop() {
	ExtBuilder::default().build_and_execute(|| {
		let agent: AccountId = 200;
		let reward_acc: AccountId = 201;
		let delegator: AccountId = 300;

		// set intention to accept delegation.
		fund(&agent, 1000);
		assert_ok!(DelegatedStaking::register_agent(RawOrigin::Signed(agent).into(), reward_acc));

		fund(&delegator, 1000);

		// a zero amount delegation succeeds and does nothing.
		assert_ok!(DelegatedStaking::delegate_to_agent(
			RawOrigin::Signed(delegator).into(),
			agent,
			0
		));

		// no delegation record is stored and no provider reference is taken, so the account is
		// not wrongly marked as a delegator.
		assert!(Delegators::<T>::get(delegator).is_none());
		assert!(!DelegatedStaking::is_delegator(&delegator));

		// the agent ledger is untouched.
		assert_eq!(get_agent_ledger(&agent).ledger.total_delegated, 0);

		// exactly one provider reference, from the funded balance: the no-op did not
		// `inc_providers`. A leak would have taken this to 2.
		assert_eq!(System::providers(&delegator), 1);

		// the account is left in no blocking state, so it can still register as an agent.
		assert_ok!(DelegatedStaking::register_agent(RawOrigin::Signed(delegator).into(), 301));
	});
}

#[test]
fn zero_amount_migration_is_a_noop() {
	ExtBuilder::default().build_and_execute(|| {
		let agent: AccountId = 200;
		let reward_acc: AccountId = 201;
		let delegator: AccountId = 300;

		// set intention to accept delegation.
		fund(&agent, 1000);
		assert_ok!(DelegatedStaking::register_agent(RawOrigin::Signed(agent).into(), reward_acc));

		fund(&delegator, 1000);

		// a zero amount migration succeeds and does nothing.
		assert_ok!(DelegatedStaking::migrate_delegation(
			RawOrigin::Signed(agent).into(),
			delegator,
			0
		));

		// no delegation record is stored for the destination and no provider reference is
		// taken, so it is not wrongly marked as a delegator.
		assert!(Delegators::<T>::get(delegator).is_none());
		assert!(!DelegatedStaking::is_delegator(&delegator));

		// exactly one provider reference, from the funded balance: the no-op did not
		// `inc_providers`. A leak would have taken this to 2.
		assert_eq!(System::providers(&delegator), 1);

		// the destination is left in no blocking state, so it can still register as an agent.
		assert_ok!(DelegatedStaking::register_agent(RawOrigin::Signed(delegator).into(), 301));
	});
}

#[test]
fn zero_amount_topup_on_existing_delegation_is_a_noop() {
	ExtBuilder::default().build_and_execute(|| {
		let agent: AccountId = 200;
		let reward_acc: AccountId = 201;
		let delegator: AccountId = 300;

		// set intention to accept delegation.
		fund(&agent, 1000);
		assert_ok!(DelegatedStaking::register_agent(RawOrigin::Signed(agent).into(), reward_acc));

		fund(&delegator, 1000);

		// a legitimate delegation first, so the delegator already exists in storage.
		assert_ok!(DelegatedStaking::delegate_to_agent(
			RawOrigin::Signed(delegator).into(),
			agent,
			100
		));

		// a zero amount top-up on an existing delegation is a no-op rather than an error.
		// nomination-pools depends on this: `bond_extra(BondExtra::Rewards)` bonds a claimed
		// amount of zero when there is nothing pending, e.g. at 100% commission.
		assert_ok!(DelegatedStaking::delegate_to_agent(
			RawOrigin::Signed(delegator).into(),
			agent,
			0
		));

		// the existing delegation is left untouched.
		assert_eq!(Delegators::<T>::get(delegator).unwrap().amount, 100);
		assert_eq!(get_agent_ledger(&agent).ledger.total_delegated, 100);
	});
}

/// Integration tests with pallet-staking.
mod staking_integration {
	use super::*;
	use sp_staking::Stake;

	#[test]
	fn bond() {
		ExtBuilder::default().build_and_execute(|| {
			let agent: AccountId = 99;
			let reward_acc: AccountId = 100;
			assert_eq!(Staking::status(&agent), Err(StakingError::<T>::NotStash.into()));

			// set intention to become an agent
			fund(&agent, 100);
			assert_ok!(DelegatedStaking::register_agent(
				RawOrigin::Signed(agent).into(),
				reward_acc
			));
			assert_eq!(DelegatedStaking::stakeable_balance(Agent::from(agent)), 0);

			let mut delegated_balance: Balance = 0;

			// set some delegations
			for delegator in 200..250 {
				fund(&delegator, 200);
				assert_ok!(DelegatedStaking::delegate_to_agent(
					RawOrigin::Signed(delegator).into(),
					agent,
					100
				));
				delegated_balance += 100;
				assert_eq!(
					Balances::balance_on_hold(&HoldReason::StakingDelegation.into(), &delegator),
					100
				);
				assert_eq!(
					DelegatedStaking::delegator_balance(Delegator::from(delegator)).unwrap(),
					100
				);

				let agent_obj = get_agent_ledger(&agent);
				assert_eq!(agent_obj.ledger.stakeable_balance(), delegated_balance);
				assert_eq!(agent_obj.available_to_bond(), 0);
				assert_eq!(agent_obj.bonded_stake(), delegated_balance);
			}

			assert_eq!(Staking::stake(&agent).unwrap(), Stake { total: 50 * 100, active: 50 * 100 })
		});
	}

	#[test]
	fn withdraw_test() {
		ExtBuilder::default().build_and_execute(|| {
			// initial era
			start_era(1);
			let agent: AccountId = 200;
			let reward_acc: AccountId = 201;
			let delegators: Vec<AccountId> = (301..=350).collect();
			let total_staked =
				setup_delegation_stake(agent, reward_acc, delegators.clone(), 10, 10);

			// lets go to a new era
			start_era(2);

			assert!(eq_stake(agent, total_staked, total_staked));
			// Withdrawing without unbonding would fail.
			assert_noop!(
				DelegatedStaking::release_delegation(RawOrigin::Signed(agent).into(), 301, 50, 0),
				Error::<T>::NotEnoughFunds
			);

			// 305 wants to unbond 50 in era 2, withdrawable in era 5.
			assert_ok!(Staking::unbond(RawOrigin::Signed(agent).into(), 50));

			// 310 wants to unbond 100 in era 3, withdrawable in era 6.
			start_era(3);
			assert_ok!(Staking::unbond(RawOrigin::Signed(agent).into(), 100));

			// 320 wants to unbond 200 in era 4, withdrawable in era 7.
			start_era(4);
			assert_ok!(Staking::unbond(RawOrigin::Signed(agent).into(), 200));

			// active stake is now reduced..
			let expected_active = total_staked - (50 + 100 + 200);
			assert!(eq_stake(agent, total_staked, expected_active));

			// nothing to withdraw at era 4
			assert_noop!(
				DelegatedStaking::release_delegation(RawOrigin::Signed(agent).into(), 305, 50, 0),
				Error::<T>::NotEnoughFunds
			);

			assert_eq!(get_agent_ledger(&agent).available_to_bond(), 0);
			// full amount is still delegated
			assert_eq!(get_agent_ledger(&agent).ledger.effective_balance(), total_staked);

			start_era(5);
			// at era 5, 50 tokens are withdrawable, cannot withdraw more.
			assert_noop!(
				DelegatedStaking::release_delegation(RawOrigin::Signed(agent).into(), 305, 51, 0),
				Error::<T>::NotEnoughFunds
			);
			// less is possible
			assert_ok!(DelegatedStaking::release_delegation(
				RawOrigin::Signed(agent).into(),
				305,
				30,
				0
			));
			assert_ok!(DelegatedStaking::release_delegation(
				RawOrigin::Signed(agent).into(),
				305,
				20,
				0
			));

			// Lets go to future era where everything is unbonded. Withdrawable amount: 100 + 200
			start_era(7);
			// 305 has no more amount delegated so it cannot withdraw.
			assert_noop!(
				DelegatedStaking::release_delegation(RawOrigin::Signed(agent).into(), 305, 5, 0),
				Error::<T>::NotDelegator
			);
			// 309 is an active delegator but has total delegation of 90, so it cannot withdraw more
			// than that.
			assert_noop!(
				DelegatedStaking::release_delegation(RawOrigin::Signed(agent).into(), 309, 91, 0),
				Error::<T>::NotEnoughFunds
			);
			// 310 cannot withdraw more than delegated funds.
			assert_noop!(
				DelegatedStaking::release_delegation(RawOrigin::Signed(agent).into(), 310, 101, 0),
				Error::<T>::NotEnoughFunds
			);
			// but can withdraw all its delegation amount.
			assert_ok!(DelegatedStaking::release_delegation(
				RawOrigin::Signed(agent).into(),
				310,
				100,
				0
			));
			// 320 can withdraw all its delegation amount.
			assert_ok!(DelegatedStaking::release_delegation(
				RawOrigin::Signed(agent).into(),
				320,
				200,
				0
			));

			// cannot withdraw anything more..
			assert_noop!(
				DelegatedStaking::release_delegation(RawOrigin::Signed(agent).into(), 301, 1, 0),
				Error::<T>::NotEnoughFunds
			);
			assert_noop!(
				DelegatedStaking::release_delegation(RawOrigin::Signed(agent).into(), 350, 1, 0),
				Error::<T>::NotEnoughFunds
			);
		});
	}

	#[test]
	fn withdraw_happens_with_unbonded_balance_first() {
		ExtBuilder::default().build_and_execute(|| {
			start_era(1);
			let agent = 200;
			setup_delegation_stake(agent, 201, (300..350).collect(), 320, 0);

			// verify withdraw not possible yet
			assert_noop!(
				DelegatedStaking::release_delegation(RawOrigin::Signed(agent).into(), 300, 320, 0),
				Error::<T>::NotEnoughFunds
			);

			// fill up unlocking chunks in core staking.
			// 32 is the max chunks
			for i in 2..=33 {
				start_era(i);
				assert_ok!(Staking::unbond(RawOrigin::Signed(agent).into(), 10));
				// no withdrawals from core staking yet.
				assert_eq!(get_agent_ledger(&agent).ledger.unclaimed_withdrawals, 0);
			}

			// another unbond would trigger withdrawal
			start_era(34);
			assert_ok!(Staking::unbond(RawOrigin::Signed(agent).into(), 10));

			// 30 previous unbonds would be withdrawn as they were already unlocked. Unlocking
			// period is 3 eras.
			assert_eq!(get_agent_ledger(&agent).ledger.unclaimed_withdrawals, 30 * 10);

			// release some delegation now.
			assert_ok!(DelegatedStaking::release_delegation(
				RawOrigin::Signed(agent).into(),
				300,
				160,
				0
			));
			assert_eq!(get_agent_ledger(&agent).ledger.unclaimed_withdrawals, 300 - 160);

			// cannot release more than available
			assert_noop!(
				DelegatedStaking::release_delegation(RawOrigin::Signed(agent).into(), 300, 141, 0),
				Error::<T>::NotEnoughFunds
			);
			assert_ok!(DelegatedStaking::release_delegation(
				RawOrigin::Signed(agent).into(),
				300,
				140,
				0
			));

			assert_eq!(DelegatedStaking::held_balance_of(Delegator::from(300)), 320 - 300);
		});
	}

	#[test]
	fn reward_destination_restrictions() {
		ExtBuilder::default().build_and_execute(|| {
			// give some funds to 200
			fund(&200, 1000);
			let balance_200 = Balances::free_balance(200);

			// `Agent` account cannot be reward destination
			assert_noop!(
				DelegatedStaking::register_agent(RawOrigin::Signed(200).into(), 200),
				Error::<T>::InvalidRewardDestination
			);

			// different reward account works
			assert_ok!(DelegatedStaking::register_agent(RawOrigin::Signed(200).into(), 201));
			// add some delegations to it
			fund(&300, 1000);
			assert_ok!(DelegatedStaking::delegate_to_agent(
				RawOrigin::Signed(300).into(),
				200,
				100
			));

			// set_payee to self fails.
			assert_noop!(
				<Staking as StakingInterface>::set_payee(&200, &200),
				StakingError::<T>::RewardDestinationRestricted
			);

			// passing correct reward destination works
			assert_ok!(<Staking as StakingInterface>::set_payee(&200, &201));

			// amount is staked correctly
			assert!(eq_stake(200, 100, 100));
			assert_eq!(get_agent_ledger(&200).available_to_bond(), 0);
			assert_eq!(get_agent_ledger(&200).ledger.effective_balance(), 100);

			// free balance of delegate is untouched
			assert_eq!(Balances::free_balance(200), balance_200);
		});
	}

	#[test]
	fn agent_restrictions() {
		ExtBuilder::default().build_and_execute(|| {
			setup_delegation_stake(200, 201, (202..203).collect(), 100, 0);

			// Registering again is noop
			assert_noop!(
				DelegatedStaking::register_agent(RawOrigin::Signed(200).into(), 201),
				Error::<T>::NotAllowed
			);
			// a delegator cannot become delegate
			assert_noop!(
				DelegatedStaking::register_agent(RawOrigin::Signed(202).into(), 203),
				Error::<T>::NotAllowed
			);
		});
	}

	#[test]
	fn migration_works() {
		ExtBuilder::default().build_and_execute(|| {
			// initial era
			start_era(1);

			// add a nominator
			let staked_amount = 4000;
			let agent_amount = 5000;
			let agent = 200;
			fund(&agent, agent_amount);

			assert_ok!(Staking::bond(
				RuntimeOrigin::signed(agent),
				staked_amount,
				RewardDestination::Account(201)
			));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(agent), vec![GENESIS_VALIDATOR],));
			let init_stake = Staking::stake(&agent).unwrap();
			// no extra provider added.
			assert_eq!(System::providers(&agent), 1);

			// scenario: 200 is a pool account, and the stake comes from its 4 delegators (300..304)
			// in equal parts. lets try to migrate this nominator into delegate based stake.

			// all balance currently is in 200
			assert_eq!(pallet_staking::asset::total_balance::<T>(&agent), agent_amount);

			// to migrate, nominator needs to set an account as a proxy delegator where staked funds
			// will be moved and delegated back to this old nominator account. This should be funded
			// with at least ED.
			let proxy_delegator =
				DelegatedStaking::generate_proxy_delegator(Agent::from(agent)).get();

			assert_ok!(DelegatedStaking::migrate_to_agent(RawOrigin::Signed(agent).into(), 201));
			// after migration, no provider left since free balance is 0 and staking pallet released
			// all funds.
			assert_eq!(System::providers(&agent), 0);
			assert_eq!(Balances::free_balance(agent), 0);
			// proxy delegator has one provider as well with no free balance.
			assert_eq!(System::providers(&proxy_delegator), 1);
			assert_eq!(Balances::free_balance(proxy_delegator), 0);

			// verify all went well
			let mut expected_proxy_delegated_amount = agent_amount;
			assert_eq!(
				Balances::balance_on_hold(&HoldReason::StakingDelegation.into(), &proxy_delegator),
				expected_proxy_delegated_amount
			);
			// stake amount is transferred from delegate to proxy delegator account.
			assert_eq!(Balances::free_balance(agent), 0);
			assert_eq!(Staking::stake(&agent).unwrap(), init_stake);
			assert_eq!(get_agent_ledger(&agent).ledger.effective_balance(), agent_amount);
			assert_eq!(get_agent_ledger(&agent).available_to_bond(), 0);
			assert_eq!(
				get_agent_ledger(&agent).ledger.unclaimed_withdrawals,
				agent_amount - staked_amount
			);

			// now lets migrate the delegators
			let delegator_share = agent_amount / 4;
			for delegator in 300..304 {
				assert_eq!(Balances::free_balance(delegator), 0);
				assert_eq!(System::providers(&delegator), 0);

				// No pre-balance needed to migrate delegator.
				assert_ok!(DelegatedStaking::migrate_delegation(
					RawOrigin::Signed(agent).into(),
					delegator,
					delegator_share
				));
				assert_eq!(System::providers(&delegator), 1);
				assert_eq!(
					Balances::balance_on_hold(&HoldReason::StakingDelegation.into(), &delegator),
					delegator_share
				);
				expected_proxy_delegated_amount -= delegator_share;
				assert_eq!(
					Balances::balance_on_hold(
						&HoldReason::StakingDelegation.into(),
						&proxy_delegator
					),
					expected_proxy_delegated_amount
				);

				// delegate stake is unchanged.
				assert_eq!(Staking::stake(&agent).unwrap(), init_stake);
				assert_eq!(get_agent_ledger(&agent).ledger.effective_balance(), agent_amount);
				assert_eq!(get_agent_ledger(&agent).available_to_bond(), 0);
				assert_eq!(
					get_agent_ledger(&agent).ledger.unclaimed_withdrawals,
					agent_amount - staked_amount
				);
			}

			// cannot use migrate delegator anymore
			assert_noop!(
				DelegatedStaking::migrate_delegation(RawOrigin::Signed(agent).into(), 305, 1),
				Error::<T>::NotEnoughFunds
			);

			// no provider left on proxy delegator since all funds are migrated
			assert_eq!(System::providers(&proxy_delegator), 0);

			// withdraw all delegations from delegators
			assert_ok!(Staking::chill(RuntimeOrigin::signed(agent)));
			assert_ok!(Staking::unbond(RawOrigin::Signed(agent).into(), staked_amount));
			start_era(4);
			assert_ok!(Staking::withdraw_unbonded(RawOrigin::Signed(agent).into(), 0));
			for delegator in 300..304 {
				assert_ok!(DelegatedStaking::release_delegation(
					RawOrigin::Signed(agent).into(),
					delegator,
					delegator_share,
					0
				));
				// delegator is cleaned up from storage.
				assert!(!Delegators::<T>::contains_key(delegator));
				// has free balance now
				assert_eq!(Balances::free_balance(delegator), delegator_share);
				// and only one provider as delegator_share > ED
				assert_eq!(System::providers(&delegator), 1);
			}

			// Agent can be removed now.
			assert_ok!(DelegatedStaking::remove_agent(RawOrigin::Signed(agent).into()));
			// agent is correctly removed.
			assert!(!Agents::<T>::contains_key(agent));
			// and no provider left.
			assert_eq!(System::providers(&agent), 0);
		});
	}

	#[test]
	fn accounts_are_cleaned_up() {
		ExtBuilder::default().build_and_execute(|| {
			let agent: AccountId = 200;
			let reward_acc: AccountId = 201;
			let delegator: AccountId = 300;

			// set intention to accept delegation.
			fund(&agent, 1000);

			// Agent is provided since it has balance > ED.
			assert_eq!(System::providers(&agent), 1);
			assert_ok!(DelegatedStaking::register_agent(
				RawOrigin::Signed(agent).into(),
				reward_acc
			));

			// delegate to this account
			fund(&delegator, 1000);
			// account has one provider since its funded.
			assert_eq!(System::providers(&delegator), 1);
			assert_ok!(DelegatedStaking::delegate_to_agent(
				RawOrigin::Signed(delegator).into(),
				agent,
				500
			));
			// delegator has an extra provider now.
			assert_eq!(System::providers(&delegator), 2);
			// all 1000 tokens including ED can be held.
			assert_ok!(DelegatedStaking::delegate_to_agent(
				RawOrigin::Signed(delegator).into(),
				agent,
				500
			));
			// free balance dropping below ED will reduce a provider, but it still has one provider
			// left.
			assert_eq!(System::providers(&delegator), 1);

			// withdraw all delegation
			assert_ok!(Staking::unbond(RawOrigin::Signed(agent).into(), 1000));
			start_era(4);
			assert_ok!(Staking::withdraw_unbonded(RawOrigin::Signed(agent).into(), 0));

			// Since delegations are still left, agents cannot be removed yet from storage.
			assert_noop!(
				DelegatedStaking::remove_agent(RawOrigin::Signed(agent).into()),
				Error::<T>::NotAllowed
			);

			assert_ok!(DelegatedStaking::release_delegation(
				RawOrigin::Signed(agent).into(),
				delegator,
				1000,
				0
			));

			// now agents can be removed.
			assert_ok!(DelegatedStaking::remove_agent(RawOrigin::Signed(agent).into()));

			// agent and delegator provider is decremented.
			assert_eq!(System::providers(&delegator), 1);
			assert_eq!(System::providers(&agent), 1);

			// if we transfer all funds, providers are removed.
			assert_ok!(Balances::transfer_all(RawOrigin::Signed(delegator).into(), 1337, false));
			assert_ok!(Balances::transfer_all(RawOrigin::Signed(agent).into(), 1337, false));
			assert_eq!(System::providers(&delegator), 0);
			assert_eq!(System::providers(&agent), 0);
		});
	}
}

mod pool_integration {
	use super::*;
	use pallet_nomination_pools::{BondExtra, BondedPools, PoolState};

	#[test]
	fn create_pool_test() {
		ExtBuilder::default().build_and_execute(|| {
			let creator: AccountId = 100;
			fund(&creator, 500);
			let delegate_amount = 200;

			// nothing held initially
			assert_eq!(DelegatedStaking::held_balance_of(Delegator::from(creator)), 0);

			// create pool
			assert_ok!(Pools::create(
				RawOrigin::Signed(creator).into(),
				delegate_amount,
				creator,
				creator,
				creator
			));

			// correct amount is locked in depositor's account.
			assert_eq!(
				DelegatedStaking::held_balance_of(Delegator::from(creator)),
				delegate_amount
			);

			let pool_account = Pools::generate_bonded_account(1);
			let agent = get_agent_ledger(&pool_account);

			// verify state
			assert_eq!(agent.ledger.effective_balance(), delegate_amount);
			assert_eq!(agent.available_to_bond(), 0);
			assert_eq!(agent.total_unbonded(), 0);
		});
	}

	#[test]
	fn join_pool() {
		ExtBuilder::default().build_and_execute(|| {
			// create a pool
			let pool_id = create_pool(100, 200);
			// keep track of staked amount.
			let mut staked_amount: Balance = 200;

			// fund delegator
			let delegator: AccountId = 300;
			fund(&delegator, 500);
			// nothing held initially
			assert_eq!(DelegatedStaking::held_balance_of(Delegator::from(delegator)), 0);

			// delegator joins pool
			assert_ok!(Pools::join(RawOrigin::Signed(delegator).into(), 100, pool_id));
			staked_amount += 100;

			// correct amount is locked in depositor's account.
			assert_eq!(DelegatedStaking::held_balance_of(Delegator::from(delegator)), 100);

			// delegator is not actively exposed to core staking.
			assert_eq!(Staking::status(&delegator), Err(StakingError::<T>::NotStash.into()));

			let pool_agent = get_agent_ledger(&Pools::generate_bonded_account(1));
			// verify state
			assert_eq!(pool_agent.ledger.effective_balance(), staked_amount);
			assert_eq!(pool_agent.bonded_stake(), staked_amount);
			assert_eq!(pool_agent.available_to_bond(), 0);
			assert_eq!(pool_agent.total_unbonded(), 0);

			// cannot reap agent in staking.
			assert_noop!(
				Staking::reap_stash(RuntimeOrigin::signed(100), pool_agent.key, 0),
				StakingError::<T>::VirtualStakerNotAllowed
			);

			// let a bunch of delegators join this pool
			for i in 301..350 {
				fund(&i, 500);
				assert_ok!(Pools::join(RawOrigin::Signed(i).into(), 100 + i, pool_id));
				staked_amount += 100 + i;
				assert_eq!(DelegatedStaking::held_balance_of(Delegator::from(i)), 100 + i);
			}

			let pool_agent = pool_agent.reload().unwrap();
			assert_eq!(pool_agent.ledger.effective_balance(), staked_amount);
			assert_eq!(pool_agent.bonded_stake(), staked_amount);
			assert_eq!(pool_agent.available_to_bond(), 0);
			assert_eq!(pool_agent.total_unbonded(), 0);
		});
	}

	#[test]
	fn bond_extra_to_pool() {
		ExtBuilder::default().build_and_execute(|| {
			let pool_id = create_pool(100, 200);
			add_delegators_to_pool(pool_id, (300..310).collect(), 100);
			let mut staked_amount = 200 + 100 * 10;
			assert_eq!(get_pool_agent(pool_id).bonded_stake(), staked_amount);

			// bond extra to pool
			for i in 300..310 {
				assert_ok!(Pools::bond_extra(
					RawOrigin::Signed(i).into(),
					BondExtra::FreeBalance(50)
				));
				staked_amount += 50;
				assert_eq!(get_pool_agent(pool_id).bonded_stake(), staked_amount);
			}
		});
	}

	#[test]
	fn claim_pool_rewards() {
		ExtBuilder::default().build_and_execute(|| {
			let creator = 100;
			let creator_stake = 1000;
			let pool_id = create_pool(creator, creator_stake);
			add_delegators_to_pool(pool_id, (300..310).collect(), 100);
			add_delegators_to_pool(pool_id, (310..320).collect(), 200);
			let total_staked = creator_stake + 100 * 10 + 200 * 10;

			// give some rewards
			let reward_acc = Pools::generate_reward_account(pool_id);
			let reward_amount = 1000;
			fund(&reward_acc, reward_amount);

			// claim rewards
			for i in 300..320 {
				let pre_balance = Balances::free_balance(i);
				let delegator_staked_balance =
					DelegatedStaking::held_balance_of(Delegator::from(i));
				// payout reward
				assert_ok!(Pools::claim_payout(RawOrigin::Signed(i).into()));

				let reward = Balances::free_balance(i) - pre_balance;
				assert_eq!(reward, delegator_staked_balance * reward_amount / total_staked);
			}

			// payout creator
			let pre_balance = Balances::free_balance(creator);
			assert_ok!(Pools::claim_payout(RawOrigin::Signed(creator).into()));
			// verify they are paid out correctly
			let reward = Balances::free_balance(creator) - pre_balance;
			assert_eq!(reward, creator_stake * reward_amount / total_staked);

			// reward account should only have left minimum balance after paying out everyone.
			assert_eq!(Balances::free_balance(reward_acc), ExistentialDeposit::get());
		});
	}

	#[test]
	fn withdraw_from_pool() {
		ExtBuilder::default().build_and_execute(|| {
			// initial era
			start_era(1);

			let pool_id = create_pool(100, 1000);
			let bond_amount = 200;
			add_delegators_to_pool(pool_id, (300..310).collect(), bond_amount);
			let total_staked = 1000 + bond_amount * 10;
			let pool_acc = Pools::generate_bonded_account(pool_id);

			start_era(2);
			// nothing to release yet.
			assert_noop!(
				Pools::withdraw_unbonded(RawOrigin::Signed(301).into(), 301, 0),
				PoolsError::<T>::SubPoolsNotFound
			);

			// 301 wants to unbond 50 in era 2, withdrawable in era 5.
			assert_ok!(Pools::unbond(RawOrigin::Signed(301).into(), 301, 50));

			// 302 wants to unbond 100 in era 3, withdrawable in era 6.
			start_era(3);
			assert_ok!(Pools::unbond(RawOrigin::Signed(302).into(), 302, 100));

			// 303 wants to unbond 200 in era 4, withdrawable in era 7.
			start_era(4);
			assert_ok!(Pools::unbond(RawOrigin::Signed(303).into(), 303, 200));

			// active stake is now reduced..
			let expected_active = total_staked - (50 + 100 + 200);
			assert!(eq_stake(pool_acc, total_staked, expected_active));

			// nothing to withdraw at era 4
			for i in 301..310 {
				assert_noop!(
					Pools::withdraw_unbonded(RawOrigin::Signed(i).into(), i, 0),
					PoolsError::<T>::CannotWithdrawAny
				);
			}

			assert!(eq_stake(pool_acc, total_staked, expected_active));

			start_era(5);
			// at era 5, 301 can withdraw.

			System::reset_events();
			let held_301 = DelegatedStaking::held_balance_of(Delegator::from(301));
			let free_301 = Balances::free_balance(301);

			assert_ok!(Pools::withdraw_unbonded(RawOrigin::Signed(301).into(), 301, 0));
			assert_eq!(
				events_since_last_call(),
				vec![Event::Released { agent: pool_acc, delegator: 301, amount: 50 }]
			);
			assert_eq!(
				pool_events_since_last_call(),
				vec![PoolsEvent::Withdrawn { member: 301, pool_id, balance: 50, points: 50 }]
			);
			assert_eq!(DelegatedStaking::held_balance_of(Delegator::from(301)), held_301 - 50);
			assert_eq!(Balances::free_balance(301), free_301 + 50);

			start_era(7);
			// era 7 both delegators can withdraw
			assert_ok!(Pools::withdraw_unbonded(RawOrigin::Signed(302).into(), 302, 0));
			assert_ok!(Pools::withdraw_unbonded(RawOrigin::Signed(303).into(), 303, 0));

			assert_eq!(
				events_since_last_call(),
				vec![
					Event::Released { agent: pool_acc, delegator: 302, amount: 100 },
					Event::Released { agent: pool_acc, delegator: 303, amount: 200 },
				]
			);
			assert_eq!(
				pool_events_since_last_call(),
				vec![
					PoolsEvent::Withdrawn { member: 302, pool_id, balance: 100, points: 100 },
					PoolsEvent::Withdrawn { member: 303, pool_id, balance: 200, points: 200 },
					PoolsEvent::MemberRemoved { pool_id: 1, member: 303, released_balance: 0 },
				]
			);

			// 303 is killed
			assert!(!Delegators::<T>::contains_key(303));
		});
	}

	#[test]
	fn pool_withdraw_unbonded() {
		ExtBuilder::default().build_and_execute(|| {
			// initial era
			start_era(1);
			let pool_id = create_pool(100, 1000);
			add_delegators_to_pool(pool_id, (300..310).collect(), 200);

			start_era(2);
			// 1000 tokens to be unbonded in era 5.
			for i in 300..310 {
				assert_ok!(Pools::unbond(RawOrigin::Signed(i).into(), i, 100));
			}

			start_era(3);
			// 500 tokens to be unbonded in era 6.
			for i in 300..310 {
				assert_ok!(Pools::unbond(RawOrigin::Signed(i).into(), i, 50));
			}

			start_era(5);
			// withdraw pool should withdraw 1000 tokens
			assert_ok!(Pools::pool_withdraw_unbonded(RawOrigin::Signed(100).into(), pool_id, 0));
			assert_eq!(get_pool_agent(pool_id).total_unbonded(), 1000);

			start_era(6);
			// should withdraw 500 more
			assert_ok!(Pools::pool_withdraw_unbonded(RawOrigin::Signed(100).into(), pool_id, 0));
			assert_eq!(get_pool_agent(pool_id).total_unbonded(), 1000 + 500);

			start_era(7);
			// Nothing to withdraw, still at 1500.
			assert_ok!(Pools::pool_withdraw_unbonded(RawOrigin::Signed(100).into(), pool_id, 0));
			assert_eq!(get_pool_agent(pool_id).total_unbonded(), 1500);
		});
	}

	#[test]
	fn update_nominations() {
		ExtBuilder::default().build_and_execute(|| {
			start_era(1);
			// can't nominate for non-existent pool
			assert_noop!(
				Pools::nominate(RawOrigin::Signed(100).into(), 1, vec![99]),
				PoolsError::<T>::PoolNotFound
			);

			let pool_id = create_pool(100, 1000);
			let pool_acc = Pools::generate_bonded_account(pool_id);
			assert_ok!(Pools::nominate(RawOrigin::Signed(100).into(), 1, vec![20, 21, 22]));
			assert!(Staking::status(&pool_acc) == Ok(StakerStatus::Nominator(vec![20, 21, 22])));

			start_era(3);
			assert_ok!(Pools::nominate(RawOrigin::Signed(100).into(), 1, vec![18, 19, 22]));
			assert!(Staking::status(&pool_acc) == Ok(StakerStatus::Nominator(vec![18, 19, 22])));
		});
	}

	#[test]
	fn destroy_pool() {
		ExtBuilder::default().build_and_execute(|| {
			start_era(1);
			let creator = 100;
			let creator_stake = 1000;
			let pool_id = create_pool(creator, creator_stake);
			add_delegators_to_pool(pool_id, (300..310).collect(), 200);

			start_era(3);
			// lets destroy the pool
			assert_ok!(Pools::set_state(
				RawOrigin::Signed(creator).into(),
				pool_id,
				PoolState::Destroying
			));
			assert_ok!(Pools::chill(RawOrigin::Signed(creator).into(), pool_id));

			// unbond all members by the creator/admin
			for i in 300..310 {
				assert_ok!(Pools::unbond(RawOrigin::Signed(creator).into(), i, 200));
			}

			start_era(6);
			// withdraw all members by the creator/admin
			for i in 300..310 {
				assert_ok!(Pools::withdraw_unbonded(RawOrigin::Signed(creator).into(), i, 0));
			}

			// unbond creator
			assert_ok!(Pools::unbond(RawOrigin::Signed(creator).into(), creator, creator_stake));

			start_era(9);
			System::reset_events();
			// Withdraw self
			assert_ok!(Pools::withdraw_unbonded(RawOrigin::Signed(creator).into(), creator, 0));
			assert_eq!(
				pool_events_since_last_call(),
				vec![
					PoolsEvent::Withdrawn {
						member: creator,
						pool_id,
						balance: creator_stake,
						points: creator_stake,
					},
					PoolsEvent::MemberRemoved { pool_id, member: creator, released_balance: 0 },
					PoolsEvent::Destroyed { pool_id },
				]
			);

			// Make sure all data is cleaned up.
			assert!(!Agents::<T>::contains_key(Pools::generate_bonded_account(pool_id)));
			assert!(!System::account_exists(&Pools::generate_bonded_account(pool_id)));
			assert!(!Delegators::<T>::contains_key(creator));
			for i in 300..310 {
				assert!(!Delegators::<T>::contains_key(i));
			}
		});
	}

	#[test]
	fn pool_partially_slashed() {
		ExtBuilder::default().build_and_execute(|| {
			start_era(1);
			let creator = 100;
			let creator_stake = 500;
			let pool_id = create_pool(creator, creator_stake);
			let delegator_stake = 100;
			add_delegators_to_pool(pool_id, (300..306).collect(), delegator_stake);
			let pool_acc = Pools::generate_bonded_account(pool_id);

			let total_staked = creator_stake + delegator_stake * 6;
			assert_eq!(Staking::stake(&pool_acc).unwrap().total, total_staked);

			// lets unbond a delegator each in next eras (2, 3, 4).
			start_era(2);
			assert_ok!(Pools::unbond(RawOrigin::Signed(300).into(), 300, delegator_stake));

			start_era(3);
			assert_ok!(Pools::unbond(RawOrigin::Signed(301).into(), 301, delegator_stake));

			start_era(4);
			assert_ok!(Pools::unbond(RawOrigin::Signed(302).into(), 302, delegator_stake));
			System::reset_events();

			// slash the pool at era 3
			assert_eq!(
				BondedPools::<T>::get(1).unwrap().points,
				creator_stake + delegator_stake * 6 - delegator_stake * 3
			);

			// pool has currently no pending slash
			assert_eq!(Pools::api_pool_pending_slash(pool_id), 0);

			// slash the pool partially
			pallet_staking::slashing::do_slash::<T>(
				&pool_acc,
				500,
				&mut Default::default(),
				&mut Default::default(),
				3,
			);

			// pool has now pending slash of 500.
			assert_eq!(Pools::api_pool_pending_slash(pool_id), 500);

			assert_eq!(
				pool_events_since_last_call(),
				vec![
					// 300 did not get slashed as all as it unbonded in an era before slash.
					// 301 got slashed 50% of 100 = 50.
					PoolsEvent::UnbondingPoolSlashed { pool_id: 1, era: 6, balance: 50 },
					// 302 got slashed 50% of 100 = 50.
					PoolsEvent::UnbondingPoolSlashed { pool_id: 1, era: 7, balance: 50 },
					// Rest of the pool slashed 50% of 800 = 400.
					PoolsEvent::PoolSlashed { pool_id: 1, balance: 400 },
				]
			);

			// slash is lazy and balance is still locked in user's accounts.
			assert_eq!(DelegatedStaking::held_balance_of(Delegator::from(creator)), creator_stake);
			for i in 300..306 {
				assert_eq!(DelegatedStaking::held_balance_of(Delegator::from(i)), delegator_stake);
			}
			assert_eq!(
				get_pool_agent(pool_id).ledger.effective_balance(),
				Staking::total_stake(&pool_acc).unwrap()
			);

			// pending slash is book kept.
			assert_eq!(get_pool_agent(pool_id).ledger.pending_slash, 500);

			// go in some distant future era.
			start_era(10);
			System::reset_events();

			// 300 is not slashed and can withdraw all balance.
			assert_ok!(Pools::withdraw_unbonded(RawOrigin::Signed(300).into(), 300, 1));
			assert_eq!(
				events_since_last_call(),
				vec![Event::Released { agent: pool_acc, delegator: 300, amount: 100 }]
			);
			assert_eq!(get_pool_agent(pool_id).ledger.pending_slash, 500);

			// withdraw the other two delegators (301 and 302) who were unbonding.
			for i in 301..=302 {
				let pre_balance = Balances::free_balance(i);
				let pre_pending_slash = get_pool_agent(pool_id).ledger.pending_slash;
				assert_ok!(Pools::withdraw_unbonded(RawOrigin::Signed(i).into(), i, 0));
				assert_eq!(
					events_since_last_call(),
					vec![
						Event::Slashed { agent: pool_acc, delegator: i, amount: 50 },
						Event::Released { agent: pool_acc, delegator: i, amount: 50 },
					]
				);
				assert_eq!(get_pool_agent(pool_id).ledger.pending_slash, pre_pending_slash - 50);
				assert_eq!(DelegatedStaking::held_balance_of(Delegator::from(i)), 0);
				assert_eq!(Balances::free_balance(i) - pre_balance, 50);
			}

			// let's update all the slash
			let slash_reporter = 99;
			// give our reporter some balance.
			fund(&slash_reporter, 100);

			for i in 303..306 {
				let pre_pending_slash = get_pool_agent(pool_id).ledger.pending_slash;
				// pool api returns correct pending slash.
				assert_eq!(Pools::api_pool_pending_slash(pool_id), pre_pending_slash);
				// delegator has pending slash of 50.
				assert_eq!(Pools::api_member_pending_slash(i), 50);
				// apply slash
				assert_ok!(Pools::apply_slash(RawOrigin::Signed(slash_reporter).into(), i));
				// nothing pending anymore.
				assert_eq!(Pools::api_member_pending_slash(i), 0);

				// each member is slashed 50% of 100 = 50.
				assert_eq!(get_pool_agent(pool_id).ledger.pending_slash, pre_pending_slash - 50);
				// pool api returns correctly as well.
				assert_eq!(Pools::api_pool_pending_slash(pool_id), pre_pending_slash - 50);
				// left with 50.
				assert_eq!(DelegatedStaking::held_balance_of(Delegator::from(i)), 50);
			}

			// pool has still pending slash of creator.
			assert_eq!(Pools::api_pool_pending_slash(pool_id), 250);

			// reporter is paid SlashRewardFraction of the slash, i.e. 10% of 50 = 5
			assert_eq!(Balances::free_balance(slash_reporter), 100 + 5 * 3);
			// creator has pending slash.
			assert_eq!(Pools::api_member_pending_slash(creator), 250);
			// slash creator
			assert_ok!(Pools::apply_slash(RawOrigin::Signed(slash_reporter).into(), creator));
			// no pending slash anymore.
			assert_eq!(Pools::api_member_pending_slash(creator), 0);

			// all slash should be applied now.
			assert_eq!(get_pool_agent(pool_id).ledger.pending_slash, 0);
			assert_eq!(Pools::api_pool_pending_slash(pool_id), 0);
			// for creator, 50% of stake should be slashed (250), 10% of which should go to reporter
			// (25).
			assert_eq!(Balances::free_balance(slash_reporter), 115 + 25);
		});
	}

	#[test]
	fn existing_pool_member_cannot_stake() {
		// A pool member is able to stake directly since staking only uses free funds but once a
		// staker, they cannot join/add extra bond to the pool. They can still withdraw funds.
		ExtBuilder::default().build_and_execute(|| {
			start_era(1);
			// GIVEN: a pool.
			fund(&200, 1000);
			let pool_id = create_pool(200, 800);

			// WHEN: delegator joins a pool
			let delegator = 100;
			fund(&delegator, 1000);
			assert_ok!(Pools::join(RawOrigin::Signed(delegator).into(), 200, pool_id));

			// THEN: they cannot stake anymore
			assert_noop!(
				Staking::bond(
					RuntimeOrigin::signed(delegator),
					500,
					RewardDestination::Account(101)
				),
				StakingError::<T>::Restricted
			);
		});
	}

	#[test]
	fn stakers_cannot_join_pool() {
		ExtBuilder::default().build_and_execute(|| {
			start_era(1);
			// GIVEN: a pool.
			fund(&200, 1000);
			let pool_id = create_pool(200, 800);

			// WHEN: an account is a staker.
			let staker = 100;
			fund(&staker, 1000);

			assert_ok!(Staking::bond(
				RuntimeOrigin::signed(staker),
				500,
				RewardDestination::Account(101)
			));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(staker), vec![GENESIS_VALIDATOR]));

			// THEN: they cannot join pool.
			assert_noop!(
				Pools::join(RawOrigin::Signed(staker).into(), 200, pool_id),
				PoolsError::<T>::Restricted
			);
		});
	}

	fn create_pool(creator: AccountId, amount: Balance) -> u32 {
		fund(&creator, amount * 2);
		assert_ok!(Pools::create(
			RawOrigin::Signed(creator).into(),
			amount,
			creator,
			creator,
			creator
		));

		pallet_nomination_pools::LastPoolId::<T>::get()
	}

	fn add_delegators_to_pool(pool_id: u32, delegators: Vec<AccountId>, amount: Balance) {
		for delegator in delegators {
			fund(&delegator, amount * 2);
			assert_ok!(Pools::join(RawOrigin::Signed(delegator).into(), amount, pool_id));
		}
	}

	fn get_pool_agent(pool_id: u32) -> AgentLedgerOuter<T> {
		get_agent_ledger(&Pools::generate_bonded_account(pool_id))
	}
}
