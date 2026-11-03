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

//! Test utilities

use super::*;

use crate as recovery;
use crate::HoldReason;
use frame::{
	deps::sp_io,
	testing_prelude::*,
	token::fungible::{Balanced as _, HoldConsideration},
	traits::{LinearStoragePrice, OnUnbalanced},
};

type Block = frame_system::mocking::MockBlock<Test>;

construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		Balances: pallet_balances,
		Recovery: recovery,
		Utility: pallet_utility,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u128>;
}

parameter_types! {
	pub const ExistentialDeposit: u64 = 1;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type Balance = u128;
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
}

parameter_types! {
	pub const MaxFriendsPerConfig: u32 = 128;

	pub const FriendGroupsHoldReason: RuntimeHoldReason = RuntimeHoldReason::Recovery(HoldReason::FriendGroupsStorage);
	pub const AttemptHoldReason: RuntimeHoldReason = RuntimeHoldReason::Recovery(HoldReason::AttemptStorage);
	pub const InheritorHoldReason: RuntimeHoldReason = RuntimeHoldReason::Recovery(HoldReason::InheritorStorage);
}

pub const SECURITY_DEPOSIT: u128 = 100;

parameter_types! {
	pub storage SlashReceiverAccount: Option<u64> = None;
}

/// Test-only `OnUnbalanced` adapter that picks burn-vs-resolve at runtime based on
/// `SlashReceiverAccount`. Lets one test exercise both behaviors.
pub struct DynamicSlash;
impl OnUnbalanced<crate::CreditOf<Test>> for DynamicSlash {
	fn on_nonzero_unbalanced(amount: crate::CreditOf<Test>) {
		match SlashReceiverAccount::get() {
			None => drop(amount),
			Some(receiver) => {
				let _ = Balances::resolve(&receiver, amount);
			},
		}
	}
}

impl pallet_utility::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type PalletsOrigin = OriginCaller;
	type WeightInfo = ();
}

impl Config for Test {
	type RuntimeCall = RuntimeCall;
	type RuntimeHoldReason = RuntimeHoldReason;
	type MaxFriendsPerConfig = MaxFriendsPerConfig;
	type BlockNumberProvider = System;
	type Currency = Balances;
	type FriendGroupsConsideration = HoldConsideration<
		u64,
		Balances,
		FriendGroupsHoldReason,
		LinearStoragePrice<ConstU128<5>, ConstU128<1>, u128>, // 5 + n
	>;
	type AttemptConsideration = HoldConsideration<
		u64,
		Balances,
		AttemptHoldReason,
		LinearStoragePrice<ConstU128<3>, ConstU128<1>, u128>, // 2 + n
	>;
	type InheritorConsideration = HoldConsideration<
		u64,
		Balances,
		InheritorHoldReason,
		LinearStoragePrice<ConstU128<2>, ConstU128<1>, u128>, // 2 + n
	>;
	type SecurityDeposit = ConstU128<SECURITY_DEPOSIT>;
	type Slash = DynamicSlash;
	type WeightInfo = ();
}

pub const ALICE: u64 = 1;
pub const BOB: u64 = 2;
pub const CHARLIE: u64 = 3;
pub const DAVE: u64 = 4;
pub const EVE: u64 = 5;
pub const FERDIE: u64 = 6;

pub const START_BALANCE: u128 = 10_000;
pub const ABORT_DELAY: u64 = 5;

pub fn new_test_ext() -> sp_io::TestExternalities {
	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	pallet_balances::GenesisConfig::<Test> {
		balances: vec![
			(ALICE, START_BALANCE),
			(BOB, START_BALANCE),
			(CHARLIE, START_BALANCE),
			(DAVE, START_BALANCE),
			(EVE, START_BALANCE),
			(FERDIE, START_BALANCE),
		],
		..Default::default()
	}
	.assimilate_storage(&mut t)
	.unwrap();
	let mut ext: sp_io::TestExternalities = t.into();
	ext.execute_with(|| System::set_block_number(1));
	ext
}

// Common test helpers
pub fn assert_last_event<T: Config>(generic_event: crate::Event<T>) {
	frame_system::Pallet::<T>::assert_last_event(generic_event.into());
}

pub fn friends(friends: impl IntoIterator<Item = u64>) -> FriendsOf<Test> {
	friends.into_iter().map(|f| f.into()).collect::<Vec<_>>().try_into().unwrap()
}

pub fn fg(fs: impl IntoIterator<Item = u64>) -> FriendGroupOf<Test> {
	FriendGroupOf::<Test> {
		friends: friends(fs),
		friends_needed: 2,
		inheritor: FERDIE,
		inheritance_delay: 10,
		inheritance_priority: 0,
		cancel_delay: ABORT_DELAY,
	}
}

pub fn signed(account: u64) -> RuntimeOrigin {
	RuntimeOrigin::signed(account)
}

pub fn assert_fg_deposit(who: u64, deposit: u128) {
	use frame::traits::fungible::InspectHold;
	assert_eq!(
		<Test as crate::Config>::Currency::balance_on_hold(
			&crate::HoldReason::FriendGroupsStorage.into(),
			&who
		),
		deposit
	);
}

pub fn assert_attempt_deposit(who: u64, deposit: u128) {
	use frame::traits::fungible::InspectHold;
	assert_eq!(
		<Test as crate::Config>::Currency::balance_on_hold(
			&crate::HoldReason::AttemptStorage.into(),
			&who
		),
		deposit
	);
}

pub fn assert_security_deposit(who: u64, deposit: u128) {
	use frame::traits::fungible::InspectHold;
	assert_eq!(
		<Test as crate::Config>::Currency::balance_on_hold(
			&crate::HoldReason::SecurityDeposit.into(),
			&who
		),
		deposit
	);
}

pub fn assert_inheritor_deposit(who: u64, deposit: u128) {
	use frame::traits::fungible::InspectHold;
	assert_eq!(
		<Test as crate::Config>::Currency::balance_on_hold(
			&crate::HoldReason::InheritorStorage.into(),
			&who
		),
		deposit
	);
}

pub fn clear_events() {
	frame_system::Pallet::<Test>::reset_events();
}

pub fn inc_block_number(by: u64) {
	frame_system::Pallet::<Test>::set_block_number(
		frame_system::Pallet::<Test>::current_block_number() + by,
	);
}

pub fn can_control_account(
	inheritor: AccountIdLookupOf<Test>,
	recovered: AccountIdLookupOf<Test>,
) -> bool {
	let call: RuntimeCall = frame_system::Call::remark { remark: vec![] }.into();
	Recovery::control_inherited_account(signed(inheritor), recovered, Box::new(call)).is_ok()
}

pub fn root_without_events() -> Vec<u8> {
	hypothetically!({
		clear_events();
		sp_io::storage::root(sp_runtime::StateVersion::V1)
	})
}

pub fn setup_alice_fgs(fs: impl IntoIterator<Item = impl IntoIterator<Item = u64>>) {
	let fgs = fs.into_iter().map(fg).collect::<Vec<_>>();
	assert_ok!(Recovery::set_friend_groups(signed(ALICE), fgs));
}

// Migration config for v0 -> v1 migration tests
impl crate::migrations::v0::MigrationConfig for Test {
	type Currency = Balances;
}
