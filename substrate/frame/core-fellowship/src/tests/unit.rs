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

#![allow(deprecated)]

use std::collections::BTreeMap;

use core::cell::RefCell;
use frame_support::{
	assert_noop, assert_ok, derive_impl, hypothetically, ord_parameter_types,
	pallet_prelude::Weight,
	parameter_types,
	traits::{tokens::GetSalary, ConstU16, ConstU32, IsInVec, TryMapSuccess},
};
use frame_system::EnsureSignedBy;
use sp_runtime::{bounded_vec, traits::TryMorphInto, BuildStorage, DispatchError, DispatchResult};

use crate as pallet_core_fellowship;
use crate::*;

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		CoreFellowship: pallet_core_fellowship,
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

thread_local! {
	pub static CLUB: RefCell<BTreeMap<u64, u16>> = RefCell::new(BTreeMap::new());
}

pub struct TestClub;
impl RankedMembers for TestClub {
	type AccountId = u64;
	type Rank = u16;
	fn min_rank() -> Self::Rank {
		0
	}
	fn rank_of(who: &Self::AccountId) -> Option<Self::Rank> {
		CLUB.with(|club| club.borrow().get(who).cloned())
	}
	fn induct(who: &Self::AccountId) -> DispatchResult {
		CLUB.with(|club| club.borrow_mut().insert(*who, 0));
		Ok(())
	}
	fn promote(who: &Self::AccountId) -> DispatchResult {
		CLUB.with(|club| {
			club.borrow_mut().entry(*who).and_modify(|r| *r += 1);
		});
		Ok(())
	}
	fn demote(who: &Self::AccountId) -> DispatchResult {
		CLUB.with(|club| match Self::rank_of(who) {
			None => Err(sp_runtime::DispatchError::Unavailable),
			Some(0) => {
				club.borrow_mut().remove(&who);
				Ok(())
			},
			Some(_) => {
				club.borrow_mut().entry(*who).and_modify(|x| *x -= 1);
				Ok(())
			},
		})
	}
}

fn set_rank(who: u64, rank: u16) {
	CLUB.with(|club| club.borrow_mut().insert(who, rank));
}

fn unrank(who: u64) {
	CLUB.with(|club| club.borrow_mut().remove(&who));
}

parameter_types! {
	pub ZeroToNine: Vec<u64> = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
}
ord_parameter_types! {
	pub const One: u64 = 1;
}

impl Config for Test {
	type WeightInfo = ();
	type RuntimeEvent = RuntimeEvent;
	type Members = TestClub;
	type Balance = u64;
	type ParamsOrigin = EnsureSignedBy<One, u64>;
	type InductOrigin = EnsureInducted<Test, (), 1>;
	type ApproveOrigin = TryMapSuccess<EnsureSignedBy<IsInVec<ZeroToNine>, u64>, TryMorphInto<u16>>;
	type PromoteOrigin = TryMapSuccess<EnsureSignedBy<IsInVec<ZeroToNine>, u64>, TryMorphInto<u16>>;
	type FastPromoteOrigin = Self::PromoteOrigin;
	type EvidenceSize = ConstU32<1024>;
	type MaxRank = ConstU16<9>;
	type BlockNumberProvider = frame_system::Pallet<Test>;
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| {
		set_rank(100, 9);
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

fn next_block() {
	System::set_block_number(System::block_number() + 1);
}

fn run_to(n: u64) {
	while System::block_number() < n {
		next_block();
	}
}

fn signed(who: u64) -> RuntimeOrigin {
	RuntimeOrigin::signed(who)
}

fn next_demotion(who: u64) -> u64 {
	let member = Member::<Test>::get(who).unwrap();
	let demotion_period = Params::<Test>::get().demotion_period;
	member.last_proof + demotion_period[TestClub::rank_of(&who).unwrap() as usize - 1]
}

#[test]
fn basic_stuff() {
	new_test_ext().execute_with(|| {
		assert_eq!(CoreFellowship::rank_to_index(0), None);
		assert_eq!(CoreFellowship::rank_to_index(1), Some(0));
		assert_eq!(CoreFellowship::rank_to_index(9), Some(8));
		assert_eq!(CoreFellowship::rank_to_index(10), None);
		assert_eq!(CoreFellowship::get_salary(0, &1), 0);
	});
}

#[test]
fn set_params_works() {
	new_test_ext().execute_with(|| {
		let params = ParamsType {
			active_salary: bounded_vec![10, 20, 30, 40, 50, 60, 70, 80, 90],
			passive_salary: bounded_vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
			demotion_period: bounded_vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
			min_promotion_period: bounded_vec![1, 2, 3, 4, 5, 10, 15, 20, 30],
			offboard_timeout: 1,
		};
		assert_noop!(
			CoreFellowship::set_params(signed(2), Box::new(params.clone())),
			DispatchError::BadOrigin
		);
		assert_ok!(CoreFellowship::set_params(signed(1), Box::new(params)));
	});
}

#[test]
fn set_partial_params_works() {
	new_test_ext().execute_with(|| {
		let params = ParamsType {
			active_salary: bounded_vec![None; 9],
			passive_salary: bounded_vec![None; 9],
			demotion_period: bounded_vec![None, Some(10), None, None, None, None, None, None, None],
			min_promotion_period: bounded_vec![None; 9],
			offboard_timeout: Some(2),
		};
		assert_noop!(
			CoreFellowship::set_partial_params(signed(2), Box::new(params.clone())),
			DispatchError::BadOrigin
		);
		assert_ok!(CoreFellowship::set_partial_params(signed(1), Box::new(params)));

		// Update params from the base params value declared in `new_test_ext`
		let raw_updated_params = ParamsType {
			active_salary: bounded_vec![10, 20, 30, 40, 50, 60, 70, 80, 90],
			passive_salary: bounded_vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
			demotion_period: bounded_vec![2, 10, 6, 8, 10, 12, 14, 16, 18],
			min_promotion_period: bounded_vec![3, 6, 9, 12, 15, 18, 21, 24, 27],
			offboard_timeout: 2,
		};
		// Updated params stored in Params storage value
		let updated_params = Params::<Test>::get();
		assert_eq!(raw_updated_params, updated_params);

		System::assert_last_event(
			Event::<Test, _>::ParamsChanged { params: updated_params }.into(),
		);
	});
}

#[test]
fn import_member_works() {
	new_test_ext().execute_with(|| {
		assert_noop!(CoreFellowship::import_member(signed(0), 0), Error::<Test>::Unranked);
		assert_noop!(CoreFellowship::import(signed(0)), Error::<Test>::Unranked);

		// Make induction work:
		set_rank(0, 1);
		assert!(!Member::<Test>::contains_key(0), "not yet imported");

		// `import_member` can be used to induct ourselves:
		hypothetically!({
			assert_ok!(CoreFellowship::import_member(signed(0), 0));
			assert!(Member::<Test>::contains_key(0), "got imported");

			// Twice does not work:
			assert_noop!(
				CoreFellowship::import_member(signed(0), 0),
				Error::<Test>::AlreadyInducted
			);
			assert_noop!(CoreFellowship::import(signed(0)), Error::<Test>::AlreadyInducted);
		});

		// But we could have also used `import`:
		hypothetically!({
			assert_ok!(CoreFellowship::import(signed(0)));
			assert!(Member::<Test>::contains_key(0), "got imported");

			// Twice does not work:
			assert_noop!(
				CoreFellowship::import_member(signed(0), 0),
				Error::<Test>::AlreadyInducted
			);
			assert_noop!(CoreFellowship::import(signed(0)), Error::<Test>::AlreadyInducted);
		});
	});
}

#[test]
fn import_member_same_as_import() {
	new_test_ext().execute_with(|| {
		for rank in 0..=9 {
			set_rank(0, rank);

			let import_root = hypothetically!({
				assert_ok!(CoreFellowship::import(signed(0)));
				sp_io::storage::root(sp_runtime::StateVersion::V1)
			});

			let import_member_root = hypothetically!({
				assert_ok!(CoreFellowship::import_member(signed(1), 0));
				sp_io::storage::root(sp_runtime::StateVersion::V1)
			});

			// `import` and `import_member` do exactly the same thing.
			assert_eq!(import_root, import_member_root);
		}
	});
}

#[test]
fn induct_works() {
	new_test_ext().execute_with(|| {
		set_rank(0, 0);
		assert_ok!(CoreFellowship::import(signed(0)));
		set_rank(1, 1);
		assert_ok!(CoreFellowship::import(signed(1)));

		assert_noop!(CoreFellowship::induct(signed(10), 10), DispatchError::BadOrigin);
		assert_noop!(CoreFellowship::induct(signed(0), 10), DispatchError::BadOrigin);
		assert_ok!(CoreFellowship::induct(signed(1), 10));
		assert_noop!(CoreFellowship::induct(signed(1), 10), Error::<Test>::AlreadyInducted);
	});
}

#[test]
fn promote_works() {
	new_test_ext().execute_with(|| {
		set_rank(1, 1);
		assert_ok!(CoreFellowship::import(signed(1)));
		assert_noop!(CoreFellowship::promote(signed(1), 10, 1), Error::<Test>::Unranked);

		assert_ok!(CoreFellowship::induct(signed(1), 10));
		assert_noop!(CoreFellowship::promote(signed(10), 10, 1), DispatchError::BadOrigin);
		assert_noop!(CoreFellowship::promote(signed(0), 10, 1), Error::<Test>::NoPermission);
		assert_noop!(CoreFellowship::promote(signed(3), 10, 2), Error::<Test>::UnexpectedRank);
		run_to(3);
		assert_noop!(CoreFellowship::promote(signed(1), 10, 1), Error::<Test>::TooSoon);
		run_to(4);
		assert_ok!(CoreFellowship::promote(signed(1), 10, 1));
		set_rank(11, 0);
		assert_noop!(CoreFellowship::promote(signed(1), 11, 1), Error::<Test>::NotTracked);
	});
}

#[test]
fn promote_fast_works() {
	let alice = 1;

	new_test_ext().execute_with(|| {
		assert_noop!(
			CoreFellowship::promote_fast(signed(alice), alice, 1),
			Error::<Test>::Unranked
		);
		set_rank(alice, 0);
		assert_noop!(
			CoreFellowship::promote_fast(signed(alice), alice, 1),
			Error::<Test>::NotTracked
		);
		assert_ok!(CoreFellowship::import(signed(alice)));

		// Cannot fast promote to the same rank:
		assert_noop!(
			CoreFellowship::promote_fast(signed(alice), alice, 0),
			Error::<Test>::UnexpectedRank
		);
		assert_ok!(CoreFellowship::promote_fast(signed(alice), alice, 1));
		assert_eq!(TestClub::rank_of(&alice), Some(1));

		// Cannot promote normally because of the period:
		assert_noop!(CoreFellowship::promote(signed(2), alice, 2), Error::<Test>::TooSoon);
		// But can fast promote:
		assert_ok!(CoreFellowship::promote_fast(signed(2), alice, 2));
		assert_eq!(TestClub::rank_of(&alice), Some(2));

		// Cannot promote to lower rank:
		assert_noop!(
			CoreFellowship::promote_fast(signed(alice), alice, 0),
			Error::<Test>::UnexpectedRank
		);
		assert_noop!(
			CoreFellowship::promote_fast(signed(alice), alice, 1),
			Error::<Test>::UnexpectedRank
		);
		// Permission is checked:
		assert_noop!(
			CoreFellowship::promote_fast(signed(alice), alice, 2),
			Error::<Test>::NoPermission
		);

		// Can fast promote up to the maximum:
		assert_ok!(CoreFellowship::promote_fast(signed(9), alice, 9));
		// But not past the maximum:
		assert_noop!(
			CoreFellowship::promote_fast(RuntimeOrigin::root(), alice, 10),
			Error::<Test>::InvalidRank
		);
	});
}

/// Compare the storage root hashes of a normal promote and a fast promote.
#[test]
fn promote_fast_identical_to_promote() {
	let alice = 1;

	new_test_ext().execute_with(|| {
		set_rank(alice, 0);
		assert_eq!(TestClub::rank_of(&alice), Some(0));
		assert_ok!(CoreFellowship::import(signed(alice)));
		run_to(3);
		assert_eq!(TestClub::rank_of(&alice), Some(0));
		assert_ok!(CoreFellowship::submit_evidence(
			signed(alice),
			Wish::Promotion,
			bounded_vec![0; 1024]
		));

		let root_promote = hypothetically!({
			assert_ok!(CoreFellowship::promote(signed(alice), alice, 1));
			// Don't clean the events since they should emit the same events:
			sp_io::storage::root(sp_runtime::StateVersion::V1)
		});

		// This is using thread locals instead of storage...
		TestClub::demote(&alice).unwrap();

		let root_promote_fast = hypothetically!({
			assert_ok!(CoreFellowship::promote_fast(signed(alice), alice, 1));

			sp_io::storage::root(sp_runtime::StateVersion::V1)
		});

		assert_eq!(root_promote, root_promote_fast);
		// Ensure that we don't compare trivial stuff like `()` from a type error above.
		assert_eq!(root_promote.len(), 32);
	});
}

#[test]
fn sync_works() {
	new_test_ext().execute_with(|| {
		set_rank(10, 5);
		assert_noop!(CoreFellowship::approve(signed(4), 10, 5), Error::<Test>::NoPermission);
		assert_noop!(CoreFellowship::approve(signed(6), 10, 6), Error::<Test>::UnexpectedRank);
		assert_ok!(CoreFellowship::import(signed(10)));
		assert!(Member::<Test>::contains_key(10));
		assert_eq!(next_demotion(10), 11);
	});
}

#[test]
fn auto_demote_works() {
	new_test_ext().execute_with(|| {
		set_rank(10, 5);
		assert_ok!(CoreFellowship::import(signed(10)));

		run_to(10);
		assert_noop!(CoreFellowship::bump(signed(0), 10), Error::<Test>::NothingDoing);
		run_to(11);
		assert_ok!(CoreFellowship::bump(signed(0), 10));
		assert_eq!(TestClub::rank_of(&10), Some(4));
		assert_noop!(CoreFellowship::bump(signed(0), 10), Error::<Test>::NothingDoing);
		assert_eq!(next_demotion(10), 19);
	});
}

#[test]
fn auto_demote_offboard_works() {
	new_test_ext().execute_with(|| {
		set_rank(10, 1);
		assert_ok!(CoreFellowship::import(signed(10)));

		run_to(3);
		assert_ok!(CoreFellowship::bump(signed(0), 10));
		assert_eq!(TestClub::rank_of(&10), Some(0));
		assert_noop!(CoreFellowship::bump(signed(0), 10), Error::<Test>::NothingDoing);
		run_to(4);
		assert_ok!(CoreFellowship::bump(signed(0), 10));
		assert_noop!(CoreFellowship::bump(signed(0), 10), Error::<Test>::NotTracked);
	});
}

#[test]
fn offboard_works() {
	new_test_ext().execute_with(|| {
		assert_noop!(CoreFellowship::offboard(signed(0), 10), Error::<Test>::NotTracked);
		set_rank(10, 0);
		assert_noop!(CoreFellowship::offboard(signed(0), 10), Error::<Test>::Ranked);

		assert_ok!(CoreFellowship::import(signed(10)));
		assert_noop!(CoreFellowship::offboard(signed(0), 10), Error::<Test>::Ranked);

		unrank(10);
		assert_ok!(CoreFellowship::offboard(signed(0), 10));
		assert_noop!(CoreFellowship::offboard(signed(0), 10), Error::<Test>::NotTracked);
		assert_noop!(CoreFellowship::bump(signed(0), 10), Error::<Test>::NotTracked);
	});
}

#[test]
fn infinite_demotion_period_works() {
	new_test_ext().execute_with(|| {
		let params = ParamsType {
			active_salary: bounded_vec![10, 10, 10, 10, 10, 10, 10, 10, 10],
			passive_salary: bounded_vec![10, 10, 10, 10, 10, 10, 10, 10, 10],
			min_promotion_period: bounded_vec![10, 10, 10, 10, 10, 10, 10, 10, 10],
			demotion_period: bounded_vec![0, 0, 0, 0, 0, 0, 0, 0, 0],
			offboard_timeout: 0,
		};
		assert_ok!(CoreFellowship::set_params(signed(1), Box::new(params)));

		set_rank(0, 0);
		assert_ok!(CoreFellowship::import(signed(0)));
		set_rank(1, 1);
		assert_ok!(CoreFellowship::import(signed(1)));

		assert_noop!(CoreFellowship::bump(signed(0), 0), Error::<Test>::NothingDoing);
		assert_noop!(CoreFellowship::bump(signed(0), 1), Error::<Test>::NothingDoing);
	});
}

#[test]
fn proof_postpones_auto_demote() {
	new_test_ext().execute_with(|| {
		set_rank(10, 5);
		assert_ok!(CoreFellowship::import(signed(10)));

		run_to(11);
		assert_ok!(CoreFellowship::approve(signed(5), 10, 5));
		assert_eq!(next_demotion(10), 21);
		assert_noop!(CoreFellowship::bump(signed(0), 10), Error::<Test>::NothingDoing);
	});
}

#[test]
fn promote_postpones_auto_demote() {
	new_test_ext().execute_with(|| {
		set_rank(10, 5);
		assert_ok!(CoreFellowship::import(signed(10)));

		run_to(19);
		assert_ok!(CoreFellowship::promote(signed(6), 10, 6));
		assert_eq!(next_demotion(10), 31);
		assert_noop!(CoreFellowship::bump(signed(0), 10), Error::<Test>::NothingDoing);
	});
}

#[test]
fn get_salary_works() {
	new_test_ext().execute_with(|| {
		for i in 1..=9u64 {
			set_rank(10 + i, i as u16);
			assert_ok!(CoreFellowship::import(signed(10 + i)));
			assert_eq!(CoreFellowship::get_salary(i as u16, &(10 + i)), i * 10);
		}
	});
}

#[test]
fn active_changing_get_salary_works() {
	new_test_ext().execute_with(|| {
		for i in 1..=9u64 {
			set_rank(10 + i, i as u16);
			assert_ok!(CoreFellowship::import(signed(10 + i)));
			assert_ok!(CoreFellowship::set_active(signed(10 + i), false));
			assert_eq!(CoreFellowship::get_salary(i as u16, &(10 + i)), i);
			assert_ok!(CoreFellowship::set_active(signed(10 + i), true));
			assert_eq!(CoreFellowship::get_salary(i as u16, &(10 + i)), i * 10);
		}
	});
}
