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

//! bounties pallet tests.

#![cfg(test)]

use crate as pallet_bounties;
use crate::pallet::*;

use crate::{Bounty, BountyStatus, TransferAllFungibles};
use codec::Encode;
use frame_support::{
	assert_noop, assert_ok, derive_impl, hypothetically, hypothetically_ok,
	pallet_prelude::*,
	parameter_types,
	traits::{
		fungible,
		fungible::{NativeFromLeft, NativeOrWithId},
		tokens::{PayFromAccount, UnityAssetBalanceConversion},
		AsEnsureOriginWithArg, ConstU32, ConstU64, Currency, Imbalance, OnInitialize,
	},
	PalletId,
};
use frame_system::{pallet_prelude::*, EnsureSigned};
use sp_runtime::{
	traits::{BadOrigin, IdentityLookup},
	BuildStorage, Perbill, Permill, Storage,
};

use super::Event as BountiesEvent;

type Block = frame_system::mocking::MockBlock<Test>;

// This function directly jumps to a block number, and calls `on_initialize`.
fn go_to_block(n: u64) {
	<Test as pallet_treasury::Config>::BlockNumberProvider::set_block_number(n);
	<Treasury as OnInitialize<u64>>::on_initialize(n);
}

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		Balances: pallet_balances,
		Assets: pallet_assets,
		Bounties: pallet_bounties,
		Bounties1: pallet_bounties::<Instance1>,
		Treasury: pallet_treasury,
		Treasury1: pallet_treasury::<Instance1>,
	}
);

parameter_types! {
	pub const AvailableBlockRatio: Perbill = Perbill::one();
}

type Balance = u64;
type AccountId = u128;

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type AccountId = AccountId; // u64 is not enough to hold bytes used to generate bounty account
	type Lookup = IdentityLookup<Self::AccountId>;
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u64>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type AccountStore = System;
}
parameter_types! {
	pub static Burn: Permill = Permill::from_percent(50);
	pub const TreasuryPalletId: PalletId = PalletId(*b"py/trsry");
	pub const TreasuryPalletId2: PalletId = PalletId(*b"py/trsr2");
	pub static SpendLimit: Balance = u64::MAX;
	pub static SpendLimit1: Balance = u64::MAX;
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
	type SpendFunds = Bounties;
	type MaxApprovals = ConstU32<100>;
	type SpendOrigin = frame_system::EnsureRootWithSuccess<Self::AccountId, SpendLimit>;
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
	type SpendFunds = Bounties1;
	type MaxApprovals = ConstU32<100>;
	type SpendOrigin = frame_system::EnsureRootWithSuccess<Self::AccountId, SpendLimit1>;
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

#[derive_impl(pallet_assets::config_preludes::TestDefaultConfig)]
impl pallet_assets::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type CreateOrigin = AsEnsureOriginWithArg<EnsureSigned<Self::AccountId>>;
	type ForceOrigin = frame_system::EnsureRoot<Self::AccountId>;
	type AssetDeposit = ConstU64<1>;
	type AssetAccountDeposit = ConstU64<10>;
	type MetadataDepositBase = ConstU64<1>;
	type MetadataDepositPerByte = ConstU64<1>;
	type ApprovalDeposit = ConstU64<1>;
}

pub type NativeAndAssets =
	fungible::UnionOf<Balances, Assets, NativeFromLeft, NativeOrWithId<u32>, AccountId>;

parameter_types! {
	// This will be 50% of the bounty fee.
	pub const CuratorDepositMultiplier: Permill = Permill::from_percent(50);
	pub const CuratorDepositMax: Balance = 1_000;
	pub const CuratorDepositMin: Balance = 3;
	pub static BountyUpdatePeriod: u64 = 20;
	pub static DataDepositPerByte: u64 = 1;
	// Native asset *not* last in the list to test our withdraw logic:
	pub static RelevantAssets: Vec<NativeOrWithId<u32>> = vec![NativeOrWithId::WithId(1), NativeOrWithId::Native, NativeOrWithId::WithId(2)];
}

impl Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type BountyDepositBase = ConstU64<80>;
	type BountyDepositPayoutDelay = ConstU64<3>;
	type BountyUpdatePeriod = BountyUpdatePeriod;
	type CuratorDepositMultiplier = CuratorDepositMultiplier;
	type CuratorDepositMax = CuratorDepositMax;
	type CuratorDepositMin = CuratorDepositMin;
	type BountyValueMinimum = ConstU64<1>;
	type DataDepositPerByte = DataDepositPerByte;
	type MaximumReasonLength = ConstU32<16384>;
	type WeightInfo = ();
	type ChildBountyManager = ();
	type OnSlash = ();
	type TransferAllAssets = TransferAllFungibles<AccountId, NativeAndAssets, RelevantAssets>;
}

impl Config<Instance1> for Test {
	type RuntimeEvent = RuntimeEvent;
	type BountyDepositBase = ConstU64<80>;
	type BountyDepositPayoutDelay = ConstU64<3>;
	type BountyUpdatePeriod = BountyUpdatePeriod;
	type CuratorDepositMultiplier = CuratorDepositMultiplier;
	type CuratorDepositMax = CuratorDepositMax;
	type CuratorDepositMin = CuratorDepositMin;
	type BountyValueMinimum = ConstU64<1>;
	type DataDepositPerByte = DataDepositPerByte;
	type MaximumReasonLength = ConstU32<16384>;
	type WeightInfo = ();
	type ChildBountyManager = ();
	type OnSlash = ();
	type TransferAllAssets = ();
}

type TreasuryError = pallet_treasury::Error<Test>;
type TreasuryError1 = pallet_treasury::Error<Test, Instance1>;

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
			treasury: Default::default(),
			treasury_1: Default::default(),
			assets: pallet_assets::GenesisConfig {
				assets: vec![
					(0, 0, false, 5),
					(1, 0, false, 5),
					(2, 0, true, 5),
					(3, 0, false, 5),
					(4, 0, true, 5),
				],
				accounts: vec![(0, 0, 100), (1, 0, 100), (2, 0, 100), (3, 0, 100), (4, 0, 100)],
				..Default::default()
			},
		}
		.build_storage()
		.unwrap()
		.into();
		ext.execute_with(|| {
			<Test as pallet_treasury::Config>::BlockNumberProvider::set_block_number(1)
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

fn last_events(n: usize) -> Vec<BountiesEvent<Test>> {
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

fn last_event() -> BountiesEvent<Test> {
	last_events(1).into_iter().next().unwrap()
}

fn expect_events(e: Vec<BountiesEvent<Test>>) {
	assert_eq!(last_events(e.len()), e);
}

#[test]
#[allow(deprecated)]
fn genesis_config_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert_eq!(Treasury::pot(), 0);
		assert_eq!(Treasury::proposal_count(), 0);
	});
}

#[test]
fn minting_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Check that accumulate works when we have Some value in Dummy already.
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_eq!(Treasury::pot(), 100);
	});
}

#[test]
#[allow(deprecated)]
fn accepted_spend_proposal_ignored_outside_spend_period() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);

		assert_ok!({ Treasury::spend_local(RuntimeOrigin::root(), 100, 3) });

		go_to_block(1);
		assert_eq!(Balances::free_balance(3), 0);
		assert_eq!(Treasury::pot(), 100);
	});
}

#[test]
fn unused_pot_should_diminish() {
	ExtBuilder::default().build_and_execute(|| {
		let init_total_issuance = pallet_balances::TotalIssuance::<Test>::get();
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_eq!(pallet_balances::TotalIssuance::<Test>::get(), init_total_issuance + 100);

		go_to_block(2);
		assert_eq!(Treasury::pot(), 50);
		assert_eq!(pallet_balances::TotalIssuance::<Test>::get(), init_total_issuance + 50);
	});
}

#[test]
#[allow(deprecated)]
fn accepted_spend_proposal_enacted_on_spend_period() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_eq!(Treasury::pot(), 100);

		assert_ok!({ Treasury::spend_local(RuntimeOrigin::root(), 100, 3) });

		go_to_block(2);
		assert_eq!(Balances::free_balance(3), 100);
		assert_eq!(Treasury::pot(), 0);
	});
}

#[test]
#[allow(deprecated)]
fn pot_underflow_should_not_diminish() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_eq!(Treasury::pot(), 100);

		assert_ok!({ Treasury::spend_local(RuntimeOrigin::root(), 150, 3) });

		go_to_block(2);
		assert_eq!(Treasury::pot(), 100); // Pot hasn't changed

		assert_ok!(Balances::deposit_into_existing(&Treasury::account_id(), 100));
		go_to_block(4);
		assert_eq!(Balances::free_balance(3), 150); // Fund has been spent
		assert_eq!(Treasury::pot(), 25); // Pot has finally changed
	});
}

// Treasury account doesn't get deleted if amount approved to spend is all its free balance.
// i.e. pot should not include existential deposit needed for account survival.
#[test]
#[allow(deprecated)]
fn treasury_account_doesnt_get_deleted() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_eq!(Treasury::pot(), 100);
		let treasury_balance = Balances::free_balance(&Treasury::account_id());

		assert_ok!({ Treasury::spend_local(RuntimeOrigin::root(), treasury_balance, 3) });

		go_to_block(2);
		assert_eq!(Treasury::pot(), 100); // Pot hasn't changed

		assert_ok!({ Treasury::spend_local(RuntimeOrigin::root(), Treasury::pot(), 3) });

		go_to_block(4);
		assert_eq!(Treasury::pot(), 0); // Pot is emptied
		assert_eq!(Balances::free_balance(Treasury::account_id()), 1); // but the account is still there
	});
}

// In case treasury account is not existing then it works fine.
// This is useful for chain that will just update runtime.
#[test]
#[allow(deprecated)]
fn inexistent_account_works() {
	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	pallet_balances::GenesisConfig::<Test> {
		balances: vec![(0, 100), (1, 99), (2, 1)],
		..Default::default()
	}
	.assimilate_storage(&mut t)
	.unwrap();
	// Treasury genesis config is not build thus treasury account does not exist
	let mut t: sp_io::TestExternalities = t.into();

	t.execute_with(|| {
		assert_eq!(Balances::free_balance(Treasury::account_id()), 0); // Account does not exist
		assert_eq!(Treasury::pot(), 0); // Pot is empty

		assert_ok!({ Treasury::spend_local(RuntimeOrigin::root(), 99, 3) });
		assert_ok!({ Treasury::spend_local(RuntimeOrigin::root(), 1, 3) });
		go_to_block(2);

		assert_eq!(Treasury::pot(), 0); // Pot hasn't changed
		assert_eq!(Balances::free_balance(3), 0); // Balance of `3` hasn't changed

		Balances::make_free_balance_be(&Treasury::account_id(), 100);
		assert_eq!(Treasury::pot(), 99); // Pot now contains funds
		assert_eq!(Balances::free_balance(Treasury::account_id()), 100); // Account does exist

		go_to_block(4);

		assert_eq!(Treasury::pot(), 0); // Pot has changed
		assert_eq!(Balances::free_balance(3), 99); // Balance of `3` has changed
	});
}

#[test]
fn propose_bounty_works() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_eq!(Treasury::pot(), 100);

		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 10, b"1234567890".to_vec()));

		assert_eq!(last_event(), BountiesEvent::BountyProposed { index: 0 });

		let deposit: u64 = 85 + 5;
		assert_eq!(Balances::reserved_balance(0), deposit);
		assert_eq!(Balances::free_balance(0), 100 - deposit);

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee: 0,
				curator_deposit: 0,
				value: 10,
				bond: deposit,
				status: BountyStatus::Proposed,
			}
		);

		assert_eq!(
			pallet_bounties::BountyDescriptions::<Test>::get(0).unwrap(),
			b"1234567890".to_vec()
		);

		assert_eq!(pallet_bounties::BountyCount::<Test>::get(), 1);
	});
}

#[test]
fn propose_bounty_validation_works() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_eq!(Treasury::pot(), 100);

		assert_noop!(
			Bounties::propose_bounty(RuntimeOrigin::signed(1), 0, [0; 17_000].to_vec()),
			Error::<Test>::ReasonTooBig
		);

		assert_noop!(
			Bounties::propose_bounty(
				RuntimeOrigin::signed(1),
				10,
				b"12345678901234567890".to_vec()
			),
			Error::<Test>::InsufficientProposersBalance
		);

		assert_noop!(
			Bounties::propose_bounty(RuntimeOrigin::signed(1), 0, b"12345678901234567890".to_vec()),
			Error::<Test>::InvalidValue
		);
	});
}

#[test]
#[allow(deprecated)]
fn close_bounty_works() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_noop!(Bounties::close_bounty(RuntimeOrigin::root(), 0), Error::<Test>::InvalidIndex);

		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 10, b"12345".to_vec()));

		assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), 0));

		let deposit: u64 = 80 + 5;

		assert_eq!(last_event(), BountiesEvent::BountyRejected { index: 0, bond: deposit });

		assert_eq!(Balances::reserved_balance(0), 0);
		assert_eq!(Balances::free_balance(0), 100 - deposit);

		assert_eq!(pallet_bounties::Bounties::<Test>::get(0), None);
		assert!(!pallet_treasury::Proposals::<Test>::contains_key(0));

		assert_eq!(pallet_bounties::BountyDescriptions::<Test>::get(0), None);
	});
}

#[test]
#[allow(deprecated)]
fn close_bounty_with_additional_assets_works() {
	ExtBuilder::default().build_and_execute(|| {
		let pot = Bounties::bounty_account_id(0);
		let ed = 1;

		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 10, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));
		go_to_block(2);
		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, 0, 1));
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(0), 0));

		assert!(matches!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap().status,
			BountyStatus::Active { .. }
		));
		// Send the Bounty account some funds
		Balances::make_free_balance_be(&pot, 100);

		// We *could* close the bounty now
		hypothetically_ok!(Bounties::close_bounty(RuntimeOrigin::root(), 0));

		// If we send an insufficient asset to the bounty, then the reaping will preserve ED.
		// Otherwise it will transfer out all balance.
		// Case 1: Bounty acc is blocked
		hypothetically!({
			// Send irrelevant insufficient asset to the bounty account.
			assert_ok!(Assets::transfer(RuntimeOrigin::signed(0), 0, pot, 10));

			// Now we can close the bounty, but the account is not gone
			assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), 0));
			// Bounty account must exist
			assert!(frame_system::Account::<Test>::contains_key(pot));

			assert_eq!(Balances::total_balance(&pot), ed, "ED is preserved");
			// This ^ was fixed by #10728. Otherwise, instead of just ED, everything would remain.
		});

		// Case 2: Bounty acc is not blocked and transfers our all balances
		hypothetically!({
			assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), 0));
			assert!(!frame_system::Account::<Test>::contains_key(pot));
			assert_eq!(Balances::total_balance(&pot), 0, "All balances are transferred");
		});

		// Case 3: Blocked by relevant asset (insufficient)
		hypothetically!({
			assert_ok!(Assets::transfer(RuntimeOrigin::signed(0), 1, pot, 10));

			assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), 0));
			assert!(!frame_system::Account::<Test>::contains_key(pot));
			assert_eq!(Balances::total_balance(&pot), 0, "All balances are transferred");
		});

		// Case 4: Blocked by relevant asset (sufficient)
		hypothetically!({
			assert_ok!(Assets::transfer(RuntimeOrigin::signed(0), 2, pot, 10));

			assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), 0));
			assert!(!frame_system::Account::<Test>::contains_key(pot));
			assert_eq!(Balances::total_balance(&pot), 0, "All balances are transferred");
		});

		// Case 5: Blocked by irrelevant asset (sufficient)
		hypothetically!({
			assert_ok!(Assets::transfer(RuntimeOrigin::signed(0), 4, pot, 10));

			assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), 0));
			assert!(frame_system::Account::<Test>::contains_key(pot));
			assert_eq!(Balances::total_balance(&pot), 0, "All balances are transferred");
			assert_eq!(Assets::balance(4, &pot), 10);
		});

		// Case 6: Blocked by irrelevant asset (insufficient)
		hypothetically!({
			assert_ok!(Assets::transfer(RuntimeOrigin::signed(0), 3, pot, 10));

			assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), 0));
			assert!(frame_system::Account::<Test>::contains_key(pot));
			assert_eq!(Balances::total_balance(&pot), ed, "ED is preserved");
			assert_eq!(Assets::balance(3, &pot), 10);
		});
	});
}

#[test]
#[allow(deprecated)]
fn close_bounty_with_random_references_works() {
	ExtBuilder::default().build_and_execute(|| {
		let pot = Bounties::bounty_account_id(0);
		let ed = 1;

		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 10, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));
		go_to_block(2);
		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, 0, 1));
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(0), 0));

		assert!(matches!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap().status,
			BountyStatus::Active { .. }
		));
		// Send some funds and assets to the bounty account
		Balances::make_free_balance_be(&pot, 100);
		assert_ok!(Assets::transfer(RuntimeOrigin::signed(0), 0, pot, 10));
		assert_ok!(Assets::transfer(RuntimeOrigin::signed(0), 1, pot, 10));
		assert_ok!(Assets::transfer(RuntimeOrigin::signed(0), 2, pot, 10));
		assert_ok!(Assets::transfer(RuntimeOrigin::signed(0), 3, pot, 10));
		assert_ok!(Assets::transfer(RuntimeOrigin::signed(0), 4, pot, 10));

		for ((s, c), p) in (0..10).zip(0..10).zip(0..10) {
			hypothetically!({
				// Completely mess up the account's references and check that closing still works
				frame_system::Account::<Test>::mutate(&pot, |a| {
					a.sufficients = s;
					a.consumers = c;
					a.providers = p;
				});

				// Bounty acc has all the balances
				assert_eq!(Balances::total_balance(&pot), 100);
				assert_eq!(Assets::balance(0, &pot), 10);
				assert_eq!(Assets::balance(1, &pot), 10);
				assert_eq!(Assets::balance(2, &pot), 10);
				assert_eq!(Assets::balance(3, &pot), 10);
				assert_eq!(Assets::balance(4, &pot), 10);

				assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), 0));
				// Native balance not more than ED
				assert!(Balances::total_balance(&pot) <= ed);
				// Only the non-relevant assets remain
				assert_eq!(Assets::balance(0, &pot), 10);
				assert_eq!(Assets::balance(1, &pot), 0);
				assert_eq!(Assets::balance(2, &pot), 0);
				assert_eq!(Assets::balance(3, &pot), 10);
				assert_eq!(Assets::balance(4, &pot), 10);
			});
		}
	});
}

#[test]
fn approve_bounty_works() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_noop!(
			Bounties::approve_bounty(RuntimeOrigin::root(), 0),
			Error::<Test>::InvalidIndex
		);

		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));

		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));

		let deposit: u64 = 80 + 5;

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee: 0,
				value: 50,
				curator_deposit: 0,
				bond: deposit,
				status: BountyStatus::Approved,
			}
		);
		assert_eq!(pallet_bounties::BountyApprovals::<Test>::get(), vec![0]);

		assert_noop!(
			Bounties::close_bounty(RuntimeOrigin::root(), 0),
			Error::<Test>::UnexpectedStatus
		);

		// deposit not returned yet
		assert_eq!(Balances::reserved_balance(0), deposit);
		assert_eq!(Balances::free_balance(0), 100 - deposit);

		go_to_block(2);

		// return deposit
		assert_eq!(Balances::reserved_balance(0), 0);
		assert_eq!(Balances::free_balance(0), 100);

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee: 0,
				curator_deposit: 0,
				value: 50,
				bond: deposit,
				status: BountyStatus::Funded,
			}
		);

		assert_eq!(Treasury::pot(), 100 - 50 - 25); // burn 25
		assert_eq!(Balances::free_balance(Bounties::bounty_account_id(0)), 50);
	});
}

#[test]
fn assign_curator_works() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);

		assert_noop!(
			Bounties::propose_curator(RuntimeOrigin::root(), 0, 4, 4),
			Error::<Test>::InvalidIndex
		);

		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));

		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));

		go_to_block(2);

		assert_noop!(
			Bounties::propose_curator(RuntimeOrigin::root(), 0, 4, 50),
			Error::<Test>::InvalidFee
		);

		let fee = 4;
		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, 4, fee));

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee,
				curator_deposit: 0,
				value: 50,
				bond: 85,
				status: BountyStatus::CuratorProposed { curator: 4 },
			}
		);

		assert_noop!(
			Bounties::accept_curator(RuntimeOrigin::signed(1), 0),
			Error::<Test>::RequireCurator
		);
		assert_noop!(
			Bounties::accept_curator(RuntimeOrigin::signed(4), 0),
			pallet_balances::Error::<Test, _>::InsufficientBalance
		);

		Balances::make_free_balance_be(&4, 10);

		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(4), 0));

		let expected_deposit = Bounties::calculate_curator_deposit(&fee);

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee,
				curator_deposit: expected_deposit,
				value: 50,
				bond: 85,
				status: BountyStatus::Active { curator: 4, update_due: 22 },
			}
		);

		assert_eq!(Balances::free_balance(&4), 10 - expected_deposit);
		assert_eq!(Balances::reserved_balance(&4), expected_deposit);
	});
}

#[test]
fn unassign_curator_works() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));

		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));

		go_to_block(2);

		let fee = 4;

		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, 4, fee));
		assert_noop!(Bounties::unassign_curator(RuntimeOrigin::signed(1), 0), BadOrigin);
		assert_ok!(Bounties::unassign_curator(RuntimeOrigin::signed(4), 0));

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee,
				curator_deposit: 0,
				value: 50,
				bond: 85,
				status: BountyStatus::Funded,
			}
		);

		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, 4, fee));
		Balances::make_free_balance_be(&4, 10);
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(4), 0));
		let expected_deposit = Bounties::calculate_curator_deposit(&fee);
		assert_ok!(Bounties::unassign_curator(RuntimeOrigin::root(), 0));

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee,
				curator_deposit: 0,
				value: 50,
				bond: 85,
				status: BountyStatus::Funded,
			}
		);

		assert_eq!(Balances::free_balance(&4), 10 - expected_deposit);
		assert_eq!(Balances::reserved_balance(&4), 0); // slashed curator deposit
	});
}

#[test]
fn award_and_claim_bounty_works() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		Balances::make_free_balance_be(&4, 10);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));

		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));

		go_to_block(2);

		let fee = 4;
		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, 4, fee));
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(4), 0));

		let expected_deposit = Bounties::calculate_curator_deposit(&fee);
		assert_eq!(Balances::free_balance(4), 10 - expected_deposit);

		assert_noop!(
			Bounties::award_bounty(RuntimeOrigin::signed(1), 0, 3),
			Error::<Test>::RequireCurator
		);

		assert_ok!(Bounties::award_bounty(RuntimeOrigin::signed(4), 0, 3));

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee,
				curator_deposit: expected_deposit,
				value: 50,
				bond: 85,
				status: BountyStatus::PendingPayout { curator: 4, beneficiary: 3, unlock_at: 5 },
			}
		);

		assert_noop!(Bounties::claim_bounty(RuntimeOrigin::signed(1), 0), Error::<Test>::Premature);

		go_to_block(5);

		assert_ok!(Balances::transfer_allow_death(
			RuntimeOrigin::signed(0),
			Bounties::bounty_account_id(0),
			10
		));

		assert_ok!(Bounties::claim_bounty(RuntimeOrigin::signed(1), 0));

		assert_eq!(
			last_event(),
			BountiesEvent::BountyClaimed { index: 0, payout: 56, beneficiary: 3 }
		);

		assert_eq!(Balances::free_balance(4), 14); // initial 10 + fee 4

		assert_eq!(Balances::free_balance(3), 56);
		assert_eq!(Balances::free_balance(Bounties::bounty_account_id(0)), 0);

		assert_eq!(pallet_bounties::Bounties::<Test>::get(0), None);
		assert_eq!(pallet_bounties::BountyDescriptions::<Test>::get(0), None);
	});
}

#[test]
fn claim_handles_high_fee() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		Balances::make_free_balance_be(&4, 30);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));

		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));

		go_to_block(2);

		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, 4, 49));
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(4), 0));

		assert_ok!(Bounties::award_bounty(RuntimeOrigin::signed(4), 0, 3));

		go_to_block(5);

		// make fee > balance
		let res = Balances::slash(&Bounties::bounty_account_id(0), 10);
		assert_eq!(res.0.peek(), 10);

		assert_ok!(Bounties::claim_bounty(RuntimeOrigin::signed(1), 0));

		assert_eq!(
			last_event(),
			BountiesEvent::BountyClaimed { index: 0, payout: 0, beneficiary: 3 }
		);

		assert_eq!(Balances::free_balance(4), 70); // 30 + 50 - 10
		assert_eq!(Balances::free_balance(3), 0);
		assert_eq!(Balances::free_balance(Bounties::bounty_account_id(0)), 0);

		assert_eq!(pallet_bounties::Bounties::<Test>::get(0), None);
		assert_eq!(pallet_bounties::BountyDescriptions::<Test>::get(0), None);
	});
}

#[test]
fn cancel_and_refund() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);

		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));

		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));

		go_to_block(2);

		assert_ok!(Balances::transfer_allow_death(
			RuntimeOrigin::signed(0),
			Bounties::bounty_account_id(0),
			10
		));

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee: 0,
				curator_deposit: 0,
				value: 50,
				bond: 85,
				status: BountyStatus::Funded,
			}
		);

		assert_eq!(Balances::free_balance(Bounties::bounty_account_id(0)), 60);

		assert_noop!(Bounties::close_bounty(RuntimeOrigin::signed(0), 0), BadOrigin);

		assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), 0));

		// `- 25 + 10`
		assert_eq!(Treasury::pot(), 85);
	});
}

#[test]
fn award_and_cancel() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));

		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));

		go_to_block(2);

		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, 0, 10));
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(0), 0));

		assert_eq!(Balances::free_balance(0), 95);
		assert_eq!(Balances::reserved_balance(0), 5);

		assert_ok!(Bounties::award_bounty(RuntimeOrigin::signed(0), 0, 3));

		// Cannot close bounty directly when payout is happening...
		assert_noop!(
			Bounties::close_bounty(RuntimeOrigin::root(), 0),
			Error::<Test>::PendingPayout
		);

		// Instead unassign the curator to slash them and then close.
		assert_ok!(Bounties::unassign_curator(RuntimeOrigin::root(), 0));
		assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), 0));

		assert_eq!(last_event(), BountiesEvent::BountyCanceled { index: 0 });

		assert_eq!(Balances::free_balance(Bounties::bounty_account_id(0)), 0);

		// Slashed.
		assert_eq!(Balances::free_balance(0), 95);
		assert_eq!(Balances::reserved_balance(0), 0);

		assert_eq!(pallet_bounties::Bounties::<Test>::get(0), None);
		assert_eq!(pallet_bounties::BountyDescriptions::<Test>::get(0), None);
	});
}

#[test]
fn expire_and_unassign() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));

		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));

		go_to_block(2);

		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, 1, 10));
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(1), 0));

		assert_eq!(Balances::free_balance(1), 93);
		assert_eq!(Balances::reserved_balance(1), 5);

		go_to_block(22);

		assert_noop!(
			Bounties::unassign_curator(RuntimeOrigin::signed(0), 0),
			Error::<Test>::Premature
		);

		go_to_block(23);

		assert_ok!(Bounties::unassign_curator(RuntimeOrigin::signed(0), 0));

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee: 10,
				curator_deposit: 0,
				value: 50,
				bond: 85,
				status: BountyStatus::Funded,
			}
		);

		assert_eq!(Balances::free_balance(1), 93);
		assert_eq!(Balances::reserved_balance(1), 0); // slashed
	});
}

#[test]
fn extend_expiry() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		Balances::make_free_balance_be(&4, 10);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));

		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));

		assert_noop!(
			Bounties::extend_bounty_expiry(RuntimeOrigin::signed(1), 0, Vec::new()),
			Error::<Test>::UnexpectedStatus
		);

		go_to_block(2);

		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, 4, 10));
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(4), 0));

		assert_eq!(Balances::free_balance(4), 5);
		assert_eq!(Balances::reserved_balance(4), 5);

		go_to_block(10);

		assert_noop!(
			Bounties::extend_bounty_expiry(RuntimeOrigin::signed(0), 0, Vec::new()),
			Error::<Test>::RequireCurator
		);
		assert_ok!(Bounties::extend_bounty_expiry(RuntimeOrigin::signed(4), 0, Vec::new()));

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee: 10,
				curator_deposit: 5,
				value: 50,
				bond: 85,
				status: BountyStatus::Active { curator: 4, update_due: 30 },
			}
		);

		assert_ok!(Bounties::extend_bounty_expiry(RuntimeOrigin::signed(4), 0, Vec::new()));

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee: 10,
				curator_deposit: 5,
				value: 50,
				bond: 85,
				status: BountyStatus::Active { curator: 4, update_due: 30 }, // still the same
			}
		);

		go_to_block(25);

		assert_noop!(
			Bounties::unassign_curator(RuntimeOrigin::signed(0), 0),
			Error::<Test>::Premature
		);
		assert_ok!(Bounties::unassign_curator(RuntimeOrigin::signed(4), 0));

		assert_eq!(Balances::free_balance(4), 10); // not slashed
		assert_eq!(Balances::reserved_balance(4), 0);
	});
}

#[test]
fn test_migration_v4() {
	let mut s = Storage::default();

	let index: u32 = 10;

	let bounty = Bounty::<u128, u64, u64> {
		proposer: 0,
		value: 20,
		fee: 20,
		curator_deposit: 20,
		bond: 50,
		status: BountyStatus::<u128, u64>::Proposed,
	};

	let data = vec![
		(pallet_bounties::BountyCount::<Test>::hashed_key().to_vec(), 10.encode().to_vec()),
		(pallet_bounties::Bounties::<Test>::hashed_key_for(index), bounty.encode().to_vec()),
		(pallet_bounties::BountyDescriptions::<Test>::hashed_key_for(index), vec![0, 0]),
		(
			pallet_bounties::BountyApprovals::<Test>::hashed_key().to_vec(),
			vec![10 as u32].encode().to_vec(),
		),
	];

	s.top = data.into_iter().collect();

	sp_io::TestExternalities::new(s).execute_with(|| {
		use frame_support::traits::PalletInfo;
		let old_pallet_name = <Test as frame_system::Config>::PalletInfo::name::<Bounties>()
			.expect("Bounties is part of runtime, so it has a name; qed");
		let new_pallet_name = "NewBounties";

		crate::migrations::v4::pre_migration::<Test, Bounties, _>(old_pallet_name, new_pallet_name);
		crate::migrations::v4::migrate::<Test, Bounties, _>(old_pallet_name, new_pallet_name);
		crate::migrations::v4::post_migration::<Test, Bounties, _>(
			old_pallet_name,
			new_pallet_name,
		);
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
fn unassign_curator_self() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));

		go_to_block(2);

		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, 1, 10));
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(1), 0));

		assert_eq!(Balances::free_balance(1), 93);
		assert_eq!(Balances::reserved_balance(1), 5);

		go_to_block(8);

		assert_ok!(Bounties::unassign_curator(RuntimeOrigin::signed(1), 0));

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee: 10,
				curator_deposit: 0,
				value: 50,
				bond: 85,
				status: BountyStatus::Funded,
			}
		);

		assert_eq!(Balances::free_balance(1), 98);
		assert_eq!(Balances::reserved_balance(1), 0); // not slashed
	});
}

#[test]
fn accept_curator_handles_different_deposit_calculations() {
	// This test will verify that a bounty with and without a fee results
	// in a different curator deposit: one using the value, and one using the fee.
	ExtBuilder::default().build_and_execute(|| {
		// Case 1: With a fee
		let user = 1;
		let bounty_index = 0;
		let value = 88;
		let fee = 42;

		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		Balances::make_free_balance_be(&user, 100);
		// Allow for a larger spend limit:
		SpendLimit::set(value);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), value, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), bounty_index));

		go_to_block(2);

		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), bounty_index, user, fee));
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(user), bounty_index));

		let expected_deposit = CuratorDepositMultiplier::get() * fee;
		assert_eq!(Balances::free_balance(&user), 100 - expected_deposit);
		assert_eq!(Balances::reserved_balance(&user), expected_deposit);

		// Case 2: Lower bound
		let user = 2;
		let bounty_index = 1;
		let value = 35;
		let fee = 0;

		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		Balances::make_free_balance_be(&user, 100);

		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), value, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), bounty_index));

		go_to_block(4);

		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), bounty_index, user, fee));
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(user), bounty_index));

		let expected_deposit = CuratorDepositMin::get();
		assert_eq!(Balances::free_balance(&user), 100 - expected_deposit);
		assert_eq!(Balances::reserved_balance(&user), expected_deposit);

		// Case 3: Upper bound
		let user = 3;
		let bounty_index = 2;
		let value = 1_000_000;
		let fee = 50_000;
		let starting_balance = fee * 2;

		Balances::make_free_balance_be(&Treasury::account_id(), value * 2);
		Balances::make_free_balance_be(&user, starting_balance);
		Balances::make_free_balance_be(&0, starting_balance);

		// Allow for a larger spend limit:
		SpendLimit::set(value);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), value, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), bounty_index));

		go_to_block(6);

		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), bounty_index, user, fee));
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(user), bounty_index));

		let expected_deposit = CuratorDepositMax::get();
		assert_eq!(Balances::free_balance(&user), starting_balance - expected_deposit);
		assert_eq!(Balances::reserved_balance(&user), expected_deposit);
	});
}

#[test]
fn approve_bounty_works_second_instance() {
	ExtBuilder::default().build_and_execute(|| {
		// Set burn to 0 to make tracking funds easier.
		Burn::set(Permill::from_percent(0));

		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		Balances::make_free_balance_be(&Treasury1::account_id(), 201);
		assert_eq!(Balances::free_balance(&Treasury::account_id()), 101);
		assert_eq!(Balances::free_balance(&Treasury1::account_id()), 201);

		assert_ok!(Bounties1::propose_bounty(RuntimeOrigin::signed(0), 10, b"12345".to_vec()));
		assert_ok!(Bounties1::approve_bounty(RuntimeOrigin::root(), 0));
		go_to_block(2);
		<Treasury1 as OnInitialize<u64>>::on_initialize(2);

		// Bounties 1 is funded... but from where?
		assert_eq!(Balances::free_balance(Bounties1::bounty_account_id(0)), 10);
		// Treasury 1 unchanged
		assert_eq!(Balances::free_balance(&Treasury::account_id()), 101);
		// Treasury 2 has funds removed
		assert_eq!(Balances::free_balance(&Treasury1::account_id()), 201 - 10);
	});
}

#[test]
fn approve_bounty_insufficient_spend_limit_errors() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_eq!(Treasury::pot(), 100);

		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 51, b"123".to_vec()));
		// 51 will not work since the limit is 50.
		SpendLimit::set(50);
		assert_noop!(
			Bounties::approve_bounty(RuntimeOrigin::root(), 0),
			TreasuryError::InsufficientPermission
		);
	});
}

#[test]
fn approve_bounty_instance1_insufficient_spend_limit_errors() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury1::account_id(), 101);
		assert_eq!(Treasury1::pot(), 100);

		assert_ok!(Bounties1::propose_bounty(RuntimeOrigin::signed(0), 51, b"123".to_vec()));
		// 51 will not work since the limit is 50.
		SpendLimit1::set(50);
		assert_noop!(
			Bounties1::approve_bounty(RuntimeOrigin::root(), 0),
			TreasuryError1::InsufficientPermission
		);
	});
}

#[test]
fn propose_curator_insufficient_spend_limit_errors() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);

		// Temporarily set a larger spend limit;
		SpendLimit::set(51);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 51, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));

		go_to_block(2);

		SpendLimit::set(50);
		// 51 will not work since the limit is 50.
		assert_noop!(
			Bounties::propose_curator(RuntimeOrigin::root(), 0, 0, 0),
			TreasuryError::InsufficientPermission
		);
	});
}

#[test]
fn propose_curator_instance1_insufficient_spend_limit_errors() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);

		// Temporarily set a larger spend limit;
		SpendLimit1::set(11);
		assert_ok!(Bounties1::propose_bounty(RuntimeOrigin::signed(0), 11, b"12345".to_vec()));
		assert_ok!(Bounties1::approve_bounty(RuntimeOrigin::root(), 0));

		<Treasury1 as OnInitialize<u64>>::on_initialize(2);

		SpendLimit1::set(10);
		// 11 will not work since the limit is 10.
		assert_noop!(
			Bounties1::propose_curator(RuntimeOrigin::root(), 0, 0, 0),
			TreasuryError1::InsufficientPermission
		);
	});
}

#[test]
fn approve_bounty_with_curator_works() {
	ExtBuilder::default().build_and_execute(|| {
		let fee = 10;
		let curator = 4;
		System::set_block_number(1);
		Balances::make_free_balance_be(&Treasury::account_id(), 101);

		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));
		assert_noop!(
			Bounties::approve_bounty_with_curator(RuntimeOrigin::signed(1), 0, curator, 10),
			BadOrigin
		);

		SpendLimit::set(1);
		assert_noop!(
			Bounties::approve_bounty_with_curator(RuntimeOrigin::root(), 0, curator, 10),
			TreasuryError::InsufficientPermission
		);
		SpendLimit::set(u64::MAX);

		assert_noop!(
			Bounties::approve_bounty_with_curator(RuntimeOrigin::root(), 0, curator, 51),
			Error::<Test>::InvalidFee
		);

		assert_eq!(pallet_bounties::BountyApprovals::<Test>::get().len(), 0);
		assert_ok!(Bounties::approve_bounty_with_curator(RuntimeOrigin::root(), 0, curator, 10));
		assert_eq!(pallet_bounties::BountyApprovals::<Test>::get().len(), 1);

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee,
				curator_deposit: 0,
				value: 50,
				bond: 85,
				status: BountyStatus::ApprovedWithCurator { curator },
			}
		);

		expect_events(vec![
			BountiesEvent::BountyApproved { index: 0 },
			BountiesEvent::CuratorProposed { bounty_id: 0, curator },
		]);

		assert_noop!(
			Bounties::approve_bounty_with_curator(RuntimeOrigin::root(), 0, curator, 10),
			Error::<Test>::UnexpectedStatus
		);

		System::set_block_number(2);
		<Treasury as OnInitialize<u64>>::on_initialize(2);
		assert_eq!(pallet_bounties::BountyApprovals::<Test>::get().len(), 0);

		expect_events(vec![BountiesEvent::BountyBecameActive { index: 0 }]);

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee,
				curator_deposit: 0,
				value: 50,
				bond: 85,
				status: BountyStatus::CuratorProposed { curator },
			}
		);

		assert_noop!(
			Bounties::accept_curator(RuntimeOrigin::signed(curator), 0),
			pallet_balances::Error::<Test, _>::InsufficientBalance
		);
		Balances::make_free_balance_be(&curator, 6);
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(curator), 0));

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee,
				curator_deposit: 5,
				value: 50,
				bond: 85,
				status: BountyStatus::Active { curator, update_due: 22 },
			}
		);

		assert_ok!(Bounties::award_bounty(RuntimeOrigin::signed(curator), 0, 5));
		System::set_block_number(5);
		<Treasury as OnInitialize<u64>>::on_initialize(5);
		assert_ok!(Bounties::claim_bounty(RuntimeOrigin::signed(curator), 0));
		assert_eq!(
			last_event(),
			BountiesEvent::BountyClaimed { index: 0, payout: 40, beneficiary: 5 }
		);
		assert_eq!(Balances::free_balance(5), 40); // 50 - 10
	});
}

#[test]
fn approve_bounty_with_curator_early_unassign_works() {
	ExtBuilder::default().build_and_execute(|| {
		let fee = 10;
		let curator = 4;
		System::set_block_number(1);
		Balances::make_free_balance_be(&Treasury::account_id(), 101);

		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty_with_curator(RuntimeOrigin::root(), 0, curator, 10));

		// unassign curator while bounty is not yet funded
		assert_ok!(Bounties::unassign_curator(RuntimeOrigin::root(), 0));

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee,
				curator_deposit: 0,
				value: 50,
				bond: 85,
				status: BountyStatus::Approved,
			}
		);

		assert_eq!(last_event(), BountiesEvent::CuratorUnassigned { bounty_id: 0 });

		System::set_block_number(2);
		<Treasury as OnInitialize<u64>>::on_initialize(2);
		assert_eq!(last_event(), BountiesEvent::BountyBecameActive { index: 0 });
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee,
				curator_deposit: 0,
				value: 50,
				bond: 85,
				status: BountyStatus::Funded,
			}
		);

		// assign curator again through separate process
		let new_fee = 15;
		let new_curator = 5;
		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, new_curator, new_fee));

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee: new_fee,
				curator_deposit: 0,
				value: 50,
				bond: 85,
				status: BountyStatus::CuratorProposed { curator: new_curator },
			}
		);
		assert_eq!(
			last_event(),
			BountiesEvent::CuratorProposed { bounty_id: 0, curator: new_curator }
		);
	});
}

#[test]
fn approve_bounty_with_curator_proposed_unassign_works() {
	ExtBuilder::default().build_and_execute(|| {
		let fee = 10;
		let curator = 4;
		System::set_block_number(1);
		Balances::make_free_balance_be(&Treasury::account_id(), 101);

		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty_with_curator(RuntimeOrigin::root(), 0, curator, 10));

		System::set_block_number(2);
		<Treasury as OnInitialize<u64>>::on_initialize(2);

		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee,
				curator_deposit: 0,
				value: 50,
				bond: 85,
				status: BountyStatus::CuratorProposed { curator },
			}
		);

		assert_ok!(Bounties::unassign_curator(RuntimeOrigin::signed(curator), 0));
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				fee,
				curator_deposit: 0,
				value: 50,
				bond: 85,
				status: BountyStatus::Funded,
			}
		);
		assert_eq!(last_event(), BountiesEvent::CuratorUnassigned { bounty_id: 0 });
	});
}

#[test]
fn accept_curator_sets_update_due_correctly() {
	ExtBuilder::default().build_and_execute(|| {
		// Given (BountyUpdatePeriod = 20)
		let bounty_id = 0;
		let proposer = 0;
		let fee = 10;
		let curator = 4;
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		Balances::make_free_balance_be(&curator, 12);
		assert_ok!(Bounties::propose_bounty(
			RuntimeOrigin::signed(proposer),
			50,
			b"12345".to_vec()
		));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));
		go_to_block(4);
		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), bounty_id, curator, fee));

		// When
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(curator), bounty_id));

		// Then
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(bounty_id).unwrap().status,
			BountyStatus::Active { curator, update_due: 24 }
		);

		// Given (BountyUpdatePeriod = BlockNumber::max_value())
		BountyUpdatePeriod::set(BlockNumberFor::<Test>::max_value());
		Balances::make_free_balance_be(&Treasury1::account_id(), 101);
		assert_ok!(Bounties1::propose_bounty(
			RuntimeOrigin::signed(proposer),
			50,
			b"12345".to_vec()
		));
		assert_ok!(Bounties1::approve_bounty(RuntimeOrigin::root(), bounty_id));
		go_to_block(6);
		<Treasury1 as OnInitialize<u64>>::on_initialize(6);
		assert_ok!(Bounties1::propose_curator(RuntimeOrigin::root(), bounty_id, curator, fee));

		// When
		assert_ok!(Bounties1::accept_curator(RuntimeOrigin::signed(curator), bounty_id));

		// Then
		assert_eq!(
			pallet_bounties::Bounties::<Test, Instance1>::get(bounty_id).unwrap().status,
			BountyStatus::Active { curator, update_due: BlockNumberFor::<Test>::max_value() }
		);

		// When
		assert_ok!(Bounties1::extend_bounty_expiry(
			RuntimeOrigin::signed(curator),
			bounty_id,
			Vec::new()
		));

		// Then
		assert_eq!(
			pallet_bounties::Bounties::<Test, Instance1>::get(bounty_id).unwrap().status,
			BountyStatus::Active { curator, update_due: BlockNumberFor::<Test>::max_value() }
		);
	});
}

#[test]
fn poke_deposit_fails_for_insufficient_balance() {
	ExtBuilder::default().build_and_execute(|| {
		// Create a description for the bounty
		let description = b"12345".to_vec();
		let bounded_description = description.clone().try_into().unwrap();
		// Create a bounty
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, description.clone()));

		// BountyDepositBase (80) + DataDepositPerByte (1) * description.len() (5)
		let deposit =
			pallet_bounties::Pallet::<Test>::calculate_bounty_deposit(&bounded_description);

		// Verify initial state
		assert_eq!(Balances::reserved_balance(0), deposit);
		assert_eq!(Balances::free_balance(0), 100 - deposit);
		assert_eq!(last_event(), BountiesEvent::BountyProposed { index: 0 });
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(0).unwrap(),
			Bounty {
				proposer: 0,
				value: 50,
				fee: 0,
				curator_deposit: 0,
				bond: deposit,
				status: BountyStatus::Proposed,
			}
		);
		assert_eq!(pallet_bounties::BountyDescriptions::<Test>::get(0).unwrap(), description);
		assert_eq!(pallet_bounties::BountyCount::<Test>::get(), 1);

		// Increase the DataDepositPerByte to be more than the total balance of the proposer
		DataDepositPerByte::set(20);

		// Poke deposit should fail due to insufficient balance
		assert_noop!(
			Bounties::poke_deposit(RuntimeOrigin::signed(0), 0),
			pallet_balances::Error::<Test>::InsufficientBalance
		);
	});
}

#[test]
fn poke_deposit_fails_for_unsigned_origin() {
	ExtBuilder::default().build_and_execute(|| {
		assert_noop!(Bounties::poke_deposit(RuntimeOrigin::none(), 0), DispatchError::BadOrigin);
	});
}

#[test]
fn poke_deposit_fails_for_non_existent_bounty() {
	ExtBuilder::default().build_and_execute(|| {
		assert_noop!(
			Bounties::poke_deposit(RuntimeOrigin::signed(0), 0),
			Error::<Test>::InvalidIndex
		);
	});
}

#[test]
fn poke_deposit_fails_for_any_status_other_than_proposed() {
	ExtBuilder::default().build_and_execute(|| {
		let bounty_id = 0;
		let proposer = 0;
		let curator = 4;
		let deposit = 85;
		let mut bounty = Bounty {
			proposer,
			value: 50,
			fee: 0,
			curator_deposit: 0,
			bond: deposit,
			status: BountyStatus::Proposed,
		};
		let description = b"12345".to_vec();
		assert_ok!(Bounties::propose_bounty(
			RuntimeOrigin::signed(proposer),
			50,
			description.clone()
		));

		// Verify initial state
		assert_eq!(Balances::reserved_balance(proposer), deposit);
		assert_eq!(Balances::reserved_balance(1), 0);
		assert_eq!(Balances::free_balance(proposer), 100 - deposit);
		assert_eq!(last_event(), BountiesEvent::BountyProposed { index: bounty_id });
		assert_eq!(pallet_bounties::Bounties::<Test>::get(0).unwrap(), bounty);
		assert_eq!(pallet_bounties::BountyDescriptions::<Test>::get(0).unwrap(), description);
		assert_eq!(pallet_bounties::BountyCount::<Test>::get(), 1);

		// Change status to approved
		bounty.status = BountyStatus::Approved;
		pallet_bounties::Bounties::<Test>::insert(bounty_id, &bounty);
		// Poke deposit should fail due to invalid status
		assert_noop!(
			Bounties::poke_deposit(RuntimeOrigin::signed(proposer), bounty_id),
			Error::<Test>::UnexpectedStatus
		);

		// Change status to funded
		bounty.status = BountyStatus::Funded;
		pallet_bounties::Bounties::<Test>::insert(bounty_id, &bounty);
		// Poke deposit should fail due to invalid status
		assert_noop!(
			Bounties::poke_deposit(RuntimeOrigin::signed(proposer), bounty_id),
			Error::<Test>::UnexpectedStatus
		);

		// Change status to curator proposed
		bounty.status = BountyStatus::CuratorProposed { curator };
		pallet_bounties::Bounties::<Test>::insert(bounty_id, &bounty);
		// Poke deposit should fail due to invalid status
		assert_noop!(
			Bounties::poke_deposit(RuntimeOrigin::signed(proposer), bounty_id),
			Error::<Test>::UnexpectedStatus
		);

		// Change status to active
		bounty.status = BountyStatus::Active { curator, update_due: 24 };
		pallet_bounties::Bounties::<Test>::insert(bounty_id, &bounty);
		// Poke deposit should fail due to invalid status
		assert_noop!(
			Bounties::poke_deposit(RuntimeOrigin::signed(proposer), bounty_id),
			Error::<Test>::UnexpectedStatus
		);

		// Change status to PendingPayout
		bounty.status = BountyStatus::PendingPayout { curator, beneficiary: 0, unlock_at: 24 };
		pallet_bounties::Bounties::<Test>::insert(bounty_id, &bounty);
		// Poke deposit should fail due to invalid status
		assert_noop!(
			Bounties::poke_deposit(RuntimeOrigin::signed(proposer), bounty_id),
			Error::<Test>::UnexpectedStatus
		);

		// Change status to ApprovedWithCurator
		bounty.status = BountyStatus::ApprovedWithCurator { curator };
		pallet_bounties::Bounties::<Test>::insert(bounty_id, &bounty);
		// Poke deposit should fail due to invalid status
		assert_noop!(
			Bounties::poke_deposit(RuntimeOrigin::signed(proposer), bounty_id),
			Error::<Test>::UnexpectedStatus
		);
	});
}

#[test]
fn poke_deposit_works_and_charges_fee_for_unchanged_deposit() {
	ExtBuilder::default().build_and_execute(|| {
		let bounty_id = 0;
		let proposer = 0;
		let description = b"12345".to_vec();
		let bounded_description = description.clone().try_into().unwrap();
		let deposit = Bounties::calculate_bounty_deposit(&bounded_description);
		let bounty = Bounty {
			proposer,
			value: 50,
			fee: 0,
			curator_deposit: 0,
			bond: deposit,
			status: BountyStatus::Proposed,
		};
		assert_ok!(Bounties::propose_bounty(
			RuntimeOrigin::signed(proposer),
			50,
			description.clone()
		));

		// Verify initial state
		assert_eq!(Balances::reserved_balance(proposer), deposit);
		assert_eq!(Balances::free_balance(proposer), 100 - deposit);
		assert_eq!(last_event(), BountiesEvent::BountyProposed { index: bounty_id });
		assert_eq!(pallet_bounties::Bounties::<Test>::get(0).unwrap(), bounty);
		assert_eq!(pallet_bounties::BountyDescriptions::<Test>::get(0).unwrap(), description);
		assert_eq!(pallet_bounties::BountyCount::<Test>::get(), 1);

		// Poke deposit should charge fee
		let result = Bounties::poke_deposit(RuntimeOrigin::signed(proposer), bounty_id);
		assert_ok!(result.as_ref());
		assert_eq!(result.unwrap(), Pays::Yes.into());

		// Verify final state
		assert_eq!(Balances::reserved_balance(proposer), deposit);
		assert_eq!(pallet_bounties::Bounties::<Test>::get(0).unwrap(), bounty);
		assert_eq!(pallet_bounties::BountyDescriptions::<Test>::get(0).unwrap(), description);
		assert_eq!(pallet_bounties::BountyCount::<Test>::get(), 1);
		assert_eq!(last_event(), BountiesEvent::BountyProposed { index: bounty_id });
	});
}

#[test]
fn poke_deposit_works_for_deposit_increase() {
	ExtBuilder::default().build_and_execute(|| {
		let bounty_id = 0;
		let proposer = 0;
		let description = b"12345".to_vec();
		let bounded_description = description.clone().try_into().unwrap();
		let deposit = Bounties::calculate_bounty_deposit(&bounded_description);
		let mut bounty = Bounty {
			proposer,
			value: 50,
			fee: 0,
			curator_deposit: 0,
			bond: deposit,
			status: BountyStatus::Proposed,
		};
		assert_ok!(Bounties::propose_bounty(
			RuntimeOrigin::signed(proposer),
			50,
			description.clone()
		));

		// Verify initial state
		assert_eq!(Balances::reserved_balance(proposer), deposit);
		assert_eq!(Balances::free_balance(proposer), 100 - deposit);
		assert_eq!(last_event(), BountiesEvent::BountyProposed { index: bounty_id });
		assert_eq!(pallet_bounties::Bounties::<Test>::get(0).unwrap(), bounty);
		assert_eq!(pallet_bounties::BountyDescriptions::<Test>::get(0).unwrap(), description);
		assert_eq!(pallet_bounties::BountyCount::<Test>::get(), 1);

		// Increase the DataDepositPerByte
		DataDepositPerByte::set(2);
		// BountyDepositBase (80) + DataDepositPerByte (2) * description.len() (5)
		let new_deposit = Bounties::calculate_bounty_deposit(&bounded_description);

		// Poke deposit should increase reserve
		let result = Bounties::poke_deposit(RuntimeOrigin::signed(proposer), bounty_id);
		assert_ok!(result.as_ref());
		assert_eq!(result.unwrap(), Pays::No.into());

		// Verify final state
		assert_eq!(Balances::reserved_balance(proposer), new_deposit);
		assert_eq!(Balances::free_balance(proposer), 100 - new_deposit);
		assert_eq!(
			last_event(),
			BountiesEvent::DepositPoked { bounty_id, proposer, old_deposit: deposit, new_deposit }
		);
		bounty.bond = new_deposit;
		assert_eq!(pallet_bounties::Bounties::<Test>::get(0).unwrap(), bounty);
		assert_eq!(pallet_bounties::BountyDescriptions::<Test>::get(0).unwrap(), description);
		assert_eq!(pallet_bounties::BountyCount::<Test>::get(), 1);
	});
}

#[test]
fn poke_deposit_works_for_deposit_decrease() {
	ExtBuilder::default().build_and_execute(|| {
		let bounty_id = 0;
		let proposer = 0;
		let description = b"12345".to_vec();
		let bounded_description = description.clone().try_into().unwrap();
		DataDepositPerByte::set(2);
		let deposit = Bounties::calculate_bounty_deposit(&bounded_description);
		let mut bounty = Bounty {
			proposer,
			value: 50,
			fee: 0,
			curator_deposit: 0,
			bond: deposit,
			status: BountyStatus::Proposed,
		};

		assert_ok!(Bounties::propose_bounty(
			RuntimeOrigin::signed(proposer),
			50,
			description.clone()
		));

		// Verify initial state
		assert_eq!(Balances::reserved_balance(proposer), deposit);
		assert_eq!(Balances::free_balance(proposer), 100 - deposit);
		assert_eq!(last_event(), BountiesEvent::BountyProposed { index: bounty_id });
		assert_eq!(pallet_bounties::Bounties::<Test>::get(0).unwrap(), bounty);
		assert_eq!(pallet_bounties::BountyDescriptions::<Test>::get(0).unwrap(), description);
		assert_eq!(pallet_bounties::BountyCount::<Test>::get(), 1);

		// Decrease the DataDepositPerByte
		DataDepositPerByte::set(1);
		// BountyDepositBase (80) + DataDepositPerByte (2) * description.len() (5)
		let new_deposit = Bounties::calculate_bounty_deposit(&bounded_description);

		// Poke deposit should increase reserve
		let result = Bounties::poke_deposit(RuntimeOrigin::signed(proposer), bounty_id);
		assert_ok!(result.as_ref());
		assert_eq!(result.unwrap(), Pays::No.into());

		// Verify final state
		assert_eq!(Balances::reserved_balance(proposer), new_deposit);
		assert_eq!(Balances::free_balance(proposer), 100 - new_deposit);
		assert_eq!(
			last_event(),
			BountiesEvent::DepositPoked { bounty_id, proposer, old_deposit: deposit, new_deposit }
		);
		bounty.bond = new_deposit;
		assert_eq!(pallet_bounties::Bounties::<Test>::get(0).unwrap(), bounty);
		assert_eq!(pallet_bounties::BountyDescriptions::<Test>::get(0).unwrap(), description);
		assert_eq!(pallet_bounties::BountyCount::<Test>::get(), 1);
	});
}

#[test]
fn poke_deposit_works_for_non_proposer() {
	ExtBuilder::default().build_and_execute(|| {
		let bounty_id = 0;
		let proposer = 0;
		let non_proposer = 1;
		let description = b"12345".to_vec();
		let bounded_description = description.clone().try_into().unwrap();

		DataDepositPerByte::set(2);
		let deposit = Bounties::calculate_bounty_deposit(&bounded_description);
		let mut bounty = Bounty {
			proposer,
			value: 50,
			fee: 0,
			curator_deposit: 0,
			bond: deposit,
			status: BountyStatus::Proposed,
		};
		assert_ok!(Bounties::propose_bounty(
			RuntimeOrigin::signed(proposer),
			50,
			description.clone()
		));

		// Verify initial state
		assert_eq!(Balances::reserved_balance(proposer), deposit);
		assert_eq!(Balances::free_balance(proposer), 100 - deposit);
		assert_eq!(last_event(), BountiesEvent::BountyProposed { index: bounty_id });
		assert_eq!(pallet_bounties::Bounties::<Test>::get(0).unwrap(), bounty);
		assert_eq!(pallet_bounties::BountyDescriptions::<Test>::get(0).unwrap(), description);
		assert_eq!(pallet_bounties::BountyCount::<Test>::get(), 1);

		// Decrease the DataDepositPerByte
		DataDepositPerByte::set(1);
		// BountyDepositBase (80) + DataDepositPerByte (2) * description.len() (5)
		let new_deposit = Bounties::calculate_bounty_deposit(&bounded_description);

		// Poke deposit should increase reserve
		let result = Bounties::poke_deposit(RuntimeOrigin::signed(non_proposer), bounty_id);
		assert_ok!(result.as_ref());
		assert_eq!(result.unwrap(), Pays::No.into());

		// Verify final state
		assert_eq!(Balances::reserved_balance(proposer), new_deposit);
		assert_eq!(Balances::free_balance(proposer), 100 - new_deposit);
		assert_eq!(
			last_event(),
			BountiesEvent::DepositPoked { bounty_id, proposer, old_deposit: deposit, new_deposit }
		);
		bounty.bond = new_deposit;
		assert_eq!(pallet_bounties::Bounties::<Test>::get(0).unwrap(), bounty);
		assert_eq!(pallet_bounties::BountyDescriptions::<Test>::get(0).unwrap(), description);
		assert_eq!(pallet_bounties::BountyCount::<Test>::get(), 1);
	});
}

#[test]
fn reclaim_bounty_funds_works_for_funded_bounty() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);

		// Propose and approve a bounty so it gets funded.
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));
		go_to_block(2);

		// Bounty is now Funded; its sub-account holds 50.
		let bounty_account = Bounties::bounty_account_id(0);
		assert_eq!(Balances::free_balance(&bounty_account), 50);

		pallet_bounties::Bounties::<Test>::remove(0);
		pallet_bounties::BountyDescriptions::<Test>::remove(0);

		let treasury_before = Treasury::pot();

		// Anyone can call reclaim_bounty_funds.
		assert_ok!(Bounties::reclaim_bounty_funds(RuntimeOrigin::signed(1), 0));

		assert_eq!(last_event(), BountiesEvent::BountyFundsReclaimed { bounty_id: 0 },);

		// Bounty account is now empty.
		assert_eq!(Balances::free_balance(&bounty_account), 0);

		// Treasury received the funds (minus any burn that happened at go_to_block(2)).
		assert!(Treasury::pot() > treasury_before);
	});
}

#[test]
fn reclaim_bounty_funds_works_after_accidental_refund() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);

		// Full lifecycle: propose → approve → fund → curator → award → claim
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));
		go_to_block(2);

		let fee = 4;
		Balances::make_free_balance_be(&4, 10);
		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, 4, fee));
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(4), 0));
		assert_ok!(Bounties::award_bounty(RuntimeOrigin::signed(4), 0, 3));
		go_to_block(5);
		assert_ok!(Bounties::claim_bounty(RuntimeOrigin::signed(1), 0));

		// Bounty is now fully closed; verify it is gone from storage.
		assert!(pallet_bounties::Bounties::<Test>::get(0).is_none());

		let bounty_account = Bounties::bounty_account_id(0);
		// Account should already be empty after claim.
		assert_eq!(Balances::free_balance(&bounty_account), 0);

		// Simulate someone accidentally sending funds to the closed bounty account.
		Balances::make_free_balance_be(&bounty_account, 25);
		assert_eq!(Balances::free_balance(&bounty_account), 25);

		let treasury_before = Treasury::pot();

		// Dust the account.
		assert_ok!(Bounties::reclaim_bounty_funds(RuntimeOrigin::signed(99), 0));
		assert_eq!(last_event(), BountiesEvent::BountyFundsReclaimed { bounty_id: 0 },);

		assert_eq!(Balances::free_balance(&bounty_account), 0);
		assert!(Treasury::pot() > treasury_before);
	});
}

#[test]
fn reclaim_bounty_funds_fails_when_bounty_still_active() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));
		go_to_block(2);

		// Bounty is in Funded state (still exists in storage).
		assert_noop!(
			Bounties::reclaim_bounty_funds(RuntimeOrigin::signed(1), 0),
			Error::<Test>::BountyStillActive
		);
	});
}

#[test]
fn reclaim_bounty_funds_fails_when_bounty_in_proposed_state() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));

		// Bounty is in Proposed state (still exists in storage).
		assert_noop!(
			Bounties::reclaim_bounty_funds(RuntimeOrigin::signed(1), 0),
			Error::<Test>::BountyStillActive
		);
	});
}

#[test]
fn reclaim_bounty_funds_fails_when_bounty_in_curator_proposed_state() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));
		go_to_block(2);
		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, 4, 4));

		// Bounty is in CuratorProposed state.
		assert_noop!(
			Bounties::reclaim_bounty_funds(RuntimeOrigin::signed(1), 0),
			Error::<Test>::BountyStillActive
		);
	});
}

#[test]
fn reclaim_bounty_funds_is_paid_noop_when_account_already_empty() {
	ExtBuilder::default().build_and_execute(|| {
		// No bounty at index 99 and its derived account is empty: the call succeeds as
		// a paid no-op, so it cannot be used to grief the network.
		let result = Bounties::reclaim_bounty_funds(RuntimeOrigin::signed(1), 99);
		assert_ok!(result.as_ref());
		assert_eq!(result.unwrap().pays_fee, Pays::Yes);
	});
}

#[test]
fn reclaim_bounty_funds_fails_for_unsigned_origin() {
	ExtBuilder::default().build_and_execute(|| {
		assert_noop!(
			Bounties::reclaim_bounty_funds(RuntimeOrigin::none(), 0),
			DispatchError::BadOrigin
		);
	});
}

#[test]
fn reclaim_bounty_funds_can_be_called_by_anyone() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));
		go_to_block(2);

		// Forcibly remove bounty, leave account funded.
		pallet_bounties::Bounties::<Test>::remove(0);
		pallet_bounties::BountyDescriptions::<Test>::remove(0);

		// A random account (2) with only 1 token should be able to call this.
		assert_ok!(Bounties::reclaim_bounty_funds(RuntimeOrigin::signed(2), 0));
		assert_eq!(last_event(), BountiesEvent::BountyFundsReclaimed { bounty_id: 0 },);
	});
}

#[test]
fn reclaim_bounty_funds_works_with_additional_assets() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));
		go_to_block(2);
		assert_ok!(Bounties::propose_curator(RuntimeOrigin::root(), 0, 0, 1));
		assert_ok!(Bounties::accept_curator(RuntimeOrigin::signed(0), 0));

		let bounty_account = Bounties::bounty_account_id(0);

		// Load the bounty account with both native and asset 1.
		Balances::make_free_balance_be(&bounty_account, 100);
		assert_ok!(Assets::transfer(RuntimeOrigin::signed(0), 1, bounty_account, 10));

		pallet_bounties::Bounties::<Test>::remove(0);
		pallet_bounties::BountyDescriptions::<Test>::remove(0);

		let treasury_native_before = Balances::free_balance(Bounties::account_id());
		let treasury_asset1_before = Assets::balance(1, &Bounties::account_id());

		// A single call reclaims both the native token and asset 1.
		assert_ok!(Bounties::reclaim_bounty_funds(RuntimeOrigin::signed(5), 0));
		assert_eq!(last_event(), BountiesEvent::BountyFundsReclaimed { bounty_id: 0 },);

		// Native balance drained to at most ED.
		assert!(Balances::free_balance(&bounty_account) <= 1);

		// Asset 1 swept to treasury.
		assert_eq!(Assets::balance(1, &bounty_account), 0);
		assert!(Assets::balance(1, &Bounties::account_id()) > treasury_asset1_before);

		// Treasury received native funds.
		assert!(Balances::free_balance(Bounties::account_id()) > treasury_native_before);
	});
}

#[test]
fn reclaim_bounty_funds_works_after_close_bounty() {
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));
		go_to_block(2);

		// Extra funds added to the bounty account.
		let bounty_account = Bounties::bounty_account_id(0);
		Balances::make_free_balance_be(&bounty_account, 60);

		// Close the bounty through the normal path.
		assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), 0));

		// After close_bounty the bounty no longer exists in storage.
		assert!(pallet_bounties::Bounties::<Test>::get(0).is_none());

		// close_bounty already swept the account, so reclaiming again is a paid no-op.
		assert_ok!(Bounties::reclaim_bounty_funds(RuntimeOrigin::signed(1), 0));
	});
}

#[test]
fn reclaim_bounty_funds_is_free_for_caller() {
	// Verify Pays::No is returned on success so the caller isn't charged.
	ExtBuilder::default().build_and_execute(|| {
		Balances::make_free_balance_be(&Treasury::account_id(), 101);
		assert_ok!(Bounties::propose_bounty(RuntimeOrigin::signed(0), 50, b"12345".to_vec()));
		assert_ok!(Bounties::approve_bounty(RuntimeOrigin::root(), 0));
		go_to_block(2);

		pallet_bounties::Bounties::<Test>::remove(0);
		pallet_bounties::BountyDescriptions::<Test>::remove(0);

		let result = Bounties::reclaim_bounty_funds(RuntimeOrigin::signed(1), 0);
		assert_ok!(result.as_ref());
		// The extrinsic must return Pays::No.
		assert_eq!(
			result.unwrap(),
			frame_support::dispatch::PostDispatchInfo {
				actual_weight: None,
				pays_fee: frame_support::pallet_prelude::Pays::No,
			}
		);
	});
}

#[test]
fn reclaim_bounty_funds_respects_native_locks() {
	use frame_support::traits::{LockableCurrency, WithdrawReasons};

	ExtBuilder::default().build_and_execute(|| {
		let bounty_account = Bounties::bounty_account_id(0);
		let treasury = Bounties::account_id();

		Balances::make_free_balance_be(&bounty_account, 100);
		// Lock 60 of the 100 native; only the 40 liquid units should be reclaimable.
		<Balances as LockableCurrency<_>>::set_lock(
			*b"testlock",
			&bounty_account,
			60,
			WithdrawReasons::all(),
		);

		let treasury_before = Balances::free_balance(&treasury);
		assert!(!pallet_bounties::Bounties::<Test>::contains_key(0));
		let res = Bounties::reclaim_bounty_funds(RuntimeOrigin::signed(1), 0);
		assert_ok!(res.as_ref());

		// Liquid part swept, locked part left behind.
		assert_eq!(Balances::free_balance(&treasury).saturating_sub(treasury_before), 40);
		assert_eq!(Balances::free_balance(&bounty_account), 60);
		assert_eq!(last_event(), BountiesEvent::BountyFundsReclaimed { bounty_id: 0 });
		assert_eq!(res.unwrap().pays_fee, Pays::No);
	});
}
