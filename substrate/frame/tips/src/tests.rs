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

//! Treasury pallet tests.

#![cfg(test)]

use sp_core::H256;
use sp_runtime::{
	traits::{BadOrigin, BlakeTwo256, IdentityLookup},
	BuildStorage, Perbill, Permill,
};
use sp_storage::Storage;

use frame_support::{
	assert_noop, assert_ok, derive_impl, parameter_types,
	storage::StoragePrefixedMap,
	traits::{
		tokens::{PayFromAccount, UnityAssetBalanceConversion},
		ConstU32, ConstU64, IntegrityTest, SortedMembers, StorageVersion,
	},
	PalletId,
};

use super::*;
use crate::{self as pallet_tips, Event as TipEvent};

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		Balances: pallet_balances,
		Treasury: pallet_treasury,
		Treasury1: pallet_treasury::<Instance1>,
		Tips: pallet_tips,
		Tips1: pallet_tips::<Instance1>,
	}
);

parameter_types! {
	pub const AvailableBlockRatio: Perbill = Perbill::one();
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type AccountId = u128; // u64 is not enough to hold bytes used to generate bounty account
	type Lookup = IdentityLookup<Self::AccountId>;
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u64>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type AccountStore = System;
}
parameter_types! {
	static TenToFourteenTestValue: Vec<u128> = vec![10,11,12,13,14];
}
pub struct TenToFourteen;
impl SortedMembers<u128> for TenToFourteen {
	fn sorted_members() -> Vec<u128> {
		TenToFourteenTestValue::get().clone()
	}
	#[cfg(feature = "runtime-benchmarks")]
	fn add(new: &u128) {
		TenToFourteenTestValue::mutate(|members| {
			members.push(*new);
			members.sort();
		})
	}
}
impl ContainsLengthBound for TenToFourteen {
	fn max_len() -> usize {
		TenToFourteenTestValue::get().len()
	}
	fn min_len() -> usize {
		0
	}
}
parameter_types! {
	pub const Burn: Permill = Permill::from_percent(50);
	pub const TreasuryPalletId: PalletId = PalletId(*b"py/trsry");
	pub const TreasuryPalletId2: PalletId = PalletId(*b"py/trsr2");
	pub TreasuryAccount: u128 = Treasury::account_id();
	pub TreasuryInstance1Account: u128 = Treasury1::account_id();
}

impl pallet_treasury::Config for Test {
	type PalletId = TreasuryPalletId;
	type Currency = pallet_balances::Pallet<Test>;
	type RejectOrigin = frame_system::EnsureRoot<u128>;
	type RuntimeEvent = RuntimeEvent;
	type SpendPeriod = ConstU64<2>;
	type Burn = Burn;
	type BurnDestination = (); // Just gets burned.
	type WeightInfo = ();
	type SpendFunds = ();
	type MaxApprovals = ConstU32<100>;
	type SpendOrigin = frame_support::traits::NeverEnsureOrigin<u64>;
	type AssetKind = ();
	type Beneficiary = Self::AccountId;
	type BeneficiaryLookup = IdentityLookup<Self::Beneficiary>;
	type Paymaster = PayFromAccount<Balances, TreasuryAccount>;
	type BalanceConverter = UnityAssetBalanceConversion;
	type PayoutPeriod = ConstU64<10>;
	type BlockNumberProvider = System;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = ();
}

impl pallet_treasury::Config<Instance1> for Test {
	type PalletId = TreasuryPalletId2;
	type Currency = pallet_balances::Pallet<Test>;
	type RejectOrigin = frame_system::EnsureRoot<u128>;
	type RuntimeEvent = RuntimeEvent;
	type SpendPeriod = ConstU64<2>;
	type Burn = Burn;
	type BurnDestination = (); // Just gets burned.
	type WeightInfo = ();
	type SpendFunds = ();
	type MaxApprovals = ConstU32<100>;
	type SpendOrigin = frame_support::traits::NeverEnsureOrigin<u64>;
	type AssetKind = ();
	type Beneficiary = Self::AccountId;
	type BeneficiaryLookup = IdentityLookup<Self::Beneficiary>;
	type Paymaster = PayFromAccount<Balances, TreasuryInstance1Account>;
	type BalanceConverter = UnityAssetBalanceConversion;
	type PayoutPeriod = ConstU64<10>;
	type BlockNumberProvider = System;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = ();
}

parameter_types! {
	pub const TipFindersFee: Percent = Percent::from_percent(20);
	pub static TipReportDepositBase: u64 = 1;
}
impl Config for Test {
	type MaximumReasonLength = ConstU32<16384>;
	type Tippers = TenToFourteen;
	type TipCountdown = ConstU64<1>;
	type TipFindersFee = TipFindersFee;
	type TipReportDepositBase = TipReportDepositBase;
	type DataDepositPerByte = ConstU64<1>;
	type MaxTipAmount = ConstU64<10_000_000>;
	type RuntimeEvent = RuntimeEvent;
	type OnSlash = ();
	type WeightInfo = ();
}

impl Config<Instance1> for Test {
	type MaximumReasonLength = ConstU32<16384>;
	type Tippers = TenToFourteen;
	type TipCountdown = ConstU64<1>;
	type TipFindersFee = TipFindersFee;
	type TipReportDepositBase = TipReportDepositBase;
	type DataDepositPerByte = ConstU64<1>;
	type MaxTipAmount = ConstU64<10_000_000>;
	type RuntimeEvent = RuntimeEvent;
	type OnSlash = ();
	type WeightInfo = ();
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let mut ext: sp_io::TestExternalities = RuntimeGenesisConfig {
		system: frame_system::GenesisConfig::default(),
		balances: pallet_balances::GenesisConfig {
			balances: vec![(0, 100), (1, 98), (2, 1)],
			..Default::default()
		},
		treasury: Default::default(),
		treasury_1: Default::default(),
	}
	.build_storage()
	.unwrap()
	.into();
	ext.execute_with(|| System::set_block_number(1));
	ext
}

/// Run the function pointer inside externalities and asserts the try_state hook at the end.
pub fn build_and_execute(test: impl FnOnce() -> ()) {
	new_test_ext().execute_with(|| {
		test();
		Tips::do_try_state().expect("All invariants must hold after a test");
	});
}

fn last_event() -> TipEvent<Test> {
	System::events()
		.into_iter()
		.map(|r| r.event)
		.filter_map(|e| if let RuntimeEvent::Tips(inner) = e { Some(inner) } else { None })
		.last()
		.unwrap()
}

#[test]
#[allow(deprecated)]
fn genesis_config_works() {
	build_and_execute(|| {
		assert_eq!(Treasury::pot(), 0);
		assert_eq!(Treasury::proposal_count(), 0);
	});
}

fn tip_hash() -> H256 {
	BlakeTwo256::hash_of(&(BlakeTwo256::hash(b"awesome.dot"), 3u128))
}

#[test]
fn tip_new_cannot_be_used_twice() {
	build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Tips::tip_new(RuntimeOrigin::signed(10), b"awesome.dot".to_vec(), 3, 10));
		assert_noop!(
			Tips::tip_new(RuntimeOrigin::signed(11), b"awesome.dot".to_vec(), 3, 10),
			Error::<Test>::AlreadyKnown
		);
	});
}

#[test]
fn report_awesome_and_tip_works() {
	build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Tips::report_awesome(RuntimeOrigin::signed(0), b"awesome.dot".to_vec(), 3));
		assert_eq!(Balances::reserved_balance(0), 12);
		assert_eq!(Balances::free_balance(0), 88);

		// other reports don't count.
		assert_noop!(
			Tips::report_awesome(RuntimeOrigin::signed(1), b"awesome.dot".to_vec(), 3),
			Error::<Test>::AlreadyKnown
		);

		let h = tip_hash();
		assert_ok!(Tips::tip(RuntimeOrigin::signed(10), h, 10));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(11), h, 10));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(12), h, 10));
		assert_noop!(Tips::tip(RuntimeOrigin::signed(9), h, 10), BadOrigin);
		System::set_block_number(2);
		assert_ok!(Tips::close_tip(RuntimeOrigin::signed(100), h.into()));
		assert_eq!(Balances::reserved_balance(0), 0);
		assert_eq!(Balances::free_balance(0), 102);
		assert_eq!(Balances::free_balance(3), 8);
	});
}

#[test]
fn report_awesome_from_beneficiary_and_tip_works() {
	build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Tips::report_awesome(RuntimeOrigin::signed(0), b"awesome.dot".to_vec(), 0));
		assert_eq!(Balances::reserved_balance(0), 12);
		assert_eq!(Balances::free_balance(0), 88);
		let h = BlakeTwo256::hash_of(&(BlakeTwo256::hash(b"awesome.dot"), 0u128));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(10), h, 10));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(11), h, 10));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(12), h, 10));
		System::set_block_number(2);
		assert_ok!(Tips::close_tip(RuntimeOrigin::signed(100), h.into()));
		assert_eq!(Balances::reserved_balance(0), 0);
		assert_eq!(Balances::free_balance(0), 110);
	});
}

#[test]
fn close_tip_works() {
	build_and_execute(|| {
		System::set_block_number(1);

		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_eq!(Treasury::pot(), 100);

		assert_ok!(Tips::tip_new(RuntimeOrigin::signed(10), b"awesome.dot".to_vec(), 3, 10));

		let h = tip_hash();

		assert_eq!(last_event(), TipEvent::NewTip { tip_hash: h });

		assert_ok!(Tips::tip(RuntimeOrigin::signed(11), h, 10));

		assert_noop!(Tips::close_tip(RuntimeOrigin::signed(0), h.into()), Error::<Test>::StillOpen);

		assert_ok!(Tips::tip(RuntimeOrigin::signed(12), h, 10));

		assert_eq!(last_event(), TipEvent::TipClosing { tip_hash: h });

		assert_noop!(Tips::close_tip(RuntimeOrigin::signed(0), h.into()), Error::<Test>::Premature);

		System::set_block_number(2);
		assert_noop!(Tips::close_tip(RuntimeOrigin::none(), h.into()), BadOrigin);
		assert_ok!(Tips::close_tip(RuntimeOrigin::signed(0), h.into()));
		assert_eq!(Balances::free_balance(3), 10);

		assert_eq!(last_event(), TipEvent::TipClosed { tip_hash: h, who: 3, payout: 10 });

		assert_noop!(
			Tips::close_tip(RuntimeOrigin::signed(100), h.into()),
			Error::<Test>::UnknownTip
		);
	});
}

#[test]
fn close_tip_fails_when_all_tippers_are_inactive() {
	build_and_execute(|| {
		TenToFourteenTestValue::reset();
		System::set_block_number(1);

		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Tips::tip_new(RuntimeOrigin::signed(10), b"awesome.dot".to_vec(), 3, 10));

		let h = tip_hash();
		let reason_hash = BlakeTwo256::hash(b"awesome.dot");

		assert_ok!(Tips::tip(RuntimeOrigin::signed(11), h, 10));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(12), h, 10));
		System::set_block_number(2);

		// All original tippers are now inactive.
		TenToFourteenTestValue::set(Vec::new());

		assert_noop!(
			Tips::close_tip(RuntimeOrigin::signed(0), h.into()),
			Error::<Test>::NoActiveTippers
		);

		// Calls are transactional: failed close must not remove tip state.
		assert!(Tips::tips(h).is_some());
		assert!(Tips::reasons(reason_hash).is_some());

		TenToFourteenTestValue::reset();
	});
}

#[test]
fn slash_tip_works() {
	build_and_execute(|| {
		System::set_block_number(1);
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_eq!(Treasury::pot(), 100);

		assert_eq!(Balances::reserved_balance(0), 0);
		assert_eq!(Balances::free_balance(0), 100);

		assert_ok!(Tips::report_awesome(RuntimeOrigin::signed(0), b"awesome.dot".to_vec(), 3));

		assert_eq!(Balances::reserved_balance(0), 12);
		assert_eq!(Balances::free_balance(0), 88);

		let h = tip_hash();
		assert_eq!(last_event(), TipEvent::NewTip { tip_hash: h });

		// can't remove from any origin
		assert_noop!(Tips::slash_tip(RuntimeOrigin::signed(0), h), BadOrigin);

		// can remove from root.
		assert_ok!(Tips::slash_tip(RuntimeOrigin::root(), h));
		assert_eq!(last_event(), TipEvent::TipSlashed { tip_hash: h, finder: 0, deposit: 12 });

		// tipper slashed
		assert_eq!(Balances::reserved_balance(0), 0);
		assert_eq!(Balances::free_balance(0), 88);
	});
}

#[test]
fn retract_tip_works() {
	build_and_execute(|| {
		// with report awesome
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Tips::report_awesome(RuntimeOrigin::signed(0), b"awesome.dot".to_vec(), 3));
		let h = tip_hash();
		assert_ok!(Tips::tip(RuntimeOrigin::signed(10), h, 10));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(11), h, 10));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(12), h, 10));
		assert_noop!(Tips::retract_tip(RuntimeOrigin::signed(10), h), Error::<Test>::NotFinder);
		assert_ok!(Tips::retract_tip(RuntimeOrigin::signed(0), h));
		System::set_block_number(2);
		assert_noop!(
			Tips::close_tip(RuntimeOrigin::signed(0), h.into()),
			Error::<Test>::UnknownTip
		);

		// with tip new
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Tips::tip_new(RuntimeOrigin::signed(10), b"awesome.dot".to_vec(), 3, 10));
		let h = tip_hash();
		assert_ok!(Tips::tip(RuntimeOrigin::signed(11), h, 10));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(12), h, 10));
		assert_noop!(Tips::retract_tip(RuntimeOrigin::signed(0), h), Error::<Test>::NotFinder);
		assert_ok!(Tips::retract_tip(RuntimeOrigin::signed(10), h));
		System::set_block_number(2);
		assert_noop!(
			Tips::close_tip(RuntimeOrigin::signed(10), h.into()),
			Error::<Test>::UnknownTip
		);
	});
}

#[test]
fn tip_median_calculation_works() {
	build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Tips::tip_new(RuntimeOrigin::signed(10), b"awesome.dot".to_vec(), 3, 0));
		let h = tip_hash();
		assert_ok!(Tips::tip(RuntimeOrigin::signed(11), h, 10));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(12), h, 1000000));
		System::set_block_number(2);
		assert_ok!(Tips::close_tip(RuntimeOrigin::signed(0), h.into()));
		assert_eq!(Balances::free_balance(3), 10);
	});
}

#[test]
fn tip_large_should_fail() {
	build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Tips::tip_new(RuntimeOrigin::signed(10), b"awesome.dot".to_vec(), 3, 0));
		let h = tip_hash();
		assert_noop!(
			Tips::tip(
				RuntimeOrigin::signed(12),
				h,
				<<Test as Config>::MaxTipAmount as Get<u64>>::get() + 1
			),
			Error::<Test>::MaxTipAmountExceeded
		);
	});
}

#[test]
fn tip_changing_works() {
	build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Tips::tip_new(RuntimeOrigin::signed(10), b"awesome.dot".to_vec(), 3, 10000));
		let h = tip_hash();
		assert_ok!(Tips::tip(RuntimeOrigin::signed(11), h, 10000));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(12), h, 10000));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(13), h, 0));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(14), h, 0));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(12), h, 1000));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(11), h, 100));
		assert_ok!(Tips::tip(RuntimeOrigin::signed(10), h, 10));
		System::set_block_number(2);
		assert_ok!(Tips::close_tip(RuntimeOrigin::signed(0), h.into()));
		assert_eq!(Balances::free_balance(3), 10);
	});
}

#[test]
fn test_last_reward_migration() {
	let mut s = Storage::default();

	#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug)]
	pub struct OldOpenTip<
		AccountId: Parameter,
		Balance: Parameter,
		BlockNumber: Parameter,
		Hash: Parameter,
	> {
		/// The hash of the reason for the tip. The reason should be a human-readable UTF-8 encoded
		/// string. A URL would be sensible.
		reason: Hash,
		/// The account to be tipped.
		who: AccountId,
		/// The account who began this tip and the amount held on deposit.
		finder: Option<(AccountId, Balance)>,
		/// The block number at which this tip will close if `Some`. If `None`, then no closing is
		/// scheduled.
		closes: Option<BlockNumber>,
		/// The members who have voted for this tip. Sorted by AccountId.
		tips: Vec<(AccountId, Balance)>,
	}

	let reason1 = BlakeTwo256::hash(b"reason1");
	let hash1 = BlakeTwo256::hash_of(&(reason1, 10u64));

	let old_tip_finder = OldOpenTip::<u128, u64, u64, H256> {
		reason: reason1,
		who: 10,
		finder: Some((20, 30)),
		closes: Some(13),
		tips: vec![(40, 50), (60, 70)],
	};

	let reason2 = BlakeTwo256::hash(b"reason2");
	let hash2 = BlakeTwo256::hash_of(&(reason2, 20u64));

	let old_tip_no_finder = OldOpenTip::<u128, u64, u64, H256> {
		reason: reason2,
		who: 20,
		finder: None,
		closes: Some(13),
		tips: vec![(40, 50), (60, 70)],
	};

	let data = vec![
		(pallet_tips::Tips::<Test>::hashed_key_for(hash1), old_tip_finder.encode().to_vec()),
		(pallet_tips::Tips::<Test>::hashed_key_for(hash2), old_tip_no_finder.encode().to_vec()),
	];

	s.top = data.into_iter().collect();

	sp_io::TestExternalities::new(s).execute_with(|| {
		let module = pallet_tips::Tips::<Test>::pallet_prefix();
		let item = pallet_tips::Tips::<Test>::storage_prefix();
		Tips::migrate_retract_tip_for_tip_new(module, item);

		// Test w/ finder
		assert_eq!(
			pallet_tips::Tips::<Test>::get(hash1),
			Some(OpenTip {
				reason: reason1,
				who: 10,
				finder: 20,
				deposit: 30,
				closes: Some(13),
				tips: vec![(40, 50), (60, 70)],
				finders_fee: true,
			})
		);

		// Test w/o finder
		assert_eq!(
			pallet_tips::Tips::<Test>::get(hash2),
			Some(OpenTip {
				reason: reason2,
				who: 20,
				finder: Default::default(),
				deposit: 0,
				closes: Some(13),
				tips: vec![(40, 50), (60, 70)],
				finders_fee: false,
			})
		);
	});
}

#[test]
fn test_migration_v4() {
	let reason1 = BlakeTwo256::hash(b"reason1");
	let hash1 = BlakeTwo256::hash_of(&(reason1, 10u64));

	let tip = OpenTip::<u128, u64, u64, H256> {
		reason: reason1,
		who: 10,
		finder: 20,
		deposit: 30,
		closes: Some(13),
		tips: vec![(40, 50), (60, 70)],
		finders_fee: true,
	};

	let data = vec![
		(pallet_tips::Reasons::<Test>::hashed_key_for(hash1), reason1.encode().to_vec()),
		(pallet_tips::Tips::<Test>::hashed_key_for(hash1), tip.encode().to_vec()),
	];

	let mut s = Storage::default();
	s.top = data.into_iter().collect();

	sp_io::TestExternalities::new(s).execute_with(|| {
		use frame_support::traits::PalletInfoAccess;

		let old_pallet = "Treasury";
		let new_pallet = <Tips as PalletInfoAccess>::name();
		frame_support::storage::migration::move_pallet(
			new_pallet.as_bytes(),
			old_pallet.as_bytes(),
		);
		StorageVersion::new(0).put::<Tips>();

		crate::migrations::v4::pre_migrate::<Test, Tips, _>(old_pallet);
		crate::migrations::v4::migrate::<Test, Tips, _>(old_pallet);
		crate::migrations::v4::post_migrate::<Test, Tips, _>(old_pallet);
	});

	sp_io::TestExternalities::new(Storage::default()).execute_with(|| {
		use frame_support::traits::PalletInfoAccess;

		let old_pallet = "Treasury";
		let new_pallet = <Tips as PalletInfoAccess>::name();
		frame_support::storage::migration::move_pallet(
			new_pallet.as_bytes(),
			old_pallet.as_bytes(),
		);
		StorageVersion::new(0).put::<Tips>();

		crate::migrations::v4::pre_migrate::<Test, Tips, _>(old_pallet);
		crate::migrations::v4::migrate::<Test, Tips, _>(old_pallet);
		crate::migrations::v4::post_migrate::<Test, Tips, _>(old_pallet);
	});
}

#[test]
fn genesis_funding_works() {
	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	let initial_funding = 100;
	pallet_balances::GenesisConfig::<Test> {
		// Total issuance will be 200 with treasury account initialized with 100.
		balances: vec![(0, 100), (Treasury::account_id(), initial_funding)],
		..Default::default()
	}
	.assimilate_storage(&mut t)
	.unwrap();
	pallet_treasury::GenesisConfig::<Test>::default()
		.assimilate_storage(&mut t)
		.unwrap();
	let mut t: sp_io::TestExternalities = t.into();

	t.execute_with(|| {
		assert_eq!(Balances::free_balance(Treasury::account_id()), initial_funding);
		assert_eq!(Treasury::pot(), initial_funding - Balances::minimum_balance());
	});
}

#[test]
fn report_awesome_and_tip_works_second_instance() {
	build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		Balances::make_free_balance_be(&Treasury1::account_id(), 201);
		assert_eq!(Balances::free_balance(&Treasury::account_id()), 101);
		assert_eq!(Balances::free_balance(&Treasury1::account_id()), 201);

		assert_ok!(Tips1::report_awesome(RuntimeOrigin::signed(0), b"awesome.dot".to_vec(), 3));
		// duplicate report in tips1 reports don't count.
		assert_noop!(
			Tips1::report_awesome(RuntimeOrigin::signed(1), b"awesome.dot".to_vec(), 3),
			Error::<Test, Instance1>::AlreadyKnown
		);
		// but tips is separate
		assert_ok!(Tips::report_awesome(RuntimeOrigin::signed(0), b"awesome.dot".to_vec(), 3));

		let h = tip_hash();
		assert_ok!(Tips1::tip(RuntimeOrigin::signed(10), h, 10));
		assert_ok!(Tips1::tip(RuntimeOrigin::signed(11), h, 10));
		assert_ok!(Tips1::tip(RuntimeOrigin::signed(12), h, 10));
		assert_noop!(Tips1::tip(RuntimeOrigin::signed(9), h, 10), BadOrigin);

		System::set_block_number(2);

		assert_ok!(Tips1::close_tip(RuntimeOrigin::signed(100), h.into()));
		// Treasury 1 unchanged
		assert_eq!(Balances::free_balance(&Treasury::account_id()), 101);
		// Treasury 2 gave the funds
		assert_eq!(Balances::free_balance(&Treasury1::account_id()), 191);
	});
}

#[test]
fn equal_entries_invariant() {
	new_test_ext().execute_with(|| {
		use frame_support::pallet_prelude::DispatchError::Other;

		Balances::make_free_balance_be(&Treasury::account_id(), 101);

		assert_ok!(Tips::report_awesome(RuntimeOrigin::signed(0), b"awesome.dot".to_vec(), 3));

		let reason1 = BlakeTwo256::hash(b"reason1");
		let hash1 = BlakeTwo256::hash_of(&(reason1, 10u64));

		let tip = OpenTip::<u128, u64, u64, H256> {
			reason: reason1,
			who: 10,
			finder: 20,
			deposit: 30,
			closes: Some(13),
			tips: vec![(40, 50), (60, 70)],
			finders_fee: true,
		};

		// Breaks invariant by adding an entry to only `Tips` Storage.
		pallet_tips::Tips::<Test>::insert(hash1, tip);

		// Invariant violated
		assert_eq!(
			Tips::do_try_state(),
			Err(Other("Equal length of entries in `Tips` and `Reasons` Storage"))
		);
	})
}

#[test]
fn finders_fee_invariant() {
	new_test_ext().execute_with(|| {
		use frame_support::pallet_prelude::DispatchError::Other;

		// Breaks invariant by having a zero deposit.
		TipReportDepositBase::set(0);

		Balances::make_free_balance_be(&Treasury::account_id(), 101);

		assert_ok!(Tips::report_awesome(RuntimeOrigin::signed(0), b"".to_vec(), 3));

		// Invariant violated
		assert_eq!(
			Tips::do_try_state(),
			Err(Other("Tips with `finders_fee` should have non-zero `deposit`."))
		);
	})
}

#[test]
fn reasons_invariant() {
	new_test_ext().execute_with(|| {
		use frame_support::pallet_prelude::DispatchError::Other;

		Balances::make_free_balance_be(&Treasury::account_id(), 101);

		assert_ok!(Tips::report_awesome(RuntimeOrigin::signed(0), b"awesome.dot".to_vec(), 0));

		let hash: Vec<_> = pallet_tips::Tips::<Test>::iter_keys().collect();

		let mut open_tip = pallet_tips::Tips::<Test>::take(hash[0]).unwrap();

		// Breaks invariant by changing value `open_tip.reason` in `Tips` Storage.
		open_tip.reason = <Test as frame_system::Config>::Hashing::hash(&b"".to_vec());

		pallet_tips::Tips::<Test>::insert(hash[0], open_tip);

		// Invariant violated
		assert_eq!(Tips::do_try_state(), Err(Other("no reason for this tip")));
	})
}

#[test]
#[should_panic = "`TipReportDepositBase` should not be zero"]
fn zero_base_deposit_prohibited() {
	new_test_ext().execute_with(|| {
		TipReportDepositBase::set(0);
		Tips::integrity_test();
	});
}
