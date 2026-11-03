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

//! Integration test together with the ranked-collective pallet.

#![allow(deprecated)]

use frame_support::{
	assert_noop, assert_ok, derive_impl, hypothetically, hypothetically_ok, ord_parameter_types,
	pallet_prelude::Weight,
	parameter_types,
	traits::{ConstU16, EitherOf, IsInVec, MapSuccess, NoOpPoll, TryMapSuccess},
};
use frame_system::{pallet_prelude::BlockNumberFor, EnsureSignedBy};
use pallet_ranked_collective::{EnsureRanked, Geometric, Rank};
use sp_core::Get;
use sp_runtime::{
	bounded_vec,
	traits::{Convert, ReduceBy, ReplaceWithDefault, TryMorphInto},
	BuildStorage,
};
type Class = Rank;

use crate as pallet_core_fellowship;
use crate::*;

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		CoreFellowship: pallet_core_fellowship,
		Club: pallet_ranked_collective,
	}
);

parameter_types! {
	pub BlockWeights: frame_system::limits::BlockWeights =
		frame_system::limits::BlockWeights::simple_max(Weight::from_parts(1_000_000, u64::max_value()));
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
}

parameter_types! {
	pub static MinRankOfClassDelta: Rank = 0;
}

parameter_types! {
	pub ZeroToNine: Vec<u64> = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
	pub EvidenceSize: u32 = 1024;
}
ord_parameter_types! {
	pub const One: u64 = 1;
}

impl Config for Test {
	type WeightInfo = ();
	type RuntimeEvent = RuntimeEvent;
	type Members = Club;
	type Balance = u64;
	type ParamsOrigin = EnsureSignedBy<One, u64>;
	type InductOrigin = EnsureInducted<Test, (), 1>;
	type ApproveOrigin = TryMapSuccess<EnsureSignedBy<IsInVec<ZeroToNine>, u64>, TryMorphInto<u16>>;
	type PromoteOrigin = TryMapSuccess<EnsureSignedBy<IsInVec<ZeroToNine>, u64>, TryMorphInto<u16>>;
	type FastPromoteOrigin = Self::PromoteOrigin;
	type EvidenceSize = EvidenceSize;
	type MaxRank = ConstU16<9>;
	type BlockNumberProvider = frame_system::Pallet<Test>;
}

/// Convert the tally class into the minimum rank required to vote on the poll.
/// MinRank(Class) = Class - Delta
pub struct MinRankOfClass<Delta>(PhantomData<Delta>);
impl<Delta: Get<Rank>> Convert<Class, Rank> for MinRankOfClass<Delta> {
	fn convert(a: Class) -> Rank {
		a.saturating_sub(Delta::get())
	}
}

impl pallet_ranked_collective::Config for Test {
	type WeightInfo = ();
	type RuntimeEvent = RuntimeEvent;
	type PromoteOrigin = EitherOf<
		// Root can promote arbitrarily.
		frame_system::EnsureRootWithSuccess<Self::AccountId, ConstU16<65535>>,
		// Members can promote up to the rank of 2 below them.
		MapSuccess<EnsureRanked<Test, (), 2>, ReduceBy<ConstU16<2>>>,
	>;
	type AddOrigin = MapSuccess<Self::PromoteOrigin, ReplaceWithDefault<()>>;
	type DemoteOrigin = EitherOf<
		// Root can demote arbitrarily.
		frame_system::EnsureRootWithSuccess<Self::AccountId, ConstU16<65535>>,
		// Members can demote up to the rank of 3 below them.
		MapSuccess<EnsureRanked<Test, (), 3>, ReduceBy<ConstU16<3>>>,
	>;
	type RemoveOrigin = Self::DemoteOrigin;
	type ExchangeOrigin = EitherOf<
		// Root can exchange arbitrarily.
		frame_system::EnsureRootWithSuccess<Self::AccountId, ConstU16<65535>>,
		// Members can exchange up to the rank of 2 below them.
		MapSuccess<EnsureRanked<Test, (), 2>, ReduceBy<ConstU16<2>>>,
	>;
	type Polls = NoOpPoll<BlockNumberFor<Test>>;
	type MinRankOfClass = MinRankOfClass<MinRankOfClassDelta>;
	type MemberSwappedHandler = CoreFellowship;
	type VoteWeight = Geometric;
	type MaxMemberCount = ();
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkSetup = CoreFellowship;
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| {
		assert_ok!(Club::add_member(RuntimeOrigin::root(), 100));
		promote_n_times(100, 9);
		let params = ParamsType {
			active_salary: bounded_vec![10, 20, 30, 40, 50, 60, 70, 80, 90],
			passive_salary: bounded_vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
			demotion_period: bounded_vec![2, 4, 6, 8, 10, 12, 14, 16, 18],
			min_promotion_period: bounded_vec![3, 6, 9, 12, 15, 18, 21, 24, 27],
			offboard_timeout: 1,
		};
		assert_ok!(CoreFellowship::set_params(signed(1), Box::new(params)));
		System::set_block_number(1);
	});
	ext
}

fn promote_n_times(acc: u64, r: u16) {
	for _ in 0..r {
		assert_ok!(Club::promote_member(RuntimeOrigin::root(), acc));
	}
}

fn signed(who: u64) -> RuntimeOrigin {
	RuntimeOrigin::signed(who)
}

fn assert_last_event(generic_event: <Test as Config>::RuntimeEvent) {
	let events = frame_system::Pallet::<Test>::events();
	let system_event: <Test as frame_system::Config>::RuntimeEvent = generic_event.into();
	let frame_system::EventRecord { event, .. } = events.last().expect("Event expected");
	assert_eq!(event, &system_event.into());
}

fn evidence(e: u32) -> Evidence<Test, ()> {
	e.encode()
		.into_iter()
		.cycle()
		.take(1024)
		.collect::<Vec<_>>()
		.try_into()
		.expect("Static length matches")
}

#[test]
fn import_simple_works() {
	new_test_ext().execute_with(|| {
		for i in 0u16..9 {
			let acc = i as u64;

			// Does not work yet
			assert_noop!(CoreFellowship::import(signed(acc)), Error::<Test>::Unranked);
			assert_noop!(
				CoreFellowship::import_member(signed(acc + 1), acc),
				Error::<Test>::Unranked
			);

			assert_ok!(Club::add_member(RuntimeOrigin::root(), acc));
			promote_n_times(acc, i);

			hypothetically_ok!(CoreFellowship::import(signed(acc)));
			hypothetically_ok!(CoreFellowship::import_member(signed(acc), acc));
			// Works from other accounts
			assert_ok!(CoreFellowship::import_member(signed(acc + 1), acc));

			// Does not work again
			assert_noop!(CoreFellowship::import(signed(acc)), Error::<Test>::AlreadyInducted);
			assert_noop!(
				CoreFellowship::import_member(signed(acc + 1), acc),
				Error::<Test>::AlreadyInducted
			);
		}
	});
}

#[test]
fn swap_simple_works() {
	new_test_ext().execute_with(|| {
		for i in 0u16..9 {
			let acc = i as u64;

			assert_ok!(Club::add_member(RuntimeOrigin::root(), acc));
			promote_n_times(acc, i);
			hypothetically_ok!(CoreFellowship::import(signed(acc)));
			assert_ok!(CoreFellowship::import_member(signed(acc), acc));

			// Swapping normally works:
			assert_ok!(Club::exchange_member(RuntimeOrigin::root(), acc, acc + 10));
			assert_last_event(Event::Swapped { who: acc, new_who: acc + 10 }.into());
		}
	});
}

/// Exhaustively test that adding member `1` is equivalent to adding member `0` and then swapping.
///
/// The member also submits evidence before the swap.
#[test]
fn swap_exhaustive_works() {
	new_test_ext().execute_with(|| {
		let root_add = hypothetically!({
			assert_ok!(Club::add_member(RuntimeOrigin::root(), 1));
			promote_n_times(1, 4);
			assert_ok!(CoreFellowship::import(signed(1)));
			assert_ok!(CoreFellowship::submit_evidence(signed(1), Wish::Retention, evidence(1)));

			// The events mess up the storage root:
			System::reset_events();
			sp_io::storage::root(sp_runtime::StateVersion::V1)
		});

		let root_swap = hypothetically!({
			assert_ok!(Club::add_member(RuntimeOrigin::root(), 0));
			promote_n_times(0, 4);
			assert_ok!(CoreFellowship::import(signed(0)));
			assert_ok!(CoreFellowship::submit_evidence(signed(0), Wish::Retention, evidence(1)));

			// Now we swap:
			assert_ok!(Club::exchange_member(RuntimeOrigin::root(), 0, 1));

			System::reset_events();
			sp_io::storage::root(sp_runtime::StateVersion::V1)
		});

		assert_eq!(root_add, root_swap);
		// Ensure that we don't compare trivial stuff like `()` from a type error above.
		assert_eq!(root_add.len(), 32);
	});
}

#[test]
fn swap_bad_noops() {
	new_test_ext().execute_with(|| {
		assert_ok!(Club::add_member(RuntimeOrigin::root(), 0));
		promote_n_times(0, 0);
		assert_ok!(CoreFellowship::import(signed(0)));
		assert_ok!(Club::add_member(RuntimeOrigin::root(), 1));
		promote_n_times(1, 1);
		assert_ok!(CoreFellowship::import(signed(1)));

		// Swapping for another member is a noop:
		assert_noop!(
			Club::exchange_member(RuntimeOrigin::root(), 0, 1),
			pallet_ranked_collective::Error::<Test>::AlreadyMember
		);
		// Swapping for the same member is a noop:
		assert_noop!(
			Club::exchange_member(RuntimeOrigin::root(), 0, 0),
			pallet_ranked_collective::Error::<Test>::SameMember
		);
	});
}
