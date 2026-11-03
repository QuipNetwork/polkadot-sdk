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

//! Test mock for the accumulate-and-forward pallet.

use crate::{self as pallet_accumulate_and_forward, Config};
use frame_support::{
	derive_impl, parameter_types,
	sp_runtime::traits::AccountIdConversion,
	traits::{
		fungible::Mutate,
		tokens::{Fortitude, Precision, Preservation},
	},
	weights::constants::RocksDbWeight,
	PalletId,
};
use sp_runtime::{traits::BlockNumberProvider, BuildStorage};
use std::cell::RefCell;

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		AccumulateForward: pallet_accumulate_and_forward,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u64>;
	/// Use non-zero DB weights so that weight exhaustion can be tested.
	type DbWeight = RocksDbWeight;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type AccountStore = System;
	type ExistentialDeposit = ExistentialDeposit;
	type DustRemoval = AccumulateForward;
}

thread_local! {
	/// Counts successful `MockForwarder::forward` calls.
	pub static SEND_COUNT: RefCell<u32> = RefCell::new(0);
	/// Set to `true` to make `MockForwarder::forward` return an error.
	pub static SEND_FAIL: RefCell<bool> = RefCell::new(false);
	/// Records the amount from the most recent successful `MockForwarder::forward` call.
	pub static LAST_SENT_AMOUNT: RefCell<Option<u64>> = RefCell::new(None);
	/// The block number reported by [`MockBlockNumberProvider`].
	pub static PROVIDER_BLOCK: RefCell<u64> = RefCell::new(0);
}

/// Block number provider that moves independently of the system block number, as
/// `RelaychainDataProvider` does on a parachain.
pub struct MockBlockNumberProvider;

impl BlockNumberProvider for MockBlockNumberProvider {
	type BlockNumber = u64;

	fn current_block_number() -> u64 {
		PROVIDER_BLOCK.with(|b| *b.borrow())
	}

	fn set_block_number(block: u64) {
		PROVIDER_BLOCK.with(|b| *b.borrow_mut() = block);
	}
}

/// Mock implementation of [`pallet_accumulate_and_forward::Forwarder`].
pub struct MockForwarder;

impl crate::Forwarder<u64, u64> for MockForwarder {
	fn forward(source: u64, amount: u64) -> Result<(), ()> {
		if SEND_FAIL.with(|f| *f.borrow()) {
			return Err(());
		}
		// Simulate the real implementation: burn funds from the source account.
		Balances::burn_from(
			&source,
			amount,
			Preservation::Preserve,
			Precision::Exact,
			Fortitude::Polite,
		)
		.map_err(|_| ())?;
		SEND_COUNT.with(|c| *c.borrow_mut() += 1);
		LAST_SENT_AMOUNT.with(|a| *a.borrow_mut() = Some(amount));
		Ok(())
	}
}

parameter_types! {
	pub const AccumulateForwardPalletId: PalletId = PalletId(*b"acf/dott");
	pub const ExistentialDeposit: u64 = 10;
	/// The transfer period in blocks. Mutable so tests can pick a period that observed blocks
	/// never hit exactly.
	pub static TransferPeriod: u64 = 5;
	/// The smallest transferable amount (above ED).
	pub const MinTransferAmount: u64 = 10;
}

impl Config for Test {
	type Currency = Balances;
	type PalletId = AccumulateForwardPalletId;
	type Forwarder = MockForwarder;
	type TransferPeriod = TransferPeriod;
	type MinTransferAmount = MinTransferAmount;
	type BlockNumberProvider = MockBlockNumberProvider;
	type WeightInfo = ();
}

pub fn new_test_ext(fund_accumulation: bool) -> sp_io::TestExternalities {
	let mut balances = vec![(1, 100), (2, 200), (3, 300)];

	if fund_accumulation {
		let accumulation_account: u64 = AccumulateForwardPalletId::get().into_account_truncating();
		balances.push((accumulation_account, ExistentialDeposit::get()));
	}

	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	pallet_balances::GenesisConfig::<Test> { balances, ..Default::default() }
		.assimilate_storage(&mut t)
		.unwrap();

	let mut ext: sp_io::TestExternalities = t.into();
	// Events are not recorded on the genesis block.
	ext.execute_with(|| System::set_block_number(1));
	ext
}
