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

//! Test mock for the on-demand pallet.

use crate::{
	self as pallet_on_demand_para, Config, DefaultPricingProvider, PoolCapacityProvider,
	QueueOnDemandOrders, DEFAULT_BASE_FEE,
};
use fp_coretime::TaskId;
use frame_support::{
	derive_impl, ord_parameter_types,
	pallet_prelude::*,
	parameter_types,
	traits::{EitherOfDiverse, Hooks},
	PalletId,
};
use frame_system::{EnsureRoot, EnsureSignedBy};
use sp_runtime::{traits::BlockNumberProvider, BuildStorage};
use std::cell::RefCell;

pub type Balance = u64;
type RelayBlockNumber = u32;

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		OnDemand: pallet_on_demand_para,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountData = pallet_balances::AccountData<Balance>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type AccountStore = System;
}

thread_local! {
	/// The current Relay-chain block number, as seen by the pallet.
	pub static RELAY_BLOCK_NUMBER: RefCell<RelayBlockNumber> = const { RefCell::new(0) };
	/// The current number of cores in the on-demand pool, as seen by the pallet.
	pub static CORE_POOL_SIZE: RefCell<u32> = const { RefCell::new(1) };
	/// Records every batch handed to [`RecordingOrderQueue`].
	pub static QUEUED_BATCHES: RefCell<Vec<Vec<(TaskId, RelayBlockNumber)>>> =
		const { RefCell::new(Vec::new()) };
}

/// A Relay-chain block number provider the tests can move forward at will.
pub struct MockRelayBlockNumberProvider;
impl BlockNumberProvider for MockRelayBlockNumberProvider {
	type BlockNumber = RelayBlockNumber;

	fn current_block_number() -> Self::BlockNumber {
		RELAY_BLOCK_NUMBER.with(|n| *n.borrow())
	}
}

/// Stands in for the XCM that would carry the batch to the Relay chain.
pub struct RecordingOrderQueue;
impl QueueOnDemandOrders<RelayBlockNumber> for RecordingOrderQueue {
	fn queue_batch(batch: Vec<(TaskId, RelayBlockNumber)>) {
		QUEUED_BATCHES.with(|b| b.borrow_mut().push(batch));
	}
}

parameter_types! {
	pub const OnDemandPalletId: PalletId = PalletId(*b"TsOnDmnd");
}
ord_parameter_types! {
	pub const One: u64 = 1;
}

type EnsureOneOrRoot = EitherOfDiverse<EnsureRoot<u64>, EnsureSignedBy<One, u64>>;

pub struct MockCorePool;
impl PoolCapacityProvider for MockCorePool {
	fn pool_cores() -> u32 {
		CORE_POOL_SIZE.with(|n| *n.borrow())
	}
}

impl MockCorePool {
	pub fn set_pool_cores(n: u32) {
		CORE_POOL_SIZE.with(|pool| {
			*pool.borrow_mut() = n;
		});
	}
}

impl Config for Test {
	type WeightInfo = ();
	type Currency = Balances;
	type AdminOrigin = EnsureOneOrRoot;
	type RelayBlockNumberProvider = MockRelayBlockNumberProvider;
	type PoolCapacityProvider = MockCorePool;
	type PricingProvider = DefaultPricingProvider;
	type OrderQueue = RecordingOrderQueue;
	type MaxBatchSize = ConstU32<1000>;
	type PalletId = OnDemandPalletId;
}

/// Move the Relay chain to `relay_block_number`.
pub fn set_relay_block_number(relay_block_number: RelayBlockNumber) {
	RELAY_BLOCK_NUMBER.with(|n| *n.borrow_mut() = relay_block_number);
}

/// All batches forwarded to the Relay chain so far.
pub fn queued_batches() -> Vec<Vec<(TaskId, RelayBlockNumber)>> {
	QUEUED_BATCHES.with(|b| b.borrow().clone())
}

/// Finalize the current block and start the next one, running the pallet's hooks.
pub fn advance_block() {
	let now = System::block_number();
	OnDemand::on_finalize(now);
	System::set_block_number(now + 1);
	OnDemand::on_initialize(now + 1);
}

/// The default balance for test accounts.
pub const DEFAULT_ACCOUNT_BALANCE: u64 = DEFAULT_BASE_FEE as u64 * 1000;

pub fn new_test_ext() -> sp_io::TestExternalities {
	set_relay_block_number(0);
	QUEUED_BATCHES.with(|b| b.borrow_mut().clear());

	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	pallet_balances::GenesisConfig::<Test> {
		balances: vec![
			(1, DEFAULT_ACCOUNT_BALANCE),
			(2, DEFAULT_ACCOUNT_BALANCE),
			(3, DEFAULT_ACCOUNT_BALANCE),
		],
		..Default::default()
	}
	.assimilate_storage(&mut t)
	.unwrap();

	let mut ext: sp_io::TestExternalities = t.into();
	// Get past the genesis block so that events are recorded, and let the pallet initialize.
	ext.execute_with(|| {
		System::set_block_number(1);
		OnDemand::on_initialize(1);
	});
	ext
}
