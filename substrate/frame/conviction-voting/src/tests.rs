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

//! The crate's tests.

use std::{cell::RefCell, collections::BTreeMap};

use frame_support::{
	assert_noop, assert_ok, derive_impl, parameter_types,
	traits::{ConstU32, ConstU64, Contains, Polling, VoteTally},
};
use sp_runtime::BuildStorage;

use super::*;
use crate as pallet_conviction_voting;

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		Balances: pallet_balances,
		Voting: pallet_conviction_voting,
	}
);

// Test that a filtered call can be dispatched.
pub struct BaseFilter;
impl Contains<RuntimeCall> for BaseFilter {
	fn contains(call: &RuntimeCall) -> bool {
		!matches!(call, &RuntimeCall::Balances(pallet_balances::Call::force_set_balance { .. }))
	}
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type BaseCallFilter = BaseFilter;
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u64>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type AccountStore = System;
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TestPollState {
	Ongoing(TallyOf<Test>, u8),
	Completed(u64, bool),
}
use TestPollState::*;

parameter_types! {
	pub static Polls: BTreeMap<u8, TestPollState> = vec![
		(1, Completed(1, true)),
		(2, Completed(2, false)),
		(3, Ongoing(Tally::from_parts(0, 0, 0), 0)),
	].into_iter().collect();
}

pub struct TestPolls;
impl Polling<TallyOf<Test>> for TestPolls {
	type Index = u8;
	type Votes = u64;
	type Moment = u64;
	type Class = u8;
	fn classes() -> Vec<u8> {
		vec![0, 1, 2]
	}
	fn as_ongoing(index: u8) -> Option<(TallyOf<Test>, Self::Class)> {
		Polls::get().remove(&index).and_then(|x| {
			if let TestPollState::Ongoing(t, c) = x {
				Some((t, c))
			} else {
				None
			}
		})
	}
	fn access_poll<R>(
		index: Self::Index,
		f: impl FnOnce(PollStatus<&mut TallyOf<Test>, u64, u8>) -> R,
	) -> R {
		let mut polls = Polls::get();
		let entry = polls.get_mut(&index);
		let r = match entry {
			Some(Ongoing(ref mut tally_mut_ref, class)) => {
				f(PollStatus::Ongoing(tally_mut_ref, *class))
			},
			Some(Completed(when, succeeded)) => f(PollStatus::Completed(*when, *succeeded)),
			None => f(PollStatus::None),
		};
		Polls::set(polls);
		r
	}
	fn try_access_poll<R>(
		index: Self::Index,
		f: impl FnOnce(PollStatus<&mut TallyOf<Test>, u64, u8>) -> Result<R, DispatchError>,
	) -> Result<R, DispatchError> {
		let mut polls = Polls::get();
		let entry = polls.get_mut(&index);
		let r = match entry {
			Some(Ongoing(ref mut tally_mut_ref, class)) => {
				f(PollStatus::Ongoing(tally_mut_ref, *class))
			},
			Some(Completed(when, succeeded)) => f(PollStatus::Completed(*when, *succeeded)),
			None => f(PollStatus::None),
		}?;
		Polls::set(polls);
		Ok(r)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn create_ongoing(class: Self::Class) -> Result<Self::Index, ()> {
		let mut polls = Polls::get();
		let i = polls.keys().rev().next().map_or(0, |x| x + 1);
		polls.insert(i, Ongoing(Tally::new(0), class));
		Polls::set(polls);
		Ok(i)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn end_ongoing(index: Self::Index, approved: bool) -> Result<(), ()> {
		let mut polls = Polls::get();
		match polls.get(&index) {
			Some(Ongoing(..)) => {},
			_ => return Err(()),
		}
		let now = frame_system::Pallet::<Test>::block_number();
		polls.insert(index, Completed(now, approved));
		Polls::set(polls);
		Ok(())
	}
}

impl Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type Currency = pallet_balances::Pallet<Self>;
	type VoteLockingPeriod = ConstU64<3>;
	type MaxVotes = ConstU32<3>;
	type WeightInfo = ();
	type MaxTurnout = frame_support::traits::TotalIssuanceOf<Balances, Self::AccountId>;
	type Polls = TestPolls;
	type BlockNumberProvider = System;
	type VotingHooks = HooksHandler;
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	pallet_balances::GenesisConfig::<Test> {
		balances: vec![(1, 10), (2, 20), (3, 30), (4, 40), (5, 50), (6, 60)],
		..Default::default()
	}
	.assimilate_storage(&mut t)
	.unwrap();
	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| System::set_block_number(1));
	ext
}

#[test]
fn params_should_work() {
	new_test_ext().execute_with(|| {
		assert_eq!(Balances::free_balance(42), 0);
		assert_eq!(pallet_balances::TotalIssuance::<Test>::get(), 210);
	});
}

fn next_block() {
	System::set_block_number(System::block_number() + 1);
}

#[allow(dead_code)]
fn run_to(n: u64) {
	while System::block_number() < n {
		next_block();
	}
}

fn aye(amount: u64, conviction: u8) -> AccountVote<u64> {
	let vote = Vote { aye: true, conviction: conviction.try_into().unwrap() };
	AccountVote::Standard { vote, balance: amount }
}

fn nay(amount: u64, conviction: u8) -> AccountVote<u64> {
	let vote = Vote { aye: false, conviction: conviction.try_into().unwrap() };
	AccountVote::Standard { vote, balance: amount }
}

fn split(aye: u64, nay: u64) -> AccountVote<u64> {
	AccountVote::Split { aye, nay }
}

fn split_abstain(aye: u64, nay: u64, abstain: u64) -> AccountVote<u64> {
	AccountVote::SplitAbstain { aye, nay, abstain }
}

fn tally(index: u8) -> TallyOf<Test> {
	<TestPolls as Polling<TallyOf<Test>>>::as_ongoing(index).expect("No poll").0
}

fn class(index: u8) -> u8 {
	<TestPolls as Polling<TallyOf<Test>>>::as_ongoing(index).expect("No poll").1
}

#[test]
#[ignore]
#[should_panic(expected = "No poll")]
fn unknown_poll_should_panic() {
	let _ = tally(0);
}

#[test]
#[ignore]
#[should_panic(expected = "No poll")]
fn completed_poll_should_panic() {
	let _ = tally(1);
}

#[test]
fn basic_stuff() {
	new_test_ext().execute_with(|| {
		assert_eq!(tally(3), Tally::from_parts(0, 0, 0));
	});
}

#[test]
fn basic_voting_works() {
	new_test_ext().execute_with(|| {
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, aye(2, 5)));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::Voted {
			who: 1,
			vote: aye(2, 5),
			poll_index: 3,
		}));
		assert_eq!(tally(3), Tally::from_parts(10, 0, 2));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, nay(2, 5)));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::Voted {
			who: 1,
			vote: nay(2, 5),
			poll_index: 3,
		}));
		assert_eq!(tally(3), Tally::from_parts(0, 10, 0));
		assert_eq!(Balances::usable_balance(1), 8);

		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, aye(5, 1)));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::Voted {
			who: 1,
			vote: aye(5, 1),
			poll_index: 3,
		}));
		assert_eq!(tally(3), Tally::from_parts(5, 0, 5));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, nay(5, 1)));
		assert_eq!(tally(3), Tally::from_parts(0, 5, 0));
		assert_eq!(Balances::usable_balance(1), 5);

		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, aye(10, 0)));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::Voted {
			who: 1,
			vote: aye(10, 0),
			poll_index: 3,
		}));
		assert_eq!(tally(3), Tally::from_parts(1, 0, 10));

		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, nay(10, 0)));
		assert_eq!(tally(3), Tally::from_parts(0, 1, 0));
		assert_eq!(Balances::usable_balance(1), 0);

		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), None, 3));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::VoteRemoved {
			who: 1,
			vote: nay(10, 0),
			poll_index: 3,
		}));
		assert_eq!(tally(3), Tally::from_parts(0, 0, 0));

		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), class(3), 1));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::VoteUnlocked {
			who: 1,
			class: class(3),
		}));
		assert_eq!(Balances::usable_balance(1), 10);
	});
}

#[test]
fn split_voting_works() {
	new_test_ext().execute_with(|| {
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, split(10, 0)));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::Voted {
			who: 1,
			vote: split(10, 0),
			poll_index: 3,
		}));
		assert_eq!(tally(3), Tally::from_parts(1, 0, 10));

		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, split(5, 5)));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::Voted {
			who: 1,
			vote: split(5, 5),
			poll_index: 3,
		}));
		assert_eq!(tally(3), Tally::from_parts(0, 0, 5));
		assert_eq!(Balances::usable_balance(1), 0);

		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), None, 3));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::VoteRemoved {
			who: 1,
			vote: split(5, 5),
			poll_index: 3,
		}));
		assert_eq!(tally(3), Tally::from_parts(0, 0, 0));

		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), class(3), 1));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::VoteUnlocked {
			who: 1,
			class: class(3),
		}));
		assert_eq!(Balances::usable_balance(1), 10);
	});
}

#[test]
fn abstain_voting_works() {
	new_test_ext().execute_with(|| {
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, split_abstain(0, 0, 10)));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::Voted {
			who: 1,
			vote: split_abstain(0, 0, 10),
			poll_index: 3,
		}));
		assert_eq!(tally(3), Tally::from_parts(0, 0, 10));

		assert_ok!(Voting::vote(RuntimeOrigin::signed(6), 3, split_abstain(10, 0, 20)));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::Voted {
			who: 6,
			vote: split_abstain(10, 0, 20),
			poll_index: 3,
		}));
		assert_eq!(tally(3), Tally::from_parts(1, 0, 40));

		assert_ok!(Voting::vote(RuntimeOrigin::signed(6), 3, split_abstain(0, 0, 40)));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::Voted {
			who: 6,
			vote: split_abstain(0, 0, 40),
			poll_index: 3,
		}));

		assert_eq!(tally(3), Tally::from_parts(0, 0, 50));
		assert_eq!(Balances::usable_balance(1), 0);
		assert_eq!(Balances::usable_balance(6), 20);

		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), None, 3));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::VoteRemoved {
			who: 1,
			vote: split_abstain(0, 0, 10),
			poll_index: 3,
		}));
		assert_eq!(tally(3), Tally::from_parts(0, 0, 40));

		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(6), Some(class(3)), 3));
		System::assert_last_event(tests::RuntimeEvent::Voting(Event::VoteRemoved {
			who: 6,
			vote: split_abstain(0, 0, 40),
			poll_index: 3,
		}));
		assert_eq!(tally(3), Tally::from_parts(0, 0, 0));

		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), class(3), 1));
		assert_eq!(Balances::usable_balance(1), 10);

		assert_ok!(Voting::unlock(RuntimeOrigin::signed(6), class(3), 6));
		assert_eq!(Balances::usable_balance(6), 60);
	});
}

#[test]
fn voting_balance_gets_locked() {
	new_test_ext().execute_with(|| {
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, aye(2, 5)));
		assert_eq!(tally(3), Tally::from_parts(10, 0, 2));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, nay(2, 5)));
		assert_eq!(tally(3), Tally::from_parts(0, 10, 0));
		assert_eq!(Balances::usable_balance(1), 8);

		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, aye(5, 1)));
		assert_eq!(tally(3), Tally::from_parts(5, 0, 5));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, nay(5, 1)));
		assert_eq!(tally(3), Tally::from_parts(0, 5, 0));
		assert_eq!(Balances::usable_balance(1), 5);

		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, aye(10, 0)));
		assert_eq!(tally(3), Tally::from_parts(1, 0, 10));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, nay(10, 0)));
		assert_eq!(tally(3), Tally::from_parts(0, 1, 0));
		assert_eq!(Balances::usable_balance(1), 0);

		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), None, 3));
		assert_eq!(tally(3), Tally::from_parts(0, 0, 0));

		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), class(3), 1));
		assert_eq!(Balances::usable_balance(1), 10);
	});
}

#[test]
fn successful_but_zero_conviction_vote_balance_can_be_unlocked() {
	new_test_ext().execute_with(|| {
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, aye(1, 1)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(2), 3, nay(20, 0)));
		let c = class(3);
		Polls::set(vec![(3, Completed(3, false))].into_iter().collect());
		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(2), Some(c), 3));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(2), c, 2));
		assert_eq!(Balances::usable_balance(2), 20);
	});
}

#[test]
fn unsuccessful_conviction_vote_balance_can_be_unlocked() {
	new_test_ext().execute_with(|| {
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, aye(1, 1)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(2), 3, nay(20, 0)));
		let c = class(3);
		Polls::set(vec![(3, Completed(3, false))].into_iter().collect());
		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), Some(c), 3));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), c, 1));
		assert_eq!(Balances::usable_balance(1), 10);
	});
}

#[test]
fn successful_conviction_vote_balance_stays_locked_for_correct_time() {
	new_test_ext().execute_with(|| {
		for i in 1..=5 {
			assert_ok!(Voting::vote(RuntimeOrigin::signed(i), 3, aye(10, i as u8)));
		}
		let c = class(3);
		Polls::set(vec![(3, Completed(3, true))].into_iter().collect());
		for i in 1..=5 {
			assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(i), Some(c), 3));
		}
		for block in 1..=(3 + 5 * 3) {
			run_to(block);
			for i in 1..=5 {
				assert_ok!(Voting::unlock(RuntimeOrigin::signed(i), c, i));
				let expired = block >= (3 << (i - 1)) + 3;
				assert_eq!(Balances::usable_balance(i), i * 10 - if expired { 0 } else { 10 });
			}
		}
	});
}

#[test]
fn classwise_delegation_works() {
	new_test_ext().execute_with(|| {
		Polls::set(
			vec![
				(0, Ongoing(Tally::new(0), 0)),
				(1, Ongoing(Tally::new(0), 1)),
				(2, Ongoing(Tally::new(0), 2)),
				(3, Ongoing(Tally::new(0), 2)),
			]
			.into_iter()
			.collect(),
		);
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 0, 2, Conviction::Locked1x, 5));
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 1, 3, Conviction::Locked1x, 5));
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 2, 4, Conviction::Locked1x, 5));
		assert_eq!(Balances::usable_balance(1), 5);

		assert_ok!(Voting::vote(RuntimeOrigin::signed(2), 0, aye(10, 0)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(2), 1, nay(10, 0)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(2), 2, nay(10, 0)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(3), 0, nay(10, 0)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(3), 1, aye(10, 0)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(3), 2, nay(10, 0)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(4), 0, nay(10, 0)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(4), 1, nay(10, 0)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(4), 2, aye(10, 0)));
		// 4 hasn't voted yet

		assert_eq!(
			Polls::get(),
			vec![
				(0, Ongoing(Tally::from_parts(6, 2, 15), 0)),
				(1, Ongoing(Tally::from_parts(6, 2, 15), 1)),
				(2, Ongoing(Tally::from_parts(6, 2, 15), 2)),
				(3, Ongoing(Tally::from_parts(0, 0, 0), 2)),
			]
			.into_iter()
			.collect()
		);

		// 4 votes nay to 3.
		assert_ok!(Voting::vote(RuntimeOrigin::signed(4), 3, nay(10, 0)));
		assert_eq!(
			Polls::get(),
			vec![
				(0, Ongoing(Tally::from_parts(6, 2, 15), 0)),
				(1, Ongoing(Tally::from_parts(6, 2, 15), 1)),
				(2, Ongoing(Tally::from_parts(6, 2, 15), 2)),
				(3, Ongoing(Tally::from_parts(0, 6, 0), 2)),
			]
			.into_iter()
			.collect()
		);

		// Redelegate for class 2 to account 3.
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 2));
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 2, 3, Conviction::Locked1x, 5));
		assert_eq!(
			Polls::get(),
			vec![
				(0, Ongoing(Tally::from_parts(6, 2, 15), 0)),
				(1, Ongoing(Tally::from_parts(6, 2, 15), 1)),
				(2, Ongoing(Tally::from_parts(1, 7, 10), 2)),
				(3, Ongoing(Tally::from_parts(0, 1, 0), 2)),
			]
			.into_iter()
			.collect()
		);

		// Redelegating with a lower lock does not forget previous lock and updates correctly.
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 0));
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 1));
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 2));
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 0, 2, Conviction::Locked1x, 3));
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 1, 3, Conviction::Locked1x, 3));
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 2, 4, Conviction::Locked1x, 3));
		assert_eq!(
			Polls::get(),
			vec![
				(0, Ongoing(Tally::from_parts(4, 2, 13), 0)),
				(1, Ongoing(Tally::from_parts(4, 2, 13), 1)),
				(2, Ongoing(Tally::from_parts(4, 2, 13), 2)),
				(3, Ongoing(Tally::from_parts(0, 4, 0), 2)),
			]
			.into_iter()
			.collect()
		);
		assert_eq!(Balances::usable_balance(1), 5);

		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 1, 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 2, 1));
		// unlock does nothing since the delegation already took place.
		assert_eq!(Balances::usable_balance(1), 5);

		// Redelegating with higher amount extends previous lock.
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 0));
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 0, 2, Conviction::Locked1x, 6));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 4);
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 1));
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 1, 3, Conviction::Locked1x, 7));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 1, 1));
		assert_eq!(Balances::usable_balance(1), 3);
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 2));
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 2, 4, Conviction::Locked1x, 8));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 2, 1));
		assert_eq!(Balances::usable_balance(1), 2);
		assert_eq!(
			Polls::get(),
			vec![
				(0, Ongoing(Tally::from_parts(7, 2, 16), 0)),
				(1, Ongoing(Tally::from_parts(8, 2, 17), 1)),
				(2, Ongoing(Tally::from_parts(9, 2, 18), 2)),
				(3, Ongoing(Tally::from_parts(0, 9, 0), 2)),
			]
			.into_iter()
			.collect()
		);
	});
}

#[test]
fn redelegation_after_vote_ending_should_keep_lock() {
	new_test_ext().execute_with(|| {
		Polls::set(vec![(0, Ongoing(Tally::new(0), 0))].into_iter().collect());
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 0, 2, Conviction::Locked1x, 5));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(2), 0, aye(10, 1)));
		Polls::set(vec![(0, Completed(1, true))].into_iter().collect());
		assert_eq!(Balances::usable_balance(1), 5);
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 0));
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 0, 3, Conviction::Locked1x, 3));
		assert_eq!(Balances::usable_balance(1), 5);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 5);
	});
}

#[test]
fn lock_amalgamation_valid_with_multiple_removed_votes() {
	new_test_ext().execute_with(|| {
		Polls::set(
			vec![
				(0, Ongoing(Tally::new(0), 0)),
				(1, Ongoing(Tally::new(0), 0)),
				(2, Ongoing(Tally::new(0), 0)),
			]
			.into_iter()
			.collect(),
		);
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 0, aye(5, 1)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 1, aye(10, 1)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 2, aye(5, 2)));
		assert_eq!(Balances::usable_balance(1), 0);

		Polls::set(
			vec![(0, Completed(1, true)), (1, Completed(1, true)), (2, Completed(1, true))]
				.into_iter()
				.collect(),
		);
		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), Some(0), 0));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 0);

		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), Some(0), 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 0);

		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), Some(0), 2));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 0);

		run_to(3);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 0);

		run_to(6);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert!(Balances::usable_balance(1) <= 5);

		run_to(7);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 10);
	});
}

#[test]
fn lock_amalgamation_valid_with_multiple_delegations() {
	new_test_ext().execute_with(|| {
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 0, 2, Conviction::Locked1x, 5));
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 0));
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 0, 2, Conviction::Locked1x, 10));
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 0));
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 0, 2, Conviction::Locked2x, 5));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 0);
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 0));

		run_to(3);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 0);

		run_to(6);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert!(Balances::usable_balance(1) <= 5);

		run_to(7);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 10);
	});
}

#[test]
fn lock_amalgamation_valid_with_move_roundtrip_to_delegation() {
	new_test_ext().execute_with(|| {
		Polls::set(vec![(0, Ongoing(Tally::new(0), 0))].into_iter().collect());
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 0, aye(5, 1)));
		Polls::set(vec![(0, Completed(1, true))].into_iter().collect());
		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), Some(0), 0));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 5);

		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 0, 2, Conviction::Locked1x, 10));
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 0));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 0);

		Polls::set(vec![(1, Ongoing(Tally::new(0), 0))].into_iter().collect());
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 1, aye(5, 2)));
		Polls::set(vec![(1, Completed(1, true))].into_iter().collect());
		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), Some(0), 1));

		run_to(3);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 0);

		run_to(6);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert!(Balances::usable_balance(1) <= 5);

		run_to(7);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 10);
	});
}

#[test]
fn lock_amalgamation_valid_with_move_roundtrip_to_casting() {
	new_test_ext().execute_with(|| {
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 0, 2, Conviction::Locked1x, 5));
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 0));

		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 5);

		Polls::set(vec![(0, Ongoing(Tally::new(0), 0))].into_iter().collect());
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 0, aye(10, 1)));
		Polls::set(vec![(0, Completed(1, true))].into_iter().collect());
		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), Some(0), 0));

		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 0);

		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 0, 2, Conviction::Locked2x, 10));
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 0));

		run_to(3);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 0);

		run_to(6);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert!(Balances::usable_balance(1) <= 5);

		run_to(7);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_eq!(Balances::usable_balance(1), 10);
	});
}

#[test]
fn lock_aggregation_over_different_classes_with_delegation_works() {
	new_test_ext().execute_with(|| {
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 0, 2, Conviction::Locked1x, 5));
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 1, 2, Conviction::Locked2x, 5));
		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 2, 2, Conviction::Locked1x, 10));

		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 0));
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 1));
		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 2));

		run_to(3);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 1, 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 2, 1));
		assert_eq!(Balances::usable_balance(1), 0);

		run_to(6);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 1, 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 2, 1));
		assert_eq!(Balances::usable_balance(1), 5);

		run_to(7);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 1, 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 2, 1));
		assert_eq!(Balances::usable_balance(1), 10);
	});
}

#[test]
fn lock_aggregation_over_different_classes_with_casting_works() {
	new_test_ext().execute_with(|| {
		Polls::set(
			vec![
				(0, Ongoing(Tally::new(0), 0)),
				(1, Ongoing(Tally::new(0), 1)),
				(2, Ongoing(Tally::new(0), 2)),
			]
			.into_iter()
			.collect(),
		);
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 0, aye(5, 1)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 1, aye(10, 1)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 2, aye(5, 2)));
		Polls::set(
			vec![(0, Completed(1, true)), (1, Completed(1, true)), (2, Completed(1, true))]
				.into_iter()
				.collect(),
		);
		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), Some(0), 0));
		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), Some(1), 1));
		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), Some(2), 2));

		run_to(3);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 1, 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 2, 1));
		assert_eq!(Balances::usable_balance(1), 0);

		run_to(6);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 1, 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 2, 1));
		assert_eq!(Balances::usable_balance(1), 5);

		run_to(7);
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 0, 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 1, 1));
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), 2, 1));
		assert_eq!(Balances::usable_balance(1), 10);
	});
}

#[test]
fn errors_with_vote_work() {
	new_test_ext().execute_with(|| {
		assert_noop!(
			Voting::vote(RuntimeOrigin::signed(1), 0, aye(10, 0)),
			Error::<Test>::NotOngoing
		);
		assert_noop!(
			Voting::vote(RuntimeOrigin::signed(1), 1, aye(10, 0)),
			Error::<Test>::NotOngoing
		);
		assert_noop!(
			Voting::vote(RuntimeOrigin::signed(1), 2, aye(10, 0)),
			Error::<Test>::NotOngoing
		);
		assert_noop!(
			Voting::vote(RuntimeOrigin::signed(1), 3, aye(11, 0)),
			Error::<Test>::InsufficientFunds
		);

		assert_ok!(Voting::delegate(RuntimeOrigin::signed(1), 0, 2, Conviction::None, 10));
		assert_noop!(
			Voting::vote(RuntimeOrigin::signed(1), 3, aye(10, 0)),
			Error::<Test>::AlreadyDelegating
		);

		assert_ok!(Voting::undelegate(RuntimeOrigin::signed(1), 0));
		Polls::set(
			vec![
				(0, Ongoing(Tally::new(0), 0)),
				(1, Ongoing(Tally::new(0), 0)),
				(2, Ongoing(Tally::new(0), 0)),
				(3, Ongoing(Tally::new(0), 0)),
			]
			.into_iter()
			.collect(),
		);
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 0, aye(10, 0)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 1, aye(10, 0)));
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 2, aye(10, 0)));
		assert_noop!(
			Voting::vote(RuntimeOrigin::signed(1), 3, aye(10, 0)),
			Error::<Test>::MaxVotesReached
		);
	});
}

#[test]
fn errors_with_delegating_work() {
	new_test_ext().execute_with(|| {
		assert_noop!(
			Voting::delegate(RuntimeOrigin::signed(1), 0, 2, Conviction::None, 11),
			Error::<Test>::InsufficientFunds
		);
		assert_noop!(
			Voting::delegate(RuntimeOrigin::signed(1), 3, 2, Conviction::None, 10),
			Error::<Test>::BadClass
		);

		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, aye(10, 0)));
		assert_noop!(
			Voting::delegate(RuntimeOrigin::signed(1), 0, 2, Conviction::None, 10),
			Error::<Test>::AlreadyVoting
		);

		assert_noop!(Voting::undelegate(RuntimeOrigin::signed(1), 0), Error::<Test>::NotDelegating);
	});
}

#[test]
fn remove_other_vote_works() {
	new_test_ext().execute_with(|| {
		assert_noop!(
			Voting::remove_other_vote(RuntimeOrigin::signed(2), 1, 0, 3),
			Error::<Test>::NotVoter
		);
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, aye(10, 2)));
		assert_noop!(
			Voting::remove_other_vote(RuntimeOrigin::signed(2), 1, 0, 3),
			Error::<Test>::NoPermission
		);
		Polls::set(vec![(3, Completed(1, true))].into_iter().collect());
		run_to(6);
		assert_noop!(
			Voting::remove_other_vote(RuntimeOrigin::signed(2), 1, 0, 3),
			Error::<Test>::NoPermissionYet
		);
		run_to(7);
		assert_ok!(Voting::remove_other_vote(RuntimeOrigin::signed(2), 1, 0, 3));
	});
}

#[test]
fn errors_with_remove_vote_work() {
	new_test_ext().execute_with(|| {
		assert_noop!(
			Voting::remove_vote(RuntimeOrigin::signed(1), Some(0), 3),
			Error::<Test>::NotVoter
		);
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, aye(10, 2)));
		Polls::set(vec![(3, Completed(1, true))].into_iter().collect());
		assert_noop!(
			Voting::remove_vote(RuntimeOrigin::signed(1), None, 3),
			Error::<Test>::ClassNeeded
		);
	});
}

thread_local! {
	static LAST_ON_VOTE_DATA: RefCell<Option<(u64, u8, AccountVote<u64>)>> = RefCell::new(None);
	static LAST_ON_REMOVE_VOTE_DATA: RefCell<Option<(u64, u8, Status)>> = RefCell::new(None);
	static LAST_LOCKED_IF_UNSUCCESSFUL_VOTE_DATA: RefCell<Option<(u64, u8)>> = RefCell::new(None);
	static REMOVE_VOTE_LOCKED_AMOUNT: RefCell<Option<u64>> = RefCell::new(None);
}

pub struct HooksHandler;

impl HooksHandler {
	fn last_on_vote_data() -> Option<(u64, u8, AccountVote<u64>)> {
		LAST_ON_VOTE_DATA.with(|data| *data.borrow())
	}

	fn last_on_remove_vote_data() -> Option<(u64, u8, Status)> {
		LAST_ON_REMOVE_VOTE_DATA.with(|data| *data.borrow())
	}

	fn last_locked_if_unsuccessful_vote_data() -> Option<(u64, u8)> {
		LAST_LOCKED_IF_UNSUCCESSFUL_VOTE_DATA.with(|data| *data.borrow())
	}

	fn reset() {
		LAST_ON_VOTE_DATA.with(|data| *data.borrow_mut() = None);
		LAST_ON_REMOVE_VOTE_DATA.with(|data| *data.borrow_mut() = None);
		LAST_LOCKED_IF_UNSUCCESSFUL_VOTE_DATA.with(|data| *data.borrow_mut() = None);
		REMOVE_VOTE_LOCKED_AMOUNT.with(|data| *data.borrow_mut() = None);
	}

	fn with_remove_locked_amount(v: u64) {
		REMOVE_VOTE_LOCKED_AMOUNT.with(|data| *data.borrow_mut() = Some(v));
	}
}

impl VotingHooks<u64, u8, u64> for HooksHandler {
	fn on_before_vote(who: &u64, ref_index: u8, vote: AccountVote<u64>) -> DispatchResult {
		LAST_ON_VOTE_DATA.with(|data| {
			*data.borrow_mut() = Some((*who, ref_index, vote));
		});
		Ok(())
	}

	fn on_remove_vote(who: &u64, ref_index: u8, ongoing: Status) {
		LAST_ON_REMOVE_VOTE_DATA.with(|data| {
			*data.borrow_mut() = Some((*who, ref_index, ongoing));
		});
	}

	fn lock_balance_on_unsuccessful_vote(who: &u64, ref_index: u8) -> Option<u64> {
		LAST_LOCKED_IF_UNSUCCESSFUL_VOTE_DATA.with(|data| {
			*data.borrow_mut() = Some((*who, ref_index));

			REMOVE_VOTE_LOCKED_AMOUNT.with(|data| *data.borrow())
		})
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn on_vote_worst_case(_who: &u64) {}

	#[cfg(feature = "runtime-benchmarks")]
	fn on_remove_vote_worst_case(_who: &u64) {}
}

#[test]
fn voting_hooks_are_called_correctly() {
	new_test_ext().execute_with(|| {
		let c = class(3);

		let usable_balance_1 = Balances::usable_balance(1);
		dbg!(usable_balance_1);

		// Voting
		assert_ok!(Voting::vote(RuntimeOrigin::signed(1), 3, aye(1, 1)));
		assert_eq!(
			HooksHandler::last_on_vote_data(),
			Some((
				1,
				3,
				AccountVote::Standard {
					vote: Vote { aye: true, conviction: Conviction::Locked1x },
					balance: 1
				}
			))
		);
		assert_ok!(Voting::vote(RuntimeOrigin::signed(2), 3, nay(20, 2)));
		assert_eq!(
			HooksHandler::last_on_vote_data(),
			Some((
				2,
				3,
				AccountVote::Standard {
					vote: Vote { aye: false, conviction: Conviction::Locked2x },
					balance: 20
				}
			))
		);
		HooksHandler::reset();

		// removing vote while ongoing
		assert_ok!(Voting::vote(RuntimeOrigin::signed(3), 3, nay(20, 0)));
		assert_eq!(
			HooksHandler::last_on_vote_data(),
			Some((
				3,
				3,
				AccountVote::Standard {
					vote: Vote { aye: false, conviction: Conviction::None },
					balance: 20
				}
			))
		);
		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(3), Some(c), 3));
		assert_eq!(HooksHandler::last_on_remove_vote_data(), Some((3, 3, Status::Ongoing)));
		HooksHandler::reset();

		Polls::set(vec![(3, Completed(3, false))].into_iter().collect());

		// removing successful vote while completed
		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(2), Some(c), 3));
		assert_eq!(HooksHandler::last_on_remove_vote_data(), Some((2, 3, Status::Completed)));
		assert_eq!(HooksHandler::last_locked_if_unsuccessful_vote_data(), None);

		HooksHandler::reset();

		HooksHandler::with_remove_locked_amount(5);

		// removing unsuccessful vote when completed
		assert_ok!(Voting::remove_vote(RuntimeOrigin::signed(1), Some(c), 3));
		assert_eq!(HooksHandler::last_on_remove_vote_data(), Some((1, 3, Status::Completed)));
		assert_eq!(HooksHandler::last_locked_if_unsuccessful_vote_data(), Some((1, 3)));

		// Removing unsuccessful vote when completed should lock if given amount from the hook
		assert_ok!(Voting::unlock(RuntimeOrigin::signed(1), c, 1));
		assert_eq!(Balances::usable_balance(1), 5);
	});
}

#[test]
fn empty_tally_approval_is_zero() {
	new_test_ext().execute_with(|| {
		let empty_tally = Tally::<u64, <Test as Config>::MaxTurnout>::from_parts(0, 0, 0);
		assert_eq!(
			<TallyOf<Test> as VoteTally<u64, u8>>::approval(&empty_tally, 0),
			Perbill::zero(),
		);

		let only_ayes = Tally::<u64, <Test as Config>::MaxTurnout>::from_parts(10, 0, 10);
		assert_eq!(<TallyOf<Test> as VoteTally<u64, u8>>::approval(&only_ayes, 0), Perbill::one(),);

		let only_nays = Tally::<u64, <Test as Config>::MaxTurnout>::from_parts(0, 10, 0);
		assert_eq!(<TallyOf<Test> as VoteTally<u64, u8>>::approval(&only_nays, 0), Perbill::zero(),);

		let mixed = Tally::<u64, <Test as Config>::MaxTurnout>::from_parts(3, 7, 3);
		assert_eq!(
			<TallyOf<Test> as VoteTally<u64, u8>>::approval(&mixed, 0),
			Perbill::from_percent(30),
		);
	});
}
