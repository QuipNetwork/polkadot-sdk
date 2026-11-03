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

#![cfg(feature = "runtime-benchmarks")]

extern crate alloc;

use super::*;
use crate::Pallet;
use frame::{benchmarking::prelude::*, traits::fungible::Mutate};

const SEED: u32 = 0;

fn assert_last_event<T: Config>(generic_event: crate::Event<T>) {
	frame_system::Pallet::<T>::assert_last_event(generic_event.into());
}

fn fund_account<T: Config>(who: &T::AccountId) {
	let balance = BalanceOf::<T>::max_value() / 100000u32.into();
	T::Currency::mint_into(who, balance).expect("funding account");
}

fn generate_friends<T: Config>(seed: u32, num: u32) -> Vec<T::AccountId> {
	let mut friends = (0..num).map(|x| account("friend", x, seed)).collect::<Vec<_>>();
	friends.sort();

	for friend in &friends {
		fund_account::<T>(friend);
	}

	friends
}

/// Returns the last friend in the sorted friends list (worst case for O(n) lookups).
fn last_friend<T: Config>(seed: u32, num: u32) -> T::AccountId {
	let friends = generate_friends::<T>(seed, num);
	friends.into_iter().last().expect("at least one friend")
}

fn create_friend_group<T: Config>(
	seed: u32,
	num_friends: u32,
	threshold: u32,
	inheritance_priority: InheritancePriority,
	inheritance_delay: ProvidedBlockNumberOf<T>,
	cancel_delay: ProvidedBlockNumberOf<T>,
) -> FriendGroupOf<T> {
	let friends = generate_friends::<T>(num_friends * seed + 1, num_friends);
	let inheritor: T::AccountId = account("inheritor", inheritance_priority, SEED);
	fund_account::<T>(&inheritor);

	FriendGroupOf::<T> {
		friends: friends.try_into().unwrap(),
		friends_needed: threshold,
		inheritor,
		inheritance_delay,
		inheritance_priority,
		cancel_delay,
	}
}

fn create_friend_groups<T: Config>(num_friends: u32, seed: u32) -> FriendGroupsOf<T> {
	let mut friend_groups = Vec::new();

	for i in 0..MAX_GROUPS_PER_ACCOUNT {
		friend_groups.push(create_friend_group::<T>(
			seed + i,
			num_friends,
			1,
			0,
			10u32.into(),
			10u32.into(),
		));
	}

	friend_groups.try_into().unwrap()
}

fn setup_friend_groups<T: Config>(
	lost: &T::AccountId,
	num_friends: u32,
	seed: u32,
) -> FriendGroupsOf<T> {
	let friend_groups = create_friend_groups::<T>(num_friends, seed);

	let footprint = Pallet::<T>::friend_group_footprint(&friend_groups);
	let ticket = T::FriendGroupsConsideration::new(lost, footprint).unwrap();
	FriendGroups::<T>::insert(lost, (&friend_groups, ticket));

	friend_groups
}

#[benchmarks]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn control_inherited_account() {
		let inheritor: T::AccountId = whitelisted_caller();
		let recovered: T::AccountId = account("recovered", 0, SEED);
		let recovered_lookup = T::Lookup::unlookup(recovered.clone());

		fund_account::<T>(&inheritor);

		let ticket = Pallet::<T>::inheritor_ticket(&inheritor).unwrap();
		Inheritor::<T>::insert(&recovered, (0u32, &inheritor, ticket));

		let call: <T as Config>::RuntimeCall =
			frame_system::Call::<T>::remark { remark: Vec::new() }.into();
		let call_hash = call.using_encoded(&T::Hashing::hash);

		#[extrinsic_call]
		_(RawOrigin::Signed(inheritor.clone()), recovered_lookup, Box::new(call));

		assert_last_event::<T>(
			Event::<T>::RecoveredAccountControlled {
				recovered,
				inheritor,
				call_hash,
				call_result: Ok(()),
			}
			.into(),
		);
	}

	#[benchmark]
	fn revoke_inheritor() {
		let lost: T::AccountId = whitelisted_caller();
		let depositor: T::AccountId = account("depositor", 0, SEED);

		fund_account::<T>(&lost);
		fund_account::<T>(&depositor);

		let ticket = Pallet::<T>::inheritor_ticket(&depositor).unwrap();
		Inheritor::<T>::insert(
			&lost,
			(0u32, &account::<T::AccountId>("inheritor", 0, SEED), ticket),
		);

		#[extrinsic_call]
		_(RawOrigin::Signed(lost.clone()));

		assert_last_event::<T>(Event::<T>::InheritorRevoked { lost }.into());
	}

	#[benchmark]
	fn set_friend_groups() {
		let lost: T::AccountId = whitelisted_caller();
		fund_account::<T>(&lost);

		let _ = setup_friend_groups::<T>(&lost, T::MaxFriendsPerConfig::get(), 0);
		let new_friend_groups =
			create_friend_groups::<T>(T::MaxFriendsPerConfig::get(), 1).into_inner();

		#[extrinsic_call]
		_(RawOrigin::Signed(lost.clone()), new_friend_groups);

		assert_last_event::<T>(Event::<T>::FriendGroupsChanged { lost }.into());
	}

	#[benchmark]
	fn initiate_attempt() {
		let lost: T::AccountId = whitelisted_caller();
		let lost_lookup = T::Lookup::unlookup(lost.clone());
		// Use last friend for worst case O(n) lookup in friends.iter().position().
		let initiator: T::AccountId = last_friend::<T>(1, T::MaxFriendsPerConfig::get());

		fund_account::<T>(&lost);

		let friend_groups =
			setup_friend_groups::<T>(&lost, T::MaxFriendsPerConfig::get(), 0).into_inner();

		crate::pallet::Pallet::<T>::set_friend_groups(
			RawOrigin::Signed(lost.clone()).into(),
			friend_groups,
		)
		.unwrap();

		#[extrinsic_call]
		_(RawOrigin::Signed(initiator.clone()), lost_lookup, 0);

		assert_last_event::<T>(
			Event::<T>::AttemptApproved { lost, friend_group_index: 0, friend: initiator }.into(),
		);
	}

	#[benchmark]
	fn approve_attempt() {
		let lost: T::AccountId = whitelisted_caller();
		let lost_lookup = T::Lookup::unlookup(lost.clone());
		// Use last friend for worst case O(n) lookup in friends.iter().position()
		let friend: T::AccountId = last_friend::<T>(1, T::MaxFriendsPerConfig::get());

		fund_account::<T>(&lost);

		let friend_groups =
			setup_friend_groups::<T>(&lost, T::MaxFriendsPerConfig::get(), 0).into_inner();
		crate::pallet::Pallet::<T>::set_friend_groups(
			RawOrigin::Signed(lost.clone()).into(),
			friend_groups,
		)
		.unwrap();
		crate::pallet::Pallet::<T>::initiate_attempt(
			RawOrigin::Signed(friend.clone()).into(),
			lost_lookup.clone(),
			0,
		)
		.unwrap();

		// Initiation auto-approves the initiator. Reset approvals so this benchmark can still
		// measure a successful `approve_attempt` call.
		crate::pallet::Attempt::<T>::mutate(&lost, 0, |maybe_attempt| {
			let (attempt, _, _) = maybe_attempt.as_mut().expect("attempt exists after initiation");
			attempt.approvals = ApprovalBitfieldOf::<T>::default();
		});

		#[extrinsic_call]
		_(RawOrigin::Signed(friend.clone()), lost_lookup, 0);

		assert_last_event::<T>(
			Event::<T>::AttemptApproved { lost, friend_group_index: 0, friend }.into(),
		);
	}

	#[benchmark]
	fn finish_attempt() {
		let lost: T::AccountId = whitelisted_caller();
		let lost_lookup = T::Lookup::unlookup(lost.clone());
		// Use last friend for worst case O(n) lookup
		let initiator: T::AccountId = last_friend::<T>(1, T::MaxFriendsPerConfig::get());
		let inheritor: T::AccountId = account("inheritor", 0, SEED);
		let previous_inheritor: T::AccountId = account("old_inheritor", 0, SEED);

		fund_account::<T>(&lost);
		fund_account::<T>(&previous_inheritor);

		// Set up existing inheritor with higher order (1) to trigger replacement path
		let ticket = Pallet::<T>::inheritor_ticket(&previous_inheritor).unwrap();
		Inheritor::<T>::insert(&lost, (1u32, &previous_inheritor, ticket));

		// Friend groups have order 0, which is lower than existing order 1
		let friend_groups =
			setup_friend_groups::<T>(&lost, T::MaxFriendsPerConfig::get(), 0).into_inner();
		crate::pallet::Pallet::<T>::set_friend_groups(
			RawOrigin::Signed(lost.clone()).into(),
			friend_groups,
		)
		.unwrap();
		crate::pallet::Pallet::<T>::initiate_attempt(
			RawOrigin::Signed(initiator.clone()).into(),
			lost_lookup.clone(),
			0,
		)
		.unwrap();
		frame_system::Pallet::<T>::set_block_number(100u32.into());

		#[extrinsic_call]
		_(RawOrigin::Signed(initiator.clone()), lost_lookup, 0);

		assert_last_event::<T>(
			Event::<T>::AttemptFinished {
				lost,
				friend_group_index: 0,
				inheritor,
				previous_inheritor: Some(previous_inheritor),
			}
			.into(),
		);
	}

	#[benchmark]
	fn cancel_attempt() {
		let lost: T::AccountId = whitelisted_caller();
		let lost_lookup = T::Lookup::unlookup(lost.clone());
		let initiator: T::AccountId = account("friend", 0, 1);

		fund_account::<T>(&lost);
		fund_account::<T>(&initiator);

		let friend_groups =
			setup_friend_groups::<T>(&lost, T::MaxFriendsPerConfig::get(), 0).into_inner();
		crate::pallet::Pallet::<T>::set_friend_groups(
			RawOrigin::Signed(lost.clone()).into(),
			friend_groups,
		)
		.unwrap();
		crate::pallet::Pallet::<T>::initiate_attempt(
			RawOrigin::Signed(initiator.clone()).into(),
			lost_lookup.clone(),
			0,
		)
		.unwrap();
		frame_system::Pallet::<T>::set_block_number(100u32.into());

		#[extrinsic_call]
		_(RawOrigin::Signed(initiator.clone()), lost_lookup, 0);

		assert_last_event::<T>(
			Event::<T>::AttemptCanceled { lost, friend_group_index: 0, canceler: initiator }.into(),
		);
	}

	#[benchmark]
	fn slash_attempt() {
		let lost: T::AccountId = whitelisted_caller();
		let lost_lookup = T::Lookup::unlookup(lost.clone());
		let initiator: T::AccountId = account("friend", 0, 1);

		fund_account::<T>(&lost);
		fund_account::<T>(&initiator);

		let friend_groups =
			setup_friend_groups::<T>(&lost, T::MaxFriendsPerConfig::get(), 0).into_inner();
		crate::pallet::Pallet::<T>::set_friend_groups(
			RawOrigin::Signed(lost.clone()).into(),
			friend_groups,
		)
		.unwrap();
		crate::pallet::Pallet::<T>::initiate_attempt(
			RawOrigin::Signed(initiator.clone()).into(),
			lost_lookup.clone(),
			0,
		)
		.unwrap();

		#[extrinsic_call]
		_(RawOrigin::Signed(lost.clone()), 0);
		assert_last_event::<T>(Event::<T>::AttemptSlashed { lost, friend_group_index: 0 }.into());
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Test);
}
