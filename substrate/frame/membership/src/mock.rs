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
use crate as pallet_membership;

use sp_runtime::{bounded_vec, BuildStorage};

use frame_support::{derive_impl, ord_parameter_types, parameter_types, traits::ConstU32};
use frame_system::EnsureSignedBy;

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		Membership: pallet_membership,
	}
);

parameter_types! {
	pub static Members: Vec<u64> = vec![];
	pub static Prime: Option<u64> = None;
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
}
ord_parameter_types! {
	pub const One: u64 = 1;
	pub const Two: u64 = 2;
	pub const Three: u64 = 3;
	pub const Four: u64 = 4;
	pub const Five: u64 = 5;
}

pub struct TestChangeMembers;
impl ChangeMembers<u64> for TestChangeMembers {
	fn change_members_sorted(incoming: &[u64], outgoing: &[u64], new: &[u64]) {
		let mut old_plus_incoming = Members::get();
		old_plus_incoming.extend_from_slice(incoming);
		old_plus_incoming.sort();
		let mut new_plus_outgoing = new.to_vec();
		new_plus_outgoing.extend_from_slice(outgoing);
		new_plus_outgoing.sort();
		assert_eq!(old_plus_incoming, new_plus_outgoing);

		Members::set(new.to_vec());
		Prime::set(None);
	}
	fn set_prime(who: Option<u64>) {
		Prime::set(who);
	}
	fn get_prime() -> Option<u64> {
		Prime::get()
	}
}

impl InitializeMembers<u64> for TestChangeMembers {
	fn initialize_members(members: &[u64]) {
		MEMBERS.with(|m| *m.borrow_mut() = members.to_vec());
	}
}

impl Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type AddOrigin = EnsureSignedBy<One, u64>;
	type RemoveOrigin = EnsureSignedBy<Two, u64>;
	type SwapOrigin = EnsureSignedBy<Three, u64>;
	type ResetOrigin = EnsureSignedBy<Four, u64>;
	type PrimeOrigin = EnsureSignedBy<Five, u64>;
	type MembershipInitialized = TestChangeMembers;
	type MembershipChanged = TestChangeMembers;
	type MaxMembers = ConstU32<10>;
	type WeightInfo = ();
}

#[derive(Default)]
pub struct ExtBuilder;

impl ExtBuilder {
	pub fn build(self) -> sp_io::TestExternalities {
		let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
		// We use default for brevity, but you can configure as desired if needed.
		pallet_membership::GenesisConfig::<Test> {
			members: bounded_vec![10, 20, 30],
			..Default::default()
		}
		.assimilate_storage(&mut t)
		.unwrap();
		t.into()
	}

	pub fn build_and_execute(self, test: impl FnOnce() -> ()) {
		self.build().execute_with(|| {
			test();
			Membership::do_try_state().expect("All invariants must hold after a test");
		})
	}
}

#[cfg(feature = "runtime-benchmarks")]
pub(crate) fn new_bench_ext() -> sp_io::TestExternalities {
	frame_system::GenesisConfig::<Test>::default().build_storage().unwrap().into()
}

#[cfg(feature = "runtime-benchmarks")]
pub(crate) fn clean() {
	Members::set(vec![]);
	Prime::set(None);
}
