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

//! Benchmarks for the nomination pools coupled with the staking and bags list pallets.

use alloc::{vec, vec::Vec};
use frame_benchmarking::v2::*;
use frame_election_provider_support::SortedListProvider;
use frame_support::{
	assert_ok, ensure,
	traits::{
		fungible::{Inspect, Mutate, Unbalanced},
		tokens::Preservation,
		Get, Imbalance,
	},
};
use frame_system::RawOrigin as RuntimeOrigin;
use pallet_nomination_pools::{
	adapter::{Member, Pool, StakeStrategy, StakeStrategyType},
	BalanceOf, BondExtra, BondedPoolInner, BondedPools, ClaimPermission, ClaimPermissions,
	Commission, CommissionChangeRate, CommissionClaimPermission, ConfigOp, GlobalMaxCommission,
	MaxPoolMembers, MaxPoolMembersPerPool, MaxPools, Metadata, MinCreateBond, MinJoinBond,
	Pallet as Pools, PoolId, PoolMembers, PoolRoles, PoolState, RewardPools, SubPoolsStorage,
};
use pallet_staking_async::MaxNominationsOf;
use sp_runtime::{
	traits::{Bounded, StaticLookup, Zero},
	Perbill,
};
use sp_staking::{EraIndex, StakingInterface, StakingUnchecked};
// `frame_benchmarking::benchmarks!` macro needs this
use pallet_nomination_pools::Call;

type CurrencyOf<T> = <T as pallet_nomination_pools::Config>::Currency;

const USER_SEED: u32 = 0;
const MAX_SPANS: u32 = 100;

pub(crate) type VoterBagsListInstance = pallet_bags_list::Instance1;
pub trait Config:
	pallet_nomination_pools::Config
	+ pallet_staking_async::Config
	+ pallet_bags_list::Config<VoterBagsListInstance>
{
}

pub struct Pallet<T: Config>(Pools<T>);

fn create_funded_user_with_balance<T: pallet_nomination_pools::Config>(
	string: &'static str,
	n: u32,
	balance: BalanceOf<T>,
) -> T::AccountId {
	let user = account(string, n, USER_SEED);
	T::Currency::set_balance(&user, balance);
	user
}

// Create a funded validator and register it as such, so that pools can nominate it.
fn create_validator<T: Config>(n: u32, balance: BalanceOf<T>) -> T::AccountId
where
	pallet_staking_async::BalanceOf<T>: From<u128>,
	BalanceOf<T>: Into<u128>,
{
	let validator = create_funded_user_with_balance::<T>("validator", n, balance);
	let stake: pallet_staking_async::BalanceOf<T> = (balance.into() / 2).into();
	pallet_staking_async::Pallet::<T>::bond(
		RuntimeOrigin::Signed(validator.clone()).into(),
		stake,
		pallet_staking_async::RewardDestination::Staked,
	)
	.expect("validator can bond; qed");
	pallet_staking_async::Pallet::<T>::validate(
		RuntimeOrigin::Signed(validator.clone()).into(),
		Default::default(),
	)
	.expect("validator can validate; qed");
	validator
}

// Create a bonded pool account, bonding `balance` and giving the account `balance * 2` free
// balance.
fn create_pool_account<T: pallet_nomination_pools::Config>(
	n: u32,
	balance: BalanceOf<T>,
	commission: Option<Perbill>,
) -> (T::AccountId, T::AccountId) {
	let ed = CurrencyOf::<T>::minimum_balance();
	let pool_creator: T::AccountId =
		create_funded_user_with_balance::<T>("pool_creator", n, ed + balance * 2u32.into());
	let pool_creator_lookup = T::Lookup::unlookup(pool_creator.clone());

	Pools::<T>::create(
		RuntimeOrigin::Signed(pool_creator.clone()).into(),
		balance,
		pool_creator_lookup.clone(),
		pool_creator_lookup.clone(),
		pool_creator_lookup,
	)
	.unwrap();

	if let Some(c) = commission {
		let pool_id = pallet_nomination_pools::LastPoolId::<T>::get();
		Pools::<T>::set_commission(
			RuntimeOrigin::Signed(pool_creator.clone()).into(),
			pool_id,
			Some((c, pool_creator.clone())),
		)
		.expect("pool just created, commission can be set by root; qed");
	}

	let pool_account = pallet_nomination_pools::BondedPools::<T>::iter()
		.find(|(_, bonded_pool)| bonded_pool.roles.depositor == pool_creator)
		.map(|(pool_id, _)| Pools::<T>::generate_bonded_account(pool_id))
		.expect("pool_creator created a pool above");

	(pool_creator, pool_account)
}

fn migrate_to_transfer_stake<T: Config>(pool_id: PoolId) {
	if T::StakeAdapter::strategy_type() == StakeStrategyType::Transfer {
		// should already be in the correct strategy
		return;
	}
	let pool_acc = Pools::<T>::generate_bonded_account(pool_id);
	// drop the agent and its associated delegators .
	T::StakeAdapter::remove_as_agent(Pool::from(pool_acc.clone()));

	// tranfer funds from all members to the pool account.
	PoolMembers::<T>::iter()
		.filter(|(_, member)| member.pool_id == pool_id)
		.for_each(|(member_acc, member)| {
			let member_balance = member.total_balance();
			<T as pallet_nomination_pools::Config>::Currency::transfer(
				&member_acc,
				&pool_acc,
				member_balance,
				Preservation::Preserve,
			)
			.expect("member should have enough balance to transfer");
		});

	// Pool needs to have ED balance free to stake so give it some.
	// Note: we didn't require ED until pallet-staking migrated from locks to holds.
	let _ = CurrencyOf::<T>::mint_into(&pool_acc, CurrencyOf::<T>::minimum_balance());

	pallet_staking_async::Pallet::<T>::migrate_to_direct_staker(&pool_acc);
}

fn vote_to_balance<T: pallet_nomination_pools::Config>(
	vote: u64,
) -> Result<BalanceOf<T>, &'static str> {
	vote.try_into().map_err(|_| "could not convert u64 to Balance")
}

#[allow(unused)]
struct ListScenario<T: pallet_nomination_pools::Config> {
	/// Stash/Controller that is expected to be moved.
	origin1: T::AccountId,
	creator1: T::AccountId,
	dest_weight: BalanceOf<T>,
	origin1_member: Option<T::AccountId>,
}

impl<T: Config> ListScenario<T> {
	/// An expensive scenario for bags-list implementation:
	///
	/// - the node to be updated (r) is the head of a bag that has at least one other node. The bag
	///   itself will need to be read and written to update its head. The node pointed to by r.next
	///   will need to be read and written as it will need to have its prev pointer updated. Note
	///   that there are two other worst case scenarios for bag removal: 1) the node is a tail and
	///   2) the node is a middle node with prev and next; all scenarios end up with the same number
	///   of storage reads and writes.
	///
	/// - the destination bag has at least one node, which will need its next pointer updated.
	pub(crate) fn new(origin_weight: BalanceOf<T>, is_increase: bool) -> Result<Self, &'static str>
	where
		pallet_staking_async::BalanceOf<T>: From<u128>,
		BalanceOf<T>: Into<u128>,
	{
		ensure!(!origin_weight.is_zero(), "origin weight must be greater than 0");

		ensure!(
			pallet_nomination_pools::MaxPools::<T>::get().unwrap_or(0) >= 3,
			"must allow at least three pools for benchmarks"
		);

		// Burn the entire issuance.
		CurrencyOf::<T>::set_total_issuance(Zero::zero());

		// Create a real validator the pools can nominate.
		let validator =
			create_validator::<T>(0, CurrencyOf::<T>::minimum_balance() * 1000u32.into());

		// Create accounts with the origin weight
		let (pool_creator1, pool_origin1) =
			create_pool_account::<T>(USER_SEED + 1, origin_weight, Some(Perbill::from_percent(50)));

		T::StakeAdapter::nominate(Pool::from(pool_origin1.clone()), vec![validator.clone()])?;

		let (_, pool_origin2) =
			create_pool_account::<T>(USER_SEED + 2, origin_weight, Some(Perbill::from_percent(50)));

		T::StakeAdapter::nominate(Pool::from(pool_origin2.clone()), vec![validator.clone()])?;

		// Find a destination weight that will trigger the worst case scenario
		let dest_weight_as_vote =
			<T as pallet_staking_async::Config>::VoterList::score_update_worst_case(
				&pool_origin1,
				is_increase,
			);

		let dest_weight: BalanceOf<T> =
			dest_weight_as_vote.try_into().map_err(|_| "could not convert u64 to Balance")?;

		// Create an account with the worst case destination weight
		let (_, pool_dest1) =
			create_pool_account::<T>(USER_SEED + 3, dest_weight, Some(Perbill::from_percent(50)));

		T::StakeAdapter::nominate(Pool::from(pool_dest1.clone()), vec![validator.clone()])?;

		let weight_of = pallet_staking_async::Pallet::<T>::weight_of_fn();
		assert_eq!(vote_to_balance::<T>(weight_of(&pool_origin1)).unwrap(), origin_weight);
		assert_eq!(vote_to_balance::<T>(weight_of(&pool_origin2)).unwrap(), origin_weight);
		assert_eq!(vote_to_balance::<T>(weight_of(&pool_dest1)).unwrap(), dest_weight);

		Ok(ListScenario {
			origin1: pool_origin1,
			creator1: pool_creator1,
			dest_weight,
			origin1_member: None,
		})
	}

	fn add_joiner(mut self, amount: BalanceOf<T>) -> Self {
		let amount = MinJoinBond::<T>::get()
			.max(CurrencyOf::<T>::minimum_balance())
			// Max `amount` with minimum thresholds for account balance and joining a pool
			// to ensure 1) the user can be created and 2) can join the pool
			.max(amount);

		let joiner: T::AccountId = account("joiner", USER_SEED, 0);
		self.origin1_member = Some(joiner.clone());
		CurrencyOf::<T>::set_balance(&joiner, amount * 2u32.into());

		let original_bonded = T::StakeAdapter::active_stake(Pool::from(self.origin1.clone()));

		// Unbond `amount` from the underlying pool account so when the member joins
		// we will maintain `current_bonded`.
		T::StakeAdapter::unbond(Pool::from(self.origin1.clone()), amount)
			.expect("the pool was created in `Self::new`.");

		// Account pool points for the unbonded balance.
		BondedPools::<T>::mutate(&1, |maybe_pool| {
			maybe_pool.as_mut().map(|pool| pool.points -= amount)
		});

		Pools::<T>::join(RuntimeOrigin::Signed(joiner.clone()).into(), amount, 1).unwrap();

		// check that the vote weight is still the same as the original bonded
		let weight_of = pallet_staking_async::Pallet::<T>::weight_of_fn();
		assert_eq!(vote_to_balance::<T>(weight_of(&self.origin1)).unwrap(), original_bonded);

		// check the member was added correctly
		let member = PoolMembers::<T>::get(&joiner).unwrap();
		assert_eq!(member.points, amount);
		assert_eq!(member.pool_id, 1);

		self
	}
}

#[benchmarks(
	where
		T: pallet_staking_async::Config,
		pallet_staking_async::BalanceOf<T>: From<u128>,
		BalanceOf<T>: Into<u128>,
)]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn join() {
		let origin_weight = Pools::<T>::depositor_min_bond() * 2u32.into();

		// setup the worst case list scenario.
		let scenario = ListScenario::<T>::new(origin_weight, true).unwrap();
		assert_eq!(
			T::StakeAdapter::active_stake(Pool::from(scenario.origin1.clone())),
			origin_weight
		);

		let max_additional = scenario.dest_weight - origin_weight;
		let joiner_free = CurrencyOf::<T>::minimum_balance() + max_additional;

		let joiner: T::AccountId = create_funded_user_with_balance::<T>("joiner", 0, joiner_free);

		whitelist_account!(joiner);

		#[extrinsic_call]
		_(RuntimeOrigin::Signed(joiner.clone()), max_additional, 1);

		assert_eq!(CurrencyOf::<T>::balance(&joiner), joiner_free - max_additional);
		assert_eq!(
			T::StakeAdapter::active_stake(Pool::from(scenario.origin1)),
			scenario.dest_weight
		);
	}

	#[benchmark]
	fn bond_extra_transfer() {
		let origin_weight = Pools::<T>::depositor_min_bond() * 2u32.into();
		let scenario = ListScenario::<T>::new(origin_weight, true).unwrap();
		let extra = scenario.dest_weight - origin_weight;

		// The creator bonds `extra` from free balance. Let's top it up first.
		let _ = CurrencyOf::<T>::mint_into(
			&scenario.creator1,
			extra + CurrencyOf::<T>::minimum_balance(),
		);

		// creator of the src pool will bond-extra, bumping itself to dest bag.

		#[extrinsic_call]
		bond_extra(RuntimeOrigin::Signed(scenario.creator1.clone()), BondExtra::FreeBalance(extra));

		assert!(
			T::StakeAdapter::active_stake(Pool::from(scenario.origin1)) >= scenario.dest_weight
		);
	}

	#[benchmark]
	fn bond_extra_other() {
		let claimer: T::AccountId = account("claimer", USER_SEED + 4, 0);

		let origin_weight = Pools::<T>::depositor_min_bond() * 2u32.into();
		let scenario = ListScenario::<T>::new(origin_weight, true).unwrap();
		let extra = (scenario.dest_weight - origin_weight).max(CurrencyOf::<T>::minimum_balance());

		// set claim preferences to `PermissionlessAll` to any account to bond extra on member's
		// behalf.
		let _ = Pools::<T>::set_claim_permission(
			RuntimeOrigin::Signed(scenario.creator1.clone()).into(),
			ClaimPermission::PermissionlessAll,
		);

		// transfer exactly `extra` to the depositor of the src pool (1),
		let reward_account1 = Pools::<T>::generate_reward_account(1);
		assert!(extra >= CurrencyOf::<T>::minimum_balance());
		let _ = CurrencyOf::<T>::mint_into(&reward_account1, extra);

		#[extrinsic_call]
		_(
			RuntimeOrigin::Signed(claimer),
			T::Lookup::unlookup(scenario.creator1.clone()),
			BondExtra::Rewards,
		);

		// commission of 50% deducted here.
		assert!(
			T::StakeAdapter::active_stake(Pool::from(scenario.origin1)) >=
				scenario.dest_weight / 2u32.into()
		);
	}

	#[benchmark]
	fn claim_payout() {
		let claimer: T::AccountId = account("claimer", USER_SEED + 4, 0);
		let commission = Perbill::from_percent(50);
		let origin_weight = Pools::<T>::depositor_min_bond() * 2u32.into();
		let ed = CurrencyOf::<T>::minimum_balance();
		let (depositor, _pool_account) =
			create_pool_account::<T>(0, origin_weight, Some(commission));
		let reward_account = Pools::<T>::generate_reward_account(1);

		// Send funds to the reward account of the pool
		CurrencyOf::<T>::set_balance(&reward_account, ed + origin_weight);

		// set claim preferences to `PermissionlessAll` so any account can claim rewards on member's
		// behalf.
		let _ = Pools::<T>::set_claim_permission(
			RuntimeOrigin::Signed(depositor.clone()).into(),
			ClaimPermission::PermissionlessAll,
		);

		// Sanity check
		assert_eq!(CurrencyOf::<T>::balance(&depositor), origin_weight);

		whitelist_account!(depositor);

		#[extrinsic_call]
		claim_payout_other(RuntimeOrigin::Signed(claimer), depositor.clone());

		assert_eq!(
			CurrencyOf::<T>::balance(&depositor),
			origin_weight + commission * origin_weight
		);
		assert_eq!(CurrencyOf::<T>::balance(&reward_account), ed + commission * origin_weight);
	}

	#[benchmark]
	fn unbond() {
		<T::StakeAdapter as StakeStrategy>::CoreStaking::set_era(0);

		// The weight the nominator will start at. The value used here is expected to be
		// significantly higher than the first position in a list (e.g. the first bag threshold).
		let origin_weight = Pools::<T>::depositor_min_bond() * 200u32.into();
		let scenario = ListScenario::<T>::new(origin_weight, false).unwrap();
		let amount = origin_weight - scenario.dest_weight;

		let scenario = scenario.add_joiner(amount);
		let member_id = scenario.origin1_member.unwrap().clone();
		let member_id_lookup = T::Lookup::unlookup(member_id.clone());
		let all_points = PoolMembers::<T>::get(&member_id).unwrap().points;
		whitelist_account!(member_id);

		#[extrinsic_call]
		_(RuntimeOrigin::Signed(member_id.clone()), member_id_lookup, all_points);

		let bonded_after = T::StakeAdapter::active_stake(Pool::from(scenario.origin1));
		// We at least went down to the destination bag
		assert!(bonded_after <= scenario.dest_weight);
		let member = PoolMembers::<T>::get(&member_id).unwrap();
		assert_eq!(
			member.unbonding_eras.keys().cloned().collect::<Vec<_>>(),
			vec![0 + T::StakeAdapter::bonding_duration()]
		);
		assert_eq!(member.unbonding_eras.values().cloned().collect::<Vec<_>>(), vec![all_points]);
	}

	#[benchmark]
	fn pool_withdraw_unbonded(s: Linear<0, MAX_SPANS>) {
		<T::StakeAdapter as StakeStrategy>::CoreStaking::set_era(0);

		let min_create_bond = Pools::<T>::depositor_min_bond();
		let (_depositor, pool_account) = create_pool_account::<T>(0, min_create_bond, None);

		// Add a new member
		let min_join_bond = MinJoinBond::<T>::get().max(CurrencyOf::<T>::minimum_balance());
		let joiner = create_funded_user_with_balance::<T>("joiner", 0, min_join_bond * 2u32.into());
		Pools::<T>::join(RuntimeOrigin::Signed(joiner.clone()).into(), min_join_bond, 1).unwrap();

		// Sanity check join worked
		assert_eq!(
			T::StakeAdapter::active_stake(Pool::from(pool_account.clone())),
			min_create_bond + min_join_bond
		);
		assert_eq!(CurrencyOf::<T>::balance(&joiner), min_join_bond);

		// Unbond the new member
		Pools::<T>::fully_unbond(RuntimeOrigin::Signed(joiner.clone()).into(), joiner.clone())
			.unwrap();

		// Sanity check that unbond worked
		assert_eq!(
			T::StakeAdapter::active_stake(Pool::from(pool_account.clone())),
			min_create_bond
		);
		assert_eq!(
			pallet_staking_async::Ledger::<T>::get(&pool_account).unwrap().unlocking.len(),
			1
		);
		// Set the current era
		<T::StakeAdapter as StakeStrategy>::CoreStaking::set_era(EraIndex::max_value());

		whitelist_account!(pool_account);

		#[extrinsic_call]
		_(RuntimeOrigin::Signed(pool_account.clone()), 1, s);

		// The joiners funds didn't change
		assert_eq!(CurrencyOf::<T>::balance(&joiner), min_join_bond);
		// The unlocking chunk was removed
		assert_eq!(
			pallet_staking_async::Ledger::<T>::get(pool_account).unwrap().unlocking.len(),
			0
		);
	}

	#[benchmark]
	fn withdraw_unbonded_update(s: Linear<0, MAX_SPANS>) {
		let min_create_bond = Pools::<T>::depositor_min_bond();
		let (_depositor, pool_account) = create_pool_account::<T>(0, min_create_bond, None);

		// Add a new member
		let min_join_bond = MinJoinBond::<T>::get().max(CurrencyOf::<T>::minimum_balance());
		let joiner = create_funded_user_with_balance::<T>("joiner", 0, min_join_bond * 2u32.into());
		let joiner_lookup = T::Lookup::unlookup(joiner.clone());
		Pools::<T>::join(RuntimeOrigin::Signed(joiner.clone()).into(), min_join_bond, 1).unwrap();

		// Sanity check join worked
		assert_eq!(
			T::StakeAdapter::active_stake(Pool::from(pool_account.clone())),
			min_create_bond + min_join_bond
		);
		assert_eq!(CurrencyOf::<T>::balance(&joiner), min_join_bond);

		// Unbond the new member
		<T::StakeAdapter as StakeStrategy>::CoreStaking::set_era(0);
		Pools::<T>::fully_unbond(RuntimeOrigin::Signed(joiner.clone()).into(), joiner.clone())
			.unwrap();

		// Sanity check that unbond worked
		assert_eq!(
			T::StakeAdapter::active_stake(Pool::from(pool_account.clone())),
			min_create_bond
		);
		assert_eq!(
			pallet_staking_async::Ledger::<T>::get(&pool_account).unwrap().unlocking.len(),
			1
		);

		// Set the current era to ensure we can withdraw unbonded funds
		<T::StakeAdapter as StakeStrategy>::CoreStaking::set_era(EraIndex::max_value());

		whitelist_account!(joiner);

		#[extrinsic_call]
		withdraw_unbonded(RuntimeOrigin::Signed(joiner.clone()), joiner_lookup, s);

		assert_eq!(CurrencyOf::<T>::balance(&joiner), min_join_bond * 2u32.into());
		// The unlocking chunk was removed
		assert_eq!(
			pallet_staking_async::Ledger::<T>::get(&pool_account).unwrap().unlocking.len(),
			0
		);
	}

	#[benchmark]
	fn withdraw_unbonded_kill(s: Linear<0, MAX_SPANS>) {
		let min_create_bond = Pools::<T>::depositor_min_bond();
		let (depositor, pool_account) = create_pool_account::<T>(0, min_create_bond, None);
		let depositor_lookup = T::Lookup::unlookup(depositor.clone());

		// We set the pool to the destroying state so the depositor can leave
		BondedPools::<T>::try_mutate(&1, |maybe_bonded_pool| {
			maybe_bonded_pool.as_mut().ok_or(()).map(|bonded_pool| {
				bonded_pool.state = PoolState::Destroying;
			})
		})
		.unwrap();

		// Unbond the creator
		<T::StakeAdapter as StakeStrategy>::CoreStaking::set_era(0);
		// Simulate some rewards so we can check if the rewards storage is cleaned up. We check this
		// here to ensure the complete flow for destroying a pool works - the reward pool account
		// should never exist by time the depositor withdraws so we test that it gets cleaned
		// up when unbonding.
		let reward_account = Pools::<T>::generate_reward_account(1);
		assert!(frame_system::Account::<T>::contains_key(&reward_account));
		Pools::<T>::fully_unbond(
			RuntimeOrigin::Signed(depositor.clone()).into(),
			depositor.clone(),
		)
		.unwrap();

		// Sanity check that unbond worked
		assert_eq!(T::StakeAdapter::active_stake(Pool::from(pool_account.clone())), Zero::zero());
		assert_eq!(
			T::StakeAdapter::total_balance(Pool::from(pool_account.clone())),
			Some(min_create_bond)
		);
		assert_eq!(
			pallet_staking_async::Ledger::<T>::get(&pool_account).unwrap().unlocking.len(),
			1
		);

		// Set the current era to ensure we can withdraw unbonded funds
		<T::StakeAdapter as StakeStrategy>::CoreStaking::set_era(EraIndex::max_value());

		// Some last checks that storage items we expect to get cleaned up are present
		assert!(pallet_staking_async::Ledger::<T>::contains_key(&pool_account));
		assert!(BondedPools::<T>::contains_key(&1));
		assert!(SubPoolsStorage::<T>::contains_key(&1));
		assert!(RewardPools::<T>::contains_key(&1));
		assert!(PoolMembers::<T>::contains_key(&depositor));
		assert!(frame_system::Account::<T>::contains_key(&reward_account));

		whitelist_account!(depositor);

		#[extrinsic_call]
		withdraw_unbonded(RuntimeOrigin::Signed(depositor.clone()), depositor_lookup, s);

		// Pool removal worked
		assert!(!pallet_staking_async::Ledger::<T>::contains_key(&pool_account));
		assert!(!BondedPools::<T>::contains_key(&1));
		assert!(!SubPoolsStorage::<T>::contains_key(&1));
		assert!(!RewardPools::<T>::contains_key(&1));
		assert!(!PoolMembers::<T>::contains_key(&depositor));
		assert!(!frame_system::Account::<T>::contains_key(&pool_account));
		assert!(!frame_system::Account::<T>::contains_key(&reward_account));

		// Funds where transferred back correctly
		assert_eq!(
			CurrencyOf::<T>::balance(&depositor),
			// gets bond back + rewards collecting when unbonding
			min_create_bond * 2u32.into() + CurrencyOf::<T>::minimum_balance()
		);
	}

	#[benchmark]
	fn create() {
		let min_create_bond = Pools::<T>::depositor_min_bond();
		let depositor: T::AccountId = account("depositor", USER_SEED, 0);
		let depositor_lookup = T::Lookup::unlookup(depositor.clone());

		// Give the depositor some balance to bond
		// it needs to transfer min balance to reward account as well so give additional min
		// balance.
		CurrencyOf::<T>::set_balance(
			&depositor,
			min_create_bond + CurrencyOf::<T>::minimum_balance() * 2u32.into(),
		);
		// Make sure no Pools exist at a pre-condition for our verify checks
		assert_eq!(RewardPools::<T>::count(), 0);
		assert_eq!(BondedPools::<T>::count(), 0);

		whitelist_account!(depositor);

		#[extrinsic_call]
		_(
			RuntimeOrigin::Signed(depositor.clone()),
			min_create_bond,
			depositor_lookup.clone(),
			depositor_lookup.clone(),
			depositor_lookup,
		);

		assert_eq!(RewardPools::<T>::count(), 1);
		assert_eq!(BondedPools::<T>::count(), 1);
		let (_, new_pool) = BondedPools::<T>::iter().next().unwrap();
		assert_eq!(
			new_pool,
			BondedPoolInner {
				commission: Commission::default(),
				member_counter: 1,
				points: min_create_bond,
				roles: PoolRoles {
					depositor: depositor.clone(),
					root: Some(depositor.clone()),
					nominator: Some(depositor.clone()),
					bouncer: Some(depositor.clone()),
				},
				state: PoolState::Open,
			}
		);
		assert_eq!(
			T::StakeAdapter::active_stake(Pool::from(Pools::<T>::generate_bonded_account(1))),
			min_create_bond
		);
	}

	#[benchmark]
	fn nominate(n: Linear<1, { MaxNominationsOf::<T>::get() }>) {
		// Create a pool
		let min_create_bond = Pools::<T>::depositor_min_bond() * 2u32.into();
		let (depositor, _pool_account) = create_pool_account::<T>(0, min_create_bond, None);

		// Create real validators to nominate.
		let validator_balance = CurrencyOf::<T>::minimum_balance() * 1000u32.into();
		let validators: Vec<_> =
			(0..n).map(|i| create_validator::<T>(i, validator_balance)).collect();

		whitelist_account!(depositor);

		#[extrinsic_call]
		_(RuntimeOrigin::Signed(depositor.clone()), 1, validators);

		assert_eq!(RewardPools::<T>::count(), 1);
		assert_eq!(BondedPools::<T>::count(), 1);
		let (_, new_pool) = BondedPools::<T>::iter().next().unwrap();
		assert_eq!(
			new_pool,
			BondedPoolInner {
				commission: Commission::default(),
				member_counter: 1,
				points: min_create_bond,
				roles: PoolRoles {
					depositor: depositor.clone(),
					root: Some(depositor.clone()),
					nominator: Some(depositor.clone()),
					bouncer: Some(depositor.clone()),
				},
				state: PoolState::Open,
			}
		);
		assert_eq!(
			T::StakeAdapter::active_stake(Pool::from(Pools::<T>::generate_bonded_account(1))),
			min_create_bond
		);
	}

	#[benchmark]
	fn set_state() {
		// Create a pool
		let min_create_bond = Pools::<T>::depositor_min_bond();
		// Don't need the accounts, but the pool.
		let _ = create_pool_account::<T>(0, min_create_bond, None);
		BondedPools::<T>::mutate(&1, |maybe_pool| {
			// Force the pool into an invalid state
			maybe_pool.as_mut().map(|pool| pool.points = min_create_bond * 10u32.into());
		});

		let caller = account("caller", 0, USER_SEED);
		whitelist_account!(caller);

		#[extrinsic_call]
		_(RuntimeOrigin::Signed(caller), 1, PoolState::Destroying);

		assert_eq!(BondedPools::<T>::get(1).unwrap().state, PoolState::Destroying);
	}

	#[benchmark]
	fn set_metadata(
		n: Linear<1, { <T as pallet_nomination_pools::Config>::MaxMetadataLen::get() }>,
	) {
		// Create a pool
		let (depositor, _pool_account) =
			create_pool_account::<T>(0, Pools::<T>::depositor_min_bond() * 2u32.into(), None);

		// Create metadata of the max possible size
		let metadata: Vec<u8> = (0..n).map(|_| 42).collect();

		whitelist_account!(depositor);

		#[extrinsic_call]
		_(RuntimeOrigin::Signed(depositor), 1, metadata.clone());
		assert_eq!(Metadata::<T>::get(&1), metadata);
	}

	#[benchmark]
	fn set_configs() {
		#[extrinsic_call]
		_(
			RuntimeOrigin::Root,
			ConfigOp::Set(BalanceOf::<T>::max_value()),
			ConfigOp::Set(BalanceOf::<T>::max_value()),
			ConfigOp::Set(u32::MAX),
			ConfigOp::Set(u32::MAX),
			ConfigOp::Set(u32::MAX),
			ConfigOp::Set(Perbill::max_value()),
		);

		assert_eq!(MinJoinBond::<T>::get(), BalanceOf::<T>::max_value());
		assert_eq!(MinCreateBond::<T>::get(), BalanceOf::<T>::max_value());
		assert_eq!(MaxPools::<T>::get(), Some(u32::MAX));
		assert_eq!(MaxPoolMembers::<T>::get(), Some(u32::MAX));
		assert_eq!(MaxPoolMembersPerPool::<T>::get(), Some(u32::MAX));
		assert_eq!(GlobalMaxCommission::<T>::get(), Some(Perbill::max_value()));
	}

	#[benchmark]
	fn update_roles() {
		let first_id = pallet_nomination_pools::LastPoolId::<T>::get() + 1;
		let (root, _) =
			create_pool_account::<T>(0, Pools::<T>::depositor_min_bond() * 2u32.into(), None);
		let random: T::AccountId =
			account("but is anything really random in computers..?", 0, USER_SEED);

		#[extrinsic_call]
		_(
			RuntimeOrigin::Signed(root.clone()),
			first_id,
			ConfigOp::Set(random.clone()),
			ConfigOp::Set(random.clone()),
			ConfigOp::Set(random.clone()),
		);
		assert_eq!(
			pallet_nomination_pools::BondedPools::<T>::get(first_id).unwrap().roles,
			pallet_nomination_pools::PoolRoles {
				depositor: root,
				nominator: Some(random.clone()),
				bouncer: Some(random.clone()),
				root: Some(random),
			},
		)
	}

	#[benchmark]
	fn chill() {
		// Create a pool
		let (depositor, pool_account) =
			create_pool_account::<T>(0, Pools::<T>::depositor_min_bond() * 2u32.into(), None);

		// Nominate with the pool.
		let validator_balance = CurrencyOf::<T>::minimum_balance() * 1000u32.into();
		let validators: Vec<_> = (0..MaxNominationsOf::<T>::get())
			.map(|i| create_validator::<T>(i, validator_balance))
			.collect();

		assert_ok!(T::StakeAdapter::nominate(Pool::from(pool_account.clone()), validators));
		assert!(T::StakeAdapter::nominations(Pool::from(pool_account.clone())).is_some());

		whitelist_account!(depositor);

		#[extrinsic_call]
		_(RuntimeOrigin::Signed(depositor.clone()), 1);

		assert!(T::StakeAdapter::nominations(Pool::from(pool_account.clone())).is_none());
	}

	#[benchmark]
	fn set_commission() {
		// Create a pool - do not set a commission yet.
		let (depositor, _pool_account) =
			create_pool_account::<T>(0, Pools::<T>::depositor_min_bond() * 2u32.into(), None);
		// set a max commission
		Pools::<T>::set_commission_max(
			RuntimeOrigin::Signed(depositor.clone()).into(),
			1u32.into(),
			Perbill::from_percent(50),
		)
		.unwrap();
		// set a change rate
		Pools::<T>::set_commission_change_rate(
			RuntimeOrigin::Signed(depositor.clone()).into(),
			1u32.into(),
			CommissionChangeRate {
				max_increase: Perbill::from_percent(20),
				min_delay: 0u32.into(),
			},
		)
		.unwrap();
		// set a claim permission to an account.
		Pools::<T>::set_commission_claim_permission(
			RuntimeOrigin::Signed(depositor.clone()).into(),
			1u32.into(),
			Some(CommissionClaimPermission::Account(depositor.clone())),
		)
		.unwrap();

		#[extrinsic_call]
		_(
			RuntimeOrigin::Signed(depositor.clone()),
			1u32.into(),
			Some((Perbill::from_percent(20), depositor.clone())),
		);

		assert_eq!(
			BondedPools::<T>::get(1).unwrap().commission,
			Commission {
				current: Some((Perbill::from_percent(20), depositor.clone())),
				max: Some(Perbill::from_percent(50)),
				change_rate: Some(CommissionChangeRate {
					max_increase: Perbill::from_percent(20),
					min_delay: 0u32.into()
				}),
				throttle_from: Some(0u32.into()),
				claim_permission: Some(CommissionClaimPermission::Account(depositor)),
			}
		);
	}

	#[benchmark]
	fn set_commission_max() {
		// Create a pool, setting a commission that will update when max commission is set.
		let (depositor, _pool_account) = create_pool_account::<T>(
			0,
			Pools::<T>::depositor_min_bond() * 2u32.into(),
			Some(Perbill::from_percent(50)),
		);

		#[extrinsic_call]
		_(RuntimeOrigin::Signed(depositor.clone()), 1u32.into(), Perbill::from_percent(50));

		assert_eq!(
			BondedPools::<T>::get(1).unwrap().commission,
			Commission {
				current: Some((Perbill::from_percent(50), depositor)),
				max: Some(Perbill::from_percent(50)),
				change_rate: None,
				throttle_from: Some(0u32.into()),
				claim_permission: None,
			}
		);
	}

	#[benchmark]
	fn set_commission_change_rate() {
		// Create a pool
		let (depositor, _pool_account) =
			create_pool_account::<T>(0, Pools::<T>::depositor_min_bond() * 2u32.into(), None);

		#[extrinsic_call]
		_(
			RuntimeOrigin::Signed(depositor.clone()),
			1u32.into(),
			CommissionChangeRate {
				max_increase: Perbill::from_percent(50),
				min_delay: 1000u32.into(),
			},
		);

		assert_eq!(
			BondedPools::<T>::get(1).unwrap().commission,
			Commission {
				current: None,
				max: None,
				change_rate: Some(CommissionChangeRate {
					max_increase: Perbill::from_percent(50),
					min_delay: 1000u32.into(),
				}),
				throttle_from: Some(0_u32.into()),
				claim_permission: None,
			}
		);
	}

	#[benchmark]
	fn set_commission_claim_permission() {
		// Create a pool.
		let (depositor, _pool_account) =
			create_pool_account::<T>(0, Pools::<T>::depositor_min_bond() * 2u32.into(), None);

		#[extrinsic_call]
		_(
			RuntimeOrigin::Signed(depositor.clone()),
			1u32.into(),
			Some(CommissionClaimPermission::Account(depositor.clone())),
		);

		assert_eq!(
			BondedPools::<T>::get(1).unwrap().commission,
			Commission {
				current: None,
				max: None,
				change_rate: None,
				throttle_from: None,
				claim_permission: Some(CommissionClaimPermission::Account(depositor)),
			}
		);
	}

	#[benchmark]
	fn set_claim_permission() {
		// Create a pool
		let min_create_bond = Pools::<T>::depositor_min_bond();
		let (_depositor, pool_account) = create_pool_account::<T>(0, min_create_bond, None);

		// Join pool
		let min_join_bond = MinJoinBond::<T>::get().max(CurrencyOf::<T>::minimum_balance());
		let joiner = create_funded_user_with_balance::<T>("joiner", 0, min_join_bond * 4u32.into());
		Pools::<T>::join(RuntimeOrigin::Signed(joiner.clone()).into(), min_join_bond, 1).unwrap();

		// Sanity check join worked
		assert_eq!(
			T::StakeAdapter::active_stake(Pool::from(pool_account.clone())),
			min_create_bond + min_join_bond
		);

		#[extrinsic_call]
		_(RuntimeOrigin::Signed(joiner.clone()), ClaimPermission::Permissioned);

		assert_eq!(ClaimPermissions::<T>::get(joiner), ClaimPermission::Permissioned);
	}

	#[benchmark]
	fn claim_commission() {
		let claimer: T::AccountId = account("claimer_member", USER_SEED + 4, 0);
		let commission = Perbill::from_percent(50);
		let origin_weight = Pools::<T>::depositor_min_bond() * 2u32.into();
		let ed = CurrencyOf::<T>::minimum_balance();
		let (depositor, _pool_account) =
			create_pool_account::<T>(0, origin_weight, Some(commission));
		let reward_account = Pools::<T>::generate_reward_account(1);
		CurrencyOf::<T>::set_balance(&reward_account, ed + origin_weight);

		// member claims a payout to make some commission available.
		let _ = Pools::<T>::claim_payout(RuntimeOrigin::Signed(claimer.clone()).into());
		// set a claim permission to an account.
		let _ = Pools::<T>::set_commission_claim_permission(
			RuntimeOrigin::Signed(depositor.clone()).into(),
			1u32.into(),
			Some(CommissionClaimPermission::Account(claimer)),
		);
		whitelist_account!(depositor);

		#[extrinsic_call]
		_(RuntimeOrigin::Signed(depositor.clone()), 1u32.into());

		assert_eq!(
			CurrencyOf::<T>::balance(&depositor),
			origin_weight + commission * origin_weight
		);
		assert_eq!(CurrencyOf::<T>::balance(&reward_account), ed + commission * origin_weight);
	}

	#[benchmark]
	fn adjust_pool_deposit() {
		// Create a pool
		let (depositor, _) =
			create_pool_account::<T>(0, Pools::<T>::depositor_min_bond() * 2u32.into(), None);

		// Remove ed freeze to create a scenario where the ed deposit needs to be adjusted.
		let _ = Pools::<T>::unfreeze_pool_deposit(&Pools::<T>::generate_reward_account(1));

		assert_eq!(Pools::<T>::check_ed_imbalance().unwrap(), 1);

		whitelist_account!(depositor);

		#[extrinsic_call]
		_(RuntimeOrigin::Signed(depositor), 1);

		assert_eq!(Pools::<T>::check_ed_imbalance().unwrap(), 0);
	}

	#[benchmark]
	fn apply_slash() {
		// We want to fill member's unbonding pools. So let's bond with big enough amount.
		let deposit_amount =
			Pools::<T>::depositor_min_bond() * T::MaxUnbonding::get().into() * 4u32.into();
		let (depositor, pool_account) = create_pool_account::<T>(0, deposit_amount, None);
		let depositor_lookup = T::Lookup::unlookup(depositor.clone());

		// verify user balance in the pool.
		assert_eq!(PoolMembers::<T>::get(&depositor).unwrap().total_balance(), deposit_amount);
		// verify delegated balance.
		assert!(
			T::StakeAdapter::member_delegation_balance(Member::from(depositor.clone())) ==
				Some(deposit_amount),
		);

		// ugly type conversion between balances of pallet staking and pools (which really are same
		// type). Maybe there is a better way?
		let slash_amount: u128 = deposit_amount.into() / 2;

		// slash pool by half
		pallet_staking_async::slashing::do_slash::<T>(
			&pool_account,
			slash_amount.into(),
			&mut pallet_staking_async::BalanceOf::<T>::zero(),
			&mut pallet_staking_async::NegativeImbalanceOf::<T>::zero(),
			EraIndex::zero(),
		);

		// verify user balance is slashed in the pool.
		assert_eq!(
			PoolMembers::<T>::get(&depositor).unwrap().total_balance(),
			deposit_amount / 2u32.into()
		);
		// verify delegated balance are not yet slashed.
		assert!(
			T::StakeAdapter::member_delegation_balance(Member::from(depositor.clone())) ==
				Some(deposit_amount),
		);

		// Fill member's sub pools for the worst case.
		for i in 1..(T::MaxUnbonding::get() + 1) {
			<T::StakeAdapter as StakeStrategy>::CoreStaking::set_era(i);
			assert!(Pools::<T>::unbond(
				RuntimeOrigin::Signed(depositor.clone()).into(),
				depositor_lookup.clone(),
				Pools::<T>::depositor_min_bond()
			)
			.is_ok());
		}

		<T::StakeAdapter as StakeStrategy>::CoreStaking::set_era(T::MaxUnbonding::get() + 2);

		let slash_reporter =
			create_funded_user_with_balance::<T>("slasher", 0, CurrencyOf::<T>::minimum_balance());
		whitelist_account!(depositor);

		#[block]
		{
			assert!(Pools::<T>::apply_slash(
				RuntimeOrigin::Signed(slash_reporter.clone()).into(),
				depositor_lookup.clone(),
			)
			.is_ok(),);
		}

		// verify balances are correct and slash applied.
		assert_eq!(
			PoolMembers::<T>::get(&depositor).unwrap().total_balance(),
			deposit_amount / 2u32.into()
		);
		assert!(
			T::StakeAdapter::member_delegation_balance(Member::from(depositor.clone())) ==
				Some(deposit_amount / 2u32.into()),
		);
	}

	#[benchmark]
	fn apply_slash_fail() {
		// Bench the scenario where pool has some unapplied slash but the member does not have any
		// slash to be applied.
		let deposit_amount = Pools::<T>::depositor_min_bond() * 10u32.into();
		// Create pool.
		let (_depositor, pool_account) = create_pool_account::<T>(0, deposit_amount, None);

		// slash pool by half
		let slash_amount: u128 = deposit_amount.into() / 2;
		pallet_staking_async::slashing::do_slash::<T>(
			&pool_account,
			slash_amount.into(),
			&mut pallet_staking_async::BalanceOf::<T>::zero(),
			&mut pallet_staking_async::NegativeImbalanceOf::<T>::zero(),
			EraIndex::zero(),
		);

		<T::StakeAdapter as StakeStrategy>::CoreStaking::set_era(1);

		// new member joins the pool who should not be affected by slash.
		let min_join_bond = MinJoinBond::<T>::get().max(CurrencyOf::<T>::minimum_balance());
		let join_amount = min_join_bond * T::MaxUnbonding::get().into() * 2u32.into();
		let joiner = create_funded_user_with_balance::<T>("joiner", 0, join_amount * 2u32.into());
		let joiner_lookup = T::Lookup::unlookup(joiner.clone());
		assert!(
			Pools::<T>::join(RuntimeOrigin::Signed(joiner.clone()).into(), join_amount, 1).is_ok()
		);

		// Fill member's sub pools for the worst case.
		for i in 0..T::MaxUnbonding::get() {
			<T::StakeAdapter as StakeStrategy>::CoreStaking::set_era(i + 2); // +2 because we already set the era to 1.
			assert!(Pools::<T>::unbond(
				RuntimeOrigin::Signed(joiner.clone()).into(),
				joiner_lookup.clone(),
				min_join_bond
			)
			.is_ok());
		}

		<T::StakeAdapter as StakeStrategy>::CoreStaking::set_era(T::MaxUnbonding::get() + 3);
		whitelist_account!(joiner);

		// Since the StakeAdapter can be different based on the runtime config, the errors could be
		// different as well.
		#[block]
		{
			assert!(Pools::<T>::apply_slash(
				RuntimeOrigin::Signed(joiner.clone()).into(),
				joiner_lookup.clone()
			)
			.is_err());
		}
	}

	#[benchmark]
	fn pool_migrate() {
		// create a pool.
		let deposit_amount = Pools::<T>::depositor_min_bond() * 2u32.into();
		let (depositor, pool_account) = create_pool_account::<T>(0, deposit_amount, None);

		// migrate pool to transfer stake.
		let _ = migrate_to_transfer_stake::<T>(1);
		#[block]
		{
			assert!(Pools::<T>::migrate_pool_to_delegate_stake(
				RuntimeOrigin::Signed(depositor.clone()).into(),
				1u32.into(),
			)
			.is_ok(),);
		}
		// this queries agent balance.
		assert_eq!(
			T::StakeAdapter::total_balance(Pool::from(pool_account.clone())),
			Some(deposit_amount + CurrencyOf::<T>::minimum_balance())
		);
	}

	#[benchmark]
	fn migrate_delegation() {
		// create a pool.
		let deposit_amount = Pools::<T>::depositor_min_bond() * 2u32.into();
		let (depositor, _pool_account) = create_pool_account::<T>(0, deposit_amount, None);
		let depositor_lookup = T::Lookup::unlookup(depositor.clone());

		// migrate pool to transfer stake.
		let _ = migrate_to_transfer_stake::<T>(1);

		// Now migrate pool to delegate stake keeping delegators unmigrated.
		assert!(Pools::<T>::migrate_pool_to_delegate_stake(
			RuntimeOrigin::Signed(depositor.clone()).into(),
			1u32.into(),
		)
		.is_ok(),);

		// delegation does not exist.
		assert!(
			T::StakeAdapter::member_delegation_balance(Member::from(depositor.clone())).is_none()
		);
		// contribution exists in the pool.
		assert_eq!(PoolMembers::<T>::get(&depositor).unwrap().total_balance(), deposit_amount);

		whitelist_account!(depositor);

		#[block]
		{
			assert!(Pools::<T>::migrate_delegation(
				RuntimeOrigin::Signed(depositor.clone()).into(),
				depositor_lookup.clone(),
			)
			.is_ok(),);
		}
		// verify balances once more.
		assert!(
			T::StakeAdapter::member_delegation_balance(Member::from(depositor.clone())) ==
				Some(deposit_amount),
		);
		assert_eq!(PoolMembers::<T>::get(&depositor).unwrap().total_balance(), deposit_amount);
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Runtime);
}
