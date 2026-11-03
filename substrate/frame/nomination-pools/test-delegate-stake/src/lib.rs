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

#![cfg(test)]

// We do not declare all features used by `construct_runtime`
#[allow(unexpected_cfgs)]
mod mock;

use frame_support::{
	assert_noop, assert_ok, hypothetically,
	traits::{
		fungible::{InspectHold, Mutate},
		Currency,
	},
};
use mock::*;
use pallet_nomination_pools::{
	BondExtra, BondedPools, CommissionChangeRate, ConfigOp, Error as PoolsError,
	Event as PoolsEvent, LastPoolId, PoolMember, PoolMembers, PoolState,
};
use pallet_staking_async::{
	AreNominatorsSlashable, Error as StakingError, Event as StakingEvent, Payee, RewardDestination,
};

use pallet_delegated_staking::Event as DelegatedStakingEvent;

use sp_runtime::{bounded_btree_map, traits::Zero, Perbill};
use sp_staking::{Agent, StakingInterface};

/// Helper to assert the current active era.
fn assert_active_era(expected: u32) {
	assert_eq!(pallet_staking_async::ActiveEra::<Runtime>::get().map(|e| e.index), Some(expected));
}

#[test]
fn pool_lifecycle_e2e() {
	new_test_ext().execute_with(|| {
		assert_eq!(Balances::minimum_balance(), 5);
		assert_active_era(0);

		// create the pool, we know this has id 1.
		assert_ok!(Pools::create(RuntimeOrigin::signed(10), 50, 10, 10, 10));
		assert_eq!(LastPoolId::<Runtime>::get(), 1);

		// have the pool nominate.
		assert_ok!(Pools::nominate(RuntimeOrigin::signed(10), 1, vec![1, 2, 3]));

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Bonded { stash: POOL1_BONDED, amount: 50 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Created { depositor: 10, pool_id: 1 },
				PoolsEvent::Bonded { member: 10, pool_id: 1, bonded: 50, joined: true },
				PoolsEvent::PoolNominationMade { pool_id: 1, caller: 10 },
			]
		);

		// have two members join
		assert_ok!(Pools::join(RuntimeOrigin::signed(20), 10, 1));
		assert_ok!(Pools::join(RuntimeOrigin::signed(21), 10, 1));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 10 },
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 10 },
			]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Bonded { member: 20, pool_id: 1, bonded: 10, joined: true },
				PoolsEvent::Bonded { member: 21, pool_id: 1, bonded: 10, joined: true },
			]
		);

		// pool goes into destroying
		assert_ok!(Pools::set_state(RuntimeOrigin::signed(10), 1, PoolState::Destroying));

		// depositor cannot unbond yet.
		assert_noop!(
			Pools::unbond(RuntimeOrigin::signed(10), 10, 50),
			PoolsError::<Runtime>::MinimumBondNotMet,
		);

		// now the members want to unbond.
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(20), 20, 10));
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(21), 21, 10));

		assert_eq!(PoolMembers::<Runtime>::get(20).unwrap().unbonding_eras.len(), 1);
		assert_eq!(PoolMembers::<Runtime>::get(20).unwrap().points, 0);
		assert_eq!(PoolMembers::<Runtime>::get(21).unwrap().unbonding_eras.len(), 1);
		assert_eq!(PoolMembers::<Runtime>::get(21).unwrap().points, 0);

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 10, era: 3 },
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 10, era: 3 },
			]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::StateChanged { pool_id: 1, new_state: PoolState::Destroying },
				PoolsEvent::Unbonded { member: 20, pool_id: 1, points: 10, balance: 10, era: 3 },
				PoolsEvent::Unbonded { member: 21, pool_id: 1, points: 10, balance: 10, era: 3 },
			]
		);

		// depositor cannot still unbond
		assert_noop!(
			Pools::unbond(RuntimeOrigin::signed(10), 10, 50),
			PoolsError::<Runtime>::MinimumBondNotMet,
		);

		for e in 1..BondingDuration::get() {
			Staking::set_era(e);
			assert_noop!(
				Pools::withdraw_unbonded(RuntimeOrigin::signed(20), 20, 0),
				PoolsError::<Runtime>::CannotWithdrawAny
			);
		}

		// members are now unlocked.
		Staking::set_era(BondingDuration::get());

		// depositor cannot still unbond
		assert_noop!(
			Pools::unbond(RuntimeOrigin::signed(10), 10, 50),
			PoolsError::<Runtime>::MinimumBondNotMet,
		);

		// but members can now withdraw.
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(20), 20, 0));
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(21), 21, 0));
		assert!(PoolMembers::<Runtime>::get(20).is_none());
		assert!(PoolMembers::<Runtime>::get(21).is_none());

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Withdrawn { stash: POOL1_BONDED, amount: 20 },]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Withdrawn { member: 20, pool_id: 1, points: 10, balance: 10 },
				PoolsEvent::MemberRemoved { pool_id: 1, member: 20, released_balance: 0 },
				PoolsEvent::Withdrawn { member: 21, pool_id: 1, points: 10, balance: 10 },
				PoolsEvent::MemberRemoved { pool_id: 1, member: 21, released_balance: 0 },
			]
		);

		assert_ok!(Pools::unbond(RuntimeOrigin::signed(10), 10, 50));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Chilled { stash: POOL1_BONDED },
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 50, era: 6 },
			]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::Unbonded { member: 10, pool_id: 1, points: 50, balance: 50, era: 6 },]
		);

		// waiting another bonding duration:
		Staking::set_era(BondingDuration::get() * 2);
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(10), 10, 1));

		// pools is fully destroyed now.
		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::StakerRemoved { stash: POOL1_BONDED },
				StakingEvent::Withdrawn { stash: POOL1_BONDED, amount: 50 },
			]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Withdrawn { member: 10, pool_id: 1, points: 50, balance: 50 },
				PoolsEvent::MemberRemoved { pool_id: 1, member: 10, released_balance: 0 },
				PoolsEvent::Destroyed { pool_id: 1 }
			]
		);
	})
}

#[test]
fn pool_chill_e2e() {
	new_test_ext().execute_with(|| {
		assert_eq!(Balances::minimum_balance(), 5);
		assert_active_era(0);

		// create the pool, we know this has id 1.
		assert_ok!(Pools::create(RuntimeOrigin::signed(10), 50, 10, 10, 10));
		assert_eq!(LastPoolId::<Runtime>::get(), 1);

		// have the pool nominate.
		assert_ok!(Pools::nominate(RuntimeOrigin::signed(10), 1, vec![1, 2, 3]));

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Bonded { stash: POOL1_BONDED, amount: 50 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Created { depositor: 10, pool_id: 1 },
				PoolsEvent::Bonded { member: 10, pool_id: 1, bonded: 50, joined: true },
				PoolsEvent::PoolNominationMade { pool_id: 1, caller: 10 },
			]
		);

		// have two members join
		assert_ok!(Pools::join(RuntimeOrigin::signed(20), 10, 1));
		assert_ok!(Pools::join(RuntimeOrigin::signed(21), 10, 1));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 10 },
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 10 },
			]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Bonded { member: 20, pool_id: 1, bonded: 10, joined: true },
				PoolsEvent::Bonded { member: 21, pool_id: 1, bonded: 10, joined: true },
			]
		);

		// in case depositor does not have more than `MinNominatorBond` staked, we can end up in
		// situation where a member unbonding would cause pool balance to drop below
		// `MinNominatorBond` and hence not allowed. This can happen if the `MinNominatorBond` is
		// increased after the pool is created.
		assert_ok!(Staking::set_staking_configs(
			RuntimeOrigin::root(),
			pallet_staking_async::ConfigOp::Set(55), // minimum nominator bond
			pallet_staking_async::ConfigOp::Noop,
			pallet_staking_async::ConfigOp::Noop,
			pallet_staking_async::ConfigOp::Noop,
			pallet_staking_async::ConfigOp::Noop,
			pallet_staking_async::ConfigOp::Noop,
			pallet_staking_async::ConfigOp::Noop,
			pallet_staking_async::ConfigOp::Noop, // are_nominators_slashable
			pallet_staking_async::ConfigOp::Noop
		));

		// members can unbond as long as total stake of the pool is above min nominator bond
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(20), 20, 10),);
		assert_eq!(PoolMembers::<Runtime>::get(20).unwrap().unbonding_eras.len(), 1);
		assert_eq!(PoolMembers::<Runtime>::get(20).unwrap().points, 0);

		// this member cannot unbond since it will cause `pool stake < MinNominatorBond`
		assert_noop!(
			Pools::unbond(RuntimeOrigin::signed(21), 21, 10),
			StakingError::<Runtime>::InsufficientBond,
		);

		// members can call `chill` permissionlessly now
		assert_ok!(Pools::chill(RuntimeOrigin::signed(20), 1));

		// now another member can unbond.
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(21), 21, 10));
		assert_eq!(PoolMembers::<Runtime>::get(21).unwrap().unbonding_eras.len(), 1);
		assert_eq!(PoolMembers::<Runtime>::get(21).unwrap().points, 0);

		// nominator can not resume nomination until depositor have enough stake
		assert_noop!(
			Pools::nominate(RuntimeOrigin::signed(10), 1, vec![1, 2, 3]),
			PoolsError::<Runtime>::MinimumBondNotMet,
		);

		// other members joining pool does not affect the depositor's ability to resume nomination
		assert_ok!(Pools::join(RuntimeOrigin::signed(22), 10, 1));

		assert_noop!(
			Pools::nominate(RuntimeOrigin::signed(10), 1, vec![1, 2, 3]),
			PoolsError::<Runtime>::MinimumBondNotMet,
		);

		// depositor can bond extra stake
		assert_ok!(Pools::bond_extra(RuntimeOrigin::signed(10), BondExtra::FreeBalance(10)));

		// `chill` can not be called permissionlessly anymore
		assert_noop!(
			Pools::chill(RuntimeOrigin::signed(20), 1),
			PoolsError::<Runtime>::NotNominator,
		);

		// now nominator can resume nomination
		assert_ok!(Pools::nominate(RuntimeOrigin::signed(10), 1, vec![1, 2, 3]));

		// skip to make the unbonding period end.
		Staking::set_era(BondingDuration::get());

		// members can now withdraw.
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(20), 20, 0));
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(21), 21, 0));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 10, era: 3 },
				StakingEvent::Chilled { stash: POOL1_BONDED },
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 10, era: 3 },
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 10 }, // other member bonding
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 10 }, // depositor bond extra
				StakingEvent::Withdrawn { stash: POOL1_BONDED, amount: 20 },
			]
		);
	})
}

#[test]
fn pool_slash_e2e() {
	new_test_ext().execute_with(|| {
		ExistentialDeposit::set(1);
		assert_eq!(Balances::minimum_balance(), 1);
		assert_active_era(0);

		// create the pool, we know this has id 1.
		assert_ok!(Pools::create(RuntimeOrigin::signed(10), 40, 10, 10, 10));
		assert_eq!(LastPoolId::<Runtime>::get(), 1);

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Bonded { stash: POOL1_BONDED, amount: 40 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Created { depositor: 10, pool_id: 1 },
				PoolsEvent::Bonded { member: 10, pool_id: 1, bonded: 40, joined: true },
			]
		);

		assert_eq!(
			Payee::<Runtime>::get(POOL1_BONDED),
			Some(RewardDestination::Account(POOL1_REWARD))
		);

		// have two members join
		assert_ok!(Pools::join(RuntimeOrigin::signed(20), 20, 1));
		assert_ok!(Pools::join(RuntimeOrigin::signed(21), 20, 1));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 20 },
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 20 }
			]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Bonded { member: 20, pool_id: 1, bonded: 20, joined: true },
				PoolsEvent::Bonded { member: 21, pool_id: 1, bonded: 20, joined: true },
			]
		);

		// now let's progress a bit.
		Staking::set_era(1);

		// 20 / 80 of the total funds are unlocked, and safe from any further slash.
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(10), 10, 10));
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(20), 20, 10));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 10, era: 4 },
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 10, era: 4 }
			]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Unbonded { member: 10, pool_id: 1, balance: 10, points: 10, era: 4 },
				PoolsEvent::Unbonded { member: 20, pool_id: 1, balance: 10, points: 10, era: 4 }
			]
		);

		Staking::set_era(2);

		// note: depositor cannot fully unbond at this point.
		// these funds will still get slashed.
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(10), 10, 10));
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(20), 20, 10));
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(21), 21, 10));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 10, era: 5 },
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 10, era: 5 },
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 10, era: 5 },
			]
		);

		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Unbonded { member: 10, pool_id: 1, balance: 10, points: 10, era: 5 },
				PoolsEvent::Unbonded { member: 20, pool_id: 1, balance: 10, points: 10, era: 5 },
				PoolsEvent::Unbonded { member: 21, pool_id: 1, balance: 10, points: 10, era: 5 },
			]
		);

		// At this point, 20 are safe from slash, 30 are unlocking but vulnerable to slash, and and
		// another 30 are active and vulnerable to slash. Let's slash half of them.
		pallet_staking_async::slashing::do_slash::<Runtime>(
			&POOL1_BONDED,
			30,
			&mut Default::default(),
			&mut Default::default(),
			2, // slash era 2, affects chunks at era 5 onwards.
		);

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Slashed { staker: POOL1_BONDED, amount: 30 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				// 30 has been slashed to 15 (15 slash)
				PoolsEvent::UnbondingPoolSlashed { pool_id: 1, era: 5, balance: 15 },
				// 30 has been slashed to 15 (15 slash)
				PoolsEvent::PoolSlashed { pool_id: 1, balance: 15 }
			]
		);

		Staking::set_era(3);
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(21), 21, 10));

		assert_eq!(
			PoolMembers::<Runtime>::get(21).unwrap(),
			PoolMember {
				pool_id: 1,
				points: 0,
				last_recorded_reward_counter: Zero::zero(),
				// the 10 points unlocked just now correspond to 5 points in the unbond pool.
				unbonding_eras: bounded_btree_map!(5 => 10, 6 => 5)
			}
		);
		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 5, era: 6 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::Unbonded { member: 21, pool_id: 1, balance: 5, points: 5, era: 6 }]
		);

		// now we start withdrawing. we do it all at once, at era 6 where 20 and 21 are fully free.
		Staking::set_era(6);
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(20), 20, 0));
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(21), 21, 0));

		assert_eq!(
			pool_events_since_last_call(),
			vec![
				// 20 had unbonded 10 safely, and 10 got slashed by half.
				PoolsEvent::Withdrawn { member: 20, pool_id: 1, balance: 10 + 5, points: 20 },
				PoolsEvent::MemberRemoved { pool_id: 1, member: 20, released_balance: 0 },
				// 21 unbonded all of it after the slash
				PoolsEvent::Withdrawn { member: 21, pool_id: 1, balance: 5 + 5, points: 15 },
				PoolsEvent::MemberRemoved { pool_id: 1, member: 21, released_balance: 0 }
			]
		);
		assert_eq!(
			staking_events_since_last_call(),
			// a 10 (un-slashed) + 10/2 (slashed) balance from 10 has also been unlocked
			vec![StakingEvent::Withdrawn { stash: POOL1_BONDED, amount: 15 + 10 + 15 }]
		);

		// now, finally, we can unbond the depositor further than their current limit.
		assert_ok!(Pools::set_state(RuntimeOrigin::signed(10), 1, PoolState::Destroying));
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(10), 10, 20));

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 10, era: 9 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::StateChanged { pool_id: 1, new_state: PoolState::Destroying },
				PoolsEvent::Unbonded { member: 10, pool_id: 1, points: 10, balance: 10, era: 9 }
			]
		);

		Staking::set_era(9);
		assert_eq!(
			PoolMembers::<Runtime>::get(10).unwrap(),
			PoolMember {
				pool_id: 1,
				points: 0,
				last_recorded_reward_counter: Zero::zero(),
				unbonding_eras: bounded_btree_map!(4 => 10, 5 => 10, 9 => 10)
			}
		);
		// withdraw the depositor, they should lose 12 balance in total due to slash.
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(10), 10, 0));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::StakerRemoved { stash: POOL1_BONDED },
				StakingEvent::Withdrawn { stash: POOL1_BONDED, amount: 10 },
			]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Withdrawn { member: 10, pool_id: 1, balance: 10 + 15, points: 30 },
				PoolsEvent::MemberRemoved { pool_id: 1, member: 10, released_balance: 0 },
				PoolsEvent::Destroyed { pool_id: 1 }
			]
		);
	});
}

#[test]
fn pool_slash_proportional() {
	// a typical example where 3 pool members unbond in era 99, 100, and 101, and a slash that
	// happened in era 100 should only affect the latter two.
	new_test_ext().execute_with(|| {
		ExistentialDeposit::set(2);
		BondingDuration::set(28);
		assert_eq!(Balances::minimum_balance(), 2);
		assert_active_era(0);

		// create the pool, we know this has id 1.
		assert_ok!(Pools::create(RuntimeOrigin::signed(10), 40, 10, 10, 10));
		assert_eq!(LastPoolId::<T>::get(), 1);

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Bonded { stash: POOL1_BONDED, amount: 40 }]
		);
		assert_eq!(
			delegated_staking_events_since_last_call(),
			vec![DelegatedStakingEvent::Delegated {
				agent: POOL1_BONDED,
				delegator: 10,
				amount: 40
			}]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Created { depositor: 10, pool_id: 1 },
				PoolsEvent::Bonded { member: 10, pool_id: 1, bonded: 40, joined: true },
			]
		);

		// have two members join
		let bond = 20;
		assert_ok!(Pools::join(RuntimeOrigin::signed(20), bond, 1));
		assert_ok!(Pools::join(RuntimeOrigin::signed(21), bond, 1));
		assert_ok!(Pools::join(RuntimeOrigin::signed(22), bond, 1));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: bond },
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: bond },
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: bond },
			]
		);
		assert_eq!(
			delegated_staking_events_since_last_call(),
			vec![
				DelegatedStakingEvent::Delegated {
					agent: POOL1_BONDED,
					delegator: 20,
					amount: bond
				},
				DelegatedStakingEvent::Delegated {
					agent: POOL1_BONDED,
					delegator: 21,
					amount: bond
				},
				DelegatedStakingEvent::Delegated {
					agent: POOL1_BONDED,
					delegator: 22,
					amount: bond
				}
			]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Bonded { member: 20, pool_id: 1, bonded: bond, joined: true },
				PoolsEvent::Bonded { member: 21, pool_id: 1, bonded: bond, joined: true },
				PoolsEvent::Bonded { member: 22, pool_id: 1, bonded: bond, joined: true },
			]
		);

		// now let's progress a lot.
		Staking::set_era(99);

		// and unbond
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(20), 20, bond));

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Unbonded { stash: POOL1_BONDED, amount: bond, era: 127 },]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::Unbonded {
				member: 20,
				pool_id: 1,
				balance: bond,
				points: bond,
				era: 127
			}]
		);

		Staking::set_era(100);
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(21), 21, bond));
		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Unbonded { stash: POOL1_BONDED, amount: bond, era: 128 },]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::Unbonded {
				member: 21,
				pool_id: 1,
				balance: bond,
				points: bond,
				era: 128
			}]
		);

		Staking::set_era(101);
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(22), 22, bond));
		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Unbonded { stash: POOL1_BONDED, amount: bond, era: 129 },]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::Unbonded {
				member: 22,
				pool_id: 1,
				balance: bond,
				points: bond,
				era: 129
			}]
		);

		// Apply a slash that happened in era 100. This is typically applied with a delay.
		// Of the total 100, 50 is slashed.
		assert_eq!(BondedPools::<T>::get(1).unwrap().points, 40);

		// no pending slash yet.
		assert_eq!(Pools::api_pool_pending_slash(1), 0);
		// and therefore applying slash fails
		assert_noop!(
			Pools::apply_slash(RuntimeOrigin::signed(10), 21),
			PoolsError::<Runtime>::NothingToSlash
		);

		hypothetically!({
			// a very small amount is slashed
			pallet_staking_async::slashing::do_slash::<Runtime>(
				&POOL1_BONDED,
				3,
				&mut Default::default(),
				&mut Default::default(),
				100,
			);

			// ensure correct amount is pending to be slashed
			assert_eq!(Pools::api_pool_pending_slash(1), 3);

			// 21 has pending slash lower than ED (2)
			assert_eq!(Pools::api_member_pending_slash(21), 1);

			// slash fails as minimum pending slash amount not met.
			assert_noop!(
				Pools::apply_slash(RuntimeOrigin::signed(10), 21),
				PoolsError::<Runtime>::SlashTooLow
			);
		});

		pallet_staking_async::slashing::do_slash::<Runtime>(
			&POOL1_BONDED,
			50,
			&mut Default::default(),
			&mut Default::default(),
			100,
		);

		// Pools api returns correct slash amount.
		assert_eq!(Pools::api_pool_pending_slash(1), 50);

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Slashed { staker: POOL1_BONDED, amount: 50 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				// This era got slashed 12.5, which rounded up to 13.
				PoolsEvent::UnbondingPoolSlashed { pool_id: 1, era: 128, balance: 7 },
				// This era got slashed 12 instead of 12.5 because an earlier chunk got 0.5 more
				// slashed, and 12 is all the remaining slash
				PoolsEvent::UnbondingPoolSlashed { pool_id: 1, era: 129, balance: 8 },
				// Bonded pool got slashed for 25, remaining 15 in it.
				PoolsEvent::PoolSlashed { pool_id: 1, balance: 15 }
			]
		);

		// 21's balance in the pool is slashed.
		assert_eq!(PoolMembers::<Runtime>::get(21).unwrap().total_balance(), 7);
		// But their actual balance is still unslashed.
		assert_eq!(Balances::total_balance_on_hold(&21), bond);
		// 21 has pending slash
		assert_eq!(Pools::api_member_pending_slash(21), bond - 7);
		// apply slash permissionlessly.
		assert_ok!(Pools::apply_slash(RuntimeOrigin::signed(10), 21));
		// member balance is slashed.
		assert_eq!(Balances::total_balance_on_hold(&21), 7);
		// 21 has no pending slash anymore
		assert_eq!(Pools::api_member_pending_slash(21), 0);

		assert_eq!(
			delegated_staking_events_since_last_call(),
			vec![DelegatedStakingEvent::Slashed {
				agent: POOL1_BONDED,
				delegator: 21,
				amount: bond - 7
			}]
		);

		// 22 balance isn't slashed yet as well.
		assert_eq!(PoolMembers::<Runtime>::get(22).unwrap().total_balance(), 8);
		assert_eq!(Balances::total_balance_on_hold(&22), bond);

		// they try to withdraw. This should slash them.
		Staking::set_era(129);
		let pre_balance = Balances::free_balance(&22);
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(22), 22, 0));
		// all balance should be released.
		assert_eq!(Balances::total_balance_on_hold(&22), 0);
		assert_eq!(Balances::free_balance(&22), pre_balance + 8);

		assert_eq!(
			delegated_staking_events_since_last_call(),
			vec![
				DelegatedStakingEvent::Slashed {
					agent: POOL1_BONDED,
					delegator: 22,
					amount: bond - 8
				},
				DelegatedStakingEvent::Released { agent: POOL1_BONDED, delegator: 22, amount: 8 },
			]
		);
	});
}

#[test]
fn pool_slash_non_proportional_only_bonded_pool() {
	// A typical example where a pool member unbonds in era 99, and they can get away with a slash
	// that happened in era 100, as long as the pool has enough active bond to cover the slash. If
	// everything else in the slashing/staking system works, this should always be the case.
	// Nonetheless, `ledger.slash` has been written such that it will slash greedily from any chunk
	// if it runs out of chunks that it thinks should be affected by the slash.
	new_test_ext().execute_with(|| {
		ExistentialDeposit::set(1);
		BondingDuration::set(28);
		assert_eq!(Balances::minimum_balance(), 1);
		assert_active_era(0);

		// create the pool, we know this has id 1.
		assert_ok!(Pools::create(RuntimeOrigin::signed(10), 40, 10, 10, 10));
		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Bonded { stash: POOL1_BONDED, amount: 40 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Created { depositor: 10, pool_id: 1 },
				PoolsEvent::Bonded { member: 10, pool_id: 1, bonded: 40, joined: true },
			]
		);

		// have two members join
		let bond = 20;
		assert_ok!(Pools::join(RuntimeOrigin::signed(20), bond, 1));
		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Bonded { stash: POOL1_BONDED, amount: bond }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::Bonded { member: 20, pool_id: 1, bonded: bond, joined: true }]
		);

		// progress and unbond.
		Staking::set_era(99);
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(20), 20, bond));
		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Unbonded { stash: POOL1_BONDED, amount: bond, era: 127 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::Unbonded {
				member: 20,
				pool_id: 1,
				balance: bond,
				points: bond,
				era: 127
			}]
		);

		// slash for 30. This will be deducted only from the bonded pool.
		Staking::set_era(100);
		assert_eq!(BondedPools::<T>::get(1).unwrap().points, 40);
		pallet_staking_async::slashing::do_slash::<Runtime>(
			&POOL1_BONDED,
			30,
			&mut Default::default(),
			&mut Default::default(),
			100,
		);

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Slashed { staker: POOL1_BONDED, amount: 30 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::PoolSlashed { pool_id: 1, balance: 10 }]
		);
	});
}

#[test]
fn pool_slash_non_proportional_bonded_pool_and_chunks() {
	// An uncommon example where even though some funds are unlocked such that they should not be
	// affected by a slash, we still slash out of them. This should not happen at all. If a
	// nomination has unbonded, from the next era onwards, their exposure will drop, so if an era
	// happens in that era, then their share of that slash should naturally be less, such that only
	// their active ledger stake is enough to compensate it.
	new_test_ext().execute_with(|| {
		ExistentialDeposit::set(1);
		BondingDuration::set(28);
		assert_eq!(Balances::minimum_balance(), 1);
		assert_active_era(0);

		// create the pool, we know this has id 1.
		assert_ok!(Pools::create(RuntimeOrigin::signed(10), 40, 10, 10, 10));
		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Bonded { stash: POOL1_BONDED, amount: 40 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Created { depositor: 10, pool_id: 1 },
				PoolsEvent::Bonded { member: 10, pool_id: 1, bonded: 40, joined: true },
			]
		);

		// have two members join
		let bond = 20;
		assert_ok!(Pools::join(RuntimeOrigin::signed(20), bond, 1));
		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Bonded { stash: POOL1_BONDED, amount: bond }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::Bonded { member: 20, pool_id: 1, bonded: bond, joined: true }]
		);

		// progress and unbond.
		Staking::set_era(99);
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(20), 20, bond));
		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Unbonded { stash: POOL1_BONDED, amount: bond, era: 127 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::Unbonded {
				member: 20,
				pool_id: 1,
				balance: bond,
				points: bond,
				era: 127
			}]
		);

		// slash 50. This will be deducted only from the bonded pool and one of the unbonding pools.
		Staking::set_era(100);
		assert_eq!(BondedPools::<T>::get(1).unwrap().points, 40);
		pallet_staking_async::slashing::do_slash::<Runtime>(
			&POOL1_BONDED,
			50,
			&mut Default::default(),
			&mut Default::default(),
			100,
		);

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Slashed { staker: POOL1_BONDED, amount: 50 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				// out of 20, 10 was taken.
				PoolsEvent::UnbondingPoolSlashed { pool_id: 1, era: 127, balance: 10 },
				// out of 40, all was taken.
				PoolsEvent::PoolSlashed { pool_id: 1, balance: 0 }
			]
		);
	});
}

#[test]
fn pool_migration_e2e() {
	new_test_ext().execute_with(|| {
		LegacyAdapter::set(true);
		assert_active_era(0);

		// hack: mint ED to pool so that the deprecated `TransferStake` works correctly with
		// staking.
		assert_eq!(Balances::minimum_balance(), 5);
		assert_ok!(Balances::mint_into(&POOL1_BONDED, 5));

		// create the pool with TransferStake strategy.
		assert_ok!(Pools::create(RuntimeOrigin::signed(10), 50, 10, 10, 10));
		assert_eq!(LastPoolId::<Runtime>::get(), 1);

		// have the pool nominate.
		assert_ok!(Pools::nominate(RuntimeOrigin::signed(10), 1, vec![1, 2, 3]));

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Bonded { stash: POOL1_BONDED, amount: 50 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Created { depositor: 10, pool_id: 1 },
				PoolsEvent::Bonded { member: 10, pool_id: 1, bonded: 50, joined: true },
				PoolsEvent::PoolNominationMade { pool_id: 1, caller: 10 }
			]
		);

		// have three members join
		let pre_20 = Balances::free_balance(20);
		assert_ok!(Pools::join(RuntimeOrigin::signed(20), 10, 1));
		let pre_21 = Balances::free_balance(21);
		assert_ok!(Pools::join(RuntimeOrigin::signed(21), 10, 1));
		let pre_22 = Balances::free_balance(22);
		assert_ok!(Pools::join(RuntimeOrigin::signed(22), 10, 1));

		// verify members balance is moved to pool.
		assert_eq!(Balances::free_balance(20), pre_20 - 10);
		assert_eq!(Balances::free_balance(21), pre_21 - 10);
		assert_eq!(Balances::free_balance(22), pre_22 - 10);

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 10 },
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 10 },
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 10 },
			]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Bonded { member: 20, pool_id: 1, bonded: 10, joined: true },
				PoolsEvent::Bonded { member: 21, pool_id: 1, bonded: 10, joined: true },
				PoolsEvent::Bonded { member: 22, pool_id: 1, bonded: 10, joined: true },
			]
		);

		Staking::set_era(2);
		// 20 is partially unbonding
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(20), 20, 5));

		Staking::set_era(3);
		// 21 is fully unbonding
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(21), 21, 10));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 5, era: 5 },
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 10, era: 6 },
			]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Unbonded { member: 20, pool_id: 1, balance: 5, points: 5, era: 5 },
				PoolsEvent::Unbonded { member: 21, pool_id: 1, balance: 10, points: 10, era: 6 },
			]
		);

		// with `TransferStake`, we can't migrate.
		assert!(!Pools::api_pool_needs_delegate_migration(1));
		assert_noop!(
			Pools::migrate_pool_to_delegate_stake(RuntimeOrigin::signed(10), 1),
			PoolsError::<Runtime>::NotSupported
		);

		// we reset the adapter to `DelegateStake`.
		LegacyAdapter::set(false);

		// cannot migrate the member delegation unless pool is migrated first.
		assert_noop!(
			Pools::migrate_delegation(RuntimeOrigin::signed(10), 20),
			PoolsError::<Runtime>::NotMigrated
		);

		// migrate the pool.
		assert!(Pools::api_pool_needs_delegate_migration(1));
		assert_ok!(Pools::migrate_pool_to_delegate_stake(RuntimeOrigin::signed(10), 1));

		// migrate again does not work.
		assert!(!Pools::api_pool_needs_delegate_migration(1));
		assert_noop!(
			Pools::migrate_pool_to_delegate_stake(RuntimeOrigin::signed(10), 1),
			PoolsError::<Runtime>::AlreadyMigrated
		);

		// unclaimed delegations to the pool are stored in this account.
		let proxy_delegator_1 =
			DelegatedStaking::generate_proxy_delegator(Agent::from(POOL1_BONDED)).get();

		assert_eq!(
			delegated_staking_events_since_last_call(),
			// delegated also contains the extra ED that we minted when pool was `TransferStake` .
			vec![DelegatedStakingEvent::Delegated {
				agent: POOL1_BONDED,
				delegator: proxy_delegator_1,
				amount: 50 + 10 * 3 + 5
			}]
		);

		// move to era 5 when 20 can withdraw unbonded funds.
		Staking::set_era(5);

		// Cannot unbond without claiming delegation. Lets unbond 22.
		assert_noop!(
			Pools::unbond(RuntimeOrigin::signed(22), 22, 5),
			PoolsError::<Runtime>::NotMigrated
		);

		// withdraw fails for 20 before claiming delegation
		assert_noop!(
			Pools::withdraw_unbonded(RuntimeOrigin::signed(20), 20, 10),
			PoolsError::<Runtime>::NotMigrated
		);

		let pre_claim_balance_20 = Balances::total_balance(&20);
		assert_eq!(Balances::total_balance_on_hold(&20), 0);

		// migrate delegation for 20. This is permissionless and can be called by anyone.
		assert!(Pools::api_member_needs_delegate_migration(20));
		assert_ok!(Pools::migrate_delegation(RuntimeOrigin::signed(10), 20));

		// tokens moved to 20's account and held there.
		assert_eq!(Balances::total_balance(&20), pre_claim_balance_20 + 10);
		assert_eq!(Balances::total_balance_on_hold(&20), 10);

		// withdraw works now
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(20), 20, 5));

		// balance unlocked in 20's account
		assert_eq!(Balances::total_balance_on_hold(&20), 5);
		assert_eq!(Balances::total_balance(&20), pre_claim_balance_20 + 10);

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Withdrawn { stash: POOL1_BONDED, amount: 5 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::Withdrawn { member: 20, pool_id: 1, balance: 5, points: 5 },]
		);
		assert_eq!(
			delegated_staking_events_since_last_call(),
			vec![
				DelegatedStakingEvent::MigratedDelegation {
					agent: POOL1_BONDED,
					delegator: 20,
					amount: 10
				},
				DelegatedStakingEvent::Released { agent: POOL1_BONDED, delegator: 20, amount: 5 }
			]
		);

		// MIGRATE 21
		let pre_migrate_balance_21 = Balances::total_balance(&21);
		assert_eq!(Balances::total_balance_on_hold(&21), 0);

		// migrate delegation for 21.
		assert!(Pools::api_member_needs_delegate_migration(21));
		assert_ok!(Pools::migrate_delegation(RuntimeOrigin::signed(10), 21));

		// tokens moved to 21's account and held there.
		assert_eq!(Balances::total_balance(&21), pre_migrate_balance_21 + 10);
		assert_eq!(Balances::total_balance_on_hold(&21), 10);

		// withdraw fails since 21 only unbonds at era 6.
		assert_noop!(
			Pools::withdraw_unbonded(RuntimeOrigin::signed(21), 21, 10),
			PoolsError::<Runtime>::CannotWithdrawAny
		);

		// go to era when 21 can unbond
		Staking::set_era(6);

		// withdraw works now
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(21), 21, 10));

		// all balance unlocked in 21's account
		assert_eq!(Balances::total_balance_on_hold(&21), 0);
		assert_eq!(Balances::total_balance(&21), pre_migrate_balance_21 + 10);

		// MIGRATE 22
		assert_eq!(Balances::total_balance_on_hold(&22), 0);
		// make balance of 22 as 0.
		let _ = Balances::make_free_balance_be(&22, 0);

		// migrate delegation for 22.
		assert!(Pools::api_member_needs_delegate_migration(22));
		assert_ok!(Pools::migrate_delegation(RuntimeOrigin::signed(10), 22));

		// cannot migrate a pool member again.
		assert!(!Pools::api_member_needs_delegate_migration(22));
		assert_noop!(
			Pools::migrate_delegation(RuntimeOrigin::signed(10), 22),
			PoolsError::<Runtime>::AlreadyMigrated
		);

		// tokens moved to 22's account and held there.
		assert_eq!(Balances::total_balance(&22), 10);
		assert_eq!(Balances::total_balance_on_hold(&22), 10);

		// unbond 22 should work now
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(22), 22, 5));

		// withdraw fails since 22 only unbonds after era 9.
		assert_noop!(
			Pools::withdraw_unbonded(RuntimeOrigin::signed(22), 22, 5),
			PoolsError::<Runtime>::CannotWithdrawAny
		);

		// go to era when 22 can unbond
		Staking::set_era(9);

		// withdraw works now
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(22), 22, 10));

		// balance of 5 unlocked in 22's account
		assert_eq!(Balances::total_balance_on_hold(&22), 10 - 5);

		// assert events for 21 and 22.
		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Withdrawn { stash: POOL1_BONDED, amount: 10 },
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 5, era: 9 },
				StakingEvent::Withdrawn { stash: POOL1_BONDED, amount: 5 }
			]
		);

		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Withdrawn { member: 21, pool_id: 1, balance: 10, points: 10 },
				// 21 was fully unbonding and removed from pool.
				PoolsEvent::MemberRemoved { member: 21, pool_id: 1, released_balance: 0 },
				PoolsEvent::Unbonded { member: 22, pool_id: 1, balance: 5, points: 5, era: 9 },
				PoolsEvent::Withdrawn { member: 22, pool_id: 1, balance: 5, points: 5 },
			]
		);
		assert_eq!(
			delegated_staking_events_since_last_call(),
			vec![
				DelegatedStakingEvent::MigratedDelegation {
					agent: POOL1_BONDED,
					delegator: 21,
					amount: 10
				},
				DelegatedStakingEvent::Released { agent: POOL1_BONDED, delegator: 21, amount: 10 },
				DelegatedStakingEvent::MigratedDelegation {
					agent: POOL1_BONDED,
					delegator: 22,
					amount: 10
				},
				DelegatedStakingEvent::Released { agent: POOL1_BONDED, delegator: 22, amount: 5 }
			]
		);
	})
}

#[test]
fn disable_pool_operations_on_non_migrated() {
	new_test_ext().execute_with(|| {
		LegacyAdapter::set(true);
		assert_eq!(Balances::minimum_balance(), 5);
		assert_active_era(0);

		// hack: mint ED to pool so that the deprecated `TransferStake` works correctly with
		// staking.
		assert_eq!(Balances::minimum_balance(), 5);
		assert_ok!(Balances::mint_into(&POOL1_BONDED, 5));

		// create the pool with TransferStake strategy.
		assert_ok!(Pools::create(RuntimeOrigin::signed(10), 50, 10, 10, 10));
		assert_eq!(LastPoolId::<Runtime>::get(), 1);

		// have the pool nominate.
		assert_ok!(Pools::nominate(RuntimeOrigin::signed(10), 1, vec![1, 2, 3]));

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Bonded { stash: POOL1_BONDED, amount: 50 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Created { depositor: 10, pool_id: 1 },
				PoolsEvent::Bonded { member: 10, pool_id: 1, bonded: 50, joined: true },
				PoolsEvent::PoolNominationMade { pool_id: 1, caller: 10 }
			]
		);

		let pre_20 = Balances::free_balance(20);
		assert_ok!(Pools::join(RuntimeOrigin::signed(20), 10, 1));

		// verify members balance is moved to pool.
		assert_eq!(Balances::free_balance(20), pre_20 - 10);

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Bonded { stash: POOL1_BONDED, amount: 10 },]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::Bonded { member: 20, pool_id: 1, bonded: 10, joined: true },]
		);

		// we reset the adapter to `DelegateStake`.
		LegacyAdapter::set(false);

		// pool is pending migration.
		assert!(Pools::api_pool_needs_delegate_migration(1));

		// ensure pool mutation is not allowed until pool is migrated.
		assert_noop!(
			Pools::join(RuntimeOrigin::signed(21), 10, 1),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::pool_withdraw_unbonded(RuntimeOrigin::signed(10), 1, 0),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::nominate(RuntimeOrigin::signed(10), 1, vec![1, 2, 3]),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::set_state(RuntimeOrigin::signed(10), 1, PoolState::Blocked),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::set_metadata(RuntimeOrigin::signed(10), 1, vec![1, 1]),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::update_roles(
				RuntimeOrigin::signed(10),
				1,
				ConfigOp::Set(5),
				ConfigOp::Set(6),
				ConfigOp::Set(7)
			),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::chill(RuntimeOrigin::signed(10), 1),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::set_commission(RuntimeOrigin::signed(10), 1, None),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::set_commission_max(RuntimeOrigin::signed(10), 1, Zero::zero()),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::set_commission_change_rate(
				RuntimeOrigin::signed(10),
				1,
				CommissionChangeRate { max_increase: Perbill::from_percent(1), min_delay: 2_u64 }
			),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::claim_commission(RuntimeOrigin::signed(10), 1),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::adjust_pool_deposit(RuntimeOrigin::signed(10), 1),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::set_commission_claim_permission(RuntimeOrigin::signed(10), 1, None),
			PoolsError::<Runtime>::NotMigrated
		);

		// migrate the pool.
		assert_ok!(Pools::migrate_pool_to_delegate_stake(RuntimeOrigin::signed(10), 1));
		assert_eq!(
			delegated_staking_events_since_last_call(),
			// delegated also contains the extra ED that we minted when pool was `TransferStake` .
			vec![DelegatedStakingEvent::Delegated {
				agent: POOL1_BONDED,
				delegator: DelegatedStaking::generate_proxy_delegator(Agent::from(POOL1_BONDED))
					.get(),
				amount: 50 + 10 + 5
			},]
		);

		// member is pending migration.
		assert!(Pools::api_member_needs_delegate_migration(20));

		// ensure member mutation is not allowed until member's delegation is migrated.
		assert_noop!(
			Pools::bond_extra(RuntimeOrigin::signed(20), BondExtra::FreeBalance(5)),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::bond_extra_other(RuntimeOrigin::signed(10), 20, BondExtra::Rewards),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::claim_payout(RuntimeOrigin::signed(20)),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::unbond(RuntimeOrigin::signed(20), 20, 5),
			PoolsError::<Runtime>::NotMigrated
		);
		assert_noop!(
			Pools::withdraw_unbonded(RuntimeOrigin::signed(20), 20, 0),
			PoolsError::<Runtime>::NotMigrated
		);

		// migrate 20
		assert_ok!(Pools::migrate_delegation(RuntimeOrigin::signed(10), 20));
		// now `bond_extra` for 20 works.
		assert_ok!(Pools::bond_extra(RuntimeOrigin::signed(20), BondExtra::FreeBalance(5)));

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Bonded { stash: POOL1_BONDED, amount: 5 },]
		);

		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::Bonded { member: 20, pool_id: 1, bonded: 5, joined: false },]
		);

		assert_eq!(
			delegated_staking_events_since_last_call(),
			vec![
				DelegatedStakingEvent::MigratedDelegation {
					agent: POOL1_BONDED,
					delegator: 20,
					amount: 10
				},
				DelegatedStakingEvent::Delegated { agent: POOL1_BONDED, delegator: 20, amount: 5 },
			]
		);
	})
}

#[test]
fn pool_no_dangling_delegation() {
	new_test_ext().execute_with(|| {
		ExistentialDeposit::set(1);
		assert_eq!(Balances::minimum_balance(), 1);
		assert_active_era(0);
		// pool creator
		let alice = 10;
		let bob = 20;
		let charlie = 21;

		// create the pool, we know this has id 1.
		assert_ok!(Pools::create(RuntimeOrigin::signed(alice), 40, alice, alice, alice));
		assert_eq!(LastPoolId::<Runtime>::get(), 1);

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Bonded { stash: POOL1_BONDED, amount: 40 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Created { depositor: alice, pool_id: 1 },
				PoolsEvent::Bonded { member: alice, pool_id: 1, bonded: 40, joined: true },
			]
		);
		assert_eq!(
			delegated_staking_events_since_last_call(),
			vec![DelegatedStakingEvent::Delegated {
				agent: POOL1_BONDED,

				delegator: alice,
				amount: 40
			},]
		);

		assert_eq!(
			Payee::<Runtime>::get(POOL1_BONDED),
			Some(RewardDestination::Account(POOL1_REWARD))
		);

		// have two members join
		assert_ok!(Pools::join(RuntimeOrigin::signed(bob), 20, 1));
		assert_ok!(Pools::join(RuntimeOrigin::signed(charlie), 20, 1));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 20 },
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 20 }
			]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Bonded { member: bob, pool_id: 1, bonded: 20, joined: true },
				PoolsEvent::Bonded { member: charlie, pool_id: 1, bonded: 20, joined: true },
			]
		);
		assert_eq!(
			delegated_staking_events_since_last_call(),
			vec![
				DelegatedStakingEvent::Delegated {
					agent: POOL1_BONDED,
					delegator: bob,
					amount: 20
				},
				DelegatedStakingEvent::Delegated {
					agent: POOL1_BONDED,
					delegator: charlie,
					amount: 20
				},
			]
		);

		// now let's progress a bit.
		Staking::set_era(1);

		// bob is completely unbonding
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(bob), 20, 20));

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 20, era: 4 },]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::Unbonded { member: bob, pool_id: 1, balance: 20, points: 20, era: 4 }]
		);

		// this era will get slashed
		Staking::set_era(2);

		assert_ok!(Pools::unbond(RuntimeOrigin::signed(alice), 10, 10));
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(charlie), 21, 10));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 10, era: 5 },
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 10, era: 5 },
			]
		);

		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Unbonded { member: alice, pool_id: 1, balance: 10, points: 10, era: 5 },
				PoolsEvent::Unbonded {
					member: charlie,
					pool_id: 1,
					balance: 10,
					points: 10,
					era: 5
				},
			]
		);

		// At this point, bob's 20 that is unlocking is safe from slash, 10 (alice) + 10 (charlie)
		// are also unlocking but vulnerable to slash, and another 40 are active and vulnerable to
		// slash. Let's slash half of them.
		pallet_staking_async::slashing::do_slash::<Runtime>(
			&POOL1_BONDED,
			30,
			&mut Default::default(),
			&mut Default::default(),
			2, // slash era 2, affects chunks at era 5 onwards.
		);

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Slashed { staker: POOL1_BONDED, amount: 30 }]
		);

		assert_eq!(
			pool_events_since_last_call(),
			vec![
				// unbonding pool of 20 for era 5 has been slashed to 10
				PoolsEvent::UnbondingPoolSlashed { pool_id: 1, era: 5, balance: 10 },
				// active stake of 40 has been slashed to half
				PoolsEvent::PoolSlashed { pool_id: 1, balance: 20 } /* no slash to era 4
				                                                     * unbonding pool */
			]
		);

		// alice's initial stake of 40 is reduced to half
		assert_eq!(PoolMembers::<Runtime>::get(alice).unwrap().total_balance(), 20);
		// bob unbonded in era 1 and is safe from slash
		assert_eq!(PoolMembers::<Runtime>::get(bob).unwrap().total_balance(), 20);
		// charlie's initial stake of 20 is slashed to half
		assert_eq!(PoolMembers::<Runtime>::get(charlie).unwrap().total_balance(), 10);

		// apply pending slash to alice.
		assert_eq!(Pools::api_member_pending_slash(alice), 20);
		assert_ok!(Pools::apply_slash(RuntimeOrigin::signed(10), alice));
		// apply pending slash to charlie.
		assert_eq!(Pools::api_member_pending_slash(charlie), 10);
		assert_ok!(Pools::apply_slash(RuntimeOrigin::signed(10), charlie));
		// no pending slash for bob
		assert_eq!(Pools::api_member_pending_slash(bob), 0);

		assert_eq!(
			delegated_staking_events_since_last_call(),
			vec![
				DelegatedStakingEvent::Slashed {
					agent: POOL1_BONDED,
					delegator: alice,
					amount: 20
				},
				DelegatedStakingEvent::Slashed {
					agent: POOL1_BONDED,
					delegator: charlie,
					amount: 10
				},
			]
		);

		// go forward to an era after the effective post-unbonding window
		// (MaxUnbondingPools(13) - bonding_duration(3) = 10) ends for era 5.
		Staking::set_era(15);
		// At this point subpools will all be merged in no-era causing Bob to lose some value while
		// Alice and Charlie will gain some value.
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(charlie), charlie, 10));

		// Now alice and charlie has less balance locked than their contribution.
		assert_eq!(Balances::total_balance_on_hold(&alice), 20);
		assert_eq!(PoolMembers::<Runtime>::get(alice).unwrap().total_balance(), 22);
		assert_eq!(Balances::total_balance_on_hold(&charlie), 10);
		assert_eq!(PoolMembers::<Runtime>::get(charlie).unwrap().total_balance(), 12);

		// and bob has more balance locked than his contribution.
		assert_eq!(Balances::total_balance_on_hold(&bob), 20);
		assert_eq!(PoolMembers::<Runtime>::get(bob).unwrap().total_balance(), 15);

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 5, era: 18 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![PoolsEvent::Unbonded {
				member: charlie,
				pool_id: 1,
				balance: 5,
				points: 5,
				era: 18
			}]
		);

		// When bob withdraws all, he gets all his locked funds back.
		let bob_pre_withdraw_balance = Balances::free_balance(&bob);
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(bob), bob, 0));
		assert_eq!(Balances::free_balance(&bob), bob_pre_withdraw_balance + 20);
		assert_eq!(Balances::total_balance_on_hold(&bob), 0);
		assert!(!PoolMembers::<Runtime>::contains_key(bob));

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Withdrawn { stash: POOL1_BONDED, amount: 30 },]
		);

		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Withdrawn { pool_id: 1, member: bob, balance: 15, points: 20 },
				// dangling delegation of 5 is released
				PoolsEvent::MemberRemoved { pool_id: 1, member: bob, released_balance: 5 },
			]
		);
		assert_eq!(
			delegated_staking_events_since_last_call(),
			vec![
				DelegatedStakingEvent::Released { agent: POOL1_BONDED, delegator: bob, amount: 15 },
				// the second release is the dangling delegation when member is removed.
				DelegatedStakingEvent::Released { agent: POOL1_BONDED, delegator: bob, amount: 5 },
			]
		);

		// Charlie can withdraw as much as he has locked.
		Staking::set_era(18);
		let charlie_pre_withdraw_balance = Balances::free_balance(&charlie);
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(charlie), charlie, 0));
		// Charlie's total balance was 12, but we don't have enough funds to unlock. We try the best
		// effort and unlock 10.
		assert_eq!(Balances::free_balance(&charlie), charlie_pre_withdraw_balance + 10);

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Withdrawn { stash: POOL1_BONDED, amount: 5 },]
		);

		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Withdrawn { pool_id: 1, member: charlie, balance: 10, points: 15 },
				PoolsEvent::MemberRemoved { member: charlie, pool_id: 1, released_balance: 0 }
			]
		);
		assert_eq!(
			delegated_staking_events_since_last_call(),
			vec![DelegatedStakingEvent::Released {
				agent: POOL1_BONDED,
				delegator: charlie,
				amount: 10
			},]
		);

		// Set pools to destroying so alice can withdraw
		assert_ok!(Pools::set_state(RuntimeOrigin::signed(alice), 1, PoolState::Destroying));
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(alice), alice, 30));

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 15, era: 21 }]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::StateChanged { pool_id: 1, new_state: PoolState::Destroying },
				PoolsEvent::Unbonded {
					member: alice,
					pool_id: 1,
					points: 15,
					balance: 15,
					era: 21
				}
			]
		);

		Staking::set_era(21);
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(alice), alice, 0));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::StakerRemoved { stash: POOL1_BONDED },
				StakingEvent::Withdrawn { stash: POOL1_BONDED, amount: 15 },
			]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Withdrawn { member: alice, pool_id: 1, balance: 20, points: 25 },
				PoolsEvent::MemberRemoved { pool_id: 1, member: alice, released_balance: 0 },
				PoolsEvent::Destroyed { pool_id: 1 }
			]
		);

		// holds for all members are released.
		assert_eq!(Balances::total_balance_on_hold(&alice), 0);
		assert_eq!(Balances::total_balance_on_hold(&bob), 0);
		assert_eq!(Balances::total_balance_on_hold(&charlie), 0);
	});
}

/// When `AreNominatorsSlashable` is false, pool members can unbond and withdraw in 1 era
/// instead of waiting the full bonding duration.
#[test]
fn pool_members_unbond_in_one_era_when_nominators_not_slashable() {
	new_test_ext().execute_with(|| {
		// Set nominators as not slashable - this enables fast unbonding.
		AreNominatorsSlashable::<Runtime>::put(false);
		assert!(!AreNominatorsSlashable::<Runtime>::get());

		assert_eq!(Balances::minimum_balance(), 5);
		assert_active_era(0);
		assert_eq!(BondingDuration::get(), 3); // Full bonding duration is 3 eras.

		// Create the pool with depositor (10) bonding 50.
		assert_ok!(Pools::create(RuntimeOrigin::signed(10), 50, 10, 10, 10));
		assert_eq!(LastPoolId::<Runtime>::get(), 1);

		// Have the pool nominate.
		assert_ok!(Pools::nominate(RuntimeOrigin::signed(10), 1, vec![1, 2, 3]));

		// Member 20 joins with 10.
		assert_ok!(Pools::join(RuntimeOrigin::signed(20), 10, 1));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 50 },
				StakingEvent::Bonded { stash: POOL1_BONDED, amount: 10 },
			]
		);

		// Progress to era 1.
		Staking::set_era(1);

		// Member 20 unbonds their 10.
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(20), 20, 10));

		// The unbond era should be current_era + NominatorFastUnbondDuration = 1 + 2 = 3
		// (not current_era + BondingDuration = 1 + 3 = 4).
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Created { depositor: 10, pool_id: 1 },
				PoolsEvent::Bonded { member: 10, pool_id: 1, bonded: 50, joined: true },
				PoolsEvent::PoolNominationMade { pool_id: 1, caller: 10 },
				PoolsEvent::Bonded { member: 20, pool_id: 1, bonded: 10, joined: true },
				// Unbond era is 3 (current era 1 + NominatorFastUnbondDuration 2), not 4!
				PoolsEvent::Unbonded { member: 20, pool_id: 1, points: 10, balance: 10, era: 3 },
			]
		);

		// Cannot withdraw yet - still in era 1.
		assert_noop!(
			Pools::withdraw_unbonded(RuntimeOrigin::signed(20), 20, 0),
			PoolsError::<Runtime>::CannotWithdrawAny
		);

		// Progress to era 3 - member should now be able to withdraw.
		Staking::set_era(3);

		// Now can withdraw after NominatorFastUnbondDuration eras (not BondingDuration).
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(20), 20, 0));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::Unbonded { stash: POOL1_BONDED, amount: 10, era: 3 },
				StakingEvent::Withdrawn { stash: POOL1_BONDED, amount: 10 },
			]
		);
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Withdrawn { member: 20, pool_id: 1, points: 10, balance: 10 },
				PoolsEvent::MemberRemoved { pool_id: 1, member: 20, released_balance: 0 },
			]
		);

		// Member 20 is fully withdrawn.
		assert!(PoolMembers::<Runtime>::get(20).is_none());
	});
}

/// When `AreNominatorsSlashable` is false, pool members are NOT slashed even when the
/// validator they nominated is slashed.
#[test]
fn pool_members_not_slashed_when_nominators_not_slashable() {
	new_test_ext().execute_with(|| {
		// Set nominators as not slashable.
		AreNominatorsSlashable::<Runtime>::put(false);
		assert!(!AreNominatorsSlashable::<Runtime>::get());

		ExistentialDeposit::set(1);
		assert_eq!(Balances::minimum_balance(), 1);
		assert_active_era(0);

		// Create the pool with depositor (10) bonding 40.
		assert_ok!(Pools::create(RuntimeOrigin::signed(10), 40, 10, 10, 10));

		// Have the pool nominate validator 1.
		assert_ok!(Pools::nominate(RuntimeOrigin::signed(10), 1, vec![1]));

		// Members join.
		assert_ok!(Pools::join(RuntimeOrigin::signed(20), 20, 1));
		assert_ok!(Pools::join(RuntimeOrigin::signed(21), 20, 1));

		let _ = staking_events_since_last_call();
		let _ = pool_events_since_last_call();

		// Progress to era 1 so the pool is in validator 1's exposure.
		Staking::set_era(1);

		// Record pool's total stake before slash.
		let pool_stake_before =
			pallet_staking_async::Ledger::<Runtime>::get(&POOL1_BONDED).unwrap().active;
		assert_eq!(pool_stake_before, 80); // 40 + 20 + 20

		// Slash validator 1 for 50% of their stake.
		pallet_staking_async::slashing::do_slash::<Runtime>(
			&1,  // validator
			500, // slash 500 (50% of validator's 1000)
			&mut Default::default(),
			&mut Default::default(),
			0,
		);

		// Pool's stake should be unchanged - nominators are not slashed.
		let pool_stake_after =
			pallet_staking_async::Ledger::<Runtime>::get(&POOL1_BONDED).unwrap().active;
		assert_eq!(pool_stake_after, pool_stake_before);
		assert_eq!(pool_stake_after, 80);

		// Individual pool members' points should also be unchanged.
		assert_eq!(PoolMembers::<Runtime>::get(10).unwrap().points, 40);
		assert_eq!(PoolMembers::<Runtime>::get(20).unwrap().points, 20);
		assert_eq!(PoolMembers::<Runtime>::get(21).unwrap().points, 20);

		// Members can still unbond and withdraw their full amounts.
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(20), 20, 20));
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(21), 21, 20));

		// Fast forward to withdrawal era (era 1 + NominatorFastUnbondDuration = 1 + 2 = era 3).
		Staking::set_era(3);

		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(20), 20, 0));
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(21), 21, 0));

		// Members got their full amounts back (no slash applied).
		assert!(PoolMembers::<Runtime>::get(20).is_none());
		assert!(PoolMembers::<Runtime>::get(21).is_none());

		// Check that the withdrawn amount was the full 20 each.
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				PoolsEvent::Unbonded { member: 20, pool_id: 1, points: 20, balance: 20, era: 3 },
				PoolsEvent::Unbonded { member: 21, pool_id: 1, points: 20, balance: 20, era: 3 },
				PoolsEvent::Withdrawn { member: 20, pool_id: 1, points: 20, balance: 20 },
				PoolsEvent::MemberRemoved { pool_id: 1, member: 20, released_balance: 0 },
				PoolsEvent::Withdrawn { member: 21, pool_id: 1, points: 20, balance: 20 },
				PoolsEvent::MemberRemoved { pool_id: 1, member: 21, released_balance: 0 },
			]
		);
	});
}

/// Test that pool members still need full bonding duration when `AreNominatorsSlashable` is true.
/// This is a sanity check to ensure the default behavior is preserved.
#[test]
fn pool_members_need_full_bonding_duration_when_nominators_slashable() {
	new_test_ext().execute_with(|| {
		// Ensure nominators are slashable (default).
		assert!(AreNominatorsSlashable::<Runtime>::get());

		assert_eq!(Balances::minimum_balance(), 5);
		assert_active_era(0);
		assert_eq!(BondingDuration::get(), 3);

		// Create the pool.
		assert_ok!(Pools::create(RuntimeOrigin::signed(10), 50, 10, 10, 10));
		assert_ok!(Pools::nominate(RuntimeOrigin::signed(10), 1, vec![1, 2, 3]));

		// Member 20 joins.
		assert_ok!(Pools::join(RuntimeOrigin::signed(20), 10, 1));

		let _ = pool_events_since_last_call();

		// Progress to era 1.
		Staking::set_era(1);

		// Member 20 unbonds.
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(20), 20, 10));

		// The unbond era should be current_era + BondingDuration = 1 + 3 = 4.
		assert_eq!(
			pool_events_since_last_call(),
			vec![
				// Unbond era is 4 (full bonding duration).
				PoolsEvent::Unbonded { member: 20, pool_id: 1, points: 10, balance: 10, era: 4 },
			]
		);

		// Cannot withdraw in era 2 (only 1 era passed).
		Staking::set_era(2);
		assert_noop!(
			Pools::withdraw_unbonded(RuntimeOrigin::signed(20), 20, 0),
			PoolsError::<Runtime>::CannotWithdrawAny
		);

		// Cannot withdraw in era 3 (only 2 eras passed).
		Staking::set_era(3);
		assert_noop!(
			Pools::withdraw_unbonded(RuntimeOrigin::signed(20), 20, 0),
			PoolsError::<Runtime>::CannotWithdrawAny
		);

		// Can finally withdraw in era 4 (full bonding duration passed).
		Staking::set_era(4);
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(20), 20, 0));
		assert!(PoolMembers::<Runtime>::get(20).is_none());
	});
}

#[test]
fn do_claim_trapped_balance_applies_pending_slash_first() {
	new_test_ext().execute_with(|| {
		let alice = 100;
		assert_ok!(Balances::mint_into(&alice, 300));
		assert_ok!(Balances::mint_into(&10, 250));

		// Create pool and join
		assert_ok!(Pools::create(RuntimeOrigin::signed(10), 200, 10, 10, 10));
		assert_ok!(Pools::nominate(RuntimeOrigin::signed(10), 1, vec![1, 2, 3]));
		assert_ok!(Pools::join(RuntimeOrigin::signed(alice), 100, 1));

		let pool_account = Pools::generate_bonded_account(1);

		// Apply a slash to the pool
		Staking::set_era(1);
		pallet_staking_async::slashing::do_slash::<Runtime>(
			&pool_account,
			50,
			&mut Default::default(),
			&mut Default::default(),
			1,
		);

		// Verify pool and member have pending slash
		assert!(Pools::api_pool_pending_slash(1) > 0);
		assert!(Pools::api_member_pending_slash(alice) > 0);

		// do_claim_trapped_balance applies the slash as a side effect, even though there's
		// no trapped balance (returns Ok as a no-op for the trapped balance part).
		assert_ok!(Pools::do_claim_trapped_balance(&alice));

		// Verify slash was applied
		assert_eq!(Pools::api_member_pending_slash(alice), 0);
	});
}
