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

//! Tests.

#![cfg(test)]

use crate::{
	self as pallet_balances, AccountData, Config, CreditOf, Error, Pallet, TotalIssuance,
	DEFAULT_ADDRESS_URI,
};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame_support::{
	assert_err, assert_noop, assert_ok, assert_storage_noop, derive_impl,
	dispatch::{DispatchInfo, GetDispatchInfo},
	parameter_types,
	traits::{
		fungible, ConstU32, ConstU8, Imbalance as ImbalanceT, LockIdentifier, LockableCurrency,
		OnUnbalanced, StorageMapShim, StoredMap, VariantCount, VariantCountOf,
		WhitelistedStorageKeys, WithdrawReasons,
	},
	weights::{IdentityFee, Weight},
};
use frame_system::{self as system, RawOrigin};
use pallet_transaction_payment::{ChargeTransactionPayment, FungibleAdapter, Multiplier};
use scale_info::TypeInfo;
use sp_core::{hexdisplay::HexDisplay, sr25519::Pair as SrPair, Pair};
use sp_io;
use sp_runtime::{
	traits::{BadOrigin, Zero},
	ArithmeticError, BuildStorage, Debug, DispatchError, DispatchResult, FixedPointNumber,
	TokenError,
};
use std::collections::BTreeSet;

mod consumer_limit_tests;
mod currency_tests;
mod dispatchable_tests;
mod fungible_and_currency;
mod fungible_conformance_tests;
mod fungible_tests;
mod general_tests;
mod reentrancy_tests;

type Block = frame_system::mocking::MockBlock<Test>;

#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Copy,
	Clone,
	Eq,
	PartialEq,
	Ord,
	PartialOrd,
	MaxEncodedLen,
	TypeInfo,
	Debug,
)]
pub enum TestId {
	Foo,
	Bar,
	Baz,
}

impl VariantCount for TestId {
	const VARIANT_COUNT: u32 = 3;
}

pub(crate) type AccountId = <Test as frame_system::Config>::AccountId;
pub(crate) type Balance = <Test as Config>::Balance;

frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		TransactionPayment: pallet_transaction_payment,
	}
);

parameter_types! {
	pub BlockWeights: frame_system::limits::BlockWeights =
		frame_system::limits::BlockWeights::simple_max(
			frame_support::weights::Weight::from_parts(1024, u64::MAX),
		);
	pub static ExistentialDeposit: u64 = 1;
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountData = super::AccountData<u64>;
}

#[derive_impl(pallet_transaction_payment::config_preludes::TestDefaultConfig)]
impl pallet_transaction_payment::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type OnChargeTransaction = FungibleAdapter<Pallet<Test>, ()>;
	type OperationalFeeMultiplier = ConstU8<5>;
	type WeightToFee = IdentityFee<u64>;
	type LengthToFee = IdentityFee<u64>;
}

parameter_types! {
	pub FooReason: TestId = TestId::Foo;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl Config for Test {
	type DustRemoval = DustTrap;
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = TestAccountStore;
	type MaxReserves = ConstU32<2>;
	type ReserveIdentifier = TestId;
	type RuntimeHoldReason = TestId;
	type RuntimeFreezeReason = TestId;
	type FreezeIdentifier = TestId;
	type MaxFreezes = VariantCountOf<TestId>;
}

#[derive(Clone)]
pub struct ExtBuilder {
	existential_deposit: u64,
	monied: bool,
	dust_trap: Option<u64>,
	reentrant_dust_action: Option<ReentrantDustAction>,
}
impl Default for ExtBuilder {
	fn default() -> Self {
		Self { existential_deposit: 1, monied: false, dust_trap: None, reentrant_dust_action: None }
	}
}
impl ExtBuilder {
	pub fn existential_deposit(mut self, existential_deposit: u64) -> Self {
		self.existential_deposit = existential_deposit;
		self
	}
	pub fn monied(mut self, monied: bool) -> Self {
		self.monied = monied;
		if self.existential_deposit == 0 {
			self.existential_deposit = 1;
		}
		self
	}
	pub fn dust_trap(mut self, account: u64) -> Self {
		self.dust_trap = Some(account);
		self
	}
	pub fn reentrant_dust_hold(mut self, account: u64) -> Self {
		self.reentrant_dust_action = Some(ReentrantDustAction::Hold(account));
		self
	}
	pub fn reentrant_dust_freeze(mut self, account: u64) -> Self {
		self.reentrant_dust_action = Some(ReentrantDustAction::Freeze(account));
		self
	}
	pub fn reentrant_dust_lock(mut self, account: u64) -> Self {
		self.reentrant_dust_action = Some(ReentrantDustAction::Lock(account));
		self
	}
	#[cfg(feature = "try-runtime")]
	pub fn auto_try_state(self, auto_try_state: bool) -> Self {
		AutoTryState::set(auto_try_state);
		self
	}
	pub fn set_associated_consts(&self) {
		DUST_TRAP_TARGET.with(|v| v.replace(self.dust_trap));
		REENTRANT_DUST_ACTION_VALUE.with(|v| v.replace(self.reentrant_dust_action));
		EXISTENTIAL_DEPOSIT.with(|v| v.replace(self.existential_deposit));
	}
	pub fn build(self) -> sp_io::TestExternalities {
		self.set_associated_consts();
		let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
		pallet_balances::GenesisConfig::<Test> {
			balances: if self.monied {
				vec![
					(1, 10 * self.existential_deposit),
					(2, 20 * self.existential_deposit),
					(3, 30 * self.existential_deposit),
					(4, 40 * self.existential_deposit),
					(12, 10 * self.existential_deposit),
				]
			} else {
				vec![]
			},
			dev_accounts: Some((
				1000,
				self.existential_deposit,
				Some(DEFAULT_ADDRESS_URI.to_string()),
			)),
		}
		.assimilate_storage(&mut t)
		.unwrap();

		let mut ext = sp_io::TestExternalities::new(t);
		ext.execute_with(|| System::set_block_number(1));
		ext
	}
	pub fn build_and_execute_with(self, f: impl Fn()) {
		let other = self.clone();
		UseSystem::set(false);
		other.build().execute_with(|| {
			f();
			if AutoTryState::get() {
				Balances::do_try_state(System::block_number()).unwrap();
			}
		});
		UseSystem::set(true);
		self.build().execute_with(|| {
			f();
			if AutoTryState::get() {
				Balances::do_try_state(System::block_number()).unwrap();
			}
		});
	}
}

#[derive(Clone, Copy)]
enum ReentrantDustAction {
	Hold(u64),
	Freeze(u64),
	Lock(u64),
}

const REENTRANT_LOCK_ID: LockIdentifier = *b"RELOCK__";

parameter_types! {
	static DustTrapTarget: Option<u64> = None;
	static ReentrantDustActionValue: Option<ReentrantDustAction> = None;
}

pub struct DustTrap;

impl OnUnbalanced<CreditOf<Test, ()>> for DustTrap {
	fn on_nonzero_unbalanced(amount: CreditOf<Test, ()>) {
		match ReentrantDustActionValue::get() {
			Some(ReentrantDustAction::Hold(account)) => {
				<Balances as fungible::Mutate<_>>::mint_into(&account, 20)
					.expect("reentrant mint should succeed");
				<Balances as fungible::MutateHold<_>>::hold(&TestId::Bar, &account, 7)
					.expect("reentrant hold should succeed");
			},
			Some(ReentrantDustAction::Freeze(account)) => {
				<Balances as fungible::MutateFreeze<_>>::set_freeze(&TestId::Bar, &account, 7)
					.expect("reentrant freeze should succeed");
			},
			Some(ReentrantDustAction::Lock(account)) => {
				<Balances as fungible::Mutate<_>>::mint_into(&account, 20)
					.expect("reentrant mint should succeed");
				<Balances as LockableCurrency<_>>::set_lock(
					REENTRANT_LOCK_ID,
					&account,
					7,
					WithdrawReasons::all(),
				);
			},
			None => {},
		}

		match DustTrapTarget::get() {
			None => drop(amount),
			Some(a) => {
				let result = <Balances as fungible::Balanced<_>>::resolve(&a, amount);
				debug_assert!(result.is_ok());
			},
		}
	}
}

parameter_types! {
	pub static UseSystem: bool = false;
	pub static AutoTryState: bool = true;
}

type BalancesAccountStore = StorageMapShim<super::Account<Test>, u64, super::AccountData<u64>>;
type SystemAccountStore = frame_system::Pallet<Test>;

pub struct TestAccountStore;
impl StoredMap<u64, super::AccountData<u64>> for TestAccountStore {
	fn get(k: &u64) -> super::AccountData<u64> {
		if UseSystem::get() {
			<SystemAccountStore as StoredMap<_, _>>::get(k)
		} else {
			<BalancesAccountStore as StoredMap<_, _>>::get(k)
		}
	}
	fn try_mutate_exists<R, E: From<DispatchError>>(
		k: &u64,
		f: impl FnOnce(&mut Option<super::AccountData<u64>>) -> Result<R, E>,
	) -> Result<R, E> {
		if UseSystem::get() {
			<SystemAccountStore as StoredMap<_, _>>::try_mutate_exists(k, f)
		} else {
			<BalancesAccountStore as StoredMap<_, _>>::try_mutate_exists(k, f)
		}
	}
	fn mutate<R>(
		k: &u64,
		f: impl FnOnce(&mut super::AccountData<u64>) -> R,
	) -> Result<R, DispatchError> {
		if UseSystem::get() {
			<SystemAccountStore as StoredMap<_, _>>::mutate(k, f)
		} else {
			<BalancesAccountStore as StoredMap<_, _>>::mutate(k, f)
		}
	}
	fn mutate_exists<R>(
		k: &u64,
		f: impl FnOnce(&mut Option<super::AccountData<u64>>) -> R,
	) -> Result<R, DispatchError> {
		if UseSystem::get() {
			<SystemAccountStore as StoredMap<_, _>>::mutate_exists(k, f)
		} else {
			<BalancesAccountStore as StoredMap<_, _>>::mutate_exists(k, f)
		}
	}
	fn insert(k: &u64, t: super::AccountData<u64>) -> Result<(), DispatchError> {
		if UseSystem::get() {
			<SystemAccountStore as StoredMap<_, _>>::insert(k, t)
		} else {
			<BalancesAccountStore as StoredMap<_, _>>::insert(k, t)
		}
	}
	fn remove(k: &u64) -> Result<(), DispatchError> {
		if UseSystem::get() {
			<SystemAccountStore as StoredMap<_, _>>::remove(k)
		} else {
			<BalancesAccountStore as StoredMap<_, _>>::remove(k)
		}
	}
}

pub fn events() -> Vec<RuntimeEvent> {
	let evt = System::events().into_iter().map(|evt| evt.event).collect::<Vec<_>>();
	System::reset_events();
	evt
}

/// create a transaction info struct from weight. Handy to avoid building the whole struct.
pub fn info_from_weight(w: Weight) -> DispatchInfo {
	DispatchInfo { call_weight: w, ..Default::default() }
}

/// Check that the total-issuance matches the sum of all accounts' total balances.
pub fn ensure_ti_valid() {
	let mut sum = 0;

	// Fetch the dev accounts from Account Storage.
	let dev_accounts = (1000, EXISTENTIAL_DEPOSIT, DEFAULT_ADDRESS_URI.to_string());
	let (num_accounts, _balance, ref derivation) = dev_accounts;

	// Generate the dev account public keys.
	let dev_account_ids: Vec<_> = (0..num_accounts)
		.map(|index| {
			let derivation_string = derivation.replace("{}", &index.to_string());
			let pair: SrPair =
				Pair::from_string(&derivation_string, None).expect("Invalid derivation string");
			<crate::tests::Test as frame_system::Config>::AccountId::decode(
				&mut &pair.public().encode()[..],
			)
			.unwrap()
		})
		.collect();

	// Iterate over all account keys (i.e., the account IDs).
	for acc in frame_system::Account::<Test>::iter_keys() {
		// Skip dev accounts by checking if the account is in the dev_account_ids list.
		// This also proves dev_accounts exists in storage.
		if dev_account_ids.contains(&acc) {
			continue;
		}

		// Check if we are using the system pallet or some other custom storage for accounts.
		if UseSystem::get() {
			let data = frame_system::Pallet::<Test>::account(acc);
			sum += data.data.total();
		} else {
			let data = crate::Account::<Test>::get(acc);
			sum += data.total();
		}
	}

	// Ensure the total issuance matches the sum of the account balances
	assert_eq!(TotalIssuance::<Test>::get(), sum, "Total Issuance is incorrect");
}

#[test]
fn weights_sane() {
	let info = crate::Call::<Test>::transfer_allow_death { dest: 10, value: 4 }.get_dispatch_info();
	assert_eq!(<() as crate::WeightInfo>::transfer_allow_death(), info.call_weight);

	let info = crate::Call::<Test>::force_unreserve { who: 10, amount: 4 }.get_dispatch_info();
	assert_eq!(<() as crate::WeightInfo>::force_unreserve(), info.call_weight);
}

#[test]
fn check_whitelist() {
	let whitelist: BTreeSet<String> = AllPalletsWithSystem::whitelisted_storage_keys()
		.iter()
		.map(|s| HexDisplay::from(&s.key).to_string())
		.collect();
	// Inactive Issuance
	assert!(whitelist.contains("c2261276cc9d1f8598ea4b6a74b15c2f1ccde6872881f893a21de93dfe970cd5"));
	// Total Issuance
	assert!(whitelist.contains("c2261276cc9d1f8598ea4b6a74b15c2f57c875e4cff74148e4628f264b974c80"));
}

/// This pallet runs tests twice, once with system as `type AccountStore` and once this pallet. This
/// function will return the right value based on the `UseSystem` flag.
pub(crate) fn get_test_account_data(who: AccountId) -> AccountData<Balance> {
	if UseSystem::get() {
		<SystemAccountStore as StoredMap<_, _>>::get(&who)
	} else {
		<BalancesAccountStore as StoredMap<_, _>>::get(&who)
	}
}

/// Same as `get_test_account_data`, but returns a `frame_system::AccountInfo` with the data filled
/// in.
pub(crate) fn get_test_account(
	who: AccountId,
) -> frame_system::AccountInfo<u32, AccountData<Balance>> {
	let mut system_account = frame_system::Account::<Test>::get(&who);
	let account_data = get_test_account_data(who);
	system_account.data = account_data;
	system_account
}
