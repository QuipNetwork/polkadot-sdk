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

//! Bounties pallet tests.

#![cfg(test)]

use crate as pallet_bounties;
use crate::{Event as BountiesEvent, *};

use alloc::collections::btree_map::BTreeMap;
use core::cell::RefCell;
use frame_support::{
	assert_ok, derive_impl, parameter_types,
	traits::{
		fungible::{HoldConsideration, Mutate},
		tokens::UnityAssetBalanceConversion,
		ConstU64, Currency,
	},
	weights::constants::ParityDbWeight,
	PalletId,
};
use sp_runtime::{
	traits::{BlakeTwo256, Convert, Hash, Identity, IdentityLookup},
	BuildStorage, Perbill,
};

type Block = frame_system::mocking::MockBlock<Test>;

thread_local! {
	pub static PAID: RefCell<BTreeMap<(u128, u32), u64>> = RefCell::new(BTreeMap::new());
	pub static STATUS: RefCell<BTreeMap<u64, PaymentStatus>> = RefCell::new(BTreeMap::new());
	pub static LAST_ID: RefCell<u64> = RefCell::new(0u64);
}

pub struct TestBountiesPay;
impl PayWithSource for TestBountiesPay {
	type Source = u128;
	type Beneficiary = u128;
	type Balance = u64;
	type Id = u64;
	type AssetKind = u32;
	type Error = ();

	fn pay(
		_: &Self::Source,
		to: &Self::Beneficiary,
		asset_kind: Self::AssetKind,
		amount: Self::Balance,
	) -> Result<Self::Id, Self::Error> {
		PAID.with(|paid| *paid.borrow_mut().entry((*to, asset_kind)).or_default() += amount);
		Ok(LAST_ID.with(|lid| {
			let x = *lid.borrow();
			lid.replace(x + 1);
			x
		}))
	}
	fn check_payment(id: Self::Id) -> PaymentStatus {
		STATUS.with(|s| s.borrow().get(&id).cloned().unwrap_or(PaymentStatus::InProgress))
	}
	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful(
		_: &Self::Source,
		_: &Self::Beneficiary,
		_: Self::AssetKind,
		_: Self::Balance,
	) {
	}
	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_concluded(id: Self::Id) {
		set_status(id, PaymentStatus::Success);
	}
}

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		Balances: pallet_balances,
		Preimage: pallet_preimage,
		Utility: pallet_utility,
		Bounties: pallet_bounties,
		Bounties1: pallet_bounties::<Instance1>,
	}
);

parameter_types! {
	pub const AvailableBlockRatio: Perbill = Perbill::one();
}

type Balance = u64;

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type AccountId = u128; // u64 is not enough to hold bytes used to generate bounty account
	type Lookup = IdentityLookup<Self::AccountId>;
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u64>;
	type DbWeight = ParityDbWeight;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type AccountStore = System;
	type RuntimeHoldReason = RuntimeHoldReason;
}

impl pallet_preimage::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = ();
	type Currency = Balances;
	type ManagerOrigin = frame_system::EnsureRoot<u64>;
	type Consideration = ();
}

impl pallet_utility::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type PalletsOrigin = OriginCaller;
	type WeightInfo = ();
}

parameter_types! {
	pub static Burn: Permill = Permill::from_percent(50);
	pub const BountyPalletId: PalletId = PalletId(*b"py/mbnty");
	pub const BountyPalletId2: PalletId = PalletId(*b"py/mbnt2");
	pub static SpendLimit: Balance = u64::MAX;
	pub static SpendLimit1: Balance = u64::MAX;
}

pub struct TestSpendOrigin;
impl frame_support::traits::EnsureOrigin<RuntimeOrigin> for TestSpendOrigin {
	type Success = u64;
	fn try_origin(o: RuntimeOrigin) -> Result<Self::Success, RuntimeOrigin> {
		Result::<frame_system::RawOrigin<_>, RuntimeOrigin>::from(o).and_then(|o| match o {
			frame_system::RawOrigin::Root => Ok(SpendLimit::get()),
			frame_system::RawOrigin::Signed(10) => Ok(5),
			frame_system::RawOrigin::Signed(11) => Ok(10),
			frame_system::RawOrigin::Signed(12) => Ok(20),
			frame_system::RawOrigin::Signed(13) => Ok(50),
			frame_system::RawOrigin::Signed(14) => Ok(500),
			r => Err(RuntimeOrigin::from(r)),
		})
	}
	#[cfg(feature = "runtime-benchmarks")]
	fn try_successful_origin() -> Result<RuntimeOrigin, ()> {
		Ok(frame_system::RawOrigin::Root.into())
	}
}

parameter_types! {
	// This will be 50% of the bounty value.
	pub const CuratorDepositMultiplier: Permill = Permill::from_percent(50);
	pub const CuratorDepositMin: Balance = 3;
	pub const CuratorDepositMax: Balance = 1_000;
	pub const CuratorDepositHoldReason: RuntimeHoldReason = RuntimeHoldReason::Bounties(pallet_bounties::HoldReason::CuratorDeposit);
	pub static MaxActiveChildBountyCount: u32 = 3;
}

impl Config for Test {
	type Balance = <Self as pallet_balances::Config>::Balance;
	type RejectOrigin = frame_system::EnsureRoot<u128>;
	type SpendOrigin = TestSpendOrigin;
	type AssetKind = u32;
	type Beneficiary = u128;
	type BeneficiaryLookup = IdentityLookup<Self::Beneficiary>;
	type BountyValueMinimum = ConstU64<2>;
	type ChildBountyValueMinimum = ConstU64<1>;
	type MaxActiveChildBountyCount = MaxActiveChildBountyCount;
	type WeightInfo = ();
	type FundingSource = PalletIdAsFundingSource<BountyPalletId, Test, Identity, ()>;
	type BountySource =
		BountySourceFromPalletId<BountyPalletId, BountyAccountPrefix, Test, Identity, ()>;
	type ChildBountySource =
		ChildBountySourceFromPalletId<BountyPalletId, ChildBountyAccountPrefix, Test, Identity, ()>;
	type Paymaster = TestBountiesPay;
	type BalanceConverter = UnityAssetBalanceConversion;
	type Preimages = Preimage;
	type Consideration = HoldConsideration<
		Self::AccountId,
		Balances,
		CuratorDepositHoldReason,
		CuratorDepositAmount<
			CuratorDepositMultiplier,
			CuratorDepositMin,
			CuratorDepositMax,
			Balance,
		>,
		Balance,
	>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = ();
}
type CuratorDeposit =
	CuratorDepositAmount<CuratorDepositMultiplier, CuratorDepositMin, CuratorDepositMax, Balance>;
impl Config<Instance1> for Test {
	type Balance = <Self as pallet_balances::Config>::Balance;
	type RejectOrigin = frame_system::EnsureRoot<u128>;
	type SpendOrigin = TestSpendOrigin;
	type AssetKind = u32;
	type Beneficiary = u128;
	type BeneficiaryLookup = IdentityLookup<Self::Beneficiary>;
	type BountyValueMinimum = ConstU64<2>;
	type ChildBountyValueMinimum = ConstU64<1>;
	type MaxActiveChildBountyCount = MaxActiveChildBountyCount;
	type WeightInfo = ();
	type FundingSource = PalletIdAsFundingSource<BountyPalletId2, Test, Identity, Instance1>;
	type BountySource =
		BountySourceFromPalletId<BountyPalletId2, BountyAccountPrefix, Test, Identity, Instance1>;
	type ChildBountySource = ChildBountySourceFromPalletId<
		BountyPalletId2,
		ChildBountyAccountPrefix,
		Test,
		Identity,
		Instance1,
	>;
	type Paymaster = TestBountiesPay;
	type BalanceConverter = UnityAssetBalanceConversion;
	type Preimages = Preimage;
	type Consideration = HoldConsideration<
		Self::AccountId,
		Balances,
		CuratorDepositHoldReason,
		CuratorDeposit,
		Balance,
	>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = ();
}

pub struct ExtBuilder {}

impl Default for ExtBuilder {
	fn default() -> Self {
		Self {}
	}
}

impl ExtBuilder {
	pub fn build(self) -> sp_io::TestExternalities {
		let mut ext: sp_io::TestExternalities = RuntimeGenesisConfig {
			system: frame_system::GenesisConfig::default(),
			balances: pallet_balances::GenesisConfig {
				balances: vec![(0, 100), (1, 98), (2, 1)],
				..Default::default()
			},
		}
		.build_storage()
		.unwrap()
		.into();
		ext.execute_with(|| {
			frame_system::Pallet::<Test>::set_block_number(1);
		});
		ext
	}

	pub fn build_and_execute(self, test: impl FnOnce() -> ()) {
		self.build().execute_with(|| {
			test();
			Bounties::do_try_state().expect("All invariants must hold after a test");
			Bounties1::do_try_state().expect("All invariants must hold after a test");
		})
	}
}

/// paid balance for a given account and asset ids
pub fn paid(who: u128, asset_id: u32) -> u64 {
	PAID.with(|p| p.borrow().get(&(who, asset_id)).cloned().unwrap_or(0))
}

/// reduce paid balance for a given account and asset ids
pub fn unpay(who: u128, asset_id: u32, amount: u64) {
	PAID.with(|p| p.borrow_mut().entry((who, asset_id)).or_default().saturating_reduce(amount))
}

/// set status for a given payment id
pub fn set_status(id: u64, s: PaymentStatus) {
	STATUS.with(|m| m.borrow_mut().insert(id, s));
}

pub fn last_events(n: usize) -> Vec<BountiesEvent<Test>> {
	let mut res = System::events()
		.into_iter()
		.rev()
		.filter_map(
			|e| if let RuntimeEvent::Bounties(inner) = e.event { Some(inner) } else { None },
		)
		.take(n)
		.collect::<Vec<_>>();
	res.reverse();
	res
}

pub fn last_event() -> BountiesEvent<Test> {
	last_events(1).into_iter().next().unwrap()
}

pub fn expect_events(e: Vec<BountiesEvent<Test>>) {
	assert_eq!(last_events(e.len()), e);
}

/// note a new preimage without registering.
pub fn note_preimage(who: u128) -> <Test as frame_system::Config>::Hash {
	use std::sync::atomic::{AtomicU8, Ordering};
	// note a new preimage on every function invoke.
	static COUNTER: AtomicU8 = AtomicU8::new(0);
	let data = vec![COUNTER.fetch_add(1, Ordering::Relaxed)];
	assert_ok!(Preimage::note_preimage(RuntimeOrigin::signed(who), data.clone()));
	let hash = BlakeTwo256::hash(&data);
	assert!(!Preimage::is_requested(&hash));
	hash
}

/// create consideration for comparison in tests
pub fn consideration(amount: u64) -> <Test as Config>::Consideration {
	<Test as Config>::Consideration::new(&0, amount).unwrap()
}

pub fn get_payment_id(
	parent_bounty_id: BountyIndex,
	child_bounty_id: Option<BountyIndex>,
) -> Option<u64> {
	let bounty =
		pallet_bounties::Pallet::<Test>::get_bounty_details(parent_bounty_id, child_bounty_id)
			.expect("no bounty");

	match bounty.3 {
		BountyStatus::FundingAttempted {
			payment_status: PaymentState::Attempted { id }, ..
		} => Some(id),
		BountyStatus::RefundAttempted {
			payment_status: PaymentState::Attempted { id }, ..
		} => Some(id),
		BountyStatus::PayoutAttempted {
			payment_status: PaymentState::Attempted { id }, ..
		} => Some(id),
		_ => None,
	}
}

pub fn approve_payment(
	dest: u128,
	parent_bounty_id: BountyIndex,
	child_bounty_id: Option<BountyIndex>,
	asset_kind: u32,
	amount: u64,
) {
	assert_eq!(paid(dest, asset_kind), amount);
	let payment_id = get_payment_id(parent_bounty_id, child_bounty_id).expect("no payment attempt");
	set_status(payment_id, PaymentStatus::Success);
	assert_ok!(Bounties::check_status(RuntimeOrigin::signed(0), parent_bounty_id, child_bounty_id));
}

pub fn reject_payment(
	dest: u128,
	parent_bounty_id: BountyIndex,
	child_bounty_id: Option<BountyIndex>,
	asset_kind: u32,
	amount: u64,
) {
	unpay(dest, asset_kind, amount);
	let payment_id = get_payment_id(parent_bounty_id, child_bounty_id).expect("no payment attempt");
	set_status(payment_id, PaymentStatus::Failure);
	assert_ok!(Bounties::check_status(RuntimeOrigin::signed(0), parent_bounty_id, child_bounty_id));
}

#[derive(Clone)]
pub struct TestBounty {
	pub parent_bounty_id: BountyIndex,
	pub child_bounty_id: BountyIndex,
	pub asset_kind: u32,
	pub value: u64,
	pub child_value: u64,
	pub curator: u128,
	pub curator_deposit: u64,
	pub child_curator: u128,
	pub child_curator_deposit: u64,
	pub beneficiary: u128,
	pub child_beneficiary: u128,
	pub metadata: <Test as frame_system::Config>::Hash,
}

pub fn setup_bounty() -> TestBounty {
	let asset_kind = 1;
	let value = 50;
	let child_value = 10;
	let curator = 4;
	let child_curator = 8;
	let beneficiary = 5;
	let child_beneficiary = 9;
	let expected_deposit = CuratorDeposit::convert(value);
	let child_expected_deposit = CuratorDeposit::convert(child_value);
	let metadata = note_preimage(1);
	Balances::set_balance(&curator, Balances::minimum_balance() + expected_deposit);
	Balances::set_balance(&child_curator, Balances::minimum_balance() + child_expected_deposit);

	TestBounty {
		parent_bounty_id: 0,
		child_bounty_id: 0,
		asset_kind,
		value,
		child_value,
		curator,
		curator_deposit: expected_deposit,
		child_curator,
		child_curator_deposit: child_expected_deposit,
		beneficiary,
		child_beneficiary,
		metadata,
	}
}

pub fn create_parent_bounty() -> TestBounty {
	let mut s = setup_bounty();

	assert_ok!(Bounties::fund_bounty(
		RuntimeOrigin::root(),
		Box::new(s.asset_kind),
		s.value,
		s.curator,
		s.metadata
	));
	let parent_bounty_id = pallet_bounties::BountyCount::<Test>::get() - 1;
	s.parent_bounty_id = parent_bounty_id;

	s
}

pub fn create_funded_parent_bounty() -> TestBounty {
	let s = create_parent_bounty();

	let parent_bounty_account =
		Bounties::bounty_account(s.parent_bounty_id, s.asset_kind).expect("conversion failed");
	approve_payment(parent_bounty_account, s.parent_bounty_id, None, s.asset_kind, s.value);

	s
}

pub fn create_active_parent_bounty() -> TestBounty {
	let s = create_funded_parent_bounty();

	assert_ok!(Bounties::accept_curator(
		RuntimeOrigin::signed(s.curator),
		s.parent_bounty_id,
		None,
	));

	s
}

pub fn create_parent_bounty_with_unassigned_curator() -> TestBounty {
	let s = create_funded_parent_bounty();

	assert_ok!(Bounties::unassign_curator(
		RuntimeOrigin::signed(s.curator),
		s.parent_bounty_id,
		None,
	));

	s
}

pub fn create_awarded_parent_bounty() -> TestBounty {
	let s = create_active_parent_bounty();

	assert_ok!(Bounties::award_bounty(
		RuntimeOrigin::signed(s.curator),
		s.parent_bounty_id,
		None,
		s.beneficiary,
	));

	s
}

pub fn create_canceled_parent_bounty() -> TestBounty {
	let s = create_active_parent_bounty();

	assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), s.parent_bounty_id, None,));

	s
}

pub fn create_child_bounty_with_curator() -> TestBounty {
	let mut s = create_active_parent_bounty();

	assert_ok!(Bounties::fund_child_bounty(
		RuntimeOrigin::signed(s.curator),
		s.parent_bounty_id,
		s.child_value,
		s.metadata,
		Some(s.child_curator),
	));
	s.child_bounty_id =
		pallet_bounties::TotalChildBountiesPerParent::<Test>::get(s.parent_bounty_id) - 1;

	s
}

pub fn create_funded_child_bounty() -> TestBounty {
	let s = create_child_bounty_with_curator();

	let child_bounty_account =
		Bounties::child_bounty_account(s.parent_bounty_id, s.child_bounty_id, s.asset_kind)
			.expect("conversion failed");
	approve_payment(
		child_bounty_account,
		s.parent_bounty_id,
		Some(s.child_bounty_id),
		s.asset_kind,
		s.child_value,
	);

	s
}

pub fn create_child_bounty_with_unassigned_curator() -> TestBounty {
	let s = create_funded_child_bounty();

	assert_ok!(Bounties::unassign_curator(
		RuntimeOrigin::signed(s.curator),
		s.parent_bounty_id,
		Some(s.child_bounty_id),
	));

	s
}

pub fn create_active_child_bounty() -> TestBounty {
	let s = create_funded_child_bounty();

	assert_ok!(Bounties::accept_curator(
		RuntimeOrigin::signed(s.child_curator),
		s.parent_bounty_id,
		Some(s.child_bounty_id)
	));

	s
}

pub fn create_canceled_child_bounty() -> TestBounty {
	let s = create_active_child_bounty();

	assert_ok!(Bounties::close_bounty(
		RuntimeOrigin::signed(s.curator),
		s.parent_bounty_id,
		Some(s.child_bounty_id),
	));

	s
}

pub fn create_awarded_child_bounty() -> TestBounty {
	let s = create_active_child_bounty();

	assert_ok!(Bounties::award_bounty(
		RuntimeOrigin::signed(s.child_curator),
		s.parent_bounty_id,
		Some(s.child_bounty_id),
		s.child_beneficiary
	));

	s
}
