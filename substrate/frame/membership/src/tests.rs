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

//! Tests for the module.

use crate as pallet_membership;
use crate::{mock::*, *};

use sp_runtime::{bounded_vec, traits::BadOrigin, BuildStorage};

use frame_support::{assert_noop, assert_ok, assert_storage_noop, traits::StorageVersion};

#[test]
fn query_membership_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert_eq!(crate::Members::<Test>::get(), vec![10, 20, 30]);
		assert_eq!(MEMBERS.with(|m| m.borrow().clone()), vec![10, 20, 30]);
	});
}

#[test]
fn prime_member_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert_noop!(Membership::set_prime(RuntimeOrigin::signed(4), 20), BadOrigin);
		assert_noop!(
			Membership::set_prime(RuntimeOrigin::signed(5), 15),
			Error::<Test, _>::NotMember
		);
		assert_ok!(Membership::set_prime(RuntimeOrigin::signed(5), 20));
		assert_eq!(crate::Prime::<Test>::get(), Some(20));
		assert_eq!(PRIME.with(|m| *m.borrow()), crate::Prime::<Test>::get());

		assert_ok!(Membership::clear_prime(RuntimeOrigin::signed(5)));
		assert_eq!(crate::Prime::<Test>::get(), None);
		assert_eq!(PRIME.with(|m| *m.borrow()), crate::Prime::<Test>::get());
	});
}

#[test]
fn add_member_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert_noop!(Membership::add_member(RuntimeOrigin::signed(5), 15), BadOrigin);
		assert_noop!(
			Membership::add_member(RuntimeOrigin::signed(1), 10),
			Error::<Test, _>::AlreadyMember
		);
		assert_ok!(Membership::add_member(RuntimeOrigin::signed(1), 15));
		assert_eq!(crate::Members::<Test>::get(), vec![10, 15, 20, 30]);
		assert_eq!(MEMBERS.with(|m| m.borrow().clone()), crate::Members::<Test>::get().to_vec());
	});
}

#[test]
fn remove_member_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert_noop!(Membership::remove_member(RuntimeOrigin::signed(5), 20), BadOrigin);
		assert_noop!(
			Membership::remove_member(RuntimeOrigin::signed(2), 15),
			Error::<Test, _>::NotMember
		);
		assert_ok!(Membership::set_prime(RuntimeOrigin::signed(5), 20));
		assert_ok!(Membership::remove_member(RuntimeOrigin::signed(2), 20));
		assert_eq!(crate::Members::<Test>::get(), vec![10, 30]);
		assert_eq!(MEMBERS.with(|m| m.borrow().clone()), crate::Members::<Test>::get().to_vec());
		assert_eq!(crate::Prime::<Test>::get(), None);
		assert_eq!(PRIME.with(|m| *m.borrow()), crate::Prime::<Test>::get());
	});
}

#[test]
fn swap_member_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert_noop!(Membership::swap_member(RuntimeOrigin::signed(5), 10, 25), BadOrigin);
		assert_noop!(
			Membership::swap_member(RuntimeOrigin::signed(3), 15, 25),
			Error::<Test, _>::NotMember
		);
		assert_noop!(
			Membership::swap_member(RuntimeOrigin::signed(3), 10, 30),
			Error::<Test, _>::AlreadyMember
		);

		assert_ok!(Membership::set_prime(RuntimeOrigin::signed(5), 20));
		assert_ok!(Membership::swap_member(RuntimeOrigin::signed(3), 20, 20));
		assert_eq!(crate::Members::<Test>::get(), vec![10, 20, 30]);
		assert_eq!(crate::Prime::<Test>::get(), Some(20));
		assert_eq!(PRIME.with(|m| *m.borrow()), crate::Prime::<Test>::get());

		assert_ok!(Membership::set_prime(RuntimeOrigin::signed(5), 10));
		assert_ok!(Membership::swap_member(RuntimeOrigin::signed(3), 10, 25));
		assert_eq!(crate::Members::<Test>::get(), vec![20, 25, 30]);
		assert_eq!(MEMBERS.with(|m| m.borrow().clone()), crate::Members::<Test>::get().to_vec());
		assert_eq!(crate::Prime::<Test>::get(), None);
		assert_eq!(PRIME.with(|m| *m.borrow()), crate::Prime::<Test>::get());
	});
}

#[test]
fn swap_member_works_that_does_not_change_order() {
	ExtBuilder::default().build_and_execute(|| {
		assert_ok!(Membership::swap_member(RuntimeOrigin::signed(3), 10, 5));
		assert_eq!(crate::Members::<Test>::get(), vec![5, 20, 30]);
		assert_eq!(MEMBERS.with(|m| m.borrow().clone()), crate::Members::<Test>::get().to_vec());
	});
}

#[test]
fn swap_member_with_identical_arguments_changes_nothing() {
	ExtBuilder::default().build_and_execute(|| {
		assert_storage_noop!(assert_ok!(Membership::swap_member(RuntimeOrigin::signed(3), 10, 10)));
	});
}

#[test]
fn change_key_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert_ok!(Membership::set_prime(RuntimeOrigin::signed(5), 10));
		assert_noop!(
			Membership::change_key(RuntimeOrigin::signed(3), 25),
			Error::<Test, _>::NotMember
		);
		assert_noop!(
			Membership::change_key(RuntimeOrigin::signed(10), 20),
			Error::<Test, _>::AlreadyMember
		);
		assert_ok!(Membership::change_key(RuntimeOrigin::signed(10), 40));
		assert_eq!(crate::Members::<Test>::get(), vec![20, 30, 40]);
		assert_eq!(MEMBERS.with(|m| m.borrow().clone()), crate::Members::<Test>::get().to_vec());
		assert_eq!(crate::Prime::<Test>::get(), Some(40));
		assert_eq!(PRIME.with(|m| *m.borrow()), crate::Prime::<Test>::get());
	});
}

#[test]
fn change_key_works_that_does_not_change_order() {
	ExtBuilder::default().build_and_execute(|| {
		assert_ok!(Membership::change_key(RuntimeOrigin::signed(10), 5));
		assert_eq!(crate::Members::<Test>::get(), vec![5, 20, 30]);
		assert_eq!(MEMBERS.with(|m| m.borrow().clone()), crate::Members::<Test>::get().to_vec());
	});
}

#[test]
fn change_key_preserves_prime_notification_when_prime_is_unaffected() {
	ExtBuilder::default().build_and_execute(|| {
		assert_ok!(Membership::set_prime(RuntimeOrigin::signed(5), 20));
		assert_ok!(Membership::change_key(RuntimeOrigin::signed(10), 40));
		assert_eq!(crate::Members::<Test>::get(), vec![20, 30, 40]);
		assert_eq!(crate::Prime::<Test>::get(), Some(20));
		assert_eq!(PRIME.with(|m| *m.borrow()), Some(20));
	});
}

#[test]
fn change_key_with_same_caller_as_argument_changes_nothing() {
	ExtBuilder::default().build_and_execute(|| {
		assert_storage_noop!(assert_ok!(Membership::change_key(RuntimeOrigin::signed(10), 10)));
	});
}

#[test]
fn reset_members_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert_ok!(Membership::set_prime(RuntimeOrigin::signed(5), 20));
		assert_noop!(
			Membership::reset_members(RuntimeOrigin::signed(1), bounded_vec![20, 40, 30]),
			BadOrigin
		);

		assert_ok!(Membership::reset_members(RuntimeOrigin::signed(4), vec![20, 40, 30]));
		assert_eq!(crate::Members::<Test>::get(), vec![20, 30, 40]);
		assert_eq!(MEMBERS.with(|m| m.borrow().clone()), crate::Members::<Test>::get().to_vec());
		assert_eq!(crate::Prime::<Test>::get(), Some(20));
		assert_eq!(PRIME.with(|m| *m.borrow()), crate::Prime::<Test>::get());

		assert_ok!(Membership::reset_members(RuntimeOrigin::signed(4), vec![10, 40, 30]));
		assert_eq!(crate::Members::<Test>::get(), vec![10, 30, 40]);
		assert_eq!(MEMBERS.with(|m| m.borrow().clone()), crate::Members::<Test>::get().to_vec());
		assert_eq!(crate::Prime::<Test>::get(), None);
		assert_eq!(PRIME.with(|m| *m.borrow()), crate::Prime::<Test>::get());
	});
}

#[test]
#[should_panic(expected = "Members cannot contain duplicate accounts.")]
fn genesis_build_panics_with_duplicate_members() {
	pallet_membership::GenesisConfig::<Test> {
		members: bounded_vec![1, 2, 3, 1],
		phantom: Default::default(),
	}
	.build_storage()
	.unwrap();
}

#[test]
fn migration_v4() {
	ExtBuilder::default().build_and_execute(|| {
		use frame_support::traits::PalletInfo;
		let old_pallet_name = "OldMembership";
		let new_pallet_name =
			<Test as frame_system::Config>::PalletInfo::name::<Membership>().unwrap();

		frame_support::storage::migration::move_pallet(
			new_pallet_name.as_bytes(),
			old_pallet_name.as_bytes(),
		);

		StorageVersion::new(0).put::<Membership>();

		crate::migrations::v4::pre_migrate::<Membership, _>(old_pallet_name, new_pallet_name);
		crate::migrations::v4::migrate::<Test, Membership, _>(old_pallet_name, new_pallet_name);
		crate::migrations::v4::post_migrate::<Membership, _>(old_pallet_name, new_pallet_name);
	});
}
